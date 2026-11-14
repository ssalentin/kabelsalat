//! The single control plane (PLAN §2c).
//!
//! The UI, the built-in assistant, the CLI, and the future MCP server are
//! sibling frontends to this one typed API. Commands mutate the graph; the
//! engine consumes them at block boundaries via a lock-free bus. Queries read
//! snapshots. Nothing here touches the audio thread directly.

use serde::{Deserialize, Serialize};

pub type NodeId = u32;
pub type EdgeId = u32;
pub type ParamId = u32;

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
    Clap { plugin_id: String },
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
    pub name: String,
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

/// The patch. Serializable for sessions (task-8).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    next_node: NodeId,
    next_edge: EdgeId,
}

impl Graph {
    pub fn add_node(&mut self, kind: NodeKind, name: impl Into<String>) -> NodeId {
        let id = self.next_node;
        self.next_node += 1;
        self.nodes.push(Node { id, kind, name: name.into() });
        id
    }

    pub fn remove_node(&mut self, id: NodeId) {
        self.nodes.retain(|n| n.id != id);
        self.edges.retain(|e| e.from.node != id && e.to.node != id);
    }

    /// Connect two ports. Rejects self-connections and duplicates.
    pub fn connect(&mut self, from: PortRef, to: PortRef, kind: PortKind) -> Option<EdgeId> {
        if from.node == to.node {
            return None;
        }
        if !self.nodes.iter().any(|n| n.id == from.node)
            || !self.nodes.iter().any(|n| n.id == to.node)
        {
            return None;
        }
        if self.edges.iter().any(|e| e.from == from && e.to == to) {
            return None;
        }
        let id = self.next_edge;
        self.next_edge += 1;
        self.edges.push(Edge { id, from, to, kind });
        Some(id)
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

    /// The starter patch matching the rack: Keys → Surge → Delay → Master,
    /// plus a dry Surge → Master run. Pre-wired and playable (PLAN §2b).
    pub fn demo_patch() -> Self {
        let mut g = Self::default();
        let keys = g.add_node(NodeKind::Keys, "KEYS");
        let surge = g.add_node(
            NodeKind::Clap { plugin_id: "org.surge-synth-team.surge-xt".into() },
            "SURGE XT",
        );
        let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), "DELAY");
        let master = g.add_node(NodeKind::MasterOut, "MASTER OUT");
        g.connect(PortRef { node: keys, port: 0 }, PortRef { node: surge, port: 0 }, PortKind::Midi);
        g.connect(PortRef { node: surge, port: 0 }, PortRef { node: delay, port: 0 }, PortKind::Audio);
        g.connect(PortRef { node: delay, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect(PortRef { node: surge, port: 0 }, PortRef { node: master, port: 1 }, PortKind::Audio);
        g
    }
}

/// Everything a frontend can ask the engine to do.
#[derive(Clone, Debug)]
pub enum Command {
    AddNode { kind: NodeKind, name: String },
    RemoveNode(NodeId),
    Connect { from: PortRef, to: PortRef, kind: PortKind },
    Disconnect(EdgeId),
    SetParam { node: NodeId, param: ParamId, value: f64 },
    NoteOn { note: u8, velocity: f32 },
    NoteOff { note: u8 },
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
        assert_eq!(g.nodes.len(), 4);
        assert_eq!(g.edges.len(), 4);
        assert!(g.has_route(0, 1, PortKind::Midi));
        assert!(g.has_route(1, 3, PortKind::Audio));
    }

    #[test]
    fn connect_rejects_self_and_duplicates() {
        let mut g = Graph::demo_patch();
        let p = PortRef { node: 0, port: 0 };
        assert!(g.connect(p, p, PortKind::Midi).is_none());
        let before = g.edges.len();
        g.connect(PortRef { node: 0, port: 0 }, PortRef { node: 1, port: 0 }, PortKind::Midi);
        assert_eq!(g.edges.len(), before);
    }

    #[test]
    fn remove_node_cleans_edges() {
        let mut g = Graph::demo_patch();
        g.remove_node(1); // Surge
        assert_eq!(g.nodes.len(), 3);
        assert!(g.edges.iter().all(|e| e.from.node != 1 && e.to.node != 1));
    }

    #[test]
    fn bus_roundtrip() {
        let (mut bus, mut drain) = command_bus();
        assert!(bus.send(Command::NoteOn { note: 60, velocity: 0.8 }));
        let mut got = Vec::new();
        drain.drain(|c| got.push(c));
        assert_eq!(got.len(), 1);
    }
}
