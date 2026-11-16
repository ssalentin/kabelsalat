//! Master recording (GitHub #6): tap the final master output into a WAV file.
//!
//! Same RT pattern as capture.rs/player.rs: the audio thread pushes interleaved
//! stereo f32 into a preallocated lock-free ring (never blocks; a full ring
//! drops frames and counts them); a writer thread drains the ring into a
//! 32-bit-float WAV under ~/Music/kabelsalat/ with a timestamped name, patching
//! the header when recording stops. The header is hand-written (no `hound`).
//!
//! Ownership: the engine holds the [`RecorderTap`] (ring producer + a channel
//! to the writer) and is the authority on start/stop; the UI/agents hold the
//! [`Arc<RecorderShared>`] to read live status. Per-rig stems are not captured
//! yet — the tap sits after the final master sum, not per-rig (documented in
//! PLAN §13).

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
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
}

/// State shared by the audio thread (tap), the writer thread, and the UI.
/// `recording` and `dropped_frames` are atomics (RT-safe read/write); `status`
/// is a mutex owned by the writer thread — the audio callback never touches it.
pub struct RecorderShared {
    recording: AtomicBool,
    dropped_frames: AtomicU64,
    status: Mutex<WavStatus>,
}

#[derive(Clone, Debug, Default)]
struct WavStatus {
    path: Option<String>,
    seconds: f64,
    finalized: bool,
}

impl RecorderShared {
    pub fn new() -> Self {
        Self {
            recording: AtomicBool::new(false),
            dropped_frames: AtomicU64::new(0),
            status: Mutex::new(WavStatus::default()),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// Assemble the current snapshot from the atomics + writer status.
    pub fn snapshot(&self) -> RecordStatus {
        let (path, seconds, finalized) = self
            .status
            .lock()
            .map(|s| (s.path.clone(), s.seconds, s.finalized))
            .unwrap_or((None, 0.0, false));
        RecordStatus {
            recording: self.recording.load(Ordering::Relaxed),
            path,
            seconds,
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            finalized,
        }
    }
}

/// Engine side: push the final output into the ring and signal the writer.
pub struct RecorderTap {
    producer: Producer<f32>,
    tx: Sender<RecorderMsg>,
    shared: Arc<RecorderShared>,
}

impl RecorderTap {
    /// Idempotent start: only the false→true transition messages the writer and
    /// resets the drop counter for the new take.
    pub fn begin(&self) {
        if !self.shared.recording.swap(true, Ordering::Relaxed) {
            self.shared.dropped_frames.store(0, Ordering::Relaxed);
            let _ = self.tx.send(RecorderMsg::Start);
        }
    }

    /// Idempotent stop: only the true→false transition tells the writer.
    pub fn stop(&self) {
        if self.shared.recording.swap(false, Ordering::Relaxed) {
            let _ = self.tx.send(RecorderMsg::Stop);
        }
    }

    /// Push one block of interleaved f32 as stereo (channels 0/1). RT-safe:
    /// never blocks; a frame that can't fit is dropped and counted.
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
            if self.producer.push(l).is_err() {
                dropped += 1; // ring full: drop the whole frame (R never pushed)
                continue;
            }
            if self.producer.push(r).is_err() {
                dropped += 1; // L landed, R lost — overrun glitch
            }
        }
        if dropped > 0 {
            self.shared.dropped_frames.fetch_add(dropped, Ordering::Relaxed);
        }
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
    let shared = Arc::new(RecorderShared::new());
    let (producer, consumer) = RingBuffer::new(cap.max(2));
    let (tx, rx) = mpsc::channel();
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("kabelsalat-recorder".into())
        .spawn(move || writer_loop(rx, consumer, dir, sample_rate, channels, thread_shared))
        .expect("spawn recorder thread");
    (RecorderTap { producer, tx, shared: shared.clone() }, shared)
}

fn writer_loop(
    rx: Receiver<RecorderMsg>,
    mut consumer: Consumer<f32>,
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
    loop {
        if file.is_some() {
            let n = drain(&mut consumer, &mut buf);
            if n > 0 {
                samples_to_bytes(&buf[..n], &mut bytes);
                let written = file.as_mut().map(|f| f.write_all(&bytes).is_ok()).unwrap_or(false);
                if !written {
                    // Disk error: abandon this take and report idle.
                    set_status(&shared, None, 0.0, true);
                    shared.recording.store(false, Ordering::Relaxed);
                    file = None;
                    path = None;
                    samples = 0;
                    continue;
                }
                samples += n as u64;
                set_status(&shared, path.clone(), seconds(samples, sample_rate, channels), false);
            }
            match rx.try_recv() {
                Ok(RecorderMsg::Stop) => {
                    finish(&mut consumer, &mut file, &mut samples, sample_rate, channels, &shared, &path, &mut buf, &mut bytes);
                    file = None;
                    path = None;
                    samples = 0;
                }
                Ok(RecorderMsg::Start) => {
                    // Unreachable via the engine's idempotent begin(); handled
                    // anyway: finalize the current take and begin a fresh one.
                    finish(&mut consumer, &mut file, &mut samples, sample_rate, channels, &shared, &path, &mut buf, &mut bytes);
                    file = None;
                    path = None;
                    samples = 0;
                    begin_take(&dir, sample_rate, channels, &mut file, &mut path, &mut samples, &shared);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    finish(&mut consumer, &mut file, &mut samples, sample_rate, channels, &shared, &path, &mut buf, &mut bytes);
                    break;
                }
            }
        } else {
            match rx.recv() {
                Ok(RecorderMsg::Start) => {
                    begin_take(&dir, sample_rate, channels, &mut file, &mut path, &mut samples, &shared);
                }
                Ok(RecorderMsg::Stop) => {} // idle no-op
                Err(_) => break,
            }
        }
    }
}

/// Drain the ring to empty, write it, patch the header, and publish the final
/// status. A Stop must not leave pushed-but-unwritten samples behind.
fn finish(
    consumer: &mut Consumer<f32>,
    file: &mut Option<std::fs::File>,
    samples: &mut u64,
    sample_rate: u32,
    channels: usize,
    shared: &Arc<RecorderShared>,
    path: &Option<String>,
    buf: &mut [f32],
    bytes: &mut Vec<u8>,
) {
    loop {
        let n = drain(consumer, buf);
        if n == 0 {
            break;
        }
        samples_to_bytes(&buf[..n], bytes);
        if let Some(f) = file.as_mut() {
            if f.write_all(bytes).is_err() {
                break;
            }
        }
        *samples += n as u64;
    }
    finalize(file, *samples);
    set_status(shared, path.clone(), seconds(*samples, sample_rate, channels), true);
    shared.recording.store(false, Ordering::Relaxed);
}

fn seconds(samples: u64, sample_rate: u32, channels: usize) -> f64 {
    samples as f64 / (sample_rate.max(1) as f64 * channels.max(1) as f64)
}

fn begin_take(
    dir: &PathBuf,
    sample_rate: u32,
    channels: usize,
    file: &mut Option<std::fs::File>,
    path: &mut Option<String>,
    samples: &mut u64,
    shared: &Arc<RecorderShared>,
) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("record: cannot create {}: {e}", dir.display());
        set_status(shared, None, 0.0, true);
        shared.recording.store(false, Ordering::Relaxed);
        return;
    }
    let p = dir.join(format!("kabelsalat-{}.wav", timestamp()));
    match std::fs::File::create(&p) {
        Ok(mut f) => {
            if write_wav_header(&mut f, sample_rate, channels as u16, 0).is_ok() {
                let name = p.to_string_lossy().into_owned();
                *file = Some(f);
                *path = Some(name.clone());
                *samples = 0;
                set_status(shared, Some(name), 0.0, false);
            } else {
                let _ = std::fs::remove_file(&p);
                eprintln!("record: cannot write header for {}", p.display());
                set_status(shared, None, 0.0, true);
                shared.recording.store(false, Ordering::Relaxed);
            }
        }
        Err(e) => {
            eprintln!("record: cannot create {}: {e}", p.display());
            set_status(shared, None, 0.0, true);
            shared.recording.store(false, Ordering::Relaxed);
        }
    }
}

fn drain(consumer: &mut Consumer<f32>, buf: &mut [f32]) -> usize {
    let mut n = 0;
    while n < buf.len() {
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

fn samples_to_bytes(samples: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 4);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

fn set_status(shared: &Arc<RecorderShared>, path: Option<String>, seconds: f64, finalized: bool) {
    if let Ok(mut s) = shared.status.lock() {
        s.path = path;
        s.seconds = seconds;
        s.finalized = finalized;
    }
}

/// Patch the RIFF size (offset 4) and data size (offset 40) with the real
/// counts. The streaming header was written with zeroes. `samples` is the
/// total f32 sample count (4 bytes each), counted exactly (never per-frame
/// floored), so a mid-push drain can't under-count the header.
fn finalize(file: &mut Option<std::fs::File>, samples: u64) {
    let Some(f) = file.as_mut() else { return };
    let data_size = (samples * 4).min(u32::MAX as u64) as u32;
    let _ = f.seek(SeekFrom::Start(4));
    let _ = f.write_all(&(36u32.wrapping_add(data_size)).to_le_bytes());
    let _ = f.seek(SeekFrom::Start(40));
    let _ = f.write_all(&data_size.to_le_bytes());
    let _ = f.flush();
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

fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
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
    fn dropped_frames_counted_when_ring_full() {
        // A 4-sample ring holds 2 stereo frames; pushing 4 frames with no
        // consumer drops the last 2 (per-frame, no interleaving glitch).
        let (producer, _consumer) = RingBuffer::<f32>::new(4);
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(RecorderShared::new());
        shared.recording.store(true, Ordering::Relaxed);
        let mut tap = RecorderTap { producer, tx, shared: shared.clone() };
        let data = [0.5f32, -0.5, 0.25, -0.25, 0.75, -0.75, 0.1, -0.1];
        tap.push_stereo(&data, 2);
        assert_eq!(shared.dropped_frames.load(Ordering::Relaxed), 2, "2 frames of 4 dropped");
        // Idle tap pushes nothing (and drops nothing).
        shared.recording.store(false, Ordering::Relaxed);
        tap.push_stereo(&data, 2);
        assert_eq!(shared.dropped_frames.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn begin_and_stop_are_idempotent() {
        let (producer, _consumer) = RingBuffer::<f32>::new(8);
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(RecorderShared::new());
        let tap = RecorderTap { producer, tx, shared: shared.clone() };
        tap.begin();
        tap.begin(); // second start is a no-op
        assert_eq!(rx.try_recv().ok(), Some(RecorderMsg::Start));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)), "only one Start sent");
        tap.stop();
        tap.stop(); // second stop is a no-op
        assert_eq!(rx.try_recv().ok(), Some(RecorderMsg::Stop));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)), "only one Stop sent");
        assert!(!shared.is_recording());
    }

    #[test]
    fn ring_writer_roundtrip() {
        // A tiny end-to-end: 2 s of a ramp through a 2-frame ring would drop
        // everything, so use the real ~4 s headroom and verify the written WAV
        // is byte-identical to what was pushed.
        let dir = std::env::temp_dir().join(format!("ksalat-record-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 2);
        let sr = 48000u32;
        let frames = sr as usize / 4; // 0.25 s
        let mut data = vec![0.0f32; frames * 2];
        for i in 0..frames {
            data[i * 2] = (i as f32 / frames as f32) - 0.5;
            data[i * 2 + 1] = 0.5 - (i as f32 / frames as f32);
        }
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
        let wav: Vec<f32> = bytes[WAV_HEADER_LEN..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        assert_eq!(wav, data, "round trip is lossless");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
