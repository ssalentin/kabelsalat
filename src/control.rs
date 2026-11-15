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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// MIDI source: on-screen keys, laptop keys, hardware MIDI.
    Keys,
    /// Hosted CLAP plugin (e.g. Surge XT).
    Clap { plugin_id: Name },
    /// Built-in processing node.
    Dsp(DspKind),
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
        self.nodes.push(Node { id, kind, name });
        id
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
}
