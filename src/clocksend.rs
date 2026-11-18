//! MIDI Clock Send (issue #85, AUM parity): kabelsalat is the master clock
//! and streams MIDI beat clock (24 ppqn) plus Start/Continue/Stop and an
//! optional Song Position Pointer to external gear and apps.
//!
//! Parity with AUM's "Clock and transport" chapter:
//! - Start when the transport starts from the beginning, Continue when it
//!   resumes, SPP+Continue instead of Start when SPP is enabled.
//! - A sync offset in ms, plus latency compensation: `None` aligns the clock
//!   with the audio output disregarding device output latency, `Output`
//!   includes it (gear mixed externally), `Input` aligns to the input (gear
//!   fed back into kabelsalat and heard through its latency).
//! - Negative net offsets get an automatic pre-roll: the engine start is
//!   deferred so the first ticks are still deliverable — the clock counts
//!   |offset| while the song waits.
//!
//! Wiring is a row of the MIDI matrix view: one cell per MIDI out port. The
//! wired ports (by name, rule 58) live in [`ClockControl`] and config.json
//! (hardware is per machine, so it is a preference, not session content);
//! the clock thread subscribes its ALSA port to each wired, live port. The
//! virtual "MIDI Clock" port is always there for apps to subscribe to.
//!
//! Timing (rule 31 spirit: never per-UI-frame): the engine publishes its
//! transport position with a wall-time anchor every audio block
//! ([`ClockPos`]); the clock thread extrapolates the 24-ppqn grid from that
//! snapshot and hands each tick to the ALSA sequencer queue with an
//! absolute real-time timestamp — the kernel delivers sample-accurately,
//! no sleep loop. Threads: audio (publishes), clock (schedules), UI/agent
//! (config only).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::alsaseq::{self, Addr, Event, PortDesc, SeqClient};
use crate::control::CommandBus;

/// MIDI beat clock resolution: 24 timing clocks per quarter note.
pub const PPQN: f64 = 24.0;
/// Largest manual sync offset (ms) — beyond this nobody can hear alignment.
pub const MAX_OFFSET_MS: f32 = 2000.0;
/// Clock-thread poll period: config/rules/transport state checks.
const POLL_MS: u64 = 25;
/// Schedule this far ahead on the ALSA queue.
const LOOKAHEAD_NS: u64 = 200_000_000;
/// A tick older than this is dropped (phase advances) instead of burst-fired.
const LATE_GRACE_NS: u64 = 50_000_000;
/// MIDI-out port rescan period (rule 48: hot-plug, never per UI frame).
const RESCAN_SECS: f32 = 2.0;

pub const MAX_CLOCK_OUTS: usize = 32;

// ---- configuration ---------------------------------------------------------

/// Latency compensation mode (AUM's three settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CompMode {
    /// Align the clock with the audio output, disregarding device latency.
    #[default]
    None,
    /// Include device output latency — for gear mixed externally.
    Output,
    /// Align to the audio input — for gear fed back into kabelsalat.
    Input,
}

impl CompMode {
    pub fn parse(s: &str) -> Option<CompMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Some(CompMode::None),
            "output" => Some(CompMode::Output),
            "input" => Some(CompMode::Input),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            CompMode::None => "none",
            CompMode::Output => "output",
            CompMode::Input => "input",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClockConfig {
    pub enabled: bool,
    /// Send Song Position Pointer + Continue instead of Start when resuming
    /// away from beat 0.
    pub spp: bool,
    /// Manual sync offset in ms (positive: clock later than audio).
    pub offset_ms: f32,
    pub comp: CompMode,
}

impl Default for ClockConfig {
    fn default() -> Self {
        ClockConfig { enabled: false, spp: false, offset_ms: 0.0, comp: CompMode::None }
    }
}

impl ClockConfig {
    pub fn normalized(self) -> Self {
        ClockConfig {
            enabled: self.enabled,
            spp: self.spp,
            offset_ms: if self.offset_ms.is_finite() {
                self.offset_ms.clamp(-MAX_OFFSET_MS, MAX_OFFSET_MS)
            } else {
                0.0
            },
            comp: self.comp,
        }
    }
}

// ---- engine → clock publication -------------------------------------------

/// Lock-free transport snapshot the engine writes each audio block and the
/// clock thread reads each pass. Relaxed atomics are enough: the reader
/// sanity-checks pairs (position/wall move together); a torn cross-field
/// read costs one stale pass, never a wrong scheduled tick (each pass
/// re-derives the whole future grid from the newest snapshot).
pub struct ClockPos {
    beats: AtomicU64,       // f64 bits, position at block start
    wall_ns: AtomicU64,     // monotonic ns when that block started
    bpm: AtomicU32,         // f32 bits
    flags: AtomicU32,       // bit 0 playing, bit 1 counting in
    start_seq: AtomicU32,   // bumped on every stopped→playing flip
    start_beats: AtomicU64, // f64 bits, position at the last start flip
    /// cpal's playback−callback timestamp (ns), written by the audio
    /// callback when the host provides one; 0 = unknown.
    out_lat_ns: AtomicU64,
}

impl Default for ClockPos {
    fn default() -> Self {
        Self {
            beats: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
            bpm: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            start_seq: AtomicU32::new(0),
            start_beats: AtomicU64::new(0),
            out_lat_ns: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockSnapshot {
    pub playing: bool,
    pub counting_in: bool,
    pub beats: f64,
    pub wall_ns: u64,
    pub bpm: f64,
    pub start_seq: u32,
    pub start_beats: f64,
}

impl ClockPos {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    /// Engine, once per audio block, at the block's start.
    pub fn publish_block(&self, beats: f64, bpm: f64, playing: bool, counting_in: bool, wall_ns: u64) {
        self.beats.store(beats.to_bits(), Ordering::Relaxed);
        self.wall_ns.store(wall_ns, Ordering::Relaxed);
        self.bpm.store((bpm as f32).to_bits(), Ordering::Relaxed);
        let mut flags = 0u32;
        if playing { flags |= 1; }
        if counting_in { flags |= 2; }
        self.flags.store(flags, Ordering::Relaxed);
    }

    /// Engine, at every stopped→playing flip: the song position the run
    /// started from (0 = from the beginning).
    pub fn publish_start(&self, beats: f64) {
        self.start_beats.store(beats.to_bits(), Ordering::Relaxed);
        self.start_seq.fetch_add(1, Ordering::Release);
    }

    /// Audio callback, when the host reports output latency.
    pub fn publish_out_latency(&self, ns: u64) {
        self.out_lat_ns.store(ns, Ordering::Relaxed);
    }

    pub fn out_latency_ns(&self) -> u64 { self.out_lat_ns.load(Ordering::Relaxed) }

    pub fn snapshot(&self) -> ClockSnapshot {
        let flags = self.flags.load(Ordering::Relaxed);
        ClockSnapshot {
            playing: flags & 1 != 0,
            counting_in: flags & 2 != 0,
            beats: f64::from_bits(self.beats.load(Ordering::Relaxed)),
            wall_ns: self.wall_ns.load(Ordering::Relaxed),
            bpm: f32::from_bits(self.bpm.load(Ordering::Relaxed)) as f64,
            start_seq: self.start_seq.load(Ordering::Acquire),
            start_beats: f64::from_bits(self.start_beats.load(Ordering::Relaxed)),
        }
    }
}

// ---- pure scheduling core -------------------------------------------------

/// What one pass wants on the wire. `EnginePlay` is the deferred master
/// start of a pre-roll (the thread sends `Command::TransportPlay`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutEvent {
    EnginePlay,
    Start { at_ns: u64 },
    Continue { at_ns: u64 },
    Spp { at_ns: u64, value: u16 },
    Tick { at_ns: u64 },
    /// Immediate (direct dispatch, no queue).
    Stop,
}

/// A pre-roll in progress (negative net offset, engine start deferred).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Armed {
    /// When the thread sends `Command::TransportPlay`.
    pub play_at_ns: u64,
    /// Intended wall time of song beat 0 (the engine should land here,
    /// within one audio block).
    pub epoch_ns: u64,
    /// Delivery time of tick 0 = `epoch − preroll`: clock counts |offset|
    /// while the song waits.
    pub tick0_ns: u64,
    pub spb_ns: f64,
    pub play_sent: bool,
    pub start_sent: bool,
}

/// Everything the pure planner keeps between passes.
#[derive(Clone, Debug, PartialEq)]
pub struct SenderState {
    /// `start_seq` as last seen; `None` until the first snapshot is read.
    last_start_seq: Option<u32>,
    /// Tick index (k, absolute since song position 0) next to schedule.
    next_tick: i64,
    /// We are mid-run (a start/continue was sent, ticks flow).
    running: bool,
    /// A pre-roll is ticking before the engine confirmed the start.
    pub armed: Option<Armed>,
}

impl Default for SenderState {
    fn default() -> Self {
        SenderState { last_start_seq: None, next_tick: 0, running: false, armed: None }
    }
}

impl SenderState {
    /// Drop everything scheduled on the ALSA queue (stop/disable/preroll
    /// cancel): the caller flushes pending events right after seeing
    /// `active() == false`.
    pub fn active(&self) -> bool {
        self.running || self.armed.is_some()
    }
}

fn spb_ns(bpm: f64) -> f64 {
    if bpm.is_finite() && bpm > 1.0 { 60.0e9 / bpm } else { 60.0e9 / 120.0 }
}

/// Manual offset + the compensation the mode asks for, in ns.
pub fn net_offset_ns(cfg: ClockConfig, align_ns: i64) -> i64 {
    (cfg.offset_ms as f64 * 1_000_000.0).round() as i64 + align_ns
}

/// SPP value for a start position: 16th notes, clamped to 14 bits.
pub fn spp_value(start_beats: f64) -> u16 {
    (start_beats.max(0.0) * 4.0).round().clamp(0.0, 16383.0) as u16
}

/// Delivery time of song position `beats`, from one engine snapshot:
/// audio for `beats` is generated at `wall + (beats − snap.beats)·spb`
/// (sample-0 of the block `wall` was taken at); the clock adds the net
/// offset. Position and wall both refer to the same block start.
fn delivery_ns(snap: &ClockSnapshot, beats: f64, net_off_ns: i64, now_ns: u64) -> u64 {
    let dt = (beats - snap.beats) * spb_ns(snap.bpm);
    let t = snap.wall_ns as i64 + dt.round() as i64 + net_off_ns;
    t.max(now_ns as i64).max(0) as u64
}

/// The song position whose clock delivery is `at` (inverse of
/// [`delivery_ns`], for finding the first tick due).
fn position_at_ns(snap: &ClockSnapshot, at_ns: u64, net_off_ns: i64) -> f64 {
    let dt = at_ns as f64 - snap.wall_ns as f64 - net_off_ns as f64;
    snap.beats + dt / spb_ns(snap.bpm)
}

/// Arm the automatic pre-roll for a negative net offset (AUM: "Any negative
/// clock offsets will need a suitable pre-roll to not miss the first clock
/// ticks. This is done automatically if the transport is stopped").
/// `count_in_secs` is the configured count-in the deferred start will run
/// (the pre-roll absorbs part of it, see `plan_pass` tests).
pub fn arm_preroll(now_ns: u64, net_off_ns: i64, count_in_secs: f64, bpm: f64) -> Armed {
    let preroll = (-net_off_ns).max(0) as u64;
    let ci = (count_in_secs.max(0.0) * 1e9) as u64;
    // Song beat 0 sounds at play_at + count-in; defer the engine start so
    // that beat-0's tick (delivery = epoch − preroll) is never in the past.
    let play_at = now_ns + preroll.saturating_sub(ci);
    let epoch = play_at + ci;
    Armed {
        play_at_ns: play_at,
        epoch_ns: epoch,
        tick0_ns: epoch.saturating_sub(preroll).max(now_ns),
        spb_ns: spb_ns(bpm),
        play_sent: false,
        start_sent: false,
    }
}

/// One planner pass — pure: same state + snapshot + time ⇒ same events.
/// `net_off_ns` is the full net offset (manual + latency compensation).
pub fn plan_pass(
    st: &mut SenderState,
    cfg: &ClockConfig,
    net_off_ns: i64,
    snap: &ClockSnapshot,
    now_ns: u64,
) -> Vec<OutEvent> {
    let mut out = Vec::new();

    // Disabled: one trailing Stop, then forget everything.
    if !cfg.enabled {
        if st.active() {
            out.push(OutEvent::Stop);
        }
        *st = SenderState::default();
        if st.last_start_seq.is_none() {
            st.last_start_seq = Some(snap.start_seq);
        }
        return out;
    }
    if st.last_start_seq.is_none() {
        st.last_start_seq = Some(snap.start_seq);
    }

    // Pre-roll in progress: tick on the intended grid until the engine
    // confirms (its start_seq moves), sending the deferred start at
    // play_at. The Start message rides the same grid (delivery of tick 0).
    if let Some(a) = &mut st.armed {
        if !a.play_sent && now_ns >= a.play_at_ns {
            a.play_sent = true;
            out.push(OutEvent::EnginePlay);
        }
        if !a.start_sent {
            a.start_sent = true;
            out.push(OutEvent::Start { at_ns: a.tick0_ns.max(now_ns) });
            st.running = true;
        }
        while let Some(k) = next_grid_k(a.tick0_ns, a.spb_ns, st.next_tick, now_ns) {
            let at = a.tick0_ns + (k as u64) * (a.spb_ns / PPQN) as u64;
            out.push(OutEvent::Tick { at_ns: at.max(now_ns) });
            st.next_tick = k + 1;
        }
        // The engine confirmed the (deferred) start: adopt its timeline,
        // keep the tick index for continuity.
        if snap.start_seq != st.last_start_seq.unwrap_or(u32::MAX) {
            st.last_start_seq = Some(snap.start_seq);
            st.armed = None;
        }
        return out;
    }

    let started = snap.start_seq != st.last_start_seq.unwrap_or(u32::MAX);
    if started {
        st.last_start_seq = Some(snap.start_seq);
        // A count-in end or a direct start: only now does the clock run.
        let at = delivery_ns(snap, snap.start_beats, net_off_ns, now_ns);
        // The grid is absolute (tick k = song position k/24) and begins AT
        // the start position: earlier ticks would be clamped onto "now" and
        // burst out right behind the Continue.
        st.next_tick = ((snap.start_beats * PPQN) - 1e-6).ceil().max(0.0) as i64;
        if snap.start_beats <= 1e-9 {
            out.push(OutEvent::Start { at_ns: at });
        } else {
            if cfg.spp {
                out.push(OutEvent::Spp { at_ns: at, value: spp_value(snap.start_beats) });
            }
            out.push(OutEvent::Continue { at_ns: at });
        }
        st.running = true;
    }

    // Stop when the engine halted (a count-in is not a halt: it precedes a
    // start we have not seen yet — but once running, count-in means the
    // user re-started with a count-in; the coming start re-anchors). A
    // start consumed THIS pass never stops in the same breath: the marker
    // is bumped inside the audio block, one poll before the block's own
    // snapshot lands with playing=true.
    if st.running && !started && !snap.playing && !snap.counting_in {
        st.running = false;
        st.next_tick = 0;
        out.push(OutEvent::Stop);
        return out;
    }

    if st.running && snap.playing {
        // Grid from the engine's own snapshot: tick k is song position k/24.
        let first = next_due_k(snap, net_off_ns, st.next_tick, now_ns);
        st.next_tick = first;
        let mut k = first;
        loop {
            let pos = k as f64 / PPQN;
            let at = delivery_ns(snap, pos, net_off_ns, now_ns);
            if at > now_ns + LOOKAHEAD_NS || k > first + 512 { break; }
            out.push(OutEvent::Tick { at_ns: at });
            k += 1;
        }
        st.next_tick = k;
    }
    out
}

/// Next armed-grid tick index ≥ `from` that is still worth scheduling
/// (due or upcoming, not yet hopelessly late).
fn next_grid_k(tick0: u64, spb: f64, from: i64, now_ns: u64) -> Option<i64> {
    let per = (spb / PPQN) as u64;
    let mut k = from.max(0);
    loop {
        let at = tick0 + k as u64 * per;
        if at + LATE_GRACE_NS >= now_ns { return Some(k); }
        k += 1;
        if k - from > 4096 { return None; } // paranoia: clock wedged
    }
}

/// First unscheduled tick index whose delivery is ≥ now − grace, from the
/// engine snapshot (keeps the song-position grid across pauses and tempo
/// changes: k is absolute).
fn next_due_k(snap: &ClockSnapshot, net_off_ns: i64, from: i64, now_ns: u64) -> i64 {
    let k_at_now = (position_at_ns(snap, now_ns.saturating_sub(LATE_GRACE_NS), net_off_ns) * PPQN).ceil() as i64;
    k_at_now.max(from)
}

// ---- out-port table (name-keyed stable indices, rule 58) ------------------

/// A MIDI port name without ALSA's trailing "client:port" numbers
/// ("FM-1:FM-1 MIDI 1 24:0" → "FM-1:FM-1 MIDI 1"): stable across re-plugs.
/// Shared with the hardware-input matcher in ui.rs.
pub fn midi_port_key(name: &str) -> &str {
    match name.rsplit_once(' ') {
        Some((head, tail)) if tail.split_once(':').is_some_and(|(c, p)| {
            !c.is_empty() && !p.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) && p.bytes().all(|b| b.is_ascii_digit())
        }) => head,
        _ => name,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClockOutPort {
    /// Stable identity: the port name without the client:port suffix.
    pub key: String,
    /// Full ALSA name as listed ("MIDIMATE eX:MIDIMATE eX MIDI 1 24:1").
    pub raw: String,
    /// ALSA address of the live port (valid only while `live`).
    pub addr: (u8, u8),
    pub live: bool,
    pub wired: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ClockStats {
    pub ticks: u64,
    pub starts: u64,
    pub dropped: u64,
    pub last_error: String,
    pub alsa: &'static str, // "ok" | "unavailable"
}

/// Everything the UI/agent shares with the clock thread.
pub struct ClockControl {
    pub cfg: Mutex<ClockConfig>,
    /// Engine transport publication (set when the engine attaches).
    pub pos: Mutex<Option<Arc<ClockPos>>>,
    /// Command bus (the deferred pre-roll start is sent from the thread).
    pub bus: Mutex<Option<Arc<Mutex<CommandBus>>>>,
    /// Wired out ports by stable key (name without client:port).
    pub wired: Mutex<Vec<String>>,
    /// Out ports as the thread sees them (name-keyed stable indices).
    pub outs: Mutex<Vec<ClockOutPort>>,
    pub out_rev: AtomicU64,
    pub stats: Mutex<ClockStats>,
    /// Net offset (manual + compensation) as computed last pass, for the
    /// UI's pre-roll decision at play time.
    net_off_ns: AtomicI64,
    /// A pre-roll is armed (the thread sets/clears this; the UI's play
    /// path reads it to avoid re-arming a running pre-roll).
    armed_state: AtomicBool,
    /// The UI pressed stop during a pre-roll: cancel the armed start.
    cancel_arm: AtomicBool,
    /// The thread's ALSA queue (-1 until it exists): the self-test stamps
    /// its receiving port on it.
    queue_id: AtomicI64,
    /// A pre-roll the UI asked for; the thread adopts it next pass.
    arm_req: Mutex<Option<Armed>>,
}

impl ClockControl {
    pub fn new() -> Arc<Self> {
        Arc::new(ClockControl {
            cfg: Mutex::new(ClockConfig::default()),
            pos: Mutex::new(None),
            bus: Mutex::new(None),
            wired: Mutex::new(Vec::new()),
            outs: Mutex::new(Vec::new()),
            out_rev: AtomicU64::new(0),
            stats: Mutex::new(ClockStats::default()),
            net_off_ns: AtomicI64::new(0),
            armed_state: AtomicBool::new(false),
            cancel_arm: AtomicBool::new(false),
            queue_id: AtomicI64::new(-1),
            arm_req: Mutex::new(None),
        })
    }

    pub fn config(&self) -> ClockConfig {
        self.cfg.lock().map(|c| c.normalized()).unwrap_or_default()
    }

    pub fn set_config(&self, cfg: ClockConfig) {
        if let Ok(mut c) = self.cfg.lock() {
            *c = cfg.normalized();
        }
    }

    /// Is a pre-roll armed right now (the play button is counting it in)?
    pub fn armed(&self) -> bool {
        self.armed_state.load(Ordering::Relaxed)
    }

    /// UI play path with a negative net offset: hand the computed pre-roll to
    /// the thread (the thread sends Start + ticks from the intended epoch
    /// and `Command::TransportPlay` at `Armed::play_at_ns`).
    pub fn request_arm(&self, a: Armed) {
        if let Ok(mut slot) = self.arm_req.lock() {
            *slot = Some(a);
        }
    }

    /// UI stop path: cancel an armed pre-roll (the thread flushes pending
    /// ticks and stops anything gear already saw).
    pub fn cancel_preroll(&self) {
        self.cancel_arm.store(true, Ordering::Relaxed);
    }

    /// Attach the engine once built: its transport publication, the command
    /// bus and the published rules.
    pub fn set_engine(&self, pos: Arc<ClockPos>, bus: Arc<Mutex<CommandBus>>) {
        if let Ok(mut p) = self.pos.lock() { *p = Some(pos); }
        if let Ok(mut b) = self.bus.lock() { *b = Some(bus); }
    }

    /// Wire (or unwire) the clock to a MIDI out port, identified by its
    /// stable key. Returns whether anything changed.
    pub fn set_wired(&self, key: &str, on: bool) -> bool {
        let Ok(mut w) = self.wired.lock() else { return false };
        let at = w.iter().position(|k| k == key);
        match (on, at) {
            (true, None) => { w.push(key.to_string()); true }
            (false, Some(i)) => { w.remove(i); true }
            _ => false,
        }
    }

    pub fn wired_keys(&self) -> Vec<String> {
        self.wired.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Settings as saved in config.json.
    pub fn saved(&self) -> ClockSaved {
        ClockSaved { config: self.config(), wired: self.wired_keys() }
    }

    pub fn restore(&self, s: ClockSaved) {
        self.set_config(s.config);
        if let Ok(mut w) = self.wired.lock() { *w = s.wired; }
    }

    /// The thread's computed net offset (manual + compensation), for the
    /// UI's pre-roll decision at play time.
    pub fn publish_net_offset_ns(&self, ns: i64) {
        self.net_off_ns.store(ns, Ordering::Relaxed);
    }

    pub fn net_offset_ms(&self) -> f32 {
        self.net_off_ns.load(Ordering::Relaxed) as f32 / 1e6
    }

    /// Snapshot of the out-port table for the matrix UI.
    pub fn outs_snapshot(&self) -> Vec<ClockOutPort> {
        self.outs.lock().map(|o| o.clone()).unwrap_or_default()
    }

    pub fn stats_snapshot(&self) -> ClockStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn note_error(&self, msg: impl Into<String>) {
        if let Ok(mut s) = self.stats.lock() {
            s.last_error = msg.into();
        }
    }
}

/// The latency the compensation modes use. `reported_out_ms` comes from
/// cpal's playback−callback timestamp when the host gives one; the fixed
/// estimate is the stream's own buffer period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Latencies {
    pub reported_out_ms: f32,
    pub est_out_ms: f32,
    pub est_in_ms: f32,
}

impl Latencies {
    /// What the current config's mode adds to the manual offset (ms).
    pub fn align_ms(&self, mode: CompMode) -> f32 {
        match mode {
            CompMode::None => 0.0,
            CompMode::Output => {
                if self.reported_out_ms > 0.0 { self.reported_out_ms } else { self.est_out_ms }
            }
            CompMode::Input => -self.est_in_ms,
        }
    }
}

/// What config.json keeps: the settings and the wired ports.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClockSaved {
    #[serde(default)]
    pub config: ClockConfig,
    #[serde(default)]
    pub wired: Vec<String>,
}

/// The one clock control of the process: the UI, the agent tools and the
/// clock thread share it (settings are per process, the wiring is the
/// matrix's Clock row).
pub fn global() -> Arc<ClockControl> {
    static CLOCK: std::sync::OnceLock<Arc<ClockControl>> = std::sync::OnceLock::new();
    CLOCK.get_or_init(|| {
        let c = ClockControl::new();
        // Never read in tests (rule 37): load_config_value is a no-op there.
        if let Some(saved) = crate::assistant::load_config_value("midi_clock")
            .and_then(|v| serde_json::from_value::<ClockSaved>(v).ok())
        {
            c.restore(saved);
        }
        c
    }).clone()
}

/// Spawn the clock thread. It owns the ALSA sequencer client (one client,
/// one readable "MIDI Clock" port — our virtual out — plus one running
/// queue), rescans out ports every 2 s and schedules passes every 25 ms.
/// Ends with the app (weak-count loop like the MIDI input thread).
pub fn spawn_clock_thread(ctl: Arc<ClockControl>) {
    let spawned = std::thread::Builder::new().name("kabelsalat-clock".into()).spawn(move || {
        let mut seq = SeqClient::open().ok();
        if seq.is_none() {
            eprintln!("midi clock: ALSA sequencer unavailable — sending disabled (config still editable)");
            if let Ok(mut s) = ctl.stats.lock() { s.alsa = "unavailable"; }
        }
        let (out_port, queue) = match &mut seq {
            Some(s) => {
                let port = s.create_port("MIDI Clock", alsaseq::PORT_CAP_READ | alsaseq::PORT_CAP_SUBS_READ)
                    .map_err(|e| { ctl.note_error(format!("create port: {e}")); e }).ok();
                let q = s.create_running_queue("kabelsalat-clock")
                    .map_err(|e| { ctl.note_error(format!("create queue: {e}")); e }).ok();
                (port, q)
            }
            None => (None, None),
        };
        if let Some(q) = queue { ctl.queue_id.store(q as i64, Ordering::Relaxed); }
        let mut st = SenderState::default();
        let mut subscriptions: Vec<bool> = Vec::new();
        let mut last_rescan = std::time::Instant::now() - std::time::Duration::from_secs(10);
        loop {
            if Arc::strong_count(&ctl) < 2 { break; }
            let now = alsaseq::mono_ns();

            // UI requests first: cancel a pre-roll (stop button), adopt a
            // freshly armed one (play with a negative net offset).
            if ctl.cancel_arm.swap(false, Ordering::Relaxed) && st.armed.is_some() {
                st.armed = None;
                st.next_tick = 0;
                // Undelivered scheduled ticks/starts die here; whatever gear
                // already saw gets its Stop from plan_pass's stop path
                // (st.running stays set until the engine idles out).
                if let (Some(s), Some(q)) = (seq.as_mut(), queue) {
                    let _ = s.flush_pending(q);
                }
            }
            if let Some(req) = ctl.arm_req.lock().ok().and_then(|mut a| a.take()) {
                st.armed = Some(req);
                st.next_tick = 0;
            }
            ctl.armed_state.store(st.armed.is_some(), Ordering::Relaxed);

            let cfg = ctl.config();

            // Latencies for the compensation modes: cpal's reported
            // playback−callback delta when the host gives one, else the
            // stream's own buffer period.
            let reported = ctl.pos.lock().ok()
                .and_then(|p| p.as_ref().map(|p| p.out_latency_ns()))
                .unwrap_or(0);
            let sr = crate::bridge::SAMPLE_RATE.load(Ordering::Relaxed).max(1) as f32;
            let buffer_ms = crate::bridge::BUFFER_FRAMES as f32 / sr * 1000.0;
            let lat = Latencies {
                reported_out_ms: reported as f32 / 1e6,
                est_out_ms: buffer_ms,
                est_in_ms: buffer_ms,
            };
            let align_ns = (lat.align_ms(cfg.comp) as f64 * 1e6).round() as i64;
            let net_off = net_offset_ns(cfg, align_ns);
            ctl.publish_net_offset_ns(net_off);

            // Engine snapshot; a missing engine keeps the clock idle.
            let snap = ctl.pos.lock().ok()
                .and_then(|p| p.as_ref().map(|p| p.snapshot()))
                .unwrap_or(ClockSnapshot {
                    playing: false, counting_in: false, beats: 0.0, wall_ns: now,
                    bpm: 120.0, start_seq: 0, start_beats: 0.0,
                });

            let was_active = st.active();
            let events = plan_pass(&mut st, &cfg, net_off, &snap, now);

            // Out-port table + wiring: read the engine's published rules.
            let wired = wired_outs(&ctl, &mut subscriptions);

            if let Some(s) = seq.as_mut() {
                if let (Some(p), Some(q)) = (out_port, queue) {
                    deliver(s, p, q, &events, &ctl);
                }
            } else {
                // No ALSA: only the deferred engine start still matters.
                for ev in &events {
                    if matches!(ev, OutEvent::EnginePlay) {
                        send_engine_play(&ctl);
                    }
                }
            }
            if was_active && !st.active() {
                // Stopped: flush anything still pending on the queue.
                if let (Some(s), Some(q)) = (seq.as_mut(), queue) {
                    let _ = s.flush_pending(q);
                }
            }

            if last_rescan.elapsed().as_secs_f32() >= RESCAN_SECS {
                last_rescan = std::time::Instant::now();
                rescan_outs(&ctl, seq.as_mut(), out_port, &wired, &mut subscriptions);
            }

            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    });
    if let Err(e) = spawned {
        eprintln!("midi clock: thread failed to start: {e}");
    }
}

/// Which out-port table entries are wired (by stable key).
fn wired_outs(ctl: &ClockControl, subscriptions: &mut Vec<bool>) -> Vec<bool> {
    let keys: Vec<String> = ctl.outs.lock().map(|o| o.iter().map(|p| p.key.clone()).collect()).unwrap_or_default();
    subscriptions.resize(keys.len(), false);
    let wired = ctl.wired_keys();
    keys.iter().map(|k| wired.contains(k)).collect()
}

fn send_engine_play(ctl: &ClockControl) {
    let bus = ctl.bus.lock().ok().and_then(|b| b.clone());
    if let Some(bus) = bus {
        if let Ok(mut b) = bus.lock() {
            b.send(crate::control::Command::TransportPlay);
        }
    }
}

/// Hand one pass's events to the sequencer.
fn deliver(seq: &mut SeqClient, port: u8, queue: u8, events: &[OutEvent], ctl: &ClockControl) {
    use alsaseq::{EVENT_CLOCK, EVENT_CONTINUE, EVENT_SONGPOS, EVENT_START, EVENT_STOP};
    for ev in events {
        let mut batch = [Event::empty(); 2];
        let mut n = 0;
        match *ev {
            OutEvent::EnginePlay => send_engine_play(ctl),
            OutEvent::Start { at_ns } => {
                batch[n] = seq.timed_clock_event(queue, port, EVENT_START, at_ns, 0);
                n += 1;
            }
            OutEvent::Continue { at_ns } => {
                batch[n] = seq.timed_clock_event(queue, port, EVENT_CONTINUE, at_ns, 0);
                n += 1;
            }
            OutEvent::Spp { at_ns, value } => {
                // MIDI order: SPP first; the planner's Continue follows it.
                batch[n] = seq.timed_clock_event(queue, port, EVENT_SONGPOS, at_ns, value as u32);
                n += 1;
            }
            OutEvent::Tick { at_ns } => {
                batch[n] = seq.timed_clock_event(queue, port, EVENT_CLOCK, at_ns, 0);
                n += 1;
            }
            OutEvent::Stop => {
                batch[n] = seq.direct_clock_event(port, EVENT_STOP);
                n += 1;
            }
        }
        if let Err(e) = seq.write_events(&batch[..n]) {
            if let Ok(mut s) = ctl.stats.lock() {
                s.dropped += 1;
                s.last_error = e.to_string();
            }
        } else if let Ok(mut s) = ctl.stats.lock() {
            match ev {
                OutEvent::Tick { .. } => s.ticks += 1,
                OutEvent::Start { .. } | OutEvent::Continue { .. } => s.starts += 1,
                _ => {}
            }
        }
    }
}

/// Rescan pass: refresh the out-port table (stable indices), then sync the
/// subscriptions the wiring asks for.
fn rescan_outs(ctl: &ClockControl, mut seq: Option<&mut SeqClient>, out_port: Option<u8>, wired: &[bool], subscriptions: &mut Vec<bool>) {
    let own = seq.as_deref().map(|s| s.client_id());
    let through_ok = std::env::var("KABELSALAT_MIDI_THROUGH").is_ok_and(|v| !v.is_empty() && v != "0");
    let listed: Vec<PortDesc> = seq.as_deref_mut().map(|s| s.list_ports()).unwrap_or_default();
    let candidates: Vec<&PortDesc> = listed.iter()
        .filter(|p| p.can_receive())
        .filter(|p| p.client != 0) // System timer/announce are not gear
        .filter(|p| own.map_or(true, |o| p.client != o))
        .filter(|p| !p.client_name.contains("kabelsalat"))
        .filter(|p| through_ok || !p.client_name.contains("Midi Through"))
        .collect();

    let mut changed = false;
    {
        let Ok(mut outs) = ctl.outs.lock() else { return };
        for o in outs.iter_mut() { o.live = false; }
        for c in candidates {
            let raw = c.full_name();
            let key = midi_port_key(&raw).to_string();
            match outs.iter().position(|o| o.key == key) {
                Some(i) => {
                    let o = &mut outs[i];
                    if o.raw != raw || o.addr != (c.client, c.port) {
                        o.raw = raw;
                        o.addr = (c.client, c.port);
                        changed = true;
                    }
                    o.live = true;
                }
                None if outs.len() < MAX_CLOCK_OUTS => {
                    outs.push(ClockOutPort {
                        key: key.clone(),
                        raw,
                        addr: (c.client, c.port),
                        live: true,
                        wired: false,
                    });
                    changed = true;
                }
                None => {}
            }
        }
        for (i, o) in outs.iter_mut().enumerate() {
            o.wired = wired.get(i).copied().unwrap_or(false);
        }
    }
    if changed {
        ctl.out_rev.fetch_add(1, Ordering::Release);
    }

    // Subscription sync: wired + live ports get our events; unwired lose them.
    if let (Some(s), Some(port)) = (seq, out_port) {
        let Ok(outs) = ctl.outs.lock() else { return };
        subscriptions.resize(outs.len(), false);
        for (i, o) in outs.iter().enumerate() {
            let want = o.wired && o.live;
            let have = subscriptions.get(i).copied().unwrap_or(false);
            if want && !have {
                match s.subscribe(
                    Addr { client: s.client_id(), port },
                    Addr { client: o.addr.0, port: o.addr.1 },
                ) {
                    Ok(()) => { subscriptions[i] = true; }
                    Err(e) => ctl.note_error(format!("subscribe {}: {e}", o.key)),
                }
            } else if !want && have {
                let _ = s.unsubscribe(
                    Addr { client: s.client_id(), port },
                    Addr { client: o.addr.0, port: o.addr.1 },
                );
                subscriptions[i] = false;
            }
        }
    }
}

// ---- CLI self-test ---------------------------------------------------------

/// `kabelsalat clock-test`: end-to-end over the real ALSA sequencer with
/// kernel virtual ports only (no hardware): a stand-in publisher plays the
/// engine's role (transport snapshots), the real clock thread schedules, and
/// a receiver client subscribed to our port measures what gear would see —
/// 24 clocks per beat within jitter, Start/SPP/Continue/Stop in order after
/// pause/resume, and the offset shifting the grid.
pub fn run_clock_test() -> anyhow::Result<()> {
    use crate::control::command_bus;
    use std::time::Instant;

    anyhow::ensure!(alsaseq::available(), "no /dev/snd/seq on this machine");
    let ctl = ClockControl::new();

    // A fake engine: ClockPos + the command bus.
    let pos = ClockPos::new();
    let (bus, mut drain) = command_bus();
    let bus = Arc::new(Mutex::new(bus));
    // Destination: a second kernel client's writable port, standing in for
    // gear (own client name, so the clock's port scan treats it as an
    // external destination and never matches real hardware).
    let mut gear = SeqClient::open_named("gear")?;
    let gear_port = gear.create_port("gear-in", alsaseq::PORT_CAP_WRITE | alsaseq::PORT_CAP_SUBS_WRITE)?;
    ctl.set_wired("gear:gear-in", true);
    ctl.set_config(ClockConfig { enabled: true, spp: true, offset_ms: 0.0, comp: CompMode::None });
    ctl.set_engine(pos.clone(), bus.clone());
    // Pre-seed the out-port table so index 0 IS the gear port no matter what
    // else this machine has plugged in (tests never touch real hardware):
    // the key matches what the scanner will compute for the same port.
    {
        let mut outs = ctl.outs.lock().unwrap();
        outs.push(ClockOutPort {
            key: "gear:gear-in".into(),
            raw: format!("gear:gear-in {}:{}", gear.client_id(), gear_port),
            addr: (gear.client_id(), gear_port),
            live: true,
            wired: true,
        });
    }

    spawn_clock_thread(ctl.clone());
    let started = Instant::now();
    // Wait until the clock thread created its port and subscribed to the
    // gear port, then subscribe the gear as a receiver of our port.
    let mut probe = SeqClient::open_named("clock-probe")?;
    let clock_client = loop {
        let ports = probe.list_ports();
        if let Some(p) = ports.iter().find(|p| p.client_name == "kabelsalat" && p.port_name == "MIDI Clock") {
            gear.subscribe(
                Addr { client: p.client, port: p.port },
                Addr { client: gear.client_id(), port: gear_port },
            )?;
            break p.client;
        }
        anyhow::ensure!(started.elapsed() < std::time::Duration::from_secs(5),
            "clock port never appeared ({:?}; ports: {:?})", ctl.stats_snapshot(),
            ports.iter().map(|p| p.full_name()).collect::<Vec<_>>());
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // Let the clock thread finish a pass with the idle engine first, so the
    // start below is seen as a start (the thread adopts the marker it finds
    // on its very first pass).
    std::thread::sleep(std::time::Duration::from_millis(150));
    // Let the kernel stamp what the gear port receives, on the clock's own
    // queue: the measure then holds the delivery, not this receiver's
    // scheduling latency on a loaded machine.
    let stamped = match ctl.queue_id.load(Ordering::Relaxed) {
        q if q >= 0 => match gear.stamp_port(gear_port, q as u8) {
            Ok(()) => true,
            Err(e) => { eprintln!("clock-test: cannot stamp the gear port: {e}"); false }
        },
        _ => false,
    };
    println!("clock-test: clock port found (client {clock_client}), gear subscribed{}",
        if stamped { ", kernel-stamped deliveries" } else { ", wake-time arrivals" });


    // 1. Run at 120 BPM for ~1.2 s from the beginning: Start + 24 ppqn.
    const SR: f64 = 48_000.0;
    const BLOCK: usize = 480;
    let bpm = 120.0f64;
    let mut beats = 0.0f64;
    let t0 = alsaseq::mono_ns();
    // The stand-in engine publishes exact block times (as the audio
    // callback does): block n starts at t0 + n * 10 ms, beats follow.
    let mut wall = t0;
    let block_ns = (BLOCK as f64 / SR * 1e9) as u64;
    let wait_until = |t: u64| {
        while alsaseq::mono_ns() < t {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    };
    pos.publish_block(beats, bpm, true, false, t0);
    pos.publish_start(0.0);
    // The receiver is its own thread, stamping each message with the
    // monotonic time poll woke at: what external gear would see.
    let inbox: Arc<Mutex<Vec<(u64, u8, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_rx = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rx_thread = {
        let (inbox, stop_rx) = (inbox.clone(), stop_rx.clone());
        std::thread::spawn(move || {
            let mut gear = gear;
            while !stop_rx.load(Ordering::Relaxed) {
                if !gear.poll_in(20) { continue; }
                let arrived = alsaseq::mono_ns();
                let mut buf = [Event::empty(); 32];
                let n = gear.read_events(&mut buf);
                if let Ok(mut v) = inbox.lock() {
                    for ev in &buf[..n] {
                        if ev.source.client == clock_client {
                            let at = if stamped { ev.time.to_ns() } else { arrived };
                            v.push((at, ev.ty, ev.songpos_value()));
                        }
                    }
                }
            }
        })
    };
    let take = |inbox: &Arc<Mutex<Vec<(u64, u8, u32)>>>| inbox.lock().map(|v| v.clone()).unwrap_or_default();
    for _blk in 0..(1200 * 48 / BLOCK) {
        wall += block_ns;
        wait_until(wall);
        beats += BLOCK as f64 * bpm / (60.0 * SR);
        pos.publish_block(beats, bpm, true, false, wall);
    }
    // Stop, collect the tail.
    pos.publish_block(beats, bpm, false, false, alsaseq::mono_ns());
    std::thread::sleep(std::time::Duration::from_millis(200));
    let received = take(&inbox);

    let ticks = received.iter().filter(|(_, ty, _)| *ty == alsaseq::EVENT_CLOCK).count();
    let has_start = received.iter().any(|(_, ty, _)| *ty == alsaseq::EVENT_START);
    let has_stop = received.iter().any(|(_, ty, _)| *ty == alsaseq::EVENT_STOP);
    anyhow::ensure!(has_start, "no Start message seen ({} events: {:?}; stats {:?}; outs {:?})", received.len(), &received[..received.len().min(8)], ctl.stats_snapshot(), ctl.outs_snapshot());
    anyhow::ensure!(has_stop, "no Stop message seen");
    anyhow::ensure!((54..=62).contains(&ticks), "expected ~58 ticks in 1.2 s at 120 BPM (48 per second), got {ticks}");
    // Tick grid: 24 per beat at 120 BPM = 20.833 ms apart. Each tick's
    // arrival is compared with the ideal grid (base = the median offset, so
    // one descheduled receiver on a loaded machine shows up as one late
    // tick, never as drift).
    let tick_times: Vec<u64> = received.iter().filter(|(_, ty, _)| *ty == alsaseq::EVENT_CLOCK).map(|(t, _, _)| *t).collect();
    let per_ms = 60_000.0 / bpm / PPQN;
    let offsets: Vec<f64> = tick_times.iter().enumerate()
        .map(|(k, t)| *t as f64 / 1e6 - k as f64 * per_ms).collect();
    let mut sorted = offsets.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let base = sorted[sorted.len() / 2];
    let mut dev: Vec<f64> = offsets.iter().map(|o| (o - base).abs()).collect();
    dev.sort_by(|a, b| a.total_cmp(b));
    let median = dev[dev.len() / 2];
    let p95 = dev[(dev.len() * 95 / 100).min(dev.len() - 1)];
    let worst = *dev.last().unwrap();
    println!("clock-test: {ticks} ticks over ~1.2 s at 120 BPM ({per_ms:.3} ms apart): deviation from the grid median {median:.2} ms, p95 {p95:.2} ms, worst {worst:.2} ms (Start + Stop seen)");
    anyhow::ensure!(p95 < 3.0, "tick jitter p95 {p95:.2} ms is beyond MIDI-clock tolerance");
    anyhow::ensure!(worst < 15.0, "a tick was {worst:.2} ms off the grid");

    // 2. Pause + resume with SPP: Continue + SPP(16th) instead of Start.
    if let Ok(mut v) = inbox.lock() { v.clear(); }
    let resume_beats = beats;
    pos.publish_start(resume_beats);
    wall = alsaseq::mono_ns();
    pos.publish_block(beats, bpm, true, false, wall);
    for _ in 0..(600 * 48 / BLOCK) {
        wall += block_ns;
        wait_until(wall);
        beats += BLOCK as f64 * bpm / (60.0 * SR);
        pos.publish_block(beats, bpm, true, false, wall);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let received = take(&inbox);
    stop_rx.store(true, Ordering::Relaxed);
    let _ = rx_thread.join();
    let spp = received.iter().find(|(_, ty, _)| *ty == alsaseq::EVENT_SONGPOS);
    let cont = received.iter().any(|(_, ty, _)| *ty == alsaseq::EVENT_CONTINUE);
    let want_spp = (resume_beats * 4.0).round() as u32;
    println!("clock-test: resume at beat {resume_beats:.2} → Continue={cont} SPP={} (want {want_spp})",
        spp.map(|s| s.2).unwrap_or(u32::MAX));
    anyhow::ensure!(cont, "no Continue on resume");
    anyhow::ensure!(spp.is_some_and(|s| s.2 == want_spp), "SPP missing or wrong");
    // SPP arrives before Continue.
    let spp_idx = received.iter().position(|(_, ty, _)| *ty == alsaseq::EVENT_SONGPOS);
    let cont_idx = received.iter().position(|(_, ty, _)| *ty == alsaseq::EVENT_CONTINUE);
    anyhow::ensure!(spp_idx < cont_idx, "SPP must precede Continue");
    let conts = received.iter().filter(|(_, ty, _)| *ty == alsaseq::EVENT_CONTINUE).count();
    anyhow::ensure!(conts == 1, "expected exactly one Continue, saw {conts}");

    // 3. The command bus never saw a spontaneous play (only the pre-roll
    // sends EnginePlay; offset is 0 here).
    let stray_play = {
        let mut n = 0;
        while let Some(c) = drain.pop() {
            if matches!(c, crate::control::Command::TransportPlay) { n += 1; }
        }
        n
    };
    anyhow::ensure!(stray_play == 0, "clock sent TransportPlay without a pre-roll");

    let stats = ctl.stats_snapshot();
    println!("clock-test: OK — stats: ticks {} starts {} dropped {} alsa {}",
        stats.ticks, stats.starts, stats.dropped, stats.alsa);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(playing: bool, beats: f64, wall: u64, bpm: f64, seq: u32, start_beats: f64) -> ClockSnapshot {
        ClockSnapshot { playing, counting_in: false, beats, wall_ns: wall, bpm, start_seq: seq, start_beats }
    }

    #[test]
    fn spp_encoding_is_sixteenths_clamped() {
        assert_eq!(spp_value(0.0), 0);
        assert_eq!(spp_value(1.0), 4); // one quarter note = 4 sixteenths
        assert_eq!(spp_value(2.5), 10);
        assert_eq!(spp_value(1e9), 16383);
        assert_eq!(spp_value(-5.0), 0);
    }

    #[test]
    fn comp_modes_align_the_grid() {
        let lat = Latencies { reported_out_ms: 21.0, est_out_ms: 5.3, est_in_ms: 5.3 };
        assert_eq!(lat.align_ms(CompMode::None), 0.0);
        assert_eq!(lat.align_ms(CompMode::Output), 21.0, "Output uses the reported device latency");
        let mut est = lat;
        est.reported_out_ms = 0.0;
        assert_eq!(est.align_ms(CompMode::Output), 5.3, "falls back to the buffer-period estimate");
        assert_eq!(lat.align_ms(CompMode::Input), -5.3, "Input shifts the clock EARLIER by the capture latency");
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: 10.0, comp: CompMode::Output };
        assert_eq!(net_offset_ns(cfg, (lat.align_ms(CompMode::Output) as f64 * 1e6) as i64), 31_000_000);
    }

    #[test]
    fn config_clamps_offset_and_parses_modes() {
        let c = ClockConfig { enabled: true, spp: true, offset_ms: 99999.0, comp: CompMode::None }.normalized();
        assert_eq!(c.offset_ms, MAX_OFFSET_MS);
        assert_eq!(CompMode::parse("Output"), Some(CompMode::Output));
        assert_eq!(CompMode::parse("input"), Some(CompMode::Input));
        assert_eq!(CompMode::parse("none"), Some(CompMode::None));
        assert_eq!(CompMode::parse("wat"), None);
        let json = serde_json::to_string(&c).unwrap();
        let back: ClockConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn wiring_is_by_port_key_and_saved_with_the_settings() {
        let c = ClockControl::new();
        assert!(c.set_wired("MIDIMATE eX:MIDIMATE eX MIDI 1", true));
        assert!(!c.set_wired("MIDIMATE eX:MIDIMATE eX MIDI 1", true), "already wired: no change");
        c.set_config(ClockConfig { enabled: true, spp: true, offset_ms: -12.5, comp: CompMode::Output });
        let json = serde_json::to_value(c.saved()).unwrap();
        let back: ClockSaved = serde_json::from_value(json).unwrap();
        let d = ClockControl::new();
        d.restore(back);
        assert_eq!(d.config(), c.config());
        assert_eq!(d.wired_keys(), vec!["MIDIMATE eX:MIDIMATE eX MIDI 1".to_string()]);
        assert!(d.set_wired("MIDIMATE eX:MIDIMATE eX MIDI 1", false));
        assert!(d.wired_keys().is_empty());
        // An old or empty config reads as the default (off, nothing wired).
        let none: ClockSaved = serde_json::from_str("{}").unwrap();
        assert_eq!(none, ClockSaved::default());
    }

    #[test]
    fn disabled_clock_sends_one_stop_and_resets() {
        let mut st = SenderState::default();
        st.running = true;
        st.next_tick = 40;
        st.last_start_seq = Some(3);
        let cfg = ClockConfig::default();
        let s = snap(true, 1.0, 1_000, 120.0, 3, 0.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 2_000);
        assert_eq!(out, vec![OutEvent::Stop]);
        assert!(!st.active());
        // A second pass sends nothing more.
        let out2 = plan_pass(&mut st, &cfg, 0, &s, 3_000);
        assert!(out2.is_empty());
    }

    #[test]
    fn start_from_beginning_sends_start_and_grid_ticks() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(1);
        let cfg = ClockConfig { enabled: true, spp: true, offset_ms: 0.0, comp: CompMode::None };
        // Snapshot right after a start at beat 0: wall 1s, position 0, 120 BPM.
        let s = snap(true, 0.0, 1_000_000_000, 120.0, 2, 0.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 1_000_000_000);
        assert!(matches!(out[0], OutEvent::Start { .. }), "Start first, got {out:?}");
        let ticks: Vec<u64> = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).collect();
        assert!(!ticks.is_empty(), "grid scheduled ahead");
        // First tick is at beat 0 = the snapshot wall time, then every
        // 60/120/24 s = 20.833 ms.
        assert_eq!(ticks[0], 1_000_000_000);
        let per = 20.833e6;
        for (i, w) in ticks.windows(2).enumerate() {
            let dt = (w[1] - w[0]) as f64;
            assert!((dt - per).abs() < 1_000.0, "tick {i}: {dt} ns");
        }
        // Enough lookahead to survive the 25 ms poll.
        assert!(ticks.last().unwrap() > &1_100_000_000u64, "lookahead covers at least 100 ms");
    }

    #[test]
    fn positive_offset_shifts_the_whole_stream() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(1);
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: 100.0, comp: CompMode::None };
        let s = snap(true, 0.0, 1_000_000_000, 120.0, 2, 0.0);
        let out = plan_pass(&mut st, &cfg, 100_000_000, &s, 1_000_000_000);
        match out[0] {
            OutEvent::Start { at_ns } => assert_eq!(at_ns, 1_100_000_000, "Start rides the offset"),
            ref other => panic!("expected Start, got {other:?}"),
        }
        if let Some(OutEvent::Tick { at_ns }) = out.iter().find(|e| matches!(e, OutEvent::Tick { .. })) {
            assert_eq!(*at_ns, 1_100_000_000, "first tick at beat 0 + offset");
        } else {
            panic!("no ticks scheduled: {out:?}");
        }
    }

    #[test]
    fn resume_sends_spp_then_continue_and_keeps_the_absolute_grid() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(5);
        let cfg = ClockConfig { enabled: true, spp: true, offset_ms: 0.0, comp: CompMode::None };
        // Resume mid-song at beat 10: the engine's marker says so.
        let s = snap(true, 10.0, 1_000_000_000, 120.0, 6, 10.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 1_000_000_000);
        assert_eq!(out[0], OutEvent::Spp { at_ns: 1_000_000_000, value: 40 }, "SPP in 16ths before Continue");
        assert_eq!(out[1], OutEvent::Continue { at_ns: 1_000_000_000 });
        // Ticks continue the ABSOLUTE grid: the first is k=240 (position 10,
        // on the Continue like Start's tick 0), the next k=241, never k=0.
        let first_tick = out.iter().find_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).expect("ticks resume");
        let spb = 0.5e9; // 120 BPM
        assert_eq!(first_tick, 1_000_000_000, "k=240 sits on the Continue");
        let second_tick = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).nth(1).expect("a second tick");
        let k241 = 1_000_000_000 + (spb / PPQN) as u64; // position 10 + 1/24
        assert!((second_tick as i64 - k241 as i64).abs() < 2, "second resumed tick {second_tick}, expected {k241}");
        // With SPP off it is a bare Continue.
        let mut st2 = SenderState::default();
        st2.last_start_seq = Some(5);
        let cfg2 = ClockConfig { enabled: true, spp: false, offset_ms: 0.0, comp: CompMode::None };
        let out2 = plan_pass(&mut st2, &cfg2, 0, &s, 1_000_000_000);
        assert_eq!(out2[0], OutEvent::Continue { at_ns: 1_000_000_000 });
        assert!(!out2.iter().any(|e| matches!(e, OutEvent::Spp { .. })));
    }

    #[test]
    fn count_in_is_not_a_stop_and_not_a_start() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(2);
        let cfg = ClockConfig { enabled: true, spp: true, offset_ms: 0.0, comp: CompMode::None };
        // Engine counting in (not playing yet, no new start marker).
        let s = ClockSnapshot { playing: false, counting_in: true, beats: 0.0, wall_ns: 1_000_000_000, bpm: 120.0, start_seq: 2, start_beats: 0.0 };
        let out = plan_pass(&mut st, &cfg, 0, &s, 1_000_000_000);
        assert!(out.is_empty(), "count-in alone sends nothing (gear starts with the song): {out:?}");
        // The count-in end: start marker at beat 0 → Start.
        let s2 = snap(true, 0.0, 2_000_000_000, 120.0, 3, 0.0);
        let out2 = plan_pass(&mut st, &cfg, 0, &s2, 2_000_000_000);
        assert!(matches!(out2[0], OutEvent::Start { .. }));
    }

    #[test]
    fn stop_then_idle_sends_one_stop_only() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(2);
        st.running = true;
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: 0.0, comp: CompMode::None };
        let s = snap(false, 4.0, 1_000_000_000, 120.0, 2, 0.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 1_000_000_000);
        assert_eq!(out, vec![OutEvent::Stop]);
        let out2 = plan_pass(&mut st, &cfg, 0, &s, 1_050_000_000);
        assert!(out2.is_empty(), "no repeated Stop: {out2:?}");
    }

    #[test]
    fn negative_offset_preroll_defers_the_engine_start_and_ticks_early() {
        // 500 ms negative net offset, no count-in: the engine start must be
        // deferred by 500 ms (play_at), tick 0 delivered immediately
        // (epoch − preroll = now), Start at the same moment.
        let now = 1_000_000_000u64;
        let a = arm_preroll(now, -500_000_000, 0.0, 120.0);
        assert_eq!(a.play_at_ns, now + 500_000_000, "engine start deferred by the offset");
        assert_eq!(a.epoch_ns, now + 500_000_000);
        assert_eq!(a.tick0_ns, now, "first tick deliverable at once (the pre-roll)");

        let mut st = SenderState::default();
        st.armed = Some(a);
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: -500.0, comp: CompMode::None };
        let s = snap(false, 0.0, now, 120.0, 0, 0.0);
        let out = plan_pass(&mut st, &cfg, -500_000_000, &s, now);
        // Start rides the grid at tick0 = now.
        assert_eq!(out[0], OutEvent::Start { at_ns: now });
        let ticks: Vec<u64> = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).collect();
        assert_eq!(ticks[0], now, "clock ticks from the press");
        let per = (0.5e9 / PPQN) as u64;
        assert_eq!(ticks[1] - ticks[0], per);
        // No EnginePlay before play_at.
        assert!(!out.iter().any(|e| matches!(e, OutEvent::EnginePlay)));
        // At play_at the deferred start goes out.
        let out2 = plan_pass(&mut st, &cfg, -500_000_000, &s, a.play_at_ns);
        assert!(out2.iter().any(|e| matches!(e, OutEvent::EnginePlay)));
    }

    #[test]
    fn preroll_with_count_in_absorbs_what_it_can() {
        // 500 ms negative offset with a 2 s count-in: the count-in already
        // provides lead time, so nothing is deferred (play_at = now) and
        // ticks start |offset| before the count-in ends.
        let now = 1_000_000_000u64;
        let a = arm_preroll(now, -500_000_000, 2.0, 120.0);
        assert_eq!(a.play_at_ns, now, "no extra deferral — the count-in covers it");
        assert_eq!(a.epoch_ns, now + 2_000_000_000, "song beat 0 at count-in end");
        assert_eq!(a.tick0_ns, now + 1_500_000_000, "ticks start |offset| before the song");
        // A count-in LONGER than the offset keeps the start immediate; an
        // offset longer than the count-in defers only the difference.
        let b = arm_preroll(now, -2_600_000_000, 2.0, 120.0);
        assert_eq!(b.play_at_ns, now + 600_000_000);
        assert_eq!(b.tick0_ns, now, "first tick still at the press");
    }

    #[test]
    fn preroll_ticks_continue_after_the_engine_confirms() {
        // The engine confirms the deferred start (start_seq moves): the
        // arm is consumed, ticks keep their index, no duplicate Start.
        let now = 1_000_000_000u64;
        let a = arm_preroll(now, -300_000_000, 0.0, 120.0);
        let mut st = SenderState::default();
        st.armed = Some(a);
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: -300.0, comp: CompMode::None };
        let s = snap(false, 0.0, now, 120.0, 0, 0.0);
        let _ = plan_pass(&mut st, &cfg, -300_000_000, &s, now);
        let scheduled = st.next_tick;
        assert!(scheduled > 0);
        // Engine running a bit late vs. the intended epoch: adopt its
        // timeline, tick index continuity.
        let late = a.play_at_ns + 8_000_000;
        let s2 = snap(true, 0.0, late, 120.0, 1, 0.0);
        let out = plan_pass(&mut st, &cfg, -300_000_000, &s2, late + 1_000_000);
        assert!(st.armed.is_none(), "arm consumed");
        assert!(!out.iter().any(|e| matches!(e, OutEvent::Start { .. })), "no duplicate Start");
        assert!(st.next_tick >= scheduled, "tick index never rewinds");
        let ticks: Vec<u64> = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).collect();
        assert!(!ticks.is_empty(), "ticks continue after confirmation");
    }

    #[test]
    fn late_ticks_are_dropped_not_burst_fired() {
        // Stalled for 2 s at 120 BPM: the next pass must not dump ~96
        // stale ticks at once — it skips ahead on the grid.
        let mut st = SenderState::default();
        st.last_start_seq = Some(1);
        st.running = true;
        st.next_tick = 10;
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: 0.0, comp: CompMode::None };
        let s = snap(true, 0.0, 1_000_000_000, 120.0, 1, 0.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 3_000_000_000);
        let ticks: Vec<u64> = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).collect();
        assert!(ticks.len() < 16, "burst of {} late ticks", ticks.len());
        // And it caught up on the grid.
        let per = (0.5e9 / PPQN) as u64;
        assert!(ticks[0] + per >= 3_000_000_000 - LATE_GRACE_NS, "first kept tick is current");
    }

    #[test]
    fn tempo_change_reanchors_the_future_grid() {
        let mut st = SenderState::default();
        st.last_start_seq = Some(1);
        st.running = true;
        st.next_tick = 100;
        let cfg = ClockConfig { enabled: true, spp: false, offset_ms: 0.0, comp: CompMode::None };
        // Position 100/24 beats at wall 1s, engine now at 240 BPM.
        let s = snap(true, 100.0 / 24.0, 1_000_000_000, 240.0, 1, 0.0);
        let out = plan_pass(&mut st, &cfg, 0, &s, 1_000_000_000);
        let ticks: Vec<u64> = out.iter().filter_map(|e| match e {
            OutEvent::Tick { at_ns } => Some(*at_ns),
            _ => None,
        }).collect();
        assert!(!ticks.is_empty());
        let per = (0.25e9 / PPQN) as u64; // 240 BPM: 10.4 ms
        assert_eq!(ticks[0], 1_000_000_000, "tick k=100 lands on position 100/24 = now");
        for w in ticks.windows(2) {
            assert!((w[1] - w[0]).abs_diff(per) <= 1, "grid follows the new tempo");
        }
    }

    #[test]
    fn port_key_strips_the_alsa_suffix() {
        assert_eq!(midi_port_key("FM-1:FM-1 MIDI 1 24:0"), "FM-1:FM-1 MIDI 1");
        assert_eq!(midi_port_key("ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 2 24:1"), "ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 2");
        assert_eq!(midi_port_key("Midi Through:Port-0 14:0"), "Midi Through:Port-0");
        // No numeric suffix: unchanged.
        assert_eq!(midi_port_key("a2j:Alicia's Keys"), "a2j:Alicia's Keys");
        assert_eq!(midi_port_key("weird 12:34:56"), "weird 12:34:56");
    }

    #[cfg(test)]
    impl ClockPos {
        pub fn test_new() -> Arc<Self> { Self::new() }
    }

    #[test]
    fn engine_publication_round_trips_through_the_snapshot() {
        let pos = ClockPos::new();
        pos.publish_block(3.5, 96.0, true, false, 123_456);
        pos.publish_start(3.5);
        let s = pos.snapshot();
        assert_eq!((s.beats, s.bpm, s.playing, s.counting_in, s.wall_ns), (3.5, 96.0, true, false, 123_456));
        assert_eq!(s.start_beats, 3.5);
        assert_eq!(s.start_seq, 1);
        pos.publish_block(4.0, 96.0, false, true, 130_000);
        let s2 = pos.snapshot();
        assert!(!s2.playing && s2.counting_in);
        assert_eq!(s2.start_seq, 1, "only playing flips bump the start marker");
        pos.publish_out_latency(13_000_000);
        assert_eq!(pos.out_latency_ns(), 13_000_000);
    }
}
