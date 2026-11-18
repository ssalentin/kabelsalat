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
    name_str, split_port, AudioInChannel, Command, CommandDrain, DspKind, Graph, MidiFxPlugin,
    NodeId, NodeKind, ParamId, PortKind, RigSnapshot,
};
use crate::dsp::{build_processor, Processor};
use crate::host::ClapVoice;
use crate::matrix::{CcMapping, CcTarget, Matrix, MidiMsg, Rule, SourceId};
use crate::capture::SystemCapture;
use crate::meter::{MasterMeter, MeterSnapshot};
use crate::player::{read_frame, PlayerCfg, PlayerData};
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

/// A built-in MIDI-FX unit's TRANSPOSE knob, in semitones −24..=24 mapped to
/// 0..1 (`SetParam` on the MidiFx node). CLAP/LV2 MIDI-FX units have no
/// built-in knobs — their knobs are the plugin's main controls (rule 89).
pub const P_MIDIFX_SEMI: ParamId = 67;

/// A built-in transposer's knob value in semitones (TRANSPOSE is ±24).
pub const MIDIFX_SEMI_MAX: i8 = 24;

/// The arp unit's knobs (issue #90): mode, rate, octaves, gate, swing and
/// the chord shape of chord mode. Normalized 0..1 like every rack knob; the
/// stepped ones quantize in `arp::ArpState::set_param` (and on the face).
pub use crate::arp::{P_ARP_CHORD, P_ARP_GATE, P_ARP_MODE, P_ARP_OCTAVE, P_ARP_RATE, P_ARP_SWING};

/// One rack-unit knob: the label on the face (canonical for agents too),
/// its default, the engine param it drives (None = local-only), and the long
/// names agents may use instead of the label.
pub struct KnobSpec {
    pub label: &'static str,
    pub default: f32,
    pub param: Option<ParamId>,
    /// Not drawn as a knob (the face renders it as a chip); still bindable.
    pub hidden: bool,
    /// > 1 = a stepped knob: the face snaps the needle to this many evenly
    /// spaced positions, and `CcScale::stepped` maps a controller onto them.
    /// The engine quantizes defensively on apply.
    pub steps: u8,
    pub aliases: &'static [&'static str],
}

const fn ks(label: &'static str, default: f32, param: Option<ParamId>, aliases: &'static [&'static str]) -> KnobSpec {
    KnobSpec { label, default, param, hidden: false, steps: 0, aliases }
}

/// A stepped knob (mode/rate-style pickers): `steps` evenly spaced positions.
const fn ks_steps(label: &'static str, default: f32, param: Option<ParamId>, steps: u8, aliases: &'static [&'static str]) -> KnobSpec {
    KnobSpec { label, default, param, hidden: false, steps, aliases }
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
        NodeKind::MasterOut => knobs!(ks("VOLUME", 0.75, Some(P_VOL), &["VOL"]), ks("WARM", 0.15, Some(P_WARM), &["WARMTH"])),
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
        // setBfree has no control ports: its face drives MIDI CCs (`organ`).
        // Drawbars are hidden knobs, painted as faders.
        k @ NodeKind::Lv2 { .. } if crate::organ::is_organ(k) => {
            const fn bar(label: &'static str, i: usize, out: bool, aliases: &'static [&'static str]) -> KnobSpec {
                KnobSpec { label, default: if out { 1.0 } else { 0.0 }, param: Some(crate::organ::drawbar_param(i)), hidden: true, steps: 9, aliases }
            }
            knobs!(
                bar("16'", 0, true, &["DRAWBAR 16", "DRAWBAR 1"]),
                bar("5⅓'", 1, true, &["DRAWBAR 5 1/3", "DRAWBAR 2"]),
                bar("8'", 2, true, &["DRAWBAR 8", "DRAWBAR 3"]),
                bar("4'", 3, false, &["DRAWBAR 4", "DRAWBAR 4TH"]),
                bar("2⅔'", 4, false, &["DRAWBAR 2 2/3", "DRAWBAR 5"]),
                bar("2'", 5, false, &["DRAWBAR 2'", "DRAWBAR 6"]),
                bar("1⅗'", 6, false, &["DRAWBAR 1 3/5", "DRAWBAR 7"]),
                bar("1⅓'", 7, false, &["DRAWBAR 1 1/3", "DRAWBAR 8TH"]),
                bar("1'", 8, false, &["DRAWBAR 1'", "DRAWBAR 9"]),
                ks("LESLIE", 0.5, Some(crate::organ::P_LESLIE), &["ROTARY", "SPEAKER"]),
                ks("PERC", 0.0, Some(crate::organ::P_PERC), &["PERCUSSION"]),
                ks("DRIVE", 0.0, Some(crate::organ::P_DRIVE), &["OVERDRIVE"]),
                ks("REVERB", 0.2, Some(crate::organ::P_REVERB), &[]),
            )
        }
        // An LV2 unit's knobs are its plugin's main controls (`main_controls`):
        // param 0 is whatever port the plugin lists first, never a "gain".
        NodeKind::Lv2 { .. } => &[],
        // Starts at 0 like the engine's system_gain: a fresh unit stays muted.
        NodeKind::SystemAudio => knobs!(ks("GAIN", 0.0, Some(P_SYSTEM_GAIN), &[])),
        // A hosted MIDI-FX unit's own knobs are the plugin's main controls
        // (rule 89); the BUILTIN transposer's face shows SEMIS.
        NodeKind::MidiFx { plugin } => match plugin {
            MidiFxPlugin::Builtin { .. } => knobs!(ks("SEMIS", 0.5, Some(P_MIDIFX_SEMI), &["TRANSPOSE", "SEMI", "SEMITONES"])),
            _ => &[],
        },
        // The arp's six knobs (issue #90). MODE/RATE/OCT/CHORD are stepped —
        // the face snaps the needle, and `ArpState::set_param` quantizes.
        NodeKind::Arp => knobs!(
            ks_steps("MODE", 0.0, Some(P_ARP_MODE), 5, &["ORDER", "PATTERN"]),
            ks_steps("RATE", 0.6, Some(P_ARP_RATE), 6, &["SPEED"]), // position 3 = 1/16
            ks_steps("OCT", 0.0, Some(P_ARP_OCTAVE), 4, &["OCTAVES", "RANGE"]),
            ks("GATE", 0.75, Some(P_ARP_GATE), &["LENGTH"]),
            ks("SWING", 0.0, Some(P_ARP_SWING), &["SHUFFLE"]),
            ks_steps("CHORD", 0.0, Some(P_ARP_CHORD), 8, &["SHAPE", "CHORD SHAPE"]),
        ),
        NodeKind::AudioIn { .. } => knobs!(
            ks("GAIN", 0.5, Some(P_AUDIO_IN_GAIN), &[]),
            KnobSpec { label: "MON", default: 0.0, param: Some(P_AUDIO_IN_MONITOR), hidden: true, steps: 0, aliases: &["MONITOR"] },
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

/// Input events a MIDI-FX unit may hold between blocks (a full queue drops,
/// never allocates).
const FX_QUEUE_CAP: usize = 256;
/// Output events one unit may emit per block (one shared engine scratch
/// buffer; a full one drops).
const FX_OUT_CAP: usize = 256;

/// A hosted MIDI-FX node: the unit the matrix routes events through (AUM
/// MIDI strips). The input queue is filled by `apply` at block boundaries
/// and drained by `pump_midi_fx` on the audio thread — allocation-free
/// both sides. Without a live processor (still loading, load failed, or a
/// built-in unit) events pass through unprocessed: the rack stays playable
/// and note-offs always flow (rule 41).
struct MidiFxSlot {
    node: NodeId,
    /// The hosted plugin. `None` = pass-through.
    proc: Option<MidiFxProc>,
    /// The unit's raw-MIDI input queue (preallocated, cleared per pump).
    in_q: Vec<MidiMsg>,
    /// BUILT-IN transposer: the TRANSPOSE knob's semitones (CLAP/LV2 units
    /// ignore this and process with their own plugin).
    semis: i8,
    /// `release_all` recursion guard (a wired FX1 → FX2 → FX1 loop).
    releasing: bool,
    /// Knob values sent before the plugin is live (rule 113): fixed capacity,
    /// replayed by `attach_midi_fx_lv2`, never grown on the audio thread.
    pending: Vec<(ParamId, f32)>,
}

enum MidiFxProc {
    /// The loaded LV2 plugin, in the loader's own slot (so its retirement
    /// goes back through `retire_lv2`, never a free on the audio thread).
    Lv2(Lv2Slot),
}

/// Knob values one unit may hold while its plugin loads.
const FX_PENDING_CAP: usize = 16;

impl MidiFxSlot {
    fn new(node: NodeId, semis: i8) -> Self {
        Self {
            node,
            proc: None,
            in_q: Vec::with_capacity(FX_QUEUE_CAP),
            semis,
            releasing: false,
            pending: Vec::with_capacity(FX_PENDING_CAP),
        }
    }

    /// Queue one event (allocation-free; a full queue drops the event).
    fn queue(&mut self, msg: MidiMsg) {
        if self.in_q.len() < FX_QUEUE_CAP {
            self.in_q.push(msg);
        }
    }
}

/// Build the slot for a node (rare structural op — allocating here is fine).
fn midi_fx_slot(n: &crate::control::Node) -> MidiFxSlot {
    let semis = match &n.kind {
        NodeKind::MidiFx { plugin: MidiFxPlugin::Builtin { semis } } => *semis,
        _ => 0,
    };
    MidiFxSlot::new(n.id, semis)
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

/// One arp state per Arp node (rare structural op; capacity 8 pre-reserved
/// so runtime `AddNode`s within it never allocate).
fn build_arp_slots(graph: &Graph) -> Vec<crate::arp::ArpState> {
    let mut v = Vec::with_capacity(8);
    for n in &graph.nodes {
        if matches!(n.kind, NodeKind::Arp) {
            v.push(crate::arp::ArpState::new(n.id));
        }
    }
    v
}

/// Work the audio thread hands to the LV2 loader thread: instantiating a
/// plugin (~200 ms for Vitalium), parsing its presets and freeing an
/// instance all block far longer than one audio buffer.
enum Lv2Request {
    Load { node: NodeId, uri: crate::control::Lv2Uri, gen: u64 },
    Preset { node: NodeId, uri: crate::control::Lv2Uri, preset_uri: String },
    /// A fresh instance with this state restored, to replace the node's.
    State { node: NodeId, uri: crate::control::Lv2Uri, gen: u64, state: Vec<u8> },
    Retire(Lv2Slot),
    RetirePreset(crate::lv2::Lv2Preset),
}

/// What the loader hands back; attached at the next block boundary.
enum Lv2Response {
    Loaded { node: NodeId, gen: u64, slot: Lv2Slot },
    Preset { node: NodeId, preset: crate::lv2::Lv2Preset },
    Restored { node: NodeId, gen: u64, slot: Lv2Slot },
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

/// A new instance of `uri` with `state` restored: checked first, because a
/// state the plugin cannot read can hang the thread restoring it
/// (`vital::check_patch`).
fn restored_lv2_slot(node: NodeId, uri: &str, state: &[u8], sample_rate: f64) -> Option<Lv2Slot> {
    let Some(format) = crate::lv2::state_format(uri) else {
        eprintln!("lv2 node {node}: no known state format for {uri}");
        return None;
    };
    if let Err(e) = (format.check)(state) {
        eprintln!("lv2 node {node}: {e}");
        return None;
    }
    let mut slot = load_lv2_slot(node, uri, sample_rate)?;
    match slot.plugin.restore_state(&[(format.key, format.kind, state)]) {
        Ok(()) => Some(slot),
        Err(e) => {
            eprintln!("lv2 node {node}: {e}");
            crate::lv2::retire(slot.plugin);
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
            Lv2Request::State { node, uri, gen, state } => {
                restored_lv2_slot(node, uri.as_str(), &state, sample_rate).map(|slot| Lv2Response::Restored { node, gen, slot })
            }
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

/// One source node's strip output gains (L, R), or None when the node has no
/// strip state (unity — the common case, no work at all). Free function on
/// field references: the render loops hold a mutable borrow of the slot while
/// the strip table and graph are read. Constant-power pan like the rig bus:
/// gl = sqrt(1-pan), gr = sqrt(1+pan).
fn strip_gains_of(
    strips: &[(NodeId, StripMix)],
    graph: &Graph,
    node: NodeId,
    solo_rigs: &[bool; 8],
) -> Option<(f32, f32)> {
    // A source nobody touched has no entry but is still silenced by another
    // source's solo in its rig (rule 134).
    let m = strips.iter().find(|(n, _)| *n == node).map(|(_, m)| *m).unwrap_or(StripMix::DEFAULT);
    let rig = graph.node(node).map(|n| n.rig as usize).unwrap_or(0).min(7);
    if m.is_default() && !solo_rigs[rig] { return None; }
    let g = if m.mute || (solo_rigs[rig] && !m.solo) { 0.0 } else { 1.0 };
    Some((g * (1.0 - m.pan).sqrt(), g * (1.0 + m.pan).sqrt()))
}

/// Is this FX node bypassed (#74)? Allocation-free linear scan.
fn strip_bypassed(strips: &[(NodeId, StripMix)], node: NodeId) -> bool {
    strips.iter().any(|(n, m)| *n == node && m.bypass)
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

/// How many per-node strip states the engine tracks (and publishes). The
/// architecture's fixed RT limit is 64 nodes; the table is allocated once in
/// the constructor and never grows — an entry past it is dropped.
pub const MAX_STRIP_MIXES: usize = 64;

/// How many master-insert FX the render path walks per block (#77). A longer
/// chain than this keeps playing but its tail is ignored.
pub const MAX_MASTER_INSERTS: usize = 8;

/// One source strip's engine state (#73): pan and mute/solo applied to the
/// node's own output (post LEVEL — the strip fader), plus the insert bypass
/// flag (#74) an FX node carries in the same table. Solo scope is WITHIN the
/// node's rig. Unity by default.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StripMix {
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub bypass: bool,
}

impl StripMix {
    pub const DEFAULT: Self = Self { pan: 0.0, mute: false, solo: false, bypass: false };
    /// True when nothing differs from a node the table never heard of.
    pub fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }
}

impl Default for StripMix {
    fn default() -> Self { Self::DEFAULT }
}

/// One entry of the published strip table (`GraphPub::strips`). An empty slot
/// carries `node == NodeId::MAX`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripPub {
    pub node: u32,
    pub mix: StripMix,
}

impl Default for StripPub {
    fn default() -> Self { Self { node: u32::MAX, mix: StripMix::DEFAULT } }
}

impl StripPub {
    pub const EMPTY: Self = Self { node: u32::MAX, mix: StripMix::DEFAULT };
}

impl Default for GraphPub {
    /// `strips` needs a manual impl: derived `Default` only covers arrays
    /// up to 32 elements.
    fn default() -> Self {
        Self {
            rev: 0,
            graph: Graph::default(),
            rules: Vec::new(),
            mix: [RigMix::DEFAULT; 8],
            strips: [StripPub::EMPTY; MAX_STRIP_MIXES],
            system_gain: 0.0,
            mix_rev: 0,
            voices: Vec::new(),
            voices_rev: 0,
            transport: TransportPub::default(),
            transport_rev: 0,
            player: PlayerPub::default(),
            player_rev: 0,
            meter: MeterSnapshot::default(),
            meter_rev: 0,
        }
    }
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

/// Published file-player state (issue #84): what the engine is playing and
/// how — frontends (UI mirror, bridge get_status, MCP) read it under
/// `player_rev`. Copied under `try_lock`, zero allocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerPub {
    /// A file is loaded.
    pub loaded: bool,
    /// The play-enable (AUM's circular toggle) is on.
    pub enabled: bool,
    /// Enabled but waiting for the bar line (sync on, transport rolling).
    pub pending: bool,
    /// Sounding right now (enabled, started, not finished).
    pub playing: bool,
    pub loop_on: bool,
    pub sync: bool,
    /// Number of Beats setting (0 = unset).
    pub beats: f32,
    /// Beat Offset setting.
    pub offset: f32,
    /// Original BPM setting (adopted from "nnn bpm" in the filename on load).
    pub bpm: f32,
    /// The tempo the file is played against: beats-derived while loop+sync
    /// are on, otherwise the original BPM.
    pub eff_bpm: f32,
    /// File length in seconds.
    pub duration: f32,
    /// Mix gain (0..1).
    pub gain: f32,
}

impl Default for PlayerPub {
    fn default() -> Self {
        Self {
            loaded: false,
            enabled: false,
            pending: false,
            playing: false,
            loop_on: false,
            sync: false,
            beats: 0.0,
            offset: 0.0,
            bpm: crate::player::DEFAULT_BPM,
            eff_bpm: 0.0,
            duration: 0.0,
            gain: 0.0,
        }
    }
}

/// Shared graph publication for frontends (MCP→UI live sync).
/// The engine writes it ONLY on structural ops that actually changed the
/// graph (loads, add/remove node, connect/disconnect) at block boundaries;
/// the steady render path never locks it. `rev` lets the UI skip unchanged
/// snapshots cheaply; duplicate Connects (the UI's per-frame mirror) do NOT
/// bump it, so UI-originated edits never trigger a rebuild feedback loop.
#[derive(Clone, Debug)]
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
    /// Per-node strip state (#73/#74): pan/mute/solo on sources, bypass on
    /// FX nodes — same publication discipline as `mix` (fixed-size copy).
    pub strips: [StripPub; MAX_STRIP_MIXES],
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
    /// File-player state (loop/sync/enable), bumped by `player_rev`.
    pub player: PlayerPub,
    pub player_rev: u64,
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

/// The strip table as `GraphPub` publishes it: a fixed array (default =
/// empty slots), so the audio thread never allocates for a strip edit.
/// Public for tests that need to fake an engine publication.
pub fn strips_pub_table(strips: &[(NodeId, StripMix)]) -> [StripPub; MAX_STRIP_MIXES] {
    let mut out = [StripPub::EMPTY; MAX_STRIP_MIXES];
    for (i, (node, mix)) in strips.iter().take(MAX_STRIP_MIXES).enumerate() {
        out[i] = StripPub { node: *node, mix: *mix };
    }
    out
}

impl GraphPub {
    /// Make room for `n` published voices. Called off the audio thread by
    /// whoever raises the engine's voice capacity.
    pub fn reserve_voices(&mut self, n: usize) {
        self.voices.reserve(n.saturating_sub(self.voices.len()));
    }
}

/// What a played note picks up between a Keys node and its destination: the
/// zone's octave shift and the Keys node's scale snap (issue #90).
#[derive(Clone, Copy)]
struct KeysRoute {
    shift: i16,
    scale: Option<crate::arp::ScaleChoice>,
}

/// The loaded file and its runtime clock. Plain engine-owned state: no
/// locks, no atomics on the render path.
struct PlayerFile {
    /// Interleaved stereo at the file's own rate, shared with the host
    /// (freed only through the retire ring).
    samples: Arc<[f32]>,
    frames: usize,
    file_sr: f64,
    duration: f64,
    /// Play-enable (AUM's circular toggle).
    enabled: bool,
    /// Enabled but waiting for the bar line (sync on, transport active).
    pending: bool,
    /// Song beat the pending start waits for.
    target_beats: f64,
    /// Play position in file frames. Authoritative in free-run; the sync
    /// path re-derives it from the anchors every frame and mirrors it here.
    pos: f64,
    /// The song-clock mapping: file beats ↔ song beats while synced.
    anchor_song_beats: f64,
    anchor_file_beats: f64,
    /// A one-shot (loop off) played past its end.
    finished: bool,
}

/// Start sounding from the configured point (Beat Offset), anchored to the
/// song clock at `song_beats`. Returns the start position in file frames.
fn player_start_now(p: &mut PlayerFile, cfg: PlayerCfg, song_beats: f64) -> f64 {
    let ref_bpm = cfg.ref_bpm(p.duration);
    let start_beats = cfg.start_beats(ref_bpm, p.duration);
    let pos = start_beats * 60.0 / ref_bpm * p.file_sr;
    p.pending = false;
    p.finished = false;
    p.pos = pos;
    p.anchor_file_beats = start_beats;
    p.anchor_song_beats = song_beats;
    pos
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
    /// Organ (setBfree) knob values that arrived before the node's LV2
    /// instance was live (it loads on its own thread, rule 46); replayed by
    /// `attach_lv2`. Fixed size: no allocation on the audio thread.
    organ_pending: [Option<OrganPending>; 8],
    /// Control values that arrived before ANY still-loading LV2 instance
    /// was live (`Lv2Pending`); replayed by `attach_lv2` (rule 113).
    lv2_pending: [Option<Lv2Pending>; 8],
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
    /// MIDI clock send (#85): the song position, published every block.
    clock_pos: Option<Arc<crate::clocksend::ClockPos>>,
    out_peak: Arc<AtomicU32>,
    // Per-edge audio levels (source node's block peak), published per block.
    edge_levels: Arc<EdgeLevels>,
    /// Render time per voice and effect (Engine Info), written per block.
    node_timer: crate::engine_info::NodeTimer,
    // MIDI routing matrix (source → instrument: channel/transpose/ranges).
    // Default rules mirror "every source plays every instrument"; keyboard
    // zones are NOT matrix rules — a split Keys node's LOW/HIGH ports gate
    // notes by graph wiring (keys_gate).
    matrix: Matrix,
    route_out: Vec<(NodeId, MidiMsg)>,
    // Hosted MIDI-FX units (AUM MIDI strips): the event path source →
    // MIDI-FX → rig input. `apply` queues into the slots at block
    // boundaries; `pump_midi_fx` (render) processes + routes their output.
    midi_fx: Vec<MidiFxSlot>,
    /// Scratch: raw MIDI bytes queued into one CLAP MIDI-FX per pump.
    fx_in: Vec<[u8; 3]>,
    /// Scratch: output events one MIDI-FX emitted, before routing.
    fx_out: Vec<crate::lv2::FxOutEvent>,
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
    /// Per-node strip state (SetStripMix/SetStripBypass): pan/mute/solo on
    /// sources, bypass on FX nodes. Capacity is fixed (MAX_STRIP_MIXES): a
    /// node past it keeps playing at unity. Linear scans, like the rest.
    strips: Vec<(NodeId, StripMix)>,
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
    // File player (backing track) mixed into the master. The decoded file
    // arrives through `player_rx` (parse happens on the decode thread); the
    // engine owns the clock and reads the buffer directly — RT-safe, no
    // locks. Retired buffers leave through `player_back` (dropping a
    // several-MB allocation on the audio thread would stall it).
    player: Option<PlayerFile>,
    player_cfg: PlayerCfg,
    player_gain: f32,
    player_buf: Vec<f32>,
    /// Decode thread → audio: one decoded file per `play_file`.
    player_rx: Option<rtrb::Consumer<PlayerData>>,
    /// Audio → host: buffers to free off the audio thread.
    player_back: Option<rtrb::Producer<Arc<[f32]>>>,
    /// Retires waiting while the return ring is full (preallocated).
    player_backlog: Vec<Arc<[f32]>>,
    /// The player state changed since its last publication.
    player_dirty: bool,
    // Audio In nodes (live mic/guitar/interface): one slot per node, rings fed
    // by input callbacks off the audio thread (rule 46).
    audio_in: Vec<AudioInSlot>,
    audio_in_levels: Arc<AudioInLevels>,
    // Arp nodes (issue #90): one state per node, stepped from render and
    // fed by the note commands. Capacity 8 pre-reserved so an AddNode
    // within it never allocates (rare op beyond, like the matrix).
    arp: Vec<crate::arp::ArpState>,
    /// Ids of the Keys → synth cables the last arps took over (fixed size, no
    /// allocation). The UI's per-frame mirror re-sends every cable of ITS
    /// rack until it adopts the new graph; a Connect of one of these ids
    /// would plug the keys straight back past the arp (rule 80 class).
    /// Cleared by a load.
    arp_taken: [crate::control::EdgeId; 8],
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
    // Record-arm state: master mix + up to 8 rig stems.
    arm_master: bool,
    arm_rigs: [bool; 8],
    // Bar-quantized start/stop when record is pressed while transport rolls:
    // Some((true, beat)) = start at `beat`, Some((false, beat)) = stop at `beat`.
    record_pending: Option<(bool, f64)>,
    // Preallocated scratch buffer for each rig's post-fader sum (RT-safe).
    rig_buf: Vec<f32>,
    /// The audio judge's master tap (#35): pushes only while a listen records.
    listen: Option<crate::listen::Tap>,
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
        let arp = build_arp_slots(&graph);
        // Default routing: every source class feeds every rig (full range).
        let mut matrix = Matrix::new();
        matrix.reserve(64);
        // Routes belong to the Clap node, not to the voice that plays it: a
        // voice hosted after startup finds its node already routed.
        // An arp is a MIDI destination like an instrument (issue #90).
        for n in graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Clap { .. } | NodeKind::Arp)) {
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
        let midi_fx = graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::MidiFx { .. }))
            .map(midi_fx_slot)
            .collect();
        Self {
            graph,
            drain,
            sample_rate,
            voices,
            dsp,
            lv2,
            lv2_in: vec![0.0; 8192 * 2],
            fx_done: Vec::with_capacity(256),
            master_vol: 0.75,
            master_warm: 0.15,
            master_lim: MasterLimiter::new(sample_rate),
            organ_pending: [None; 8],
            lv2_pending: [None; 8],
            transport: Transport::stopped(96.0),
            master_clock: MasterClock::Stopped,
            count_in: CountInConfig::default(),
            count_in_slot: SlotTransport::default(),
            count_in_buf: vec![0.0; 8192 * 2],
            last_count_in_beat: 0,
            transport_dirty: false,
            clock_pos: None,
            out_peak: Arc::new(AtomicU32::new(0)),
            edge_levels: Arc::new(EdgeLevels::new()),
            node_timer: crate::engine_info::NodeTimer::new(),
            matrix,
            route_out: Vec::with_capacity(route_cap),
            midi_fx,
            fx_in: Vec::with_capacity(FX_QUEUE_CAP),
            fx_out: Vec::with_capacity(FX_OUT_CAP),
            #[cfg(test)]
            note_log: Vec::new(),
            #[cfg(test)]
            cc_log: Vec::new(),
            #[cfg(test)]
            cc_passthrough_log: Vec::new(),
            rig_meta: Vec::new(),
            active_rig: 0,
            rig_mix: [RigMix::DEFAULT; 8],
            strips: Vec::with_capacity(MAX_STRIP_MIXES),
            mix_dirty: false,
            rig_snapshot: None,
            graph_pub: None,
            system_capture: None,
            system_gain: 0.0,
            system_buf: vec![0.0; 8192 * 2],
            player: None,
            player_cfg: PlayerCfg::default(),
            player_gain: 0.0,
            player_buf: vec![0.0; 8192 * 2],
            player_rx: None,
            player_back: None,
            player_backlog: Vec::with_capacity(8),
            player_dirty: false,
            audio_in,
            audio_in_levels: Arc::new(AudioInLevels::new()),
            arp,
            arp_taken: [crate::control::EdgeId::MAX; 8],
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
            arm_master: true,
            arm_rigs: [false; 8],
            record_pending: None,
            rig_buf: vec![0.0; 8192 * 2],
            listen: None,
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
        if self.is_midi_fx(node) {
            self.attach_midi_fx_lv2(slot);
            return;
        }
        let inst = slot.plugin.is_instrument();
        self.lv2.push(slot);
        // The face's organ values sent while the instance was loading.
        if let Some(i) = self.organ_pending.iter().position(|p| p.is_some_and(|p| p.node == node)) {
            let p = self.organ_pending[i].take().unwrap();
            if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                for k in 0..crate::organ::PARAMS {
                    if p.set & (1 << k) == 0 { continue; }
                    if let Some((msgs, n)) = crate::organ::ccs(crate::organ::P_BASE + k as ParamId, p.values[k]) {
                        for &(cc, val) in &msgs[..n] { s.plugin.control_change(cc, val); }
                    }
                }
            }
        }
        // Control values any still-loading LV2 node was sent (rule 113).
        if let Some(i) = self.lv2_pending.iter().position(|p| p.is_some_and(|p| p.node == node)) {
            let p = self.lv2_pending[i].take().unwrap();
            if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
                for &(ordinal, v) in p.pairs.iter().take(p.count) {
                    s.plugin.set_control_normalized(ordinal as usize, v);
                }
            }
        }
        if inst {
            // Units wired to this instrument route to it from now on.
            for k in 0..self.midi_fx.len() {
                let fx = self.midi_fx[k].node;
                self.route_midi_fx_outputs(fx);
            }
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

    /// Put an instance with restored state in place of the node's own.
    fn adopt_restored_lv2(&mut self, node: NodeId, gen: u64, slot: Lv2Slot) {
        let live = gen == self.lv2_gen && matches!(self.graph.node(node).map(|n| &n.kind), Some(NodeKind::Lv2 { .. }));
        match self.lv2.iter().position(|s| s.node == node) {
            Some(i) if live => {
                let old = std::mem::replace(&mut self.lv2[i], slot);
                self.retire_lv2(old);
            }
            None if live && self.lv2.len() < self.lv2.capacity() => {
                self.attach_lv2(slot);
                self.publish_graph();
            }
            _ => self.retire_lv2(slot),
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
                        && matches!(self.graph.node(node).map(|n| &n.kind), Some(NodeKind::Lv2 { .. } | NodeKind::MidiFx { .. }))
                        && !self.lv2.iter().any(|s| s.node == node)
                        && self.lv2.len() < self.lv2.capacity();
                    if wanted {
                        self.attach_lv2(slot);
                        self.publish_graph();
                    } else {
                        self.retire_lv2(slot);
                    }
                }
                Lv2Response::Restored { node, gen, slot } => self.adopt_restored_lv2(node, gen, slot),
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

    /// Attach the file-player channels: `rx` delivers decoded files from the
    /// decode thread (popped at block boundaries), `back` returns retired
    /// buffers to whoever can free them off the audio thread.
    pub fn set_player_channels(
        &mut self,
        rx: rtrb::Consumer<PlayerData>,
        back: rtrb::Producer<Arc<[f32]>>,
    ) {
        self.player_rx = Some(rx);
        self.player_back = Some(back);
    }

    /// The file player's own stereo output of the last rendered block, before
    /// gain and the master chain (self-tests measure the loop seam and the
    /// beat lock on it).
    pub fn player_block(&self, frames: usize) -> &[f32] {
        &self.player_buf[..(frames * 2).min(self.player_buf.len())]
    }

    /// Current file-player state for frontends (rule 33: what the engine
    /// owns is published back).
    pub fn player_pub(&self) -> PlayerPub {
        let cfg = &self.player_cfg;
        let p = self.player.as_ref();
        PlayerPub {
            loaded: p.is_some(),
            enabled: p.map_or(false, |p| p.enabled),
            pending: p.map_or(false, |p| p.pending),
            playing: p.map_or(false, |p| p.enabled && !p.pending && !p.finished),
            loop_on: cfg.loop_on,
            sync: cfg.sync,
            beats: cfg.beats,
            offset: cfg.offset,
            bpm: cfg.bpm,
            eff_bpm: p.map_or(0.0, |p| cfg.ref_bpm(p.duration) as f32),
            duration: p.map_or(0.0, |p| p.duration as f32),
            gain: self.player_gain,
        }
    }

    /// Load a decoded file (from the decode thread's ring): adopt the
    /// filename BPM (AUM: it is the original-BPM default) and start it —
    /// at the next bar line when sync is on and the transport rolls, else
    /// now. Retires whatever file it replaces.
    fn load_player(&mut self, d: PlayerData) {
        let old = self.player.take();
        if let Some(b) = d.bpm {
            self.player_cfg.bpm = b;
        }
        let frames = d.frames().max(1);
        let mut p = PlayerFile {
            samples: d.samples,
            file_sr: d.sample_rate.max(1) as f64,
            duration: frames as f64 / d.sample_rate.max(1) as f64,
            frames,
            enabled: true,
            pending: false,
            target_beats: 0.0,
            pos: 0.0,
            anchor_song_beats: 0.0,
            anchor_file_beats: 0.0,
            finished: false,
        };
        if self.player_cfg.sync && self.master_clock.is_active() {
            p.pending = true;
            p.target_beats = self.player_target_beats();
        } else {
            player_start_now(&mut p, self.player_cfg, self.transport.beats);
        }
        self.player = Some(p);
        self.player_dirty = true;
        if let Some(old) = old {
            self.retire_player(old.samples);
        }
    }

    /// The bar line a synced start waits for: beat 0 (the song's own
    /// downbeat) during a count-in, else the next bar boundary.
    fn player_target_beats(&self) -> f64 {
        if self.master_clock.is_counting_in() {
            0.0
        } else {
            crate::transport::next_bar_beats(self.transport.beats)
        }
    }

    /// Hand a retired decoded buffer to the host so it is freed off the
    /// audio thread. A full return ring parks it in the preallocated
    /// backlog; the only remaining option leaks, never frees here.
    fn retire_player(&mut self, samples: Arc<[f32]>) {
        let item = match self.player_back.as_mut() {
            Some(tx) => match tx.push(samples) {
                Ok(()) => return,
                Err(rtrb::PushError::Full(s)) => s,
            },
            None => samples,
        };
        if self.player_backlog.len() < self.player_backlog.capacity() {
            self.player_backlog.push(item);
        } else {
            std::mem::forget(item);
        }
    }

    /// Drain the retire backlog into the return ring (one slot per block is
    /// plenty — loads are rare).
    fn flush_player_backlog(&mut self) {
        while let Some(item) = self.player_backlog.pop() {
            let Some(tx) = self.player_back.as_mut() else {
                self.player_backlog.push(item);
                return;
            };
            if let Err(rtrb::PushError::Full(s)) = tx.push(item) {
                self.player_backlog.push(s);
                return;
            }
        }
    }

    /// Fill the player mix buffer for this block: the AUM FilePlayer clock.
    /// While sync is on and the song clock rolls, the file position is
    /// DERIVED from the song beats (same factor the metronome counts with,
    /// rule 43) — drift-free by construction, and a tempo change is
    /// followed in the same block with no phase jump. Otherwise the
    /// position integrates at the session tempo (sync on, transport idle)
    /// or the file's own rate (sync off). Allocation-free.
    fn fill_player(&mut self, frames: usize, block_start_beats: f64, beat_inc: f64) {
        let n2 = frames * 2;
        while let Some(rx) = self.player_rx.as_mut() {
            let Ok(d) = rx.pop() else { break };
            self.load_player(d);
        }
        self.flush_player_backlog();
        let Some(p) = self.player.as_mut() else {
            for x in &mut self.player_buf[..n2] {
                *x = 0.0;
            }
            return;
        };
        let cfg = self.player_cfg;
        let bpm = self.transport.bpm;
        let sr = self.sample_rate;
        let rolling = self.master_clock == MasterClock::Playing;
        // The clock only truly rolls once it has advanced: the block that
        // ends a count-in flips to Playing without advancing (beat 0 is the
        // FIRST frame of the block after it).
        let clock_live = rolling && (self.transport.beats > 0.0 || self.transport.seconds > 0.0);
        let ref_bpm = cfg.ref_bpm(p.duration);
        let total_beats = (ref_bpm * p.duration / 60.0).max(1e-9);
        let mut dirty = false;
        for i in 0..frames {
            let song_b = block_start_beats + i as f64 * beat_inc;
            // Position this frame reads, or None for silence.
            let mut pos: Option<f64> = None;
            if p.pending {
                let wait = cfg.sync
                    && self.master_clock.is_active()
                    && (!clock_live || song_b < p.target_beats);
                if !wait {
                    // The bar line arrived (or sync/transport no longer
                    // asks for it): start at the configured point, anchored
                    // to the song clock exactly here.
                    pos = Some(player_start_now(p, cfg, song_b));
                    dirty = true;
                }
            } else if p.enabled && !p.finished {
                if cfg.sync && rolling {
                    // Song-clock-derived position: file beats advance 1:1
                    // with song beats at matched varispeed, so the loop
                    // pass lasts exactly its beats in song beats and a
                    // tempo change is followed without a phase jump.
                    let file_beats = p.anchor_file_beats + (song_b - p.anchor_song_beats);
                    let b = if cfg.loop_on { file_beats.rem_euclid(total_beats) } else { file_beats };
                    let t = b * 60.0 / ref_bpm;
                    if !cfg.loop_on && t >= p.duration {
                        p.finished = true;
                        dirty = true;
                    } else {
                        pos = Some(t * p.file_sr);
                        p.pos = t * p.file_sr; // coherent handover to free-run
                    }
                } else {
                    // Free-run: read at the current position, then advance
                    // by the rate the song clock would run at (sync on) or
                    // the file's own rate (sync off).
                    if cfg.loop_on {
                        p.pos = p.pos.rem_euclid(p.frames as f64);
                    } else if p.pos >= p.frames as f64 {
                        p.finished = true;
                        dirty = true;
                    }
                    let rate = if cfg.sync { bpm / ref_bpm } else { 1.0 };
                    pos = Some(p.pos);
                    p.pos += rate * p.file_sr / sr;
                    if p.pos >= p.frames as f64 {
                        if cfg.loop_on {
                            p.pos = p.pos.rem_euclid(p.frames as f64);
                        } else {
                            p.finished = true;
                            dirty = true;
                        }
                    }
                }
            }
            let (sound, x) = match pos {
                Some(x) if x.is_finite() => {
                    // A looped position sitting exactly on the end (float
                    // rounding at the seam) wraps; it must never re-anchor.
                    let x = if cfg.loop_on && x >= p.frames as f64 { x.rem_euclid(p.frames as f64) } else { x };
                    ((0.0..p.frames as f64).contains(&x), x)
                }
                Some(_) => {
                    // Heal (rule 32): a non-finite position restarts the
                    // file instead of seeding NaNs into the mix.
                    let start = player_start_now(p, cfg, song_b);
                    (start.is_finite() && (0.0..p.frames as f64).contains(&start), start)
                }
                None => (false, 0.0),
            };
            if sound {
                let s = read_frame(&p.samples, p.frames, x, cfg.loop_on);
                self.player_buf[i * 2] = s[0];
                self.player_buf[i * 2 + 1] = s[1];
            } else {
                self.player_buf[i * 2] = 0.0;
                self.player_buf[i * 2 + 1] = 0.0;
            }
        }
        if dirty {
            self.player_dirty = true;
        }
    }

    /// Attach the master recording tap (writer thread started elsewhere).
    pub fn set_recorder(&mut self, tap: RecorderTap) {
        tap.shared().set_arms(self.arm_master, &self.arm_rigs);
        self.recorder = Some(tap);
    }

    pub fn set_listen_tap(&mut self, tap: crate::listen::Tap) {
        self.listen = Some(tap);
    }

    fn record_mask(&self) -> u16 {
        let mut mask = if self.arm_master { crate::record::MASK_MASTER } else { 0 };
        for rig in 0..8u8 {
            if self.arm_rigs[rig as usize] && self.recorder.as_ref().is_some_and(|r| r.stem_ready(rig)) {
                mask |= crate::record::mask_rig(rig);
            }
        }
        mask
    }

    fn record_start_now(&mut self) {
        self.record_pending = None;
        let mask = self.record_mask();
        if let Some(rec) = &mut self.recorder {
            rec.set_pending(false);
            if mask != 0 {
                rec.begin(mask);
            }
        }
    }

    fn record_stop_now(&mut self) {
        self.record_pending = None;
        if let Some(rec) = &mut self.recorder {
            rec.set_pending(false);
            rec.stop();
        }
    }

    /// Start recording armed targets (bar-quantized when transport is rolling).
    fn record_start(&mut self) {
        let recording = self.recorder.as_ref().is_some_and(|r| r.shared().is_recording());
        if recording {
            if matches!(self.record_pending, Some((false, _))) {
                self.record_pending = None;
                if let Some(rec) = &mut self.recorder {
                    rec.set_pending(false);
                }
            }
            return;
        }
        if self.record_mask() == 0 {
            return;
        }
        if self.master_clock == MasterClock::Playing {
            if matches!(self.record_pending, Some((true, _))) {
                return;
            }
            let bpb = crate::transport::BEATS_PER_BAR as f64;
            let next_bar = ((self.transport.beats / bpb).floor() + 1.0) * bpb;
            self.record_pending = Some((true, next_bar));
            if let Some(rec) = &mut self.recorder {
                rec.set_pending(true);
            }
        } else {
            self.record_start_now();
        }
    }

    /// Stop recording (bar-quantized when transport is rolling; double-stop stops immediately).
    fn record_stop(&mut self) {
        let recording = self.recorder.as_ref().is_some_and(|r| r.shared().is_recording());
        if !recording {
            if self.record_pending.is_some() {
                self.record_pending = None;
                if let Some(rec) = &mut self.recorder {
                    rec.set_pending(false);
                }
            }
            return;
        }
        if matches!(self.record_pending, Some((false, _))) || self.master_clock != MasterClock::Playing {
            self.record_stop_now();
        } else {
            let bpb = crate::transport::BEATS_PER_BAR as f64;
            let next_bar = ((self.transport.beats / bpb).floor() + 1.0) * bpb;
            self.record_pending = Some((false, next_bar));
            if let Some(rec) = &mut self.recorder {
                rec.set_pending(true);
            }
        }
    }

    /// What `render`'s master chain (limiter, WARM, master volume) makes of
    /// a summed rig signal, starting from a fresh limiter: the master output
    /// of a take that began after silence with nothing else mixed in
    /// (monitor, count-in, system audio, player, master inserts). `record-test` rebuilds the
    /// master from the stems with it, sample for sample.
    pub fn master_chain_from_silence(&self, sum: &[f32]) -> Vec<f32> {
        let mut lim = MasterLimiter::new(self.sample_rate);
        let drive = 1.0 + self.master_warm * 3.0;
        let mut out = Vec::with_capacity(sum.len());
        for f in sum.chunks_exact(2) {
            let (l, r) = lim.process(f[0], f[1], drive, self.master_vol);
            out.push(l);
            out.push(r);
        }
        out
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
    /// Attach the MIDI clock sender's position publication (main-thread
    /// setup, like `set_graph_pub`).
    pub fn set_clock_pos(&mut self, p: Arc<crate::clocksend::ClockPos>) {
        self.clock_pos = Some(p);
    }

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
    pub fn edge_levels_handle(&self) -> Arc<EdgeLevels> {        self.edge_levels.clone()
    }

    /// Lock-free render time per voice and effect node.
    pub fn node_times_handle(&self) -> Arc<crate::engine_info::NodeTimes> {
        self.node_timer.handle()
    }

    /// The strip table as it goes into `GraphPub::strips`.
    fn strips_pub(&self) -> [StripPub; MAX_STRIP_MIXES] {
        strips_pub_table(&self.strips)
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
            // A master-insert cable (Master OUT → FX) carries the mix bus the
            // FX ran on: its level is the FX's own output peak, so the cable
            // glows with what actually flows through it.
            if self.graph.node(src).is_some_and(|n| matches!(n.kind, NodeKind::MasterOut)) {
                let peak = self.dsp.iter().find(|d| d.node == e.to.node).map(|d| block_peak(&d.buf[..n2]))
                    .or_else(|| self.lv2.iter().find(|l| l.node == e.to.node).map(|l| block_peak(&l.buf[..n2])));
                if let Some(p) = peak {
                    self.edge_levels.store(e.id, p);
                }
                continue;
            }
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

    /// Which rigs have a soloed source (#73): solo scope is WITHIN a rig — a
    /// soloed source silences only the other sources of its own rig, while
    /// rig buses keep their own global solo. Allocation-free.
    fn rigs_with_solo_source(&self) -> [bool; 8] {
        let mut solo = [false; 8];
        for (node, m) in &self.strips {
            if !m.solo { continue; }
            let rig = self.graph.node(*node).map(|n| n.rig as usize).unwrap_or(0).min(7);
            solo[rig] = true;
        }
        solo
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

    /// Arp verification hook (self-test): the notes this arp node has
    /// generated and not released — chord mode holds 3 per key (a triad).
    pub fn arp_sounding(&self, node: NodeId) -> Vec<u8> {
        self.arp.iter().find(|a| a.node == node).map(|a| a.sounding()).unwrap_or_default()
    }

    /// Arp verification hook (self-test): the keys this arp node holds.
    pub fn arp_held(&self, node: NodeId) -> Vec<u8> {
        self.arp.iter().find(|a| a.node == node).map(|a| a.held_notes().collect()).unwrap_or_default()
    }

    /// One "time passed" step of every arp node: the held-notes check's
    /// Tick — it drives the real note delivery the same way `render` does.
    #[cfg(test)]
    pub(crate) fn arp_test_step(&mut self) {
        let mut arps = std::mem::take(&mut self.arp);
        for a in &mut arps {
            let (node, vel) = (a.node, a.velocity());
            let mut emit = |note: u8, on: bool| self.deliver_arp_note(node, note, on, vel);
            a.test_step(&mut emit);
        }
        self.arp = arps;
    }

    fn has_midi_cable(&self, dest: NodeId) -> bool {
        self.graph.edges.iter().any(|e| e.kind == PortKind::Midi && e.to.node == dest)
    }

    /// Does a MIDI cable carry THIS unit's output to `dest`? The output leg
    /// of a MIDI-FX chain is gated by its own cable, exactly like a Keys
    /// node's leg is (`keys_route`).
    fn fx_cable(&self, fx: NodeId, dest: NodeId) -> bool {
        self.graph
            .edges
            .iter()
            .any(|e| e.kind == PortKind::Midi && e.from.node == fx && e.to.node == dest)
    }

    /// Is `node` a hosted MIDI-FX unit?
    fn is_midi_fx(&self, node: NodeId) -> bool {
        matches!(self.graph.node(node).map(|n| &n.kind), Some(NodeKind::MidiFx { .. }))
    }

    /// Default matrix rules for one MIDI-FX node (rule 105: routes belong to
    /// the node): the rig's input sources feed the unit (`device`: only that
    /// hardware device, else the keyboard and every device), and its output
    /// cables become rules. Both legs also need their cable (`keys_route`
    /// gates the input, `fx_cable` the output). Idempotent.
    fn route_midi_fx(&mut self, fx: NodeId, device: Option<u32>) {
        let connect = |src: SourceId, m: &mut Matrix| {
            if !m.rules.iter().any(|r| r.source == src && r.dest == fx) {
                m.connect(src, fx);
            }
        };
        match device {
            Some(dev) => connect(SourceId::Hardware(dev), &mut self.matrix),
            None => {
                connect(SourceId::Keys, &mut self.matrix);
                for dev in 0..MAX_HW_DEVICES {
                    connect(SourceId::Hardware(dev), &mut self.matrix);
                }
            }
        }
        self.route_midi_fx_outputs(fx);
    }

    /// Wire + route a freshly added MIDI-FX node like an instrument is
    /// (rule 19: playable out of the box). Wiring inserts the unit into its
    /// rig's chain: the instruments' direct Keys cables are replaced, so
    /// everything held on them is released first (rule 41).
    fn setup_midi_fx(&mut self, id: NodeId, wire: bool) {
        if wire {
            let rig = self.graph.node(id).map_or(0, |n| n.rig);
            for inst in self.graph.midi_sinks(rig) {
                self.release_all(inst);
            }
            self.graph.wire_midi_fx(id);
        }
        let device = self
            .graph
            .node(id)
            .and_then(|n| self.rig_meta.get(n.rig as usize))
            .and_then(|m| m.midi_device);
        self.route_midi_fx(id, device);
    }

    /// May an event that came from `origin` enter `fx`? The per-rig source
    /// switches gate the chain behind the unit: it accepts when some
    /// instrument it feeds accepts that source (a unit feeding only chained
    /// units or LV2 instruments always accepts). Note-offs never ask —
    /// a stray release is harmless, a dropped one hangs a note (rule 41).
    fn fx_accepts(&self, fx: NodeId, origin: SourceId) -> bool {
        let mut fed = false;
        for r in self.matrix.rules.iter() {
            if r.source != SourceId::MidiFx(fx) || !r.enabled {
                continue;
            }
            fed = true;
            match self.voices.iter().find(|v| v.node == r.dest) {
                Some(v) if source_accepted(v, origin) => return true,
                Some(_) => {}
                None => return true,
            }
        }
        !fed
    }

    /// Release every MIDI-FX unit upstream of `node` (an instrument whose
    /// source switch or device filter is about to change): the filter that
    /// used to pass a held key's note-on would drop its note-off (rule 41).
    fn release_upstream(&mut self, node: NodeId) {
        let mut i = 0;
        while let Some(r) = self.matrix.rules.get(i) {
            i += 1;
            if r.dest == node {
                if let SourceId::MidiFx(fx) = r.source {
                    self.release_all(fx);
                }
            }
        }
    }

    /// Set a MIDI-FX unit's knob: the built-in transposer's semitones, or a
    /// plugin control. Before the plugin is live the value is parked. A
    /// transposer's semitones change the pitch a held note's release takes,
    /// so the unit releases first.
    fn apply_midi_fx_param(&mut self, node: NodeId, param: ParamId, v: f32) -> bool {
        let Some(i) = self.midi_fx.iter().position(|s| s.node == node) else { return false };
        let v = v.clamp(0.0, 1.0);
        if param == P_MIDIFX_SEMI && matches!(self.midi_fx[i].proc, None) {
            let semis = ((v * 2.0 - 1.0) * MIDIFX_SEMI_MAX as f32).round() as i8;
            if semis != self.midi_fx[i].semis {
                self.release_all(node);
                self.midi_fx[i].semis = semis;
            }
            return true;
        }
        // A hosted plugin's knob may change what its note-offs mean (the
        // transposition a held note leaves with): when the value really
        // moves, everything held through the unit is released first
        // (rule 41; found by `midifx-test`: a note-off at the new pitch left
        // the old one sounding).
        let moves = matches!(&self.midi_fx[i].proc, Some(MidiFxProc::Lv2(s))
            if s.plugin.control_normalized(param as usize).is_some_and(|cur| (cur - v).abs() > 1e-4));
        if moves {
            self.release_all(node);
        }
        let slot = &mut self.midi_fx[i];
        match &mut slot.proc {
            Some(MidiFxProc::Lv2(s)) => s.plugin.set_control_normalized(param as usize, v),
            _ => match slot.pending.iter_mut().find(|(p, _)| *p == param) {
                Some(e) => e.1 = v,
                None => {
                    if slot.pending.len() < slot.pending.capacity() {
                        slot.pending.push((param, v));
                    }
                }
            },
        }
        true
    }

    /// A loaded MIDI-only LV2 plugin takes its node's place in the event
    /// path. The note-off path of everything held changes with it, so the
    /// unit releases (rule 41); parked knob values replay (rule 113).
    fn attach_midi_fx_lv2(&mut self, slot: Lv2Slot) {
        let node = slot.node;
        if !slot.plugin.is_midi_fx() || !self.midi_fx.iter().any(|s| s.node == node) {
            eprintln!("midi-fx node {node}: {} is not a MIDI unit", slot.plugin.uri());
            self.retire_lv2(slot);
            return;
        }
        self.release_all(node);
        let Some(i) = self.midi_fx.iter().position(|s| s.node == node) else { return };
        let mut slot = slot;
        for k in 0..self.midi_fx[i].pending.len() {
            let (param, v) = self.midi_fx[i].pending[k];
            slot.plugin.set_control_normalized(param as usize, v);
        }
        self.midi_fx[i].pending.clear();
        if let Some(MidiFxProc::Lv2(old)) = self.midi_fx[i].proc.replace(MidiFxProc::Lv2(slot)) {
            self.retire_lv2(old);
        }
    }

    /// Run every MIDI-FX unit for one block: the events queued by `apply`
    /// go in, the plugin's output is routed on through the matrix
    /// (`SourceId::MidiFx(node)`), gated by the unit's own output cables.
    /// A unit without a live plugin passes events through (transposed when
    /// it is the built-in one). Allocation-free: every buffer is preallocated
    /// and bounded.
    fn pump_midi_fx(&mut self, frames: usize) {
        use crate::lv2::FxOutEvent;
        for i in 0..self.midi_fx.len() {
            let node = self.midi_fx[i].node;
            self.fx_in.clear();
            self.fx_out.clear();
            {
                let slot = &mut self.midi_fx[i];
                for m in slot.in_q.drain(..) {
                    if self.fx_in.len() < self.fx_in.capacity() {
                        self.fx_in.push([m.status | m.channel, m.data[0], m.data[1]]);
                    }
                }
            }
            let slot = &mut self.midi_fx[i];
            match &mut slot.proc {
                Some(MidiFxProc::Lv2(s)) => {
                    for &ev in self.fx_in.iter() {
                        s.plugin.send_midi(ev);
                    }
                    s.plugin.process(&[], &mut [], frames);
                    s.plugin.read_midi_out(&mut self.fx_out);
                }
                None => {
                    let semis = slot.semis as i16;
                    for &d in self.fx_in.iter() {
                        if self.fx_out.len() >= self.fx_out.capacity() {
                            break;
                        }
                        let status = d[0] & 0xF0;
                        let ch = d[0] & 0x0F;
                        let is_note = matches!(status, 0x80 | 0x90);
                        let note = if is_note {
                            match shifted(d[1], semis) {
                                Some(n) => n,
                                None => continue,
                            }
                        } else {
                            d[1]
                        };
                        self.fx_out.push(match status {
                            0x90 if d[2] > 0 => FxOutEvent::NoteOn { channel: ch, note, velocity: d[2] },
                            0x80 | 0x90 => FxOutEvent::NoteOff { channel: ch, note },
                            _ => FxOutEvent::Midi(d),
                        });
                    }
                }
            }
            for j in 0..self.fx_out.len() {
                let ev = self.fx_out[j];
                self.deliver_fx_event(node, ev);
            }
        }
    }

    /// Route one event a MIDI-FX unit emitted to everything its output
    /// cables reach (rule transforms apply, like any matrix route).
    fn deliver_fx_event(&mut self, fx: NodeId, ev: crate::lv2::FxOutEvent) {
        use crate::lv2::FxOutEvent;
        let msg = match ev {
            FxOutEvent::NoteOn { channel, note, velocity } => MidiMsg::note_on(channel, note, velocity),
            FxOutEvent::NoteOff { channel, note } => MidiMsg::note_off(channel, note, 0),
            FxOutEvent::Midi(d) => MidiMsg::new(d[0] & 0xF0, d[0] & 0x0F, d[1], d[2]),
        };
        self.matrix.route_into(SourceId::MidiFx(fx), msg, &mut self.route_out);
        for k in 0..self.route_out.len() {
            let (dest, m) = self.route_out[k];
            if !self.fx_cable(fx, dest) {
                continue;
            }
            let is_on = m.status == 0x90 && m.data[1] > 0;
            let is_off = m.status == 0x80 || (m.status == 0x90 && m.data[1] == 0);
            if let Some(j) = self.midi_fx.iter().position(|s| s.node == dest) {
                if dest != fx {
                    self.midi_fx[j].queue(m);
                }
                continue;
            }
            if is_on || is_off {
                #[cfg(test)]
                self.note_log.push((dest, m.data[0], is_on));
                let note = m.data[0];
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                    if is_on {
                        s.voice.note_on(note as u16, m.data[1] as f64 / 127.0);
                    } else {
                        s.voice.note_off(note as u16);
                    }
                }
                if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                    if s.plugin.is_instrument() {
                        if is_on {
                            s.plugin.note_on(note, m.data[1]);
                        } else {
                            s.plugin.note_off(note);
                        }
                    }
                }
            } else if m.status == 0xB0 {
                if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                    s.voice.send_cc_rt(m.channel, m.data[0], m.data[1]);
                }
                if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                    if s.plugin.is_instrument() {
                        s.plugin.control_change(m.data[0], m.data[1]);
                    }
                }
            }
        }
    }

    /// Matrix rules for the output cables of a MIDI-FX unit: one rule per
    /// MIDI cable leaving it (idempotent). Called when a cable is added and
    /// when an LV2 instrument attaches (its rules come with the load).
    fn route_midi_fx_outputs(&mut self, fx: NodeId) {
        for i in 0..self.graph.edges.len() {
            let e = &self.graph.edges[i];
            if e.kind == PortKind::Midi && e.from.node == fx {
                let dest = e.to.node;
                if !self.matrix.rules.iter().any(|r| r.source == SourceId::MidiFx(fx) && r.dest == dest) {
                    self.matrix.connect(SourceId::MidiFx(fx), dest);
                }
            }
        }
        self.fit_route_out();
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

    /// Park a control value for an LV2 instance that is still loading
    /// (`attach_lv2` replays it). Most recent value per ordinal wins; the
    /// table is small and fixed — a template load parks one value per knob.
    fn park_lv2(&mut self, node: NodeId, ordinal: u16, value: f32) {
        let mut slot = None;
        for (i, p) in self.lv2_pending.iter().enumerate() {
            if p.as_ref().is_some_and(|p| p.node == node) {
                slot = Some(i);
                break;
            }
        }
        if slot.is_none() {
            slot = self.lv2_pending.iter().position(|p| p.is_none());
        }
        let Some(i) = slot else {
            eprintln!("lv2 pending table full: value for node {node} dropped");
            return;
        };
        let p = self.lv2_pending[i].get_or_insert(Lv2Pending { node, pairs: [(0, 0.0); 8], count: 0 });
        if let Some(existing) = p.pairs.iter_mut().take(p.count).find(|(o, _)| *o == ordinal) {
            existing.1 = value;
        } else if p.count < p.pairs.len() {
            p.pairs[p.count] = (ordinal, value);
            p.count += 1;
        }
    }

    /// Apply an engine-level knob/parameter change (shared by `Command::SetParam`
    /// and `CcTarget::Knob` CC bindings). Allocation-free.
    fn apply_engine_param(&mut self, node: NodeId, param: ParamId, value: f64) -> bool {
        let v = value as f32;
        if self.midi_fx.iter().any(|s| s.node == node) {
            self.apply_midi_fx_param(node, param, v)
        } else if let Some(s) = self.lv2.iter_mut().find(|s| s.node == node) {
            // setBfree's face controls are MIDI CCs (organ.rs); every other
            // LV2 knob maps param index → nth control port (normalized).
            match crate::organ::ccs(param, v) {
                Some((msgs, n)) => {
                    for &(cc, val) in &msgs[..n] { s.plugin.control_change(cc, val); }
                }
                None => s.plugin.set_control_normalized(param as usize, v),
            }
            true
        } else if let Some(k) = crate::organ::index(param)
            .filter(|_| self.graph.node(node).is_some_and(|n| crate::organ::is_organ(&n.kind)))
        {
            // setBfree still loading: keep the value for attach_lv2.
            let slot = self.organ_pending.iter().position(|p| p.is_some_and(|p| p.node == node))
                .or_else(|| self.organ_pending.iter().position(|p| p.is_none()));
            if let Some(i) = slot {
                let p = self.organ_pending[i].get_or_insert(OrganPending { node, values: [0.0; crate::organ::PARAMS], set: 0 });
                p.values[k] = v;
                p.set |= 1 << k;
            }
            true
        } else if self.graph.node(node).is_some_and(|n| matches!(n.kind, NodeKind::Lv2 { .. })) {
            // Any other LV2 node still loading on the loader thread: park
            // the value, never drop it (rule 113; the template's amp Drive
            // is one of these between LoadGraph and attach).
            self.park_lv2(node, param.min(0xFFFF) as u16, v);
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
        } else if (crate::arp::P_ARP_MODE..=crate::arp::P_ARP_CHORD).contains(&param)
            && self.arp.iter().any(|a| a.node == node)
        {
            // An arp unit's knobs (issue #90). Settings that shape the burst
            // release what is sounding first (rule 41) — ArpState handles it.
            self.with_arp_slot(node, |a, eng| {
                let (nid, vel) = (a.node, a.velocity());
                let mut emit = |n: u8, on: bool| eng.deliver_arp_note(nid, n, on, vel);
                a.set_param(param, value, &mut emit)
            })
            .unwrap_or(false)
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

    /// What a played note picks up on its way from a Keys node to `dest`:
    /// that zone's octave shift, and the Keys node's scale snap (issue #90).
    /// Note-on and note-off take the same answer — the snap is a pure
    /// function of the note — so a release always meets the pitch its
    /// note-on started (rule 41). Allocation-free.
    fn keys_route(&self, dest: NodeId, played: u8) -> Option<KeysRoute> {
        self.graph.nodes.iter().find_map(|n| {
            if !matches!(n.kind, NodeKind::Keys) { return None; }
            let port = split_port(n.split, played);
            self.graph.edges.iter().any(|e| {
                e.kind == PortKind::Midi
                    && e.from.node == n.id
                    && e.from.port == port
                    && e.to.node == dest
            }).then(|| KeysRoute {
                shift: crate::control::Graph::zone_shift(n, played),
                scale: n.scale,
            })
        })
    }

    /// The note that reaches `dest`: the matrix-transformed note, the zone's
    /// octave shift, and (Some scale) the Keys node's snap to the nearest
    /// in-scale note. Pure: a note-off computes the same outgoing note its
    /// note-on did.
    fn routed_note(m: &MidiMsg, route: &KeysRoute) -> Option<u8> {
        let out = shifted(m.note(), route.shift)?;
        Some(match route.scale {
            Some(sc) => crate::arp::snap_note(out, sc),
            None => out,
        })
    }

    /// Note-off for all 128 notes on one instrument node. The matrix is
    /// stateless, so every edit that can change a held note's note-off path
    /// (split, rule, MIDI cable, source filter, node removal, graph load)
    /// calls this first. An Arp node CASCADES: its held keys and generated
    /// notes die, and so does everything it feeds — an arp-generated note's
    /// off path is the arp's own cable, which the edit may be about to move
    /// (rule 41). Rare op, allocation-free (fixed 8-deep fan-out). Checked
    /// by `held_notes` (verify-held-notes).
    ///
    /// A MIDI-FX dest releases BOTH sides of the unit (rule 41): the events
    /// still in its input queue are dropped (they never reached the plugin),
    /// all-notes-off is queued into the plugin itself, and every instrument
    /// the unit feeds is released (its own notes can be sounding there).
    fn release_all(&mut self, dest: NodeId) {
        if self.arp.iter().any(|a| a.node == dest) {
            let mut fed = [0u32; 8];
            let mut n_fed = 0usize;
            for i in 0..self.graph.edges.len() {
                let e = &self.graph.edges[i];
                if e.kind == PortKind::Midi && e.from.node == dest && n_fed < fed.len() {
                    if !fed[..n_fed].contains(&e.to.node) {
                        fed[n_fed] = e.to.node;
                        n_fed += 1;
                    }
                }
            }
            self.with_arp_slot(dest, |a, eng| {
                let (node, vel) = (a.node, a.velocity());
                let mut emit = |note: u8, on: bool| eng.deliver_arp_note(node, note, on, vel);
                a.release_all(&mut emit);
            });
            for i in 0..n_fed {
                self.release_instrument(fed[i]);
            }
            return;
        }
        self.release_instrument(dest);
    }

    /// Note-off for all 128 notes on one instrument (voice or LV2).
    fn release_instrument(&mut self, dest: NodeId) {
        if let Some(s) = self.voices.iter().find(|s| s.node == dest) {
            for n in 0..128u16 { s.voice.note_off(n); }
        }
        if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
            for n in 0..128u8 { s.plugin.note_off(n); }
        }
        if self.is_midi_fx(dest) {
            let Some(fx) = self.midi_fx.iter_mut().find(|s| s.node == dest) else { return };
            // Cycle guard: a user-wired FX1 → FX2 → FX1 loop must not recurse
            // forever; the second entry only requeues all-notes-off.
            if !fx.releasing {
                fx.releasing = true;
                // Own queue: never-delivered events die here.
                fx.in_q.clear();
                // All-notes-off INTO the plugin: the queue is capped, and
                // 128 events fit (FX_QUEUE_CAP = 256), so this never drops.
                for note in 0..128u8 {
                    fx.queue(MidiMsg::note_off(0, note, 0));
                }
                // Downstream: every instrument the unit feeds, by rule
                // index (allocation-free), enabled or not — a just-disabled
                // rule may have been the path a note took.
                let mut i = 0;
                while let Some(r) = self.matrix.rules.get(i) {
                    i += 1;
                    if r.source == SourceId::MidiFx(dest) {
                        let inst = r.dest;
                        self.release_all(inst);
                    }
                }
                if let Some(fx) = self.midi_fx.iter_mut().find(|s| s.node == dest) {
                    fx.releasing = false;
                }
            }
        }
        #[cfg(test)]
        self.note_log.push((dest, 255, false)); // 255 = all-notes-off marker
    }

    /// Run `f` on the arp slot of `node` with the engine passable to its
    /// emit path. `self.arp` is moved out for the call (pointer swap, no
    /// allocation) so `deliver_arp_note` can borrow the engine freely.
    fn with_arp_slot<R>(&mut self, node: NodeId, f: impl FnOnce(&mut crate::arp::ArpState, &mut Self) -> R) -> Option<R> {
        let mut arps = std::mem::take(&mut self.arp);
        let out = arps.iter_mut().find(|a| a.node == node).map(|a| f(a, self));
        self.arp = arps;
        out
    }

    /// Deliver one note the arp generated to every instrument the arp node
    /// feeds by MIDI cable — plain graph wiring (the matrix routes sources,
    /// and the arp is not a source; its level/scale are its own knobs).
    /// Block-boundary timing, the timing of every other note here.
    fn deliver_arp_note(&mut self, arp_node: NodeId, note: u8, on: bool, vel: f32) {
        for i in 0..self.graph.edges.len() {
            let e = &self.graph.edges[i];
            if e.kind != PortKind::Midi || e.from.node != arp_node { continue; }
            let dest = e.to.node;
            #[cfg(test)]
            self.note_log.push((dest, note, on));
            // An arp wired into a MIDI strip (arp → unit → synth): the unit
            // takes the note into its queue like one from the keys.
            if let Some(j) = self.midi_fx.iter().position(|s| s.node == dest) {
                let v7 = (vel.clamp(0.0, 1.0) * 127.0) as u8;
                self.midi_fx[j].queue(if on { MidiMsg::note_on(0, note, v7) } else { MidiMsg::note_off(0, note, 0) });
                continue;
            }
            if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                if on {
                    s.voice.note_on(note as u16, vel as f64);
                } else {
                    s.voice.note_off(note as u16);
                }
            }
            if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                if s.plugin.is_instrument() {
                    if on {
                        s.plugin.note_on(note, 127);
                    } else {
                        s.plugin.note_off(note);
                    }
                }
            }
        }
    }

    /// The arp's own beat clock at this moment: the song position while the
    /// master transport plays (rule 43 — the pattern sits on the music's
    /// grid), else the slot's free-running phase (a live arp keeps arping
    /// between songs at the transport's BPM).
    fn arp_clock(&self, node: NodeId) -> f64 {
        let playing = self.transport.playing;
        self.arp.iter().find(|a| a.node == node)
            .map(|a| if playing { self.transport.beats } else { a.phase })
            .unwrap_or(0.0)
    }

    fn apply(&mut self, cmd: Command) {
        if matches!(cmd, Command::AddNode { .. } | Command::LoadGraph { .. } | Command::LoadRigs { .. }) {
            self.drain_voice_msgs();
        }
        match cmd {
            Command::AddNode { kind, name, rig, wire } => {
                let id = self.graph.add_node(kind.clone(), name);
                self.graph.set_rig(id, rig);
                if wire && !matches!(kind, NodeKind::MidiFx { .. }) {
                    if matches!(kind, NodeKind::Arp) {
                        // The arp takes over the rig's Keys→synth MIDI cables:
                        // every note held on those synths loses its off path
                        // (rule 41). Release them BEFORE rewiring.
                        let mut taken = 0usize;
                        for i in 0..self.graph.edges.len() {
                            let e = &self.graph.edges[i];
                            if e.kind != PortKind::Midi || e.to.node == id { continue; }
                            if self.graph.node(e.from.node)
                                .is_some_and(|n| matches!(n.kind, NodeKind::Keys) && n.rig == rig)
                            {
                                let (dest, eid) = (e.to.node, e.id);
                                self.release_all(dest);
                                let synth = self.graph.node(dest).is_some_and(|d| matches!(d.kind, NodeKind::Clap { .. }));
                                if synth && taken < self.arp_taken.len() {
                                    self.arp_taken[taken] = eid;
                                    taken += 1;
                                }
                            }
                        }
                        for slot in &mut self.arp_taken[taken..] {
                            *slot = crate::control::EdgeId::MAX;
                        }
                        self.graph.wire_arp(id);
                    } else {
                        self.graph.wire_instrument(id);
                    }
                }
                let sr = self.sample_rate;
                match &kind {
                    NodeKind::Dsp(dk) => {
                        if let Some(n) = self.graph.node(id) {
                            let slot = dsp_slot(n, dk, sr);
                            self.dsp.push(slot);
                        }
                    }
                    NodeKind::Lv2 { uri } => self.request_lv2(id, *uri),
                    NodeKind::MidiFx { plugin } => {
                        if let Some(n) = self.graph.node(id) {
                            self.midi_fx.push(midi_fx_slot(n));
                        }
                        if let MidiFxPlugin::Lv2 { uri } = plugin {
                            self.request_lv2(id, *uri);
                        }
                        self.setup_midi_fx(id, wire);
                    }
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
                    // A MIDI destination like an instrument: its input rules
                    // come with the node so the keys route to it at once.
                    NodeKind::Arp => {
                        self.arp.push(crate::arp::ArpState::new(id));
                        self.matrix.connect(SourceId::Keys, id);
                        for dev in 0..MAX_HW_DEVICES {
                            self.matrix.connect(SourceId::Hardware(dev), id);
                        }
                        self.fit_route_out();
                    }
                    _ => {}
                }
                self.publish_graph();
            }
            Command::RemoveNode(id) => {
                // An idle voice keeps a held note and replays it when rebound.
                // (An Arp node cascades: its generated notes die with it.)
                self.release_all(id);
                let rig_of_fx = self.graph.node(id).filter(|n| matches!(n.kind, NodeKind::MidiFx { .. })).map(|n| n.rig);
                self.graph.remove_node(id);
                if let Some(i) = self.midi_fx.iter().position(|s| s.node == id) {
                    let slot = self.midi_fx.remove(i);
                    if let Some(MidiFxProc::Lv2(s)) = slot.proc {
                        self.retire_lv2(s);
                    }
                    // Notes of the removed unit are released; its rules go
                    // with it, and the rig's instruments get their direct
                    // cables back so the rack stays playable.
                    self.matrix.rules.retain(|r| r.dest != id && r.source != SourceId::MidiFx(id));
                    if let Some(rig) = rig_of_fx {
                        self.graph.heal_midi_after_fx(rig);
                    }
                }
                self.dsp.retain(|s| s.node != id);
                self.audio_in.retain(|s| s.node != id);
                self.arp.retain(|s| s.node != id);
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
                // No release: a cable plugged after a note-on only ever
                // delivers a stray note-off to a dest that never sounded
                // (and the arp ignores offs for keys it never held) — the
                // held-notes check proves this path safe.
                if self.arp_taken.contains(&id) {
                    return;
                }
                if self.graph.connect_with_id(id, from, to, kind) {
                    if kind == PortKind::Midi && self.is_midi_fx(from.node) {
                        self.route_midi_fx_outputs(from.node);
                    }
                    self.publish_graph();
                }
            }
            Command::Disconnect(id) => {
                // keys_gate drops a note-off whose Keys cable is gone; an
                // arp-generated note's off path is the arp's own cable and a
                // MIDI-FX unit's output cable is a note-off path of its own.
                let fed = self.graph.edges.iter().find(|e| {
                    e.id == id
                        && e.kind == PortKind::Midi
                        && self.graph.node(e.from.node).map_or(false, |n| {
                            matches!(n.kind, NodeKind::Keys | NodeKind::Arp | NodeKind::MidiFx { .. })
                        })
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
            Command::LoadLv2State { node, state } => {
                let Some(NodeKind::Lv2 { uri }) = self.graph.node(node).map(|n| n.kind.clone()) else { return };
                let gen = self.lv2_gen;
                if let Some(tx) = self.lv2_req.as_mut() {
                    if tx.push(Lv2Request::State { node, uri, gen, state }).is_err() {
                        eprintln!("lv2 loader queue full: state for node {node} dropped");
                    }
                    return;
                }
                if let Some(slot) = restored_lv2_slot(node, uri.as_str(), &state, self.sample_rate) {
                    self.adopt_restored_lv2(node, gen, slot);
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
                self.arp_taken = [crate::control::EdgeId::MAX; 8];
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
                while let Some(slot) = self.midi_fx.pop() {
                    if let Some(MidiFxProc::Lv2(s)) = slot.proc {
                        self.retire_lv2(s);
                    }
                }
                self.midi_fx = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::MidiFx { .. }))
                    .map(midi_fx_slot)
                    .collect();
                // Arp nodes of the new graph start fresh (the loop above
                // released every old node — an old arp cascaded to what it
                // fed, and the slots themselves die with the old graph).
                self.arp = build_arp_slots(&self.graph);
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
                let arp_ids: Vec<NodeId> = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::Arp))
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
                // Arp nodes are MIDI destinations like instruments.
                for &id in &arp_ids {
                    self.matrix.connect(SourceId::Keys, id);
                    for dev in 0..MAX_HW_DEVICES {
                        self.matrix.connect(SourceId::Hardware(dev), id);
                    }
                }
                let fx_ids: Vec<NodeId> = self.midi_fx.iter().map(|s| s.node).collect();
                for fx in fx_ids {
                    self.route_midi_fx(fx, None);
                }
                self.fit_route_out();
                self.rig_meta.clear();
                // Values parked for the old graph's organs (ids are reused).
                self.organ_pending = [None; 8];
                self.lv2_pending = [None; 8];
                for i in 0..self.graph.nodes.len() {
                    let id = self.graph.nodes[i].id;
                    match self.graph.nodes[i].kind {
                        NodeKind::Lv2 { uri } => self.request_lv2(id, uri),
                        NodeKind::MidiFx { plugin: MidiFxPlugin::Lv2 { uri } } => self.request_lv2(id, uri),
                        _ => {}
                    }
                }
                self.active_rig = 0;
                self.rig_mix = [RigMix::DEFAULT; 8];
                self.strips.clear();
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
                self.arp_taken = [crate::control::EdgeId::MAX; 8];
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
                        // The rig's MIDI-FX units: only that device feeds
                        // them, and their output rules came out with the rest.
                        let fxs: Vec<NodeId> = self
                            .graph
                            .nodes
                            .iter()
                            .filter(|n| n.rig == tag && matches!(n.kind, NodeKind::MidiFx { .. }))
                            .map(|n| n.id)
                            .collect();
                        for fx in fxs {
                            self.route_midi_fx(fx, Some(dev));
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
                    let Some(route) = self.keys_route(dest, note) else { continue };
                    let Some(out) = Self::routed_note(&m, &route) else { continue };
                    // An arp destination: the note joins its hold (or, chord
                    // mode, sounds its chord at once); the arp generates its
                    // own notes downstream from here.
                    if self.arp.iter().any(|a| a.node == dest) {
                        let now = self.arp_clock(dest);
                        let vel = velocity as f32;
                        self.with_arp_slot(dest, |a, eng| {
                            let (node, v) = (a.node, a.velocity());
                            let mut emit = |n: u8, on: bool| eng.deliver_arp_note(node, n, on, v);
                            a.note_in(out, vel, now, &mut emit);
                        });
                        continue;
                    }
                    #[cfg(test)]
                    self.note_log.push((dest, out, true));
                    if let Some(j) = self.midi_fx.iter().position(|s| s.node == dest) {
                        if self.fx_accepts(dest, source) {
                            self.midi_fx[j].queue(MidiMsg::new(m.status, m.channel, out, m.data[1]));
                        }
                        continue;
                    }
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
                    let Some(route) = self.keys_route(dest, note) else { continue };
                    let Some(out) = Self::routed_note(&m, &route) else { continue };
                    // The release meets the arp at the same snapped pitch its
                    // note-on did; the arp stops what it had going.
                    if self.arp.iter().any(|a| a.node == dest) {
                        self.with_arp_slot(dest, |a, eng| {
                            let (node, v) = (a.node, a.velocity());
                            let mut emit = |n: u8, on: bool| eng.deliver_arp_note(node, n, on, v);
                            a.note_out(out, &mut emit);
                        });
                        continue;
                    }
                    #[cfg(test)]
                    self.note_log.push((dest, out, false));
                    if let Some(j) = self.midi_fx.iter().position(|s| s.node == dest) {
                        self.midi_fx[j].queue(MidiMsg::new(m.status, m.channel, out, m.data[1]));
                        continue;
                    }
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
                self.release_upstream(node);
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
                self.release_upstream(node);
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
            Command::SetStripMix { node, pan, mute, solo } => {
                let e = self.strips.iter_mut().find(|(n, _)| *n == node);
                match e {
                    Some((_, m)) => {
                        if let Some(p) = pan { m.pan = p.clamp(-1.0, 1.0); }
                        if let Some(b) = mute { m.mute = b; }
                        if let Some(b) = solo { m.solo = b; }
                    }
                    None => {
                        if self.strips.len() >= MAX_STRIP_MIXES { return; }
                        let mut m = StripMix::DEFAULT;
                        if let Some(p) = pan { m.pan = p.clamp(-1.0, 1.0); }
                        if let Some(b) = mute { m.mute = b; }
                        if let Some(b) = solo { m.solo = b; }
                        if m.is_default() { return; }
                        self.strips.push((node, m));
                    }
                }
                self.strips.retain(|(_, m)| !m.is_default());
                self.mix_dirty = true;
            }
            Command::SetStripBypass { node, bypass } => {
                if bypass {
                    match self.strips.iter_mut().find(|(n, _)| *n == node) {
                        Some((_, m)) => m.bypass = true,
                        None => {
                            if self.strips.len() < MAX_STRIP_MIXES {
                                self.strips.push((node, StripMix { bypass: true, ..StripMix::DEFAULT }));
                            }
                        }
                    }
                } else if let Some((_, m)) = self.strips.iter_mut().find(|(n, _)| *n == node) {
                    m.bypass = false;
                    self.strips.retain(|(_, m)| !m.is_default());
                }
                self.mix_dirty = true;
            }
            Command::RenameNode { node, name } => {
                // Metadata in the graph (sessions carry it with the patch);
                // the graph publication makes every frontend see it.
                let n = crate::control::name_str(&name).trim();
                if !n.is_empty() {
                    if let Some(m) = self.graph.nodes.iter_mut().find(|m| m.id == node) {
                        m.name = crate::control::name_bytes(n);
                        self.publish_graph();
                    }
                }
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
            Command::SetKeysScale { node, scale } => {
                // A Keys-node property (like a split move): a scale change
                // mid-hold would snap the release to a different pitch than
                // its note-on snapped to — release everything this Keys node
                // feeds, from all ports, BEFORE the snap moves (rule 41).
                let choice = scale.map(|c| crate::arp::ScaleChoice { key: c.key.min(11), ..c });
                let is_change = self.graph.node(node).is_some_and(|n| {
                    matches!(n.kind, NodeKind::Keys) && n.scale != choice
                });
                if is_change {
                    for i in 0..self.graph.edges.len() {
                        let e = &self.graph.edges[i];
                        if e.kind == PortKind::Midi && e.from.node == node {
                            let dest = e.to.node;
                            self.release_all(dest);
                        }
                    }
                    self.graph.set_scale(node, choice);
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
                    let norm = mapping.map_or(m.data[1] as f64 / 127.0, |b| b.scale.norm(m.data[1]));
                    if let Some(CcMapping { param: pid, target: CcTarget::Knob, .. }) = mapping {
                        if self.apply_engine_param(dest, pid, norm) || is_clap_node {
                            #[cfg(test)]
                            self.cc_log.push((dest, pid, norm));
                        }
                        continue;
                    }
                    // A MIDI-FX unit takes the CC into its plugin (needs its cable).
                    if let Some(j) = self.midi_fx.iter().position(|s| s.node == dest) {
                        match mapping {
                            // A learned knob (PluginParam target) moves the unit's control.
                            Some(b) => {
                                self.apply_midi_fx_param(dest, b.param, norm as f32);
                                #[cfg(test)]
                                self.cc_log.push((dest, b.param, norm));
                            }
                            None if self.has_midi_cable(dest) => self.midi_fx[j].queue(m),
                            None => {}
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
                    } else if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        // An unmapped CC reaches an LV2 instrument the same
                        // way it reaches a CLAP voice: the plugin implements
                        // the controller (mod wheel CC 1, sustain CC 64, …).
                        if s.plugin.is_instrument() && has_midi {
                            s.plugin.control_change(m.data[0], m.data[1]);
                            #[cfg(test)]
                            self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                        }
                    }
                }
            }
            Command::PitchBend { source, channel, value } => {
                // A bend is a channel message like CC: routed through the
                // matrix, passed through to the instruments (plugins bend
                // what they play); no binding applies to it.
                let msg = MidiMsg::pitch_bend(channel, value);
                self.matrix.route_into(source, msg, &mut self.route_out);
                for i in 0..self.route_out.len() {
                    let (dest, m) = self.route_out[i];
                    let has_midi = self.has_midi_cable(dest);
                    if let Some(s) = self.voices.iter_mut().find(|s| s.node == dest) {
                        if !has_midi || !source_accepted(s, source) {
                            continue;
                        }
                        s.voice.send_pitch_bend_rt(m.channel, m.pitch_bend_value());
                        #[cfg(test)]
                        self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                    } else if self.graph.node(dest).map_or(false, |n| matches!(n.kind, NodeKind::Clap { .. })) {
                        // A Clap node whose voice is not hosted yet: nothing to
                        // bend (same shape as the CC path; logged under test).
                        #[cfg(test)]
                        if has_midi {
                            self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                        }
                    } else if let Some(s) = self.lv2.iter_mut().find(|s| s.node == dest) {
                        if s.plugin.is_instrument() && has_midi {
                            s.plugin.pitch_bend(m.pitch_bend_value());
                            #[cfg(test)]
                            self.cc_passthrough_log.push((dest, m.data[0], m.data[1]));
                        }
                    }
                }
            }
            Command::MatrixCcMap { source, dest, cc, param, target, scale } => {
                let changed = match param {
                    Some(p) => match self
                        .matrix
                        .rules
                        .iter_mut()
                        .find(|r| r.source == source && r.dest == dest)
                    {
                        Some(r) => r.set_cc_mapping(CcMapping { cc, param: p, target, scale }),
                        None => {
                            // Learning a knob must not also wire the keys.
                            let mut r = Rule::new(source, dest);
                            r.cc_only = true;
                            r.set_cc_mapping(CcMapping { cc, param: p, target, scale });
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
                let gain = gain.clamp(0.0, 1.0);
                if (self.player_gain - gain).abs() > 1e-9 {
                    self.player_gain = gain;
                    self.player_dirty = true;
                }
            }
            Command::SetPlayerCfg { loop_on, sync, beats, offset, bpm } => {
                let old = self.player_cfg;
                if let Some(v) = loop_on {
                    self.player_cfg.loop_on = v;
                }
                if let Some(v) = sync {
                    self.player_cfg.sync = v;
                }
                if let Some(v) = beats {
                    self.player_cfg.beats = v.max(0.0);
                }
                if let Some(v) = offset {
                    self.player_cfg.offset = v.clamp(-64.0, 64.0);
                }
                if let Some(v) = bpm {
                    self.player_cfg.bpm = v.clamp(20.0, 300.0);
                }
                if self.player_cfg == old {
                    return;
                }
                if let Some(p) = self.player.as_mut() {
                    // Looping again unfreezes a one-shot that ran out.
                    if self.player_cfg.loop_on {
                        p.finished = false;
                    }
                    // A pending start with sync now off begins immediately
                    // (AUM: no sync, no waiting for the quantum).
                    if p.pending && !self.player_cfg.sync {
                        player_start_now(p, self.player_cfg, self.transport.beats);
                    } else if p.enabled
                        && !p.pending
                        && !p.finished
                        && old.sync != self.player_cfg.sync
                        && self.master_clock == MasterClock::Playing
                    {
                        // Toggle sync mid-play without a jump: re-anchor
                        // the song-clock mapping at the current position.
                        let ref_bpm = self.player_cfg.ref_bpm(p.duration);
                        let t = (p.pos / p.file_sr).clamp(0.0, p.duration);
                        p.anchor_file_beats = t * ref_bpm / 60.0;
                        p.anchor_song_beats = self.transport.beats;
                    }
                }
                self.player_dirty = true;
            }
            Command::SetPlayerEnable { on } => {
                let target = self.player_target_beats();
                let Some(p) = self.player.as_mut() else { return };
                if on {
                    p.enabled = true;
                    if self.player_cfg.sync && self.master_clock.is_active() {
                        // AUM: while the transport rolls, the play-enable
                        // starts at the next sync-quantum (bar) boundary;
                        // during a count-in that is beat 0.
                        p.pending = true;
                        p.target_beats = target;
                    } else {
                        player_start_now(p, self.player_cfg, self.transport.beats);
                    }
                } else {
                    p.enabled = false;
                    p.pending = false;
                }
                self.player_dirty = true;
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
            Command::SetRecordArm { rig, armed } => {
                match rig {
                    None => self.arm_master = armed,
                    Some(r) => self.arm_rigs[(r as usize).min(7)] = armed,
                }
                if let Some(rec) = &self.recorder {
                    rec.shared().set_arms(self.arm_master, &self.arm_rigs);
                }
            }
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
                if let Some(cp) = &self.clock_pos {
                    cp.publish_start(0.0);
                }
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
            // File player (AUM): a synced, enabled player starts WITH the
            // transport, anchored to the song clock's beat 0 — right after
            // any count-in, so an aligned loop lands on the downbeat.
            if self.player_cfg.sync {
                if let Some(p) = self.player.as_mut().filter(|p| p.enabled) {
                    p.pending = true;
                    p.target_beats = 0.0;
                    self.player_dirty = true;
                }
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
                self.record_stop_now();
            } else if self.record_pending.is_some() {
                let stop_now = matches!(self.record_pending, Some((false, _)));
                self.record_pending = None;
                if stop_now {
                    self.record_stop_now();
                } else if let Some(rec) = &mut self.recorder {
                    rec.set_pending(false);
                }
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
            // A backlog (capture ahead of its consumer) is cut to the target
            // fill in one go; the ring holds interleaved stereo, so every
            // discard is a whole frame (one sample would swap L and R).
            let backlog = s.drift.catch_up(consumer.slots());
            if backlog > 0 {
                for _ in 0..backlog {
                    let _ = consumer.pop();
                }
                s.dropped += (backlog / 2) as u64;
            }
            match s.drift.action(consumer.slots()) {
                DriftAction::DropOne => {
                    let _ = consumer.pop();
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
        if let Some(rec) = &mut self.recorder {
            rec.drain_stems();
        }
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }
        self.drain_lv2_loader();
        // Sync-to-transport: begin the take the block the song clock starts at
        // beat 0 (immediately on play without a count-in, or right after one).
        if self.record_sync_pending && self.master_clock == MasterClock::Playing {
            self.record_sync_pending = false;
            self.record_start_now();
        }
        // Bar-quantized start/stop when record was pressed while transport rolls.
        if let Some((start, target_beat)) = self.record_pending {
            if self.master_clock == MasterClock::Playing && self.transport.beats >= target_beat - 1e-9 {
                if start {
                    self.record_start_now();
                } else {
                    self.record_stop_now();
                }
            }
        }
        self.drain_voice_msgs();
        self.publish_voices();
        // MIDI-FX units run before the instruments render, so the notes they
        // emit sound in the same block the events arrived in.
        let fx_frames = data.len() / ch;
        self.pump_midi_fx(fx_frames);
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
                    g.strips = self.strips_pub();
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
        // Per-source strip state (#73), once per block: which rigs have a
        // soloed source decides every source's mute/solo gain below.
        let solo_rigs = self.rigs_with_solo_source();

        // 1. Every instrument renders and applies its level.
        self.node_timer.begin_block();
        let mut node_t0 = std::time::Instant::now();
        for s in &mut self.voices {
            // A free voice (no Clap node bound): skip the plugin entirely.
            if s.node == NodeId::MAX { continue; }
            s.voice.render(&mut s.scratch[..n2], self.transport);
            for x in s.scratch[..n2].iter_mut() {
                *x *= s.level;
            }
            // Strip mix (mute/solo/pan) at the source's own output: the edge
            // levels published from this buffer stay post-fader (rule 53).
            if let Some((gl, gr)) = strip_gains_of(&self.strips, &self.graph, s.node, &solo_rigs) {
                if gl == gr {
                    if gl != 1.0 {
                        for x in s.scratch[..n2].iter_mut() { *x *= gl; }
                    }
                } else {
                    for f in s.scratch[..n2].chunks_exact_mut(2) {
                        f[0] *= gl;
                        f[1] *= gr;
                    }
                }
            }
            // One clock read per voice: each ends where the next begins.
            let now = std::time::Instant::now();
            self.node_timer.record(s.node, now - node_t0);
            node_t0 = now;
        }

        // 1a. Audio In nodes: pull the input ring into each node's stereo
        // buffer, map the chosen channel, apply gain. Allocation- and lock-free.
        self.fill_audio_in(frames);
        for a in &mut self.audio_in {
            if let Some((gl, gr)) = strip_gains_of(&self.strips, &self.graph, a.node, &solo_rigs) {
                if gl == gr {
                    if gl != 1.0 {
                        for x in a.buf[..n2].iter_mut() { *x *= gl; }
                    }
                } else {
                    for f in a.buf[..n2].chunks_exact_mut(2) {
                        f[0] *= gl;
                        f[1] *= gr;
                    }
                }
            }
        }

        // 2a. Metronome nodes: click generators gated by their OWN playing
        // flag (per-instrument transport). Output reaches the master via
        // normal audio routing like any DSP node.
        let (sr, bpm) = (self.sample_rate, self.transport.bpm);
        // MIDI clock send: the song position as of THIS block's first frame,
        // stamped with the monotonic clock (one vDSO read, no allocation).
        let block_wall_ns = match &self.clock_pos {
            Some(cp) => {
                let wall = crate::alsaseq::mono_ns();
                cp.publish_block(
                    self.transport.beats,
                    bpm,
                    self.master_clock == MasterClock::Playing,
                    self.master_clock.is_counting_in(),
                    wall,
                );
                wall
            }
            None => 0,
        };
        let beat_inc = Transport::beats_per_frame(bpm, sr);
        // Song position of THIS block's first frame — the file player and
        // the metronome both count beats from it with the same factor
        // (rule 43). Snapped before the clock advances below.
        let block_start_beats = self.transport.beats;
        let block_beats = frames as f64 * beat_inc;
        let counting_in_this_block = self.master_clock.is_counting_in();
        let suppress_wrap_click = matches!(
            self.master_clock,
            MasterClock::CountingIn { remaining_beats, .. } if remaining_beats <= block_beats + 1e-9
        );
        for d in &mut self.dsp {
            if d.kind == DspKind::Metronome {
                render_metronome(&mut d.transport, &mut d.buf[..n2], frames, sr, bpm, suppress_wrap_click);
                if let Some((gl, gr)) = strip_gains_of(&self.strips, &self.graph, d.node, &solo_rigs) {
                    if gl == gr {
                        if gl != 1.0 {
                            for x in d.buf[..n2].iter_mut() { *x *= gl; }
                        }
                    } else {
                        for f in d.buf[..n2].chunks_exact_mut(2) {
                            f[0] *= gl;
                            f[1] *= gr;
                        }
                    }
                }
            }
        }
        if counting_in_this_block {
            render_metronome(&mut self.count_in_slot, &mut self.count_in_buf[..n2], frames, sr, bpm, suppress_wrap_click);
        }
        // 2a-arp. Arpeggiator units step on the SONG CLOCK while the master
        // transport plays (rule 43: the pattern sits on the music's grid, and
        // a tempo change retempoes it mid-beat), and on their own free-run
        // beat phase at the same BPM when it does not — a live arp keeps
        // arping between songs. Mirroring the song position into the phase
        // while playing makes the stop seamless; a rewind (Play) is caught
        // by the tick's retime instead of machine-gunning missed steps.
        {
            let mut arps = std::mem::take(&mut self.arp);
            for a in &mut arps {
                if self.transport.playing {
                    a.phase = self.transport.beats;
                }
                let (t0, t1) = (a.phase, a.phase + block_beats);
                let (node, vel) = (a.node, a.velocity());
                let mut emit = |note: u8, on: bool| self.deliver_arp_note(node, note, on, vel);
                a.tick(t0, t1, &mut emit);
                if !self.transport.playing {
                    a.phase = t1;
                }
            }
            self.arp = arps;
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
                    if let Some(cp) = &self.clock_pos {
                        // Beat 0 starts with the NEXT block: anchor it there.
                        let next_wall = block_wall_ns + (frames as f64 / sr * 1e9) as u64;
                        cp.publish_block(0.0, bpm, true, false, next_wall);
                        cp.publish_start(0.0);
                    }
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
        if self.transport_dirty || self.player_dirty {
            let tp = self.transport_pub();
            let pp = self.player_pub();
            match &self.graph_pub {
                Some(p) => if let Ok(mut g) = p.try_lock() {
                    if self.transport_dirty {
                        g.transport = tp;
                        g.transport_rev = g.transport_rev.wrapping_add(1);
                        self.transport_dirty = false;
                    }
                    if self.player_dirty {
                        g.player = pp;
                        g.player_rev = g.player_rev.wrapping_add(1);
                        self.player_dirty = false;
                    }
                },
                None => {
                    self.transport_dirty = false;
                    self.player_dirty = false;
                }
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
            let node_t0 = std::time::Instant::now();
            self.fx_done[f] = true;
            let node = if f < n_dsp { self.dsp[f].node } else { self.lv2[f - n_dsp].node };
            self.sum_audio_into(node, &mut bus[..n2]);
            // Bypass (#74): the slot passes its summed input through
            // unchanged; its knobs keep their values for when it returns.
            let bypassed = strip_bypassed(&self.strips, node);
            let strip_gain = strip_gains_of(&self.strips, &self.graph, node, &solo_rigs);
            if f < n_dsp {
                let d = &mut self.dsp[f];
                d.buf[..n2].copy_from_slice(&bus[..n2]);
                if !bypassed {
                    d.proc.process(&mut d.buf[..n2], frames);
                }
            } else {
                let slot = &mut self.lv2[f - n_dsp];
                if bypassed {
                    slot.buf[..n2].copy_from_slice(&bus[..n2]);
                } else {
                    slot.plugin.process(&bus[..n2], &mut slot.buf[..n2], frames);
                }
            }
            // An LV2 node that is a SOURCE (an instrument) carries strip
            // state; effects do not (their strip row is the bypass flag).
            if let Some((gl, gr)) = strip_gain {
                let buf = if f < n_dsp { &mut self.dsp[f].buf } else { &mut self.lv2[f - n_dsp].buf };
                if gl == gr {
                    if gl != 1.0 {
                        for x in buf[..n2].iter_mut() { *x *= gl; }
                    }
                } else {
                    for fr in buf[..n2].chunks_exact_mut(2) {
                        fr[0] *= gl;
                        fr[1] *= gr;
                    }
                }
            }
            self.node_timer.record(node, node_t0.elapsed());
        }
        self.node_timer.end_block();
        self.lv2_in = bus;

        // 3. Master sums everything wired to it.
        // Pull system-audio capture once per block (RT-safe ring pop).
        let sys = self.system_gain > 0.0 && self.system_capture.is_some();
        if sys {
            if let Some(cap) = &mut self.system_capture {
                cap.read(&mut self.system_buf[..n2]);
            }
        }
        // File player: read the decoded buffer at the clock's position
        // (loop/sync/beat-offset all live in the engine, issue #84). Runs
        // even without a master node so the clock stays true.
        self.fill_player(frames, block_start_beats, beat_inc);
        let ply = self.player.is_some() && self.player_gain > 0.0;
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
                let rig = self.graph.node(master).map(|n| n.rig as usize).unwrap_or(0).min(7);
                let (gl, gr) = gains[mi];
                let rig_buf = &mut self.rig_buf[..n2];
                for x in rig_buf.iter_mut() {
                    *x = 0.0;
                }
                if gl != 0.0 || gr != 0.0 {
                    for v in &self.voices {
                        if v.node == NodeId::MAX || !self.graph.has_route(v.node, master, PortKind::Audio) {
                            continue;
                        }
                        if self.graph.has_route_port(v.node, master, 2, PortKind::Audio) {
                            for i in 0..frames {
                                let m = v.scratch[i * 2]; // mono in: center to both
                                rig_buf[i * 2] += m * gl;
                                rig_buf[i * 2 + 1] += m * gr;
                            }
                        } else {
                            for i in 0..frames {
                                rig_buf[i * 2] += v.scratch[i * 2] * gl;
                                rig_buf[i * 2 + 1] += v.scratch[i * 2 + 1] * gr;
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
                                rig_buf[i * 2] += m * gl;
                                rig_buf[i * 2 + 1] += m * gr;
                            }
                        } else {
                            for i in 0..frames {
                                rig_buf[i * 2] += d.buf[i * 2] * gl;
                                rig_buf[i * 2 + 1] += d.buf[i * 2 + 1] * gr;
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
                                rig_buf[i * 2] += m * gl;
                                rig_buf[i * 2 + 1] += m * gr;
                            }
                        } else {
                            for i in 0..frames {
                                rig_buf[i * 2] += slot.buf[i * 2] * gl;
                                rig_buf[i * 2 + 1] += slot.buf[i * 2 + 1] * gr;
                            }
                        }
                    }
                    // Audio In wired into this master (OUT jack → graph routing).
                    for a in &self.audio_in {
                        if !self.graph.has_route(a.node, master, PortKind::Audio) {
                            continue;
                        }
                        for i in 0..frames {
                            rig_buf[i * 2] += a.buf[i * 2] * gl;
                            rig_buf[i * 2 + 1] += a.buf[i * 2 + 1] * gr;
                        }
                    }
                    for i in 0..n2 {
                        acc[i] += rig_buf[i];
                    }
                }
                if let Some(rec) = &mut self.recorder {
                    if rec.is_stem_active(rig as u8) {
                        rec.push_rig_stereo(rig as u8, rig_buf);
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
            // Master inserts (#77): FX nodes wired FROM a MasterOut node
            // (Master OUT → FX → FX → …) run on the summed mix bus, in chain
            // order, BEFORE the fixed limiter + warmth. The cables stay the
            // truth — an insert is exactly an audio edge out of a master.
            // Allocation-free: a fixed stack table, linear scans.
            let mut inserts = [(NodeId::MAX, false); MAX_MASTER_INSERTS];
            let mut n_ins = 0usize;
            loop {
                if n_ins >= MAX_MASTER_INSERTS { break; }
                let mut next = None;
                for f in 0..(self.dsp.len() + self.lv2.len()) {
                    let (node, is_dsp) = if f < self.dsp.len() {
                        (self.dsp[f].node, true)
                    } else {
                        (self.lv2[f - self.dsp.len()].node, false)
                    };
                    if inserts[..n_ins].iter().any(|(n, _)| *n == node) { continue; }
                    let fed = if n_ins == 0 {
                        masters[..n_masters].iter().any(|&m| self.graph.has_route(m, node, PortKind::Audio))
                    } else {
                        self.graph.has_route(inserts[n_ins - 1].0, node, PortKind::Audio)
                    };
                    if fed { next = Some((node, is_dsp)); break; }
                }
                match next {
                    Some(x) => inserts[n_ins] = x,
                    None => break,
                }
                n_ins += 1;
            }
            for &(node, is_dsp) in inserts.iter().take(n_ins) {
                if strip_bypassed(&self.strips, node) { continue; }
                if is_dsp {
                    let Some(d) = self.dsp.iter_mut().find(|d| d.node == node) else { continue };
                    d.buf[..n2].copy_from_slice(&acc[..n2]);
                    d.proc.process(&mut d.buf[..n2], frames);
                    acc.copy_from_slice(&d.buf[..n2]);
                } else {
                    let Some(slot) = self.lv2.iter_mut().find(|s| s.node == node) else { continue };
                    slot.plugin.process(&acc[..n2], &mut slot.buf[..n2], frames);
                    acc.copy_from_slice(&slot.buf[..n2]);
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
            if let Some(listen) = &mut self.listen {
                listen.push_stereo(data, ch);
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

/// Organ knob values waiting for their setBfree instance (`organ_pending`).
#[derive(Clone, Copy)]
struct OrganPending {
    node: NodeId,
    values: [f32; crate::organ::PARAMS],
    set: u16,
}

/// Control-port values waiting for an LV2 instance that is still loading on
/// the loader thread (rule 46): parked here by `apply_engine_param` and
/// replayed by `attach_lv2` — the general case of `organ_pending` (rule 113:
/// a value sent before the instance is live is parked, never dropped; the
/// Guitar rig's template Drive otherwise never reached the amp). Fixed size,
/// no allocation (audio thread).
#[derive(Clone, Copy)]
struct Lv2Pending {
    node: NodeId,
    /// (control ordinal, normalized value), most recent per ordinal.
    pairs: [(u16, f32); 8],
    count: usize,
}

/// Zero-latency peak limiter for the master bus: holds the summed signal
/// under `CEILING` so the warmth stage after it only colours, never buzzes.
/// No lookahead (it would add its window to every played note); the brief
/// attack overshoot lands in the soft `tanh` that follows.
struct MasterLimiter {
    env: f32,
    gain: f32,
    rel: f32,
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
            env: 0.0, gain: 1.0, rel: coef(0.120),
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
        if target < self.gain {
            self.gain = target;
        } else {
            self.gain = self.rel * self.gain + (1.0 - self.rel) * target;
        }
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

    #[test]
    fn sudden_chord_onset_never_overshoots_and_default_vol_stays_below_knee() {
        let mut lim = MasterLimiter::new(48000.0);
        let default_vol = super::knob_specs(&crate::control::NodeKind::MasterOut)
            .iter()
            .find(|k| k.param == Some(super::P_VOL))
            .unwrap()
            .default;
        let drive = 1.0 + 0.15 * 3.0;
        let mut max_out = 0.0f32;
        for i in 0..4800 {
            let x = 4.0 * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin().max(0.001)
                + if i == 0 { 4.0 } else { 0.0 };
            let (l, _) = lim.process(x, x, drive, default_vol);
            max_out = max_out.max(l.abs());
        }
        assert!(max_out <= MasterLimiter::KNEE, "default master_vol={default_vol} must keep chord onsets below knee ({max_out} > {})", MasterLimiter::KNEE);
    }
}

#[cfg(test)]
mod tests {
    /// Rule 129: a source nobody touched has no strip entry, yet another
    /// source's solo in its rig must still silence it.
    #[test]
    fn another_sources_solo_silences_an_untouched_source() {
        use crate::control::{name_bytes, Graph, NodeKind};
        let mut g = Graph::default();
        g.reserve(8, 8);
        let a = g.add_node(NodeKind::Clap { plugin_id: name_bytes("x") }, name_bytes("A"));
        let b = g.add_node(NodeKind::Clap { plugin_id: name_bytes("x") }, name_bytes("B"));
        let strips = [(b, super::StripMix { solo: true, ..super::StripMix::DEFAULT })];
        let mut solo = [false; 8];
        solo[0] = true;
        assert_eq!(super::strip_gains_of(&strips, &g, a, &solo), Some((0.0, 0.0)), "untouched A is silenced by B's solo");
        let b_gain = super::strip_gains_of(&strips, &g, b, &solo).expect("the soloed source has a gain");
        assert!(b_gain.0 > 0.99 && b_gain.1 > 0.99, "the soloed source plays: {b_gain:?}");
        assert_eq!(super::strip_gains_of(&[], &g, a, &[false; 8]), None, "no solo, no entry: untouched path");
    }

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

    fn add_arp(engine: &mut super::GraphEngine, bus: &mut crate::control::CommandBus) -> u32 {
        use crate::control::{name_bytes, Command, NodeKind};
        play(engine, bus, Command::AddNode { kind: NodeKind::Arp, name: name_bytes("ARP"), rig: 0, wire: true });
        engine.graph().nodes.iter().find(|n| matches!(n.kind, NodeKind::Arp)).expect("the arp was added").id
    }

    #[test]
    fn an_arp_takes_over_the_keys_cables_and_a_stale_reconnect_cannot_undo_it() {
        use crate::control::{Command, PortKind, PortRef};
        let (mut e, mut bus) = split_engine(None, false);
        play(&mut e, &mut bus, on(60));
        let log = play(&mut e, &mut bus, Command::AddNode {
            kind: crate::control::NodeKind::Arp, name: crate::control::name_bytes("ARP"), rig: 0, wire: true });
        assert!(log.contains(&(1, 255, false)) && log.contains(&(4, 255, false)), "held notes on both synths are released before the rewire: {log:?}");
        let arp = e.graph().nodes.iter().find(|n| matches!(n.kind, crate::control::NodeKind::Arp)).unwrap().id;
        let midi = |e: &super::GraphEngine| e.graph().edges.iter().filter(|x| x.kind == PortKind::Midi).map(|x| (x.from.node, x.to.node)).collect::<Vec<_>>();
        assert!(midi(&e).contains(&(0, arp)) && midi(&e).contains(&(arp, 1)) && midi(&e).contains(&(arp, 4)), "keys → arp → both synths: {:?}", midi(&e));
        assert!(!midi(&e).contains(&(0, 1)) && !midi(&e).contains(&(0, 4)), "no direct keys → synth cable is left: {:?}", midi(&e));
        // The UI's per-frame mirror still holds the old cables until it adopts the new graph.
        play(&mut e, &mut bus, Command::Connect { id: 1, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 1, port: 0 }, kind: PortKind::Midi });
        play(&mut e, &mut bus, Command::Connect { id: 6, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 4, port: 0 }, kind: PortKind::Midi });
        assert!(!midi(&e).contains(&(0, 1)) && !midi(&e).contains(&(0, 4)), "the stale cables stayed out: {:?}", midi(&e));
        // A load starts over: the id is free again.
        let g = crate::control::Graph::demo_patch();
        play(&mut e, &mut bus, Command::LoadGraph { graph: g });
        assert!(midi(&e).contains(&(0, 1)));
    }

    #[test]
    fn the_arp_arpeggiates_the_held_keys_into_its_synths_and_every_release_silences_it() {
        use crate::control::Command;
        let (mut e, mut bus) = split_engine(None, false);
        let arp = add_arp(&mut e, &mut bus);
        // Held keys reach the arp, not the synths.
        assert!(play(&mut e, &mut bus, on(60)).is_empty(), "a held key plays nothing by itself");
        assert!(play(&mut e, &mut bus, on(64)).is_empty());
        assert_eq!(e.arp_held(arp), vec![60, 64]);
        // Time passes: the arp starts its pattern on both synths.
        e.note_log.clear();
        e.arp_test_step();
        let ons: Vec<(u32, u8)> = e.note_log.iter().filter(|x| x.2).map(|x| (x.0, x.1)).collect();
        assert!(ons.contains(&(1, 60)) && ons.contains(&(4, 60)), "the first note goes to both synths: {ons:?}");
        // Releasing every key silences the arp's notes on both.
        let mut log = play(&mut e, &mut bus, off(60));
        log.extend(play(&mut e, &mut bus, off(64)));
        assert!(e.arp_sounding(arp).is_empty() && e.arp_held(arp).is_empty());
        let downs: Vec<(u32, u8)> = log.iter().filter(|x| !x.2).map(|x| (x.0, x.1)).collect();
        let sounding_at_tick: Vec<u8> = ons.iter().filter(|x| x.0 == 1).map(|x| x.1).collect();
        assert!(downs.contains(&(1, *sounding_at_tick.last().unwrap())) && downs.contains(&(4, *sounding_at_tick.last().unwrap())),
            "the burst the tick left down went off on both synths: {log:?}");
        // Removing the arp while keys are held silences what it fed.
        play(&mut e, &mut bus, on(60));
        e.arp_test_step();
        let log = play(&mut e, &mut bus, Command::RemoveNode(arp));
        assert!(log.contains(&(1, 255, false)) && log.contains(&(4, 255, false)), "RemoveNode releases the synths it fed: {log:?}");
    }

    #[test]
    fn chord_mode_sounds_three_voices_per_key_through_the_engine() {
        use crate::control::Command;
        let (mut e, mut bus) = split_engine(None, false);
        let arp = add_arp(&mut e, &mut bus);
        play(&mut e, &mut bus, Command::SetParam { node: arp, param: crate::arp::P_ARP_MODE, value: crate::arp::step_value(3, 5) as f64 });
        let log = play(&mut e, &mut bus, on(60));
        let on_a: Vec<u8> = log.iter().filter(|x| x.0 == 1 && x.2).map(|x| x.1).collect();
        assert_eq!(on_a, vec![60, 64, 67], "one key, a triad on synth A: {log:?}");
        let log = play(&mut e, &mut bus, off(60));
        let off_a: Vec<u8> = log.iter().filter(|x| x.0 == 1 && !x.2).map(|x| x.1).collect();
        assert_eq!(off_a, vec![60, 64, 67], "the key's release takes the whole chord off: {log:?}");
        assert!(e.arp_sounding(arp).is_empty());
    }

    #[test]
    fn scale_snap_moves_a_key_and_its_release_to_the_same_note() {
        use crate::control::Command;
        use crate::arp::{Scale, ScaleChoice};
        let (mut e, mut bus) = split_engine(None, false);
        let cmaj = ScaleChoice { scale: Scale::Major, key: 0 };
        play(&mut e, &mut bus, Command::SetKeysScale { node: 0, scale: Some(cmaj) });
        assert_eq!(e.graph().node(0).unwrap().scale, Some(cmaj));
        assert_eq!(play(&mut e, &mut bus, on(61)), vec![(1, 60, true), (4, 60, true)], "C# plays as C on both synths");
        assert_eq!(play(&mut e, &mut bus, off(61)), vec![(1, 60, false), (4, 60, false)], "its release meets the C");
        assert_eq!(play(&mut e, &mut bus, on(62)), vec![(1, 62, true), (4, 62, true)], "an in-scale key is untouched");
        // Changing the scale while a key is held releases first (rule 41).
        let log = play(&mut e, &mut bus, Command::SetKeysScale { node: 0, scale: None });
        assert!(log.contains(&(1, 255, false)) && log.contains(&(4, 255, false)), "{log:?}");
        assert_eq!(play(&mut e, &mut bus, on(61)), vec![(1, 61, true), (4, 61, true)], "snapping off: the key plays as pressed");
        // Only Keys nodes snap.
        play(&mut e, &mut bus, Command::SetKeysScale { node: 1, scale: Some(cmaj) });
        assert_eq!(e.graph().node(1).unwrap().scale, None);
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
            wire: false,
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
    fn lv2_values_sent_while_the_instance_loads_are_parked_and_replayed() {
        use crate::control::{command_bus, name_bytes, Graph, Lv2Uri, NodeKind};
        // Rule 113 for every LV2 node, not only the organ: a SetParam that
        // arrives while the instance is still on the loader thread is parked
        // and replayed at attach — the Guitar rig template's amp Drive is
        // one between LoadGraph and attach. Probes the real plugin when the
        // Guitarix card is installed, skips in CI.
        let uri = crate::guitar::AMP;
        let (mut bus, drain) = command_bus();
        let mut g = Graph::default();
        let amp = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name_bytes("AMP"));
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![]);
        if engine.lv2.is_empty() {
            return; // the plugin is not installed (CI): nothing to park for
        }
        let Some(slot) = engine.lv2.pop() else { return };
        let mut buf = vec![0.0f32; 256 * 2];
        bus.send(Command::SetParam { node: amp, param: crate::guitar::AMP_DRIVE_CTRL as super::ParamId, value: 1.0 });
        engine.render(&mut buf, 2); // drains the command: parks (no live slot)
        assert!(
            engine.lv2_pending.iter().any(|p| p.is_some_and(|p| p.node == amp)),
            "the value sent while loading is parked, not dropped"
        );
        engine.attach_lv2(slot);
        let live = engine.lv2.iter().find(|s| s.node == amp).unwrap();
        let desc = live.plugin.control_descs()[crate::guitar::AMP_DRIVE_CTRL].clone();
        assert!(
            (desc.value - desc.max).abs() < 1e-6,
            "the parked Drive was replayed at attach: {} (max {})",
            desc.value, desc.max
        );
        engine.lv2_pending = [None; 8];
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
        bus.send(Command::MatrixCcMap { source: dev, dest: 1, cc: 74, param: Some(42), target: CcTarget::PluginParam, scale: Default::default() });
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

    /// A stepped binding (an instrument's switch) reaches the organ as the
    /// organ's own step, through the engine's CC path.
    #[test]
    fn a_scaled_cc_binding_lands_on_the_organ_step() {
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::default();
        let organ = g.add_node(NodeKind::Lv2 { uri: crate::control::Lv2Uri::new(crate::organ::B_SYNTH) }, name_bytes("ORGAN"));
        let mut engine = super::GraphEngine::new(crate::control::Graph::default(), drain, 48000.0, vec![]);
        bus.send(Command::LoadGraph { graph: g });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        let nord = SourceId::Hardware(5);
        bus.send(Command::MatrixCcMap {
            source: nord, dest: organ, cc: 82, param: Some(crate::organ::P_LESLIE),
            target: CcTarget::Knob, scale: crate::matrix::CcScale::stepped(2, 64, 127),
        });
        engine.cc_log.clear();
        bus.send(Command::Cc { source: nord, channel: 0, cc: 82, value: 10 });
        bus.send(Command::Cc { source: nord, channel: 0, cc: 82, value: 100 });
        engine.render(&mut buf, 2);
        let steps: Vec<_> = engine.cc_log.iter().map(|&(_, p, v)| (p, crate::organ::step_name(p, v as f32))).collect();
        assert_eq!(steps, vec![(crate::organ::P_LESLIE, Some("SLOW")), (crate::organ::P_LESLIE, Some("FAST"))]);
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
            scale: Default::default(),
        });
        engine.render(&mut buf, 2);
        let rev_after = pub_.lock().unwrap().rev;
        assert_eq!(rev_after, rev_before + 1, "MatrixCcMap must publish cc_map");
        let r = pub_.lock().unwrap().rules.iter().find(|r| r.source == SourceId::Hardware(0) && r.dest == 1).cloned().unwrap();
        assert_eq!(r.param_for_cc(74), Some(42));

        // Duplicate MatrixCcMap does not bump rev.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(42), target: CcTarget::PluginParam, scale: Default::default() });
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
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(99), target: CcTarget::PluginParam, scale: Default::default() });
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: filter, cc: 7, param: Some(0), target: CcTarget::PluginParam, scale: Default::default() });
        engine.cc_log.clear();
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 74, value: 0 });
        bus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: 7, value: 64 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_log[0], (1, 99, 0.0));
        assert_eq!(engine.cc_log[1].0, filter);
        assert_eq!(engine.cc_log[1].1, 0);
        assert!((engine.cc_log[1].2 - 64.0 / 127.0).abs() < 1e-6);

        // 5. Remove CC 74 -> node 1 returns to empty-map transparency.
        bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: None, target: CcTarget::PluginParam, scale: Default::default() });
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
            scale: Default::default(),
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

    /// GitHub #97: a keyboard-strip pitch bend routes through the matrix
    /// like a CC — every rig the Keys source feeds bends, gated by the
    /// MIDI cable (and, with a hosted voice, the per-rig keyboard switch).
    #[test]
    fn pitch_bend_routes_to_the_rigs_the_keys_feed() {
        let (mut bus, drain) = command_bus();
        let mut engine = super::GraphEngine::new(crate::control::Graph::default(), drain, 48000.0, vec![]);
        bus.send(Command::LoadGraph { graph: crate::control::Graph::demo_patch() });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        // +4095 = 0x2FFF: lsb 127, msb 95.
        engine.cc_passthrough_log.clear();
        bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 4095 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_passthrough_log, vec![(1, 127, 95), (4, 127, 95)], "both rigs bend by +4095");
        // Center: 0 = 0x2000 (lsb 0, msb 64).
        engine.cc_passthrough_log.clear();
        bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 0 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_passthrough_log, vec![(1, 0, 64), (4, 0, 64)], "back to center on both");
        // The strip's mod (CC 1) and sustain (CC 64) also pass through to
        // the rigs the keys feed.
        engine.cc_passthrough_log.clear();
        bus.send(Command::Cc { source: SourceId::Keys, channel: 0, cc: 1, value: 127 });
        bus.send(Command::Cc { source: SourceId::Keys, channel: 0, cc: 64, value: 127 });
        engine.render(&mut buf, 2);
        assert_eq!(engine.cc_passthrough_log, vec![(1, 1, 127), (4, 1, 127), (1, 64, 127), (4, 64, 127)], "mod and sustain reach every rig the keys feed");
    }

    /// GitHub #97, the audible half: a full pitch bend (+2 st at Surge's
    /// default range) raises the rendered pitch — counted as zero crossings
    /// of the master output, like `organ-test` counts drawbar brightness.
    #[test]
    fn surge_pitch_bend_bends_the_note() {
        let Ok((voice, _main)) = crate::host::ClapVoice::new(48000.0) else { return };
        let (mut bus, drain) = command_bus();
        let mut g = crate::control::Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let mut engine = super::GraphEngine::new(g, drain, 48000.0, vec![(surge, voice)]);
        let mut buf = vec![0.0f32; 256 * 2];
        // Zero crossings/s of the left channel: a saw's period crosses twice,
        // so crossings/s ≈ 2 × the fundamental.
        let mut crossings = |engine: &mut super::GraphEngine, blocks: usize, settle: usize| -> f32 {
            for _ in 0..settle { engine.render(&mut buf, 2); }
            let mut n = 0usize;
            for _ in 0..blocks {
                engine.render(&mut buf, 2);
                for i in (2..buf.len()).step_by(2) {
                    if (buf[i - 2] <= 0.0) != (buf[i] <= 0.0) { n += 1; }
                }
            }
            n as f32 * 48000.0 / (blocks as f32 * 256.0)
        };
        bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
        let straight = crossings(&mut engine, 100, 20);
        bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 8191 });
        let bent = crossings(&mut engine, 100, 20);
        bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 0 });
        let back = crossings(&mut engine, 100, 20);
        bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
        engine.render(&mut buf, 2);
        println!("pitch bend: {straight:.0} → {bent:.0} → {back:.0} crossings/s (full up = +2 st ≈ ×1.12)");
        assert!(straight > 50.0, "no signal at the master: {straight:.0} crossings/s");
        assert!(bent > straight * 1.05, "full pitch bend did not raise the pitch: {straight:.0} → {bent:.0} crossings/s");
        assert!(
            (back / straight - 1.0).abs() < 0.05,
            "releasing the wheel did not return to the straight pitch: {straight:.0} → {back:.0} crossings/s"
        );
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
            scale: Default::default(),
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
            scale: Default::default(),
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
/// setBfree's face controls through the engine (`organ.rs`): drawbar
/// registrations reach the real organ as its MIDI CCs, including values the
/// face sends while the instance is still loading on the loader thread.
pub fn organ_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, NodeKind, PortKind, PortRef};
    use crate::matrix::SourceId;
    use crate::organ::{drawbar_param, B_SYNTH, DRAWBARS};
    let sr = 48000.0;
    anyhow::ensure!(crate::lv2::Lv2Plugin::load(B_SYNTH, sr, 512).is_ok(), "setBfree is not installed");
    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let organ = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(B_SYNTH) }, name_bytes("ORGAN"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: organ, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: organ, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);
    engine.spawn_lv2_loader();
    let mut buf = vec![0.0f32; 512 * 2];
    // Zero crossings per second of the left channel while `note` is held:
    // a registration's pitch content (16' alone ≈ the fundamental an octave
    // down, 1' alone ≈ three octaves up).
    let measure = |engine: &mut GraphEngine, bus: &mut crate::control::CommandBus, note: u8| -> (f32, f32) {
        let mut buf = vec![0.0f32; 512 * 2];
        bus.send(Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys });
        for _ in 0..20 { engine.render(&mut buf, 2); }
        let (mut zc, mut prev, mut peak, mut n) = (0usize, 0.0f32, 0.0f32, 0usize);
        for _ in 0..47 {
            engine.render(&mut buf, 2);
            for f in buf.chunks(2) {
                if (f[0] >= 0.0) != (prev >= 0.0) { zc += 1; }
                prev = f[0];
                peak = peak.max(f[0].abs());
                n += 1;
            }
        }
        bus.send(Command::NoteOff { note, source: SourceId::Keys });
        for _ in 0..200 { engine.render(&mut buf, 2); }
        (zc as f32 / (n as f32 / sr as f32), peak)
    };
    let registration = |bus: &mut crate::control::CommandBus, bars: [u8; DRAWBARS]| {
        for (i, b) in bars.iter().enumerate() {
            bus.send(Command::SetParam { node: organ, param: drawbar_param(i), value: crate::organ::drawbar_value(*b) as f64 });
        }
    };
    // The face publishes right after the load, long before the instance is
    // live (it loads on the loader thread): 1' only.
    bus.send(Command::LoadGraph { graph: g });
    registration(&mut bus, [0, 0, 0, 0, 0, 0, 0, 0, 8]);
    let t0 = std::time::Instant::now();
    while !engine.lv2.iter().any(|s| s.node == organ) && t0.elapsed().as_secs() < 10 {
        engine.render(&mut buf, 2);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    anyhow::ensure!(engine.lv2.iter().any(|s| s.node == organ), "setBfree did not load within 10 s");
    let (high, high_peak) = measure(&mut engine, &mut bus, 48);
    registration(&mut bus, [8, 0, 0, 0, 0, 0, 0, 0, 0]);
    for _ in 0..10 { engine.render(&mut buf, 2); }
    let (low, low_peak) = measure(&mut engine, &mut bus, 48);
    println!("organ-test: C3 with 1' only: {high:.0} zero crossings/s (peak {high_peak:.3}); 16' only: {low:.0}/s (peak {low_peak:.3})");
    anyhow::ensure!(high_peak > 1e-3 && low_peak > 1e-3, "the organ is silent");
    // 1' sounds 4 octaves above 16': 16x the fundamental. Ask for 6x, so the
    // Leslie's movement and the percussion cannot fake it.
    anyhow::ensure!(high > 6.0 * low, "the drawbars did not change the registration ({high:.0} vs {low:.0} crossings/s): the CCs (or the ones sent while loading) did not arrive");

    // A Nord Electro 2 on a MIDI interface plays the same registrations with
    // its own drawbar CCs, through the profile's bindings.
    let nord = SourceId::Hardware(3);
    let profile = crate::instruments::builtin().into_iter().find(|p| p.id == "nord-electro-2").expect("built-in profile");
    let organ_kind = NodeKind::Lv2 { uri: Lv2Uri::new(B_SYNTH) };
    for b in profile.bindings(&organ_kind) {
        bus.send(Command::MatrixCcMap { source: nord, dest: organ, cc: b.cc, param: Some(b.param), target: b.target, scale: b.scale });
    }
    let nord_bars = |bus: &mut crate::control::CommandBus, bars: [u8; DRAWBARS]| {
        for (i, b) in bars.iter().enumerate() {
            // The Electro's LED drawbars send the start of each position's section.
            let value = (*b as u32 * 128).div_ceil(9) as u8;
            bus.send(Command::Cc { source: nord, channel: 0, cc: 16 + i as u8, value });
        }
    };
    nord_bars(&mut bus, [0, 0, 0, 0, 0, 0, 0, 0, 8]);
    for _ in 0..10 { engine.render(&mut buf, 2); }
    let (nord_high, _) = measure(&mut engine, &mut bus, 48);
    nord_bars(&mut bus, [8, 0, 0, 0, 0, 0, 0, 0, 0]);
    for _ in 0..10 { engine.render(&mut buf, 2); }
    let (nord_low, _) = measure(&mut engine, &mut bus, 48);
    println!("organ-test: Nord Electro 2 drawbars: 1' only {nord_high:.0}/s; 16' only {nord_low:.0}/s");
    anyhow::ensure!(nord_high > 6.0 * nord_low, "the Nord Electro 2 drawbar CCs did not change the registration ({nord_high:.0} vs {nord_low:.0} crossings/s)");
    println!("organ-test: OK — drawbar CCs reach setBfree, also those sent while it loaded, and a Nord Electro 2 pulls them");
    Ok(())
}

/// `midifx-test`: the REAL Carla "MIDI Transpose" LV2 plugin as a MIDI strip
/// (issue #86). Scripted events → matrix → MIDI-FX unit → rig instrument:
/// the instrument must receive the transposed pitch, a knob value sent while
/// the plugin still loads must apply (rule 113), and a key released after the
/// knob moved must leave nothing sounding (rule 41).
pub fn midifx_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, Graph, Lv2Uri, MidiFxPlugin, NodeKind, PortKind, PortRef};
    use crate::matrix::SourceId;
    const TRANSPOSE: &str = "http://kxstudio.sf.net/carla/plugins/miditranspose";
    const OSC: &str = "urn:distrho:vitalium";
    let sr = 48000.0;
    anyhow::ensure!(crate::lv2::Lv2Plugin::load(TRANSPOSE, sr, 512).is_ok_and(|p| p.is_midi_fx()), "Carla MIDI Transpose is not installed");
    anyhow::ensure!(crate::lv2::Lv2Plugin::load(OSC, sr, 512).is_ok_and(|p| p.is_instrument()), "Vitalium is not installed");
    let semis_ctl = crate::lv2::control_descs_for_uri(TRANSPOSE)
        .into_iter()
        .find(|c| c.symbol == "semitones")
        .ok_or_else(|| anyhow::anyhow!("the plugin has no 'semitones' control"))?;
    let norm = |semis: f32| ((semis - semis_ctl.min) / (semis_ctl.max - semis_ctl.min)) as f64;

    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let fx = g.add_node(NodeKind::MidiFx { plugin: MidiFxPlugin::Lv2 { uri: Lv2Uri::new(TRANSPOSE) } }, name_bytes("MIDI TRANSPOSE"));
    let inst = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(OSC) }, name_bytes("OSC"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: fx, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi);
    g.connect_with_id(3, PortRef { node: inst, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, sr, vec![]);
    engine.spawn_lv2_loader();
    let mut buf = vec![0.0f32; 512 * 2];
    bus.send(Command::LoadGraph { graph: g });
    // +7 semitones, sent before the plugin is live.
    bus.send(Command::SetParam { node: fx, param: semis_ctl.index as u32, value: norm(7.0) });
    let t0 = std::time::Instant::now();
    let live = |e: &GraphEngine| e.midi_fx.iter().any(|s| s.node == fx && s.proc.is_some()) && e.lv2.iter().any(|s| s.node == inst);
    while !live(&engine) && t0.elapsed().as_secs() < 15 {
        engine.render(&mut buf, 2);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    anyhow::ensure!(live(&engine), "the MIDI unit and the instrument did not load within 15 s");

    let mut peak = |engine: &mut GraphEngine, blocks: usize| {
        let mut p = 0.0f32;
        for _ in 0..blocks {
            engine.render(&mut buf, 2);
            p = buf.iter().fold(p, |m, s| m.max(s.abs()));
        }
        p
    };
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    let held = peak(&mut engine, 20);
    let ev = engine.lv2_last_note(inst);
    println!("midifx-test: C4 through MIDI Transpose (+7, set while loading) reached the instrument as {ev:?}; master peak {held:.4}");
    anyhow::ensure!(matches!(ev, Some([0x90, 67, _])), "expected note-on 67 at the instrument, got {ev:?}");
    anyhow::ensure!(held > 1e-3, "the instrument is silent behind the MIDI unit");
    // Move the knob while the key is held, then release the key.
    bus.send(Command::SetParam { node: fx, param: semis_ctl.index as u32, value: norm(-5.0) });
    peak(&mut engine, 5);
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    peak(&mut engine, 375);
    let after = peak(&mut engine, 94);
    println!("midifx-test: knob moved to -5 while C4 was held, key released: master peak afterwards {after:.5} (held {held:.4})");
    anyhow::ensure!(after < held * 0.01, "a note stayed on after release: the unit lost a note-off when its knob moved");
    // The new setting applies to the next note.
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    peak(&mut engine, 5);
    let ev = engine.lv2_last_note(inst);
    println!("midifx-test: next C4 at -5 reached the instrument as {ev:?}");
    anyhow::ensure!(matches!(ev, Some([0x90, 55, _])), "expected note-on 55 after the knob moved, got {ev:?}");
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    peak(&mut engine, 375);
    let tail = peak(&mut engine, 94);
    anyhow::ensure!(tail < held * 0.01, "second note stayed on after release");
    // Removing the unit while a key is held must release it, not strand it.
    bus.send(Command::NoteOn { note: 62, velocity: 0.9, source: SourceId::Keys });
    peak(&mut engine, 5);
    bus.send(Command::RemoveNode(fx));
    peak(&mut engine, 5);
    bus.send(Command::NoteOff { note: 62, source: SourceId::Keys });
    peak(&mut engine, 375);
    let removed = peak(&mut engine, 94);
    anyhow::ensure!(removed < held * 0.01, "removing the MIDI unit under a held key left the note sounding");
    println!("midifx-test: OK — Carla MIDI Transpose transposes through the matrix, takes knob values sent while loading, and leaves no held note after a knob move or removal");
    Ok(())
}

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
    // The keyboard strip's standard signals (GitHub #97) reach an LV2
    // instrument as-is: modulation (CC 1), sustain (CC 64) and pitch bend.
    // What they DO is the plugin's business, like a pedal over real MIDI.
    bus.send(Command::Cc { source: SourceId::Keys, channel: 0, cc: 1, value: 90 });
    peak_over(&mut engine, 2);
    anyhow::ensure!(
        matches!(engine.lv2_last_note(inst), Some(e) if e == [0xB0, 1, 90]),
        "mod wheel CC 1 did not reach the LV2 instrument: {:?}", engine.lv2_last_note(inst)
    );
    bus.send(Command::Cc { source: SourceId::Keys, channel: 0, cc: 64, value: 127 });
    peak_over(&mut engine, 2);
    anyhow::ensure!(
        matches!(engine.lv2_last_note(inst), Some(e) if e == [0xB0, 64, 127]),
        "sustain CC 64 did not reach the LV2 instrument: {:?}", engine.lv2_last_note(inst)
    );
    bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 2010 });
    peak_over(&mut engine, 2);
    anyhow::ensure!(
        matches!(engine.lv2_last_note(inst), Some(e) if e[0] == 0xE0 && (((e[2] as i16) << 7) | e[1] as i16) - 8192 == 2010),
        "pitch bend did not reach the LV2 instrument: {:?}", engine.lv2_last_note(inst)
    );
    // Leave the plugin centered and the pedal up for what follows.
    bus.send(Command::Cc { source: SourceId::Keys, channel: 0, cc: 64, value: 0 });
    bus.send(Command::PitchBend { source: SourceId::Keys, channel: 0, value: 0 });
    peak_over(&mut engine, 2);
    println!("lv2-instrument-engine-test: strip signals (mod, sustain, bend) delivered");
    // Through a built-in effect, like a live organ rig (setBfree → Drive →
    // Master): the effect must hear the LV2 instrument.
    let drive = master + 1;
    bus.send(Command::AddNode { kind: NodeKind::Dsp(crate::control::DspKind::Saturation), name: name_bytes("DRIVE"), rig: 0, wire: false });
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

/// Share of the signal's power that is not the `freq` sine (least-squares
/// fit of one sine+cosine over `x`): ~0 for a clean tone, grows with
/// distortion. Robust to a slowly decaying envelope, unlike a single
/// Goertzel bin.
fn non_fundamental_share(x: &[f32], freq: f32, sr: f32) -> f32 {
    let k = 2.0 * std::f32::consts::PI * freq / sr;
    let (mut ss, mut cc, mut sc, mut xs, mut xc, mut xx) = (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = ((k * i as f32).sin() as f64, (k * i as f32).cos() as f64);
        let v = v as f64;
        ss += s * s; cc += c * c; sc += s * c;
        xs += v * s; xc += v * c; xx += v * v;
    }
    let det = ss * cc - sc * sc;
    if det.abs() < 1e-9 || xx < 1e-12 {
        return 0.0;
    }
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let fit = a * xs + b * xc;
    ((xx - fit).max(0.0) / xx) as f32
}

/// `guitar-rig-test` (issue #92): the Guitar rig template end to end — a
/// synthetic plucked-string input into the Audio In ring, through the
/// template's stomps, amp and cabinet, audibly transformed at the Master.
/// Skips cleanly when the Guitarix card is not installed.
pub fn guitar_rig_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, Command, NodeKind};
    let sr = 48000.0f32;
    if crate::lv2::Lv2Plugin::load(crate::guitar::AMP, sr as f64, 512).is_err() {
        println!("guitar-rig-test: SKIP — the Guitarix card is not installed (catalog card 'guitarix')");
        return Ok(());
    }
    let session = crate::session::Template::GuitarRig.session();
    let ain = session.graph.nodes.iter()
        .find(|n| matches!(n.kind, NodeKind::AudioIn { .. }))
        .map(|n| n.id)
        .ok_or_else(|| anyhow::anyhow!("the Guitar rig template has no Audio In"))?;
    let (mut bus, drain) = command_bus();
    // The app's path (rule 46): the loader thread instantiates the LV2 units
    // after a LoadGraph; the rig's values follow, and nothing is fed until
    // every unit is live (an unattached unit would pass nothing and make the
    // measurement a race).
    let mut engine = GraphEngine::new(crate::control::Graph::default(), drain, sr as f64, vec![]);
    engine.spawn_lv2_loader();
    bus.send(Command::LoadGraph { graph: session.graph.clone() });
    for p in session.params() {
        bus.send(Command::SetParam { node: p.node, param: p.param, value: p.value });
    }
    let want = session.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Lv2 { .. })).count();
    let t0 = std::time::Instant::now();
    let mut warm = vec![0.0f32; crate::bridge::BUFFER_FRAMES as usize * 2];
    while engine.lv2.len() < want {
        anyhow::ensure!(t0.elapsed().as_secs() < 15, "the guitarix units did not load ({}/{want})", engine.lv2.len());
        engine.render(&mut warm, 2);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for _ in 0..8 { engine.render(&mut warm, 2); }
    // Side channel + data ring, exactly as the app's capture path hands them
    // to the engine (the unit tests do the same).
    let (mut side_tx, side_rx) = rtrb::RingBuffer::<(u32, rtrb::Consumer<f32>)>::new(8);
    engine.set_audio_in_channel(side_rx);
    let ring = sr as usize * 2; // one second of stereo input
    let (mut data_tx, data_rx) = rtrb::RingBuffer::<f32>::new(ring);
    let _ = side_tx.push((ain, data_rx));

    // The "string": a 220 Hz tone with a slow pluck envelope (the tube stage
    // needs level to crunch, so the decay is gentle), then a 40 ms fade so
    // the input ends at silence (a starved ring repeats its last sample —
    // never leave it holding a nonzero constant).
    let steady_secs = 1.0f32;
    let n = (sr * steady_secs) as usize;
    let mut input = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / sr;
        let env = (-t * 0.8).exp();
        let s = (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.45 * env;
        input.push(s);
        input.push(s);
    }
    let fade = (0.04 * sr) as usize;
    for i in 0..fade {
        let g = 1.0 - i as f32 / fade as f32;
        input[i * 2] *= g;
        input[i * 2 + 1] *= g;
    }
    // Render in 256-frame blocks, feeding the ring as the engine drains it.
    let block = crate::bridge::BUFFER_FRAMES as usize;
    let mut buf = vec![0.0f32; block * 2];
    let mut master: Vec<f32> = Vec::with_capacity(n * 2);
    let mut fed = 0usize;
    engine.render(&mut buf, 2); // attach the consumer, settle the chain
    // A capture delivers a block at a time, never a second at once (the
    // engine cuts a backlog to its target fill, Drift::catch_up): keep the
    // ring two blocks ahead of the render.
    while master.len() < n * 2 {
        while fed < input.len() && ring - data_tx.slots() < block * 4 && data_tx.push(input[fed]).is_ok() {
            fed += 1;
        }
        engine.render(&mut buf, 2);
        master.extend_from_slice(&buf);
    }
    let steady = &master[..n * 2];
    let l: Vec<f32> = steady.iter().step_by(2).copied().collect();
    let peak = l.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    // The middle of the steady phase, clear of the pluck envelope's start.
    let mid = &l[(n / 4)..(3 * n / 4)];
    // 100 ms of the middle: short enough that the pluck decay is flat.
    let win = &mid[mid.len() / 2..mid.len() / 2 + (sr as usize / 10)];
    let ratio = non_fundamental_share(win, 220.0, sr);
    let rms = (mid.iter().map(|s| (s * s) as f64).sum::<f64>() / mid.len() as f64).sqrt() as f32;

    // The tail: input silent, the delay/reverb part of the chain still sounds.
    // The first 100 ms are the played note's own decay; the peak after that
    // is the echoes and the reverb.
    let mut tail_peak = 0.0f32;
    for blk in 0..(sr as usize * 12 / 10 / block) {
        engine.render(&mut buf, 2);
        if blk * block >= sr as usize / 10 {
            tail_peak = buf.iter().fold(tail_peak, |m, s| m.max(s.abs()));
        }
    }
    let (dropped, starved) = engine.audio_in_status(ain).unwrap_or((0, 0));
    println!(
        "guitar-rig-test: Audio In → Scream → Delay → Reverb → Amp → Cabinet → Master\n  \
         steady peak {peak:.4} RMS {rms:.4} · non-fundamental power {ratio:.4} · \
         tail peak after silence {tail_peak:.4} · ring {dropped} dropped {starved} starved"
    );
    anyhow::ensure!(peak > 1e-3, "the rig is silent at the master (peak {peak:.4})");
    anyhow::ensure!(ratio > 0.005, "the rig is transparent (non-fundamental power {ratio:.4}): the amp did not transform the input");
    anyhow::ensure!(tail_peak > 1e-4, "no delay/reverb tail after the input stopped (peak {tail_peak:.4})");
    println!("guitar-rig-test: OK — the input is audible and audibly transformed on the way to Master");
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
        bus.send(Command::AddNode { kind: NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name: name_bytes("VITALIUM"), rig: 0, wire: false });
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
        Load::SurgePatch(_) | Load::DexedVoice { .. } | Load::ValueTreePatch(_) | Load::AeolusRegistration(_) => {
            let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("no live plugin"))?;
            let state = main.save_state()?;
            anyhow::ensure!(Some(&state) != state_before.as_ref(), "plugin state unchanged after loading '{label}'");
            "plugin state changed".to_string()
        }
        Load::VitalPatch(path) => vital_patch_readback(&mut engine, node, path)?,
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
    // Aeolus' face draws one stop the way the face does it (state out, one
    // stop changed, state in): the pedal's Bourdon 16 must be heard and read back.
    let mut stop = String::new();
    if instrument.id() == crate::aeolus::AEOLUS_ID {
        let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("no live plugin"))?;
        let next = crate::aeolus::with_stop(&main.save_state()?, 3, 1, true).ok_or_else(|| anyhow::anyhow!("no Pedal Bourdon 16 in the state"))?;
        main.load_state(&next)?;
        for _ in 0..40 {
            engine.render(&mut buf, 2);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (main, _) = clap.as_mut().ok_or_else(|| anyhow::anyhow!("no live plugin"))?;
        let drawn = crate::aeolus::drawn(&main.save_state()?).ok_or_else(|| anyhow::anyhow!("the stops do not read back"))?;
        anyhow::ensure!(drawn[3] == 0b10, "the pedal reads back {:#b}, expected only Bourdon 16", drawn[3]);
        let bourdon = timbre_of_note(&mut engine, &mut bus, 60, sr);
        let d = after.distance(bourdon);
        anyhow::ensure!(d > 0.1, "drawing the Bourdon 16 changed nothing (distance {d:.3})");
        stop = format!(" · stop Pedal Bourdon 16 drawn, reads back, rms {:.4} ~{:.0} Hz (distance {d:.2})", bourdon.rms, bourdon.zero_cross_hz);
    }
    Ok(format!("{} {} · '{query}' finds {} (first: '{label}') · loaded: {readback} · note 60 before rms {:.4} ~{:.0} Hz, after rms {:.4} ~{:.0} Hz (distance {distance:.2}){stop}",
        library.sounds.len(), library.kind, hits.len(), before.rms, before.zero_cross_hz, after.rms, after.zero_cross_hz))
}

/// Does the node's Vitalium now hold `path`'s patch? Its saved state must
/// carry the patch's settings: every number in it, wavetables aside, and
/// the tempo, which follows the host's song clock.
fn vital_patch_readback(engine: &mut GraphEngine, node: NodeId, path: &std::path::Path) -> anyhow::Result<String> {
    let parse = |bytes: &[u8]| -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::from_str(String::from_utf8_lossy(bytes).trim_end_matches('\0'))?)
    };
    let patch = parse(&std::fs::read(path)?)?;
    let slot = engine.lv2.iter_mut().find(|s| s.node == node).ok_or_else(|| anyhow::anyhow!("LV2 slot gone"))?;
    let saved = parse(&slot.plugin.saved_state("urn:juce:stateBinary")?)?;
    let (want, have) = (&patch["settings"], &saved["settings"]);
    let numbers: Vec<(&String, f64)> = want.as_object().into_iter().flatten().filter(|(k, _)| *k != "beats_per_minute").filter_map(|(k, v)| Some((k, v.as_f64()?))).collect();
    let same = numbers.iter().filter(|(k, v)| have[k.as_str()].as_f64().is_some_and(|h| (h - v).abs() <= 1e-4 * v.abs().max(1.0))).count();
    let differ: Vec<String> = numbers.iter().filter(|(k, v)| !have[k.as_str()].as_f64().is_some_and(|h| (h - v).abs() <= 1e-4 * v.abs().max(1.0)))
        .map(|(k, v)| format!("{k} {v} → {}", have[k.as_str()])).collect();
    anyhow::ensure!(!numbers.is_empty() && same == numbers.len(), "{same} of {} patch settings hold the patch's value ({})", numbers.len(), differ.join(", "));
    Ok(format!("{same}/{} patch settings hold the patch's values", numbers.len()))
}

/// `sound-library-test`: every catalog instrument installed on this machine
/// lists its sound library, answers a search and loads one sound through
/// the real engine, proven by a readback and by the note sounding different.
/// `queries` (`<catalog id>=<words>`) replace the built-in search per
/// instrument, e.g. `vitalium=woody` to load an imported Vital patch.
pub fn sound_library_test(queries: &[String]) -> anyhow::Result<()> {
    use crate::sounds::Instrument;
    let asked = |id: &str| queries.iter().find_map(|q| q.strip_prefix(id).and_then(|r| r.strip_prefix('=')).map(str::to_string));
    // A search the instrument's library should answer.
    let builtin = |id: &str| match id {
        "setbfree" => "jazz",
        "dexed" => "brass",
        "vitalium" => "pad",
        "aeolus" => "flute",
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
        let query = asked(entry.id).unwrap_or_else(|| builtin(entry.id).to_string());
        match sound_library_check(&instrument, &query) {
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
    bus.send(Command::AddNode { kind: NodeKind::Dsp(DspKind::Gain), name: name_bytes("GAIN"), rig: 0, wire: false });
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
    // The master's DC blocker and limiter ring on for ~100 ms after a cut
    // (rule 133): flush that tail, then measure what stays.
    for _ in 0..30 { engine.render(&mut buf, 2); }
    let (zero_l, zero_r) = measure(&mut engine, &mut buf);
    anyhow::ensure!(zero_l < 1e-4 && zero_r < 1e-4, "vol 0 not silent ({zero_l}, {zero_r})");
    println!("rig-mix-test: OK — mute/solo/vol/pan shape the rig sum (both {both_l:.4}, muted {muted_l:.4}, solo {solo_l:.4}, pan R {pan_r:.4})");

    // ---- #73: per-SOURCE strip mix in a two-source rig ------------------
    // One rig, TWO synths into the SAME master: muting/soloing one source
    // must change only that source (#73 acceptance). LoadGraph remaps the
    // two hosted voices onto the new Clap nodes in node-id order.
    let (sa, sb) = (1u32, 2u32);
    {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let s0 = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE A"));
        let s1 = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE B"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: s0, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: s0, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: keys, port: 0 }, PortRef { node: s1, port: 0 }, PortKind::Midi);
        g.connect_with_id(4, PortRef { node: s1, port: 0 }, PortRef { node: m, port: 1 }, PortKind::Audio);
        let _ = (s0, s1);
        bus.send(Command::LoadGraph { graph: g });
    }
    // Every measurement retriggers the note: Surge's envelope decays, so a
    // measurement taken later would read lower for no mixing reason.
    let shot = |bus: &mut crate::control::CommandBus, e: &mut GraphEngine, buf: &mut [f32]| -> (f32, f32) {
        bus.send(Command::NoteOff { note: 62, source: SourceId::Keys });
        for _ in 0..40 { e.render(buf, 2); }
        bus.send(Command::NoteOn { note: 62, velocity: 0.9, source: SourceId::Keys });
        let (mut pl, mut pr) = (0.0f32, 0.0f32);
        for _ in 0..12 {
            e.render(buf, 2);
            for f in buf.chunks(2) {
                pl = pl.max(f[0].abs());
                pr = pr.max(f[1].abs());
            }
        }
        (pl, pr)
    };
    let (src_both_l, src_both_r) = shot(&mut bus, &mut engine, &mut buf);
    anyhow::ensure!(src_both_l > 1e-3 && src_both_r > 1e-3, "two sources produced no output ({src_both_l}, {src_both_r})");
    // Mute source A → only source B sounds.
    bus.send(Command::SetStripMix { node: sa, pan: None, mute: Some(true), solo: None });
    let (b_only_l, _b_only_r) = shot(&mut bus, &mut engine, &mut buf);
    anyhow::ensure!(b_only_l > 1e-3, "muted source A killed source B too ({b_only_l})");
    anyhow::ensure!(src_both_l > b_only_l * 1.1, "muting one source changed nothing (both {src_both_l} vs b-only {b_only_l})");
    // Unmute, then solo source B ≈ the same "B only" picture.
    bus.send(Command::SetStripMix { node: sa, pan: None, mute: Some(false), solo: None });
    bus.send(Command::SetStripMix { node: sb, pan: None, mute: None, solo: Some(true) });
    let (solo_b_l, _solo_b_r) = shot(&mut bus, &mut engine, &mut buf);
    anyhow::ensure!((solo_b_l - b_only_l).abs() < b_only_l * 0.25, "source solo != source mute-other (solo {solo_b_l} vs b-only {b_only_l})");
    bus.send(Command::SetStripMix { node: sb, pan: None, mute: None, solo: Some(false) });
    // Pan source A hard left: source B stays on both channels.
    bus.send(Command::SetStripMix { node: sa, pan: Some(-1.0), mute: None, solo: None });
    let (pan_src_l, pan_src_r) = shot(&mut bus, &mut engine, &mut buf);
    anyhow::ensure!(pan_src_r < src_both_r * 0.8, "hard-left pan did not reduce right (both {src_both_r} vs {pan_src_r})");
    anyhow::ensure!(pan_src_l > 1e-3 && pan_src_r > 1e-3, "source pan killed a channel (L {pan_src_l}, R {pan_src_r})");
    bus.send(Command::SetStripMix { node: sa, pan: Some(0.0), mute: None, solo: None });
    // Strip state is published back (rule 33): attach a publication, change
    // one strip, and read the entry the UI mirror adopts.
    let pub_: std::sync::Arc<std::sync::Mutex<GraphPub>> = Default::default();
    engine.set_graph_pub(pub_.clone());
    bus.send(Command::SetStripMix { node: sb, pan: None, mute: Some(true), solo: None });
    engine.render(&mut buf, 2);
    let strips = pub_.lock().unwrap().strips;
    let sb_entry = strips.iter().find(|s| s.node == sb).map(|s| s.mix);
    anyhow::ensure!(sb_entry == Some(StripMix { pan: 0.0, mute: true, solo: false, bypass: false }),
        "strip state not published back: {strips:?}");
    bus.send(Command::SetStripMix { node: sb, pan: None, mute: Some(false), solo: None });

    // ---- #74: bypass passes the input through --------------------------
    // Source A → GAIN(0) → master: silent while the gain is 0, audible the
    // moment the unit is bypassed (its knobs untouched).
    // Effects slew their gain over ~10 ms: let the change settle, then measure.
    let settled = |e: &mut GraphEngine, buf: &mut [f32]| -> (f32, f32) {
        for _ in 0..8 { e.render(buf, 2); }
        let (mut pl, mut pr) = (0.0f32, 0.0f32);
        for _ in 0..8 {
            e.render(buf, 2);
            for f in buf.chunks(2) {
                pl = pl.max(f[0].abs());
                pr = pr.max(f[1].abs());
            }
        }
        (pl, pr)
    };
    let gain = 2u32;
    {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let s0 = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE A"));
        let fx = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("GAIN"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: s0, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: s0, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: fx, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        bus.send(Command::LoadGraph { graph: g });
    }
    bus.send(Command::NoteOn { note: 64, velocity: 0.9, source: SourceId::Keys });
    bus.send(Command::SetParam { node: gain, param: 0, value: 0.0 });
    let silent = settled(&mut engine, &mut buf).0;
    anyhow::ensure!(silent < 1e-3, "gain 0 did not silence the chain ({silent})");
    bus.send(Command::SetStripBypass { node: gain, bypass: true });
    let bypassed = settled(&mut engine, &mut buf).0;
    anyhow::ensure!(bypassed > 1e-3, "bypass did not pass the input through ({bypassed})");
    bus.send(Command::SetStripBypass { node: gain, bypass: false });
    let again = settled(&mut engine, &mut buf).0;
    anyhow::ensure!(again < 1e-3, "un-bypassing did not restore the effect ({again})");

    // ---- #77: master inserts run on the summed bus, pre-limiter ---------
    // Master OUT → GAIN: the whole mix passes through the insert, and its
    // gain scales the summed output.
    let mfx = 3u32;
    {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let s0 = g.add_node(NodeKind::Clap { plugin_id: name_bytes("Surge XT") }, name_bytes("SURGE A"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let fx = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("INSERT"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: s0, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: s0, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: m, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Audio);
        let _ = (s0, keys);
        bus.send(Command::LoadGraph { graph: g });
    }
    bus.send(Command::NoteOn { note: 65, velocity: 0.9, source: SourceId::Keys });
    bus.send(Command::SetParam { node: mfx, param: 0, value: 1.0 });
    let with_insert = settled(&mut engine, &mut buf).0;
    bus.send(Command::SetParam { node: mfx, param: 0, value: 0.25 });
    let insert_quiet = settled(&mut engine, &mut buf).0;
    anyhow::ensure!(with_insert > 1e-3, "master insert chain produced no output ({with_insert})");
    anyhow::ensure!(insert_quiet < with_insert * 0.6 && insert_quiet > with_insert * 0.05,
        "master insert gain did not scale the summed mix (1.0 → {with_insert:.4}, 0.25 → {insert_quiet:.4})");
    bus.send(Command::SetStripBypass { node: mfx, bypass: true });
    let insert_bypassed = settled(&mut engine, &mut buf).0;
    anyhow::ensure!(insert_bypassed > insert_quiet * 1.8, "bypassing the master insert did not pass the mix ({insert_bypassed} vs {insert_quiet})");
    println!("strip-mix: OK — source mute/solo/pan only touch their source, bypass passes through, master inserts shape the sum");
    println!("  source both {src_both_l:.4} → B-only {b_only_l:.4}, solo B {solo_b_l:.4}; gain-0 {silent:.4} → bypass {bypassed:.4}; insert 1.0 {with_insert:.4} → 0.25 {insert_quiet:.4} → bypass {insert_bypassed:.4}");
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

    // Engine Info's node statistics, from the same blocks: every unit has a
    // row, and together they explain most of the block.
    let stats = crate::engine_info::EngineStats::build(
        crate::engine_info::EngineCounters {
            sample_rate: SR as u32,
            callback_frames: BLOCK as u32,
            render_avg_ns: (median * 1000.0) as u32,
            ..Default::default()
        },
        Some(&engine.node_times_handle()),
        0,
        |node| units.iter().position(|(n, _)| *n == node).map(|i| plugins[i].0.clone()),
    );
    println!("  node statistics (avg / max per {BLOCK}-frame block, share of the {budget_us:.0} us budget):");
    for n in &stats.nodes {
        println!("    {:<10} node {:>2}  avg {:7.1} us  max {:7.1} us  {:5.2} %", n.name, n.node, n.avg_us, n.max_us, n.percent);
    }
    let node_sum: f64 = stats.nodes.iter().map(|n| n.avg_us).sum();
    println!("    sum of units {node_sum:.0} us = {:.0} % of the median block ({median:.0} us)", node_sum / median * 100.0);
    anyhow::ensure!(stats.nodes.len() == units.len(), "{} of {} units were timed", stats.nodes.len(), units.len());
    anyhow::ensure!(stats.nodes.iter().all(|n| n.avg_us > 0.0 && n.max_us >= n.avg_us), "a unit has no render time: {:?}", stats.nodes);
    // The averages follow the latest blocks, so a spike lifts them over the
    // median; the worst block bounds them.
    anyhow::ensure!(node_sum > median * 0.5 && node_sum < max, "units sum to {node_sum:.0} us (median block {median:.0} us, worst {max:.0} us)");

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
    // The master's 5 Hz DC blocker rings on after the cut (rule 133): the
    // residual of this chord falls below 1e-4 after about 48 blocks.
    for _ in 0..64 {
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

/// `kabelsalat arp-test` (issue #90): Keys → Arp → a real Surge XT voice →
/// Master through the real engine. Two bars at 120 BPM of an up pattern over
/// a held triad must be C E G repeating on the 1/16 grid (125 ms apart, to
/// the block) and audible; chord mode must sound three voices per held key
/// and release them with the key; the Keys scale snap must move a C# to C
/// before the arp sees it.
pub fn arp_test() -> anyhow::Result<()> {
    use crate::arp::{step_value, ArpMode, ChordShape, Scale, ScaleChoice, P_ARP_CHORD, P_ARP_MODE};
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef, SURGE_PLUGIN_ID};
    use crate::host::ClapVoice;
    use crate::matrix::SourceId;
    const SR: f64 = 48000.0;
    const BLOCK: usize = 256;
    const BPM: f64 = 120.0;

    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let arp = g.add_node(NodeKind::Arp, name_bytes("ARP"));
    let surge = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_PLUGIN_ID) }, name_bytes("SURGE"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: arp, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: arp, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
    g.connect_with_id(3, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(4, PortRef { node: surge, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);

    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g.clone(), drain, SR, vec![]);
    let graph_pub = Arc::new(Mutex::new(GraphPub { graph: g, ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    let (mut tx, rx) = rtrb::RingBuffer::<VoiceMsg>::new(16);
    let (back_tx, _back) = rtrb::RingBuffer::<VoiceBack>::new(16);
    engine.set_voice_channels(rx, back_tx);
    let (voice, _main) = ClapVoice::new_plugin(SR, SURGE_PLUGIN_ID)?;
    tx.push(VoiceMsg::Add(VoiceSlot::new(voice))).map_err(|_| anyhow::anyhow!("voice ring full"))?;
    let mut buf = vec![0.0f32; BLOCK * 2];
    bus.send(Command::SetBpm { bpm: BPM as f32 });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_held(arp).is_empty() && engine.arp_sounding(arp).is_empty(), "a fresh arp is not silent");

    // 1. Up pattern, two bars.
    for note in [60u8, 64, 67] {
        bus.send(Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys });
    }
    let blocks = (SR * 60.0 / BPM * 8.0 / BLOCK as f64).ceil() as usize; // 8 beats
    let mut onsets: Vec<(usize, u8)> = Vec::new();
    let mut last: Vec<u8> = Vec::new();
    let mut peak = 0.0f32;
    for b in 0..blocks {
        engine.render(&mut buf, 2);
        peak = peak.max(buf.iter().fold(0.0f32, |m, x| m.max(x.abs())));
        let now = engine.arp_sounding(arp);
        for n in &now {
            if !last.contains(n) {
                onsets.push((b, *n));
            }
        }
        last = now;
    }
    let notes: Vec<u8> = onsets.iter().map(|o| o.1).collect();
    println!("  up, 1/16 @ {BPM} BPM, 2 bars: {} steps, first 9: {:?}", notes.len(), &notes[..notes.len().min(9)]);
    anyhow::ensure!((31..=33).contains(&notes.len()), "expected 32 sixteenth steps in 2 bars, got {}", notes.len());
    for (i, n) in notes.iter().enumerate() {
        let want = [60u8, 64, 67][i % 3];
        anyhow::ensure!(*n == want, "step {i}: played {n}, the up pattern wants {want} ({notes:?})");
    }
    let step_blocks = SR * 60.0 / BPM / 4.0 / BLOCK as f64; // 125 ms in blocks
    for w in onsets.windows(2) {
        let gap = (w[1].0 - w[0].0) as f64;
        anyhow::ensure!((gap - step_blocks).abs() <= 1.01, "steps {gap} blocks apart, the 1/16 grid is {step_blocks:.2}");
    }
    anyhow::ensure!(peak > 1e-3, "the arpeggio is silent: peak {peak}");
    println!("    audible (peak {peak:.3}); steps {:.1} ms apart on the grid", step_blocks * BLOCK as f64 / SR * 1000.0);

    // Releasing every key silences the arp at once.
    for note in [60u8, 64, 67] {
        bus.send(Command::NoteOff { note, source: SourceId::Keys });
    }
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp).is_empty() && engine.arp_held(arp).is_empty(), "arp still holds notes after the release");

    // 2. Chord mode: three voices per held key.
    let knob = |bus: &mut crate::control::CommandBus, param: u32, pos: usize, n: usize| {
        bus.send(Command::SetParam { node: arp, param, value: step_value(pos, n) as f64 });
    };
    knob(&mut bus, P_ARP_MODE, ArpMode::ALL.iter().position(|m| *m == ArpMode::Chord).unwrap(), ArpMode::ALL.len());
    bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp) == vec![60, 64, 67], "one key must sound a triad, got {:?}", engine.arp_sounding(arp));
    bus.send(Command::NoteOn { note: 65, velocity: 0.9, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp) == vec![60, 64, 65, 67, 69, 72], "two keys, two triads, got {:?}", engine.arp_sounding(arp));
    let sevenths = ChordShape::ALL.iter().position(|c| *c == ChordShape::Dominant7).unwrap();
    knob(&mut bus, P_ARP_CHORD, sevenths, ChordShape::ALL.len());
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp).len() == 8, "dominant 7ths on two keys = 8 voices, got {:?}", engine.arp_sounding(arp));
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp) == vec![65, 69, 72, 75], "releasing C leaves F7, got {:?}", engine.arp_sounding(arp));
    bus.send(Command::NoteOff { note: 65, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp).is_empty(), "chords left sounding after both releases");
    println!("  chord mode: 1 key → 3 voices, 2 keys → 6, 7th shape → 4 per key, release clears them");

    // 3. Scale snap on the Keys node: C# in C major sounds as C.
    bus.send(Command::SetKeysScale { node: keys, scale: Some(ScaleChoice { scale: Scale::Major, key: 0 }) });
    knob(&mut bus, P_ARP_CHORD, 0, ChordShape::ALL.len());
    bus.send(Command::NoteOn { note: 61, velocity: 0.9, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp) == vec![60, 64, 67], "C# in C major must snap to C, got {:?}", engine.arp_sounding(arp));
    bus.send(Command::NoteOff { note: 61, source: SourceId::Keys });
    engine.render(&mut buf, 2);
    anyhow::ensure!(engine.arp_sounding(arp).is_empty(), "the snapped note's release did not meet its pitch");
    println!("  scale snap: C# in C major → C (triad 60 64 67), released cleanly");

    println!("arp-test: OK");
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
        scale: Default::default(),
    });
    bus.send(Command::MatrixCcMap {
        source: SourceId::Hardware(0),
        dest: surge,
        cc: 7,
        param: Some(gain_p.id),
        target: CcTarget::PluginParam,
        scale: Default::default(),
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
    bus.send(Command::MatrixCcMap { source: SourceId::Hardware(0), dest: inst, cc: 20, param: Some(c.id), target: CcTarget::PluginParam, scale: Default::default() });
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

#[cfg(test)]
mod midifx_cc_tests {
    use super::*;
    use crate::control::{command_bus, name_bytes, Command, Graph, MidiFxPlugin, NodeKind, PortKind, PortRef};
    use crate::matrix::{CcTarget, SourceId};

    /// Keys → built-in transposer → Master (no instrument: only the unit's knob is observed).
    fn engine_with_strip() -> (GraphEngine, crate::control::CommandBus, u32) {
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::default(), drain, 48000.0, vec![]);
        let mut g = Graph::default();
        g.reserve(8, 8);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let fx = g.add_node(NodeKind::MidiFx { plugin: MidiFxPlugin::Builtin { semis: 0 } }, name_bytes("TRANSPOSE"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Midi);
        let _ = master;
        bus.send(Command::LoadGraph { graph: g });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        (engine, bus, fx)
    }

    fn semis(e: &GraphEngine, fx: u32) -> i8 {
        e.midi_fx.iter().find(|s| s.node == fx).expect("unit").semis
    }

    #[test]
    fn a_learned_cc_moves_a_midi_strips_knob() {
        for target in [CcTarget::Knob, CcTarget::PluginParam] {
            let (mut engine, mut bus, fx) = engine_with_strip();
            let mut buf = vec![0.0f32; 64 * 2];
            let dev = SourceId::Hardware(0);
            bus.send(Command::MatrixCcMap { source: dev, dest: fx, cc: 20, param: Some(P_MIDIFX_SEMI), target, scale: Default::default() });
            bus.send(Command::Cc { source: dev, channel: 0, cc: 20, value: 127 });
            engine.render(&mut buf, 2);
            assert_eq!(semis(&engine, fx), MIDIFX_SEMI_MAX, "{target:?}: full CC = top of the range");
            bus.send(Command::Cc { source: dev, channel: 0, cc: 20, value: 0 });
            engine.render(&mut buf, 2);
            assert_eq!(semis(&engine, fx), -MIDIFX_SEMI_MAX, "{target:?}: zero CC = bottom");
        }
    }
}
