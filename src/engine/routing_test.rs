//! Routing utility units through the real engine (#308): the A/B switch and
//! crossfader's levels, the splitter summing back, and delay compensation
//! keeping parallel paths in phase. No plugins.

use super::probe::{Probe, Tap, MAX_LAG, SETTLE_BLOCKS};
use crate::analysis::{white, xcorr_peak};
use crate::control::{name_bytes, AudioInChannel, Command, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef};

fn audio_in(g: &mut Graph, name: &str) -> NodeId {
    g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes(name))
}

fn dsp(g: &mut Graph, kind: DspKind, name: &str) -> NodeId {
    g.add_node(NodeKind::Dsp(kind), name_bytes(name))
}

fn wire(g: &mut Graph, id: u32, from: NodeId, from_port: u8, to: NodeId, to_port: u8) {
    assert!(g.connect_with_id(id, PortRef { node: from, port: from_port }, PortRef { node: to, port: to_port }, PortKind::Audio));
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn corr(a: &[f32], b: &[f32]) -> f32 {
    xcorr_peak(a, b, 2 * MAX_LAG).r.abs()
}

/// A and B (independent noise) into a crossfader into Master; settled with
/// `mode`/`mix`. Returns (A, B, crossfader out, master L) of one run.
fn crossfade(mode: f64, mix: f64) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut g = Graph::default();
    let (a, b) = (audio_in(&mut g, "A"), audio_in(&mut g, "B"));
    let x = dsp(&mut g, DspKind::Crossfade, "XFADE");
    let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    wire(&mut g, 1, a, 0, x, 0);
    wire(&mut g, 2, b, 0, x, 1);
    wire(&mut g, 3, x, 0, m, 0);
    wire(&mut g, 4, x, 0, m, 1);
    let mut p = Probe::new(g);
    p.send(Command::SetParam { node: x, param: 1, value: mode });
    p.send(Command::SetParam { node: x, param: 0, value: mix });
    p.feed("A", a, white(3).map(|v| v * 0.1));
    p.feed("B", b, white(4).map(|v| v * 0.1));
    p.settle(SETTLE_BLOCKS);
    let run = p.run(32, &[Tap::Node(x), Tap::MasterL]);
    let mut f = run.feeds.into_iter();
    let (fa, fb) = (f.next().unwrap().1, f.next().unwrap().1);
    let mut t = run.taps.into_iter();
    (fa, fb, t.next().unwrap().1, t.next().unwrap().1)
}

#[test]
fn a_only_b_only_and_the_midpoint_have_the_stated_levels() {
    // The node tap is L+R of the unit's output: 2x one input's level.
    for (mode, mix, expect, what) in [
        (0.0, 0.0, 1.0, "equal power, A"),
        (0.0, 1.0, 1.0, "equal power, B"),
        (0.0, 0.5, 1.0, "equal power, midpoint: two uncorrelated inputs at -3 dB keep the level"),
        (0.5, 0.5, std::f64::consts::FRAC_1_SQRT_2, "linear, midpoint: -3 dB for uncorrelated sources"),
        (1.0, 0.25, 1.0, "switch below the midpoint is A"),
        (1.0, 0.75, 1.0, "switch above the midpoint is B"),
    ] {
        let (a, b, out, _) = crossfade(mode, mix);
        let reference = 2.0 * rms(&a);
        let got = rms(&out) / reference;
        assert!((got - expect).abs() < 0.05, "{what}: level {got:.3} of one input, expected {expect:.3}");
        let _ = &b;
    }
}

#[test]
fn only_the_chosen_input_is_heard_at_the_ends() {
    let (a, b, _, master) = crossfade(0.0, 0.0);
    assert!(corr(&a, &master) > 0.9 && corr(&b, &master) < 0.1, "A only");
    let (a, b, _, master) = crossfade(0.0, 1.0);
    assert!(corr(&b, &master) > 0.9 && corr(&a, &master) < 0.1, "B only");
    let (a, b, _, master) = crossfade(0.0, 0.5);
    assert!(corr(&a, &master) > 0.3 && corr(&b, &master) > 0.3, "both at the midpoint");
}

/// One noise into a splitter in `mode`; both outputs into Master L and R
/// (port 0 -> both masters' inputs, port 1 -> both). Master L vs the feed.
fn split_sum(mode: f64, freq: f64) -> (Vec<f32>, Vec<f32>) {
    let mut g = Graph::default();
    let a = audio_in(&mut g, "IN");
    let s = dsp(&mut g, DspKind::Split, "SPLIT");
    let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    wire(&mut g, 1, a, 0, s, 0);
    for (i, (fp, tp)) in [(0, 0), (0, 1), (1, 0), (1, 1)].into_iter().enumerate() {
        wire(&mut g, 2 + i as u32, s, fp, m, tp);
    }
    let mut p = Probe::new(g);
    p.send(Command::SetParam { node: s, param: 0, value: mode });
    p.send(Command::SetParam { node: s, param: 1, value: freq });
    p.feed("IN", a, white(7).map(|v| v * 0.1));
    p.settle(SETTLE_BLOCKS);
    let run = p.run(32, &[Tap::MasterL]);
    (run.feeds.into_iter().next().unwrap().1, run.taps.into_iter().next().unwrap().1)
}

#[test]
fn a_splitters_outputs_recombine_into_the_input() {
    // Band split is an all-pass sum (flat level, shifted phase), mid/side and
    // left/right sum exactly. Master L carries the input at one gain, so the
    // level ratio is the same in every mode; noise is flat, so the all-pass
    // keeps the RMS (within 0.5 dB) though not the waveform.
    let mut ratios = Vec::new();
    for (mode, name) in [(0.5, "mid/side"), (1.0, "left/right"), (0.0, "bands")] {
        let (input, master) = split_sum(mode, 0.2917);
        ratios.push((name, rms(&master) / rms(&input), corr(&input, &master)));
    }
    let base = ratios[0].1;
    for (name, r, c) in &ratios {
        assert!((20.0 * (r / base).log10()).abs() < 0.5, "{name}: level {r:.3} against mid/side {base:.3}");
        assert!(*c > 0.3, "{name}: correlation {c}");
    }
    // Mid/side and left/right keep the waveform exactly.
    assert!(ratios[0].2 > 0.95 && ratios[1].2 > 0.95, "{ratios:?}");
}

#[test]
fn a_crossfader_after_a_latent_path_stays_in_phase_with_its_other_input() {
    // Noise -> Limiter (240 frames) -> IN A, and the same noise -> IN B,
    // linear midpoint: the plan delays B by 240 frames, so the paths sum in
    // phase (one peak at the limiter's latency, near-unity correlation)
    // instead of a comb.
    let mut g = Graph::default();
    let a = audio_in(&mut g, "IN");
    let lim = dsp(&mut g, DspKind::Limiter, "LIM");
    let x = dsp(&mut g, DspKind::Crossfade, "XFADE");
    let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    wire(&mut g, 1, a, 0, lim, 0);
    wire(&mut g, 2, lim, 0, x, 0);
    wire(&mut g, 3, a, 0, x, 1);
    wire(&mut g, 4, x, 0, m, 0);
    wire(&mut g, 5, x, 0, m, 1);
    let mut p = Probe::new(g);
    p.send(Command::SetParam { node: x, param: 1, value: 0.5 });
    p.send(Command::SetParam { node: x, param: 0, value: 0.5 });
    p.feed("IN", a, white(9).map(|v| v * 0.1));
    p.settle(SETTLE_BLOCKS);
    let run = p.run(32, &[Tap::MasterL]);
    let peak = xcorr_peak(&run.feeds[0].1, &run.taps[0].1, 2 * MAX_LAG);
    assert_eq!(peak.lag_frames, 240, "{peak:?}");
    assert!(peak.r > 0.95, "the two paths sum in phase: {peak:?}");
}
