//! Headless MCP server — `kabelsalat mcp` (PLAN §2c, task-14).
//!
//! A sibling frontend to the one typed control plane (`control.rs`), giving
//! agents 1:1 access to the graph engine without any GUI. Architecture:
//!
//! - This process owns a local [`Graph`] model (started from `demo_patch`)
//!   AND a running [`GraphEngine`] fed by the same lock-free command bus the
//!   UI uses. Mutating tools validate + update the local model and send the
//!   matching [`Command`] to the engine; query tools read the local model
//!   plus engine status (output peak via `level_handle`). No shared state
//!   ever touches the audio thread.
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

use crate::control::{
    self, name_bytes, name_str, Command, CommandBus, DspKind, EdgeId, Graph, NodeId, NodeKind,
    PortKind, PortRef,
};
use crate::engine::GraphEngine;
use crate::host::{ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;

const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

// ---------------------------------------------------------------------------
// Shared state: local model + bus + engine status (never the engine itself)
// ---------------------------------------------------------------------------

struct State {
    graph: Graph,
    bus: CommandBus,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    audio_live: bool,
    playing: bool,
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
    if st.bus.send(cmd) {
        Ok(())
    } else {
        Err("command bus full — try again".into())
    }
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

// ---------------------------------------------------------------------------
// Tools — 1:1 with the control plane
// ---------------------------------------------------------------------------

#[tool_router]
impl McpServer {
    /// The current patch: all nodes and cables.
    #[tool(name = "get_graph", description = "Query the full patch graph: nodes (id, name, kind) and edges/cables (id, from/to port, midi|audio).")]
    fn get_graph(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        ok_json(graph_json(&st.graph))
    }

    /// Engine status snapshot.
    #[tool(name = "get_status", description = "Query engine status: node/edge counts, sample rate, whether live audio output is running, current output peak level (0..1) and transport state.")]
    fn get_status(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        let level = f32::from_bits(st.level.load(Ordering::Relaxed));
        ok_json(json!({
            "nodes": st.graph.nodes.len(),
            "edges": st.graph.edges.len(),
            "sample_rate": st.sample_rate,
            "audio_live": st.audio_live,
            "output_level": level,
            "transport_playing": st.playing,
        }))
    }

    /// Add a node to the patch.
    #[tool(name = "add_node", description = "Add a node (Command::AddNode). kind: keys|clap|dsp|system_audio|master_out; dsp nodes need 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb); clap nodes take an optional plugin_id (default Surge XT). Note: voices/processors for dynamically added nodes are a follow-up — the node joins the graph, but new clap/dsp nodes do not sound yet.")]
    fn add_node(&self, Parameters(args): Parameters<AddNodeArgs>) -> Result<CallToolResult, McpError> {
        let kind = match parse_node_kind(&args) {
            Ok(k) => k,
            Err(e) => return tool_err(e),
        };
        let label = args.name.clone().unwrap_or_else(|| match &kind {
            NodeKind::Keys => "KEYS".into(),
            NodeKind::Clap { .. } => "CLAP".into(),
            NodeKind::Dsp(k) => dsp_kind_str(k).to_ascii_uppercase(),
            NodeKind::SystemAudio => "SYSTEM AUDIO".into(),
            NodeKind::MasterOut => "MASTER OUT".into(),
        });
        let name = name_bytes(&label);
        let mut st = self.state.lock().unwrap();
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
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::SetRigSource { node: args.node, keys: args.keys, laptop: args.laptop, hardware: args.hardware }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "keys": args.keys, "laptop": args.laptop, "hardware": args.hardware}))
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
}

#[tool_handler(
    router = self.tool_router,
    name = "kabelsalat",
    instructions = "Headless control plane for the kabelsalat live audio/MIDI graph. \
Mutating tools mirror control-plane commands into the running engine; get_graph/get_status \
read the local model. Start from the demo patch: 0=KEYS, 1=SURGE A, 2=DELAY A, 3=MASTER OUT, \
4=SURGE B, 5=DELAY B. Play notes with note_on/note_off; wire cables with connect/disconnect."
)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "kabelsalat",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Headless control plane for the kabelsalat live audio/MIDI graph. \
Mutating tools mirror control-plane commands into the running engine; get_graph/get_status \
read the local model. Demo patch nodes: 0=KEYS, 1=SURGE A, 2=DELAY A, 3=MASTER OUT, \
4=SURGE B, 5=DELAY B.",
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

fn try_build_audio() -> anyhow::Result<AudioHandles> {
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
fn dummy_audio() -> AudioHandles {
    let sample_rate = 48_000.0;
    let (bus, drain) = control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, vec![]);
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

fn build_audio() -> AudioHandles {
    match try_build_audio() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mcp: live audio unavailable ({e:#}) — serving with dummy render loop");
            dummy_audio()
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() -> anyhow::Result<()> {
    // stdout is the MCP JSON-RPC channel. The CLAP host logs to stdout while
    // loading plugins, so route fd 1 → stderr for the whole process and
    // serve MCP on a private duplicate of the original stdout.
    let rpc_stdout = unsafe {
        let saved = libc::dup(1);
        if saved < 0 {
            anyhow::bail!("dup(stdout) failed: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            libc::close(saved);
            anyhow::bail!("dup2(stderr, stdout) failed: {}", std::io::Error::last_os_error());
        }
        std::fs::File::from_raw_fd(saved)
    };

    eprintln!("kabelsalat mcp: headless control-plane server (MCP over stdio)");
    let audio = build_audio();
    let state = Arc::new(Mutex::new(State {
        graph: Graph::demo_patch(),
        bus: audio.bus,
        level: audio.level,
        sample_rate: audio.sample_rate,
        audio_live: audio.live,
        playing: false,
    }));
    let server = McpServer { state, tool_router: McpServer::tool_router() };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(async move {
        let writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(rpc_stdout));
        let transport = (tokio::io::stdin(), writer);
        let service = server.serve(transport).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });
    drop(rt);

    // The stream and the CLAP main-thread handles stayed alive on this (the
    // creating) thread for the whole session; tear them down last.
    drop(audio.stream);
    drop(audio.mains);
    result
}