//! Delay compensation (issues #13, #76): two paths that carry the same
//! sound into one summing point arrive in phase when one of them runs
//! through a latent unit (the Limiter's 5 ms lookahead, a hosted plugin's
//! reported latency), instead of that many frames apart.
//!
//! The plan is a pure function of the graph and each node's latency. Every
//! node knows how late each source's sound is at its output. At a summing
//! point (an effect's input, a rig's master) the inputs that share a source
//! are delayed until that source lines up; inputs that share none are left
//! alone, so an instrument never waits for a latent effect on another
//! instrument (the played note path gains nothing it does not need). The
//! engine runs effects in signal order and breaks a feedback loop at one
//! cable, which then carries the previous block; that cable neither counts
//! nor gets a line, so the plan ends. Master inserts run serially on the mix
//! bus and are not part of it.
//!
//! The engine replans on graph edits, when a send starts or stops sounding
//! and when a hosted plugin reports another latency, never per block; with
//! no latent unit the plan is empty and every sum is the plain sum. A
//! replan for a latency change allocates nothing: the planner reuses its
//! tables, and while the graph holds a unit whose latency can change, every
//! line that a plan could ask for exists with room for `MAX_FRAMES`.

use crate::control::{AuxSend, DspKind, Graph, NodeId, NodeKind, PortKind, SendSource};

/// The longest latency the plan counts for one unit, and the room of a
/// line that has to follow latency changes without allocating (341 ms at
/// 48 kHz; a linear-phase EQ reports up to about half of that).
pub const MAX_FRAMES: usize = 16384;

/// A source's sound does not reach this node.
const NONE: u32 = u32::MAX;

fn reset<T: Clone>(v: &mut Vec<T>, len: usize, x: T) {
    v.clear();
    v.resize(len, x);
}

/// Plans the delays, and keeps its tables between plans: planning the same
/// graph again (another latency, nothing else) allocates nothing.
#[derive(Default)]
pub struct Planner {
    ids: Vec<NodeId>,
    own: Vec<u32>,
    /// `inp[d * n + s]`: `d` sums `s`.
    inp: Vec<bool>,
    rank: Vec<usize>,
    /// The effects in the order they render.
    order: Vec<usize>,
    wet: Vec<bool>,
    /// `arr[i * n + s]`: how late source `s` is at `i`'s output.
    arr: Vec<u32>,
    off: Vec<i64>,
    seen: Vec<bool>,
    stack: Vec<usize>,
    comp: Vec<usize>,
    group: Vec<usize>,
    slow: Vec<u32>,
    rig_wait: Vec<u32>,
    plan: Vec<(NodeId, NodeId, usize)>,
    cand: Vec<(NodeId, NodeId)>,
    late: Vec<(NodeId, u32)>,
}

impl Planner {
    /// The delays that align every summing point: (source, destination,
    /// frames).
    ///
    /// Sends are not cables, so they come in beside the graph. A bus return
    /// has to line up with the dry sound of whoever sent into the bus, not
    /// with the master its cable ends at:
    /// - a send of a whole rig taps the rig's sum, so its return is as late
    ///   as that sum already, whatever the rig holds: no delay;
    /// - a send of one source taps it before the latent units on its way to
    ///   the master: it waits for them on its way into the bus, `(source,
    ///   bus, frames)`;
    /// - a latent unit in a bus's own chain makes the return late for every
    ///   sender: their rigs wait after their sends are tapped, `(master,
    ///   master, frames)`, and a faster return of the same bus waits too.
    /// Without a latent unit in a bus's chain, rig sends change nothing.
    pub fn plan(&mut self, graph: &Graph, sends: &[AuxSend], latency_of: impl Fn(NodeId) -> usize) -> &[(NodeId, NodeId, usize)] {
        let nodes = &graph.nodes;
        let n = nodes.len();
        let Planner { ids, own, inp, rank, order, wet, arr, off, seen, stack, comp, group, slow, rig_wait, plan, cand, late } = self;
        let index = |id: NodeId| nodes.iter().position(|x| x.id == id);
        let is_dsp = |i: usize| matches!(&nodes[i].kind, NodeKind::Dsp(k) if *k != DspKind::Metronome);
        let is_lv2 = |i: usize| matches!(nodes[i].kind, NodeKind::Lv2 { .. });
        let is_master = |i: usize| matches!(nodes[i].kind, NodeKind::MasterOut);
        let is_clap_fx = |i: usize| {
            matches!(nodes[i].kind, NodeKind::Clap { .. })
                && graph.edges.iter().any(|e| {
                    e.kind == PortKind::Audio
                        && e.to.node == nodes[i].id
                        && e.from.node != nodes[i].id
                        && index(e.from.node).is_some_and(|s| !is_master(s))
                })
        };
        let is_bus = |i: usize| matches!(nodes[i].kind, NodeKind::Dsp(DspKind::Bus));
        let buses = (0..n).filter(|&i| is_bus(i)).count();
        ids.clear();
        ids.extend(nodes.iter().map(|x| x.id));
        own.clear();
        own.extend(nodes.iter().map(|x| latency_of(x.id).min(MAX_FRAMES) as u32));
        let most = graph.edges.len() + crate::engine::MAX_SENDS + 2 * n + n * buses;
        for v in [&mut *order, &mut *stack, &mut *comp] {
            v.clear();
            v.reserve(n);
        }
        plan.clear();
        plan.reserve(most);
        cand.clear();
        cand.reserve(most);
        late.clear();
        late.reserve(n);
        // Each source once per destination: the engine sums a source into a
        // destination once, however many cables join them. A cable out of a
        // master is an insert.
        reset(inp, n * n, false);
        for e in graph.edges.iter().filter(|e| e.kind == PortKind::Audio) {
            let (Some(s), Some(d)) = (index(e.from.node), index(e.to.node)) else { continue };
            if s != d && !is_master(s) && (is_dsp(d) || is_lv2(d) || is_clap_fx(d) || is_master(d)) {
                inp[d * n + s] = true;
            }
        }
        // When each node renders: sources first, then the effects in the
        // order `GraphEngine::next_effect` runs them (built-in DSP in graph
        // order, then LV2, then CLAP effects), then the masters.
        let effects = || {
            (0..n)
                .filter(|&i| is_dsp(i))
                .chain((0..n).filter(|&i| is_lv2(i)))
                .chain((0..n).filter(|&i| is_clap_fx(i)))
        };
        reset(rank, n, 0);
        for m in (0..n).filter(|&m| is_master(m)) {
            rank[m] = usize::MAX;
        }
        for step in 1..=effects().count() {
            let waits = |f: usize| effects().any(|g| g != f && rank[g] == 0 && inp[f * n + g]);
            let next = effects().find(|&f| rank[f] == 0 && !waits(f));
            let Some(f) = next.or_else(|| effects().find(|&f| rank[f] == 0)) else { break };
            rank[f] = step;
            order.push(f);
        }
        // A cable from a node that renders later carries the previous block.
        for d in 0..n {
            for s in 0..n {
                if rank[s] >= rank[d] {
                    inp[d * n + s] = false;
                }
            }
        }
        // The returns: a bus and what its cables lead to (`mark_after_bus`).
        reset(wet, n, false);
        for b in (0..n).filter(|&b| is_bus(b)) {
            wet[b] = true;
        }
        while let Some(d) = (0..n).find(|&d| !wet[d] && !is_master(d) && (0..n).any(|s| wet[s] && inp[d * n + s])) {
            wet[d] = true;
        }
        // A bus hears its sends, not what is cabled into it.
        for b in (0..n).filter(|&b| is_bus(b)) {
            inp[b * n..(b + 1) * n].fill(false);
        }
        // How late each source is at each node, in render order; ranks rise
        // along every cable left. A node nothing is cabled into is a source
        // (a bus is the source of its return). A master lines up its dry
        // inputs here and its returns below.
        reset(arr, n * n, NONE);
        reset(off, n, 0);
        let masters = || (0..n).filter(|&m| is_master(m));
        let in_order = (0..n).filter(|&i| rank[i] == 0).chain(order.iter().copied()).chain(masters());
        for d in in_order {
            let summed = |s: usize| inp[d * n + s] && !(is_master(d) && wet[s]);
            if !(0..n).any(summed) {
                arr[d * n + d] = own[d];
                continue;
            }
            // Inputs that share a source form a set; each set is shifted so
            // its sources line up, and its earliest input is not delayed.
            reset(seen, n, false);
            for first in (0..n).filter(|&s| summed(s)) {
                if seen[first] {
                    continue;
                }
                seen[first] = true;
                off[first] = 0;
                comp.clear();
                stack.clear();
                stack.push(first);
                while let Some(j) = stack.pop() {
                    comp.push(j);
                    for k in (0..n).filter(|&k| summed(k)) {
                        if seen[k] {
                            continue;
                        }
                        // Where they disagree, the slowest source decides.
                        let ahead = (0..n).filter(|&s| arr[j * n + s] != NONE && arr[k * n + s] != NONE).map(|s| arr[j * n + s] as i64 - arr[k * n + s] as i64).max();
                        if let Some(ahead) = ahead {
                            seen[k] = true;
                            off[k] = off[j] + ahead;
                            stack.push(k);
                        }
                    }
                }
                let least = comp.iter().map(|&k| off[k]).min().unwrap_or(0);
                // In node order, so one graph always gives one plan.
                comp.sort_unstable();
                for &k in comp.iter() {
                    off[k] -= least;
                    if off[k] > 0 {
                        plan.push((ids[k], ids[d], off[k] as usize));
                    }
                }
            }
            for j in (0..n).filter(|&j| summed(j)) {
                cand.push((ids[j], ids[d]));
                for s in 0..n {
                    if arr[j * n + s] != NONE {
                        let at = own[d] + arr[j * n + s] + off[j] as u32;
                        let slot = &mut arr[d * n + s];
                        *slot = if *slot == NONE { at } else { (*slot).max(at) };
                    }
                }
            }
        }
        let out = |i: usize| arr[i * n..(i + 1) * n].iter().filter(|&&a| a != NONE).max().copied().unwrap_or(0);
        let master_of = |rig: u8| masters().find(|&m| nodes[m].rig == rig);
        // Who shares a bus: senders' masters with the bus, and a bus with
        // what its cables lead to. `group[i]` names the set node `i` is in.
        group.clear();
        group.extend(0..n);
        let join = |group: &mut Vec<usize>, a: usize, b: usize| {
            let (ga, gb) = (group[a], group[b]);
            if ga != gb {
                for g in group.iter_mut().filter(|g| **g == gb) {
                    *g = ga;
                }
            }
        };
        for d in (0..n).filter(|&d| wet[d]) {
            for s in (0..n).filter(|&s| wet[s] && inp[d * n + s]) {
                join(group, s, d);
            }
        }
        for b in (0..n).filter(|&b| is_bus(b)) {
            for i in (0..n).filter(|&i| !wet[i] && !is_master(i)) {
                cand.push((ids[i], ids[b]));
            }
        }
        for s in sends.iter().filter(|s| s.level > 0.0) {
            let Some(b) = index(s.bus).filter(|&b| is_bus(b)) else { continue };
            let rig = match s.from {
                SendSource::Node(x) => match index(x).filter(|&i| !wet[i]) {
                    Some(i) => nodes[i].rig,
                    None => continue,
                },
                SendSource::Rig(r) => r,
            };
            let Some(m) = master_of(rig) else { continue };
            join(group, m, b);
        }
        // Returns into the mix: the slowest return of a set is what its
        // faster returns wait for, and what its senders align to.
        reset(slow, n, 0);
        let (wet, inp): (&[bool], &[bool]) = (wet, inp);
        let returns = |m: usize| (0..n).filter(move |&w| wet[w] && inp[m * n + w]);
        for m in masters() {
            for w in returns(m) {
                slow[group[w]] = slow[group[w]].max(out(w));
            }
        }
        // Per-master rig_out wait: a rig send taps the rig's aligned sum and
        // waits the full return latency; a source send taps before its strip's
        // latent units (`ahead`), so the rig waits only the excess beyond what
        // the strip already has (never compensating a latent strip + latent
        // bus twice).
        let source_ahead = |i: usize, m: usize| {
            (0..n).filter(|&r| arr[i * n + r] != NONE && arr[m * n + r] != NONE)
                .map(|r| arr[m * n + r].saturating_sub(arr[i * n + r]))
                .max()
                .unwrap_or(0)
        };
        reset(rig_wait, n, 0);
        for s in sends.iter().filter(|s| s.level > 0.0) {
            let Some(b) = index(s.bus).filter(|&b| is_bus(b)) else { continue };
            let ret_lat = slow[group[b]];
            match s.from {
                SendSource::Rig(r) => {
                    if let Some(m) = master_of(r) {
                        rig_wait[m] = rig_wait[m].max(ret_lat);
                    }
                }
                SendSource::Node(x) => {
                    let Some(i) = index(x).filter(|&i| !wet[i]) else { continue };
                    let Some(m) = master_of(nodes[i].rig) else { continue };
                    rig_wait[m] = rig_wait[m].max(ret_lat.saturating_sub(source_ahead(i, m)));
                }
            }
        }
        for s in sends.iter().filter(|s| s.level > 0.0) {
            let SendSource::Node(x) = s.from else { continue };
            let Some(b) = index(s.bus).filter(|&b| is_bus(b)) else { continue };
            let Some(i) = index(x).filter(|&i| !wet[i]) else { continue };
            let Some(m) = master_of(nodes[i].rig) else { continue };
            let wait = (source_ahead(i, m) + rig_wait[m]).saturating_sub(slow[group[b]]);
            if wait > 0 {
                plan.push((ids[i], ids[b], wait as usize));
            }
        }
        for m in masters() {
            for w in returns(m) {
                cand.push((ids[w], ids[m]));
                if out(w) < slow[group[w]] {
                    plan.push((ids[w], ids[m], (slow[group[w]] - out(w)) as usize));
                }
            }
        }
        for m in masters() {
            cand.push((ids[m], ids[m]));
            if rig_wait[m] > 0 {
                plan.push((ids[m], ids[m], rig_wait[m] as usize));
            }
        }
        // How late each strip sounds in the mix: a source through its rig,
        // a rig's slowest source, a bus through its slowest return.
        for i in 0..n {
            let total = if is_master(i) {
                out(i) + rig_wait[i]
            } else if wet[i] {
                if is_bus(i) { slow[group[i]] } else { 0 }
            } else if arr[i * n + i] != NONE {
                masters().filter(|&m| arr[m * n + i] != NONE).map(|m| arr[m * n + i] + rig_wait[m]).max().unwrap_or(own[i])
            } else {
                0
            };
            if total > 0 {
                late.push((ids[i], total));
            }
        }
        plan
    }
}

/// The plan for `graph` (`Planner::plan`), for callers that plan once.
pub fn compensation(graph: &Graph, sends: &[AuxSend], latency_of: impl Fn(NodeId) -> usize) -> Vec<(NodeId, NodeId, usize)> {
    Planner::default().plan(graph, sends, latency_of).to_vec()
}

/// A delay of `frames` stereo frames, in a ring with room for more.
struct Line {
    src: NodeId,
    dest: NodeId,
    ring: Vec<f32>,
    /// 0: the line is parked and what passes is not delayed.
    frames: usize,
    pos: usize,
}

impl Line {
    #[inline]
    fn push(&mut self, l: f32, r: f32) -> (f32, f32) {
        let p = self.pos * 2;
        let out = (self.ring[p], self.ring[p + 1]);
        self.ring[p] = l;
        self.ring[p + 1] = r;
        self.pos += 1;
        if self.pos == self.frames {
            self.pos = 0;
        }
        out
    }

    /// Another delay starts from silence; the same one keeps what it holds
    /// (a replan for an unrelated edit must not drop 5 ms of audio). A
    /// delay beyond the ring's room is cut to it. Allocation-free.
    fn set(&mut self, frames: usize) {
        let frames = frames.min(self.ring.len() / 2);
        if frames != self.frames {
            self.frames = frames;
            self.pos = 0;
            self.ring[..frames * 2].fill(0.0);
        }
    }
}

/// Make `lines` follow `plan`. With `allocate`, lines the plan and `keep`
/// do not name are dropped and missing ones made, each with room for
/// `room` frames at least; without, the lines there are only set.
fn fit(lines: &mut Vec<Line>, plan: &[(NodeId, NodeId, usize)], keep: &[(NodeId, NodeId)], room: usize, allocate: bool) {
    if allocate {
        lines.retain(|l| plan.iter().any(|p| (p.0, p.1) == (l.src, l.dest)) || keep.contains(&(l.src, l.dest)));
        let wanted = plan.iter().map(|p| (p.0, p.1, p.2.max(room))).chain(keep.iter().map(|k| (k.0, k.1, room)));
        for (src, dest, frames) in wanted {
            match lines.iter_mut().find(|l| (l.src, l.dest) == (src, dest)) {
                Some(l) if l.ring.len() < frames * 2 => l.ring.resize(frames * 2, 0.0),
                Some(_) => {}
                None => lines.push(Line { src, dest, ring: vec![0.0; frames * 2], frames: 0, pos: 0 }),
            }
        }
    }
    for l in lines.iter_mut() {
        l.set(plan.iter().find(|p| (p.0, p.1) == (l.src, l.dest)).map_or(0, |p| p.2));
    }
}

/// The engine's delay lines: one per cable pair the plan delays, and one per
/// latent unit for its bypass.
#[derive(Default)]
pub struct Pdc {
    planner: Planner,
    lines: Vec<Line>,
    bypass: Vec<Line>,
    latent: Vec<(NodeId, NodeId, usize)>,
    varying: Vec<(NodeId, NodeId)>,
}

impl Pdc {
    /// Follow the plan for `graph`. `varies` names the units whose latency
    /// can change while they run (hosted plugins that report one).
    ///
    /// With `allocate` (graph edits) the lines are made to fit: exactly the
    /// plan's when nothing varies, and otherwise every line a plan of this
    /// graph could ask for, with room for `MAX_FRAMES`. Without it (a
    /// plugin reported another latency) the lines there are set to the new
    /// plan and nothing is allocated or freed.
    pub fn replan(&mut self, graph: &Graph, sends: &[AuxSend], latency_of: impl Fn(NodeId) -> usize, varies: impl Fn(NodeId) -> bool, allocate: bool) {
        let Pdc { planner, lines, bypass, latent, varying } = self;
        planner.plan(graph, sends, &latency_of);
        latent.clear();
        varying.clear();
        if allocate {
            latent.reserve(graph.nodes.len());
            varying.reserve(graph.nodes.len());
        }
        for n in &graph.nodes {
            let frames = latency_of(n.id).min(MAX_FRAMES);
            if frames > 0 {
                latent.push((n.id, n.id, frames));
            }
            if varies(n.id) {
                varying.push((n.id, n.id));
            }
        }
        let has_bus = graph.nodes.iter().any(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus)));
        let (keep, room): (&[(NodeId, NodeId)], usize) = if !varying.is_empty() {
            (&planner.cand, MAX_FRAMES)
        } else if has_bus && !latent.is_empty() {
            // Preallocate bus-send/return candidates so turning a send on or
            // off across zero replans without allocating on the audio thread.
            let max_fixed = latent.iter().map(|l| l.2).sum::<usize>().min(MAX_FRAMES);
            (&planner.cand, max_fixed)
        } else {
            (&[], 0)
        };
        fit(lines, &planner.plan, keep, room, allocate);
        fit(bypass, latent, varying, if varying.is_empty() { 0 } else { MAX_FRAMES }, allocate);
    }

    /// How late each strip sounds in the mix, in frames, of the last plan:
    /// sources, rigs (by their master) and buses, those on time left out.
    pub fn late(&self) -> &[(NodeId, u32)] {
        &self.planner.late
    }

    /// Does any line end at `dest`? Then its sum must run every block, also
    /// at zero gain, or the line would replay stale audio later.
    pub fn feeds(&self, dest: NodeId) -> bool {
        self.lines.iter().any(|l| l.dest == dest && l.frames > 0)
    }

    /// Add `src`'s block (stereo interleaved, `out.len()` samples) into
    /// `out` as `dest` hears it: through its line when the plan has one,
    /// scaled by `gains`; `mono` takes the left channel to both sides (a
    /// cable into a MONO port). Allocation-free.
    #[inline]
    pub fn mix(&mut self, src: NodeId, dest: NodeId, buf: &[f32], mono: bool, (gl, gr): (f32, f32), out: &mut [f32]) {
        let frames = out.len() / 2;
        match self.lines.iter_mut().find(|l| l.src == src && l.dest == dest && l.frames > 0) {
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
        let Some(line) = self.lines.iter_mut().find(|l| l.src == master && l.dest == master && l.frames > 0) else { return };
        for f in rig_buf.chunks_exact_mut(2) {
            (f[0], f[1]) = line.push(f[0], f[1]);
        }
    }

    /// Does a line start at `src`? Then what it sends must keep running
    /// while it is silenced.
    pub fn delays_from(&self, src: NodeId) -> bool {
        self.lines.iter().any(|l| l.src == src && l.frames > 0)
    }

    /// A latent unit delays by its latency also while bypassed, so the
    /// paths it sums with stay aligned and a bypass toggle moves nothing in
    /// time. Bypassed: `out` becomes `input` that many frames late. Running:
    /// the line only keeps up with `input`, so a later bypass is seamless.
    /// A unit without latency has no line. Allocation-free.
    pub fn bypass(&mut self, node: NodeId, input: &[f32], out: &mut [f32], bypassed: bool) {
        let Some(line) = self.bypass.iter_mut().find(|l| l.src == node && l.frames > 0) else { return };
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
        // A source with a Limiter sending into a bus with a Limiter already
        // matches at 240 frames: no double compensation (not 480).
        let (plan, p) = dry_and_return(lim, lim, true);
        assert!(plan.is_empty(), "matched strip and bus latencies need no extra delay: {plan:?}");
        assert!(p.lag_frames == 240 && p.r > 0.95, "source send + latent bus stays at 240: {p:?}");
    }

    /// Rig A: In A → Limiter → Master A, and the aux bus → `ret` units →
    /// Master A. Rig B: In B → Master B, fed with noise, sending into rig
    /// A's bus by `sends`. Returns the plan (as node names) and how the
    /// master lines up with rig B's noise.
    fn other_rigs_send(ret: &[DspKind], sends: impl Fn(NodeId) -> Vec<SendSource>) -> (Vec<(String, String, usize)>, crate::analysis::XcorrPeak) {
        use crate::control::AudioInChannel;
        use crate::engine::probe::{Probe, Tap, MAX_LAG, SETTLE_BLOCKS};
        let mut g = Graph::default();
        let audio_in = |g: &mut Graph, name: &str| g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes(name));
        let (in_a, in_b) = (audio_in(&mut g, "IN A"), audio_in(&mut g, "IN B"));
        let lim = dsp(&mut g, DspKind::Limiter, "LIM A");
        let bus = dsp(&mut g, DspKind::Bus, "BUS");
        let (master_a, master_b) = (g.add_node(NodeKind::MasterOut, name_bytes("MASTER A")), g.add_node(NodeKind::MasterOut, name_bytes("MASTER B")));
        g.set_rig(in_b, 1);
        g.set_rig(master_b, 1);
        wire(&mut g, 1, in_a, lim, 0);
        wire(&mut g, 2, lim, master_a, 0);
        wire(&mut g, 3, in_b, master_b, 0);
        let mut prev = bus;
        for (i, k) in ret.iter().enumerate() {
            let u = dsp(&mut g, k.clone(), "RETURN FX");
            wire(&mut g, 10 + i as u32, prev, u, 0);
            prev = u;
        }
        wire(&mut g, 4, prev, master_a, 0);
        let mut probe = Probe::new(g);
        for from in sends(in_b) {
            probe.send(Command::SetSend { from, bus, level: Some(1.0), pre: Some(false) });
        }
        probe.feed("B", in_b, crate::analysis::white(9).map(|x| x * 0.1));
        probe.settle(SETTLE_BLOCKS);
        let run = probe.run(32, &[Tap::MasterL]);
        let graph = probe.engine.graph();
        let name = |id: NodeId| graph.node(id).map_or(String::new(), |n| crate::control::name_str(&n.name).to_string());
        let plan = compensation(graph, &probe.engine.sends, lim_latency(graph)).into_iter().map(|(s, d, f)| (name(s), name(d), f)).collect();
        (plan, crate::analysis::xcorr_peak(&run.feeds[0].1, &run.taps[0].1, 2 * MAX_LAG))
    }

    /// A return lines up with whoever sent into the bus, not with the rig
    /// the bus is cabled to: rig B plays and returns on time although rig A
    /// (the bus's own rig) has a Limiter, alone or sending beside it. Only a
    /// Limiter in the bus's chain makes rig B wait, and only rig B.
    #[test]
    fn a_send_from_another_rig_returns_in_phase_with_its_own_rig() {
        let cases: [(&str, fn(NodeId) -> Vec<SendSource>); 3] = [
            ("rig B sends", |_| vec![SendSource::Rig(1)]),
            ("In B sends", |in_b| vec![SendSource::Node(in_b)]),
            ("both rigs send", |_| vec![SendSource::Rig(0), SendSource::Rig(1)]),
        ];
        for (what, sends) in cases {
            let (plan, p) = other_rigs_send(&[], sends);
            assert!(plan.is_empty(), "{what}: nothing waits: {plan:?}");
            assert!(p.lag_frames == 0 && p.r > 0.95, "{what}: {p:?}");
        }
        let (plan, p) = other_rigs_send(&[DspKind::Limiter], |_| vec![SendSource::Rig(1)]);
        assert_eq!(plan, vec![("MASTER B".to_string(), "MASTER B".to_string(), 240)], "only the sender waits for the return");
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
    /// What the planner says each strip is late by, as node names.
    fn late(g: &Graph, latency_of: impl Fn(NodeId) -> usize) -> Vec<(String, u32)> {
        let mut planner = Planner::default();
        planner.plan(g, &[], latency_of);
        planner.late.iter().map(|&(id, f)| (crate::control::name_str(&g.node(id).unwrap().name).to_string(), f)).collect()
    }

    /// The played note path gains nothing it does not need (#76): a latent
    /// unit delays only the paths that carry the same sound.
    #[test]
    fn sources_that_share_no_sound_do_not_wait_for_each_other() {
        // Two sources in one rig, B through a Limiter: A stays on time.
        let mut g = Graph::default();
        let a = dsp(&mut g, DspKind::Metronome, "A");
        let b = dsp(&mut g, DspKind::Metronome, "B");
        let lim = dsp(&mut g, DspKind::Limiter, "LIM");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, a, master, 0);
        wire(&mut g, 2, b, lim, 0);
        wire(&mut g, 3, lim, master, 0);
        assert!(compensation(&g, &[], lim_latency(&g)).is_empty(), "A does not wait for B's limiter");
        assert_eq!(late(&g, lim_latency(&g)), vec![("B".to_string(), 240), ("MASTER".to_string(), 240)]);
        // B dry beside its limiter is the same sound: that path waits.
        wire(&mut g, 4, b, master, 1);
        assert_eq!(compensation(&g, &[], lim_latency(&g)), vec![(b, master, 240)]);
        // A and the limited B through one shared unit, and A dry beside it:
        // inside the unit A is on time, so its dry path is not delayed
        // (one number per node would call the unit 240 late and delay it).
        let mut g = Graph::default();
        let a = dsp(&mut g, DspKind::Metronome, "A");
        let b = dsp(&mut g, DspKind::Metronome, "B");
        let lim = dsp(&mut g, DspKind::Limiter, "LIM");
        let shared = dsp(&mut g, DspKind::Gain, "SHARED");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, a, shared, 0);
        wire(&mut g, 2, b, lim, 0);
        wire(&mut g, 3, lim, shared, 0);
        wire(&mut g, 4, shared, master, 0);
        wire(&mut g, 5, a, master, 0);
        assert!(compensation(&g, &[], lim_latency(&g)).is_empty(), "A is on time on both of its paths");
        // A latent unit nothing is cabled into delays nobody.
        let mut g = Graph::default();
        let a = dsp(&mut g, DspKind::Metronome, "A");
        let lim = dsp(&mut g, DspKind::Limiter, "LEFT OVER");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, a, master, 0);
        wire(&mut g, 2, lim, master, 0);
        assert!(compensation(&g, &[], lim_latency(&g)).is_empty());
    }

    /// An instrument's own latency is on every path of its sound, so it
    /// moves nothing, and it is what its strip shows.
    #[test]
    fn a_latent_instrument_delays_nothing_and_shows_on_its_strip() {
        let mut g = Graph::default();
        let synth = dsp(&mut g, DspKind::Metronome, "SYNTH");
        let other = dsp(&mut g, DspKind::Metronome, "OTHER");
        let fx = dsp(&mut g, DspKind::Gain, "FX");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, synth, master, 0);
        wire(&mut g, 2, synth, fx, 0);
        wire(&mut g, 3, fx, master, 0);
        wire(&mut g, 4, other, master, 0);
        let latency = |id: NodeId| if id == synth { 512 } else { 0 };
        assert!(compensation(&g, &[], latency).is_empty());
        assert_eq!(late(&g, latency), vec![("SYNTH".to_string(), 512), ("MASTER".to_string(), 512)]);
        // A unit reporting more than a line holds counts as the most.
        assert_eq!(late(&g, |id| if id == synth { 1 << 20 } else { 0 })[0].1, MAX_FRAMES as u32);
    }

    use crate::engine::latent;

    fn delay(probe: &mut crate::engine::probe::Probe, unit: NodeId, frames: usize) {
        probe.send(Command::SetParam { node: unit, param: latent::P_DELAY, value: latent::delay_value(frames) });
    }

    /// A hosted plugin's reported latency (LV2 `lv2:reportsLatency`) is
    /// compensated like a built-in unit's, also bypassed, and the paths
    /// are back in phase after the plugin reports another one.
    #[test]
    fn a_hosted_plugins_reported_latency_is_compensated_and_followed() {
        latent::install().unwrap();
        let (mut probe, units) = crate::engine::probe::split_paths_of(&[latent::kind()]);
        let (input, unit) = (probe.engine.graph().nodes[0].id, units[0]);
        let mut in_phase = |probe: &mut crate::engine::probe::Probe, frames: usize, what: &str| {
            let p = lag_r_behind_l(probe);
            assert!(p.lag_frames == 0 && p.r > 0.95, "{what}: the latent path is off the dry one: {p:?}");
            let late: Vec<_> = probe.engine.pdc.late().iter().filter(|l| l.0 == input).map(|l| l.1 as usize).collect();
            assert_eq!(late, if frames > 0 { vec![frames] } else { vec![] }, "{what}: the input's strip");
        };
        in_phase(&mut probe, latent::DEFAULT_FRAMES, "as loaded");
        delay(&mut probe, unit, 1000);
        in_phase(&mut probe, 1000, "raised to 1000 frames");
        probe.send(Command::SetStripBypass { node: unit, bypass: true });
        in_phase(&mut probe, 1000, "bypassed");
        probe.send(Command::SetStripBypass { node: unit, bypass: false });
        delay(&mut probe, unit, 37);
        in_phase(&mut probe, 37, "lowered to 37 frames");
        delay(&mut probe, unit, 0);
        in_phase(&mut probe, 0, "no latency");
        assert!(!probe.engine.pdc.lines.iter().any(|l| l.frames > 0), "nothing is delayed any more");
        // Two in series, each with its own latency.
        let (mut probe, units) = crate::engine::probe::split_paths_of(&[latent::kind(), latent::kind()]);
        delay(&mut probe, units[1], 700);
        let p = lag_r_behind_l(&mut probe);
        assert!(p.lag_frames == 0 && p.r > 0.95, "two latent plugins in series: {p:?}");
        assert_eq!(probe.engine.pdc.lines.iter().map(|l| l.frames).max(), Some(latent::DEFAULT_FRAMES + 700));
    }

    /// A latency change is a knob move on a plugin, so the audio thread
    /// follows it without allocating: the plan, the lines and what is
    /// published to the UI.
    #[test]
    fn a_latency_change_replans_without_allocating() {
        use crate::dsp::tests::rt_alloc;
        latent::install().unwrap();
        let (mut probe, units) = crate::engine::probe::split_paths_of(&[latent::kind()]);
        let input = probe.engine.graph().nodes[0].id;
        let published = std::sync::Arc::new(std::sync::Mutex::new(crate::engine::GraphPub::default()));
        probe.engine.set_graph_pub(published.clone());
        // A graph edit publishes and sizes everything, as the app's load does.
        probe.send(Command::RenameNode { node: units[0], name: name_bytes("LATENT") });
        probe.settle(4);
        let mut out = vec![0.0f32; crate::bridge::BUFFER_FRAMES as usize * 2];
        for frames in [1000, 37, 0, MAX_FRAMES, 240] {
            delay(&mut probe, units[0], frames);
            let before = rt_alloc::count();
            for _ in 0..8 {
                probe.block(&mut out);
            }
            assert_eq!(rt_alloc::count() - before, 0, "allocations while following a latency of {frames} frames");
            let line = probe.engine.pdc.lines.iter().map(|l| l.frames).max();
            assert_eq!(line, Some(frames), "the dry path waits {frames} frames");
            let shown: Vec<u32> = published.lock().unwrap().latency.iter().filter(|l| l.0 == input).map(|l| l.1).collect();
            assert_eq!(shown, if frames > 0 { vec![frames as u32] } else { vec![] }, "the published strip latency");
        }
    }

    /// A hosted latent LV2 plugin in an aux bus's chain keeps the dry rig and
    /// bus return in phase as its latency changes at run time, and both
    /// latency changes and crossing a send across zero allocate nothing.
    #[test]
    fn a_latent_hosted_plugin_on_a_bus_aligns_and_follows_changes() {
        use crate::control::AudioInChannel;
        use crate::dsp::tests::rt_alloc;
        use crate::engine::probe::{Probe, Tap, SETTLE_BLOCKS};
        latent::install().unwrap();
        let mut g = Graph::default();
        let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let bus = dsp(&mut g, DspKind::Bus, "BUS");
        let fx = g.add_node(latent::kind(), name_bytes("BUS LATENT"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, ain, master, 0);
        wire(&mut g, 2, ain, master, 1);
        wire(&mut g, 3, bus, fx, 0);
        wire(&mut g, 4, fx, master, 0);
        wire(&mut g, 5, fx, master, 1);
        let mut probe = Probe::hosting(g);
        probe.feed("noise", ain, crate::analysis::white(11).map(|x| x * 0.1));
        probe.settle(4);
        // Crossing a send from 0 to 1 and back to 0 allocates nothing.
        let mut out = vec![0.0f32; crate::bridge::BUFFER_FRAMES as usize * 2];
        for level in [1.0f32, 0.0, 1.0] {
            probe.send(Command::SetSend { from: SendSource::Rig(0), bus, level: Some(level), pre: Some(false) });
            let before = rt_alloc::count();
            for _ in 0..4 {
                probe.block(&mut out);
            }
            assert_eq!(rt_alloc::count() - before, 0, "allocations when send level crossed to {level}");
        }
        // Changing the bus plugin's latency at run time stays allocation-free and in phase.
        for frames in [latent::DEFAULT_FRAMES, 1000, 37, 0, 2048] {
            delay(&mut probe, fx, frames);
            let before = rt_alloc::count();
            for _ in 0..8 {
                probe.block(&mut out);
            }
            assert_eq!(rt_alloc::count() - before, 0, "allocations on bus plugin latency {frames}");
            let settle = SETTLE_BLOCKS.max(frames / crate::bridge::BUFFER_FRAMES as usize + 4);
            probe.settle(settle);
            let win = 32 * crate::bridge::BUFFER_FRAMES as usize;
            let blocks = 32 + frames / crate::bridge::BUFFER_FRAMES as usize + 1;
            let run = probe.run(blocks, &[Tap::MasterL]);
            let p = crate::analysis::xcorr_peak(&run.feeds[0].1[..win], &run.taps[0].1[frames..frames + win], 600);
            assert!(p.lag_frames == 0 && p.r > 0.95, "bus plugin at {frames} frames: {p:?}");
        }
    }

    /// A muted source whose send has a delay line still feeds silence into the
    /// line so unmuting later does not replay stale pre-mute audio.
    #[test]
    fn a_muted_sources_delayed_send_flushes_stale_audio() {
        use crate::control::AudioInChannel;
        use crate::engine::probe::{Probe, Tap};
        let mut g = Graph::default();
        let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let lim = dsp(&mut g, DspKind::Limiter, "LIM");
        let bus = dsp(&mut g, DspKind::Bus, "BUS");
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        wire(&mut g, 1, ain, lim, 0);
        wire(&mut g, 2, lim, master, 0);
        wire(&mut g, 3, bus, master, 0);
        let mut probe = Probe::new(g);
        probe.send(Command::SetSend { from: SendSource::Node(ain), bus, level: Some(1.0), pre: Some(false) });
        // Loud constant for 4 blocks, then silence from block 4 onward.
        let n_loud = 4 * crate::bridge::BUFFER_FRAMES as usize;
        probe.feed("burst", ain, (0..).map(move |i| if i < n_loud { 0.4 } else { 0.0 }));
        probe.settle(4);
        // Mute the source while silence is fed for 8 blocks (> 240 frames), then unmute.
        probe.send(Command::SetStripMix { node: ain, pan: None, mute: Some(true), solo: None });
        probe.settle(8);
        probe.send(Command::SetStripMix { node: ain, pan: None, mute: Some(false), solo: None });
        let run = probe.run(2, &[Tap::Node(bus)]);
        let peak = run.taps[0].1.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-4, "unmuting replayed stale audio from the send delay line: peak {peak}");
    }
}
