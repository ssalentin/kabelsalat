//! `kabelsalat engine-bench`: what the audio thread costs, offline. No audio
//! device and no plugins, so two builds can be compared on any machine: every
//! line ends in an FNV hash of the output, which must not move when a change
//! claims to leave the sound alone.
//!
//! - `render`: the demo rack's shape (two sources, two delays), as before.
//! - `session`: a full live rack (4 rigs, 8 inputs, 20 effects, an aux bus
//!   with a reverb, a master insert, sends, pans), fed with chords and noise
//!   through the real input rings, publishing to a `GraphPub` like the app.
//! - `routing`: the same rack with every effect a Gain: what the engine
//!   itself costs around the units (routing, strips, publication, master).
//! - `idle`: 1 live rig vs 1 live + 7 idle rigs (real Surge XT + Reverb, #206).
//! - one line per built-in unit, the master meter and the master limiter.
//!
//! Times are per frame (median of `REPS` runs, with the range) and per block:
//! `worst` is the slowest block of a run, the lowest of the runs (what the
//! code needs at worst, not what the scheduler did to one run).

use super::probe::{self, Probe};
use super::{GraphEngine, GraphPub, MasterLimiter};
use crate::control::{command_bus, name_bytes, Command, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef, SendSource};
use std::sync::{Arc, Mutex};

const REPS: usize = 9;
const SECONDS: usize = 20;
/// The session scenes run 8 feeds; shorter runs keep the whole bench brief.
const SESSION_SECONDS: usize = 5;
const SR: f64 = 48000.0;

fn fnv(hash: &mut u64, s: f32) {
    *hash = (*hash ^ s.to_bits() as u64).wrapping_mul(0x100000001b3);
}

/// (median, lowest, highest).
fn median(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.total_cmp(b));
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

/// A live rack without plugins. `plain` swaps every effect for a Gain.
fn session(plain: bool) -> Probe {
    let mut g = Graph::default();
    g.reserve(64, 256);
    let mut edge = 0u32;
    let mut wire = |g: &mut Graph, from: NodeId, to: NodeId, port: u8| {
        edge += 1;
        probe::wire(g, edge, from, to, port);
    };
    let fx = |g: &mut Graph, kind: DspKind, name: &str| {
        g.add_node(NodeKind::Dsp(if plain { DspKind::Gain } else { kind }), name_bytes(name))
    };
    let mut ins = Vec::new();
    let mut masters = Vec::new();
    for rig in 0..4u8 {
        let a = probe::audio_in(&mut g, "KEYS IN");
        let b = probe::audio_in(&mut g, "PAD IN");
        let chain_a = [fx(&mut g, DspKind::Filter, "FILTER"), fx(&mut g, DspKind::Saturation, "DRIVE"), fx(&mut g, DspKind::Delay, "DELAY")];
        let chain_b = [fx(&mut g, DspKind::Eq3, "EQ"), fx(&mut g, DspKind::Reverb, "REVERB")];
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        for (src, chain) in [(a, &chain_a[..]), (b, &chain_b[..])] {
            let mut prev = src;
            for &u in chain {
                wire(&mut g, prev, u, 0);
                prev = u;
            }
            wire(&mut g, prev, master, 0);
            wire(&mut g, prev, master, 1);
        }
        // The pad is also heard dry.
        wire(&mut g, b, master, 0);
        for n in [a, b, master].into_iter().chain(chain_a).chain(chain_b) {
            g.set_rig(n, rig);
        }
        ins.push((a, b));
        masters.push(master);
    }
    let bus = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("BUS"));
    let hall = fx(&mut g, DspKind::Reverb, "HALL");
    wire(&mut g, bus, hall, 0);
    wire(&mut g, hall, masters[0], 0);
    let glue = fx(&mut g, DspKind::Limiter, "GLUE");
    wire(&mut g, masters[0], glue, 0);
    let click = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
    wire(&mut g, click, masters[0], 0);

    let mut p = Probe::new(g);
    p.engine.set_graph_pub(Arc::new(Mutex::new(GraphPub::default())));
    for (rig, &(a, b)) in ins.iter().enumerate() {
        p.feed("keys", a, probe::chord(0.04));
        p.feed("pad", b, probe::pink_noise(7 + rig as u64));
        let pan = [-0.4, 0.4, -0.15, 0.15][rig];
        p.send(Command::SetStripMix { node: a, pan: Some(pan), mute: None, solo: None });
        p.send(Command::SetRigMix { rig: rig as u8, vol: Some(0.8), pan: Some(-pan * 0.5), mute: None, solo: None });
        p.send(Command::SetSend { from: SendSource::Rig(rig as u8), bus, level: Some(0.3), pre: Some(false) });
        p.send(Command::SetSend { from: SendSource::Node(b), bus, level: Some(0.2), pre: Some(true) });
    }
    p.send(Command::TransportPlay);
    p
}

fn bench_session(label: &str, plain: bool) {
    let frames = crate::bridge::BUFFER_FRAMES as usize;
    let blocks = SESSION_SECONDS * SR as usize / frames;
    let (mut per_frame, mut worst, mut p999) = (Vec::new(), f64::MAX, Vec::new());
    let (mut hash, mut peak, mut units) = (0u64, 0.0f32, 0);
    let mut times = vec![0u64; blocks];
    for _ in 0..REPS {
        let mut p = session(plain);
        units = p.engine.dsp.len();
        let mut out = vec![0.0f32; frames * 2];
        p.settle(probe::SETTLE_BLOCKS);
        hash = 0xcbf29ce484222325;
        peak = 0.0;
        for t in times.iter_mut() {
            *t = p.render_timed(std::hint::black_box(&mut out)).as_nanos() as u64;
            for &s in &out {
                fnv(&mut hash, s);
                peak = peak.max(s.abs());
            }
        }
        per_frame.push(times.iter().sum::<u64>() as f64 / (blocks * frames) as f64);
        times.sort_unstable();
        worst = worst.min(times[blocks - 1] as f64);
        p999.push(times[blocks * 999 / 1000] as f64);
    }
    let (med, lo, hi) = median(per_frame);
    let (p999, _, _) = median(p999);
    let block_us = frames as f64 / SR * 1e6;
    println!(
        "{label:<7} units={units:<2} median={med:7.2} ns/frame  range={lo:.2}..{hi:.2}  block={:6.2} us ({:.2}% of {frames})  p99.9={:6.2} us  worst={:6.2} us  peak={peak:.3}  hash={hash:016x}",
        med * frames as f64 / 1000.0,
        med * frames as f64 / 1000.0 / block_us * 100.0,
        p999 / 1000.0,
        worst / 1000.0,
    );
}

/// One built-in unit on a chord, at its default settings.
fn bench_unit(kind: DspKind) {
    let frames = crate::bridge::BUFFER_FRAMES as usize;
    let blocks = SECONDS * SR as usize / frames / 4;
    let input: Vec<f32> = probe::chord(0.1).take(blocks * frames).flat_map(|s| [s, s * 0.7]).collect();
    let mut runs = Vec::with_capacity(REPS);
    let mut hash = 0u64;
    let mut buf = vec![0.0f32; frames * 2];
    for _ in 0..REPS {
        let mut proc = crate::dsp::build_processor(&kind, SR);
        hash = 0xcbf29ce484222325;
        let mut spent = std::time::Duration::ZERO;
        for block in input.chunks_exact(frames * 2) {
            buf.copy_from_slice(block);
            let t = std::time::Instant::now();
            proc.process(std::hint::black_box(&mut buf), frames);
            spent += t.elapsed();
            for &s in &buf {
                fnv(&mut hash, s);
            }
        }
        runs.push(spent.as_nanos() as f64 / (blocks * frames) as f64);
    }
    let (med, lo, hi) = median(runs);
    println!("unit    {:<12} median={med:7.2} ns/frame  range={lo:.2}..{hi:.2}  hash={hash:016x}", format!("{kind:?}"));
}

/// The master meter (loudness, true peak, bands) on a chord.
fn bench_meter() {
    let frames = crate::bridge::BUFFER_FRAMES as usize;
    let blocks = SECONDS * SR as usize / frames / 4;
    let input: Vec<f32> = probe::chord(0.1).take(blocks * frames).flat_map(|s| [s, s * 0.7]).collect();
    let mut runs = Vec::with_capacity(REPS);
    let mut hash = 0u64;
    for _ in 0..REPS {
        let mut meter = crate::meter::MasterMeter::new(SR);
        let t = std::time::Instant::now();
        for block in input.chunks_exact(frames * 2) {
            meter.process(std::hint::black_box(block), 2);
        }
        runs.push(t.elapsed().as_nanos() as f64 / (blocks * frames) as f64);
        let m = meter.snapshot();
        hash = 0xcbf29ce484222325;
        for v in [m.momentary_lufs, m.short_term_lufs, m.integrated_lufs, m.true_peak_db, m.true_peak_max_db] {
            fnv(&mut hash, v);
        }
        for v in m.band_db {
            fnv(&mut hash, v);
        }
    }
    let (med, lo, hi) = median(runs);
    println!("meter   {:<12} median={med:7.2} ns/frame  range={lo:.2}..{hi:.2}  hash={hash:016x}", "master");
}

struct IdleRep {
    ns_per_frame: f64,
    p999_ns: f64,
    worst_ns: f64,
    peak: f32,
    hash: u64,
    sleeping_after_settle: u32,
    sleeping_after_play: u32,
}

struct IdleStats {
    med_ns: f64,
    lo_ns: f64,
    hi_ns: f64,
    p999_ns: f64,
    worst_ns: f64,
    peak: f32,
    hash: u64,
    sleeping_after_settle: u32,
    sleeping_after_play: u32,
}

fn run_idle_rep(total_rigs: usize, state: Option<&[u8]>, frames: usize) -> anyhow::Result<IdleRep> {
    use crate::control::{ClapId, RigZone, MAX_RIGS, SURGE_PLUGIN_ID};
    use crate::host::{ClapVoice, ClapVoiceMain};
    use crate::matrix::SourceId;

    const IDLE_SECONDS: usize = 3;
    let settle_blocks = (2.5 * SR / frames as f64).ceil() as usize;
    let blocks = IDLE_SECONDS * SR as usize / frames;
    let cycle_blocks = (0.75 * SR / frames as f64).round() as usize;
    let on_blocks = (0.50 * SR / frames as f64).round() as usize;

    let mut g = Graph::default();
    g.reserve(total_rigs * 4, total_rigs * 4);
    let mut voices: Vec<(NodeId, ClapVoice)> = Vec::with_capacity(total_rigs);
    let mut mains: Vec<ClapVoiceMain> = Vec::with_capacity(total_rigs);
    let mut eid = 1u32;
    for rig in 0..total_rigs {
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) }, name_bytes("SURGE"));
        let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        for id in [keys, synth, verb, master] {
            g.set_rig(id, rig as u8);
        }
        g.connect_with_id(eid, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
        g.connect_with_id(eid + 1, PortRef { node: synth, port: 0 }, PortRef { node: verb, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 2, PortRef { node: verb, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 3, PortRef { node: verb, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        eid += 4;
        let (v, mut m) = ClapVoice::new_plugin(SR, SURGE_PLUGIN_ID)?;
        if let Some(blob) = state {
            m.load_state(blob)?;
        }
        voices.push((synth, v));
        mains.push(m);
    }

    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, voices);
    engine.set_graph_pub(Arc::new(Mutex::new(GraphPub::default())));

    let mut out = vec![0.0f32; frames * 2];
    for _ in 0..4 {
        engine.render(&mut out, 2);
    }
    let mut zones = [RigZone { live: false, ..RigZone::OPEN }; MAX_RIGS];
    zones[0] = RigZone::OPEN;
    assert!(bus.send(Command::SetRigZones { zones }));
    for _ in 0..settle_blocks {
        engine.render(&mut out, 2);
    }
    let sleeping_after_settle = engine.sleeping_rigs_mask().count_ones();
    if total_rigs > 1 && sleeping_after_settle != (total_rigs - 1) as u32 {
        anyhow::bail!(
            "expected {} sleeping idle rigs after settle, got {sleeping_after_settle} (mask={:#b})",
            total_rigs - 1,
            engine.sleeping_rigs_mask()
        );
    }

    let mut times = vec![0u64; blocks];
    let mut hash = 0xcbf29ce484222325u64;
    let mut peak = 0.0f32;
    for (b, t) in times.iter_mut().enumerate() {
        let phase = b % cycle_blocks;
        if phase == 0 {
            for note in [60u8, 64, 67] {
                let _ = bus.send(Command::NoteOn { note, velocity: 0.85, source: SourceId::Keys, channel: 0 });
            }
        } else if phase == on_blocks {
            for note in [60u8, 64, 67] {
                let _ = bus.send(Command::NoteOff { note, source: SourceId::Keys, channel: 0 });
            }
        }
        let t0 = std::time::Instant::now();
        engine.render(std::hint::black_box(&mut out), 2);
        *t = t0.elapsed().as_nanos() as u64;
        for &s in &out {
            fnv(&mut hash, s);
            peak = peak.max(s.abs());
        }
    }
    let sleeping_after_play = engine.sleeping_rigs_mask().count_ones();
    if total_rigs > 1 && sleeping_after_play != (total_rigs - 1) as u32 {
        anyhow::bail!(
            "expected {} sleeping idle rigs after play, got {sleeping_after_play} (mask={:#b})",
            total_rigs - 1,
            engine.sleeping_rigs_mask()
        );
    }
    let ns_per_frame = times.iter().sum::<u64>() as f64 / (blocks * frames) as f64;
    times.sort_unstable();
    let p999_ns = times[blocks * 999 / 1000] as f64;
    let worst_ns = times[blocks - 1] as f64;
    drop(engine);
    drop(mains);
    Ok(IdleRep { ns_per_frame, p999_ns, worst_ns, peak, hash, sleeping_after_settle, sleeping_after_play })
}

fn summarize_idle(reps: &[IdleRep]) -> IdleStats {
    let (med_ns, lo_ns, hi_ns) = median(reps.iter().map(|r| r.ns_per_frame).collect());
    let (p999_ns, _, _) = median(reps.iter().map(|r| r.p999_ns).collect());
    let worst_ns = reps.iter().map(|r| r.worst_ns).fold(f64::MAX, f64::min);
    let last = reps.last().expect("at least one rep");
    IdleStats {
        med_ns,
        lo_ns,
        hi_ns,
        p999_ns,
        worst_ns,
        peak: last.peak,
        hash: last.hash,
        sleeping_after_settle: last.sleeping_after_settle,
        sleeping_after_play: last.sleeping_after_play,
    }
}

/// 1 live rig vs 1 live + 7 idle rigs (real Surge XT + built-in Reverb each, #206).
fn bench_idle() -> anyhow::Result<()> {
    if let Err(e) = crate::host::ClapVoice::new(SR) {
        println!("idle    SKIP — Surge XT is not installed ({e})");
        return Ok(());
    }
    const IDLE_REPS: usize = 5;
    let frames = crate::bridge::BUFFER_FRAMES as usize;
    let block_us = frames as f64 / SR * 1e6;
    let state = surge_piano_state();
    // Warm up plugin library once before alternating runs.
    let _ = run_idle_rep(1, state.as_deref(), frames)?;
    let mut one_reps = Vec::with_capacity(IDLE_REPS);
    let mut eight_reps = Vec::with_capacity(IDLE_REPS);
    for _ in 0..IDLE_REPS {
        one_reps.push(run_idle_rep(1, state.as_deref(), frames)?);
        eight_reps.push(run_idle_rep(8, state.as_deref(), frames)?);
    }
    let one = summarize_idle(&one_reps);
    let eight = summarize_idle(&eight_reps);

    for (label, st, sleep_info) in [
        ("1 live", &one, String::new()),
        ("1+7 idle", &eight, format!("  sleeping={}/7", eight.sleeping_after_play.min(eight.sleeping_after_settle))),
    ] {
        let bus = st.med_ns * frames as f64 / 1000.0;
        println!(
            "idle    {label:<10} median={:7.2} ns/frame  range={:.2}..{:.2}  block={bus:6.2} us ({:.2}% of {frames})  p99.9={:6.2} us  worst={:6.2} us  peak={:.3}  hash={:016x}{sleep_info}",
            st.med_ns,
            st.lo_ns,
            st.hi_ns,
            bus / block_us * 100.0,
            st.p999_ns / 1000.0,
            st.worst_ns / 1000.0,
            st.peak,
            st.hash,
        );
    }
    if one.hash != eight.hash {
        anyhow::bail!("1+7 idle hash {:016x} diverged from 1 live {:016x}", eight.hash, one.hash);
    }
    let one_us = one.med_ns * frames as f64 / 1000.0;
    let eight_us = eight.med_ns * frames as f64 / 1000.0;
    let extra_us = eight_us - one_us;
    println!(
        "idle    overhead   +{extra_us:6.2} us/block (+{:.2}% of {frames})  {:.2} us/idle rig  ({:.2}x of 1 live)",
        extra_us / block_us * 100.0,
        extra_us / 7.0,
        eight_us / one_us.max(0.001),
    );
    Ok(())
}

pub fn bench() -> anyhow::Result<()> {
    // `engine-bench session` / `routing` / `units` / `shared`: only that part.
    let only = std::env::args().nth(2);
    if only.as_deref() == Some("shared-probe") {
        let n: usize = std::env::args()
            .nth(3)
            .ok_or_else(|| anyhow::anyhow!("shared-probe needs an instance count"))?
            .parse()?;
        return shared_probe(n);
    }
    let part = |name: &str| only.as_deref().is_none_or(|o| o == name);
    if part("render") {
        bench_render()?;
    }
    if part("session") {
        bench_session("session", false);
    }
    if part("routing") {
        bench_session("routing", true);
    }
    if part("units") {
        for kind in [DspKind::Gain, DspKind::Pan, DspKind::Eq3, DspKind::Filter, DspKind::Limiter, DspKind::Saturation, DspKind::Delay, DspKind::Reverb, DspKind::Bus] {
            bench_unit(kind);
        }
        bench_meter();
    }
    if part("shared") {
        bench_shared()?;
    }
    if part("idle") {
        bench_idle()?;
    }
    Ok(())
}

/// Memory snapshot from `/proc/self/smaps_rollup` in KiB.
#[derive(Clone, Copy, Default)]
struct MemSample {
    rss_kib: u64,
    priv_dirty_kib: u64,
    clean_kib: u64,
}

fn read_mem() -> MemSample {
    let Ok(text) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return MemSample::default();
    };
    let mut m = MemSample::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(val) = parts.next().and_then(|v| v.parse::<u64>().ok()) else { continue };
        match key {
            "Rss:" => m.rss_kib = val,
            "Private_Dirty:" => m.priv_dirty_kib = val,
            "Shared_Clean:" | "Private_Clean:" => m.clean_kib += val,
            _ => {}
        }
    }
    m
}

/// Process user+sys CPU time in ms from `/proc/self/stat`.
fn read_cpu_ms() -> f64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(end_comm) = text.rfind(')') else { return 0.0 };
    let fields: Vec<&str> = text[end_comm + 2..].split_whitespace().collect();
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (utime + stime) as f64 * 10.0 // Linux USER_HZ = 100 → 10 ms/tick
}

/// Load a real Surge XT patch blob once before timing so disk I/O for the
/// `.fxp` file does not mix with plugin instantiation.
fn surge_piano_state() -> Option<Vec<u8>> {
    let listed = crate::host::list_surge_patches();
    let patch = listed
        .iter()
        .find(|p| p.name.to_ascii_lowercase().contains("piano"))
        .or_else(|| listed.iter().find(|p| p.name == "Init Sine"))
        .or_else(|| listed.first())
        .map(|p| p.path.clone());
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(|d| std::path::PathBuf::from(d).join("surge-xt/patches_factory/Templates/Init Sine.fxp"));
    let path = patch.or_else(|| xdg.filter(|p| p.exists()))?;
    crate::host::fxp_to_state(&std::fs::read(&path).ok()?).ok()
}

/// Isolated single-process probe for `bench_shared`: loads `n` hosted Surge XT
/// instances for a 20-song show (`n == 1` shares one rig across all 20 songs;
/// `n == 20` hosts one rig per song), measures load wall/CPU time and RSS /
/// Private_Dirty memory delta, and proves every hosted instance makes sound.
fn shared_probe(n: usize) -> anyhow::Result<()> {
    use crate::control::{ClapId, SURGE_PLUGIN_ID};
    use crate::host::{ClapVoice, ClapVoiceMain};
    use crate::matrix::SourceId;
    use crate::setlist::{Part, PartRig, Setlist, Song};

    const SONGS: usize = 20;
    let frames = crate::bridge::BUFFER_FRAMES as usize;
    let patch = surge_piano_state();
    let mem0 = read_mem();
    let cpu0 = read_cpu_ms();
    let t0 = std::time::Instant::now();

    let mut g = Graph::default();
    g.reserve(n * 4, n * 4);
    let mut voices: Vec<(NodeId, ClapVoice)> = Vec::with_capacity(n);
    let mut mains: Vec<ClapVoiceMain> = Vec::with_capacity(n);
    let mut synth_nodes: Vec<NodeId> = Vec::with_capacity(n);
    let mut errors = 0usize;
    let mut eid = 1u32;
    for rig in 0..n {
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) }, name_bytes("PIANO"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        for id in [keys, synth, master] {
            g.set_rig(id, rig as u8);
        }
        g.connect_with_id(eid, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
        g.connect_with_id(eid + 1, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        eid += 3;
        match ClapVoice::new_plugin(SR, SURGE_PLUGIN_ID) {
            Ok((v, mut m)) => {
                if let Some(state) = &patch {
                    if m.load_state(state).is_err() {
                        errors += 1;
                    }
                }
                voices.push((synth, v));
                mains.push(m);
                synth_nodes.push(synth);
            }
            Err(_) => {
                errors += 1;
            }
        }
    }

    let mut sl = Setlist::default();
    for s in 0..SONGS {
        let rig = if n == 1 { 0 } else { (s % n) as u8 };
        let pr = PartRig {
            rig,
            key_lo: if s % 2 == 0 { 0 } else { 48 },
            key_hi: 127,
            level: 0.75,
            pan: Some(-0.4 + 0.8 * (s as f32 / (SONGS - 1) as f32)),
            ..PartRig::default()
        };
        sl.songs.push(Song::new(format!("Song {}", s + 1), vec![Part::new("Main", vec![pr])]));
    }
    sl.normalize();
    let hosted = sl.show_instances(&g).hosted;

    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, voices);
    let mut buf = vec![0.0f32; frames * 2];
    // Render 2 blocks so Surge finishes audio-thread state application & lazy allocs.
    for _ in 0..2 {
        engine.render(&mut buf, 2);
    }
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let cpu_ms = (read_cpu_ms() - cpu0).max(0.0);
    let mem1 = read_mem();

    // Outside the timed region: verify every hosted instance actually produces audio.
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys, channel: 0 });
    let mut peaks = vec![0.0f32; synth_nodes.len()];
    for _ in 0..16 {
        engine.render(&mut buf, 2);
        for (i, &node) in synth_nodes.iter().enumerate() {
            if let Some(out) = engine.node_out(node) {
                let p = out[..frames * 2].iter().fold(0.0f32, |m, s| m.max(s.abs()));
                peaks[i] = peaks[i].max(p);
            }
        }
    }
    let sounding = peaks.iter().filter(|&&p| p > 1e-4).count();

    println!(
        "PROBE n={n} hosted={hosted} sounding={sounding} errors={errors} load_ms={load_ms:.3} cpu_ms={cpu_ms:.3} rss_kib={} priv_dirty_kib={} clean_kib={}",
        mem1.rss_kib.saturating_sub(mem0.rss_kib),
        mem1.priv_dirty_kib.saturating_sub(mem0.priv_dirty_kib),
        mem1.clean_kib.saturating_sub(mem0.clean_kib),
    );
    drop(engine);
    drop(mains);
    Ok(())
}

#[derive(Clone, Copy)]
struct ProbeRow {
    hosted: usize,
    sounding: usize,
    errors: usize,
    load_ms: f64,
    cpu_ms: f64,
    rss_kib: u64,
    priv_dirty_kib: u64,
    clean_kib: u64,
}

fn run_shared_probe(exe: &std::path::Path, n: usize) -> anyhow::Result<ProbeRow> {
    let out = std::process::Command::new(exe)
        .args(["engine-bench", "shared-probe", &n.to_string()])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "shared-probe {n} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("PROBE "))
        .ok_or_else(|| anyhow::anyhow!("shared-probe {n} printed no PROBE line:\n{stdout}"))?;
    let mut row = ProbeRow { hosted: 0, sounding: 0, errors: 0, load_ms: 0.0, cpu_ms: 0.0, rss_kib: 0, priv_dirty_kib: 0, clean_kib: 0 };
    for tok in line.split_whitespace().skip(1) {
        let Some((k, v)) = tok.split_once('=') else { continue };
        match k {
            "hosted" => row.hosted = v.parse()?,
            "sounding" => row.sounding = v.parse()?,
            "errors" => row.errors = v.parse()?,
            "load_ms" => row.load_ms = v.parse()?,
            "cpu_ms" => row.cpu_ms = v.parse()?,
            "rss_kib" => row.rss_kib = v.parse()?,
            "priv_dirty_kib" => row.priv_dirty_kib = v.parse()?,
            "clean_kib" => row.clean_kib = v.parse()?,
            _ => {}
        }
    }
    Ok(row)
}

/// 20-song piano show: 1 shared Surge XT instance vs 20 separate instances (#275).
/// Runs each trial in a fresh subprocess, alternating 1 and 20, plus N=2 to
/// separate one-time dlopen/shared-table cost from per-instance marginal cost.
fn bench_shared() -> anyhow::Result<()> {
    if let Err(e) = crate::host::ClapVoice::new(SR) {
        println!("shared  SKIP — Surge XT is not installed ({e})");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    // Warm OS page cache so alternating trials see the same file cache state.
    let warmup = run_shared_probe(&exe, 1)?;
    anyhow::ensure!(warmup.errors == 0 && warmup.sounding == 1, "warmup probe failed: errors={} sounding={}", warmup.errors, warmup.sounding);

    const PAIRS: usize = 5;
    let mut r1 = Vec::with_capacity(PAIRS);
    let mut r20 = Vec::with_capacity(PAIRS);
    for _ in 0..PAIRS {
        r1.push(run_shared_probe(&exe, 1)?);
        r20.push(run_shared_probe(&exe, 20)?);
    }
    let mut r2 = Vec::with_capacity(3);
    for _ in 0..3 {
        r2.push(run_shared_probe(&exe, 2)?);
    }
    let check_all = |rows: &[ProbeRow], n: usize| -> anyhow::Result<()> {
        for r in rows {
            anyhow::ensure!(r.errors == 0, "n={n}: {} load errors", r.errors);
            anyhow::ensure!(r.hosted == n && r.sounding == n, "n={n}: hosted={} sounding={}", r.hosted, r.sounding);
        }
        Ok(())
    };
    check_all(&r1, 1)?;
    check_all(&r2, 2)?;
    check_all(&r20, 20)?;

    let mib = |kib: f64| kib / 1024.0;
    let summarize = |label: &str, rows: &[ProbeRow]| {
        let (lt_m, lt_lo, lt_hi) = median(rows.iter().map(|r| r.load_ms).collect());
        let (cpu_m, _, _) = median(rows.iter().map(|r| r.cpu_ms).collect());
        let (pd_m, pd_lo, pd_hi) = median(rows.iter().map(|r| mib(r.priv_dirty_kib as f64)).collect());
        let (rss_m, rss_lo, rss_hi) = median(rows.iter().map(|r| mib(r.rss_kib as f64)).collect());
        let (cl_m, _, _) = median(rows.iter().map(|r| mib(r.clean_kib as f64)).collect());
        println!(
            "shared  {label:<18} runs={}  load={lt_m:7.1} ms (range {lt_lo:.1}..{lt_hi:.1}, cpu={cpu_m:.0} ms)  priv_dirty={pd_m:6.1} MiB ({pd_lo:.1}..{pd_hi:.1})  rss={rss_m:6.1} MiB ({rss_lo:.1}..{rss_hi:.1})  clean={cl_m:.1} MiB  sounding={}/{}  errors=0",
            rows.len(),
            rows[0].sounding,
            rows[0].hosted,
        );
        (lt_m, pd_m, rss_m, cl_m)
    };
    println!(
        "engine-bench shared: 20-song Surge XT piano show, fresh subprocess per run, warm page cache (warmup 1-inst={:.1} ms)",
        warmup.load_ms
    );
    let (lt1, pd1, rss1, _) = summarize("1 inst (shared)", &r1);
    let (lt2, pd2, _rss2, _) = summarize("2 inst (control)", &r2);
    let (lt20, pd20, rss20, _) = summarize("20 inst (separate)", &r20);
    let marg_lt = (lt20 - lt1) / 19.0;
    let marg_pd = (pd20 - pd1) / 19.0;
    let marg_rss = (rss20 - rss1) / 19.0;
    println!(
        "shared  scaling: load {:.2}x faster ({lt20:.1} -> {lt1:.1} ms; marginal {marg_lt:.1} ms/inst vs 2-inst step {:.1} ms, fixed dlopen+init {:.1} ms)",
        lt20 / lt1.max(0.001),
        lt2 - lt1,
        (lt1 - marg_lt).max(0.0),
    );
    println!(
        "shared  memory:  priv_dirty {:.2}x smaller ({pd20:.1} -> {pd1:.1} MiB; marginal {marg_pd:.2} MiB/inst vs 2-inst step {:.2} MiB, fixed {:.1} MiB); rss {:.2}x ({rss20:.1} -> {rss1:.1} MiB, marginal {marg_rss:.2} MiB/inst)",
        pd20 / pd1.max(0.001),
        pd2 - pd1,
        (pd1 - marg_pd).max(0.0),
        rss20 / rss1.max(0.001),
    );
    Ok(())
}

/// The demo rack's shape at three buffer sizes, and the master limiter alone.
fn bench_render() -> anyhow::Result<()> {
    let sr = SR;
    let mut g = Graph::default();
    g.reserve(64, 256);
    let src_a = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("SRC A"));
    let delay_a = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY A"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
    let src_b = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("SRC B"));
    let delay_b = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY B"));
    let edges = [(3, src_a, delay_a, 0), (4, delay_a, master, 0), (5, src_a, master, 1),
                 (7, src_b, delay_b, 0), (8, delay_b, master, 0), (9, src_b, master, 2)];
    for (id, from, to, port) in edges {
        anyhow::ensure!(g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port }, PortKind::Audio));
    }

    println!("engine-bench: {SECONDS} s of audio per run, median of {REPS} runs");
    for frames in [128usize, 256, 512] {
        let blocks = SECONDS * sr as usize / frames;
        let mut runs = Vec::with_capacity(REPS);
        let (mut hash, mut peak) = (0u64, 0.0f32);
        for _ in 0..REPS {
            let (mut bus, drain) = command_bus();
            let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);
            bus.send(Command::LoadGraph { graph: g.clone() });
            bus.send(Command::TransportPlay);
            let mut buf = vec![0.0f32; frames * 2];
            hash = 0xcbf29ce484222325;
            peak = 0.0;
            let mut spent = std::time::Duration::ZERO;
            for _ in 0..blocks {
                let t = std::time::Instant::now();
                engine.render(std::hint::black_box(&mut buf), 2);
                spent += t.elapsed();
                for s in &buf {
                    hash = (hash ^ s.to_bits() as u64).wrapping_mul(0x100000001b3);
                    peak = peak.max(s.abs());
                }
            }
            runs.push(spent.as_nanos() as f64 / (blocks * frames) as f64);
        }
        let (med, lo, hi) = median(runs);
        println!(
            "render  buffer={frames:<4} median={med:7.2} ns/frame  range={lo:.2}..{hi:.2}  block={:6.2} us  peak={peak:.3}  hash={hash:016x}",
            med * frames as f64 / 1000.0,
        );
    }
    for (label, warm) in [("default warm", 0.15f32), ("warm 60%", 0.6)] {
        let drive = 1.0 + warm * 3.0;
        let n = SECONDS * sr as usize;
        let mut runs = Vec::with_capacity(REPS);
        let mut hash = 0u64;
        for _ in 0..REPS {
            let mut lim = MasterLimiter::new(sr);
            hash = 0xcbf29ce484222325;
            let t = std::time::Instant::now();
            for i in 0..n {
                let x = (i as f32 * 0.031).sin() * 0.9;
                let (l, r) = lim.process(std::hint::black_box(x), x * 0.5, drive, 0.75);
                hash = (hash ^ l.to_bits() as u64 ^ ((r.to_bits() as u64) << 32)).wrapping_mul(0x100000001b3);
            }
            runs.push(t.elapsed().as_nanos() as f64 / n as f64);
        }
        let (med, lo, hi) = median(runs);
        println!("limiter {label:<12} median={med:7.2} ns/frame  range={lo:.2}..{hi:.2}  hash={hash:016x}");
    }
    Ok(())
}
