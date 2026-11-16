//! Master recording (GitHub #6): tap the final master output into a WAV file.
//!
//! Same RT pattern as capture.rs/player.rs: the audio thread pushes interleaved
//! stereo f32 into a preallocated lock-free ring (never blocks; a full ring
//! drops whole frames and counts them); a writer thread drains the ring into a
//! 32-bit-float WAV under ~/Music/kabelsalat/ with a timestamped name, patching
//! the header on every written second (crash-safe) and on stop. The header is
//! hand-written (no `hound`).
//!
//! Ownership: the engine holds the [`RecorderTap`] (ring producer + a control
//! message ring) and is the authority on start/stop; the UI/agents hold the
//! [`Arc<RecorderShared>`] to read live status and to shut the writer down on
//! exit. Per-rig stems are not captured yet — the tap sits after the final
//! master sum, not per-rig (documented in PLAN §13).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{SystemTime, UNIX_EPOCH};

use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;

/// The live app's recorder, published once at startup so the control bridge can
/// answer `record_status` without threading an Arc through every signature.
pub static LIVE_RECORDER: OnceLock<Arc<RecorderShared>> = OnceLock::new();

#[derive(Debug, PartialEq)]
enum RecorderMsg {
    Start,
    Stop,
}

/// UI/agent-facing recording snapshot (serialized by `record_status`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct RecordStatus {
    pub recording: bool,
    pub path: Option<String>,
    pub seconds: f64,
    pub dropped_frames: u64,
    pub finalized: bool,
    /// Last start/stop failure (dir/file creation, write error), if any.
    pub error: Option<String>,
    /// The last take stopped at the 4 GiB WAV cap instead of on request.
    pub limit_reached: bool,
    /// Monotonic count of finalized takes (never resets, unlike `finalized`).
    pub takes: u64,
}

/// State shared by the audio thread (tap), the writer thread, and the UI.
/// `live`, `recording`, `shutdown` and `dropped_frames` are atomics (RT-safe);
/// `status` is a mutex owned by the writer thread — the audio callback never
/// touches it. `join` is the writer's JoinHandle, joined once on shutdown.
pub struct RecorderShared {
    live: AtomicBool,
    recording: AtomicBool,
    shutdown: AtomicBool,
    dropped_frames: AtomicU64,
    status: Mutex<WavStatus>,
    writer: OnceLock<Thread>,
    join: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone, Debug, Default)]
struct WavStatus {
    path: Option<String>,
    seconds: f64,
    finalized: bool,
    error: Option<String>,
    limit_reached: bool,
    takes: u64,
}

impl RecorderShared {
    pub fn new() -> Self {
        Self::with_live(false)
    }

    /// A recorder attached to a real writer thread (used by `start`).
    fn live() -> Self {
        Self::with_live(true)
    }

    fn with_live(live: bool) -> Self {
        Self {
            live: AtomicBool::new(live),
            recording: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            dropped_frames: AtomicU64::new(0),
            status: Mutex::new(WavStatus::default()),
            writer: OnceLock::new(),
            join: Mutex::new(None),
        }
    }

    /// True only when a writer thread + tap are attached (the dummy
    /// `RecorderShared::new()` used by attach mode and tests is not live).
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    fn unpark_writer(&self) {
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
    }

    /// Stop any take in flight, wake the writer, and join it so the WAV is
    /// drained and its header finalized before the process exits.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.recording.store(false, Ordering::Release);
        self.unpark_writer();
        if let Ok(mut j) = self.join.lock() {
            if let Some(h) = j.take() {
                let _ = h.join();
            }
        }
    }

    /// Assemble the current snapshot from the atomics + writer status.
    pub fn snapshot(&self) -> RecordStatus {
        let (path, seconds, finalized, error, limit_reached, takes) = self
            .status
            .lock()
            .map(|s| (s.path.clone(), s.seconds, s.finalized, s.error.clone(), s.limit_reached, s.takes))
            .unwrap_or((None, 0.0, false, None, false, 0));
        RecordStatus {
            recording: self.recording.load(Ordering::Relaxed),
            path,
            seconds,
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            finalized,
            error,
            limit_reached,
            takes,
        }
    }
}

/// Engine side: push the final output into the ring and signal the writer.
pub struct RecorderTap {
    producer: Producer<f32>,
    msg_tx: Producer<RecorderMsg>,
    shared: Arc<RecorderShared>,
}

impl RecorderTap {
    /// Idempotent start: only the false→true transition messages the writer and
    /// resets the drop counter for the new take.
    pub fn begin(&mut self) {
        if !self.shared.recording.swap(true, Ordering::Relaxed) {
            self.shared.dropped_frames.store(0, Ordering::Relaxed);
            let _ = self.msg_tx.push(RecorderMsg::Start);
            self.shared.unpark_writer();
        }
    }

    /// Idempotent stop: only the true→false transition tells the writer.
    pub fn stop(&mut self) {
        if self.shared.recording.swap(false, Ordering::Relaxed) {
            let _ = self.msg_tx.push(RecorderMsg::Stop);
            self.shared.unpark_writer();
        }
    }

    /// Push one block of interleaved f32 as stereo (channels 0/1). RT-safe:
    /// never blocks; a frame that can't fit whole is dropped and counted, so a
    /// frame is never split (an L without its R would swap every later frame).
    pub fn push_stereo(&mut self, data: &[f32], ch: usize) {
        if !self.shared.recording.load(Ordering::Relaxed) {
            return;
        }
        let ch = ch.max(1);
        let frames = data.len() / ch;
        let mut dropped = 0u64;
        for i in 0..frames {
            let l = data[i * ch];
            let r = if ch > 1 { data[i * ch + 1] } else { l };
            // `slots()` is a lower bound (the consumer only frees slots), so a
            // `>= 2` check guarantees both pushes succeed — never L-only.
            if self.producer.slots() >= 2 {
                let _ = self.producer.push(l);
                let _ = self.producer.push(r);
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.shared.dropped_frames.fetch_add(dropped, Ordering::Relaxed);
        }
    }
}

impl Drop for RecorderTap {
    fn drop(&mut self) {
        // Teardown without a UI handle (engine/stream dropped): stop the take
        // and wake the writer so it drains + finalizes + exits.
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.recording.store(false, Ordering::Release);
        self.shared.unpark_writer();
    }
}

/// Where recordings land: `$XDG_MUSIC_DIR/kabelsalat`, else `~/Music/kabelsalat`.
pub fn record_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_MUSIC_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("kabelsalat");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Music").join("kabelsalat")
}

/// Start the writer thread; return the engine tap and the shared status handle.
pub fn start(dir: PathBuf, sample_rate: u32, channels: usize) -> (RecorderTap, Arc<RecorderShared>) {
    let cap = (sample_rate as usize) * 2 * 4; // ~4 s stereo headroom
    start_with_capacity(dir, sample_rate, channels, cap)
}

fn start_with_capacity(
    dir: PathBuf,
    sample_rate: u32,
    channels: usize,
    cap: usize,
) -> (RecorderTap, Arc<RecorderShared>) {
    let shared = Arc::new(RecorderShared::live());
    let (producer, consumer) = RingBuffer::new(cap.max(2));
    let (msg_tx, msg_rx) = RingBuffer::new(64);
    let thread_shared = shared.clone();
    let handle = std::thread::Builder::new()
        .name("kabelsalat-recorder".into())
        .spawn(move || writer_loop(msg_rx, consumer, dir, sample_rate, channels, thread_shared))
        .expect("spawn recorder thread");
    // Known before anyone can begin/stop: an unpark is never lost to a
    // writer that has not registered itself yet.
    let _ = shared.writer.set(handle.thread().clone());
    if let Ok(mut j) = shared.join.lock() {
        *j = Some(handle);
    }
    (RecorderTap { producer, msg_tx, shared: shared.clone() }, shared)
}

fn writer_loop(
    mut msg: Consumer<RecorderMsg>,
    mut data: Consumer<f32>,
    dir: PathBuf,
    sample_rate: u32,
    channels: usize,
    shared: Arc<RecorderShared>,
) {
    let mut file: Option<std::fs::File> = None;
    let mut path: Option<String> = None;
    let mut samples: u64 = 0;
    let mut buf = vec![0.0f32; 4096];
    let mut bytes = Vec::with_capacity(4096 * 4);
    let max_data_samples = max_data_bytes(channels) / 4;
    let patch_every = (sample_rate as u64).saturating_mul(2).max(1); // ~1 s stereo
    let mut next_patch = 0u64;
    loop {
        // Control messages first (in order), so a Start/Stop raced by teardown
        // is still honored before the shutdown check below ends the loop.
        while let Ok(m) = msg.pop() {
            match m {
                RecorderMsg::Start => {
                    // Unreachable via the idempotent begin(); handled anyway.
                    if file.is_some() {
                        finalize_take(&mut data, &mut file, &mut samples, &mut buf, &mut bytes, max_data_samples);
                        set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), true, None, false);
                    }
                    let base = format!("kabelsalat-{}", timestamp());
                    if begin_take(&dir, sample_rate, channels, &base, &mut file, &mut path, &mut samples, &shared) {
                        next_patch = samples + patch_every;
                    } else {
                        file = None;
                        path = None;
                        samples = 0;
                    }
                }
                RecorderMsg::Stop => {
                    if file.is_some() {
                        finalize_take(&mut data, &mut file, &mut samples, &mut buf, &mut bytes, max_data_samples);
                        set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), true, None, false);
                    }
                    file = None;
                    path = None;
                    samples = 0;
                }
            }
        }
        if shared.shutdown.load(Ordering::Acquire) || msg.is_abandoned() {
            if file.is_some() {
                finalize_take(&mut data, &mut file, &mut samples, &mut buf, &mut bytes, max_data_samples);
                set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), true, None, false);
            }
            break;
        }
        if file.is_some() {
            let room = max_data_samples - samples;
            let n = drain(&mut data, &mut buf, room as usize);
            if n > 0 {
                samples_to_bytes(&buf[..n], &mut bytes);
                let written = file.as_mut().map(|f| f.write_all(&bytes).is_ok()).unwrap_or(false);
                if !written {
                    // Mid-take write error: finalize best-effort and drain the
                    // ring so stale samples don't leak into the next take.
                    if let Some(f) = file.as_mut() {
                        patch_header(f, samples, max_data_samples);
                    }
                    drain_to_empty(&mut data);
                    shared.recording.store(false, Ordering::Relaxed);
                    set_status(&shared, None, 0.0, true, Some("write failed".into()), false);
                    file = None;
                    path = None;
                    samples = 0;
                    continue;
                }
                samples += n as u64;
                if samples >= next_patch {
                    if let Some(f) = file.as_mut() {
                        patch_header(f, samples, max_data_samples);
                    }
                    next_patch = samples + patch_every;
                }
                if samples >= max_data_samples {
                    finalize_take(&mut data, &mut file, &mut samples, &mut buf, &mut bytes, max_data_samples);
                    set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), true, None, true);
                    shared.recording.store(false, Ordering::Relaxed);
                    file = None;
                    path = None;
                    samples = 0;
                    continue;
                }
                set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), false, None, false);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else {
            std::thread::park();
        }
    }
}

/// Drain the ring to empty and patch the header. A Stop must not leave
/// pushed-but-unwritten samples behind.
fn finalize_take(
    data: &mut Consumer<f32>,
    file: &mut Option<std::fs::File>,
    samples: &mut u64,
    buf: &mut [f32],
    bytes: &mut Vec<u8>,
    max_data_samples: u64,
) {
    loop {
        // Never past the frame-aligned RIFF limit; the rest is dropped.
        let room = max_data_samples.saturating_sub(*samples).min(buf.len() as u64) as usize;
        let n = drain(data, buf, if room == 0 { buf.len() } else { room });
        if n == 0 {
            break;
        }
        if room == 0 {
            continue;
        }
        samples_to_bytes(&buf[..n], bytes);
        if let Some(f) = file.as_mut() {
            if f.write_all(bytes).is_err() {
                break;
            }
        }
        *samples += n as u64;
    }
    if let Some(f) = file.as_mut() {
        patch_header(f, *samples, max_data_samples);
    }
}

fn seconds(samples: u64, sample_rate: u32, channels: usize) -> f64 {
    samples as f64 / (sample_rate.max(1) as f64 * channels.max(1) as f64)
}

/// Largest frame-aligned data size a 32-bit RIFF can address: the RIFF size
/// field holds `36 + data_size`, so data must stay ≤ `u32::MAX - 36` and be a
/// whole frame (block_align = channels * 4) or the header wraps and corrupts.
fn max_data_bytes(channels: usize) -> u64 {
    let block_align = (channels.max(1) as u64) * 4;
    (u32::MAX as u64 - 36) / block_align * block_align
}

fn begin_take(
    dir: &PathBuf,
    sample_rate: u32,
    channels: usize,
    base: &str,
    file: &mut Option<std::fs::File>,
    path: &mut Option<String>,
    samples: &mut u64,
    shared: &Arc<RecorderShared>,
) -> bool {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("record: cannot create {}: {e}", dir.display());
        set_status(shared, None, 0.0, true, Some(format!("cannot create recording dir: {e}")), false);
        shared.recording.store(false, Ordering::Relaxed);
        return false;
    }
    match unique_path(dir, base) {
        Ok((mut f, name)) => {
            if write_wav_header(&mut f, sample_rate, channels as u16, 0).is_ok() {
                *file = Some(f);
                *path = Some(name.clone());
                *samples = 0;
                set_status(shared, Some(name), 0.0, false, None, false);
                true
            } else {
                let _ = std::fs::remove_file(&name);
                eprintln!("record: cannot write header for {name}");
                set_status(shared, None, 0.0, true, Some(format!("cannot write header for {name}")), false);
                shared.recording.store(false, Ordering::Relaxed);
                false
            }
        }
        Err(e) => {
            eprintln!("record: cannot create recording: {e}");
            set_status(shared, None, 0.0, true, Some(format!("cannot create recording: {e}")), false);
            shared.recording.store(false, Ordering::Relaxed);
            false
        }
    }
}

/// Open `<base>.wav` for creation, retrying `<base>-2.wav`, `<base>-3.wav`, …
/// so two takes in the same second never overwrite each other (`create_new`).
fn unique_path(dir: &Path, base: &str) -> std::io::Result<(std::fs::File, String)> {
    let mut suffix = 0u32;
    loop {
        let name = if suffix == 0 { format!("{base}.wav") } else { format!("{base}-{}.wav", suffix + 1) };
        let p = dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => return Ok((f, p.to_string_lossy().into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(e) => return Err(e),
        }
    }
}

fn drain(consumer: &mut Consumer<f32>, buf: &mut [f32], max: usize) -> usize {
    let limit = buf.len().min(max);
    let mut n = 0;
    while n < limit {
        match consumer.pop() {
            Ok(s) => {
                buf[n] = s;
                n += 1;
            }
            Err(_) => break,
        }
    }
    n
}

fn drain_to_empty(consumer: &mut Consumer<f32>) {
    while consumer.pop().is_ok() {}
}

fn samples_to_bytes(samples: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 4);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

fn set_status(
    shared: &Arc<RecorderShared>,
    path: Option<String>,
    seconds: f64,
    finalized: bool,
    error: Option<String>,
    limit_reached: bool,
) {
    if let Ok(mut s) = shared.status.lock() {
        s.path = path;
        s.seconds = seconds;
        s.finalized = finalized;
        s.error = error;
        s.limit_reached = limit_reached;
        if finalized {
            s.takes += 1;
        }
    }
}

/// Patch the RIFF size (offset 4) and data size (offset 40) with the real
/// counts, then seek back to the end so appends continue. `samples` is the
/// total f32 sample count (4 bytes each), counted exactly (never per-frame
/// floored), so a mid-push drain can't under-count the header. `data_size` is
/// capped to the frame-aligned maximum so a >4 GiB take never wraps the header.
fn patch_header(file: &mut std::fs::File, samples: u64, max_data_samples: u64) {
    let data_size = (samples * 4).min(max_data_samples * 4) as u32;
    let _ = file.seek(SeekFrom::Start(4));
    let _ = file.write_all(&(36u32 + data_size).to_le_bytes());
    let _ = file.seek(SeekFrom::Start(40));
    let _ = file.write_all(&data_size.to_le_bytes());
    let _ = file.seek(SeekFrom::End(0));
    let _ = file.flush();
}

const WAV_HEADER_LEN: usize = 44;

/// Write a 32-bit-float (format 3) WAV header. `data_size` is the PCM byte
/// count; the writer passes 0 as a placeholder and patches it on stop.
fn write_wav_header(w: &mut impl Write, sample_rate: u32, channels: u16, data_size: u32) -> std::io::Result<()> {
    let block_align = channels * 4;
    let byte_rate = sample_rate * block_align as u32;
    let mut h = [0u8; WAV_HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32 + data_size).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    w.write_all(&h)
}

struct WavHeader {
    format: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits: u16,
    data_size: u32,
}

/// Parse a canonical 44-byte WAV header (used by tests to prove correctness).
fn parse_wav_header(bytes: &[u8]) -> Option<WavHeader> {
    if bytes.len() < WAV_HEADER_LEN {
        return None;
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" || &bytes[12..16] != b"fmt " {
        return None;
    }
    let u16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32 = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    Some(WavHeader {
        format: u16(20),
        channels: u16(22),
        sample_rate: u32(24),
        byte_rate: u32(28),
        block_align: u16(32),
        bits: u16(34),
        data_size: u32(40),
    })
}

/// Take names use LOCAL wall-clock time (the user reads them in a file
/// manager); a 16:31 take named 15:31 looked like someone else's file.
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs + local_utc_offset(secs));
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Seconds east of UTC for `secs` in the system time zone (0 if unknown).
fn local_utc_offset(secs: i64) -> i64 {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into `tm`; both pointers are valid.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok { tm.tm_gmtoff as i64 } else { 0 }
}

/// Civil (year, month, day, h, m, s) from a unix timestamp (Hinnant's algorithm).
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    (if mo <= 2 { y + 1 } else { y }, mo as u32, d as u32, h as u32, mi as u32, s as u32)
}

/// `record-test`: render a Keys→Surge→Master patch with real Surge XT while
/// recording, then read the WAV back and assert it is a non-silent, correct
/// stereo float capture that matches the rendered output sample-for-sample.
pub fn run_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::engine::GraphEngine;
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;

    const SR: f64 = 48000.0;
    const BLOCK: usize = 480; // 100 blocks per second

    let (voice, _main) = ClapVoice::new(SR)?;
    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(
        NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
        name_bytes("SYNTH"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, vec![(synth, voice)]);

    // Recorder in a temp dir (rule 37: tests never touch the user's home).
    let dir = std::env::temp_dir().join(format!("ksalat-record-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let (tap, shared) = crate::record::start(dir.clone(), SR as u32, 2);
    engine.set_recorder(tap);

    let mut buf = [0.0f32; BLOCK * 2];
    engine.render(&mut buf, 2); // Surge applies its init patch on the audio thread

    // Record: 1 block, ~2 s of a held chord, ~0.5 s tail, then stop.
    let mut reference: Vec<f32> = Vec::new();
    bus.send(Command::RecordStart);
    engine.render(&mut buf, 2);
    reference.extend_from_slice(&buf);
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOn { note: n, velocity: 0.9, source: SourceId::Keys });
    }
    for _ in 0..(2.0 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        reference.extend_from_slice(&buf);
    }
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOff { note: n, source: SourceId::Keys });
    }
    for _ in 0..(0.5 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        reference.extend_from_slice(&buf);
    }
    bus.send(Command::RecordStop);
    engine.render(&mut buf, 2); // recording already off; this block is not captured

    // Wait for the writer to finalize the header.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        let s = shared.snapshot();
        if s.finalized && !s.recording {
            break s;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "recorder did not finalize");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let path = status.path.clone().ok_or_else(|| anyhow::anyhow!("no recording path"))?;
    let bytes = std::fs::read(&path)?;
    let hdr = parse_wav_header(&bytes).ok_or_else(|| anyhow::anyhow!("bad WAV header"))?;
    anyhow::ensure!(hdr.format == 3, "format {} != 3 (IEEE float)", hdr.format);
    anyhow::ensure!(hdr.channels == 2, "channels {} != 2", hdr.channels);
    anyhow::ensure!(hdr.sample_rate == SR as u32, "sample rate {} != {}", hdr.sample_rate, SR);
    anyhow::ensure!(hdr.bits == 32, "bits {} != 32", hdr.bits);
    anyhow::ensure!(hdr.block_align == 8, "block align {} != 8", hdr.block_align);
    anyhow::ensure!(hdr.byte_rate == SR as u32 * 8, "byte rate {} != {}", hdr.byte_rate, SR as u32 * 8);

    let data_size = hdr.data_size as usize;
    anyhow::ensure!(bytes.len() == WAV_HEADER_LEN + data_size, "file length {} != header {}", bytes.len(), WAV_HEADER_LEN + data_size);
    anyhow::ensure!(data_size == reference.len() * 4, "data size {data_size} != {} rendered samples * 4", reference.len());

    let wav: Vec<f32> = bytes[WAV_HEADER_LEN..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    anyhow::ensure!(wav == reference, "WAV samples do not match the rendered output");

    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in &wav {
        peak = peak.max(s.abs());
        sum_sq += (s as f64) * (s as f64);
    }
    let rms = (sum_sq / wav.len().max(1) as f64).sqrt() as f32;
    anyhow::ensure!(peak > 0.01, "recording is silent (peak {peak})");
    anyhow::ensure!(status.dropped_frames == 0, "{} frames dropped (ring overran)", status.dropped_frames);
    anyhow::ensure!((status.seconds - 2.5).abs() < 0.02, "duration {} s, expected ~2.5", status.seconds);

    // Keep the file when asked (external ffprobe/soxi verification); otherwise
    // remove every trace (rule 37: no leftovers, and never under the user's home).
    if std::env::var("KABELSALAT_RECORD_KEEP").is_ok() {
        println!("record-test: kept {}", path);
    } else {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!(
        "record-test: OK — {} s stereo 32-bit float WAV matches the rendered output, peak {peak:.3}, RMS {rms:.4}, {} frames, {} dropped",
        status.seconds, wav.len() / 2, status.dropped_frames
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{command_bus, Command, Graph};
    use crate::engine::GraphEngine;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ksalat-record-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ramp(frames: usize, sign: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; frames * 2];
        for i in 0..frames {
            v[i * 2] = sign * ((i as f32 / frames as f32) - 0.5);
            v[i * 2 + 1] = -sign * ((i as f32 / frames as f32) - 0.5);
        }
        v
    }

    fn wav_samples(bytes: &[u8]) -> Vec<f32> {
        bytes[WAV_HEADER_LEN..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn read_take_file(dir: &Path) -> (PathBuf, Vec<u8>) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .collect();
        files.sort();
        let p = files.last().expect("a take file").clone();
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn wav_header_roundtrips() {
        let mut bytes = Vec::new();
        write_wav_header(&mut bytes, 48000, 2, 123456).unwrap();
        assert_eq!(bytes.len(), WAV_HEADER_LEN);
        let h = parse_wav_header(&bytes).unwrap();
        assert_eq!(h.format, 3, "IEEE float");
        assert_eq!(h.channels, 2);
        assert_eq!(h.sample_rate, 48000);
        assert_eq!(h.byte_rate, 48000 * 8);
        assert_eq!(h.block_align, 8);
        assert_eq!(h.bits, 32);
        assert_eq!(h.data_size, 123456);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
    }

    #[test]
    fn civil_from_unix_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-11-16 12:00:00 UTC = 1794830400
        assert_eq!(civil_from_unix(1_794_830_400), (2026, 11, 16, 12, 0, 0));
        // A leap-day boundary: 2000-03-01 00:00:00 UTC = 951868800
        assert_eq!(civil_from_unix(951_868_800), (2000, 3, 1, 0, 0, 0));
    }

    #[test]
    fn max_data_is_frame_aligned_and_under_riff_limit() {
        let m = max_data_bytes(2);
        assert_eq!(m % 8, 0, "stereo data size must be a whole frame (block_align 8)");
        assert!(m <= u32::MAX as u64 - 36, "RIFF size must not wrap");
        assert!(36 + m <= u32::MAX as u64);
    }

    #[test]
    fn one_free_slot_never_splits_a_frame() {
        // A 3-slot ring fits one whole frame (L+R) but not two; the second
        // frame must be dropped whole, never L-only (which would swap every
        // later frame and leave a non-frame-aligned stream).
        let (producer, mut consumer) = RingBuffer::<f32>::new(3);
        let (msg_tx, _msg_rx) = RingBuffer::<RecorderMsg>::new(4);
        let shared = Arc::new(RecorderShared::live());
        shared.recording.store(true, Ordering::Relaxed);
        let mut tap = RecorderTap { producer, msg_tx, shared: shared.clone() };
        tap.push_stereo(&[1.0, 2.0, 3.0, 4.0], 2);
        assert_eq!(shared.dropped_frames.load(Ordering::Relaxed), 1, "one whole frame dropped");
        let mut popped = 0;
        while consumer.pop().is_ok() {
            popped += 1;
        }
        assert_eq!(popped, 2, "the ring must hold whole frames only (L without R would be {popped})");
    }

    #[test]
    fn dropped_frames_counted_when_ring_full() {
        let (producer, _consumer) = RingBuffer::<f32>::new(4);
        let (msg_tx, _msg_rx) = RingBuffer::<RecorderMsg>::new(4);
        let shared = Arc::new(RecorderShared::live());
        shared.recording.store(true, Ordering::Relaxed);
        let mut tap = RecorderTap { producer, msg_tx, shared: shared.clone() };
        let data = [0.5f32, -0.5, 0.25, -0.25, 0.75, -0.75, 0.1, -0.1];
        tap.push_stereo(&data, 2);
        assert_eq!(shared.dropped_frames.load(Ordering::Relaxed), 2, "2 frames of 4 dropped");
        shared.recording.store(false, Ordering::Relaxed);
        tap.push_stereo(&data, 2);
        assert_eq!(shared.dropped_frames.load(Ordering::Relaxed), 2, "idle tap drops nothing");
    }

    #[test]
    fn begin_and_stop_are_idempotent() {
        let (producer, _consumer) = RingBuffer::<f32>::new(8);
        let (msg_tx, mut msg_rx) = RingBuffer::<RecorderMsg>::new(8);
        let shared = Arc::new(RecorderShared::live());
        let mut tap = RecorderTap { producer, msg_tx, shared: shared.clone() };
        tap.begin();
        tap.begin();
        assert_eq!(msg_rx.pop().ok(), Some(RecorderMsg::Start));
        assert!(msg_rx.pop().is_err(), "only one Start sent");
        tap.stop();
        tap.stop();
        assert_eq!(msg_rx.pop().ok(), Some(RecorderMsg::Stop));
        assert!(msg_rx.pop().is_err(), "only one Stop sent");
        assert!(!shared.is_recording());
    }

    #[test]
    fn same_second_takes_get_distinct_files_and_first_is_intact() {
        let dir = temp_dir("same-second");
        let base = "kabelsalat-20260101-000000";
        // Pre-create the first name with known content.
        std::fs::write(dir.join(format!("{base}.wav")), b"first take").unwrap();
        let (mut f2, name2) = unique_path(&dir, base).unwrap();
        assert_eq!(name2, dir.join(format!("{base}-2.wav")).to_string_lossy());
        write_wav_header(&mut f2, 48000, 2, 0).unwrap();
        drop(f2);
        // The original file is untouched; the new one is a distinct file.
        assert_eq!(std::fs::read(dir.join(format!("{base}.wav"))).unwrap(), b"first take");
        assert!(dir.join(format!("{base}-2.wav")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_writer_roundtrip() {
        let dir = temp_dir("roundtrip");
        let (mut tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 2);
        let sr = 48000u32;
        let frames = sr as usize / 4; // 0.25 s
        let data = ramp(frames, 1.0);
        tap.begin();
        tap.push_stereo(&data, 2);
        tap.stop();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "writer did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let path = status.path.expect("a path");
        let bytes = std::fs::read(&path).unwrap();
        let h = parse_wav_header(&bytes).unwrap();
        assert_eq!(h.data_size as usize, data.len() * 4);
        assert_eq!(status.seconds, frames as f64 / sr as f64);
        assert_eq!(status.dropped_frames, 0);
        assert_eq!(wav_samples(&bytes), data, "round trip is lossless");
        drop(tap);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_finalizes_the_take() {
        // Exit without a stop: dropping the tap (engine teardown) + shutting
        // the shared down must drain and finalize the header, not leave
        // RIFF=36 / data_size=0.
        let dir = temp_dir("drop");
        let (tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 2);
        let data = ramp(4800, 1.0); // 0.1 s
        {
            let mut tap = tap;
            tap.begin();
            tap.push_stereo(&data, 2);
        } // tap dropped: signals shutdown, but does not join
        shared.shutdown();
        let (_, bytes) = read_take_file(&dir);
        let h = parse_wav_header(&bytes).unwrap();
        assert_eq!(h.data_size as usize, data.len() * 4, "header finalized after drop");
        assert_eq!(bytes.len(), WAV_HEADER_LEN + data.len() * 4);
        assert_eq!(wav_samples(&bytes), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_is_patched_while_recording() {
        // Crash resilience: while a take is still open, the header is patched
        // ~every second, so a kill still leaves a playable (if slightly
        // truncated) file rather than a data_size=0 shell.
        let dir = temp_dir("patch");
        // Low rate so one "second" of stereo is small (2000 samples).
        let (mut tap, shared) = start_with_capacity(dir.clone(), 1000, 2, 20000);
        let data = ramp(2500, 1.0); // 1.25 s at 1 kHz
        tap.begin();
        tap.push_stereo(&data, 2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (path, patched) = loop {
            let s = shared.snapshot();
            if let Some(p) = &s.path {
                if let Ok(bytes) = std::fs::read(p) {
                    if let Some(h) = parse_wav_header(&bytes) {
                        if h.data_size > 0 {
                            break (p.clone(), h.data_size as usize);
                        }
                    }
                }
            }
            assert!(std::time::Instant::now() < deadline, "header never patched mid-take");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(patched > 0, "header must be patched mid-take (a kill still leaves a playable file)");
        assert!(patched <= data.len() * 4, "patched size {patched} must not exceed the {} samples pushed", data.len());
        drop(tap);
        shared.shutdown();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_stop_begin_second_take_stays_live() {
        // Regression (finding #6): begin→stop→begin where the writer runs the
        // first take's finalize AFTER the second begin. `recording` must stay
        // true (the writer's Stop path never clears it), so take 2 records.
        let dir = temp_dir("race");
        let (mut tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 2);
        let a = ramp(4800, 0.6);
        let b = ramp(4800, -0.7);
        tap.begin();
        tap.push_stereo(&a, 2);
        tap.stop();
        tap.begin(); // take 2, before the writer finalizes take 1
        // Wait until take 1 is finalized on disk; then push B. The old code
        // cleared `recording` here and silently dropped B (empty take 2).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if shared.snapshot().takes >= 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "take 1 never finalized");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tap.push_stereo(&b, 2);
        tap.stop();
        drop(tap);
        shared.shutdown();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .collect();
        files.sort();
        assert_eq!(files.len(), 2, "two distinct takes");
        let s0 = wav_samples(&std::fs::read(&files[0]).unwrap());
        let s1 = wav_samples(&std::fs::read(&files[1]).unwrap());
        assert!(
            (s0 == a && s1 == b) || (s0 == b && s1 == a),
            "take 1 and take 2 must each be intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_take_starts_at_song_beat_zero_not_count_in() {
        // Finding #8/#9: with record_sync on, the take begins the block the
        // song clock reaches beat 0 (after the count-in), not on the first
        // count-in click; a stop during count-in leaves nothing recording.
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480;
        let dir = temp_dir("sync");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
        engine.set_graph_pub(graph_pub.clone());
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 2);
        engine.set_recorder(tap);

        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetRecordSync { on: true });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = [0.0f32; BLOCK * 2];
        // 1 bar = 4 beats = 200 blocks of 480 frames @ 120 BPM.
        for _ in 0..199 {
            engine.render(&mut buf, 2);
        }
        assert!(!shared.is_recording(), "no recording during the count-in");
        assert!(graph_pub.lock().unwrap().transport.record_sync, "record_sync published");
        engine.render(&mut buf, 2); // transition block: song clock → beat 0
        assert!(!shared.is_recording(), "take starts the block after the transition");
        engine.render(&mut buf, 2); // first song block
        assert!(shared.is_recording(), "recording must start at song beat 0");
        for _ in 0..49 {
            engine.render(&mut buf, 2);
        }
        bus.send(Command::TransportStop);
        engine.render(&mut buf, 2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "sync take did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // 50 blocks = 0.5 s of song, NOT 2.5 s (count-in excluded).
        assert!((status.seconds - 0.5).abs() < 0.02, "take should be ~0.5 s of song, got {} s", status.seconds);
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_during_count_in_leaves_no_take() {
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480;
        let dir = temp_dir("sync-stop");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 2);
        engine.set_recorder(tap);
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetRecordSync { on: true });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = [0.0f32; BLOCK * 2];
        for _ in 0..100 {
            engine.render(&mut buf, 2); // halfway through the count-in
        }
        bus.send(Command::TransportStop);
        engine.render(&mut buf, 2);
        assert!(!shared.is_recording(), "stopping mid count-in must not leave a take running");
        assert_eq!(shared.snapshot().takes, 0, "no click-only file was started");
        drop(engine);
        shared.shutdown();
        let files = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .count();
        assert_eq!(files, 0, "no WAV file for a cancelled count-in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_turned_off_during_count_in_starts_no_take() {
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480;
        let dir = temp_dir("sync-off");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 2);
        engine.set_recorder(tap);
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetRecordSync { on: true });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = [0.0f32; BLOCK * 2];
        for _ in 0..100 {
            engine.render(&mut buf, 2); // halfway through the count-in
        }
        bus.send(Command::SetRecordSync { on: false });
        for _ in 0..200 {
            engine.render(&mut buf, 2); // past the count-in: the song plays
        }
        assert!(!shared.is_recording(), "SYNC switched off during the count-in must not start a take");
        assert_eq!(shared.snapshot().takes, 0);
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sum the recorder thread's user+system CPU ticks (utime/stime in
    /// /proc/self/task/<tid>/stat), for the busy-spin regression check.
    #[cfg(target_os = "linux")]
    fn recorder_cpu_ticks() -> Option<u64> {
        let tasks = std::fs::read_dir("/proc/self/task").ok()?;
        for entry in tasks.flatten() {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            // The 19-char name is truncated to 15 by the kernel.
            if !comm.trim().starts_with("kabelsalat-rec") {
                continue;
            }
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let rest = stat.rsplit_once(')')?.1; // skip "pid (comm)" — comm may contain spaces
            let f: Vec<&str> = rest.split_whitespace().collect();
            // utime = field 14 → index 11 (field 3 is index 0), stime = field 15.
            let utime: u64 = f.get(11)?.parse().ok()?;
            let stime: u64 = f.get(12)?.parse().ok()?;
            return Some(utime + stime);
        }
        None
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writer_thread_idles_instead_of_spinning() {
        // Finding #1: while recording with an empty ring, the writer must sleep,
        // not busy-spin. At HZ=100 a spin burns ~200 ticks over 2 s; a sleeping
        // loop a handful.
        let dir = temp_dir("cpu");
        let (mut tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 2);
        let data = ramp(480, 1.0); // 0.01 s burst
        tap.begin();
        tap.push_stereo(&data, 2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if shared.snapshot().seconds > 0.0 {
                break; // writer drained the burst; ring now empty, still recording
            }
            assert!(std::time::Instant::now() < deadline, "writer never drained the burst");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let before = recorder_cpu_ticks().expect("recorder thread present");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let after = recorder_cpu_ticks().expect("recorder thread present");
        let delta = after - before;
        assert!(delta < 50, "writer burned {delta} CPU ticks over 2 s of empty-ring recording (busy-spin)");
        tap.stop();
        drop(tap);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
