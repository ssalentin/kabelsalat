//! The offline probe (#278): an engine of its own hosts the session's
//! plugins with their saved states, knob values, strips and sends; every rig
//! with an instrument plays its demo phrase alone (rig solo), and the master
//! says whether anything came out. Then every rig plays a chord at once and
//! each block is timed: the render headroom of the whole show. Nothing here
//! touches the app's engine or session.

use std::time::{Duration, Instant};

use super::{Installed, Probe, Render, Session, HEARD};
use crate::control::{bind_voices, command_bus, Command, CommandBus, Graph, MidiFxPlugin, Node, NodeId, NodeKind, PortKind};
use crate::engine::{GraphEngine, VoiceBack, VoiceMsg};
use crate::host::{ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;

/// Blocks after a solo switch before a rig is heard: the last rig's tail and
/// the master's DC blocker and limiter settle (rule 133).
const SETTLE: Duration = Duration::from_millis(150);
/// Audio time a rig plays before it counts as silent: the slowest demo
/// phrase's first note is under way by then.
const LISTEN: Duration = Duration::from_millis(1500);
/// Wall time an LV2 rig still gets when it was silent offline: a sample
/// player loads its samples on its worker thread (rule 178).
const WARM_UP: Duration = Duration::from_secs(3);
/// How long the LV2 loader may take to attach every instance, and to
/// restore one saved state.
const ATTACH: Duration = Duration::from_secs(10);
/// Blocks timed with every rig playing.
const TIMED_BLOCKS: usize = 200;
const CHORD: [u8; 6] = [36, 48, 55, 60, 64, 72];

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    out: Vec<f32>,
    sample_rate: f64,
    mains: Vec<(u64, ClapVoiceMain)>,
    voice_tx: rtrb::Producer<VoiceMsg>,
    back_rx: rtrb::Consumer<VoiceBack>,
    blocks: usize,
}

impl Rack {
    fn frames(&self) -> usize {
        self.out.len() / 2
    }

    /// Render one block; the loudest master sample and how long it took.
    fn block(&mut self) -> (f32, Duration) {
        let t = Instant::now();
        self.engine.render(&mut self.out, 2);
        let took = t.elapsed();
        self.blocks += 1;
        if self.blocks % 16 == 0 {
            for (_, m) in &mut self.mains {
                m.service();
            }
        }
        (self.out.iter().fold(0.0f32, |m, s| m.max(s.abs())), took)
    }

    fn blocks_in(&self, d: Duration) -> usize {
        (d.as_secs_f64() * self.sample_rate / self.frames() as f64).ceil() as usize
    }

    fn settle(&mut self, d: Duration) {
        for _ in 0..self.blocks_in(d) {
            self.block();
        }
    }

    /// Render blocks (paced a little, for the loader thread) until `done`
    /// or `wait` has passed; false on the timeout.
    fn until(&mut self, wait: Duration, done: impl Fn(&GraphEngine) -> bool) -> bool {
        let deadline = Instant::now() + wait;
        while !done(&self.engine) {
            if Instant::now() > deadline {
                return false;
            }
            self.block();
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// The bus holds 1024 commands: a long list goes in with blocks between.
    fn send_all(&mut self, cmds: impl IntoIterator<Item = Command>) {
        for (i, cmd) in cmds.into_iter().enumerate() {
            if i % 512 == 511 {
                self.block();
            }
            let _ = self.bus.send(cmd);
        }
        self.block();
    }

    fn send(&mut self, cmd: Command) {
        let _ = self.bus.send(cmd);
    }

    /// Hand every instance back the way the app does: LV2 ones to the
    /// loader's `lv2::retire`, CLAP voices to `retire` on this thread, their
    /// main thread. A never-free plugin is parked, never destroyed (rules
    /// 56, 133).
    fn retire(mut self) {
        self.send(Command::LoadGraph { graph: Graph::default() });
        for (id, _) in &self.mains {
            let _ = self.voice_tx.push(VoiceMsg::Retire(*id));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.mains.is_empty() && Instant::now() < deadline {
            self.block();
            while let Ok(back) = self.back_rx.pop() {
                let VoiceBack::Voice(slot) = back else { continue };
                let voice = slot.into_voice();
                if let Some(i) = self.mains.iter().position(|(id, _)| *id == voice.voice_id()) {
                    let (_, main) = self.mains.swap_remove(i);
                    main.retire(voice);
                }
            }
        }
        // The loader works through every queued retire before it sees the
        // engine's end of the queue go.
        self.block();
    }
}

/// Is `n` an instrument a note plays (a CLAP synth not fed by audio, an LV2
/// instrument, or an LV2 node the scan does not know)?
fn is_instrument(n: &Node, g: &Graph, installed: &Installed) -> bool {
    match &n.kind {
        NodeKind::Clap { .. } | NodeKind::Vst3 { .. } => {
            !g.edges.iter().any(|e| e.kind == PortKind::Audio && e.to.node == n.id)
        }
        NodeKind::Lv2 { uri } => installed.lv2_instrument(uri.as_str()) || !installed.lv2.iter().any(|(u, _)| u == uri.as_str()),
        _ => false,
    }
}

/// The LV2 URI an installed LV2 node (instrument, effect or MIDI unit) loads.
fn installed_lv2<'a>(n: &'a Node, installed: &Installed) -> Option<&'a str> {
    let uri = match &n.kind {
        NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: MidiFxPlugin::Lv2 { uri } } => uri.as_str(),
        _ => return None,
    };
    installed.lv2.iter().any(|(u, _)| u == uri).then_some(uri)
}

pub fn run(s: &Session, installed: &Installed, buffer_frames: u32, sample_rate: f64) -> (Probe, Render) {
    let graph = s.graph.clone();
    let mut probe = Probe::default();

    // One voice per CLAP or VST3 node whose plugin is installed, each with the
    // state of the node the engine will bind it to (rule 103).
    let mut voices: Vec<(NodeId, ClapVoice)> = Vec::new();
    let mut mains: Vec<(u64, ClapVoiceMain)> = Vec::new();
    let mut keys: Vec<(u64, String)> = Vec::new();
    for n in &graph.nodes {
        let Some(key) = n.kind.voice_plugin_key() else { continue };
        let is_installed = match &n.kind {
            NodeKind::Clap { .. } => installed.clap.iter().any(|c| c == &key),
            NodeKind::Vst3 { plugin_id } => {
                let raw = crate::vst3::strip_vst3_key(plugin_id.as_str()).unwrap_or(plugin_id.as_str());
                installed.vst3.iter().any(|c| c.eq_ignore_ascii_case(raw))
            }
            _ => false,
        };
        if !is_installed {
            continue;
        }
        match ClapVoice::new_plugin(sample_rate, &key) {
            Ok((voice, main)) => {
                keys.push((voice.voice_id(), key));
                mains.push((voice.voice_id(), main));
                voices.push((NodeId::MAX, voice));
            }
            Err(e) => probe.failed.push((n.id, format!("{e:#}"))),
        }
    }
    let pairs: Vec<(u64, &str)> = keys.iter().map(|(id, k)| (*id, k.as_str())).collect();
    for (node, voice) in bind_voices(&graph, &pairs) {
        let (Some(id), Some(st)) = (voice, s.plugin_state(node)) else { continue };
        if let Some((_, main)) = mains.iter_mut().find(|(v, _)| *v == id) {
            if let Err(e) = main.load_state(&st.data) {
                probe.failed.push((node, format!("its saved state did not load ({e:#})")));
            }
        }
    }

    let (bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, sample_rate, voices);
    let (voice_tx, voice_rx) = rtrb::RingBuffer::new(64);
    let (back_tx, back_rx) = rtrb::RingBuffer::new(64);
    engine.set_voice_channels(voice_rx, back_tx);
    engine.spawn_lv2_loader();
    let frames = (buffer_frames as usize).clamp(16, 8192);
    let mut rack = Rack { engine, bus, out: vec![0.0; frames * 2], sample_rate, mains, voice_tx, back_rx, blocks: 0 };

    // The session as the app loads it: graph and LV2 instances first (rule
    // 144), then each saved LV2 state, one at a time so a refused one is
    // named, then knobs, strips and sends.
    let lv2: Vec<NodeId> = graph.nodes.iter().filter(|n| installed_lv2(n, installed).is_some()).map(|n| n.id).collect();
    rack.send_all([Command::LoadGraph { graph: graph.clone() }]);
    rack.until(ATTACH, |e| e.lv2_attached().count() >= lv2.len());
    for &node in &lv2 {
        if !rack.engine.lv2_attached().any(|a| a == node) {
            probe.failed.push((node, "the LV2 loader did not attach it".into()));
        }
    }
    for n in graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Lv2 { .. }) && lv2.contains(&n.id)) {
        let Some(st) = s.plugin_state(n.id) else { continue };
        let (answers, restores) = rack.engine.lv2_states();
        rack.send(Command::LoadLv2State { node: n.id, state: st.data.clone() });
        rack.until(ATTACH, |e| e.lv2_states().0 > answers);
        if rack.engine.lv2_states().1 == restores {
            probe.failed.push((n.id, "its saved state did not load".into()));
        }
    }
    let mut cmds: Vec<Command> = s.params().iter().map(|p| Command::SetParam { node: p.node, param: p.param, value: p.value }).collect();
    for st in s.strips() {
        cmds.push(Command::SetStripMix { node: st.node, pan: Some(st.pan), mute: Some(st.mute), solo: Some(st.solo) });
        if st.bypass {
            cmds.push(Command::SetStripBypass { node: st.node, bypass: true });
        }
    }
    cmds.extend(s.sends().iter().map(|x| Command::SetSend { from: x.from, bus: x.bus, level: Some(x.level), pre: Some(x.pre) }));
    rack.send_all(cmds);
    rack.settle(SETTLE);

    // Each rig alone.
    let mut rigs: Vec<u8> = graph.nodes.iter().filter(|n| is_instrument(n, &graph, installed)).map(|n| n.rig).collect();
    rigs.sort_unstable();
    rigs.dedup();
    for &rig in &rigs {
        let first = graph.nodes.iter().find(|n| n.rig == rig && is_instrument(n, &graph, installed)).expect("a rig with an instrument");
        let ty = s.rig_types.get(rig as usize).copied().flatten().unwrap_or_else(|| crate::keyboard::classify_instrument(&first.kind, None, None, &[]));
        rack.send(Command::SetRigMix { rig, vol: None, pan: None, mute: None, solo: Some(true) });
        rack.settle(SETTLE);
        rack.send(Command::DemoPlay { rig, phrase: crate::demo::kind_for(ty, None, None) });
        let mut peak = 0.0f32;
        for _ in 0..rack.blocks_in(LISTEN) {
            peak = peak.max(rack.block().0);
            if peak >= HEARD {
                break;
            }
        }
        let has_lv2 = graph.nodes.iter().any(|n| n.rig == rig && matches!(n.kind, NodeKind::Lv2 { .. }));
        let block_time = Duration::from_secs_f64(rack.frames() as f64 / sample_rate);
        let warm = Instant::now() + WARM_UP;
        while has_lv2 && peak < HEARD && Instant::now() < warm {
            peak = peak.max(rack.block().0);
            std::thread::sleep(block_time);
        }
        rack.send(Command::DemoStop);
        probe.rigs.push((rig, peak));
    }
    if let Some(&last) = rigs.last() {
        rack.send(Command::SetRigMix { rig: last, vol: None, pan: None, mute: None, solo: Some(false) });
    }

    // Every rig at once, timed.
    rack.settle(SETTLE);
    rack.send_all(CHORD.map(|note| Command::NoteOn { note, velocity: 0.8, source: SourceId::Keys, channel: 0 }));
    let (mut sum, mut max) = (Duration::ZERO, Duration::ZERO);
    for _ in 0..TIMED_BLOCKS {
        let (_, took) = rack.block();
        sum += took;
        max = max.max(took);
    }
    rack.send_all(CHORD.map(|note| Command::NoteOff { note, source: SourceId::Keys, channel: 0 }));
    let render = Render {
        sample_rate: sample_rate as u32,
        buffer_frames: frames as u32,
        avg_us: sum.as_secs_f64() * 1e6 / TIMED_BLOCKS as f64,
        max_us: max.as_secs_f64() * 1e6,
    };
    rack.retire();
    (probe, render)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, AudioInChannel, ClapId, PortRef, SURGE_PLUGIN_ID};

    /// The fixture LV2 unit (built with the system's C compiler) behind an
    /// Audio In: the probe waits until it is attached, names the saved
    /// state it cannot take, and hands the instance back to the loader.
    #[test]
    fn an_lv2_unit_is_attached_first_and_a_state_it_refuses_is_named() {
        crate::engine::latent::install().unwrap();
        let installed = Installed { clap: vec![], vst3: vec![], lv2: vec![(crate::engine::latent::URI.into(), false)] };
        let mut g = Graph::default();
        let input = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let fx = g.add_node(crate::engine::latent::kind(), name_bytes("LATENT"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: input, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Audio);
        g.connect_with_id(2, PortRef { node: fx, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let mut s = Session::new("LV2", g);
        let (probe, _) = run(&s, &installed, 256, 48_000.0);
        assert_eq!(probe, Probe::default(), "attached, nothing to play, nothing refused");
        s.set_plugin_state(fx, crate::engine::latent::URI, vec![1, 2, 3]);
        let t = Instant::now();
        let (probe, _) = run(&s, &installed, 256, 48_000.0);
        assert_eq!(probe.failed, vec![(fx, "its saved state did not load".to_string())]);
        assert!(t.elapsed() < ATTACH, "the loader's refusal is the answer, not a timeout");
    }

    /// Rig A: Keys → Surge → Master. Rig B: the same Surge with no cable to
    /// its Master. Real Surge XT; skipped where it is not installed (CI).
    #[test]
    fn the_probe_hears_a_wired_rig_and_not_one_without_a_cable_to_master() {
        let installed = super::super::gather::installed();
        if !installed.clap.iter().any(|c| c == SURGE_PLUGIN_ID) {
            eprintln!("skipped: Surge XT is not installed");
            return;
        }
        let mut g = Graph::default();
        g.reserve(16, 16);
        for rig in 0..2u8 {
            let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let surge = g.add_node(NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) }, name_bytes("SURGE"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            let e = rig as u32 * 10;
            g.connect_with_id(e + 1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
            if rig == 0 {
                g.connect_with_id(e + 2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
                g.connect_with_id(e + 3, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
            }
            for n in g.nodes.iter_mut().filter(|n| [keys, surge, master].contains(&n.id)) {
                n.rig = rig;
            }
        }
        let s = Session::new("Probe", g);
        let (probe, render) = run(&s, &installed, 256, 48_000.0);
        assert!(probe.failed.is_empty(), "{:?}", probe.failed);
        assert_eq!(probe.rigs.len(), 2, "{:?}", probe.rigs);
        assert!(probe.rigs[0].1 >= HEARD, "the wired rig sounds: {:?}", probe.rigs);
        assert!(probe.rigs[1].1 < HEARD, "the rig without a cable to Master is silent: {:?}", probe.rigs);
        assert_eq!((render.buffer_frames, render.sample_rate), (256, 48_000));
        assert!(render.avg_us > 0.0 && render.max_us >= render.avg_us, "{render:?}");
    }
}
