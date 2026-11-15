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
use std::sync::{Arc, Mutex};

use crate::control::{
    Command, CommandDrain, DspKind, Graph, NodeId, NodeKind, ParamId, PortKind, RigSnapshot,
};
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
        // Metronome has no native params: tempo comes from transport SetBpm.
        DspKind::Metronome => None,
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

/// Per-rig metadata retained from the last `Command::LoadRigs` — the rig
/// list frontends (UI/bridge) can query. Empty after a plain `LoadGraph`.
/// Defined in the control plane so snapshots can be shared without engine
/// types; re-exported here for compatibility.
pub use crate::control::RigMeta;

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
    // Multi-rig state (LoadRigs): rig names/devices + the UI-selected rig.
    rig_meta: Vec<RigMeta>,
    active_rig: usize,
    // Shared frontend snapshot (UI/MCP/bridge). Written ONLY on the rare
    // LoadRigs/LoadGraph structural ops at block boundaries — the per-block
    // render path never locks it. None until a frontend attaches a handle.
    rig_snapshot: Option<Arc<Mutex<RigSnapshot>>>,
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
            rig_meta: Vec::new(),
            active_rig: 0,
            rig_snapshot: None,
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

    /// Rig list from the last `LoadRigs` (empty for single-graph loads).
    #[allow(dead_code)] // public query API for the UI (task-22 integration)
    pub fn rig_meta(&self) -> &[RigMeta] {
        &self.rig_meta
    }

    /// The UI-selected rig index from the last `LoadRigs` (0 otherwise).
    #[allow(dead_code)] // public query API for the UI (task-22 integration)
    pub fn active_rig(&self) -> usize {
        self.active_rig
    }

    /// Attach the shared rig-state snapshot frontends read (UI/MCP/bridge).
    /// The engine publishes into it on `LoadRigs`/`LoadGraph` only — rare
    /// block-boundary structural ops; the steady render path never locks it.
    /// Call before the engine moves to the audio thread (main thread setup).
    pub fn set_rig_snapshot(&mut self, snapshot: Arc<Mutex<RigSnapshot>>) {
        self.rig_snapshot = Some(snapshot);
    }

    /// Publish the current rig state into the shared snapshot (structural ops
    /// only — never called from the per-block sample path). A poisoned lock
    /// is skipped, not fatal: the snapshot is advisory frontend state.
    fn publish_rig_snapshot(&self) {
        if let Some(snap) = &self.rig_snapshot {
            if let Ok(mut s) = snap.lock() {
                s.rigs = self.rig_meta.clone();
                s.active = self.active_rig;
            }
        }
    }

    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.out_peak.clone()
    }

    /// Read back the MIDI event pending at an LV2 instrument node (verification).
    pub fn lv2_last_note(&self, node: NodeId) -> Option<[u8; 3]> {
        self.lv2.iter().find(|s| s.node == node).and_then(|s| s.plugin.last_midi_event())
    }

    /// MIDI-cable gate: a note reaches `dest` if ANY Keys node has a MIDI
    /// cable to it. Each rig carries its own Keys node after LoadRigs; a
    /// single-graph patch has exactly one, so behavior is unchanged there.
    fn keys_gate(&self, dest: NodeId) -> bool {
        if let Some(k) = self.keys_node {
            if self.graph.has_route(k, dest, PortKind::Midi) {
                return true;
            }
        }
        self.graph.nodes.iter().any(|n| {
            matches!(n.kind, NodeKind::Keys)
                && Some(n.id) != self.keys_node
                && self.graph.has_route(n.id, dest, PortKind::Midi)
        })
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
                self.rig_meta.clear();
                self.active_rig = 0;
                self.publish_rig_snapshot();
            }
            Command::LoadRigs { rigs, active } => {
                // Rare structural op: merge the rig graphs into one patch and
                // reuse the LoadGraph rebuild (DSP, LV2, CLAP voice remap in
                // node-id order, matrix, keys_node), then bind each rig's
                // instruments to its MIDI device. Allocates — off the hot path.
                let merged = crate::control::merge_rigs(&rigs);
                self.apply(Command::LoadGraph { graph: merged });
                for (i, rig) in rigs.iter().enumerate() {
                    let tag = i.min(u8::MAX as usize) as u8;
                    if let Some(dev) = rig.midi_device {
                        // Some(dev): ONLY that hardware device feeds this rig.
                        // Drop the rig's default rules, then wire exactly one
                        // Hardware(dev) rule per instrument (Clap + LV2 inst) —
                        // works for device ids beyond the pre-routed range too.
                        // None rigs keep the full default routing from LoadGraph.
                        self.matrix.rules.retain(|r| {
                            !self.graph.node(r.dest).map(|n| n.rig == tag).unwrap_or(false)
                        });
                        let mut insts: Vec<NodeId> = self
                            .graph
                            .nodes
                            .iter()
                            .filter(|n| n.rig == tag && matches!(n.kind, NodeKind::Clap { .. }))
                            .map(|n| n.id)
                            .collect();
                        for s in &self.lv2 {
                            if s.plugin.is_instrument()
                                && self.graph.node(s.node).map(|n| n.rig == tag).unwrap_or(false)
                            {
                                insts.push(s.node);
                            }
                        }
                        for id in insts {
                            self.matrix.connect(SourceId::Hardware(dev), id);
                        }
                    }
                    // Keep the voice-level source filter consistent with the
                    // matrix (a second gate on top of the cable + matrix).
                    for s in self.voices.iter_mut() {
                        if self.graph.node(s.node).map(|n| n.rig == tag).unwrap_or(false) {
                            s.src_keys = rig.midi_device.is_none();
                            s.src_laptop = rig.midi_device.is_none();
                            s.src_hw = true;
                            s.rig_device = rig.midi_device;
                        }
                    }
                }
                self.rig_meta = rigs
                    .iter()
                    .map(|r| RigMeta { name: r.name.clone(), midi_device: r.midi_device })
                    .collect();
                self.active_rig = active.min(rigs.len().saturating_sub(1));
                self.publish_rig_snapshot();
            }
            Command::NoteOn { note, velocity, source } => {
                let msg = MidiMsg::note_on(0, note, (velocity.clamp(0.0, 1.0) * 127.0) as u8);
                self.matrix.route_into(source, msg, &mut self.route_out);
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let gate = self.keys_gate(dest);
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
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let gate = self.keys_gate(dest);
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
            if d.kind == DspKind::Metronome {
                continue; // generated below from transport state, no inputs
            }
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

        // 2a. Metronome nodes: transport-driven click generators. Their output
        // reaches the master via normal audio routing like any DSP node.
        for i in 0..self.dsp.len() {
            if self.dsp[i].kind == DspKind::Metronome {
                // Take the buffer out so the helper can borrow engine state;
                // mem::take on a Vec is allocation-free (RT-safe).
                let mut buf = std::mem::take(&mut self.dsp[i].buf);
                self.render_metronome(&mut buf[..n2], frames);
                self.dsp[i].buf = buf;
            }
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
        // Multi-rig: sum every rig's MasterOut to the physical output. Fixed
        // stack array — no allocation on the audio thread (max 8 rigs).
        let mut masters = [0u32; 8];
        let mut n_masters = 0usize;
        for n in &self.graph.nodes {
            if matches!(n.kind, NodeKind::MasterOut) && n_masters < masters.len() {
                masters[n_masters] = n.id;
                n_masters += 1;
            }
        }
        if n_masters > 0 {
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for mi in 0..n_masters {
                    let master = masters[mi];
                    for v in &self.voices {
                        if self.graph.has_route(v.node, master, PortKind::Audio) {
                            if self.graph.has_route_port(v.node, master, 2, PortKind::Audio) {
                                let m = v.scratch[i * 2]; // mono in: center to both
                                l += m;
                                r += m;
                            } else {
                                l += v.scratch[i * 2];
                                r += v.scratch[i * 2 + 1];
                            }
                        }
                    }
                    for d in &self.dsp {
                        if self.graph.has_route(d.node, master, PortKind::Audio) {
                            if self.graph.has_route_port(d.node, master, 2, PortKind::Audio) {
                                let m = d.buf[i * 2];
                                l += m;
                                r += m;
                            } else {
                                l += d.buf[i * 2];
                                r += d.buf[i * 2 + 1];
                            }
                        }
                    }
                    for slot in &self.lv2 {
                        if self.graph.has_route(slot.node, master, PortKind::Audio) {
                            if self.graph.has_route_port(slot.node, master, 2, PortKind::Audio) {
                                let m = slot.buf[i * 2];
                                l += m;
                                r += m;
                            } else {
                                l += slot.buf[i * 2];
                                r += slot.buf[i * 2 + 1];
                            }
                        }
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

    /// Generate the metronome click into `buf` (stereo-interleaved): a
    /// decaying 1 kHz burst on each beat while the transport plays. Driven by
    /// TransportPlay/TransportStop/SetBpm; allocation-free.
    fn render_metronome(&mut self, buf: &mut [f32], frames: usize) {
        let frames = frames.min(buf.len() / 2);
        let n2 = frames * 2;
        for x in buf[..n2].iter_mut() {
            *x = 0.0;
        }
        for i in 0..frames {
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
                buf[i * 2] = click;
                buf[i * 2 + 1] = click;
                self.click_phase += 1.0;
                self.click_remaining -= 1;
            }
        }
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

    // --- Multi-rig load (task-22) -------------------------------------------
    // CLAP voices are main-thread-only, so these run the REAL engine without
    // audio (empty voice pool): merge, rebuild, and per-rig device routing.
    // The with-audio proof is the `multi-rig-load-test` CLI mode below.

    use crate::control::{command_bus, name_bytes, Command, NodeKind, PortKind, PortRef, Rig};

    /// One rig: Keys(0) → Clap(1) → Master(2), cables 1 (MIDI) and 2 (audio).
    fn rig_graph() -> crate::control::Graph {
        let mut g = crate::control::Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(
            NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
            name_bytes("SURGE"),
        );
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        assert!(g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi));
        assert!(g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio));
        g
    }

    /// Send LoadRigs through the bus and drain it at a block boundary.
    fn load_two_rigs(
        dev_a: Option<u32>,
        dev_b: Option<u32>,
    ) -> (super::GraphEngine, crate::control::CommandBus) {
        let (mut bus, drain) = command_bus();
        let mut engine =
            super::GraphEngine::new(crate::control::Graph::default(), drain, 48000.0, vec![]);
        bus.send(Command::LoadRigs {
            rigs: vec![Rig::new("A", rig_graph(), dev_a), Rig::new("B", rig_graph(), dev_b)],
            active: 1,
        });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        (engine, bus)
    }

    #[test]
    fn load_rigs_merges_masters_and_routes_per_device() {
        let (engine, _bus) = load_two_rigs(Some(2), None);
        let g = engine.graph();
        // Both masters present, one per rig, tagged with the rig index.
        let masters: Vec<u32> = g.nodes.iter()
            .filter(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).collect();
        assert_eq!(masters, vec![2, 5]);
        assert_eq!(g.node(2).unwrap().rig, 0);
        assert_eq!(g.node(5).unwrap().rig, 1);
        // Per-rig device routing: rig A = only Hardware(2); rig B = everything.
        let on = MidiMsg::note_on(0, 60, 100);
        // Device 2 plays rig A (bound) AND rig B (None accepts every device).
        let mut d = dests(&engine.matrix, SourceId::Hardware(2), on);
        d.sort_unstable();
        assert_eq!(d, vec![1, 4]);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(3), on), vec![4]);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(0), on), vec![4]);
        assert_eq!(dests(&engine.matrix, SourceId::Keys, on), vec![4]);
        assert_eq!(dests(&engine.matrix, SourceId::LaptopKeys, on), vec![4]);
        // Each rig's OWN Keys node gates its synth (multi-rig cable gate).
        assert!(engine.keys_gate(1));
        assert!(engine.keys_gate(4));
        assert_eq!(engine.keys_node, Some(0));
        // Rig list + active index are queryable.
        assert_eq!(engine.rig_meta().len(), 2);
        assert_eq!(engine.rig_meta()[0].name, "A");
        assert_eq!(engine.rig_meta()[0].midi_device, Some(2));
        assert_eq!(engine.rig_meta()[1].name, "B");
        assert_eq!(engine.rig_meta()[1].midi_device, None);
        assert_eq!(engine.active_rig(), 1);
    }

    #[test]
    fn load_rigs_both_bound_devices_are_isolated() {
        let (engine, _bus) = load_two_rigs(Some(2), Some(7));
        let on = MidiMsg::note_on(0, 60, 100);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(2), on), vec![1]);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(7), on), vec![4]);
        assert!(dests(&engine.matrix, SourceId::Hardware(3), on).is_empty());
        assert!(dests(&engine.matrix, SourceId::Keys, on).is_empty());
        assert!(dests(&engine.matrix, SourceId::LaptopKeys, on).is_empty());
    }

    #[test]
    fn load_rigs_device_beyond_prerouted_range() {
        // Device ids >= MAX_HW_DEVICES are wired explicitly, not pre-routed.
        let (engine, _bus) = load_two_rigs(Some(20), None);
        let on = MidiMsg::note_on(0, 60, 100);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(20), on), vec![1]);
        assert_eq!(dests(&engine.matrix, SourceId::Hardware(2), on), vec![4]);
    }

    #[test]
    fn load_graph_after_rigs_resets_meta() {
        let (mut engine, mut bus) = load_two_rigs(Some(2), None);
        bus.send(Command::LoadGraph { graph: crate::control::Graph::demo_patch() });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        assert!(engine.rig_meta().is_empty());
        assert_eq!(engine.active_rig(), 0);
    }

    #[test]
    fn rig_snapshot_published_on_load_and_cleared_on_graph() {
        use std::sync::{Arc, Mutex};
        let (mut engine, mut bus) = load_two_rigs(Some(2), None);
        let snap = Arc::new(Mutex::new(crate::control::RigSnapshot::default()));
        engine.set_rig_snapshot(snap.clone());
        bus.send(Command::LoadRigs {
            rigs: vec![Rig::new("A", rig_graph(), Some(2)), Rig::new("B", rig_graph(), None)],
            active: 1,
        });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2); // drains LoadRigs → publishes
        {
            let s = snap.lock().unwrap();
            assert_eq!(s.rigs.len(), 2);
            assert_eq!(s.rigs[0].name, "A");
            assert_eq!(s.rigs[0].midi_device, Some(2));
            assert_eq!(s.rigs[1].midi_device, None);
            assert_eq!(s.active, 1);
        }
        // Single-graph load clears the shared snapshot back to empty.
        bus.send(Command::LoadGraph { graph: crate::control::Graph::demo_patch() });
        engine.render(&mut buf, 2);
        let s = snap.lock().unwrap();
        assert!(s.rigs.is_empty());
        assert_eq!(s.active, 0);
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
        // Metronome has no native params (tempo is transport SetBpm).
        assert_eq!(dsp_native_param(&DspKind::Metronome, 0), None);
        assert_eq!(dsp_native_param(&DspKind::Metronome, super::P_TIME), None);
        assert_eq!(dsp_native_param(&DspKind::Metronome, super::P_MIX), None);
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

/// Metronome self-test: a Metronome rack node wired Metronome→Master must
/// click on TransportPlay and fall silent on TransportStop. Also verifies a
/// patch WITHOUT a metronome node stays silent while playing (the click is no
/// longer hardcoded into the master bus).
pub fn metro_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, DspKind, Graph, NodeKind, PortKind, PortRef};
    let sr = 48000.0;
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(8, 8);
    let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: metro, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![]);
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
    anyhow::ensure!(click_blocks > 0, "metronome node produced no clicks");
    // Stop and confirm silence returns.
    bus.send(Command::TransportStop);
    for _ in 0..10 {
        engine.render(&mut buf, 2);
    }
    let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    anyhow::ensure!(peak < 0.05, "metronome did not stop (peak {peak})");
    // No metronome node in the patch → playing transport must stay silent.
    bus.send(Command::LoadGraph { graph: Graph::demo_patch() });
    bus.send(Command::TransportPlay);
    let mut peak = 0.0f32;
    for _ in 0..(48000 / 512) {
        engine.render(&mut buf, 2);
        peak = peak.max(buf.iter().fold(0.0f32, |m, s| m.max(s.abs())));
    }
    anyhow::ensure!(peak < 0.05, "click leaked into master without a metronome node (peak {peak})");
    println!("metro-test: OK — {click_blocks} click-blocks in 1s @120bpm via Metronome→Master, silent after stop and without node");
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

/// Mono-master test: a source wired to the master's MONO input (port 2) must be
/// centered (L == R) and audible.
pub fn mono_master_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
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
    g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 2 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![(surge, voice)]);
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let mut buf = vec![0.0f32; 512 * 2];
    let mut peak = 0.0f32;
    let mut centered = true;
    for _ in 0..30 {
        engine.render(&mut buf, 2);
        for frame in buf.chunks(2) {
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
            if (frame[0] - frame[1]).abs() > 1e-5 {
                centered = false;
            }
        }
    }
    anyhow::ensure!(peak > 1e-3, "mono master produced no output (peak {peak})");
    anyhow::ensure!(centered, "mono master not centered (L != R)");
    println!("mono-master-test: OK — mono input centered to L=R (peak {peak:.4})");
    Ok(())
}

/// Multi-rig test: two independent rigs (each Keys→Surge→its own Master) must
/// both sound and sum to the physical output; muting one rig reduces the level.
pub fn multi_rig_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (va, _ma) = ClapVoice::new(sr)?;
    let (vb, _mb) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(32, 32);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let sa = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE A"));
    let ma = g.add_node(NodeKind::MasterOut, name_bytes("MASTER A"));
    let sb = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE B"));
    let mb = g.add_node(NodeKind::MasterOut, name_bytes("MASTER B"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: sa, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: sa, port: 0 }, PortRef { node: ma, port: 0 }, PortKind::Audio);
    g.connect_with_id(3, PortRef { node: keys, port: 0 }, PortRef { node: sb, port: 0 }, PortKind::Midi);
    g.connect_with_id(4, PortRef { node: sb, port: 0 }, PortRef { node: mb, port: 0 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![(sa, va), (sb, vb)]);
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });

    let mut buf = vec![0.0f32; 512 * 2];
    let measure = |e: &mut GraphEngine, buf: &mut [f32]| -> f32 {
        let mut peak = 0.0f32;
        for _ in 0..30 {
            e.render(buf, 2);
            peak = peak.max(buf.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        peak
    };
    let both = measure(&mut engine, &mut buf);
    anyhow::ensure!(both > 1e-3, "two rigs produced no output (peak {both})");
    // Mute rig B by unplugging its master cable; level must drop.
    bus.send(Command::Disconnect(4));
    let one = measure(&mut engine, &mut buf);
    anyhow::ensure!(both > one * 1.2, "rigs not summing (both {both} vs one {one})");
    println!("multi-rig-test: OK — two rigs sum to output (both {both:.4} > one {one:.4})");
    Ok(())
}

/// Multi-rig LOAD test (task-22): two independent rigs loaded via
/// `Command::LoadRigs`, each Keys→Surge→its OWN Master. Rig A is bound to
/// hardware device 2, rig B accepts any source. Keys must play only rig B;
/// device 2 adds rig A; both masters sum to the physical output.
/// CLI mode (CLAP voices are main-thread-only): `kabelsalat multi-rig-load-test`.
pub fn multi_rig_load_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef, Rig};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (va, _ma) = ClapVoice::new(sr)?;
    let (vb, _mb) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sr, vec![(1, va), (4, vb)]);

    let rig_graph = || -> Graph {
        let mut g = Graph::default();
        g.reserve(8, 8);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(
            NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
            name_bytes("SURGE"),
        );
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g
    };
    bus.send(Command::LoadRigs {
        rigs: vec![Rig::new("A", rig_graph(), Some(2)), Rig::new("B", rig_graph(), None)],
        active: 0,
    });

    let mut buf = vec![0.0f32; 512 * 2];
    engine.render(&mut buf, 2); // drain LoadRigs before inspecting the graph
    let measure = |e: &mut GraphEngine, buf: &mut [f32]| -> f32 {
        let mut peak = 0.0f32;
        for _ in 0..30 {
            e.render(buf, 2);
            peak = peak.max(buf.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        peak
    };

    // Merged layout: rig A nodes 0,1,2 (cables 1,2); rig B nodes 3,4,5
    // (cables 4,5). Rig B's master cable is edge 5 (synth 4 → master 5).
    anyhow::ensure!(
        engine.graph().nodes.iter().filter(|n| matches!(n.kind, NodeKind::MasterOut)).count() == 2,
        "merged graph must keep both rig masters"
    );

    // 1. Keys play ONLY rig B (rig A is bound to device 2).
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let b_only = measure(&mut engine, &mut buf);
    anyhow::ensure!(b_only > 1e-3, "rig B produced no output from keys (peak {b_only})");

    // 2. Unplug rig B's master: rig A must stay silent on keys (device-bound).
    bus.send(Command::Disconnect(5));
    let a_on_keys = measure(&mut engine, &mut buf);
    anyhow::ensure!(
        a_on_keys < b_only * 0.1,
        "device-bound rig A leaked keys audio (peak {a_on_keys} vs {b_only})"
    );

    // 3. Replug rig B, add device 2 → rig A joins; both masters must sum.
    bus.send(Command::Connect {
        id: 5,
        from: PortRef { node: 4, port: 0 },
        to: PortRef { node: 5, port: 0 },
        kind: PortKind::Audio,
    });
    bus.send(Command::NoteOn { note: 64, velocity: 0.9, source: SourceId::Hardware(2) });
    let both = measure(&mut engine, &mut buf);
    anyhow::ensure!(both > b_only * 1.2, "rigs not summing (both {both} vs b_only {b_only})");
    println!(
        "multi-rig-load-test: OK — LoadRigs: keys→rig B only ({b_only:.4}), device 2 adds rig A (both {both:.4})"
    );
    Ok(())
}
