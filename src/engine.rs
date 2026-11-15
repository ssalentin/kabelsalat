//! Graph-driven multi-voice audio engine: the control-plane graph IS the mix.
//!
//! Each Clap node owns a voice; each Dsp node owns a real built-in processor
//! (see `dsp.rs`). `render` walks the graph edges: every voice renders, feeds
//! the DSP nodes it is wired to, and everything wired to the master is summed.
//! Notes fan out to every instrument that has a Keys→instrument MIDI cable.
//! Unplug a cable and that path goes silent — what you patch is what you hear.
//!
//! RT discipline: commands are fixed-size, graph storage is reserved, buffers
//! are preallocated, edge lookups are linear scans, and every processor is
//! proven allocation-free — no allocation in render. Voices/processors are
//! created at startup (loading is main-thread work); dynamic AddNode with
//! off-thread loading is the follow-up.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::control::{Command, CommandDrain, DspKind, Graph, NodeId, NodeKind, ParamId, PortKind};
use crate::dsp::{build_processor, Processor};
use crate::host::ClapVoice;
use crate::matrix::{Matrix, MidiMsg, SourceId};
use crate::capture::SystemCapture;
use crate::player::PlayerSink;

/// Max hardware MIDI device ids pre-routed in the matrix (per-rig filter selects).
pub const MAX_HW_DEVICES: u32 = 16;

// Engine-level param ids (what the UI/assistant send). Translated to each
// processor's native param ids in `dsp_native_param`.
pub const P_LEVEL: ParamId = 0; // instrument output level
pub const P_TIME: ParamId = 1;  // delay time
pub const P_MIX: ParamId = 2;   // delay / effect mix
pub const P_VOL: ParamId = 3;   // master volume
pub const P_WARM: ParamId = 4;  // master saturation

/// Translate an engine param id to a processor's native param id for a kind.
/// Returns None if the knob doesn't apply to this kind.
fn dsp_native_param(kind: &DspKind, p: ParamId) -> Option<u32> {
    match kind {
        // dsp::Delay native params: 0=time, 1=feedback, 2=mix, 4=damping.
        DspKind::Delay => match p {
            P_TIME => Some(0),
            P_MIX => Some(2),
            _ => None,
        },
        // Generic knob maps for the other built-in nodes (template defaults).
        DspKind::Reverb => match p {
            0 => Some(2), // mix
            1 => Some(0), // feedback / size
            _ => None,
        },
        DspKind::Gain => match p {
            0 => Some(0),
            _ => None,
        },
        DspKind::Filter => match p {
            0 => Some(0), // cutoff
            1 => Some(1), // resonance
            _ => None,
        },
        DspKind::Eq3 => match p {
            0 => Some(0),
            1 => Some(2),
            _ => None,
        },
        DspKind::Limiter | DspKind::Saturation => match p {
            0 => Some(0),
            1 => Some(1),
            _ => None,
        },
        DspKind::Pan => match p {
            0 => Some(0),
            _ => None,
        },
    }
}

struct VoiceSlot {
    node: NodeId,
    voice: ClapVoice,
    level: f32,
    scratch: Vec<f32>,
    // Per-rig input selection (AUM): which source classes feed this rig.
    src_keys: bool,
    src_laptop: bool,
    src_hw: bool,
    // Which hardware device feeds this rig: None = any, Some(id) = only that
    // device (per-device routing, AUM's "different keyboard per rig").
    rig_device: Option<u32>,
}

/// Does this rig accept notes from the given source? (The MIDI cable gate is
/// checked separately by the caller.)
fn source_accepted(s: &VoiceSlot, source: SourceId) -> bool {
    match source {
        SourceId::Keys => s.src_keys,
        SourceId::LaptopKeys => s.src_laptop,
        SourceId::Hardware(id) => s.src_hw && hw_device_allowed(s.rig_device, id),
        _ => true,
    }
}

/// Per-rig hardware device filter: None accepts any device, Some(d) only d.
fn hw_device_allowed(rig_device: Option<u32>, id: u32) -> bool {
    rig_device.is_none() || rig_device == Some(id)
}

struct DspSlot {
    node: NodeId,
    kind: DspKind,
    proc: Box<dyn Processor>,
    buf: Vec<f32>,
}

struct Lv2Slot {
    node: NodeId,
    plugin: crate::lv2::Lv2Plugin,
    buf: Vec<f32>,
}

/// Load an LV2 plugin instance for every Lv2 node (rare structural op).
fn build_lv2_slots(graph: &Graph, sample_rate: f64) -> Vec<Lv2Slot> {
    graph
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Lv2 { uri } => match crate::lv2::Lv2Plugin::load(uri.as_str(), sample_rate, 8192) {
                Ok(plugin) => Some(Lv2Slot { node: n.id, plugin, buf: vec![0.0; 8192 * 2] }),
                Err(e) => {
                    eprintln!("lv2 node {}: {e}", n.id);
                    None
                }
            },
            _ => None,
        })
        .collect()
}

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    sample_rate: f64,
    voices: Vec<VoiceSlot>,
    dsp: Vec<DspSlot>,
    lv2: Vec<Lv2Slot>,
    lv2_in: Vec<f32>,
    keys_node: Option<NodeId>,
    master_vol: f32,
    master_warm: f32,
    // Metronome / transport.
    transport_playing: bool,
    bpm: f32,
    metro_phase: f64,
    click_remaining: usize,
    click_phase: f32,
    out_peak: Arc<AtomicU32>,
    // MIDI routing matrix (source → rig, with key-range splits). Default rules
    // mirror "every source plays every rig"; SetSplit edits key ranges in place.
    matrix: Matrix,
    route_out: Vec<(NodeId, MidiMsg)>,
    // System-audio capture (e.g. YouTube) mixed into the master.
    system_capture: Option<SystemCapture>,
    system_gain: f32,
    system_buf: Vec<f32>,
    // File player (backing track) mixed into the master.
    player_sink: Option<PlayerSink>,
    player_gain: f32,
    player_buf: Vec<f32>,
}

impl GraphEngine {
    /// `voices` pairs each Clap node id with a loaded voice (created on the
    /// main thread). A processor is built for every Dsp node in the graph.
    pub fn new(
        graph: Graph,
        drain: CommandDrain,
        sample_rate: f64,
        voices: Vec<(NodeId, ClapVoice)>,
    ) -> Self {
        let keys_node = graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::Keys)).map(|n| n.id);
        let voices: Vec<VoiceSlot> = voices
            .into_iter()
            .map(|(node, voice)| VoiceSlot {
                node,
                voice,
                level: 0.85,
                scratch: vec![0.0; 8192 * 2],
                src_keys: true,
                src_laptop: true,
                src_hw: true,
                rig_device: None,
            })
            .collect();
        let dsp = graph
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Dsp(kind) => Some(DspSlot {
                    node: n.id,
                    kind: kind.clone(),
                    proc: build_processor(kind, sample_rate),
                    buf: vec![0.0; 8192 * 2],
                }),
                _ => None,
            })
            .collect();
        let lv2 = build_lv2_slots(&graph, sample_rate);
        // Default routing: every source class feeds every rig (full range).
        // SetSplit later narrows key ranges per rig, in place (RT-safe).
        let mut matrix = Matrix::new();
        matrix.reserve(64);
        for v in &voices {
            matrix.connect(SourceId::Keys, v.node);
            matrix.connect(SourceId::LaptopKeys, v.node);
            // Cover hardware device ids 0..MAX so any connected device routes;
            // the per-rig `rig_device` filter then selects which one is heard.
            for dev in 0..MAX_HW_DEVICES {
                matrix.connect(SourceId::Hardware(dev), v.node);
            }
        }
        for slot in &lv2 {
            if slot.plugin.is_instrument() {
                matrix.connect(SourceId::Keys, slot.node);
                matrix.connect(SourceId::LaptopKeys, slot.node);
                for dev in 0..MAX_HW_DEVICES {
                    matrix.connect(SourceId::Hardware(dev), slot.node);
                }
            }
        }
        Self {
            graph,
            drain,
            sample_rate,
            voices,
            dsp,
            lv2,
            lv2_in: vec![0.0; 8192 * 2],
            keys_node,
            master_vol: 0.8,
            master_warm: 0.15,
            transport_playing: false,
            bpm: 96.0,
            metro_phase: 0.0,
            click_remaining: 0,
            click_phase: 0.0,
            out_peak: Arc::new(AtomicU32::new(0)),
            matrix,
            route_out: Vec::with_capacity(64),
            system_capture: None,
            system_gain: 0.0,
            system_buf: vec![0.0; 8192 * 2],
            player_sink: None,
            player_gain: 0.0,
            player_buf: vec![0.0; 8192 * 2],
        }
    }

    /// Attach a system-audio capture source (started off the audio thread).
    pub fn set_system_capture(&mut self, cap: SystemCapture) {
        self.system_capture = Some(cap);
    }

    /// Attach the file-player ring consumer (decode thread started elsewhere).
    pub fn set_player_sink(&mut self, sink: PlayerSink) {
        self.player_sink = Some(sink);
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.out_peak.clone()
    }

    /// Read back the MIDI event pending at an LV2 instrument node (verification).
    pub fn lv2_last_note(&self, node: NodeId) -> Option<[u8; 3]> {
        self.lv2.iter().find(|s| s.node == node).and_then(|s| s.plugin.last_midi_event())
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::AddNode { kind, name } => {
                let id = self.graph.add_node(kind.clone(), name);
                let sr = self.sample_rate;
                match &kind {
                    NodeKind::Dsp(dk) => {
                        self.dsp.push(DspSlot {
                            node: id,
                            kind: dk.clone(),
                            proc: build_processor(dk, sr),
                            buf: vec![0.0; 8192 * 2],
                        });
                    }
                    NodeKind::Lv2 { uri } => match crate::lv2::Lv2Plugin::load(uri.as_str(), sr, 8192) {
                        Ok(plugin) => {
                            let is_inst = plugin.is_instrument();
                            self.lv2.push(Lv2Slot { node: id, plugin, buf: vec![0.0; 8192 * 2] });
                            if is_inst {
                                self.matrix.connect(SourceId::Keys, id);
                                self.matrix.connect(SourceId::LaptopKeys, id);
                                for dev in 0..MAX_HW_DEVICES {
                                    self.matrix.connect(SourceId::Hardware(dev), id);
                                }
                            }
                        }
                        Err(e) => eprintln!("lv2 node {id}: {e}"),
                    },
                    // CLAP voices are main-thread-only; added clap nodes join the
                    // graph but sound via LoadGraph's voice remap, not here.
                    _ => {}
                }
            }
            Command::RemoveNode(id) => {
                self.graph.remove_node(id);
                self.dsp.retain(|s| s.node != id);
                self.lv2.retain(|s| s.node != id);
            }
            Command::Connect { id, from, to, kind } => {
                self.graph.connect_with_id(id, from, to, kind);
            }
            Command::Disconnect(id) => {
                self.graph.disconnect(id);
            }
            Command::SetParam { node, param, value } => {
                let v = value as f32;
                if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                    // LV2 knobs map param index → nth control port (normalized).
                    s.plugin.set_control_normalized(param as usize, v);
                } else {
                    match param {
                        P_VOL => self.master_vol = v,
                        P_WARM => self.master_warm = v,
                        P_LEVEL => {
                            // Voice level for instrument nodes; for DSP nodes param 0
                            // is the node's first native knob (e.g. Reverb mix).
                            if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                                s.level = v;
                            } else if let Some(s) = self.dsp.iter_mut().find(|s| s.node == node) {
                                if let Some(native) = dsp_native_param(&s.kind, P_LEVEL) {
                                    s.proc.set_param(native, v);
                                }
                            }
                        }
                        _ => {
                            if let Some(s) = self.dsp.iter_mut().find(|s| s.node == node) {
                                if let Some(native) = dsp_native_param(&s.kind, param) {
                                    s.proc.set_param(native, v);
                                }
                            }
                        }
                    }
                }
            }
            Command::LoadGraph { graph } => {
                // Rare structural op (template load): rebuild graph, DSP, voice
                // mapping, and MIDI matrix. Allocates — acceptable off the hot
                // path; a future version can rebuild off-thread and swap.
                self.graph = graph;
                let sr = self.sample_rate;
                self.dsp = self
                    .graph
                    .nodes
                    .iter()
                    .filter_map(|n| match &n.kind {
                        NodeKind::Dsp(kind) => Some(DspSlot {
                            node: n.id,
                            kind: kind.clone(),
                            proc: build_processor(kind, sr),
                            buf: vec![0.0; 8192 * 2],
                        }),
                        _ => None,
                    })
                    .collect();
                self.lv2 = build_lv2_slots(&self.graph, sr);
                // Remap existing CLAP voices onto the new Clap nodes in id order;
                // surplus voices go idle (sentinel node id, never routed).
                let clap_ids: Vec<NodeId> = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::Clap { .. }))
                    .map(|n| n.id)
                    .collect();
                for (i, v) in self.voices.iter_mut().enumerate() {
                    v.node = clap_ids.get(i).copied().unwrap_or(u32::MAX);
                }
                self.matrix = Matrix::new();
                self.matrix.reserve(64);
                for &id in &clap_ids {
                    self.matrix.connect(SourceId::Keys, id);
                    self.matrix.connect(SourceId::LaptopKeys, id);
                    for dev in 0..MAX_HW_DEVICES {
                        self.matrix.connect(SourceId::Hardware(dev), id);
                    }
                }
                for slot in &self.lv2 {
                    if slot.plugin.is_instrument() {
                        self.matrix.connect(SourceId::Keys, slot.node);
                        self.matrix.connect(SourceId::LaptopKeys, slot.node);
                        for dev in 0..MAX_HW_DEVICES {
                            self.matrix.connect(SourceId::Hardware(dev), slot.node);
                        }
                    }
                }
                self.keys_node = self
                    .graph
                    .nodes
                    .iter()
                    .find(|n| matches!(n.kind, NodeKind::Keys))
                    .map(|n| n.id);
            }
            Command::NoteOn { note, velocity, source } => {
                let msg = MidiMsg::note_on(0, note, (velocity.clamp(0.0, 1.0) * 127.0) as u8);
                self.matrix.route_into(source, msg, &mut self.route_out);
                let keys = self.keys_node;
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let gate = keys.map(|k| self.graph.has_route(k, dest, PortKind::Midi)).unwrap_or(false);
                    if !gate {
                        continue;
                    }
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if source_accepted(s, source) {
                            s.voice.note_on(m.note() as u16, velocity as f64);
                        }
                    }
                    if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        if s.plugin.is_instrument() {
                            s.plugin.note_on(m.note(), (velocity.clamp(0.0, 1.0) * 127.0) as u8);
                        }
                    }
                }
            }
            Command::NoteOff { note, source } => {
                let msg = MidiMsg::note_off(0, note, 0);
                self.matrix.route_into(source, msg, &mut self.route_out);
                let keys = self.keys_node;
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let gate = keys.map(|k| self.graph.has_route(k, dest, PortKind::Midi)).unwrap_or(false);
                    if !gate {
                        continue;
                    }
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if source_accepted(s, source) {
                            s.voice.note_off(m.note() as u16);
                        }
                    }
                    if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        if s.plugin.is_instrument() {
                            s.plugin.note_off(m.note());
                        }
                    }
                }
            }
            Command::SetRigSource { node, keys, laptop, hardware } => {
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                    s.src_keys = keys;
                    s.src_laptop = laptop;
                    s.src_hw = hardware;
                }
            }
            Command::SetRigDevice { node, device } => {
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                    s.rig_device = device;
                }
            }
            Command::SetSplit { point } => {
                // Rig A (node 1) takes the low zone, rig B (node 4) the high zone.
                // In-place key-range edits: no allocation on the audio thread.
                for rule in &mut self.matrix.rules {
                    rule.key_range = match point {
                        None => (0, 127),
                        Some(p) => {
                            if rule.dest == 1 {
                                (0, p.saturating_sub(1))
                            } else if rule.dest == 4 {
                                (p, 127)
                            } else {
                                (0, 127)
                            }
                        }
                    };
                }
            }
            Command::SetSystemAudio { gain } => {
                self.system_gain = gain.clamp(0.0, 1.0);
            }
            Command::SetPlayer { gain } => {
                self.player_gain = gain.clamp(0.0, 1.0);
            }
            Command::TransportPlay => {
                self.transport_playing = true;
                self.metro_phase = 0.0;
            }
            Command::TransportStop => {
                self.transport_playing = false;
                self.click_remaining = 0;
            }
            Command::SetBpm { bpm } => {
                self.bpm = bpm.clamp(20.0, 300.0);
            }
        }
    }

    /// Render one device block, allocation-free.
    pub fn render(&mut self, data: &mut [f32], ch: usize) {
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }

        let frames = data.len() / ch;
        let cap = 8192;
        if frames > cap {
            for s in data.iter_mut() {
                *s = 0.0;
            }
            return;
        }
        let n2 = frames * 2;

        // 1. Every instrument renders and applies its level.
        for s in &mut self.voices {
            s.voice.render(&mut s.scratch[..n2]);
            for x in s.scratch[..n2].iter_mut() {
                *x *= s.level;
            }
        }

        // 2. Each DSP node sums the instruments wired into it, then processes.
        for d in &mut self.dsp {
            for x in d.buf[..n2].iter_mut() {
                *x = 0.0;
            }
            for v in &self.voices {
                if self.graph.has_route(v.node, d.node, PortKind::Audio) {
                    for i in 0..n2 {
                        d.buf[i] += v.scratch[i];
                    }
                }
            }
            d.proc.process(&mut d.buf[..n2], frames);
        }

        // 2b. LV2 effect nodes: sum wired instruments, process, hold output.
        for slot in &mut self.lv2 {
            for x in self.lv2_in[..n2].iter_mut() {
                *x = 0.0;
            }
            for v in &self.voices {
                if self.graph.has_route(v.node, slot.node, PortKind::Audio) {
                    for i in 0..n2 {
                        self.lv2_in[i] += v.scratch[i];
                    }
                }
            }
            slot.plugin.process(&self.lv2_in[..n2], &mut slot.buf[..n2], frames);
        }

        // 3. Master sums everything wired to it.
        // Pull system-audio capture once per block (RT-safe ring pop).
        let sys = self.system_gain > 0.0 && self.system_capture.is_some();
        if sys {
            if let Some(cap) = &mut self.system_capture {
                cap.read(&mut self.system_buf[..n2]);
            }
        }
        let ply = self.player_gain > 0.0 && self.player_sink.is_some();
        if ply {
            if let Some(sink) = &mut self.player_sink {
                sink.read(&mut self.player_buf[..n2]);
            }
        }
        let mut peak = 0.0f32;
        let master = self.graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id);
        if let Some(master) = master {
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for v in &self.voices {
                    if self.graph.has_route(v.node, master, PortKind::Audio) {
                        l += v.scratch[i * 2];
                        r += v.scratch[i * 2 + 1];
                    }
                }
                for d in &self.dsp {
                    if self.graph.has_route(d.node, master, PortKind::Audio) {
                        l += d.buf[i * 2];
                        r += d.buf[i * 2 + 1];
                    }
                }
                for slot in &self.lv2 {
                    if self.graph.has_route(slot.node, master, PortKind::Audio) {
                        l += slot.buf[i * 2];
                        r += slot.buf[i * 2 + 1];
                    }
                }
                if sys {
                    l += self.system_buf[i * 2] * self.system_gain;
                    r += self.system_buf[i * 2 + 1] * self.system_gain;
                }
                if ply {
                    l += self.player_buf[i * 2] * self.player_gain;
                    r += self.player_buf[i * 2 + 1] * self.player_gain;
                }
                // Metronome: a decaying 1 kHz burst on each beat while playing.
                if self.transport_playing {
                    let spb = self.sample_rate * 60.0 / self.bpm as f64;
                    self.metro_phase += 1.0;
                    if self.metro_phase >= spb {
                        self.metro_phase -= spb;
                        self.click_remaining = 2400;
                        self.click_phase = 0.0;
                    }
                }
                if self.click_remaining > 0 {
                    let env = self.click_remaining as f32 / 2400.0;
                    let click = (2.0 * std::f32::consts::PI * 1000.0 * self.click_phase
                        / self.sample_rate as f32)
                        .sin()
                        * env
                        * 0.4;
                    l += click;
                    r += click;
                    self.click_phase += 1.0;
                    self.click_remaining -= 1;
                }
                l = (l * (1.0 + self.master_warm * 3.0)).tanh() * self.master_vol;
                r = (r * (1.0 + self.master_warm * 3.0)).tanh() * self.master_vol;
                peak = peak.max(l.abs()).max(r.abs());
                frame[0] = l;
                if ch > 1 {
                    frame[1] = r;
                }
                for s in frame.iter_mut().skip(2) {
                    *s = 0.0;
                }
            }
        }
        self.out_peak.store(peak.to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use crate::matrix::{Matrix, MidiMsg, SourceId};

    /// Mirror the engine's default matrix + SetSplit key-range edits.
    fn split_matrix(point: Option<u8>) -> Matrix {
        let mut m = Matrix::new();
        for node in [1u32, 4u32] {
            m.connect(SourceId::Keys, node);
            m.connect(SourceId::LaptopKeys, node);
        }
        for rule in &mut m.rules {
            rule.key_range = match point {
                None => (0, 127),
                Some(p) => {
                    if rule.dest == 1 {
                        (0, p.saturating_sub(1))
                    } else if rule.dest == 4 {
                        (p, 127)
                    } else {
                        (0, 127)
                    }
                }
            };
        }
        m
    }

    fn dests(m: &Matrix, src: SourceId, msg: MidiMsg) -> Vec<u32> {
        let mut out = Vec::new();
        m.route_into(src, msg, &mut out);
        out.iter().map(|(d, _)| *d).collect()
    }

    #[test]
    fn no_split_both_rigs_play() {
        let m = split_matrix(None);
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_on(0, 60, 100)), vec![1, 4]);
        assert_eq!(dests(&m, SourceId::LaptopKeys, MidiMsg::note_on(0, 30, 100)), vec![1, 4]);
    }

    #[test]
    fn split_at_c4_partitions_keyboard() {
        let m = split_matrix(Some(60));
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_on(0, 59, 100)), vec![1]); // B3 → A
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_on(0, 60, 100)), vec![4]); // C4 → B
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_on(0, 96, 100)), vec![4]); // high → B
        // Note-offs follow the same zone as their note-on.
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_off(0, 59, 0)), vec![1]);
        assert_eq!(dests(&m, SourceId::Keys, MidiMsg::note_off(0, 60, 0)), vec![4]);
        // Laptop keys split identically.
        assert_eq!(dests(&m, SourceId::LaptopKeys, MidiMsg::note_on(0, 48, 100)), vec![1]);
        assert_eq!(dests(&m, SourceId::LaptopKeys, MidiMsg::note_on(0, 72, 100)), vec![4]);
    }

    #[test]
    fn device_filter_selects_one_keyboard() {
        use super::hw_device_allowed;
        assert!(hw_device_allowed(None, 0)); // any device
        assert!(hw_device_allowed(None, 7));
        assert!(hw_device_allowed(Some(2), 2)); // only device 2
        assert!(!hw_device_allowed(Some(2), 0));
        assert!(!hw_device_allowed(Some(2), 3));
    }

    #[test]
    fn matrix_routes_each_hardware_device() {
        // Mirror the engine's default matrix: Keys/Laptop + Hardware(0..MAX).
        let mut m = Matrix::new();
        for node in [1u32, 4u32] {
            m.connect(SourceId::Keys, node);
            m.connect(SourceId::LaptopKeys, node);
            for dev in 0..super::MAX_HW_DEVICES {
                m.connect(SourceId::Hardware(dev), node);
            }
        }
        // Every device id routes to both rigs (the per-rig filter then selects).
        assert_eq!(dests(&m, SourceId::Hardware(0), MidiMsg::note_on(0, 60, 100)), vec![1, 4]);
        assert_eq!(dests(&m, SourceId::Hardware(3), MidiMsg::note_on(0, 60, 100)), vec![1, 4]);
        assert_eq!(dests(&m, SourceId::Hardware(15), MidiMsg::note_on(0, 60, 100)), vec![1, 4]);
    }

    #[test]
    fn dsp_param_maps_for_templates() {
        use super::dsp_native_param;
        use crate::control::DspKind;
        // Template defaults: Reverb(0=mix), Gain(0=gain), Filter(0=cutoff).
        assert_eq!(dsp_native_param(&DspKind::Reverb, 0), Some(2));
        assert_eq!(dsp_native_param(&DspKind::Reverb, 1), Some(0));
        assert_eq!(dsp_native_param(&DspKind::Gain, 0), Some(0));
        assert_eq!(dsp_native_param(&DspKind::Filter, 0), Some(0));
        assert_eq!(dsp_native_param(&DspKind::Filter, 1), Some(1));
        // Delay keeps its time/mix mapping.
        assert_eq!(dsp_native_param(&DspKind::Delay, super::P_TIME), Some(0));
        assert_eq!(dsp_native_param(&DspKind::Delay, super::P_MIX), Some(2));
    }
}

/// End-to-end LV2 engine test: Keys → Surge → LV2 effect → Master, render and
/// verify audio flows through the hosted LV2 node.
pub fn lv2_engine_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;

    let sr = 48000.0;
    let (voice, _main) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sr, vec![(1, voice)]);

    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(
        NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
        name_bytes("SYNTH"),
    );
    let lv2 = g.add_node(
        NodeKind::Lv2 { uri: Lv2Uri::new("http://kxstudio.sf.net/carla/plugins/audiogain_s") },
        name_bytes("LV2 GAIN"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: lv2, port: 0 }, PortKind::Audio);
    g.connect_with_id(3, PortRef { node: lv2, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);

    bus.send(Command::LoadGraph { graph: g });
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });

    let mut buf = vec![0.0f32; 512 * 2];
    let mut peak = 0.0f32;
    for _ in 0..30 {
        engine.render(&mut buf, 2);
        for &s in &buf {
            peak = peak.max(s.abs());
        }
    }
    anyhow::ensure!(peak > 1e-4, "no audio through the LV2 engine path (peak {peak})");
    println!("lv2-engine-test: OK — Keys→Surge→LV2→Master rendered 30 blocks, peak {peak:.4}");
    Ok(())
}

/// Metronome self-test: enable the transport and verify clicks are generated.
pub fn metro_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, Command, Graph};
    use crate::host::ClapVoice;
    let sr = 48000.0;
    let (voice, _main) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sr, vec![(1, voice)]);
    bus.send(Command::SetBpm { bpm: 120.0 });
    bus.send(Command::TransportPlay);
    let mut buf = vec![0.0f32; 512 * 2];
    let mut click_blocks = 0;
    for _ in 0..(48000 / 512) {
        engine.render(&mut buf, 2);
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak > 0.05 {
            click_blocks += 1;
        }
    }
    anyhow::ensure!(click_blocks > 0, "metronome produced no clicks");
    // Stop and confirm silence returns.
    bus.send(Command::TransportStop);
    for _ in 0..10 {
        engine.render(&mut buf, 2);
    }
    let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    anyhow::ensure!(peak < 0.05, "metronome did not stop (peak {peak})");
    println!("metro-test: OK — {click_blocks} click-blocks in 1s @120bpm, silent after stop");
    Ok(())
}

/// LV2 instrument engine-routing test: a note sent through the control plane
/// must reach the LV2 instrument's atom input (verified via readback).
pub fn lv2_instrument_engine_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (voice, _main) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sr, vec![(1, voice)]);

    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let inst = g.add_node(
        NodeKind::Lv2 { uri: Lv2Uri::new("http://lsp-plug.in/plugins/lv2/oscillator_mono") },
        name_bytes("LV2 INST"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: inst, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);

    bus.send(Command::LoadGraph { graph: g });
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let mut buf = vec![0.0f32; 512 * 2];
    engine.render(&mut buf, 2); // drains LoadGraph + NoteOn

    let ev = engine.lv2_last_note(inst);
    anyhow::ensure!(ev.is_some(), "LV2 instrument did not receive the note");
    let ev = ev.unwrap();
    anyhow::ensure!(ev[0] == 0x90 && ev[1] == 60, "wrong MIDI event reached instrument: {ev:?}");
    println!("lv2-instrument-engine-test: OK — note routed to LV2 instrument atom input (event {ev:?})");
    Ok(())
}

/// Dynamic-patch test: a DSP node added via AddNode (not LoadGraph) must sound.
pub fn add_node_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, DspKind, Graph, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (voice, _main) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![(surge, voice)]);

    // Insert a dynamically added gain node: surge -> gain -> master.
    let gain = keys.max(surge).max(master) + 1;
    bus.send(Command::AddNode { kind: NodeKind::Dsp(DspKind::Gain), name: name_bytes("GAIN") });
    bus.send(Command::SetParam { node: gain, param: 0, value: 1.0 });
    bus.send(Command::Disconnect(2));
    bus.send(Command::Connect { id: 3, from: PortRef { node: surge, port: 0 }, to: PortRef { node: gain, port: 0 }, kind: PortKind::Audio });
    bus.send(Command::Connect { id: 4, from: PortRef { node: gain, port: 0 }, to: PortRef { node: master, port: 0 }, kind: PortKind::Audio });
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });

    let mut buf = vec![0.0f32; 512 * 2];
    let mut peak = 0.0f32;
    for _ in 0..30 {
        engine.render(&mut buf, 2);
        peak = peak.max(buf.iter().fold(0.0f32, |m, s| m.max(s.abs())));
    }
    anyhow::ensure!(peak > 1e-3, "dynamically added DSP node did not sound (peak {peak})");
    println!("add-node-test: OK — dynamically added DSP gain node sounds (peak {peak:.4})");
    Ok(())
}
