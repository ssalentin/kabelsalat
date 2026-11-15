//! The single control plane (PLAN §2c).
//!
//! The UI, the built-in assistant, the CLI, and the future MCP server are
//! sibling frontends to this one typed API. Commands mutate the graph; the
//! engine consumes them at block boundaries via a lock-free bus. Queries read
//! snapshots. Nothing here touches the audio thread directly.
//!
//! RT discipline: commands are fixed-size (inline names, no String/Vec), so
//! draining them never allocates or frees. Graph storage is reserved up front.

use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// MIDI source: on-screen keys, laptop keys, hardware MIDI.
    Keys,
    /// Hosted CLAP plugin (e.g. Surge XT).
    Clap { plugin_id: Name },
    /// Built-in processing node.
    Dsp(DspKind),
    /// Hosted LV2 plugin (audio effect), identified by URI.
    Lv2 { uri: Lv2Uri },
    /// Desktop playback capture (P2).
    SystemAudio,
    /// Stereo output bus.
    MasterOut,
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
}

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
        self.nodes.push(Node { id, kind, name, rig: 0 });
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

    pub fn disconnect(&mut self, id: EdgeId) {
        self.edges.retain(|e| e.id != id);
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
            NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
            name_bytes("SURGE A"),
        );
        let delay_a = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY A"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
        let surge_b = g.add_node(
            NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
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
/// `None` = any source (on-screen keys, laptop keys, every hardware device);
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

/// Merge independent rig graphs into ONE patch (multi-rig load).
///
/// Node ids are offset by a cumulative base per rig (edge ids likewise), edge
/// endpoints are rewritten to the offset ids, and every node is tagged with
/// its rig index (`Node::rig`, saturating at 255). Each rig keeps its own
/// MasterOut. The merged id counters continue past every id the rigs used
/// (counters survive removals), so later edits never collide.
/// Rare structural op: allocates by design, like `Command::LoadGraph`.
pub fn merge_rigs(rigs: &[Rig]) -> Graph {
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
        let max_node = rig.graph.nodes.iter().map(|n| n.id).max().map_or(0, |m| m + 1);
        let max_edge = rig.graph.edges.iter().map(|e| e.id).max().map_or(0, |m| m + 1);
        node_base = node_base.saturating_add(rig.graph.next_node.max(max_node));
        edge_base = edge_base.saturating_add(rig.graph.next_edge.max(max_edge));
    }
    merged.next_node = node_base;
    merged.next_edge = edge_base;
    merged
}

/// Everything a frontend can ask the engine to do. Fixed-size: no heap.
/// Serde is for off-thread frontends only (bridge socket, sessions); the RT
/// bus still moves plain fixed-size values — no allocation on the audio path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    AddNode { kind: NodeKind, name: Name },
    RemoveNode(NodeId),
    Connect { id: EdgeId, from: PortRef, to: PortRef, kind: PortKind },
    Disconnect(EdgeId),
    SetParam { node: NodeId, param: ParamId, value: f64 },
    NoteOn { note: u8, velocity: f32, source: SourceId },
    NoteOff { note: u8, source: SourceId },
    /// Per-rig input selection (AUM operating model): which source classes
    /// feed this instrument node. The MIDI cable still gates routing; this
    /// filters by source on top. Default: all true.
    SetRigSource { node: NodeId, keys: bool, laptop: bool, hardware: bool },
    /// Per-rig hardware device selection: None = any device, Some(id) = only
    /// that device (AUM: a different keyboard per rig).
    SetRigDevice { node: NodeId, device: Option<u32> },
    /// Replace the whole graph (template load). The engine rebuilds DSP, remaps
    /// the existing CLAP voices onto the new Clap nodes, and rebuilds the MIDI
    /// matrix. Rare structural op: allocates (bends RT discipline by design).
    LoadGraph { graph: Graph },
    /// Load several independent rigs at once: their graphs are merged
    /// ([`merge_rigs`]), DSP/LV2 rebuilt, the existing CLAP voices remapped to
    /// Clap nodes in node-id order, and each rig's instruments driven by that
    /// rig's `midi_device` (None = keys + laptop + all hardware devices).
    /// `active` is the UI-selected rig index (engine keeps it for queries).
    /// Rare structural op: allocates (bends RT discipline by design).
    LoadRigs { rigs: Vec<Rig>, active: usize },
    /// Keyboard split across the two rigs: notes below `point` → rig A (node 1),
    /// notes at/above → rig B (node 4). `None` disables the split (both full range).
    SetSplit { point: Option<u8> },
    /// Mix system-audio capture (e.g. YouTube) into the master at `gain` (0..1).
    /// 0 mutes it. The capture source is started at app startup.
    SetSystemAudio { gain: f32 },
    /// Mix the file player (backing track) into the master at `gain` (0..1).
    /// Loading/stopping a file is done off-bus (variable-length path).
    SetPlayer { gain: f32 },
    TransportPlay,
    TransportStop,
    /// Set the metronome tempo (BPM).
    SetBpm { bpm: f32 },
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
        assert!(bus.send(Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys }));
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
            Command::AddNode { kind: NodeKind::Dsp(DspKind::Delay), name: name_bytes("DELAY X") },
            Command::RemoveNode(3),
            Command::Connect { id: 12, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 1, port: 0 }, kind: PortKind::Midi },
            Command::Disconnect(12),
            Command::SetParam { node: 3, param: 3, value: 0.75 },
            Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys },
            Command::NoteOff { note: 60, source: SourceId::Hardware(20) },
            Command::SetRigSource { node: 1, keys: true, laptop: false, hardware: true },
            Command::SetSplit { point: Some(60) },
            Command::SetSplit { point: None },
            Command::SetSystemAudio { gain: 0.5 },
            Command::TransportPlay,
            Command::TransportStop,
        ];
        for cmd in cmds {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&s).unwrap();
            assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
        }
        // The exact shape the CLI/bridge accepts.
        let c: Command = serde_json::from_str(r#"{"SetSystemAudio":{"gain":0.5}}"#).unwrap();
        assert!(matches!(c, Command::SetSystemAudio { gain } if (gain - 0.5).abs() < 1e-6));
    }

    /// A minimal rig patch: Keys(0) → Clap(1) → Master(2), cables 1 and 2.
    fn mini_rig_graph() -> Graph {
        let mut g = Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(
            NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
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
}
