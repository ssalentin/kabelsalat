//! Built-in assistant — DeepSeek-powered, bring-your-own-key.
//!
//! The assistant talks to the same control surface the UI uses: today the dev
//! preview exposes status/note/octave tools; with the P1 control plane it gains
//! full rewiring powers (and the MCP server exposes the identical toolset).
//! The key is stored locally in ~/.config/kabelsalat/config.json (mode 0600)
//! and is only ever sent to api.deepseek.com.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use serde_json::{json, Value};

pub fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("kabelsalat")
        .join("config.json")
}

pub fn load_key() -> Option<String> {
    // Tests never read the user's key: every DemoApp would start a worker
    // that calls the real API (and its threads aborted the test binary at exit).
    if cfg!(test) { return None; }
    let s = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("deepseek_api_key")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

pub fn save_key(key: &str) -> std::io::Result<()> {
    save_config_field("deepseek_api_key", key)
}

/// Default chat model when none is stored.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// The stored assistant model (config `assistant_model`), else the default.
pub fn load_model() -> String {
    std::fs::read_to_string(config_path()).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("assistant_model").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// Merge key/value pairs into a config JSON file (mode 0600), preserving
/// unrelated fields.
fn save_config_values_to(path: &std::path::Path, pairs: &[(&str, Value)]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut v: Value = std::fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| json!({}));
    for (k, val) in pairs {
        v[*k] = val.clone();
    }
    std::fs::write(path, serde_json::to_string_pretty(&v).unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Merge one string field into config.json (mode 0600) — never drops the
/// other fields (writing the key used to overwrite the whole file).
pub fn save_config_field(field: &str, value: &str) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); } // tests never touch the user's real config
    save_config_values_to(&config_path(), &[(field, json!(value))])
}

/// Load persisted count-in `(enabled, bars)` from `path` (defaults: `(false, 1)`).
pub fn load_count_in_from(path: &std::path::Path) -> (bool, u8) {
    let v: Option<Value> = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok());
    let enabled = v.as_ref().and_then(|v| v.get("count_in_enabled")).and_then(Value::as_bool).unwrap_or(false);
    let bars = v
        .as_ref()
        .and_then(|v| v.get("count_in_bars"))
        .and_then(Value::as_u64)
        .map(|b| b.clamp(1, crate::transport::MAX_COUNT_IN_BARS as u64) as u8)
        .unwrap_or(1);
    (enabled, bars)
}

pub fn save_count_in_to(path: &std::path::Path, enabled: bool, bars: u8) -> std::io::Result<()> {
    let bars = bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS);
    save_config_values_to(path, &[("count_in_enabled", json!(enabled)), ("count_in_bars", json!(bars))])
}

/// Persisted count-in `(enabled, bars)` in config.json (test-safe no-op under cfg(test)).
pub fn load_count_in() -> (bool, u8) {
    if cfg!(test) { return (false, 1); }
    load_count_in_from(&config_path())
}

pub fn save_count_in(enabled: bool, bars: u8) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); }
    save_count_in_to(&config_path(), enabled, bars)
}

#[derive(Clone)]
pub struct ChatLine {
    pub role: String,
    pub text: String,
}

pub enum ToWorker {
    User(String),
    ToolResult { id: String, content: String },
}

pub enum FromWorker {
    /// Chat models the API key can use (GET /models), fetched once at start.
    Models(Vec<String>),
    Text(String),
    ToolCall { id: String, name: String, args: Value },
    Done,
    Error(String),
}

pub struct Assistant {
    pub history: Vec<ChatLine>,
    pub busy: bool,
    pub key_set: bool,
    /// Current chat model, read by the worker on every request — switching
    /// takes effect with the next message, the conversation is kept.
    model: std::sync::Arc<std::sync::Mutex<String>>,
    /// Models offered by the API (filled by the worker; a sane default list
    /// until it answers).
    pub models: Vec<String>,
    tx: Option<Sender<ToWorker>>,
    rx: Option<Receiver<FromWorker>>,
}

impl Assistant {
    pub fn new() -> Self {
        let key = load_key();
        let mut a = Self {
            history: Vec::new(),
            busy: false,
            key_set: key.is_some(),
            model: std::sync::Arc::new(std::sync::Mutex::new(load_model())),
            models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
            tx: None,
            rx: None,
        };
        if let Some(k) = key {
            a.start_worker(k);
        }
        a
    }

    pub fn set_key(&mut self, key: String) {
        self.key_set = true;
        self.start_worker(key);
    }

    fn start_worker(&mut self, key: String) {
        let (tx_in, rx_in) = channel::<ToWorker>();
        let (tx_out, rx_out) = channel::<FromWorker>();
        self.tx = Some(tx_in);
        self.rx = Some(rx_out);
        let model = self.model.clone();
        std::thread::spawn(move || worker(key, model, rx_in, tx_out));
    }

    pub fn model(&self) -> String {
        self.model.lock().map(|m| m.clone()).unwrap_or_else(|_| DEFAULT_MODEL.into())
    }

    /// Switch the chat model (persisted; applies from the next request).
    pub fn set_model(&mut self, m: &str) {
        if let Ok(mut cur) = self.model.lock() { *cur = m.to_string(); }
        let _ = save_config_field("assistant_model", m);
    }

    pub fn send_user(&mut self, text: String) {
        self.history.push(ChatLine {
            role: "user".into(),
            text: text.clone(),
        });
        self.busy = true;
        if let Some(tx) = &self.tx {
            let _ = tx.send(ToWorker::User(text));
        }
    }

    /// Show `shown` in the transcript but send `sent` (e.g. with a state
    /// header) to the model.
    pub fn send_user_display(&mut self, shown: String, sent: String) {
        self.history.push(ChatLine { role: "user".into(), text: shown });
        self.busy = true;
        if let Some(tx) = &self.tx {
            let _ = tx.send(ToWorker::User(sent));
        }
    }

    pub fn send_tool_result(&mut self, id: String, content: String) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ToWorker::ToolResult { id, content });
        }
    }

    pub fn poll(&mut self) -> Vec<FromWorker> {
        let mut out = Vec::new();
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(ev) => out.push(ev),
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        out
    }
}

fn system_prompt() -> String {
    "You are the built-in assistant of kabelsalat, a live audio/MIDI wiring app for Linux \
     (AUM-style routing, Reason-style rack flip, written in Rust). You help with setup, \
     routing, sound design, and troubleshooting (PipeWire, ALSA MIDI, latency, xruns).\n\n\
     The app is a graph of nodes — Keys, Surge XT synth, built-in DSP (delay/reverb/EQ/\
     filter/limiter/etc.), LV2 effects, system-audio capture, file player, Master — wired \
     by animated cables. Tab flips the rack front (perform) / back (patch). You have full \
     control through tools:\n\
     - Inspect: get_graph (nodes + cables with ids — call it before wiring), \
     get_status, list_plugins.\n\
     - Play: play_note, set_octave.\n\
     - Routing: set_rig_source (which source classes feed a rig: keyboard — on-screen      keys and laptop typing are ONE source — and hardware), set_rig_device (which \
     hardware keyboard), set_split (split a Keys node: it then has TWO MIDI outs, port 0 = \
     LOW below the point, port 1 = HIGH at/above; wire each to a rig with connect \
     from_port 0/1 — there is no automatic rig split), flip_rack.\n\
     - Sound: system_audio (mix YouTube/browser playback), play_file/stop_file/set_player \
     (backing tracks), set_bpm (tempo — ONE global tempo shared by every metronome and by the \
     tempo-synced features of hosted CLAP plugins (synced LFOs, sequencers, arps); \
     the user drags it on any metronome's face).\n\
     - Transport: start/stop is a property of each rack instrument that supports it \
     (metronome today; get_graph marks them transport=true with their playing state). \
     set_node_playing(node, playing) starts/stops ONE instrument; transport_play/\
     transport_stop are the master and start/stop ALL of them. The master is also the \
     song clock CLAP plugins follow: play runs it from beat 0, stop halts it. \
     set_count_in(enabled, bars) enables/configures a count-in bar of clicks before \
     the song clock starts (and the user can tap tempo with Shift+T or TAP on a metronome).\n\
     - Sessions: save_session/load_session/list_sessions.\n\
     - Patching: load_template (starter patches), add_node/connect/disconnect/\
     remove_node (build custom graphs; use get_graph for node/cable ids). Added dsp/lv2 nodes \
     sound once wired.\n\
     - Rigs (you operate on ALL of them, not only the one in view): get_rigs lists every \
     rig; add_rig creates a new playable rig (Keys -> Surge -> Master); delete_rig removes \
     one (ask the user first, then confirm=true); select_rig changes the view; add_node \
     takes rig=<index>; set_rig_mix sets a rig's vol/pan/mute/solo. Wire only nodes of the \
     same rig. Every rig sounds at once.\n\
     UI shortcuts the user can press: F5/F6/F7 load starter templates (Lead+Reverb, \
     Drums+Bass, Ambient Pad), F8 restores the demo, F9 opens the plugin browser, F2/F3 \
     embed the Surge editor.\n\n\
     WHERE TO WORK: every user message starts with a [context: …] line naming \
     the rig in view and its units. When the user does not name a rig, act on \
     the rig IN VIEW (load_sound/add_node default to it) — never on another rig, \
     never ask which rig unless the request is truly ambiguous. Use node ids \
     from the context or get_graph; do not create a new rig unless asked.\n\
     STYLE: answer in at most 2-3 short sentences (or a short list) — no \
     preamble, no recap. Name rigs and units EXACTLY as the app shows them \
     (e.g. Rig A, Surge B, Delay, Reverb, Master) so the UI can highlight them; \
     do not use markdown headings.\n\
     SOUNDS: a new rig or synth starts on Surge's init patch — when the user asks \
     for an instrument or character (flute, warm pad, 808 bass…), call \
     load_sound(rig, query) (or add_rig with sound=…); list_sounds(query) browses \
     the ~3000 installed Surge patches by category/name.\n\
     Be concise and practical. When something is broken, diagnose step by step starting \
     with get_status. Notes: loading a template or adding a plugin rebuilds the current \
     patch; only Surge XT is loadable as a CLAP instrument so far (other CLAP and LV2 \
     instruments are coming soon); system-audio capture of the output monitor can feed \
     back if kabelsalat plays to the same output."
        .into()
}

pub(crate) fn tools() -> Value {
    json!([
        {"type":"function","function":{
            "name":"get_graph",
            "description":"The current patch as JSON: nodes (id, kind, name, rig, playing — the per-instrument transport state of metronomes, split — a Keys node's split point or null) and edges/cables (id, from/to node+port, kind). ALWAYS call this first to find node and cable ids for connect/disconnect.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"get_status",
            "description":"Get current audio/MIDI/UI status of the app, including recent MIDI events",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"play_note",
            "description":"Play a note on the built-in synth (MIDI note number, duration in ms)",
            "parameters":{"type":"object","properties":{"note":{"type":"integer"},"ms":{"type":"integer"}},"required":["note"]}
        }},
        {"type":"function","function":{
            "name":"set_octave",
            "description":"Set the computer-keyboard (laptop typing) base octave (1-7)",
            "parameters":{"type":"object","properties":{"octave":{"type":"integer"}},"required":["octave"]}
        }},
        {"type":"function","function":{
            "name":"flip_rack",
            "description":"Flip the rack between front (perform) and back (patch cables)",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_rig_source",
            "description":"Select which input sources feed a rig (AUM per-rig input). node 1=Surge A, 4=Surge B. Toggle keyboard (the computer keyboard: on-screen keys and laptop typing are one source) and hardware (MIDI devices). Omitted flags default to true.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"keyboard":{"type":"boolean"},"hardware":{"type":"boolean"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"set_split",
            "description":"Split a Keys node's keyboard at a MIDI note (a property of the Keys node). Played notes below the point leave on its port 0 (LOW), notes at/above on port 1 (HIGH). point null/omitted = unsplit (one MIDI OUT; HIGH cables are removed). Routing is plain wiring: after splitting, connect from_port 1 of the Keys node to the rig that should play the high zone (existing cables stay on LOW). Example: {point: 60} splits at C4.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer","description":"Keys node id (get_graph); omit = the Keys node in view"},"point":{"type":"integer","description":"MIDI note 0-127 to split at; omit = unsplit"}}}
        }},
        {"type":"function","function":{
            "name":"save_session",
            "description":"Save the current patch, knob values, and Surge plugin state as a named session package (.ksalat) in ~/kabelsalat-sessions.",
            "parameters":{"type":"object","properties":{"name":{"type":"string","description":"Session name (no slashes)"}},"required":["name"]}
        }},
        {"type":"function","function":{
            "name":"load_session",
            "description":"Load a saved session by name, restoring knob values and Surge plugin state.",
            "parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}
        }},
        {"type":"function","function":{
            "name":"list_sessions",
            "description":"List saved session names in ~/kabelsalat-sessions.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"system_audio",
            "description":"Mix system audio (e.g. YouTube/browser playback) into the master. gain 0..1 (0 mutes, default). Captures the default output's monitor source. Warn the user about feedback if kabelsalat plays to the same output.",
            "parameters":{"type":"object","properties":{"gain":{"type":"number","description":"0..1 mix level; 0 mutes"}},"required":["gain"]}
        }},
        {"type":"function","function":{
            "name":"play_file",
            "description":"Play a backing-track audio file (WAV: 16/24-bit PCM or 32-bit float, e.g. a kabelsalat recording) mixed into the master. path is an absolute file path; gain 0..1 (default 0.8).",
            "parameters":{"type":"object","properties":{"path":{"type":"string"},"gain":{"type":"number"}},"required":["path"]}
        }},
        {"type":"function","function":{
            "name":"stop_file",
            "description":"Stop the backing-track file player.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_player",
            "description":"Set the file-player mix gain (0..1, 0 mutes) without changing what's playing.",
            "parameters":{"type":"object","properties":{"gain":{"type":"number"}},"required":["gain"]}
        }},
        {"type":"function","function":{
            "name":"set_rig_device",
            "description":"Select which hardware MIDI device feeds a rig (AUM: a different keyboard per rig). node 1=Surge A, 4=Surge B. device is a device index (see get_status / hw list); omit device (null) for any device.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"device":{"type":"integer"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"get_matrix",
            "description":"Read the hot-wire MIDI matrix: sources (keys = the computer keyboard, on-screen + laptop; hw_all; hw:<device index>) x instrument sinks, with per-cell routing state, MIDI cable gate, hardware device list and split state.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_matrix_cell",
            "description":"Hot-wire one MIDI matrix cell live: route a source to an instrument rig (or unroute). Turning on also plugs the MIDI cable if missing. source: keys (the computer keyboard) | hw_all | hw:<device index>. Hardware rows are a radio group per rig (one device, all, or off).",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"},"on":{"type":"boolean"}},"required":["node","source","on"]}
        }},
        {"type":"function","function":{
            "name":"set_matrix_rule",
            "description":"Shape ONE MIDI route live: channel filter (0-15, omit=omni), transpose (-24..24 semitones), key_lo/key_hi (split zone), vel_lo/vel_hi (velocity layer), enabled (mute). source: keys (the computer keyboard) | hw:<device index>. Releases held notes on the destination. Example: node 1, source keys, key_hi 59, transpose -12 = bass zone an octave down.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"},"channel":{"type":"integer"},"transpose":{"type":"integer"},"key_lo":{"type":"integer"},"key_hi":{"type":"integer"},"vel_lo":{"type":"integer"},"vel_hi":{"type":"integer"},"enabled":{"type":"boolean"}},"required":["node","source"]}
        }},
        {"type":"function","function":{
            "name":"list_params",
            "description":"List bindable plugin/DSP parameters (id, name) and rack face knobs on a node, optionally filtered by query words. Use the returned id/target with set_cc_map or learn_cc.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"query":{"type":"string"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"list_cc_maps",
            "description":"List active MIDI CC → parameter/knob bindings across matrix rules (plus any armed MIDI-learn target). Filter optionally by node or source ('keys' | 'hw:<d>').",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"}}}
        }},
        {"type":"function","function":{
            "name":"set_cc_map",
            "description":"Bind ONE MIDI CC (0..127) on source ('keys' | 'hw:<device index>') → dest node to a parameter id or rack knob (target: 'plugin' (default) | 'knob', or knob: 'LEVEL'|'MIX'|'VOLUME'). Omit param/knob to unbind that CC.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"},"cc":{"type":"integer"},"param":{"type":"integer"},"target":{"type":"string","enum":["plugin","knob"]},"knob":{"type":"string"}},"required":["node","source","cc"]}
        }},
        {"type":"function","function":{
            "name":"clear_cc_map",
            "description":"Clear one CC binding (when cc is given) or matching bindings (filtered by optional target/knob, or ALL when omitted) on source → node.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"},"cc":{"type":"integer"},"target":{"type":"string","enum":["plugin","knob"]},"knob":{"type":"string"}},"required":["node","source"]}
        }},
        {"type":"function","function":{
            "name":"learn_cc",
            "description":"Arm MIDI-learn for (node, param/target or knob) on source ('keys' = default, any hardware device, records sender | 'hw:<d>') so the next incoming hardware CC binds to it. Pass cancel=true to disarm.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"},"param":{"type":"integer"},"target":{"type":"string","enum":["plugin","knob"]},"knob":{"type":"string"},"cancel":{"type":"boolean"}}}
        }},
        {"type":"function","function":{
            "name":"set_rig_mix",
            "description":"Per-rig mixer: rig index (0-based), vol 0..1, pan -1..1, mute, solo (solo mutes other rigs). Omitted fields keep their value. Example: rig 1 mute true.",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"},"vol":{"type":"number"},"pan":{"type":"number"},"mute":{"type":"boolean"},"solo":{"type":"boolean"}},"required":["rig"]}
        }},
        {"type":"function","function":{
            "name":"list_plugins",
            "description":"List installed CLAP and LV2 plugins (name, id/uri, features) available to host.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"transport_play",
            "description":"Master transport play: starts EVERY transport-capable instrument (all metronomes) and runs the song clock hosted CLAP plugins follow, from beat 0.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"transport_stop",
            "description":"Master transport stop: stops EVERY transport-capable instrument and halts the song clock CLAP plugins follow.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"record_start",
            "description":"Start recording the final master output (what the user hears) to a new timestamped 32-bit-float WAV under ~/Music/kabelsalat/. Idempotent: a second start while recording is a no-op.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"record_stop",
            "description":"Stop recording: the writer drains the ring and finalizes the WAV header. Idempotent: a stop while idle is a no-op.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"record_status",
            "description":"Get recording status: recording (bool), path (the last/take WAV), seconds (elapsed), dropped_frames (ring overruns), finalized.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_record_sync",
            "description":"Toggle 'sync to transport': while on, master transport play starts a recording and stop stops it. The take begins at song beat 0 (after any count-in).",
            "parameters":{"type":"object","properties":{"on":{"type":"boolean"}}}
        }},
        {"type":"function","function":{
            "name":"set_node_playing",
            "description":"Start or stop ONE rack instrument that has its own transport (a metronome) without touching the others. node = node id from get_graph.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"playing":{"type":"boolean"}},"required":["node","playing"]}
        }},
        {"type":"function","function":{
            "name":"set_bpm",
            "description":"Set the global tempo in BPM (20..300), shared by every metronome (the same value the metronome faces show) and by tempo-synced CLAP plugin features.",
            "parameters":{"type":"object","properties":{"bpm":{"type":"number"}},"required":["bpm"]}
        }},
        {"type":"function","function":{
            "name":"set_count_in",
            "description":"Enable/disable master transport count-in or set its length in bars (1..=4). When enabled, master play plays N bars of metronome clicks before the song clock and plugins start at beat 0.",
            "parameters":{"type":"object","properties":{"enabled":{"type":"boolean"},"bars":{"type":"integer"}}}
        }},
        {"type":"function","function":{
            "name":"load_template",
            "description":"Load a starter template (rebuilds the patch). name: lead_reverb | drums_bass | ambient_pad | demo | multi_rig (two independent rigs, switch view with 1-9).",
            "parameters":{"type":"object","properties":{"name":{"type":"string","enum":["lead_reverb","drums_bass","ambient_pad","demo","multi_rig"]}},"required":["name"]}
        }},
        {"type":"function","function":{
            "name":"add_node",
            "description":"Add a node to the patch. kind: keys|clap|dsp|lv2|system_audio|master_out. dsp needs 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb|metronome); lv2 needs 'uri' (see list_plugins); clap takes optional plugin_id. rig: which rig it joins (index from get_rigs; default = the rig in view). Added dsp/lv2 nodes sound once wired with connect — only wire nodes of the SAME rig. Use get_graph to see node ids.",
            "parameters":{"type":"object","properties":{"kind":{"type":"string"},"dsp":{"type":"string"},"uri":{"type":"string"},"plugin_id":{"type":"string"},"name":{"type":"string"},"rig":{"type":"integer"}},"required":["kind"]}
        }},
        {"type":"function","function":{
            "name":"list_sounds",
            "description":"Search the installed Surge patches (factory + 3rd-party, ~3000) by words in category/name, e.g. 'flute', 'pad warm', 'bass'. Returns up to 25 'Category/Name' entries.",
            "parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
        }},
        {"type":"function","function":{
            "name":"load_sound",
            "description":"Load a Surge patch into a rig's synth so it really sounds like that instrument (a rig's Surge otherwise plays the init patch). query: words or 'Category/Name' from list_sounds; rig: index from get_rigs (default: the rig in view). Returns the patch that was loaded.",
            "parameters":{"type":"object","properties":{"query":{"type":"string"},"rig":{"type":"integer"}},"required":["query"]}
        }},
        {"type":"function","function":{
            "name":"get_rigs",
            "description":"List every rig (all of them, not just the one in view): index, name, MIDI device, units (node ids), mixer vol/pan/mute/solo, plus which rig is in view. Every rig sounds at once; get_graph nodes carry their rig index too.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"add_rig",
            "description":"Create a new rig (an independent rack: Keys -> Surge -> its own Master, wired and playable; a bare Keys -> Master when all 4 synth voices are taken). Existing rigs keep their patches, knobs and sounds. The view switches to the new rig. sound: optional patch query loaded into the new rig's Surge (e.g. 'flute'). Returns get_rigs.",
            "parameters":{"type":"object","properties":{"name":{"type":"string","description":"Optional rig name (default Rig A/B/C… or the plugin's name)"},"sound":{"type":"string","description":"Optional Surge patch query, e.g. 'flute'"},"plugin_id":{"type":"string","description":"Optional CLAP plugin id from list_plugins (e.g. 'org.yanes.native'): the rig is Keys -> that plugin -> Master instead of Surge"}}}
        }},
        {"type":"function","function":{
            "name":"delete_rig",
            "description":"Delete a rig and everything in it (units, cables, synth settings). DESTRUCTIVE: ask the user and only then call with confirm=true. The last rig cannot be deleted.",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"},"confirm":{"type":"boolean"}},"required":["rig","confirm"]}
        }},
        {"type":"function","function":{
            "name":"select_rig",
            "description":"Show a rig in the big rack view (view only — every rig keeps sounding).",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"}},"required":["rig"]}
        }},
        {"type":"function","function":{
            "name":"connect",
            "description":"Wire a cable between two nodes. kind: audio (default) | midi. Ports default to 0.",
            "parameters":{"type":"object","properties":{"from_node":{"type":"integer"},"to_node":{"type":"integer"},"from_port":{"type":"integer"},"to_port":{"type":"integer"},"kind":{"type":"string","enum":["audio","midi"]}},"required":["from_node","to_node"]}
        }},
        {"type":"function","function":{
            "name":"disconnect",
            "description":"Remove a cable by its id (see get_graph).",
            "parameters":{"type":"object","properties":{"id":{"type":"integer"}},"required":["id"]}
        }},
        {"type":"function","function":{
            "name":"remove_node",
            "description":"Remove a node by id (see get_graph); its cables are removed too. Use this to delete duplicate or unwanted instruments/effects.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"}},"required":["node"]}
        }}
    ])
}

fn worker(key: String, model: std::sync::Arc<std::sync::Mutex<String>>, rx: Receiver<ToWorker>, tx: Sender<FromWorker>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(FromWorker::Error(format!("http client: {e}")));
            return;
        }
    };
    // Which models can this key use? (once; the UI falls back to defaults)
    if let Ok(r) = client.get("https://api.deepseek.com/models").bearer_auth(&key).send() {
        if let Ok(v) = r.json::<Value>() {
            let ids: Vec<String> = v.get("data").and_then(|d| d.as_array())
                .map(|a| a.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
                .unwrap_or_default();
            if !ids.is_empty() { let _ = tx.send(FromWorker::Models(ids)); }
        }
    }
    let mut msgs: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt() })];
    loop {
        let user_text = match rx.recv() {
            Ok(ToWorker::User(t)) => t,
            _ => break,
        };
        msgs.push(json!({ "role": "user", "content": user_text }));
        loop {
            let body = json!({
                "model": model.lock().map(|m| m.clone()).unwrap_or_else(|_| DEFAULT_MODEL.into()),
                "messages": msgs,
                "tools": tools(),
                "tool_choice": "auto",
            });
            let resp = client
                .post("https://api.deepseek.com/chat/completions")
                .bearer_auth(&key)
                .json(&body)
                .send();
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(FromWorker::Error(format!("request failed: {e}")));
                    let _ = tx.send(FromWorker::Done);
                    break;
                }
            };
            let status = resp.status();
            let v: Value = match resp.json() {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(FromWorker::Error(format!("bad response ({status}): {e}")));
                    let _ = tx.send(FromWorker::Done);
                    break;
                }
            };
            if !status.is_success() {
                let m = v
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown API error");
                let _ = tx.send(FromWorker::Error(format!("DeepSeek API: {m}")));
                let _ = tx.send(FromWorker::Done);
                break;
            }
            let msg = match v.pointer("/choices/0/message") {
                Some(m) => m.clone(),
                None => {
                    let _ = tx.send(FromWorker::Error("empty response".into()));
                    let _ = tx.send(FromWorker::Done);
                    break;
                }
            };
            let calls = msg.get("tool_calls").and_then(|c| c.as_array()).cloned();
            if let Some(calls) = calls.filter(|c| !c.is_empty()) {
                msgs.push(msg.clone());
                let mut ok = true;
                for tc in calls {
                    let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let name = tc
                        .pointer("/function/name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args: Value = tc
                        .pointer("/function/arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(json!({}));
                    let _ = tx.send(FromWorker::ToolCall {
                        id: id.clone(),
                        name,
                        args,
                    });
                    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                        Ok(ToWorker::ToolResult { id: rid, content }) => {
                            msgs.push(json!({ "role": "tool", "tool_call_id": rid, "content": content }));
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    continue;
                }
                let _ = tx.send(FromWorker::Done);
                break;
            }
            let text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            msgs.push(json!({ "role": "assistant", "content": text }));
            let _ = tx.send(FromWorker::Text(text));
            let _ = tx.send(FromWorker::Done);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_in_persists_in_config_json_preserving_other_fields() {
        let dir = std::env::temp_dir().join(format!("ksalat-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, r#"{"assistant_model":"deepseek-chat"}"#).unwrap();

        assert_eq!(load_count_in_from(&p), (false, 1));
        save_count_in_to(&p, true, 2).unwrap();
        assert_eq!(load_count_in_from(&p), (true, 2));
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["assistant_model"], json!("deepseek-chat"));

        save_count_in_to(&p, false, 9).unwrap();
        assert_eq!(load_count_in_from(&p), (false, crate::transport::MAX_COUNT_IN_BARS));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
