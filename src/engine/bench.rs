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

pub fn bench() -> anyhow::Result<()> {
    // `engine-bench session` / `routing` / `units`: only that part.
    let only = std::env::args().nth(2);
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
