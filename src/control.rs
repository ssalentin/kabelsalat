//! The single control plane (docs/ARCHITECTURE.md §2c).
//!
//! The UI, the built-in assistant, the CLI, and the future MCP server are
//! sibling frontends to this one typed API. Commands mutate the graph; the
//! engine consumes them at block boundaries via a lock-free bus. Queries read
//! snapshots. Nothing here touches the audio thread directly.
//!
//! RT discipline: commands are fixed-size (inline names, no String/Vec), so
//! draining them never allocates or frees. Graph storage is reserved up front.

use serde::{Deserialize, Serialize};

use crate::layer::{VelCurve, ZoneHands};
use crate::matrix::SourceId;

pub type NodeId = u32;
pub type EdgeId = u32;
pub type ParamId = u32;

/// Fixed-capacity inline name (no heap — safe to move through the RT bus).
pub type Name = [u8; 32];

pub fn name_bytes(s: &str) -> Name {
    let mut n = [0u8; 32];
    let b = s.as_bytes();
    let len = b.len().min(31);
    n[..len].copy_from_slice(&b[..len]);
    n
}

pub fn name_str(n: &Name) -> &str {
    let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
    std::str::from_utf8(&n[..end]).unwrap_or("?")
}

/// An LV2 plugin URI — longer than `Name`, stored in a fixed 128-byte buffer
/// and (de)serialized as a string so graphs/sessions stay self-contained.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Lv2Uri(pub [u8; 128]);

impl Lv2Uri {
    pub fn new(s: &str) -> Self {
        let mut b = [0u8; 128];
        let bytes = s.as_bytes();
        let len = bytes.len().min(127);
        b[..len].copy_from_slice(&bytes[..len]);
        Self(b)
    }
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&c| c == 0).unwrap_or(self.0.len());
        std::str::from_utf8(&self.0[..end]).unwrap_or("")
    }
}

impl std::fmt::Debug for Lv2Uri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Lv2Uri({})", self.as_str())
    }
}

impl Serialize for Lv2Uri {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Lv2Uri {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Lv2Uri::new(&s))
    }
}

/// A CLAP plugin id. Ids run past `Name`'s 31 bytes
/// (`studio.kx.distrho.cardinal#synth` is 32, LSP's up to 55), so it is held
/// in 128 bytes like [`Lv2Uri`] and saved as a string. Sessions saved before
/// hold the id as a 32-byte array; both forms load.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClapId([u8; 128]);

impl ClapId {
    pub fn new(s: &str) -> Self {
        Self(Lv2Uri::new(s).0)
    }
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&c| c == 0).unwrap_or(self.0.len());
        std::str::from_utf8(&self.0[..end]).unwrap_or("")
    }
}

impl std::fmt::Debug for ClapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClapId({})", self.as_str())
    }
}

impl Serialize for ClapId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ClapId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Text(String),
            Bytes(Name),
        }
        Ok(match Saved::deserialize(d)? {
            Saved::Text(s) => ClapId::new(&s),
            Saved::Bytes(n) => ClapId::new(name_str(&n)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DspKind {
    Gain,
    Pan,
    Eq3,
    Filter,
    Limiter,
    Saturation,
    Delay,
    Reverb,
    /// Transport-driven click generator. A normal rack node: wire its audio
    /// output to a master to hear it. No native params (BPM comes from the
    /// transport's SetBpm command).
    Metronome,
    /// Aux bus (issue #5): a mixer channel fed by sends (`Command::SetSend`),
    /// not by cables. Its OUT is a normal audio jack: the cables from it are
    /// its insert chain, and whatever reaches a master from there returns to
    /// the main mix past the rig faders. Param 0 = its fader (linear gain).
    Bus,
}

/// Where a send taps its signal (issue #5): a whole rig at its rig bus, or
/// one node at its own output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendSource {
    Rig(u8),
    Node(NodeId),
}

/// One send (issue #5): `level` (linear gain, 0..=1) of `from` into the aux
/// bus node `bus`. `pre` taps before the source's fader and pan, so the send
/// keeps its level when the fader moves; mute and solo silence it either way.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuxSend {
    pub from: SendSource,
    pub bus: NodeId,
    pub level: f32,
    #[serde(default)]
    pub pre: bool,
}

impl AuxSend {
    /// A send that does nothing and remembers nothing: not worth an entry.
    pub fn is_default(&self) -> bool {
        self.level <= 0.0 && !self.pre
    }
}

/// A PipeWire sink's node name (`alsa_output.usb-…HiFi__Line__sink`), held
/// in fixed bytes so the engine can keep and publish it without the heap.
/// Saved as a string.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SinkName(ClapId);

impl SinkName {
    /// Longest name the type holds; a longer one would be cut and never
    /// match its sink again.
    pub const MAX_LEN: usize = 127;
    pub const NONE: Self = Self(ClapId([0; 128]));
    pub fn new(s: &str) -> Self {
        Self(ClapId::new(s))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub fn is_none(&self) -> bool {
        self.as_str().is_empty()
    }
}

impl std::fmt::Debug for SinkName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SinkName({})", self.as_str())
    }
}

/// One bus that leaves the main mix (#147): the rig bus or aux bus `bus`
/// (an aux bus is `SendSource::Node` of its node) plays to `sink` and is no
/// longer part of what the master plays. A bus without an entry plays into
/// the master.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusOut {
    pub bus: SendSource,
    pub sink: SinkName,
}

/// Which device channels an `AudioIn` node turns into its stereo OUT. A guitar
/// or vocal mic is usually mono on one input of a stereo interface, so the
/// node picks the left or right channel (centered to both); `Stereo` passes the
/// pair through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioInChannel {
    MonoL,
    MonoR,
    Stereo,
}

impl Default for AudioInChannel {
    fn default() -> Self {
        AudioInChannel::MonoL
    }
}

impl AudioInChannel {
    /// Short face readout: which physical input channel of the interface.
    pub fn label(&self) -> &'static str {
        match self {
            AudioInChannel::MonoL => "In 1",
            AudioInChannel::MonoR => "In 2",
            AudioInChannel::Stereo => "ST",
        }
    }

    /// Device channels the selection reads.
    pub fn count(&self) -> u32 {
        match self {
            AudioInChannel::MonoL | AudioInChannel::MonoR => 1,
            AudioInChannel::Stereo => 2,
        }
    }
}

/// A hosted MIDI-effect plugin (AUM "MIDI strips"): events in, processed
/// events out — a rack unit that sits IN the MIDI path (arpeggiators,
/// chord tools, MIDI filters, transposers). `NodeKind::MidiFx` carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MidiFxPlugin {
    /// LV2 MIDI plugin, by URI: MIDI in, MIDI out, no audio port (a plugin
    /// that is no instrument, rule 72, and could never be an audio effect).
    Lv2 { uri: Lv2Uri },
    /// Built-in transposer — always available, no plugin to load. Also the
    /// portable engine-test path (rule 37: unit tests never host a plugin).
    Builtin { semis: i8 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// MIDI source: the computer keyboard (on-screen + laptop), hardware MIDI.
    Keys,
    /// Hosted CLAP plugin (e.g. Surge XT).
    Clap { plugin_id: ClapId },
    /// Built-in processing node.
    Dsp(DspKind),
    /// Hosted LV2 plugin (audio effect), identified by URI.
    Lv2 { uri: Lv2Uri },
    /// Hosted MIDI-effect plugin (AUM MIDI strips): the unit's MIDI OUT is a
    /// routable matrix source (`SourceId::MidiFx(node)`), so it chains
    /// source → MIDI-FX → rig input through the matrix.
    MidiFx { plugin: MidiFxPlugin },
    /// Desktop playback capture (P2).
    SystemAudio,
    /// Live audio input (microphone / guitar / interface) — a rack-able source
    /// with an audio OUT port (issue #28). `device` names the cpal input device
    /// (`None` = the default input); `channels` picks which device channels fill
    /// the stereo OUT.
    AudioIn { device: Option<String>, channels: AudioInChannel },
    /// MIDI-generating unit (issue #90): sits between a keyboard source and a
    /// synth like an effect — held notes become an arpeggio, or (chord mode)
    /// one chord per key. One MIDI IN, one MIDI OUT; no audio. Settings are
    /// its rack knobs (`engine::knob_specs`, params `arp::P_ARP_*`).
    Arp,
    /// Stereo output bus.
    MasterOut,
}

impl NodeKind {
    /// Does this kind own a start/stop transport (per-instrument Play/Stop)?
    /// Today only the metronome. The wireable file-player node (backing
    /// tracks, issue #1) joins by returning true here: the `Node::playing`
    /// flag, `Command::SetNodePlaying`, master TransportPlay/Stop fan-out, the
    /// face chip and session persistence are all keyed on this predicate, so
    /// a new transport-capable kind needs only its engine render gate.
    pub fn supports_transport(&self) -> bool {
        matches!(self, NodeKind::Dsp(DspKind::Metronome))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: Name,
    /// Which rig this node belongs to (multi-rig). Single-graph patches are
    /// rig 0; sessions saved before multi-rig load as rig 0 too.
    #[serde(default)]
    pub rig: u8,
    /// Per-instrument transport state (start/stop is a property of the rack
    /// instrument, not only of the global transport). Meaningful only for
    /// kinds where [`NodeKind::supports_transport`] is true; always false
    /// otherwise. Persisted in sessions; older sessions load as stopped.
    #[serde(default)]
    pub playing: bool,
    /// Keyboard split point (absolute MIDI note) — a property of the Keys
    /// node, not a global flag (AUM model, user 2026-11-15). `None` = one
    /// MIDI OUT (port 0, full range). `Some(p)` = TWO MIDI outs: port 0 (LOW)
    /// carries played notes below `p`, port 1 (HIGH) notes at/above it;
    /// routing to rigs is plain graph wiring. Meaningful only for
    /// `NodeKind::Keys` (always None otherwise); older sessions load unsplit.
    #[serde(default)]
    pub split: Option<u8>,
    /// Octave shift of each split zone, `[LOW, HIGH]` (port 0 / port 1),
    /// in octaves within `-ZONE_OCTAVE_MAX..=ZONE_OCTAVE_MAX`. Applied to the
    /// notes a zone sends only while split; unsplitting resets it. A bass
    /// below the split usually plays an octave up: the keys there are low and
    /// many bass sounds are pitched down already. Older sessions load at 0.
    #[serde(default)]
    pub zone_octave: [i8; 2],
    /// Key + scale this Keys node's notes snap to (issue #90, a property of
    /// the Keys source like `split`): out-of-scale keys snap to the nearest
    /// in-scale note on their way out. `None` = no snapping (the default and
    /// what older sessions load).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<crate::arp::ScaleChoice>,
}

/// Which Keys output port a PLAYED note leaves on: unsplit → 0; split at
/// `p` → 0 (LOW) below `p`, 1 (HIGH) at/above. Allocation-free.
pub fn split_port(split: Option<u8>, note: u8) -> u8 {
    match split {
        Some(p) if note >= p => 1,
        _ => 0,
    }
}

/// The C4 split point the face chip toggles to (MIDI 60).
pub const DEFAULT_SPLIT: u8 = 60;
/// Largest octave shift of a split zone, either way.
pub const ZONE_OCTAVE_MAX: i8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortKind {
    Midi,
    Audio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRef {
    pub node: NodeId,
    pub port: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub from: PortRef,
    pub to: PortRef,
    pub kind: PortKind,
}

/// The patch. Serializable for sessions (task-8). Storage is reserved so
/// block-boundary edits within capacity never allocate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    next_node: NodeId,
    next_edge: EdgeId,
}

impl Graph {
    pub fn reserve(&mut self, nodes: usize, edges: usize) {
        self.nodes.reserve(nodes);
        self.edges.reserve(edges);
    }

    pub fn add_node(&mut self, kind: NodeKind, name: Name) -> NodeId {
        let id = self.next_node;
        self.next_node += 1;
        self.nodes.push(Node { id, kind, name, rig: 0, playing: false, split: None, zone_octave: [0; 2], scale: None });
        id
    }

    /// Tag a node with its rig index (multi-rig). Returns false if the node
    /// is unknown. [`merge_rigs`] tags whole rig graphs in one pass instead.
    #[allow(dead_code)] // public API for the UI (task-22 integration)
    pub fn set_rig(&mut self, id: NodeId, rig: u8) -> bool {
        match self.nodes.iter_mut().find(|n| n.id == id) {
            Some(n) => {
                n.rig = rig;
                true
            }
            None => false,
        }
    }

    /// Set a transport-capable node's playing flag. Returns `Some(changed)`
    /// for a transport-capable node, `None` if the node is unknown or has no
    /// transport (the flag stays false there).
    pub fn set_playing(&mut self, id: NodeId, playing: bool) -> Option<bool> {
        let n = self.nodes.iter_mut().find(|n| n.id == id)?;
        if !n.kind.supports_transport() {
            return None;
        }
        let changed = n.playing != playing;
        n.playing = playing;
        Some(changed)
    }

    /// Set (Some) or clear (None) a Keys node's key + scale snap. Returns
    /// `Some(changed)` for a Keys node, `None` if the node is unknown or not
    /// a Keys source (its scale stays None). Snapping is a pure function of
    /// the played note, so a note-off always meets the pitch its note-on
    /// snapped to — but an edit between them still releases first (engine).
    pub fn set_scale(&mut self, id: NodeId, choice: Option<crate::arp::ScaleChoice>) -> Option<bool> {
        let n = self.nodes.iter_mut().find(|n| n.id == id)?;
        if !matches!(n.kind, NodeKind::Keys) {
            return None;
        }
        let choice = choice.map(|c| crate::arp::ScaleChoice { key: c.key.min(11), ..c });
        let changed = n.scale != choice;
        n.scale = choice;
        Some(changed)
    }

    /// Insert `node` (an Arp) into its rig's MIDI chain, GarageBand-style:
    /// every MIDI cable from the rig's Keys node to an instrument is
    /// REPLACED by Keys → arp, and the arp feeds each instrument that cable
    /// carried (both split ports merge into the arp's one MIDI in). Fresh
    /// cable ids above every existing one. Returns how many cables it added.
    /// The engine releases held notes on every instrument the Keys node
    /// feeds before this (rule 41).
    pub fn wire_arp(&mut self, node: NodeId) -> usize {
        let Some(rig) = self.node(node).map(|n| n.rig) else { return 0 };
        let keys = self.nodes.iter().find(|n| n.rig == rig && matches!(n.kind, NodeKind::Keys)).map(|n| n.id);
        let Some(keys) = keys else { return 0 };
        // The rig's Keys→instrument MIDI cables this arp takes over.
        let claps: Vec<NodeId> = self.nodes.iter()
            .filter(|n| n.rig == rig && matches!(n.kind, NodeKind::Clap { .. }))
            .map(|n| n.id)
            .collect();
        let take_over: Vec<(u8, NodeId)> = self.edges.iter()
            .filter(|e| e.kind == PortKind::Midi && e.from.node == keys && claps.contains(&e.to.node))
            .map(|e| (e.from.port, e.to.node))
            .collect();
        self.edges.retain(|e| {
            !(e.kind == PortKind::Midi && e.from.node == keys && claps.contains(&e.to.node))
        });
        let mut added = 0;
        for &(port, inst) in &take_over {
            let fresh = self.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
            if self.connect_with_id(fresh, PortRef { node: keys, port }, PortRef { node, port: 0 }, PortKind::Midi) {
                added += 1;
            }
            let fresh = self.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
            if self.connect_with_id(fresh, PortRef { node, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi) {
                added += 1;
            }
        }
        added
    }

    /// Set (Some) or clear (None) a Keys node's split point. Clearing removes
    /// the node's HIGH-port (port ≥ 1) MIDI cables — the jack no longer
    /// exists. Returns `Some(changed)` for a Keys node, `None` if the node is
    /// unknown or not a Keys source (its split stays None).
    pub fn set_split(&mut self, id: NodeId, point: Option<u8>) -> Option<bool> {
        let n = self.nodes.iter_mut().find(|n| n.id == id)?;
        if !matches!(n.kind, NodeKind::Keys) {
            return None;
        }
        let point = point.map(|p| p.min(127));
        let mut changed = n.split != point;
        n.split = point;
        if point.is_none() {
            changed |= n.zone_octave != [0; 2];
            n.zone_octave = [0; 2];
            let before = self.edges.len();
            self.edges.retain(|e| !(e.from.node == id && e.from.port >= 1));
            changed |= self.edges.len() != before;
        }
        Some(changed)
    }

    /// Shift one zone (0 = LOW, 1 = HIGH) of a split Keys node by `octave`
    /// (clamped to ±`ZONE_OCTAVE_MAX`). Returns `Some(changed)` for a split
    /// Keys node, `None` otherwise (unknown, not Keys, unsplit, bad zone).
    pub fn set_zone_octave(&mut self, id: NodeId, zone: u8, octave: i8) -> Option<bool> {
        let n = self.nodes.iter_mut().find(|n| n.id == id)?;
        if !matches!(n.kind, NodeKind::Keys) || n.split.is_none() || zone > 1 {
            return None;
        }
        let o = octave.clamp(-ZONE_OCTAVE_MAX, ZONE_OCTAVE_MAX);
        let changed = n.zone_octave[zone as usize] != o;
        n.zone_octave[zone as usize] = o;
        Some(changed)
    }

    /// Semitones a played note moves on its way out of a Keys node: its
    /// zone's octave while split, else 0.
    pub fn zone_shift(n: &Node, played: u8) -> i16 {
        match n.split {
            Some(_) => n.zone_octave[split_port(n.split, played) as usize] as i16 * 12,
            None => 0,
        }
    }

    /// Master transport: set the playing flag on EVERY transport-capable
    /// node. Returns true if any flag changed. Allocation-free.
    pub fn set_all_playing(&mut self, playing: bool) -> bool {
        let mut changed = false;
        for n in self.nodes.iter_mut().filter(|n| n.kind.supports_transport()) {
            changed |= n.playing != playing;
            n.playing = playing;
        }
        changed
    }

    pub fn remove_node(&mut self, id: NodeId) {
        self.nodes.retain(|n| n.id != id);
        self.edges.retain(|e| e.from.node != id && e.to.node != id);
    }

    /// Connect with a caller-chosen edge id (the UI uses its cable ids).
    /// Rejects self-connections, duplicate ids, and duplicate endpoints.
    pub fn connect_with_id(&mut self, id: EdgeId, from: PortRef, to: PortRef, kind: PortKind) -> bool {
        if from.node == to.node {
            return false;
        }
        if !self.nodes.iter().any(|n| n.id == from.node)
            || !self.nodes.iter().any(|n| n.id == to.node)
        {
            return false;
        }
        if self.edges.iter().any(|e| e.id == id || (e.from == from && e.to == to)) {
            return false;
        }
        self.edges.push(Edge { id, from, to, kind });
        true
    }

    /// Plug an instrument into its rig the way a standard rig's synth is:
    /// the rig's Keys → it (MIDI), it → the rig's Master L and R. Cables
    /// take fresh ids above every existing one. Returns how many it added
    /// (fewer when the rig has no Keys or Master, or a cable already exists).
    pub fn wire_instrument(&mut self, node: NodeId) -> usize {
        let Some(rig) = self.node(node).map(|n| n.rig) else { return 0 };
        let find = |g: &Self, kind: fn(&NodeKind) -> bool| g.nodes.iter().find(|n| n.rig == rig && kind(&n.kind)).map(|n| n.id);
        let keys = find(self, |k| matches!(k, NodeKind::Keys));
        let master = find(self, |k| matches!(k, NodeKind::MasterOut));
        let mut cables = [None; 3];
        if let Some(keys) = keys {
            cables[0] = Some((PortRef { node: keys, port: 0 }, PortRef { node, port: 0 }, PortKind::Midi));
        }
        if let Some(master) = master {
            for port in 0..2u8 {
                cables[1 + port as usize] = Some((PortRef { node, port: 0 }, PortRef { node: master, port }, PortKind::Audio));
            }
        }
        let mut added = 0;
        for (from, to, kind) in cables.into_iter().flatten() {
            let id = self.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
            if self.connect_with_id(id, from, to, kind) {
                added += 1;
            }
        }
        added
    }

    /// Plug an aux bus (issue #5) into its rig's Master L and R: the return.
    /// Returns how many cables it added.
    pub fn wire_bus(&mut self, node: NodeId) -> usize {
        let Some(rig) = self.node(node).map(|n| n.rig) else { return 0 };
        let Some(master) = self.nodes.iter().find(|n| n.rig == rig && matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id) else { return 0 };
        let mut added = 0;
        for port in 0..2u8 {
            let id = self.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
            if self.connect_with_id(id, PortRef { node, port: 0 }, PortRef { node: master, port }, PortKind::Audio) {
                added += 1;
            }
        }
        added
    }

    /// The aux bus in send slot `slot`: the patch's buses in node order.
    /// A learned send binding names its bus this way (`MixControl::Send`).
    pub fn bus_at(&self, slot: usize) -> Option<NodeId> {
        self.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus))).nth(slot).map(|n| n.id)
    }

    /// The send slot of an aux bus (see [`Graph::bus_at`]).
    pub fn bus_slot(&self, bus: NodeId) -> Option<usize> {
        self.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus))).position(|n| n.id == bus)
    }

    /// Is this node an aux bus (issue #5)?
    pub fn is_bus(&self, node: NodeId) -> bool {
        self.node(node).is_some_and(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus)))
    }

    /// Is this node part of a bus's insert chain: an audio path leads from a
    /// bus to it without crossing a master? Such nodes run after the rig
    /// sums, so they cannot be send sources themselves.
    pub fn after_bus(&self, node: NodeId) -> bool {
        let mut seen: Vec<NodeId> = Vec::new();
        let mut todo: Vec<NodeId> = self.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus))).map(|n| n.id).collect();
        while let Some(n) = todo.pop() {
            for e in self.edges.iter().filter(|e| e.kind == PortKind::Audio && e.from.node == n) {
                let to = e.to.node;
                if seen.contains(&to) || self.node(to).is_some_and(|t| matches!(t.kind, NodeKind::MasterOut)) { continue; }
                seen.push(to);
                todo.push(to);
            }
        }
        seen.contains(&node)
    }

    pub fn disconnect(&mut self, id: EdgeId) {
        self.edges.retain(|e| e.id != id);
    }

    /// The instruments of a rig's MIDI chain: every node a MIDI cable from
    /// the rig's Keys or a MIDI-FX unit ends at. Judged by the cables (the
    /// graph's own facts), so an LV2 audio effect is never one.
    pub fn midi_sinks(&self, rig: u8) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        for e in self.edges.iter().filter(|e| e.kind == PortKind::Midi) {
            let from_chain = self.node(e.from.node).is_some_and(|n| {
                n.rig == rig && matches!(n.kind, NodeKind::Keys | NodeKind::MidiFx { .. })
            });
            let to = self.node(e.to.node);
            let to_inst = to.is_some_and(|n| n.rig == rig && !matches!(n.kind, NodeKind::MidiFx { .. }));
            if from_chain && to_inst && !out.contains(&e.to.node) {
                out.push(e.to.node);
            }
        }
        out
    }

    /// Insert a MIDI-FX unit into its rig's MIDI chain (AUM "MIDI strips"):
    /// every MIDI cable that fed an instrument from the rig's Keys (or from
    /// an earlier unit) is re-pointed at the new unit, and the unit feeds
    /// each of those instruments — so the chain order is the order units
    /// were added, and notes reach an instrument once, through the units.
    /// The cables are what gate the two routing legs (the engine's
    /// `keys_route` for the unit's input, `fx_cable` for its output). A rig
    /// without instruments just gets Keys → unit. Returns the cables added.
    pub fn wire_midi_fx(&mut self, node: NodeId) -> usize {
        let Some(rig) = self.node(node).map(|n| n.rig) else { return 0 };
        let sinks = self.midi_sinks(rig);
        let keys = self.nodes.iter().find(|n| n.rig == rig && matches!(n.kind, NodeKind::Keys)).map(|n| n.id);
        let fresh = |g: &Self| g.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
        let mut added = 0;
        let mut feeders: Vec<PortRef> = Vec::new();
        let mut gone: Vec<EdgeId> = Vec::new();
        for e in self.edges.iter().filter(|e| e.kind == PortKind::Midi && sinks.contains(&e.to.node)) {
            let from_chain = self.node(e.from.node).is_some_and(|n| {
                n.rig == rig && matches!(n.kind, NodeKind::Keys | NodeKind::MidiFx { .. }) && n.id != node
            });
            if from_chain {
                gone.push(e.id);
                if !feeders.contains(&e.from) {
                    feeders.push(e.from);
                }
            }
        }
        self.edges.retain(|e| !gone.contains(&e.id));
        if feeders.is_empty() {
            if let Some(keys) = keys {
                feeders.push(PortRef { node: keys, port: 0 });
            }
        }
        for from in feeders {
            let id = fresh(self);
            added += self.connect_with_id(id, from, PortRef { node, port: 0 }, PortKind::Midi) as usize;
        }
        for inst in sinks {
            let id = fresh(self);
            added += self.connect_with_id(id, PortRef { node, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi) as usize;
        }
        added
    }

    /// After a MIDI-FX unit left `rig`: give the instruments that lost their
    /// MIDI input a cable again — from the chain's remaining tail unit (one
    /// that feeds nothing) or else from the rig's Keys. Returns the cables added.
    pub fn heal_midi_after_fx(&mut self, rig: u8) -> usize {
        let has_in = |g: &Self, id: NodeId| g.edges.iter().any(|e| e.kind == PortKind::Midi && e.to.node == id);
        let tail = self
            .nodes
            .iter()
            .find(|n| {
                n.rig == rig
                    && matches!(n.kind, NodeKind::MidiFx { .. })
                    && !self.edges.iter().any(|e| e.kind == PortKind::Midi && e.from.node == n.id)
            })
            .map(|n| n.id);
        let source = tail.or_else(|| {
            self.nodes.iter().find(|n| n.rig == rig && matches!(n.kind, NodeKind::Keys)).map(|n| n.id)
        });
        let Some(source) = source else { return 0 };
        // Instruments by their own audio cables: a node of the rig that
        // sounds (an audio cable leaves it for the rig's Master) but lost MIDI.
        let orphans: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|n| {
                n.rig == rig
                    && matches!(n.kind, NodeKind::Clap { .. } | NodeKind::Lv2 { .. })
                    && !has_in(self, n.id)
                    && self.edges.iter().any(|e| e.kind == PortKind::Audio && e.from.node == n.id)
            })
            .map(|n| n.id)
            .collect();
        let mut added = 0;
        for inst in orphans {
            let id = self.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
            added += self.connect_with_id(id, PortRef { node: source, port: 0 }, PortRef { node: inst, port: 0 }, PortKind::Midi) as usize;
        }
        added
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn has_route(&self, from: NodeId, to: NodeId, kind: PortKind) -> bool {
        self.edges
            .iter()
            .any(|e| e.from.node == from && e.to.node == to && e.kind == kind)
    }

    /// Like [`has_route`] but matches a specific destination port.
    pub fn has_route_port(&self, from: NodeId, to: NodeId, port: u8, kind: PortKind) -> bool {
        self.edges
            .iter()
            .any(|e| e.from.node == from && e.to.node == to && e.to.port == port && e.kind == kind)
    }

    /// The starter patch: TWO channel stacks mixed to a shared master.
    /// Stack A: Keys→Surge A→Delay A→Master (+dry). Stack B: Keys→Surge B→
    /// Delay B→Master (+dry). Edge ids match the UI's cable ids; node ids
    /// 0=Keys, 1=Surge A, 2=Delay A, 3=Master, 4=Surge B, 5=Delay B.
    pub fn demo_patch() -> Self {
        let mut g = Self::default();
        g.reserve(64, 256);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let surge_a = g.add_node(
            NodeKind::Clap { plugin_id: ClapId::new("org.surge-synth-team.surge-xt") },
            name_bytes("SURGE A"),
        );
        let delay_a = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY A"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
        let surge_b = g.add_node(
            NodeKind::Clap { plugin_id: ClapId::new("org.surge-synth-team.surge-xt") },
            name_bytes("SURGE B"),
        );
        let delay_b = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY B"));
        g.next_edge = 1;
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: surge_a, port: 0 }, PortKind::Midi);
        g.connect_with_id(3, PortRef { node: surge_a, port: 0 }, PortRef { node: delay_a, port: 0 }, PortKind::Audio);
        g.connect_with_id(4, PortRef { node: delay_a, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(5, PortRef { node: surge_a, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        g.connect_with_id(6, PortRef { node: keys, port: 0 }, PortRef { node: surge_b, port: 0 }, PortKind::Midi);
        g.connect_with_id(7, PortRef { node: surge_b, port: 0 }, PortRef { node: delay_b, port: 0 }, PortKind::Audio);
        g.connect_with_id(8, PortRef { node: delay_b, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(9, PortRef { node: surge_b, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        g.next_edge = 10;
        g
    }
}

/// One independent rig: a full rack graph bound to a hardware MIDI device
/// (AUM operating model — a different keyboard per rig). `midi_device`:
/// `None` = any source (the computer keyboard, every hardware device);
/// `Some(id)` = only that hardware device. Each rig carries its own MasterOut;
/// the engine sums them all to the physical output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rig {
    pub name: String,
    pub graph: Graph,
    #[serde(default)]
    pub midi_device: Option<u32>,
}

impl Rig {
    pub fn new(name: impl Into<String>, graph: Graph, midi_device: Option<u32>) -> Self {
        Self { name: name.into(), graph, midi_device }
    }
}

/// Per-rig metadata: the rig list frontends (UI/MCP/bridge) display. Mirrors
/// [`Rig`] without the graph payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RigMeta {
    pub name: String,
    pub midi_device: Option<u32>,
}

/// Shared multi-rig state for frontends: the rig list plus the UI-selected
/// rig index. The engine writes it at block boundaries on `LoadRigs` (and
/// clears it on `LoadGraph`); frontends read it through `Arc<Mutex<_>>`.
/// The steady audio render path never locks it. Empty = single-graph mode.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RigSnapshot {
    pub rigs: Vec<RigMeta>,
    pub active: usize,
}

/// Merge independent rig graphs into ONE patch (multi-rig load).
///
/// Node ids are offset by a cumulative base per rig (edge ids likewise), edge
/// endpoints are rewritten to the offset ids, and every node is tagged with
/// its rig index (`Node::rig`, saturating at 255). Each rig keeps its own
/// MasterOut. The merged id counters continue past every id the rigs used
/// (counters survive removals), so later edits never collide.
/// Rare structural op: allocates by design, like `Command::LoadGraph`.
pub fn merge_rigs(rigs: &[Rig]) -> Graph {
    let bases = rig_bases(rigs);
    let mut merged = Graph::default();
    let nodes: usize = rigs.iter().map(|r| r.graph.nodes.len()).sum();
    let edges: usize = rigs.iter().map(|r| r.graph.edges.len()).sum();
    merged.reserve(nodes + 8, edges + 16);
    let mut node_base: NodeId = 0;
    let mut edge_base: EdgeId = 0;
    for (i, rig) in rigs.iter().enumerate() {
        let tag = i.min(u8::MAX as usize) as u8;
        for n in &rig.graph.nodes {
            let mut n = n.clone();
            n.id = n.id.saturating_add(node_base);
            n.rig = tag;
            merged.nodes.push(n);
        }
        for e in &rig.graph.edges {
            let mut e = e.clone();
            e.id = e.id.saturating_add(edge_base);
            e.from.node = e.from.node.saturating_add(node_base);
            e.to.node = e.to.node.saturating_add(node_base);
            merged.edges.push(e);
        }
        // Advance past every id the rig ever used, not just its live nodes.
        let max_edge = rig.graph.edges.iter().map(|e| e.id).max().map_or(0, |m| m + 1);
        node_base = bases.get(i + 1).copied().unwrap_or(node_base);
        edge_base = edge_base.saturating_add(rig.graph.next_edge.max(max_edge));
    }
    merged.next_node = node_base;
    merged.next_edge = edge_base;
    merged
}

/// Surge XT's CLAP id: the synth of the standard rig and of legacy graphs.
pub const SURGE_PLUGIN_ID: &str = "org.surge-synth-team.surge-xt";

/// The plugin a Clap node's voice has to be: its `plugin_id`, with the
/// legacy spellings (empty, "Surge XT") meaning Surge.
pub fn clap_plugin_key(plugin_id: &str) -> &str {
    if plugin_id.is_empty() || plugin_id == "Surge XT" { SURGE_PLUGIN_ID } else { plugin_id }
}

/// Which hosted voice plays which Clap node: every node, in graph order,
/// takes the free voice of its plugin with the lowest id. `voices` lists
/// `(voice id, plugin id)`; the result pairs each Clap node with its voice
/// (None: no free voice of that plugin). The engine binds by this rule
/// (`GraphEngine::bind_free_voices`); a voice host uses it to know the
/// binding of a load it is about to send.
pub fn bind_voices(graph: &Graph, voices: &[(u64, &str)]) -> Vec<(NodeId, Option<u64>)> {
    let mut taken = vec![false; voices.len()];
    graph
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Clap { plugin_id } => Some((n.id, clap_plugin_key(plugin_id.as_str()))),
            _ => None,
        })
        .map(|(node, key)| {
            let free = (0..voices.len())
                .filter(|&i| !taken[i] && voices[i].1 == key)
                .min_by_key(|&i| voices[i].0);
            if let Some(i) = free {
                taken[i] = true;
            }
            (node, free.map(|i| voices[i].0))
        })
        .collect()
}

/// Pick a unique name (at most 24 chars) for a cloned rig: "<source> Copy",
/// "<source> Copy 2", … avoiding collisions with `used`.
pub fn unique_clone_rig_name(source: &str, used: &[String]) -> String {
    let base = source.trim();
    let base = if base.is_empty() { "Rig" } else { base };
    let fit = |suffix: &str| -> String {
        let max_base = 24usize.saturating_sub(suffix.chars().count());
        let prefix: String = base.chars().take(max_base).collect();
        format!("{}{}", prefix.trim_end(), suffix)
    };
    let first = fit(" Copy");
    if !used.iter().any(|u| u == &first) {
        return first;
    }
    for k in 2..=99 {
        let cand = fit(&format!(" Copy {k}"));
        if !used.iter().any(|u| u == &cand) {
            return cand;
        }
    }
    format!("Rig {}", used.len() + 1)
}

/// The standard new rig ("+ Rig", agent/MCP add_rig): Keys → Surge → Master
/// (MIDI + stereo L/R); `with_synth = false` leaves the bare Keys + Master.
pub fn standard_rig_graph(with_synth: bool) -> Graph {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    if with_synth {
        let synth = g.add_node(
            NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) },
            name_bytes("SURGE"),
        );
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
        g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
    }
    g
}

/// A rig around one CLAP instrument: Keys → plugin → Master (MIDI + stereo
/// L/R). `name` labels the synth unit.
pub fn plugin_rig_graph(plugin_id: &str, name: &str) -> Graph {
    instrument_rig_graph(NodeKind::Clap { plugin_id: ClapId::new(plugin_id) }, name)
}

/// Single-instrument rig for any instrument node (CLAP or LV2): Keys →
/// instrument → Master, its output on both master inputs.
pub fn instrument_rig_graph(instrument: NodeKind, name: &str) -> Graph {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(instrument, name_bytes(&name.to_uppercase()));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(3, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
    g
}

/// Node-id base of every rig in a `merge_rigs` result, plus one trailing
/// entry: the merged `next_node`. Rig `i`'s node `n` lands at `bases[i] + n`.
pub fn rig_bases(rigs: &[Rig]) -> Vec<NodeId> {
    let mut bases = Vec::with_capacity(rigs.len() + 1);
    let mut base: NodeId = 0;
    bases.push(base);
    for rig in rigs {
        let max_node = rig.graph.nodes.iter().map(|n| n.id).max().map_or(0, |m| m + 1);
        base = base.saturating_add(rig.graph.next_node.max(max_node));
        bases.push(base);
    }
    bases
}

/// Inverse of [`merge_rigs`]: split a rig-tagged patch into one graph per rig
/// tag (`0..=max tag`; a tag with no nodes yields an empty graph). Each rig is
/// rebased to RIG-RELATIVE ids (its lowest node id becomes 0, its lowest edge
/// id becomes 1) — the same base the cable colors use (rule 4), so a
/// split → edit → merge round trip keeps every rig's colors. Edges follow
/// their source node's rig; a cross-rig edge (never created by the UI) is
/// dropped. Returns `(graphs, bases)` where `bases[i]` is the absolute id of
/// rig `i`'s relative node 0. Rare structural op: allocates.
pub fn split_rigs(merged: &Graph) -> (Vec<Graph>, Vec<NodeId>) {
    let n = merged.nodes.iter().map(|n| n.rig as usize).max().map_or(1, |m| m + 1);
    let mut graphs = Vec::with_capacity(n);
    let mut bases = Vec::with_capacity(n);
    for tag in 0..n {
        let rig_of = |id: NodeId| merged.node(id).map(|x| x.rig as usize);
        let base = merged.nodes.iter().filter(|x| x.rig as usize == tag).map(|x| x.id).min().unwrap_or(0);
        let edges: Vec<&Edge> = merged.edges.iter()
            .filter(|e| rig_of(e.from.node) == Some(tag) && rig_of(e.to.node) == Some(tag))
            .collect();
        let ebase = edges.iter().map(|e| e.id).min().map_or(0, |m| m.saturating_sub(1));
        let mut g = Graph::default();
        for x in merged.nodes.iter().filter(|x| x.rig as usize == tag) {
            let mut x = x.clone();
            x.id -= base;
            x.rig = 0;
            g.nodes.push(x);
        }
        for e in edges {
            let mut e = e.clone();
            e.id -= ebase;
            e.from.node -= base;
            e.to.node -= base;
            g.edges.push(e);
        }
        g.next_node = g.nodes.iter().map(|x| x.id + 1).max().unwrap_or(0);
        g.next_edge = g.edges.iter().map(|e| e.id + 1).max().unwrap_or(1);
        graphs.push(g);
        bases.push(base);
    }
    (graphs, bases)
}

/// What one rig takes from its sources right now (a setlist part, #204).
/// The zone gates what STARTS in the rig and never routing: a release, and
/// any message that returns a control to rest, always passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RigZone {
    /// False: the rig takes no new notes.
    pub live: bool,
    /// Played-note range (inclusive, before any transpose) that starts notes.
    pub key_lo: u8,
    pub key_hi: u8,
    /// Semitones added to what the rig plays, last of every transform.
    pub transpose: i8,
    /// Struck velocity range (inclusive) that starts notes: a velocity layer (#276).
    pub vel_lo: u8,
    pub vel_hi: u8,
    /// What the rig makes of the struck velocity.
    pub curve: VelCurve,
    /// Keys each edge of the key range follows the hands (#276, `crate::layer`).
    pub float: u8,
}

impl RigZone {
    pub const OPEN: Self = Self {
        live: true,
        key_lo: 0,
        key_hi: 127,
        transpose: 0,
        vel_lo: 0,
        vel_hi: 127,
        curve: VelCurve::Linear,
        float: 0,
    };

    /// Does a note-on of `played`, struck at `velocity`, start a note in
    /// this rig, with the hands where `hands` last saw them?
    pub fn starts(&self, played: u8, velocity: u8, hands: &ZoneHands) -> bool {
        self.live && (self.vel_lo..=self.vel_hi).contains(&velocity) && hands.inside(self, played)
    }
}

impl Default for RigZone {
    fn default() -> Self {
        Self::OPEN
    }
}

/// `SetRigSource` wire body: the current `{node, keyboard, hardware}` pair or
/// the pre-unification `{node, keys, laptop, hardware}` triple. Missing
/// flags default to on.
fn de_rig_source<'de, D: serde::Deserializer<'de>>(d: D) -> Result<(NodeId, bool, bool), D::Error> {
    #[derive(Deserialize)]
    struct Wire {
        node: NodeId,
        keyboard: Option<bool>,
        keys: Option<bool>,
        laptop: Option<bool>,
        hardware: Option<bool>,
    }
    let w = Wire::deserialize(d)?;
    let keyboard = w.keyboard.unwrap_or_else(|| match (w.keys, w.laptop) {
        (None, None) => true,
        (k, l) => k.unwrap_or(false) || l.unwrap_or(false),
    });
    Ok((w.node, keyboard, w.hardware.unwrap_or(true)))
}

/// Everything a frontend can ask the engine to do. Fixed-size: no heap.
/// Serde is for off-thread frontends only (bridge socket, sessions); the RT
/// bus still moves plain fixed-size values — no allocation on the audio path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    /// `rig`: which rig the node joins (multi-rig; 0 = the first / only rig).
    /// Older wire bodies without it load as rig 0.
    /// `wire`: the node is an instrument; plug it into its rig like a
    /// standard rig's synth ([`Graph::wire_instrument`]).
    AddNode { kind: NodeKind, name: Name, #[serde(default)] rig: u8, #[serde(default)] wire: bool },
    RemoveNode(NodeId),
    Connect { id: EdgeId, from: PortRef, to: PortRef, kind: PortKind },
    Disconnect(EdgeId),
    SetParam { node: NodeId, param: ParamId, value: f64 },
    /// `channel` is 0-based (0 = MIDI channel 1); the organ dock's Lower
    /// Manual plays channel 1 (#163). Older JSON without it loads as 0.
    NoteOn { note: u8, velocity: f32, source: SourceId, #[serde(default)] channel: u8 },
    NoteOff { note: u8, source: SourceId, #[serde(default)] channel: u8 },
    /// Per-rig input selection (AUM operating model): which source classes
    /// feed this instrument node — the computer keyboard (on-screen clicks
    /// and laptop typing are one source) and hardware MIDI. The MIDI cable
    /// still gates routing; this filters by source on top. Default: both on.
    /// The old `{keys, laptop, hardware}` triple still deserializes, as
    /// `keyboard = keys || laptop`.
    #[serde(deserialize_with = "de_rig_source")]
    SetRigSource { node: NodeId, keyboard: bool, hardware: bool },
    /// Per-rig hardware device selection: None = any device, Some(id) = only
    /// that device (AUM: a different keyboard per rig).
    SetRigDevice { node: NodeId, device: Option<u32> },
    /// Rename rig `rig` (index in the rig list). Metadata only — routing and
    /// sound are untouched; the new name is published with the rig snapshot.
    RenameRig { rig: u8, name: Name },
    /// Per-rig mixer (AUM overview strip): volume (0..1), pan (-1..1),
    /// mute/solo. Applied at the rig's MasterOut when summing to the physical
    /// output. Omitted fields keep their current value. Solo is exclusive:
    /// `solo: Some(true)` clears every other rig's solo, and the soloed rig
    /// mutes all others. Resets on graph loads.
    SetRigMix {
        rig: u8,
        vol: Option<f32>,
        pan: Option<f32>,
        mute: Option<bool>,
        solo: Option<bool>,
    },
    /// Every rig's zone at once (index = rig), so a song switch flips all
    /// rigs in the same block and changes no routing. A `live` zone replaces
    /// the rig's; a zone that is not live only clears `live` and keeps the
    /// range and transpose, so a held note is released at the pitch it
    /// started. A live rig whose transpose changes is released first
    /// (rule 41). Resets to [`RigZone::OPEN`] on graph loads.
    SetRigZones { zones: [RigZone; 8] },
    /// Show levels (#273): every aux bus takes its sends again and every
    /// binding goes back to its default. Sent before the gates of a part.
    ClearShowGates,
    /// An aux bus that is not live takes no new signal (its input fades out
    /// over one block); what its chain holds rings out.
    SetBusLive { bus: NodeId, live: bool },
    /// The binding of `cc` from `source` on `dest`: `live` = it works also
    /// while its rig is not live; not live = it moves nothing (except a
    /// control back to rest). Without an entry a mixer binding always works
    /// and any other works while its rig is live. Cleared on graph loads.
    SetBindingLive { source: SourceId, dest: NodeId, cc: u8, live: bool },
    /// The show controls (#261): hardware messages the engine takes for a
    /// show action before the matrix, so no instrument sees them. Replaces
    /// the whole table; kept across graph loads.
    SetShowControls { controls: crate::show::ShowControls },
    /// Every setlist part's zones in setlist order, the table a
    /// `ShowAction::NextPart`/`PrevPart` steps through. Kept across graph
    /// loads. Sent as JSON, it is a list of 8-zone lists.
    SetShowParts { parts: crate::show::ShowParts },
    /// The active part as an index into `SetShowParts` (`None`: no part).
    SetShowPart { part: Option<usize> },
    /// Per-SOURCE strip mix (#73): pan and mute/solo on one instrument /
    /// audio-in / generator node, applied to the node's own output (post its
    /// LEVEL knob — the strip fader) before anything downstream hears it, so
    /// its FX chain and the rig bus follow. Solo scope is WITHIN the node's
    /// rig: a soloed source silences only the other sources of that rig; rig
    /// buses keep their own global solo (`SetRigMix`). Omitted fields keep
    /// their value; resets on graph loads.
    SetStripMix {
        node: NodeId,
        pan: Option<f32>,
        mute: Option<bool>,
        solo: Option<bool>,
    },
    /// One send into an aux bus (issue #5): `from` (a rig or a node) feeds
    /// the `DspKind::Bus` node `bus` at `level` (linear gain 0..=1), tapped
    /// before (`pre`) or after the source's fader. Omitted fields keep their
    /// value. Ignored when `bus` is no bus or `from` is itself a bus or part
    /// of a bus's insert chain (no feedback). Resets on graph loads.
    SetSend {
        from: SendSource,
        bus: NodeId,
        level: Option<f32>,
        pre: Option<bool>,
    },
    /// Play one rig bus or aux bus (`SendSource::Node` of the bus node) to
    /// its own output (#147): a PipeWire sink by node name, or `None` for the
    /// master's. The engine serves `engine::MAX_OUTS` such outputs at once;
    /// one more distinct sink is refused. Resets on graph loads.
    SetBusOutput { bus: SendSource, sink: Option<SinkName> },
    /// Bypass one effect node (#74): a bypassed FX passes its input through
    /// unchanged (its knob values are kept). Resets on graph loads.
    SetStripBypass { node: NodeId, bypass: bool },
    /// Rename one node (#77): the name its rack unit and mixer strip show.
    /// Metadata only — routing and sound are untouched; the engine publishes
    /// the graph (node names live in it).
    RenameNode { node: NodeId, name: Name },
    /// Replace the whole graph (template load). The engine rebuilds DSP, remaps
    /// the existing CLAP voices onto the new Clap nodes, and rebuilds the MIDI
    /// matrix. Rare structural op: allocates (bends RT discipline by design).
    LoadGraph { graph: Graph },
    /// Load several independent rigs at once: their graphs are merged
    /// ([`merge_rigs`]), DSP/LV2 rebuilt, the existing CLAP voices remapped to
    /// Clap nodes in node-id order, and each rig's instruments driven by that
    /// rig's `midi_device` (None = the keyboard + all hardware devices).
    /// `active` is the UI-selected rig index (engine keeps it for queries).
    /// Rare structural op: allocates (bends RT discipline by design).
    LoadRigs { rigs: Vec<Rig>, active: usize },
    /// Split ONE Keys node at `point` (absolute MIDI note): notes below leave
    /// on its port 0 (LOW), notes at/above on port 1 (HIGH). `None` unsplits
    /// it (one MIDI OUT, full range) and drops its port-1 cables. Routing is
    /// plain graph wiring — wire LOW/HIGH to rigs like any ports. The engine
    /// releases held notes on every instrument the node feeds.
    SetNodeSplit { node: NodeId, point: Option<u8> },
    /// Key + scale a Keys node's notes snap to (issue #90): out-of-scale keys
    /// snap to the nearest in-scale note on their way out. `None` = no
    /// snapping. The engine releases held notes on every instrument the node
    /// feeds first (rule 41: a scale change mid-hold would snap the release
    /// to a different pitch than the note-on did).
    SetKeysScale { node: NodeId, scale: Option<crate::arp::ScaleChoice> },
    /// Octave shift of one zone of a split Keys node (`zone` 0 = LOW, 1 =
    /// HIGH; `octave` clamped to ±`ZONE_OCTAVE_MAX`). The engine releases
    /// held notes on every instrument the node feeds first (rule 41).
    SetZoneOctave { node: NodeId, zone: u8, octave: i8 },
    /// Live edit of ONE matrix rule (source → dest): channel filter, transpose,
    /// key/velocity ranges, enabled. Fixed-size; CC maps use `MatrixCcMap`.
    /// The engine applies it in place and releases held notes on the destination
    /// — the matrix is stateless, so range/transpose edits can orphan releases
    /// (docs/midi-matrix.md integration contract).
    MatrixRule {
        source: SourceId,
        dest: NodeId,
        /// None = omni; Some(ch) = that 0-based channel only.
        channel: Option<u8>,
        transpose: i8,
        key_lo: u8,
        key_hi: u8,
        vel_lo: u8,
        vel_hi: u8,
        enabled: bool,
    },
    /// Incoming MIDI Control Change routed through the matrix.
    Cc {
        source: SourceId,
        channel: u8,
        cc: u8,
        value: u8,
    },
    /// Incoming MIDI pitch bend (0xE0) routed through the matrix like
    /// [`Command::Cc`]: `value` is the signed 14-bit bend (-8192..=8191,
    /// 0 = center). Instruments bend what they play; nothing is edited.
    PitchBend {
        source: SourceId,
        channel: u8,
        value: i16,
    },
    /// Incoming aftertouch routed through the matrix like
    /// [`Command::PitchBend`]: channel pressure (0xD0) when `note` is
    /// `None`, poly pressure (0xA0) on that played key otherwise. Wind
    /// controllers send breath this way.
    Pressure {
        source: SourceId,
        channel: u8,
        note: Option<u8>,
        value: u8,
    },
    /// Add/replace (`Some(param)`) or remove (`None`) one CC→param binding on
    /// the matrix rule `(source, dest)`. Fixed-size for the RT command bus.
    MatrixCcMap {
        source: SourceId,
        dest: NodeId,
        cc: u8,
        param: Option<ParamId>,
        #[serde(default)]
        target: crate::matrix::CcTarget,
        #[serde(default)]
        scale: crate::matrix::CcScale,
    },
    /// Mix system-audio capture (e.g. YouTube) into the master at `gain` (0..1).
    /// 0 mutes it. The capture source is started at app startup.
    SetSystemAudio { gain: f32 },
    /// Configure an `AudioIn` node's device + channel selection (structural:
    /// rebuilds the input slot and publishes the graph). `device` = cpal input
    /// device name (None = default input); `channels` = mono L / R / stereo.
    /// Omitted (None) fields keep their current value. Gain and monitor are
    /// engine params (`P_AUDIO_IN_GAIN` / `P_AUDIO_IN_MONITOR` via SetParam).
    SetAudioIn {
        node: NodeId,
        device: Option<String>,
        channels: Option<AudioInChannel>,
    },
    /// Mix the file player (backing track) into the master at `gain` (0..1).
    /// Loading/stopping a file is done off-bus (variable-length path).
    SetPlayer { gain: f32 },
    /// Configure the file player's playback settings (AUM FilePlayer, issue
    /// #84): `loop_on` restarts the file seamlessly, `sync` follows the
    /// transport tempo (varispeed), `beats` is the loop's length in beats
    /// (0 = derive the tempo from `bpm` instead), `offset` shifts the start
    /// point in beats (±), `bpm` is the file's original tempo. Omitted
    /// fields keep their current value; settings apply to the loaded file.
    SetPlayerCfg {
        loop_on: Option<bool>,
        sync: Option<bool>,
        beats: Option<f32>,
        offset: Option<f32>,
        bpm: Option<f32>,
    },
    /// File-player play-enable (AUM's circular toggle): with sync on and the
    /// master transport rolling (or counting in), sounding starts at the
    /// next bar boundary; otherwise it starts now. `on: false` stops.
    SetPlayerEnable { on: bool },
    /// Master transport: start EVERY transport-capable node (sets
    /// `Node::playing` on all of them) and run the song clock hosted CLAP
    /// plugins follow, from beat 0. Per-node SetNodePlaying overrides
    /// individually afterwards.
    TransportPlay,
    /// Master transport: stop every transport-capable node and halt the
    /// song clock (its position stays).
    TransportStop,
    /// Per-instrument transport: start/stop ONE transport-capable node
    /// (metronome today; file player later). Ignored for other kinds; never
    /// moves the song clock.
    SetNodePlaying { node: NodeId, playing: bool },
    /// Set the tempo (BPM) of the metronomes and of the song clock.
    SetBpm { bpm: f32 },
    /// Configure master transport count-in: `bars` (1..=4) of metronome
    /// clicks before the song clock starts at beat 0 when `enabled` is true.
    SetCountIn { enabled: bool, bars: u8 },
    /// Set the time signature (#87): `num` beats of note value `den` per
    /// bar. Bar-quantized actions, the metronome's accent, the count-in's
    /// length and the clock hosted plugins get follow it.
    SetTimeSignature { num: u8, den: u8 },
    /// Apply an installed LV2 preset (`pset:Preset` URI) to an LV2 node.
    LoadLv2Preset { node: NodeId, preset_uri: String },
    /// Restore an LV2 node's plugin state (a Vital patch for Vitalium), in
    /// the format `lv2::state_format` names for its plugin. The loader thread
    /// restores it into a fresh instance that then replaces the node's.
    LoadLv2State { node: NodeId, state: Vec<u8> },
    /// Send a MIDI programme change (0..=127) to ONE instrument node: how an
    /// instrument with built-in programmes (setBfree) changes its sound.
    ProgramChange { node: NodeId, program: u8 },
    /// Start recording the final master output to a new timestamped WAV under
    /// ~/Music/kabelsalat/. Idempotent — a second start while recording is a
    /// no-op. The writer thread generates the path, so no path crosses the
    /// fixed-size bus.
    RecordStart,
    /// Stop recording: the writer drains the ring and finalizes the WAV header.
    /// Idempotent — a stop while idle is a no-op.
    RecordStop,
    /// "Sync to transport": while on, master TransportPlay starts a recording
    /// and TransportStop stops it (recording follows the master transport).
    SetRecordSync { on: bool },
    /// Arm or disarm a recording target (`rig: None` = master mix, `Some(r)` =
    /// rig `r`'s post-fader stem). Armed targets are captured together in the
    /// next take.
    SetRecordArm { rig: Option<u8>, armed: bool },
    /// Reset the master-bus loudness meter (Bravo 1 "measure loop"): clears
    /// the integrated-loudness gate, windows, true-peak hold and band/LFE
    /// accumulators so a fresh integrated measurement starts now.
    ResetMeter,
    /// Aim the master at an integrated loudness (#12): the engine moves the
    /// trim in front of the limiter by what its meter says is missing and
    /// starts a fresh measurement for the "after" reading. `None` = off.
    SetMasterTarget { lufs: Option<f32> },
    /// Put a saved loudness target back as it was (session load, undo): no
    /// measuring, and a no-op when the engine already holds it.
    RestoreMasterTarget { target: crate::meter::MasterTarget },
    /// Mute or unmute everything coming from kabelsalat to Master out.
    SetMasterMute { muted: bool },
    /// Play a demo phrase (#200) through rig `rig` as if it were played on
    /// that rig's Keys, looping on the engine's clock until `DemoStop` or a
    /// routing edit ends it (`Command::demo_effect`). One demo at a time: a
    /// second `DemoPlay` releases the first one's notes, then starts.
    DemoPlay { rig: u8, phrase: crate::demo::PhraseKind },
    DemoStop,
}

/// What a command does to a running demo phrase. A demo note's note-off
/// takes the routes its note-on took (rule 41), so every command that can
/// move one ends the demo first, releasing its notes while those routes
/// still exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemoEffect {
    Keeps,
    Ends,
    /// Ends it when this cable is a MIDI cable that exists: an audio cable,
    /// or the UI's re-send of a cable already gone, moves no note.
    EndsIfMidiCable(EdgeId),
}

/// Lock-free UI/frontend → engine command bus (rtrb SPSC ring).
pub struct CommandBus {
    tx: rtrb::Producer<Command>,
}

/// Engine side: drained at block boundaries, never mid-block.
pub struct CommandDrain {
    rx: rtrb::Consumer<Command>,
}

pub fn command_bus() -> (CommandBus, CommandDrain) {
    let (tx, rx) = rtrb::RingBuffer::new(1024);
    (CommandBus { tx }, CommandDrain { rx })
}

impl Command {
    /// A change to the rack the user would want to undo (graph, routing,
    /// knobs, mix, presets) — not playing, transport or recording.
    pub fn is_rack_edit(&self) -> bool {
        matches!(self,
            Command::AddNode { .. } | Command::RemoveNode { .. } | Command::Connect { .. }
            | Command::Disconnect { .. } | Command::SetParam { .. } | Command::SetRigSource { .. }
            | Command::SetRigDevice { .. } | Command::RenameRig { .. } | Command::SetRigMix { .. } | Command::LoadGraph { .. }
            | Command::SetStripMix { .. } | Command::SetStripBypass { .. } | Command::SetBusOutput { .. } | Command::RenameNode { .. }
            | Command::SetAudioIn { .. } | Command::LoadRigs { .. } | Command::SetNodeSplit { .. } | Command::SetZoneOctave { .. } | Command::MatrixRule { .. }
            | Command::MatrixCcMap { .. } | Command::LoadLv2Preset { .. } | Command::LoadLv2State { .. } | Command::ProgramChange { .. }
            | Command::SetKeysScale { .. } | Command::SetMasterMute { .. })
    }
}

impl Command {
    /// Rule 41 for the demo phrase, decided per command (no wildcard: a new
    /// command must be classified). Connect only ever adds a route — a
    /// demo note's off then also reaches a dest that never sounded, like a
    /// key's (`held_notes`) — and so does an instrument added to a rig. The
    /// source switches and the matrix gate the keys, not the demo, but they
    /// end it as any routing edit does.
    pub fn demo_effect(&self) -> DemoEffect {
        use DemoEffect::{Ends, Keeps};
        match self {
            Command::AddNode { kind, .. } => match kind {
                NodeKind::Arp | NodeKind::MidiFx { .. } => Ends,
                NodeKind::Keys
                | NodeKind::Clap { .. }
                | NodeKind::Lv2 { .. }
                | NodeKind::Dsp(_)
                | NodeKind::MasterOut
                | NodeKind::SystemAudio
                | NodeKind::AudioIn { .. } => Keeps,
            },
            Command::Disconnect(id) => DemoEffect::EndsIfMidiCable(*id),
            Command::RemoveNode(_)
            | Command::LoadGraph { .. }
            | Command::LoadRigs { .. }
            | Command::SetRigSource { .. }
            | Command::SetRigDevice { .. }
            | Command::SetNodeSplit { .. }
            | Command::SetKeysScale { .. }
            | Command::SetZoneOctave { .. }
            | Command::MatrixRule { .. } => Ends,
            Command::Connect { .. }
            | Command::SetParam { .. }
            | Command::NoteOn { .. }
            | Command::NoteOff { .. }
            | Command::RenameRig { .. }
            | Command::SetRigMix { .. }
            // Zones gate live input; the demo plays past them.
            | Command::SetRigZones { .. }
            | Command::ClearShowGates
            | Command::SetBusLive { .. }
            | Command::SetBindingLive { .. }
            | Command::SetShowControls { .. }
            | Command::SetShowParts { .. }
            | Command::SetShowPart { .. }
            | Command::SetStripMix { .. }
            | Command::SetSend { .. }
            | Command::SetBusOutput { .. }
            | Command::SetStripBypass { .. }
            | Command::RenameNode { .. }
            | Command::Cc { .. }
            | Command::PitchBend { .. }
            | Command::Pressure { .. }
            | Command::MatrixCcMap { .. }
            | Command::SetSystemAudio { .. }
            | Command::SetAudioIn { .. }
            | Command::SetPlayer { .. }
            | Command::SetPlayerCfg { .. }
            | Command::SetPlayerEnable { .. }
            | Command::TransportPlay
            | Command::TransportStop
            | Command::SetNodePlaying { .. }
            | Command::SetBpm { .. }
            | Command::SetCountIn { .. }
            | Command::SetTimeSignature { .. }
            | Command::LoadLv2Preset { .. }
            | Command::LoadLv2State { .. }
            | Command::ProgramChange { .. }
            | Command::RecordStart
            | Command::RecordStop
            | Command::SetRecordSync { .. }
            | Command::SetRecordArm { .. }
            | Command::ResetMeter
            | Command::SetMasterTarget { .. }
            | Command::RestoreMasterTarget { .. }
            | Command::SetMasterMute { .. }
            | Command::DemoPlay { .. }
            | Command::DemoStop => Keeps,
        }
    }
}

impl CommandBus {
    /// Non-blocking; returns false if the ring is full (command dropped).
    pub fn send(&mut self, cmd: Command) -> bool {
        self.tx.push(cmd).is_ok()
    }
}

impl CommandDrain {
    pub fn pop(&mut self) -> Option<Command> {
        self.rx.pop().ok()
    }

    pub fn drain(&mut self, mut f: impl FnMut(Command)) {
        while let Ok(cmd) = self.rx.pop() {
            f(cmd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clap_id_longer_than_a_name_reaches_its_voice_whole() {
        let cardinal = "studio.kx.distrho.cardinal#synth"; // 32 bytes
        let g = plugin_rig_graph(cardinal, "Cardinal");
        let back: Graph = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        let voices = [(7, cardinal)];
        assert!(bind_voices(&back, &voices).iter().any(|&(_, v)| v == Some(7)), "the saved graph binds Cardinal's voice");
    }

    #[test]
    fn a_session_saved_with_the_byte_array_id_still_loads() {
        // The shape of `~/kabelsalat-sessions/Default.ksalat/session.json`
        // before ids were saved as strings.
        let mut saved = serde_json::json!({ "Clap": { "plugin_id": name_bytes("com.digital-suburban.dexed") } });
        let kind: NodeKind = serde_json::from_value(saved.take()).unwrap();
        assert_eq!(kind, NodeKind::Clap { plugin_id: ClapId::new("com.digital-suburban.dexed") });
        let now = serde_json::to_value(&kind).unwrap();
        assert_eq!(now, serde_json::json!({ "Clap": { "plugin_id": "com.digital-suburban.dexed" } }));
    }

    #[test]
    fn an_instrument_is_wired_into_its_own_rig_with_fresh_ids() {
        let rigs = [Rig::new("A", standard_rig_graph(false), None), Rig::new("B", standard_rig_graph(true), None)];
        let mut g = merge_rigs(&rigs);
        let organ = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new("urn:organ") }, name_bytes("ORGAN"));
        g.set_rig(organ, 0);
        let before: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
        assert_eq!(g.wire_instrument(organ), 3);
        let rig_a = |kind: fn(&NodeKind) -> bool| g.nodes.iter().find(|n| n.rig == 0 && kind(&n.kind)).unwrap().id;
        let (keys, master) = (rig_a(|k| matches!(k, NodeKind::Keys)), rig_a(|k| matches!(k, NodeKind::MasterOut)));
        assert!(g.has_route(keys, organ, PortKind::Midi));
        assert!(g.has_route_port(organ, master, 0, PortKind::Audio) && g.has_route_port(organ, master, 1, PortKind::Audio));
        let new: Vec<EdgeId> = g.edges.iter().map(|e| e.id).filter(|id| !before.contains(id)).collect();
        assert!(new.iter().all(|id| before.iter().all(|b| id > b)), "fresh ids {new:?} above {before:?}");
        assert_eq!(g.wire_instrument(organ), 0, "wiring twice adds nothing");
    }

    #[test]
    fn voices_bind_in_graph_order_lowest_free_id_of_the_nodes_plugin() {
        let mut g = Graph::default();
        let clap = |id: &str| NodeKind::Clap { plugin_id: ClapId::new(id) };
        let a = g.add_node(clap(SURGE_PLUGIN_ID), name_bytes("A"));
        g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let dexed = g.add_node(clap("dexed"), name_bytes("D"));
        let legacy = g.add_node(clap("Surge XT"), name_bytes("B"));
        let unnamed = g.add_node(clap(""), name_bytes("C"));
        // Listed out of order: the rule goes by voice id, not by position.
        let voices = [(7, SURGE_PLUGIN_ID), (9, "dexed"), (3, SURGE_PLUGIN_ID), (4, "yanes")];
        assert_eq!(
            bind_voices(&g, &voices),
            [(a, Some(3)), (dexed, Some(9)), (legacy, Some(7)), (unnamed, None)],
            "legacy and empty ids mean Surge; the third Surge node finds no voice, yanes fits no node"
        );
        assert!(bind_voices(&g, &[]).iter().all(|(_, v)| v.is_none()));
    }

    #[test]
    fn demo_patch_is_wired() {
        let g = Graph::demo_patch();
        assert_eq!(g.nodes.len(), 6);
        assert_eq!(g.edges.len(), 8);
        assert!(g.has_route(0, 1, PortKind::Midi));
        assert!(g.has_route(0, 4, PortKind::Midi));
        assert!(g.has_route(1, 3, PortKind::Audio));
        assert!(g.has_route(4, 3, PortKind::Audio));
    }

    #[test]
    fn connect_rejects_self_and_duplicates() {
        let mut g = Graph::demo_patch();
        let p = PortRef { node: 0, port: 0 };
        assert!(!g.connect_with_id(99, p, p, PortKind::Midi));
        let before = g.edges.len();
        g.connect_with_id(99, PortRef { node: 0, port: 0 }, PortRef { node: 1, port: 0 }, PortKind::Midi);
        assert_eq!(g.edges.len(), before);
    }

    #[test]
    fn remove_node_cleans_edges() {
        let mut g = Graph::demo_patch();
        g.remove_node(1); // Surge A
        assert_eq!(g.nodes.len(), 5);
        assert!(g.edges.iter().all(|e| e.from.node != 1 && e.to.node != 1));
    }

    #[test]
    fn bus_roundtrip() {
        let (mut bus, mut drain) = command_bus();
        assert!(bus.send(Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys, channel: 0 }));
        let mut got = Vec::new();
        drain.drain(|c| got.push(c));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn names_roundtrip() {
        let n = name_bytes("SURGE XT");
        assert_eq!(name_str(&n), "SURGE XT");
    }

    #[test]
    fn command_json_roundtrip() {
        // The bridge wire format: externally tagged JSON, one line per command.
        let cmds = vec![
            Command::AddNode { kind: NodeKind::Dsp(DspKind::Delay), name: name_bytes("DELAY X"), rig: 0, wire: false },
            Command::RemoveNode(3),
            Command::Connect { id: 12, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 1, port: 0 }, kind: PortKind::Midi },
            Command::Disconnect(12),
            Command::SetParam { node: 3, param: 3, value: 0.75 },
            Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys, channel: 0 },
            Command::NoteOff { note: 60, source: SourceId::Hardware(20), channel: 0 },
            Command::SetRigSource { node: 1, keyboard: true, hardware: true },
            Command::SetRigSource { node: 4, keyboard: false, hardware: true },
            Command::SetNodeSplit { node: 0, point: Some(60) },
            Command::SetNodeSplit { node: 0, point: None },
            Command::SetSystemAudio { gain: 0.5 },
            Command::TransportPlay,
            Command::TransportStop,
            Command::SetNodePlaying { node: 7, playing: true },
            Command::SetNodePlaying { node: 7, playing: false },
            Command::SetBpm { bpm: 128.0 },
            Command::SetCountIn { enabled: true, bars: 2 },
            Command::SetCountIn { enabled: false, bars: 1 },
            Command::SetTimeSignature { num: 6, den: 8 },
            Command::SetRigZones { zones: [RigZone { live: false, key_lo: 36, key_hi: 59, transpose: -2, vel_lo: 40, vel_hi: 110, curve: VelCurve::Soft, float: 3 }, RigZone::OPEN, RigZone::OPEN, RigZone::OPEN, RigZone::OPEN, RigZone::OPEN, RigZone::OPEN, RigZone::OPEN] },
            Command::Cc { source: SourceId::Hardware(1), channel: 2, cc: 74, value: 99 },
            Command::SetShowControls { controls: {
                let mut t = crate::show::NO_SHOW_CONTROLS;
                t[0] = Some(crate::show::ShowControl { device: Some(1), channel: None, msg: crate::show::ShowMsg::Cc(64), action: crate::show::ShowAction::PrevPart });
                t
            } },
            Command::SetShowParts { parts: crate::show::ShowParts::new(vec![
                crate::show::ShowPart { song: 0, skip: false, zones: [RigZone::OPEN; 8] },
                crate::show::ShowPart { song: 2, skip: true, zones: [RigZone { live: false, key_lo: 0, key_hi: 59, transpose: 12 }; 8] },
            ]) },
            Command::SetShowPart { part: Some(1) },
            Command::MatrixCcMap { source: SourceId::Hardware(1), dest: 1, cc: 74, param: Some(42), target: crate::matrix::CcTarget::PluginParam, scale: Default::default() },
            Command::MatrixCcMap { source: SourceId::Hardware(1), dest: 1, cc: 7, param: Some(0), target: crate::matrix::CcTarget::Knob, scale: Default::default() },
            Command::MatrixCcMap { source: SourceId::Hardware(1), dest: 1, cc: 74, param: None, target: crate::matrix::CcTarget::PluginParam, scale: Default::default() },
            Command::RecordStart,
            Command::RecordStop,
            Command::SetRecordSync { on: true },
            Command::SetRecordArm { rig: None, armed: true },
            Command::SetRecordArm { rig: Some(1), armed: true },
            Command::ResetMeter,
            Command::SetMasterTarget { lufs: Some(-14.0) },
            Command::SetMasterTarget { lufs: None },
            Command::RestoreMasterTarget { target: crate::meter::MasterTarget { lufs: Some(-16.0), gain_db: 4.5, before_lufs: Some(-20.5), guard_db: -0.4, last_point: None, limited: false } },
            Command::RestoreMasterTarget { target: crate::meter::MasterTarget::OFF },
            Command::SetMasterMute { muted: true },
            Command::SetMasterMute { muted: false },
            Command::SetAudioIn { node: 9, device: None, channels: Some(AudioInChannel::MonoR) },
            Command::SetAudioIn { node: 9, device: Some("USB Audio".into()), channels: None },
            Command::DemoPlay { rig: 1, phrase: crate::demo::PhraseKind::Bass },
            Command::DemoStop,
        ];
        for cmd in cmds {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&s).unwrap();
            assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
        }
        // A zone sent before velocity layers existed (#276) plays as it did.
        let z: RigZone = serde_json::from_str(r#"{"live":true,"key_lo":36,"key_hi":59,"transpose":-2}"#).unwrap();
        assert_eq!(z, RigZone { key_lo: 36, key_hi: 59, transpose: -2, ..RigZone::OPEN });
        // The exact shape the CLI/bridge accepts.
        let c: Command = serde_json::from_str(r#"{"SetSystemAudio":{"gain":0.5}}"#).unwrap();
        assert!(matches!(c, Command::SetSystemAudio { gain } if (gain - 0.5).abs() < 1e-6));
        let c: Command = serde_json::from_str(r#"{"SetNodePlaying":{"node":3,"playing":true}}"#).unwrap();
        assert!(matches!(c, Command::SetNodePlaying { node: 3, playing: true }));
        let c: Command = serde_json::from_str(r#"{"SetNodeSplit":{"node":0,"point":60}}"#).unwrap();
        assert!(matches!(c, Command::SetNodeSplit { node: 0, point: Some(60) }));
        let c: Command = serde_json::from_str(r#"{"SetNodeSplit":{"node":0,"point":null}}"#).unwrap();
        assert!(matches!(c, Command::SetNodeSplit { node: 0, point: None }));
        let c: Command = serde_json::from_str(r#"{"MatrixCcMap":{"source":{"Hardware":1},"dest":1,"cc":74,"param":42}}"#).unwrap();
        assert!(matches!(c, Command::MatrixCcMap { dest: 1, cc: 74, param: Some(42), target: crate::matrix::CcTarget::PluginParam, .. }));
    }

    #[test]
    fn demo_effect_classifies_routing_vs_non_routing_commands() {
        use crate::demo::PhraseKind;
        assert_eq!(Command::DemoPlay { rig: 0, phrase: PhraseKind::Keys }.demo_effect(), DemoEffect::Keeps);
        assert_eq!(Command::DemoStop.demo_effect(), DemoEffect::Keeps);
        assert_eq!(
            Command::Connect { id: 1, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 1, port: 0 }, kind: PortKind::Midi }.demo_effect(),
            DemoEffect::Keeps,
            "the UI's per-frame Connect mirror must never stop a running demo"
        );
        assert_eq!(Command::SetParam { node: 1, param: 0, value: 0.5 }.demo_effect(), DemoEffect::Keeps);
        assert_eq!(Command::SetBpm { bpm: 120.0 }.demo_effect(), DemoEffect::Keeps);
        assert_eq!(Command::AddNode { kind: NodeKind::Dsp(DspKind::Delay), name: name_bytes("DELAY"), rig: 0, wire: false }.demo_effect(), DemoEffect::Keeps);
        assert_eq!(Command::AddNode { kind: NodeKind::Arp, name: name_bytes("ARP"), rig: 0, wire: true }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::Disconnect(4).demo_effect(), DemoEffect::EndsIfMidiCable(4));
        assert_eq!(Command::RemoveNode(1).demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::LoadGraph { graph: Graph::default() }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::SetNodeSplit { node: 0, point: Some(60) }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::SetKeysScale { node: 0, scale: None }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::SetZoneOctave { node: 0, zone: 0, octave: 1 }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::SetRigSource { node: 1, keyboard: false, hardware: true }.demo_effect(), DemoEffect::Ends);
        assert_eq!(Command::SetRigDevice { node: 1, device: None }.demo_effect(), DemoEffect::Ends);
    }

    #[test]
    fn set_rig_source_pair_and_legacy_triple() {
        // Current wire shape: the (keyboard, hardware) pair.
        let c = Command::SetRigSource { node: 1, keyboard: false, hardware: true };
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"SetRigSource":{"node":1,"keyboard":false,"hardware":true}}"#);
        // Old (keys, laptop, hardware) triples migrate: keyboard = keys || laptop.
        for (keys, laptop, want) in [(true, true, true), (true, false, true), (false, true, true), (false, false, false)] {
            let json = format!(r#"{{"SetRigSource":{{"node":4,"keys":{keys},"laptop":{laptop},"hardware":false}}}}"#);
            let c: Command = serde_json::from_str(&json).unwrap();
            assert!(matches!(c, Command::SetRigSource { node: 4, keyboard, hardware: false } if keyboard == want), "{json}");
        }
        // A lone legacy flag counts; omitted flags default on.
        let c: Command = serde_json::from_str(r#"{"SetRigSource":{"node":1,"laptop":true}}"#).unwrap();
        assert!(matches!(c, Command::SetRigSource { node: 1, keyboard: true, hardware: true }));
        let c: Command = serde_json::from_str(r#"{"SetRigSource":{"node":1}}"#).unwrap();
        assert!(matches!(c, Command::SetRigSource { node: 1, keyboard: true, hardware: true }));
        // A legacy laptop note source arrives as the unified keyboard.
        let c: Command = serde_json::from_str(r#"{"NoteOn":{"note":60,"velocity":0.8,"source":"LaptopKeys"}}"#).unwrap();
        assert!(matches!(c, Command::NoteOn { source: SourceId::Keys, .. }));
    }

    #[test]
    fn keys_split_defaults_none_and_round_trips() {
        // Older sessions have no `split` field: Keys loads unsplit.
        let json = r#"{"id":0,"kind":"Keys","name":[75,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
        let n: Node = serde_json::from_str(json).unwrap();
        assert_eq!(n.split, None);
        let mut g = Graph::demo_patch();
        assert_eq!(g.set_split(0, Some(60)), Some(true));
        assert_eq!(g.set_split(0, Some(60)), Some(false), "no-op reports unchanged");
        assert_eq!(g.set_split(1, Some(60)), None, "only Keys nodes split");
        assert_eq!(g.node(1).unwrap().split, None);
        assert_eq!(g.set_split(999, Some(60)), None, "unknown node");
        let back: Graph = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back.node(0).unwrap().split, Some(60), "split survives serde (sessions)");
    }

    #[test]
    fn unsplitting_drops_high_port_cables_only() {
        let mut g = Graph::demo_patch();
        g.set_split(0, Some(60));
        // Rewire Surge B (cable 6) to HIGH: port 1.
        g.disconnect(6);
        assert!(g.connect_with_id(6, PortRef { node: 0, port: 1 }, PortRef { node: 4, port: 0 }, PortKind::Midi));
        assert_eq!(g.set_split(0, None), Some(true));
        assert!(g.edges.iter().all(|e| !(e.from.node == 0 && e.from.port == 1)), "HIGH cable removed");
        assert!(g.has_route(0, 1, PortKind::Midi), "LOW cable kept");
        assert_eq!(split_port(None, 100), 0);
        assert_eq!(split_port(Some(60), 59), 0);
        assert_eq!(split_port(Some(60), 60), 1);
    }

    /// A minimal rig patch: Keys(0) → Clap(1) → Master(2), cables 1 and 2.
    fn mini_rig_graph() -> Graph {
        let mut g = Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(
            NodeKind::Clap { plugin_id: ClapId::new("org.surge-synth-team.surge-xt") },
            name_bytes("SYNTH"),
        );
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        assert!(g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi));
        assert!(g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio));
        g
    }

    #[test]
    fn node_rig_defaults_and_set_rig() {
        let mut g = Graph::demo_patch();
        assert!(g.nodes.iter().all(|n| n.rig == 0));
        assert!(g.set_rig(4, 1));
        assert_eq!(g.node(4).unwrap().rig, 1);
        assert!(!g.set_rig(999, 2)); // unknown node
    }

    #[test]
    fn node_without_rig_field_loads_as_rig0() {
        // Backward compat: sessions saved before multi-rig have no `rig`.
        let json = r#"{"id":7,"kind":"MasterOut","name":[77,65,83,84,69,82,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
        let n: Node = serde_json::from_str(json).unwrap();
        assert_eq!(n.id, 7);
        assert_eq!(n.rig, 0);
    }

    #[test]
    fn node_playing_defaults_false_and_round_trips() {
        // Older sessions have no `playing` field: they load stopped.
        let json = r#"{"id":2,"kind":{"Dsp":"Metronome"},"name":[77,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#;
        let n: Node = serde_json::from_str(json).unwrap();
        assert!(!n.playing);
        let mut g = Graph::default();
        let m = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRO"));
        assert!(!g.node(m).unwrap().playing, "new nodes start stopped");
        assert_eq!(g.set_playing(m, true), Some(true));
        assert_eq!(g.set_playing(m, true), Some(false), "no-op reports unchanged");
        let back: Graph = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert!(back.node(m).unwrap().playing, "playing survives serde (sessions)");
    }

    #[test]
    fn playing_only_on_transport_capable_kinds() {
        let mut g = Graph::demo_patch();
        let m = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRO"));
        assert!(NodeKind::Dsp(DspKind::Metronome).supports_transport());
        assert!(!NodeKind::Dsp(DspKind::Delay).supports_transport());
        assert_eq!(g.set_playing(2, true), None, "a delay has no transport");
        assert!(!g.node(2).unwrap().playing);
        assert_eq!(g.set_playing(999, true), None, "unknown node");
        // Master fan-out touches only capable nodes.
        assert!(g.set_all_playing(true));
        assert!(g.node(m).unwrap().playing);
        assert!(g.nodes.iter().filter(|n| n.id != m).all(|n| !n.playing));
        assert!(!g.set_all_playing(true), "already playing: unchanged");
        assert!(g.set_all_playing(false));
        assert!(!g.node(m).unwrap().playing);
    }

    #[test]
    fn merge_rigs_offsets_tags_and_keeps_masters() {
        // Both rigs use the SAME local ids (0,1,2 / cables 1,2) — the merge
        // must offset them, not collide.
        let rigs = vec![
            Rig::new("A", mini_rig_graph(), Some(2)),
            Rig::new("B", mini_rig_graph(), None),
        ];
        let g = merge_rigs(&rigs);
        assert_eq!(g.nodes.len(), 6);
        assert_eq!(g.edges.len(), 4);
        // Rig A keeps ids 0..2; rig B is offset to 3..5.
        let ids: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(g.node(1).unwrap().rig, 0);
        assert_eq!(g.node(4).unwrap().rig, 1);
        // Both masters present, one per rig.
        let masters: Vec<NodeId> = g.nodes.iter()
            .filter(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).collect();
        assert_eq!(masters, vec![2, 5]);
        // Edge endpoints rewritten to the offset ids; edge ids offset too.
        assert!(g.has_route(0, 1, PortKind::Midi));
        assert!(g.has_route(1, 2, PortKind::Audio));
        assert!(g.has_route(3, 4, PortKind::Midi));
        assert!(g.has_route(4, 5, PortKind::Audio));
        // Edge base advances past rig A's highest cable id (2) → rig B: 4, 5.
        let eids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
        assert_eq!(eids, vec![1, 2, 4, 5]);
        // Counters continue past the merged ids: later edits never collide.
        let mut g = g;
        let late = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("LATE"));
        assert!(!ids.contains(&late));
        assert!(g.connect_with_id(
            g.edges.iter().map(|e| e.id).max().unwrap() + 1,
            PortRef { node: late, port: 0 },
            PortRef { node: 5, port: 0 },
            PortKind::Audio,
        ));
    }

    #[test]
    fn merge_rigs_respects_counters_after_removals() {
        // A rig whose counters outrun its live nodes (node 1 was removed):
        // the next rig must still start past id 3, not past len()==2.
        let mut a = mini_rig_graph();
        a.remove_node(1);
        let rigs = vec![Rig::new("A", a, None), Rig::new("B", mini_rig_graph(), None)];
        let g = merge_rigs(&rigs);
        let ids: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![0, 2, 3, 4, 5]);
        assert!(g.has_route(3, 4, PortKind::Midi));
        assert!(g.has_route(4, 5, PortKind::Audio));
        // Empty input merges to an empty, usable graph.
        let empty = merge_rigs(&[]);
        assert!(empty.nodes.is_empty() && empty.edges.is_empty());
    }

    #[test]
    fn split_rigs_inverts_merge_and_drops_a_rig() {
        let rigs = vec![
            Rig::new("A", mini_rig_graph(), None),
            Rig::new("B", mini_rig_graph(), None),
            Rig::new("C", mini_rig_graph(), None),
        ];
        let merged = merge_rigs(&rigs);
        let (graphs, bases) = split_rigs(&merged);
        assert_eq!(graphs.len(), 3);
        assert_eq!(bases, vec![0, 3, 6]);
        for g in &graphs {
            let ids: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
            assert_eq!(ids, vec![0, 1, 2], "rig-relative ids");
            assert!(g.nodes.iter().all(|n| n.rig == 0));
            assert!(g.has_route(0, 1, PortKind::Midi) && g.has_route(1, 2, PortKind::Audio));
            let eids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
            assert_eq!(eids, vec![1, 2]);
        }
        // Delete rig A, re-merge: B and C keep their relative layout.
        let rest: Vec<Rig> = graphs.into_iter().skip(1).zip(["B", "C"])
            .map(|(g, n)| Rig::new(n, g, None)).collect();
        let again = merge_rigs(&rest);
        assert_eq!(rig_bases(&rest), vec![0, 3, 6]);
        assert_eq!(again.nodes.len(), 6);
        assert_eq!(again.nodes.iter().map(|n| n.rig).max(), Some(1));
        assert!(again.has_route(3, 4, PortKind::Midi));
        // The merged counter continues past every rig: a later add never collides.
        let mut g = again.clone();
        let fresh = g.add_node(NodeKind::Keys, name_bytes("K"));
        assert_eq!(fresh, 6);
    }

    #[test]
    fn split_rigs_of_untagged_graph_is_one_rig() {
        let (graphs, bases) = split_rigs(&mini_rig_graph());
        assert_eq!(graphs.len(), 1);
        assert_eq!(bases, vec![0]);
        assert_eq!(graphs[0].edges.len(), 2);
    }

    #[test]
    fn rig_snapshot_json_roundtrip() {
        let s = RigSnapshot {
            rigs: vec![
                RigMeta { name: "A".into(), midi_device: Some(2) },
                RigMeta { name: "B".into(), midi_device: None },
            ],
            active: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RigSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.rigs.len(), 2);
        assert_eq!(back.rigs[0].name, "A");
        assert_eq!(back.rigs[0].midi_device, Some(2));
        assert_eq!(back.rigs[1].midi_device, None);
        // The bridge wire shape: {rigs:[{name,midi_device}],active}.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rigs"][1]["midi_device"], serde_json::Value::Null);
        assert_eq!(v["active"], serde_json::json!(1));
    }

    #[test]
    fn rig_and_load_rigs_json_roundtrip() {
        let cmd = Command::LoadRigs {
            rigs: vec![
                Rig::new("A", mini_rig_graph(), Some(2)),
                Rig::new("B", mini_rig_graph(), None),
            ],
            active: 1,
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&s).unwrap();
        assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
        // The exact wire shape the bridge accepts.
        let c: Command = serde_json::from_str(
            r#"{"LoadRigs":{"rigs":[{"name":"A","graph":{"nodes":[],"edges":[],"next_node":0,"next_edge":0},"midi_device":2}],"active":0}}"#,
        ).unwrap();
        match c {
            Command::LoadRigs { rigs, active } => {
                assert_eq!(rigs.len(), 1);
                assert_eq!(rigs[0].midi_device, Some(2));
                assert_eq!(active, 0);
            }
            _ => panic!("wrong variant"),
        }
        // midi_device is optional on the wire (defaults to None).
        let r: Rig = serde_json::from_str(
            r#"{"name":"X","graph":{"nodes":[],"edges":[],"next_node":0,"next_edge":0}}"#,
        ).unwrap();
        assert_eq!(r.midi_device, None);
    }

    #[test]
    fn audio_in_node_and_command_json_roundtrip() {
        // The wire shape: {AudioIn:{device,channels}} and {SetAudioIn:{...}}.
        let c: NodeKind = serde_json::from_str(r#"{"AudioIn":{"device":"USB Audio","channels":"MonoR"}}"#).unwrap();
        assert_eq!(c, NodeKind::AudioIn { device: Some("USB Audio".into()), channels: AudioInChannel::MonoR });
        let c: NodeKind = serde_json::from_str(r#"{"AudioIn":{"device":null,"channels":"Stereo"}}"#).unwrap();
        assert_eq!(c, NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo });

        let cmd: Command = serde_json::from_str(r#"{"SetAudioIn":{"node":3,"device":"Mic","channels":"Stereo"}}"#).unwrap();
        assert!(matches!(cmd, Command::SetAudioIn { node: 3, device, channels: Some(AudioInChannel::Stereo) } if device.as_deref() == Some("Mic")));
        let cmd: Command = serde_json::from_str(r#"{"SetAudioIn":{"node":3,"device":null,"channels":null}}"#).unwrap();
        assert!(matches!(cmd, Command::SetAudioIn { node: 3, device: None, channels: None }));

        // A graph with an AudioIn node round-trips through serde (sessions).
        let mut g = Graph::default();
        let a = g.add_node(NodeKind::AudioIn { device: Some("Iface".into()), channels: AudioInChannel::Stereo }, name_bytes("AUDIO IN"));
        let back: Graph = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back.node(a).unwrap().kind, NodeKind::AudioIn { device: Some("Iface".into()), channels: AudioInChannel::Stereo });
        assert!(Command::SetAudioIn { node: a, device: None, channels: None }.is_rack_edit());
    }
}
