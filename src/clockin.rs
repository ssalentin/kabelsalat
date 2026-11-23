//! MIDI Clock In (#401): kabelsalat follows a hardware or app clock.
//!
//! The mirror of [`crate::clocksend`]. External gear sends 24 clocks per
//! quarter note plus Start / Continue / Stop / Song Position Pointer; this
//! module turns them into the engine's own commands:
//!
//! - **tempo**: the smoothed interval between clocks becomes `SetBpm`
//!   (20..300, moved only by 0.05 BPM or more, so jitter never reaches the
//!   engine);
//! - **transport**: Start / Continue become `TransportPlay` (Continue and
//!   SPP then move the song clock to the pointer), Stop becomes
//!   `TransportStop`;
//! - **phase**: every clock is one 24th of a beat, whatever the tempo, so
//!   the external song position is `base + n / 24`. Each clock compares
//!   that with where the engine's song clock is at the clock's own
//!   (kernel-stamped) time and nudges the difference away (`NudgeBeats`),
//!   so a tempo ramp or a clock drift never walks the two apart. One tempo
//!   to beat factor everywhere (rule 43): the engine's own
//!   `beats_per_frame` runs the clock, the follower only corrects it.
//!
//! A lost clock never stops the show: the tempo and the transport hold and
//! the status says "lost".
//!
//! Threads: the follower is a pure state machine ([`Follower`]); the ALSA
//! thread feeds it kernel-stamped events and sends its commands on the bus.
//! Nothing runs on the audio thread, and a UI frame is not in the path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::alsaseq::{self, Addr, SeqClient};
use crate::clocksend::{midi_port_key, ClockPos};
use crate::control::{Command, CommandBus};

pub const PPQN: f64 = 24.0;
/// A tempo change smaller than this never reaches the engine.
const TEMPO_STEP_BPM: f64 = 0.05;
/// Smoothing of the clock interval: one clock's weight.
const INTERVAL_ALPHA: f64 = 0.15;
/// Clocks needed after a Start (or a tempo jump) before the tempo is sent.
const WARMUP_TICKS: u32 = 6;
/// Clocks to let the engine start before its phase is judged.
const SETTLE_TICKS: u32 = 3;
/// Phase error (beats) under which nothing is nudged: about 0.6 ms at 120 BPM.
const DEADBAND_BEATS: f64 = 0.0003;
/// The share of the phase error corrected per clock.
const GAIN: f64 = 0.35;
/// No clock for this long: the clock is lost (the transport keeps running).
pub const LOST_NS: u64 = 600_000_000;
const RESCAN_SECS: f32 = 2.0;
const MIN_BPM: f64 = 20.0;
const MAX_BPM: f64 = 300.0;

/// What the follower asks of the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InCmd {
    Play,
    Stop,
    Tempo(f32),
    Nudge(f64),
}

impl InCmd {
    pub fn command(self) -> Command {
        match self {
            InCmd::Play => Command::TransportPlay,
            InCmd::Stop => Command::TransportStop,
            InCmd::Tempo(bpm) => Command::SetBpm { bpm },
            InCmd::Nudge(delta) => Command::NudgeBeats { delta },
        }
    }
}

/// The state of the PLL. Pure: the same events and engine positions give
/// the same commands.
#[derive(Clone, Debug)]
pub struct Follower {
    running: bool,
    /// Song position (quarter notes) of the next run's first clock: the
    /// Song Position Pointer, or 0.
    pointer: f64,
    /// Clocks since the run's first, and the position they sit on.
    n: u64,
    base: f64,
    /// The last clock's time and the smoothed interval between clocks (ns).
    last_ns: Option<u64>,
    interval: f64,
    ticks_since_start: u32,
    /// The tempo last sent to the engine.
    sent_bpm: f64,
    /// The smoothed tempo as the clock gives it, for the status.
    pub bpm_in: f64,
    /// The last phase error (beats; positive: the engine is behind).
    pub error_beats: f64,
    /// Clocks seen in all.
    pub ticks: u64,
}

impl Default for Follower {
    fn default() -> Self {
        Follower {
            running: false,
            pointer: 0.0,
            n: 0,
            base: 0.0,
            last_ns: None,
            interval: 0.0,
            ticks_since_start: 0,
            sent_bpm: 0.0,
            bpm_in: 0.0,
            error_beats: 0.0,
            ticks: 0,
        }
    }
}

impl Follower {
    pub fn running(&self) -> bool {
        self.running
    }

    fn begin(&mut self, from: f64) -> Vec<InCmd> {
        self.running = true;
        self.n = 0;
        self.base = from;
        self.ticks_since_start = 0;
        let mut out = vec![InCmd::Play];
        if from > 0.0 {
            // Play rewinds to 0; the pointer is where the song resumes.
            out.push(InCmd::Nudge(from));
        }
        out
    }

    /// Start: from the beginning.
    pub fn on_start(&mut self) -> Vec<InCmd> {
        self.pointer = 0.0;
        self.begin(0.0)
    }

    /// Continue: from the Song Position Pointer last received.
    pub fn on_continue(&mut self) -> Vec<InCmd> {
        if self.running {
            return Vec::new();
        }
        let from = self.pointer;
        self.begin(from)
    }

    /// Stop: the clock may keep ticking on some gear; the tempo is still
    /// followed, the position is not.
    pub fn on_stop(&mut self) -> Vec<InCmd> {
        if !self.running {
            return Vec::new();
        }
        self.running = false;
        // A later Continue resumes where the external clock stopped.
        self.pointer = self.base + self.n as f64 / PPQN;
        vec![InCmd::Stop]
    }

    /// Song Position Pointer: MIDI beats (sixteenth notes).
    pub fn on_songpos(&mut self, sixteenths: u32) {
        self.pointer = (sixteenths.min(16383) as f64) / 4.0;
    }

    /// One clock at kernel time `t_ns`. `engine_beats(t_ns)` is where the
    /// engine's song clock is at that time, `None` while it is not playing.
    pub fn on_tick(&mut self, t_ns: u64, engine_beats: impl FnOnce(u64) -> Option<f64>) -> Vec<InCmd> {
        let mut out = Vec::new();
        self.ticks += 1;
        if let Some(last) = self.last_ns {
            let dt = t_ns.saturating_sub(last) as f64;
            let ok = dt > 0.0 && (self.interval == 0.0 || (dt > 0.5 * self.interval && dt < 2.0 * self.interval));
            if ok {
                self.interval = if self.interval == 0.0 { dt } else { self.interval + INTERVAL_ALPHA * (dt - self.interval) };
            } else if dt >= 2.0 * self.interval.max(1.0) && dt > LOST_NS as f64 {
                // After a gap the old tempo means nothing.
                self.interval = 0.0;
                self.ticks_since_start = 0;
            }
        }
        self.last_ns = Some(t_ns);
        if self.interval > 0.0 {
            self.bpm_in = (60.0e9 / (PPQN * self.interval)).clamp(MIN_BPM, MAX_BPM);
        }
        self.ticks_since_start = self.ticks_since_start.saturating_add(1);
        if self.ticks_since_start > WARMUP_TICKS && self.bpm_in > 0.0 {
            let bpm = (self.bpm_in * 100.0).round() / 100.0;
            if (bpm - self.sent_bpm).abs() >= TEMPO_STEP_BPM {
                self.sent_bpm = bpm;
                out.push(InCmd::Tempo(bpm as f32));
            }
        }
        if self.running {
            let expected = self.base + self.n as f64 / PPQN;
            self.n += 1;
            if self.ticks_since_start > SETTLE_TICKS {
                if let Some(engine) = engine_beats(t_ns) {
                    let err = expected - engine;
                    self.error_beats = err;
                    if err.abs() > 0.25 {
                        out.push(InCmd::Nudge(err));
                    } else if err.abs() > DEADBAND_BEATS {
                        out.push(InCmd::Nudge(err * GAIN));
                    }
                }
            }
        }
        out
    }
}

/// The engine's song position at monotonic time `t_ns` from a published
/// block: the block started at `wall_ns` at `beats`, and runs at `bpm`.
pub fn engine_beats_at(snap: &crate::clocksend::ClockSnapshot, t_ns: u64) -> Option<f64> {
    if !snap.playing || snap.counting_in {
        return None;
    }
    let dt = t_ns as f64 - snap.wall_ns as f64;
    Some(snap.beats + dt * snap.bpm / 60.0e9)
}

// ---- configuration and control --------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClockInSaved {
    pub enabled: bool,
    /// The MIDI in port to follow, by stable name (rule 58); none = only
    /// what is connected to the virtual port.
    pub input: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ClockInStats {
    pub ticks: u64,
    pub bpm_in: f64,
    pub error_ms: f64,
    pub running: bool,
    pub locked: bool,
    pub last_error: String,
    pub alsa: &'static str,
    /// Live MIDI in ports by stable name.
    pub inputs: Vec<String>,
}

pub struct ClockInControl {
    pub saved: Mutex<ClockInSaved>,
    pub pos: Mutex<Option<Arc<ClockPos>>>,
    pub bus: Mutex<Option<Arc<Mutex<CommandBus>>>>,
    pub stats: Mutex<ClockInStats>,
    /// Bumped when the config changes: the thread re-subscribes.
    pub rev: AtomicU64,
    started: AtomicBool,
}

impl ClockInControl {
    pub fn new() -> Arc<Self> {
        Arc::new(ClockInControl {
            saved: Mutex::new(ClockInSaved::default()),
            pos: Mutex::new(None),
            bus: Mutex::new(None),
            stats: Mutex::new(ClockInStats { alsa: "none", ..Default::default() }),
            rev: AtomicU64::new(0),
            started: AtomicBool::new(false),
        })
    }

    pub fn set_engine(&self, pos: Arc<ClockPos>, bus: Arc<Mutex<CommandBus>>) {
        if let Ok(mut p) = self.pos.lock() {
            *p = Some(pos);
        }
        if let Ok(mut b) = self.bus.lock() {
            *b = Some(bus);
        }
    }

    pub fn config(&self) -> ClockInSaved {
        self.saved.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn restore(&self, s: ClockInSaved) {
        if let Ok(mut c) = self.saved.lock() {
            *c = s;
        }
        self.rev.fetch_add(1, Ordering::Release);
    }

    pub fn set(&self, enabled: Option<bool>, input: Option<Option<String>>) {
        if let Ok(mut c) = self.saved.lock() {
            if let Some(e) = enabled {
                c.enabled = e;
            }
            if let Some(i) = input {
                c.input = i;
            }
        }
        self.rev.fetch_add(1, Ordering::Release);
    }

    pub fn stats(&self) -> ClockInStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Resolve a port the user or agent named: exact key first, then a
    /// unique case-insensitive part of it.
    pub fn input_key(&self, name: &str) -> Result<String, String> {
        let keys = self.stats().inputs;
        if let Some(k) = keys.iter().find(|k| k.as_str() == name) {
            return Ok(k.clone());
        }
        let want = name.to_lowercase();
        let hits: Vec<&String> = keys.iter().filter(|k| k.to_lowercase().contains(&want)).collect();
        match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!("no MIDI in port '{name}' (ports: {})", keys.join(", "))),
            many => Err(format!(
                "'{name}' matches {} ports; be more specific ({})",
                many.len(),
                many.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    pub fn status_json(&self) -> serde_json::Value {
        let c = self.config();
        let s = self.stats();
        serde_json::json!({
            "follow": c.enabled,
            "input": c.input,
            "virtual_port": "kabelsalat: Clock In",
            "inputs": s.inputs,
            "locked": s.locked,
            "transport_running": s.running,
            "tempo_in": (s.bpm_in * 100.0).round() / 100.0,
            "phase_error_ms": (s.error_ms * 100.0).round() / 100.0,
            "clocks_in": s.ticks,
            "alsa": if s.alsa == "unavailable" { "unavailable" } else { "ok" },
            "last_error": s.last_error,
        })
    }

    fn note_error(&self, msg: impl Into<String>) {
        if let Ok(mut s) = self.stats.lock() {
            s.last_error = msg.into();
        }
    }

    fn send(&self, cmds: &[InCmd]) {
        if cmds.is_empty() {
            return;
        }
        let bus = self.bus.lock().ok().and_then(|b| b.clone());
        if let Some(bus) = bus {
            if let Ok(mut b) = bus.lock() {
                for c in cmds {
                    b.send(c.command());
                }
            }
        }
    }
}

/// The one clock-in control of the process.
pub fn global() -> Arc<ClockInControl> {
    static CTL: std::sync::OnceLock<Arc<ClockInControl>> = std::sync::OnceLock::new();
    CTL.get_or_init(|| {
        let c = ClockInControl::new();
        // Never read in tests (rule 37).
        if let Some(saved) = crate::assistant::load_config_value("midi_clock_in")
            .and_then(|v| serde_json::from_value::<ClockInSaved>(v).ok())
        {
            c.restore(saved);
        }
        c
    })
    .clone()
}

/// What one ALSA event means to the follower; the thread and the self-test
/// share it.
pub fn feed_event(f: &mut Follower, ev_ty: u8, songpos: u32, t_ns: u64, snap: &crate::clocksend::ClockSnapshot) -> Vec<InCmd> {
    use alsaseq::{EVENT_CLOCK, EVENT_CONTINUE, EVENT_SONGPOS, EVENT_START, EVENT_STOP};
    match ev_ty {
        EVENT_CLOCK => f.on_tick(t_ns, |t| engine_beats_at(snap, t)),
        EVENT_START => f.on_start(),
        EVENT_CONTINUE => f.on_continue(),
        EVENT_STOP => f.on_stop(),
        EVENT_SONGPOS => {
            f.on_songpos(songpos);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Spawn the clock-in thread. It owns an ALSA sequencer client with one
/// writable "Clock In" port (kernel-stamped on its own queue), subscribes
/// the chosen MIDI in port to it while following is on, and ends with the
/// app. It sleeps in `poll` (rule 47): idle costs nothing.
pub fn spawn_clockin_thread(ctl: Arc<ClockInControl>) {
    if ctl.started.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("kabelsalat-clockin".into()).spawn(move || {
        let mut seq = match SeqClient::open() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("midi clock in: ALSA sequencer unavailable ({e}): following disabled");
                if let Ok(mut s) = ctl.stats.lock() {
                    s.alsa = "unavailable";
                }
                return;
            }
        };
        let setup = (|| -> anyhow::Result<(u8, u8)> {
            let port = seq.create_port("Clock In", alsaseq::PORT_CAP_WRITE | alsaseq::PORT_CAP_SUBS_WRITE)?;
            let queue = seq.create_running_queue("kabelsalat-clockin")?;
            seq.stamp_port(port, queue)?;
            Ok((port, queue))
        })();
        let Ok((port, _queue)) = setup else {
            ctl.note_error(format!("setup: {:?}", setup.err()));
            return;
        };
        if let Ok(mut s) = ctl.stats.lock() {
            s.alsa = "ok";
        }
        let me = Addr { client: seq.client_id(), port };
        let mut follower = Follower::default();
        let mut subscribed: Option<(String, Addr)> = None;
        let mut last_rev = u64::MAX;
        let mut last_rescan = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let mut last_tick_ns = 0u64;
        let mut was_enabled = false;
        let mut buf = vec![alsaseq::Event::empty(); 64];
        loop {
            if Arc::strong_count(&ctl) < 2 {
                break;
            }
            let cfg = ctl.config();
            let rev = ctl.rev.load(Ordering::Acquire);
            let due = last_rescan.elapsed().as_secs_f32() >= RESCAN_SECS;
            if due || rev != last_rev {
                last_rescan = std::time::Instant::now();
                last_rev = rev;
                let ports = seq.gear_ports(false);
                let keyed: Vec<(String, Addr)> = ports
                    .iter()
                    .map(|p| (midi_port_key(&p.full_name()).to_string(), Addr { client: p.client, port: p.port }))
                    .collect();
                if let Ok(mut s) = ctl.stats.lock() {
                    s.inputs = keyed.iter().map(|(k, _)| k.clone()).collect();
                }
                let want = cfg.enabled.then(|| cfg.input.as_ref().and_then(|k| keyed.iter().find(|(n, _)| n == k))).flatten();
                if subscribed.as_ref().map(|(k, a)| (k.clone(), *a)) != want.cloned() {
                    if let Some((_, a)) = subscribed.take() {
                        let _ = seq.unsubscribe(a, me);
                    }
                    if let Some((k, a)) = want {
                        match seq.subscribe(*a, me) {
                            Ok(()) => subscribed = Some((k.clone(), *a)),
                            Err(e) => ctl.note_error(format!("subscribe {k}: {e}")),
                        }
                    }
                }
            }
            if !cfg.enabled {
                if was_enabled {
                    follower = Follower::default();
                    was_enabled = false;
                    if let Ok(mut s) = ctl.stats.lock() {
                        s.locked = false;
                        s.running = false;
                    }
                }
                // Drain what a connected app sends, ignore it.
                let n = seq.read_events(&mut buf);
                let _ = n;
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            was_enabled = true;
            if !seq.poll_in(20) {
                let now = alsaseq::mono_ns();
                if let Ok(mut s) = ctl.stats.lock() {
                    s.locked = last_tick_ns != 0 && now.saturating_sub(last_tick_ns) < LOST_NS;
                }
                continue;
            }
            let n = seq.read_events(&mut buf);
            for ev in &buf[..n] {
                let t = seq.event_mono_ns(ev).unwrap_or_else(alsaseq::mono_ns);
                let snap = ctl.pos.lock().ok().and_then(|p| p.as_ref().map(|p| p.snapshot()));
                let Some(snap) = snap else { continue };
                let cmds = feed_event(&mut follower, ev.ty, ev.songpos_value(), t, &snap);
                ctl.send(&cmds);
                if ev.ty == alsaseq::EVENT_CLOCK {
                    last_tick_ns = t;
                }
            }
            if let Ok(mut s) = ctl.stats.lock() {
                s.ticks = follower.ticks;
                s.bpm_in = follower.bpm_in;
                s.running = follower.running();
                s.locked = true;
                let spb_ms = if follower.bpm_in > 0.0 { 60_000.0 / follower.bpm_in } else { 0.0 };
                s.error_ms = follower.error_beats * spb_ms;
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("midi clock in: thread failed to start: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clocksend::{plan_pass, ClockConfig, ClockSnapshot, OutEvent, SenderState};
    use crate::control::{command_bus, name_bytes, Graph, NodeKind, PortKind, PortRef};
    use crate::engine::GraphEngine;

    const SR: f64 = 48_000.0;
    const BLOCK: usize = 480;
    const BLOCK_NS: u64 = 10_000_000;

    /// A tick stream the real sender planner produces for a master that
    /// plays from beat 0 at `bpm0`, changing to `bpm1` at `change_at_beats`.
    /// Returns (time of each clock, its song position) for `secs` seconds.
    fn master_ticks(bpm0: f64, bpm1: f64, change_at_beats: f64, secs: f64) -> Vec<(u64, f64)> {
        let cfg = ClockConfig { enabled: true, ..Default::default() };
        let mut st = SenderState::default();
        let mut out = Vec::new();
        let t0 = 1_000_000_000u64;
        // Advance the master's own transport in 10 ms blocks like an engine
        // and run the planner each pass over its published snapshot.
        let (mut beats, mut bpm, mut seq) = (0.0f64, bpm0, 0u32);
        let mut t = t0;
        let end = t0 + (secs * 1e9) as u64;
        let mut first = true;
        while t < end {
            if beats >= change_at_beats {
                bpm = bpm1;
            }
            let snap = ClockSnapshot { playing: true, counting_in: false, beats, wall_ns: t, bpm, start_seq: seq, start_beats: 0.0 };
            if first {
                seq = 1;
                first = false;
                let _ = plan_pass(&mut st, &cfg, 0, &ClockSnapshot { start_seq: 0, ..snap }, t);
                continue_with(&mut st, &cfg, &mut out, &ClockSnapshot { start_seq: 1, ..snap }, t);
            } else {
                continue_with(&mut st, &cfg, &mut out, &ClockSnapshot { start_seq: seq, ..snap }, t);
            }
            beats += BLOCK as f64 * bpm / (60.0 * SR);
            t += BLOCK_NS;
        }
        out.sort_by_key(|(t, _)| *t);
        out.dedup_by_key(|(t, _)| *t);
        out
    }

    fn continue_with(st: &mut SenderState, cfg: &ClockConfig, out: &mut Vec<(u64, f64)>, snap: &ClockSnapshot, now: u64) {
        for ev in plan_pass(st, cfg, 0, snap, now) {
            if let OutEvent::Tick { at_ns } = ev {
                let at = at_ns;
                let pos = snap.beats + (at as f64 - snap.wall_ns as f64) * snap.bpm / 60.0e9;
                out.push((at, pos));
            }
        }
    }

    #[test]
    fn the_follower_turns_start_continue_stop_and_the_pointer_into_commands() {
        let mut f = Follower::default();
        assert_eq!(f.on_start(), [InCmd::Play]);
        assert!(f.on_start() == [InCmd::Play], "a second Start restarts");
        assert_eq!(f.on_stop(), [InCmd::Stop]);
        assert!(f.on_stop().is_empty(), "a Stop with nothing running is nothing");
        f.on_songpos(32);
        assert_eq!(f.on_continue(), [InCmd::Play, InCmd::Nudge(8.0)], "32 sixteenths = 8 beats");
        assert!(f.on_continue().is_empty());
        // Pointer is 14 bits.
        f.on_stop();
        f.on_songpos(99_999);
        assert_eq!(f.on_continue()[1], InCmd::Nudge(16383.0 / 4.0));
        // Stop then Continue without a pointer resumes where the clock halted.
        let mut f = Follower::default();
        f.on_start();
        for i in 0..48 {
            f.on_tick(i * 20_000_000, |_| None);
        }
        f.on_stop();
        assert_eq!(f.on_continue(), [InCmd::Play, InCmd::Nudge(2.0)]);
    }

    #[test]
    fn a_steady_clock_gives_its_tempo_once_and_a_jittery_one_never_chatters() {
        let mut f = Follower::default();
        f.on_start();
        let spb_ns = 60.0e9 / 100.0;
        let mut tempos = Vec::new();
        let mut state = 12345u64;
        for i in 0..400u64 {
            // ±40 µs of jitter, deterministic.
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = ((state >> 33) % 80_000) as i64 - 40_000;
            let t = (i as f64 * spb_ns / PPQN) as i64 + jitter + 1_000_000_000;
            for c in f.on_tick(t as u64, |_| None) {
                if let InCmd::Tempo(b) = c {
                    tempos.push(b);
                }
            }
        }
        assert!(!tempos.is_empty() && tempos.len() <= 3, "{tempos:?}");
        assert!((tempos.last().unwrap() - 100.0).abs() < 0.2, "{tempos:?}");
    }

    /// Two instances: a master sending MIDI clock (the real planner), and a
    /// follower driving a real engine through its command bus, kernel-stamped
    /// clocks and virtual time. The tempo changes mid-run; the engine's song
    /// position must stay on the master's.
    #[test]
    fn two_instances_stay_phase_locked_across_a_tempo_change() {
        let mut g = Graph::default();
        g.reserve(4, 4);
        g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(g, drain, SR, Vec::new());
        let pos = ClockPos::new();
        engine.set_clock_pos(pos.clone());
        bus.send(Command::SetBpm { bpm: 90.0 }); // not the master's tempo

        // The master: 120 BPM, then 150 after 16 beats.
        let ticks = master_ticks(120.0, 150.0, 16.0, 30.0);
        assert!(ticks.len() > 24 * 40, "{} clocks", ticks.len());
        let t0 = ticks[0].0;
        let mut f = Follower::default();
        let mut next_tick = 0usize;
        let mut started = false;
        let mut buf = vec![0.0f32; BLOCK * 2];
        let mut now = t0 - 5 * BLOCK_NS;
        let mut worst_after_lock = 0.0f64;
        let mut tempo_changes = Vec::new();
        let end = ticks.last().unwrap().0;
        let mut block = 0u64;
        while now < end {
            // The engine renders one block that starts at `now`: it publishes
            // that with the wall time the engine would have read.
            let before = pos.snapshot();
            let _ = before;
            // Publish like the audio callback does (the engine uses the real
            // monotonic clock; here the virtual one).
            engine.render(&mut buf, 2);
            let tr = engine.transport();
            pos.publish_block(tr.beats - BLOCK as f64 * tr.bpm / (60.0 * SR), tr.bpm, tr.playing, false, now);
            // Clocks that arrived during this block.
            while next_tick < ticks.len() && ticks[next_tick].0 < now + BLOCK_NS {
                let t = ticks[next_tick].0;
                if !started {
                    started = true;
                    for c in f.on_start() {
                        bus.send(c.command());
                    }
                }
                let snap = pos.snapshot();
                for c in f.on_tick(t, |t| engine_beats_at(&snap, t)) {
                    if let InCmd::Tempo(b) = c {
                        tempo_changes.push((next_tick, b));
                    }
                    bus.send(c.command());
                }
                next_tick += 1;
            }
            // Where is the master vs the engine at the end of this block?
            if next_tick > 24 * 20 {
                let m = &ticks[next_tick - 1];
                let engine_at_tick = {
                    let tr = engine.transport();
                    let block_end_ns = now + BLOCK_NS;
                    tr.beats + (m.0 as f64 - block_end_ns as f64) * tr.bpm / 60.0e9
                };
                worst_after_lock = worst_after_lock.max((m.1 - engine_at_tick).abs());
            }
            now += BLOCK_NS;
            block += 1;
        }
        let _ = block;
        let tr = engine.transport();
        assert!(tr.playing, "Start played the engine");
        assert!((tr.bpm - 150.0).abs() < 0.3, "the engine follows the new tempo: {} ({tempo_changes:?})", tr.bpm);
        assert!(tempo_changes.iter().any(|(_, b)| (*b - 120.0).abs() < 0.3));
        // 0.0012 beat = 0.6 ms at 120 BPM, 0.5 ms at 150: well under what a
        // player or a plugin's tempo sync could tell.
        assert!(worst_after_lock < 0.0012, "worst phase error {worst_after_lock} beats");
    }

    #[test]
    fn a_phase_error_over_a_quarter_beat_is_snapped_whole_and_a_small_one_a_share_at_a_time() {
        let run = |off: f64| {
            let mut f = Follower::default();
            f.on_start();
            let mut nudges = Vec::new();
            for i in 0..12u64 {
                for c in f.on_tick(1_000_000_000 + i * 20_000_000, |_| Some(i as f64 / PPQN + off)) {
                    if let InCmd::Nudge(d) = c {
                        nudges.push(d);
                    }
                }
            }
            nudges
        };
        // An engine a beat behind: one whole snap.
        assert!(run(-1.0)[0] > 0.9);
        // 0.01 beat off: a share per clock, never the whole error.
        let small = run(-0.01);
        assert!(small.iter().all(|d| *d > 0.0 && *d < 0.01), "{small:?}");
        // Inside the dead band nothing moves.
        assert!(run(-0.0001).is_empty());
        // No engine position (stopped) means no phase work at all.
        let mut f = Follower::default();
        f.on_start();
        assert!(f.on_tick(1, |_| None).is_empty());
    }

    #[test]
    fn a_nudge_moves_a_playing_engines_clock_and_never_a_stopped_one() {
        let mut g = Graph::default();
        g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(g, drain, SR, Vec::new());
        let mut buf = vec![0.0f32; BLOCK * 2];
        bus.send(Command::NudgeBeats { delta: 3.0 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.transport().beats, 0.0, "stopped: not moved");
        bus.send(Command::TransportPlay);
        engine.render(&mut buf, 2);
        let before = engine.transport().beats;
        bus.send(Command::NudgeBeats { delta: 0.25 });
        engine.render(&mut buf, 2);
        let moved = engine.transport().beats - before;
        let own = BLOCK as f64 * engine.transport().bpm / (60.0 * SR);
        assert!((moved - 0.25 - own).abs() < 1e-9, "{moved}");
        bus.send(Command::NudgeBeats { delta: -100.0 });
        engine.render(&mut buf, 2);
        assert!(engine.transport().beats >= 0.0);
        let _ = (PortKind::Audio, PortRef { node: 0, port: 0 });
    }

    #[test]
    fn the_status_names_what_is_followed() {
        let c = ClockInControl::new();
        c.set(Some(true), Some(Some("Roland SPD:SPD MIDI 1".into())));
        let v = c.status_json();
        assert_eq!(v["follow"], true);
        assert_eq!(v["input"], "Roland SPD:SPD MIDI 1");
        assert!(c.input_key("spd").unwrap_err().contains("no MIDI in port"));
        c.stats.lock().unwrap().inputs = vec!["Roland SPD:SPD MIDI 1".into(), "Roland SPD:SPD MIDI 2".into(), "Korg:Volca".into()];
        assert_eq!(c.input_key("volca").unwrap(), "Korg:Volca");
        assert!(c.input_key("spd").unwrap_err().contains("matches 2"));
    }
}
