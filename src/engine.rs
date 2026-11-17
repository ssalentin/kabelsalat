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
    name_str, split_port, AudioInChannel, Command, CommandDrain, DspKind, Graph, NodeId, NodeKind,
    ParamId, PortKind, RigSnapshot,
};
use crate::dsp::{build_processor, Processor};
use crate::host::ClapVoice;
use crate::matrix::{CcMapping, CcTarget, Matrix, MidiMsg, Rule, SourceId};
use crate::capture::SystemCapture;
use crate::meter::{MasterMeter, MeterSnapshot};
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
/// Audio In gain (linear 0..=4; the face knob is 0..1 mapped ×4). Sent via
/// SetParam; distinct node kind so no collision with plugin param space.
pub const P_AUDIO_IN_GAIN: ParamId = 64;
/// Audio In monitor toggle (0 = off, ≥ 0.5 = on): adds the input (post-gain)
/// straight to the master so the performer hears it before wiring it.
pub const P_AUDIO_IN_MONITOR: ParamId = 65;

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
        DspKind::Limiter => &[(0, "Ceiling"), (1, "Drive")],
        DspKind::Saturation => &[(0, "Drive"), (1, "Mix")],
        DspKind::Pan => &[(0, "Pan")],
        DspKind::Metronome => &[],
    }
}

/// System-audio capture gain (0..=1, the same value `SetSystemAudio` sets),
/// as a SetParam on a SystemAudio node so its GAIN knob and CC bindings work.
pub const P_SYSTEM_GAIN: ParamId = 66;

/// One rack-unit knob: the label on the face (canonical for agents too),
/// its default, the engine param it drives (None = local-only), and the long
/// names agents may use instead of the label.
pub struct KnobSpec {
    pub label: &'static str,
    pub default: f32,
    pub param: Option<ParamId>,
    /// Not drawn as a knob (the face renders it as a chip); still bindable.
    pub hidden: bool,
    pub aliases: &'static [&'static str],
}

const fn ks(label: &'static str, default: f32, param: Option<ParamId>, aliases: &'static [&'static str]) -> KnobSpec {
    KnobSpec { label, default, param, hidden: false, aliases }
}

/// A `'static` knob table: `const` items are promoted, `const fn` calls are not.
macro_rules! knobs {
    ($($k:expr),* $(,)?) => {{
        const K: &[KnobSpec] = &[$($k),*];
        K
    }};
}

/// The knobs a rack unit shows for a node kind — the ONE table both the UI
/// (`make_unit`, `resolve_unit_knob`) and headless MCP resolve knob names
/// against, so `knob="RESO"` means the same knob everywhere.
pub fn knob_specs(kind: &NodeKind) -> &'static [KnobSpec] {
    match kind {
        NodeKind::Keys => knobs!(ks("RATE", 0.5, None, &[]), ks("VEL", 0.8, None, &["VELOCITY"])),
        // The plugin's main controls join LEVEL on the face (`main_controls`).
        NodeKind::Clap { .. } => knobs!(ks("LEVEL", 0.85, Some(P_LEVEL), &[])),
        NodeKind::MasterOut => knobs!(ks("VOLUME", 1.0, Some(P_VOL), &["VOL"]), ks("WARM", 0.15, Some(P_WARM), &["WARMTH"])),
        NodeKind::Dsp(dk) => match dk {
            DspKind::Delay => knobs!(ks("TIME", 0.4, Some(P_TIME), &[]), ks("MIX", 0.5, Some(P_MIX), &[])),
            DspKind::Reverb => knobs!(ks("MIX", 0.35, Some(0), &[]), ks("SIZE", 0.5, Some(1), &[])),
            DspKind::Gain => knobs!(ks("GAIN", 0.9, Some(0), &[])),
            DspKind::Filter => knobs!(ks("CUTOFF", 0.45, Some(0), &[]), ks("RESO", 0.2, Some(1), &["RESONANCE"])),
            DspKind::Eq3 => knobs!(ks("LOW", 0.5, Some(0), &[]), ks("HIGH", 0.5, Some(1), &[])),
            DspKind::Limiter => knobs!(ks("CEIL", 0.8, Some(0), &["CEILING"]), ks("DRIVE", 0.2, Some(1), &[])),
            DspKind::Saturation => knobs!(ks("DRIVE", 0.4, Some(0), &[]), ks("MIX", 0.5, Some(1), &[])),
            DspKind::Pan => knobs!(ks("PAN", 0.5, Some(0), &[])),
            DspKind::Metronome => &[],
        },
        // An LV2 unit's knobs are its plugin's main controls (`main_controls`):
        // param 0 is whatever port the plugin lists first, never a "gain".
        NodeKind::Lv2 { .. } => &[],
        // Starts at 0 like the engine's system_gain: a fresh unit stays muted.
        NodeKind::SystemAudio => knobs!(ks("GAIN", 0.0, Some(P_SYSTEM_GAIN), &[])),
        NodeKind::AudioIn { .. } => knobs!(
            ks("GAIN", 0.5, Some(P_AUDIO_IN_GAIN), &[]),
            KnobSpec { label: "MON", default: 0.0, param: Some(P_AUDIO_IN_MONITOR), hidden: true, aliases: &["MONITOR"] },
        ),
    }
}

/// Resolve a knob name (label or alias, any case) on a node kind.
pub fn find_knob_spec(kind: &NodeKind, name: &str) -> Option<&'static KnobSpec> {
    let q = name.trim();
    knob_specs(kind)
        .iter()
        .find(|k| k.label.eq_ignore_ascii_case(q) || k.aliases.iter().any(|a| a.eq_ignore_ascii_case(q)))
}

/// A hosted CLAP voice inside the engine: the audio half of the plugin plus
/// the per-instrument state of the Clap node it plays. Built off the audio
/// thread (it owns its render buffer) and handed over with [`VoiceMsg::Add`].
pub struct VoiceSlot {
    /// The Clap node this voice plays; `NodeId::MAX` = free.
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

impl VoiceSlot {
    pub fn new(voice: ClapVoice) -> Self {
        Self {
            node: NodeId::MAX,
            voice,
            level: 0.85,
            scratch: vec![0.0; 8192 * 2],
            src_keys: true,
            src_hw: true,
            rig_device: None,
        }
    }

    pub fn voice_id(&self) -> u64 {
        self.voice.voice_id()
    }

    /// The audio half, for `ClapVoiceMain::retire` on the plugin's main thread.
    pub fn into_voice(self) -> ClapVoice {
        self.voice
    }

    /// Play `node` (or go free with `NodeId::MAX`). Level and input switches
    /// belong to the node, so they start over: a voice that kept
    /// "keyboard off" from its last node was deaf on the next one.
    fn bind(&mut self, node: NodeId) {
        self.node = node;
        self.level = 0.85;
        self.src_keys = true;
        self.src_hw = true;
        self.rig_device = None;
    }
}

/// Voice host → audio thread (`set_voice_channels`). Drained at block
/// boundaries and before every structural command, so what the host sent
/// before a load is in place when the load binds voices.
pub enum VoiceMsg {
    Add(VoiceSlot),
    /// An empty table with more capacity than the engine's: the host sends
    /// it before the `Add` that would not fit, so the audio thread never
    /// allocates to hold another voice.
    Grow(Vec<VoiceSlot>),
    /// Hand the voice with this id back, whatever it plays.
    Retire(u64),
}

/// Audio thread → voice host: everything the host has to free.
pub enum VoiceBack {
    Voice(VoiceSlot),
    Table(Vec<VoiceSlot>),
}

/// Voices the engine holds before the host has to send a larger table.
pub const VOICE_TABLE_CAP: usize = 32;

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

/// One `AudioIn` node's render-side state. The ring `consumer` is filled by the
/// input stream's callback thread (stereo-interleaved) and popped here at block
/// boundaries — allocation- and lock-free (rule 29/46). Ownership of the live
/// capture (cpal stream) lives off the audio thread; only this consumer crosses.
pub struct AudioInSlot {
    node: NodeId,
    channels: AudioInChannel,
    device: Option<String>,
    /// Read end of the engine ring (None until the UI opens a capture).
    consumer: Option<rtrb::Consumer<f32>>,
    /// Stereo-interleaved output buffer for this node this block.
    raw: Vec<f32>,
    /// Post-gain stereo output (what the OUT jack + monitor carry).
    buf: Vec<f32>,
    gain: f32,
    monitor: bool,
    /// Last raw sample (threaded through drops/inserts to avoid a click).
    last: [f32; 2],
    /// Drift reconciler (input vs output clock).
    drift: crate::audio_in::Drift,
    /// Counters for the UI status readout (test observability too).
    dropped: u64,
    starved: u64,
}

impl AudioInSlot {
    fn new(node: NodeId, channels: AudioInChannel, device: Option<String>) -> Self {
        let block = crate::bridge::BUFFER_FRAMES as usize * 2;
        Self {
            node,
            channels,
            device,
            consumer: None,
            raw: vec![0.0; 8192 * 2],
            buf: vec![0.0; 8192 * 2],
            gain: 1.0,
            monitor: false,
            last: [0.0; 2],
            drift: crate::audio_in::Drift::new(block.max(2)),
            dropped: 0,
            starved: 0,
        }
    }
}

/// Lock-free per-AudioIn-node input level (pre-gain), engine → UI (rule 33).
/// Same slot/tag scheme as [`EdgeLevels`], keyed by node id.
pub struct AudioInLevels {
    slots: [AtomicU64; 64],
}

impl AudioInLevels {
    pub fn new() -> Self {
        Self { slots: std::array::from_fn(|_| AtomicU64::new(0)) }
    }
    pub fn store(&self, node: u32, peak: f32) {
        let packed = ((node as u64) << 32) | peak.max(0.0).to_bits() as u64;
        self.slots[node as usize % 64].store(packed, Ordering::Relaxed);
    }
    pub fn load(&self, node: u32) -> f32 {
        let v = self.slots[node as usize % 64].load(Ordering::Relaxed);
        if (v >> 32) as u32 == node { f32::from_bits(v as u32) } else { 0.0 }
    }
}

impl Default for AudioInLevels {
    fn default() -> Self {
        Self::new()
    }
}

/// Build an AudioIn slot for every AudioIn node in the graph (rare structural
/// op, allocates — the slots preallocate their buffers once).
fn build_audio_in_slots(graph: &Graph) -> Vec<AudioInSlot> {
    graph
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::AudioIn { device, channels } => {
                Some(AudioInSlot::new(n.id, *channels, device.clone()))
            }
            _ => None,
        })
        .collect()
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
                crate::lv2::retire(slot.plugin); // parks never-free plugins (#14)
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

/// Per-edge audio levels, engine → UI (docs/ARCHITECTURE.md §2: cables breathe with live
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
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    pub record_sync: bool,
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
            record_sync: false,
        }
    }
}

/// Shared graph publication for frontends (MCP→UI live sync).
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
    /// System-audio capture gain, published with the mix (same `mix_rev`).
    pub system_gain: f32,
    pub mix_rev: u64,
    /// Every hosted voice and the Clap node it plays (`NodeId::MAX` = free),
    /// bumped by `voices_rev`. The engine binds voices; frontends that hold
    /// the plugins' main halves read which one serves which node here.
    pub voices: Vec<(u64, NodeId)>,
    pub voices_rev: u64,
    /// Master transport, tempo, and count-in state, bumped by `transport_rev`.
    pub transport: TransportPub,
    pub transport_rev: u64,
    /// Master-bus loudness/peak/band meter (Bravo 1 "measure loop"), bumped by
    /// `meter_rev` every render block via a non-blocking `try_lock` (rule 33).
    pub meter: MeterSnapshot,
    pub meter_rev: u64,
}

fn write_voices(g: &mut GraphPub, voices: &[VoiceSlot]) {
    g.voices.clear();
    g.voices.extend(voices.iter().map(|v| (v.voice.voice_id(), v.node)));
    g.voices_rev = g.voices_rev.wrapping_add(1);
}

impl GraphPub {
    /// Make room for `n` published voices. Called off the audio thread by
    /// whoever raises the engine's voice capacity.
    pub fn reserve_voices(&mut self, n: usize) {
        self.voices.reserve(n.saturating_sub(self.voices.len()));
    }
}

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    sample_rate: f64,
    voices: Vec<VoiceSlot>,
    dsp: Vec<DspSlot>,
    lv2: Vec<Lv2Slot>,
    lv2_in: Vec<f32>,
    /// Per-block scratch: which effects already ran (render step 2b).
    fx_done: Vec<bool>,
    master_vol: f32,
    master_warm: f32,
    /// Master bus limiter (zero latency) in front of the warmth stage: many
    /// voices at once drove `tanh` into hard, buzzing saturation.
    master_lim: MasterLimiter,
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
    // Audio In nodes (live mic/guitar/interface): one slot per node, rings fed
    // by input callbacks off the audio thread (rule 46).
    audio_in: Vec<AudioInSlot>,
    audio_in_levels: Arc<AudioInLevels>,
    /// UI → audio: delivers a freshly-opened input ring's consumer to the slot.
    audio_in_rx: Option<rtrb::Consumer<(NodeId, rtrb::Consumer<f32>)>>,
    // Runtime CLAP voice delivery + retirement side channels (host <-> audio).
    voice_rx: Option<rtrb::Consumer<VoiceMsg>>,
    voice_back_tx: Option<rtrb::Producer<VoiceBack>>,
    /// Returns waiting while the host's ring is full (preallocated: dropping
    /// a voice or a table here would free memory on the audio thread).
    voice_backlog: Vec<VoiceBack>,
    /// The voice table changed since its last publication (`GraphPub::voices`).
    voices_dirty: bool,
    /// LV2 loader thread link (`spawn_lv2_loader`); None loads inline
    /// (tests, offline self-tests).
    lv2_req: Option<rtrb::Producer<Lv2Request>>,
    lv2_resp: Option<rtrb::Consumer<Lv2Response>>,
    /// Bumped by every graph load: a plugin the loader finishes for an
    /// older graph is retired, not attached.
    lv2_gen: u64,
    /// Slots waiting to be retired while the loader's queue is full
    /// (preallocated: freeing one here would stall the audio thread).
    lv2_retire_backlog: Vec<Lv2Slot>,
    // Master recording tap (the final output). None = recording unavailable
    // (headless tests, silent demo). The tap's ring producer is pushed from
    // the render path; its `recording` flag is the authority on start/stop.
    recorder: Option<RecorderTap>,
    // "Sync to transport": start recording on master Play, stop on Stop.
    record_sync: bool,
    // A synced take is armed but not started until the song clock reaches
    // beat 0 (after any count-in); see render().
    record_sync_pending: bool,
    // Master-bus meter (Bravo 1): fed the final stereo output every block.
    meter: MasterMeter,
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
        let mut voices: Vec<VoiceSlot> = Vec::with_capacity(VOICE_TABLE_CAP.max(voices_in.len()));
        voices.extend(voices_in.into_iter().map(|(node, voice)| {
            let mut slot = VoiceSlot::new(voice);
            slot.node = node;
            slot
        }));
        let dsp = graph
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Dsp(kind) => Some(dsp_slot(n, kind, sample_rate)),
                _ => None,
            })
            .collect();
        let lv2 = build_lv2_slots(&graph, sample_rate);
        let audio_in = build_audio_in_slots(&graph);
        // Default routing: every source class feeds every rig (full range).
        let mut matrix = Matrix::new();
        matrix.reserve(64);
        // Routes belong to the Clap node, not to the voice that plays it: a
        // voice hosted after startup finds its node already routed.
        for n in graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Clap { .. })) {
            matrix.connect(SourceId::Keys, n.id);
            // Cover hardware device ids 0..MAX so any connected device routes;
            // the per-rig `rig_device` filter then selects which one is heard.
            for dev in 0..MAX_HW_DEVICES {
                matrix.connect(SourceId::Hardware(dev), n.id);
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
        let route_cap = matrix.rules.len().max(64);
        Self {
            graph,
            drain,
            sample_rate,
            voices,
            dsp,
            lv2,
            lv2_in: vec![0.0; 8192 * 2],
            fx_done: Vec::with_capacity(256),
            master_vol: 1.0,
            master_warm: 0.15,
            master_lim: MasterLimiter::new(sample_rate),
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
            route_out: Vec::with_capacity(route_cap),
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
            audio_in,
            audio_in_levels: Arc::new(AudioInLevels::new()),
            audio_in_rx: None,
            voice_rx: None,
            voice_back_tx: None,
            voice_backlog: Vec::with_capacity(64),
            voices_dirty: true,
            lv2_req: None,
            lv2_resp: None,
            lv2_gen: 0,
            lv2_retire_backlog: Vec::new(),
            recorder: None,
            record_sync: false,
            record_sync_pending: false,
            meter: MasterMeter::new(sample_rate),
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
        self.lv2_retire_backlog.reserve(LV2_SLOT_CAP);
    }

    /// Load `uri` for `node`: on the loader thread when there is one.
    fn request_lv2(&mut self, node: NodeId, uri: crate::control::Lv2Uri) {
        if let Some(tx) = self.lv2_req.as_mut() {
            if tx.push(Lv2Request::Load { node, uri, gen: self.lv2_gen }).is_ok() {
                return;
            }
            // Queue full (dozens of LV2 nodes at once): one stall beats a
            // silently dead node.
            eprintln!("lv2 loader queue full: loading node {node} inline");
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
                if self.lv2_retire_backlog.len() < self.lv2_retire_backlog.capacity() {
                    self.lv2_retire_backlog.push(slot); // handed over on a later block
                } else {
                    drop(slot); // last resort: never leak
                }
            }
        }
    }

    /// Adopt what the loader finished (block boundary, allocation-free).
    fn drain_lv2_loader(&mut self) {
        while let Some(slot) = self.lv2_retire_backlog.pop() {
            let Some(tx) = self.lv2_req.as_mut() else { break };
            if let Err(rtrb::PushError::Full(Lv2Request::Retire(slot))) = tx.push(Lv2Request::Retire(slot)) {
                self.lv2_retire_backlog.push(slot);
                break;
            }
        }
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
    pub fn set_voice_channels(&mut self, rx: rtrb::Consumer<VoiceMsg>, back_tx: rtrb::Producer<VoiceBack>) {
        self.voice_rx = Some(rx);
        self.voice_back_tx = Some(back_tx);
    }

    /// Test hook: drop the table's spare capacity, so a test reaches the
    /// `VoiceMsg::Grow` path with a handful of voices. Returns the capacity.
    #[cfg(test)]
    pub(crate) fn shrink_voice_table(&mut self) -> usize {
        self.voices.shrink_to_fit();
        self.voices.capacity()
    }

    #[cfg(test)]
    pub(crate) fn voice_table_cap(&self) -> usize {
        self.voices.capacity()
    }

    /// Give every voiceless Clap node, in graph order, the free voice of its
    /// plugin with the lowest id (`control::bind_voices` is the same rule for
    /// a host that has to know the outcome before the engine answers).
    /// Allocation-free.
    fn bind_free_voices(&mut self) {
        for n in &self.graph.nodes {
            let NodeKind::Clap { plugin_id } = &n.kind else { continue };
            if self.voices.iter().any(|v| v.node == n.id) {
                continue;
            }
            let key = crate::control::clap_plugin_key(name_str(plugin_id));
            let free = self
                .voices
                .iter_mut()
                .filter(|v| v.node == NodeId::MAX && v.voice.plugin_id() == key)
                .min_by_key(|v| v.voice.voice_id());
            if let Some(v) = free {
                v.bind(n.id);
            }
        }
        self.voices_dirty = true;
    }

    /// One note can reach every instrument: keep the routing scratch large
    /// enough for all rules, so note routing never allocates (structural
    /// ops only).
    fn fit_route_out(&mut self) {
        let need = self.matrix.rules.len();
        if self.route_out.capacity() < need {
            self.route_out.reserve(need - self.route_out.len());
        }
    }

    fn send_back(&mut self, item: VoiceBack) {
        let item = match self.voice_back_tx.as_mut() {
            Some(tx) => match tx.push(item) {
                Ok(()) => return,
                Err(rtrb::PushError::Full(item)) => item,
            },
            None => item,
        };
        if self.voice_backlog.len() < self.voice_backlog.capacity() {
            self.voice_backlog.push(item);
        } else {
            // Nobody takes returns and the backlog is full: a leak is the
            // only way left that does not free on the audio thread.
            std::mem::forget(item);
        }
    }

    /// Apply what the voice host sent: new voices, a larger table, voices
    /// to hand back. Allocation-free (the host allocates, the host frees).
    fn drain_voice_msgs(&mut self) {
        while let Some(item) = self.voice_backlog.pop() {
            let Some(tx) = self.voice_back_tx.as_mut() else {
                self.voice_backlog.push(item);
                break;
            };
            if let Err(rtrb::PushError::Full(item)) = tx.push(item) {
                self.voice_backlog.push(item);
                break;
            }
        }
        let mut changed = false;
        loop {
            let Some(rx) = self.voice_rx.as_mut() else { break };
            let Ok(msg) = rx.pop() else { break };
            match msg {
                VoiceMsg::Add(slot) => {
                    if self.voices.len() < self.voices.capacity() {
                        self.voices.push(slot);
                        changed = true;
                    } else {
                        self.send_back(VoiceBack::Voice(slot));
                    }
                }
                VoiceMsg::Grow(mut table) => {
                    if table.capacity() > self.voices.capacity() {
                        table.extend(self.voices.drain(..));
                        std::mem::swap(&mut self.voices, &mut table);
                    }
                    self.send_back(VoiceBack::Table(table));
                }
                VoiceMsg::Retire(id) => {
                    if let Some(i) = self.voices.iter().position(|v| v.voice.voice_id() == id) {
                        let slot = self.voices.swap_remove(i);
                        self.send_back(VoiceBack::Voice(slot));
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.bind_free_voices();
        }
    }

    /// Publish the voice table when it changed outside a graph publication:
    /// a non-blocking lock, retried next block (rule 33).
    fn publish_voices(&mut self) {
        if !self.voices_dirty {
            return;
        }
        let Some(p) = &self.graph_pub else {
            self.voices_dirty = false;
            return;
        };
        if let Ok(mut g) = p.try_lock() {
            write_voices(&mut g, &self.voices);
            self.voices_dirty = false;
        }
    }

    /// Attach a system-audio capture source (started off the audio thread).
    pub fn set_system_capture(&mut self, cap: SystemCapture) {
        self.system_capture = Some(cap);
    }

    /// Attach the UI→audio ring that delivers freshly-opened input captures
    /// (a `(node, consumer)` pair per opened AudioIn node).
    pub fn set_audio_in_channel(&mut self, rx: rtrb::Consumer<(NodeId, rtrb::Consumer<f32>)>) {
        self.audio_in_rx = Some(rx);
    }

    /// Lock-free per-node input level (pre-gain peak) for the face meter/tuner.
    pub fn audio_in_levels_handle(&self) -> Arc<AudioInLevels> {
        self.audio_in_levels.clone()
    }

    /// Live input state for the status readout: (dropped, starved) counters.
    #[allow(dead_code)] // query API; the UI reads `audio_in_levels`/get_status instead
    pub fn audio_in_status(&self, node: NodeId) -> Option<(u64, u64)> {
        self.audio_in.iter().find(|s| s.node == node).map(|s| (s.dropped, s.starved))
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
        if let Some(rec) = &mut self.recorder {
            rec.begin();
        }
    }

    /// Stop recording (idempotent). No-op without a tap.
    fn record_stop(&mut self) {
        if let Some(rec) = &mut self.recorder {
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
            record_sync: self.record_sync,
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
        // Room for the voice list now, so publishing it never allocates on
        // the audio thread (the host reserves again when it sends `Grow`).
        if let Ok(mut g) = p.lock() {
            g.reserve_voices(self.voices.capacity());
        }
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
                g.system_gain = self.system_gain;
                g.mix_rev = g.mix_rev.wrapping_add(1);
                g.transport = tp;
                g.transport_rev = g.transport_rev.wrapping_add(1);
                write_voices(&mut g, &self.voices);
                self.voices_dirty = false;
            }
        }
    }

    /// Publish the master meter snapshot (Bravo 1). Called from the render
    /// path every block: a fixed-size Copy under a NON-blocking `try_lock`
    /// (never waits on the UI; if busy, the fresh snapshot goes out next
    /// block — rule 33). Snapshot is computed only when a publication handle
    /// is attached and the lock succeeds.
    fn publish_meter(&mut self) {
        let Some(p) = &self.graph_pub else { return };
        if let Ok(mut g) = p.try_lock() {
            g.meter = self.meter.snapshot();
            g.meter_rev = g.meter_rev.wrapping_add(1);
        }
    }

    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.out_peak.clone()
    }

    /// The master-bus loudness/peak/band snapshot (Bravo 1), for queries and
    /// the `meter-test` self-test. Computes on demand from the meter's state.
    pub fn meter_snapshot(&self) -> MeterSnapshot {
        self.meter.snapshot()
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
            } else if let Some(a) = self.audio_in.iter().find(|a| a.node == src) {
                block_peak(&a.buf[..n2])
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

    /// The next effect to run: the first not-yet-run one whose wired effect
    /// inputs have all run; in a feedback loop, the first not-yet-run one.
    /// Allocation-free (linear scans over a handful of nodes).
    fn next_effect(&self, n_dsp: usize, n_fx: usize) -> Option<usize> {
        let node = |f: usize| if f < n_dsp { self.dsp[f].node } else { self.lv2[f - n_dsp].node };
        let mut first = None;
        for f in 0..n_fx {
            if self.fx_done[f] { continue; }
            first.get_or_insert(f);
            let waits = (0..n_fx).any(|g| {
                g != f && !self.fx_done[g] && self.graph.has_route(node(g), node(f), PortKind::Audio)
            });
            if !waits { return Some(f); }
        }
        first
    }

    /// Sum every audio source wired into `dest` into `out` (stereo
    /// interleaved). Allocation-free.
    fn sum_audio_into(&self, dest: NodeId, out: &mut [f32]) {
        out.fill(0.0);
        let mut add = |src: NodeId, buf: &[f32]| {
            if src != dest && self.graph.has_route(src, dest, PortKind::Audio) {
                for (o, x) in out.iter_mut().zip(buf) { *o += *x; }
            }
        };
        for v in &self.voices { if v.node != NodeId::MAX { add(v.node, &v.scratch); } }
        for a in &self.audio_in { add(a.node, &a.buf); }
        for d in &self.dsp { add(d.node, &d.buf); }
        for l in &self.lv2 { add(l.node, &l.buf); }
    }

    /// Read back the MIDI event pending at an LV2 instrument node (verification).
    pub fn lv2_last_note(&self, node: NodeId) -> Option<[u8; 3]> {
        self.lv2.iter().find(|s| s.node == node).and_then(|s| s.plugin.last_midi_event())
    }

    /// Current plain value of an LV2 node's nth control input (verification).
    pub fn lv2_control_value(&self, node: NodeId, nth: usize) -> Option<f32> {
        let slot = self.lv2.iter().find(|s| s.node == node)?;
        slot.plugin.control_descs().get(nth).map(|c| c.value)
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

    /// One setter for the system-audio gain (SetSystemAudio and the GAIN knob
    /// alike); the mixer publication carries it back to the UI (rule 33).
    fn set_system_gain(&mut self, gain: f32) {
        let g = gain.clamp(0.0, 1.0);
        if g != self.system_gain {
            self.system_gain = g;
            self.mix_dirty = true;
        }
    }

    /// Apply an engine-level knob/parameter change (shared by `Command::SetParam`
    /// and `CcTarget::Knob` CC bindings). Allocation-free.
    fn apply_engine_param(&mut self, node: NodeId, param: ParamId, value: f64) -> bool {
        let v = value as f32;
        if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
            // LV2 knobs map param index → nth control port (normalized).
            s.plugin.set_control_normalized(param as usize, v);
            true
        } else if param == P_LEVEL && self.voices.iter().any(|s| s.node == node) {
            if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                s.level = v;
                return true;
            }
            false
        } else if param == P_SYSTEM_GAIN
            && self.graph.node(node).is_some_and(|n| matches!(n.kind, NodeKind::SystemAudio))
        {
            self.set_system_gain(v);
            true
        } else if self.audio_in.iter().any(|s| s.node == node) {
            let Some(s) = self.audio_in.iter_mut().find(|s| s.node == node) else {
                return false;
            };
            match param {
                P_AUDIO_IN_GAIN => {
                    s.gain = v.clamp(0.0, 4.0);
                    true
                }
                P_AUDIO_IN_MONITOR => {
                    s.monitor = v >= 0.5;
                    true
                }
                _ => false,
            }
        } else {
            self.apply_dsp_or_master_param(node, param, v)
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
        self.keys_route(dest, played).is_some()
    }

    /// The Keys zone a played note reaches `dest` through: `Some(semitones)`
    /// of that zone's octave shift, `None` if no Keys cable carries the note
    /// there. Note-on and note-off take the same answer, so a release always
    /// meets the pitch its note-on started (rule 41). Allocation-free.
    fn keys_route(&self, dest: NodeId, played: u8) -> Option<i16> {
        self.graph.nodes.iter().find_map(|n| {
            if !matches!(n.kind, NodeKind::Keys) { return None; }
            let port = split_port(n.split, played);
            self.graph.edges.iter().any(|e| {
                e.kind == PortKind::Midi
                    && e.from.node == n.id
                    && e.from.port == port
                    && e.to.node == dest
            }).then(|| crate::control::Graph::zone_shift(n, played))
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
        if matches!(cmd, Command::AddNode { .. } | Command::LoadGraph { .. } | Command::LoadRigs { .. }) {
            self.drain_voice_msgs();
        }
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
                    NodeKind::AudioIn { device, channels } => {
                        self.audio_in.push(AudioInSlot::new(id, *channels, device.clone()));
                    }
                    NodeKind::Clap { .. } => {
                        self.matrix.connect(SourceId::Keys, id);
                        for dev in 0..MAX_HW_DEVICES {
                            self.matrix.connect(SourceId::Hardware(dev), id);
                        }
                        self.fit_route_out();
                        self.bind_free_voices();
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
                self.audio_in.retain(|s| s.node != id);
                if let Some(i) = self.lv2.iter().position(|s| s.node == id) {
                    let slot = self.lv2.remove(i);
                    self.retire_lv2(slot);
                }
                for s in &mut self.voices {
                    if s.node == id {
                        s.bind(NodeId::MAX);
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
                self.apply_engine_param(node, param, value);
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
            Command::ProgramChange { node, program } => {
                let program = program.min(127);
                if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                    s.plugin.program_change(program);
                } else if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                    s.voice.program_change_rt(program);
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
                self.audio_in = build_audio_in_slots(&self.graph);
                // Old instances are freed (on the loader thread when it runs);
                // new ones load below, after the matrix is rebuilt.
                while let Some(slot) = self.lv2.pop() {
                    self.retire_lv2(slot);
                }
                self.lv2_gen = self.lv2_gen.wrapping_add(1);
                // The hosted voices move over to the new graph's Clap nodes;
                // voices it has no node for stay free.
                for v in &mut self.voices {
                    v.bind(NodeId::MAX);
                }
                self.bind_free_voices();
                let clap_ids: Vec<NodeId> = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::Clap { .. }))
                    .map(|n| n.id)
                    .collect();
                self.matrix = Matrix::new();
                self.matrix.reserve(64);
                for &id in &clap_ids {
                    self.matrix.connect(SourceId::Keys, id);
                    for dev in 0..MAX_HW_DEVICES {
                        self.matrix.connect(SourceId::Hardware(dev), id);
                    }
                }
                self.fit_route_out();
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
                    self.record_sync_pending = false;
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
                    let Some(shift) = self.keys_route(dest, note) else { continue };
                    let Some(out) = shifted(m.note(), shift) else { continue };
                    #[cfg(test)]
                    self.note_log.push((dest, out, true));
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if source_accepted(s, source) {
                            s.voice.note_on(out as u16, velocity as f64);
                        }
                    }
                    if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        if s.plugin.is_instrument() {
                            s.plugin.note_on(out, (velocity.clamp(0.0, 1.0) * 127.0) as u8);
                        }
                    }
                }
            }
            Command::NoteOff { note, source } => {
                let msg = MidiMsg::note_off(0, note, 0);
                self.matrix.route_into(source, msg, &mut self.route_out);
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let Some(shift) = self.keys_route(dest, note) else { continue };
                    let Some(out) = shifted(m.note(), shift) else { continue };
                    #[cfg(test)]
                    self.note_log.push((dest, out, false));
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if source_accepted(s, source) {
                            s.voice.note_off(out as u16);
                        }
                    }
                    if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        if s.plugin.is_instrument() {
                            s.plugin.note_off(out);
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
            Command::RenameRig { rig, name } => {
                // Structural metadata (block boundary; never the render path).
                if let Some(m) = self.rig_meta.get_mut(rig as usize) {
                    let n = crate::control::name_str(&name).trim().to_string();
                    if !n.is_empty() {
                        m.name = n;
                        self.publish_rig_snapshot();
                    }
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
            Command::SetZoneOctave { node, zone, octave } => {
                // Same safety as a split move: a held note must be released
                // at the pitch it started, so release first (rule 41).
                let o = octave.clamp(-crate::control::ZONE_OCTAVE_MAX, crate::control::ZONE_OCTAVE_MAX);
                let is_change = zone <= 1 && self.graph.node(node).is_some_and(|n| {
                    matches!(n.kind, NodeKind::Keys) && n.split.is_some() && n.zone_octave[zone as usize] != o
                });
                if is_change {
                    for i in 0..self.graph.edges.len() {
                        let e = &self.graph.edges[i];
                        if e.kind == PortKind::Midi && e.from.node == node {
                            let dest = e.to.node;
                            self.release_all(dest);
                        }
                    }
                    self.graph.set_zone_octave(node, zone, octave);
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
                    let mapping = self
                        .matrix
                        .rules
                        .iter()
                        .find(|r| r.source == source && r.dest == dest && r.transform(msg).is_some())
                        .and_then(|r| r.mapping_for_cc(m.data[0]));
                    let is_clap_node = self
                        .graph
                        .node(dest)
                        .map_or(false, |n| matches!(n.kind, NodeKind::Clap { .. }));
                    let norm = m.data[1] as f64 / 127.0;
                    if let Some(CcMapping { param: pid, target: CcTarget::Knob, .. }) = mapping {
                        if self.apply_engine_param(dest, pid, norm) || is_clap_node {
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        }
                        continue;
                    }
                    let has_midi = self.has_midi_cable(dest);
                    let param = mapping.map(|m| m.param);
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
                        } else if self.apply_engine_param(dest, pid, norm) {
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        }
                    }
                }
            }
            Command::MatrixCcMap { source, dest, cc, param, target } => {
                let changed = match param {
                    Some(p) => match self
                        .matrix
                        .rules
                        .iter_mut()
                        .find(|r| r.source == source && r.dest == dest)
                    {
                        Some(r) => r.set_cc_target(cc, p, target),
                        None => {
                            // Learning a knob must not also wire the keys.
                            let mut r = Rule::new(source, dest);
                            r.cc_only = true;
                            r.set_cc_target(cc, p, target);
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
            Command::SetSystemAudio { gain } => self.set_system_gain(gain),
            Command::SetAudioIn { node, device, channels } => {
                // Structural: rewrite the node's device/channels, rebuild its
                // slot, and publish so frontends adopt the selection.
                let (next_device, next_channels) = {
                    let Some(n) = self.graph.nodes.iter_mut().find(|n| n.id == node) else { return };
                    let NodeKind::AudioIn { device: d, channels: c } = &mut n.kind else { return };
                    let nd: Option<String> = device.or_else(|| d.as_ref().cloned());
                    let nc = channels.unwrap_or(*c);
                    if nd == *d && nc == *c {
                        return; // no-op
                    }
                    *d = nd.clone();
                    *c = nc;
                    (nd, nc)
                };
                // Keep any already-opened ring; a fresh consumer arrives from
                // the UI once it (re)opens the device. Old samples drain out.
                if let Some(slot) = self.audio_in.iter_mut().find(|s| s.node == node) {
                    slot.channels = next_channels;
                    slot.device = next_device;
                }
                self.publish_graph();
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
            Command::SetRecordSync { on } => {
                if !on {
                    self.record_sync_pending = false; // a count-in still running must not start a take
                }
                if self.record_sync != on {
                    self.record_sync = on;
                    self.transport_dirty = true;
                }
            }
            Command::ResetMeter => {
                self.meter.reset();
                self.publish_meter();
            }
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
        // Sync-to-transport: recording follows the master transport. The take
        // actually begins the block the song clock reaches beat 0 (render);
        // play only arms it, stop cancels it and stops any running take.
        if playing {
            if self.record_sync {
                self.record_sync_pending = true;
            }
        } else {
            self.record_sync_pending = false;
            if self.record_sync {
                self.record_stop();
            }
        }
    }

    /// Fill every AudioIn node's stereo buffer from its input ring: map the chosen
    /// channel, apply gain, reconcile the clocks, and publish the input level.
    /// Allocation- and lock-free (rule 29).
    fn fill_audio_in(&mut self, frames: usize) {
        use crate::audio_in::DriftAction;
        let n2 = frames * 2;
        for s in &mut self.audio_in {
            for x in s.buf[..n2].iter_mut() {
                *x = 0.0;
            }
            let Some(consumer) = s.consumer.as_mut() else {
                // No capture opened for this node yet: silent, no level.
                continue;
            };
            for i in 0..frames {
                let l = match consumer.pop() {
                    Ok(v) => v,
                    Err(_) => {
                        s.starved += 1;
                        s.last[0]
                    }
                };
                let r = match consumer.pop() {
                    Ok(v) => v,
                    Err(_) => {
                        s.starved += 1;
                        s.last[1]
                    }
                };
                s.last = [l, r];
                s.raw[i * 2] = l;
                s.raw[i * 2 + 1] = r;
                let (ol, or) = match s.channels {
                    AudioInChannel::Stereo => (l, r),
                    AudioInChannel::MonoL => (l, l),
                    AudioInChannel::MonoR => (r, r),
                };
                let g = s.gain;
                s.buf[i * 2] = ol * g;
                s.buf[i * 2 + 1] = or * g;
            }
            match s.drift.action(consumer.slots()) {
                DriftAction::DropOne => {
                    let _ = consumer.pop();
                    s.dropped += 1;
                }
                DriftAction::InsertOne => {}
                DriftAction::Hold => {}
            }
            let peak = block_peak(&s.raw[..n2]);
            self.audio_in_levels.store(s.node, peak);
        }
    }

    /// Render one device block, allocation-free.
    pub fn render(&mut self, data: &mut [f32], ch: usize) {
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }
        self.drain_lv2_loader();
        // Sync-to-transport: begin the take the block the song clock starts at
        // beat 0 (immediately on play without a count-in, or right after one).
        if self.record_sync_pending && self.master_clock == MasterClock::Playing {
            self.record_sync_pending = false;
            self.record_start();
        }
        self.drain_voice_msgs();
        self.publish_voices();
        // Attach freshly-opened input captures (UI opened a device for a node):
        // a (node, ring consumer) pair, swapped in place of any stale consumer.
        while let Some(rx) = self.audio_in_rx.as_mut() {
            let Ok((node, consumer)) = rx.pop() else { break };
            if let Some(slot) = self.audio_in.iter_mut().find(|s| s.node == node) {
                slot.consumer = Some(consumer);
            }
        }
        // Mixer publication: a fixed-size copy under a NON-blocking lock
        // (never waits on the UI; retried next block if it is busy).
        if self.mix_dirty {
            match &self.graph_pub {
                Some(p) => if let Ok(mut g) = p.try_lock() {
                    g.mix = self.rig_mix;
                    g.system_gain = self.system_gain;
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
            // A free voice (no Clap node bound): skip the plugin entirely.
            if s.node == NodeId::MAX { continue; }
            s.voice.render(&mut s.scratch[..n2], self.transport);
            for x in s.scratch[..n2].iter_mut() {
                *x *= s.level;
            }
        }

        // 1a. Audio In nodes: pull the input ring into each node's stereo
        // buffer, map the chosen channel, apply gain. Allocation- and lock-free.
        self.fill_audio_in(frames);

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
                    if self.record_sync {
                        self.record_sync_pending = true;
                    }
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

        // 2b. Effects — built-in DSP and LV2 alike — in signal-flow order,
        // each summing EVERY audio source wired into it: instruments (CLAP and
        // LV2), audio inputs, metronomes and other effects. Built-in effects
        // used to hear only CLAP voices and audio inputs, and LV2 ran after
        // them, so setBfree → Drive and Delay → Reverb were silent. A feedback
        // loop reads the previous block's output of the effect not yet run.
        let mut bus = std::mem::take(&mut self.lv2_in);
        let n_dsp = self.dsp.len();
        let n_fx = n_dsp + self.lv2.len();
        self.fx_done.clear();
        self.fx_done.extend((0..n_dsp).map(|i| self.dsp[i].kind == DspKind::Metronome));
        self.fx_done.extend(std::iter::repeat(false).take(self.lv2.len()));
        while let Some(f) = self.next_effect(n_dsp, n_fx) {
            self.fx_done[f] = true;
            let node = if f < n_dsp { self.dsp[f].node } else { self.lv2[f - n_dsp].node };
            self.sum_audio_into(node, &mut bus[..n2]);
            if f < n_dsp {
                let d = &mut self.dsp[f];
                d.buf[..n2].copy_from_slice(&bus[..n2]);
                d.proc.process(&mut d.buf[..n2], frames);
            } else {
                let slot = &mut self.lv2[f - n_dsp];
                slot.plugin.process(&bus[..n2], &mut slot.buf[..n2], frames);
            }
        }
        self.lv2_in = bus;

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
                // Audio In wired into this master (OUT jack → graph routing).
                for a in &self.audio_in {
                    if !self.graph.has_route(a.node, master, PortKind::Audio) {
                        continue;
                    }
                    for i in 0..frames {
                        acc[i * 2] += a.buf[i * 2] * gl;
                        acc[i * 2 + 1] += a.buf[i * 2 + 1] * gr;
                    }
                }
            }
            // Audio In monitoring: a monitor-ON node is added dry (post-gain) to
            // the mixed master so the performer hears it before wiring it.
            for a in &self.audio_in {
                if a.monitor {
                    for i in 0..n2 {
                        acc[i] += a.buf[i];
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
            let lim = &mut self.master_lim;
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let (l, r) = lim.process(acc[i * 2], acc[i * 2 + 1], drive, vol);
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
            // Meter tap: loudness/true-peak/bands (Bravo 1) on the same final
            // output the user hears.
            self.meter.process(data, ch);
        } else {
            // No master wired: the output is silent. Keep the loudness windows
            // honest (they must decay, not hold stale energy).
            self.meter.process_silence(frames);
        }
        self.publish_meter();
        self.out_peak.store(peak.to_bits(), Ordering::Relaxed);
        self.publish_edge_levels(n2, sys);
    }
}

/// Zero-latency peak limiter for the master bus: holds the summed signal
/// under `CEILING` so the warmth stage after it only colours, never buzzes.
/// No lookahead (it would add its window to every played note); the brief
/// attack overshoot lands in the soft `tanh` that follows.
struct MasterLimiter {
    env: f32,
    gain: f32,
    rel: f32,
    atk: f32,
    /// Mean square of the reference chain (`tanh` at `DRIVE_REF`, made up)
    /// and of the chain at the current WARM, over `LOUDNESS_SECS`.
    ms_ref: f32,
    ms_cur: f32,
    ms_coef: f32,
    /// DC blocker state per channel (previous input, previous output) and
    /// its pole.
    dc: [(f32, f32); 2],
    dc_pole: f32,
}

impl MasterLimiter {
    /// Summed level a single loud voice reaches; more voices are held here.
    const CEILING: f32 = 0.7;
    /// Output peak of the whole master chain at VOLUME 100%: −1 dBFS. The
    /// limiter ceiling is mapped through the saturation and made up to this,
    /// so the headroom it buys is not lost as level (was −2.3…−4.4 dBFS).
    const OUT_PEAK: f32 = 0.891;

    /// Make-up gain after `tanh(x * drive)`: maps the limiter ceiling to
    /// `OUT_PEAK` for any WARM drive. Saturation (rule 49) is unchanged —
    /// the tanh input is the same; only the level after it moves.
    fn makeup(drive: f32) -> f32 {
        Self::OUT_PEAK / (Self::CEILING * drive).tanh()
    }

    /// Below this output level the knee is transparent (−3 dBFS).
    const KNEE: f32 = 0.708;

    /// Final soft knee: identity up to `KNEE`, then glides into `OUT_PEAK`
    /// and never beyond. The limiter has a 1 ms attack (zero latency), so a
    /// chord onset overshoots it for a moment; with make-up gain that
    /// overshoot reached +3.3 dBFS — digital clipping. The knee bounds it.
    #[inline]
    fn knee(y: f32) -> f32 {
        let a = y.abs();
        if a <= Self::KNEE { return y; }
        let room = Self::OUT_PEAK - Self::KNEE;
        (Self::KNEE + room * ((a - Self::KNEE) / room).tanh()).copysign(y)
    }

    /// Drive at the default WARM (0.15): the loudness every WARM setting keeps.
    const DRIVE_REF: f32 = 1.0 + 0.15 * 3.0;
    /// Window of the loudness match. Long against a waveform cycle (no
    /// gain ripple), short enough to follow a WARM turn.
    const LOUDNESS_SECS: f64 = 0.2;

    fn new(sample_rate: f64) -> Self {
        let coef = |secs: f64| (-1.0 / (sample_rate * secs)).exp() as f32;
        Self {
            env: 0.0, gain: 1.0, rel: coef(0.120), atk: coef(0.001),
            ms_ref: 0.0, ms_cur: 0.0, ms_coef: coef(Self::LOUDNESS_SECS),
            dc: [(0.0, 0.0); 2],
            dc_pole: 1.0 - (std::f64::consts::TAU * Self::DC_CUTOFF_HZ / sample_rate) as f32,
        }
    }

    /// Corner of the master's DC blocker: far below any note (the lowest
    /// piano key is 27.5 Hz), high enough to settle within a second.
    const DC_CUTOFF_HZ: f64 = 5.0;

    /// One-pole DC blocker, `y = x - x[-1] + pole * y[-1]`. A source stuck at
    /// a constant (a Surge instance on a MONO input once held the sum near
    /// 1) pinned the limiter: the meter read full while every rig was turned
    /// down to silence, and the speakers got a DC offset.
    #[inline]
    fn block_dc(&mut self, ch: usize, x: f32) -> f32 {
        let (px, py) = self.dc[ch];
        let y = x - px + self.dc_pole * py;
        // Flush denormals: a silent tail must not cost the CPU.
        self.dc[ch] = (x, if y.abs() < 1e-20 { 0.0 } else { y });
        y
    }

    /// The whole master chain for one stereo frame: DC blocker → limiter → `tanh(drive)`
    /// warmth → loudness match → volume → knee. WARM colours, it does not
    /// turn up: below the ceiling `tanh` is almost linear, so a make-up that
    /// only pins the full-scale peak (`makeup(drive)`) left quiet playing
    /// +7.7 dB louder at WARM 100 % than at 0 %. The output is held at the
    /// RMS the default-WARM chain would have for the same input; at the
    /// default WARM that is exactly `makeup(DRIVE_REF)`. Allocation-free.
    #[inline]
    fn process(&mut self, l: f32, r: f32, drive: f32, vol: f32) -> (f32, f32) {
        let (l, r) = (self.block_dc(0, l), self.block_dc(1, r));
        let g = self.gain_for(l, r);
        let (l, r) = (l * g, r * g);
        let (yl, yr) = ((l * drive).tanh(), (r * drive).tanh());
        let m = Self::makeup(Self::DRIVE_REF);
        let (rl, rr) = ((l * Self::DRIVE_REF).tanh() * m, (r * Self::DRIVE_REF).tanh() * m);
        let c = self.ms_coef;
        self.ms_ref = c * self.ms_ref + (1.0 - c) * (rl * rl + rr * rr);
        self.ms_cur = c * self.ms_cur + (1.0 - c) * (yl * yl + yr * yr);
        // Both means follow the same input, so their ratio is right from a
        // note's first samples; the floor only keeps silence finite.
        let auto = ((self.ms_ref + 1e-12) / (self.ms_cur + 1e-12)).sqrt();
        let k = auto * vol;
        (Self::knee(yl * k), Self::knee(yr * k))
    }

    fn gain_for(&mut self, l: f32, r: f32) -> f32 {
        let peak = l.abs().max(r.abs());
        self.env = peak.max(self.env * self.rel);
        let target = if self.env > Self::CEILING { Self::CEILING / self.env } else { 1.0 };
        let coef = if target < self.gain { self.atk } else { self.rel };
        self.gain = coef * self.gain + (1.0 - coef) * target;
        self.gain
    }
}

/// A routed note moved by a zone's octave shift; `None` when it leaves the
/// MIDI range (that key is silent in this zone, on and off alike).
#[inline]
fn shifted(note: u8, semis: i16) -> Option<u8> {
    u8::try_from(note as i16 + semis).ok().filter(|n| *n <= 127)
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
mod master_limiter_tests {
    use super::MasterLimiter;

    fn run(amp: f32) -> (f32, f32) {
        let mut lim = MasterLimiter::new(48000.0);
        let (mut gmin, mut peak) = (1.0f32, 0.0f32);
        for i in 0..48000 {
            let x = amp * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin();
            let g = lim.gain_for(x, x);
            if i > 4800 {
                gmin = gmin.min(g);
                peak = peak.max((x * g).abs());
            }
        }
        (gmin, peak)
    }

    #[test]
    fn a_chord_onset_is_caught_within_ten_milliseconds() {
        let mut lim = MasterLimiter::new(48000.0);
        let x = 4.0f32; // a big chord's sum, from silence
        let mut g = 1.0;
        for _ in 0..(48000 * 10 / 1000) {
            g = lim.gain_for(x, x);
        }
        assert!((x * g - MasterLimiter::CEILING).abs() < 0.02, "after 10 ms the sum is held: {}", x * g);
    }

    /// Peak and RMS (dBFS) of the whole chain at VOLUME 100 % over the last
    /// half of one second of a 440 Hz sine of `amp` on both channels.
    fn chain(amp: f32, warm: f32) -> (f32, f32) {
        let drive = 1.0 + warm * 3.0;
        let mut lim = MasterLimiter::new(48000.0);
        let (mut peak, mut sq, mut n) = (0.0f32, 0.0f64, 0usize);
        for i in 0..48000 {
            let x = amp * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin();
            let (l, _) = lim.process(x, x, drive, 1.0);
            if i >= 24000 {
                peak = peak.max(l.abs());
                sq += (l * l) as f64;
                n += 1;
            }
        }
        (20.0 * peak.log10(), 10.0 * (sq / n as f64).log10() as f32)
    }

    #[test]
    fn full_volume_reaches_minus_one_dbfs_and_never_beyond() {
        // A chord far above the ceiling: never past −1 dBFS at any WARM, and
        // at the default WARM it reaches it (was −2.3…−4.4 before make-up).
        for warm in [0.0f32, 0.15, 0.5, 1.0] {
            let (peak, _) = chain(4.0, warm);
            assert!(peak <= -0.99, "warm {warm}: master peak {peak:.2} dBFS");
        }
        let (peak, _) = chain(4.0, 0.15);
        assert!(peak > -1.6, "default warm reaches −1 dBFS: {peak:.2}");
        // A quiet single voice gets the make-up too (no longer 2–4 dB down).
        let (quiet, _) = chain(0.1, 0.15);
        assert!(quiet > 20.0 * (0.1f32 * 1.6).log10(), "make-up lifts a quiet voice: {quiet:.2} dBFS");
        assert_eq!(MasterLimiter::knee(0.5), 0.5, "the knee is transparent below −3 dBFS");
        assert!(MasterLimiter::knee(5.0) <= MasterLimiter::OUT_PEAK && MasterLimiter::knee(-5.0) >= -MasterLimiter::OUT_PEAK, "bounded both ways");
    }

    /// WARM is colour, not volume: from a quiet voice to a held chord the
    /// loudness stays within 0.5 dB of the default WARM's over the whole
    /// knob (the old peak-only make-up gave +7.7 dB from 0 % to 100 %).
    #[test]
    fn warm_keeps_the_loudness_of_the_default_warm() {
        for amp in [0.05f32, 0.2, 0.5, 1.0, 4.0] {
            let (_, reference) = chain(amp, 0.15);
            for warm in [0.0f32, 0.3, 0.6, 1.0] {
                let (_, rms) = chain(amp, warm);
                assert!((rms - reference).abs() < 0.5,
                    "amp {amp} warm {warm}: {rms:.2} dB RMS vs {reference:.2} at the default WARM");
            }
        }
    }

    /// A source stuck at a constant must not reach the output or pin the
    /// limiter: after a second the DC is gone, and a tone on top of it plays
    /// at its normal level instead of being turned down to nothing.
    #[test]
    fn a_stuck_constant_is_blocked_and_does_not_duck_the_music() {
        let mut lim = MasterLimiter::new(48000.0);
        let (mut sum, mut n, mut tone_peak) = (0.0f64, 0usize, 0.0f32);
        for i in 0..96000 {
            let tone = 0.2 * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin();
            let (l, _) = lim.process(3.0 + tone, 3.0 + tone, 1.45, 1.0);
            if i >= 48000 {
                sum += l as f64;
                n += 1;
                tone_peak = tone_peak.max(l.abs());
            }
        }
        let mean = sum / n as f64;
        assert!(mean.abs() < 1e-3, "DC left after a second: {mean}");
        let (_, alone) = chain(0.2, 0.15);
        assert!(20.0 * tone_peak.log10() > alone - 1.0,
            "the tone keeps its level over a stuck DC: {:.2} dBFS vs {alone:.2}", 20.0 * tone_peak.log10());
    }

    /// Turning WARM up still adds harmonics: a loud sine comes out with a
    /// lower crest factor (flatter tops) than at WARM 0.
    #[test]
    fn warm_still_saturates() {
        let crest = |warm| { let (p, r) = chain(0.6, warm); p - r };
        assert!(crest(1.0) < crest(0.0) - 1.0, "crest at WARM 100 % {:.2} dB vs 0 % {:.2} dB", crest(1.0), crest(0.0));
    }

    #[test]
    fn leaves_a_single_voice_alone_and_holds_a_chord_at_the_ceiling() {
        let (g, p) = run(0.5);
        assert_eq!((g, p), (1.0, 0.5), "below the ceiling nothing changes");
        for amp in [1.0f32, 2.0, 4.0] {
            let (_, p) = run(amp);
            assert!((p - MasterLimiter::CEILING).abs() < 0.01, "amp {amp}: held at {p}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::matrix::{CcTarget, Matrix, MidiMsg, SourceId};

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
    fn each_split_zone_plays_in_its_own_octave() {
        use crate::control::Command;
        let (mut e, mut bus) = split_engine(Some(60), true);
        // LOW one octave up (the bass case), HIGH one down.
        let log = play(&mut e, &mut bus, Command::SetZoneOctave { node: 0, zone: 0, octave: 1 });
        assert_eq!(log, vec![(1, 255, false), (4, 255, false)], "a zone move releases held notes first");
        play(&mut e, &mut bus, Command::SetZoneOctave { node: 0, zone: 1, octave: -1 });
        assert_eq!(e.graph().node(0).unwrap().zone_octave, [1, -1]);
        // The zone is chosen by the KEY played; the octave moves what sounds.
        assert_eq!(play(&mut e, &mut bus, on(40)), vec![(1, 52, true)], "E2 on LOW sounds E3");
        assert_eq!(play(&mut e, &mut bus, on(72)), vec![(4, 60, true)], "C5 on HIGH sounds C4");
        assert_eq!(play(&mut e, &mut bus, off(40)), vec![(1, 52, false)], "the note-off meets its note-on");
        // Clamped to ±3 octaves; a note pushed past the MIDI range is silent.
        play(&mut e, &mut bus, Command::SetZoneOctave { node: 0, zone: 1, octave: 9 });
        assert_eq!(e.graph().node(0).unwrap().zone_octave[1], crate::control::ZONE_OCTAVE_MAX);
        assert!(play(&mut e, &mut bus, on(100)).is_empty(), "100 + 36 is past 127");
        // Unsplit: the shift goes with the zones; one range again.
        play(&mut e, &mut bus, Command::SetNodeSplit { node: 0, point: None });
        assert_eq!(e.graph().node(0).unwrap().zone_octave, [0, 0]);
        assert_eq!(play(&mut e, &mut bus, on(40)), vec![(1, 40, true)]);
        // An unsplit Keys node takes no zone octave.
        assert!(play(&mut e, &mut bus, Command::SetZoneOctave { node: 0, zone: 0, octave: 1 }).is_empty());
        assert_eq!(e.graph().node(0).unwrap().zone_octave, [0, 0]);
    }

    /// A voice carried "keyboard off" from the node it played before a
    /// load onto the node it plays after it, while every frontend showed
    /// the default again. Real Surge XT; skipped when it is not installed.
    #[test]
    fn a_load_gives_every_clap_node_the_default_inputs_again() {
        let Ok((voice, _main)) = crate::host::ClapVoice::new(48000.0) else { return };
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::demo_patch(), drain, 48000.0, vec![(1, voice)]);
        let mut buf = vec![0.0f32; 256 * 2];
        let mut peak_of_a_note = |engine: &mut super::GraphEngine, bus: &mut crate::control::CommandBus| {
            bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
            let mut peak = 0.0f32;
            for _ in 0..40 {
                engine.render(&mut buf, 2);
                peak = peak.max(super::block_peak(&buf));
            }
            bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
            for _ in 0..200 {
                engine.render(&mut buf, 2);
            }
            peak
        };
        bus.send(Command::SetRigSource { node: 1, keyboard: false, hardware: true });
        assert!(peak_of_a_note(&mut engine, &mut bus) < 1e-4, "keyboard off: the synth ignores the keys");
        bus.send(Command::LoadGraph { graph: crate::control::Graph::demo_patch() });
        let peak = peak_of_a_note(&mut engine, &mut bus);
        assert!(peak > 1e-3, "after a load the synth hears the keyboard again (peak {peak})");
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
        let to_1 = |out: &[(u32, MidiMsg)]| out.iter().filter(|(d, _)| *d == 1).map(|(_, m)| m.note()).collect::<Vec<u8>>();
        assert_eq!(to_1(&out), [36]);
        // Note 60 is outside the zone → filtered.
        engine.matrix.route_into(SourceId::Keys, MidiMsg::note_on(2, 60, 100), &mut out);
        assert!(to_1(&out).is_empty());
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

    /// #21: the system-audio GAIN knob is a real SetParam (and so CC-bindable),
    /// and the gain goes back to frontends like SetSystemAudio's does.
    #[test]
    fn system_audio_gain_knob_sets_and_publishes_the_capture_gain() {
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::demo_patch();
        let sys = g.add_node(NodeKind::SystemAudio, name_bytes("SYSTEM AUDIO"));
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::GraphPub::default()));
        engine.set_graph_pub(pub_.clone());
        let mut buf = vec![0.0f32; 64 * 2];
        bus.send(Command::SetParam { node: sys, param: super::P_SYSTEM_GAIN, value: 0.6 });
        engine.render(&mut buf, 2);
        assert!((engine.system_gain - 0.6).abs() < 1e-6);
        assert!((pub_.lock().unwrap().system_gain - 0.6).abs() < 1e-6, "published for the knob mirror");
        // The param id means nothing on another node kind.
        bus.send(Command::SetParam { node: 1, param: super::P_SYSTEM_GAIN, value: 0.1 });
        engine.render(&mut buf, 2);
        assert!((engine.system_gain - 0.6).abs() < 1e-6);
        // SetSystemAudio (tool path) publishes too.
        bus.send(Command::SetSystemAudio { gain: 0.25 });
        engine.render(&mut buf, 2);
        assert!((pub_.lock().unwrap().system_gain - 0.25).abs() < 1e-6);
        // A CC bound to the knob (CcTarget::Knob) reaches the same setter.
        assert!(engine.apply_engine_param(sys, super::P_SYSTEM_GAIN, 1.0));
        assert!((engine.system_gain - 1.0).abs() < 1e-6);
    }

    /// #19: one knob table — rack labels canonical, long names as aliases.
    #[test]
    fn knob_specs_resolve_rack_labels_and_aliases() {
        use super::find_knob_spec;
        use crate::control::DspKind;
        let filter = NodeKind::Dsp(DspKind::Filter);
        assert_eq!(find_knob_spec(&filter, "reso").and_then(|k| k.param), Some(1));
        assert_eq!(find_knob_spec(&filter, "RESONANCE").map(|k| k.label), Some("RESO"));
        let lim = NodeKind::Dsp(DspKind::Limiter);
        // CEILING is the CEIL knob (param 0), never DRIVE (param 1).
        assert_eq!(find_knob_spec(&lim, "CEILING").and_then(|k| k.param), Some(0));
        assert_eq!(find_knob_spec(&lim, "DRIVE").and_then(|k| k.param), Some(1));
        assert!(find_knob_spec(&lim, "THRESHOLD").is_none());
        // Engine param names agree with the knob a param drives.
        for dk in [DspKind::Delay, DspKind::Reverb, DspKind::Gain, DspKind::Filter, DspKind::Eq3,
                   DspKind::Limiter, DspKind::Saturation, DspKind::Pan] {
            let kind = NodeKind::Dsp(dk.clone());
            for &(id, name) in super::dsp_param_choices(&dk) {
                let spec = find_knob_spec(&kind, name).unwrap_or_else(|| panic!("{dk:?} '{name}' has no knob"));
                assert_eq!(spec.param, Some(id), "{dk:?} '{name}'");
            }
        }
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

    /// Effects hear every wired source, in signal-flow order whatever the
    /// node order: Metronome → Gain A → Gain B → Master, with Gain B added
    /// first. Built-in effects heard only CLAP voices and audio inputs, so
    /// any chain of two effects (or LV2 → effect, like setBfree → Drive)
    /// was silent at the master.
    #[test]
    fn effect_chains_reach_the_master_in_signal_order() {
        use crate::control::DspKind;
        let mut g = crate::control::Graph::default();
        let gain_b = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN B"));
        let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
        let gain_a = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN A"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: metro, port: 0 }, PortRef { node: gain_a, port: 0 }, PortKind::Audio);
        g.connect_with_id(2, PortRef { node: gain_a, port: 0 }, PortRef { node: gain_b, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: gain_b, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        for n in [gain_a, gain_b] {
            bus.send(Command::SetParam { node: n, param: 0, value: 0.9 });
        }
        bus.send(Command::SetNodePlaying { node: metro, playing: true });
        let peak = peak_1s(&mut engine);
        assert!(peak > 0.01, "the click passes two effects to the master: peak {peak}");
        // A feedback loop (B → A) must not hang the render.
        bus.send(Command::Connect { id: 4, from: PortRef { node: gain_b, port: 0 }, to: PortRef { node: gain_a, port: 0 }, kind: PortKind::Audio });
        assert!(peak_1s(&mut engine).is_finite());
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
    fn rename_rig_changes_only_the_published_name() {
        let (mut engine, mut bus) = load_two_rigs(Some(2), None);
        let snap = std::sync::Arc::new(std::sync::Mutex::new(crate::control::RigSnapshot::default()));
        engine.set_rig_snapshot(snap.clone());
        let before = engine.graph.clone();
        bus.send(Command::RenameRig { rig: 1, name: crate::control::name_bytes("Bass") });
        bus.send(Command::RenameRig { rig: 9, name: crate::control::name_bytes("nope") });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        assert_eq!(engine.rig_meta()[1].name, "Bass");
        assert_eq!(engine.rig_meta()[0].name, "A", "other rigs keep their names");
        assert_eq!(engine.rig_meta()[0].midi_device, Some(2), "device binding untouched");
        assert_eq!(snap.lock().unwrap().rigs[1].name, "Bass", "published to frontends");
        assert_eq!(format!("{:?}", engine.graph), format!("{before:?}"), "routing untouched");
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
    fn audio_in_routes_and_meters() {
        use crate::control::{command_bus, name_bytes, AudioInChannel, Command, NodeKind, PortKind, PortRef};
        use super::{P_AUDIO_IN_GAIN, P_AUDIO_IN_MONITOR};
        let (mut bus, drain) = command_bus();
        // AudioIn → Master (audio), single-rig graph.
        let mut g = crate::control::Graph::default();
        let ain = g.add_node(
            NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo },
            name_bytes("AUDIO IN"),
        );
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: ain, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        let levels = engine.audio_in_levels_handle();
        // Side channel: UI hands the engine a freshly-opened input ring.
        let (mut side_tx, side_rx) = rtrb::RingBuffer::<(u32, rtrb::Consumer<f32>)>::new(8);
        engine.set_audio_in_channel(side_rx);
        // Data ring: a stereo interleaved sine at 0.4 (L) / 0.2 (R).
        let (mut data_tx, data_rx) = rtrb::RingBuffer::<f32>::new(4096);
        for i in 0..1024usize {
            let t = i as f32 / 48000.0;
            let _ = data_tx.push((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.4);
            let _ = data_tx.push((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2);
        }
        let _ = side_tx.push((ain, data_rx));

        let mut buf = vec![0.0f32; 256 * 2];
        engine.render(&mut buf, 2);
        // Audio reaches the master (wired OUT), scaled by unity gain.
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.0, "AudioIn output must reach master: {peak}");
        // The input level (pre-gain) is published for the face meter.
        assert!(levels.load(ain) > 0.0, "published input level");

        // Gain via SetParam scales the wired path.
        bus.send(Command::SetParam { node: ain, param: P_AUDIO_IN_GAIN, value: 0.25 });
        // Monitor via SetParam: 1.0 = on.
        bus.send(Command::SetParam { node: ain, param: P_AUDIO_IN_MONITOR, value: 1.0 });
        engine.render(&mut buf, 2);
        let slot = engine.audio_in.iter().find(|s| s.node == ain).unwrap();
        assert_eq!(slot.gain, 0.25, "gain applied (0.25 raw)");
        assert!(slot.monitor, "monitor on");
    }

    #[test]
    fn audio_in_monitor_without_wiring_reaches_master() {
        use crate::control::{command_bus, name_bytes, AudioInChannel, Command, NodeKind};
        use super::P_AUDIO_IN_MONITOR;
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::default();
        let ain = g.add_node(
            NodeKind::AudioIn { device: None, channels: AudioInChannel::MonoL },
            name_bytes("AUDIO IN"),
        );
        let _master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        // NO cable: monitor must carry it. The master sum runs for a MasterOut node.
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        let (mut side_tx, side_rx) = rtrb::RingBuffer::<(u32, rtrb::Consumer<f32>)>::new(8);
        engine.set_audio_in_channel(side_rx);
        let (mut data_tx, data_rx) = rtrb::RingBuffer::<f32>::new(4096);
        for _ in 0..1024usize {
            let _ = data_tx.push(0.5);
            let _ = data_tx.push(0.5);
        }
        let _ = side_tx.push((ain, data_rx));
        bus.send(Command::SetParam { node: ain, param: P_AUDIO_IN_MONITOR, value: 1.0 });
        let mut buf = vec![0.0f32; 256 * 2];
        engine.render(&mut buf, 2);
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.0, "monitor carries the input to master without wiring: {peak}");
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
        bus.send(Command::MatrixCcMap { source: dev, dest: 1, cc: 74, param: Some(42), target: CcTarget::PluginParam });
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
            target: CcTarget::PluginParam,
        });
        engine.render(&mut buf, 2);
        let rev_after = pub_.lock().unwrap().rev;
        assert_eq!(rev_after, rev_before + 1, "MatrixCcMap must publish cc_map");
        let r = pub_.lock().unwrap().rules.iter().find(|r| r.source == SourceId::Hardware(0) && r.dest == 1).cloned().unwrap();
        assert_eq!(r.param_for_cc(74), Some(42));

        // Duplicate MatrixCcMap does not bump rev.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(42), target: CcTarget::PluginParam });
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
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(99), target: CcTarget::PluginParam });
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: filter, cc: 7, param: Some(0), target: CcTarget::PluginParam });
        engine.cc_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 0 });
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 7, value: 64 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_log[0], (1, 99, 0.0));
        assert_eq!(engine.cc_log[1].0, filter);
        assert_eq!(engine.cc_log[1].1, 0);
        assert!((engine.cc_log[1].2 - 64.0 / 127.0).abs() < 1e-6);

        // 5. Remove CC 74 -> node 1 returns to empty-map transparency.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: None, target: CcTarget::PluginParam });
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
            target: CcTarget::PluginParam,
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

    #[test]
    fn cc_knob_target_matches_set_param_on_dsp_and_clap() {
        // 1. DSP Delay knob: CC target Knob with P_MIX moves the exact same Delay mix as Command::SetParam.
        let build_dsp = || {
            let (mut bus, drain) = command_bus();
            let mut g = crate::control::Graph::default();
            let metro = g.add_node(NodeKind::Dsp(crate::control::DspKind::Metronome), name_bytes("METRO"));
            let delay = g.add_node(NodeKind::Dsp(crate::control::DspKind::Delay), name_bytes("DELAY"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: metro, port: 0 }, PortRef { node: delay, port: 0 }, PortKind::Audio);
            g.connect_with_id(2, PortRef { node: delay, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
            let engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
            bus.send(Command::SetNodePlaying { node: metro, playing: true });
            (engine, bus, delay)
        };
        let (mut eng_cc, mut bus_cc, delay_cc) = build_dsp();
        let (mut eng_sp, mut bus_sp, delay_sp) = build_dsp();
        let val = 90u8;
        let norm = val as f64 / 127.0;
        bus_cc.send(Command::MatrixCcMap {
            source: SourceId::Hardware(0),
            dest: delay_cc,
            cc: 12,
            param: Some(super::P_MIX),
            target: CcTarget::Knob,
        });
        bus_cc.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 12, value: val });
        bus_sp.send(Command::SetParam { node: delay_sp, param: super::P_MIX, value: norm });
        let mut buf_cc = vec![0.0f32; 256 * 2];
        let mut buf_sp = vec![0.0f32; 256 * 2];
        for _ in 0..8 {
            eng_cc.render(&mut buf_cc, 2);
            eng_sp.render(&mut buf_sp, 2);
            assert_eq!(buf_cc, buf_sp, "DSP knob CC must match SetParam sample-for-sample");
        }

        // 2. CLAP unit knob (P_LEVEL): moves voice level without touching CLAP param 0.
        let Ok((voice, mut main)) = crate::host::ClapVoice::new(48000.0) else {
            return;
        };
        let params = main.param_list();
        let p0_before = params.first().and_then(|p| main.param_value(p.id));
        let p0_id = params.first().map(|p| p.id).unwrap_or(0);
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![(surge, voice)]);
        // Even on an unrouted device (or if the voice rig_device differs), a Knob CC moves s.level.
        bus.send(Command::MatrixCcMap {
            source: SourceId::Hardware(3),
            dest: surge,
            cc: 7,
            param: Some(super::P_LEVEL),
            target: CcTarget::Knob,
        });
        bus.send(Command::Cc { source: SourceId::Hardware(3), channel: 0, cc: 7, value: 0 });
        let mut buf = vec![0.0f32; 128 * 2];
        for _ in 0..2 { engine.render(&mut buf, 2); }
        assert!((engine.voices[0].level - 0.0).abs() < 1e-6, "CC 7=0 must set voice level to 0");
        bus.send(Command::Cc { source: SourceId::Hardware(3), channel: 0, cc: 7, value: 64 });
        for _ in 0..2 { engine.render(&mut buf, 2); }
        assert!((engine.voices[0].level - 64.0 / 127.0).abs() < 1e-6, "CC 7=64 must match SetParam(P_LEVEL)");
        assert_eq!(main.param_value(p0_id), p0_before, "Knob P_LEVEL binding must not touch CLAP param 0");
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

/// Per-edge audio level self-test (docs/ARCHITECTURE.md §2 signature): the demo's chain
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
    // KABELSALAT_LV2_URI tests one specific instrument (e.g. a freshly
    // installed catalog entry); default: real synths first, then the LSP
    // oscillator, which only proves routing.
    let wanted = std::env::var("KABELSALAT_LV2_URI").ok().filter(|u| !u.is_empty());
    let defaults = ["urn:distrho:vitalium", "http://lsp-plug.in/plugins/lv2/oscillator_mono"];
    let candidates: Vec<&str> = match &wanted { Some(u) => vec![u.as_str()], None => defaults.to_vec() };
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
    // A chord pressed and released within one block each: every note-off
    // must reach the plugin (one event per block left a note hanging).
    for n in [60u8, 64, 67] {
        bus.send(Command::NoteOn { note: n, velocity: 0.9, source: SourceId::Keys });
    }
    let chord = peak_over(&mut engine, 47);
    for n in [60u8, 64, 67] {
        bus.send(Command::NoteOff { note: n, source: SourceId::Keys });
    }
    peak_over(&mut engine, 375);
    let chord_after = peak_over(&mut engine, 94);
    println!("lv2-instrument-engine-test: chord held={chord:.4} 4-5 s after one-block release={chord_after:.4}");
    anyhow::ensure!(chord > 1e-3, "LV2 instrument {uri} is silent for a chord");
    anyhow::ensure!(chord_after < chord * 0.01, "LV2 instrument {uri} leaves a chord note hanging after note-off");
    // Legato: release 60 and press 62 in the same block, then release 62.
    // With one event per block the note-off for 60 was lost (a hanging
    // organ note, reported live with setBfree).
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    peak_over(&mut engine, 20);
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    bus.send(Command::NoteOn { note: 62, velocity: 0.9, source: SourceId::Keys });
    let legato = peak_over(&mut engine, 20);
    bus.send(Command::NoteOff { note: 62, source: SourceId::Keys });
    peak_over(&mut engine, 375);
    let legato_after = peak_over(&mut engine, 94);
    println!("lv2-instrument-engine-test: legato held={legato:.4} 4-5 s after release={legato_after:.4}");
    anyhow::ensure!(legato_after < legato * 0.01, "LV2 instrument {uri} leaves a legato note hanging (note-off and note-on in one block)");
    // Through a built-in effect, like a live organ rig (setBfree → Drive →
    // Master): the effect must hear the LV2 instrument.
    let drive = master + 1;
    bus.send(Command::AddNode { kind: NodeKind::Dsp(crate::control::DspKind::Saturation), name: name_bytes("DRIVE"), rig: 0 });
    bus.send(Command::Disconnect(2));
    bus.send(Command::Connect { id: 3, from: PortRef { node: inst, port: 0 }, to: PortRef { node: drive, port: 0 }, kind: PortKind::Audio });
    bus.send(Command::Connect { id: 4, from: PortRef { node: drive, port: 0 }, to: PortRef { node: master, port: 0 }, kind: PortKind::Audio });
    peak_over(&mut engine, 5);
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let through = peak_over(&mut engine, 47);
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    peak_over(&mut engine, 100);
    println!("lv2-instrument-engine-test: through a Saturation unit: master peak held={through:.4}");
    anyhow::ensure!(through > 1e-3, "LV2 instrument {uri} → built-in effect → Master is silent");
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

/// What a held note sounds like, coarsely: loudness, pitch-ish zero-crossing
/// rate and brightness (high-frequency share).
#[derive(Clone, Copy)]
struct Timbre {
    rms: f32,
    zero_cross_hz: f32,
    brightness: f32,
}

impl Timbre {
    /// How far apart two timbres are: the sum of their relative differences.
    fn distance(self, other: Timbre) -> f32 {
        let rel = |a: f32, b: f32| (a - b).abs() / a.abs().max(b.abs()).max(1e-6);
        rel(self.rms, other.rms) + rel(self.zero_cross_hz, other.zero_cross_hz) + rel(self.brightness, other.brightness)
    }
}

/// Hold `note` for half a second at the engine's master and describe it,
/// then release it and let the tail ring out.
fn timbre_of_note(engine: &mut GraphEngine, bus: &mut crate::control::CommandBus, note: u8, sr: f64) -> Timbre {
    use crate::control::Command;
    use crate::matrix::SourceId;
    let mut buf = vec![0.0f32; 512 * 2];
    bus.send(Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys });
    let (mut zc, mut sq, mut dsq, mut n, mut prev) = (0usize, 0.0f64, 0.0f64, 0usize, 0.0f32);
    for _ in 0..(sr as usize / 2 / 512) {
        engine.render(&mut buf, 2);
        for f in buf.chunks(2) {
            let x = f[0];
            if (x > 0.0) != (prev > 0.0) { zc += 1; }
            sq += (x * x) as f64;
            dsq += ((x - prev) * (x - prev)) as f64;
            prev = x;
            n += 1;
        }
    }
    bus.send(Command::NoteOff { note, source: SourceId::Keys });
    for _ in 0..(sr as usize * 2 / 512) { engine.render(&mut buf, 2); }
    let rms = (sq / n as f64).sqrt() as f32;
    Timbre { rms, zero_cross_hz: zc as f32 / n as f32 * sr as f32 / 2.0, brightness: (dsq / n as f64).sqrt() as f32 / rms.max(1e-9) }
}

/// One instrument of `sound-library-test`: its library, a search, and one
/// sound loaded through the engine. Ok(line) describes what was proven.
fn sound_library_check(instrument: &crate::sounds::Instrument, query: &str) -> anyhow::Result<String> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, NodeKind, PortKind, PortRef};
    use crate::sounds::{Instrument, Libraries, Load, Lookup, Target};
    let sr = 48000.0;
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let kind = match instrument {
        Instrument::Clap(id) => NodeKind::Clap { plugin_id: name_bytes(id) },
        Instrument::Lv2(uri) => NodeKind::Lv2 { uri: Lv2Uri::new(uri) },
    };
    let node = g.add_node(kind, name_bytes("INSTRUMENT"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);

    let mut buf = vec![0.0f32; 512 * 2];
    let (mut engine, mut clap) = match instrument {
        Instrument::Clap(id) => {
            let (voice, main) = crate::host::ClapVoice::new_plugin(sr, id)?;
            let controller = voice.controller();
            (GraphEngine::new(g, drain, sr, vec![(node, voice)]), Some((main, controller)))
        }
        Instrument::Lv2(uri) => {
            // The app's path: the loader thread instantiates the plugin.
            let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);
            engine.spawn_lv2_loader();
            bus.send(Command::LoadGraph { graph: g });
            let start = std::time::Instant::now();
            while !engine.lv2.iter().any(|s| s.node == node) {
                anyhow::ensure!(start.elapsed() < std::time::Duration::from_secs(10), "{uri} never attached");
                engine.render(&mut buf, 2);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (engine, None)
        }
    };

    let library = match Libraries::blocking(crate::sounds::scan).get(instrument) {
        Lookup::Ready(library) => library,
        Lookup::AskPlugin => {
            let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("a preset parameter needs the live plugin"))?;
            let params = main.param_list();
            std::sync::Arc::new(crate::sounds::preset_param_library(&params, |id, value| main.param_text(id, value)))
        }
        Lookup::Scanning => anyhow::bail!("a blocking library never reports a scan in progress"),
    };
    anyhow::ensure!(!library.sounds.is_empty(), "no {} found (its parameters: {})", library.kind, library.params.iter().take(6).cloned().collect::<Vec<_>>().join(", "));
    let hits = crate::sounds::search(&library.sounds, query, 5).labels;
    let label = hits.first().ok_or_else(|| anyhow::anyhow!("no {} matches '{query}' ({} in the library)", library.kind, library.sounds.len()))?;
    let sound = crate::sounds::find(&library.sounds, label).ok_or_else(|| anyhow::anyhow!("'{label}' was listed but is not found"))?;
    anyhow::ensure!(sound.label() == *label, "'{label}' was listed, but find picks '{}'", sound.label());

    for _ in 0..20 { engine.render(&mut buf, 2); }
    let before = timbre_of_note(&mut engine, &mut bus, 60, sr);
    let state_before = clap.as_mut().and_then(|(main, _)| main.save_state().ok());
    crate::sounds::load(sound, Target { node, clap: clap.as_mut().map(|(main, controller)| (main, &*controller)), bus: Some(&mut bus) })
        .map_err(|e| anyhow::anyhow!("loading '{label}': {e}"))?;
    // A plugin applies state and parameters on the audio thread; an LV2
    // preset comes back from the loader thread.
    for _ in 0..40 {
        engine.render(&mut buf, 2);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let readback = match &sound.load {
        Load::ParamValue { param, value } => {
            let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("no live plugin"))?;
            let now = main.param_value(*param);
            anyhow::ensure!(now == Some(*value), "parameter {param} reads {now:?} after loading '{label}', expected {value}");
            format!("parameter {param} = {value} ('{}')", main.param_text(*param, *value))
        }
        Load::MidiProgram(program) => {
            let last = engine.lv2_last_note(node);
            anyhow::ensure!(last == Some([0xC0, *program, 0]), "the plugin's last MIDI event is {last:?}, expected programme change {program}");
            format!("programme change {program} reached the plugin")
        }
        Load::SurgePatch(_) | Load::DexedVoice { .. } => {
            let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("no live plugin"))?;
            let state = main.save_state()?;
            anyhow::ensure!(Some(&state) != state_before.as_ref(), "plugin state unchanged after loading '{label}'");
            "plugin state changed".to_string()
        }
        Load::Lv2Preset(uri) => {
            let preset = crate::lv2::discover_presets(instrument.id())
                .into_iter().find(|p| p.uri == *uri).ok_or_else(|| anyhow::anyhow!("preset {uri} vanished"))?;
            let slot = engine.lv2.iter().find(|s| s.node == node).ok_or_else(|| anyhow::anyhow!("LV2 slot gone"))?;
            let controls = slot.plugin.control_descs();
            let (set, total) = preset.values.iter().fold((0, 0), |(set, total), (symbol, value)| {
                match controls.iter().find(|c| c.symbol == *symbol) {
                    Some(c) => (set + ((c.value - value).abs() < 1e-5) as usize, total + 1),
                    None => (set, total),
                }
            });
            anyhow::ensure!(total > 0 && set == total, "{set} of {total} preset controls hold the preset's value");
            format!("{set}/{total} controls hold the preset's values")
        }
    };
    let after = timbre_of_note(&mut engine, &mut bus, 60, sr);
    anyhow::ensure!(after.rms > 1e-4, "'{label}' is silent (rms {:.5})", after.rms);
    let distance = before.distance(after);
    anyhow::ensure!(distance > 0.1, "'{label}' sounds like the sound before it (distance {distance:.3})");
    Ok(format!("{} {} · '{query}' finds {} (first: '{label}') · loaded: {readback} · note 60 before rms {:.4} ~{:.0} Hz, after rms {:.4} ~{:.0} Hz (distance {distance:.2})",
        library.sounds.len(), library.kind, hits.len(), before.rms, before.zero_cross_hz, after.rms, after.zero_cross_hz))
}

/// `sound-library-test`: every catalog instrument installed on this machine
/// lists its sound library, answers a search and loads one sound through
/// the real engine, proven by a readback and by the note sounding different.
pub fn sound_library_test() -> anyhow::Result<()> {
    use crate::sounds::Instrument;
    // A search the instrument's library should answer.
    let query = |id: &str| match id {
        "setbfree" => "jazz",
        "dexed" => "brass",
        "vitalium" => "pad",
        _ => "bass",
    };
    let clap = crate::host::list_clap_plugins();
    let lv2 = crate::lv2::discover();
    let (mut tested, mut failed) = (0, Vec::new());
    for entry in crate::catalog::CATALOG {
        let instrument = match entry.clap_id {
            Some(id) => clap.iter().any(|p| p.id == id).then(|| Instrument::Clap(id.to_string())),
            None => entry.lv2_uris.iter().find(|u| lv2.iter().any(|p| p.uri == **u)).map(|u| Instrument::Lv2(u.to_string())),
        };
        let Some(instrument) = instrument else {
            println!("  {:<9} not installed, skipped", entry.name);
            continue;
        };
        tested += 1;
        if let Instrument::Clap(id) = &instrument {
            let providers = crate::host::clap_preset_providers(id)?;
            println!("  {:<9} CLAP preset-discovery providers in its bundle: {}", entry.name,
                if providers.is_empty() { "none".to_string() } else { format!("{} ({})", providers.len(), providers.join(", ")) });
        }
        match sound_library_check(&instrument, query(entry.id)) {
            Ok(line) => println!("  {:<9} {line}", entry.name),
            Err(e) => {
                println!("  {:<9} FAILED: {e:#}", entry.name);
                failed.push(entry.name);
            }
        }
    }
    anyhow::ensure!(tested > 0, "no catalog instrument is installed");
    anyhow::ensure!(failed.is_empty(), "sound-library-test: {} of {tested} instrument(s) failed: {}", failed.len(), failed.join(", "));
    println!("sound-library-test: OK — {tested} instrument(s): library listed, searched, one sound loaded through the engine and heard");
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

/// `kabelsalat many-voices-test`: a rack with more CLAP instruments than the
/// old four-voice pool. Eight Surge XT units (plus Dexed and YANES when
/// installed) start without a voice; this thread hosts one per unit the way
/// the app does (`VoiceMsg::Add`, a `Grow` on the way), every unit must
/// sound, and retiring them hands every voice back to be freed here.
pub fn many_voices_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef, SURGE_PLUGIN_ID};
    use crate::host::{ClapVoice, ClapVoiceMain};
    use crate::matrix::SourceId;
    use std::time::Instant;
    const SR: f64 = 48000.0;
    const BLOCK: usize = 256;

    let installed = crate::host::list_clap_plugins();
    let mut plugins: Vec<(String, String)> = (1..=8).map(|i| (format!("Surge XT {i}"), SURGE_PLUGIN_ID.to_string())).collect();
    for name in ["Dexed", "YANES"] {
        match installed.iter().find(|p| p.name == name && p.features.iter().any(|f| f == "instrument")) {
            Some(p) => plugins.push((name.to_string(), p.id.clone())),
            None => println!("  {name}: not installed, skipped"),
        }
    }

    let mut g = Graph::default();
    g.reserve(64, 256);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    // (node, its cable into the master's left input)
    let mut units: Vec<(NodeId, u32)> = Vec::new();
    let mut edge = 1u32;
    for (name, id) in &plugins {
        let node = g.add_node(NodeKind::Clap { plugin_id: name_bytes(id) }, name_bytes(&name.to_uppercase()));
        g.connect_with_id(edge, PortRef { node: keys, port: 0 }, PortRef { node, port: 0 }, PortKind::Midi);
        g.connect_with_id(edge + 1, PortRef { node, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(edge + 2, PortRef { node, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        units.push((node, edge + 1));
        edge += 3;
    }

    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g.clone(), drain, SR, vec![]);
    let graph_pub = Arc::new(Mutex::new(GraphPub { graph: g, ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    let levels = engine.edge_levels_handle();
    let (mut tx, rx) = rtrb::RingBuffer::<VoiceMsg>::new(64);
    let (back_tx, mut back) = rtrb::RingBuffer::<VoiceBack>::new(64);
    engine.set_voice_channels(rx, back_tx);
    let mut buf = vec![0.0f32; BLOCK * 2];
    let bound = |p: &Arc<Mutex<GraphPub>>, id: u64| -> Option<NodeId> {
        p.lock().ok()?.voices.iter().find(|(v, _)| *v == id).map(|(_, n)| *n).filter(|n| *n != NodeId::MAX)
    };

    let mut mains: Vec<(u64, ClapVoiceMain)> = Vec::new();
    let table_cap = engine.voices.capacity();
    for (i, (name, id)) in plugins.iter().enumerate() {
        let t = Instant::now();
        let (voice, main) = ClapVoice::new_plugin(SR, id)?;
        let load_ms = t.elapsed().as_secs_f64() * 1000.0;
        let vid = voice.voice_id();
        // A larger table on the way, while four voices already play.
        if i == 4 {
            graph_pub.lock().map_err(|_| anyhow::anyhow!("graph publication poisoned"))?.reserve_voices(table_cap * 2);
            tx.push(VoiceMsg::Grow(Vec::with_capacity(table_cap * 2))).map_err(|_| anyhow::anyhow!("voice ring full"))?;
        }
        tx.push(VoiceMsg::Add(VoiceSlot::new(voice))).map_err(|_| anyhow::anyhow!("voice ring full"))?;
        engine.render(&mut buf, 2);
        let node = bound(&graph_pub, vid);
        anyhow::ensure!(node == Some(units[i].0), "{name}: voice {vid} bound to {node:?}, expected node {}", units[i].0);
        println!("  {name:<10} voice {vid:>2} -> node {:>2}   instantiate {load_ms:6.1} ms", units[i].0);
        mains.push((vid, main));
    }
    anyhow::ensure!(engine.voices.capacity() == table_cap * 2, "the larger voice table was not adopted");
    anyhow::ensure!(
        matches!(back.pop(), Ok(VoiceBack::Table(t)) if t.capacity() == table_cap),
        "the old voice table did not come back to be freed"
    );

    // Every unit holds a chord at once: per-voice peak at its master cable
    // and the cost of a block with all of them sounding.
    for note in [48u8, 55, 60, 64] {
        bus.send(Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys });
    }
    let blocks = (SR as usize * 3) / BLOCK;
    let mut peaks = vec![0.0f32; units.len()];
    let mut times_us: Vec<f64> = Vec::with_capacity(blocks);
    let mut master_peak = 0.0f32;
    for _ in 0..blocks {
        let t = Instant::now();
        engine.render(&mut buf, 2);
        times_us.push(t.elapsed().as_secs_f64() * 1e6);
        for (i, (_, cable)) in units.iter().enumerate() {
            peaks[i] = peaks[i].max(levels.load(*cable));
        }
        master_peak = master_peak.max(block_peak(&buf));
    }
    for note in [48u8, 55, 60, 64] {
        bus.send(Command::NoteOff { note, source: SourceId::Keys });
    }
    engine.render(&mut buf, 2);
    for (i, (name, _)) in plugins.iter().enumerate() {
        println!("  {name:<10} peak {:.4}", peaks[i]);
        anyhow::ensure!(peaks[i] > 1e-3, "{name} (node {}) is silent: peak {}", units[i].0, peaks[i]);
    }
    let errors: u64 = engine.voices.iter().map(|v| v.voice.process_errors()).sum();
    anyhow::ensure!(errors == 0, "{errors} failed CLAP process calls");
    anyhow::ensure!(master_peak <= 1.0, "master clipped: {master_peak}");
    times_us.sort_by(|a, b| a.total_cmp(b));
    let budget_us = BLOCK as f64 / SR * 1e6;
    let (median, p99, max) = (times_us[blocks / 2], times_us[blocks * 99 / 100], times_us[blocks - 1]);
    println!(
        "  render per {BLOCK}-frame block, {} voices on a 4-note chord, {blocks} blocks: median {median:.0} us, p99 {p99:.0} us, render_max_us {max:.0} (budget {budget_us:.0} us, {:.0}% at the median)",
        units.len(),
        median / budget_us * 100.0
    );

    // Retire: every voice comes back and is stopped and deactivated here.
    for (vid, _) in &mains {
        tx.push(VoiceMsg::Retire(*vid)).map_err(|_| anyhow::anyhow!("voice ring full"))?;
    }
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.voices.is_empty(), "{} voices left in the engine", engine.voices.len());
    anyhow::ensure!(graph_pub.lock().map(|p| p.voices.is_empty()).unwrap_or(false), "voices still published");
    let mut freed = 0;
    while let Ok(item) = back.pop() {
        let VoiceBack::Voice(slot) = item else { continue };
        let pos = mains.iter().position(|(vid, _)| *vid == slot.voice_id())
            .ok_or_else(|| anyhow::anyhow!("an unknown voice came back"))?;
        mains.remove(pos).1.retire(slot.into_voice());
        freed += 1;
    }
    anyhow::ensure!(mains.is_empty(), "{} voices never came back", mains.len());
    for _ in 0..8 {
        engine.render(&mut buf, 2);
    }
    anyhow::ensure!(block_peak(&buf) < 1e-4, "sound after every voice was retired");
    println!(
        "many-voices-test: OK — {} CLAP voices hosted on demand, all audible, table grown {table_cap} -> {}, {freed} retired and freed",
        units.len(),
        table_cap * 2
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
        target: CcTarget::PluginParam,
    });
    bus.send(Command::MatrixCcMap {
        source: SourceId::Hardware(0),
        dest: surge,
        cc: 7,
        param: Some(gain_p.id),
        target: CcTarget::PluginParam,
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

/// `kabelsalat main-controls-test`: for every installed instrument, print its
/// main controls and the rule that found them, then move the first one
/// through the engine and read the plugin's own value back.
pub fn main_controls_test() -> anyhow::Result<()> {
    let clap = crate::host::list_clap_plugins();
    let lv2 = crate::lv2::discover();
    let mut tested = 0;
    let mut failed: Vec<String> = Vec::new();
    let mut run = |name: &str, result: Option<anyhow::Result<()>>| match result {
        None => println!("{name}: not installed, skipped"),
        Some(Ok(())) => tested += 1,
        Some(Err(e)) => failed.push(format!("{name}: {e:#}")),
    };
    for name in ["Surge XT", "Dexed", "YANES"] {
        let id = clap.iter().find(|p| p.name.eq_ignore_ascii_case(name)).map(|p| p.id.clone());
        run(name, id.map(|id| clap_main_controls_check(name, &id)));
    }
    for (name, uri) in [("setBfree", "http://gareus.org/oss/lv2/b_synth"), ("Vitalium", "urn:distrho:vitalium")] {
        let installed = lv2.iter().any(|p| p.uri == uri);
        run(name, installed.then(|| lv2_main_controls_check(name, uri)));
    }
    anyhow::ensure!(failed.is_empty(), "main-controls-test FAILED:\n{}", failed.join("\n"));
    anyhow::ensure!(tested > 0, "main-controls-test: no instrument installed");
    println!("main-controls-test: OK ({tested} instruments)");
    Ok(())
}

fn print_main_controls(name: &str, found: &crate::main_controls::MainControls, mut text: impl FnMut(&crate::main_controls::MainControl) -> String) {
    if found.controls.is_empty() {
        println!("main-controls-test: {name}: no main controls (the plugin declares no usable parameter)");
        return;
    }
    println!("main-controls-test: {name}: {} main controls, source: {}", found.controls.len(), found.source.as_str());
    for (i, c) in found.controls.iter().enumerate() {
        let face = if i < crate::main_controls::FACE_MAIN_CONTROLS { "face" } else { "list" };
        println!(
            "  {}. [{face}] {:<28} page {:<24} id {:<10} = {} (range {}..{})",
            i + 1, c.name, format!("\"{}\"", c.page), c.id, text(c), c.min, c.max
        );
    }
}

/// Keys → instrument → Master, the smallest rig an instrument plays in.
fn instrument_graph(kind: crate::control::NodeKind) -> (Graph, NodeId) {
    use crate::control::{name_bytes, NodeKind, PortKind, PortRef};
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let inst = g.add_node(kind, name_bytes("INSTRUMENT"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: inst, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    (g, inst)
}

fn clap_main_controls_check(name: &str, plugin_id: &str) -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, NodeKind};
    let sr = 48000.0;
    let (voice, mut main) = ClapVoice::new_plugin(sr, plugin_id)?;
    let found = main.main_controls().clone();
    print_main_controls(name, &found, |c| {
        let v = main.param_value(c.id).unwrap_or(c.default);
        main.param_text(c.id, v)
    });
    let Some(c) = found.controls.first() else { return Ok(()) };
    let knob = voice.controller();
    let (g, inst) = instrument_graph(NodeKind::Clap { plugin_id: name_bytes(plugin_id) });
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, sr, vec![(inst, voice)]);
    let mut buf = vec![0.0f32; 256 * 2];
    let mut settle = |engine: &mut GraphEngine| for _ in 0..8 { engine.render(&mut buf, 2) };
    settle(&mut engine);
    let start = main.param_value(c.id).unwrap_or(f64::NAN);
    // What a face knob sends: the voice controller, applied in the next block.
    knob.set_param(clack_host::prelude::ClapId::new(c.id), c.plain(0.25));
    settle(&mut engine);
    let by_knob = main.param_value(c.id).unwrap_or(f64::NAN);
    // What a MIDI-learned knob does: a hardware CC through the matrix.
    bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: inst, cc: 20, param: Some(c.id), target: CcTarget::PluginParam });
    bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 20, value: 127 });
    settle(&mut engine);
    let by_cc = main.param_value(c.id).unwrap_or(f64::NAN);
    println!(
        "main-controls-test: {name}: moved '{}' (id {}): start {start:.4} -> knob at 25% {by_knob:.4} ({}) -> CC 127 {by_cc:.4} ({})",
        c.name, c.id, main.param_text(c.id, by_knob), main.param_text(c.id, by_cc)
    );
    let span = c.max - c.min;
    anyhow::ensure!((by_knob - c.plain(0.25)).abs() < span * 0.02, "knob did not move '{}': wanted {}, plugin reads {by_knob}", c.name, c.plain(0.25));
    anyhow::ensure!((by_cc - c.max).abs() < span * 0.02, "CC did not move '{}': wanted {}, plugin reads {by_cc}", c.name, c.max);
    if name == "Surge XT" {
        surge_macro_names_follow_the_patch(&mut main, &mut engine)?;
    }
    Ok(())
}

/// Surge names its macros per patch and announces nothing: the polls after
/// a state load must pick the new names up once the audio thread applied it.
fn surge_macro_names_follow_the_patch(main: &mut crate::host::ClapVoiceMain, engine: &mut GraphEngine) -> anyhow::Result<()> {
    let patches = crate::host::list_surge_patches();
    let Some(patch) = patches.iter().find(|p| p.name.eq_ignore_ascii_case("bell pad")) else {
        println!("main-controls-test: Surge XT: factory patch 'Bell Pad' not installed, patch-reload check skipped");
        return Ok(());
    };
    let names = |main: &crate::host::ClapVoiceMain| -> Vec<String> { main.main_controls().face().iter().map(|c| c.name.clone()).collect() };
    let before = names(main);
    main.load_state(&crate::host::fxp_to_state(&std::fs::read(&patch.path)?)?)?;
    let mut buf = vec![0.0f32; 256 * 2];
    let mut changed = false;
    for _ in 0..10 {
        for _ in 0..8 { engine.render(&mut buf, 2); }
        changed |= main.poll_main_controls();
    }
    let after = names(main);
    println!("main-controls-test: Surge XT: patch '{}/{}' loaded: face controls {before:?} -> {after:?}", patch.category, patch.name);
    anyhow::ensure!(changed && after.iter().any(|n| n.starts_with("M1: ")), "Surge XT main controls did not follow the patch's named macros");
    anyhow::ensure!(!main.poll_main_controls(), "a settled list must not report a change");
    Ok(())
}

fn lv2_main_controls_check(name: &str, uri: &str) -> anyhow::Result<()> {
    use crate::control::{command_bus, Command, Lv2Uri, NodeKind};
    let sr = 48000.0;
    let found = crate::lv2::main_controls_for_uri(uri);
    print_main_controls(name, &found, |c| format!("{:.4} (default)", c.default));
    let Some(c) = found.controls.first() else { return Ok(()) };
    let (g, inst) = instrument_graph(NodeKind::Lv2 { uri: Lv2Uri::new(uri) });
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);
    bus.send(Command::LoadGraph { graph: g });
    let mut buf = vec![0.0f32; 512 * 2];
    let mut settle = |engine: &mut GraphEngine| for _ in 0..8 { engine.render(&mut buf, 2) };
    settle(&mut engine);
    let start = engine.lv2_control_value(inst, c.id as usize).ok_or_else(|| anyhow::anyhow!("{uri} did not load in the engine"))?;
    let norm = if c.normalized(start as f64) < 0.5 { 0.9 } else { 0.1 };
    // What a face knob sends, and what a CC bound to it applies.
    bus.send(Command::SetParam { node: inst, param: c.id, value: norm as f64 });
    settle(&mut engine);
    let moved = engine.lv2_control_value(inst, c.id as usize).unwrap_or(f32::NAN);
    println!("main-controls-test: {name}: moved '{}' (control {}): start {start:.4} -> knob at {:.0}% {moved:.4}", c.name, c.id, norm * 100.0);
    anyhow::ensure!((moved as f64 - c.plain(norm)).abs() < (c.max - c.min) * 1e-3, "SetParam did not move '{}': wanted {}, plugin port reads {moved}", c.name, c.plain(norm));
    Ok(())
}

/// `kabelsalat meter-test`: render REAL audio through `GraphEngine::render`
/// (a Surge voice playing notes, as the existing engine self-tests do) and
/// check the meter reads sane loudness/bands/true-peak against the signal,
/// plus the reset path. Modeled on `record-test` (rule 37: temp only).
pub fn meter_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;

    const SR: f64 = 48000.0;
    const BLOCK: usize = 256;

    let (voice, _main) = ClapVoice::new(SR)?;
    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SYNTH"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, vec![(synth, voice)]);

    // Attach a publication handle so the render path exercises `publish_meter`.
    let graph_pub = std::sync::Arc::new(std::sync::Mutex::new(GraphPub {
        rev: 0, graph: crate::control::Graph::default(), rules: Vec::new(), ..Default::default()
    }));
    engine.set_graph_pub(graph_pub.clone());

    let mut buf = vec![0.0f32; BLOCK * 2];
    engine.render(&mut buf, 2); // Surge applies its init patch on the audio thread

    // 1. Held chord for ~1.5 s: the meter (momentary, short-term) must read
    //    above the silent floor, and bands 2/3 (> 500 Hz) should carry energy.
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOn { note: n, velocity: 0.9, source: SourceId::Keys });
    }
    for _ in 0..(1.5 * SR / BLOCK as f64) as usize {
        engine.render(&mut buf, 2);
    }
    let snap = engine.meter_snapshot();
    anyhow::ensure!(
        snap.momentary_lufs > -60.0 && snap.short_term_lufs > -60.0,
        "a held chord must read above silence: momentary {}, short-term {}",
        snap.momentary_lufs, snap.short_term_lufs
    );
    anyhow::ensure!(
        snap.band_db[2] > snap.band_db[0].max(snap.band_db[1]) - 10.0 || snap.band_db[3] > snap.band_db[0],
        "a synth chord should have HF energy: bands {:?}",
        snap.band_db
    );

    // 2. True peak must be finite (the limiter caps the sum, so ≥ -1 dBFS-ish).
    anyhow::ensure!(
        snap.true_peak_db.is_finite() && snap.true_peak_db > -60.0,
        "true peak {} should be a real in-range reading", snap.true_peak_db
    );

    // 3. Integrated loudness accumulates once above the absolute gate.
    anyhow::ensure!(
        snap.integrated_lufs.is_finite() && snap.integrated_lufs > -60.0,
        "integrated loudness should have gated data: {}", snap.integrated_lufs
    );

    // 4. Reset clears the integration (then silence leaves it NaN).
    for &n in &[60u8, 64, 67] {
        bus.send(Command::NoteOff { note: n, source: SourceId::Keys });
    }
    for _ in 0..8 { engine.render(&mut buf, 2); }
    bus.send(Command::ResetMeter);
    engine.render(&mut buf, 2);
    let after_reset = engine.meter_snapshot();
    anyhow::ensure!(
        after_reset.integrated_lufs.is_nan(),
        "reset must clear the integration gate: {}", after_reset.integrated_lufs
    );
    anyhow::ensure!(
        graph_pub.lock().unwrap().meter_rev > 0,
        "the meter must publish through GraphPub"
    );

    println!(
        "meter-test: OK — chord momentary {:.2} LUFS / short-term {:.2} / integrated {:.2}, true peak {:.2} dBTP, bands {:?}, low-end corr {:.2}",
        snap.momentary_lufs, snap.short_term_lufs, snap.integrated_lufs,
        snap.true_peak_db, snap.band_db, snap.low_end_corr
    );
    Ok(())
}
