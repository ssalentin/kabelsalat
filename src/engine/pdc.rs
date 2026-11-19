//! Delay compensation (issue #13): parallel paths into one summing point
//! arrive in phase when one of them runs through a unit with a lookahead
//! (the Limiter's 5 ms), instead of 240 frames apart.
//!
//! The plan is a pure function of the graph and each node's latency. A
//! node's output latency is its own plus the largest output latency of its
//! audio inputs; at every summing point (an effect's input, a rig's master)
//! each input is delayed by the difference to the largest. The engine runs
//! effects in signal order and breaks a feedback loop at one cable, which
//! then carries the previous block; that cable neither counts nor gets a
//! line, so the plan ends. Master inserts run serially on the mix bus and
//! are not part of it.
//!
//! The engine replans on graph edits (`GraphEngine::publish_graph`) and when
//! a send starts or stops sounding, never per block;
//! with no latent unit the plan is empty and every sum is the plain sum.

use crate::control::{AuxSend, DspKind, Graph, NodeId, NodeKind, PortKind, SendSource};

/// The delays that align every summing point: (source, destination, frames).
///
/// Sends are not cables, so they come in beside the graph: an aux bus is as
/// late as the latest thing sent into it. A send of a source is as late as
/// that source; a send of a whole rig taps the rig's sum, which is already
/// aligned, so it is as late as that sum. What returns from a bus meets the
/// rig sums at the mix: early returns wait there, and when a return is later
/// than a rig's sum (a latent unit in the bus's chain) that rig waits after
/// its sends are tapped, as a `(master, master, frames)` delay.
pub fn compensation(graph: &Graph, sends: &[AuxSend], latency_of: impl Fn(NodeId) -> usize) -> Vec<(NodeId, NodeId, usize)> {
    let n = graph.nodes.len();
    let index = |id: NodeId| graph.nodes.iter().position(|x| x.id == id);
    let is_dsp = |i: usize| matches!(&graph.nodes[i].kind, NodeKind::Dsp(k) if *k != DspKind::Metronome);
    let is_lv2 = |i: usize| matches!(graph.nodes[i].kind, NodeKind::Lv2 { .. });
    let is_master = |i: usize| matches!(graph.nodes[i].kind, NodeKind::MasterOut);
    let is_bus = |i: usize| matches!(graph.nodes[i].kind, NodeKind::Dsp(DspKind::Bus));
    // Each source once per destination: the engine sums a source into a
    // destination once, however many cables join them. A cable out of a
    // master is an insert.
    let mut inputs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in graph.edges.iter().filter(|e| e.kind == PortKind::Audio) {
        let (Some(s), Some(d)) = (index(e.from.node), index(e.to.node)) else { continue };
        if s != d && !is_master(s) && (is_dsp(d) || is_lv2(d) || is_master(d)) && !inputs[d].contains(&s) {
            inputs[d].push(s);
        }
    }
    // When each node renders: sources first, then the effects in the order
    // `GraphEngine::next_effect` runs them (built-in DSP in graph order,
    // then LV2), then the masters.
    let effects: Vec<usize> = (0..n).filter(|&i| is_dsp(i)).chain((0..n).filter(|&i| is_lv2(i))).collect();
    let mut rank: Vec<usize> = (0..n).map(|i| if is_master(i) { usize::MAX } else { 0 }).collect();
    for step in 1..=effects.len() {
        let waits = |f: usize| effects.iter().any(|&g| g != f && rank[g] == 0 && inputs[f].contains(&g));
        let next = effects.iter().copied().find(|&f| rank[f] == 0 && !waits(f));
        let f = next.or_else(|| effects.iter().copied().find(|&f| rank[f] == 0)).unwrap_or(0);
        rank[f] = step;
    }
    // A cable from a node that renders later carries the previous block.
    for (d, ins) in inputs.iter_mut().enumerate() {
        ins.retain(|&s| rank[s] < rank[d]);
    }
    // The returns: a bus and what its cables lead to (`mark_after_bus`).
    let mut wet: Vec<bool> = (0..n).map(is_bus).collect();
    loop {
        let fed = (0..n).find(|&d| !wet[d] && !is_master(d) && inputs[d].iter().any(|&s| wet[s]));
        match fed {
            Some(d) => wet[d] = true,
            None => break,
        }
    }
    // What is sent into each bus: a source, or the master whose rig sum it
    // taps. A return cannot send.
    let mut sent: Vec<Vec<usize>> = vec![Vec::new(); n];
    for s in sends.iter().filter(|s| s.level > 0.0) {
        let from = match s.from {
            SendSource::Node(id) => index(id).filter(|&i| !wet[i]),
            SendSource::Rig(r) => (0..n).find(|&i| is_master(i) && graph.nodes[i].rig == r),
        };
        if let (Some(b), Some(f)) = (index(s.bus), from) {
            sent[b].push(f);
        }
    }
    struct Plan<'a> {
        inputs: &'a [Vec<usize>],
        sent: &'a [Vec<usize>],
        wet: &'a [bool],
        master: Vec<bool>,
        own: Vec<usize>,
        memo: Vec<Option<usize>>,
    }
    impl Plan<'_> {
        /// A master's rig sum: its latest dry input.
        fn dry(&mut self, m: usize) -> usize {
            let ins = self.inputs;
            ins[m].iter().filter(|&&s| !self.wet[s]).map(|&s| self.out(s)).max().unwrap_or(0)
        }
        /// Ranks rise along every cable left, sends lead from dry nodes
        /// into buses only, and a rig sum has dry inputs only: this ends.
        fn out(&mut self, i: usize) -> usize {
            if let Some(l) = self.memo[i] {
                return l;
            }
            let (ins, sent) = (self.inputs, self.sent);
            let cabled = ins[i].iter().map(|&s| self.out(s)).max().unwrap_or(0);
            let by_send = sent[i].iter().map(|&f| if self.master[f] { self.dry(f) } else { self.out(f) }).max().unwrap_or(0);
            let l = self.own[i] + cabled.max(by_send);
            self.memo[i] = Some(l);
            l
        }
    }
    let mut p = Plan {
        inputs: &inputs,
        sent: &sent,
        wet: &wet,
        master: (0..n).map(is_master).collect(),
        own: graph.nodes.iter().map(|x| latency_of(x.id)).collect(),
        memo: vec![None; n],
    };
    let mut plan = Vec::new();
    for (d, ins) in inputs.iter().enumerate() {
        let id = |i: usize| graph.nodes[i].id;
        if is_master(d) {
            let dry = p.dry(d);
            let mix = ins.iter().filter(|&&s| wet[s]).map(|&s| p.out(s)).max().unwrap_or(0).max(dry);
            for &s in ins {
                let target = if wet[s] { mix } else { dry };
                if p.out(s) < target {
                    plan.push((id(s), id(d), target - p.out(s)));
                }
            }
            if mix > dry {
                plan.push((id(d), id(d), mix - dry));
            }
        } else {
            let max = ins.iter().map(|&s| p.out(s)).max().unwrap_or(0);
            for &s in ins {
                if p.out(s) < max {
                    plan.push((id(s), id(d), max - p.out(s)));
                }
            }
        }
    }
    plan
}

/// A delay of `ring.len() / 2` stereo frames.
struct Line {
    src: NodeId,
    dest: NodeId,
    ring: Vec<f32>,
    pos: usize,
}

impl Line {
    fn frames(&self) -> usize {
        self.ring.len() / 2
    }

    #[inline]
    fn push(&mut self, l: f32, r: f32) -> (f32, f32) {
        let p = self.pos * 2;
        let out = (self.ring[p], self.ring[p + 1]);
        self.ring[p] = l;
        self.ring[p + 1] = r;
        self.pos += 1;
        if self.pos == self.frames() {
            self.pos = 0;
        }
        out
    }
}

/// Make `lines` follow `plan`; a line whose delay is unchanged keeps what it
/// holds (a replan for an unrelated edit must not drop 5 ms of audio).
fn refit(lines: &mut Vec<Line>, plan: &[(NodeId, NodeId, usize)]) {
    let key = |l: &Line| (l.src, l.dest, l.frames());
    if lines.len() == plan.len() && lines.iter().zip(plan).all(|(l, p)| key(l) == *p) {
        return;
    }
    let mut old = std::mem::take(lines);
    *lines = plan
        .iter()
        .map(|&(src, dest, frames)| match old.iter().position(|l| key(l) == (src, dest, frames)) {
            Some(i) => old.swap_remove(i),
            None => Line { src, dest, ring: vec![0.0; frames * 2], pos: 0 },
        })
        .collect();
}

/// The engine's delay lines: one per cable pair the plan delays, and one per
/// latent unit for its bypass.
#[derive(Default)]
pub struct Pdc {
    lines: Vec<Line>,
    bypass: Vec<Line>,
}

impl Pdc {
    /// Follow the plan for `graph`. Allocates only for lines that are new
    /// (graph edits, never per block).
    pub fn replan(&mut self, graph: &Graph, sends: &[AuxSend], latency_of: impl Fn(NodeId) -> usize) {
        let plan = compensation(graph, sends, &latency_of);
        let latent: Vec<_> = graph.nodes.iter().map(|n| (n.id, n.id, latency_of(n.id))).filter(|l| l.2 > 0).collect();
        refit(&mut self.lines, &plan);
        refit(&mut self.bypass, &latent);
    }

    /// Does any line end at `dest`? Then its sum must run every block, also
    /// at zero gain, or the line would replay stale audio later.
    pub fn feeds(&self, dest: NodeId) -> bool {
        self.lines.iter().any(|l| l.dest == dest)
    }

    /// Add `src`'s block (stereo interleaved, `out.len()` samples) into
    /// `out` as `dest` hears it: through its line when the plan has one,
    /// scaled by `gains`; `mono` takes the left channel to both sides (a
    /// cable into a MONO port). Allocation-free.
    #[inline]
    pub fn mix(&mut self, src: NodeId, dest: NodeId, buf: &[f32], mono: bool, (gl, gr): (f32, f32), out: &mut [f32]) {
        let frames = out.len() / 2;
        match self.lines.iter_mut().find(|l| l.src == src && l.dest == dest) {
            None if mono => {
                for i in 0..frames {
                    let m = buf[i * 2];
                    out[i * 2] += m * gl;
                    out[i * 2 + 1] += m * gr;
                }
            }
            None => {
                for i in 0..frames {
                    out[i * 2] += buf[i * 2] * gl;
                    out[i * 2 + 1] += buf[i * 2 + 1] * gr;
                }
            }
            Some(line) => {
                for i in 0..frames {
                    let l = buf[i * 2];
                    let (dl, dr) = line.push(l, if mono { l } else { buf[i * 2 + 1] });
                    out[i * 2] += dl * gl;
                    out[i * 2 + 1] += dr * gr;
                }
            }
        }
    }

    /// A rig's sum on its way into the mix, after its sends are tapped:
    /// delayed in place when a bus return is later than the sum.
    /// Allocation-free.
    pub fn rig_out(&mut self, master: NodeId, rig_buf: &mut [f32]) {
        let Some(line) = self.lines.iter_mut().find(|l| l.src == master && l.dest == master) else { return };
        for f in rig_buf.chunks_exact_mut(2) {
            (f[0], f[1]) = line.push(f[0], f[1]);
        }
    }

    /// A unit with a lookahead delays by it also while bypassed, so the
    /// paths it sums with stay aligned and a bypass toggle moves nothing in
    /// time. Bypassed: `out` becomes `input` that many frames late. Running:
    /// the line only keeps up with `input`, so a later bypass is seamless.
    /// A unit without latency has no line. Allocation-free.
    pub fn bypass(&mut self, node: NodeId, input: &[f32], out: &mut [f32], bypassed: bool) {
        let Some(line) = self.bypass.iter_mut().find(|l| l.src == node) else { return };
        for i in 0..input.len() / 2 {
            let (l, r) = line.push(input[i * 2], input[i * 2 + 1]);
            if bypassed {
                out[i * 2] = l;
                out[i * 2 + 1] = r;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, Command};
    use crate::control::SendSource;
    use crate::engine::probe::{lag_r_behind_l, split_paths, wire};

    fn dsp(g: &mut Graph, kind: DspKind, name: &str) -> NodeId {
        g.add_node(NodeKind::Dsp(kind), name_bytes(name))
    }

    /// The Limiter's latency, as `GraphEngine::replan_delays` reads it.
    fn lim_latency(g: &Graph) -> impl Fn(NodeId) -> usize + '_ {
        |id| match g.node(id).map(|n| &n.kind) {
            Some(NodeKind::Dsp(DspKind::Limiter)) => 240,
            _ => 0,
        }
    }

    #[test]
    fn the_plan_delays_the_faster_inputs_of_every_summing_point() {
        // In → Gain → Master and In → Limiter → Limiter → Master: the dry
        // side waits for both lookaheads, at the master only.
        let mut g = Graph::default();
        let src = dsp(&mut g, DspKind::Metronome, "SRC");
        let dry = dsp(&mut g, DspKind::Gain, "DRY");
        let (a, b) = (dsp(&mut g, DspKind::Limiter, "LIM A"), dsp(&mut g, DspKind::Limiter, "LIM B"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, src, dry, 0);
        wire(&mut g, 2, src, a, 0);
        wire(&mut g, 3, a, b, 0);
        wire(&mut g, 4, dry, master, 0);
        wire(&mut g, 5, b, master, 0);
        // A second cable of the same pair (L and R) is one input.
        wire(&mut g, 6, dry, master, 1);
        assert_eq!(compensation(&g, &[], lim_latency(&g)), vec![(dry, master, 480)]);
        // A source straight into the second limiter as well: it is 240
        // early there, and the master sees only the chain's 480.
        wire(&mut g, 7, src, b, 0);
        assert_eq!(compensation(&g, &[], lim_latency(&g)), vec![(src, b, 240), (dry, master, 480)]);
        // Nothing latent: no line at all.
        assert!(compensation(&g, &[], |_| 0).is_empty());
    }

    #[test]
    fn a_feedback_loop_ends_the_plan_where_the_engine_breaks_it() {
        // SRC → A (limiter) → B → A loops; B → Master beside SRC → Master.
        let mut g = Graph::default();
        let src = dsp(&mut g, DspKind::Metronome, "SRC");
        let a = dsp(&mut g, DspKind::Limiter, "A");
        let b = dsp(&mut g, DspKind::Gain, "B");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, src, a, 0);
        wire(&mut g, 2, a, b, 0);
        wire(&mut g, 3, b, a, 0);
        wire(&mut g, 4, b, master, 0);
        wire(&mut g, 5, src, master, 1);
        // A runs first (graph order), so B → A carries the previous block
        // and does not count; the master aligns the dry source with B.
        assert_eq!(compensation(&g, &[], lim_latency(&g)), vec![(src, master, 240)]);
        // A loop of two limiters with nothing else.
        let mut g = Graph::default();
        let a = dsp(&mut g, DspKind::Limiter, "A");
        let b = dsp(&mut g, DspKind::Limiter, "B");
        wire(&mut g, 1, a, b, 0);
        wire(&mut g, 2, b, a, 0);
        assert!(compensation(&g, &[], lim_latency(&g)).is_empty());
    }

    #[test]
    fn master_inserts_and_unsummed_nodes_are_outside_the_plan() {
        // Master → Limiter is an insert (serial on the mix bus); a
        // metronome does not sum what is wired into it.
        let mut g = Graph::default();
        let src = dsp(&mut g, DspKind::Metronome, "SRC");
        let lim = dsp(&mut g, DspKind::Limiter, "LIM");
        let click = dsp(&mut g, DspKind::Metronome, "CLICK");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, src, master, 0);
        wire(&mut g, 2, master, lim, 0);
        wire(&mut g, 3, lim, click, 0);
        wire(&mut g, 4, src, click, 0);
        assert!(compensation(&g, &[], lim_latency(&g)).is_empty());
    }

    #[test]
    fn a_limiter_on_one_of_two_parallel_paths_stays_in_phase_with_the_dry_one() {
        let (mut probe, _) = split_paths(&[DspKind::Limiter]);
        let p = lag_r_behind_l(&mut probe);
        assert_eq!(p.lag_frames, 0, "the limited path is behind the dry one: {p:?}");
        assert!(p.r > 0.95, "both sides carry the same noise: {p:?}");
        // Two in series wait for both lookaheads.
        let (mut probe, _) = split_paths(&[DspKind::Limiter, DspKind::Gain, DspKind::Limiter]);
        let p = lag_r_behind_l(&mut probe);
        assert_eq!(p.lag_frames, 0, "two limiters in series: {p:?}");
        // No latent unit: no line, and the paths were never apart.
        let (probe, _) = split_paths(&[DspKind::Gain]);
        assert!(probe.engine.pdc.lines.is_empty() && probe.engine.pdc.bypass.is_empty());
    }

    /// Audio In → `dry` units → Master, and an aux bus → `ret` units →
    /// Master, fed by `from` (the Audio In, or the whole rig when None).
    /// Returns the probe and how the master's left side lines up with the
    /// fed noise: in phase the two paths read as one copy of it (r ≈ 1),
    /// 5 ms apart as two (r ≈ 0.7).
    fn dry_and_return(dry: &[DspKind], ret: &[DspKind], from_input: bool) -> (Vec<(NodeId, NodeId, usize)>, crate::analysis::XcorrPeak) {
        use crate::control::AudioInChannel;
        use crate::engine::probe::{Probe, Tap, MAX_LAG, SETTLE_BLOCKS};
        let mut g = Graph::default();
        let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let bus = dsp(&mut g, DspKind::Bus, "BUS");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut id = 0;
        for (start, chain) in [(ain, dry), (bus, ret)] {
            let mut prev = start;
            for k in chain {
                let u = dsp(&mut g, k.clone(), "UNIT");
                id += 1;
                wire(&mut g, id, prev, u, 0);
                prev = u;
            }
            wire(&mut g, id + 1, prev, master, 0);
            wire(&mut g, id + 2, prev, master, 1);
            id += 2;
        }
        let from = if from_input { SendSource::Node(ain) } else { SendSource::Rig(0) };
        let mut probe = Probe::new(g);
        probe.send(Command::SetSend { from, bus, level: Some(1.0), pre: Some(false) });
        probe.feed("noise", ain, crate::analysis::white(9).map(|x| x * 0.1));
        probe.settle(SETTLE_BLOCKS);
        let run = probe.run(32, &[Tap::MasterL]);
        let plan = compensation(probe.engine.graph(), &probe.engine.sends, lim_latency(probe.engine.graph()));
        (plan, crate::analysis::xcorr_peak(&run.feeds[0].1, &run.taps[0].1, 2 * MAX_LAG))
    }

    /// A send is not a cable, and still its return meets the dry sound in
    /// phase: a whole-rig send is as late as the rig's sum (delaying its
    /// return again put it 5 ms behind), a send of the input waits for the
    /// rig's Limiter, and a Limiter in the bus's chain makes the rig wait.
    #[test]
    fn a_bus_return_meets_the_dry_rig_in_phase() {
        let lim = &[DspKind::Limiter][..];
        let (plan, p) = dry_and_return(lim, &[], false);
        assert!(plan.is_empty(), "a rig send is already as late as the rig: {plan:?}");
        assert!(p.lag_frames == 240 && p.r > 0.95, "rig send beside a limited rig: {p:?}");
        let (plan, p) = dry_and_return(lim, &[], true);
        assert_eq!(plan.iter().map(|l| l.2).collect::<Vec<_>>(), vec![240], "the early return waits: {plan:?}");
        assert!(p.lag_frames == 240 && p.r > 0.95, "input send beside a limited rig: {p:?}");
        let (plan, p) = dry_and_return(&[], lim, false);
        assert!(plan.len() == 1 && plan[0].0 == plan[0].1 && plan[0].2 == 240, "the rig waits after its send: {plan:?}");
        assert!(p.lag_frames == 240 && p.r > 0.95, "a limiter in the bus's chain: {p:?}");
    }

    #[test]
    fn a_bypassed_limiter_keeps_its_delay_and_the_paths_stay_aligned() {
        let (mut probe, units) = split_paths(&[DspKind::Limiter]);
        probe.settle(8);
        probe.send(Command::SetStripBypass { node: units[0], bypass: true });
        let p = lag_r_behind_l(&mut probe);
        assert_eq!(p.lag_frames, 0, "bypassed: {p:?}");
        assert!(p.r > 0.99, "a bypassed limiter passes the noise unchanged: {p:?}");
        probe.send(Command::SetStripBypass { node: units[0], bypass: false });
        let p = lag_r_behind_l(&mut probe);
        assert_eq!(p.lag_frames, 0, "running again: {p:?}");
    }

    #[test]
    fn a_disconnect_replans_and_an_unrelated_edit_keeps_the_lines() {
        let (mut probe, units) = split_paths(&[DspKind::Limiter]);
        probe.settle(4);
        assert_eq!(probe.engine.pdc.lines.len(), 1);
        let held = probe.engine.pdc.lines[0].ring.as_ptr();
        probe.send(Command::RenameNode { node: units[0], name: name_bytes("LOOKAHEAD") });
        probe.settle(1);
        assert_eq!(probe.engine.pdc.lines[0].ring.as_ptr(), held, "a rename keeps the line and what it holds");
        // Without the limited path nothing is delayed any more.
        probe.send(Command::Disconnect(4));
        probe.settle(1);
        assert!(probe.engine.pdc.lines.is_empty());
    }
}
