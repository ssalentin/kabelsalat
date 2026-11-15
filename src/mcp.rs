//! MCP server — `kabelsalat mcp` (PLAN §2c, task-14) and `kabelsalat mcp
//! --attach` (task-16).
//!
//! A sibling frontend to the one typed control plane (`control.rs`), giving
//! agents 1:1 access to the graph engine. ONE set of 20 tool definitions
//! serves two backends ([`Backend`]):
//!
//! - **Local (headless):** this process owns a local [`Graph`] model (started
//!   from `demo_patch`) AND a running [`GraphEngine`] fed by the same
//!   lock-free command bus the UI uses. Mutating tools validate + update the
//!   local model and send the matching [`Command`] to the engine; query tools
//!   read the local model plus engine status (output peak via `level_handle`).
//!   No shared state ever touches the audio thread.
//! - **Remote (attach):** no engine here — every command/query is proxied to
//!   the RUNNING GUI app over the unix-socket bridge (`bridge.rs`), so agents
//!   control exactly what the user sees. The local graph is kept as a mirror
//!   (refreshed from the app before queries/validation) so tool semantics,
//!   ids and schemas stay identical to headless mode.
//! - Audio: a cpal output stream renders `GraphEngine::render` in its
//!   callback (same pattern as the app, minus egui). Two Surge XT voices
//!   serve nodes 1 and 4; their `ClapVoiceMain` halves stay on the main
//!   thread. If audio init fails, a dummy render loop keeps draining the
//!   bus so every tool still works headless.
//! - Transport: MCP over stdio via rmcp. stdout is reserved for JSON-RPC:
//!   fd 1 is rerouted to stderr for the whole process (the CLAP host logs
//!   to stdout while loading plugins) and the server writes to a private
//!   duplicate of the original stdout.

use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bridge;
use crate::control::{
    self, merge_rigs, name_bytes, name_str, Command, CommandBus, DspKind, EdgeId, Graph, NodeId,
    NodeKind, PortKind, PortRef, Rig, RigSnapshot,
};
use crate::engine::GraphEngine;
use crate::host::{ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;

const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

// ---------------------------------------------------------------------------
// Shared state: model mirror + backend + engine status (never the engine itself)
// ---------------------------------------------------------------------------

/// Where commands go and queries come from. The tool definitions below are
/// backend-agnostic: they build the same typed [`Command`]s either way.
enum Backend {
    /// Headless: our own engine, fed through the local lock-free bus.
    Local(CommandBus),
    /// Attach: the running GUI app, via the unix-socket control bridge.
    Remote,
}

struct State {
    graph: Graph,
    backend: Backend,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    audio_live: bool,
    playing: bool,
    /// Multi-rig state. Local: the engine's shared snapshot (it publishes on
    /// LoadRigs/LoadGraph at block boundaries). Remote: unused — get_rigs
    /// forwards to the app's bridge instead.
    rigs: Arc<Mutex<RigSnapshot>>,
}

#[derive(Clone)]
struct McpServer {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Tool argument types (JSON Schema via schemars, parsed by rmcp)
// ---------------------------------------------------------------------------

/// Arguments for `add_node`.
#[derive(Debug, Deserialize, JsonSchema)]
struct AddNodeArgs {
    /// Node kind: "keys", "clap", "dsp", "system_audio" or "master_out".
    kind: String,
    /// Display name (max 31 bytes; defaults to the kind).
    name: Option<String>,
    /// For kind "dsp": gain, pan, eq3, filter, limiter, saturation, delay, reverb.
    dsp: Option<String>,
    /// For kind "clap": CLAP plugin id (defaults to Surge XT).
    plugin_id: Option<String>,
    /// For kind "lv2": the LV2 plugin URI (see list_plugins).
    uri: Option<String>,
}

/// Arguments for `remove_node`.
#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveNodeArgs {
    /// Id of the node to remove (its cables are removed too).
    node: NodeId,
}

/// Arguments for `connect`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ConnectArgs {
    /// Cable id; auto-assigned (max existing + 1) when omitted.
    id: Option<EdgeId>,
    /// Source node id.
    from_node: NodeId,
    /// Source port (default 0).
    from_port: Option<u8>,
    /// Destination node id.
    to_node: NodeId,
    /// Destination port (default 0).
    to_port: Option<u8>,
    /// Cable kind: "midi" or "audio" (default "audio").
    kind: Option<String>,
}

/// Arguments for `disconnect`.
#[derive(Debug, Deserialize, JsonSchema)]
struct DisconnectArgs {
    /// Id of the cable to unplug.
    edge: EdgeId,
}

/// Arguments for `set_param`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetParamArgs {
    /// Target node id.
    node: NodeId,
    /// Engine param id: 0=level (instrument), 1=delay time, 2=delay mix,
    /// 3=master volume, 4=master warmth/saturation.
    param: u32,
    /// New value (plain units, e.g. 0.0..=1.0 for levels/mix).
    value: f64,
}

/// Arguments for `set_rig_source`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRigSourceArgs {
    /// Instrument node id whose input selection to set.
    node: NodeId,
    /// Accept notes from the on-screen keys.
    keys: bool,
    /// Accept notes from the laptop keyboard.
    laptop: bool,
    /// Accept notes from hardware MIDI inputs.
    hardware: bool,
}

/// Arguments for `set_split`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSplitArgs {
    /// MIDI note 0..=127 to split the keyboard at: rig A (node 1) plays below,
    /// rig B (node 4) at/above. Omit (null) to disable the split.
    point: Option<u8>,
}

/// Arguments for `set_system_audio`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSystemAudioArgs {
    /// Mix level 0..1 for system-audio capture (0 mutes). Headless mode has no
    /// capture source, so this only sets the gain.
    gain: f32,
}

/// Arguments for `set_player`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetPlayerArgs {
    /// Mix level 0..1 for the file player (0 mutes).
    gain: f32,
}

/// Arguments for `set_bpm`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetBpmArgs {
    /// Metronome tempo in BPM (20..300).
    bpm: f32,
}

/// Arguments for `set_rig_device`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRigDeviceArgs {
    /// Instrument node id (1 = Surge A, 4 = Surge B).
    node: NodeId,
    /// Hardware device index, or null for any device.
    device: Option<u32>,
}

/// Arguments for `note_on`.
#[derive(Debug, Deserialize, JsonSchema)]
struct NoteOnArgs {
    /// MIDI note number (0..=127).
    note: u8,
    /// Velocity 0.0..=1.0 (default 0.8).
    velocity: Option<f32>,
    /// Source: "keys" (default), "laptop", "hardware:<port>",
    /// "virtual:<port>" or "file_player:<id>".
    source: Option<String>,
}

/// Arguments for `note_off`.
#[derive(Debug, Deserialize, JsonSchema)]
struct NoteOffArgs {
    /// MIDI note number (0..=127).
    note: u8,
    /// Source (same syntax as note_on; default "keys").
    source: Option<String>,
}

/// One rig for `load_rigs`: a full rack graph bound to a MIDI device.
#[derive(Debug, Deserialize, JsonSchema)]
struct RigArg {
    /// Rig display name.
    name: String,
    /// The rig's patch: {"nodes":[{"id","kind","name"?}],"edges":[{"id",
    /// "from":{"node","port"},"to":{"node","port"},"kind":"midi"|"audio"}]}.
    /// Kinds: "Keys", "MasterOut", "SystemAudio", {"Clap":{"plugin_id"}},
    /// {"Dsp":"delay"|...}, {"Lv2":{"uri"}}. Names/plugin ids accept plain
    /// strings; id counters are filled in automatically. Each rig needs its
    /// own MasterOut.
    graph: Value,
    /// Hardware MIDI device id feeding this rig; null/omitted = any source
    /// (on-screen keys, laptop keys, every hardware device).
    midi_device: Option<u32>,
}

/// Arguments for `load_rigs`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadRigsArgs {
    /// The rigs to load (each a full rack with its own MasterOut).
    rigs: Vec<RigArg>,
    /// UI-selected rig index (default 0).
    active: Option<usize>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json(v: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(v.to_string())]))
}

/// A failure the calling agent should see (isError tool result, not a
/// JSON-RPC protocol error).
fn tool_err(msg: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(msg.into())]))
}

fn send_cmd(st: &mut State, cmd: Command) -> Result<(), String> {
    match &mut st.backend {
        Backend::Local(bus) => {
            if bus.send(cmd) {
                Ok(())
            } else {
                Err("command bus full — try again".into())
            }
        }
        Backend::Remote => {
            let reply = bridge::request(&json!({"op": "cmd", "cmd": cmd}))
                .map_err(|e| format!("{e:#}"))?;
            if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(())
            } else {
                Err(reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("bridge rejected the command")
                    .to_string())
            }
        }
    }
}

/// Attach mode: pull the LIVE graph from the GUI app so queries and
/// validation see what the user sees (the user may have re-patched since our
/// last call). No-op for the headless backend, which owns its model.
fn refresh_remote(st: &mut State) -> Result<(), String> {
    if !matches!(st.backend, Backend::Remote) {
        return Ok(());
    }
    let reply = bridge::request(&json!({"op": "query", "name": "get_graph"}))
        .map_err(|e| format!("{e:#}"))?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("bridge query failed")
            .to_string());
    }
    let data = reply.get("data").cloned().unwrap_or(Value::Null);
    st.graph = serde_json::from_value(data).map_err(|e| format!("bad graph from bridge: {e}"))?;
    Ok(())
}

fn dsp_kind_str(k: &DspKind) -> &'static str {
    match k {
        DspKind::Gain => "gain",
        DspKind::Pan => "pan",
        DspKind::Eq3 => "eq3",
        DspKind::Filter => "filter",
        DspKind::Limiter => "limiter",
        DspKind::Saturation => "saturation",
        DspKind::Delay => "delay",
        DspKind::Reverb => "reverb",
        DspKind::Metronome => "metronome",
    }
}

fn parse_dsp_kind(s: &str) -> Result<DspKind, String> {
    match s.to_ascii_lowercase().as_str() {
        "gain" => Ok(DspKind::Gain),
        "pan" => Ok(DspKind::Pan),
        "eq3" => Ok(DspKind::Eq3),
        "filter" => Ok(DspKind::Filter),
        "limiter" => Ok(DspKind::Limiter),
        "saturation" => Ok(DspKind::Saturation),
        "delay" => Ok(DspKind::Delay),
        "reverb" => Ok(DspKind::Reverb),
        "metronome" => Ok(DspKind::Metronome),
        other => Err(format!("unknown dsp kind '{other}'")),
    }
}

fn parse_node_kind(args: &AddNodeArgs) -> Result<NodeKind, String> {
    match args.kind.to_ascii_lowercase().as_str() {
        "keys" => Ok(NodeKind::Keys),
        "clap" => Ok(NodeKind::Clap {
            plugin_id: name_bytes(args.plugin_id.as_deref().unwrap_or(SURGE_ID)),
        }),
        "dsp" => {
            let dsp = args
                .dsp
                .as_deref()
                .ok_or_else(|| "kind 'dsp' requires the 'dsp' field".to_string())?;
            Ok(NodeKind::Dsp(parse_dsp_kind(dsp)?))
        }
        "system_audio" => Ok(NodeKind::SystemAudio),
        "master_out" => Ok(NodeKind::MasterOut),
        "lv2" => {
            let uri = args
                .uri
                .as_deref()
                .ok_or_else(|| "kind 'lv2' requires the 'uri' field".to_string())?;
            Ok(NodeKind::Lv2 { uri: crate::control::Lv2Uri::new(uri) })
        }
        other => Err(format!("unknown node kind '{other}'")),
    }
}

fn parse_port_kind(s: Option<&str>) -> Result<PortKind, String> {
    match s.unwrap_or("audio").to_ascii_lowercase().as_str() {
        "midi" => Ok(PortKind::Midi),
        "audio" => Ok(PortKind::Audio),
        other => Err(format!("unknown cable kind '{other}' (use midi|audio)")),
    }
}

fn parse_source(s: Option<&str>) -> Result<SourceId, String> {
    let s = s.unwrap_or("keys").to_ascii_lowercase();
    let (tag, arg) = match s.split_once(':') {
        Some((t, a)) => (t, Some(a)),
        None => (s.as_str(), None),
    };
    let port = || -> Result<u32, String> {
        arg.ok_or_else(|| format!("source '{tag}' needs ':<port>'"))?
            .parse()
            .map_err(|_| format!("invalid port in source '{s}'"))
    };
    match tag {
        "keys" => Ok(SourceId::Keys),
        "laptop" | "laptop_keys" => Ok(SourceId::LaptopKeys),
        "hardware" => Ok(SourceId::Hardware(port()?)),
        "virtual" => Ok(SourceId::Virtual(port()?)),
        "file_player" => Ok(SourceId::FilePlayer(port()?)),
        other => Err(format!("unknown source '{other}'")),
    }
}

/// Agent-friendly view of the graph (names as strings, kinds as tags).
fn graph_json(g: &Graph) -> Value {
    let nodes: Vec<Value> = g
        .nodes
        .iter()
        .map(|n| {
            let kind = match &n.kind {
                NodeKind::Keys => json!({"type": "keys"}),
                NodeKind::Clap { plugin_id } => {
                    json!({"type": "clap", "plugin_id": name_str(plugin_id)})
                }
                NodeKind::Dsp(k) => json!({"type": "dsp", "dsp": dsp_kind_str(k)}),
                NodeKind::Lv2 { uri } => json!({"type": "lv2", "uri": uri.as_str()}),
                NodeKind::SystemAudio => json!({"type": "system_audio"}),
                NodeKind::MasterOut => json!({"type": "master_out"}),
            };
            json!({"id": n.id, "name": name_str(&n.name), "kind": kind})
        })
        .collect();
    let edges: Vec<Value> = g
        .edges
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "from": {"node": e.from.node, "port": e.from.port},
                "to": {"node": e.to.node, "port": e.to.port},
                "kind": match e.kind { PortKind::Midi => "midi", PortKind::Audio => "audio" },
            })
        })
        .collect();
    json!({"nodes": nodes, "edges": edges})
}

/// Mirrors `Graph::connect_with_id` validation without mutating, so the
/// command is only sent (and the model only changed) when it will stick.
fn validate_connect(g: &Graph, id: EdgeId, from: PortRef, to: PortRef) -> Result<(), String> {
    if from.node == to.node {
        return Err("self-connections are rejected".into());
    }
    if g.node(from.node).is_none() {
        return Err(format!("node {} does not exist", from.node));
    }
    if g.node(to.node).is_none() {
        return Err(format!("node {} does not exist", to.node));
    }
    if g.edges.iter().any(|e| e.id == id) {
        return Err(format!("edge id {id} already in use"));
    }
    if g.edges.iter().any(|e| e.from == from && e.to == to) {
        return Err("these endpoints are already connected".into());
    }
    Ok(())
}

/// Accept agent-friendly rig graphs: plain-string names/plugin ids are
/// converted to the control plane's inline byte arrays, missing names
/// default to empty, and the id counters are filled in from the highest
/// ids. The raw serde shape (byte arrays + explicit counters) passes through
/// unchanged, so bridge-style graphs work too.
fn normalize_rig_graph(v: &mut Value) -> Result<(), String> {
    let obj = v.as_object_mut().ok_or("rig graph must be a JSON object")?;
    let mut max_node = 0u32;
    let mut max_edge = 0u32;
    if let Some(nodes) = obj.get_mut("nodes").and_then(Value::as_array_mut) {
        for n in nodes {
            let no = n.as_object_mut().ok_or("each node must be a JSON object")?;
            match no.get("name") {
                Some(Value::String(s)) => {
                    no.insert("name".into(), json!(name_bytes(s)));
                }
                None => {
                    no.insert("name".into(), json!(name_bytes("")));
                }
                _ => {} // already inline bytes
            }
            let pid = no.get("kind")
                .and_then(|k| k.pointer("/Clap/plugin_id"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            if let Some(pid) = pid {
                no["kind"]["Clap"]["plugin_id"] = json!(name_bytes(&pid));
            }
            if let Some(id) = no.get("id").and_then(Value::as_u64) {
                max_node = max_node.max(id as u32 + 1);
            }
        }
    }
    if let Some(edges) = obj.get("edges").and_then(Value::as_array) {
        for e in edges {
            if let Some(id) = e.get("id").and_then(Value::as_u64) {
                max_edge = max_edge.max(id as u32 + 1);
            }
        }
    }
    let nn = obj.get("next_node").and_then(Value::as_u64).unwrap_or(0) as u32;
    obj.insert("next_node".into(), json!(nn.max(max_node)));
    let ne = obj.get("next_edge").and_then(Value::as_u64).unwrap_or(0) as u32;
    obj.insert("next_edge".into(), json!(ne.max(max_edge)));
    Ok(())
}

// ---------------------------------------------------------------------------
// Tools — 1:1 with the control plane
// ---------------------------------------------------------------------------

#[tool_router]
impl McpServer {
    /// The current patch: all nodes and cables.
    #[tool(name = "get_graph", description = "Query the full patch graph: nodes (id, name, kind) and edges/cables (id, from/to port, midi|audio).")]
    fn get_graph(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        ok_json(graph_json(&st.graph))
    }

    /// Engine status snapshot.
    #[tool(name = "get_status", description = "Query engine status: node/edge counts, sample rate, whether live audio output is running, current output peak level (0..1) and transport state.")]
    fn get_status(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            // The app reports its own live status (peak, rate, counts).
            let reply = match bridge::request(&json!({"op": "query", "name": "get_status"})) {
                Ok(r) => r,
                Err(e) => return tool_err(format!("{e:#}")),
            };
            if reply.get("ok").and_then(Value::as_bool) != Some(true) {
                return tool_err(
                    reply.get("error").and_then(Value::as_str).unwrap_or("bridge query failed"),
                );
            }
            return ok_json(reply.get("data").cloned().unwrap_or(Value::Null));
        }
        let level = f32::from_bits(st.level.load(Ordering::Relaxed));
        let (rig_count, active_rig) = match st.rigs.lock() {
            Ok(s) => (s.rigs.len(), s.active),
            Err(_) => (0, 0),
        };
        ok_json(json!({
            "nodes": st.graph.nodes.len(),
            "edges": st.graph.edges.len(),
            "sample_rate": st.sample_rate,
            "audio_live": st.audio_live,
            "output_level": level,
            "transport_playing": st.playing,
            "buffer_frames": bridge::BUFFER_FRAMES,
            "buffer_latency_ms": bridge::BUFFER_FRAMES as f64 / st.sample_rate * 1000.0,
            "xruns": bridge::XRUNS.load(Ordering::Relaxed),
            "midi_latency_ms": bridge::MIDI_LATENCY_US.load(Ordering::Relaxed) as f64 / 1000.0,
            "rigs": rig_count,
            "active_rig": active_rig,
        }))
    }

    /// Add a node to the patch.
    #[tool(name = "add_node", description = "Add a node (Command::AddNode). kind: keys|clap|dsp|lv2|system_audio|master_out; dsp nodes need 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb); lv2 nodes need 'uri'; clap nodes take an optional plugin_id (default Surge XT). Added dsp/lv2 nodes sound immediately (processors are built); wire them with connect. Clap voices are main-thread-only, so new clap nodes sound via load_graph's voice remap, not add_node.")]
    fn add_node(&self, Parameters(args): Parameters<AddNodeArgs>) -> Result<CallToolResult, McpError> {
        let kind = match parse_node_kind(&args) {
            Ok(k) => k,
            Err(e) => return tool_err(e),
        };
        let label = args.name.clone().unwrap_or_else(|| match &kind {
            NodeKind::Keys => "KEYS".into(),
            NodeKind::Clap { .. } => "CLAP".into(),
            NodeKind::Dsp(k) => dsp_kind_str(k).to_ascii_uppercase(),
            NodeKind::Lv2 { uri } => uri
                .as_str()
                .rsplit(['/', '#'])
                .next()
                .unwrap_or("LV2")
                .to_ascii_uppercase(),
            NodeKind::SystemAudio => "SYSTEM AUDIO".into(),
            NodeKind::MasterOut => "MASTER OUT".into(),
        });
        let name = name_bytes(&label);
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        // Send first, then mirror into the local model: both id counters
        // advance exactly once per accepted command and stay in sync.
        if let Err(e) = send_cmd(&mut st, Command::AddNode { kind: kind.clone(), name }) {
            return tool_err(e);
        }
        let id = st.graph.add_node(kind, name);
        ok_json(json!({"id": id, "name": name_str(&name)}))
    }

    /// Remove a node and its cables.
    #[tool(name = "remove_node", description = "Remove a node by id (Command::RemoveNode); all cables touching it are removed too.")]
    fn remove_node(&self, Parameters(args): Parameters<RemoveNodeArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::RemoveNode(args.node)) {
            return tool_err(e);
        }
        st.graph.remove_node(args.node);
        ok_json(json!({"removed": args.node}))
    }

    /// Connect two ports with a cable.
    #[tool(name = "connect", description = "Connect two ports (Command::Connect). Rejects self-connections, duplicate ids and duplicate endpoints. kind defaults to audio; ports default to 0; id is auto-assigned when omitted. Returns the cable id.")]
    fn connect(&self, Parameters(args): Parameters<ConnectArgs>) -> Result<CallToolResult, McpError> {
        let kind = match parse_port_kind(args.kind.as_deref()) {
            Ok(k) => k,
            Err(e) => return tool_err(e),
        };
        let from = PortRef { node: args.from_node, port: args.from_port.unwrap_or(0) };
        let to = PortRef { node: args.to_node, port: args.to_port.unwrap_or(0) };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let id = match args.id {
            Some(id) => id,
            None => st.graph.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1,
        };
        if let Err(e) = validate_connect(&st.graph, id, from, to) {
            return tool_err(e);
        }
        if let Err(e) = send_cmd(&mut st, Command::Connect { id, from, to, kind }) {
            return tool_err(e);
        }
        st.graph.connect_with_id(id, from, to, kind);
        ok_json(json!({"id": id, "from": {"node": from.node, "port": from.port}, "to": {"node": to.node, "port": to.port}}))
    }

    /// Unplug a cable.
    #[tool(name = "disconnect", description = "Unplug a cable by edge id (Command::Disconnect). Unknown ids are a no-op.")]
    fn disconnect(&self, Parameters(args): Parameters<DisconnectArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::Disconnect(args.edge)) {
            return tool_err(e);
        }
        st.graph.disconnect(args.edge);
        ok_json(json!({"disconnected": args.edge}))
    }

    /// Set an engine parameter.
    #[tool(name = "set_param", description = "Set a parameter (Command::SetParam). Param ids: 0=instrument level, 1=delay time, 2=delay mix, 3=master volume, 4=master warmth. Values are plain units (levels/mix 0..1).")]
    fn set_param(&self, Parameters(args): Parameters<SetParamArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: args.param, value: args.value }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "param": args.param, "value": args.value}))
    }

    /// Per-rig input selection.
    #[tool(name = "set_rig_source", description = "Select which source classes feed an instrument node (Command::SetRigSource): keys, laptop, hardware. The MIDI cable still gates routing; this filters by source on top.")]
    fn set_rig_source(&self, Parameters(args): Parameters<SetRigSourceArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::SetRigSource { node: args.node, keys: args.keys, laptop: args.laptop, hardware: args.hardware }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "keys": args.keys, "laptop": args.laptop, "hardware": args.hardware}))
    }

    /// Split the keyboard across the two rigs.
    #[tool(name = "set_split", description = "Split the keyboard across the two rigs (Command::SetSplit). point is a MIDI note 0..=127: rig A (node 1) plays below it, rig B (node 4) at/above. Omit point (null) to disable the split so both rigs play the full keyboard. Example: point 60 splits at C4.")]
    fn set_split(&self, Parameters(args): Parameters<SetSplitArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = args.point {
            if p > 127 {
                return tool_err("point must be 0..=127");
            }
        }
        if let Err(e) = send_cmd(&mut st, Command::SetSplit { point: args.point }) {
            return tool_err(e);
        }
        ok_json(json!({"point": args.point, "split": args.point.is_some()}))
    }

    /// Mix system audio into the master.
    #[tool(name = "set_system_audio", description = "Mix system-audio capture (e.g. YouTube) into the master (Command::SetSystemAudio). gain 0..1, 0 mutes. In the GUI app this captures the default output's monitor; headless mode has no capture source so it only stores the gain.")]
    fn set_system_audio(&self, Parameters(args): Parameters<SetSystemAudioArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let gain = args.gain.clamp(0.0, 1.0);
        if let Err(e) = send_cmd(&mut st, Command::SetSystemAudio { gain }) {
            return tool_err(e);
        }
        ok_json(json!({"gain": gain}))
    }

    /// Set the file-player mix gain.
    #[tool(name = "set_player", description = "Set the backing-track file player's mix gain (Command::SetPlayer), 0..1, 0 mutes. Loading a file is done in the GUI app (play_file assistant tool) since paths can't cross the fixed-size command bus.")]
    fn set_player(&self, Parameters(args): Parameters<SetPlayerArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let gain = args.gain.clamp(0.0, 1.0);
        if let Err(e) = send_cmd(&mut st, Command::SetPlayer { gain }) {
            return tool_err(e);
        }
        ok_json(json!({"gain": gain}))
    }

    /// Select which hardware device feeds a rig.
    #[tool(name = "set_rig_device", description = "Select which hardware MIDI device feeds a rig (Command::SetRigDevice) — AUM's 'different keyboard per rig'. node 1=Surge A, 4=Surge B. device is a device index; omit (null) for any device.")]
    fn set_rig_device(&self, Parameters(args): Parameters<SetRigDeviceArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::SetRigDevice { node: args.node, device: args.device }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "device": args.device}))
    }

    /// Load several independent rigs at once.
    #[tool(name = "load_rigs", description = "Load multiple independent rigs (Command::LoadRigs): each rig is a full rack graph with its own MasterOut, bound to a hardware MIDI device (midi_device null = keys/laptop/all devices). Graphs use the patch shape: nodes [{id, kind, name?}] and edges [{id, from:{node,port}, to:{node,port}, kind:'midi'|'audio'}]; kinds are 'Keys', 'MasterOut', 'SystemAudio', {Clap:{plugin_id}}, {Dsp:'delay'|'reverb'|...}, {Lv2:{uri}}; names/plugin ids may be plain strings. The 2 CLAP voices are remapped to Clap nodes in node-id order across all rigs (surplus Clap nodes are silent — use LV2 instruments for more). Returns {loaded, active, nodes, edges}. Query the result with get_rigs.")]
    fn load_rigs(&self, Parameters(args): Parameters<LoadRigsArgs>) -> Result<CallToolResult, McpError> {
        let active = args.active.unwrap_or(0);
        let mut rigs = Vec::with_capacity(args.rigs.len());
        for r in args.rigs {
            let mut gv = r.graph;
            if let Err(e) = normalize_rig_graph(&mut gv) {
                return tool_err(format!("rig '{}': {e}", r.name));
            }
            let graph: Graph = match serde_json::from_value(gv) {
                Ok(g) => g,
                Err(e) => return tool_err(format!("rig '{}': invalid graph: {e}", r.name)),
            };
            rigs.push(Rig::new(r.name, graph, r.midi_device));
        }
        // Plain-language integrity check (names, active range, cable wiring).
        if let Err(e) = crate::session::MultiRigSession::new("mcp", rigs.clone(), active).validate() {
            return tool_err(format!("{e}"));
        }
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::LoadRigs { rigs: rigs.clone(), active }) {
            return tool_err(e);
        }
        // Mirror the merged patch locally so follow-up edits validate against
        // the real (offset) node ids.
        let merged = merge_rigs(&rigs);
        let summary = json!({
            "loaded": rigs.len(),
            "active": active,
            "nodes": merged.nodes.len(),
            "edges": merged.edges.len(),
        });
        st.graph = merged;
        if matches!(st.backend, Backend::Local(_)) {
            // Immediate consistency; the engine re-publishes the same state
            // when it drains the command at the next block boundary.
            if let Ok(mut s) = st.rigs.lock() {
                s.rigs = rigs.iter().map(|r| crate::control::RigMeta {
                    name: r.name.clone(),
                    midi_device: r.midi_device,
                }).collect();
                s.active = active;
            }
        }
        ok_json(summary)
    }

    /// Query the multi-rig state.
    #[tool(name = "get_rigs", description = "Query the multi-rig state: {rigs:[{name, midi_device}], active}. Empty rigs means single-graph mode (a plain graph load clears it). midi_device null = any source; a number = only that hardware MIDI device.")]
    fn get_rigs(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            let reply = match bridge::request(&json!({"op": "query", "name": "get_rigs"})) {
                Ok(r) => r,
                Err(e) => return tool_err(format!("{e:#}")),
            };
            if reply.get("ok").and_then(Value::as_bool) != Some(true) {
                return tool_err(
                    reply.get("error").and_then(Value::as_str).unwrap_or("bridge query failed"),
                );
            }
            return ok_json(reply.get("data").cloned().unwrap_or(Value::Null));
        }
        let s = match st.rigs.lock() {
            Ok(s) => s,
            Err(_) => return tool_err("rig snapshot lock poisoned"),
        };
        match serde_json::to_value(&*s) {
            Ok(v) => ok_json(v),
            Err(e) => tool_err(format!("serializing rig snapshot: {e}")),
        }
    }

    /// List installed CLAP and LV2 plugins.
    #[tool(name = "list_plugins", description = "List installed CLAP and LV2 plugins (name, id/uri, features) available to host.")]
    fn list_plugins(&self) -> Result<CallToolResult, McpError> {
        let clap = crate::host::list_clap_plugins();
        let lv2 = crate::lv2::discover();
        let clap_json: Vec<_> = clap
            .iter()
            .map(|p| json!({"id": p.id, "name": p.name, "vendor": p.vendor, "features": p.features}))
            .collect();
        let lv2_json: Vec<_> = lv2
            .iter()
            .take(60)
            .map(|p| json!({"uri": p.uri, "name": p.name, "num_ports": p.num_ports, "is_instrument": p.is_instrument}))
            .collect();
        ok_json(json!({"clap": clap_json, "lv2_count": lv2.len(), "lv2": lv2_json}))
    }

    /// Press a key.
    #[tool(name = "note_on", description = "Send a note-on (Command::NoteOn) to every instrument wired from the keys node that accepts the source. note 0..=127, velocity 0..1 (default 0.8), source default 'keys'.")]
    fn note_on(&self, Parameters(args): Parameters<NoteOnArgs>) -> Result<CallToolResult, McpError> {
        if args.note > 127 {
            return tool_err("note must be 0..=127");
        }
        let source = match parse_source(args.source.as_deref()) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let velocity = args.velocity.unwrap_or(0.8).clamp(0.0, 1.0);
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::NoteOn { note: args.note, velocity, source }) {
            return tool_err(e);
        }
        ok_json(json!({"note": args.note, "velocity": velocity, "source": format!("{source:?}")}))
    }

    /// Release a key.
    #[tool(name = "note_off", description = "Send a note-off (Command::NoteOff). note 0..=127, source default 'keys' (same syntax as note_on).")]
    fn note_off(&self, Parameters(args): Parameters<NoteOffArgs>) -> Result<CallToolResult, McpError> {
        if args.note > 127 {
            return tool_err("note must be 0..=127");
        }
        let source = match parse_source(args.source.as_deref()) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::NoteOff { note: args.note, source }) {
            return tool_err(e);
        }
        ok_json(json!({"note": args.note, "source": format!("{source:?}")}))
    }

    /// Start the transport.
    #[tool(name = "transport_play", description = "Start the transport (Command::TransportPlay).")]
    fn transport_play(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::TransportPlay) {
            return tool_err(e);
        }
        st.playing = true;
        ok_json(json!({"playing": true}))
    }

    /// Stop the transport.
    #[tool(name = "transport_stop", description = "Stop the transport (Command::TransportStop).")]
    fn transport_stop(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::TransportStop) {
            return tool_err(e);
        }
        st.playing = false;
        ok_json(json!({"playing": false}))
    }

    /// Set the metronome tempo.
    #[tool(name = "set_bpm", description = "Set the metronome tempo in BPM (Command::SetBpm), 20..300. Start the metronome with transport_play.")]
    fn set_bpm(&self, Parameters(args): Parameters<SetBpmArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let bpm = args.bpm.clamp(20.0, 300.0);
        if let Err(e) = send_cmd(&mut st, Command::SetBpm { bpm }) {
            return tool_err(e);
        }
        ok_json(json!({"bpm": bpm}))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "kabelsalat",
    instructions = "Control plane for the kabelsalat live audio/MIDI graph — headless engine, \
or attached to the RUNNING GUI app (--attach) so you control what the user sees. \
Mutating tools issue control-plane commands; get_graph/get_status read the live patch. \
Demo patch nodes: 0=KEYS, 1=SURGE A, 2=DELAY A, 3=MASTER OUT, 4=SURGE B, 5=DELAY B. \
Play notes with note_on/note_off; wire cables with connect/disconnect. \
Load several independent rigs (each its own rack + MIDI device) with load_rigs; query them with get_rigs."
)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "kabelsalat",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Control plane for the kabelsalat live audio/MIDI graph — headless engine, \
or attached to the RUNNING GUI app (--attach) so you control what the user sees. \
Demo patch nodes: 0=KEYS, 1=SURGE A, 2=DELAY A, 3=MASTER OUT, 4=SURGE B, 5=DELAY B.",
            )
    }
}

// ---------------------------------------------------------------------------
// Audio: live cpal output, or a dummy render loop when unavailable
// ---------------------------------------------------------------------------

struct AudioHandles {
    /// CLAP main-thread halves — must stay on the thread that loaded them.
    mains: Vec<ClapVoiceMain>,
    stream: Option<cpal::Stream>,
    bus: CommandBus,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    live: bool,
}

/// Prefer the native PipeWire host (same policy as the app), fall back to
/// the default host.
fn preferred_host() -> cpal::Host {
    for id in cpal::available_hosts() {
        if id.name().to_lowercase().contains("pipewire") {
            if let Ok(h) = cpal::host_from_id(id) {
                return h;
            }
        }
    }
    cpal::default_host()
}

fn try_build_audio(rigs: Arc<Mutex<RigSnapshot>>) -> anyhow::Result<AudioHandles> {
    let host = preferred_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let cfg = device.default_output_config()?;
    let sample_rate = cfg.sample_rate() as f64;
    let ch = cfg.channels() as usize;

    // Two instrument voices — one per demo channel stack (nodes 1 and 4).
    // A failed load degrades to fewer voices, not to no audio.
    let mut voices = Vec::new();
    let mut mains = Vec::new();
    for node in [1u32, 4] {
        match ClapVoice::new(sample_rate) {
            Ok((voice, main)) => {
                voices.push((node, voice));
                mains.push(main);
            }
            Err(e) => eprintln!("mcp: voice for node {node} unavailable: {e:#}"),
        }
    }

    let mut config: cpal::StreamConfig = cfg.into();
    config.buffer_size = cpal::BufferSize::Fixed(512);
    eprintln!(
        "mcp audio: host={} device={} rate={} ch={} buffer=512 frames voices={}",
        host.id().name(),
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        sample_rate,
        ch,
        voices.len(),
    );

    let (bus, drain) = control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, voices);
    engine.set_rig_snapshot(rigs);
    let level = engine.level_handle();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            engine.render(data, ch);
        },
        |e| eprintln!("mcp audio stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok(AudioHandles { mains, stream: Some(stream), bus, level, sample_rate, live: true })
}

/// No audio device (or plugin failure): keep the engine rendering into a
/// dummy buffer at block pace so commands drain and levels stay live.
fn dummy_audio(rigs: Arc<Mutex<RigSnapshot>>) -> AudioHandles {
    let sample_rate = 48_000.0;
    let (bus, drain) = control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, vec![]);
    engine.set_rig_snapshot(rigs);
    let level = engine.level_handle();
    std::thread::Builder::new()
        .name("mcp-audio-dummy".into())
        .spawn(move || {
            let mut buf = vec![0.0f32; 512 * 2];
            loop {
                engine.render(&mut buf, 2);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("spawning dummy audio thread");
    AudioHandles { mains: vec![], stream: None, bus, level, sample_rate, live: false }
}

fn build_audio(rigs: Arc<Mutex<RigSnapshot>>) -> AudioHandles {
    match try_build_audio(rigs.clone()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mcp: live audio unavailable ({e:#}) — serving with dummy render loop");
            dummy_audio(rigs)
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// stdout is the MCP JSON-RPC channel. The CLAP host logs to stdout while
/// loading plugins, so route fd 1 → stderr for the whole process and serve
/// MCP on a private duplicate of the original stdout.
fn reroute_stdout() -> anyhow::Result<std::fs::File> {
    unsafe {
        let saved = libc::dup(1);
        if saved < 0 {
            anyhow::bail!("dup(stdout) failed: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            libc::close(saved);
            anyhow::bail!("dup2(stderr, stdout) failed: {}", std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_fd(saved))
    }
}

/// Serve MCP over stdio on the private stdout duplicate (shared by both
/// backends — the tool definitions are identical).
fn serve_stdio(server: McpServer, rpc_stdout: std::fs::File) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(rpc_stdout));
        let transport = (tokio::io::stdin(), writer);
        let service = server.serve(transport).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Headless mode: own engine + local model.
pub fn run() -> anyhow::Result<()> {
    let rpc_stdout = reroute_stdout()?;
    eprintln!("kabelsalat mcp: headless control-plane server (MCP over stdio)");
    let rigs = Arc::new(Mutex::new(RigSnapshot::default()));
    let audio = build_audio(rigs.clone());
    let state = Arc::new(Mutex::new(State {
        graph: Graph::demo_patch(),
        backend: Backend::Local(audio.bus),
        level: audio.level,
        sample_rate: audio.sample_rate,
        audio_live: audio.live,
        playing: false,
        rigs,
    }));
    let server = McpServer { state, tool_router: McpServer::tool_router() };
    let result = serve_stdio(server, rpc_stdout);

    // The stream and the CLAP main-thread handles stayed alive on this (the
    // creating) thread for the whole session; tear them down last.
    drop(audio.stream);
    drop(audio.mains);
    result
}

/// Attach mode (`kabelsalat mcp --attach`): no local engine — the SAME 20
/// tools proxy to the running GUI app through the control bridge. Fails fast
/// with a clear message when the app isn't running.
pub fn run_attach() -> anyhow::Result<()> {
    let rpc_stdout = reroute_stdout()?;
    eprintln!("kabelsalat mcp: attaching to the running GUI app (control bridge)");
    let mut state = State {
        graph: Graph::default(),
        backend: Backend::Remote,
        level: Arc::new(AtomicU32::new(0)),
        sample_rate: 0.0,
        audio_live: true,
        playing: false,
        rigs: Arc::new(Mutex::new(RigSnapshot::default())),
    };
    // Connect + pull the live graph before serving; a missing socket surfaces
    // here as an actionable error ("start the app"), not mid-tool-call.
    refresh_remote(&mut state).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Ok(reply) = bridge::request(&json!({"op": "query", "name": "get_status"})) {
        if let Some(sr) = reply.pointer("/data/sample_rate").and_then(Value::as_f64) {
            state.sample_rate = sr;
        }
    }
    let server = McpServer {
        state: Arc::new(Mutex::new(state)),
        tool_router: McpServer::tool_router(),
    };
    serve_stdio(server, rpc_stdout)
}

// ---------------------------------------------------------------------------
// Tests — headless load_rigs → get_rigs round trip through the real engine
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A rig graph in the agent-friendly shape (string names, no counters).
    fn rig_graph_json(synth_name: &str) -> Value {
        json!({
            "nodes": [
                {"id": 0, "kind": "Keys", "name": "KEYS"},
                {"id": 1, "kind": {"Clap": {"plugin_id": SURGE_ID}}, "name": synth_name},
                {"id": 2, "kind": "MasterOut", "name": "MASTER"}
            ],
            "edges": [
                {"id": 1, "from": {"node": 0, "port": 0}, "to": {"node": 1, "port": 0}, "kind": "Midi"},
                {"id": 2, "from": {"node": 1, "port": 0}, "to": {"node": 2, "port": 0}, "kind": "Audio"}
            ]
        })
    }

    /// Unwrap a successful tool result's JSON payload.
    fn tool_json(res: &Result<CallToolResult, McpError>) -> Value {
        let r = res.as_ref().expect("tool result");
        let v = serde_json::to_value(r).unwrap();
        assert_ne!(v.get("isError").and_then(Value::as_bool), Some(true), "tool error: {v}");
        let text = v.pointer("/content/0/text").and_then(Value::as_str).expect("text content");
        serde_json::from_str(text).unwrap()
    }

    /// Local-backend server with a real (voice-less) engine the test drains
    /// by hand — no audio device or CLAP needed.
    fn local_server() -> (McpServer, GraphEngine) {
        let (bus, drain) = control::command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, 48000.0, vec![]);
        let rigs = Arc::new(Mutex::new(RigSnapshot::default()));
        engine.set_rig_snapshot(rigs.clone());
        let state = State {
            graph: Graph::demo_patch(),
            backend: Backend::Local(bus),
            level: engine.level_handle(),
            sample_rate: 48000.0,
            audio_live: false,
            playing: false,
            rigs,
        };
        let server = McpServer {
            state: Arc::new(Mutex::new(state)),
            tool_router: McpServer::tool_router(),
        };
        (server, engine)
    }

    #[test]
    fn load_rigs_get_rigs_round_trip_headless() {
        let (server, mut engine) = local_server();

        // Single-graph mode: empty rig list.
        let empty = tool_json(&server.get_rigs());
        assert_eq!(empty["rigs"].as_array().unwrap().len(), 0);
        assert_eq!(empty["active"], json!(0));

        let args = LoadRigsArgs {
            rigs: vec![
                RigArg { name: "A".into(), graph: rig_graph_json("SURGE A"), midi_device: Some(2) },
                RigArg { name: "B".into(), graph: rig_graph_json("SURGE B"), midi_device: None },
            ],
            active: Some(1),
        };
        let loaded = tool_json(&server.load_rigs(Parameters(args)));
        assert_eq!(loaded["loaded"], json!(2));
        assert_eq!(loaded["active"], json!(1));
        assert_eq!(loaded["nodes"], json!(6));
        assert_eq!(loaded["edges"], json!(4));

        // Wipe the tool's immediate write: only the engine's block-boundary
        // publish may refill the snapshot.
        *server.state.lock().unwrap().rigs.lock().unwrap() = RigSnapshot::default();
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2); // drains LoadRigs → publishes

        let got = tool_json(&server.get_rigs());
        assert_eq!(got["active"], json!(1));
        assert_eq!(got["rigs"][0]["name"], json!("A"));
        assert_eq!(got["rigs"][0]["midi_device"], json!(2));
        assert_eq!(got["rigs"][1]["name"], json!("B"));
        assert_eq!(got["rigs"][1]["midi_device"], json!(Value::Null));

        // The model mirror is the merged graph: both masters, offset ids.
        let st = server.state.lock().unwrap();
        let masters: Vec<NodeId> = st.graph.nodes.iter()
            .filter(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).collect();
        assert_eq!(masters, vec![2, 5]);
        drop(st);

        // get_status reports the rig count + active index.
        let status = tool_json(&server.get_status());
        assert_eq!(status["rigs"], json!(2));
        assert_eq!(status["active_rig"], json!(1));
    }

    #[test]
    fn load_rigs_rejects_bad_args() {
        let (server, _engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        // Active index out of range.
        assert!(is_err(server.load_rigs(Parameters(LoadRigsArgs {
            rigs: vec![RigArg { name: "A".into(), graph: rig_graph_json("S"), midi_device: None }],
            active: Some(5),
        }))));
        // Empty rig list.
        assert!(is_err(server.load_rigs(Parameters(LoadRigsArgs { rigs: vec![], active: None }))));
        // Dangling cable endpoint.
        let mut bad = rig_graph_json("S");
        bad["edges"][0]["to"]["node"] = json!(99);
        assert!(is_err(server.load_rigs(Parameters(LoadRigsArgs {
            rigs: vec![RigArg { name: "A".into(), graph: bad, midi_device: None }],
            active: None,
        }))));
        // Malformed graph (not an object).
        assert!(is_err(server.load_rigs(Parameters(LoadRigsArgs {
            rigs: vec![RigArg { name: "A".into(), graph: json!(42), midi_device: None }],
            active: None,
        }))));
    }
}