//! The offline probe (#278): an engine of its own hosts the session's
//! plugins with their saved states and knob values, every rig with an
//! instrument plays its demo phrase alone (rig solo), and the master says
//! whether anything came out. Then every rig plays a chord at once and each
//! block is timed, which is the render headroom when no running engine
//! measured it. Nothing here touches the app's engine or session.

use std::time::{Duration, Instant};

use super::{Installed, Probe, Render, Session, HEARD};
use crate::control::{bind_voices, clap_plugin_key, command_bus, Command, CommandBus, Graph, Node, NodeId, NodeKind, PortKind};
use crate::engine::{GraphEngine, VoiceBack, VoiceMsg};
use crate::host::{ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;

const SR: f64 = 48_000.0;
/// Blocks after a solo switch before a rig is heard: the last rig's tail and
/// the master's DC blocker and limiter settle (rule 133).
const SETTLE: Duration = Duration::from_millis(150);
/// Audio time a rig plays before it counts as silent: the slowest demo
/// phrase's first note is under way by then.
const LISTEN: Duration = Duration::from_millis(1500);
/// Wall time an LV2 rig still gets when it was silent offline: a sample
/// player loads its samples on its worker thread (rule 178).
const WARM_UP: Duration = Duration::from_secs(3);
/// How long the LV2 loader may take to attach every instance.
const ATTACH: Duration = Duration::from_secs(10);
/// Blocks timed with every rig playing.
const TIMED_BLOCKS: usize = 200;
const CHORD: [u8; 6] = [36, 48, 55, 60, 64, 72];

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    out: Vec<f32>,
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
        (d.as_secs_f64() * SR / self.frames() as f64).ceil() as usize
    }

    fn settle(&mut self, d: Duration) {
        for _ in 0..self.blocks_in(d) {
            self.block();
        }
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

    /// Hand every voice back and retire it on this thread, its main thread:
    /// a never-free plugin is parked here, never destroyed (rules 56, 133).
    fn retire(mut self) {
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
    }
}

/// Is `n` an instrument a note plays (a CLAP synth not fed by audio, an LV2
/// instrument, or an LV2 node the scan does not know)?
fn is_instrument(n: &Node, g: &Graph, installed: &Installed) -> bool {
    match &n.kind {
        NodeKind::Clap { .. } => !g.edges.iter().any(|e| e.kind == PortKind::Audio && e.to.node == n.id),
        NodeKind::Lv2 { uri } => installed.lv2_instrument(uri.as_str()) || !installed.lv2.iter().any(|(u, _)| u == uri.as_str()),
        _ => false,
    }
}

pub fn run(s: &Session, installed: &Installed, buffer_frames: u32) -> (Probe, Render) {
    let graph = s.graph.clone();
    let mut probe = Probe::default();

    // One voice per CLAP node whose plugin is installed, each with the state
    // of the node the engine will bind it to (rule 103).
    let mut voices: Vec<(NodeId, ClapVoice)> = Vec::new();
    let mut mains: Vec<(u64, ClapVoiceMain)> = Vec::new();
    let mut keys: Vec<(u64, String)> = Vec::new();
    for n in &graph.nodes {
        let NodeKind::Clap { plugin_id } = &n.kind else { continue };
        let key = clap_plugin_key(plugin_id.as_str());
        if !installed.clap.iter().any(|c| c == key) {
            continue;
        }
        match ClapVoice::new_plugin(SR, key) {
            Ok((voice, main)) => {
                keys.push((voice.voice_id(), key.to_string()));
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
    let mut engine = GraphEngine::new(Graph::default(), drain, SR, voices);
    let (voice_tx, voice_rx) = rtrb::RingBuffer::new(64);
    let (back_tx, back_rx) = rtrb::RingBuffer::new(64);
    engine.set_voice_channels(voice_rx, back_tx);
    engine.spawn_lv2_loader();
    let frames = (buffer_frames as usize).clamp(16, 8192);
    let mut rack = Rack { engine, bus, out: vec![0.0; frames * 2], mains, voice_tx, back_rx, blocks: 0 };

    // The session as the app loads it: graph, LV2 states, knobs, strips.
    let lv2: Vec<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| matches!(&n.kind, NodeKind::Lv2 { uri } if installed.lv2.iter().any(|(u, _)| u == uri.as_str())))
        .map(|n| n.id)
        .collect();
    let mut cmds = vec![Command::LoadGraph { graph: graph.clone() }];
    for &node in &lv2 {
        if let Some(st) = s.plugin_state(node) {
            cmds.push(Command::LoadLv2State { node, state: st.data.clone() });
        }
    }
    cmds.extend(s.params().iter().map(|p| Command::SetParam { node: p.node, param: p.param, value: p.value }));
    for st in s.strips() {
        cmds.push(Command::SetStripMix { node: st.node, pan: Some(st.pan), mute: Some(st.mute), solo: Some(st.solo) });
        if st.bypass {
            cmds.push(Command::SetStripBypass { node: st.node, bypass: true });
        }
    }
    rack.send_all(cmds);
    let deadline = Instant::now() + ATTACH;
    while rack.engine.lv2_attached().count() < lv2.len() && Instant::now() < deadline {
        rack.block();
        std::thread::sleep(Duration::from_millis(2));
    }
    for &node in &lv2 {
        if !rack.engine.lv2_attached().any(|a| a == node) {
            probe.failed.push((node, "the LV2 loader did not attach it".into()));
        }
    }
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
        let block_time = Duration::from_secs_f64(rack.frames() as f64 / SR);
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
        sample_rate: SR as u32,
        buffer_frames: frames as u32,
        avg_us: sum.as_secs_f64() * 1e6 / TIMED_BLOCKS as f64,
        max_us: max.as_secs_f64() * 1e6,
        xruns: None,
    };
    rack.retire();
    (probe, render)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, ClapId, PortRef, SURGE_PLUGIN_ID};

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
        let (probe, render) = run(&s, &installed, 256);
        assert!(probe.failed.is_empty(), "{:?}", probe.failed);
        assert_eq!(probe.rigs.len(), 2, "{:?}", probe.rigs);
        assert!(probe.rigs[0].1 >= HEARD, "the wired rig sounds: {:?}", probe.rigs);
        assert!(probe.rigs[1].1 < HEARD, "the rig without a cable to Master is silent: {:?}", probe.rigs);
        assert_eq!((render.buffer_frames, render.sample_rate), (256, 48_000));
        assert!(render.avg_us > 0.0 && render.max_us >= render.avg_us, "{render:?}");
    }
}
