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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::control::{
    name_str, split_port, Command, CommandDrain, DspKind, Graph, NodeId, NodeKind, ParamId,
    PortKind, RigSnapshot,
};
use crate::dsp::{build_processor, Processor};
use crate::host::ClapVoice;
use crate::matrix::{Matrix, MidiMsg, Rule, SourceId};
use crate::capture::SystemCapture;
use crate::player::PlayerSink;
use crate::record::RecorderTap;
use crate::transport::{CountInConfig, MasterClock, Transport};

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

/// Exposed engine param ids and labels for a built-in DSP node.
pub fn dsp_param_choices(kind: &DspKind) -> &'static [(ParamId, &'static str)] {
    match kind {
        DspKind::Delay => &[(P_TIME, "Time"), (P_MIX, "Mix")],
        DspKind::Reverb => &[(0, "Mix"), (1, "Size")],
        DspKind::Gain => &[(0, "Gain")],
        DspKind::Filter => &[(0, "Cutoff"), (1, "Resonance")],
        DspKind::Eq3 => &[(0, "Low"), (1, "High")],
        DspKind::Limiter => &[(0, "Threshold"), (1, "Ceiling")],
        DspKind::Saturation => &[(0, "Drive"), (1, "Mix")],
        DspKind::Pan => &[(0, "Pan")],
        DspKind::Metronome => &[],
    }
}

struct VoiceSlot {
    node: NodeId,
    voice: ClapVoice,
    level: f32,
    scratch: Vec<f32>,
    // Per-rig input selection (AUM): which source classes feed this rig —
    // the computer keyboard (on-screen + laptop) and hardware MIDI.
    src_keys: bool,
    src_hw: bool,
    // Which hardware device feeds this rig: None = any, Some(id) = only that
    // device (per-device routing, AUM's "different keyboard per rig").
    rig_device: Option<u32>,
}

/// Does this rig accept notes from the given source? (The MIDI cable gate is
/// checked separately by the caller.)
fn source_accepted(s: &VoiceSlot, source: SourceId) -> bool {
    source_allowed(s.src_keys, s.src_hw, s.rig_device, source)
}

/// The per-rig source filter as a pure function: `keyboard` gates the one
/// computer-keyboard source (on-screen clicks and laptop typing alike),
/// `hardware` + `rig_device` gate hardware MIDI. Other sources always pass.
fn source_allowed(keyboard: bool, hardware: bool, rig_device: Option<u32>, source: SourceId) -> bool {
    match source {
        SourceId::Keys => keyboard,
        SourceId::Hardware(id) => hardware && hw_device_allowed(rig_device, id),
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
    /// Per-instrument transport (metronome; the file-player node joins via
    /// `NodeKind::supports_transport`). Engine-owned plain state, set only by
    /// commands drained at block boundaries — the render path reads it with
    /// no lock and no atomics.
    transport: SlotTransport,
}

/// One transport-capable node's own clock: whether it runs, its phase within
/// the beat (0..1), and the click envelope in flight. Per slot, so two
/// metronomes never share (and double-advance) one phase.
#[derive(Clone, Copy, Default)]
struct SlotTransport {
    playing: bool,
    phase: f64,
    click_remaining: usize,
    click_phase: f32,
}

impl SlotTransport {
    /// Start/stop at a block boundary. Start lands on the downbeat (the click
    /// fires on the first rendered frame); stop cuts any click in flight.
    fn set_playing(&mut self, playing: bool) {
        if playing && !self.playing {
            self.phase = 0.0;
            self.click_remaining = CLICK_FRAMES;
            self.click_phase = 0.0;
        }
        if !playing {
            self.click_remaining = 0;
        }
        self.playing = playing;
    }
}

/// Click length in frames (decaying 1 kHz burst).
const CLICK_FRAMES: usize = 2400;

/// Build the processor slot for a Dsp node, carrying the node's persisted
/// playing flag into its transport (sessions/LoadGraph restore it).
fn dsp_slot(node: &crate::control::Node, kind: &DspKind, sample_rate: f64) -> DspSlot {
    let mut transport = SlotTransport::default();
    if node.kind.supports_transport() {
        transport.set_playing(node.playing);
    }
    DspSlot {
        node: node.id,
        kind: kind.clone(),
        proc: build_processor(kind, sample_rate),
        buf: vec![0.0; 8192 * 2],
        transport,
    }
}

struct Lv2Slot {
    node: NodeId,
    plugin: crate::lv2::Lv2Plugin,
    buf: Vec<f32>,
}

/// Work the audio thread hands to the LV2 loader thread: instantiating a
/// plugin (~200 ms for Vitalium), parsing its presets and freeing an
/// instance all block far longer than one audio buffer.
enum Lv2Request {
    Load { node: NodeId, uri: crate::control::Lv2Uri, gen: u64 },
    Preset { node: NodeId, uri: crate::control::Lv2Uri, preset_uri: String },
    Retire(Lv2Slot),
    RetirePreset(crate::lv2::Lv2Preset),
}

/// What the loader hands back; attached at the next block boundary.
enum Lv2Response {
    Loaded { node: NodeId, gen: u64, slot: Lv2Slot },
    Preset { node: NodeId, preset: crate::lv2::Lv2Preset },
}

const LV2_RING: usize = 64;
const LV2_SLOT_CAP: usize = 32;

fn load_lv2_slot(node: NodeId, uri: &str, sample_rate: f64) -> Option<Lv2Slot> {
    match crate::lv2::Lv2Plugin::load(uri, sample_rate, 8192) {
        Ok(plugin) => Some(Lv2Slot { node, plugin, buf: vec![0.0; 8192 * 2] }),
        Err(e) => {
            eprintln!("lv2 node {node}: {e}");
            None
        }
    }
}

/// The LV2 loader thread: serves requests until the engine drops its end.
fn lv2_loader_loop(mut req: rtrb::Consumer<Lv2Request>, mut resp: rtrb::Producer<Lv2Response>, sample_rate: f64) {
    loop {
        let Ok(r) = req.pop() else {
            if req.is_abandoned() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        };
        let out = match r {
            Lv2Request::Load { node, uri, gen } => {
                load_lv2_slot(node, uri.as_str(), sample_rate).map(|slot| Lv2Response::Loaded { node, gen, slot })
            }
            Lv2Request::Preset { node, uri, preset_uri } => crate::lv2::discover_presets(uri.as_str())
                .into_iter()
                .find(|p| p.uri == preset_uri)
                .map(|preset| Lv2Response::Preset { node, preset }),
            Lv2Request::Retire(slot) => {
                drop(slot);
                None
            }
            Lv2Request::RetirePreset(p) => {
                drop(p);
                None
            }
        };
        let mut pending = out;
        while let Some(msg) = pending.take() {
            match resp.push(msg) {
                Ok(()) => {}
                Err(rtrb::PushError::Full(m)) => {
                    if resp.is_abandoned() {
                        return;
                    }
                    pending = Some(m);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
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

/// Number of lock-free per-edge level slots (`EdgeLevels`).
pub const EDGE_LEVEL_SLOTS: usize = 256;

/// Per-edge audio levels, engine → UI (PLAN §2: cables breathe with live
/// level). Fixed-size lock-free slots indexed by `edge id % EDGE_LEVEL_SLOTS`;
/// each slot packs `(edge id << 32) | f32 peak bits` into one AtomicU64, so a
/// reader never attributes another edge's level to its cable — on a slot
/// collision the losing edge just reads silence. Written once per block by
/// the render path (allocation-free, lock-free); smoothing happens in the UI.
pub struct EdgeLevels {
    slots: [AtomicU64; EDGE_LEVEL_SLOTS],
}

impl EdgeLevels {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    /// Publish `edge`'s block peak (linear, ≥ 0).
    pub fn store(&self, edge: u32, peak: f32) {
        let packed = ((edge as u64) << 32) | peak.max(0.0).to_bits() as u64;
        self.slots[edge as usize % EDGE_LEVEL_SLOTS].store(packed, Ordering::Relaxed);
    }

    /// Last published peak for `edge`; 0.0 if never published (or evicted by
    /// a colliding edge id).
    pub fn load(&self, edge: u32) -> f32 {
        let v = self.slots[edge as usize % EDGE_LEVEL_SLOTS].load(Ordering::Relaxed);
        if (v >> 32) as u32 == edge { f32::from_bits(v as u32) } else { 0.0 }
    }
}

impl Default for EdgeLevels {
    fn default() -> Self {
        Self::new()
    }
}

fn block_peak(buf: &[f32]) -> f32 {
    buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Per-rig mixer state (AUM overview strip). Unity by default.
#[derive(Clone, Copy, Debug)]
pub struct RigMix {
    pub vol: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
}

impl RigMix {
    pub const DEFAULT: Self = Self { vol: 1.0, pan: 0.0, mute: false, solo: false };
}

impl Default for RigMix {
    fn default() -> Self { Self::DEFAULT }
}

/// Published engine transport + count-in state (copied under `try_lock`, zero allocation).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportPub {
    pub playing: bool,
    pub counting_in: bool,
    pub count_in_beat: u8,
    pub count_in_enabled: bool,
    pub count_in_bars: u8,
    pub bpm: f32,
}

impl Default for TransportPub {
    fn default() -> Self {
        Self {
            playing: false,
            counting_in: false,
            count_in_beat: 0,
            count_in_enabled: false,
            count_in_bars: 1,
            bpm: 96.0,
        }
    }
}

/// Shared graph publication for frontends (MCP→UI live sync, PLAN §13).
/// The engine writes it ONLY on structural ops that actually changed the
/// graph (loads, add/remove node, connect/disconnect) at block boundaries;
/// the steady render path never locks it. `rev` lets the UI skip unchanged
/// snapshots cheaply; duplicate Connects (the UI's per-frame mirror) do NOT
/// bump it, so UI-originated edits never trigger a rebuild feedback loop.
#[derive(Clone, Debug, Default)]
pub struct GraphPub {
    pub rev: u64,
    pub graph: Graph,
    /// Matrix rules at publication time — lets frontends mirror remote rule
    /// edits (MCP set_matrix_rule) as well as graph changes.
    pub rules: Vec<Rule>,
    /// Per-rig mixer state, bumped by `mix_rev` on every SetRigMix (a plain
    /// array copy — never a graph clone on the audio thread). Lets the UI
    /// mixer mirror remote (MCP/CLI/agent) mix changes.
    pub mix: [RigMix; 8],
    pub mix_rev: u64,
    /// Master transport, tempo, and count-in state, bumped by `transport_rev`.
    pub transport: TransportPub,
    pub transport_rev: u64,
}

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    sample_rate: f64,
    voices: Vec<VoiceSlot>,
    dsp: Vec<DspSlot>,
    lv2: Vec<Lv2Slot>,
    lv2_in: Vec<f32>,
    master_vol: f32,
    master_warm: f32,
    // Song clock handed to every CLAP voice. Its tempo is shared by every
    // transport-capable node (per-node run state lives in each DspSlot's
    // SlotTransport); its playing flag is the MASTER transport only.
    transport: Transport,
    master_clock: MasterClock,
    count_in: CountInConfig,
    count_in_slot: SlotTransport,
    count_in_buf: Vec<f32>,
    last_count_in_beat: u8,
    transport_dirty: bool,
    out_peak: Arc<AtomicU32>,
    // Per-edge audio levels (source node's block peak), published per block.
    edge_levels: Arc<EdgeLevels>,
    // MIDI routing matrix (source → instrument: channel/transpose/ranges).
    // Default rules mirror "every source plays every instrument"; keyboard
    // zones are NOT matrix rules — a split Keys node's LOW/HIGH ports gate
    // notes by graph wiring (keys_gate).
    matrix: Matrix,
    route_out: Vec<(NodeId, MidiMsg)>,
    /// Test-only delivery log: (dest, note, on) for every gated note event
    /// (incl. split-change releases), so routing is observable without voices.
    #[cfg(test)]
    note_log: Vec<(NodeId, u8, bool)>,
    #[cfg(test)]
    cc_log: Vec<(NodeId, ParamId, f64)>,
    #[cfg(test)]
    cc_passthrough_log: Vec<(NodeId, u8, u8)>,
    // Multi-rig state (LoadRigs): rig names/devices + the UI-selected rig.
    rig_meta: Vec<RigMeta>,
    active_rig: usize,
    // Per-rig mixer (SetRigMix): vol/pan/mute/solo applied at each rig's
    // MasterOut when summing. Fixed 8 slots — no allocation on the audio path.
    rig_mix: [RigMix; 8],
    /// A SetRigMix awaits publication to `graph_pub` (retried each block:
    /// the audio thread only ever `try_lock`s the publication).
    mix_dirty: bool,
    // Shared frontend snapshot (UI/MCP/bridge). Written ONLY on the rare
    // LoadRigs/LoadGraph structural ops at block boundaries — the per-block
    // render path never locks it. None until a frontend attaches a handle.
    rig_snapshot: Option<Arc<Mutex<RigSnapshot>>>,
    // Shared graph publication (MCP→UI live sync). Same discipline as
    // rig_snapshot: written on structural ops only, never on the render path.
    graph_pub: Option<Arc<Mutex<GraphPub>>>,
    // System-audio capture (e.g. YouTube) mixed into the master.
    system_capture: Option<SystemCapture>,
    system_gain: f32,
    system_buf: Vec<f32>,
    // File player (backing track) mixed into the master.
    player_sink: Option<PlayerSink>,
    player_gain: f32,
    player_buf: Vec<f32>,
    // Runtime CLAP voice delivery + retirement side channels (UI <-> audio).
    voice_rx: Option<rtrb::Consumer<(NodeId, ClapVoice, Vec<f32>)>>,
    /// LV2 loader thread link (`spawn_lv2_loader`); None loads inline
    /// (tests, offline self-tests).
    lv2_req: Option<rtrb::Producer<Lv2Request>>,
    lv2_resp: Option<rtrb::Consumer<Lv2Response>>,
    /// Bumped by every graph load: a plugin the loader finishes for an
    /// older graph is retired, not attached.
    lv2_gen: u64,
    voice_retire_tx: Option<rtrb::Producer<ClapVoice>>,
    // Master recording tap (the final output). None = recording unavailable
    // (headless tests, silent demo). The tap's ring producer is pushed from
    // the render path; its `recording` flag is the authority on start/stop.
    recorder: Option<RecorderTap>,
    // "Sync to transport": start recording on master Play, stop on Stop.
    record_sync: bool,
}

impl GraphEngine {
    /// `voices` pairs each Clap node id with a loaded voice (created on the
    /// main thread). A processor is built for every Dsp node in the graph.
    pub fn new(
        graph: Graph,
        drain: CommandDrain,
        sample_rate: f64,
        voices_in: Vec<(NodeId, ClapVoice)>,
    ) -> Self {
        let mut voices: Vec<VoiceSlot> = Vec::with_capacity(32);
        voices.extend(
            voices_in
                .into_iter()
                .map(|(node, voice)| VoiceSlot {
                    node,
                    voice,
                    level: 0.85,
                    scratch: vec![0.0; 8192 * 2],
                    src_keys: true,
                    src_hw: true,
                    rig_device: None,
                }),
        );
        let dsp = graph
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Dsp(kind) => Some(dsp_slot(n, kind, sample_rate)),
                _ => None,
            })
            .collect();
        let lv2 = build_lv2_slots(&graph, sample_rate);
        // Default routing: every source class feeds every rig (full range).
        let mut matrix = Matrix::new();
        matrix.reserve(64);
        for v in voices.iter().filter(|v| v.node != NodeId::MAX) {
            matrix.connect(SourceId::Keys, v.node);
            // Cover hardware device ids 0..MAX so any connected device routes;
            // the per-rig `rig_device` filter then selects which one is heard.
            for dev in 0..MAX_HW_DEVICES {
                matrix.connect(SourceId::Hardware(dev), v.node);
            }
        }
        for slot in &lv2 {
            if slot.plugin.is_instrument() {
                matrix.connect(SourceId::Keys, slot.node);
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
            master_vol: 0.8,
            master_warm: 0.15,
            transport: Transport::stopped(96.0),
            master_clock: MasterClock::Stopped,
            count_in: CountInConfig::default(),
            count_in_slot: SlotTransport::default(),
            count_in_buf: vec![0.0; 8192 * 2],
            last_count_in_beat: 0,
            transport_dirty: false,
            out_peak: Arc::new(AtomicU32::new(0)),
            edge_levels: Arc::new(EdgeLevels::new()),
            matrix,
            route_out: Vec::with_capacity(64),
            #[cfg(test)]
            note_log: Vec::new(),
            #[cfg(test)]
            cc_log: Vec::new(),
            #[cfg(test)]
            cc_passthrough_log: Vec::new(),
            rig_meta: Vec::new(),
            active_rig: 0,
            rig_mix: [RigMix::DEFAULT; 8],
            mix_dirty: false,
            rig_snapshot: None,
            graph_pub: None,
            system_capture: None,
            system_gain: 0.0,
            system_buf: vec![0.0; 8192 * 2],
            player_sink: None,
            player_gain: 0.0,
            player_buf: vec![0.0; 8192 * 2],
            voice_rx: None,
            lv2_req: None,
            lv2_resp: None,
            lv2_gen: 0,
            voice_retire_tx: None,
            recorder: None,
            record_sync: false,
        }
    }

    /// Instantiate LV2 plugins, parse their presets and free them on a
    /// loader thread instead of the audio thread. Call before the engine
    /// moves to the audio thread.
    pub fn spawn_lv2_loader(&mut self) {
        let (req_tx, req_rx) = rtrb::RingBuffer::new(LV2_RING);
        let (resp_tx, resp_rx) = rtrb::RingBuffer::new(LV2_RING);
        let sr = self.sample_rate;
        std::thread::Builder::new()
            .name("kabelsalat-lv2-loader".into())
            .spawn(move || lv2_loader_loop(req_rx, resp_tx, sr))
            .expect("spawn lv2 loader");
        self.lv2_req = Some(req_tx);
        self.lv2_resp = Some(resp_rx);
        self.lv2.reserve(LV2_SLOT_CAP);
    }

    /// Load `uri` for `node`: on the loader thread when there is one.
    fn request_lv2(&mut self, node: NodeId, uri: crate::control::Lv2Uri) {
        if let Some(tx) = self.lv2_req.as_mut() {
            if tx.push(Lv2Request::Load { node, uri, gen: self.lv2_gen }).is_err() {
                eprintln!("lv2 loader queue full: node {node} not loaded");
            }
            return;
        }
        if let Some(slot) = load_lv2_slot(node, uri.as_str(), self.sample_rate) {
            self.attach_lv2(slot);
        }
    }

    /// Put a loaded LV2 slot into the render list; an instrument gets the
    /// default MIDI routes (only its rig's device when the rig has one).
    fn attach_lv2(&mut self, slot: Lv2Slot) {
        let node = slot.node;
        let inst = slot.plugin.is_instrument();
        self.lv2.push(slot);
        if inst {
            let device = self.graph.node(node)
                .and_then(|n| self.rig_meta.get(n.rig as usize))
                .and_then(|m| m.midi_device);
            match device {
                Some(dev) => {
                    self.matrix.connect(SourceId::Hardware(dev), node);
                }
                None => {
                    self.matrix.connect(SourceId::Keys, node);
                    for dev in 0..MAX_HW_DEVICES {
                        self.matrix.connect(SourceId::Hardware(dev), node);
                    }
                }
            }
        }
    }

    /// Free an LV2 slot off the audio thread when the loader runs.
    fn retire_lv2(&mut self, slot: Lv2Slot) {
        if let Some(tx) = self.lv2_req.as_mut() {
            if let Err(rtrb::PushError::Full(Lv2Request::Retire(slot))) = tx.push(Lv2Request::Retire(slot)) {
                drop(slot); // queue full: free here rather than leak
            }
        }
    }

    /// Adopt what the loader finished (block boundary, allocation-free).
    fn drain_lv2_loader(&mut self) {
        loop {
            let msg = match self.lv2_resp.as_mut() {
                Some(rx) => match rx.pop() {
                    Ok(m) => m,
                    Err(_) => return,
                },
                None => return,
            };
            match msg {
                Lv2Response::Loaded { node, gen, slot } => {
                    let wanted = gen == self.lv2_gen
                        && matches!(self.graph.node(node).map(|n| &n.kind), Some(NodeKind::Lv2 { .. }))
                        && !self.lv2.iter().any(|s| s.node == node)
                        && self.lv2.len() < self.lv2.capacity();
                    if wanted {
                        self.attach_lv2(slot);
                        self.publish_graph();
                    } else {
                        self.retire_lv2(slot);
                    }
                }
                Lv2Response::Preset { node, preset } => {
                    if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                        s.plugin.apply_preset(&preset);
                    }
                    if let Some(tx) = self.lv2_req.as_mut() {
                        let _ = tx.push(Lv2Request::RetirePreset(preset));
                    }
                }
            }
        }
    }

    /// Attach runtime CLAP voice delivery + retirement ring buffers.
    pub fn set_voice_channels(
        &mut self,
        rx: rtrb::Consumer<(NodeId, ClapVoice, Vec<f32>)>,
        retire_tx: rtrb::Producer<ClapVoice>,
    ) {
        self.voice_rx = Some(rx);
        self.voice_retire_tx = Some(retire_tx);
    }

    /// Attach a system-audio capture source (started off the audio thread).
    pub fn set_system_capture(&mut self, cap: SystemCapture) {
        self.system_capture = Some(cap);
    }

    /// Attach the file-player ring consumer (decode thread started elsewhere).
    pub fn set_player_sink(&mut self, sink: PlayerSink) {
        self.player_sink = Some(sink);
    }

    /// Attach the master recording tap (writer thread started elsewhere).
    pub fn set_recorder(&mut self, tap: RecorderTap) {
        self.recorder = Some(tap);
    }

    /// Start recording the master output (idempotent). No-op without a tap.
    fn record_start(&mut self) {
        if let Some(rec) = &self.recorder {
            rec.begin();
        }
    }

    /// Stop recording (idempotent). No-op without a tap.
    fn record_stop(&mut self) {
        if let Some(rec) = &self.recorder {
            rec.stop();
        }
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Song clock as of the next block's first frame.
    pub fn transport(&self) -> Transport {
        self.transport
    }

    #[cfg(test)]
    pub fn master_clock(&self) -> MasterClock {
        self.master_clock
    }

    #[cfg(test)]
    pub fn count_in(&self) -> CountInConfig {
        self.count_in
    }

    pub fn transport_pub(&self) -> TransportPub {
        TransportPub {
            playing: self.transport.playing,
            counting_in: self.master_clock.is_counting_in(),
            count_in_beat: self.master_clock.count_in_beat(),
            count_in_enabled: self.count_in.enabled,
            count_in_bars: self.count_in.bars,
            bpm: self.transport.bpm as f32,
        }
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

    /// Attach the shared graph publication (main-thread setup, before the
    /// engine moves to the audio thread).
    pub fn set_graph_pub(&mut self, p: Arc<Mutex<GraphPub>>) {
        self.graph_pub = Some(p);
    }

    /// Publish the current graph (structural ops only). A poisoned lock is
    /// skipped, not fatal — the publication is advisory frontend state.
    fn publish_graph(&mut self) {
        let tp = self.transport_pub();
        if let Some(p) = &self.graph_pub {
            if let Ok(mut g) = p.lock() {
                g.rev = g.rev.wrapping_add(1);
                g.graph = self.graph.clone();
                g.rules = self.matrix.rules.clone();
                g.mix = self.rig_mix;
                g.mix_rev = g.mix_rev.wrapping_add(1);
                g.transport = tp;
                g.transport_rev = g.transport_rev.wrapping_add(1);
            }
        }
    }

    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.out_peak.clone()
    }

    /// Lock-free per-edge audio levels for the UI's breathing/flowing cables.
    pub fn edge_levels_handle(&self) -> Arc<EdgeLevels> {
        self.edge_levels.clone()
    }

    /// Publish every audio edge's level = its source node's output peak this
    /// block (voices post-level, DSP/LV2 post-process, system capture
    /// post-gain). Block rate, allocation-free: linear scans over reserved
    /// graph storage, like the rest of the render path.
    fn publish_edge_levels(&self, n2: usize, sys: bool) {
        for e in &self.graph.edges {
            if e.kind != PortKind::Audio {
                continue;
            }
            let src = e.from.node;
            let peak = if let Some(v) = self.voices.iter().find(|v| v.node == src) {
                block_peak(&v.scratch[..n2])
            } else if let Some(d) = self.dsp.iter().find(|d| d.node == src) {
                block_peak(&d.buf[..n2])
            } else if let Some(l) = self.lv2.iter().find(|l| l.node == src) {
                block_peak(&l.buf[..n2])
            } else if sys
                && self.graph.node(src).map_or(false, |n| matches!(n.kind, NodeKind::SystemAudio))
            {
                block_peak(&self.system_buf[..n2]) * self.system_gain
            } else {
                0.0
            };
            self.edge_levels.store(e.id, peak);
        }
    }

    /// Read back the MIDI event pending at an LV2 instrument node (verification).
    pub fn lv2_last_note(&self, node: NodeId) -> Option<[u8; 3]> {
        self.lv2.iter().find(|s| s.node == node).and_then(|s| s.plugin.last_midi_event())
    }

    fn has_midi_cable(&self, dest: NodeId) -> bool {
        self.graph.edges.iter().any(|e| e.kind == PortKind::Midi && e.to.node == dest)
    }

    fn apply_dsp_or_master_param(&mut self, node: NodeId, param: ParamId, v: f32) -> bool {
        match param {
            P_VOL => {
                self.master_vol = v;
                true
            }
            P_WARM => {
                self.master_warm = v;
                true
            }
            _ => {
                if let Some(s) = self.dsp.iter_mut().find(|s| s.node == node) {
                    if let Some(native) = dsp_native_param(&s.kind, param) {
                        s.proc.set_param(native, v);
                        return true;
                    }
                }
                false
            }
        }
    }

    /// MIDI-cable gate for one PLAYED note (before transpose): it reaches
    /// `dest` if ANY Keys node has a MIDI cable to `dest` from the port that
    /// note leaves on — port 0 when unsplit; LOW (0) below the split point,
    /// HIGH (1) at/above it when split. Each rig carries its own Keys node
    /// (and its own split) after LoadRigs. Note-offs take the same port as
    /// their note-on because the zone is a pure function of the note.
    /// Allocation-free linear scans, like the rest of the render path.
    fn keys_gate(&self, dest: NodeId, played: u8) -> bool {
        self.graph.nodes.iter().any(|n| {
            matches!(n.kind, NodeKind::Keys) && {
                let port = split_port(n.split, played);
                self.graph.edges.iter().any(|e| {
                    e.kind == PortKind::Midi
                        && e.from.node == n.id
                        && e.from.port == port
                        && e.to.node == dest
                })
            }
        })
    }

    /// Note-off for all 128 notes on one instrument. The matrix is stateless,
    /// so every edit that can change a held note's note-off path (split, rule,
    /// MIDI cable, source filter, node removal, graph load) calls this first.
    /// Rare op, allocation-free. Checked by `held_notes` (verify-held-notes).
    fn release_all(&mut self, dest: NodeId) {
        if let Some(s) = self.voices.iter().find(|s| s.node == dest) {
            for n in 0..128u16 { s.voice.note_off(n); }
        }
        if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
            for n in 0..128u8 { s.plugin.note_off(n); }
        }
        #[cfg(test)]
        self.note_log.push((dest, 255, false)); // 255 = all-notes-off marker
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::AddNode { kind, name, rig } => {
                let id = self.graph.add_node(kind.clone(), name);
                self.graph.set_rig(id, rig);
                let sr = self.sample_rate;
                match &kind {
                    NodeKind::Dsp(dk) => {
                        if let Some(n) = self.graph.node(id) {
                            let slot = dsp_slot(n, dk, sr);
                            self.dsp.push(slot);
                        }
                    }
                    NodeKind::Lv2 { uri } => self.request_lv2(id, *uri),
                    NodeKind::Clap { plugin_id } => {
                        self.matrix.connect(SourceId::Keys, id);
                        for dev in 0..MAX_HW_DEVICES {
                            self.matrix.connect(SourceId::Hardware(dev), id);
                        }
                        let pid = name_str(plugin_id);
                        if let Some(v) = self.voices.iter_mut().find(|v| v.node == u32::MAX && v.voice.plugin_id() == pid) {
                            v.node = id;
                        } else if pid.is_empty() || pid == "Surge XT" {
                            if let Some(v) = self.voices.iter_mut().find(|v| v.node == u32::MAX) {
                                v.node = id;
                            }
                        }
                    }
                    _ => {}
                }
                self.publish_graph();
            }
            Command::RemoveNode(id) => {
                // An idle voice keeps a held note and replays it when rebound.
                self.release_all(id);
                self.graph.remove_node(id);
                self.dsp.retain(|s| s.node != id);
                if let Some(i) = self.lv2.iter().position(|s| s.node == id) {
                    let slot = self.lv2.remove(i);
                    self.retire_lv2(slot);
                }
                for s in &mut self.voices {
                    if s.node == id {
                        s.node = u32::MAX;
                    }
                }
                self.publish_graph();
            }
            Command::Connect { id, from, to, kind } => {
                // Publish only on a real change: the UI's per-frame mirror
                // re-sends duplicate Connects, which must not bump the rev.
                if self.graph.connect_with_id(id, from, to, kind) {
                    self.publish_graph();
                }
            }
            Command::Disconnect(id) => {
                // keys_gate drops a note-off whose Keys cable is gone.
                let fed = self.graph.edges.iter().find(|e| {
                    e.id == id
                        && e.kind == PortKind::Midi
                        && self.graph.node(e.from.node).map_or(false, |n| matches!(n.kind, NodeKind::Keys))
                });
                if let Some(dest) = fed.map(|e| e.to.node) {
                    self.release_all(dest);
                }
                let had = self.graph.edges.iter().any(|e| e.id == id);
                self.graph.disconnect(id);
                if had {
                    self.publish_graph();
                }
            }
            Command::SetParam { node, param, value } => {
                let v = value as f32;
                if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                    // LV2 knobs map param index → nth control port (normalized).
                    s.plugin.set_control_normalized(param as usize, v);
                } else if param == P_LEVEL && self.voices.iter().any(|s| s.node == node) {
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                        s.level = v;
                    }
                } else {
                    self.apply_dsp_or_master_param(node, param, v);
                }
            }
            Command::LoadLv2Preset { node, preset_uri } => {
                if let Some(NodeKind::Lv2 { uri }) = self.graph.nodes.iter().find(|n| n.id == node).map(|n| &n.kind) {
                    if let Some(tx) = self.lv2_req.as_mut() {
                        let _ = tx.push(Lv2Request::Preset { node, uri: *uri, preset_uri });
                        return;
                    }
                    let presets = crate::lv2::discover_presets(uri.as_str());
                    if let Some(preset) = presets.iter().find(|p| p.uri == preset_uri) {
                        if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                            s.plugin.apply_preset(preset);
                        }
                    }
                }
            }
            Command::LoadGraph { graph } => {
                // Rare structural op (template load): rebuild graph, DSP, voice
                // mapping, and MIDI matrix. Allocates — acceptable off the hot
                // path; a future version can rebuild off-thread and swap.
                // Voices survive the rebuild, the old note-off paths do not.
                for i in 0..self.graph.nodes.len() {
                    let id = self.graph.nodes[i].id;
                    self.release_all(id);
                }
                self.graph = graph;
                let sr = self.sample_rate;
                self.dsp = self
                    .graph
                    .nodes
                    .iter()
                    .filter_map(|n| match &n.kind {
                        NodeKind::Dsp(kind) => Some(dsp_slot(n, kind, sr)),
                        _ => None,
                    })
                    .collect();
                // Old instances are freed (on the loader thread when it runs);
                // new ones load below, after the matrix is rebuilt.
                while let Some(slot) = self.lv2.pop() {
                    self.retire_lv2(slot);
                }
                self.lv2_gen = self.lv2_gen.wrapping_add(1);
                // Remap existing CLAP voices onto the new Clap nodes by matching
                // plugin_id (falling back to any idle Surge voice for legacy
                // "Surge XT" / empty ids); unmatched voices go idle (u32::MAX).
                let clap_nodes: Vec<(NodeId, String)> = self
                    .graph
                    .nodes
                    .iter()
                    .filter_map(|n| match &n.kind {
                        NodeKind::Clap { plugin_id } => Some((n.id, name_str(plugin_id).to_string())),
                        _ => None,
                    })
                    .collect();
                for v in &mut self.voices {
                    v.node = u32::MAX;
                }
                for (id, pid) in &clap_nodes {
                    if let Some(v) = self.voices.iter_mut().find(|v| v.node == u32::MAX && v.voice.plugin_id() == pid) {
                        v.node = *id;
                    } else if pid.is_empty() || pid == "Surge XT" {
                        if let Some(v) = self.voices.iter_mut().find(|v| v.node == u32::MAX) {
                            v.node = *id;
                        }
                    }
                }
                let clap_ids: Vec<NodeId> = clap_nodes.iter().map(|(id, _)| *id).collect();
                self.matrix = Matrix::new();
                self.matrix.reserve(64);
                for &id in &clap_ids {
                    self.matrix.connect(SourceId::Keys, id);
                    for dev in 0..MAX_HW_DEVICES {
                        self.matrix.connect(SourceId::Hardware(dev), id);
                    }
                }
                self.rig_meta.clear();
                for i in 0..self.graph.nodes.len() {
                    if let NodeKind::Lv2 { uri } = self.graph.nodes[i].kind {
                        let id = self.graph.nodes[i].id;
                        self.request_lv2(id, uri);
                    }
                }
                self.active_rig = 0;
                self.rig_mix = [RigMix::DEFAULT; 8];
                if self.master_clock.is_counting_in() {
                    self.master_clock = MasterClock::Stopped;
                    self.transport.set_playing(false);
                    self.count_in_slot.set_playing(false);
                    self.last_count_in_beat = 0;
                    self.transport_dirty = true;
                }
                self.publish_rig_snapshot();
                self.publish_graph();
            }
            Command::LoadRigs { rigs, active } => {
                // Rare structural op: merge the rig graphs into one patch and
                // reuse the LoadGraph rebuild (DSP, LV2, CLAP voice remap in
                // node-id order, matrix), then bind each rig's
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
                    if !self.keys_gate(dest, note) {
                        continue;
                    }
                    #[cfg(test)]
                    self.note_log.push((dest, m.note(), true));
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
                    if !self.keys_gate(dest, note) {
                        continue;
                    }
                    #[cfg(test)]
                    self.note_log.push((dest, m.note(), false));
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
            Command::SetRigSource { node, keyboard, hardware } => {
                // source_accepted would drop the note-off of a source turned off.
                self.release_all(node);
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                    s.src_keys = keyboard;
                    s.src_hw = hardware;
                }
            }
            Command::SetRigDevice { node, device } => {
                self.release_all(node);
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                    s.rig_device = device;
                }
            }
            Command::SetRigMix { rig, vol, pan, mute, solo } => {
                let m = &mut self.rig_mix[(rig as usize).min(7)];
                if let Some(v) = vol { m.vol = v.clamp(0.0, 1.0); }
                if let Some(p) = pan { m.pan = p.clamp(-1.0, 1.0); }
                if let Some(b) = mute { m.mute = b; }
                if let Some(b) = solo { m.solo = b; }
                self.mix_dirty = true;
            }
            Command::SetNodeSplit { node, point } => {
                // A Keys-node property: only a real change on a Keys node acts.
                let point = point.map(|p| p.min(127));
                let is_change = self.graph.node(node).map_or(false, |n| {
                    matches!(n.kind, NodeKind::Keys) && n.split != point
                });
                if is_change {
                    // Release held notes on EVERY instrument this Keys node
                    // feeds, from both ports, BEFORE the zones move — a note
                    // held across the change would otherwise lose its
                    // note-off to the other port. Index loop: no allocation.
                    for i in 0..self.graph.edges.len() {
                        let e = &self.graph.edges[i];
                        if e.kind == PortKind::Midi && e.from.node == node {
                            let dest = e.to.node;
                            self.release_all(dest);
                        }
                    }
                    // Unsplitting also drops the node's HIGH-port cables.
                    self.graph.set_split(node, point);
                    self.publish_graph();
                }
            }
            Command::MatrixRule { source, dest, channel, transpose, key_lo, key_hi, vel_lo, vel_hi, enabled } => {
                // Live per-cell rule edit (hot-wire matrix). In-place when the
                // rule exists; otherwise create it (reserved capacity; rare op).
                let (kl, kh) = (key_lo.min(key_hi), key_lo.max(key_hi));
                let (vl, vh) = (vel_lo.min(vel_hi), vel_lo.max(vel_hi));
                match self.matrix.rules.iter_mut().find(|r| r.source == source && r.dest == dest) {
                    Some(r) => {
                        r.channel = channel;
                        r.transpose = transpose;
                        r.key_range = (kl, kh);
                        r.vel_range = (vl, vh);
                        r.enabled = enabled;
                        r.cc_only = false;
                    }
                    None => {
                        let mut r = Rule::new(source, dest);
                        r.channel = channel;
                        r.transpose = transpose;
                        r.key_range = (kl, kh);
                        r.vel_range = (vl, vh);
                        r.enabled = enabled;
                        self.matrix.add_rule(r);
                    }
                }
                // Stateless-matrix safety: release held notes on the destination
                // so a range/transpose edit can't orphan a release (rare op).
                self.release_all(dest);
                self.publish_graph();
            }
            Command::Cc { source, channel, cc, value } => {
                let msg = MidiMsg::cc(channel, cc, value);
                self.matrix.route_into(source, msg, &mut self.route_out);
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let param = self
                        .matrix
                        .rules
                        .iter()
                        .find(|r| r.source == source && r.dest == dest && r.transform(msg).is_some())
                        .and_then(|r| r.param_for_cc(m.data[0]));
                    let has_midi = self.has_midi_cable(dest);
                    let is_clap_node = self
                        .graph
                        .node(dest)
                        .map_or(false, |n| matches!(n.kind, NodeKind::Clap { .. }));
                    let norm = m.data[1] as f64 / 127.0;
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if !has_midi || !source_accepted(s, source) {
                            continue;
                        }
                        if let Some(pid) = param {
                            s.voice.set_param_normalized_rt(pid, norm);
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        } else {
                            s.voice.send_cc_rt(m.channel, m.data[0], m.data[1]);
                            #[cfg(test)]
                            self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                        }
                    } else if is_clap_node {
                        if !has_midi {
                            continue;
                        }
                        #[cfg(test)]
                        if let Some(pid) = param {
                            self.cc_log.push((dest, pid, norm));
                        } else {
                            self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                        }
                    } else if let Some(pid) = param {
                        if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                            if s.plugin.is_instrument() && !has_midi {
                                continue;
                            }
                            s.plugin.set_control_normalized(pid as usize, norm as f32);
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        } else if self.apply_dsp_or_master_param(dest, pid, norm as f32) {
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        }
                    }
                }
            }
            Command::MatrixCcMap { source, dest, cc, param } => {
                let changed = match param {
                    Some(p) => match self
                        .matrix
                        .rules
                        .iter_mut()
                        .find(|r| r.source == source && r.dest == dest)
                    {
                        Some(r) => r.set_cc(cc, p),
                        None => {
                            // Learning a knob must not also wire the keys.
                            let mut r = Rule::new(source, dest);
                            r.cc_only = true;
                            r.set_cc(cc, p);
                            self.matrix.add_rule(r);
                            true
                        }
                    },
                    None => self
                        .matrix
                        .rules
                        .iter_mut()
                        .find(|r| r.source == source && r.dest == dest)
                        .map_or(false, |r| r.unmap_cc(cc)),
                };
                if changed {
                    self.publish_graph();
                }
            }
            Command::SetSystemAudio { gain } => {
                self.system_gain = gain.clamp(0.0, 1.0);
            }
            Command::SetPlayer { gain } => {
                self.player_gain = gain.clamp(0.0, 1.0);
            }
            // Global transport = MASTER: fan out to every transport-capable
            // node (all start together on the downbeat). Per-node
            // SetNodePlaying overrides individually afterwards and never
            // touches the song clock plugins see.
            Command::TransportPlay => self.master_transport(true),
            Command::TransportStop => self.master_transport(false),
            Command::SetNodePlaying { node, playing } => {
                // Unknown / non-transport nodes are ignored (flag stays false).
                if let Some(changed) = self.graph.set_playing(node, playing) {
                    if let Some(d) = self.dsp.iter_mut().find(|d| d.node == node) {
                        d.transport.set_playing(playing);
                    }
                    if changed {
                        self.publish_graph();
                    }
                }
            }
            Command::SetBpm { bpm } => {
                let clamped = bpm.clamp(20.0, 300.0) as f64;
                if (self.transport.bpm - clamped).abs() > 1e-6 {
                    self.transport.bpm = clamped;
                    self.transport_dirty = true;
                }
            }
            Command::SetCountIn { enabled, bars } => {
                let next = CountInConfig::normalized(enabled, bars);
                if self.count_in != next {
                    self.count_in = next;
                    self.transport_dirty = true;
                }
            }
            Command::RecordStart => self.record_start(),
            Command::RecordStop => self.record_stop(),
            Command::SetRecordSync { on } => self.record_sync = on,
        }
    }

    /// Master transport (TransportPlay/Stop): run or halt the song clock (with
    /// optional count-in bar before beat 0) and set every transport-capable
    /// node's flag and slot clock; publish only when state actually changed.
    fn master_transport(&mut self, playing: bool) {
        if playing {
            if self.master_clock.is_active() {
                return;
            }
            self.transport = Transport::stopped(self.transport.bpm);
            if self.count_in.enabled {
                let total = self.count_in.total_beats();
                self.master_clock = MasterClock::CountingIn {
                    remaining_beats: total,
                    total_beats: total,
                };
                self.last_count_in_beat = self.master_clock.count_in_beat();
                self.count_in_slot.set_playing(false);
                self.count_in_slot.set_playing(true);
            } else {
                self.master_clock = MasterClock::Playing;
                self.last_count_in_beat = 0;
                self.transport.set_playing(true);
                self.count_in_slot.set_playing(false);
            }
            let changed = self.graph.set_all_playing(true);
            for d in &mut self.dsp {
                if self.graph.node(d.node).map_or(false, |n| n.kind.supports_transport()) {
                    d.transport.set_playing(false);
                    d.transport.set_playing(true);
                }
            }
            self.transport_dirty = true;
            if changed {
                self.publish_graph();
            }
        } else {
            let was_active = self.master_clock.is_active() || self.transport.playing;
            self.master_clock = MasterClock::Stopped;
            self.last_count_in_beat = 0;
            self.transport.set_playing(false);
            self.count_in_slot.set_playing(false);
            let changed = self.graph.set_all_playing(false);
            for d in &mut self.dsp {
                if self.graph.node(d.node).map_or(false, |n| n.kind.supports_transport()) {
                    d.transport.set_playing(false);
                }
            }
            if was_active {
                self.transport_dirty = true;
            }
            if changed {
                self.publish_graph();
            }
        }
        // Sync-to-transport: recording follows the master transport.
        if self.record_sync {
            if playing {
                self.record_start();
            } else {
                self.record_stop();
            }
        }
    }

    /// Render one device block, allocation-free.
    pub fn render(&mut self, data: &mut [f32], ch: usize) {
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }
        self.drain_lv2_loader();
        // Attach any runtime-loaded CLAP voices delivered from the UI thread.
        while let Some(rx) = self.voice_rx.as_mut() {
            let Ok((node, voice, scratch)) = rx.pop() else { break };
            if let Some(slot) = self.voices.iter_mut().find(|s| s.node == node) {
                let old = std::mem::replace(&mut slot.voice, voice);
                if let Some(tx) = self.voice_retire_tx.as_mut() {
                    let _ = tx.push(old);
                }
            } else if self.voices.len() < self.voices.capacity() {
                self.voices.push(VoiceSlot {
                    node,
                    voice,
                    level: 0.85,
                    scratch,
                    src_keys: true,
                    src_hw: true,
                    rig_device: None,
                });
            } else if let Some(slot) = self.voices.iter_mut().find(|s| s.node == u32::MAX) {
                slot.node = node;
                slot.level = 0.85;
                slot.src_keys = true;
                slot.src_hw = true;
                slot.rig_device = None;
                let old = std::mem::replace(&mut slot.voice, voice);
                if let Some(tx) = self.voice_retire_tx.as_mut() {
                    let _ = tx.push(old);
                }
            } else if let Some(tx) = self.voice_retire_tx.as_mut() {
                let _ = tx.push(voice);
            }
        }
        // Mixer publication: a fixed-size copy under a NON-blocking lock
        // (never waits on the UI; retried next block if it is busy).
        if self.mix_dirty {
            match &self.graph_pub {
                Some(p) => if let Ok(mut g) = p.try_lock() {
                    g.mix = self.rig_mix;
                    g.mix_rev = g.mix_rev.wrapping_add(1);
                    self.mix_dirty = false;
                },
                None => self.mix_dirty = false,
            }
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
            // Idle pool voice (no Clap node mapped): skip the plugin entirely.
            if s.node == NodeId::MAX { continue; }
            s.voice.render(&mut s.scratch[..n2], self.transport);
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

        // 2a. Metronome nodes: click generators gated by their OWN playing
        // flag (per-instrument transport). Output reaches the master via
        // normal audio routing like any DSP node.
        let (sr, bpm) = (self.sample_rate, self.transport.bpm);
        let beat_inc = Transport::beats_per_frame(bpm, sr);
        let block_beats = frames as f64 * beat_inc;
        let counting_in_this_block = self.master_clock.is_counting_in();
        let suppress_wrap_click = matches!(
            self.master_clock,
            MasterClock::CountingIn { remaining_beats, .. } if remaining_beats <= block_beats + 1e-9
        );
        for d in &mut self.dsp {
            if d.kind == DspKind::Metronome {
                render_metronome(&mut d.transport, &mut d.buf[..n2], frames, sr, bpm, suppress_wrap_click);
            }
        }
        if counting_in_this_block {
            render_metronome(&mut self.count_in_slot, &mut self.count_in_buf[..n2], frames, sr, bpm, suppress_wrap_click);
        }
        match self.master_clock {
            MasterClock::Stopped => {}
            MasterClock::Playing => self.transport.advance(frames, sr),
            MasterClock::CountingIn { remaining_beats, total_beats } => {
                let rem = remaining_beats - block_beats;
                if rem <= 1e-9 {
                    self.master_clock = MasterClock::Playing;
                    self.last_count_in_beat = 0;
                    self.transport.set_playing(true);
                    self.count_in_slot.set_playing(false);
                    for d in &mut self.dsp {
                        if self.graph.node(d.node).map_or(false, |n| n.kind.supports_transport())
                            && d.transport.playing
                        {
                            d.transport.set_playing(false);
                            d.transport.set_playing(true);
                        }
                    }
                    self.transport_dirty = true;
                } else {
                    self.master_clock = MasterClock::CountingIn { remaining_beats: rem, total_beats };
                    let beat = self.master_clock.count_in_beat();
                    if beat != self.last_count_in_beat {
                        self.last_count_in_beat = beat;
                        self.transport_dirty = true;
                    }
                }
            }
        }
        if self.transport_dirty {
            let tp = self.transport_pub();
            match &self.graph_pub {
                Some(p) => if let Ok(mut g) = p.try_lock() {
                    g.transport = tp;
                    g.transport_rev = g.transport_rev.wrapping_add(1);
                    self.transport_dirty = false;
                },
                None => self.transport_dirty = false,
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
            // Per-rig mix gains, once per block (rig_mix changes only via
            // commands drained at block start). Constant-power pan, unity
            // center: gl = sqrt(1-pan), gr = sqrt(1+pan).
            let any_solo = self.rig_mix.iter().any(|m| m.solo);
            let mut gains = [(1.0f32, 1.0f32); 8];
            for mi in 0..n_masters {
                let rig = self.graph.node(masters[mi]).map(|n| n.rig as usize).unwrap_or(0).min(7);
                let m = self.rig_mix[rig];
                let g = if m.mute || (any_solo && !m.solo) { 0.0 } else { m.vol };
                gains[mi] = (g * (1.0 - m.pan).sqrt(), g * (1.0 + m.pan).sqrt());
            }
            // Reuse `lv2_in` (unused after step 2b) as the stereo mix bus so
            // route checks happen once per node per block, not per sample.
            let acc = &mut self.lv2_in[..n2];
            for x in acc.iter_mut() {
                *x = 0.0;
            }
            for mi in 0..n_masters {
                let master = masters[mi];
                let (gl, gr) = gains[mi];
                if gl == 0.0 && gr == 0.0 { continue; } // muted: skip the work
                for v in &self.voices {
                    if v.node == NodeId::MAX || !self.graph.has_route(v.node, master, PortKind::Audio) {
                        continue;
                    }
                    if self.graph.has_route_port(v.node, master, 2, PortKind::Audio) {
                        for i in 0..frames {
                            let m = v.scratch[i * 2]; // mono in: center to both
                            acc[i * 2] += m * gl;
                            acc[i * 2 + 1] += m * gr;
                        }
                    } else {
                        for i in 0..frames {
                            acc[i * 2] += v.scratch[i * 2] * gl;
                            acc[i * 2 + 1] += v.scratch[i * 2 + 1] * gr;
                        }
                    }
                }
                for d in &self.dsp {
                    if !self.graph.has_route(d.node, master, PortKind::Audio) {
                        continue;
                    }
                    if self.graph.has_route_port(d.node, master, 2, PortKind::Audio) {
                        for i in 0..frames {
                            let m = d.buf[i * 2];
                            acc[i * 2] += m * gl;
                            acc[i * 2 + 1] += m * gr;
                        }
                    } else {
                        for i in 0..frames {
                            acc[i * 2] += d.buf[i * 2] * gl;
                            acc[i * 2 + 1] += d.buf[i * 2 + 1] * gr;
                        }
                    }
                }
                for slot in &self.lv2 {
                    if !self.graph.has_route(slot.node, master, PortKind::Audio) {
                        continue;
                    }
                    if self.graph.has_route_port(slot.node, master, 2, PortKind::Audio) {
                        for i in 0..frames {
                            let m = slot.buf[i * 2];
                            acc[i * 2] += m * gl;
                            acc[i * 2 + 1] += m * gr;
                        }
                    } else {
                        for i in 0..frames {
                            acc[i * 2] += slot.buf[i * 2] * gl;
                            acc[i * 2 + 1] += slot.buf[i * 2 + 1] * gr;
                        }
                    }
                }
            }
            if counting_in_this_block {
                let mut has_routed_metro = false;
                for d in &self.dsp {
                    if d.kind == DspKind::Metronome
                        && d.transport.playing
                        && masters[..n_masters].iter().any(|&m| self.graph.has_route(d.node, m, PortKind::Audio))
                    {
                        has_routed_metro = true;
                        break;
                    }
                }
                if !has_routed_metro {
                    for i in 0..n2 {
                        acc[i] += self.count_in_buf[i];
                    }
                }
            }
            if sys {
                let g = self.system_gain;
                for i in 0..n2 {
                    acc[i] += self.system_buf[i] * g;
                }
            }
            if ply {
                let g = self.player_gain;
                for i in 0..n2 {
                    acc[i] += self.player_buf[i] * g;
                }
            }
            let drive = 1.0 + self.master_warm * 3.0;
            let vol = self.master_vol;
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let l = (acc[i * 2] * drive).tanh() * vol;
                let r = (acc[i * 2 + 1] * drive).tanh() * vol;
                peak = peak.max(l.abs()).max(r.abs());
                frame[0] = l;
                if ch > 1 {
                    frame[1] = r;
                }
                for s in frame.iter_mut().skip(2) {
                    *s = 0.0;
                }
            }
            // Record tap: the final output (post master vol/warm), what the
            // user hears. RT-safe — push never blocks; a full ring drops frames.
            if let Some(rec) = &mut self.recorder {
                rec.push_stereo(data, ch);
            }
        }
        self.out_peak.store(peak.to_bits(), Ordering::Relaxed);
        self.publish_edge_levels(n2, sys);
    }
}

/// Generate one metronome's click into `buf` (stereo-interleaved): a
/// decaying 1 kHz burst on each beat while ITS transport plays. Driven by
/// SetNodePlaying / master TransportPlay·Stop / SetBpm; allocation-free.
fn render_metronome(
    t: &mut SlotTransport,
    buf: &mut [f32],
    frames: usize,
    sample_rate: f64,
    bpm: f64,
    suppress_wrap_click: bool,
) {
    let frames = frames.min(buf.len() / 2);
    let n2 = frames * 2;
    for x in buf[..n2].iter_mut() {
        *x = 0.0;
    }
    // Phase counts beats with the song clock's own factor, so a tempo change
    // mid-beat leaves a master-started click on the beats plugins see.
    let beat_inc = Transport::beats_per_frame(bpm, sample_rate);
    for i in 0..frames {
        if t.playing {
            t.phase += beat_inc;
            if t.phase >= 1.0 {
                t.phase -= 1.0;
                if !suppress_wrap_click {
                    t.click_remaining = CLICK_FRAMES;
                    t.click_phase = 0.0;
                }
            }
        }
        if t.click_remaining > 0 {
            let env = t.click_remaining as f32 / CLICK_FRAMES as f32;
            let click = (2.0 * std::f32::consts::PI * 1000.0 * t.click_phase / sample_rate as f32).sin()
                * env
                * 0.4;
            buf[i * 2] = click;
            buf[i * 2 + 1] = click;
            t.click_phase += 1.0;
            t.click_remaining -= 1;
        }
    }
}

#[cfg(test)]
mod held_notes;

#[cfg(test)]
mod tests {
    use crate::matrix::{Matrix, MidiMsg, SourceId};

    fn dests(m: &Matrix, src: SourceId, msg: MidiMsg) -> Vec<u32> {
        let mut out = Vec::new();
        m.route_into(src, msg, &mut out);
        out.iter().map(|(d, _)| *d).collect()
    }

    /// Demo patch engine (Keys 0 → Surge A 1 / Surge B 4) with a published
    /// graph. Cable 6 (Keys → Surge B) moves to HIGH when `high_b`.
    fn split_engine(split: Option<u8>, high_b: bool) -> (super::GraphEngine, crate::control::CommandBus) {
        use crate::control::{command_bus, Graph, PortKind, PortRef};
        let mut g = Graph::demo_patch();
        g.set_split(0, split);
        if high_b {
            g.disconnect(6);
            assert!(g.connect_with_id(6, PortRef { node: 0, port: 1 }, PortRef { node: 4, port: 0 }, PortKind::Midi));
        }
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(Graph::default(), drain, 48000.0, vec![]);
        bus.send(crate::control::Command::LoadGraph { graph: g });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        engine.note_log.clear();
        (engine, bus)
    }

    fn play(engine: &mut super::GraphEngine, bus: &mut crate::control::CommandBus, cmd: crate::control::Command) -> Vec<(u32, u8, bool)> {
        engine.note_log.clear();
        bus.send(cmd);
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        let mut log = engine.note_log.clone();
        log.sort_unstable();
        log
    }

    fn on(note: u8) -> crate::control::Command {
        crate::control::Command::NoteOn { note, velocity: 0.8, source: SourceId::Keys }
    }
    fn off(note: u8) -> crate::control::Command {
        crate::control::Command::NoteOff { note, source: SourceId::Keys }
    }

    #[test]
    fn unsplit_keys_play_every_wired_instrument() {
        let (mut e, mut bus) = split_engine(None, false);
        assert_eq!(play(&mut e, &mut bus, on(59)), vec![(1, 59, true), (4, 59, true)]);
        assert_eq!(play(&mut e, &mut bus, on(60)), vec![(1, 60, true), (4, 60, true)]);
        // No global auto-split any more: the matrix keeps full-range rules.
        assert!(e.matrix.rules.iter().all(|r| r.key_range == (0, 127)));
    }

    #[test]
    fn split_at_c4_routes_low_and_high_ports() {
        // LOW (port 0) → Surge A, HIGH (port 1) → Surge B, split at 60.
        let (mut e, mut bus) = split_engine(Some(60), true);
        assert_eq!(play(&mut e, &mut bus, on(59)), vec![(1, 59, true)], "59 exits LOW only");
        assert_eq!(play(&mut e, &mut bus, on(60)), vec![(4, 60, true)], "60 exits HIGH only");
        assert_eq!(play(&mut e, &mut bus, on(96)), vec![(4, 96, true)]);
        // Note-offs follow the same port as their note-ons.
        assert_eq!(play(&mut e, &mut bus, off(59)), vec![(1, 59, false)]);
        assert_eq!(play(&mut e, &mut bus, off(60)), vec![(4, 60, false)]);
        // Laptop typing is the same Keyboard source: same Keys-node zones.
        let lap = crate::control::Command::NoteOn { note: 48, velocity: 0.8, source: SourceId::Keys };
        assert_eq!(play(&mut e, &mut bus, lap), vec![(1, 48, true)]);
    }

    #[test]
    fn split_with_both_cables_on_low_plays_only_the_low_zone() {
        // Enabling split keeps existing cables on LOW: high notes go nowhere
        // until something is wired to HIGH (pure graph routing).
        let (mut e, mut bus) = split_engine(Some(60), false);
        assert_eq!(play(&mut e, &mut bus, on(40)), vec![(1, 40, true), (4, 40, true)]);
        assert!(play(&mut e, &mut bus, on(72)).is_empty());
    }

    #[test]
    fn split_change_releases_held_notes_on_both_ports() {
        use crate::control::Command;
        let (mut e, mut bus) = split_engine(Some(60), true);
        // Hold 59 (LOW → A) and 72 (HIGH → B), then move the split.
        play(&mut e, &mut bus, on(59));
        play(&mut e, &mut bus, on(72));
        let log = play(&mut e, &mut bus, Command::SetNodeSplit { node: 0, point: Some(48) });
        assert_eq!(log, vec![(1, 255, false), (4, 255, false)], "all-notes-off on both zones' instruments");
        assert_eq!(e.graph().node(0).unwrap().split, Some(48));
        // A no-op (same point) releases nothing.
        assert!(play(&mut e, &mut bus, Command::SetNodeSplit { node: 0, point: Some(48) }).is_empty());
        // Unsplit: release again, HIGH cable dropped, full range on LOW.
        let log = play(&mut e, &mut bus, Command::SetNodeSplit { node: 0, point: None });
        assert_eq!(log, vec![(1, 255, false), (4, 255, false)]);
        assert!(!e.graph().edges.iter().any(|x| x.from.node == 0 && x.from.port == 1));
        assert_eq!(play(&mut e, &mut bus, on(72)), vec![(1, 72, true)]);
        // Non-Keys nodes ignore the command.
        assert!(play(&mut e, &mut bus, Command::SetNodeSplit { node: 1, point: Some(60) }).is_empty());
        assert_eq!(e.graph().node(1).unwrap().split, None);
    }

    #[test]
    fn rig_source_filter_is_a_keyboard_hardware_pair() {
        use super::source_allowed;
        // Keyboard off: on-screen AND laptop notes (one source) are blocked;
        // hardware still plays.
        assert!(!source_allowed(false, true, None, SourceId::Keys));
        assert!(source_allowed(false, true, None, SourceId::Hardware(3)));
        // Hardware off: only the keyboard plays.
        assert!(source_allowed(true, false, None, SourceId::Keys));
        assert!(!source_allowed(true, false, None, SourceId::Hardware(3)));
        // Device binding narrows hardware only.
        assert!(source_allowed(true, true, Some(2), SourceId::Hardware(2)));
        assert!(!source_allowed(true, true, Some(2), SourceId::Hardware(0)));
        // Non-keyboard, non-hardware sources are never filtered.
        assert!(source_allowed(false, false, None, SourceId::FilePlayer(0)));
    }

    #[test]
    fn default_matrix_has_one_keyboard_rule_per_instrument() {
        // One Keyboard row: exactly one Keys rule per instrument, no second
        // laptop rule (the sources are unified).
        let (e, _bus) = split_engine(None, false);
        let keys_rules = |dest| e.matrix.rules.iter().filter(|r| r.dest == dest && r.source == SourceId::Keys).count();
        assert_eq!(keys_rules(1), 1);
        assert_eq!(keys_rules(4), 1);
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
        // Mirror the engine's default matrix: Keyboard + Hardware(0..MAX).
        let mut m = Matrix::new();
        for node in [1u32, 4u32] {
            m.connect(SourceId::Keys, node);
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
    fn matrix_rule_edits_in_place_and_creates_missing() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::demo_patch(), drain, 48000.0, vec![]);
        let mut buf = vec![0.0f32; 64 * 2];
        // Edit the existing Keys→1 rule: bass zone, octave down, channel 2.
        bus.send(Command::MatrixRule {
            source: SourceId::Keys, dest: 1,
            channel: Some(2), transpose: -12,
            key_lo: 59, key_hi: 0, // reversed on purpose → normalized
            vel_lo: 64, vel_hi: 127, enabled: true,
        });
        engine.render(&mut buf, 2);
        let r = engine.matrix.rules.iter().find(|r| r.source == SourceId::Keys && r.dest == 1).unwrap();
        assert_eq!(r.channel, Some(2));
        assert_eq!(r.transpose, -12);
        assert_eq!(r.key_range, (0, 59));
        assert_eq!(r.vel_range, (64, 127));
        // Routing honors the edit: note 48 (in zone) transposes to 36.
        let mut out = Vec::new();
        engine.matrix.route_into(SourceId::Keys, MidiMsg::note_on(2, 48, 100), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.note(), 36);
        // Note 60 is outside the zone → filtered.
        engine.matrix.route_into(SourceId::Keys, MidiMsg::note_on(2, 60, 100), &mut out);
        assert!(out.is_empty());
        // Unknown pair (device 9 → node 2) creates a rule.
        let before = engine.matrix.rules.len();
        bus.send(Command::MatrixRule {
            source: SourceId::Hardware(9), dest: 2,
            channel: None, transpose: 5,
            key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: false,
        });
        engine.render(&mut buf, 2);
        assert_eq!(engine.matrix.rules.len(), before + 1);
        let r = engine.matrix.rules.iter().find(|r| r.source == SourceId::Hardware(9) && r.dest == 2).unwrap();
        assert_eq!(r.transpose, 5);
        assert!(!r.enabled);
    }

    #[test]
    fn matrix_rule_publishes_rules_for_frontends() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::demo_patch(), drain, 48000.0, vec![]);
        let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::GraphPub {
            rev: 0, graph: crate::control::Graph::default(), rules: Vec::new(), ..Default::default()
        }));
        engine.set_graph_pub(pub_.clone());
        let mut buf = vec![0.0f32; 64 * 2];
        bus.send(Command::MatrixRule {
            source: SourceId::Keys, dest: 1,
            channel: None, transpose: 7,
            key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: true,
        });
        engine.render(&mut buf, 2);
        let p = pub_.lock().unwrap();
        assert_eq!(p.rev, 1, "rule edit must bump the publication rev");
        let r = p.rules.iter().find(|r| r.source == SourceId::Keys && r.dest == 1).unwrap();
        assert_eq!(r.transpose, 7);
    }

    /// Two metronomes: M1 wired to the master (audible), M2 unwired. Returns
    /// (engine, bus, m1, m2, graph publication).
    fn two_metronomes() -> (
        super::GraphEngine,
        crate::control::CommandBus,
        u32,
        u32,
        std::sync::Arc<std::sync::Mutex<super::GraphPub>>,
    ) {
        use crate::control::DspKind;
        let mut g = crate::control::Graph::default();
        let m1 = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("M1"));
        let m2 = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("M2"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: m1, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let (bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::GraphPub {
            rev: 0,
            graph: crate::control::Graph::default(),
            rules: Vec::new(), ..Default::default()
        }));
        engine.set_graph_pub(pub_.clone());
        (engine, bus, m1, m2, pub_)
    }

    /// Peak output over ~1 s (enough for two beats at 120 BPM).
    fn peak_1s(engine: &mut super::GraphEngine) -> f32 {
        let mut buf = vec![0.0f32; 512 * 2];
        let mut peak = 0.0f32;
        for _ in 0..(48000 / 512) {
            engine.render(&mut buf, 2);
            peak = buf.iter().fold(peak, |m, s| m.max(s.abs()));
        }
        peak
    }

    #[test]
    fn edge_levels_follow_the_source_node() {
        // M1 → Master (edge 1): playing raises the edge level above
        // silence; stopping lets it fall back to ~0. M2 has no edges.
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        let levels = engine.edge_levels_handle();
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetNodePlaying { node: m1, playing: true });
        let mut buf = vec![0.0f32; 512 * 2];
        let mut max_edge = 0.0f32;
        for _ in 0..(48000 / 512) {
            engine.render(&mut buf, 2);
            max_edge = max_edge.max(levels.load(1));
        }
        assert!(max_edge > 0.05, "playing metronome must light its audio edge (got {max_edge})");
        bus.send(Command::SetNodePlaying { node: m1, playing: false });
        for _ in 0..(48000 / 512) {
            engine.render(&mut buf, 2);
        }
        assert!(levels.load(1) < 1e-3, "stopped source must decay to silence (got {})", levels.load(1));
    }

    #[test]
    fn edge_level_slots_never_misattribute() {
        let l = super::EdgeLevels::new();
        l.store(3, 0.5);
        assert_eq!(l.load(3), 0.5);
        // Same slot, different id: the newer edge wins, the older reads 0.
        let other = 3 + super::EDGE_LEVEL_SLOTS as u32;
        l.store(other, 0.25);
        assert_eq!(l.load(other), 0.25);
        assert_eq!(l.load(3), 0.0);
        assert_eq!(l.load(42), 0.0, "never-published edge reads silence");
    }

    #[test]
    fn metronome_follows_its_own_playing_flag() {
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        bus.send(Command::SetBpm { bpm: 120.0 });
        // Global transport plays, but M1 is stopped individually → silent.
        bus.send(Command::TransportPlay);
        bus.send(Command::SetNodePlaying { node: m1, playing: false });
        assert!(peak_1s(&mut engine) < 0.05, "stopped metronome must stay silent while the transport plays");
        assert!(!engine.graph().node(m1).unwrap().playing);
        // Start M1 on its own → clicks.
        bus.send(Command::SetNodePlaying { node: m1, playing: true });
        assert!(peak_1s(&mut engine) > 0.05, "playing metronome must click");
        // Stop M1 again → silence returns (in-flight click cut).
        bus.send(Command::SetNodePlaying { node: m1, playing: false });
        assert!(peak_1s(&mut engine) < 0.05, "metronome did not stop");
    }

    #[test]
    fn metronome_clicks_without_global_transport() {
        // Per-instrument transport: the node's own flag is enough.
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        assert!(peak_1s(&mut engine) < 0.05, "metronomes start stopped");
        bus.send(Command::SetNodePlaying { node: m1, playing: true });
        assert!(peak_1s(&mut engine) > 0.05);
    }

    #[test]
    fn master_transport_sets_every_capable_node() {
        let (mut engine, mut bus, m1, m2, pub_) = two_metronomes();
        let mut buf = vec![0.0f32; 64 * 2];
        bus.send(Command::TransportPlay);
        engine.render(&mut buf, 2);
        let g = engine.graph();
        assert!(g.node(m1).unwrap().playing && g.node(m2).unwrap().playing, "master play starts all");
        assert!(g.nodes.iter().filter(|n| n.id != m1 && n.id != m2).all(|n| !n.playing),
            "non-transport nodes never get the flag");
        // The published graph carries the flags (UI face mirrors it).
        let rev = pub_.lock().unwrap().rev;
        assert!(rev > 0);
        assert!(pub_.lock().unwrap().graph.node(m2).unwrap().playing);
        // Per-node override after the master.
        bus.send(Command::SetNodePlaying { node: m2, playing: false });
        engine.render(&mut buf, 2);
        assert!(engine.graph().node(m1).unwrap().playing);
        assert!(!engine.graph().node(m2).unwrap().playing);
        assert!(!pub_.lock().unwrap().graph.node(m2).unwrap().playing);
        // Master stop clears all; repeating it changes nothing → no rev bump.
        bus.send(Command::TransportStop);
        engine.render(&mut buf, 2);
        assert!(engine.graph().nodes.iter().all(|n| !n.playing));
        let rev = pub_.lock().unwrap().rev;
        bus.send(Command::TransportStop);
        engine.render(&mut buf, 2);
        assert_eq!(pub_.lock().unwrap().rev, rev, "a no-op transport stop must not republish");
    }

    #[test]
    fn song_clock_follows_master_transport_and_tempo() {
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        let mut buf = vec![0.0f32; 480 * 2];
        let mut run = |engine: &mut super::GraphEngine, blocks: usize| {
            for _ in 0..blocks {
                engine.render(&mut buf, 2);
            }
            engine.transport()
        };
        bus.send(Command::SetBpm { bpm: 120.0 });
        let t = run(&mut engine, 100);
        assert!(!t.playing && t.beats == 0.0 && t.bpm == 120.0, "stopped clock stands still: {t:?}");

        // A single instrument's own Play is not the song: plugins stay stopped.
        bus.send(Command::SetNodePlaying { node: m1, playing: true });
        let t = run(&mut engine, 100);
        assert!(!t.playing && t.beats == 0.0, "SetNodePlaying must not move the song clock: {t:?}");

        bus.send(Command::TransportPlay);
        let t = run(&mut engine, 100); // 1 s at 120 BPM
        assert!(t.playing && (t.beats - 2.0).abs() < 1e-9 && (t.seconds - 1.0).abs() < 1e-9, "{t:?}");
        bus.send(Command::SetNodePlaying { node: m1, playing: false });
        bus.send(Command::SetBpm { bpm: 60.0 });
        let t = run(&mut engine, 100); // 1 s at 60 BPM
        assert!(t.playing && (t.beats - 3.0).abs() < 1e-9 && t.bpm == 60.0, "tempo change mid-run: {t:?}");

        bus.send(Command::TransportStop);
        let t = run(&mut engine, 100);
        assert!(!t.playing && (t.beats - 3.0).abs() < 1e-9, "stop keeps the position: {t:?}");
        bus.send(Command::TransportPlay);
        let t = run(&mut engine, 50);
        assert!((t.beats - 0.5).abs() < 1e-9 && (t.seconds - 0.5).abs() < 1e-9, "play rewinds to beat 0: {t:?}");
    }

    #[test]
    fn master_play_realigns_a_metronome_that_was_already_running() {
        // Started alone (or restored playing from a session), then the master
        // starts the song: the click must land on the song clock's beats.
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetNodePlaying { node: m1, playing: true });
        let mut buf = vec![0.0f32; 2];
        for _ in 0..12000 {
            engine.render(&mut buf, 2); // half a beat at 120 BPM
        }
        bus.send(Command::TransportPlay);
        let (mut clicks, mut was_quiet) = (Vec::new(), true);
        for frame in 0..48000usize * 2 {
            let beats = engine.transport().beats;
            engine.render(&mut buf, 2);
            let loud = buf[0].abs() > 1e-4;
            if loud && was_quiet && clicks.last().map_or(true, |&(f, _)| frame - f > super::CLICK_FRAMES) {
                clicks.push((frame, beats));
            }
            was_quiet = !loud;
        }
        assert!(clicks.len() >= 3, "expected a click per beat, got {clicks:?}");
        for (frame, beats) in &clicks {
            let off_beat = (beats - beats.round()).abs();
            assert!(off_beat < 3.0 * 120.0 / (60.0 * 48000.0), "click at frame {frame} is {off_beat:.5} beats off the song beat ({beats:.5})");
        }
    }

    #[test]
    fn master_started_metronome_clicks_on_the_song_clocks_beats() {
        // Also across a tempo change in the middle of a beat: the click and
        // the beats a tempo-synced plugin sees must not drift apart.
        let (mut engine, mut bus, _m1, _m2, _pub) = two_metronomes();
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::TransportPlay);
        let mut buf = vec![0.0f32; 2];
        let (mut clicks, mut was_quiet) = (Vec::new(), true);
        for frame in 0..48000usize * 3 {
            if frame == 30000 {
                bus.send(Command::SetBpm { bpm: 90.0 }); // 1.25 beats in
            }
            let beats = engine.transport().beats;
            engine.render(&mut buf, 2);
            // CLICK_FRAMES of 1 kHz decay follow each onset; an onset is the
            // first loud frame after the previous click died away.
            let loud = buf[0].abs() > 1e-4;
            if loud && was_quiet && clicks.last().map_or(true, |&(f, _)| frame - f > super::CLICK_FRAMES) {
                clicks.push((frame, beats));
            }
            was_quiet = !loud;
        }
        assert!(clicks.len() >= 5, "expected a click per beat, got {clicks:?}");
        for (frame, beats) in &clicks {
            // The 1 kHz burst starts at a zero crossing: its first loud frame
            // is one or two frames after the beat.
            let off_beat = (beats - beats.round()).abs();
            assert!(off_beat < 3.0 * 120.0 / (60.0 * 48000.0), "click at frame {frame} is {off_beat:.5} beats off the beat ({beats:.5})");
        }
    }

    #[test]
    fn count_in_clicks_for_one_bar_then_starts_song_at_beat_zero() {
        let (mut engine, mut bus, _m1, _m2, pub_) = two_metronomes();
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = vec![0.0f32; 480 * 2];
        let mut clicks = Vec::new();
        let mut was_quiet = true;
        // 200 blocks of 480 frames @ 48 kHz = 2.0 s = 4 beats @ 120 BPM.
        for blk in 0..300 {
            let pre_t = engine.transport();
            engine.render(&mut buf, 2);
            if blk < 200 {
                assert!(!pre_t.playing && pre_t.beats == 0.0, "block {blk} leaked song start: {pre_t:?}");
            }
            for f in 0..480 {
                let frame = blk * 480 + f;
                let loud = buf[f * 2].abs() > 1e-4;
                if loud && was_quiet && clicks.last().map_or(true, |&prev| frame - prev > super::CLICK_FRAMES) {
                    clicks.push(frame);
                }
                was_quiet = !loud;
            }
            if blk == 199 {
                let t = engine.transport();
                assert!(t.playing && t.beats == 0.0, "song clock must start at beat 0 right after count-in: {t:?}");
                assert!(!engine.master_clock().is_counting_in());
                let pub_t = pub_.lock().unwrap().transport;
                assert!(pub_t.playing && !pub_t.counting_in && pub_t.count_in_beat == 0);
            }
            if blk == 50 {
                let pub_t = pub_.lock().unwrap().transport;
                assert!(!pub_t.playing && pub_t.counting_in && pub_t.count_in_beat == 2, "expected count-in beat 2: {pub_t:?}");
            }
        }
        // 4 count-in clicks (frames ~0, 24000, 48000, 72000) + song beat 0 (~96000) + beat 1 (~120000).
        assert_eq!(clicks.len(), 6, "expected 4 count-in + 2 song clicks, got {clicks:?}");
        for (i, &frame) in clicks.iter().enumerate() {
            let expected = i * 24000;
            assert!((frame as isize - expected as isize).abs() <= 3, "click {i} at {frame}, expected ~{expected}");
        }
        // 100 song blocks = 1.0 s @ 120 BPM = 2.0 beats.
        let end_t = engine.transport();
        assert!(end_t.playing && (end_t.beats - 2.0).abs() < 1e-9, "song beats after 1s: {end_t:?}");
    }

    #[test]
    fn count_in_stop_cancels_before_song_starts() {
        let (mut engine, mut bus, _m1, _m2, pub_) = two_metronomes();
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = vec![0.0f32; 480 * 2];
        for _ in 0..100 {
            engine.render(&mut buf, 2); // 1.0 s (halfway through 1-bar count-in)
        }
        assert!(engine.master_clock().is_counting_in());
        bus.send(Command::TransportStop);
        engine.render(&mut buf, 2);
        assert_eq!(engine.master_clock(), super::MasterClock::Stopped);
        assert!(!engine.transport().playing && engine.transport().beats == 0.0);
        let pub_t = pub_.lock().unwrap().transport;
        assert!(!pub_t.playing && !pub_t.counting_in && pub_t.count_in_beat == 0);
        // Subsequent blocks past where count-in would have ended stay silent and stopped.
        let mut peak = 0.0f32;
        for _ in 0..150 {
            engine.render(&mut buf, 2);
            for &s in &buf { peak = peak.max(s.abs()); }
        }
        assert!(!engine.transport().playing && engine.transport().beats == 0.0);
        assert!(peak < 1e-4, "expected silence after cancel, got peak {peak}");
    }

    #[test]
    fn count_in_works_without_metronome_node() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::demo_patch(), drain, 48000.0, vec![]);
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetCountIn { enabled: true, bars: 1 });
        bus.send(Command::TransportPlay);
        let mut buf = vec![0.0f32; 480 * 2];
        let mut clicks = Vec::new();
        let mut was_quiet = true;
        for blk in 0..250 {
            engine.render(&mut buf, 2);
            for f in 0..480 {
                let frame = blk * 480 + f;
                let loud = buf[f * 2].abs() > 1e-4;
                if loud && was_quiet && clicks.last().map_or(true, |&prev| frame - prev > super::CLICK_FRAMES) {
                    clicks.push(frame);
                }
                was_quiet = !loud;
            }
            if blk == 199 {
                assert!(engine.transport().playing && engine.transport().beats == 0.0);
            }
        }
        assert_eq!(clicks.len(), 4, "expected 4 count-in clicks without a metronome node, got {clicks:?}");
    }

    #[test]
    fn set_node_playing_ignores_non_transport_nodes() {
        let (mut engine, mut bus, _m1, _m2, pub_) = two_metronomes();
        let mut buf = vec![0.0f32; 64 * 2];
        let master = engine.graph().nodes.iter().find(|n| matches!(n.kind, NodeKind::MasterOut)).unwrap().id;
        bus.send(Command::SetNodePlaying { node: master, playing: true });
        bus.send(Command::SetNodePlaying { node: 999, playing: true });
        engine.render(&mut buf, 2);
        assert!(!engine.graph().node(master).unwrap().playing);
        assert_eq!(pub_.lock().unwrap().rev, 0, "ignored commands must not republish");
    }

    #[test]
    fn load_graph_restores_persisted_playing() {
        // Session restore: a graph whose metronome was saved playing clicks
        // right after LoadGraph — no TransportPlay needed.
        let (mut engine, mut bus, m1, _m2, _pub) = two_metronomes();
        let mut g = engine.graph().clone();
        assert_eq!(g.set_playing(m1, true), Some(true));
        bus.send(Command::LoadGraph { graph: g });
        assert!(peak_1s(&mut engine) > 0.05);
        assert!(engine.graph().node(m1).unwrap().playing);
    }

    #[test]
    fn graph_pub_revs_on_structural_changes_only() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::demo_patch(), drain, 48000.0, vec![]);
        let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::GraphPub {
            rev: 0,
            graph: crate::control::Graph::default(),
            rules: Vec::new(), ..Default::default()
        }));
        engine.set_graph_pub(pub_.clone());
        let mut buf = vec![0.0f32; 64 * 2];
        let rev = || pub_.lock().unwrap().rev;
        assert_eq!(rev(), 0);
        // Duplicate Connect (the UI's per-frame mirror) must NOT bump the rev.
        let g = crate::control::Graph::demo_patch();
        let e0 = &g.edges[0];
        bus.send(Command::Connect { id: e0.id, from: e0.from, to: e0.to, kind: e0.kind });
        engine.render(&mut buf, 2);
        assert_eq!(rev(), 0);
        // AddNode bumps.
        bus.send(Command::AddNode {
            kind: NodeKind::Dsp(crate::control::DspKind::Gain),
            name: crate::control::name_bytes("G"),
            rig: 0,
        });
        engine.render(&mut buf, 2);
        assert_eq!(rev(), 1);
        // LoadGraph bumps and publishes the new graph.
        bus.send(Command::LoadGraph { graph: crate::control::Graph::default() });
        engine.render(&mut buf, 2);
        assert_eq!(rev(), 2);
        assert!(pub_.lock().unwrap().graph.nodes.is_empty());
        // Disconnect of a nonexistent id must NOT bump.
        bus.send(Command::Disconnect(999));
        engine.render(&mut buf, 2);
        assert_eq!(rev(), 2);
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
        // Each rig's OWN Keys node gates its synth (multi-rig cable gate).
        assert!(engine.keys_gate(1, 60));
        assert!(engine.keys_gate(4, 60));
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

    #[test]
    fn learning_a_knob_on_an_unrouted_device_does_not_route_its_notes() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::default(), drain, 48000.0, vec![]);
        bus.send(Command::LoadGraph { graph: crate::control::Graph::demo_patch() });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        // Device 20 is beyond the pre-routed hardware range: no rule to Surge A.
        let dev = SourceId::Hardware(20);
        bus.send(Command::MatrixCcMap { source: dev, dest: 1, cc: 74, param: Some(42) });
        engine.render(&mut buf, 2);
        engine.note_log.clear();
        engine.cc_log.clear();
        bus.send(Command::NoteOn { note: 60, velocity: 0.8, source: dev });
        bus.send(Command::Cc { source: dev, channel: 0, cc: 74, value: 127 });
        engine.render(&mut buf, 2);
        assert!(engine.note_log.is_empty(), "learn wired the device's notes: {:?}", engine.note_log);
        assert_eq!(engine.cc_log, vec![(1, 42, 1.0)], "the learned CC must still reach its param");
        // Shaping the route in the matrix makes it a normal (note) route.
        bus.send(Command::MatrixRule { source: dev, dest: 1, channel: None, transpose: 0, key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: true });
        bus.send(Command::NoteOn { note: 62, velocity: 0.8, source: dev });
        engine.render(&mut buf, 2);
        assert!(engine.note_log.iter().any(|&(d, n, on)| d == 1 && n == 62 && on), "{:?}", engine.note_log);
    }

    #[test]
    fn cc_routes_to_mapped_param_and_whitelists() {
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::demo_patch();
        let filter = g.add_node(NodeKind::Dsp(crate::control::DspKind::Filter), name_bytes("FILTER"));
        let mut engine = super::GraphEngine::new(crate::control::Graph::default(), drain, 48000.0, vec![]);
        let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::GraphPub::default()));
        engine.set_graph_pub(pub_.clone());
        bus.send(Command::LoadGraph { graph: g });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);

        // 1. Empty cc_map: CCs pass through to CLAP unchanged.
        engine.cc_log.clear();
        engine.cc_passthrough_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 64 });
        engine.render(&mut buf, 2);
        assert!(engine.cc_log.is_empty());
        assert_eq!(engine.cc_passthrough_log, vec![(1, 74, 64), (4, 74, 64)]);

        // 2. Bind CC 74 -> param 42 on Hardware(0) -> node 1.
        let rev_before = pub_.lock().unwrap().rev;
        bus.send(Command::MatrixCcMap {
            source: SourceId::Hardware(0),
            dest: 1,
            cc: 74,
            param: Some(42),
        });
        engine.render(&mut buf, 2);
        let rev_after = pub_.lock().unwrap().rev;
        assert_eq!(rev_after, rev_before + 1, "MatrixCcMap must publish cc_map");
        let r = pub_.lock().unwrap().rules.iter().find(|r| r.source == SourceId::Hardware(0) && r.dest == 1).cloned().unwrap();
        assert_eq!(r.param_for_cc(74), Some(42));

        // Duplicate MatrixCcMap does not bump rev.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(42) });
        engine.render(&mut buf, 2);
        assert_eq!(pub_.lock().unwrap().rev, rev_after);

        // 3. Whitelist active on Hardware(0)->1: CC 74 maps to param 42; CC 1 is dropped on node 1 (still transparent on node 4).
        engine.cc_log.clear();
        engine.cc_passthrough_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 127 });
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 1, value: 90 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_log, vec![(1, 42, 1.0)]);
        assert!(!engine.cc_passthrough_log.iter().any(|&(d, _, _)| d == 1), "node 1 whitelist must filter unmapped CC 1");
        assert_eq!(engine.cc_passthrough_log, vec![(4, 74, 127), (4, 1, 90)]);

        // 4. Replace CC 74 -> param 99, and bind CC 7 -> Filter cutoff (param 0).
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(99) });
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: filter, cc: 7, param: Some(0) });
        engine.cc_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 0 });
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 7, value: 64 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_log[0], (1, 99, 0.0));
        assert_eq!(engine.cc_log[1].0, filter);
        assert_eq!(engine.cc_log[1].1, 0);
        assert!((engine.cc_log[1].2 - 64.0 / 127.0).abs() < 1e-6);

        // 5. Remove CC 74 -> node 1 returns to empty-map transparency.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: None });
        engine.cc_log.clear();
        engine.cc_passthrough_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 1, value: 55 });
        engine.render(&mut buf, 2);
        assert!(engine.cc_log.is_empty());
        assert_eq!(engine.cc_passthrough_log, vec![(1, 1, 55), (4, 1, 55)]);
    }

    #[test]
    fn surge_cc_moves_real_param() {
        let Ok((voice, mut main)) = crate::host::ClapVoice::new(48000.0) else {
            return;
        };
        let params = main.param_list();
        let cutoff = params
            .iter()
            .find(|p| p.automatable && !p.hidden && !p.readonly && p.name.to_lowercase().contains("cutoff") && p.max > p.min)
            .expect("Surge XT exposes filter cutoff");
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![(surge, voice)]);
        bus.send(Command::MatrixCcMap {
            source: SourceId::Hardware(0),
            dest: surge,
            cc: 74,
            param: Some(cutoff.id),
        });
        let mut buf = vec![0.0f32; 256 * 2];
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 127 });
        for _ in 0..4 { engine.render(&mut buf, 2); }
        let at_max = main.param_value(cutoff.id).expect("cutoff readback");
        assert!((at_max - cutoff.max).abs() < 1e-3, "CC 127 should reach max {}: got {at_max}", cutoff.max);

        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 0 });
        for _ in 0..4 { engine.render(&mut buf, 2); }
        let at_min = main.param_value(cutoff.id).expect("cutoff readback");
        assert!((at_min - cutoff.min).abs() < 1e-3, "CC 0 should reach min {}: got {at_min}", cutoff.min);
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

/// Per-edge audio level self-test (PLAN §2 signature): the demo's chain
/// Keys→Surge→Delay→Master + Surge→Master. While a note sounds every audio
/// edge must publish a level above silence; after note-off (release + delay
/// tail) the levels must decay back to ~0.
pub fn edge_level_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, DspKind, Graph, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (voice, _main) = ClapVoice::new(sr)?;
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE"));
    let fx = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Audio);
    g.connect_with_id(3, PortRef { node: fx, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(4, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![(synth, voice)]);
    let levels = engine.edge_levels_handle();
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let mut buf = vec![0.0f32; 512 * 2];
    let mut lit = [0.0f32; 5];
    for _ in 0..40 {
        engine.render(&mut buf, 2);
        for e in 1..5u32 {
            lit[e as usize] = lit[e as usize].max(levels.load(e));
        }
    }
    anyhow::ensure!(lit[1] == 0.0, "MIDI edge must not carry an audio level ({})", lit[1]);
    for e in 2..5 {
        anyhow::ensure!(lit[e] > 1e-3, "audio edge {e} stayed silent while playing ({})", lit[e]);
    }
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    let mut secs = 0.0f32;
    let silent = |l: &EdgeLevels| (2..5u32).all(|e| l.load(e) < 1e-3);
    while !silent(&levels) && secs < 20.0 {
        engine.render(&mut buf, 2);
        secs += 512.0 / sr as f32;
    }
    anyhow::ensure!(silent(&levels), "edge levels did not decay after note-off ({:?})",
        (2..5u32).map(|e| levels.load(e)).collect::<Vec<_>>());
    println!(
        "edge-level-test: OK — playing: surge→delay {:.3}, delay→master {:.3}, surge→master {:.3}; silent {secs:.1}s after note-off",
        lit[2], lit[3], lit[4]
    );
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
    use crate::matrix::SourceId;
    let sr = 48000.0;
    // Real synths first; the LSP oscillator only proves routing.
    let candidates = ["urn:distrho:vitalium", "http://lsp-plug.in/plugins/lv2/oscillator_mono"];
    let uri = candidates
        .iter()
        .find(|u| crate::lv2::Lv2Plugin::load(u, sr, 512).map_or(false, |p| p.is_instrument()))
        .ok_or_else(|| anyhow::anyhow!("no LV2 instrument installed (tried {candidates:?})"))?;
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);

    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let inst = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name_bytes("LV2 INST"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: inst, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    bus.send(Command::LoadGraph { graph: g });

    let mut buf = vec![0.0f32; 512 * 2];
    let mut peak_over = |engine: &mut GraphEngine, blocks: usize| {
        let mut peak = 0.0f32;
        for _ in 0..blocks {
            engine.render(&mut buf, 2);
            peak = buf.iter().fold(peak, |m, s| m.max(s.abs()));
        }
        peak
    };
    let before = peak_over(&mut engine, 10);
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let held = peak_over(&mut engine, 47); // ~0.5 s
    let ev = engine.lv2_last_note(inst);
    anyhow::ensure!(matches!(ev, Some(e) if e[1] == 60), "LV2 instrument did not receive the note: {ev:?}");
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    peak_over(&mut engine, 375); // ~4 s release tail
    let after = peak_over(&mut engine, 94); // the following ~1 s
    println!("lv2-instrument-engine-test: {uri}: master peak silent={before:.4} held={held:.4} 4-5 s after release={after:.4}");
    anyhow::ensure!(held > 1e-3, "LV2 instrument {uri} is silent at the master while a key is held");
    anyhow::ensure!(after < held * 0.01, "LV2 instrument {uri} keeps sounding after note-off");
    println!("lv2-instrument-engine-test: OK — Keys → {uri} → Master is audible and releases");
    Ok(())
}

/// `lv2-async-test`: adding an LV2 synth must not stall the audio thread.
/// Adds Vitalium via AddNode with and without the loader thread, reports
/// the longest render block, then plays a note and checks it sounds.
pub fn lv2_async_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, NodeKind, PortKind, PortRef};
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let uri = "urn:distrho:vitalium";
    let mut worst = Vec::new();
    for with_loader in [false, true] {
        let (mut bus, drain) = command_bus();
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut engine = GraphEngine::new(g, drain, sr, vec![]);
        if with_loader {
            engine.spawn_lv2_loader();
        }
        let inst = keys.max(master) + 1;
        bus.send(Command::AddNode { kind: NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name: name_bytes("VITALIUM"), rig: 0 });
        bus.send(Command::Connect { id: 1, from: PortRef { node: keys, port: 0 }, to: PortRef { node: inst, port: 0 }, kind: PortKind::Midi });
        bus.send(Command::Connect { id: 2, from: PortRef { node: inst, port: 0 }, to: PortRef { node: master, port: 0 }, kind: PortKind::Audio });
        let mut buf = vec![0.0f32; 256 * 2];
        let (mut max_block, mut attached_after) = (std::time::Duration::ZERO, None);
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            let t = std::time::Instant::now();
            engine.render(&mut buf, 2);
            max_block = max_block.max(t.elapsed());
            if engine.lv2.iter().any(|s| s.node == inst) {
                attached_after = Some(start.elapsed());
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(5333)); // one 256-frame buffer
        }
        let attached_after = attached_after.ok_or_else(|| anyhow::anyhow!("{uri} never attached (with_loader={with_loader})"))?;
        bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
        let mut peak = 0.0f32;
        for _ in 0..94 {
            engine.render(&mut buf, 2);
            peak = buf.iter().fold(peak, |m, x| m.max(x.abs()));
        }
        println!("lv2-async-test: loader={with_loader}: longest render block {:.1} ms, attached after {:.0} ms, note peak {peak:.4}",
            max_block.as_secs_f64() * 1000.0, attached_after.as_secs_f64() * 1000.0);
        anyhow::ensure!(peak > 1e-3, "{uri} silent after attach (loader={with_loader})");
        worst.push(max_block);
    }
    anyhow::ensure!(worst[1] < std::time::Duration::from_millis(5), "render blocked {:?} with the loader", worst[1]);
    println!("lv2-async-test: OK — the loader keeps every render block under one buffer while {uri} loads");
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
    bus.send(Command::AddNode { kind: NodeKind::Dsp(DspKind::Gain), name: name_bytes("GAIN"), rig: 0 });
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

/// Per-rig mixer self-test: mute/solo/vol/pan applied at the rig masters.
pub fn rig_mix_test() -> anyhow::Result<()> {
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
    // Tag rig B's nodes (merge_rigs does this in real multi-rig loads).
    for n in &mut g.nodes {
        if n.id == sb || n.id == mb { n.rig = 1; }
    }
    let mut engine = GraphEngine::new(g, drain, sr, vec![(sa, va), (sb, vb)]);
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });

    let mut buf = vec![0.0f32; 512 * 2];
    let measure = |e: &mut GraphEngine, buf: &mut [f32]| -> (f32, f32) {
        let (mut pl, mut pr) = (0.0f32, 0.0f32);
        for _ in 0..30 {
            e.render(buf, 2);
            for f in buf.chunks(2) {
                pl = pl.max(f[0].abs());
                pr = pr.max(f[1].abs());
            }
        }
        (pl, pr)
    };
    let (both_l, both_r) = measure(&mut engine, &mut buf);
    anyhow::ensure!(both_l > 1e-3 && both_r > 1e-3, "two rigs produced no output");
    // Mute rig B → level drops.
    bus.send(Command::SetRigMix { rig: 1, vol: None, pan: None, mute: Some(true), solo: None });
    let (muted_l, _) = measure(&mut engine, &mut buf);
    anyhow::ensure!(both_l > muted_l * 1.2, "mute did not drop level (both {both_l} vs muted {muted_l})");
    // Solo rig A ≈ mute rig B.
    bus.send(Command::SetRigMix { rig: 1, vol: None, pan: None, mute: Some(false), solo: None });
    bus.send(Command::SetRigMix { rig: 0, vol: None, pan: None, mute: None, solo: Some(true) });
    let (solo_l, _) = measure(&mut engine, &mut buf);
    anyhow::ensure!((solo_l - muted_l).abs() < both_l * 0.15, "solo != mute-other (solo {solo_l} vs muted {muted_l})");
    bus.send(Command::SetRigMix { rig: 0, vol: None, pan: None, mute: None, solo: Some(false) });
    // Pan rig A hard left: rig A leaves the right channel, rig B stays centered.
    bus.send(Command::SetRigMix { rig: 0, vol: None, pan: Some(-1.0), mute: None, solo: None });
    let (pan_l, pan_r) = measure(&mut engine, &mut buf);
    anyhow::ensure!(pan_r < both_r * 0.95, "hard-left pan did not reduce right (both {both_r} vs pan {pan_r})");
    anyhow::ensure!(pan_r > 1e-3, "hard-left pan of rig A killed rig B's right ({pan_r})");
    anyhow::ensure!(pan_l > 1e-3, "hard-left pan killed left");
    // Volume 0 on both rigs → silence.
    bus.send(Command::SetRigMix { rig: 0, vol: Some(0.0), pan: Some(0.0), mute: None, solo: None });
    bus.send(Command::SetRigMix { rig: 1, vol: Some(0.0), pan: None, mute: None, solo: None });
    let (zero_l, zero_r) = measure(&mut engine, &mut buf);
    anyhow::ensure!(zero_l < 1e-4 && zero_r < 1e-4, "vol 0 not silent ({zero_l}, {zero_r})");
    println!("rig-mix-test: OK — mute/solo/vol/pan shape the rig sum (both {both_l:.4}, muted {muted_l:.4}, solo {solo_l:.4}, pan R {pan_r:.4})");
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

/// Real-plugin CC-mapping self-test: maps CC 74 on Hardware(0) to Surge XT's
/// Filter 1 Cutoff, sends CC 0 and CC 127, and verifies both parameter readback
/// and rendered audio difference.
pub fn cc_map_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    let sr = 48000.0;
    let (voice, mut main) = ClapVoice::new(sr)?;
    let params = main.param_list();
    let cutoff = params
        .iter()
        .find(|p| {
            p.automatable
                && !p.hidden
                && !p.readonly
                && p.max > p.min
                && p.module.to_lowercase().contains("filter")
                && p.name.to_lowercase().contains("cutoff")
        })
        .or_else(|| {
            params.iter().find(|p| {
                p.automatable && !p.hidden && !p.readonly && p.max > p.min && p.name.to_lowercase().contains("cutoff")
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Surge XT filter cutoff parameter not found"))?;
    let gain_p = params
        .iter()
        .find(|p| {
            p.automatable
                && !p.hidden
                && !p.readonly
                && p.max > p.min
                && p.name.to_lowercase().contains("volume")
        })
        .ok_or_else(|| anyhow::anyhow!("Surge XT volume parameter not found"))?;

    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let mut engine = GraphEngine::new(g, drain, sr, vec![(surge, voice)]);
    bus.send(Command::MatrixCcMap {
        source: SourceId::Hardware(0),
        dest: surge,
        cc: 74,
        param: Some(cutoff.id),
    });
    bus.send(Command::MatrixCcMap {
        source: SourceId::Hardware(0),
        dest: surge,
        cc: 7,
        param: Some(gain_p.id),
    });
    let mut buf = vec![0.0f32; 256 * 2];
    let mut render_note = |engine: &mut GraphEngine, cc: u8, val: u8| -> Vec<f32> {
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc, value: val });
        for _ in 0..4 { engine.render(&mut buf, 2); }
        bus.send(Command::NoteOn { note: 48, velocity: 0.9, source: SourceId::Hardware(0) });
        let mut out = Vec::with_capacity(256 * 2 * 16);
        for _ in 0..16 {
            engine.render(&mut buf, 2);
            out.extend_from_slice(&buf);
        }
        bus.send(Command::NoteOff { note: 48, source: SourceId::Hardware(0) });
        for _ in 0..8 { engine.render(&mut buf, 2); }
        out
    };
    let rms = |s: &[f32]| -> f32 {
        (s.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / s.len().max(1) as f64).sqrt() as f32
    };

    // 1. Cutoff param readback check (CC 74).
    let _ = render_note(&mut engine, 74, 0);
    let val_low = main.param_value(cutoff.id).unwrap_or(f64::NAN);
    let _ = render_note(&mut engine, 74, 127);
    let val_high = main.param_value(cutoff.id).unwrap_or(f64::NAN);
    anyhow::ensure!(
        (val_low - cutoff.min).abs() < 1e-3 && (val_high - cutoff.max).abs() < 1e-3,
        "CC did not sweep {}/{} ({}..{}): low={val_low}, high={val_high}",
        cutoff.module, cutoff.name, cutoff.min, cutoff.max
    );

    // 2. Volume param readback + audible RMS check (CC 7).
    let quiet = render_note(&mut engine, 7, 0);
    let g_low = main.param_value(gain_p.id).unwrap_or(f64::NAN);
    let loud = render_note(&mut engine, 7, 127);
    let g_high = main.param_value(gain_p.id).unwrap_or(f64::NAN);
    let rms_low = rms(&quiet);
    let rms_high = rms(&loud);
    anyhow::ensure!(
        (g_low - gain_p.min).abs() < 1e-3 && (g_high - gain_p.max).abs() < 1e-3,
        "CC did not sweep {}/{} ({}..{}): low={g_low}, high={g_high}",
        gain_p.module, gain_p.name, gain_p.min, gain_p.max
    );
    anyhow::ensure!(
        rms_high > rms_low * 1.5 + 1e-4,
        "Volume CC sweep did not change audio energy: low_rms={rms_low}, high_rms={rms_high}"
    );
    println!(
        "cc-map-test: OK — Surge XT '{}/{}' (id {}) CC74 0→127 moved {:.2}→{:.2}; '{}/{}' (id {}) CC7 0→127 moved {:.2}→{:.2}, RMS {:.4}→{:.4}",
        cutoff.module, cutoff.name, cutoff.id, val_low, val_high,
        gain_p.module, gain_p.name, gain_p.id, g_low, g_high, rms_low, rms_high
    );
    Ok(())
}
