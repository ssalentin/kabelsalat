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
use crate::engine::{GraphEngine, GraphPub};
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
    /// Engine publication handle (Local: live transport/count-in/BPM status).
    graph_pub: Option<Arc<Mutex<GraphPub>>>,
    count_in_enabled: bool,
    count_in_bars: u8,
    /// Headless CC binding mirror keyed by (source, dest).
    cc_maps: std::collections::HashMap<(SourceId, NodeId), Vec<crate::matrix::CcMapping>>,
    /// Recorder status (headless): the shared handle the engine's tap writes.
    record: Arc<crate::record::RecorderShared>,
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
    /// For kind "dsp": gain, pan, eq3, filter, limiter, saturation, delay, reverb, metronome.
    dsp: Option<String>,
    /// For kind "clap": CLAP plugin id (defaults to Surge XT).
    plugin_id: Option<String>,
    /// For kind "lv2": the LV2 plugin URI (see list_plugins).
    uri: Option<String>,
    /// Multi-rig: which rig (0-based, see get_rigs) the node joins. Default 0.
    rig: Option<u8>,
}

/// Arguments for `add_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct AddRigArgs {
    /// Optional rig name (default "Rig A/B/C…").
    name: Option<String>,
}

/// Arguments for `delete_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteRigArgs {
    /// Rig index (0-based, see get_rigs).
    rig: usize,
    /// Must be true: deleting removes the rig's units, cables and synth state.
    confirm: bool,
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
    /// Accept notes from the computer keyboard (on-screen keys and laptop
    /// typing are one source). Default true.
    #[serde(alias = "keys")]
    keyboard: Option<bool>,
    /// Accept notes from hardware MIDI inputs. Default true.
    hardware: Option<bool>,
    /// Pre-unification laptop flag: folded into `keyboard` (keys || laptop).
    #[schemars(skip)]
    #[serde(default)]
    laptop: Option<bool>,
}

impl SetRigSourceArgs {
    /// The (keyboard, hardware) pair; a legacy `laptop` flag ORs into keyboard.
    fn pair(&self) -> (bool, bool) {
        let keyboard = match (self.keyboard, self.laptop) {
            (None, None) => true,
            (k, l) => k.unwrap_or(false) || l.unwrap_or(false),
        };
        (keyboard, self.hardware.unwrap_or(true))
    }
}

/// Arguments for `set_node_playing`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetNodePlayingArgs {
    /// Transport-capable node id (a metronome; see get_graph "transport").
    node: NodeId,
    /// true = start this instrument, false = stop it.
    playing: bool,
}

/// Arguments for `list_sounds`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListSoundsArgs {
    /// Words matched against "Category/Name", e.g. "flute" or "pad warm".
    query: String,
}

/// Arguments for `load_sound`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSoundArgs {
    /// Patch query (words or "Category/Name" from list_sounds).
    query: String,
    /// Rig index (default: the rig in view).
    rig: Option<usize>,
}

/// Arguments for `set_split`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSplitArgs {
    /// Keys node id to split (see get_graph, kind keys). Omit = the first
    /// Keys node in the patch.
    node: Option<NodeId>,
    /// MIDI note 0..=127 to split at: notes below leave on the Keys node's
    /// port 0 (LOW), notes at/above on port 1 (HIGH). Omit (null) = unsplit.
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

/// Arguments for `set_count_in`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetCountInArgs {
    /// Enable or disable count-in before master play (defaults to true when `bars >= 1`, or toggles if omitted).
    enabled: Option<bool>,
    /// Number of count-in bars (1..=4; 0 disables count-in).
    bars: Option<u8>,
}

/// Arguments for `set_rig_device`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRigDeviceArgs {
    /// Instrument node id (1 = Surge A, 4 = Surge B).
    node: NodeId,
    /// Hardware device index, or null for any device.
    device: Option<u32>,
}

/// Arguments for `set_matrix_rule`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetMatrixRuleArgs {
    /// Destination instrument node id.
    node: NodeId,
    /// Source: "keys" (the computer keyboard: on-screen + laptop) or
    /// "hw:<device index>".
    source: String,
    /// MIDI channel filter 0..=15; omit for omni (any channel).
    channel: Option<u8>,
    /// Semitones added to notes (-24..=24, clamped; default 0).
    transpose: Option<i8>,
    /// Inclusive played-key range low (default 0).
    key_lo: Option<u8>,
    /// Inclusive played-key range high (default 127).
    key_hi: Option<u8>,
    /// Inclusive velocity range low for sounding note-ons (default 0).
    vel_lo: Option<u8>,
    /// Inclusive velocity range high (default 127).
    vel_hi: Option<u8>,
    /// Mute/unmute this route (default true).
    enabled: Option<bool>,
}

/// Arguments for `set_cc_map`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetCcMapArgs {
    /// Destination node id.
    node: NodeId,
    /// Source: "keys" or "hw:<device index>".
    source: String,
    /// MIDI CC number (0..=127).
    cc: u8,
    /// Parameter id on the destination node; omit/null to remove the binding.
    param: Option<u32>,
}

/// Arguments for `clear_cc_map`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ClearCcMapArgs {
    /// Destination node id.
    node: NodeId,
    /// Source: "keys" or "hw:<device index>".
    source: String,
    /// Optional CC number (0..=127); omit to clear all bindings on this rule.
    cc: Option<u8>,
}

/// Arguments for `list_cc_maps`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListCcMapsArgs {
    /// Optional destination node filter.
    node: Option<NodeId>,
    /// Optional source filter ("keys" or "hw:<device index>").
    source: Option<String>,
}

/// Arguments for `list_params`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListParamsArgs {
    /// Node id to inspect.
    node: NodeId,
    /// Optional case-insensitive filter words.
    query: Option<String>,
}

/// Arguments for `learn_cc`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LearnCcArgs {
    /// Destination node id.
    node: NodeId,
    /// Source: "keys" or "hw:<device index>".
    source: String,
    /// Parameter id on the destination node to bind to the next hardware CC.
    param: u32,
    /// Set true to cancel any armed MIDI-learn instead of arming.
    cancel: Option<bool>,
}

/// Arguments for `set_rig_mix`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRigMixArgs {
    /// Rig index (0-based, as in get_rigs).
    rig: u8,
    /// Volume 0..1 (omit to keep).
    vol: Option<f32>,
    /// Pan -1 (L) .. 1 (R) (omit to keep).
    pan: Option<f32>,
    /// Mute (omit to keep).
    mute: Option<bool>,
    /// Solo — mutes every non-solo rig (omit to keep).
    solo: Option<bool>,
}

/// Arguments for `note_on`.
#[derive(Debug, Deserialize, JsonSchema)]
struct NoteOnArgs {
    /// MIDI note number (0..=127).
    note: u8,
    /// Velocity 0.0..=1.0 (default 0.8).
    velocity: Option<f32>,
    /// Source: "keys" (default; the computer keyboard), "hardware:<port>",
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
    /// (the computer keyboard, every hardware device).
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

/// Current rig metadata (local snapshot, or the live app's over the bridge).
fn rig_meta(st: &State) -> Result<Vec<crate::control::RigMeta>, String> {
    if matches!(st.backend, Backend::Remote) {
        let reply = bridge::request(&json!({"op": "query", "name": "get_rigs"})).map_err(|e| format!("{e:#}"))?;
        let data = reply.get("data").cloned().unwrap_or(Value::Null);
        let snap: crate::control::RigSnapshot = serde_json::from_value(data).map_err(|e| format!("get_rigs: {e}"))?;
        return Ok(snap.rigs);
    }
    Ok(st.rigs.lock().map(|s| s.rigs.clone()).unwrap_or_default())
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
        // Legacy spelling: laptop typing IS the keyboard source now.
        "laptop" | "laptop_keys" | "keyboard" => Ok(SourceId::Keys),
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
            // `transport`: the node has its own Play/Stop (set_node_playing);
            // `playing`: its current state (always false without transport).
            json!({
                "id": n.id,
                "name": name_str(&n.name),
                "kind": kind,
                "transport": n.kind.supports_transport(),
                "playing": n.playing,
                // Keys only: split point (null = unsplit, one MIDI OUT on
                // port 0); when set, port 0 = LOW (< point), port 1 = HIGH.
                "split": n.split,
            })
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

impl McpServer {
    /// Shared add/delete rig path: split the live patch per rig, edit the
    /// list, send ONE LoadRigs (the UI adopts it via GraphPub sync).
    fn edit_rigs(&self, delete: Option<usize>, name: Option<String>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let meta = match rig_meta(&st) { Ok(m) => m, Err(e) => return tool_err(e) };
        let (graphs, _) = control::split_rigs(&st.graph);
        let mut rigs: Vec<Rig> = graphs.into_iter().enumerate().map(|(i, g)| {
            let m = meta.get(i);
            let name = m.map(|m| m.name.clone()).filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Rig {}", (b'A' + i.min(25) as u8) as char));
            Rig::new(name, g, m.and_then(|m| m.midi_device))
        }).collect();
        let active = match delete {
            Some(r) => {
                if rigs.len() <= 1 { return tool_err("the last rig cannot be deleted"); }
                if r >= rigs.len() { return tool_err(format!("no rig {r}: there are {}", rigs.len())); }
                rigs.remove(r);
                0
            }
            None => {
                if rigs.len() >= 8 { return tool_err("at most 8 rigs"); }
                let used: Vec<String> = rigs.iter().map(|r| r.name.clone()).collect();
                let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
                    (b'A'..=b'Z').map(|c| format!("Rig {}", c as char)).find(|n| !used.contains(n)).unwrap_or_else(|| "Rig".into())
                });
                let claps = st.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Clap { .. })).count();
                rigs.push(Rig::new(name, control::standard_rig_graph(claps < control::VOICE_POOL), None));
                rigs.len() - 1
            }
        };
        if let Err(e) = send_cmd(&mut st, Command::LoadRigs { rigs: rigs.clone(), active }) {
            return tool_err(e);
        }
        st.graph = merge_rigs(&rigs);
        let metas: Vec<crate::control::RigMeta> = rigs.iter()
            .map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
        if matches!(st.backend, Backend::Local(_)) {
            if let Ok(mut s) = st.rigs.lock() { s.rigs = metas.clone(); s.active = active; }
        }
        ok_json(json!({"rigs": metas, "active": active}))
    }
}

#[tool_router]
impl McpServer {
    /// The current patch: all nodes and cables.
    #[tool(name = "get_graph", description = "Query the full patch graph: nodes (id, name, kind, transport, playing) and edges/cables (id, from/to port, midi|audio). transport=true marks nodes with their own Play/Stop (metronome); playing is their current state.")]
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
        let tp = st
            .graph_pub
            .as_ref()
            .and_then(|p| p.lock().ok())
            .map(|g| g.transport);
        let (playing, counting_in, count_in_beat, count_in_enabled, count_in_bars, bpm) = match tp {
            Some(t) => (t.playing, t.counting_in, t.count_in_beat, t.count_in_enabled, t.count_in_bars, t.bpm),
            None => (st.playing, false, 0, st.count_in_enabled, st.count_in_bars, 96.0),
        };
        ok_json(json!({
            "nodes": st.graph.nodes.len(),
            "edges": st.graph.edges.len(),
            "sample_rate": st.sample_rate,
            "audio_live": st.audio_live,
            "output_level": level,
            "transport_playing": playing,
            "counting_in": counting_in,
            "count_in_beat": count_in_beat,
            "count_in_enabled": count_in_enabled,
            "count_in_bars": count_in_bars,
            "bpm": bpm,
            "buffer_frames": bridge::BUFFER_FRAMES,
            "buffer_latency_ms": bridge::BUFFER_FRAMES as f64 / st.sample_rate * 1000.0,
            "xruns": bridge::XRUNS.load(Ordering::Relaxed),
            "midi_latency_ms": bridge::MIDI_LATENCY_US.load(Ordering::Relaxed) as f64 / 1000.0,
            "rigs": rig_count,
            "active_rig": active_rig,
        }))
    }

    /// Add a node to the patch.
    #[tool(name = "add_node", description = "Add a node (Command::AddNode). kind: keys|clap|dsp|lv2|system_audio|master_out; dsp nodes need 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb|metronome); lv2 nodes need 'uri'; clap nodes take an optional plugin_id (default Surge XT). Added dsp/lv2 nodes sound immediately (processors are built); wire them with connect. Clap voices are main-thread-only, so new clap nodes sound via load_graph's voice remap, not add_node.")]
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
        let rig = args.rig.unwrap_or(0);
        if let Err(e) = send_cmd(&mut st, Command::AddNode { kind: kind.clone(), name, rig }) {
            return tool_err(e);
        }
        let id = st.graph.add_node(kind, name);
        st.graph.set_rig(id, rig);
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
    #[tool(name = "set_rig_source", description = "Select which source classes feed an instrument node (Command::SetRigSource): keyboard (the computer keyboard — on-screen keys and laptop typing are ONE source) and hardware (MIDI devices). Omitted flags default to true. The MIDI cable still gates routing; this filters by source on top.")]
    fn set_rig_source(&self, Parameters(args): Parameters<SetRigSourceArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        let (keyboard, hardware) = args.pair();
        if let Err(e) = send_cmd(&mut st, Command::SetRigSource { node: args.node, keyboard, hardware }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "keyboard": keyboard, "hardware": hardware}))
    }

    /// Split one Keys node into LOW/HIGH outputs.
    #[tool(name = "set_split", description = "Split a Keys node's keyboard (Command::SetNodeSplit) — a property of the Keys node, not a global setting. point is a MIDI note 0..=127: played notes below it leave on the Keys node's port 0 (LOW), notes at/above on port 1 (HIGH). point null/omitted = unsplit (one MIDI OUT, port 0; HIGH-port cables are removed). node defaults to the first Keys node. Routing is plain wiring: connect LOW (from_port 0) and HIGH (from_port 1) to rigs with connect. Held notes are released. Example: {point: 60} splits at C4; then connect {from_node: 0, from_port: 1, to_node: 4, kind: midi}.")]
    fn set_split(&self, Parameters(args): Parameters<SetSplitArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = args.point {
            if p > 127 {
                return tool_err("point must be 0..=127");
            }
        }
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let node = match args.node {
            Some(id) => match st.graph.node(id) {
                None => return tool_err(format!("node {id} does not exist")),
                Some(n) if !matches!(n.kind, NodeKind::Keys) => {
                    return tool_err(format!("node {id} ({}) is not a Keys node — only keyboards split", name_str(&n.name)))
                }
                Some(_) => id,
            },
            None => match st.graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::Keys)) {
                Some(n) => n.id,
                None => return tool_err("the patch has no Keys node"),
            },
        };
        if let Err(e) = send_cmd(&mut st, Command::SetNodeSplit { node, point: args.point }) {
            return tool_err(e);
        }
        st.graph.set_split(node, args.point);
        ok_json(json!({"node": node, "point": args.point, "split": args.point.is_some()}))
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

    fn parse_rule_source(s: &str) -> Result<SourceId, String> {
        match s {
            // "laptop" is the legacy spelling of the one keyboard source.
            "keys" | "keyboard" | "laptop" => Ok(SourceId::Keys),
            s if s.starts_with("hw:") => s[3..]
                .parse::<u32>()
                .map(SourceId::Hardware)
                .map_err(|_| format!("bad source '{s}'")),
            s => Err(format!("unknown source '{s}' (keys|hw:<d>)")),
        }
    }

    fn ui_bridge_tool(tool: &str, args: Value) -> Result<CallToolResult, McpError> {
        match bridge::request(&json!({"op": "ui", "tool": tool, "args": args})) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(true) => {
                let data = reply.get("data").cloned().unwrap_or(Value::Null);
                if let Some(s) = data.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        return ok_json(parsed);
                    }
                }
                ok_json(data)
            }
            Ok(reply) => tool_err(
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("ui tool failed")
                    .to_string(),
            ),
            Err(e) => tool_err(format!("{tool} needs the running app (attach): {e:#}")),
        }
    }

    /// Edit one matrix rule live (the hot-wire matrix's per-cell shaping).
    #[tool(name = "set_matrix_rule", description = "Edit ONE MIDI routing rule live (Command::MatrixRule): source ('keys' = the computer keyboard, on-screen + laptop; 'hw:<device index>') → instrument node. Optional channel filter (0..15, omit = omni), transpose (-24..24 semitones), key_lo/key_hi (inclusive played-note split zone), vel_lo/vel_hi (velocity layer), enabled (mute). The engine releases held notes on the destination when the rule changes. Example: {node:1, source:'keys', key_lo:0, key_hi:59, transpose:-12} = bass zone an octave down.")]
    fn set_matrix_rule(&self, Parameters(args): Parameters<SetMatrixRuleArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        let source = match Self::parse_rule_source(&args.source) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let cmd = Command::MatrixRule {
            source,
            dest: args.node,
            channel: args.channel.map(|c| c.min(15)),
            transpose: args.transpose.unwrap_or(0).clamp(-24, 24),
            key_lo: args.key_lo.unwrap_or(0).min(127),
            key_hi: args.key_hi.unwrap_or(127).min(127),
            vel_lo: args.vel_lo.unwrap_or(0).min(127),
            vel_hi: args.vel_hi.unwrap_or(127).min(127),
            enabled: args.enabled.unwrap_or(true),
        };
        if let Err(e) = send_cmd(&mut st, cmd) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "source": args.source}))
    }

    /// Bind or unbind one CC on a matrix rule.
    #[tool(name = "set_cc_map", description = "Bind ONE MIDI CC (0..=127) on source ('keys' | 'hw:<device index>') → dest node to a parameter id (Command::MatrixCcMap). Omit param to unbind that CC. A non-empty map acts as a whitelist for that rule.")]
    fn set_cc_map(&self, Parameters(args): Parameters<SetCcMapArgs>) -> Result<CallToolResult, McpError> {
        if args.cc > 127 {
            return tool_err("cc must be 0..=127");
        }
        let source = match Self::parse_rule_source(&args.source) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::MatrixCcMap { source, dest: args.node, cc: args.cc, param: args.param }) {
            return tool_err(e);
        }
        let entry = st.cc_maps.entry((source, args.node)).or_default();
        match args.param {
            Some(p) => {
                if let Some(m) = entry.iter_mut().find(|m| m.cc == args.cc) {
                    m.param = p;
                } else {
                    entry.push(crate::matrix::CcMapping { cc: args.cc, param: p });
                }
            }
            None => entry.retain(|m| m.cc != args.cc),
        }
        let map = st.cc_maps.get(&(source, args.node)).cloned().unwrap_or_default();
        if map.is_empty() {
            st.cc_maps.remove(&(source, args.node));
        }
        ok_json(json!({"node": args.node, "source": args.source, "cc": args.cc, "param": args.param, "cc_map": map}))
    }

    /// Clear one or all CC bindings on a matrix rule.
    #[tool(name = "clear_cc_map", description = "Clear one CC binding (when cc is provided) or all CC bindings (when cc is omitted) on source → node.")]
    fn clear_cc_map(&self, Parameters(args): Parameters<ClearCcMapArgs>) -> Result<CallToolResult, McpError> {
        if let Some(cc) = args.cc {
            if cc > 127 {
                return tool_err("cc must be 0..=127");
            }
        }
        let source = match Self::parse_rule_source(&args.source) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote && args.cc.is_none() {
            return Self::ui_bridge_tool("clear_cc_map", json!({"node": args.node, "source": args.source}));
        }
        let mut st = self.state.lock().unwrap();
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("node {} does not exist", args.node));
        }
        let ccs: Vec<u8> = match args.cc {
            Some(c) => vec![c],
            None => st.cc_maps.get(&(source, args.node)).map(|v| v.iter().map(|m| m.cc).collect()).unwrap_or_default(),
        };
        for cc in ccs {
            if let Err(e) = send_cmd(&mut st, Command::MatrixCcMap { source, dest: args.node, cc, param: None }) {
                return tool_err(e);
            }
            if let Some(v) = st.cc_maps.get_mut(&(source, args.node)) {
                v.retain(|m| m.cc != cc);
            }
        }
        if st.cc_maps.get(&(source, args.node)).is_some_and(|v| v.is_empty()) {
            st.cc_maps.remove(&(source, args.node));
        }
        let map = st.cc_maps.get(&(source, args.node)).cloned().unwrap_or_default();
        ok_json(json!({"node": args.node, "source": args.source, "cc_map": map}))
    }

    /// List active CC bindings.
    #[tool(name = "list_cc_maps", description = "List active MIDI CC → parameter bindings (plus armed MIDI-learn state when attached to the GUI app).")]
    fn list_cc_maps(&self, Parameters(args): Parameters<ListCcMapsArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool("list_cc_maps", json!({"node": args.node, "source": args.source}));
        }
        let filter_src = match args.source.as_deref() {
            Some(s) => match Self::parse_rule_source(s) {
                Ok(src) => Some(src),
                Err(e) => return tool_err(e),
            },
            None => None,
        };
        let st = self.state.lock().unwrap();
        let entries: Vec<Value> = st.cc_maps.iter()
            .filter(|((src, node), v)| {
                !v.is_empty()
                    && args.node.map_or(true, |n| n == *node)
                    && filter_src.map_or(true, |s| s == *src)
            })
            .map(|((src, node), v)| json!({
                "source": match src {
                    SourceId::Keys => "keys".to_string(),
                    SourceId::Hardware(d) => format!("hw:{d}"),
                    other => format!("{other:?}"),
                },
                "node": node,
                "bindings": v,
            }))
            .collect();
        ok_json(json!({"cc_maps": entries}))
    }

    /// List bindable parameter ids and names for a node.
    #[tool(name = "list_params", description = "List parameter ids and names for a node (for set_cc_map / learn_cc). CLAP plugin params require --attach to the running GUI app.")]
    fn list_params(&self, Parameters(args): Parameters<ListParamsArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool("list_params", json!({"node": args.node, "query": args.query}));
        }
        let st = self.state.lock().unwrap();
        let Some(n) = st.graph.node(args.node) else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let choices: Vec<(u32, String)> = match &n.kind {
            NodeKind::Dsp(dk) => crate::engine::dsp_param_choices(dk)
                .iter()
                .map(|&(id, name)| (id, name.to_string()))
                .collect(),
            NodeKind::Lv2 { uri } => crate::lv2::control_descs_for_uri(uri.as_str())
                .into_iter()
                .map(|c| (c.index as u32, c.name))
                .collect(),
            NodeKind::MasterOut => vec![(3, "Volume".into()), (4, "Warmth".into())],
            NodeKind::Clap { .. } => {
                return tool_err("CLAP parameter listing needs the running app (kabelsalat mcp --attach)");
            }
            NodeKind::Keys | NodeKind::SystemAudio => Vec::new(),
        };
        let q = args.query.unwrap_or_default().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let total = choices.len();
        let params: Vec<Value> = choices
            .into_iter()
            .filter(|(_, name)| {
                let low = name.to_lowercase();
                words.iter().all(|w| low.contains(w))
            })
            .take(50)
            .map(|(id, name)| json!({"id": id, "name": name}))
            .collect();
        ok_json(json!({"node": args.node, "total": total, "params": params}))
    }

    /// Arm MIDI-learn in the running GUI app.
    #[tool(name = "learn_cc", description = "Arm MIDI-learn in the running GUI app for (source, node, param) so the next incoming hardware CC binds to that parameter. Pass cancel=true to disarm. Needs --attach.")]
    fn learn_cc(&self, Parameters(args): Parameters<LearnCcArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool(
            "learn_cc",
            json!({"node": args.node, "source": args.source, "param": args.param, "cancel": args.cancel}),
        )
    }

    /// Per-rig mixer (the overview strip's vol/pan/mute/solo).
    #[tool(name = "set_rig_mix", description = "Per-rig mixer (Command::SetRigMix): rig index (0-based, see get_rigs), vol 0..1, pan -1..1, mute, solo. Omitted fields keep their value. Solo mutes every non-solo rig. Applied at the rig's master when summing to the output; resets on graph loads.")]
    fn set_rig_mix(&self, Parameters(args): Parameters<SetRigMixArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::SetRigMix { rig: args.rig, vol: args.vol, pan: args.pan, mute: args.mute, solo: args.solo }) {
            return tool_err(e);
        }
        ok_json(json!({"rig": args.rig, "vol": args.vol, "pan": args.pan, "mute": args.mute, "solo": args.solo}))
    }

    /// Load several independent rigs at once.
    #[tool(name = "load_rigs", description = "Load multiple independent rigs (Command::LoadRigs): each rig is a full rack graph with its own MasterOut, bound to a hardware MIDI device (midi_device null = the keyboard + all devices). Graphs use the patch shape: nodes [{id, kind, name?}] and edges [{id, from:{node,port}, to:{node,port}, kind:'midi'|'audio'}]; kinds are 'Keys', 'MasterOut', 'SystemAudio', {Clap:{plugin_id}}, {Dsp:'delay'|'reverb'|...}, {Lv2:{uri}}; names/plugin ids may be plain strings. The 2 CLAP voices are remapped to Clap nodes in node-id order across all rigs (surplus Clap nodes are silent — use LV2 instruments for more). Returns {loaded, active, nodes, edges}. Query the result with get_rigs.")]
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

    /// Add a standard rig to the running setup.
    #[tool(name = "add_rig", description = "Add a new rig (an independent rack with its own Master): Keys -> Surge -> Master, wired and playable (a bare Keys + Master when every synth voice is taken). Existing rigs keep their graphs and device bindings (knob/synth state is kept by the in-app agent path only). A single-graph patch becomes rig 0. Returns {rigs, active} like get_rigs.")]
    fn add_rig(&self, Parameters(args): Parameters<AddRigArgs>) -> Result<CallToolResult, McpError> {
        self.edit_rigs(None, args.name)
    }

    /// Delete one rig.
    #[tool(name = "delete_rig", description = "Delete rig `rig` (index from get_rigs) with everything in it. DESTRUCTIVE: requires confirm=true; the last rig cannot be deleted. Returns {rigs, active}.")]
    fn delete_rig(&self, Parameters(args): Parameters<DeleteRigArgs>) -> Result<CallToolResult, McpError> {
        if !args.confirm {
            return tool_err("not deleted: pass confirm=true (ask the user first)");
        }
        self.edit_rigs(Some(args.rig), None)
    }

    /// Query the multi-rig state.
    /// Search installed Surge patches.
    #[tool(name = "list_sounds", description = "Search the installed Surge patches (factory + 3rd-party, ~3000) by words in 'Category/Name', e.g. 'flute', 'pad warm'. Returns up to 25 entries.")]
    fn list_sounds(&self, Parameters(args): Parameters<ListSoundsArgs>) -> Result<CallToolResult, McpError> {
        let hits = crate::host::search_surge_patches(&crate::host::list_surge_patches(), &args.query, 25);
        ok_json(json!({"patches": hits}))
    }

    /// Load a Surge patch into a rig's synth (needs the running app).
    #[tool(name = "load_sound", description = "Load a Surge patch into a rig's synth so it sounds like that instrument (a new rig plays the init patch otherwise). query: words or 'Category/Name' from list_sounds; rig: index (default: the rig in view). Needs the running app (kabelsalat mcp --attach): the synths live on its UI thread.")]
    fn load_sound(&self, Parameters(args): Parameters<LoadSoundArgs>) -> Result<CallToolResult, McpError> {
        let mut a = json!({"query": args.query});
        if let Some(r) = args.rig { a["rig"] = json!(r); }
        match bridge::request(&json!({"op": "ui", "tool": "load_sound", "args": a})) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(true) =>
                ok_json(reply.get("data").cloned().unwrap_or(Value::Null)),
            Ok(reply) => tool_err(reply.get("error").and_then(Value::as_str).unwrap_or("load_sound failed").to_string()),
            Err(e) => tool_err(format!("load_sound needs the running app (attach): {e:#}")),
        }
    }

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
    #[tool(name = "transport_play", description = "Master transport play (Command::TransportPlay): starts EVERY transport-capable node (all metronomes) and runs the song clock hosted CLAP plugins follow, from beat 0 (after the count-in bar if count-in is enabled). Use set_node_playing to start/stop one instrument.")]
    fn transport_play(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::TransportPlay) {
            return tool_err(e);
        }
        st.playing = !st.count_in_enabled;
        st.graph.set_all_playing(true);
        ok_json(json!({
            "playing": st.playing,
            "counting_in": st.count_in_enabled,
            "count_in_enabled": st.count_in_enabled,
            "count_in_bars": st.count_in_bars,
        }))
    }

    /// Stop the transport.
    #[tool(name = "transport_stop", description = "Master transport stop (Command::TransportStop): stops EVERY transport-capable node (cancelling any active count-in) and halts the song clock CLAP plugins follow.")]
    fn transport_stop(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::TransportStop) {
            return tool_err(e);
        }
        st.playing = false;
        st.graph.set_all_playing(false);
        ok_json(json!({"playing": false, "counting_in": false}))
    }

    /// Per-instrument transport: start/stop one node.
    #[tool(name = "set_node_playing", description = "Start or stop ONE transport-capable rack instrument (Command::SetNodePlaying) — e.g. a metronome — independently of the master transport. Only nodes with transport=true in get_graph accept it. Returns {node, playing}.")]
    fn set_node_playing(&self, Parameters(args): Parameters<SetNodePlayingArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        match st.graph.node(args.node) {
            None => return tool_err(format!("node {} does not exist", args.node)),
            Some(n) if !n.kind.supports_transport() => {
                return tool_err(format!("node {} ({}) has no transport (only metronomes start/stop)", args.node, name_str(&n.name)))
            }
            Some(_) => {}
        }
        if let Err(e) = send_cmd(&mut st, Command::SetNodePlaying { node: args.node, playing: args.playing }) {
            return tool_err(e);
        }
        st.graph.set_playing(args.node, args.playing);
        ok_json(json!({"node": args.node, "playing": args.playing}))
    }

    /// Set the metronome tempo.
    #[tool(name = "set_bpm", description = "Set the tempo in BPM (Command::SetBpm), 20..300: the metronome and the tempo-synced features of hosted CLAP plugins follow it. Start the metronome with transport_play.")]
    fn set_bpm(&self, Parameters(args): Parameters<SetBpmArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let bpm = args.bpm.clamp(20.0, 300.0);
        if let Err(e) = send_cmd(&mut st, Command::SetBpm { bpm }) {
            return tool_err(e);
        }
        ok_json(json!({"bpm": bpm}))
    }

    /// Configure master transport count-in.
    #[tool(name = "set_count_in", description = "Configure master transport count-in (Command::SetCountIn): plays N bars (1..=4, default 1) of metronome clicks before the song clock starts at beat 0. Pass enabled (bool) and/or bars (0 disables, 1..=4 enables).")]
    fn set_count_in(&self, Parameters(args): Parameters<SetCountInArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            if let Ok(reply) = bridge::request(&json!({"op": "query", "name": "get_status"})) {
                if let Some(en) = reply.pointer("/data/count_in_enabled").and_then(Value::as_bool) {
                    st.count_in_enabled = en;
                }
                if let Some(b) = reply.pointer("/data/count_in_bars").and_then(Value::as_u64) {
                    st.count_in_bars = b.clamp(1, crate::transport::MAX_COUNT_IN_BARS as u64) as u8;
                }
            }
        }
        let (enabled, bars) = match args.bars {
            Some(0) => (false, st.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS)),
            Some(b) => (
                args.enabled.unwrap_or(true),
                b.clamp(1, crate::transport::MAX_COUNT_IN_BARS),
            ),
            None => (
                args.enabled.unwrap_or(!st.count_in_enabled),
                st.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS),
            ),
        };
        if let Err(e) = send_cmd(&mut st, Command::SetCountIn { enabled, bars }) {
            return tool_err(e);
        }
        st.count_in_enabled = enabled;
        st.count_in_bars = bars;
        let _ = crate::assistant::save_count_in(enabled, bars);
        ok_json(json!({"enabled": enabled, "bars": bars}))
    }

    /// Start recording the master output.
    #[tool(name = "record_start", description = "Start recording the final master output (what the user hears) to a new timestamped 32-bit-float WAV under ~/Music/kabelsalat/ (Command::RecordStart). Idempotent: a second start while recording is a no-op.")]
    fn record_start(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::RecordStart) {
            return tool_err(e);
        }
        ok_json(json!({"recording": true}))
    }

    /// Stop recording.
    #[tool(name = "record_stop", description = "Stop recording (Command::RecordStop): the writer drains the ring and finalizes the WAV header. Idempotent: a stop while idle is a no-op.")]
    fn record_stop(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::RecordStop) {
            return tool_err(e);
        }
        ok_json(json!({"recording": false}))
    }

    /// Recording status: path, elapsed seconds, dropped frames.
    #[tool(name = "record_status", description = "Get recording status: recording (bool), path (the last/finalized WAV), seconds (elapsed), dropped_frames (ring overruns), finalized.")]
    fn record_status(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            let reply = match bridge::request(&json!({"op": "query", "name": "get_record"})) {
                Ok(r) => r,
                Err(e) => return tool_err(format!("{e:#}")),
            };
            if reply.get("ok").and_then(Value::as_bool) != Some(true) {
                return tool_err(reply.get("error").and_then(Value::as_str).unwrap_or("bridge query failed"));
            }
            return ok_json(reply.get("data").cloned().unwrap_or(Value::Null));
        }
        ok_json(serde_json::to_value(st.record.snapshot()).unwrap_or(json!({})))
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
    graph_pub: Arc<Mutex<GraphPub>>,
    sample_rate: f64,
    live: bool,
    record: Arc<crate::record::RecorderShared>,
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
    engine.spawn_lv2_loader();
    engine.set_rig_snapshot(rigs);
    let graph_pub = Arc::new(Mutex::new(GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    let (recorder_tap, record) = crate::record::start(crate::record::record_dir(), sample_rate as u32, 2);
    engine.set_recorder(recorder_tap);
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
    Ok(AudioHandles { mains, stream: Some(stream), bus, level, graph_pub, sample_rate, live: true, record })
}

/// No audio device (or plugin failure): keep the engine rendering into a
/// dummy buffer at block pace so commands drain and levels stay live.
fn dummy_audio(rigs: Arc<Mutex<RigSnapshot>>) -> AudioHandles {
    let sample_rate = 48_000.0;
    let (bus, drain) = control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, vec![]);
    engine.set_rig_snapshot(rigs);
    let graph_pub = Arc::new(Mutex::new(GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    let (recorder_tap, record) = crate::record::start(crate::record::record_dir(), sample_rate as u32, 2);
    engine.set_recorder(recorder_tap);
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
    AudioHandles { mains: vec![], stream: None, bus, level, graph_pub, sample_rate, live: false, record }
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
    let mut audio = build_audio(rigs.clone());
    let (ci_enabled, ci_bars) = crate::assistant::load_count_in();
    if ci_enabled || ci_bars != 1 {
        let _ = audio.bus.send(Command::SetCountIn { enabled: ci_enabled, bars: ci_bars });
    }
    let state = Arc::new(Mutex::new(State {
        graph: Graph::demo_patch(),
        backend: Backend::Local(audio.bus),
        level: audio.level,
        sample_rate: audio.sample_rate,
        audio_live: audio.live,
        playing: false,
        rigs,
        graph_pub: Some(audio.graph_pub),
        count_in_enabled: ci_enabled,
        count_in_bars: ci_bars,
        cc_maps: std::collections::HashMap::new(),
        record: audio.record,
    }));
    let server = McpServer { state, tool_router: McpServer::tool_router() };
    let result = serve_stdio(server, rpc_stdout);

    // The stream and the CLAP main-thread handles stayed alive on this (the
    // creating) thread for the whole session; tear them down last.
    drop(audio.stream);
    drop(audio.mains);
    result
}

/// Attach mode (`kabelsalat mcp --attach`): no local engine — the SAME
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
        graph_pub: None,
        count_in_enabled: false,
        count_in_bars: 1,
        cc_maps: std::collections::HashMap::new(),
        record: Arc::new(crate::record::RecorderShared::new()),
    };
    // Connect + pull the live graph before serving; a missing socket surfaces
    // here as an actionable error ("start the app"), not mid-tool-call.
    refresh_remote(&mut state).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Ok(reply) = bridge::request(&json!({"op": "query", "name": "get_status"})) {
        if let Some(sr) = reply.pointer("/data/sample_rate").and_then(Value::as_f64) {
            state.sample_rate = sr;
        }
        if let Some(en) = reply.pointer("/data/count_in_enabled").and_then(Value::as_bool) {
            state.count_in_enabled = en;
        }
        if let Some(bars) = reply.pointer("/data/count_in_bars").and_then(Value::as_u64) {
            state.count_in_bars = bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS as u64) as u8;
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
        let graph_pub = Arc::new(Mutex::new(GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
        engine.set_graph_pub(graph_pub.clone());
        let state = State {
            graph: Graph::demo_patch(),
            backend: Backend::Local(bus),
            level: engine.level_handle(),
            sample_rate: 48000.0,
            audio_live: false,
            playing: false,
            rigs,
            graph_pub: Some(graph_pub),
            count_in_enabled: false,
            count_in_bars: 1,
            cc_maps: std::collections::HashMap::new(),
            record: Arc::new(crate::record::RecorderShared::new()),
        };
        let server = McpServer {
            state: Arc::new(Mutex::new(state)),
            tool_router: McpServer::tool_router(),
        };
        (server, engine)
    }

    #[test]
    fn set_rig_source_takes_keyboard_hardware_pair() {
        let (server, _engine) = local_server();
        let args = |v: Value| -> SetRigSourceArgs { serde_json::from_value(v).unwrap() };
        let out = tool_json(&server.set_rig_source(Parameters(args(json!({"node": 1, "keyboard": false})))));
        assert_eq!(out, json!({"node": 1, "keyboard": false, "hardware": true}));
        // Legacy triple: keyboard = keys || laptop.
        let out = tool_json(&server.set_rig_source(Parameters(args(json!({"node": 1, "keys": false, "laptop": true, "hardware": false})))));
        assert_eq!(out, json!({"node": 1, "keyboard": true, "hardware": false}));
        assert_eq!(args(json!({"node": 1, "keys": false, "laptop": false})).pair(), (false, true));
        // The schema advertises the pair only (no laptop flag).
        let schema = serde_json::to_value(schemars::schema_for!(SetRigSourceArgs)).unwrap();
        let mut props: Vec<&str> = schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        props.sort_unstable();
        assert_eq!(props, ["hardware", "keyboard", "node"], "{schema}");
        // "laptop" is a legacy spelling of the one keyboard source.
        assert_eq!(parse_source(Some("laptop")), Ok(SourceId::Keys));
        assert_eq!(parse_source(Some("keys")), Ok(SourceId::Keys));
    }

    #[test]
    fn add_and_delete_rig_through_the_engine_headless() {
        let (server, mut engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        let mut buf = vec![0.0f32; 64 * 2];
        // Single-graph demo (2 Surge stacks) → becomes rig 0; rig 1 is new.
        let added = tool_json(&server.add_rig(Parameters(AddRigArgs { name: Some("Bass".into()) })));
        assert_eq!(added["rigs"].as_array().unwrap().len(), 2);
        assert_eq!(added["rigs"][1]["name"], json!("Bass"));
        assert_eq!(added["active"], json!(1));
        engine.render(&mut buf, 64);
        let g = engine.graph();
        let rig1: Vec<_> = g.nodes.iter().filter(|n| n.rig == 1).collect();
        assert_eq!(rig1.len(), 3, "Keys + Master + Surge");
        assert!(rig1.iter().any(|n| matches!(n.kind, NodeKind::Clap { .. })));
        assert_eq!(g.edges.iter().filter(|e| g.node(e.from.node).map(|n| n.rig) == Some(1)).count(), 3);
        // Delete needs confirm, then removes rig 0 and keeps "Bass".
        assert!(is_err(server.delete_rig(Parameters(DeleteRigArgs { rig: 0, confirm: false }))));
        let left = tool_json(&server.delete_rig(Parameters(DeleteRigArgs { rig: 0, confirm: true })));
        assert_eq!(left["rigs"].as_array().unwrap().len(), 1);
        assert_eq!(left["rigs"][0]["name"], json!("Bass"));
        engine.render(&mut buf, 64);
        assert_eq!(engine.graph().nodes.len(), 3);
        assert!(is_err(server.delete_rig(Parameters(DeleteRigArgs { rig: 0, confirm: true }))), "last rig stays");
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
    fn set_node_playing_tool_drives_engine_and_graph() {
        let (server, mut engine) = local_server();
        // Add a metronome through the real tool path (model + engine).
        let added = tool_json(&server.add_node(Parameters(AddNodeArgs {
            kind: "dsp".into(),
            name: Some("METRO".into()),
            dsp: Some("metronome".into()),
            plugin_id: None,
            uri: None,
            rig: None,
        })));
        let id = added["id"].as_u64().unwrap() as NodeId;
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        let node = |v: &Value| v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(id)).cloned().unwrap();
        let g = tool_json(&server.get_graph());
        assert_eq!(node(&g)["transport"], json!(true));
        assert_eq!(node(&g)["playing"], json!(false));

        let r = tool_json(&server.set_node_playing(Parameters(SetNodePlayingArgs { node: id, playing: true })));
        assert_eq!(r["playing"], json!(true));
        engine.render(&mut buf, 2);
        assert!(engine.graph().node(id).unwrap().playing, "engine received SetNodePlaying");
        assert_eq!(node(&tool_json(&server.get_graph()))["playing"], json!(true));

        // Master stop clears it in the model mirror too.
        tool_json(&server.transport_stop());
        engine.render(&mut buf, 2);
        assert!(!engine.graph().node(id).unwrap().playing);
        assert_eq!(node(&tool_json(&server.get_graph()))["playing"], json!(false));

        // Non-transport and unknown nodes are rejected (nothing sent).
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        assert!(is_err(server.set_node_playing(Parameters(SetNodePlayingArgs { node: 2, playing: true }))));
        assert!(is_err(server.set_node_playing(Parameters(SetNodePlayingArgs { node: 999, playing: true }))));
    }

    #[test]
    fn set_split_tool_targets_the_keys_node() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let r = tool_json(&server.set_split(Parameters(SetSplitArgs { node: None, point: Some(60) })));
        assert_eq!(r["node"], json!(0), "defaults to the first Keys node");
        assert_eq!(r["split"], json!(true));
        engine.render(&mut buf, 2);
        assert_eq!(engine.graph().node(0).unwrap().split, Some(60), "engine received SetNodeSplit");
        let keys = |v: &Value| v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(0)).cloned().unwrap();
        assert_eq!(keys(&tool_json(&server.get_graph()))["split"], json!(60), "get_graph exposes split");
        tool_json(&server.set_split(Parameters(SetSplitArgs { node: Some(0), point: None })));
        engine.render(&mut buf, 2);
        assert_eq!(engine.graph().node(0).unwrap().split, None);
        assert_eq!(keys(&tool_json(&server.get_graph()))["split"], Value::Null);
        // Non-Keys / unknown nodes and bad points are rejected.
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: Some(1), point: Some(60) }))));
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: Some(999), point: Some(60) }))));
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: None, point: Some(200) }))));
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

    #[test]
    fn cc_map_mcp_tools_drive_engine_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let res = tool_json(&server.set_cc_map(Parameters(SetCcMapArgs {
            node: 1,
            source: "keys".into(),
            cc: 74,
            param: Some(42),
        })));
        assert_eq!(res["cc"], json!(74));
        assert_eq!(res["param"], json!(42));
        let listed = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: None, source: None })));
        assert_eq!(listed["cc_maps"][0]["bindings"][0]["cc"], json!(74));
        let dsp = tool_json(&server.list_params(Parameters(ListParamsArgs { node: 2, query: Some("mix".into()) })));
        assert_eq!(dsp["params"][0]["name"], json!("Mix"));
        engine.render(&mut buf, 2);
        let cleared = tool_json(&server.clear_cc_map(Parameters(ClearCcMapArgs {
            node: 1,
            source: "keys".into(),
            cc: None,
        })));
        assert_eq!(cleared["cc_map"].as_array().unwrap().len(), 0);
        engine.render(&mut buf, 2);
        let empty = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: None, source: None })));
        assert_eq!(empty["cc_maps"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn set_count_in_and_status_drive_engine_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 480 * 2];

        let cfg = tool_json(&server.set_count_in(Parameters(SetCountInArgs { enabled: Some(true), bars: Some(1) })));
        assert_eq!(cfg, json!({"enabled": true, "bars": 1}));
        engine.render(&mut buf, 2);
        let st = tool_json(&server.get_status());
        assert_eq!(st["count_in_enabled"], json!(true));
        assert_eq!(st["count_in_bars"], json!(1));
        assert_eq!(st["counting_in"], json!(false));

        let played = tool_json(&server.transport_play());
        assert_eq!(played["counting_in"], json!(true));
        engine.render(&mut buf, 2);
        let st2 = tool_json(&server.get_status());
        assert_eq!(st2["counting_in"], json!(true));
        assert_eq!(st2["transport_playing"], json!(false));
        assert_eq!(st2["count_in_beat"], json!(1));

        let stopped = tool_json(&server.transport_stop());
        assert_eq!(stopped["counting_in"], json!(false));
        engine.render(&mut buf, 2);
        let st3 = tool_json(&server.get_status());
        assert_eq!(st3["counting_in"], json!(false));
        assert_eq!(st3["transport_playing"], json!(false));
    }
}