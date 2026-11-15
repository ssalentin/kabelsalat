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
    let s = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("deepseek_api_key")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

pub fn save_key(key: &str) -> std::io::Result<()> {
    let p = config_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&json!({ "deepseek_api_key": key })).unwrap(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
    Text(String),
    ToolCall { id: String, name: String, args: Value },
    Done,
    Error(String),
}

pub struct Assistant {
    pub history: Vec<ChatLine>,
    pub busy: bool,
    pub key_set: bool,
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
        std::thread::spawn(move || worker(key, rx_in, tx_out));
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
     routing, and troubleshooting (PipeWire, ALSA MIDI, latency, xruns). You can inspect \
     the app state and play notes through tools; full graph rewiring arrives with the \
     control plane. Be concise and practical. When something is broken, diagnose step by \
     step, starting with get_status."
        .into()
}

fn tools() -> Value {
    json!([
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
            "description":"Set the laptop-keyboard base octave (1-7)",
            "parameters":{"type":"object","properties":{"octave":{"type":"integer"}},"required":["octave"]}
        }},
        {"type":"function","function":{
            "name":"flip_rack",
            "description":"Flip the rack between front (perform) and back (patch cables)",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_rig_source",
            "description":"Select which input sources feed a rig (AUM per-rig input). node 1=Surge A, 4=Surge B. Toggle keys (on-screen keyboard), laptop (laptop keyboard), hardware (MIDI devices). Omitted flags default to true.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"keys":{"type":"boolean"},"laptop":{"type":"boolean"},"hardware":{"type":"boolean"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"set_split",
            "description":"Split the keyboard across the two rigs at a MIDI note: rig A plays below, rig B at/above. Omit point (or null) to disable the split so both rigs play the full keyboard. Example: point 60 splits at C4.",
            "parameters":{"type":"object","properties":{"point":{"type":"integer","description":"MIDI note 0-127 to split at; omit to disable"}}}
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
            "description":"Play a backing-track audio file (16-bit WAV) mixed into the master. path is an absolute file path; gain 0..1 (default 0.8).",
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
        }}
    ])
}

fn worker(key: String, rx: Receiver<ToWorker>, tx: Sender<FromWorker>) {
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
    let mut msgs: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt() })];
    loop {
        let user_text = match rx.recv() {
            Ok(ToWorker::User(t)) => t,
            _ => break,
        };
        msgs.push(json!({ "role": "user", "content": user_text }));
        loop {
            let body = json!({
                "model": "deepseek-chat",
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
