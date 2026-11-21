//! Master + per-rig stem recording (GitHub #6, AUM "Recording"): one take is
//! every armed target captured at once — the master mix and each armed rig's
//! post-fader sum, each into its own 32-bit-float WAV inside one take folder
//! under ~/Music/kabelsalat/.
//!
//! Same RT pattern as capture.rs/player.rs: the audio thread pushes interleaved
//! stereo f32 into preallocated lock-free rings (never blocks; a full ring
//! drops whole frames and counts them); ONE writer thread owns every file of
//! the take (a single decider names the folder, so all files of a take group
//! without any cross-thread naming race), patching each header on every
//! written second (crash-safe) and on stop. The header is hand-written (no
//! `hound`).
//!
//! Ownership: the engine holds the [`RecorderTap`] (the master ring producer,
//! one parked producer per armed rig's stem, and a control message ring) and
//! is the authority on start/stop — one `Start { mask }` names every target of
//! the take, so all files open (and all lengths stay sample-aligned) together.
//! The UI/agents hold the [`Arc<RecorderShared>`] to arm a rig's stem (the ring
//! is created and sized OFF the audio thread, before the arm command), to read
//! live status, and to shut the writer down on exit. The master ring is
//! preallocated at startup; a stem ring is preallocated when its rig is first
//! armed ([`RecorderShared::arm_stem`]) and parked after that (re-arm reuses it).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{SystemTime, UNIX_EPOCH};

use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;

use crate::control::MAX_RIGS;

/// The live app's recorder, published once at startup so the control bridge can
/// answer `record_status` without threading an Arc through every signature.
pub static LIVE_RECORDER: OnceLock<Arc<RecorderShared>> = OnceLock::new();

/// One take's targets, as a bitmask: bit `MAX_RIGS` = the master mix, the bits
/// below it the rig stems (index = rig). Set by the engine from its arm tables
/// when the take begins, frozen for the take's length (an arm change mid-take
/// only applies to the NEXT take, so every file of the take stays
/// sample-aligned).
pub const MASK_MASTER: u64 = 1 << MAX_RIGS;
const _: () = assert!(MAX_RIGS < 64, "the take mask holds every rig and the master");

fn slot(rig: u8) -> usize {
    (rig as usize).min(MAX_RIGS - 1)
}

/// Mask bit of rig `rig`'s stem.
pub fn mask_rig(rig: u8) -> u64 {
    1 << slot(rig)
}

#[derive(Debug, PartialEq)]
enum RecorderMsg {
    /// Begin a take of every target in `mask`; the writer opens one WAV per
    /// target inside a fresh timestamped take folder.
    Start { mask: u64 },
    Stop,
}

/// A rig stem's engine-side half, delivered through the engine's stem channel:
/// the audio thread pops it at a block boundary and parks it in the tap.
pub struct StemWire {
    pub rig: u8,
    pub ring: Producer<f32>,
}

/// Per-stem file status (assembled by [`RecorderShared::snapshot`]).
#[derive(Clone, Debug, Default, Serialize)]
pub struct StemStatus {
    /// Rig index the stem captures (0-based, like every tool's `rig`).
    pub rig: u8,
    /// The rig's display name — the file name without ".wav".
    pub name: String,
    pub path: Option<String>,
    pub seconds: f64,
    pub dropped_frames: u64,
}

/// UI/agent-facing recording snapshot (serialized by `record_status`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct RecordStatus {
    pub recording: bool,
    /// A take is armed but waiting: bar-quantized start/stop while the
    /// transport rolls, or a synced take waiting for song beat 0.
    pub pending: bool,
    /// The primary file of the last/active take (the master WAV when the
    /// master is armed, else the first stem).
    pub path: Option<String>,
    /// The take folder every WAV of the last/active take sits in.
    pub folder: Option<String>,
    pub seconds: f64,
    /// Whole frames the rings could not hold (master + stems).
    pub dropped_frames: u64,
    pub finalized: bool,
    /// Last start/stop failure (dir/file creation, write error), if any.
    pub error: Option<String>,
    /// The last take stopped at the 4 GiB WAV cap instead of on request.
    pub limit_reached: bool,
    /// Monotonic count of finalized takes (never resets, unlike `finalized`).
    pub takes: u64,
    /// One entry per stem of the last/active take, in rig order.
    pub stems: Vec<StemStatus>,
    /// What the next take records: the master's arm and one flag per rig.
    pub armed_master: bool,
    pub armed_rigs: Vec<bool>,
    pub retro: RetroStatus,
}

/// Retrospective recording (#215): what `save_last` can save.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RetroStatus {
    /// How far back a save reaches, as set (0 = off).
    pub length_seconds: u32,
    /// What a save would hold now.
    pub kept_seconds: f64,
    pub saving: bool,
    /// The take folder of the last save.
    pub last_folder: Option<String>,
}

/// State shared by the audio thread (tap), the writer thread, and the UI.
/// `live`, `recording`, `pending`, `arm_*`, `shutdown` and the drop counters
/// are atomics (RT-safe); `status` is a mutex owned by the writer thread — the
/// audio callback never touches it. `stems` holds stem ring consumers waiting
/// to be adopted by the writer; `rig_names` is the display-name list the
/// writer names stem files from. `join` is the writer's JoinHandle, joined
/// once on shutdown.
pub struct RecorderShared {
    live: AtomicBool,
    recording: AtomicBool,
    pending: AtomicBool,
    shutdown: AtomicBool,
    dropped_frames: AtomicU64,
    stem_dropped: [AtomicU64; MAX_RIGS],
    arm_master: AtomicBool,
    arm_rigs: [AtomicBool; MAX_RIGS],
    status: Mutex<WavStatus>,
    writer: OnceLock<Thread>,
    join: Mutex<Option<JoinHandle<()>>>,
    /// Stem ring consumers created by `arm_stem`; the writer adopts (moves)
    /// them into its own table on its next wake.
    stems: Mutex<[Option<Consumer<f32>>; MAX_RIGS]>,
    /// Which rigs already have an allocated stem ring (so re-arming reuses it).
    stem_allocated: [AtomicBool; MAX_RIGS],
    /// Channel to deliver newly-allocated stem producers to the audio thread.
    stem_tx: Mutex<Option<Producer<StemWire>>>,
    /// Rig display names, one per rig, newest last; stem files are named
    /// `<rig name>.wav`. Set by whoever loads/renames rigs (never the audio
    /// thread); falls back to `rig-N` when a rig has no name.
    rig_names: Mutex<Vec<String>>,
    sample_rate: u32,
    retro_clock: Arc<crate::retro::Clock>,
    /// The retrospective length the writer keeps (seconds, 0 = off).
    retro_secs: AtomicU32,
    /// Frames a save asked for (0 = none, `u64::MAX` = everything kept).
    retro_request: AtomicU64,
    /// Saves asked for (counted after their request is stored), and the
    /// count the writer had seen before it last took a request: a save asked
    /// during another keeps `saving` on until its own take is written.
    retro_asked: AtomicU64,
    retro_done: AtomicU64,
}

#[derive(Clone, Debug, Default)]
struct WavStatus {
    folder: Option<String>,
    path: Option<String>,
    seconds: f64,
    finalized: bool,
    error: Option<String>,
    limit_reached: bool,
    takes: u64,
    /// Per-stem file state of the last/active take (writer-owned).
    stems: Vec<StemFileStatus>,
    retro_last: Option<String>,
}

/// Writer-side per-stem state (`dropped_frames` lives in the shared atomics).
#[derive(Clone, Debug, Default)]
struct StemFileStatus {
    rig: u8,
    path: Option<String>,
    seconds: f64,
}

impl RecorderShared {
    pub fn new() -> Self {
        Self::with_live(false, 48_000, Arc::new(crate::retro::Clock::new()))
    }

    #[cfg(test)]
    fn live(sample_rate: u32) -> Self {
        Self::with_live(true, sample_rate, Arc::new(crate::retro::Clock::new()))
    }

    fn with_live(live: bool, sample_rate: u32, retro_clock: Arc<crate::retro::Clock>) -> Self {
        Self {
            live: AtomicBool::new(live),
            recording: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            dropped_frames: AtomicU64::new(0),
            stem_dropped: std::array::from_fn(|_| AtomicU64::new(0)),
            arm_master: AtomicBool::new(true),
            arm_rigs: std::array::from_fn(|_| AtomicBool::new(false)),
            status: Mutex::new(WavStatus::default()),
            writer: OnceLock::new(),
            join: Mutex::new(None),
            stems: Mutex::new(std::array::from_fn(|_| None)),
            stem_allocated: std::array::from_fn(|_| AtomicBool::new(false)),
            stem_tx: Mutex::new(None),
            rig_names: Mutex::new(Vec::new()),
            sample_rate,
            retro_clock,
            retro_secs: AtomicU32::new(0),
            retro_request: AtomicU64::new(0),
            retro_asked: AtomicU64::new(0),
            retro_done: AtomicU64::new(0),
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

    /// Is a take armed and waiting for its bar / beat 0?
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    fn unpark_writer(&self) {
        if let Some(t) = self.writer.get() {
            t.unpark();
        }
    }

    /// Engine side: the arm tables the engine holds authoritatively, mirrored
    /// here so `record_status` (UI, bridge, MCP) reports what the next take
    /// records without locking the engine's publication.
    pub fn set_arms(&self, master: bool, rigs: &[bool; MAX_RIGS]) {
        self.arm_master.store(master, Ordering::Relaxed);
        for (a, r) in self.arm_rigs.iter().zip(rigs.iter()) {
            a.store(*r, Ordering::Relaxed);
        }
        self.unpark_writer(); // the retrospective lanes follow the rig arms
    }

    fn armed_rig_mask(&self) -> u64 {
        self.arm_rigs.iter().enumerate().fold(0, |m, (r, a)| if a.load(Ordering::Relaxed) { m | 1 << r } else { m })
    }

    /// Keep the last `secs` of the master and of every armed rig's stem
    /// (0 = off). The writer allocates the ring; a new length starts it over.
    pub fn set_retro_seconds(&self, secs: u32) {
        self.retro_secs.store(secs, Ordering::Relaxed);
        self.unpark_writer();
    }

    /// Save the last `secs` (None = everything kept) of the master and the
    /// armed stems as a new take, written by the writer thread. Returns the
    /// seconds the take will hold; a refusal also becomes the status error,
    /// which the top bar shows beside REC.
    pub fn save_last(&self, secs: Option<f64>) -> Result<f64, String> {
        let (want, kept) = self.retro_request_of(secs).inspect_err(|e| {
            if let Ok(mut s) = self.status.lock() {
                s.error = Some(e.clone());
            }
        })?;
        self.retro_request.store(want, Ordering::Release);
        self.retro_asked.fetch_add(1, Ordering::Release);
        self.unpark_writer();
        Ok(kept.min(want) as f64 / self.sample_rate.max(1) as f64)
    }

    /// (frames asked for, frames kept) of a save that can run.
    fn retro_request_of(&self, secs: Option<f64>) -> Result<(u64, u64), String> {
        if !self.is_live() {
            return Err("recording unavailable: no audio engine".into());
        }
        if self.is_recording() {
            return Err("REC is recording: stop it first (its take has this audio already)".into());
        }
        if self.retro_secs.load(Ordering::Relaxed) == 0 {
            return Err("retrospective recording is off (Settings → Audio → Keep the last)".into());
        }
        let sr = self.sample_rate.max(1) as f64;
        let want = match secs {
            Some(s) if s.is_finite() && s > 0.0 => (s * sr).round().max(1.0) as u64,
            Some(s) => return Err(format!("seconds must be more than 0, not {s}")),
            None => u64::MAX,
        };
        if !self.arm_master.load(Ordering::Relaxed) && self.armed_rig_mask() == 0 {
            return Err("nothing is armed: arm the master or a rig".into());
        }
        let kept = self.retro_clock.kept_frames();
        if kept == 0 {
            return Err("nothing has been played since retrospective recording started".into());
        }
        Ok((want, kept))
    }

    /// Display names of the rig list, used for stem file names. Callers hold
    /// the rig list already (loads, renames) — never the audio thread.
    pub fn set_rig_names(&self, names: Vec<String>) {
        if let Ok(mut n) = self.rig_names.lock() {
            *n = names;
        }
        self.unpark_writer(); // a parked writer may owe a take file a name
    }

    /// Preallocate (OFF the audio thread) the stereo ring for rig `rig`'s stem
    /// and queue its consumer for the writer; returns the producer the ENGINE
    /// side holds. Re-arming reuses the parked ring (returns None) — a ring is
    /// created once per rig and stays parked while unarmed. None when this
    /// recorder has no writer (attach mode, tests).
    pub fn arm_stem(&self, rig: u8) -> Option<Producer<f32>> {
        let rig = slot(rig);
        if !self.is_live() {
            return None;
        }
        if self.stem_allocated[rig].swap(true, Ordering::AcqRel) {
            return None;
        }
        let cap = (self.sample_rate as usize).max(8000) * 2 * 4; // ~4 s stereo
        let (producer, consumer) = RingBuffer::new(cap);
        if let Ok(mut stems) = self.stems.lock() {
            stems[rig] = Some(consumer);
        } else {
            self.stem_allocated[rig].store(false, Ordering::Release);
            return None;
        }
        self.unpark_writer();
        Some(producer)
    }

    /// Ensure rig `rig`'s stem ring is preallocated (off the audio thread) and
    /// queued for delivery to the engine's `RecorderTap`. Idempotent.
    pub fn ensure_stem(&self, rig: u8) {
        if let Some(producer) = self.arm_stem(rig) {
            if let Ok(mut tx) = self.stem_tx.lock() {
                if let Some(tx) = tx.as_mut() {
                    let _ = tx.push(StemWire { rig: slot(rig) as u8, ring: producer });
                }
            }
        }
    }

    /// Stop any take in flight, wake the writer, and join it so the WAVs are
    /// drained and their headers finalized before the process exits.
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
        // Before the status: a save the writer finished shows its folder.
        let saving = self.retro_asked.load(Ordering::Acquire) > self.retro_done.load(Ordering::Acquire);
        let (folder, path, seconds, finalized, error, limit_reached, takes, files, retro_last) = self
            .status
            .lock()
            .map(|s| (
                s.folder.clone(), s.path.clone(), s.seconds, s.finalized,
                s.error.clone(), s.limit_reached, s.takes, s.stems.clone(), s.retro_last.clone(),
            ))
            .unwrap_or((None, None, 0.0, false, None, false, 0, Vec::new(), None));
        let names = self.rig_names.lock().map(|n| n.clone()).unwrap_or_default();
        let stems: Vec<StemStatus> = files
            .iter()
            .map(|f| StemStatus {
                rig: f.rig,
                name: names.get(f.rig as usize).cloned()
                    .unwrap_or_else(|| format!("rig-{}", f.rig + 1)),
                path: f.path.clone(),
                seconds: f.seconds,
                dropped_frames: self.stem_dropped[f.rig as usize].load(Ordering::Relaxed),
            })
            .collect();
        let stem_drops: u64 = self.stem_dropped.iter().map(|d| d.load(Ordering::Relaxed)).sum();
        RecordStatus {
            recording: self.recording.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Relaxed),
            path,
            folder,
            seconds,
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed) + stem_drops,
            finalized,
            error,
            limit_reached,
            takes,
            stems,
            armed_master: self.arm_master.load(Ordering::Relaxed),
            armed_rigs: self.arm_rigs.iter().map(|a| a.load(Ordering::Relaxed)).collect(),
            retro: RetroStatus {
                length_seconds: self.retro_secs.load(Ordering::Relaxed),
                kept_seconds: self.retro_clock.kept_frames() as f64 / self.sample_rate.max(1) as f64,
                saving,
                last_folder: retro_last,
            },
        }
    }
}

/// Engine side: push the final output (and each armed rig's post-fader sum)
/// into the take's rings and signal the writer.
pub struct RecorderTap {
    producer: Producer<f32>,
    /// One parked producer per rig stem (delivered through the engine's stem
    /// channel before the arm command applies — see `RecorderShared::arm_stem`).
    stems: [Option<Producer<f32>>; MAX_RIGS],
    stem_rx: Option<Consumer<StemWire>>,
    active_mask: u64,
    msg_tx: Producer<RecorderMsg>,
    shared: Arc<RecorderShared>,
    retro: crate::retro::Feed,
}

impl RecorderTap {
    /// Idempotent take begin: only the false→true transition messages the
    /// writer (with the take's target mask) and resets the drop counters.
    pub fn begin(&mut self, mask: u64) {
        if !self.shared.recording.swap(true, Ordering::Relaxed) {
            self.active_mask = mask;
            self.shared.dropped_frames.store(0, Ordering::Relaxed);
            for d in &self.shared.stem_dropped {
                d.store(0, Ordering::Relaxed);
            }
            let _ = self.msg_tx.push(RecorderMsg::Start { mask });
            self.shared.unpark_writer();
        }
    }

    /// Idempotent stop: only the true→false transition tells the writer.
    pub fn stop(&mut self) {
        if self.shared.recording.swap(false, Ordering::Relaxed) {
            self.active_mask = 0;
            let _ = self.msg_tx.push(RecorderMsg::Stop);
            self.shared.unpark_writer();
        }
    }

    /// Pop any newly-allocated stem producers sent by `RecorderShared::ensure_stem`.
    /// Allocation- and lock-free (RT-safe).
    pub fn drain_stems(&mut self) {
        while let Some(w) = self.stem_rx.as_mut().and_then(|rx| rx.pop().ok()) {
            self.attach_stem(w.rig, w.ring);
        }
    }

    /// True when the active take is capturing rig `rig`'s stem.
    pub fn is_stem_active(&self, rig: u8) -> bool {
        self.shared.recording.load(Ordering::Relaxed)
            && (self.active_mask & mask_rig(rig) != 0)
            && self.stems[slot(rig)].is_some()
    }

    pub fn shared(&self) -> &Arc<RecorderShared> {
        &self.shared
    }

    /// A take is armed but has not begun (bar-quantized or synced): the REC
    /// chip shows the waiting state.
    pub fn set_pending(&self, on: bool) {
        self.shared.pending.store(on, Ordering::Relaxed);
    }

    /// Park a rig's stem producer (engine side of `RecorderShared::arm_stem`).
    pub fn attach_stem(&mut self, rig: u8, ring: Producer<f32>) {
        self.stems[slot(rig)] = Some(ring);
    }

    /// Does the tap hold rig `rig`'s stem ring? The engine's take mask only
    /// arms rigs whose ring is parked here, so a take never opens a stem file
    /// nothing feeds.
    pub fn stem_ready(&self, rig: u8) -> bool {
        self.stems[slot(rig)].is_some()
    }

    /// Push one block of interleaved f32 as stereo (channels 0/1) into the
    /// MASTER ring. RT-safe: never blocks; a frame that can't fit whole is
    /// dropped and counted, so a frame is never split (an L without its R
    /// would swap every later frame).
    pub fn push_stereo(&mut self, data: &[f32], ch: usize) {
        if !self.shared.recording.load(Ordering::Relaxed) || (self.active_mask & MASK_MASTER == 0) {
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

    /// The rack's rigs changed: every retrospective stem starts over. RT-safe.
    pub fn retro_restart_stems(&self) {
        self.retro.restart_stems();
    }

    /// Rig `rig`'s post-fader sum into its retrospective lane. RT-safe.
    pub fn retro_rig(&mut self, rig: u8, data: &[f32]) {
        self.retro.rig(rig, data);
    }

    /// The block's final output into the retrospective ring, once per block
    /// after every `retro_rig`. RT-safe; wakes the writer only when the ring
    /// gave a lane set back for it to free.
    pub fn retro_block(&mut self, data: &[f32], ch: usize) {
        self.retro.end_block(data, ch);
        if self.retro.take_swapped() {
            self.shared.unpark_writer();
        }
    }

    /// Push one block of interleaved stereo into rig `rig`'s stem ring. The
    /// buffer is the rig's post-fader sum (exactly what its MasterOut feeds
    /// the physical mix). A rig whose ring was never delivered counts the
    /// block as dropped, never silently misaligns the file.
    pub fn push_rig_stereo(&mut self, rig: u8, data: &[f32]) {
        if !self.shared.recording.load(Ordering::Relaxed) || (self.active_mask & mask_rig(rig) == 0) {
            return;
        }
        let frames = data.len() / 2;
        let Some(ring) = &mut self.stems[slot(rig)] else {
            self.shared.stem_dropped[slot(rig)].fetch_add(frames as u64, Ordering::Relaxed);
            return;
        };
        let mut dropped = 0u64;
        for i in 0..frames {
            if ring.slots() >= 2 {
                let _ = ring.push(data[i * 2]);
                let _ = ring.push(data[i * 2 + 1]);
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.shared.stem_dropped[slot(rig)].fetch_add(dropped, Ordering::Relaxed);
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

const RETRO_MINUTES_FIELD: &str = "retro_minutes";

/// The retrospective length the user chose, in minutes (0 = off). Unset, it
/// is 2 minutes, or off on a machine with under 3 GiB of memory.
pub fn retro_minutes_setting() -> u32 {
    let max = crate::retro::MINUTES_CHOICES.iter().copied().max().unwrap_or(0) as u64;
    crate::assistant::load_config_value(RETRO_MINUTES_FIELD)
        .and_then(|v| v.as_u64())
        .map(|m| m.min(max) as u32)
        .unwrap_or_else(|| {
            let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
            default_retro_minutes(mem_total_kib(&meminfo))
        })
}

pub fn save_retro_minutes(minutes: u32) -> std::io::Result<()> {
    crate::assistant::save_config_value(RETRO_MINUTES_FIELD, serde_json::json!(minutes))
}

fn default_retro_minutes(mem_kib: Option<u64>) -> u32 {
    if mem_kib.is_some_and(|k| k >= 3 << 20) { 2 } else { 0 }
}

fn mem_total_kib(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    line.trim().strip_suffix("kB")?.trim().parse().ok()
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
    let (retro, keeper, retro_clock) = crate::retro::channel(sample_rate);
    let shared = Arc::new(RecorderShared::with_live(true, sample_rate, retro_clock));
    let (producer, consumer) = RingBuffer::new(cap.max(2));
    let (msg_tx, msg_rx) = RingBuffer::new(64);
    // Each rig delivers its stem ring once per recorder: arming every rig
    // before the engine's next block never finds this channel full.
    let (stem_tx, stem_rx) = RingBuffer::new(MAX_RIGS);
    if let Ok(mut tx) = shared.stem_tx.lock() {
        *tx = Some(stem_tx);
    }
    let thread_shared = shared.clone();
    let handle = std::thread::Builder::new()
        .name("kabelsalat-recorder".into())
        .spawn(move || writer_loop(msg_rx, consumer, keeper, dir, sample_rate, channels, thread_shared))
        .expect("spawn recorder thread");
    // Known before anyone can begin/stop: an unpark is never lost to a
    // writer that has not registered itself yet.
    let _ = shared.writer.set(handle.thread().clone());
    if let Ok(mut j) = shared.join.lock() {
        *j = Some(handle);
    }
    (
        RecorderTap {
            producer,
            stems: std::array::from_fn(|_| None),
            stem_rx: Some(stem_rx),
            active_mask: 0,
            msg_tx,
            shared: shared.clone(),
            retro,
        },
        shared,
    )
}

/// Which target one open WAV of a take captures.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Master,
    Rig(u8),
}

/// One open WAV of the running take: its target, file, exact sample count
/// (rule 45) and the next crash-safe header patch point.
struct TakeFile {
    kind: TargetKind,
    file: std::fs::File,
    path: String,
    samples: u64,
    next_patch: u64,
}

/// The running take: its folder and one open file per armed target.
struct Take {
    folder: String,
    files: Vec<TakeFile>,
}

/// What the status shows for a take (the primary file is the master WAV when
/// the master is armed, else the first stem).
#[derive(Clone, Default)]
struct TakeStatusView {
    folder: Option<String>,
    path: Option<String>,
    seconds: f64,
    stems: Vec<StemFileStatus>,
}

fn take_view(t: &Take, sample_rate: u32) -> TakeStatusView {
    let seconds_of = |f: &TakeFile| seconds(f.samples, sample_rate, 2);
    let primary = t.files.iter().find(|f| f.kind == TargetKind::Master).or_else(|| t.files.first());
    TakeStatusView {
        folder: Some(t.folder.clone()),
        path: primary.map(|f| f.path.clone()),
        seconds: primary.map(seconds_of).unwrap_or(0.0),
        stems: t
            .files
            .iter()
            .filter_map(|f| match f.kind {
                TargetKind::Rig(r) => Some(StemFileStatus {
                    rig: r,
                    path: Some(f.path.clone()),
                    seconds: seconds_of(f),
                }),
                TargetKind::Master => None,
            })
            .collect(),
    }
}

fn writer_loop(
    mut msg: Consumer<RecorderMsg>,
    mut data: Consumer<f32>,
    mut keeper: crate::retro::Keeper,
    dir: PathBuf,
    sample_rate: u32,
    channels: usize,
    shared: Arc<RecorderShared>,
) {
    // Stem ring consumers, adopted from `shared.stems` (never locked again on
    // the hot path; the writer owns them once adopted).
    let mut stems: [Option<Consumer<f32>>; MAX_RIGS] = std::array::from_fn(|_| None);
    let mut take: Option<Take> = None;
    let mut buf = vec![0.0f32; 4096];
    let mut bytes = Vec::with_capacity(4096 * 4);
    // Every target file is stereo f32.
    let max_data_samples = max_data_bytes(2) / 4;
    let patch_every = (sample_rate as u64).saturating_mul(2).max(1); // ~1 s stereo
    loop {
        // Adopt stem rings armed since the last wake — BEFORE the messages,
        // so a Start that masks a freshly armed rig finds its ring.
        if let Ok(mut pending) = shared.stems.lock() {
            for (rig, slot) in pending.iter_mut().enumerate() {
                if let Some(c) = slot.take() {
                    stems[rig] = Some(c);
                }
            }
        }
        // Allocating lanes and writing a save take this thread for as long as
        // they need, and a running take's rings hold 4 s: both wait for it.
        keeper.collect();
        let asked = shared.retro_asked.load(Ordering::Acquire);
        let want = shared.retro_request.swap(0, Ordering::AcqRel);
        if take.is_none() {
            keeper.reconcile(shared.retro_secs.load(Ordering::Relaxed), shared.armed_rig_mask());
            if want > 0 {
                save_retro(&keeper, want, &dir, sample_rate, &shared);
            }
        } else if want > 0 {
            eprintln!("record: retrospective save refused while a take records");
        }
        shared.retro_done.store(asked, Ordering::Release);
        // Control messages next (in order), so a Start/Stop raced by teardown
        // is still honored before the shutdown check below ends the loop.
        while let Ok(m) = msg.pop() {
            match m {
                RecorderMsg::Start { mask } => {
                    // Unreachable via the idempotent begin(); handled anyway.
                    if let Some(mut t) = take.take() {
                        finalize_take(&mut t, &mut data, &mut stems, &mut buf, &mut bytes, max_data_samples);
                        set_status(&shared, take_view(&t, sample_rate), true, None, false);
                    }
                    match begin_take(&dir, sample_rate, channels, mask, &stems, &shared) {
                        Some(t) => take = Some(t),
                        None => {
                            shared.recording.store(false, Ordering::Relaxed);
                        }
                    }
                }
                RecorderMsg::Stop => {
                    if let Some(mut t) = take.take() {
                        finalize_take(&mut t, &mut data, &mut stems, &mut buf, &mut bytes, max_data_samples);
                        set_status(&shared, take_view(&t, sample_rate), true, None, false);
                    }
                }
            }
        }
        if shared.shutdown.load(Ordering::Acquire) || msg.is_abandoned() {
            if let Some(mut t) = take.take() {
                finalize_take(&mut t, &mut data, &mut stems, &mut buf, &mut bytes, max_data_samples);
                set_status(&shared, take_view(&t, sample_rate), true, None, false);
            }
            break;
        }
        if take.is_some() {
            let mut wrote = 0usize;
            let mut failed = false;
            let mut capped = false;
            {
                let t = take.as_mut().expect("checked above");
                for tf in t.files.iter_mut() {
                    let ring: &mut Consumer<f32> = match tf.kind {
                        TargetKind::Master => &mut data,
                        TargetKind::Rig(r) => match stems[r as usize].as_mut() {
                            Some(c) => c,
                            None => continue, // no ring adopted: nothing to drain
                        },
                    };
                    // Never past the frame-aligned RIFF limit; the rest is
                    // dropped when finalizing.
                    let room = max_data_samples.saturating_sub(tf.samples).min(buf.len() as u64) as usize;
                    if room == 0 {
                        continue;
                    }
                    let n = drain(ring, &mut buf, room);
                    if n == 0 {
                        continue;
                    }
                    samples_to_bytes(&buf[..n], &mut bytes);
                    if tf.file.write_all(&bytes).is_err() {
                        failed = true;
                        break;
                    }
                    tf.samples += n as u64;
                    wrote += n;
                    if tf.samples >= tf.next_patch {
                        patch_header(&mut tf.file, tf.samples, max_data_samples);
                        tf.next_patch = tf.samples + patch_every;
                    }
                    if tf.samples >= max_data_samples {
                        capped = true;
                    }
                }
            }
            if failed {
                // Mid-take write error: finalize best-effort and drain the
                // rings so stale samples don't leak into the next take.
                if let Some(mut t) = take.take() {
                    finalize_take(&mut t, &mut data, &mut stems, &mut buf, &mut bytes, max_data_samples);
                }
                drain_to_empty(&mut data);
                for c in stems.iter_mut().flatten() {
                    drain_to_empty(c);
                }
                shared.recording.store(false, Ordering::Relaxed);
                set_status(&shared, TakeStatusView::default(), true, Some("write failed".into()), false);
                continue;
            }
            // Any target at the RIFF cap ends the whole take, so every file
            // of the take keeps the same length.
            if capped {
                if let Some(mut t) = take.take() {
                    finalize_take(&mut t, &mut data, &mut stems, &mut buf, &mut bytes, max_data_samples);
                    set_status(&shared, take_view(&t, sample_rate), true, None, true);
                }
                shared.recording.store(false, Ordering::Relaxed);
                continue;
            }
            if wrote > 0 {
                let view = take_view(take.as_ref().expect("checked above"), sample_rate);
                set_status(&shared, view, false, None, false);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else {
            std::thread::park();
        }
    }
}

/// Drain every target's ring to empty and patch each header. A Stop must not
/// leave pushed-but-unwritten samples behind.
fn finalize_take(
    t: &mut Take,
    data: &mut Consumer<f32>,
    stems: &mut [Option<Consumer<f32>>; MAX_RIGS],
    buf: &mut [f32],
    bytes: &mut Vec<u8>,
    max_data_samples: u64,
) {
    for tf in t.files.iter_mut() {
        let ring: &mut Consumer<f32> = match tf.kind {
            TargetKind::Master => data,
            TargetKind::Rig(r) => match stems[r as usize].as_mut() {
                Some(c) => c,
                None => continue, // no ring adopted: nothing to drain
            },
        };
        loop {
            // Never past the frame-aligned RIFF limit; the rest is dropped.
            let room = max_data_samples.saturating_sub(tf.samples).min(buf.len() as u64) as usize;
            let n = drain(ring, buf, if room == 0 { buf.len() } else { room });
            if n == 0 {
                break;
            }
            if room == 0 {
                continue;
            }
            samples_to_bytes(&buf[..n], bytes);
            if tf.file.write_all(bytes).is_err() {
                break;
            }
            tf.samples += n as u64;
        }
        patch_header(&mut tf.file, tf.samples, max_data_samples);
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

/// Open one WAV per masked target inside a fresh take folder
/// `<dir>/kabelsalat-<local timestamp>` (rule 54) — `master.wav` plus one
/// `<rig name>.wav` per armed rig. One thread decides every name, so all
/// files of a take group without a naming race; same-second takes retry with
/// `-2`, `-3`, … on the folder.
fn begin_take(
    dir: &Path,
    sample_rate: u32,
    channels: usize,
    mask: u64,
    stems: &[Option<Consumer<f32>>; MAX_RIGS],
    shared: &Arc<RecorderShared>,
) -> Option<Take> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("record: cannot create {}: {e}", dir.display());
        set_status(shared, TakeStatusView::default(), true, Some(format!("cannot create recording dir: {e}")), false);
        return None;
    }
    let base = format!("kabelsalat-{}", timestamp());
    let folder = match unique_dir(dir, &base) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("record: cannot create take folder: {e}");
            set_status(shared, TakeStatusView::default(), true, Some(format!("cannot create take folder: {e}")), false);
            return None;
        }
    };
    let names = shared.rig_names.lock().map(|n| n.clone()).unwrap_or_default();
    let mut files: Vec<TakeFile> = Vec::new();
    let mut open = |kind: TargetKind, name: &str, files: &mut Vec<TakeFile>| {
        match unique_path(&folder, name) {
            Ok((mut f, path)) => {
                if write_wav_header(&mut f, sample_rate, channels as u16, 0).is_err() {
                    let _ = std::fs::remove_file(&path);
                    eprintln!("record: cannot write header for {path}");
                    set_status(shared, TakeStatusView::default(), true, Some(format!("cannot write header for {path}")), false);
                    return false;
                }
                files.push(TakeFile { kind, file: f, path, samples: 0, next_patch: 0 });
                true
            }
            Err(e) => {
                eprintln!("record: cannot create recording: {e}");
                set_status(shared, TakeStatusView::default(), true, Some(format!("cannot create recording: {e}")), false);
                false
            }
        }
    };
    let mut ok = true;
    if mask & MASK_MASTER != 0 {
        ok = open(TargetKind::Master, "master", &mut files);
    }
    for rig in 0..MAX_RIGS as u8 {
        if mask & mask_rig(rig) == 0 || stems[rig as usize].is_none() {
            continue;
        }
        let fallback = format!("rig-{}", rig + 1);
        let name = names.get(rig as usize).map(|n| sanitize_file_name(n, &fallback)).unwrap_or(fallback);
        ok = ok && open(TargetKind::Rig(rig), &name, &mut files);
    }
    if !ok || files.is_empty() {
        let _ = std::fs::remove_dir(&folder);
        return None;
    }
    let folder_str = folder.to_string_lossy().into_owned();
    let view = TakeStatusView {
        folder: Some(folder_str.clone()),
        path: files.iter().find(|f| f.kind == TargetKind::Master).or_else(|| files.first()).map(|f| f.path.clone()),
        seconds: 0.0,
        stems: files
            .iter()
            .filter_map(|f| match f.kind {
                TargetKind::Rig(r) => Some(StemFileStatus { rig: r, path: Some(f.path.clone()), seconds: 0.0 }),
                TargetKind::Master => None,
            })
            .collect(),
    };
    set_status(shared, view, false, None, false);
    Some(Take { folder: folder_str, files })
}

/// Write the last `want` frames the retrospective ring kept, for every armed
/// target, as one take folder, and show it as the last take.
fn save_retro(keeper: &crate::retro::Keeper, want: u64, dir: &Path, sample_rate: u32, shared: &Arc<RecorderShared>) {
    let master = shared.arm_master.load(Ordering::Relaxed);
    let saved = keeper
        .window(want, master, shared.armed_rig_mask())
        .and_then(|w| write_retro_take(dir, sample_rate, &w, shared));
    match saved {
        Ok(view) => {
            if let Ok(mut s) = shared.status.lock() {
                s.retro_last = view.folder.clone();
            }
            shared.dropped_frames.store(0, Ordering::Relaxed);
            for d in &shared.stem_dropped {
                d.store(0, Ordering::Relaxed);
            }
            set_status(shared, view, true, None, false);
        }
        Err(e) => {
            eprintln!("record: retrospective save failed: {e}");
            if let Ok(mut s) = shared.status.lock() {
                s.error = Some(e);
            }
        }
    }
}

/// One take folder named after the moment its audio begins, `master.wav` and
/// `<rig name>.wav`, each with its exact sample count (rule 45). The frames
/// are read oldest first, a second at a time across every file, so a save
/// needs little memory and stays ahead of the feed.
fn write_retro_take(
    dir: &Path,
    sample_rate: u32,
    w: &crate::retro::Window,
    shared: &Arc<RecorderShared>,
) -> Result<TakeStatusView, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create recording dir: {e}"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let began = now - w.frames as f64 / sample_rate.max(1) as f64;
    let folder = unique_dir(dir, &format!("kabelsalat-{}", timestamp_at(began as i64)))
        .map_err(|e| format!("cannot create take folder: {e}"))?;
    let names = shared.rig_names.lock().map(|n| n.clone()).unwrap_or_default();
    let targets = w.targets();
    let written = (|| -> Result<TakeStatusView, String> {
        let io = |e: std::io::Error| format!("cannot write the take: {e}");
        let mut files = Vec::new();
        for t in &targets {
            let name = match t {
                None => "master".to_string(),
                Some(rig) => {
                    let fallback = format!("rig-{}", rig + 1);
                    names.get(*rig as usize).map(|n| sanitize_file_name(n, &fallback)).unwrap_or(fallback)
                }
            };
            let (f, path) = unique_path(&folder, &name).map_err(io)?;
            let mut f = std::io::BufWriter::new(f);
            write_wav_header(&mut f, sample_rate, 2, (w.frames * 2 * 4) as u32).map_err(io)?;
            files.push((f, path));
        }
        let (mut buf, mut bytes) = (Vec::new(), Vec::new());
        let end = w.start + w.frames;
        let mut from = w.start;
        while from < end {
            let to = (from + sample_rate.max(1) as u64).min(end);
            for (i, (f, _)) in files.iter_mut().enumerate() {
                w.read(i, from, to, &mut buf)?;
                samples_to_bytes(&buf, &mut bytes);
                f.write_all(&bytes).map_err(io)?;
            }
            from = to;
        }
        let seconds = seconds(w.frames * 2, sample_rate, 2);
        let mut path = None;
        let mut stems = Vec::new();
        for (t, (mut f, p)) in targets.iter().zip(files) {
            f.flush().map_err(io)?;
            match t {
                None => path = Some(p),
                Some(rig) => stems.push(StemFileStatus { rig: *rig, path: Some(p), seconds }),
            }
        }
        let path = path.or_else(|| stems.first().and_then(|s| s.path.clone()));
        Ok(TakeStatusView { folder: Some(folder.to_string_lossy().into_owned()), path, seconds, stems })
    })();
    written.inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&folder);
    })
}

/// A file name a rig's display name becomes: control characters and path
/// separators out, leading dots trimmed, at most 64 characters. Empty (or
/// empty-after-cleaning) falls back to `fallback`.
fn sanitize_file_name(name: &str, fallback: &str) -> String {
    let cleaned: String = name.chars()
        .map(|c| if c.is_control() || c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cut: String = cleaned.chars().take(64).collect();
    if cut.is_empty() { fallback.to_string() } else { cut }
}

/// Create `<dir>/<base>`, retrying `<dir>/<base>-2`, `<dir>/<base>-3`, … so
/// two takes in the same second never share a folder (`create_dir` fails on
/// an existing path; the writer is the only decider).
fn unique_dir(dir: &Path, base: &str) -> std::io::Result<PathBuf> {
    let mut suffix = 0u32;
    loop {
        let name = if suffix == 0 { base.to_string() } else { format!("{base}-{}", suffix + 1) };
        let p = dir.join(&name);
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(e) => return Err(e),
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
    view: TakeStatusView,
    finalized: bool,
    error: Option<String>,
    limit_reached: bool,
) {
    if let Ok(mut s) = shared.status.lock() {
        s.folder = view.folder;
        s.path = view.path;
        s.seconds = view.seconds;
        s.stems = view.stems;
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
    timestamp_at(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
}

fn timestamp_at(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs + local_utc_offset(secs));
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Seconds east of UTC for `secs` in the system time zone (0 if unknown).
pub(crate) fn local_utc_offset(secs: i64) -> i64 {
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
        NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") },
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
        bus.send(Command::NoteOn { note: n, velocity: 0.9, source: SourceId::Keys, channel: 0 });
    }
    for _ in 0..(2.0 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        reference.extend_from_slice(&buf);
    }
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOff { note: n, source: SourceId::Keys, channel: 0 });
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

    // Part 2: two rigs with distinct content -> two stems + master in one
    // take folder; the master chain applied to the stems' sum gives the
    // master file back sample for sample. Part 3 solos rig A: rig B's
    // (post-fader, rule 53) stem is digital silence and the master is the
    // mixdown of rig A's stem alone. Surge instances differ from each other
    // (free-running phases), so every comparison stays inside one take.
    let peak = |w: &[f32]| w.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    let differing = |a: &[f32], b: &[f32]| a.iter().zip(b).filter(|(x, y)| x != y).count();
    let full = record_stems_take(None)?;
    let (peak0, peak1) = (peak(&full.stems[0]), peak(&full.stems[1]));
    anyhow::ensure!(peak0 > 0.01 && peak1 > 0.01, "stems silent: {peak0}, {peak1}");
    anyhow::ensure!(full.stems[0] != full.stems[1], "stems must have distinct content");
    anyhow::ensure!(
        full.master == full.rebuilt,
        "the master is not the mixdown of its stems ({} samples differ)",
        differing(&full.master, &full.rebuilt)
    );
    let solo = record_stems_take(Some(0))?;
    anyhow::ensure!(peak(&solo.stems[0]) > 0.01, "rig A's stem is silent in its solo");
    anyhow::ensure!(solo.stems[1].iter().all(|&x| x == 0.0), "rig B's stem is not silent while rig A is soloed");
    anyhow::ensure!(
        solo.master == solo.rebuilt,
        "the soloed master is not the mixdown of rig A's stem ({} samples differ)",
        differing(&solo.master, &solo.rebuilt)
    );
    println!(
        "record-test (stems): OK — master + 2 stems in one take folder ({} frames each, peaks {peak0:.3} / {peak1:.3}, 0 dropped); \
         the master equals the master chain of stem A + stem B sample for sample",
        full.master.len() / 2
    );
    println!(
        "record-test (solo): OK — rig A soloed: rig B's stem is {} samples of 0.0, the master equals the mixdown of rig A's stem sample for sample",
        solo.stems[1].len()
    );
    let (secs, peak) = record_retro_take()?;
    println!(
        "record-test (retrospective): OK — a chord held 2 s before the save; the saved last {secs} s equal the last {secs} s rendered sample for sample \
         through the file player's reader, peak {peak:.3}"
    );
    Ok(())
}

/// `record-test` part 4 (#215): nothing is recording; a chord starts and
/// sounds for 2 s, then "save the last 1.5 s" while it still sounds. The
/// take must be the last 1.5 s the engine rendered, sample for sample.
fn record_retro_take() -> anyhow::Result<(f64, f32)> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::engine::GraphEngine;
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;

    const SR: f64 = 48000.0;
    const BLOCK: usize = 480;
    let (voice, _main) = ClapVoice::new(SR)?;
    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(
        NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") },
        name_bytes("SYNTH"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, vec![(synth, voice)]);
    let dir = std::env::temp_dir().join(format!("ksalat-retro-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let (tap, shared) = start(dir.clone(), SR as u32, 2);
    engine.set_recorder(tap);
    shared.set_retro_seconds(3);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while shared.retro_clock.keep_frames() == 0 {
        anyhow::ensure!(std::time::Instant::now() < deadline, "the writer did not allocate the ring");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let mut buf = [0.0f32; BLOCK * 2];
    let mut heard: Vec<f32> = Vec::new();
    for _ in 0..(SR as usize / BLOCK) {
        engine.render(&mut buf, 2);
        heard.extend_from_slice(&buf);
    }
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOn { note: n, velocity: 0.9, source: SourceId::Keys, channel: 0 });
    }
    for _ in 0..(2.0 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        heard.extend_from_slice(&buf);
    }
    anyhow::ensure!(!shared.is_recording(), "nothing was recording");
    let secs = shared.save_last(Some(1.5)).map_err(anyhow::Error::msg)?;
    while shared.snapshot().retro.saving {
        anyhow::ensure!(std::time::Instant::now() < deadline + std::time::Duration::from_secs(10), "the save was not written");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let status = shared.snapshot();
    let folder = status.retro.last_folder.clone().ok_or_else(|| anyhow::anyhow!("no take: {:?}", status.error))?;
    anyhow::ensure!(status.folder.as_deref() == Some(folder.as_str()), "the save is not shown as the last take");
    let path = Path::new(&folder).join("master.wav");
    let saved = crate::player::parse_wav(&mut std::fs::File::open(&path)?)
        .map_err(|e| anyhow::anyhow!("the file player cannot read {}: {e}", path.display()))?
        .data;
    let n = (1.5 * SR) as usize * 2;
    anyhow::ensure!(saved.len() == n, "{} samples saved, {n} expected", saved.len());
    let tail = &heard[heard.len() - n..];
    let differ = saved.iter().zip(tail).filter(|(a, b)| a != b).count();
    anyhow::ensure!(differ == 0, "{differ} of {n} saved samples differ from what was rendered");
    let peak = saved.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    anyhow::ensure!(peak > 0.01, "the saved take is silent (peak {peak})");
    drop(engine);
    shared.shutdown();
    if std::env::var("KABELSALAT_RECORD_KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok((secs, peak))
}

/// The samples of one stems take (`record-test` parts 2 and 3).
struct StemsTake {
    master: Vec<f32>,
    stems: [Vec<f32>; 2],
    /// The engine's master chain applied to stem A + stem B, in the order
    /// `render` sums the rigs.
    rebuilt: Vec<f32>,
}

/// Render a fixed two-rig performance (rig A "Keys": a high note from the
/// laptop keys, panned left; rig B "Bass": a low note from hardware 0, at
/// 0.7) with master + both stems armed, and return what the files hold
/// after the writer finalized them. `solo` solos that rig for the take.
fn record_stems_take(solo: Option<u8>) -> anyhow::Result<StemsTake> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::engine::GraphEngine;
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;

    const SR: f64 = 48000.0;
    const BLOCK: usize = 480;

    let rig_graph = |name: &str| {
        let mut g = Graph::default();
        g.reserve(4, 4);
        let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let s = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") }, name_bytes(name));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g
    };
    let (v0, _m0) = ClapVoice::new(SR)?;
    let (v1, _m1) = ClapVoice::new(SR)?;
    let merged = crate::control::merge_rigs(&[
        crate::control::Rig::new("Keys", rig_graph("SURGE A"), None),
        crate::control::Rig::new("Bass", rig_graph("SURGE B"), None),
    ]);
    let clap_of = |rig: u8| merged.nodes.iter().find(|n| n.rig == rig && matches!(n.kind, NodeKind::Clap { .. })).map(|n| n.id);
    let (s0_id, s1_id) = (clap_of(0).unwrap(), clap_of(1).unwrap());
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(merged, drain, SR, vec![(s0_id, v0), (s1_id, v1)]);
    let dir = std::env::temp_dir().join(format!("ksalat-stems-test-{}-{}", std::process::id(), solo.map_or(9, |r| r)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let (tap, shared) = start(dir.clone(), SR as u32, 2);
    shared.set_rig_names(vec!["Keys".into(), "Bass".into()]);
    engine.set_recorder(tap);
    shared.ensure_stem(0);
    shared.ensure_stem(1);
    bus.send(Command::SetRecordArm { rig: None, armed: true });
    bus.send(Command::SetRecordArm { rig: Some(0), armed: true });
    bus.send(Command::SetRecordArm { rig: Some(1), armed: true });
    bus.send(Command::SetRigSource { node: s0_id, keyboard: true, hardware: false });
    bus.send(Command::SetRigSource { node: s1_id, keyboard: false, hardware: true });
    bus.send(Command::SetRigMix { rig: 0, vol: None, pan: Some(-0.35), mute: None, solo: None });
    bus.send(Command::SetRigMix { rig: 1, vol: Some(0.7), pan: None, mute: None, solo: None });
    if let Some(r) = solo {
        bus.send(Command::SetRigMix { rig: r, vol: None, pan: None, mute: None, solo: Some(true) });
    }

    let mut buf = [0.0f32; BLOCK * 2];
    engine.render(&mut buf, 2); // Surge applies its init patch on the audio thread
    let mut master_ref: Vec<f32> = Vec::new();
    bus.send(Command::RecordStart);
    engine.render(&mut buf, 2);
    master_ref.extend_from_slice(&buf);
    bus.send(Command::NoteOn { note: 72, velocity: 0.9, source: SourceId::Keys, channel: 0 });
    bus.send(Command::NoteOn { note: 48, velocity: 0.9, source: SourceId::Hardware(0), channel: 0 });
    for _ in 0..(1.0 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        master_ref.extend_from_slice(&buf);
    }
    bus.send(Command::NoteOff { note: 72, source: SourceId::Keys, channel: 0 });
    bus.send(Command::NoteOff { note: 48, source: SourceId::Hardware(0), channel: 0 });
    for _ in 0..(0.3 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
        master_ref.extend_from_slice(&buf);
    }
    bus.send(Command::RecordStop);
    engine.render(&mut buf, 2);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        let s = shared.snapshot();
        if s.finalized && !s.recording {
            break s;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "stems recorder did not finalize");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    anyhow::ensure!(status.dropped_frames == 0, "stems dropped {} frames", status.dropped_frames);
    anyhow::ensure!(status.stems.len() == 2, "expected 2 stems, got {}", status.stems.len());
    let master_path = status.path.clone().ok_or_else(|| anyhow::anyhow!("no master path"))?;
    let stem0_path = status.stems[0].path.clone().ok_or_else(|| anyhow::anyhow!("no rig 0 stem path"))?;
    let stem1_path = status.stems[1].path.clone().ok_or_else(|| anyhow::anyhow!("no rig 1 stem path"))?;
    anyhow::ensure!(stem0_path.ends_with("Keys.wav"), "expected Keys.wav, got {stem0_path}");
    anyhow::ensure!(stem1_path.ends_with("Bass.wav"), "expected Bass.wav, got {stem1_path}");
    let folder = |p: &str| Path::new(p).parent().map(Path::to_path_buf);
    anyhow::ensure!(
        folder(&master_path) == folder(&stem0_path) && folder(&stem0_path) == folder(&stem1_path),
        "files of one take are in different folders: {master_path}, {stem0_path}, {stem1_path}"
    );

    // Read back through the app's own file reader too (rule 54).
    let read_f32 = |p: &str| -> anyhow::Result<Vec<f32>> {
        let b = std::fs::read(p)?;
        let h = parse_wav_header(&b).ok_or_else(|| anyhow::anyhow!("bad header in {p}"))?;
        anyhow::ensure!(h.format == 3 && h.bits == 32 && h.channels == 2, "{p} is not 32-bit float stereo");
        anyhow::ensure!(b.len() == WAV_HEADER_LEN + h.data_size as usize, "length mismatch in {p}");
        Ok(b[WAV_HEADER_LEN..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    };
    let master = read_f32(&master_path)?;
    let stems = [read_f32(&stem0_path)?, read_f32(&stem1_path)?];
    anyhow::ensure!(master == master_ref, "master.wav does not match the rendered output");
    anyhow::ensure!(stems.iter().all(|s| s.len() == master.len()), "stem lengths differ from the master");
    for (p, ours) in [(&master_path, &master), (&stem0_path, &stems[0]), (&stem1_path, &stems[1])] {
        let w = crate::player::parse_wav(&mut std::fs::File::open(p)?)
            .map_err(|e| anyhow::anyhow!("the file player cannot read {p}: {e}"))?;
        anyhow::ensure!(&w.data == ours, "the file player reads other samples from {p}");
    }

    let sum: Vec<f32> = stems[0].iter().zip(&stems[1]).map(|(&a, &b)| 0.0 + a + b).collect();
    let rebuilt = engine.master_chain_from_silence(&sum);

    drop(engine);
    shared.shutdown();
    if std::env::var("KABELSALAT_RECORD_KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(StemsTake { master, stems, rebuilt })
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
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                for sub in std::fs::read_dir(&p).unwrap().flatten() {
                    let sp = sub.path();
                    if sp.extension().map(|e| e == "wav").unwrap_or(false) {
                        files.push(sp);
                    }
                }
            } else if p.extension().map(|e| e == "wav").unwrap_or(false) {
                files.push(p);
            }
        }
        files.sort();
        let p = files.last().expect("a take file").clone();
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    fn test_tap(producer: Producer<f32>, msg_tx: Producer<RecorderMsg>, shared: Arc<RecorderShared>) -> RecorderTap {
        RecorderTap {
            producer,
            stems: std::array::from_fn(|_| None),
            stem_rx: None,
            active_mask: MASK_MASTER,
            msg_tx,
            shared,
            retro: crate::retro::channel(48_000).0,
        }
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
        let shared = Arc::new(RecorderShared::live(48_000));
        shared.recording.store(true, Ordering::Relaxed);
        let mut tap = test_tap(producer, msg_tx, shared.clone());
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
        let shared = Arc::new(RecorderShared::live(48_000));
        shared.recording.store(true, Ordering::Relaxed);
        let mut tap = test_tap(producer, msg_tx, shared.clone());
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
        let shared = Arc::new(RecorderShared::live(48_000));
        let mut tap = test_tap(producer, msg_tx, shared.clone());
        tap.begin(MASK_MASTER);
        tap.begin(MASK_MASTER);
        assert_eq!(msg_rx.pop().ok(), Some(RecorderMsg::Start { mask: MASK_MASTER }));
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
        tap.begin(MASK_MASTER);
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
            tap.begin(MASK_MASTER);
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
        tap.begin(MASK_MASTER);
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
        tap.begin(MASK_MASTER);
        tap.push_stereo(&a, 2);
        tap.stop();
        tap.begin(MASK_MASTER); // take 2, before the writer finalizes take 1
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
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                for sub in std::fs::read_dir(&p).unwrap().flatten() {
                    let sp = sub.path();
                    if sp.extension().map(|e| e == "wav").unwrap_or(false) {
                        files.push(sp);
                    }
                }
            }
        }
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
        let files: usize = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() {
                    std::fs::read_dir(&p).unwrap().flatten().filter(|s| s.path().extension().map(|ex| ex == "wav").unwrap_or(false)).count()
                } else if p.extension().map(|ex| ex == "wav").unwrap_or(false) {
                    1
                } else {
                    0
                }
            })
            .sum();
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
        tap.begin(MASK_MASTER);
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

    /// In 3/4 the bar lines are beats 3, 6, 9 (#87): a start pressed at beat
    /// 1 begins at 3, a stop pressed at beat 4 ends at 6, and a signature
    /// set while a start waits moves it to the new bar line.
    #[test]
    fn bar_quantized_start_and_stop_follow_the_time_signature() {
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480; // 0.02 beats/block at 120 BPM
        let dir = temp_dir("bar-quant-34");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 4);
        engine.set_recorder(tap);
        let mut buf = [0.0f32; BLOCK * 2];
        // Render until the song clock is at `beat`; the next block applies
        // whatever is due there.
        fn run_to(engine: &mut GraphEngine, buf: &mut [f32], beat: f64, mut each: impl FnMut()) {
            while engine.transport().beats < beat - 1e-6 {
                engine.render(buf, 2);
                each();
            }
        }

        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetTimeSignature { num: 3, den: 4 });
        bus.send(Command::TransportPlay);
        run_to(&mut engine, &mut buf, 1.0, || {});
        bus.send(Command::RecordStart);
        run_to(&mut engine, &mut buf, 3.0, || {});
        assert!(!shared.is_recording() && shared.is_pending(), "waits for the bar line at beat 3");
        engine.render(&mut buf, 2);
        assert!(shared.is_recording() && !shared.is_pending(), "starts at beat 3, not 4");

        run_to(&mut engine, &mut buf, 4.0, || {});
        bus.send(Command::RecordStop);
        run_to(&mut engine, &mut buf, 6.0, || {});
        assert!(shared.is_recording() && shared.is_pending(), "keeps recording until beat 6");
        engine.render(&mut buf, 2);
        assert!(!shared.is_recording() && !shared.is_pending(), "stops at beat 6, not 8");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!((status.seconds - 1.5).abs() < 0.02, "expected one 3/4 bar (1.5 s), got {} s", status.seconds);

        // Pressed at beat 7 in 3/4 it would wait for 9; in 5/4, set while it
        // waits, the next bar line is 10.
        run_to(&mut engine, &mut buf, 7.0, || {});
        bus.send(Command::RecordStart);
        engine.render(&mut buf, 2);
        bus.send(Command::SetTimeSignature { num: 5, den: 4 });
        run_to(&mut engine, &mut buf, 10.0, || {});
        assert!(!shared.is_recording() && shared.is_pending(), "beat 9 is no bar line in 5/4");
        engine.render(&mut buf, 2);
        assert!(shared.is_recording(), "starts at beat 10");
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bar_quantized_start_and_stop_while_transport_rolls() {
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480; // 0.02 beats/block at 120 BPM -> 200 blocks/bar
        let dir = temp_dir("bar-quant");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 4);
        engine.set_recorder(tap);

        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::TransportPlay);
        let mut buf = [0.0f32; BLOCK * 2];
        for _ in 0..50 {
            engine.render(&mut buf, 2); // beats = 1.0
        }
        bus.send(Command::RecordStart);
        for _ in 50..199 {
            engine.render(&mut buf, 2);
            assert!(!shared.is_recording(), "must wait for bar boundary");
            assert!(shared.is_pending(), "pending must be true while waiting for bar");
        }
        engine.render(&mut buf, 2); // block 199 -> beats reaches 4.0 at end of block
        engine.render(&mut buf, 2); // block 200: beats == 4.0 -> starts!
        assert!(shared.is_recording(), "must start at bar boundary (beat 4.0)");
        assert!(!shared.is_pending(), "pending clears once recording starts");

        for _ in 0..49 {
            engine.render(&mut buf, 2); // beats = 5.0
        }
        bus.send(Command::RecordStop);
        for _ in 250..399 {
            engine.render(&mut buf, 2);
            assert!(shared.is_recording(), "must keep recording until next bar");
            assert!(shared.is_pending(), "pending must be true while waiting for stop bar");
        }
        engine.render(&mut buf, 2); // block 399
        engine.render(&mut buf, 2); // block 400: beats == 8.0 -> stops!
        assert!(!shared.is_recording(), "must stop at bar boundary (beat 8.0)");
        assert!(!shared.is_pending());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!((status.seconds - 2.0).abs() < 0.02, "expected 1 bar (2.0 s), got {} s", status.seconds);
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transport_stop_keeps_recording_for_tails() {
        const SR: f64 = 48000.0;
        const BLOCK: usize = 480;
        let dir = temp_dir("tails");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, vec![]);
        let (tap, shared) = start_with_capacity(dir.clone(), SR as u32, 2, SR as usize * 2 * 4);
        engine.set_recorder(tap);

        bus.send(Command::RecordStart);
        bus.send(Command::TransportPlay);
        let mut buf = [0.0f32; BLOCK * 2];
        for _ in 0..25 {
            engine.render(&mut buf, 2);
        }
        assert!(shared.is_recording());
        bus.send(Command::TransportStop);
        for _ in 0..25 {
            engine.render(&mut buf, 2);
        }
        assert!(shared.is_recording(), "transport stop without record_sync must keep recording for tails");
        bus.send(Command::RecordStop);
        engine.render(&mut buf, 2);
        assert!(!shared.is_recording(), "record stop while transport stopped stops immediately");
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_rig_stems_and_master_in_same_take_folder() {
        let dir = temp_dir("stems");
        let (mut tap, shared) = start_with_capacity(dir.clone(), 48000, 2, 48000 * 2 * 4);
        shared.set_rig_names(vec!["Keys".into(), "Bass".into()]);
        shared.ensure_stem(0);
        shared.ensure_stem(1);
        tap.drain_stems();
        let m_data = ramp(480, 0.8);
        let r0_data = ramp(480, 0.3);
        let r1_data = ramp(480, -0.5);
        tap.begin(MASK_MASTER | mask_rig(0) | mask_rig(1));
        tap.push_stereo(&m_data, 2);
        tap.push_rig_stereo(0, &r0_data);
        tap.push_rig_stereo(1, &r1_data);
        tap.stop();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(status.stems.len(), 2);
        let folder = status.folder.expect("take folder");
        let m_path = PathBuf::from(&folder).join("master.wav");
        let r0_path = PathBuf::from(&folder).join("Keys.wav");
        let r1_path = PathBuf::from(&folder).join("Bass.wav");
        assert!(m_path.exists() && r0_path.exists() && r1_path.exists(), "all 3 WAVs in same take folder");
        assert_eq!(wav_samples(&std::fs::read(&m_path).unwrap()), m_data);
        assert_eq!(wav_samples(&std::fs::read(&r0_path).unwrap()), r0_data);
        assert_eq!(wav_samples(&std::fs::read(&r1_path).unwrap()), r1_data);
        drop(tap);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn twenty_rigs_each_mix_and_record_their_own_stem() {
        // #205: past the old 8-slot tables. Every rig is a click into its
        // master and every rig is armed before the engine's next block. Rig
        // 7 is muted, so its stem is silent while rig 19's is not (the old
        // tables clamped rig 19 onto slot 7); rig 19 alone plays on the
        // right (rule 182), so its fader shows on that channel.
        use crate::control::{merge_rigs, name_bytes, rig_letter, DspKind, NodeKind, PortKind, PortRef, Rig};
        const SR: u32 = 48000;
        const RIGS: usize = 20;
        let rigs: Vec<Rig> = (0..RIGS).map(|i| {
            let mut g = Graph::default();
            let click = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: click, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
            Rig::new(format!("Rig {}", rig_letter(i)), g, None)
        }).collect();
        let clicks: Vec<u32> = merge_rigs(&rigs).nodes.iter()
            .filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Metronome))).map(|n| n.id).collect();
        assert_eq!(clicks.len(), RIGS);
        let dir = temp_dir("twenty-rigs");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::default(), drain, SR as f64, vec![]);
        let (tap, shared) = start(dir.clone(), SR, 2);
        engine.set_recorder(tap);
        shared.set_rig_names(rigs.iter().map(|r| r.name.clone()).collect());
        bus.send(Command::LoadRigs { rigs, active: 0 });
        let mut buf = vec![0.0f32; 480 * 2];
        engine.render(&mut buf, 2);
        for rig in 0..RIGS as u8 {
            let pan = if rig == 19 { 1.0 } else { -1.0 };
            bus.send(Command::SetRigMix { rig, vol: None, pan: Some(pan), mute: Some(rig == 7), solo: None });
        }
        // A fader or mute takes 5 ms to land (#274): before the clicks start.
        engine.render(&mut buf, 2);
        for &node in &clicks {
            bus.send(Command::SetNodePlaying { node, playing: true });
        }
        for rig in 0..RIGS as u8 {
            shared.ensure_stem(rig);
            bus.send(Command::SetRecordArm { rig: Some(rig), armed: true });
        }
        bus.send(Command::SetRecordArm { rig: None, armed: false });
        bus.send(Command::RecordStart);
        let mut right = 0.0f32;
        for _ in 0..100 {
            engine.render(&mut buf, 2);
            right = buf.chunks(2).fold(right, |p, f| p.max(f[1].abs()));
        }
        bus.send(Command::RecordStop);
        engine.render(&mut buf, 2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            let s = shared.snapshot();
            if s.finalized && !s.recording {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "the take did not finalize");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(right > 0.01, "rig 19 plays on its own side: right peak {right}");
        assert_eq!(status.stems.iter().map(|s| s.rig as usize).collect::<Vec<_>>(), (0..RIGS).collect::<Vec<_>>(), "one stem per armed rig");
        let folder = PathBuf::from(status.folder.expect("take folder"));
        let peak = |name: &str| wav_samples(&std::fs::read(folder.join(format!("{name}.wav"))).unwrap())
            .iter().fold(0.0f32, |p, x| p.max(x.abs()));
        assert!(peak("Rig T") > 0.01, "rig 19 (T) records its click: {}", peak("Rig T"));
        assert!(peak("Rig H") < 1e-6, "muted rig 7 (H) records silence: {}", peak("Rig H"));
        assert!(peak("Rig A") > 0.01, "rig 0 records its click");

        bus.send(Command::SetRigMix { rig: 19, vol: None, pan: None, mute: Some(true), solo: None });
        for _ in 0..30 { engine.render(&mut buf, 2); } // the master's tails (rule 133)
        let mut right = 0.0f32;
        for _ in 0..100 {
            engine.render(&mut buf, 2);
            right = buf.chunks(2).fold(right, |p, f| p.max(f[1].abs()));
        }
        assert!(right < 1e-4, "muting rig 19 silences the right side: {right}");
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The app's own file reader (rule 54).
    fn played_back(path: &Path) -> Vec<f32> {
        crate::player::parse_wav(&mut std::fs::File::open(path).unwrap()).expect("the file player reads the take").data
    }

    /// #215: two rigs of clicks, rig B armed for its stem and panned, 6 s
    /// played into a 3 s history (its lanes wrap). A normal take runs and is
    /// stopped first; the save of the last 2 s then holds exactly the last
    /// 2 s the engine rendered, and its stem is the take's stem at the same
    /// frames.
    #[test]
    fn a_retrospective_save_is_what_was_heard_sample_for_sample() {
        use crate::control::{merge_rigs, name_bytes, DspKind, NodeKind, PortKind, PortRef, Rig};
        let rig_graph = || {
            let mut g = Graph::default();
            let click = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: click, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
            g
        };
        let merged = merge_rigs(&[Rig::new("Clicks", rig_graph(), None), Rig::new("Second", rig_graph(), None)]);
        let clicks: Vec<u32> = merged.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Metronome))).map(|n| n.id).collect();
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(merged, drain, 48000.0, vec![]);
        let dir = temp_dir("retro");
        let (tap, shared) = start(dir.clone(), 48_000, 2);
        shared.set_rig_names(vec!["Clicks".into(), "Second".into()]);
        engine.set_recorder(tap);
        assert!(shared.save_last(None).unwrap_err().contains("off"), "a save while it is off says so");

        let mut buf = vec![0.0f32; 256 * 2];
        shared.ensure_stem(1);
        bus.send(Command::SetRecordArm { rig: Some(1), armed: true });
        bus.send(Command::SetBpm { bpm: 240.0 });
        bus.send(Command::SetRigMix { rig: 1, vol: Some(0.5), pan: Some(0.6), mute: None, solo: None });
        for &c in &clicks {
            bus.send(Command::SetNodePlaying { node: c, playing: true });
        }
        engine.render(&mut buf, 2);
        shared.set_retro_seconds(3);
        wait_until("the writer allocated the ring", || shared.retro_clock.keep_frames() > 0);
        bus.send(Command::RecordStart);
        let mut heard: Vec<f32> = Vec::new();
        for _ in 0..(6 * 48000 / 256) {
            engine.render(&mut buf, 2);
            heard.extend_from_slice(&buf);
        }
        assert!(shared.save_last(None).unwrap_err().contains("REC is recording"), "a save never competes with a running take");
        bus.send(Command::RecordStop);
        engine.render(&mut buf, 2); // the stop applies before this block: in the history, not in the take
        heard.extend_from_slice(&buf);
        wait_until("the take was finalized", || shared.snapshot().takes == 1);
        let take = PathBuf::from(shared.snapshot().folder.expect("the normal take"));
        assert_eq!(shared.snapshot().retro.kept_seconds, 3.0, "6 s played, 3 s kept");
        assert_eq!(shared.save_last(Some(2.0)), Ok(2.0));
        wait_until("the save was written", || !shared.snapshot().retro.saving);

        let status = shared.snapshot();
        let retro = PathBuf::from(status.retro.last_folder.clone().expect("a retrospective take"));
        assert_eq!(status.folder, status.retro.last_folder, "the save shows as the last take");
        let master = played_back(&retro.join("master.wav"));
        let stem = played_back(&retro.join("Second.wav"));
        let n = 2 * 48000 * 2;
        assert_eq!(master.len(), n);
        assert!(master.iter().any(|&s| s.abs() > 0.05), "the clicks are in the take");
        assert!(master == heard[heard.len() - n..], "the saved master is the last 2 s heard, sample for sample");
        let take_stem = played_back(&take.join("Second.wav"));
        let in_take = n - buf.len();
        assert!(stem[..in_take] == take_stem[take_stem.len() - in_take..], "the saved stem is rig B's post-fader sum at the take's frames");
        assert!(stem.chunks_exact(2).any(|f| f[1] > f[0] + 0.01), "the stem carries rig B's pan");
        let bytes = std::fs::read(retro.join("master.wav")).unwrap();
        let h = parse_wav_header(&bytes).unwrap();
        assert_eq!((h.format, h.channels, h.bits, h.data_size as usize), (3, 2, 32, n * 4), "the header counts every sample (rule 45)");
        let first = status.retro.last_folder.clone();
        shared.save_last(Some(1.0)).unwrap();
        shared.save_last(Some(0.5)).unwrap();
        wait_until("both saves were written", || !shared.snapshot().retro.saving);
        let last = PathBuf::from(shared.snapshot().retro.last_folder.expect("a take"));
        assert_ne!(Some(last.to_string_lossy().into_owned()), first);
        assert_eq!(played_back(&last.join("master.wav")).len(), 48000, "saving stays on until the second save's own take exists");
        shared.set_arms(false, &[false; MAX_RIGS]);
        let refused = shared.save_last(None).unwrap_err();
        assert!(refused.contains("nothing is armed"));
        assert_eq!(shared.snapshot().error, Some(refused), "a refusal shows beside REC");
        drop(engine);
        shared.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retrospective_recording_is_on_unless_the_machine_has_little_memory() {
        // The first lines of this machine's /proc/meminfo (rule 63).
        let meminfo = "MemTotal:       32011200 kB\nMemFree:         9828408 kB\nMemAvailable:   23780968 kB\n";
        assert_eq!(mem_total_kib(meminfo), Some(32_011_200));
        assert_eq!(default_retro_minutes(mem_total_kib(meminfo)), 2);
        assert_eq!(default_retro_minutes(Some(2 << 20)), 0, "a 2 GiB machine keeps nothing");
        assert_eq!(default_retro_minutes(None), 0);
    }
}
