//! MCP server — `kabelsalat mcp` (docs/ARCHITECTURE.md §2c, task-14) and `kabelsalat mcp
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
//!   thread. Headless mode hosts no further voices (the app hosts one per
//!   CLAP unit): tools name Clap nodes without one (`note_silent_claps`). If audio init fails, a dummy render loop keeps draining the
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
    NodeKind, PortKind, PortRef, Rig, RigSnapshot, SendSource, SinkName,
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
    /// Render time per node (Local: the headless engine's own table).
    node_times: Option<Arc<crate::engine_info::NodeTimes>>,
    count_in_enabled: bool,
    count_in_bars: u8,
    /// Headless CC binding mirror keyed by (source, dest).
    cc_maps: std::collections::HashMap<(SourceId, NodeId), Vec<crate::matrix::CcMapping>>,
    /// Headless: the instrument profile picked per hardware device index
    /// (the app keeps its own, per port name, in config.json).
    device_instruments: std::collections::BTreeMap<u32, String>,
    /// Headless: the CC bindings those profiles hold in `cc_maps`.
    instrument_bound: Vec<crate::instruments::Bound>,
    /// Recorder status (headless): the shared handle the engine's tap writes.
    record: Arc<crate::record::RecorderShared>,
    /// "Sync to transport" mirror (the engine is authoritative via SetRecordSync).
    record_sync: bool,
    /// Single-rig preset directory (isolated in unit tests).
    rig_presets_dir: std::path::PathBuf,
    /// Each rig's instrument-type mark by rig index (#198, headless
    /// mirror of the app's); `None` = the rig follows its instrument.
    rig_types: Vec<Option<crate::keyboard::KeyboardType>>,
    /// Headless setlist mirror (#204); in Remote mode the GUI app owns it.
    setlist: crate::setlist::Setlist,
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
    /// Node kind: "keys", "clap", "dsp", "audio_in", "system_audio" or "master_out".
    kind: String,
    /// Display name (max 31 bytes; defaults to the kind).
    name: Option<String>,
    /// For kind "dsp": gain, pan, eq3, filter, limiter, saturation, delay, reverb, metronome.
    dsp: Option<String>,
    /// For kind "clap": CLAP plugin id (defaults to Surge XT).
    plugin_id: Option<String>,
    /// For kind "lv2": the LV2 plugin URI (see list_plugins).
    uri: Option<String>,
    /// For kind "audio_in": the PipeWire source (node name from
    /// list_audio_devices; null/omitted = the default input).
    device: Option<String>,
    /// For kind "audio_in": which channel(s) feed the OUT: "in_1"|"in_2"|"stereo".
    channels: Option<String>,
    /// Multi-rig: which rig (0-based, see get_rigs) the node joins. Default 0.
    rig: Option<u8>,
    /// For kind "midi_fx" with neither `uri` nor `plugin_id`: the built-in
    /// transposer's semitones (-24..24, default 0).
    semis: Option<i8>,
}

/// Arguments for `add_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct AddRigArgs {
    /// Optional rig name (default "Rig A/B/C…").
    name: Option<String>,
    /// Optional CLAP plugin id (from list_plugins): the rig is Keys → that
    /// plugin → Master instead of the default Surge rig.
    plugin_id: Option<String>,
}

/// Arguments for `clone_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct CloneRigArgs {
    /// Source rig index (0-based, see get_rigs). Defaults to the active rig.
    rig: Option<usize>,
    /// Optional name for the cloned rig (up to 24 characters; default "<source> Copy").
    name: Option<String>,
}

/// Arguments for `rename_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct RenameRigArgs {
    /// Rig index (0-based, see get_rigs).
    rig: usize,
    /// New name (up to 24 characters).
    name: String,
}

/// Arguments for `delete_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteRigArgs {
    /// Rig index (0-based, see get_rigs).
    rig: usize,
    /// Must be true: deleting removes the rig's units, cables and synth state.
    confirm: bool,
}

/// Arguments for `save_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SaveRigArgs {
    /// Preset name (no slashes).
    name: String,
    /// Rig index (0-based, see get_rigs). Defaults to the active rig.
    rig: Option<usize>,
}

/// Arguments for `load_rig`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadRigArgs {
    /// Saved rig preset name (see list_rig_presets).
    name: String,
    /// Optional rig index to replace; omit to load as a new rig.
    rig: Option<usize>,
}

/// Arguments for `install_instrument`.
#[derive(Debug, Deserialize, JsonSchema)]
struct InstallInstrumentArgs {
    /// Catalog instrument id from list_instruments, e.g. setbfree, surge-xt, odin2.
    id: String,
    /// Must be true: installing changes the user's system.
    confirm: bool,
}

/// Arguments for `load_template`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadTemplateArgs {
    /// Template name: cathedral_organ | jazz_organ | rock_organ | ambient_pad | synth_lead | guitar_rig | demo | multi_rig.
    name: String,
}

/// Every starter template id, for the "use …" hints.
fn template_ids() -> String {
    crate::session::Template::ALL.iter().map(|t| t.id()).collect::<Vec<_>>().join(", ")
}

fn setlist_json(sl: &crate::setlist::Setlist) -> Value {
    let songs: Vec<Value> = sl.songs.iter().enumerate().map(|(si, s)| {
        let parts: Vec<Value> = s.parts.iter().enumerate().map(|(pi, p)| {
            let rigs: Vec<Value> = p.rigs.iter().map(|r| json!({
                "rig": r.rig,
                "key_lo": r.key_lo,
                "key_hi": r.key_hi,
                "transpose": r.transpose,
                "level": r.level,
            })).collect();
            json!({ "index": pi, "name": p.name, "rigs": rigs, "params_count": p.params.len() })
        }).collect();
        json!({
            "index": si,
            "name": s.name,
            "bpm": s.bpm,
            "time_signature": s.time_sig.map(|t| t.to_json()),
            "parts": parts,
        })
    }).collect();
    let active = sl.current().map(|(s, p, pos)| json!({
        "song": pos.song,
        "part": pos.part,
        "song_name": s.name,
        "part_name": p.name,
    }));
    let fs = |f: Option<crate::setlist::FootSwitch>| f.map(|x| json!({"cc": x.cc, "channel": x.channel}));
    json!({
        "songs": songs,
        "active": active,
        "foot_switch": { "next_cc": fs(sl.next_cc), "prev_cc": fs(sl.prev_cc) },
    })
}

fn apply_setlist_song_headless(st: &mut State, args: &SetSetlistSongArgs) -> Result<(), String> {
    use crate::setlist::{Part, PartRig, Song};
    let parse_sig = |v: &Value| -> Result<crate::transport::TimeSig, String> {
        let num = v.get("numerator").and_then(Value::as_u64).ok_or("time_signature.numerator required")?;
        let den = v.get("denominator").and_then(Value::as_u64).ok_or("time_signature.denominator required")?;
        crate::transport::TimeSig::checked(num, den)
    };
    let parse_parts = |ps: &[SetlistPartArg]| -> Result<Vec<Part>, String> {
        if ps.is_empty() { return Err("a song needs at least one part".into()); }
        let mut out = Vec::with_capacity(ps.len());
        for p in ps {
            let name = p.name.trim();
            if name.is_empty() { return Err("part name cannot be empty".into()); }
            let mut part = Part::new(name, Vec::new());
            if let Some(rigs) = &p.rigs {
                for r in rigs {
                    if (r.rig as usize) >= crate::setlist::MAX_RIGS {
                        return Err(format!("rig {} is out of range (0..{})", r.rig, crate::setlist::MAX_RIGS));
                    }
                    part.set_rig(PartRig {
                        rig: r.rig,
                        key_lo: r.key_lo.unwrap_or(0).min(127),
                        key_hi: r.key_hi.unwrap_or(127).min(127),
                        transpose: r.transpose.unwrap_or(0).clamp(-48, 48),
                        level: r.level.unwrap_or(1.0).clamp(0.0, 1.0),
                    });
                }
            }
            out.push(part);
        }
        Ok(out)
    };
    if let Some(idx) = args.song {
        if idx >= st.setlist.songs.len() {
            return Err(format!("no song at index {idx} (setlist has {})", st.setlist.songs.len()));
        }
        if args.remove == Some(true) {
            st.setlist.remove_song(idx);
            return Ok(());
        }
        if let Some(name) = &args.name {
            let t = name.trim();
            if t.is_empty() { return Err("song name cannot be empty".into()); }
            st.setlist.songs[idx].name = t.to_string();
        }
        if let Some(bpm_opt) = args.bpm {
            if let Some(b) = bpm_opt {
                if !b.is_finite() || !(20.0..=300.0).contains(&b) { return Err("bpm must be 20..300".into()); }
            }
            st.setlist.songs[idx].bpm = bpm_opt;
        }
        if let Some(sig_opt) = &args.time_signature {
            st.setlist.songs[idx].time_sig = match sig_opt {
                Some(v) => Some(parse_sig(v)?),
                None => None,
            };
        }
        if let Some(ps) = &args.parts {
            st.setlist.songs[idx].parts = parse_parts(ps)?;
        }
        if let Some(to) = args.move_to {
            if to >= st.setlist.songs.len() {
                return Err(format!("move_to {to} is out of range"));
            }
            st.setlist.move_song(idx, to);
        }
        st.setlist.clamp_active();
        Ok(())
    } else {
        let name = args.name.as_deref().map(str::trim).filter(|s| !s.is_empty())
            .ok_or("name is required when creating a song")?;
        let parts = match &args.parts {
            Some(ps) => parse_parts(ps)?,
            None => vec![Part::new("Main", vec![PartRig::new(0)])],
        };
        let mut song = Song::new(name, parts);
        if let Some(Some(b)) = args.bpm {
            if !b.is_finite() || !(20.0..=300.0).contains(&b) { return Err("bpm must be 20..300".into()); }
            song.bpm = Some(b);
        }
        if let Some(Some(v)) = &args.time_signature {
            song.time_sig = Some(parse_sig(v)?);
        }
        st.setlist.songs.push(song);
        Ok(())
    }
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
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct SetParamArgs {
    /// Target node id.
    node: NodeId,
    /// Parameter id from `list_params` (optional when `name` is provided).
    #[serde(default)]
    param: Option<u32>,
    /// Parameter name, main control name, face knob label, or musical alias
    /// (e.g. "cutoff", "time", "pingpong", "8'", "leslie").
    #[serde(default, alias = "knob")]
    name: Option<String>,
    /// Optional CLAP module filter when matching by `name` (requires `--attach` for CLAP).
    #[serde(default)]
    module: Option<String>,
    /// New value in plain/native units (`min..max` from `list_params`, or 0..1 for built-in DSP).
    #[serde(default)]
    value: Option<f64>,
    /// New value normalized `0..1` across `min..max` (alternative to `value`/`text`).
    #[serde(default)]
    normalized: Option<f64>,
    /// Musical text value (e.g. "300 ms", "1.2 kHz", "+4 dB", "Ping-Pong", "FAST", "6/8", "40%").
    #[serde(default)]
    text: Option<String>,
    /// On CLAP nodes: `"plugin"` (default, internal synth/FX parameter) or
    /// `"knob"` (rack LEVEL knob).
    #[serde(default)]
    target: Option<String>,
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
    /// Words matched against the sounds' names, e.g. "flute", "pad warm", "jazz".
    query: String,
    /// Rig index whose instrument is searched (default: the rig in view; headless: 0).
    rig: Option<usize>,
    /// Instrument node id, for a rig with several different instruments.
    node: Option<NodeId>,
}

/// Arguments for `load_sound`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSoundArgs {
    /// Sound query (words, or an entry of list_sounds).
    query: String,
    /// Rig index (default: the rig in view; headless: 0).
    rig: Option<usize>,
    /// Instrument node id, for a rig with several different instruments.
    node: Option<NodeId>,
}

/// Arguments for `set_split`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSplitArgs {
    /// Keys node id to split (see get_graph, kind keys). Omit = the first
    /// Keys node in the patch.
    node: Option<NodeId>,
    /// MIDI note 0..=127 to split at: notes below leave on the Keys node's
    /// port 0 (LOW), notes at/above on port 1 (HIGH). Omit (null) with no
    /// octaves = unsplit; omit with octaves = keep the current split.
    point: Option<u8>,
    /// Octave shift of the LOW zone, -3..=3 (what it plays, not which keys).
    low_octave: Option<i8>,
    /// Octave shift of the HIGH zone, -3..=3.
    high_octave: Option<i8>,
}

/// Arguments for `set_scale`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetScaleArgs {
    /// Keys node id (see get_graph, kind keys). Omit = the first Keys node.
    node: Option<NodeId>,
    /// Key the scale starts on: "C", "F#", … or 0..=11. Default C.
    key: Option<String>,
    /// Scale: major, minor, dorian, mixolydian, harmonic minor, maj penta,
    /// min penta, blues. Default major.
    scale: Option<String>,
    /// true = switch snapping off (every key plays as pressed).
    off: Option<bool>,
}

/// Arguments for `set_arp`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetArpArgs {
    /// Arp node id (see get_graph, kind arp).
    node: NodeId,
    /// up, down, up-down, chord, random.
    mode: Option<String>,
    /// Note value synced to the song clock: 1/4, 1/8, 1/8T, 1/16, 1/16T, 1/32.
    rate: Option<String>,
    /// How many octaves the pattern spans, 1..=4.
    octaves: Option<u8>,
    /// How much of a step each note sounds, 0.05..=1.
    gate: Option<f64>,
    /// 0..=1: every second step starts late.
    swing: Option<f64>,
    /// Chord shape one held key becomes in chord mode: maj, min, sus2, sus4,
    /// 7, maj7, min7, power.
    chord: Option<String>,
}

/// Arguments for `set_system_audio`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSystemAudioArgs {
    /// Mix level 0..1 for system-audio capture (0 mutes). Headless mode has no
    /// capture source, so this only sets the gain.
    gain: f32,
}

/// Arguments for `set_audio_in`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetAudioInArgs {
    /// The AudioIn node id (from get_graph, kind "audio_in").
    node: NodeId,
    /// PipeWire source (node name from list_audio_devices). null/empty = the
    /// default input.
    device: Option<String>,
    /// Which input channel(s) feed the OUT: "in_1" (left / mic), "in_2"
    /// (right / Hi-Z guitar), or "stereo".
    channels: Option<String>,
    /// Input gain 0..1 (engine maps ×4 → 0..=4 linear).
    gain: Option<f64>,
    /// Low-latency monitoring on/off (adds the input, post-gain, to the master).
    monitor: Option<bool>,
}

/// Arguments for `set_audio_output`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetAudioOutputArgs {
    /// PipeWire sink name from list_audio_outputs; omit or empty = system default.
    output: Option<String>,
}

/// Arguments for `set_buffer_size`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetBufferSizeArgs {
    /// Output buffer size in frames: 64, 128, 256, 512, 1024, or 2048.
    frames: u32,
}

/// Arguments for `set_app_stream`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetAppStreamArgs {
    /// Stream index from list_app_streams.
    index: u64,
    /// Volume 0..1.5 (1.0 = 100 %).
    volume: Option<f32>,
    /// Mute on/off.
    muted: Option<bool>,
}

/// Arguments for `set_player`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetPlayerArgs {
    /// Mix level 0..1 for the file player (0 mutes).
    gain: Option<f32>,
    /// Loop: restart the file seamlessly at its end.
    #[serde(rename = "loop")]
    loop_on: Option<bool>,
    /// Sync: playback rate follows the transport tempo (varispeed).
    sync: Option<bool>,
    /// Number of Beats: loop length in beats (loop + sync derive the tempo).
    beats: Option<f32>,
    /// Beat Offset (+/-): start point in beats.
    offset: Option<f32>,
    /// The file's original BPM.
    bpm: Option<f32>,
}

/// Arguments for `play_file`.
#[derive(Debug, Deserialize, JsonSchema)]
struct PlayFileArgs {
    /// Absolute path to a WAV file (16/24-bit PCM or 32-bit float).
    path: String,
    /// Mix level 0..1 (default 0.8).
    gain: Option<f32>,
    /// Loop: restart the file seamlessly at its end.
    #[serde(rename = "loop")]
    loop_on: Option<bool>,
    /// Sync: playback rate follows the transport tempo (varispeed).
    sync: Option<bool>,
    /// Number of Beats: loop length in beats (loop + sync derive the tempo).
    beats: Option<f32>,
    /// Beat Offset (+/-): start point in beats.
    offset: Option<f32>,
    /// The file's original BPM (a "120 bpm" in the filename is adopted).
    bpm: Option<f32>,
}

/// Arguments for `set_bpm`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetBpmArgs {
    /// Metronome tempo in BPM (20..300).
    bpm: f32,
}

/// Arguments for `set_time_signature`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetTimeSignatureArgs {
    /// Beats per bar, 1..=12 (the 3 of 3/4).
    numerator: u8,
    /// The note value of one beat: 2, 4, 8 or 16 (the 4 of 3/4).
    denominator: u8,
}

/// Arguments for `play_demo`.
#[derive(Debug, Deserialize, JsonSchema)]
struct PlayDemoArgs {
    /// 0-based rig index (0 = first rig / "rig 1", 1 = second rig / "rig 2"; omit for the active rig).
    rig: Option<u8>,
    /// Optional phrase kind override: "keys", "bass", "lead", "pad", "organ", "drums", or "wind". Omit to choose from the rig's sound.
    phrase: Option<String>,
}

/// One rig entry inside a `set_setlist_song` part.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct SetlistPartRigArg {
    /// 0-based rig index.
    rig: u8,
    /// Lowest played MIDI note (0..=127, default 0).
    key_lo: Option<u8>,
    /// Highest played MIDI note (0..=127, default 127).
    key_hi: Option<u8>,
    /// Semitone transpose (-48..=48, default 0).
    transpose: Option<i8>,
    /// Rig mixer level (0.0..=1.0, default 1.0).
    level: Option<f32>,
}

/// One part inside `set_setlist_song`.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct SetlistPartArg {
    /// Part name (e.g. "Verse", "Chorus", "Solo").
    name: String,
    /// Live rigs in this part.
    rigs: Option<Vec<SetlistPartRigArg>>,
    /// Snapshot the current knob values of those rigs into this part.
    capture_knobs: Option<bool>,
}

/// Arguments for `set_setlist_song`.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct SetSetlistSongArgs {
    /// 0-based song index to update/move/remove; omit to append a new song.
    song: Option<usize>,
    /// Song title (required when creating).
    name: Option<String>,
    /// Optional song tempo in BPM (20..=300; null clears).
    bpm: Option<Option<f32>>,
    /// Optional time signature `{numerator, denominator}` (null clears).
    time_signature: Option<Option<Value>>,
    /// Ordered parts of the song.
    parts: Option<Vec<SetlistPartArg>>,
    /// Reorder `song` to this 0-based index.
    move_to: Option<usize>,
    /// Set true with `song` to delete that song.
    remove: Option<bool>,
}

/// Arguments for `select_setlist_part`.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct SelectSetlistPartArgs {
    /// Step action: "next_part" | "prev_part" | "next_song" | "prev_song" | "clear".
    action: Option<String>,
    /// 0-based song index to jump to (when `action` is omitted).
    song: Option<usize>,
    /// 0-based part index (defaults to 0 when `song` is given).
    part: Option<usize>,
}

/// Arguments for `add_song_from_session`.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct AddSongFromSessionArgs {
    /// Saved session name (from `list_sessions`).
    session: String,
    /// Optional song name (defaults to the session name).
    name: Option<String>,
}

/// Arguments for `set_setlist_footswitch`.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
struct SetSetlistFootswitchArgs {
    /// Which step to bind: "next" | "prev".
    action: String,
    /// MIDI CC number (0..=127), or null/omitted to clear.
    cc: Option<u8>,
    /// Optional MIDI channel (0..=15); omit for any channel.
    channel: Option<u8>,
}

/// Arguments for `set_count_in`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetCountInArgs {
    /// Enable or disable count-in before master play (defaults to true when `bars >= 1`, or toggles if omitted).
    enabled: Option<bool>,
    /// Number of count-in bars (1..=4; 0 disables count-in).
    bars: Option<u8>,
}

/// Arguments for `set_midi_clock`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetMidiClockArgs {
    /// Send MIDI beat clock and Start/Stop with the transport.
    enabled: Option<bool>,
    /// Sync offset in ms (positive = clock later than the audio, negative =
    /// earlier; a pre-roll is added automatically at play).
    offset_ms: Option<f64>,
    /// none | output | input — what the clock aligns to (see the tool text).
    latency_mode: Option<String>,
    /// Send Song Position Pointer + Continue when resuming.
    song_position: Option<bool>,
    /// A MIDI out port, by (part of) the name `get_midi_clock` lists.
    output: Option<String>,
    /// Wire (default) or unplug that port.
    output_enabled: Option<bool>,
}

/// Arguments for `set_record_sync`.
#[derive(Debug, Deserialize, JsonSchema)]
struct RecordSyncArgs {
    /// Whether recording follows the master transport (play starts, stop stops).
    on: bool,
}

/// Arguments for `set_record_arm`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRecordArmArgs {
    /// Target: "master" or "rig" (defaults to "rig" when `rig` is set, else "master").
    target: Option<String>,
    /// 0-based rig index when arming a rig stem.
    rig: Option<u8>,
    /// True to arm, false to disarm (defaults to true).
    armed: Option<bool>,
}

/// Arguments for `set_master_target`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetMasterTargetArgs {
    /// A named target: spotify, youtube or tidal (-14 LUFS), apple (-16),
    /// club (-8), or off (no target).
    target: Option<String>,
    /// Target integrated loudness in LUFS (-36 to -6), instead of `target`.
    lufs: Option<f64>,
}

/// Arguments for `set_keyboard_mode`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetKeyboardModeArgs {
    /// The rig's type: "auto" (follow its instrument), "piano", "electric_piano", "jazz_organ", "pipe_organ", "synth", "bass", "wind" or "drums".
    mode: Option<String>,
    /// 0-based rig index (rig 0 is the first rig, "Rig A"); default the active rig.
    rig: Option<usize>,
    /// Show (true) or hide (false) the virtual keyboard dock.
    open: Option<bool>,
    /// Expand the keyboard dock to fullscreen performance mode (true) or return to bottom dock (false).
    expanded: Option<bool>,
    /// Jazz Organ: stack Upper (Ch 1) and Lower (Ch 2) manuals.
    dual_manual: Option<bool>,
    /// Synth: fold the keybed to show only in-scale keys.
    scale_fold: Option<bool>,
    /// Piano: vertical hit position modulates velocity.
    y_touch: Option<bool>,
    /// Synth horizontal drag behavior: "gliss" or "slide".
    synth_drag: Option<String>,
}

/// Arguments for `measure_mix`.
#[derive(Debug, Deserialize, JsonSchema)]
struct MeasureMixArgs {
    /// Restart the integrated-loudness gate from now (default false).
    reset: Option<bool>,
}

/// Arguments for `listen`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListenArgs {
    /// Words the sound should match, each scored 0-10 (e.g. ["8-bit", "heroic"]).
    attributes: Option<Vec<String>>,
    /// Seconds of the master to record, 0.5-8 (default 4).
    seconds: Option<f32>,
    /// Anything else to ask about the sound.
    question: Option<String>,
}

/// Arguments for `set_rig_device`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetRigDeviceArgs {
    /// Instrument node id (1 = Surge A, 4 = Surge B).
    node: NodeId,
    /// Hardware device index, or null for any device.
    device: Option<u32>,
}

/// Arguments for `set_device_instrument`.
#[derive(Debug, Deserialize, JsonSchema)]
struct SetDeviceInstrumentArgs {
    /// Hardware device index (the N of source 'hw:N').
    device: u32,
    /// Profile id or name from list_instrument_profiles; "none" for no profile.
    profile: String,
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
    /// Parameter id on the destination node; omit/null to remove the binding (unless `knob` is set).
    param: Option<u32>,
    /// Target parameter space: "plugin" (default, CLAP/LV2/DSP native), "knob" (rack unit knob / SetParam) or "mix" (mixer control, see `mix`).
    target: Option<String>,
    /// Optional rack unit knob label (e.g. "LEVEL", "MIX", "VOLUME") — resolves `param` + `target` automatically.
    knob: Option<String>,
    /// Optional MIXER control: "vol" | "pan" | "mute" | "solo" | "send1".."send8" (the send level into the 1st..8th aux bus, see get_sends). On a rig's Master out node it is that rig's bus; on a source node the source strip's pan, mute, solo or send. Resolves `param` + `target` ("mix").
    mix: Option<String>,
    /// Lowest parameter value the controller reaches (0..1, default 0).
    min: Option<f32>,
    /// Highest parameter value the controller reaches (0..1, default 1), e.g. 0.6 so a pedal never pushes drive past 60 %.
    max: Option<f32>,
    /// Lowest controller CC value mapped to `min` (0..=127, default 0), e.g. 64 for a bite sensor.
    in_min: Option<u8>,
    /// Highest controller CC value mapped to `max` (0..=127, default 127), e.g. 104 for a bite sensor.
    in_max: Option<u8>,
}

/// Arguments for `clear_cc_map`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ClearCcMapArgs {
    /// Destination node id.
    node: NodeId,
    /// Source: "keys" or "hw:<device index>".
    source: String,
    /// Optional CC number (0..=127); omit to clear all bindings on this rule (or matching `target`/`knob`).
    cc: Option<u8>,
    /// Optional target filter ("plugin" | "knob" | "mix") when `cc` is omitted.
    target: Option<String>,
    /// Optional rack knob label when `cc` is omitted.
    knob: Option<String>,
    /// Optional mixer control ("vol" | "pan" | "mute" | "solo") when `cc` is omitted.
    mix: Option<String>,
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
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListParamsArgs {
    /// Node id to inspect.
    node: NodeId,
    /// Optional case-insensitive filter words.
    query: Option<String>,
    /// Optional CLAP module filter (e.g. "Filter", "Osc", "AEG", "FX").
    module: Option<String>,
    /// Return only parameters currently differing from their default.
    only_modified: Option<bool>,
    /// Include discrete option labels for stepped/enum parameters.
    include_options: Option<bool>,
    /// Max parameters to return (default 50, max 200).
    limit: Option<usize>,
}

/// Arguments for `inspect_plugin`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct InspectPluginArgs {
    /// Plugin name, CLAP id, LV2 URI, built-in DSP kind, or catalog id.
    query: Option<String>,
    /// Optional format filter: "all" | "builtin" | "clap" | "lv2" | "catalog".
    format: Option<String>,
    /// Optional live rack node id to inspect its active plugin and configuration.
    node: Option<NodeId>,
}

/// Arguments for `reset_params`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ResetParamsArgs {
    /// Target node id.
    node: NodeId,
    /// Optional CLAP module filter.
    module: Option<String>,
    /// Optional name filter words.
    query: Option<String>,
}

/// Arguments for `learn_cc`.
#[derive(Debug, Deserialize, JsonSchema)]
struct LearnCcArgs {
    /// Destination node id (required unless `cancel` is true).
    node: Option<NodeId>,
    /// Source: "keys" (default, any hardware controller — records the sender) or "hw:<device index>".
    source: Option<String>,
    /// Parameter id on the destination node to bind to the next hardware CC (omit if `knob` or `cancel=true`).
    param: Option<u32>,
    /// Target parameter space: "plugin" (default) or "knob" (rack unit knob / SetParam).
    target: Option<String>,
    /// Optional rack unit knob label (e.g. "LEVEL", "MIX", "VOLUME") — resolves `param` + `target` automatically.
    knob: Option<String>,
    /// Optional MIXER control: "vol" | "pan" | "mute" | "solo" (rig bus on a Master out node, source strip elsewhere).
    mix: Option<String>,
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
    /// Solo — exclusive: clears every other rig's solo and mutes them (omit to keep).
    solo: Option<bool>,
}

/// Arguments for `set_strip_mix` (#73).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetStripMixArgs {
    /// Source node id (get_graph).
    node: NodeId,
    /// Pan -1 (L) .. 1 (R) (omit to keep).
    pan: Option<f32>,
    /// Mute (omit to keep).
    mute: Option<bool>,
    /// Solo — within the node's rig (omit to keep).
    solo: Option<bool>,
}

/// Arguments for `add_bus` (issue #5).
#[derive(Debug, Deserialize, JsonSchema)]
struct AddBusArgs {
    /// Up to 24 characters; default "BUS A", "BUS B", ….
    name: Option<String>,
    /// The rig whose rack shows the bus unit (0-based; default 0, in the
    /// running app the rig in view).
    rig: Option<u8>,
}

/// Arguments for `set_send` (issue #5).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetSendArgs {
    /// The aux bus's node id (add_bus / get_sends).
    bus: NodeId,
    /// Source: a whole rig (0-based index), tapped at its rig bus. Give rig OR node.
    rig: Option<u8>,
    /// Source: one node (an instrument, an audio input, an effect). Give rig OR node.
    node: Option<NodeId>,
    /// Linear gain 0..1 (1 = 0 dB, 0 = off; omit to keep).
    level: Option<f32>,
    /// true = tapped before the source's fader (omit to keep; default post).
    pre: Option<bool>,
}

/// Arguments for `remove_bus` (issue #5).
#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveBusArgs {
    /// The aux bus's node id.
    bus: NodeId,
}

/// Arguments for `set_bus_output` (#147).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetBusOutputArgs {
    /// The rig whose bus to route (0-based). Give `rig` OR `bus`.
    rig: Option<u8>,
    /// An aux bus's node id (add_bus / get_sends). Give `rig` OR `bus`.
    bus: Option<u32>,
    /// PipeWire sink name from list_audio_outputs; omit or empty = back into the master.
    output: Option<String>,
}

/// Arguments for `get_bus_outputs` (#147).
#[derive(Debug, Deserialize, JsonSchema)]
struct GetBusOutputsArgs {}

/// Arguments for `get_sends` (issue #5).
#[derive(Debug, Deserialize, JsonSchema)]
struct GetSendsArgs {}

/// Arguments for `set_node_bypass` (#74, #305).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetNodeBypassArgs {
    /// Unit node id (get_graph).
    node: NodeId,
    /// True = bypass (default), false = enable.
    bypass: Option<bool>,
}

/// Arguments for `duplicate_node` (#305).
#[derive(Debug, Deserialize, JsonSchema)]
struct DuplicateNodeArgs {
    /// Source unit node id (get_graph).
    node: NodeId,
    /// Optional target 0-based rig index (default: source node's rig).
    rig: Option<u8>,
    /// Optional custom name for the duplicate (up to 24 characters).
    name: Option<String>,
}

/// Arguments for `reset_node_params` (#305).
#[derive(Debug, Deserialize, JsonSchema)]
struct ResetNodeParamsArgs {
    /// Unit node id (get_graph).
    node: NodeId,
}

/// Arguments for `randomize_node_params` (#305).
#[derive(Debug, Deserialize, JsonSchema)]
struct RandomizeNodeParamsArgs {
    /// Unit node id (get_graph).
    node: NodeId,
    /// Randomization strength in 0..=1 (default 1.0).
    amount: Option<f64>,
    /// Optional deterministic RNG seed.
    seed: Option<u64>,
}

/// Arguments for `list_plugins`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListPluginsArgs {
    /// Filter by plugin type: "all", "instrument", "effect", "midi_fx", or "utility".
    #[serde(rename = "type")]
    plugin_type: Option<String>,
    /// Filter by subcategory (e.g. "synth", "organ", "sampler", "delay_reverb", "eq_filter", "dynamics", "drive_amp", "modulation", "pitch_spatial", "multi_fx", "analyzer").
    category: Option<String>,
    /// Filter by format: "all", "builtin", "clap", or "lv2".
    format: Option<String>,
    /// Free-text search across name, author/vendor, id/uri, and category.
    query: Option<String>,
    /// Maximum CLAP / LV2 entries returned (default 60, max 300).
    limit: Option<u64>,
}

/// Arguments for `add_insert` (#74).
#[derive(Debug, Deserialize, JsonSchema)]
struct AddInsertArgs {
    /// The strip's source node id; omit for a MASTER insert.
    source: Option<NodeId>,
    /// "dsp" (with dsp=…), "lv2" (with uri=…), or "clap" (with plugin_id=…).
    kind: String,
    /// Built-in effect: gain, pan, eq3, filter, limiter, saturation, delay, reverb.
    dsp: Option<String>,
    /// LV2 effect URI (list_plugins).
    uri: Option<String>,
    /// CLAP effect plugin_id (list_plugins).
    plugin_id: Option<String>,
    /// Optional display name for the new unit.
    name: Option<String>,
}

/// Arguments for `remove_insert` (#74).
#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveInsertArgs {
    /// The effect node id to remove.
    fx: NodeId,
    /// Its strip's source node id; omit for a master insert.
    source: Option<NodeId>,
}

/// Arguments for `rename_strip` (#77).
#[derive(Debug, Deserialize, JsonSchema)]
struct RenameStripArgs {
    /// Node id (get_graph).
    node: NodeId,
    /// New name (≤ 24 characters).
    name: String,
}

/// Arguments for `set_strip_color` (#77).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetStripColorArgs {
    /// Node id (get_graph).
    node: NodeId,
    /// 0xRRGGBB, a palette name (teal, amber, rose, sand, violet, cyan), or none.
    color: String,
}

/// Arguments for `move_strip` (#77).
#[derive(Debug, Deserialize, JsonSchema)]
struct MoveStripArgs {
    /// Source node id (get_graph).
    node: NodeId,
    /// 1-based slot within its rig.
    to: u64,
}

/// Arguments for `collapse_strip` (#77).
#[derive(Debug, Deserialize, JsonSchema)]
struct CollapseStripArgs {
    /// Node id (get_graph).
    node: NodeId,
    /// True = collapse (default), false = expand.
    collapsed: Option<bool>,
}

/// Arguments for `get_strip_knobs` (#107).
#[derive(Debug, Deserialize, JsonSchema)]
struct GetStripKnobsArgs {
    /// Optional node id to inspect one slot.
    node: Option<NodeId>,
}

/// Arguments for `set_strip_knob` (#107).
#[derive(Debug, Deserialize, JsonSchema)]
struct SetStripKnobArgs {
    /// Node id (get_graph).
    node: NodeId,
    /// Knob label or parameter name/id; empty string resets to default.
    knob: String,
}

/// Arguments for `list_channels` (#108).
#[derive(Debug, Deserialize, JsonSchema)]
struct ListChannelsArgs {
    /// Optional filter: "channels", "plugins", or "all" (default "all").
    filter: Option<String>,
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

/// Rigs of the headless patch (a single-graph patch is one rig).
fn headless_rig_count(st: &State) -> usize {
    st.graph.nodes.iter().map(|n| n.rig as usize + 1).max().unwrap_or(1)
}

fn headless_active_rig(st: &State) -> usize {
    st.rigs.lock().map(|s| s.active).unwrap_or(0).min(headless_rig_count(st) - 1)
}

/// One headless rig's type (#198): its mark, else what its first
/// instrument node is, as the app classifies it.
fn headless_rig_type(st: &State, rig: usize) -> Value {
    let detected = st.graph.nodes.iter()
        .find(|n| n.rig as usize == rig && matches!(n.kind, NodeKind::Clap { .. } | NodeKind::Lv2 { .. }))
        .map(|n| crate::keyboard::classify_instrument(&n.kind, None, Some(crate::control::name_str(&n.name)), &[]));
    let mark = st.rig_types.get(rig).copied().flatten();
    let name = st.rigs.lock().ok().and_then(|s| s.rigs.get(rig).map(|m| m.name.clone())).filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Rig {}", (b'A' + rig.min(25) as u8) as char));
    json!({
        "rig": rig,
        "name": name,
        "type": mark.or(detected).map(|t| t.as_str()),
        "detected": detected.map(|t| t.as_str()),
        "marked": mark.is_some(),
    })
}

/// Headless `get_keyboard_state`: no dock, the rigs' types.
fn headless_keyboard_state(st: &State) -> Value {
    let active = headless_active_rig(st);
    let rigs: Vec<Value> = (0..headless_rig_count(st)).map(|r| headless_rig_type(st, r)).collect();
    let a = &rigs[active];
    json!({
        "open": false,
        "expanded": false,
        "rig": active,
        "auto": !a["marked"].as_bool().unwrap_or(false),
        "marked": a["marked"],
        "mode": a["type"].as_str().unwrap_or(crate::keyboard::KeyboardType::Synth.as_str()),
        "detected_mode": a["detected"].as_str().unwrap_or(crate::keyboard::KeyboardType::Synth.as_str()),
        "rigs": rigs,
    })
}

/// A rig list (`{rigs: [...]}`) with each rig's `type` and `type_marked`
/// from a `rigs` list as `get_keyboard_state` gives it.
fn with_rig_types(mut data: Value, types: &[Value]) -> Value {
    if let Some(list) = data.get_mut("rigs").and_then(Value::as_array_mut) {
        for (i, r) in list.iter_mut().enumerate() {
            if let (Some(obj), Some(t)) = (r.as_object_mut(), types.get(i)) {
                obj.insert("type".into(), t["type"].clone());
                obj.insert("type_marked".into(), t["marked"].clone());
            }
        }
    }
    data
}

fn sound_args(query: &str, rig: Option<usize>, node: Option<NodeId>) -> Value {
    let mut a = json!({"query": query});
    if let Some(r) = rig { a["rig"] = json!(r); }
    if let Some(n) = node { a["node"] = json!(n); }
    a
}

/// The instruments of a headless graph, as the sound tools address them.
/// LV2 nodes count when their plugin declares itself an instrument.
fn headless_instruments(graph: &Graph, lv2: &[crate::lv2::Lv2PluginInfo]) -> Vec<crate::sounds::Unit> {
    graph.nodes.iter().filter_map(|n| {
        let instrument = crate::sounds::Instrument::of(&n.kind)?;
        if let crate::sounds::Instrument::Lv2(uri) = &instrument {
            if !lv2.iter().any(|p| p.uri == *uri && p.is_instrument) { return None; }
        }
        Some(crate::sounds::Unit { node: n.id, rig: n.rig as usize, name: name_str(&n.name).to_string(), instrument, slot: None })
    }).collect()
}

/// The instrument a headless sound tool means and its library, scanned in
/// the caller (no UI thread here). A plugin's own preset list is only
/// known to the running app.
fn headless_sound_library(graph: &Graph, rig: Option<usize>, node: Option<NodeId>) -> Result<(crate::sounds::Unit, Arc<crate::sounds::Library>), String> {
    static LIBRARIES: std::sync::OnceLock<crate::sounds::Libraries> = std::sync::OnceLock::new();
    let has_lv2 = graph.nodes.iter().any(|n| matches!(n.kind, NodeKind::Lv2 { .. }));
    let lv2 = if has_lv2 { crate::lv2::discover() } else { Vec::new() };
    let rig = rig.unwrap_or(0);
    let unit = crate::sounds::pick_unit(&headless_instruments(graph, &lv2), rig, &format!("Rig {}", rig + 1), node)?;
    match LIBRARIES.get_or_init(|| crate::sounds::Libraries::blocking(crate::sounds::scan).watching(crate::sounds::folder_stamp)).get(&unit.instrument) {
        crate::sounds::Lookup::Ready(library) => Ok((unit, library)),
        _ => Err(format!("{}'s presets are read from the live plugin: attach to the running app (kabelsalat mcp --attach)", unit.name)),
    }
}

/// Headless mode hosts only its startup voices (the plugins' main thread
/// serves MCP); the running app hosts one per CLAP unit. Adds the Clap nodes
/// of the current patch that no voice plays to a tool result, so a surplus
/// synth is reported, not just silent.
fn note_silent_claps(st: &State, result: &mut Value) {
    if !matches!(st.backend, Backend::Local(_)) {
        return;
    }
    let Some(hosted) = st.graph_pub.as_ref().and_then(|p| p.lock().ok().map(|g| g.voices.clone())) else { return };
    let voices: Vec<(u64, &str)> = hosted.iter().map(|(id, _)| (*id, control::SURGE_PLUGIN_ID)).collect();
    let silent: Vec<NodeId> = control::bind_voices(&st.graph, &voices)
        .into_iter()
        .filter(|(_, voice)| voice.is_none())
        .map(|(node, _)| node)
        .collect();
    if !silent.is_empty() {
        result["silent_clap_nodes"] = json!(silent);
        result["note"] = json!(format!(
            "headless mcp hosts {} Surge XT voice(s): these Clap nodes have none and stay silent. The running app (kabelsalat mcp --attach) hosts a voice for every CLAP unit.",
            hosted.len()
        ));
    }
}

/// Headless: bring the CC mirror and the engine in line with the profiles
/// picked per device (the app's `sync_instrument_maps`, run on demand here).
fn sync_instruments(st: &mut State, profiles: &[crate::instruments::Profile]) -> Result<(), String> {
    let wanted = {
        let published = st.graph_pub.as_ref().and_then(|p| p.lock().ok());
        let devices = st.device_instruments.iter().filter_map(|(&d, id)| Some((d, profiles.iter().find(|p| &p.id == id)?)));
        // Before the engine's first publication there are no rules to ask.
        crate::instruments::wanted(devices, &st.graph, |src, dest| {
            published.as_ref().filter(|g| !g.rules.is_empty()).map_or(true, |g| {
                g.rules.iter().any(|r| r.source == src && r.dest == dest && !r.cc_only)
            })
        })
    };
    let changes = crate::instruments::changes(&st.instrument_bound, &wanted, |(src, dest, b)| {
        st.cc_maps.get(&(*src, *dest)).and_then(|v| v.iter().find(|m| m.cc == b.cc)).is_some_and(|m| {
            (m.param, m.target, m.scale) == (b.param, b.target, b.scale)
        })
    });
    for ((source, dest, b), on) in changes {
        send_cmd(st, Command::MatrixCcMap { source, dest, cc: b.cc, param: on.then_some(b.param), target: b.target, scale: b.scale })?;
        let entry = st.cc_maps.entry((source, dest)).or_default();
        entry.retain(|m| m.cc != b.cc);
        if on {
            entry.push(crate::matrix::CcMapping { cc: b.cc, param: b.param, target: b.target, scale: b.scale });
        }
        if entry.is_empty() {
            st.cc_maps.remove(&(source, dest));
        }
    }
    st.instrument_bound = wanted;
    Ok(())
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
        DspKind::Bus => "bus",
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
        "bus" => Ok(DspKind::Bus),
        other => Err(format!("unknown dsp kind '{other}'")),
    }
}

fn parse_node_kind(args: &AddNodeArgs) -> Result<NodeKind, String> {
    match args.kind.to_ascii_lowercase().as_str() {
        "keys" => Ok(NodeKind::Keys),
        "clap" => Ok(NodeKind::Clap {
            plugin_id: crate::control::ClapId::new(args.plugin_id.as_deref().unwrap_or(SURGE_ID)),
        }),
        "dsp" => {
            let dsp = args
                .dsp
                .as_deref()
                .ok_or_else(|| "kind 'dsp' requires the 'dsp' field".to_string())?;
            Ok(NodeKind::Dsp(parse_dsp_kind(dsp)?))
        }
        "system_audio" => Ok(NodeKind::SystemAudio),
        // The arpeggiator / chord unit (issue #90): inserted between the
        // rig's Keys and its synth(s) when wired.
        "arp" | "arpeggiator" => Ok(NodeKind::Arp),
        "audio_in" => Ok(NodeKind::AudioIn {
            device: args.device.clone().filter(|d| !d.is_empty()),
            channels: parse_audio_ch(args.channels.as_deref()),
        }),
        "master_out" => Ok(NodeKind::MasterOut),
        "midi_fx" | "midi-fx" => Ok(NodeKind::MidiFx {
            plugin: match (args.uri.as_deref(), args.plugin_id.as_deref()) {
                (Some(uri), _) => crate::control::MidiFxPlugin::Lv2 { uri: crate::control::Lv2Uri::new(uri) },
                (None, Some(_)) => {
                    return Err("CLAP note effects are listed by list_midi_fx but cannot be hosted yet; use an LV2 MIDI plugin (uri) or the built-in transposer".into());
                }
                (None, None) => crate::control::MidiFxPlugin::Builtin {
                    semis: args.semis.unwrap_or(0).clamp(-crate::engine::MIDIFX_SEMI_MAX, crate::engine::MIDIFX_SEMI_MAX),
                },
            },
        }),
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

/// Parse an AudioIn channel selector: "in_1"/"l"/"1" → MonoL, "in_2"/"r"/"2"
/// → MonoR, "st"/"stereo" → Stereo.
fn parse_audio_ch(s: Option<&str>) -> crate::control::AudioInChannel {
    match s.unwrap_or("in_1").to_ascii_lowercase().as_str() {
        "r" | "2" | "right" | "in_2" | "in2" | "mono_r" | "monor" => crate::control::AudioInChannel::MonoR,
        "st" | "s" | "stereo" | "12" => crate::control::AudioInChannel::Stereo,
        _ => crate::control::AudioInChannel::MonoL,
    }
}

/// Machine-readable channel tag for `get_graph` / `set_audio_in` output.
fn ch_kind_str(c: crate::control::AudioInChannel) -> &'static str {
    match c {
        crate::control::AudioInChannel::MonoL => "in_1",
        crate::control::AudioInChannel::MonoR => "in_2",
        crate::control::AudioInChannel::Stereo => "stereo",
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
                    json!({"type": "clap", "plugin_id": plugin_id.as_str()})
                }
                NodeKind::Dsp(k) => json!({"type": "dsp", "dsp": dsp_kind_str(k)}),
                NodeKind::Lv2 { uri } => json!({"type": "lv2", "uri": uri.as_str()}),
                NodeKind::SystemAudio => json!({"type": "system_audio"}),
                NodeKind::AudioIn { device, channels } => json!({
                    "type": "audio_in",
                    "device": device,
                    "channels": ch_kind_str(*channels),
                }),
                NodeKind::Arp => json!({"type": "arp"}),
                NodeKind::MasterOut => json!({"type": "master_out"}),
                NodeKind::MidiFx { plugin } => match plugin {
                    crate::control::MidiFxPlugin::Lv2 { uri } => {
                        json!({"type": "midi_fx", "plugin": "lv2", "uri": uri.as_str()})
                    }
                    crate::control::MidiFxPlugin::Builtin { semis } => {
                        json!({"type": "midi_fx", "plugin": "builtin", "semis": semis})
                    }
                },
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
                // Keys only: key + scale its notes snap to (null = no snap).
                "scale": n.scale.map(|c| json!({"key": c.key, "scale": c.scale})),
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
                // Marks follow their rig (#198).
                if r < st.rig_types.len() { st.rig_types.remove(r); }
                0
            }
            None => {
                if rigs.len() >= 8 { return tool_err("at most 8 rigs"); }
                let used: Vec<String> = rigs.iter().map(|r| r.name.clone()).collect();
                let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
                    (b'A'..=b'Z').map(|c| format!("Rig {}", c as char)).find(|n| !used.contains(n)).unwrap_or_else(|| "Rig".into())
                });
                rigs.push(Rig::new(name, control::standard_rig_graph(true), None));
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
        let mut result = json!({"rigs": metas, "active": active});
        note_silent_claps(&st, &mut result);
        ok_json(result)
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
        let (playing, counting_in, count_in_beat, count_in_enabled, count_in_bars, bpm, record_sync) = match tp {
            Some(t) => (t.playing, t.counting_in, t.count_in_beat, t.count_in_enabled, t.count_in_bars, t.bpm, t.record_sync),
            None => (st.playing, false, 0, st.count_in_enabled, st.count_in_bars, 96.0, st.record_sync),
        };
        let time_sig = tp.map_or(crate::transport::TimeSig::FOUR_FOUR, |t| t.time_sig);
        let meter = st
            .graph_pub
            .as_ref()
            .and_then(|p| p.lock().ok())
            .map(|g| serde_json::to_value(g.meter).unwrap_or_else(|_| json!(null)))
            .unwrap_or_else(|| json!(null));
        let master_target = st
            .graph_pub
            .as_ref()
            .and_then(|p| p.lock().ok())
            .map(|g| g.master_target.readout(&g.meter))
            .unwrap_or_else(|| json!(null));
        let master_mute = st
            .graph_pub
            .as_ref()
            .and_then(|p| p.lock().ok())
            .map(|g| g.master_mute)
            .unwrap_or(false);
        let counters = crate::engine_info::EngineCounters::load();
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
            "time_signature": time_sig.to_json(),
            "record_sync": record_sync,
            "bpm": bpm,
            "buffer_frames": counters.buffer_frames(),
            "buffer_latency_ms": counters.buffer_frames() as f64 / st.sample_rate * 1000.0,
            "xruns": counters.xruns,
            "midi_latency_ms": counters.midi_latency_us as f64 / 1000.0,
            "rigs": rig_count,
            "active_rig": active_rig,
            "meter": meter,
            "master_target": master_target,
            "master_mute": master_mute,
        }))
    }

    /// The engine's load and audio session (the app's Engine Info panel).
    #[tool(name = "get_engine_info", description = "The engine's load and audio session (the app's DSP readout and Engine Info panel): dsp_percent (average render time as a share of one buffer; above 70 is high, above 90 dropouts are likely), render_avg_us, render_max_us, late_blocks (buffers that took over 80 % of their time), xruns, sample_rate, buffer_frames, buffer_ms (the output latency the buffer adds), inputs (channels of the Audio In units), outputs, midi_input_latency_ms (null until a hardware MIDI event was measured), audio_live, and nodes: every instrument and effect with avg_us, max_us and percent of the buffer, most expensive first. The sound card's own latency and the audio-input latency are not measured and not included. Read-only.")]
    fn get_engine_info(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("get_engine_info", json!({}));
        }
        let inputs = st.graph.nodes.iter()
            .map(|n| match &n.kind { NodeKind::AudioIn { channels, .. } => channels.count(), _ => 0 })
            .sum();
        let stats = crate::engine_info::EngineStats::build(
            crate::engine_info::EngineCounters::load(),
            st.node_times.as_deref(),
            inputs,
            |node| st.graph.node(node).map(|n| crate::ui::sentence_case(name_str(&n.name))),
        );
        ok_json(serde_json::to_value(stats).unwrap_or(Value::Null))
    }

    #[tool(name = "reset_engine_peaks", description = "Start the engine's peak counters again from now: render_max_us, late_blocks, xruns and every unit's max_us (the Engine Info panel's Reset peaks button). Averages are not touched.")]
    fn reset_engine_peaks(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("reset_engine_peaks", json!({}));
        }
        crate::engine_info::reset_counter_peaks();
        if let Some(t) = &st.node_times { t.reset_peaks(); }
        ok_json(json!("reset: worst render time, late blocks, xruns and each unit's maximum start again from now"))
    }

    /// The audio judge (#35): record the master and ask a listening model.
    #[tool(name = "listen", description = "Hear the master: record the next `seconds` of the master output and ask a listening model (audio-capable, OpenRouter) to score it 0-10 on each attribute and say what it heard; the answer adds the app's loudness/peak/band measurements. Only sound that plays during the recording is heard: send note_on first and note_off after. Takes about seconds + 3 s.")]
    fn listen(&self, Parameters(args): Parameters<ListenArgs>) -> Result<CallToolResult, McpError> {
        let args = json!({ "attributes": args.attributes, "seconds": args.seconds, "question": args.question });
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if !remote {
            // Off the async runtime: the HTTP client blocks and owns a runtime
            // of its own, which may not be dropped inside this one.
            return match std::thread::spawn(move || crate::listen::listen(&args)).join() {
                Ok(text) => ok_json(json!(text)),
                Err(_) => tool_err("listen: the listening thread panicked"),
            };
        }
        let wait = Duration::from_secs_f32(crate::listen::MAX_SECS + 60.0);
        match bridge::request_within(&json!({"op": "listen", "args": args}), wait) {
            Ok(reply) if reply.get("ok").and_then(Value::as_bool) == Some(true) => ok_json(reply.get("data").cloned().unwrap_or(Value::Null)),
            Ok(reply) => tool_err(reply.get("error").and_then(Value::as_str).unwrap_or("listen failed").to_string()),
            Err(e) => tool_err(format!("listen needs the running app (attach): {e:#}")),
        }
    }

    /// Master-bus loudness/true-peak/band meter (Bravo 1 "measure loop").
    #[tool(name = "measure_mix", description = "Read the master-bus loudness/profile meter: momentary (400 ms) and short-term (3 s) LUFS, gated integrated LUFS since the last reset, true peak (dBTP), low-end (< 120 Hz) phase correlation with a mono-compat flag, and a 4-band energy summary (< 120, 120-500, 500-4000, > 4000 Hz) in dB. Read-only; pass reset=true to restart the integrated-loudness gate.")]
    fn measure_mix(&self, Parameters(args): Parameters<MeasureMixArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if args.reset == Some(true) {
            if let Err(e) = send_cmd(&mut st, Command::ResetMeter) {
                return tool_err(e);
            }
        }
        if matches!(st.backend, Backend::Remote) {
            let reply = match bridge::request(&json!({"op": "query", "name": "get_status"})) {
                Ok(r) => r,
                Err(e) => return tool_err(format!("{e:#}")),
            };
            if reply.get("ok").and_then(Value::as_bool) != Some(true) {
                return tool_err(reply.get("error").and_then(Value::as_str).unwrap_or("bridge query failed"));
            }
            let data = reply.get("data").cloned().unwrap_or(Value::Null);
            return ok_json(data.get("meter").cloned().unwrap_or(Value::Null));
        }
        let meter = st
            .graph_pub
            .as_ref()
            .and_then(|p| p.lock().ok())
            .map(|g| serde_json::to_value(g.meter).unwrap_or_else(|_| json!(null)))
            .unwrap_or_else(|| json!(null));
        ok_json(meter)
    }

    /// Loudness target of the master (#12).
    #[tool(name = "set_master_target", description = "Master the mix to a loudness: give target (spotify, youtube or tidal = -14 LUFS, apple = -16, club = -8, off = no target) or lufs (a number, -36 to -6). The engine trims the level in front of the master limiter by the difference to the integrated loudness measured so far, and holds the true peak under -1 dBTP. Sound must have played first; the result gives the loudness before and the trim. Let sound play, read the loudness after with get_master_target, and call set_master_target again with the same target to correct what is still off. Setting a target restarts measure_mix's integrated loudness.")]
    fn set_master_target(&self, Parameters(args): Parameters<SetMasterTargetArgs>) -> Result<CallToolResult, McpError> {
        let lufs = match crate::meter::parse_target(args.target.as_deref(), args.lufs) {
            Ok(l) => l,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("set_master_target", json!({"target": args.target, "lufs": args.lufs}));
        }
        // What the engine will hold: the same `matched` on the same reading.
        let Some((target, meter)) = st.graph_pub.as_ref().and_then(|p| p.lock().ok()).map(|g| (g.master_target, g.meter)) else {
            return tool_err("no engine");
        };
        if let Err(e) = send_cmd(&mut st, Command::SetMasterTarget { lufs }) {
            return tool_err(e);
        }
        let next = match lufs {
            Some(l) => target.matched(l, meter.integrated_lufs.is_finite().then_some(meter.integrated_lufs)),
            None => crate::meter::MasterTarget::OFF,
        };
        ok_json(next.readout(&crate::meter::MeterSnapshot::default()))
    }

    #[tool(name = "get_master_target", description = "Read the master's loudness target and its before/after readout: target_lufs, gain_db (the trim in front of the limiter), before_lufs (the mix without the trim), after_lufs (integrated loudness measured since the trim was set; null until sound played), true_peak_dbtp with true_peak_ok (ceiling -1 dBTP), guard_db and a status line.")]
    fn get_master_target(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("get_master_target", json!({}));
        }
        let readout = st.graph_pub.as_ref().and_then(|p| p.lock().ok()).map(|g| g.master_target.readout(&g.meter));
        match readout {
            Some(r) => ok_json(r),
            None => tool_err("no engine"),
        }
    }

    #[tool(name = "get_keyboard_state", description = "Read the virtual software keyboard dock state: open, expanded (fullscreen mode), the active rig's instrument type (mode: piano | electric_piano | jazz_organ | pipe_organ | synth | bass | wind | drums), whether it is marked or follows its instrument (auto, marked), the detected type, rigs (every rig's index, name, type, detected type and marked), target instrument, octave, sustain, swell, dual_manual, scale_fold, y_touch and synth_drag.")]
    fn get_keyboard_state(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("get_keyboard_state", json!({}));
        }
        ok_json(headless_keyboard_state(&st))
    }

    #[tool(name = "set_keyboard_mode", description = "Configure the virtual software keyboard dock: open/close it, toggle expanded/fullscreen performance view, mark a rig's instrument type (mode: piano | electric_piano | jazz_organ | pipe_organ | synth | bass | wind | drums) or return it to auto (follow its instrument), or toggle surface options (dual_manual, scale_fold, y_touch, synth_drag: gliss|slide). rig is 0-based (rig 0 = the first rig, Rig A) and defaults to the active rig; the keyboard plays the active rig's type.")]
    fn set_keyboard_mode(&self, Parameters(args): Parameters<SetKeyboardModeArgs>) -> Result<CallToolResult, McpError> {
        let parsed_mode = match args.mode.as_deref() {
            Some(s) => match crate::keyboard::KeyboardMode::parse(s) {
                Some(m) => Some(m),
                None => return tool_err(format!("unknown keyboard mode '{s}' (use {})", crate::keyboard::KeyboardMode::CHOICES)),
            },
            None => None,
        };
        if let Some(d) = args.synth_drag.as_deref() {
            if crate::keyboard::SynthDragMode::parse(d).is_none() {
                return tool_err(format!("unknown synth_drag '{d}' (use gliss or slide)"));
            }
        }
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("set_keyboard_mode", json!({
                "mode": args.mode,
                "rig": args.rig,
                "open": args.open,
                "expanded": args.expanded,
                "dual_manual": args.dual_manual,
                "scale_fold": args.scale_fold,
                "y_touch": args.y_touch,
                "synth_drag": args.synth_drag,
            }));
        }
        let n = headless_rig_count(&st);
        let rig = match args.rig {
            Some(r) if r >= n => return tool_err(format!("no rig {r}: rig is 0-based and there are {n} (0..{})", n - 1)),
            Some(r) => r,
            None => headless_active_rig(&st),
        };
        if let Some(m) = parsed_mode {
            if st.rig_types.len() <= rig { st.rig_types.resize(rig + 1, None); }
            st.rig_types[rig] = m.pinned();
            while st.rig_types.last() == Some(&None) { st.rig_types.pop(); }
        }
        ok_json(headless_keyboard_state(&st))
    }

    /// Add a node to the patch.
    #[tool(name = "add_node", description = "Add a node (Command::AddNode). kind: keys|clap|dsp|lv2|midi_fx|audio_in|system_audio|master_out; midi_fx = a MIDI-effect unit in the rig's MIDI chain (AUM MIDI strips: arpeggiator, chord tool, transposer): give uri (an LV2 MIDI plugin from list_midi_fx) or neither for the built-in transposer (semis); it is inserted between the rig's Keys and its instruments; dsp nodes need 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb|metronome); lv2 nodes need 'uri'; clap nodes take an optional plugin_id (default Surge XT); audio_in nodes take optional device (PipeWire source name from list_audio_devices; omit = default input) and channels (in_1|in_2|stereo). Added dsp/lv2 nodes sound immediately (processors are built); wire them with connect. An instrument (clap, or an lv2 plugin that declares lv2:InstrumentPlugin) is wired like a standard rig's synth: the rig's Keys -> it (MIDI) and it -> the rig's Master L and R; 'cables' lists those cable ids. In the running app (mcp --attach) every clap node gets its own voice, however many there are; headless mcp hosts two Surge XT voices and returns silent_clap_nodes when the new node has none.")]
    fn add_node(&self, Parameters(args): Parameters<AddNodeArgs>) -> Result<CallToolResult, McpError> {
        let kind = match parse_node_kind(&args) {
            Ok(k) => k,
            Err(e) => return tool_err(e),
        };
        let label = args.name.clone().unwrap_or_else(|| match &kind {
            NodeKind::Keys => "KEYS".into(),
            NodeKind::Clap { .. } => "CLAP".into(),
            NodeKind::Dsp(k) => dsp_kind_str(k).to_ascii_uppercase(),
            NodeKind::AudioIn { .. } => "AUDIO IN".into(),
            NodeKind::Lv2 { uri } => uri
                .as_str()
                .rsplit(['/', '#'])
                .next()
                .unwrap_or("LV2")
                .to_ascii_uppercase(),
            NodeKind::SystemAudio => "SYSTEM AUDIO".into(),
            NodeKind::Arp => "ARP".into(),
            NodeKind::MasterOut => "MASTER OUT".into(),
            NodeKind::MidiFx { plugin } => match plugin {
                crate::control::MidiFxPlugin::Builtin { .. } => "TRANSPOSE".into(),
                crate::control::MidiFxPlugin::Lv2 { uri } => {
                    uri.as_str().rsplit(['/', '#']).next().unwrap_or("MIDI FX").to_ascii_uppercase()
                }
            },
        });
        let name = name_bytes(&label);
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        // Send first, then mirror into the local model: both id counters
        // advance exactly once per accepted command and stay in sync.
        let rig = args.rig.unwrap_or(0);
        let wire = match &kind {
            NodeKind::Clap { plugin_id } => {
                !crate::plugins::clap_id_is_effect(plugin_id.as_str(), &crate::host::list_clap_plugins())
            }
            NodeKind::Lv2 { uri } => {
                let lv2 = crate::lv2::discover();
                let Some(p) = lv2.iter().find(|p| p.uri == uri.as_str()) else {
                    let installed: Vec<String> = lv2.iter().filter(|p| p.is_instrument).map(|p| format!("{} = {}", p.name, p.uri)).collect();
                    return tool_err(format!("'{}' is not an installed LV2 plugin. Installed LV2 instruments: {}", uri.as_str(), installed.join(", ")));
                };
                p.is_instrument
            }
            // An arp and a MIDI strip are inserted between the rig's Keys and
            // its synth(s) like an instrument is wired into its rig.
            NodeKind::Arp | NodeKind::MidiFx { .. } => true,
            // An aux bus comes wired to its rig's master (see add_bus).
            NodeKind::Dsp(DspKind::Bus) => true,
            _ => false,
        };
        if let Err(e) = send_cmd(&mut st, Command::AddNode { kind: kind.clone(), name, rig, wire }) {
            return tool_err(e);
        }
        let id = st.graph.add_node(kind, name);
        st.graph.set_rig(id, rig);
        let before = st.graph.edges.len();
        if wire {
            match st.graph.node(id).map(|n| n.kind.clone()) {
                Some(NodeKind::Arp) => {
                    let _ = st.graph.wire_arp(id);
                }
                Some(NodeKind::MidiFx { .. }) => {
                    st.graph.wire_midi_fx(id);
                }
                Some(NodeKind::Dsp(DspKind::Bus)) => {
                    st.graph.wire_bus(id);
                }
                _ => {
                    let _ = st.graph.wire_instrument(id);
                }
            }
        }
        let cables: Vec<u32> = st.graph.edges[before..].iter().map(|e| e.id).collect();
        let mut result = json!({"id": id, "name": name_str(&name), "cables": cables});
        note_silent_claps(&st, &mut result);
        ok_json(result)
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

    /// Directly edit an internal synth/FX parameter or rack knob.
    #[tool(name = "set_param", description = "Directly edit a node's internal parameter or rack knob live (CLAP synths/FX like Starbreath or Surge XT, LV2 plugins, setBfree organ, built-in DSP, Master, Arp, Audio In, or MIDI FX). Identify by param (id) or name (+ optional module for CLAP), using musical text (e.g. '300 ms', '1.2 kHz', '+4 dB', 'Ping-Pong', 'FAST', '6/8'), native value (min..max from list_params), or normalized (0..1). On CLAP nodes, target defaults to 'plugin' (requires --attach); pass target='knob' or name='LEVEL' for the rack LEVEL knob.")]
    fn set_param(&self, Parameters(args): Parameters<SetParamArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            let mut payload = json!({ "node": args.node });
            if let Some(p) = args.param { payload["param"] = json!(p); }
            if let Some(n) = args.name { payload["name"] = json!(n); }
            if let Some(m) = args.module { payload["module"] = json!(m); }
            if let Some(v) = args.value { payload["value"] = json!(v); }
            if let Some(n) = args.normalized { payload["normalized"] = json!(n); }
            if let Some(t) = args.text { payload["text"] = json!(t); }
            if let Some(t) = args.target { payload["target"] = json!(t); }
            return Self::ui_bridge_tool("set_param", payload);
        }
        let mut st = self.state.lock().unwrap();
        let Some(node) = st.graph.node(args.node).cloned() else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let name_q = args.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let text_q = args.text.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if args.param.is_none() && name_q.is_none() {
            return tool_err("set_param needs 'param' (id) or 'name'");
        }
        if args.value.is_none() && args.normalized.is_none() && text_q.is_none() {
            return tool_err("set_param needs 'text', 'value', or 'normalized'");
        }
        if let Some(v) = args.value {
            if !v.is_finite() { return tool_err("'value' must be finite"); }
        }
        if let Some(n) = args.normalized {
            if !n.is_finite() { return tool_err("'normalized' must be finite"); }
        }
        if let Some(t) = args.target.as_deref() {
            match crate::matrix::CcTarget::parse(Some(t)) {
                Ok(crate::matrix::CcTarget::Mix) => return tool_err("set_param does not edit mixer strips — use set_rig_mix or set_strip_mix"),
                Ok(crate::matrix::CcTarget::MidiCc | crate::matrix::CcTarget::ChannelPressure) => {
                    return tool_err("set_param sets parameters, not MIDI messages — map a controller with set_cc_map");
                }
                Ok(_) => {}
                Err(e) => return tool_err(e),
            }
        }
        // Reject MIDI CC pedal/wheel names like "swell" as parameter ids.
        if let Some(n) = name_q {
            if let Ok((cc, crate::matrix::CcTarget::MidiCc)) = Self::resolve_headless_knob(&node.kind, n) {
                return tool_err(format!("'{n}' is MIDI CC {cc}, not a parameter — map a controller to it with set_cc_map"));
            }
        }

        let target_is_knob = args.target.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("knob"));
        let (param_id, param_name, send_val, display_val, display_text) = match &node.kind {
            NodeKind::Dsp(dk) => {
                let specs = crate::dsp::dsp_param_specs(dk);
                let spec = if let Some(pid) = args.param {
                    specs.iter().find(|s| s.engine_param == pid)
                } else {
                    name_q.and_then(|n| crate::dsp::find_dsp_param(dk, n))
                };
                let Some(spec) = spec else {
                    return tool_err(format!("unknown DSP parameter on node {}", args.node));
                };
                let norm = if let Some(t) = text_q {
                    let Some(v) = crate::dsp::parse_dsp_text_value(dk, spec.native_param, t) else {
                        return tool_err(format!("could not parse '{t}' for {}", spec.label));
                    };
                    v
                } else {
                    args.normalized.or(args.value).unwrap().clamp(0.0, 1.0) as f32
                };
                let txt = crate::dsp::format_dsp_value(dk, spec.native_param, norm);
                (spec.engine_param, spec.label.to_string(), norm as f64, norm as f64, txt)
            }
            nk if crate::organ::is_organ(nk) => {
                let spec = if let Some(pid) = args.param {
                    crate::organ::param_specs().iter().find(|s| s.param == pid)
                } else {
                    name_q.and_then(crate::organ::find_param)
                };
                let Some(spec) = spec else {
                    if let Some(cc) = name_q.and_then(crate::matrix::parse_midi_cc_name) {
                        return tool_err(format!("'{}' is MIDI CC {cc}, not a parameter — map a controller to it with set_cc_map", name_q.unwrap_or("")));
                    }
                    return tool_err("unknown setBfree organ control");
                };
                let norm = if let Some(t) = text_q {
                    let Some(v) = crate::organ::parse_text_value(spec.param, t) else {
                        return tool_err(format!("could not parse '{t}' for {}", spec.name));
                    };
                    v
                } else if let Some(n) = args.normalized {
                    n.clamp(0.0, 1.0) as f32
                } else {
                    let raw = args.value.unwrap() as f32;
                    if spec.param < crate::organ::P_BASE + crate::organ::DRAWBARS as u32 && raw > 1.0 {
                        crate::organ::drawbar_value(raw.round().clamp(0.0, 8.0) as u8)
                    } else {
                        raw.clamp(0.0, 1.0)
                    }
                };
                let txt = crate::organ::format_value(spec.param, norm);
                (spec.param, spec.name.to_string(), norm as f64, norm as f64, txt)
            }
            NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: crate::control::MidiFxPlugin::Lv2 { uri } } => {
                let infos = crate::lv2::control_infos_blocking(uri.as_str());
                let found = if let Some(pid) = args.param {
                    infos.iter().find(|i| i.index as u32 == pid)
                } else if let Some(n) = name_q {
                    let q = n.to_lowercase();
                    infos.iter().find(|i| i.name.to_lowercase() == q || i.symbol.to_lowercase() == q)
                        .or_else(|| infos.iter().find(|i| i.name.to_lowercase().contains(&q) || i.symbol.to_lowercase().contains(&q)))
                } else {
                    None
                };
                if let Some(info) = found {
                    let native = if let Some(t) = text_q {
                        let Some(v) = crate::lv2::parse_lv2_control_text(info, t) else {
                            return tool_err(format!("could not parse '{t}' for LV2 control '{}'", info.name));
                        };
                        v
                    } else if let Some(n) = args.normalized {
                        crate::lv2::lv2_norm_to_native(info, n as f32)
                    } else {
                        (args.value.unwrap() as f32).clamp(info.min, info.max)
                    };
                    let norm = crate::lv2::lv2_native_to_norm(info, native);
                    let txt = crate::lv2::format_lv2_control_value(info, native);
                    (info.index as u32, info.name.clone(), norm as f64, native as f64, txt)
                } else if let Some(n) = name_q {
                    match Self::resolve_headless_knob(&node.kind, n) {
                        Ok((pid, crate::matrix::CcTarget::Knob)) => {
                            let mc = crate::lv2::main_controls_blocking(uri.as_str()).controls.into_iter().find(|c| c.id == pid);
                            let (norm, disp) = if let Some(c) = mc {
                                let span = c.max - c.min;
                                if let Some(nv) = args.normalized {
                                    let nc = nv.clamp(0.0, 1.0);
                                    (nc, c.min + nc * span)
                                } else {
                                    let p = args.value.unwrap().clamp(c.min, c.max);
                                    let nc = if span.abs() > 1e-12 { ((p - c.min) / span).clamp(0.0, 1.0) } else { 0.0 };
                                    (nc, p)
                                }
                            } else {
                                let v = args.normalized.or(args.value).unwrap_or(0.5).clamp(0.0, 1.0);
                                (v, v)
                            };
                            (pid, n.to_string(), norm, disp, format!("{disp:.3}"))
                        }
                        _ => return tool_err(format!("LV2 parameter '{n}' not found")),
                    }
                } else if let Some(pid) = args.param {
                    let norm = args.normalized.or(args.value).unwrap_or(0.5).clamp(0.0, 1.0);
                    (pid, format!("param {pid}"), norm, norm, format!("{norm:.3}"))
                } else {
                    return tool_err("LV2 parameter not found");
                }
            }
            NodeKind::MasterOut => {
                let (pid, pname) = match (args.param, name_q.map(|s| s.to_lowercase())) {
                    (Some(crate::engine::P_VOL), _) => (crate::engine::P_VOL, "Volume"),
                    (None, Some(ref s)) if s.contains("vol") => (crate::engine::P_VOL, "Volume"),
                    (Some(crate::engine::P_WARM), _) => (crate::engine::P_WARM, "Warmth"),
                    (None, Some(ref s)) if s.contains("warm") => (crate::engine::P_WARM, "Warmth"),
                    _ => return tool_err("Master Out parameters are Volume (3) and Warmth (4)"),
                };
                let norm = args.normalized.or(args.value).unwrap_or(0.8).clamp(0.0, 1.0);
                (pid, pname.to_string(), norm, norm, format!("{:.0}%", norm * 100.0))
            }
            NodeKind::Clap { .. } => {
                if (args.param == Some(crate::engine::P_LEVEL) && (target_is_knob || args.target.is_none()))
                    || name_q.is_some_and(|n| n.eq_ignore_ascii_case("level"))
                {
                    let norm = args.normalized.or(args.value).unwrap_or(1.0).clamp(0.0, 1.0);
                    (crate::engine::P_LEVEL, "LEVEL".into(), norm, norm, format!("{:.0}%", norm * 100.0))
                } else {
                    return tool_err("CLAP internal parameter editing needs the running app (kabelsalat mcp --attach)");
                }
            }
            _ => {
                let pid = if let Some(p) = args.param {
                    p
                } else if let Some(n) = name_q {
                    match Self::resolve_headless_knob(&node.kind, n) {
                        Ok((p, crate::matrix::CcTarget::Knob)) => p,
                        Ok((cc, _)) => return tool_err(format!("'{n}' is MIDI CC {cc}, not a parameter — map a controller to it with set_cc_map")),
                        Err(e) => return tool_err(e),
                    }
                } else {
                    return tool_err("set_param needs 'param' or 'name'");
                };
                let v = args.normalized.or(args.value).unwrap_or(0.5);
                (pid, format!("param {pid}"), v, v, format!("{v:.3}"))
            }
        };

        if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: param_id, value: send_val }) {
            return tool_err(e);
        }
        ok_json(json!({
            "ok": true,
            "node": args.node,
            "param": param_id,
            "name": param_name,
            "value": display_val,
            "text": display_text,
        }))
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
    #[tool(name = "set_split", description = "Split a Keys node's keyboard (Command::SetNodeSplit) — a property of the Keys node, not a global setting. point is a MIDI note 0..=127: played notes below it leave on the Keys node's port 0 (LOW), notes at/above on port 1 (HIGH). point null/omitted with no octaves = unsplit (one MIDI OUT, port 0; HIGH-port cables are removed, octaves reset). low_octave/high_octave (-3..=3) shift what a zone plays by whole octaves (Command::SetZoneOctave); octaves alone keep the current split (or split at C4). node defaults to the first Keys node. Routing is plain wiring: connect LOW (from_port 0) and HIGH (from_port 1) to rigs with connect. Held notes are released. Examples: {point: 60} splits at C4; then connect {from_node: 0, from_port: 1, to_node: 4, kind: midi}; {low_octave: 1} lifts a bass on the low zone an octave.")]
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
        let octaves = [args.low_octave, args.high_octave];
        let point = match args.point {
            Some(p) => Some(p),
            None if octaves.iter().any(Option::is_some) => {
                Some(st.graph.node(node).and_then(|n| n.split).unwrap_or(crate::control::DEFAULT_SPLIT))
            }
            None => None,
        };
        if let Err(e) = send_cmd(&mut st, Command::SetNodeSplit { node, point }) {
            return tool_err(e);
        }
        st.graph.set_split(node, point);
        for (zone, o) in octaves.into_iter().enumerate() {
            let Some(o) = o else { continue };
            let o = o.clamp(-crate::control::ZONE_OCTAVE_MAX, crate::control::ZONE_OCTAVE_MAX);
            if let Err(e) = send_cmd(&mut st, Command::SetZoneOctave { node, zone: zone as u8, octave: o }) {
                return tool_err(e);
            }
            st.graph.set_zone_octave(node, zone as u8, o);
        }
        let oct = st.graph.node(node).map_or([0; 2], |n| n.zone_octave);
        ok_json(json!({"node": node, "point": point, "split": point.is_some(), "low_octave": oct[0], "high_octave": oct[1]}))
    }

    /// Scale snap on a Keys node (issue #90).
    #[tool(name = "set_scale", description = "Make a Keys node's keyboard play only one scale (Command::SetKeysScale) — a property of the Keys node like set_split: out-of-scale keys snap to the nearest in-scale note (ties go down), so every key sounds 'right'. key: C, C#, … B (default C); scale: major, minor, dorian, mixolydian, harmonic minor, maj penta, min penta, blues (default major); off:true switches snapping off. node defaults to the first Keys node. Held notes are released. Example: {key:'A', scale:'min penta'} for an A minor pentatonic keyboard.")]
    fn set_scale(&self, Parameters(args): Parameters<SetScaleArgs>) -> Result<CallToolResult, McpError> {
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            return Self::ui_bridge_tool("set_scale", json!({"node": args.node, "key": args.key, "scale": args.scale, "off": args.off}));
        }
        let choice = if args.off == Some(true) {
            None
        } else {
            let key = match args.key.as_deref() {
                Some(k) => match crate::arp::key_from_name(k) {
                    Some(k) => k,
                    None => return tool_err(format!("'{k}' is not a key (C, C#, D, … B, or 0..11)")),
                },
                None => 0,
            };
            let scale = match args.scale.as_deref() {
                Some(s) => match crate::arp::Scale::from_name(s) {
                    Some(s) => s,
                    None => return tool_err(format!("'{s}' is not a scale (major, minor, dorian, mixolydian, harmonic minor, maj penta, min penta, blues)")),
                },
                None => crate::arp::Scale::Major,
            };
            Some(crate::arp::ScaleChoice { key, scale })
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let node = match args.node {
            Some(id) => match st.graph.node(id) {
                None => return tool_err(format!("node {id} does not exist")),
                Some(n) if !matches!(n.kind, NodeKind::Keys) => {
                    return tool_err(format!("node {id} ({}) is not a Keys node — only keyboards snap to a scale", name_str(&n.name)))
                }
                Some(_) => id,
            },
            None => match st.graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::Keys)) {
                Some(n) => n.id,
                None => return tool_err("the patch has no Keys node"),
            },
        };
        if let Err(e) = send_cmd(&mut st, Command::SetKeysScale { node, scale: choice }) {
            return tool_err(e);
        }
        st.graph.set_scale(node, choice);
        ok_json(json!({"node": node, "scale": choice.map(|c| c.label())}))
    }

    /// The arp unit's settings by name (issue #90).
    #[tool(name = "set_arp", description = "Set an Arp unit's settings by name (its rack knobs, Command::SetParam). Add the unit first with add_node {kind:'arp'} — it is inserted between the rig's Keys and its synth(s), so held keys become an arpeggio. mode: up | down | up-down | chord | random ('chord' = one held key sounds a whole chord, shape from chord); rate: 1/4 | 1/8 | 1/8T | 1/16 | 1/16T | 1/32 (synced to the song clock/tempo); octaves 1..4; gate 0.05..1 (how much of each step a note sounds); swing 0..1; chord: maj | min | sus2 | sus4 | 7 | maj7 | min7 | power. Give any subset. Changing the pattern releases what is sounding. Example: {node:7, mode:'up-down', rate:'1/16', octaves:2, gate:0.6}.")]
    fn set_arp(&self, Parameters(args): Parameters<SetArpArgs>) -> Result<CallToolResult, McpError> {
        let body = json!({
            "node": args.node, "mode": args.mode, "rate": args.rate, "octaves": args.octaves,
            "gate": args.gate, "swing": args.swing, "chord": args.chord,
        });
        // Null fields mean "not given" for the shared parser.
        let mut given = body.clone();
        if let Some(o) = given.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            return Self::ui_bridge_tool("set_arp", given);
        }
        let values = match crate::arp::knob_values_from_args(&given) {
            Ok(v) => v,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        match st.graph.node(args.node) {
            None => return tool_err(format!("node {} does not exist", args.node)),
            Some(n) if !matches!(n.kind, NodeKind::Arp) => {
                return tool_err(format!("node {} ({}) is not an arp unit", args.node, name_str(&n.name)))
            }
            Some(_) => {}
        }
        let mut set = Vec::new();
        for (label, param, v) in values {
            if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param, value: v as f64 }) {
                return tool_err(e);
            }
            set.push(label.to_lowercase());
        }
        ok_json(json!({"node": args.node, "set": set}))
    }

    /// Mix system audio into the master.
    #[tool(name = "set_system_audio", description = "Mix system-audio capture (e.g. YouTube) into the master (Command::SetSystemAudio). gain 0..1, 0 mutes. In the GUI app this captures the other apps that are playing (list_app_streams), never kabelsalat's own output; headless mode has no capture source so it only stores the gain.")]
    fn set_system_audio(&self, Parameters(args): Parameters<SetSystemAudioArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let gain = args.gain.clamp(0.0, 1.0);
        if let Err(e) = send_cmd(&mut st, Command::SetSystemAudio { gain }) {
            return tool_err(e);
        }
        ok_json(json!({"gain": gain}))
    }

    /// Configure an Audio In node (device, channels, gain, monitor).
    #[tool(name = "set_audio_in", description = "Configure a live Audio In rack unit (Command::SetAudioIn + SetParam) — a mic/guitar/interface patched into the graph like a source. node is the AudioIn node id; device is a PipeWire source name from list_audio_devices (null/empty = default input); channels is 'in_1' (left/mic), 'in_2' (right/Hi-Z), or 'stereo'; gain 0..1 sets the input gain; monitor true adds the input (post-gain) straight to the master for low-latency monitoring. Omitted fields keep their value.")]
    fn set_audio_in(&self, Parameters(args): Parameters<SetAudioInArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let Some(node) = st.graph.nodes.iter().find(|n| n.id == args.node) else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        if !matches!(node.kind, NodeKind::AudioIn { .. }) {
            return tool_err(format!("node {} is not an Audio In node (kind {:?})", args.node, node.kind));
        }
        // device/channels → the node kind (structural, published, remapped).
        if args.device.is_some() || args.channels.is_some() {
            let (cur_dev, cur_ch) = match &node.kind {
                NodeKind::AudioIn { device, channels } => (device.clone(), *channels),
                _ => unreachable!(),
            };
            let device = match &args.device {
                Some(d) if !d.is_empty() => Some(d.clone()),
                Some(_) => None, // empty string → default input
                None => cur_dev,
            };
            let channels = args.channels.as_deref().map(|s| parse_audio_ch(Some(s))).unwrap_or(cur_ch);
            if let Err(e) = send_cmd(&mut st, Command::SetAudioIn { node: args.node, device, channels: Some(channels) }) {
                return tool_err(e);
            }
            // Mirror into the local model.
            if let Some(n) = st.graph.nodes.iter_mut().find(|n| n.id == args.node) {
                if let NodeKind::AudioIn { device: d, channels: c } = &mut n.kind {
                    *d = match &args.device { Some(s) if !s.is_empty() => Some(s.clone()), Some(_) => None, None => d.clone() };
                    *c = channels;
                }
            }
        }
        // gain / monitor → engine params (SetParam, session-persisted).
        if let Some(gain) = args.gain {
            let g = gain.clamp(0.0, 1.0);
            if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: crate::engine::P_AUDIO_IN_GAIN, value: g }) {
                return tool_err(e);
            }
        }
        if let Some(mon) = args.monitor {
            let v = if mon { 1.0 } else { 0.0 };
            if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: crate::engine::P_AUDIO_IN_MONITOR, value: v }) {
                return tool_err(e);
            }
        }
        let summary = st.graph.nodes.iter().find(|n| n.id == args.node).map(|n| match &n.kind {
            NodeKind::AudioIn { device, channels } => {
                serde_json::json!({"node": n.id, "device": device, "channels": ch_kind_str(*channels)})
            }
            _ => serde_json::json!({}),
        }).unwrap_or(serde_json::json!({}));
        ok_json(summary)
    }

    #[tool(name = "get_midi_clock", description = "Read the MIDI Clock Send (kabelsalat as the master MIDI beat clock, 24 per beat, plus Start/Stop): enabled, offset_ms, latency_mode (none|output|input), song_position, every MIDI out port (live/wired) and the virtual port apps can subscribe to. Needs the running app (kabelsalat mcp --attach).")]
    fn get_midi_clock(&self) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("get_midi_clock", json!({}))
    }

    #[tool(name = "set_midi_clock", description = "Configure the MIDI Clock Send: enabled; offset_ms (negative = earlier, a pre-roll is added at play); latency_mode none (align to the audio output) | output (include the sound device's output latency, for gear mixed externally) | input (align to the audio input, for gear fed back into kabelsalat); song_position (SPP + Continue when resuming); output + output_enabled (wire the clock to one MIDI out port, by part of its name from get_midi_clock). Starts with the next transport start. Needs the running app.")]
    fn set_midi_clock(&self, Parameters(args): Parameters<SetMidiClockArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("set_midi_clock", json!({
            "enabled": args.enabled, "offset_ms": args.offset_ms, "latency_mode": args.latency_mode,
            "song_position": args.song_position, "output": args.output, "output_enabled": args.output_enabled,
        }))
    }

    #[tool(name = "list_audio_outputs", description = "List the sound cards kabelsalat can play to (PipeWire sinks), the one it plays to now and the user's choice. Needs the running app (kabelsalat mcp --attach).")]
    fn list_audio_outputs(&self) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("list_audio_outputs", json!({}))
    }

    #[tool(name = "set_audio_output", description = "Choose the sound card kabelsalat plays to (name from list_audio_outputs; omit/empty = system default). Moves the live stream, remembered across restarts. Needs the running app.")]
    fn set_audio_output(&self, Parameters(args): Parameters<SetAudioOutputArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("set_audio_output", json!({"output": args.output}))
    }

    #[tool(name = "set_buffer_size", description = "Change the audio output buffer size in frames (64, 128, 256, 512, 1024, or 2048). Smaller reduces latency; larger gives more headroom against dropouts (xruns). Reopens the audio stream live and is remembered across restarts. Needs the running app.")]
    fn set_buffer_size(&self, Parameters(args): Parameters<SetBufferSizeArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("set_buffer_size", json!({"frames": args.frames}))
    }

    #[tool(name = "list_app_streams", description = "List other apps playing audio right now (YouTube, Spotify, …): index, app, title, volume, muted — the mixer's System audio strips. Needs the running app.")]
    fn list_app_streams(&self) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("list_app_streams", json!({}))
    }

    #[tool(name = "set_app_stream", description = "Set another app's playback volume (0..1.5, 1.0 = 100 %) and/or mute, by index from list_app_streams. Needs the running app.")]
    fn set_app_stream(&self, Parameters(args): Parameters<SetAppStreamArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool("set_app_stream", json!({"index": args.index, "volume": args.volume, "muted": args.muted}))
    }

    /// List live audio input devices.
    #[tool(name = "list_audio_devices", description = "List live audio input devices (microphones, guitars, interfaces) available to Audio In units: the PipeWire source name (use as set_audio_in device), a readable label, channel count and which is the default.")]
    fn list_audio_devices(&self) -> Result<CallToolResult, McpError> {
        let devs = crate::audio_in::list_input_devices();
        ok_json(json!({"devices": devs.iter().map(|d| serde_json::json!({
            "name": d.name, "raw": d.raw, "channels": d.channels, "default": d.default,
        })).collect::<Vec<_>>()}))
    }

    /// Change the file player's mix gain and/or its AUM FilePlayer options.
    #[tool(name = "set_player", description = "Change the backing-track file player: mix gain 0..1 (0 mutes) and/or the AUM FilePlayer options loop (seamless restart), sync (playback follows the transport tempo as varispeed from the file's original BPM; play starts at the next bar while the transport rolls), beats (Number of Beats: loop length, so an aligned loop never drifts), offset (Beat Offset, +/-), bpm (original BPM). Give at least one. Start a file with play_file (needs the running app).")]
    fn set_player(&self, Parameters(args): Parameters<SetPlayerArgs>) -> Result<CallToolResult, McpError> {
        if args.gain.is_none() && args.loop_on.is_none() && args.sync.is_none() && args.beats.is_none() && args.offset.is_none() && args.bpm.is_none() {
            return tool_err("set_player needs gain and/or loop, sync, beats, offset, bpm");
        }
        let mut st = self.state.lock().unwrap();
        if let Some(g) = args.gain {
            if let Err(e) = send_cmd(&mut st, Command::SetPlayer { gain: g.clamp(0.0, 1.0) }) {
                return tool_err(e);
            }
        }
        if args.loop_on.is_some() || args.sync.is_some() || args.beats.is_some() || args.offset.is_some() || args.bpm.is_some() {
            let cmd = Command::SetPlayerCfg {
                loop_on: args.loop_on,
                sync: args.sync,
                beats: args.beats,
                offset: args.offset,
                bpm: args.bpm,
            };
            if let Err(e) = send_cmd(&mut st, cmd) {
                return tool_err(e);
            }
        }
        ok_json(json!({
            "gain": args.gain.map(|g| g.clamp(0.0, 1.0)), "loop": args.loop_on, "sync": args.sync,
            "beats": args.beats, "offset": args.offset, "bpm": args.bpm,
        }))
    }

    /// Start a backing track in the running app (the path crosses the UI
    /// bridge; the fixed-size command bus cannot carry it).
    #[tool(name = "play_file", description = "Play a backing-track audio file (WAV: 16/24-bit PCM or 32-bit float, e.g. a kabelsalat recording) mixed into the master. path is an absolute file path; gain 0..1 (default 0.8). Optional AUM FilePlayer options: loop, sync (follow the transport tempo; starts at the next bar while it rolls), beats (loop length in beats), offset (beats, +/-), bpm (original tempo). Needs the running app (kabelsalat mcp --attach).")]
    fn play_file(&self, Parameters(args): Parameters<PlayFileArgs>) -> Result<CallToolResult, McpError> {
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if !remote {
            return tool_err("play_file needs the running app (kabelsalat mcp --attach) — headless mode has no file player");
        }
        let mut a = json!({"path": args.path, "gain": args.gain.unwrap_or(0.8)});
        for (k, v) in [("loop", args.loop_on.map(Value::from)), ("sync", args.sync.map(Value::from)),
            ("beats", args.beats.map(Value::from)), ("offset", args.offset.map(Value::from)), ("bpm", args.bpm.map(Value::from))] {
            if let Some(v) = v {
                a[k] = v;
            }
        }
        Self::ui_bridge_tool("play_file", a)
    }

    /// Stop the backing track.
    #[tool(name = "stop_file", description = "Stop the backing-track file player (the file and its settings stay; play_file restarts it).")]
    fn stop_file(&self) -> Result<CallToolResult, McpError> {
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            return Self::ui_bridge_tool("stop_file", json!({}));
        }
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::SetPlayerEnable { on: false }) {
            return tool_err(e);
        }
        ok_json(json!({"stopped": true}))
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

    /// List instrument profiles and what each device has.
    #[tool(name = "list_instrument_profiles", description = "List the instrument profiles (what every controller of a hardware instrument means, e.g. the Nord Electro 2: drawbars, rotary, percussion, drive) with the rack nodes each would drive and which CC lands on which parameter, and the hardware MIDI devices with the profile each has now. Attached: the app's ports (chosen_by: pick = set by hand or by an agent, port name = recognised from the port's name). Headless: no MIDI ports are open, so devices lists the indices a profile was set for.")]
    fn list_instrument_profiles(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("list_instrument_profiles", json!({}));
        }
        let profiles = crate::instruments::load_all().0;
        if let Err(e) = sync_instruments(&mut st, &profiles) {
            return tool_err(e);
        }
        let devices: Vec<Value> = st.device_instruments.iter().map(|(&d, id)| {
            let profile = profiles.iter().find(|p| &p.id == id);
            let mut v = crate::instruments::device_report(d, &format!("hw:{d}"), profile, &st.instrument_bound, &st.graph);
            v["chosen_by"] = json!("pick");
            v
        }).collect();
        ok_json(json!({
            "profiles": crate::instruments::listing(&profiles, &st.graph),
            "devices": devices,
            "more_profiles": "MIDI Guide CSV files in ~/.config/kabelsalat/instruments/ (read at start)",
        }))
    }

    /// Pick the instrument profile of a hardware device.
    #[tool(name = "set_device_instrument", description = "Say which hardware instrument is on a MIDI port: device is the hardware device index (the N of source 'hw:N'), profile a profile id or name from list_instrument_profiles, or 'none' for no profile. The profile's controllers then drive the rack (all drawbars, rotary, percussion and drive of an organ the device plays) without a MIDI learn per knob; 'none' takes back exactly what the profile bound. Attached: the app remembers the choice for that port across sessions and keeps the bindings in step with the rack. Headless: the choice lasts for this server and the bindings are brought in step with the rack on each call of this tool or list_instrument_profiles.")]
    fn set_device_instrument(&self, Parameters(args): Parameters<SetDeviceInstrumentArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("set_device_instrument", json!({"device": args.device, "profile": args.profile}));
        }
        if args.device >= crate::engine::MAX_HW_DEVICES {
            return tool_err(format!("no hardware MIDI device {}: indices are 0..{}", args.device, crate::engine::MAX_HW_DEVICES - 1));
        }
        let profiles = crate::instruments::load_all().0;
        let profile = match crate::instruments::find(&profiles, &args.profile) {
            Ok(p) => p,
            Err(e) => return tool_err(e),
        };
        match profile {
            Some(p) => st.device_instruments.insert(args.device, p.id.clone()),
            None => st.device_instruments.remove(&args.device),
        };
        if let Err(e) = sync_instruments(&mut st, &profiles) {
            return tool_err(e);
        }
        ok_json(crate::instruments::device_report(args.device, &format!("hw:{}", args.device), profile, &st.instrument_bound, &st.graph))
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

    fn parse_cc_target(target: Option<&str>) -> Result<crate::matrix::CcTarget, String> {
        crate::matrix::CcTarget::parse(target)
    }

    /// The param of the mixer control `mix` names on a node of `kind`: a
    /// MasterOut node is its rig's bus, any other node its own source strip.
    fn resolve_headless_mix(kind: &NodeKind, mix: &str) -> Result<u32, String> {
        let ctl = crate::matrix::MixControl::parse(mix)?;
        ctl.fits(matches!(kind, NodeKind::MasterOut))?;
        Ok(ctl.param())
    }

    /// Same tables as the rack faces: `engine::knob_specs` (labels like
    /// "RESO"/"CEIL" are canonical, long names are aliases), and for an LV2
    /// plugin its main controls, by full name or knob label, plus standard
    /// instrument MIDI CCs ("swell", "sustain", "soft", "mod").
    fn resolve_headless_knob(kind: &NodeKind, knob: &str) -> Result<(u32, crate::matrix::CcTarget), String> {
        if let Some(spec) = crate::engine::find_knob_spec(kind, knob) {
            let p = spec.param.ok_or_else(|| format!("knob '{}' is local-only", spec.label))?;
            return Ok((p, crate::matrix::CcTarget::Knob));
        }
        if let NodeKind::Lv2 { uri } = kind {
            if let Some(c) = crate::lv2::main_controls_blocking(uri.as_str()).find(knob) {
                return Ok((c.id, crate::matrix::CcTarget::Knob));
            }
        }
        if matches!(kind, NodeKind::Clap { .. } | NodeKind::Lv2 { .. }) {
            if let Some(cc) = crate::matrix::parse_midi_cc_name(knob) {
                return Ok((cc, crate::matrix::CcTarget::MidiCc));
            }
        }
        Err(format!("no knob '{knob}' on this node"))
    }

    /// Bind or unbind one CC on a matrix rule.
    #[tool(name = "set_cc_map", description = "Bind ONE MIDI CC (0..=127) on source ('keys' | 'hw:<device index>') → dest node to a parameter id or rack unit knob (target='plugin'|'knob', or knob='LEVEL', or knob=<a main control's name from list_params>), or to a MIXER control with mix='vol'|'pan'|'mute'|'solo': on a rig's Master out node that is the rig bus (fader, pan, mute, solo), on a source node the source strip's pan, mute or solo (a source's fader is knob='LEVEL'; mute and solo are on in the upper half of the controller's travel). Optional min/max (0..1) limit how far the controller moves the parameter: its whole travel spans min..max (a pedal on organ drive that must stay below 60 %: max=0.6). Omit param/knob to unbind that CC.")]
    fn set_cc_map(&self, Parameters(args): Parameters<SetCcMapArgs>) -> Result<CallToolResult, McpError> {
        if args.cc > 127 {
            return tool_err("cc must be 0..=127");
        }
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool(
                "set_cc_map",
                json!({
                    "node": args.node,
                    "source": args.source,
                    "cc": args.cc,
                    "param": args.param,
                    "target": args.target,
                    "knob": args.knob,
                    "mix": args.mix,
                    "min": args.min,
                    "max": args.max,
                    "in_min": args.in_min,
                    "in_max": args.in_max,
                }),
            );
        }
        let (lo, hi) = (args.min.unwrap_or(0.0), args.max.unwrap_or(1.0));
        if !(0.0..=1.0).contains(&lo) || !(0.0..=1.0).contains(&hi) || lo > hi {
            return tool_err("min and max must be 0..1 with min <= max");
        }
        let (in_lo, in_hi) = (args.in_min.unwrap_or(0), args.in_max.unwrap_or(127));
        if in_lo > 127 || in_hi > 127 || in_lo > in_hi {
            return tool_err("in_min and in_max must be 0..=127 with in_min <= in_max");
        }
        let scale = crate::matrix::CcScale::range(lo, hi).with_input_range(in_lo, in_hi);
        let source = match Self::parse_rule_source(&args.source) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let mut target = match Self::parse_cc_target(args.target.as_deref()) {
            Ok(t) => t,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = refresh_remote(&mut st) {
            return tool_err(e);
        }
        let Some(node_kind) = st.graph.node(args.node).map(|n| n.kind.clone()) else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let param = match (args.mix.as_deref(), args.knob.as_deref()) {
            (Some(m), _) => match Self::resolve_headless_mix(&node_kind, m) {
                Ok(p) => {
                    target = crate::matrix::CcTarget::Mix;
                    Some(p)
                }
                Err(e) => return tool_err(e),
            },
            (None, Some(k)) => match Self::resolve_headless_knob(&node_kind, k) {
                Ok((p, t)) => {
                    target = t;
                    Some(p)
                }
                Err(e) => return tool_err(e),
            },
            (None, None) => args.param,
        };
        // target='mix' with a bare param must name a control of that strip.
        if let (crate::matrix::CcTarget::Mix, Some(p)) = (target, param) {
            let fits = crate::matrix::MixControl::from_param(p)
                .ok_or_else(|| format!("no mixer control {p}: pass mix='vol'|'pan'|'mute'|'solo'|'send1'..'send8'"))
                .and_then(|c| c.fits(matches!(node_kind, NodeKind::MasterOut)).map_err(String::from));
            if let Err(e) = fits {
                return tool_err(e);
            }
        }
        if let (crate::matrix::CcTarget::MidiCc, Some(p)) = (target, param) {
            if p > 127 {
                return tool_err("target='cc' param must be 0..=127");
            }
        }
        if let Err(e) = send_cmd(&mut st, Command::MatrixCcMap { source, dest: args.node, cc: args.cc, param, target, scale }) {
            return tool_err(e);
        }
        let entry = st.cc_maps.entry((source, args.node)).or_default();
        match param {
            Some(p) => {
                if let Some(m) = entry.iter_mut().find(|m| m.cc == args.cc) {
                    m.param = p;
                    m.target = target;
                    m.scale = scale;
                } else {
                    entry.push(crate::matrix::CcMapping { cc: args.cc, param: p, target, scale });
                }
            }
            None => entry.retain(|m| m.cc != args.cc),
        }
        let map: Vec<Value> = st.cc_maps.get(&(source, args.node))
            .map(|v| v.iter().map(|m| json!({"cc": m.cc, "param": m.param, "target": m.target.as_str(), "range": m.scale.range_json(), "input_range": m.scale.input_range_json()})).collect())
            .unwrap_or_default();
        if map.is_empty() {
            st.cc_maps.remove(&(source, args.node));
        }
        ok_json(json!({"node": args.node, "source": args.source, "cc": args.cc, "param": param, "target": target.as_str(), "cc_map": map}))
    }

    /// Clear one or all CC bindings on a matrix rule.
    #[tool(name = "clear_cc_map", description = "Clear one CC binding (when cc is provided) or all CC bindings (when cc is omitted, optionally filtered by target='plugin'|'knob'|'mix', knob='LEVEL' or mix='mute') on source → node.")]
    fn clear_cc_map(&self, Parameters(args): Parameters<ClearCcMapArgs>) -> Result<CallToolResult, McpError> {
        if let Some(cc) = args.cc {
            if cc > 127 {
                return tool_err("cc must be 0..=127");
            }
        }
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool(
                "clear_cc_map",
                json!({
                    "node": args.node,
                    "source": args.source,
                    "cc": args.cc,
                    "target": args.target,
                    "knob": args.knob,
                    "mix": args.mix,
                }),
            );
        }
        let source = match Self::parse_rule_source(&args.source) {
            Ok(s) => s,
            Err(e) => return tool_err(e),
        };
        let target_filter = match args.target.as_deref() {
            Some(t) => match Self::parse_cc_target(Some(t)) {
                Ok(tt) => Some(tt),
                Err(e) => return tool_err(e),
            },
            None => None,
        };
        let mut st = self.state.lock().unwrap();
        let Some(node_kind) = st.graph.node(args.node).map(|n| n.kind.clone()) else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let knob_param = match (args.mix.as_deref(), args.knob.as_deref()) {
            (Some(m), _) => match Self::resolve_headless_mix(&node_kind, m) {
                Ok(p) => Some((p, crate::matrix::CcTarget::Mix)),
                Err(e) => return tool_err(e),
            },
            (None, Some(k)) => match Self::resolve_headless_knob(&node_kind, k) {
                Ok(pair) => Some(pair),
                Err(e) => return tool_err(e),
            },
            (None, None) => None,
        };
        let ccs: Vec<u8> = match args.cc {
            Some(c) => vec![c],
            None => st.cc_maps.get(&(source, args.node))
                .map(|v| {
                    v.iter()
                        .filter(|m| {
                            knob_param.map_or(true, |(kp, kt)| m.param == kp && m.target == kt)
                                && target_filter.map_or(true, |tf| m.target == tf)
                        })
                        .map(|m| m.cc)
                        .collect()
                })
                .unwrap_or_default(),
        };
        for cc in ccs {
            if let Err(e) = send_cmd(&mut st, Command::MatrixCcMap { source, dest: args.node, cc, param: None, target: crate::matrix::CcTarget::PluginParam, scale: Default::default() }) {
                return tool_err(e);
            }
            if let Some(v) = st.cc_maps.get_mut(&(source, args.node)) {
                v.retain(|m| m.cc != cc);
            }
        }
        if st.cc_maps.get(&(source, args.node)).is_some_and(|v| v.is_empty()) {
            st.cc_maps.remove(&(source, args.node));
        }
        let map: Vec<Value> = st.cc_maps.get(&(source, args.node))
            .map(|v| v.iter().map(|m| json!({"cc": m.cc, "param": m.param, "target": m.target.as_str()})).collect())
            .unwrap_or_default();
        ok_json(json!({"node": args.node, "source": args.source, "cc_map": map}))
    }

    /// List active CC bindings.
    #[tool(name = "list_cc_maps", description = "List active MIDI CC → parameter/knob bindings (plus armed MIDI-learn state when attached to the GUI app).")]
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
                "bindings": v.iter().map(|m| json!({"cc": m.cc, "param": m.param, "target": m.target.as_str(), "range": m.scale.range_json(), "input_range": m.scale.input_range_json()})).collect::<Vec<_>>(),
            }))
            .collect();
        ok_json(json!({"cc_maps": entries}))
    }

    /// List bindable/editable parameter ids, names, values, ranges and options for a node.
    #[tool(name = "list_params", description = "Inspect a node's parameters for set_param / set_cc_map / learn_cc with values, musical text, defaults, min..max ranges, units, and stepped options. main_controls comes first. Supports filtering by query, module (CLAP), only_modified, include_options, and limit. CLAP plugin params require --attach to the running GUI app.")]
    fn list_params(&self, Parameters(args): Parameters<ListParamsArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool("list_params", json!({
                "node": args.node,
                "query": args.query,
                "module": args.module,
                "only_modified": args.only_modified,
                "include_options": args.include_options,
                "limit": args.limit,
            }));
        }
        let st = self.state.lock().unwrap();
        let Some(n) = st.graph.node(args.node) else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let q = args.query.unwrap_or_default().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let limit = args.limit.map(|l| l.clamp(1, 200)).unwrap_or(50);

        let (total, params): (usize, Vec<Value>) = match &n.kind {
            NodeKind::Dsp(dk) => {
                let specs = crate::dsp::dsp_param_specs(dk);
                let list: Vec<Value> = specs
                    .iter()
                    .filter(|s| {
                        if words.is_empty() { return true; }
                        let hay = format!("{} {} {} {}", s.label, s.name, s.description, s.unit).to_lowercase();
                        words.iter().all(|w| hay.contains(w))
                    })
                    .take(limit)
                    .map(|s| json!({
                        "id": s.engine_param,
                        "key": s.name,
                        "name": s.label,
                        "value": s.default,
                        "text": crate::dsp::format_dsp_value(dk, s.native_param, s.default),
                        "default": s.default,
                        "min": 0.0,
                        "max": 1.0,
                        "unit": s.unit,
                        "range": s.range,
                        "description": s.description,
                    }))
                    .collect();
                (specs.len(), list)
            }
            nk if crate::organ::is_organ(nk) => {
                let specs = crate::organ::param_specs();
                let list: Vec<Value> = specs
                    .iter()
                    .filter(|s| {
                        if words.is_empty() { return true; }
                        let hay = format!("{} {} {}", s.name, s.range, s.description).to_lowercase();
                        words.iter().all(|w| hay.contains(w))
                    })
                    .take(limit)
                    .map(|s| json!({
                        "id": s.param,
                        "name": s.name,
                        "value": s.default,
                        "text": crate::organ::format_value(s.param, s.default),
                        "default": s.default,
                        "min": 0.0,
                        "max": 1.0,
                        "stepped": s.stepped,
                        "range": s.range,
                        "description": s.description,
                    }))
                    .collect();
                (specs.len(), list)
            }
            NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: crate::control::MidiFxPlugin::Lv2 { uri } } => {
                let infos = crate::lv2::control_infos_blocking(uri.as_str());
                let list: Vec<Value> = infos
                    .iter()
                    .filter(|i| {
                        if words.is_empty() { return true; }
                        let hay = format!("{} {} {}", i.name, i.symbol, i.unit).to_lowercase();
                        words.iter().all(|w| hay.contains(w))
                    })
                    .take(limit)
                    .map(|i| json!({
                        "id": i.index as u32,
                        "symbol": i.symbol,
                        "name": i.name,
                        "value": i.default,
                        "text": crate::lv2::format_lv2_control_value(i, i.default),
                        "default": i.default,
                        "min": i.min,
                        "max": i.max,
                        "unit": i.unit,
                        "toggled": i.toggled,
                        "integer": i.integer,
                        "logarithmic": i.logarithmic,
                        "options": i.scale_points.iter().map(|sp| json!({"value": sp.value, "label": sp.label})).collect::<Vec<_>>(),
                    }))
                    .collect();
                (infos.len(), list)
            }
            NodeKind::Clap { .. } => {
                return tool_err("CLAP parameter listing needs the running app (kabelsalat mcp --attach)");
            }
            other => {
                let choices: Vec<(u32, String)> = match other {
                    NodeKind::MasterOut => vec![(3, "Volume".into()), (4, "Warmth".into())],
                    NodeKind::AudioIn { .. } => vec![(crate::engine::P_AUDIO_IN_GAIN, "Gain".into()), (crate::engine::P_AUDIO_IN_MONITOR, "Monitor".into())],
                    NodeKind::SystemAudio => vec![(crate::engine::P_SYSTEM_GAIN, "Gain".into())],
                    NodeKind::Arp => vec![
                        (crate::arp::P_ARP_MODE, "Mode (up/down/up-down/chord/random)".into()),
                        (crate::arp::P_ARP_RATE, "Rate (1/4|1/8|1/8T|1/16|1/16T|1/32)".into()),
                        (crate::arp::P_ARP_OCTAVE, "Octaves (1-4)".into()),
                        (crate::arp::P_ARP_GATE, "Gate".into()),
                        (crate::arp::P_ARP_SWING, "Swing".into()),
                        (crate::arp::P_ARP_CHORD, "Chord (maj|min|sus2|sus4|7|maj7|min7|power)".into()),
                    ],
                    NodeKind::MidiFx { plugin: crate::control::MidiFxPlugin::Builtin { .. } } => {
                        vec![(crate::engine::P_MIDIFX_SEMI, "Transpose".into())]
                    }
                    _ => Vec::new(),
                };
                let tot = choices.len();
                let list: Vec<Value> = choices
                    .into_iter()
                    .filter(|(_, name)| words.iter().all(|w| name.to_lowercase().contains(w)))
                    .take(limit)
                    .map(|(id, name)| json!({"id": id, "name": name}))
                    .collect();
                (tot, list)
            }
        };
        let mut knobs: Vec<Value> = crate::engine::knob_specs(&n.kind)
            .iter()
            .filter_map(|k| k.param.map(|p| json!({"label": k.label, "param": p, "target": "knob"})))
            .collect();
        // No plugin runs headless: an LV2 control shows the default it starts at.
        let found = match &n.kind {
            NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: crate::control::MidiFxPlugin::Lv2 { uri } } => {
                crate::lv2::main_controls_blocking(uri.as_str())
            }
            _ => Default::default(),
        };
        let main_controls: Vec<Value> = found
            .controls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let knob = (i < crate::main_controls::FACE_MAIN_CONTROLS).then(|| crate::main_controls::knob_label(&c.name));
                json!({
                    "page": c.page, "name": c.name, "id": c.id, "value": c.default, "text": format!("{:.3}", c.default),
                    "min": c.min, "max": c.max, "target": "knob", "knob": knob,
                })
            })
            .collect();
        knobs.extend(found.face().iter().map(|c| json!({"label": crate::main_controls::knob_label(&c.name), "param": c.id, "target": "knob"})));
        let source = (!main_controls.is_empty()).then_some(found.source.as_str());
        ok_json(json!({
            "node": args.node,
            "main_controls_source": source,
            "main_controls": main_controls,
            "total": total,
            "params": params,
            "knobs": knobs,
        }))
    }

    /// Deep technical and musical inspection of any plugin (built-in DSP, CLAP, LV2, catalog, or live node).
    #[tool(name = "inspect_plugin", description = "Inspect any plugin's architecture, musical role, modules, and parameter specifications (ranges, defaults, units, scale points). Pass `query` (plugin name/id/URI/DSP kind) or `node` (live rack node id).")]
    fn inspect_plugin(&self, Parameters(args): Parameters<InspectPluginArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool("inspect_plugin", json!({
                "query": args.query,
                "format": args.format,
                "node": args.node,
            }));
        }
        if let Some(node) = args.node {
            return self.list_params(Parameters(ListParamsArgs {
                node,
                query: args.query,
                include_options: Some(true),
                limit: Some(80),
                ..Default::default()
            }));
        }
        let format = args.format.unwrap_or_else(|| "all".into()).to_ascii_lowercase();
        let query = args.query.unwrap_or_default();
        let q_low = query.trim().to_ascii_lowercase();
        let words: Vec<&str> = q_low.split_whitespace().collect();
        if words.is_empty() {
            let builtins: Vec<Value> = crate::dsp::dsp_catalog()
                .iter()
                .map(|s| json!({"kind": s.kind, "name": s.name, "category": s.category, "summary": s.summary, "param_count": s.params.len()}))
                .collect();
            let catalog: Vec<Value> = crate::catalog::CATALOG
                .iter()
                .map(|c| json!({"id": c.id, "name": c.name, "format": c.format.label(), "description": c.description}))
                .collect();
            return ok_json(json!({"builtin_dsp": builtins, "catalog": catalog}));
        }
        let mut matches: Vec<Value> = Vec::new();
        if matches!(format.as_str(), "all" | "builtin") {
            let exact = crate::dsp::dsp_kind_from_str(&query);
            for spec in crate::dsp::dsp_catalog() {
                let hay = format!("{} {} {} {}", spec.kind, spec.name, spec.category, spec.summary).to_lowercase();
                if exact.as_ref().is_some_and(|dk| crate::dsp::dsp_spec(dk).kind == spec.kind)
                    || words.iter().all(|w| hay.contains(w))
                {
                    matches.push(json!({
                        "format": "builtin",
                        "kind": spec.kind,
                        "name": spec.name,
                        "category": spec.category,
                        "summary": spec.summary,
                        "latency_ms": spec.latency_ms,
                        "params": spec.params.iter().map(|p| json!({
                            "id": p.engine_param, "key": p.name, "name": p.label,
                            "unit": p.unit, "range": p.range, "default": p.default, "description": p.description,
                        })).collect::<Vec<_>>(),
                    }));
                }
            }
        }
        if matches!(format.as_str(), "all" | "catalog" | "clap" | "lv2") {
            for c in crate::catalog::CATALOG {
                let hay = format!("{} {} {} {} {} {}", c.id, c.name, c.description, c.about, c.clap_id.unwrap_or(""), c.lv2_uris.join(" ")).to_lowercase();
                if words.iter().all(|w| hay.contains(w)) {
                    let mut entry = json!({
                        "format": "catalog",
                        "plugin_format": c.format.label(),
                        "id": c.id,
                        "name": c.name,
                        "description": c.description,
                        "about": c.about,
                        "clap_id": c.clap_id,
                        "lv2_uris": c.lv2_uris,
                    });
                    if c.lv2_uris.contains(&crate::organ::B_SYNTH) {
                        entry["params"] = Value::Array(
                            crate::organ::param_specs().iter().map(|p| json!({
                                "id": p.param, "name": p.name, "range": p.range, "default": p.default, "stepped": p.stepped,
                            })).collect()
                        );
                    }
                    matches.push(entry);
                }
            }
        }
        ok_json(json!({"query": query, "count": matches.len(), "matches": matches}))
    }

    /// Reset all or filtered parameters of a node back to defaults.
    #[tool(name = "reset_params", description = "Reset all or matching (`module`/`query`) parameters of a node back to their defaults.")]
    fn reset_params(&self, Parameters(args): Parameters<ResetParamsArgs>) -> Result<CallToolResult, McpError> {
        let remote = {
            let st = self.state.lock().unwrap();
            matches!(st.backend, Backend::Remote)
        };
        if remote {
            return Self::ui_bridge_tool("reset_params", json!({
                "node": args.node,
                "module": args.module,
                "query": args.query,
            }));
        }
        let mut st = self.state.lock().unwrap();
        let Some(node) = st.graph.node(args.node).cloned() else {
            return tool_err(format!("node {} does not exist", args.node));
        };
        let q = args.query.unwrap_or_default().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let mut reset_names = Vec::new();
        match &node.kind {
            NodeKind::Dsp(dk) => {
                for spec in crate::dsp::dsp_param_specs(dk) {
                    if !words.is_empty() && !words.iter().all(|w| format!("{} {}", spec.label, spec.name).to_lowercase().contains(w)) {
                        continue;
                    }
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: spec.engine_param, value: spec.default as f64 }) {
                        return tool_err(e);
                    }
                    reset_names.push(spec.label.to_string());
                }
            }
            nk if crate::organ::is_organ(nk) => {
                for spec in crate::organ::param_specs() {
                    if !words.is_empty() && !words.iter().all(|w| spec.name.to_lowercase().contains(w)) {
                        continue;
                    }
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: spec.param, value: spec.default as f64 }) {
                        return tool_err(e);
                    }
                    reset_names.push(spec.name.to_string());
                }
            }
            NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: crate::control::MidiFxPlugin::Lv2 { uri } } => {
                for info in crate::lv2::control_infos_blocking(uri.as_str()) {
                    if !words.is_empty() && !words.iter().all(|w| format!("{} {}", info.name, info.symbol).to_lowercase().contains(w)) {
                        continue;
                    }
                    let def_norm = crate::lv2::lv2_native_to_norm(&info, info.default) as f64;
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: info.index as u32, value: def_norm }) {
                        return tool_err(e);
                    }
                    reset_names.push(info.name);
                }
            }
            NodeKind::MasterOut => {
                for &(pid, pname, def) in &[(crate::engine::P_VOL, "Volume", 0.8), (crate::engine::P_WARM, "Warmth", 0.25)] {
                    if !words.is_empty() && !words.iter().all(|w| pname.to_lowercase().contains(w)) { continue; }
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: pid, value: def }) {
                        return tool_err(e);
                    }
                    reset_names.push(pname.to_string());
                }
            }
            NodeKind::Clap { .. } => {
                return tool_err("CLAP parameter reset needs the running app (kabelsalat mcp --attach)");
            }
            _ => return tool_err("this node has no resettable parameters"),
        }
        ok_json(json!({"ok": true, "node": args.node, "reset_count": reset_names.len(), "params": reset_names}))
    }

    /// Arm MIDI-learn in the running GUI app.
    #[tool(name = "learn_cc", description = "Arm MIDI-learn in the running GUI app for (source, node, param/target, knob, or mix='vol'|'pan'|'mute'|'solo' for a mixer control) so the next incoming hardware CC binds to it. Pass cancel=true to disarm. Needs --attach.")]
    fn learn_cc(&self, Parameters(args): Parameters<LearnCcArgs>) -> Result<CallToolResult, McpError> {
        Self::ui_bridge_tool(
            "learn_cc",
            json!({
                "node": args.node,
                "source": args.source,
                "param": args.param,
                "target": args.target,
                "knob": args.knob,
                "mix": args.mix,
                "cancel": args.cancel,
            }),
        )
    }

    /// Per-rig mixer (the overview strip's vol/pan/mute/solo).
    #[tool(name = "set_rig_mix", description = "Per-rig mixer (Command::SetRigMix): rig index (0-based, see get_rigs), vol 0..1, pan -1..1, mute, solo. Omitted fields keep their value. Solo is exclusive: soloing a rig un-solos every other rig and mutes them. Applied at the rig's master when summing to the output; resets on graph loads.")]
    fn set_rig_mix(&self, Parameters(args): Parameters<SetRigMixArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::SetRigMix { rig: args.rig, vol: args.vol, pan: args.pan, mute: args.mute, solo: args.solo }) {
            return tool_err(e);
        }
        ok_json(json!({"rig": args.rig, "vol": args.vol, "pan": args.pan, "mute": args.mute, "solo": args.solo}))
    }

    /// Load several independent rigs at once.
    #[tool(name = "load_rigs", description = "Load multiple independent rigs (Command::LoadRigs): each rig is a full rack graph with its own MasterOut, bound to a hardware MIDI device (midi_device null = the keyboard + all devices). Graphs use the patch shape: nodes [{id, kind, name?}] and edges [{id, from:{node,port}, to:{node,port}, kind:'midi'|'audio'}]; kinds are 'Keys', 'MasterOut', 'SystemAudio', {Clap:{plugin_id}}, {Dsp:'delay'|'reverb'|...}, {Lv2:{uri}}; names/plugin ids may be plain strings. In the running app (mcp --attach) every Clap node gets its own voice, however many there are; headless mcp hosts two Surge XT voices, bound to the first Surge nodes in node-id order, and names the rest in silent_clap_nodes. Returns {loaded, active, nodes, edges}. Query the result with get_rigs.")]
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
        let mut summary = summary;
        note_silent_claps(&st, &mut summary);
        ok_json(summary)
    }

    /// Add a standard rig to the running setup.
    #[tool(name = "undo", description = "Undo the last agent job in the running app (needs kabelsalat mcp --attach): one step per in-app assistant request or per burst of attached calls served by the app (add_rig, load_sound, set_cc_map, ...). Restores graph, cables, knobs, synth sounds, matrix, rigs and mixer.")]
    fn undo(&self) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return Self::ui_bridge_tool("undo", json!({}));
        }
        tool_err("undo needs the running app (kabelsalat mcp --attach)")
    }

    #[tool(name = "redo", description = "Redo the agent job that undo reverted (needs kabelsalat mcp --attach).")]
    fn redo(&self) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return Self::ui_bridge_tool("redo", json!({}));
        }
        tool_err("redo needs the running app (kabelsalat mcp --attach)")
    }

    #[tool(name = "add_rig", description = "Add a new rig (an independent rack with its own Master): Keys -> Surge -> Master (or Keys -> <plugin_id> -> Master for any installed CLAP instrument from list_plugins; needs the running app), wired and playable (headless mcp hosts two Surge XT voices: a rig past them is returned in silent_clap_nodes). Existing rigs keep their graphs and device bindings (knob/synth state is kept by the in-app agent path only). A single-graph patch becomes rig 0. Returns {rigs, active} like get_rigs.")]
    fn add_rig(&self, Parameters(args): Parameters<AddRigArgs>) -> Result<CallToolResult, McpError> {
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            // The running app adds the rig itself: it keeps every rig's knobs
            // and synth state and loads non-Surge plugins on its UI thread.
            let mut a = json!({});
            if let Some(n) = &args.name { a["name"] = json!(n); }
            if let Some(p) = &args.plugin_id { a["plugin_id"] = json!(p); }
            return Self::ui_bridge_tool("add_rig", a);
        }
        if args.plugin_id.is_some() {
            return tool_err("add_rig with plugin_id needs the running app (kabelsalat mcp --attach)");
        }
        self.edit_rigs(None, args.name)
    }

    /// Clone an existing rig into a new independent rig.
    #[tool(name = "clone_rig", description = "Clone an existing rig (rack) into a new independent rig, copying its units, cables, knobs, plugin states, matrix routes, mixer strips, sends, MIDI device binding and rig mix (knob/plugin state copy needs the running app via --attach). rig defaults to the active rig. Returns {rigs, active}.")]
    fn clone_rig(&self, Parameters(args): Parameters<CloneRigArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            let mut a = json!({});
            if let Some(r) = args.rig { a["rig"] = json!(r); }
            if let Some(n) = &args.name { a["name"] = json!(n); }
            return Self::ui_bridge_tool("clone_rig", a);
        }
        let mut st = self.state.lock().unwrap();
        let meta = match rig_meta(&st) { Ok(m) => m, Err(e) => return tool_err(e) };
        let (graphs, _) = control::split_rigs(&st.graph);
        let active_now = st.rigs.lock().map(|s| s.active).unwrap_or(0);
        let src = args.rig.unwrap_or(active_now);
        if src >= graphs.len() {
            return tool_err(format!("no rig {src}: there are {}", graphs.len()));
        }
        if graphs.len() >= 8 {
            return tool_err("at most 8 rigs");
        }
        let mut rigs: Vec<Rig> = graphs.into_iter().enumerate().map(|(i, g)| {
            let m = meta.get(i);
            let name = m.map(|m| m.name.clone()).filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Rig {}", (b'A' + i.min(25) as u8) as char));
            Rig::new(name, g, m.and_then(|m| m.midi_device))
        }).collect();
        let used: Vec<String> = rigs.iter().map(|r| r.name.clone()).collect();
        let clone_name = args.name
            .map(|s| s.trim().chars().take(24).collect::<String>())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| control::unique_clone_rig_name(&rigs[src].name, &used));
        let cloned = Rig::new(clone_name, rigs[src].graph.clone(), rigs[src].midi_device);
        rigs.push(cloned);
        let target = rigs.len() - 1;
        let mut mixes = [crate::engine::RigMix::DEFAULT; 8];
        if let Some(gp) = &st.graph_pub {
            if let Ok(g) = gp.lock() { mixes = g.mix; }
        }
        mixes[target.min(7)] = mixes[src.min(7)];
        if let Err(e) = send_cmd(&mut st, Command::LoadRigs { rigs: rigs.clone(), active: target }) {
            return tool_err(e);
        }
        for (i, m) in mixes.iter().enumerate().take(rigs.len()) {
            let _ = send_cmd(&mut st, Command::SetRigMix {
                rig: i as u8, vol: Some(m.vol), pan: Some(m.pan), mute: Some(m.mute), solo: Some(m.solo),
            });
        }
        st.graph = merge_rigs(&rigs);
        // The clone keeps its source's mark (#198).
        if let Some(mark) = st.rig_types.get(src).copied().flatten() {
            st.rig_types.resize(target + 1, None);
            st.rig_types[target] = Some(mark);
        }
        let metas: Vec<crate::control::RigMeta> = rigs.iter()
            .map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
        if matches!(st.backend, Backend::Local(_)) {
            if let Ok(mut s) = st.rigs.lock() { s.rigs = metas.clone(); s.active = target; }
        }
        let mut result = json!({"rigs": metas, "active": target});
        note_silent_claps(&st, &mut result);
        ok_json(result)
    }

    /// Rename one rig.
    #[tool(name = "rename_rig", description = "Rename rig `rig` (index from get_rigs), e.g. 'Bass'. Up to 24 characters; kept in saved sessions.")]
    fn rename_rig(&self, Parameters(args): Parameters<RenameRigArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return Self::ui_bridge_tool("rename_rig", json!({"rig": args.rig, "name": args.name}));
        }
        let name: String = args.name.trim().chars().take(24).collect();
        if name.is_empty() { return tool_err("a rig name cannot be empty"); }
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::RenameRig { rig: args.rig.min(255) as u8, name: name_bytes(&name) }) {
            return tool_err(e);
        }
        ok_json(json!({"rig": args.rig, "name": name}))
    }

    /// Per-source strip mix (#73).
    #[tool(name = "set_strip_mix", description = "Per-SOURCE mixer (#73): pan -1..1, mute, solo on one instrument/audio-in/generator node, applied at the node's own output (its FX chain and the rig bus follow). Solo scope is WITHIN the node's rig: a soloed source mutes only the other sources of that rig. Omitted fields keep their value. Applied by the engine at the node's output; resets on graph loads. In the running app (mcp --attach) the Mixer view shows it live.")]
    fn set_strip_mix(&self, Parameters(args): Parameters<SetStripMixArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_strip_mix", json!({
                "node": args.node, "pan": args.pan, "mute": args.mute, "solo": args.solo,
            }));
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::SetStripMix { node: args.node, pan: args.pan, mute: args.mute, solo: args.solo }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "pan": args.pan, "mute": args.mute, "solo": args.solo}))
    }

    /// Add an aux bus (issue #5).
    #[tool(name = "add_bus", description = "Add an aux bus (send/return, issue #5): a mixer channel that rigs and sources SEND into (set_send) instead of being cabled to it. Its inserts (the effects cabled from its OUT) run on the sum of what was sent, and what reaches a master from there returns to the mix past the rig faders. Its fader is its LEVEL knob (param 0, linear gain); pan/mute/solo are set_strip_mix on its node. The bus is wired to its rig's Master L and R. Returns the bus's node id.")]
    fn add_bus(&self, Parameters(args): Parameters<AddBusArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("add_bus", json!({"name": args.name, "rig": args.rig}));
        }
        let rig = args.rig.unwrap_or(0);
        if !st.graph.nodes.iter().any(|n| n.rig == rig && matches!(n.kind, NodeKind::MasterOut)) {
            return tool_err(format!("rig {rig} has no master out for the bus to return to (see get_rigs)"));
        }
        let buses = st.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus))).count();
        if buses >= crate::engine::MAX_BUSES {
            return tool_err(format!("at most {} aux buses", crate::engine::MAX_BUSES));
        }
        let label = args.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| n.chars().take(24).collect::<String>())
            .unwrap_or_else(|| format!("BUS {}", (b'A' + buses as u8) as char));
        let name = name_bytes(&label);
        let kind = NodeKind::Dsp(DspKind::Bus);
        if let Err(e) = send_cmd(&mut st, Command::AddNode { kind: kind.clone(), name, rig, wire: true }) {
            return tool_err(e);
        }
        let id = st.graph.add_node(kind, name);
        st.graph.set_rig(id, rig);
        let before = st.graph.edges.len();
        st.graph.wire_bus(id);
        let cables: Vec<u32> = st.graph.edges[before..].iter().map(|e| e.id).collect();
        ok_json(json!({"bus": id, "name": label, "rig": rig, "cables": cables}))
    }

    /// Set one send into an aux bus (issue #5).
    #[tool(name = "set_send", description = "Set one send into an aux bus (issue #5): how much of a whole rig (rig = 0-based index, tapped at its rig bus) OR of one source (node = its node id) goes to `bus` (an aux bus's node id from add_bus / get_sends). level 0..1 is a linear gain (1 = 0 dB, 0.5 = -6 dB, 0 = off). pre=true taps before the source's fader, so the send keeps its level when the fader moves; pre=false (default) follows the fader. A muted or soloed-out source sends nothing. Omitted fields keep their value. A bus or one of its inserts cannot send into a bus. Resets on graph loads (the running app re-sends what its session holds).")]
    fn set_send(&self, Parameters(args): Parameters<SetSendArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_send", json!({
                "bus": args.bus, "rig": args.rig, "node": args.node, "level": args.level, "pre": args.pre,
            }));
        }
        let from = match (args.rig, args.node) {
            (Some(r), None) => SendSource::Rig(r),
            (None, Some(n)) => SendSource::Node(n),
            _ => return tool_err("set_send needs exactly one source: rig (0-based index) or node (a node id)"),
        };
        if !st.graph.is_bus(args.bus) {
            return tool_err(format!("node {} is not an aux bus (add_bus makes one; get_sends lists them)", args.bus));
        }
        match from {
            SendSource::Rig(r) if !st.graph.nodes.iter().any(|n| n.rig == r) => {
                return tool_err(format!("no rig {r} (see get_rigs)"));
            }
            SendSource::Node(n) if st.graph.node(n).is_none() => return tool_err(format!("no node {n} in the patch (see get_graph)")),
            SendSource::Node(n) if st.graph.is_bus(n) || st.graph.after_bus(n) => {
                return tool_err(format!("node {n} is a bus or one of a bus's inserts: it cannot send into a bus (feedback)"));
            }
            _ => {}
        }
        if let Err(e) = send_cmd(&mut st, Command::SetSend { from, bus: args.bus, level: args.level, pre: args.pre }) {
            return tool_err(e);
        }
        ok_json(json!({"bus": args.bus, "rig": args.rig, "node": args.node, "level": args.level, "pre": args.pre}))
    }

    /// List aux buses and sends (issue #5).
    #[tool(name = "get_sends", description = "List the aux buses (node id, name, rig) and every send into them (from a rig or a node, level as linear gain, pre/post fader), as the engine holds them. In the running app (mcp --attach) each bus also shows its level, pan, mute, solo, inserts and meter.")]
    fn get_sends(&self, Parameters(_args): Parameters<GetSendsArgs>) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("get_sends", json!({}));
        }
        let buses: Vec<Value> = st.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus)))
            .map(|n| json!({"bus": n.id, "name": name_str(&n.name), "rig": n.rig})).collect();
        let sends: Vec<Value> = st.graph_pub.as_ref().and_then(|p| p.lock().ok().map(|g| g.sends)).iter().flatten()
            .filter(|x| x.bus != NodeId::MAX)
            .map(|x| {
                let from = match x.from {
                    SendSource::Rig(r) => json!({"rig": r}),
                    SendSource::Node(n) => json!({"node": n}),
                };
                json!({"from": from, "bus": x.bus, "level": x.level, "pre": x.pre})
            }).collect();
        ok_json(json!({"buses": buses, "sends": sends, "max_buses": crate::engine::MAX_BUSES}))
    }

    /// Route a rig bus or aux bus to its own output (#147).
    #[tool(name = "set_bus_output", description = "Play one rig bus (rig = 0-based index) OR one aux bus (bus = its node id) to a sound card of its own instead of the master's (#147): the click or a monitor mix only on the headphones while the main mix goes to the PA. output: a PipeWire sink name (list_audio_outputs in the running app; `pactl list short sinks`); omit or empty = back into the master. A bus on its own output is no longer in the master mix; its fader, pan, mute and inserts still act on it. At most 4 different outputs besides the master's. While the device is not there the bus is silent, never on another output. Resets on graph loads (the running app saves it with the session and re-sends it).")]
    fn set_bus_output(&self, Parameters(args): Parameters<SetBusOutputArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_bus_output", json!({"rig": args.rig, "bus": args.bus, "output": args.output}));
        }
        let bus = match (args.rig, args.bus) {
            (Some(r), None) if st.graph.nodes.iter().any(|n| n.rig == r) => SendSource::Rig(r),
            (Some(r), None) => return tool_err(format!("no rig {r} (see get_rigs)")),
            (None, Some(n)) if st.graph.is_bus(n) => SendSource::Node(n),
            (None, Some(n)) => return tool_err(format!("node {n} is not an aux bus (get_bus_outputs lists the buses; a rig bus is rig=)")),
            _ => return tool_err("set_bus_output needs exactly one bus: rig (0-based index) or bus (an aux bus's node id)"),
        };
        let output = args.output.filter(|o| !o.is_empty());
        if let Some(o) = &output {
            if o.len() > SinkName::MAX_LEN {
                return tool_err(format!("the output name is longer than {} bytes", SinkName::MAX_LEN));
            }
            let held = st.graph_pub.as_ref().and_then(|p| p.lock().ok().map(|g| g.bus_outs));
            let mut others: Vec<String> = held.iter().flatten().filter(|x| !x.sink.is_none() && x.bus != bus)
                .map(|x| x.sink.as_str().to_string()).collect();
            others.sort();
            others.dedup();
            if !others.contains(o) && others.len() >= crate::engine::MAX_OUTS {
                return tool_err(format!("buses already play to {} outputs besides the master's, the most kabelsalat serves at once", crate::engine::MAX_OUTS));
            }
        }
        if let Err(e) = send_cmd(&mut st, Command::SetBusOutput { bus, sink: output.as_deref().map(SinkName::new) }) {
            return tool_err(e);
        }
        ok_json(json!({"rig": args.rig, "bus": args.bus, "output": output}))
    }

    /// List the outputs of rig buses and aux buses (#147).
    #[tool(name = "get_bus_outputs", description = "List every rig bus and aux bus with the output it plays to (null = the master's), as the engine holds it. In the running app (mcp --attach) each entry also has the output's label and whether the device is there now.")]
    fn get_bus_outputs(&self, Parameters(_args): Parameters<GetBusOutputsArgs>) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("get_bus_outputs", json!({}));
        }
        let held = st.graph_pub.as_ref().and_then(|p| p.lock().ok().map(|g| g.bus_outs));
        let out = |bus: SendSource| held.iter().flatten().find(|x| !x.sink.is_none() && x.bus == bus)
            .map(|x| x.sink.as_str().to_string());
        let mut rigs: Vec<u8> = st.graph.nodes.iter().map(|n| n.rig).collect();
        rigs.sort();
        rigs.dedup();
        let mut buses: Vec<Value> = rigs.iter().map(|&r| json!({"rig": r, "output": out(SendSource::Rig(r))})).collect();
        buses.extend(st.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus)))
            .map(|n| json!({"bus": n.id, "name": name_str(&n.name), "output": out(SendSource::Node(n.id))})));
        ok_json(json!({"buses": buses, "max_outputs": crate::engine::MAX_OUTS}))
    }

    /// Remove an aux bus (issue #5).
    #[tool(name = "remove_bus", description = "Remove an aux bus and every send into it (issue #5). In the running app (mcp --attach) its inserts go with it and undo brings it back; headless, remove its insert nodes with remove_node.")]
    fn remove_bus(&self, Parameters(args): Parameters<RemoveBusArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("remove_bus", json!({"bus": args.bus}));
        }
        if !st.graph.is_bus(args.bus) {
            return tool_err(format!("node {} is not an aux bus (get_sends lists them)", args.bus));
        }
        if let Err(e) = send_cmd(&mut st, Command::RemoveNode(args.bus)) {
            return tool_err(e);
        }
        st.graph.remove_node(args.bus);
        ok_json(json!({"removed": args.bus}))
    }

    /// Bypass one rack unit (#74, #305).
    #[tool(name = "set_node_bypass", description = "Bypass or enable one rack unit (#74, #305): a bypassed audio effect passes its input through unchanged (preserving PDC latency), a bypassed MIDI-FX/arp passes MIDI through unmodified, and a bypassed instrument is muted with held notes released (its knobs are kept). True = bypass, false = enable. Resets on graph loads.")]
    fn set_node_bypass(&self, Parameters(args): Parameters<SetNodeBypassArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_node_bypass", json!({"node": args.node, "bypass": args.bypass}));
        }
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        }
        let bypass = args.bypass.unwrap_or(true);
        if let Err(e) = send_cmd(&mut st, Command::SetStripBypass { node: args.node, bypass }) {
            return tool_err(e);
        }
        ok_json(json!({"node": args.node, "bypass": bypass}))
    }

    /// Duplicate one rack unit (#305).
    #[tool(name = "duplicate_node", description = "Duplicate one rack unit (#305): copies its node kind, knob/parameter values, and hosted plugin state (CLAP/LV2) into its rig (or optional target 0-based rig index). Duplicated instruments are wired into the rig; effects/MIDI-FX/arps are added ready to patch without disturbing existing cables. One undo step in the app. Cannot duplicate Keys or Master out.")]
    fn duplicate_node(&self, Parameters(args): Parameters<DuplicateNodeArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            let mut a = json!({"node": args.node});
            if let Some(r) = args.rig { a["rig"] = json!(r); }
            if let Some(n) = &args.name { a["name"] = json!(n); }
            return Self::ui_bridge_tool("duplicate_node", a);
        }
        let Some(src) = st.graph.node(args.node).cloned() else {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        };
        if matches!(src.kind, NodeKind::Keys | NodeKind::MasterOut) {
            return tool_err(format!("node {} ({}) cannot be duplicated", args.node, name_str(&src.name)));
        }
        let rig = args.rig.unwrap_or(src.rig);
        let base_name = args.name.as_deref().map(str::trim).filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                let raw = name_str(&src.name).trim();
                let mut n = 2usize;
                loop {
                    let cand = format!("{raw} {n}");
                    let short: String = cand.chars().take(24).collect();
                    if !st.graph.nodes.iter().any(|x| name_str(&x.name).eq_ignore_ascii_case(&short)) {
                        break short;
                    }
                    n += 1;
                }
            });
        let name = name_bytes(&base_name);
        let wire = match &src.kind {
            NodeKind::Clap { plugin_id } => {
                !crate::plugins::clap_id_is_effect(plugin_id.as_str(), &crate::host::list_clap_plugins())
            }
            NodeKind::Lv2 { uri } => {
                crate::lv2::discover().iter().any(|p| p.uri == uri.as_str() && p.is_instrument)
            }
            NodeKind::Dsp(DspKind::Bus) => true,
            _ => false,
        };
        if let Err(e) = send_cmd(&mut st, Command::AddNode { kind: src.kind.clone(), name, rig, wire }) {
            return tool_err(e);
        }
        let id = st.graph.add_node(src.kind.clone(), name);
        st.graph.set_rig(id, rig);
        let before = st.graph.edges.len();
        if wire {
            if matches!(src.kind, NodeKind::Dsp(DspKind::Bus)) {
                st.graph.wire_bus(id);
            } else {
                let _ = st.graph.wire_instrument(id);
            }
        }
        let cables: Vec<u32> = st.graph.edges[before..].iter().map(|e| e.id).collect();
        let mut result = json!({
            "node": id,
            "source": args.node,
            "name": name_str(&name),
            "rig": rig,
            "cables": cables,
        });
        note_silent_claps(&st, &mut result);
        ok_json(result)
    }

    /// Initialize (reset) a unit's parameters to defaults (#305).
    #[tool(name = "reset_node_params", description = "Initialize (reset) a rack unit's parameters and knobs back to their default values (#305). One undo step in the running app.")]
    fn reset_node_params(&self, Parameters(args): Parameters<ResetNodeParamsArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("reset_node_params", json!({"node": args.node}));
        }
        let Some(node) = st.graph.node(args.node).cloned() else {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        };
        let specs = crate::engine::knob_specs(&node.kind);
        let mut reset: Vec<Value> = Vec::new();
        for k in specs {
            if let Some(param) = k.param {
                let val = k.default as f64;
                if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param, value: val }) {
                    return tool_err(e);
                }
                reset.push(json!({"label": k.label, "param": param, "value": k.default}));
            }
        }
        if reset.is_empty() {
            if let NodeKind::Lv2 { uri } = &node.kind {
                for c in crate::lv2::control_descs_for_uri(uri.as_str()) {
                    let span = (c.max - c.min).abs();
                    let norm = if span > 1e-9 { ((c.default - c.min) / (c.max - c.min)).clamp(0.0, 1.0) } else { 0.0 };
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: c.index as u32, value: norm as f64 }) {
                        return tool_err(e);
                    }
                    reset.push(json!({"label": c.name, "param": c.index, "value": norm}));
                }
            }
        }
        ok_json(json!({
            "node": args.node,
            "name": name_str(&node.name),
            "reset_count": reset.len(),
            "knobs": reset,
        }))
    }

    /// Randomize a unit's parameters within sensible musical bounds (#305).
    #[tool(name = "randomize_node_params", description = "Randomize a rack unit's parameters within sensible musical bounds (#305). Leaves safety-critical controls like Master VOLUME and AudioIn MON untouched. One undo step in the running app.")]
    fn randomize_node_params(&self, Parameters(args): Parameters<RandomizeNodeParamsArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            let mut a = json!({"node": args.node});
            if let Some(amt) = args.amount { a["amount"] = json!(amt); }
            if let Some(seed) = args.seed { a["seed"] = json!(seed); }
            return Self::ui_bridge_tool("randomize_node_params", a);
        }
        let Some(node) = st.graph.node(args.node).cloned() else {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        };
        let amount = args.amount.unwrap_or(1.0).clamp(0.0, 1.0) as f32;
        let mut rng = args.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5eed_cafe)
                ^ ((args.node as u64).wrapping_mul(0x9e37_79b9))
        });
        let specs = crate::engine::knob_specs(&node.kind);
        let has_other = specs.len() > 1;
        let mut changed: Vec<Value> = Vec::new();
        for k in specs {
            if k.hidden { continue; }
            let Some((lo, hi)) = crate::engine::knob_random_bounds(&node.kind, k.label, k.param, has_other) else { continue };
            let Some(param) = k.param else { continue };
            let r = crate::engine::next_rand_f32(&mut rng);
            let target = lo + r * (hi - lo);
            let mut val = k.default + (target - k.default) * amount;
            if k.steps >= 2 {
                let n = (k.steps - 1) as f32;
                val = (val.clamp(0.0, 1.0) * n).round() / n;
            } else {
                val = val.clamp(0.0, 1.0);
            }
            if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param, value: val as f64 }) {
                return tool_err(e);
            }
            changed.push(json!({"label": k.label, "param": param, "value": val}));
        }
        if changed.is_empty() {
            if let NodeKind::Lv2 { uri } = &node.kind {
                for c in crate::lv2::control_descs_for_uri(uri.as_str()).into_iter().take(12) {
                    let r = crate::engine::next_rand_f32(&mut rng);
                    let target = 0.15 + r * 0.70;
                    let span = (c.max - c.min).abs();
                    let def_norm = if span > 1e-9 { ((c.default - c.min) / (c.max - c.min)).clamp(0.0, 1.0) } else { 0.5 };
                    let val = (def_norm + (target - def_norm) * amount).clamp(0.0, 1.0);
                    if let Err(e) = send_cmd(&mut st, Command::SetParam { node: args.node, param: c.index as u32, value: val as f64 }) {
                        return tool_err(e);
                    }
                    changed.push(json!({"label": c.name, "param": c.index, "value": val}));
                }
            }
        }
        ok_json(json!({
            "node": args.node,
            "name": name_str(&node.name),
            "randomized_count": changed.len(),
            "knobs": changed,
        }))
    }

    /// Add an insert to a strip's chain (#74).
    #[tool(name = "add_insert", description = "Add an effect at the end of a strip's insert chain (#74) — the chain IS the rack cables, so the rack re-wires with it and the sound changes. kind=dsp with dsp= (gain, pan, eq3, filter, limiter, saturation, delay, reverb), kind=lv2 with uri= (an installed LV2 effect, see list_plugins), or kind=clap with plugin_id= (an installed CLAP audio effect, see list_plugins). source = the strip's source node id; omit source for a MASTER insert (runs on the whole mix before the fixed limiter). Only works on a strip with a linear chain. Needs the running app (kabelsalat mcp --attach).")]
    fn add_insert(&self, Parameters(args): Parameters<AddInsertArgs>) -> Result<CallToolResult, McpError> {
        let mut a = json!({"kind": args.kind});
        if let Some(s) = args.source { a["source"] = json!(s); }
        if let Some(d) = &args.dsp { a["dsp"] = json!(d); }
        if let Some(u) = &args.uri { a["uri"] = json!(u); }
        if let Some(p) = &args.plugin_id { a["plugin_id"] = json!(p); }
        if let Some(n) = &args.name { a["name"] = json!(n); }
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("add_insert needs the running app (kabelsalat mcp --attach): it edits the rack's cables");
        }
        Self::ui_bridge_tool("add_insert", a)
    }

    /// Remove an insert from a strip's chain (#74).
    #[tool(name = "remove_insert", description = "Remove one effect from a strip's insert chain (#74); the chain closes up (source -> next). fx = the effect's node id; source = its strip's source node id, omitted for a master insert. Needs the running app (kabelsalat mcp --attach).")]
    fn remove_insert(&self, Parameters(args): Parameters<RemoveInsertArgs>) -> Result<CallToolResult, McpError> {
        let mut a = json!({"fx": args.fx});
        if let Some(s) = args.source { a["source"] = json!(s); }
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("remove_insert needs the running app (kabelsalat mcp --attach): it edits the rack's cables");
        }
        Self::ui_bridge_tool("remove_insert", a)
    }

    /// Rename one strip (#77).
    #[tool(name = "rename_strip", description = "Rename one strip (#77): the name is the node's name, so the rack face, the mixer and saved sessions all show it. Up to 24 characters. In headless mcp the engine's graph is renamed (get_graph shows it).")]
    fn rename_strip(&self, Parameters(args): Parameters<RenameStripArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return Self::ui_bridge_tool("rename_strip", json!({"node": args.node, "name": args.name}));
        }
        let name: String = args.name.trim().chars().take(24).collect();
        if name.is_empty() { return tool_err("a strip name cannot be empty"); }
        let mut st = self.state.lock().unwrap();
        if st.graph.node(args.node).is_none() {
            return tool_err(format!("no node {} in the patch (see get_graph)", args.node));
        }
        if let Err(e) = send_cmd(&mut st, Command::RenameNode { node: args.node, name: name_bytes(&name) }) {
            return tool_err(e);
        }
        if let Some(n) = st.graph.nodes.iter_mut().find(|n| n.id == args.node) {
            n.name = name_bytes(&name);
        }
        ok_json(json!({"node": args.node, "name": name}))
    }

    /// Set a strip's colour (#77).
    #[tool(name = "set_strip_color", description = "Give a mixer strip a colour (#77): 0xRRGGBB, a palette name (teal, amber, rose, sand, violet, cyan) or none to clear. Saved with the session. Needs the running app (kabelsalat mcp --attach).")]
    fn set_strip_color(&self, Parameters(args): Parameters<SetStripColorArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("set_strip_color needs the running app (kabelsalat mcp --attach): the colour is session state the app owns");
        }
        Self::ui_bridge_tool("set_strip_color", json!({"node": args.node, "color": args.color}))
    }

    /// Move a strip in the display order (#77).
    #[tool(name = "move_strip", description = "Move one source strip to slot `to` (1-based) within its rig's display order in the mixer (#77). Display only — the sound is unchanged. Needs the running app (kabelsalat mcp --attach).")]
    fn move_strip(&self, Parameters(args): Parameters<MoveStripArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("move_strip needs the running app (kabelsalat mcp --attach): the order is session state the app owns");
        }
        Self::ui_bridge_tool("move_strip", json!({"node": args.node, "to": args.to}))
    }

    /// Collapse a strip (#77).
    #[tool(name = "collapse_strip", description = "Collapse a mixer strip to its mini form (#77): meter, M/S and the expand chip. Pass collapsed=false to expand. Needs the running app (kabelsalat mcp --attach).")]
    fn collapse_strip(&self, Parameters(args): Parameters<CollapseStripArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("collapse_strip needs the running app (kabelsalat mcp --attach): the collapse flag is session state the app owns");
        }
        Self::ui_bridge_tool("collapse_strip", json!({"node": args.node, "collapsed": args.collapsed}))
    }

    /// Query quick knobs on mixer strips (#107).
    #[tool(name = "get_strip_knobs", description = "List each mixer strip slot's quick knob (#107): node id, unit name, rig, active quick knob label, parameter name, current value (0..1), custom flag, and available choices. Needs the running app (kabelsalat mcp --attach).")]
    fn get_strip_knobs(&self, Parameters(args): Parameters<GetStripKnobsArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("get_strip_knobs needs the running app (kabelsalat mcp --attach): quick knob assignments are session state the app owns");
        }
        let mut a = json!({});
        if let Some(node) = args.node { a["node"] = json!(node); }
        Self::ui_bridge_tool("get_strip_knobs", a)
    }

    /// Assign a mixer strip slot's quick knob (#107).
    #[tool(name = "set_strip_knob", description = "Assign which knob a source or insert slot's quick knob controls in the mixer (#107). Pass knob by label or parameter name/id, or empty string \"\" to reset to default. Saved with the session. Needs the running app (kabelsalat mcp --attach).")]
    fn set_strip_knob(&self, Parameters(args): Parameters<SetStripKnobArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("set_strip_knob needs the running app (kabelsalat mcp --attach): quick knob assignments are session state the app owns");
        }
        Self::ui_bridge_tool("set_strip_knob", json!({"node": args.node, "knob": args.knob}))
    }

    /// Delete one rig.
    #[tool(name = "delete_rig", description = "Delete rig `rig` (index from get_rigs) with everything in it. DESTRUCTIVE: requires confirm=true; the last rig cannot be deleted. Returns {rigs, active}.")]
    fn delete_rig(&self, Parameters(args): Parameters<DeleteRigArgs>) -> Result<CallToolResult, McpError> {
        if !args.confirm {
            return tool_err("not deleted: pass confirm=true (ask the user first)");
        }
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            // The running app deletes the rig itself: every other rig keeps
            // its knobs, synth state and type mark (#198).
            return Self::ui_bridge_tool("delete_rig", json!({"rig": args.rig, "confirm": true}));
        }
        self.edit_rigs(Some(args.rig), None)
    }

    /// Save one rig as a named rig preset.
    #[tool(name = "save_rig", description = "Save one rig (units, cables, knobs, plugin states, matrix routes, mix, name) as a named rig preset in ~/kabelsalat-rig-presets, independent of whole sessions. rig defaults to the active rig.")]
    fn save_rig(&self, Parameters(args): Parameters<SaveRigArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            let mut a = json!({"name": args.name});
            if let Some(r) = args.rig { a["rig"] = json!(r); }
            return Self::ui_bridge_tool("save_rig", a);
        }
        let st = self.state.lock().unwrap();
        let meta = match rig_meta(&st) { Ok(m) => m, Err(e) => return tool_err(e) };
        let (graphs, _) = control::split_rigs(&st.graph);
        let active = st.rigs.lock().map(|s| s.active).unwrap_or(0);
        let rig = args.rig.unwrap_or(active);
        if rig >= graphs.len() {
            return tool_err(format!("no rig {rig}: there are {}", graphs.len()));
        }
        let rig_name = meta.get(rig).map(|m| m.name.clone()).filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Rig {}", (b'A' + rig.min(25) as u8) as char));
        let sess = crate::session::Session::new(args.name.trim(), graphs[rig].clone());
        let mut preset = crate::session::RigPreset::new(args.name.trim(), rig_name.clone(), sess);
        preset.midi_device = meta.get(rig).and_then(|m| m.midi_device);
        if let Some(gp) = &st.graph_pub {
            if let Ok(g) = gp.lock() {
                preset.mix = g.mix[rig.min(7)];
            }
        }
        let path = match crate::session::RigPreset::package_path(&st.rig_presets_dir, &args.name) {
            Ok(p) => p,
            Err(e) => return tool_err(format!("{e:#}")),
        };
        if let Err(e) = preset.save(&path) {
            return tool_err(format!("save_rig failed: {e:#}"));
        }
        ok_json(json!({"saved": preset.name, "rig": rig, "rig_name": rig_name, "path": path.display().to_string()}))
    }

    /// Load a saved rig preset (as a new rig or replacing an existing rig).
    #[tool(name = "load_rig", description = "Load a saved rig preset by name, restoring units, cables, knobs, plugin states, matrix routes, mix and rig name. Omit rig to add as a new rig, or pass rig=<index> to replace an existing rig.")]
    fn load_rig(&self, Parameters(args): Parameters<LoadRigArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            let mut a = json!({"name": args.name});
            if let Some(r) = args.rig { a["rig"] = json!(r); }
            return Self::ui_bridge_tool("load_rig", a);
        }
        let mut st = self.state.lock().unwrap();
        let path = match crate::session::RigPreset::package_path(&st.rig_presets_dir, &args.name) {
            Ok(p) => p,
            Err(e) => return tool_err(format!("{e:#}")),
        };
        let preset = match crate::session::RigPreset::load(&path) {
            Ok(p) => p,
            Err(e) => return tool_err(format!("load_rig failed: {e:#}")),
        };
        let meta = match rig_meta(&st) { Ok(m) => m, Err(e) => return tool_err(e) };
        let (graphs, _) = control::split_rigs(&st.graph);
        let mut rigs: Vec<Rig> = graphs.into_iter().enumerate().map(|(i, g)| {
            let m = meta.get(i);
            let name = m.map(|m| m.name.clone()).filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Rig {}", (b'A' + i.min(25) as u8) as char));
            Rig::new(name, g, m.and_then(|m| m.midi_device))
        }).collect();
        let mut mixes = [crate::engine::RigMix::DEFAULT; 8];
        if let Some(gp) = &st.graph_pub {
            if let Ok(g) = gp.lock() { mixes = g.mix; }
        }
        let preset_rig_name = if preset.rig_name.trim().is_empty() { preset.name.clone() } else { preset.rig_name.clone() };
        let loaded_rig = Rig::new(preset_rig_name.clone(), preset.session.graph.clone(), preset.midi_device);
        let target = match args.rig {
            None => {
                if rigs.len() >= 8 { return tool_err("at most 8 rigs"); }
                rigs.push(loaded_rig);
                rigs.len() - 1
            }
            Some(r) => {
                if r >= rigs.len() { return tool_err(format!("no rig {r}: there are {}", rigs.len())); }
                rigs[r] = loaded_rig;
                r
            }
        };
        mixes[target.min(7)] = preset.mix;
        let bases = control::rig_bases(&rigs);
        if let Err(e) = send_cmd(&mut st, Command::LoadRigs { rigs: rigs.clone(), active: target }) {
            return tool_err(e);
        }
        for p in preset.session.params() {
            let _ = send_cmd(&mut st, Command::SetParam { node: bases[target] + p.node, param: p.param, value: p.value });
        }
        for (i, m) in mixes.iter().enumerate().take(rigs.len()) {
            let _ = send_cmd(&mut st, Command::SetRigMix {
                rig: i as u8, vol: Some(m.vol), pan: Some(m.pan), mute: Some(m.mute), solo: Some(m.solo),
            });
        }
        st.graph = merge_rigs(&rigs);
        let metas: Vec<crate::control::RigMeta> = rigs.iter()
            .map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
        if let Ok(mut s) = st.rigs.lock() { s.rigs = metas.clone(); s.active = target; }
        ok_json(json!({"loaded": preset.name, "rig": target, "rigs": metas, "active": target}))
    }

    /// Load a starter template, replacing the patch.
    #[tool(name = "load_template", description = "Load a starter template, replacing the current patch (Command::LoadGraph + its default knob values). name: cathedral_organ (Aeolus pipe organ, full stops, cathedral reverb) | jazz_organ (setBfree tonewheel 88 8000 000, percussion, slow Leslie) | rock_organ (setBfree through the Guitarix amp + cabinet) | ambient_pad (Surge → delay → reverb) | synth_lead (Surge → slap delay → reverb) | guitar_rig (Audio In → Scream → Delay → Reverb → Amp → Cabinet → Master: a live guitar/mic input with a tuner on the Audio In unit's face) | demo | multi_rig (the two demo patches exist in the running app only). The result lists any catalog plugins the template needs that are not installed (install them with install_instrument).")]
    fn load_template(&self, Parameters(args): Parameters<LoadTemplateArgs>) -> Result<CallToolResult, McpError> {
        if matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return Self::ui_bridge_tool("load_template", json!({"name": args.name}));
        }
        let name = args.name.to_lowercase();
        if matches!(name.as_str(), "demo" | "multi_rig" | "rigs") {
            return tool_err(format!("demo and multi_rig are demo patches of the running app; headless templates are {}", template_ids()));
        }
        let Some(t) = crate::session::Template::from_name(&name) else {
            return tool_err(format!("unknown template '{}' (use {}, demo, multi_rig)", args.name, template_ids()));
        };
        let s = t.session();
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::LoadGraph { graph: s.graph.clone() }) {
            return tool_err(e);
        }
        for p in s.params() {
            let _ = send_cmd(&mut st, Command::SetParam { node: p.node, param: p.param, value: p.value });
        }
        st.graph = s.graph.clone();
        // A single-graph load leaves no rig list (the app's multi-rig path
        // owns rig names; LoadGraph resets the engine's list too).
        if let Ok(mut r) = st.rigs.lock() { r.rigs.clear(); r.active = 0; }
        let mut result = json!({
            "loaded": t.title(),
            "nodes": s.graph.nodes.iter().map(|n| json!({"id": n.id, "name": name_str(&n.name), "rig": n.rig})).collect::<Vec<_>>(),
            "edges": s.graph.edges.len(),
        });
        note_silent_claps(&st, &mut result);
        // Headless: the same on-disk bundle probe list_instruments uses.
        let roots_raw = crate::catalog::plugin_roots(&crate::catalog::home_dir());
        let roots: Vec<&std::path::Path> = roots_raw.iter().map(|p| p.as_path()).collect();
        let scan = crate::catalog::ScanSet::default();
        let missing = crate::catalog::missing(t.plugins(), |e| crate::catalog::installed(e, &scan, &roots));
        if !missing.is_empty() {
            result["missing_plugins"] = json!(missing.iter().map(|e| e.id).collect::<Vec<_>>());
            result["install_hint"] = json!(crate::catalog::install_hint(&missing));
        }
        ok_json(result)
    }

    /// List saved single-rig presets.
    #[tool(name = "list_rig_presets", description = "List saved single-rig preset names in ~/kabelsalat-rig-presets.")]
    fn list_rig_presets(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("list_rig_presets", json!({}));
        }
        let presets = crate::session::list_rig_presets(&st.rig_presets_dir);
        ok_json(json!({"presets": presets}))
    }

    /// Search the sound library of a rig's instrument.
    #[tool(name = "list_sounds", description = "Search the sound library of ONE instrument: Surge patches, Dexed DX7 cartridge voices, an LV2 instrument's presets and MIDI programmes (setBfree's organ registrations, Vitalium's presets), or a plugin's own preset list (YANES). rig: whose instrument (default: the rig in view); node: the instrument's node id when a rig holds several different ones. Returns up to 25 sounds plus the instrument and the library they are from. A CLAP plugin's own preset list needs the running app (kabelsalat mcp --attach).")]
    fn list_sounds(&self, Parameters(args): Parameters<ListSoundsArgs>) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("list_sounds", sound_args(&args.query, args.rig, args.node));
        }
        match headless_sound_library(&st.graph, args.rig, args.node) {
            Ok((unit, library)) => {
                let answer = crate::sounds::list_answer(&unit, &library, &args.query);
                serde_json::from_str::<Value>(&answer).map_or_else(|_| tool_err(answer), ok_json)
            }
            Err(e) => tool_err(e),
        }
    }

    /// Load a sound into a rig's instrument.
    #[tool(name = "load_sound", description = "Load a sound into a rig's instrument from that instrument's own library (see list_sounds): a Surge patch, a Dexed voice, an LV2 preset or MIDI programme, a plugin preset. query: words or an entry of list_sounds; rig: index (default: the rig in view); node: the instrument's node id when a rig holds several different ones. CLAP instruments (Surge, Dexed, YANES) need the running app (kabelsalat mcp --attach): their plugins live on its UI thread.")]
    fn load_sound(&self, Parameters(args): Parameters<LoadSoundArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            return Self::ui_bridge_tool("load_sound", sound_args(&args.query, args.rig, args.node));
        }
        let (unit, library) = match headless_sound_library(&st.graph, args.rig, args.node) {
            Ok(found) => found,
            Err(e) => return tool_err(e),
        };
        let sound = match crate::sounds::find_or_explain(&unit, &library, &args.query) {
            Ok(sound) => sound,
            Err(e) => return tool_err(e),
        };
        let Backend::Local(bus) = &mut st.backend else { unreachable!("the remote backend returned above") };
        match crate::sounds::load(sound, crate::sounds::Target { node: unit.node, clap: None, bus: Some(bus) }) {
            Ok(()) => ok_json(json!(crate::sounds::loaded_answer(&unit, sound, &format!("Rig {}", unit.rig + 1)))),
            Err(e) => tool_err(format!("load_sound failed: {e} — {} needs the running app (kabelsalat mcp --attach)", unit.name)),
        }
    }

    #[tool(name = "get_rigs", description = "Query the multi-rig state: {rigs:[{name, midi_device, type, type_marked}], active}. type is the rig's instrument type (piano | electric_piano | jazz_organ | pipe_organ | synth | bass | wind | drums; null for a rig without an instrument), type_marked whether the player or set_keyboard_mode marked it. Empty rigs means single-graph mode (a plain graph load clears it). midi_device null = any source; a number = only that hardware MIDI device.")]
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
            let data = reply.get("data").cloned().unwrap_or(Value::Null);
            // The rig types live in the app (#198): ask it, and answer
            // without them only when it cannot say.
            let types = bridge::request(&json!({"op": "ui", "tool": "get_keyboard_state", "args": {}})).ok()
                .and_then(|r| r.get("data").cloned())
                .and_then(|d| d.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).or(Some(d)))
                .and_then(|d| d.get("rigs").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            return ok_json(with_rig_types(data, &types));
        }
        let snap = match st.rigs.lock() {
            Ok(s) => serde_json::to_value(&*s),
            Err(_) => return tool_err("rig snapshot lock poisoned"),
        };
        let types: Vec<Value> = (0..headless_rig_count(&st)).map(|r| headless_rig_type(&st, r)).collect();
        match snap {
            Ok(v) => ok_json(with_rig_types(v, &types)),
            Err(e) => tool_err(format!("serializing rig snapshot: {e}")),
        }
    }

    /// The Navigator's rows (#108).
    #[tool(name = "list_channels", description = "List what the Navigator shows (#108): every rig, its source channels (strip name and colour) and each channel's plugin chain (instrument, then inserts, with kind CLAP / LV2 / built-in). filter: channels | plugins | all (default all). Needs the running app (kabelsalat mcp --attach); headless, use get_rigs and get_graph.")]
    fn list_channels(&self, Parameters(args): Parameters<ListChannelsArgs>) -> Result<CallToolResult, McpError> {
        if !matches!(self.state.lock().unwrap().backend, Backend::Remote) {
            return tool_err("list_channels needs the running app (kabelsalat mcp --attach): strip names, colours and order are session state the app owns; headless, use get_rigs and get_graph");
        }
        Self::ui_bridge_tool("list_channels", json!({ "filter": args.filter }))
    }

    /// List built-in units and installed CLAP and LV2 plugins, organized by type.
    #[tool(name = "list_plugins", description = "List built-in units and installed CLAP/LV2 plugins organized by type (instrument, effect, midi_fx, utility) and subcategory (delay_reverb, eq_filter, dynamics, drive_amp, modulation, pitch_spatial, analyzer, synth, organ, sampler, …). Optional filters: type, category, format, query, limit.")]
    fn list_plugins(&self, Parameters(args): Parameters<ListPluginsArgs>) -> Result<CallToolResult, McpError> {
        let filter_args = json!({
            "type": args.plugin_type,
            "category": args.category,
            "format": args.format,
            "query": args.query,
            "limit": args.limit,
        });
        let filter = match crate::plugins::PluginListFilter::from_args(&filter_args) {
            Ok(f) => f,
            Err(e) => return tool_err(e),
        };
        let clap = crate::host::list_clap_plugins();
        let lv2 = crate::lv2::discover();
        ok_json(crate::plugins::list_plugins_json(&clap, &lv2, &filter))
    }

    /// List the MIDI-effect units a rig can host.
    #[tool(name = "list_midi_fx", description = "List the MIDI-effect units add_node(kind=midi_fx) can host (AUM MIDI strips: arpeggiators, chord tools, transposers, MIDI filters): the built-in transposer, installed LV2 MIDI plugins (uri). Installed CLAP note effects are listed too but cannot be hosted yet.")]
    fn list_midi_fx(&self) -> Result<CallToolResult, McpError> {
        let lv2: Vec<_> = crate::lv2::discover()
            .into_iter()
            .filter(|p| p.is_midi_fx)
            .map(|p| json!({"uri": p.uri, "name": p.name}))
            .collect();
        let clap: Vec<_> = crate::host::list_clap_plugins()
            .into_iter()
            .filter(crate::host::is_note_effect)
            .map(|p| json!({"plugin_id": p.id, "name": p.name, "vendor": p.vendor}))
            .collect();
        ok_json(json!({
            "builtin": [{"name": "Transpose", "add_with": "add_node kind=midi_fx (optionally semis)"}],
            "lv2": lv2,
            "clap_not_hostable_yet": clap,
        }))
    }

    /// List the built-in instrument catalog with installed/running state.
    #[tool(name = "list_instruments", description = "List the built-in instrument catalog: id, name, format (CLAP/LV2), description, about text, licence, homepage, source code URL, install route (distro package, build from source, or prebuilt GitHub release) and whether each is installed. Attached mode reports the live app's installed/running state; headless mode uses on-disk bundle detection.")]
    fn list_instruments(&self) -> Result<CallToolResult, McpError> {
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            return Self::ui_bridge_tool("list_instruments", json!({}));
        }
        // Headless: filesystem-only detection (a lilv/clack scan is heavy and
        // this module must stay scannable without one — rules 48/57).
        let home = crate::catalog::home_dir();
        let roots_raw = crate::catalog::plugin_roots(&home);
        let roots: Vec<&std::path::Path> = roots_raw.iter().map(|p| p.as_path()).collect();
        let scan = crate::catalog::ScanSet::default();
        let entries: Vec<Value> = crate::catalog::CATALOG
            .iter()
            .map(|e| crate::catalog::entry_json(e, crate::catalog::installed(e, &scan, &roots), false))
            .collect();
        ok_json(json!({"instruments": entries}))
    }

    /// Install a catalog instrument (destructive: requires confirm, and the
    /// running app which owns the install worker + log + rescan).
    #[tool(name = "install_instrument", description = "Install a catalog instrument (from list_instruments): a distro package via sudo pacman, a build from GitHub source, or a checksum-verified prebuilt GitHub release, into ~/.clap / ~/.lv2. DESTRUCTIVE: requires confirm=true after asking the user. Runs in the background in the GUI app (live log + plugin rescan), so it needs the running app (kabelsalat mcp --attach).")]
    fn install_instrument(&self, Parameters(args): Parameters<InstallInstrumentArgs>) -> Result<CallToolResult, McpError> {
        if !args.confirm {
            return tool_err("not installed: pass confirm=true (ask the user first — installing changes the system)");
        }
        let remote = matches!(self.state.lock().unwrap().backend, Backend::Remote);
        if remote {
            return Self::ui_bridge_tool("install_instrument", json!({"id": args.id, "confirm": true}));
        }
        tool_err("install_instrument needs the running app (kabelsalat mcp --attach) — installs run in the GUI app")
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
        if let Err(e) = send_cmd(&mut st, Command::NoteOn { note: args.note, velocity, source, channel: 0 }) {
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
        if let Err(e) = send_cmd(&mut st, Command::NoteOff { note: args.note, source, channel: 0 }) {
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

    /// Set the time signature.
    #[tool(name = "set_time_signature", description = "Set the time signature (Command::SetTimeSignature): numerator = beats per bar (1..=12), denominator = the note value of one beat (2, 4, 8 or 16), e.g. 3 and 4 for a waltz, 6 and 8 for 6/8. The metronome accents the first beat of each bar and clicks once per beat (eighths in x/8); the count-in lasts whole bars of it; bar-quantized starts (recording start/stop, a synced file player) wait for its bar line; hosted plugins are told. It is saved with the session. Tempo stays in quarter notes per minute. get_status reads it back as time_signature.")]
    fn set_time_signature(&self, Parameters(args): Parameters<SetTimeSignatureArgs>) -> Result<CallToolResult, McpError> {
        let sig = match crate::transport::TimeSig::checked(args.numerator as u64, args.denominator as u64) {
            Ok(sig) => sig,
            Err(e) => return tool_err(e),
        };
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::SetTimeSignature { num: sig.num, den: sig.den }) {
            return tool_err(e);
        }
        ok_json(sig.to_json())
    }

    /// Play a live demo note phrase through a rig.
    #[tool(name = "play_demo", description = "Play a short live note phrase through one rig in a loop on the engine's clock at the current tempo (Command::DemoPlay; never recorded audio; stop with stop_demo). rig is the 0-based rig index (0 = first rig / 'rig 1', 1 = 'rig 2'; omit for the active rig). Optional phrase overrides the automatic choice: keys | bass | lead | pad | organ | drums | wind (the wind phrase also sends CC 2 and channel pressure breath). Gives listen a full phrase to judge.")]
    fn play_demo(&self, Parameters(args): Parameters<PlayDemoArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            let mut a = json!({});
            if let Some(r) = args.rig { a["rig"] = json!(r); }
            if let Some(p) = &args.phrase { a["phrase"] = json!(p); }
            drop(st);
            return Self::ui_bridge_tool("play_demo", a);
        }
        let (rigs, active) = st.rigs.lock().map(|s| (s.rigs.clone(), s.active)).unwrap_or_default();
        let n_rigs = rigs.len().max(st.graph.nodes.iter().map(|n| n.rig as usize + 1).max().unwrap_or(1));
        let rig = args.rig.map(|r| r as usize).unwrap_or_else(|| active.min(n_rigs.saturating_sub(1)));
        if rig >= n_rigs {
            return tool_err(format!("no rig {rig} ({n_rigs} rig(s), 0..{})", n_rigs.saturating_sub(1)));
        }
        let rig_name = rigs.get(rig).map(|m| m.name.clone())
            .unwrap_or_else(|| format!("Rig {}", (b'A' + (rig as u8).min(25)) as char));
        let inst = st.graph.nodes.iter().find(|n| {
            n.rig as usize == rig && (matches!(n.kind, NodeKind::Clap { .. })
                || crate::organ::is_organ(&n.kind)
                || matches!(&n.kind, NodeKind::Lv2 { uri } if crate::lv2::discover().iter().any(|p| p.uri == uri.as_str() && p.is_instrument)))
        });
        let Some(inst) = inst else {
            return tool_err(format!("rig {rig} '{rig_name}' has no instrument"));
        };
        let kind = match args.phrase.as_deref() {
            Some(s) if !s.trim().is_empty() => match crate::demo::PhraseKind::parse(s) {
                Some(k) => k,
                None => return tool_err(format!("unknown phrase '{s}' (use keys, bass, lead, pad, organ, drums or wind)")),
            },
            _ => {
                let pname = name_str(&inst.name);
                // The rig's type (#198): its mark, else its instrument's.
                let ty = st.rig_types.get(rig).copied().flatten()
                    .unwrap_or_else(|| crate::keyboard::classify_instrument(&inst.kind, None, Some(pname), &[]));
                crate::demo::kind_for(ty, None, Some(pname))
            }
        };
        let bpm = st.graph_pub.as_ref().and_then(|p| p.lock().ok()).map_or(96.0, |g| g.transport.bpm);
        if let Err(e) = send_cmd(&mut st, Command::DemoPlay { rig: rig as u8, phrase: kind }) {
            return tool_err(e);
        }
        let p = crate::demo::phrase(kind);
        let secs = p.seconds(bpm);
        ok_json(json!({
            "playing": true,
            "rig": rig,
            "rig_name": rig_name,
            "phrase": kind.name(),
            "title": p.title,
            "beats": p.beats,
            "bpm": bpm,
            "seconds": (secs * 100.0).round() / 100.0,
        }))
    }

    /// Stop the running rig demo phrase.
    #[tool(name = "stop_demo", description = "Stop the running rig demo phrase (Command::DemoStop) and release any notes it holds.")]
    fn stop_demo(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("stop_demo", json!({}));
        }
        if let Err(e) = send_cmd(&mut st, Command::DemoStop) {
            return tool_err(e);
        }
        ok_json(json!({"playing": false}))
    }

    /// Read the session's setlist (#204).
    #[tool(name = "get_setlist", description = "Read the session's setlist: every song (index, name, optional bpm and time_signature, parts with their live rigs, key ranges, transpose, level and stored knob count), the active song/part (null = all rigs open), and any learned foot-switch CCs.")]
    fn get_setlist(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("get_setlist", json!({}));
        }
        ok_json(setlist_json(&st.setlist))
    }

    /// Add, update, reorder, or remove a song in the setlist (#204).
    #[tool(name = "set_setlist_song", description = "Add, update, reorder or remove a song in the setlist. Omit song to append a new song (name required); pass song (0-based index) to update it, move_to to reorder it, or remove=true to delete it. Optional bpm (20..300, null clears) and time_signature ({numerator, denominator}, null clears). Optional parts array: each part has name, rigs ([{rig, key_lo?, key_hi?, transpose?, level?}]) and optional capture_knobs.")]
    fn set_setlist_song(&self, Parameters(args): Parameters<SetSetlistSongArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_setlist_song", serde_json::to_value(&args).unwrap_or_else(|_| json!({})));
        }
        if let Err(e) = apply_setlist_song_headless(&mut st, &args) {
            return tool_err(e);
        }
        ok_json(setlist_json(&st.setlist))
    }

    /// Select or step the active setlist song/part without a gap (#204).
    #[tool(name = "select_setlist_part", description = "Switch the active setlist song/part without changing routing: held notes on outgoing rigs keep sounding until released and effect tails ring out, while new notes play only the new part's rigs. Pass action ('next_part' | 'prev_part' | 'next_song' | 'prev_song' | 'clear') OR song (0-based index) with optional part (default 0).")]
    fn select_setlist_part(&self, Parameters(args): Parameters<SelectSetlistPartArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("select_setlist_part", serde_json::to_value(&args).unwrap_or_else(|_| json!({})));
        }
        match args.action.as_deref() {
            Some("next_part") => { st.setlist.next_part(); }
            Some("prev_part") => { st.setlist.prev_part(); }
            Some("next_song") => { st.setlist.next_song(); }
            Some("prev_song") => { st.setlist.prev_song(); }
            Some("clear") => { st.setlist.active = None; }
            Some(other) => return tool_err(format!("unknown action '{other}' (use next_part, prev_part, next_song, prev_song or clear)")),
            None => {
                let Some(song) = args.song else {
                    return tool_err("pass either action or song");
                };
                let part = args.part.unwrap_or(0);
                if st.setlist.select(song, part).is_none() {
                    return tool_err(format!("song {song} part {part} is out of range"));
                }
            }
        }
        let zones = st.setlist.active_zones();
        if let Err(e) = send_cmd(&mut st, Command::SetRigZones { zones }) {
            return tool_err(e);
        }
        if let Some((song, part, _)) = st.setlist.current().map(|(s, p, pos)| (s.clone(), p.clone(), pos)) {
            if let Some(bpm) = song.bpm {
                let _ = send_cmd(&mut st, Command::SetBpm { bpm });
            }
            if let Some(sig) = song.time_sig {
                let _ = send_cmd(&mut st, Command::SetTimeSignature { num: sig.num, den: sig.den });
            }
            for pr in &part.rigs {
                let _ = send_cmd(&mut st, Command::SetRigMix { rig: pr.rig, vol: Some(pr.level), pan: None, mute: None, solo: None });
            }
        }
        ok_json(setlist_json(&st.setlist))
    }

    /// Import a saved session's rigs into the current gig as a new setlist song (#204).
    #[tool(name = "add_song_from_session", description = "Import a saved session's rigs into the current gig (up to 8 rigs total) and append a new song in the setlist pointing at those imported rigs. session = saved session name from list_sessions; optional name overrides the song title.")]
    fn add_song_from_session(&self, Parameters(args): Parameters<AddSongFromSessionArgs>) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("add_song_from_session", serde_json::to_value(&args).unwrap_or_else(|_| json!({})));
        }
        tool_err("add_song_from_session requires the running app (connect with --attach)")
    }

    /// Bind or clear a hardware MIDI CC foot-switch for stepping the setlist (#204).
    #[tool(name = "set_setlist_footswitch", description = "Bind or clear a hardware MIDI CC foot-switch to step the setlist on rising edge (value >= 64). action: 'next' | 'prev'. cc: 0..127 (omit or null to clear). channel: optional 0..15 (omit = any channel).")]
    fn set_setlist_footswitch(&self, Parameters(args): Parameters<SetSetlistFootswitchArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Remote) {
            drop(st);
            return Self::ui_bridge_tool("set_setlist_footswitch", serde_json::to_value(&args).unwrap_or_else(|_| json!({})));
        }
        let fs = args.cc.map(|cc| crate::setlist::FootSwitch {
            cc: cc & 0x7F,
            channel: args.channel.map(|c| c & 0x0F),
        });
        match args.action.as_str() {
            "next" => st.setlist.next_cc = fs,
            "prev" => st.setlist.prev_cc = fs,
            other => return tool_err(format!("unknown action '{other}' (use 'next' or 'prev')")),
        }
        ok_json(setlist_json(&st.setlist))
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

    /// Start recording armed targets (master mix and/or armed rig stems).
    #[tool(name = "record_start", description = "Start recording armed targets (master mix and/or armed rig stems) to a new timestamped take folder under ~/Music/kabelsalat/ (Command::RecordStart). Bar-quantized when the transport rolls. Idempotent.")]
    fn record_start(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if matches!(st.backend, Backend::Local(_)) && !st.record.is_live() {
            return tool_err("recording unavailable: no audio engine (recorder not attached)");
        }
        if matches!(st.backend, Backend::Local(_)) {
            let names: Vec<String> = st.rigs.lock().map(|s| s.rigs.iter().map(|r| r.name.clone()).collect()).unwrap_or_default();
            st.record.set_rig_names(names);
        }
        if let Err(e) = send_cmd(&mut st, Command::RecordStart) {
            return tool_err(e);
        }
        ok_json(json!({"recording": true}))
    }

    /// Stop recording.
    #[tool(name = "record_stop", description = "Stop recording (Command::RecordStop): the writer drains the rings and finalizes all WAV headers. Bar-quantized when the transport rolls (a second stop stops immediately). Idempotent.")]
    fn record_stop(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        if let Err(e) = send_cmd(&mut st, Command::RecordStop) {
            return tool_err(e);
        }
        ok_json(json!({"recording": false}))
    }

    /// Recording status: path, folder, elapsed seconds, dropped frames, stems, armed targets.
    #[tool(name = "record_status", description = "Get recording status: recording (bool), pending (bool), path, folder, seconds, dropped_frames, finalized, stems, armed_master, armed_rigs.")]
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

    /// Arm or disarm a recording target (master mix or a rig's post-fader stem).
    #[tool(name = "set_record_arm", description = "Arm or disarm a recording target (Command::SetRecordArm): target='master' (or omit target and rig) for the master mix, or rig=<0-based index> for a rig's post-fader stem. Armed targets record simultaneously into the same take folder.")]
    fn set_record_arm(&self, Parameters(args): Parameters<SetRecordArmArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let armed = args.armed.unwrap_or(true);
        let rig = if args.target.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("master"))
            || (args.rig.is_none() && args.target.is_none())
        {
            None
        } else {
            let r = args.rig.unwrap_or(0);
            if r >= 8 {
                return tool_err(format!("no rig {r}: stems exist for rigs 0..7"));
            }
            if matches!(st.backend, Backend::Local(_)) && armed {
                st.record.ensure_stem(r);
            }
            Some(r)
        };
        if let Err(e) = send_cmd(&mut st, Command::SetRecordArm { rig, armed }) {
            return tool_err(e);
        }
        ok_json(json!({
            "target": if rig.is_none() { "master" } else { "rig" },
            "rig": rig,
            "armed": armed,
        }))
    }

    /// Toggle sync-to-transport recording.
    #[tool(name = "set_record_sync", description = "Toggle 'sync to transport' (Command::SetRecordSync): while on, master transport play starts a recording and stop stops it. The take begins at song beat 0 (after any count-in).")]
    fn set_record_sync(&self, Parameters(args): Parameters<RecordSyncArgs>) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().unwrap();
        let on = args.on;
        if let Err(e) = send_cmd(&mut st, Command::SetRecordSync { on }) {
            return tool_err(e);
        }
        st.record_sync = on;
        ok_json(json!({"record_sync": on}))
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
    node_times: Arc<crate::engine_info::NodeTimes>,
    sample_rate: f64,
    live: bool,
    record: Arc<crate::record::RecorderShared>,
    /// Plays buses routed to their own output (#147); stops when dropped.
    _bus_outs: Option<crate::audio_out::BusOutputs>,
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
    let (listen_tap, listen_capture) = crate::listen::tap(sample_rate as u32);
    engine.set_listen_tap(listen_tap);
    crate::listen::install(listen_capture);
    let level = engine.level_handle();
    let node_times = engine.node_times_handle();
    bridge::SAMPLE_RATE.store(sample_rate as u32, Ordering::Relaxed);
    let bus_outs = engine.take_out_taps()
        .map(|taps| crate::audio_out::BusOutputs::spawn(taps, graph_pub.clone(), sample_rate as u32));
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            let t0 = std::time::Instant::now();
            engine.render(data, ch);
            bridge::note_render(t0.elapsed(), data.len() / ch.max(1), sample_rate);
        },
        |e| {
            bridge::XRUNS.fetch_add(1, Ordering::Relaxed);
            eprintln!("mcp audio stream error: {e}");
        },
        None,
    )?;
    stream.play()?;
    Ok(AudioHandles { mains, stream: Some(stream), bus, level, graph_pub, node_times, sample_rate, live: true, record, _bus_outs: bus_outs })
}

/// No audio device (or plugin failure): keep the engine rendering into a
/// dummy buffer at block pace so commands drain and levels stay live.
fn dummy_audio(rigs: Arc<Mutex<RigSnapshot>>) -> AudioHandles {
    let sample_rate = 48_000.0;
    let (bus, drain) = control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, vec![]);
    engine.spawn_lv2_loader();
    engine.set_rig_snapshot(rigs);
    let graph_pub = Arc::new(Mutex::new(GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    let (recorder_tap, record) = crate::record::start(crate::record::record_dir(), sample_rate as u32, 2);
    engine.set_recorder(recorder_tap);
    let (listen_tap, listen_capture) = crate::listen::tap(sample_rate as u32);
    engine.set_listen_tap(listen_tap);
    crate::listen::install(listen_capture);
    let level = engine.level_handle();
    let node_times = engine.node_times_handle();
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
    AudioHandles { mains: vec![], stream: None, bus, level, graph_pub, node_times, sample_rate, live: false, record, _bus_outs: None }
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
///
/// `mains` are the hosted plugins of the headless backend. This thread
/// created them, so it is their main thread: between the server's wake-ups
/// it serves what they ask the host for (callbacks, restarts, flushes), at
/// the rate the app's frame does.
fn serve_stdio(server: McpServer, rpc_stdout: std::fs::File, mains: &mut [ClapVoiceMain]) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    // `block_on` polls this future on the calling thread, so it may hold
    // the plugins' main halves, which never leave it.
    rt.block_on(async move {
        let writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(rpc_stdout));
        let transport = (tokio::io::stdin(), writer);
        let service = server.serve(transport).await?;
        let waiting = service.waiting();
        tokio::pin!(waiting);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(33));
        loop {
            tokio::select! {
                ended = &mut waiting => {
                    ended?;
                    break;
                }
                _ = tick.tick() => {
                    for main in mains.iter_mut() {
                        main.service();
                    }
                }
            }
        }
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
    let recorder = audio.record.clone(); // shut it down after the session ends
    let state = Arc::new(Mutex::new(State {
        graph: Graph::demo_patch(),
        backend: Backend::Local(audio.bus),
        level: audio.level,
        sample_rate: audio.sample_rate,
        audio_live: audio.live,
        playing: false,
        rigs,
        graph_pub: Some(audio.graph_pub),
        node_times: Some(audio.node_times),
        count_in_enabled: ci_enabled,
        count_in_bars: ci_bars,
        cc_maps: std::collections::HashMap::new(),
        device_instruments: Default::default(),
        instrument_bound: Vec::new(),
        record: audio.record,
        record_sync: false,
        rig_presets_dir: crate::session::rig_presets_dir(),
        rig_types: Vec::new(),
        setlist: crate::setlist::Setlist::default(),
    }));
    let server = McpServer { state, tool_router: McpServer::tool_router() };
    let result = serve_stdio(server, rpc_stdout, &mut audio.mains);

    // The stream and the CLAP main-thread handles stayed alive on this (the
    // creating) thread for the whole session; tear them down last. Stop + join
    // the recorder writer first so a take in flight is drained and finalized.
    recorder.shutdown();
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
        node_times: None,
        count_in_enabled: false,
        count_in_bars: 1,
        cc_maps: std::collections::HashMap::new(),
        device_instruments: Default::default(),
        instrument_bound: Vec::new(),
        record: Arc::new(crate::record::RecorderShared::new()),
        record_sync: false,
        rig_presets_dir: crate::session::rig_presets_dir(),
        rig_types: Vec::new(),
        setlist: crate::setlist::Setlist::default(),
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
    serve_stdio(server, rpc_stdout, &mut [])
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

    #[test]
    fn get_engine_info_reports_the_headless_engines_units_and_reset_zeroes_xruns() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 256 * 2];
        for _ in 0..4 { engine.render(&mut buf, 2); }
        let info = tool_json(&server.get_engine_info());
        for key in ["audio_live", "sample_rate", "buffer_frames", "buffer_ms", "inputs", "outputs", "dsp_percent",
            "render_avg_us", "render_max_us", "late_blocks", "xruns", "midi_input_latency_ms", "nodes", "nodes_not_timed"] {
            assert!(info.get(key).is_some(), "get_engine_info missing '{key}': {info}");
        }
        assert_eq!(info["outputs"], 2);
        // The demo patch's two delays render (no CLAP voice is hosted here).
        let mut names: Vec<&str> = info["nodes"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["Delay A", "Delay B"], "units by display name: {info}");
        assert!(info["nodes"].as_array().unwrap().iter().all(|n| n["max_us"].as_f64().unwrap() >= n["avg_us"].as_f64().unwrap()), "{info}");

        bridge::XRUNS.store(5, Ordering::Relaxed);
        tool_json(&server.reset_engine_peaks());
        assert_eq!(tool_json(&server.get_engine_info())["xruns"], 0, "a reset zeroes the xrun count");
    }

    #[test]
    fn headless_set_master_target_reaches_the_engine_and_reads_back() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 256 * 2];
        let mut render = |engine: &mut GraphEngine| for _ in 0..4 { engine.render(&mut buf, 2) };
        render(&mut engine);
        assert!(tool_json(&server.get_master_target())["target_lufs"].is_null());
        // Nothing sounds in the demo patch: the target is stored, the answer says why nothing moved.
        let set = tool_json(&server.set_master_target(Parameters(SetMasterTargetArgs { target: Some("apple".into()), lufs: None })));
        assert_eq!(set["target_lufs"], -16.0);
        assert!(set["status"].as_str().unwrap().contains("no sound was measured"), "{set}");
        render(&mut engine);
        assert_eq!(engine.master_target().lufs, Some(-16.0), "the engine holds it");
        let read = tool_json(&server.get_master_target());
        assert_eq!((read["target_lufs"].as_f64(), read["gain_db"].as_f64()), (Some(-16.0), Some(0.0)), "{read}");
        assert_eq!(tool_json(&server.get_status())["master_target"]["target_lufs"], -16.0);
        // A number, off, and what is refused.
        tool_json(&server.set_master_target(Parameters(SetMasterTargetArgs { target: None, lufs: Some(-11.5) })));
        render(&mut engine);
        assert_eq!(engine.master_target().lufs, Some(-11.5));
        tool_json(&server.set_master_target(Parameters(SetMasterTargetArgs { target: Some("off".into()), lufs: None })));
        render(&mut engine);
        assert_eq!(engine.master_target(), crate::meter::MasterTarget::OFF);
        let bad = server.set_master_target(Parameters(SetMasterTargetArgs { target: Some("loud".into()), lufs: None }));
        let bad = serde_json::to_value(bad.as_ref().unwrap()).unwrap();
        assert_eq!(bad["isError"], true, "{bad}");
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
            node_times: Some(engine.node_times_handle()),
            count_in_enabled: false,
            count_in_bars: 1,
            cc_maps: std::collections::HashMap::new(),
        device_instruments: Default::default(),
        instrument_bound: Vec::new(),
            record: Arc::new(crate::record::RecorderShared::new()),
            record_sync: false,
            rig_presets_dir: std::env::var_os("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(format!("test-mcp-rig-presets-{}", std::process::id())),
            rig_types: Vec::new(),
        setlist: crate::setlist::Setlist::default(),
        };
        let server = McpServer {
            state: Arc::new(Mutex::new(state)),
            tool_router: McpServer::tool_router(),
        };
        (server, engine)
    }

    #[test]
    fn install_instrument_confirm_gate_refuses_without_confirm() {
        let (server, _engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        let args = |v: Value| -> InstallInstrumentArgs { serde_json::from_value(v).unwrap() };
        assert!(is_err(server.install_instrument(Parameters(args(json!({"id": "setbfree", "confirm": false}))))),
            "install without confirm is refused (rule 23)");
        // Headless MCP has no running app; even a confirmed install is refused,
        // never a local pacman/git subprocess on the server thread.
        assert!(is_err(server.install_instrument(Parameters(args(json!({"id": "setbfree", "confirm": true}))))),
            "headless install (no running app) is refused even with confirm");
    }

    /// #19: headless knob names are the rack labels (long names as aliases),
    /// and headless list_params lists the knobs like the attached app does.
    #[test]
    fn headless_knob_names_match_the_rack_labels() {
        let lim = NodeKind::Dsp(crate::control::DspKind::Limiter);
        assert_eq!(McpServer::resolve_headless_knob(&lim, "CEIL"), Ok((0, crate::matrix::CcTarget::Knob)));
        assert_eq!(McpServer::resolve_headless_knob(&lim, "ceiling"), Ok((0, crate::matrix::CcTarget::Knob)), "CEILING never binds DRIVE");
        assert_eq!(McpServer::resolve_headless_knob(&lim, "DRIVE"), Ok((1, crate::matrix::CcTarget::Knob)));
        let filter = NodeKind::Dsp(crate::control::DspKind::Filter);
        assert_eq!(McpServer::resolve_headless_knob(&filter, "RESO"), Ok((1, crate::matrix::CcTarget::Knob)));
        assert_eq!(McpServer::resolve_headless_knob(&filter, "Resonance"), Ok((1, crate::matrix::CcTarget::Knob)));
        assert!(McpServer::resolve_headless_knob(&NodeKind::Keys, "RATE").is_err(), "local-only knob");
        assert_eq!(McpServer::resolve_headless_knob(&NodeKind::SystemAudio, "GAIN"), Ok((crate::engine::P_SYSTEM_GAIN, crate::matrix::CcTarget::Knob)));

        let (server, _engine) = local_server();
        // Demo node 2 = DELAY A: knobs TIME + MIX.
        let out = tool_json(&server.list_params(Parameters(ListParamsArgs { node: 2, query: None, ..Default::default() })));
        let labels: Vec<&str> = out["knobs"].as_array().unwrap().iter().map(|k| k["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["TIME", "MIX"], "{out}");
        assert_eq!(out["knobs"][0]["target"], json!("knob"));
        // set_cc_map with a rack label works headless.
        let set: SetCcMapArgs = serde_json::from_value(json!({"node": 2, "source": "hw:0", "cc": 20, "knob": "mix"})).unwrap();
        let res = tool_json(&server.set_cc_map(Parameters(set)));
        assert_eq!(res["param"], json!(crate::engine::P_MIX), "{res}");
        assert_eq!(res["target"], json!("knob"));
    }

    /// An LV2 node's main controls reach a headless agent first, and their
    /// names bind like the app's face knobs (the same `MainControls::find`).
    #[test]
    fn headless_list_params_leads_with_an_lv2_plugins_main_controls() {
        use crate::main_controls::{MainControl, MainControls, MainSource};
        let uri = "urn:kabelsalat:test-headless-synth";
        let control = |id: u32, name: &str| MainControl { id, name: name.into(), page: String::new(), min: 0.0, max: 1.0, default: 0.5 };
        crate::lv2::set_main_controls_for_test(uri, MainControls {
            source: MainSource::Designation,
            controls: vec![control(7, "Filter Cutoff"), control(2, "Resonance")],
        });
        let kind = NodeKind::Lv2 { uri: control::Lv2Uri::new(uri) };
        assert_eq!(McpServer::resolve_headless_knob(&kind, "filter cutoff"), Ok((7, crate::matrix::CcTarget::Knob)), "by full name");
        assert_eq!(McpServer::resolve_headless_knob(&kind, "RESO"), Ok((2, crate::matrix::CcTarget::Knob)), "by knob label");
        assert_eq!(McpServer::resolve_headless_knob(&kind, "swell"), Ok((11, crate::matrix::CcTarget::MidiCc)), "standard instrument CC");
        assert!(McpServer::resolve_headless_knob(&kind, "GAIN").is_err(), "no knob on port 0 by a made-up name");

        let (server, mut engine) = local_server();
        // Not through add_node: it refuses a URI no installed plugin has.
        let node = {
            let mut st = server.state.lock().unwrap();
            send_cmd(&mut st, Command::AddNode { kind: kind.clone(), name: name_bytes("Synth"), rig: 0, wire: false }).unwrap();
            st.graph.add_node(kind.clone(), name_bytes("Synth"))
        };
        engine.render(&mut vec![0.0f32; 64 * 2], 64);
        let out = tool_json(&server.list_params(Parameters(ListParamsArgs { node, query: None, ..Default::default() })));
        assert_eq!(out["main_controls_source"], json!("designation"), "{out}");
        assert_eq!(out["main_controls"][0]["name"], json!("Filter Cutoff"));
        assert_eq!(out["main_controls"][0]["id"], json!(7));
        assert_eq!(out["main_controls"][1]["knob"], json!("RESO"));
        let labels: Vec<&str> = out["knobs"].as_array().unwrap().iter().map(|k| k["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["CUTOFF", "RESO"], "the knobs the app's face shows");
        // set_param by a main control's name sets it; a pedal name is a MIDI CC,
        // never sent as that number's parameter.
        let set = |name: &str| -> SetParamArgs { serde_json::from_value(json!({"node": node, "name": name, "normalized": 0.5})).unwrap() };
        assert_eq!(tool_json(&server.set_param(Parameters(set("filter cutoff"))))["param"], json!(7));
        let swell = serde_json::to_value(server.set_param(Parameters(set("swell"))).unwrap()).unwrap();
        assert_eq!(swell["isError"], json!(true), "{swell}");
        assert!(swell.to_string().contains("MIDI CC 11") && swell.to_string().contains("set_cc_map"), "{swell}");
        // A node without a plugin has none, and says so with an empty list.
        let delay = tool_json(&server.list_params(Parameters(ListParamsArgs { node: 2, query: None, ..Default::default() })));
        assert_eq!(delay["main_controls"], json!([]));
        assert_eq!(delay["main_controls_source"], Value::Null);
    }

    #[test]
    fn headless_set_param_supports_name_id_and_rejects_clap_internal() {
        let (server, _engine) = local_server();
        // Set Delay MIX by name.
        let by_name: SetParamArgs = serde_json::from_value(json!({"node": 2, "name": "MIX", "value": 0.4})).unwrap();
        let out = tool_json(&server.set_param(Parameters(by_name)));
        assert_eq!(out["param"], json!(crate::engine::P_MIX), "{out}");
        assert!((out["value"].as_f64().unwrap() - 0.4).abs() < 1e-6, "{out}");
        // Set Delay TIME by id with normalized.
        let by_id: SetParamArgs = serde_json::from_value(json!({"node": 2, "param": crate::engine::P_TIME, "normalized": 0.7})).unwrap();
        let out = tool_json(&server.set_param(Parameters(by_id)));
        assert_eq!(out["param"], json!(crate::engine::P_TIME), "{out}");
        // CLAP internal param headless tells caller to use --attach.
        let clap_internal: SetParamArgs = serde_json::from_value(json!({"node": 1, "param": 42, "value": 0.5})).unwrap();
        let err = server.set_param(Parameters(clap_internal)).unwrap();
        assert_eq!(err.is_error, Some(true), "CLAP internal param rejected headless: {err:?}");
        // CLAP rack LEVEL knob (param 0 / name LEVEL) still works headless.
        let clap_level: SetParamArgs = serde_json::from_value(json!({"node": 1, "name": "LEVEL", "value": 0.8})).unwrap();
        let out = tool_json(&server.set_param(Parameters(clap_level)));
        assert_eq!(out["param"], json!(crate::engine::P_LEVEL), "{out}");
    }

    /// #22: play_file needs the app (the path cannot cross the command bus);
    /// stop_file still mutes the player headless.
    #[test]
    fn play_file_needs_the_app_and_stop_file_mutes_headless() {
        let (server, mut engine) = local_server();
        let res = serde_json::to_value(server.play_file(Parameters(PlayFileArgs { path: "/x.wav".into(), gain: None, loop_on: None, sync: None, beats: None, offset: None, bpm: None })).unwrap()).unwrap();
        assert_eq!(res["isError"], json!(true), "{res}");
        assert_eq!(tool_json(&server.stop_file()), json!({"stopped": true}));
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
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
        let added = tool_json(&server.add_rig(Parameters(AddRigArgs { name: Some("Bass".into()), plugin_id: None })));
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
    fn clone_rig_through_the_engine_headless() {
        let (server, mut engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        let mut buf = vec![0.0f32; 64 * 2];
        let _ = tool_json(&server.add_rig(Parameters(AddRigArgs { name: Some("Lead".into()), plugin_id: None })));
        engine.render(&mut buf, 64);
        let _ = tool_json(&server.set_rig_mix(Parameters(SetRigMixArgs {
            rig: 1,
            vol: Some(0.55),
            pan: Some(-0.3),
            mute: Some(false),
            solo: Some(false),
        })));
        engine.render(&mut buf, 64);

        // Clone active rig (1 = "Lead") with default name -> "Lead Copy".
        let cloned = tool_json(&server.clone_rig(Parameters(CloneRigArgs { rig: None, name: None })));
        assert_eq!(cloned["rigs"].as_array().unwrap().len(), 3);
        assert_eq!(cloned["rigs"][2]["name"], json!("Lead Copy"));
        assert_eq!(cloned["active"], json!(2));
        engine.render(&mut buf, 64);
        let g = engine.graph();
        let rig1_n = g.nodes.iter().filter(|n| n.rig == 1).count();
        let rig2_n = g.nodes.iter().filter(|n| n.rig == 2).count();
        assert_eq!(rig2_n, rig1_n, "clone has same node count as source");

        // Second clone of rig 1 with custom name.
        let cloned2 = tool_json(&server.clone_rig(Parameters(CloneRigArgs { rig: Some(1), name: Some("Lead 2".into()) })));
        assert_eq!(cloned2["rigs"][3]["name"], json!("Lead 2"));
        assert_eq!(cloned2["active"], json!(3));

        // Out-of-range rig fails cleanly.
        assert!(is_err(server.clone_rig(Parameters(CloneRigArgs { rig: Some(9), name: None }))));
    }

    #[test]
    fn save_load_list_rig_presets_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let _ = tool_json(&server.add_rig(Parameters(AddRigArgs { name: Some("Lead".into()), plugin_id: None })));
        engine.render(&mut buf, 64);
        let _ = tool_json(&server.set_rig_mix(Parameters(SetRigMixArgs {
            rig: 1,
            vol: Some(0.6),
            pan: Some(-0.25),
            mute: Some(false),
            solo: Some(true),
        })));
        engine.render(&mut buf, 64);

        let saved = tool_json(&server.save_rig(Parameters(SaveRigArgs { name: "Lead Pack".into(), rig: Some(1) })));
        assert_eq!(saved["saved"], json!("Lead Pack"));
        let listed = tool_json(&server.list_rig_presets());
        assert_eq!(listed["presets"], json!(["Lead Pack"]));

        let loaded = tool_json(&server.load_rig(Parameters(LoadRigArgs { name: "Lead Pack".into(), rig: Some(0) })));
        assert_eq!(loaded["rig"], json!(0));
        engine.render(&mut buf, 64);
        let rigs = tool_json(&server.get_rigs());
        assert_eq!(rigs["rigs"][0]["name"], json!("Lead"));
        let dir = server.state.lock().unwrap().rig_presets_dir.clone();
        let _ = std::fs::remove_dir_all(&dir);
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
            device: None,
            channels: None,
            rig: None,
            semis: None,
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

    /// Headless sound tools address the same units as the app's: CLAP
    /// synths and LV2 plugins that declare themselves instruments.
    #[test]
    fn headless_sound_tools_see_instruments_not_effects() {
        let mut g = Graph::default();
        g.reserve(8, 8);
        g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let surge = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE_ID) }, name_bytes("SURGE"));
        let organ = g.add_node(NodeKind::Lv2 { uri: control::Lv2Uri::new("urn:test:organ") }, name_bytes("ORGAN"));
        g.add_node(NodeKind::Lv2 { uri: control::Lv2Uri::new("urn:test:reverb") }, name_bytes("VERB"));
        g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let lv2 = |uri: &str, is_instrument| crate::lv2::Lv2PluginInfo { uri: uri.into(), name: uri.into(), num_ports: 4, is_instrument, ..Default::default() };
        let units = headless_instruments(&g, &[lv2("urn:test:organ", true), lv2("urn:test:reverb", false)]);
        assert_eq!(units.iter().map(|u| (u.node, u.name.as_str())).collect::<Vec<_>>(), [(surge, "SURGE"), (organ, "ORGAN")]);
        assert_eq!(units[1].instrument, crate::sounds::Instrument::Lv2("urn:test:organ".into()));
        let which = crate::sounds::pick_unit(&units, 0, "Rig 1", None).unwrap_err();
        assert!(which.contains("2 different instruments"), "{which}");
        assert_eq!(sound_args("jazz", Some(1), Some(organ)), json!({"query": "jazz", "rig": 1, "node": organ}));
        assert_eq!(sound_args("jazz", None, None), json!({"query": "jazz"}));
    }

    #[test]
    fn set_split_tool_targets_the_keys_node() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let r = tool_json(&server.set_split(Parameters(SetSplitArgs { node: None, point: Some(60), low_octave: None, high_octave: None })));
        assert_eq!(r["node"], json!(0), "defaults to the first Keys node");
        assert_eq!(r["split"], json!(true));
        engine.render(&mut buf, 2);
        assert_eq!(engine.graph().node(0).unwrap().split, Some(60), "engine received SetNodeSplit");
        let keys = |v: &Value| v["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(0)).cloned().unwrap();
        assert_eq!(keys(&tool_json(&server.get_graph()))["split"], json!(60), "get_graph exposes split");
        tool_json(&server.set_split(Parameters(SetSplitArgs { node: Some(0), point: None, low_octave: None, high_octave: None })));
        engine.render(&mut buf, 2);
        assert_eq!(engine.graph().node(0).unwrap().split, None);
        assert_eq!(keys(&tool_json(&server.get_graph()))["split"], Value::Null);
        // Non-Keys / unknown nodes and bad points are rejected.
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: Some(1), point: Some(60), low_octave: None, high_octave: None }))));
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: Some(999), point: Some(60), low_octave: None, high_octave: None }))));
        assert!(is_err(server.set_split(Parameters(SetSplitArgs { node: None, point: Some(200), low_octave: None, high_octave: None }))));
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
            target: None,
            knob: None,
            mix: None,
            min: None,
            max: None,
            in_min: None,
            in_max: None,
        })));
        assert_eq!(res["cc"], json!(74));
        assert_eq!(res["param"], json!(42));
        assert_eq!(res["target"], json!("plugin"));
        let res_knob = tool_json(&server.set_cc_map(Parameters(SetCcMapArgs {
            node: 1,
            source: "hw:0".into(),
            cc: 7,
            param: None,
            target: None,
            knob: Some("LEVEL".into()),
            mix: None,
            min: None,
            max: Some(0.6),
            in_min: None,
            in_max: None,
        })));
        assert_eq!(res_knob["param"], json!(0));
        assert_eq!(res_knob["cc_map"][0]["range"], json!([0.0, 0.6]), "{res_knob}");
        assert_eq!(res_knob["target"], json!("knob"));
        let listed = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: None, source: None })));
        let all_bindings: Vec<&Value> = listed["cc_maps"].as_array().unwrap().iter()
            .flat_map(|e| e["bindings"].as_array().unwrap().iter())
            .collect();
        assert!(all_bindings.iter().any(|b| b["cc"] == json!(74) && b["target"] == json!("plugin")));
        assert!(all_bindings.iter().any(|b| b["cc"] == json!(7) && b["target"] == json!("knob") && b["range"] == json!([0.0, 0.6])));
        let dsp = tool_json(&server.list_params(Parameters(ListParamsArgs { node: 2, query: Some("mix".into()), ..Default::default() })));
        assert_eq!(dsp["params"][0]["name"], json!("Mix"));
        engine.render(&mut buf, 2);
        let cleared = tool_json(&server.clear_cc_map(Parameters(ClearCcMapArgs {
            node: 1,
            source: "keys".into(),
            cc: None,
            target: None,
            knob: None,
            mix: None,
        })));
        assert_eq!(cleared["cc_map"].as_array().unwrap().len(), 0);
        let cleared_knob = tool_json(&server.clear_cc_map(Parameters(ClearCcMapArgs {
            node: 1,
            source: "hw:0".into(),
            cc: None,
            target: Some("knob".into()),
            knob: None,
            mix: None,
        })));
        assert_eq!(cleared_knob["cc_map"].as_array().unwrap().len(), 0);
        engine.render(&mut buf, 2);
        let empty = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: None, source: None })));
        assert_eq!(empty["cc_maps"].as_array().unwrap().len(), 0);
    }

    /// #75: a mixer control binds headless too: the rig bus on the rig's
    /// Master out node, a source strip's pan/mute/solo on the source node.
    #[test]
    fn a_mixer_control_binds_through_set_cc_map_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let master = server.state.lock().unwrap().graph.nodes.iter()
            .find(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).unwrap();
        let set = |node: NodeId, cc: u8, mix: &str| server.set_cc_map(Parameters(SetCcMapArgs {
            node, source: "hw:0".into(), cc, param: None, target: None, knob: None, mix: Some(mix.into()), min: None, max: None, in_min: None, in_max: None,
        }));
        let res = tool_json(&set(master, 20, "vol"));
        assert_eq!((&res["target"], &res["param"]), (&json!("mix"), &json!(0)), "{res}");
        let res = tool_json(&set(master, 21, "mute"));
        assert_eq!(res["cc_map"].as_array().unwrap().len(), 2, "{res}");
        let res = tool_json(&set(1, 22, "pan"));
        assert_eq!(res["target"], json!("mix"), "{res}");
        // A source strip has no fader of its own: the answer says what to bind.
        let refused = set(1, 23, "vol").unwrap();
        assert_eq!(refused.is_error, Some(true));
        assert!(format!("{refused:?}").contains("LEVEL"), "{refused:?}");
        assert_eq!(set(master, 24, "width").unwrap().is_error, Some(true));
        // The engine got the bindings: the controllers move the mix.
        {
            let mut st = server.state.lock().unwrap();
            for (cc, value) in [(20u8, 64u8), (21, 127), (22, 0)] {
                send_cmd(&mut st, Command::Cc { source: SourceId::Hardware(0), channel: 0, cc, value }).unwrap();
            }
        }
        engine.render(&mut buf, 2);
        engine.render(&mut buf, 2);
        let cleared = tool_json(&server.clear_cc_map(Parameters(ClearCcMapArgs {
            node: master, source: "hw:0".into(), cc: None, target: None, knob: None, mix: Some("mute".into()),
        })));
        assert_eq!(cleared["cc_map"].as_array().unwrap().len(), 1, "only the mute binding is cleared: {cleared}");
        let published = server.state.lock().unwrap().graph_pub.clone().unwrap();
        let p = published.lock().unwrap();
        assert_eq!(p.mix[0], crate::engine::RigMix { vol: 64.0 / 127.0, pan: 0.0, mute: true, solo: false });
        assert!(p.strips.iter().any(|s| s.node == 1 && s.mix.pan == -1.0), "the source strip is panned hard left");
    }

    /// #56 headless: a profile set for a device index binds the organ in
    /// the engine, raw controller bytes from that device land on the organ's
    /// own steps, and "none" takes the bindings back.
    #[test]
    fn a_device_instrument_profile_drives_the_organ_headless() {
        let mut graph = Graph::demo_patch();
        let organ = graph.add_node(NodeKind::Lv2 { uri: control::Lv2Uri::new(crate::organ::B_SYNTH) }, crate::control::name_bytes("ORGAN"));
        let (server, mut engine) = local_server();
        {
            let mut st = server.state.lock().unwrap();
            send_cmd(&mut st, Command::LoadGraph { graph: graph.clone() }).unwrap();
            st.graph = graph;
            // Both ports play the organ (the routes its instance gets when
            // it attaches, rule 153; no plugin is loaded here).
            for dev in [0, 1] {
                send_cmd(&mut st, Command::MatrixRule {
                    source: SourceId::Hardware(dev), dest: organ, channel: None, transpose: 0,
                    key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: true,
                }).unwrap();
            }
        }
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);

        let list = tool_json(&server.list_instrument_profiles());
        let electro = list["profiles"].as_array().unwrap().iter().find(|p| p["id"] == "nord-electro-2").expect("the built-in profile");
        assert_eq!(electro["drives"][0]["node"], json!(organ), "{electro}");
        assert_eq!(electro["drives"][0]["controls"].as_array().unwrap().len(), 12);
        assert_eq!(list["devices"], json!([]), "nothing picked yet");

        let set = |device: u32, profile: &str| server.set_device_instrument(Parameters(SetDeviceInstrumentArgs { device, profile: profile.into() }));
        let unknown = set(1, "nord-stage").unwrap();
        assert!(unknown.is_error == Some(true) && format!("{unknown:?}").contains("nord-electro-2"), "{unknown:?}");
        assert_eq!(set(99, "nord-electro-2").unwrap().is_error, Some(true));

        let res = tool_json(&set(1, "Nord Electro 2"));
        assert_eq!((&res["profile"], &res["source"]), (&json!("nord-electro-2"), &json!("hw:1")), "{res}");
        assert_eq!(res["drives"], json!([{"node": organ, "name": "ORGAN", "controls": 12}]), "{res}");
        let maps = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: Some(organ), source: Some("hw:1".into()) })));
        assert_eq!(maps["cc_maps"][0]["bindings"].as_array().unwrap().len(), 12, "{maps}");
        let list = tool_json(&server.list_instrument_profiles());
        assert_eq!((&list["devices"][0]["device"], &list["devices"][0]["profile"]), (&json!(1), &json!("nord-electro-2")), "{list}");

        // The Nord's bytes: B0 12 56 (8' drawbar), B0 52 7F (rotary fast),
        // B0 57 00 (percussion off); the same drawbar from another port.
        {
            let mut st = server.state.lock().unwrap();
            for (dev, cc, value) in [(1u32, 0x12u8, 0x56u8), (1, 0x52, 0x7F), (1, 0x57, 0x00), (0, 0x12, 0x7F)] {
                send_cmd(&mut st, Command::Cc { source: SourceId::Hardware(dev), channel: 0, cc, value }).unwrap();
            }
        }
        engine.render(&mut buf, 2);
        let log = engine.cc_log.clone();
        let value_of = |param| log.iter().rev().find(|(n, p, _)| *n == organ && *p == param).map(|(_, _, v)| *v as f32);
        assert_eq!(log.len(), 3, "only the profiled port's controllers reach the organ: {log:?}");
        assert_eq!(value_of(crate::organ::drawbar_param(2)).map(crate::organ::drawbar_position), Some(6));
        assert_eq!(value_of(crate::organ::P_LESLIE).and_then(|v| crate::organ::step_name(crate::organ::P_LESLIE, v)), Some("FAST"));
        assert_eq!(value_of(crate::organ::P_PERC).and_then(|v| crate::organ::step_name(crate::organ::P_PERC, v)), Some("OFF"));

        // A controller rebound by hand since stays; the rest goes with "none".
        tool_json(&server.set_cc_map(Parameters(SetCcMapArgs {
            node: organ, source: "hw:1".into(), cc: 111, param: Some(crate::organ::P_REVERB), target: Some("knob".into()), knob: None, mix: None, min: None, max: None, in_min: None, in_max: None,
        })));
        let off = tool_json(&set(1, "none"));
        assert_eq!((&off["profile"], &off["drives"]), (&Value::Null, &json!([])), "{off}");
        let maps = tool_json(&server.list_cc_maps(Parameters(ListCcMapsArgs { node: Some(organ), source: Some("hw:1".into()) })));
        let left: Vec<u64> = maps["cc_maps"][0]["bindings"].as_array().unwrap().iter().map(|b| b["cc"].as_u64().unwrap()).collect();
        assert_eq!(left, [111], "{maps}");
        {
            let mut st = server.state.lock().unwrap();
            send_cmd(&mut st, Command::Cc { source: SourceId::Hardware(1), channel: 0, cc: 0x12, value: 0 }).unwrap();
        }
        engine.cc_log.clear();
        engine.render(&mut buf, 2);
        assert!(engine.cc_log.is_empty(), "an unbound drawbar moves nothing: {:?}", engine.cc_log);
    }

    #[test]
    fn set_time_signature_reaches_the_engine_and_reads_back_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 480 * 2];
        engine.render(&mut buf, 2);
        assert_eq!(tool_json(&server.get_status())["time_signature"], json!({"numerator": 4, "denominator": 4}));
        let set = tool_json(&server.set_time_signature(Parameters(SetTimeSignatureArgs { numerator: 6, denominator: 8 })));
        assert_eq!(set, json!({"numerator": 6, "denominator": 8}));
        engine.render(&mut buf, 2);
        assert_eq!(engine.transport().sig, crate::transport::TimeSig { num: 6, den: 8 }, "the engine holds it");
        assert_eq!(tool_json(&server.get_status())["time_signature"], json!({"numerator": 6, "denominator": 8}));
        // A bar the clock cannot count is refused and changes nothing.
        for (n, d) in [(0, 4), (13, 4), (4, 3)] {
            let bad = server.set_time_signature(Parameters(SetTimeSignatureArgs { numerator: n, denominator: d }));
            assert_eq!(bad.unwrap().is_error, Some(true), "{n}/{d} must be refused");
        }
        engine.render(&mut buf, 2);
        assert_eq!(engine.transport().sig, crate::transport::TimeSig { num: 6, den: 8 });
    }

    #[test]
    fn setlist_tools_drive_engine_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 480 * 2];
        let added = tool_json(&server.set_setlist_song(Parameters(SetSetlistSongArgs {
            song: None,
            name: Some("Opener".into()),
            bpm: Some(Some(114.0)),
            time_signature: Some(Some(json!({"numerator": 3, "denominator": 4}))),
            parts: Some(vec![
                SetlistPartArg {
                    name: "Verse".into(),
                    rigs: Some(vec![SetlistPartRigArg { rig: 0, key_lo: Some(36), key_hi: Some(84), transpose: Some(0), level: Some(0.9) }]),
                    capture_knobs: None,
                },
                SetlistPartArg {
                    name: "Chorus".into(),
                    rigs: Some(vec![SetlistPartRigArg { rig: 1, key_lo: Some(48), key_hi: Some(96), transpose: Some(12), level: Some(0.8) }]),
                    capture_knobs: None,
                },
            ]),
            move_to: None,
            remove: None,
        })));
        assert_eq!(added["songs"].as_array().unwrap().len(), 1);
        let sel = tool_json(&server.select_setlist_part(Parameters(SelectSetlistPartArgs {
            action: None,
            song: Some(0),
            part: Some(0),
        })));
        assert_eq!(sel["active"]["part_name"], json!("Verse"));
        engine.render(&mut buf, 2);
        assert_eq!(engine.transport().sig, crate::transport::TimeSig { num: 3, den: 4 });
        let stepped = tool_json(&server.select_setlist_part(Parameters(SelectSetlistPartArgs {
            action: Some("next_part".into()),
            song: None,
            part: None,
        })));
        assert_eq!(stepped["active"]["part_name"], json!("Chorus"));
        let fs = tool_json(&server.set_setlist_footswitch(Parameters(SetSetlistFootswitchArgs {
            action: "next".into(),
            cc: Some(64),
            channel: None,
        })));
        assert_eq!(fs["foot_switch"]["next_cc"]["cc"], json!(64));
        let got = tool_json(&server.get_setlist());
        assert_eq!(got["active"]["part"], json!(1));
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

    #[test]
    fn record_start_fails_without_engine_and_set_record_sync_mirrors() {
        // Finding #9/#10: headless local_server has a dummy (non-live) recorder,
        // so record_start must report unavailable, not claim success. And
        // set_record_sync exists + mirrors into State.
        let (server, mut engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        assert!(is_err(server.record_start()), "record_start must not claim success without a recorder");
        let mut buf = vec![0.0f32; 480 * 2];
        let r = tool_json(&server.set_record_sync(Parameters(RecordSyncArgs { on: true })));
        assert_eq!(r, json!({"record_sync": true}));
        engine.render(&mut buf, 2); // drain SetRecordSync into the engine
        assert!(engine.transport_pub().record_sync, "engine adopted record_sync");
    }

    #[test]
    fn strip_knob_tools_require_attached_app_when_headless() {
        let (server, _engine) = local_server();
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        assert!(is_err(server.get_strip_knobs(Parameters(GetStripKnobsArgs { node: None }))));
        assert!(is_err(server.set_strip_knob(Parameters(SetStripKnobArgs { node: 1, knob: "LEVEL".into() }))));
    }

    #[test]
    fn list_channels_headless_points_at_the_attached_app() {
        let (server, _engine) = local_server();
        let res = server.list_channels(Parameters(ListChannelsArgs { filter: None }));
        assert!(format!("{res:?}").contains("mcp --attach"), "headless list_channels names the attach path: {res:?}");
    }

    #[test]
    fn list_plugins_validates_and_filters_by_type_and_format() {
        let (server, _engine) = local_server();
        let bad = server.list_plugins(Parameters(ListPluginsArgs {
            plugin_type: Some("nonsense".into()),
            ..Default::default()
        }));
        assert!(serde_json::to_value(bad.unwrap()).unwrap()["isError"] == json!(true));

        let fx = tool_json(&server.list_plugins(Parameters(ListPluginsArgs {
            plugin_type: Some("effect".into()),
            format: Some("builtin".into()),
            ..Default::default()
        })));
        let builtins = fx["builtin"].as_array().unwrap();
        assert!(builtins.iter().any(|b| b["name"] == "Delay"), "{fx}");
        assert!(builtins.iter().any(|b| b["name"] == "Reverb"), "{fx}");
        assert!(!builtins.iter().any(|b| b["name"] == "Metronome"), "Metronome is a utility, not an effect");
        assert_eq!(fx["counts"]["instrument"], json!(0));
        assert!(fx["effect_categories"]["delay_reverb"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn set_bus_output_and_get_bus_outputs_drive_engine_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };
        // Add an aux bus so both rig bus and aux bus paths are exercised.
        let added = tool_json(&server.add_bus(Parameters(AddBusArgs { name: Some("Mon".into()), rig: Some(0) })));
        let bus = added["bus"].as_u64().unwrap() as u32;
        engine.render(&mut buf, 2);

        let set_rig = tool_json(&server.set_bus_output(Parameters(SetBusOutputArgs {
            rig: Some(0), bus: None, output: Some("alsa_output.m2".into()),
        })));
        assert_eq!(set_rig["output"], json!("alsa_output.m2"));
        let set_aux = tool_json(&server.set_bus_output(Parameters(SetBusOutputArgs {
            rig: None, bus: Some(bus), output: Some("alsa_output.phones".into()),
        })));
        assert_eq!(set_aux["output"], json!("alsa_output.phones"));
        engine.render(&mut buf, 2);

        let listed = tool_json(&server.get_bus_outputs(Parameters(GetBusOutputsArgs {})));
        let buses = listed["buses"].as_array().unwrap();
        assert!(buses.iter().any(|b| b["rig"] == json!(0) && b["output"] == json!("alsa_output.m2")), "{listed}");
        assert!(buses.iter().any(|b| b["bus"] == json!(bus) && b["output"] == json!("alsa_output.phones")), "{listed}");

        // Invalid arguments are refused.
        assert!(is_err(server.set_bus_output(Parameters(SetBusOutputArgs { rig: Some(0), bus: Some(bus), output: Some("x".into()) }))));
        assert!(is_err(server.set_bus_output(Parameters(SetBusOutputArgs { rig: None, bus: None, output: Some("x".into()) }))));
        assert!(is_err(server.set_bus_output(Parameters(SetBusOutputArgs { rig: None, bus: Some(1), output: Some("x".into()) }))));
        assert!(is_err(server.set_bus_output(Parameters(SetBusOutputArgs { rig: Some(0), bus: None, output: Some("x".repeat(128)) }))));

        // Clearing brings the rig bus back into the master.
        let cleared = tool_json(&server.set_bus_output(Parameters(SetBusOutputArgs {
            rig: Some(0), bus: None, output: None,
        })));
        assert_eq!(cleared["output"], Value::Null);
        engine.render(&mut buf, 2);
        let listed = tool_json(&server.get_bus_outputs(Parameters(GetBusOutputsArgs {})));
        assert!(listed["buses"].as_array().unwrap().iter().any(|b| b["rig"] == json!(0) && b["output"].is_null()), "{listed}");
    }

    #[test]
    fn play_demo_and_stop_demo_drive_engine_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 256 * 2];
        let _ = server.set_bpm(Parameters(SetBpmArgs { bpm: 120.0 }));
        engine.render(&mut buf, 2);

        let started = tool_json(&server.play_demo(Parameters(PlayDemoArgs { rig: Some(0), phrase: Some("bass".into()) })));
        assert_eq!(started["playing"], json!(true));
        assert_eq!(started["rig"], json!(0));
        assert_eq!(started["phrase"], json!("bass"));
        assert_eq!(started["beats"], json!(16.0));
        assert_eq!(started["seconds"], json!(8.0));
        engine.render(&mut buf, 2);
        assert_eq!(engine.demo_pub(), Some(crate::engine::DemoPub { rig: 0, phrase: crate::demo::PhraseKind::Bass }));

        let bad_rig = server.play_demo(Parameters(PlayDemoArgs { rig: Some(5), phrase: None }));
        assert_eq!(bad_rig.unwrap().is_error, Some(true));
        let bad_phrase = server.play_demo(Parameters(PlayDemoArgs { rig: Some(0), phrase: Some("kazoo".into()) }));
        assert_eq!(bad_phrase.unwrap().is_error, Some(true));

        let stopped = tool_json(&server.stop_demo());
        assert_eq!(stopped["playing"], json!(false));
        engine.render(&mut buf, 2);
        assert_eq!(engine.demo_pub(), None);
    }

    #[test]
    fn mcp_inspect_set_and_reset_params_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];

        let insp = tool_json(&server.inspect_plugin(Parameters(InspectPluginArgs {
            query: Some("eq3".into()),
            format: Some("builtin".into()),
            node: None,
        })));
        assert_eq!(insp["matches"][0]["kind"], json!("eq3"));
        assert_eq!(insp["matches"][0]["params"].as_array().unwrap().len(), 7);

        let lp = tool_json(&server.list_params(Parameters(ListParamsArgs {
            node: 2,
            ..Default::default()
        })));
        assert_eq!(lp["total"], json!(5));

        let set = tool_json(&server.set_param(Parameters(SetParamArgs {
            node: 2,
            name: Some("pingpong".into()),
            text: Some("Ping-Pong".into()),
            ..Default::default()
        })));
        assert_eq!(set["ok"], json!(true));
        assert_eq!(set["param"], json!(3));
        assert_eq!(set["text"], json!("Ping-Pong"));

        let reset = tool_json(&server.reset_params(Parameters(ResetParamsArgs {
            node: 2,
            ..Default::default()
        })));
        assert_eq!(reset["ok"], json!(true));
        assert_eq!(reset["reset_count"], json!(5));
        engine.render(&mut buf, 2);
    }

    #[test]
    fn unit_actions_duplicate_reset_and_randomize_headless() {
        let (server, mut engine) = local_server();
        let mut buf = vec![0.0f32; 64 * 2];
        let is_err = |res: Result<CallToolResult, McpError>| {
            let v = serde_json::to_value(res.unwrap()).unwrap();
            v.get("isError").and_then(Value::as_bool) == Some(true)
        };

        // Demo patch has Delay A (node 2), Keys (node 0), MasterOut (node 3).
        let graph = tool_json(&server.get_graph());
        let delay = graph["nodes"].as_array().unwrap().iter()
            .find(|n| n["kind"] == json!({"type": "dsp", "dsp": "delay"}))
            .expect("delay node")["id"].as_u64().unwrap() as u32;
        let keys = graph["nodes"].as_array().unwrap().iter()
            .find(|n| n["kind"] == json!({"type": "keys"}))
            .expect("keys node")["id"].as_u64().unwrap() as u32;
        let master = graph["nodes"].as_array().unwrap().iter()
            .find(|n| n["kind"] == json!({"type": "master_out"}))
            .expect("master node")["id"].as_u64().unwrap() as u32;

        // Keys and Master cannot be duplicated; unknown nodes fail cleanly.
        assert!(is_err(server.duplicate_node(Parameters(DuplicateNodeArgs { node: keys, rig: None, name: None }))));
        assert!(is_err(server.duplicate_node(Parameters(DuplicateNodeArgs { node: master, rig: None, name: None }))));
        assert!(is_err(server.duplicate_node(Parameters(DuplicateNodeArgs { node: 999, rig: None, name: None }))));

        // Duplicate Delay into rig 0.
        let dup = tool_json(&server.duplicate_node(Parameters(DuplicateNodeArgs {
            node: delay, rig: None, name: Some("Delay Copy".into()),
        })));
        let dup_id = dup["node"].as_u64().unwrap() as u32;
        assert_ne!(dup_id, delay);
        assert_eq!(dup["source"], json!(delay));
        assert_eq!(dup["name"], json!("Delay Copy"));
        engine.render(&mut buf, 2);

        // Randomize with a deterministic seed returns bounded values.
        let rnd = tool_json(&server.randomize_node_params(Parameters(RandomizeNodeParamsArgs {
            node: dup_id, amount: Some(1.0), seed: Some(42),
        })));
        assert_eq!(rnd["randomized_count"], json!(2));
        for k in rnd["knobs"].as_array().unwrap() {
            let v = k["value"].as_f64().unwrap();
            assert!((0.15..=0.85).contains(&v), "bounded delay knob: {k}");
        }
        engine.render(&mut buf, 2);

        // Reset restores factory defaults (TIME 0.4, MIX 0.5).
        let rst = tool_json(&server.reset_node_params(Parameters(ResetNodeParamsArgs { node: dup_id })));
        assert_eq!(rst["reset_count"], json!(2));
        let knobs = rst["knobs"].as_array().unwrap();
        assert!((knobs[0]["value"].as_f64().unwrap() - 0.4).abs() < 1e-5);
        assert!((knobs[1]["value"].as_f64().unwrap() - 0.5).abs() < 1e-5);
        engine.render(&mut buf, 2);

        // Bypass works on the duplicated unit.
        let byp = tool_json(&server.set_node_bypass(Parameters(SetNodeBypassArgs { node: dup_id, bypass: Some(true) })));
        assert_eq!(byp, json!({"node": dup_id, "bypass": true}));
        engine.render(&mut buf, 2);
    }
}
