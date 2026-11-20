//! Routing probe (issue #13): proves where audio goes by feeding
//! deterministic signals into Audio In nodes through the real input ring
//! and correlating what arrives at each tap. The expectation is a table of
//! `(feed, tap, present)` rows, so parallel sends and aux buses (#5) add
//! rows and tap kinds, not new machinery.

use super::{GraphEngine, MasterLimiter};
use crate::analysis;
use crate::control::{command_bus, name_bytes, AudioInChannel, Command, CommandBus, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef, SendSource};

pub const SR: f32 = 48000.0;
/// Blocks rendered after an edit before measuring: the master's DC blocker
/// and limiter ring on for ~100 ms, the units' slews for ~10 ms (rule 133).
pub const SETTLE_BLOCKS: usize = 30;
/// Blocks measured: 16384 frames, so two independent noises correlate
/// below 0.04 at any lag in ±`MAX_LAG`.
pub const MEASURE_BLOCKS: usize = 64;
pub const MAX_LAG: usize = 1024;
/// A feed is at a tap when they correlate at least this much …
pub const PRESENT: f32 = 0.3;
/// … and absent below this.
pub const ABSENT: f32 = 0.1;

/// Where the probe listens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tap {
    MasterL,
    MasterR,
    /// A node's own output block, L + R: what its OUT jack carries.
    Node(NodeId),
}

impl Tap {
    fn label(&self, graph: &Graph) -> String {
        match self {
            Tap::MasterL => "Master L".into(),
            Tap::MasterR => "Master R".into(),
            Tap::Node(id) => graph.node(*id).map_or(format!("node {id}"), |n| crate::control::name_str(&n.name).to_string()),
        }
    }
}

struct Feed {
    name: &'static str,
    ring: rtrb::Producer<f32>,
    signal: Box<dyn Iterator<Item = f32>>,
}

/// What one `Probe::run` fed and heard, sample-aligned.
pub struct Run {
    pub feeds: Vec<(&'static str, Vec<f32>)>,
    pub taps: Vec<(Tap, Vec<f32>)>,
    /// The master output, stereo interleaved.
    pub master: Vec<f32>,
}

/// An engine with signal generators on its Audio In nodes. The ring gets
/// exactly one block before each render, as a capture delivers it, so the
/// engine reads every fed sample in the block it was fed (no drift action,
/// no backlog cut).
pub struct Probe {
    pub engine: GraphEngine,
    bus: CommandBus,
    side: rtrb::Producer<(NodeId, rtrb::Consumer<f32>)>,
    feeds: Vec<Feed>,
    block: usize,
}

impl Probe {
    pub fn new(graph: Graph) -> Self {
        Self::with_voices(graph, vec![])
    }

    pub fn with_voices(graph: Graph, voices: Vec<(NodeId, crate::host::ClapVoice)>) -> Self {
        let (bus, drain) = command_bus();
        let mut engine = GraphEngine::new(graph, drain, SR as f64, voices);
        let (side, side_rx) = rtrb::RingBuffer::new(16);
        engine.set_audio_in_channel(side_rx);
        Self { engine, bus, side, feeds: Vec::new(), block: crate::bridge::BUFFER_FRAMES as usize }
    }

    /// An engine that hosts `graph`'s LV2 units the way the app does (rule
    /// 144): an empty engine with the loader thread, the graph as a load,
    /// and blocks until every instance is attached.
    pub fn hosting(graph: Graph) -> Self {
        let want = graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Lv2 { .. })).count();
        let mut probe = Self::new(Graph::default());
        probe.engine.spawn_lv2_loader();
        probe.send(Command::LoadGraph { graph });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            probe.settle(1);
            if probe.engine.lv2.len() == want {
                return probe;
            }
            assert!(std::time::Instant::now() < deadline, "probe: {} of {want} LV2 units attached", probe.engine.lv2.len());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Feed `signal` (mono, to both channels) into the Audio In `node`.
    pub fn feed(&mut self, name: &'static str, node: NodeId, signal: impl Iterator<Item = f32> + 'static) {
        let (ring, rx) = rtrb::RingBuffer::new(self.block * 2 * 4);
        assert!(self.side.push((node, rx)).is_ok(), "probe: more than 16 feeds");
        self.feeds.push(Feed { name, ring, signal: Box::new(signal) });
    }

    pub fn send(&mut self, cmd: Command) {
        assert!(self.bus.send(cmd), "probe: command bus full");
    }

    /// Render `blocks` blocks and keep what the feeds sent and the taps
    /// carried.
    pub fn run(&mut self, blocks: usize, taps: &[Tap]) -> Run {
        let n = self.block;
        let mut run = Run {
            feeds: self.feeds.iter().map(|f| (f.name, Vec::with_capacity(blocks * n))).collect(),
            taps: taps.iter().map(|t| (*t, Vec::with_capacity(blocks * n))).collect(),
            master: Vec::with_capacity(blocks * n * 2),
        };
        let mut out = vec![0.0f32; n * 2];
        for _ in 0..blocks {
            for (f, (_, sent)) in self.feeds.iter_mut().zip(&mut run.feeds) {
                for _ in 0..n {
                    let s = f.signal.next().unwrap_or(0.0);
                    let _ = f.ring.push(s);
                    let _ = f.ring.push(s);
                    sent.push(s);
                }
            }
            self.engine.render(&mut out, 2);
            run.master.extend_from_slice(&out);
            for (tap, heard) in &mut run.taps {
                match tap {
                    Tap::MasterL => heard.extend(out.iter().step_by(2)),
                    Tap::MasterR => heard.extend(out.iter().skip(1).step_by(2)),
                    Tap::Node(id) => match self.engine.node_block(*id, n * 2) {
                        Some(b) => heard.extend(b.chunks_exact(2).map(|f| f[0] + f[1])),
                        None => heard.extend(std::iter::repeat(0.0).take(n)),
                    },
                }
            }
        }
        run
    }

    /// Render and discard (feeds keep running).
    /// Feed and render one block into `out` and keep nothing: what the
    /// audio thread does, so a test can count its allocations.
    pub fn block(&mut self, out: &mut [f32]) {
        for f in &mut self.feeds {
            for _ in 0..out.len() / 2 {
                let s = f.signal.next().unwrap_or(0.0);
                let _ = f.ring.push(s);
                let _ = f.ring.push(s);
            }
        }
        self.engine.render(out, 2);
    }

    pub fn settle(&mut self, blocks: usize) {
        self.run(blocks, &[]);
    }

    /// Settle, measure every tap the rows name, and compare with the rows
    /// `(feed, tap, present)`. Err names each wrong row and shows the matrix.
    pub fn check(&mut self, expect: &[(&'static str, Tap, bool)]) -> Result<Presence, String> {
        let mut taps: Vec<Tap> = Vec::new();
        for (_, t, _) in expect {
            if !taps.contains(t) {
                taps.push(*t);
            }
        }
        self.settle(SETTLE_BLOCKS);
        let run = self.run(MEASURE_BLOCKS, &taps);
        let p = Presence::of(&run, self.engine.graph());
        let wrong: Vec<String> = expect
            .iter()
            .filter_map(|&(feed, tap, want)| {
                let r = p.get(feed, tap);
                let ok = if want { r >= PRESENT } else { r < ABSENT };
                (!ok).then(|| format!("{feed} at {}: r {r:.3}, expected {}", tap.label(self.engine.graph()), if want { "present" } else { "absent" }))
            })
            .collect();
        if wrong.is_empty() { Ok(p) } else { Err(format!("{}\n{p}", wrong.join("; "))) }
    }
}

impl GraphEngine {
    /// The last block a node rendered (stereo interleaved, `n2` samples).
    fn node_block(&self, node: NodeId, n2: usize) -> Option<&[f32]> {
        let voice = self.voices.iter().find(|v| v.node == node).map(|v| &v.scratch[..n2]);
        voice
            .or_else(|| self.dsp.iter().find(|d| d.node == node).map(|d| &d.buf[..n2]))
            .or_else(|| self.lv2.iter().find(|l| l.node == node).map(|l| &l.buf[..n2]))
            .or_else(|| self.audio_in.iter().find(|a| a.node == node).map(|a| &a.buf[..n2]))
    }
}

/// A tap that peaks below this (−60 dBFS, where the meters go dark) is
/// silent: the master's DC blocker rings on at about −76 dBFS for longer
/// than the settle after pink noise is cut, and a correlation is normalized,
/// so that tail alone would read as a faint presence.
pub const SILENT: f32 = 1e-3;

fn heard_presence(sent: &[f32], heard: &[f32]) -> f32 {
    if analysis::peak(heard) < SILENT { 0.0 } else { analysis::presence(sent, heard, MAX_LAG) }
}

/// How strongly each feed is present at each tap (0..1).
pub struct Presence {
    feeds: Vec<&'static str>,
    taps: Vec<(Tap, String)>,
    r: Vec<Vec<f32>>,
}

impl Presence {
    pub fn of(run: &Run, graph: &Graph) -> Self {
        Self {
            feeds: run.feeds.iter().map(|(n, _)| *n).collect(),
            taps: run.taps.iter().map(|(t, _)| (*t, t.label(graph))).collect(),
            r: run.feeds.iter().map(|(_, sent)| run.taps.iter().map(|(_, heard)| heard_presence(sent, heard)).collect()).collect(),
        }
    }

    pub fn get(&self, feed: &str, tap: Tap) -> f32 {
        let f = self.feeds.iter().position(|n| *n == feed).unwrap_or_else(|| panic!("no feed {feed}"));
        let t = self.taps.iter().position(|(x, _)| *x == tap).unwrap_or_else(|| panic!("no tap {tap:?}"));
        self.r[f][t]
    }
}

impl std::fmt::Display for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:>10}", "")?;
        for (_, label) in &self.taps {
            write!(f, " {label:>9}")?;
        }
        for (name, row) in self.feeds.iter().zip(&self.r) {
            write!(f, "\n{name:>10}")?;
            for r in row {
                write!(f, " {r:>9.3}")?;
            }
        }
        Ok(())
    }
}

pub(super) fn wire(g: &mut Graph, id: u32, from: NodeId, to: NodeId, port: u8) {
    assert!(g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port }, PortKind::Audio));
}

fn audio_in(g: &mut Graph, name: &str) -> NodeId {
    g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes(name))
}

/// Quiet white noise: two feeds summed stay far below the master limiter.
pub fn noise(seed: u64) -> impl Iterator<Item = f32> {
    analysis::white(seed).map(|x| x * 0.1)
}

/// Quiet pink noise, about as loud as `noise`.
pub fn pink_noise(seed: u64) -> impl Iterator<Item = f32> {
    analysis::pink(seed).map(|x| x * 0.35)
}

/// Two rigs, each Audio In → Gain → its own Master, plus an unwired Gain C
/// and an aux bus nothing is sent to yet. In A gets white noise, In B pink.
pub struct TwoRigs {
    pub probe: Probe,
    pub in_a: NodeId,
    pub in_b: NodeId,
    pub gain_a: NodeId,
    pub gain_b: NodeId,
    pub gain_c: NodeId,
    pub bus: NodeId,
    /// The cable In A → Gain A.
    pub cable_a: u32,
}

impl TwoRigs {
    pub fn new() -> Self {
        let mut g = Graph::default();
        let (in_a, in_b) = (audio_in(&mut g, "IN A"), audio_in(&mut g, "IN B"));
        let gain_a = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN A"));
        let gain_b = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN B"));
        let gain_c = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN C"));
        let bus = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("BUS"));
        let master_a = g.add_node(NodeKind::MasterOut, name_bytes("MASTER A"));
        let master_b = g.add_node(NodeKind::MasterOut, name_bytes("MASTER B"));
        for n in [in_b, gain_b, master_b] {
            g.set_rig(n, 1);
        }
        wire(&mut g, 1, in_a, gain_a, 0);
        wire(&mut g, 2, gain_a, master_a, 0);
        wire(&mut g, 3, gain_a, master_a, 1);
        wire(&mut g, 4, in_b, gain_b, 0);
        wire(&mut g, 5, gain_b, master_b, 0);
        wire(&mut g, 6, gain_b, master_b, 1);
        let mut probe = Probe::new(g);
        probe.feed("A", in_a, noise(11));
        probe.feed("B", in_b, pink_noise(22));
        Self { probe, in_a, in_b, gain_a, gain_b, gain_c, bus, cable_a: 1 }
    }

    /// Rows for "A at its gain and the master, B at its gain and the
    /// master, A and B at the aux bus" (each `bool` says whether that feed
    /// is heard), with Gain C carrying nothing and each gain only its own
    /// feed.
    pub fn rows(&self, a: (bool, bool), b: (bool, bool), sent: (bool, bool)) -> Vec<(&'static str, Tap, bool)> {
        let (ga, gb, gc, bus) = (Tap::Node(self.gain_a), Tap::Node(self.gain_b), Tap::Node(self.gain_c), Tap::Node(self.bus));
        vec![
            ("A", ga, a.0), ("A", gb, false), ("A", gc, false), ("A", bus, sent.0), ("A", Tap::MasterL, a.1), ("A", Tap::MasterR, a.1),
            ("B", ga, false), ("B", gb, b.0), ("B", gc, false), ("B", bus, sent.1), ("B", Tap::MasterL, b.1), ("B", Tap::MasterR, b.1),
        ]
    }

    /// Each step: what it does to the rack, and where A and B must then be
    /// heard (at their gain, at the master), and which of them at the aux
    /// bus. The CLI and the unit test run the same table.
    pub fn steps(&self) -> Vec<(&'static str, Vec<Command>, (bool, bool), (bool, bool), (bool, bool))> {
        let (in_a, in_b) = (self.in_a, self.in_b);
        let a_cable = Command::Connect {
            id: self.cable_a,
            from: PortRef { node: in_a, port: 0 },
            to: PortRef { node: self.gain_a, port: 0 },
            kind: PortKind::Audio,
        };
        let strip = |node, mute, solo| Command::SetStripMix { node, pan: None, mute, solo };
        let rig = |rig, mute, solo| Command::SetRigMix { rig, vol: None, pan: None, mute: Some(mute), solo: Some(solo) };
        let send = |node| Command::SetSend { from: SendSource::Node(node), bus: self.bus, level: Some(1.0), pre: Some(false) };
        let (none, only_b) = ((false, false), (false, true));
        vec![
            ("as wired", vec![], (true, true), (true, true), none),
            ("In A unplugged", vec![Command::Disconnect(self.cable_a)], (false, false), (true, true), none),
            ("In A plugged back", vec![a_cable], (true, true), (true, true), none),
            ("In B muted", vec![strip(in_b, Some(true), None)], (true, true), (false, false), none),
            ("In B unmuted, In A soloed (its rig only)", vec![strip(in_b, Some(false), None), strip(in_a, None, Some(true))], (true, true), (true, true), none),
            ("rig A soloed", vec![strip(in_a, None, Some(false)), rig(0, false, true)], (true, true), (true, false), none),
            ("rig A muted, solo off", vec![rig(0, true, false)], (true, false), (true, true), none),
            ("In B sent to the bus", vec![send(in_b)], (true, false), (true, true), only_b),
            ("In B muted: its send goes too", vec![strip(in_b, Some(true), None)], (true, false), (false, false), none),
        ]
    }
}

/// Audio In → Pan hard left → Master, and the same input through `chain`
/// → Pan hard right → Master: the master's L is the dry path, its R the
/// latent one. Returns the probe and the chain's units.
pub fn split_paths(chain: &[DspKind]) -> (Probe, Vec<NodeId>) {
    let kinds: Vec<NodeKind> = chain.iter().map(|k| NodeKind::Dsp(k.clone())).collect();
    split_paths_of(&kinds)
}

/// `split_paths` with any units; LV2 ones are hosted as the app hosts them.
pub fn split_paths_of(chain: &[NodeKind]) -> (Probe, Vec<NodeId>) {
    let mut g = Graph::default();
    let ain = audio_in(&mut g, "IN");
    let left = g.add_node(NodeKind::Dsp(DspKind::Pan), name_bytes("PAN L"));
    let right = g.add_node(NodeKind::Dsp(DspKind::Pan), name_bytes("PAN R"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let units: Vec<NodeId> = chain.iter().map(|k| g.add_node(k.clone(), name_bytes("UNIT"))).collect();
    wire(&mut g, 1, ain, left, 0);
    wire(&mut g, 2, left, master, 0);
    wire(&mut g, 3, left, master, 1);
    let mut prev = ain;
    for (i, &u) in units.iter().enumerate() {
        wire(&mut g, 10 + i as u32, prev, u, 0);
        prev = u;
    }
    wire(&mut g, 4, prev, right, 0);
    wire(&mut g, 5, right, master, 0);
    wire(&mut g, 6, right, master, 1);
    let hosted = chain.iter().any(|k| matches!(k, NodeKind::Lv2 { .. }));
    let mut probe = if hosted { Probe::hosting(g) } else { Probe::new(g) };
    probe.send(Command::SetParam { node: left, param: 0, value: 0.0 });
    probe.send(Command::SetParam { node: right, param: 0, value: 1.0 });
    probe.feed("noise", ain, analysis::white(5).map(|x| x * 0.2));
    (probe, units)
}

/// Settle, then the lag of the master's R behind its L and their
/// correlation there (`split_paths`).
pub fn lag_r_behind_l(probe: &mut Probe) -> analysis::XcorrPeak {
    probe.settle(SETTLE_BLOCKS);
    let run = probe.run(32, &[Tap::MasterL, Tap::MasterR]);
    analysis::xcorr_peak(&run.taps[0].1, &run.taps[1].1, 2 * MAX_LAG)
}

/// MIDI notes of a 16-note chord (C7 over four octaves, up to 932 Hz).
const CHORD: [u8; 16] = [36, 40, 43, 46, 48, 52, 55, 58, 60, 64, 67, 70, 72, 76, 79, 82];

pub fn midi_hz(note: u8) -> f64 {
    440.0 * 2f64.powf((note as f64 - 69.0) / 12.0)
}

/// The chord at `amp` per note, as a signal (phases spread so the sum's
/// crest is a played chord's, not 16 sines starting at zero together).
pub fn chord(amp: f32) -> impl Iterator<Item = f32> {
    (0u64..).map(move |i| {
        let t = i as f64 / SR as f64;
        CHORD.iter().enumerate().map(|(k, &n)| (std::f64::consts::TAU * midi_hz(n) * t + k as f64).sin()).sum::<f64>() as f32 * amp
    })
}

/// A second of the chord at `amp` per note, Audio In → Master at the
/// default volume and WARM: the master's left output after a settle.
pub fn chord_through_master(amp: f32) -> Vec<f32> {
    let mut g = Graph::default();
    let ain = audio_in(&mut g, "CHORD");
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    wire(&mut g, 1, ain, master, 0);
    wire(&mut g, 2, ain, master, 1);
    let mut probe = Probe::new(g);
    probe.feed("chord", ain, chord(amp));
    probe.settle(SETTLE_BLOCKS);
    let blocks = SR as usize / probe.block;
    probe.run(blocks, &[Tap::MasterL]).taps.remove(0).1
}

/// The master's own threshold: below it the final knee is transparent;
/// a chord at the default volume never reaches it (rule 120).
pub const MASTER_KNEE: f32 = MasterLimiter::KNEE;

/// What the crackle checks measure on a master output: share of samples at
/// the knee, the longest flat top, and the power share above 8 kHz.
#[derive(Clone, Copy, Debug)]
pub struct Crackle {
    pub at_knee: f32,
    pub flat_top: usize,
    pub above_8k: f32,
}

impl Crackle {
    pub fn of(x: &[f32]) -> Self {
        Self {
            at_knee: analysis::hard_saturation_share(x, MASTER_KNEE),
            flat_top: analysis::flat_top_run(x),
            above_8k: analysis::Spectrum::of(x, SR).share_above(8000.0),
        }
    }
}

/// Longest flat top a clean master may show: 1 ms.
pub const FLAT_TOP_MAX: usize = SR as usize / 1000;
/// Power share above 8 kHz a clean master stays under: −60 dB. The default
/// WARM gives a loud chord harmonics up there (−74 dB measured; the same
/// chord played quietly: −145 dB), a hard clipper −50 dB.
pub const HF_MAX: f32 = 1e-6;

/// `x` driven 2× into a hard clipper at the master's knee: what a broken
/// master would put out.
pub fn hard_clipped(x: &[f32]) -> Vec<f32> {
    x.iter().map(|s| (s * 2.0).clamp(-MASTER_KNEE, MASTER_KNEE)).collect()
}

/// One second of white noise at the master, through `chain` as strip
/// inserts (Audio In → units → Master) or as master inserts (Audio In →
/// Master, Master OUT → units): the same units on the same signal either
/// way, because one source at unity is the whole mix. `bypassed` switches
/// every unit off. Stereo interleaved.
pub fn noise_through_inserts(chain: &[DspKind], on_master: bool, bypassed: bool) -> Vec<f32> {
    let mut g = Graph::default();
    let ain = audio_in(&mut g, "IN");
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let mut prev = if on_master { master } else { ain };
    let mut units = Vec::new();
    for (i, k) in chain.iter().enumerate() {
        let unit = g.add_node(NodeKind::Dsp(k.clone()), name_bytes("UNIT"));
        wire(&mut g, 10 + i as u32, prev, unit, 0);
        units.push(unit);
        prev = unit;
    }
    let from = if on_master { ain } else { prev };
    wire(&mut g, 1, from, master, 0);
    wire(&mut g, 2, from, master, 1);
    let mut probe = Probe::new(g);
    for node in units {
        probe.send(Command::SetStripBypass { node, bypass: bypassed });
    }
    probe.feed("noise", ain, noise(7));
    let blocks = SR as usize / probe.block;
    probe.run(blocks, &[]).master
}

#[cfg(test)]
mod tests {
    use super::*;

    /// White noise in two rigs shows up exactly where it is routed, before
    /// and after a disconnect, a mute, both kinds of solo and a send to an
    /// aux bus; an unwired unit carries nothing.
    #[test]
    fn noise_appears_exactly_where_it_is_routed() {
        let mut rigs = TwoRigs::new();
        for (step, cmds, a, b, sent) in rigs.steps() {
            for c in cmds {
                rigs.probe.send(c);
            }
            let rows = rigs.rows(a, b, sent);
            if let Err(e) = rigs.probe.check(&rows) {
                panic!("{step}: {e}");
            }
        }
    }

    /// The crackle regression (rules 49, 62, 120): a loud 16-note chord at
    /// the default volume never reaches the master's knee, keeps its high
    /// end under −60 dB like the same chord played quietly, and has no flat
    /// top longer than 1 ms, while the same measures flag a hard-clipped copy
    /// of it. The limiter's instant attack holds a rising peak at its
    /// ceiling for a moment (21 samples measured); a clipper for milliseconds.
    #[test]
    fn a_loud_chord_through_the_master_is_as_clean_as_a_quiet_one() {
        let loud = chord_through_master(0.25);
        let quiet = chord_through_master(0.01);
        let (l, q, c) = (Crackle::of(&loud), Crackle::of(&quiet), Crackle::of(&hard_clipped(&loud)));
        assert!(analysis::peak(&loud) > 0.5, "the chord is loud at the master: peak {}", analysis::peak(&loud));
        for m in [l, q] {
            assert!(m.at_knee == 0.0 && m.flat_top < FLAT_TOP_MAX && m.above_8k < HF_MAX, "loud {l:?}, quiet {q:?}");
        }
        assert!(c.at_knee > 0.05 && c.flat_top > FLAT_TOP_MAX && c.above_8k > HF_MAX, "clipped {c:?}");
    }

    /// A master insert runs once per block, on the mix (#154): a unit with
    /// state sounds the same after Master OUT as it does in a strip. Run a
    /// second time on silence, a Delay played a block of its line and then
    /// skipped one, and a Limiter's gain recovered between the blocks.
    #[test]
    fn a_master_insert_sounds_like_the_same_unit_in_a_strip() {
        use DspKind::*;
        let rms_of = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let apart = |a: &[f32], b: &[f32]| rms_of(&a.iter().zip(b).map(|(a, b)| a - b).collect::<Vec<f32>>());
        let dry = noise_through_inserts(&[], true, false);
        for chain in [&[Gain][..], &[Delay], &[Reverb], &[Limiter], &[Filter], &[Delay, Reverb, Limiter]] {
            let strip = noise_through_inserts(chain, false, false);
            let master = noise_through_inserts(chain, true, false);
            let (rms, off) = (rms_of(&strip), apart(&strip, &master));
            println!("{chain:?}: strip rms {rms:.5}, master insert differs by rms {off:.7}");
            assert!(rms > 0.01, "{chain:?}: the strip insert is heard, rms {rms}");
            assert!(off < rms * 1e-3, "{chain:?}: master insert differs from the strip insert, rms {off} of {rms}");
            // Bypassed, the mix passes as if the insert were not there.
            let off = apart(&dry, &noise_through_inserts(chain, true, true));
            assert!(off < rms_of(&dry) * 1e-3, "{chain:?}: a bypassed master insert changes the mix, rms {off}");
        }
    }
}
