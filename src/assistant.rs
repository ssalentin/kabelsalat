//! Built-in assistant: the agent loop behind the chat, bring-your-own-key.
//!
//! The assistant talks to the same control surface the UI uses (the MCP
//! server exposes the identical toolset). Which model answers is a route
//! (`llm::Route`); keys are stored locally in ~/.config/kabelsalat/config.json
//! (mode 0600) and only ever sent to the endpoint they belong to.

use std::sync::mpsc::{channel, Receiver, Sender};

use serde_json::{json, Value};

use crate::control::{DspKind, Edge, Graph, NodeId, NodeKind, PortKind};
use crate::llm::{self, Endpoint, LlmError, Role, Route, Transport};
use crate::vision::Image;

pub fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("kabelsalat")
        .join("config.json")
}

pub fn save_key(key: &str) -> std::io::Result<()> {
    save_config_field("deepseek_api_key", key)
}

/// A string field of config.json (None in tests — they never read the
/// user's config, rule 37).
pub fn load_config_str(field: &str) -> Option<String> {
    if cfg!(test) { return None; }
    std::fs::read_to_string(config_path()).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get(field).and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.is_empty())
}

/// Any JSON field of config.json (None in tests, rule 37).
pub fn load_config_value(field: &str) -> Option<Value> {
    if cfg!(test) { return None; }
    std::fs::read_to_string(config_path()).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|mut v| v.get_mut(field).map(Value::take))
}

/// Merge one JSON field into config.json (no-op in tests, rule 37).
pub fn save_config_value(field: &str, value: Value) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); }
    save_config_values_to(&config_path(), &[(field, value)])
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

/// Remove one field from a config JSON file (mode 0600), preserving other fields.
pub fn remove_config_field_from(path: &std::path::Path, field: &str) -> std::io::Result<()> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Ok(()); };
    let mut v: Value = serde_json::from_str(&raw).ok().filter(|v: &Value| v.is_object()).unwrap_or_else(|| json!({}));
    if let Some(obj) = v.as_object_mut() {
        if obj.remove(field).is_none() {
            return Ok(());
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&v).unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Remove one field from config.json (test-safe no-op under cfg(test), rule 37).
pub fn remove_config_field(field: &str) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); }
    remove_config_field_from(&config_path(), field)
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

pub fn load_reduce_motion_from(path: &std::path::Path) -> bool {
    let v: Option<Value> = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok());
    v.as_ref().and_then(|v| v.get("ui_reduce_motion")).and_then(Value::as_bool).unwrap_or(false)
}

pub fn save_reduce_motion_to(path: &std::path::Path, reduce: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[("ui_reduce_motion", json!(reduce))])
}

/// Persisted `ui_reduce_motion` flag in config.json (default false, test-safe no-op under cfg(test)).
pub fn load_reduce_motion() -> bool {
    if cfg!(test) { return false; }
    load_reduce_motion_from(&config_path())
}

pub fn save_reduce_motion(reduce: bool) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); }
    save_reduce_motion_to(&config_path(), reduce)
}

/// Persisted `session_autosave` flag in config.json (default on; off in
/// tests, which never write the user's sessions, rule 37).
pub fn load_session_autosave() -> bool {
    if cfg!(test) { return false; }
    std::fs::read_to_string(config_path()).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("session_autosave").and_then(Value::as_bool))
        .unwrap_or(true)
}

pub fn save_session_autosave(on: bool) -> std::io::Result<()> {
    if cfg!(test) { return Ok(()); }
    save_config_values_to(&config_path(), &[("session_autosave", json!(on))])
}

#[derive(Clone)]
pub struct ChatLine {
    pub role: String,
    pub text: String,
}

/// A tool call the model made. `args` passed the tool's schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

/// What the user sent: their words, and an image when they attached one.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMessage {
    pub text: String,
    pub image: Option<Image>,
}

/// What a tool call gave back: its result line, and a picture when the tool
/// shows something (look_at_rack).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub image: Option<Image>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self { text, image: None }
    }
}

pub enum ToWorker {
    User(UserMessage),
    /// One result per call of the last `FromWorker::ToolCalls`, in call order.
    ToolResults(Vec<ToolOutput>),
}

/// Live phase of an in-flight assistant turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunPhase {
    Deciding,
    Thinking { step: usize, model: String, seeing: bool },
}

impl RunPhase {
    pub fn route_name(model: &str) -> &str {
        let low = model.to_ascii_lowercase();
        if low.contains("jev-router") {
            "Jev router"
        } else if low.contains("jev") {
            "Jev"
        } else if low.contains("deepseek") {
            "DeepSeek"
        } else if low.contains("gemini") {
            "Gemini"
        } else if low.contains("claude") {
            "Claude"
        } else {
            model.rsplit('/').next().unwrap_or(model)
        }
    }

    pub fn label(&self) -> String {
        match self {
            RunPhase::Deciding => "Jev · deciding".to_string(),
            RunPhase::Thinking { step, model, seeing } => {
                let route = Self::route_name(model);
                if *seeing && *step <= 1 {
                    format!("{route} · looking at the image")
                } else {
                    format!("{route} · step {step}")
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum FromWorker {
    /// Chat models the DeepSeek key can use (GET /models), fetched once.
    Models(Vec<String>),
    Phase(RunPhase),
    Text(String),
    /// Every valid tool call of one model step, in call order. The host runs
    /// them and answers with one `ToWorker::ToolResults`.
    ToolCalls(Vec<ToolCall>),
    Step(StepStat),
    Done,
    Error(String),
}

/// One model round trip of a turn.
#[derive(Clone, Debug, PartialEq)]
pub struct StepStat {
    /// Wall time, retries included.
    pub ms: u64,
    /// The model that answered (a router reports the one it picked).
    pub model: String,
    pub attempts: u32,
    /// Tool calls of this step the worker sent back as errors (unknown
    /// tool, arguments that do not fit the schema).
    pub rejected: usize,
}

/// What the current (or last) turn cost: tallied as worker events pass
/// through `Assistant::poll`, shown in the Agent Activity panel and read by
/// agent-eval.
#[derive(Clone, Debug, Default)]
pub struct TurnStats {
    pub started: Option<std::time::Instant>,
    /// Wall time from the user's message to the worker's Done.
    pub wall: Option<std::time::Duration>,
    pub steps: Vec<StepStat>,
    /// Tools that ran, in call order.
    pub tools: Vec<String>,
    pub errors: Vec<String>,
}

impl TurnStats {
    /// One line for the activity panel, e.g. "3 steps · 4.2 s · claude-opus-5.5".
    /// None before the first model answer.
    pub fn summary(&self) -> Option<String> {
        let last = self.steps.last()?;
        let n = self.steps.len();
        let secs = self.steps.iter().map(|s| s.ms).sum::<u64>() as f64 / 1000.0;
        let model = last.model.rsplit('/').next().unwrap_or(&last.model);
        let retries: u32 = self.steps.iter().map(|s| s.attempts - 1).sum();
        let retried = if retries > 0 { format!(" · {retries} retr{}", if retries == 1 { "y" } else { "ies" }) } else { String::new() };
        Some(format!("{n} step{} · {secs:.1} s · {model}{retried}", if n == 1 { "" } else { "s" }))
    }
}

type SharedRoute = std::sync::Arc<std::sync::Mutex<Route>>;
type SharedOptRoute = std::sync::Arc<std::sync::Mutex<Option<Route>>>;

/// Where each kind of request goes. Every role is read on each request so
/// changes in Settings or `/model` take effect without restarting a chat.
#[derive(Clone)]
struct Routes {
    act: SharedRoute,
    decide: SharedOptRoute,
    perceive: SharedRoute,
}

pub struct Assistant {
    pub history: Vec<ChatLine>,
    pub busy: bool,
    pub key_set: bool,
    pub phase: Option<RunPhase>,
    routes: Routes,
    keys_present: llm::Keys,
    decide_enabled: bool,
    decide_model: String,
    act_overridden: bool,
    /// Models offered in the picker.
    pub models: Vec<String>,
    pub turn: TurnStats,
    tx: Option<Sender<ToWorker>>,
    rx: Option<Receiver<FromWorker>>,
}

impl Assistant {
    pub fn new() -> Self {
        Self::on_route(Route::configured(Role::Act), true)
    }

    /// An assistant on `model` for this process only: nothing is written to
    /// the user's config (agent-eval compares models without changing the
    /// one the app uses). `decide` false = no decisions model in front.
    pub fn with_model(model: &str, decide: bool) -> Self {
        Self::on_route(Route::for_model(model), decide)
    }

    fn on_route(route: Route, decide: bool) -> Self {
        let keys_present = llm::Keys {
            openrouter: Endpoint::OpenRouter.key().is_some(),
            deepseek: Endpoint::DeepSeek.key().is_some(),
        };
        let decide_cfg = load_config_str(Role::Decide.model_field());
        let decide_enabled = decide && !matches!(decide_cfg.as_deref(), Some("off"));
        let decide_route = if decide_enabled {
            Route::configured(Role::Decide)
        } else {
            Route::for_model(llm::JEV)
        };
        let decide_model = decide_route.model.clone();
        let decide_active = Some(decide_route)
            .filter(|r| decide_enabled && r.endpoint.spec().decisions_url.is_some() && keys_present.has(r.endpoint));
        let act_overridden = load_config_str(Role::Act.model_field()).is_some();
        let models = picker_models(keys_present.openrouter, keys_present.deepseek, &route.model);
        let key_set = keys_present.has(route.endpoint);
        let mut a = Self {
            history: Vec::new(),
            busy: false,
            key_set,
            phase: None,
            routes: Routes {
                act: std::sync::Arc::new(std::sync::Mutex::new(route)),
                decide: std::sync::Arc::new(std::sync::Mutex::new(decide_active)),
                perceive: std::sync::Arc::new(std::sync::Mutex::new(Route::configured(Role::Perceive))),
            },
            keys_present,
            decide_enabled,
            decide_model,
            act_overridden,
            models,
            turn: TurnStats::default(),
            tx: None,
            rx: None,
        };
        if a.key_set {
            a.start_worker();
        }
        a
    }

    /// The user just stored a DeepSeek key. A route that still has no key
    /// (an OpenRouter model without an OpenRouter key) falls back to it.
    pub fn key_saved(&mut self) {
        self.keys_present.deepseek = true;
        if let Ok(mut route) = self.routes.act.lock() {
            if !self.keys_present.has(route.endpoint) {
                *route = Route::for_model(llm::DEEPSEEK_CHAT);
            }
        }
        self.restart_worker();
    }

    /// Save an API key for `endpoint` (test-safe no-op on disk under cfg(test))
    /// and restart the worker so it takes effect immediately.
    pub fn save_api_key(&mut self, endpoint: Endpoint, key: &str) -> std::io::Result<()> {
        save_config_field(endpoint.spec().key_field, key)?;
        self.keys_present.set(endpoint, true);
        if endpoint == Endpoint::DeepSeek {
            if let Ok(mut route) = self.routes.act.lock() {
                if !self.keys_present.has(route.endpoint) {
                    *route = Route::for_model(llm::DEEPSEEK_CHAT);
                }
            }
        }
        self.restart_worker();
        Ok(())
    }

    /// Remove an API key for `endpoint` (test-safe no-op on disk under cfg(test))
    /// and restart/stop the worker accordingly.
    pub fn remove_api_key(&mut self, endpoint: Endpoint) -> std::io::Result<()> {
        remove_config_field(endpoint.spec().key_field)?;
        self.keys_present.set(endpoint, false);
        self.restart_worker();
        Ok(())
    }

    /// Drop any existing worker channels and start a fresh worker if a key is
    /// available (never spawns network threads under cfg(test), rule 37).
    pub fn restart_worker(&mut self) {
        self.tx = None;
        self.rx = None;
        self.busy = false;
        self.phase = None;

        if !self.act_overridden {
            if let Ok(mut act) = self.routes.act.lock() {
                *act = Route::resolve(Role::Act, None, self.keys_present);
            }
        }
        let act_route = self.route_for(Role::Act);
        self.models = picker_models(self.keys_present.openrouter, self.keys_present.deepseek, &act_route.model);
        self.sync_decide_route();
        self.key_set = self.keys_present.has(act_route.endpoint)
            || self.keys_present.deepseek
            || self.keys_present.openrouter;
        if self.key_set {
            self.start_worker();
        }
    }

    fn sync_decide_route(&mut self) {
        let next = if self.decide_enabled {
            let r = Route::for_model(&self.decide_model);
            let ok_endpoint = r.endpoint.spec().decisions_url.is_some();
            let ok_key = cfg!(test) || self.keys_present.has(r.endpoint);
            (ok_endpoint && ok_key).then_some(r)
        } else {
            None
        };
        if let Ok(mut d) = self.routes.decide.lock() {
            *d = next;
        }
    }

    fn start_worker(&mut self) {
        if cfg!(test) {
            return;
        }
        let (tx_in, rx_in) = channel::<ToWorker>();
        let (tx_out, rx_out) = channel::<FromWorker>();
        self.tx = Some(tx_in);
        self.rx = Some(rx_out);
        let routes = self.routes.clone();
        // Its own thread: a slow model list must not delay the first request.
        let list_tx = tx_out.clone();
        std::thread::spawn(move || {
            if let Some(ids) = fetch_deepseek_models() { let _ = list_tx.send(FromWorker::Models(ids)); }
        });
        std::thread::spawn(move || match llm::HttpTransport::new() {
            Ok(transport) => worker(llm::Client::new(transport), routes, Endpoint::key, rx_in, tx_out),
            Err(e) => { let _ = tx_out.send(FromWorker::Error(e)); }
        });
    }

    pub fn model(&self) -> String {
        self.routes.act.lock().map(|r| r.model.clone()).unwrap_or_default()
    }

    pub fn keys(&self) -> llm::Keys {
        self.keys_present
    }

    pub fn decide_enabled(&self) -> bool {
        self.decide_enabled
    }

    /// Current route for `role` (`Decide` returns its configured model even
    /// while disabled so Settings can display and edit it).
    pub fn route_for(&self, role: Role) -> Route {
        match role {
            Role::Decide => self
                .routes
                .decide
                .lock()
                .ok()
                .and_then(|d| d.clone())
                .unwrap_or_else(|| Route::for_model(&self.decide_model)),
            Role::Act => self
                .routes
                .act
                .lock()
                .map(|r| r.clone())
                .unwrap_or_else(|_| Route::for_model(llm::DEEPSEEK_CHAT)),
            Role::Perceive => self
                .routes
                .perceive
                .lock()
                .map(|r| r.clone())
                .unwrap_or_else(|_| Route::for_model(llm::PERCEIVE_MODEL)),
        }
    }

    /// Whether Jev's decision front answers requests before the chat model.
    pub fn decides_first(&self) -> bool {
        self.routes.decide.lock().is_ok_and(|d| d.is_some())
    }

    pub fn set_decide_enabled(&mut self, on: bool) {
        self.decide_enabled = on;
        let persisted = if on { self.decide_model.as_str() } else { "off" };
        let _ = save_config_field(Role::Decide.model_field(), persisted);
        self.sync_decide_route();
    }

    /// Switch any role's model (persisted; applies from the next request).
    pub fn set_role_model(&mut self, role: Role, m: &str) -> String {
        let m = m.trim();
        if m.is_empty() {
            return "model id is empty".to_string();
        }
        match role {
            Role::Act => self.set_model(m),
            Role::Decide => {
                if m.eq_ignore_ascii_case("off") {
                    self.set_decide_enabled(false);
                    return "decisions model turned off".to_string();
                }
                self.decide_model = m.to_string();
                self.decide_enabled = true;
                let _ = save_config_field(Role::Decide.model_field(), m);
                self.sync_decide_route();
                let route = Route::for_model(m);
                let spec = route.endpoint.spec();
                if route.endpoint.spec().decisions_url.is_none() {
                    format!("decisions model set to {m}, but {} has no decisions API", spec.label)
                } else if !self.keys_present.has(route.endpoint) && !cfg!(test) {
                    format!("decisions model switched to {m}, but config.json has no {} key yet ({})", spec.label, spec.key_field)
                } else {
                    format!("decisions model switched to {m}")
                }
            }
            Role::Perceive => {
                let route = Route::for_model(m);
                let spec = route.endpoint.spec();
                let missing_key = !self.keys_present.has(route.endpoint) && !cfg!(test);
                if let Ok(mut cur) = self.routes.perceive.lock() {
                    *cur = route;
                }
                let _ = save_config_field(Role::Perceive.model_field(), m);
                if missing_key {
                    format!("vision model switched to {m}, but config.json has no {} key yet ({})", spec.label, spec.key_field)
                } else {
                    format!("vision model switched to {m}")
                }
            }
        }
    }

    /// Switch the acting model (persisted; applies from the next request).
    /// Returns the line for the chat.
    pub fn set_model(&mut self, m: &str) -> String {
        let route = Route::for_model(m);
        let spec = route.endpoint.spec();
        let missing_key = !self.keys_present.has(route.endpoint) && !cfg!(test);
        self.act_overridden = true;
        if let Ok(mut cur) = self.routes.act.lock() { *cur = route; }
        let _ = save_config_field(Role::Act.model_field(), m);
        if !self.models.iter().any(|x| x == m) { self.models.push(m.to_string()); }
        if missing_key {
            format!("model switched to {m}, but config.json has no {} key yet ({})", spec.label, spec.key_field)
        } else {
            format!("model switched to {m} (the conversation continues)")
        }
    }

    pub fn send_user(&mut self, text: String) {
        self.send_user_display(text.clone(), text);
    }

    /// Show `shown` in the transcript but send `sent` (e.g. with a state
    /// header) to the model.
    pub fn send_user_display(&mut self, shown: String, sent: String) {
        self.send_user_message(shown, UserMessage { text: sent, image: None });
    }

    /// The same for a message that may carry an image.
    pub fn send_user_message(&mut self, shown: String, sent: UserMessage) {
        self.history.push(ChatLine { role: "user".into(), text: shown });
        self.busy = true;
        self.phase = Some(if sent.image.is_some() {
            RunPhase::Thinking { step: 1, model: self.route_for(Role::Perceive).model, seeing: true }
        } else if self.decides_first() {
            RunPhase::Deciding
        } else {
            RunPhase::Thinking { step: 1, model: self.model(), seeing: false }
        });
        self.turn = TurnStats { started: Some(std::time::Instant::now()), ..TurnStats::default() };
        if let Some(tx) = &self.tx {
            let _ = tx.send(ToWorker::User(sent));
        }
    }

    pub fn phase_label(&self) -> String {
        self.phase.as_ref().map_or_else(|| "Assistant · working".to_string(), RunPhase::label)
    }

    /// Answer a batch from `poll`: one result per call, in call order.
    pub fn send_tool_results(&mut self, results: Vec<ToolOutput>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ToWorker::ToolResults(results));
        }
    }

    /// Take in what the worker sent (replies, errors, stats land in
    /// `history` / `turn`) and hand back the tool-call batches the host has
    /// to run, one per model step.
    pub fn poll(&mut self) -> Vec<Vec<ToolCall>> {
        let mut batches = Vec::new();
        while let Some(ev) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match ev {
                FromWorker::Phase(p) => self.phase = Some(p),
                FromWorker::Text(t) => self.history.push(ChatLine { role: "assistant".into(), text: t }),
                FromWorker::ToolCalls(batch) => {
                    self.turn.tools.extend(batch.iter().map(|c| c.name.clone()));
                    batches.push(batch);
                }
                FromWorker::Step(s) => self.turn.steps.push(s),
                FromWorker::Models(list) => {
                    for m in list.into_iter().filter(|m| is_chat_model(m)) {
                        if !self.models.contains(&m) { self.models.push(m); }
                    }
                }
                FromWorker::Done => {
                    self.busy = false;
                    self.phase = None;
                    self.turn.wall = self.turn.started.map(|t| t.elapsed());
                }
                FromWorker::Error(e) => {
                    self.busy = false;
                    self.phase = None;
                    self.turn.errors.push(e.clone());
                    self.history.push(ChatLine { role: "error".into(), text: e });
                }
            }
        }
        batches
    }
}

#[cfg(test)]
impl Assistant {
    /// An assistant whose worker is the test: returns the worker's ends of
    /// both channels.
    pub fn with_test_worker() -> (Self, Sender<FromWorker>, Receiver<ToWorker>) {
        let (tx_in, rx_in) = channel::<ToWorker>();
        let (tx_out, rx_out) = channel::<FromWorker>();
        let mut a = Self::with_model(llm::DEEPSEEK_CHAT, false);
        a.key_set = true;
        a.tx = Some(tx_in);
        a.rx = Some(rx_out);
        (a, tx_out, rx_in)
    }
}

/// Chat-capable ids only (embedding models etc. stay out of the picker).
fn is_chat_model(id: &str) -> bool {
    ["chat", "reason", "flash", "v3", "v4"].iter().any(|k| id.contains(k))
}

fn system_prompt() -> String {
    "You are the built-in assistant of kabelsalat, a live audio/MIDI wiring app for Linux \
     (AUM-style routing, Reason-style rack flip, written in Rust). You help with setup, \
     routing, sound design, and troubleshooting (PipeWire, ALSA MIDI, latency, xruns).\n\n\
     The app is a graph of nodes — Keys, Surge XT synth, built-in DSP (delay/reverb/EQ/\
     filter/limiter/etc.), LV2 effects, an Audio In unit (live mic/guitar/interface with \
     a tuner), system-audio capture, file player, Master — wired \
     by animated cables. Tab flips the rack front (perform) / back (patch). You have full \
     control through tools:\n\
     - Inspect: get_graph (nodes + cables with ids — call it before wiring), \
     get_status, list_plugins, measure_mix (loudness/true-peak/band metering — \
     verify your own mix moves, e.g. is a note too loud, is the bass mono-safe).\n\
     - See: look_at_rack gives you a screenshot of the app window as the user sees it \
     right now (layout, what is selected, how something looks). The user can also \
     attach an image to a message; an image is visible only during its own turn.\n\
     - Play: play_note, set_octave.\n\
     - Routing: set_rig_source (which source classes feed a rig: keyboard — on-screen      keys and laptop typing are ONE source — and hardware), set_rig_device (which \
     hardware keyboard), set_split (split a Keys node: it then has TWO MIDI outs, port 0 = \
     LOW below the point, port 1 = HIGH at/above; wire each to a rig with connect \
     from_port 0/1 — there is no automatic rig split; low_octave/high_octave shift a zone by \
     octaves. The keys below a split are low already and many bass sounds are pitched down: \
     when you put a bass on the LOW zone, set low_octave 1 unless the user asked for a deep \
     bass, and tell the user; if they say it is too deep or too high, step it), flip_rack.\n\
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
     one; select_rig changes the view; add_node \
     takes rig=<index>; set_rig_mix sets a rig's vol/pan/mute/solo; \
     save_rig/load_rig/list_rig_presets save and load one rig preset independently of sessions. \
     Wire only nodes of the same rig. Every rig sounds at once. A tool's rig argument is \
     the 0-based index: the user's \"rig 1\" is rig=0, \"rig 2\" is rig=1.\n\
     - Destructive tools (delete_rig, install_instrument): call them WITHOUT confirm first; \
     the chat then shows Confirm / Cancel buttons. Ask the user in one short sentence and \
     wait — their click runs or cancels it.\n\
     UI shortcuts the user can press: F5/F6/F7 load starter templates (Lead+Reverb, \
     Drums+Bass, Ambient Pad), F8 restores the demo, F9 opens the plugin browser, F2/F3 \
     embed the Surge editor.\n\n\
     WORK IN FEW STEPS: every round trip costs the user seconds. Call several \
     tools in ONE step when they do not need each other's results (e.g. every \
     connect and disconnect of a rewiring). The [context: …] line already gives \
     the node ids of the units in view, and add_node/connect report the ids they \
     created — call get_graph only when you need cable ids or other rigs' nodes. \
     A tool result that starts with \"error:\" or says \"NOT applied\" means \
     nothing changed: correct the call, do not report success.\n\
     WHERE TO WORK: every user message starts with a [context: …] line naming \
     the rig in view and its units. When the user does not name a rig, act on \
     the rig IN VIEW (load_sound/add_node default to it) — never on another rig, \
     never ask which rig unless the request is truly ambiguous. Use node ids \
     from the context or get_graph; do not create a new rig unless asked.\n\
     STYLE: answer in at most 2-3 short sentences (or a short list) — no \
     preamble, no recap. Name rigs and units EXACTLY as the app shows them \
     (e.g. Rig A, Surge B, Delay, Reverb, Master) so the UI can highlight them; \
     do not use markdown headings.\n\
     SOUNDS: every instrument has its OWN sound library, and load_sound / list_sounds \
     work in the library of the rig's instrument: Surge has ~3000 patches (a new Surge \
     plays its init patch), Dexed its DX7 cartridge voices, setBfree its organ \
     registrations (MIDI programmes), Vitalium its LV2 presets, YANES its chip presets. \
     When the user asks for an instrument or character (flute, warm pad, 808 bass, jazz \
     organ…), call load_sound(query, rig) (or add_rig with sound=…); list_sounds(query, \
     rig) browses that library and names the instrument and library it searched. A sound \
     of one instrument cannot be loaded into another: if a library has nothing that fits, \
     say so and offer what it has. A rig with several different instruments needs \
     node=<the instrument's node id>.\n\
     Be concise and practical. When something is broken, diagnose step by step starting \
     with get_status. Notes: loading a template or adding a plugin rebuilds the current \
     patch; system-audio capture of the output monitor can feed \
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
            "name":"look_at_rack",
            "description":"See the app: a screenshot of the whole kabelsalat window as the user sees it right now (rack side in view, units, cables, mixer, open panels). Use it for questions about how something looks, what is selected or where something is; get_graph is the faster source for nodes and cables.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"measure_mix",
            "description":"Read the master-bus loudness/profile meter (Bravo measure loop): momentary (400 ms) and short-term (3 s) LUFS, gated integrated LUFS since the last reset, true peak (dBTP), low-end (< 120 Hz) phase correlation + a mono-compat flag, and a 4-band energy summary (< 120, 120-500, 500-4000, > 4000 Hz) in dB. Read-only; pass reset=true to restart the integrated-loudness measurement now (this resets the integration, not the signal).",
            "parameters":{"type":"object","properties":{"reset":{"type":"boolean","description":"Restart the integrated-loudness gate from now"}}}
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
            "description":"Split a Keys node's keyboard at a MIDI note (a property of the Keys node) and set each zone's octave. Played notes below the point leave on its port 0 (LOW), notes at/above on port 1 (HIGH). Routing is plain wiring: after splitting, connect from_port 1 of the Keys node to the rig that should play the high zone (existing cables stay on LOW). low_octave/high_octave shift what a zone plays by whole octaves (-3..3); the keys that pick the zone stay the same. A call with only octaves keeps the current split; a call with neither point nor octaves unsplits (one MIDI OUT; HIGH cables are removed, octaves reset). Examples: {point: 60} splits at C4; {low_octave: 1} raises the low zone an octave.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer","description":"Keys node id (get_graph); omit = the Keys node in view"},"point":{"type":"integer","description":"MIDI note 0-127 to split at; omit with no octaves = unsplit"},"low_octave":{"type":"integer","description":"octave shift of the LOW zone, -3..3"},"high_octave":{"type":"integer","description":"octave shift of the HIGH zone, -3..3"}}}
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
            "description":"List a node's parameters. main_controls comes first: the few parameters the instrument's designer marked as most important (page, name, id, current value and text, and the face knob that shows it). Prefer them when the user asks to shape or map a sound, and pass a main control's name as knob=<name> to set_cc_map or learn_cc. params is the full list (id, name), filtered by query words: use it only when no main control fits. knobs are the rack face knobs.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"query":{"type":"string"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"list_cc_maps",
            "description":"List active MIDI CC → parameter/knob bindings across matrix rules (plus any armed MIDI-learn target). Filter optionally by node or source ('keys' | 'hw:<d>').",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"source":{"type":"string"}}}
        }},
        {"type":"function","function":{
            "name":"set_cc_map",
            "description":"Bind ONE MIDI CC (0..127) on source ('keys' | 'hw:<device index>') → dest node to a parameter id or rack knob (target: 'plugin' (default) | 'knob', or knob: 'LEVEL'|'MIX'|'VOLUME' or a main control's name from list_params). Omit param/knob to unbind that CC.",
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
            "name":"list_instruments",
            "description":"List the built-in instrument catalog: each entry's id, name, format (CLAP/LV2), description, about text, licence, homepage, source code URL, install route (distro package, build from source, or prebuilt GitHub release), whether it is currently installed, and whether an install is running. Use install_instrument to install one.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"install_instrument",
            "description":"Install a catalog instrument (from list_instruments) onto this system: a distro package (via sudo pacman), a build from GitHub source, or a checksum-verified prebuilt GitHub release, into ~/.clap / ~/.lv2. DESTRUCTIVE (it changes the user's system): requires confirm=true after asking the user. Runs in the background with a live log in the Plugins view and rescans plugins when done.",
            "parameters":{"type":"object","properties":{"id":{"type":"string","description":"Catalog instrument id from list_instruments (e.g. 'setbfree', 'surge-xt', 'odin2')"},"confirm":{"type":"boolean","description":"Must be true after asking the user"}},"required":["id"]}
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
            "description":"Add a node to the patch. kind: keys|clap|dsp|lv2|audio_in|system_audio|master_out. dsp needs 'dsp' (gain|pan|eq3|filter|limiter|saturation|delay|reverb|metronome); lv2 needs 'uri' (see list_plugins); clap takes optional plugin_id; audio_in takes optional device (PipeWire source name from list_audio_devices; omit = default input) and channels (in_1|in_2|stereo). rig: which rig it joins (index from get_rigs; default = the rig in view). An instrument (clap, or an lv2 instrument such as setBfree) is wired for you like a standard rig's synth: the rig's Keys -> it (MIDI) and it -> the rig's Master L and R; the result names those cables, so do not connect them again. Other dsp/lv2 nodes sound once wired with connect — only wire nodes of the SAME rig. Use get_graph to see node ids.",
            "parameters":{"type":"object","properties":{"kind":{"type":"string"},"dsp":{"type":"string"},"uri":{"type":"string"},"plugin_id":{"type":"string"},"device":{"type":"string"},"channels":{"type":"string"},"name":{"type":"string"},"rig":{"type":"integer"}},"required":["kind"]}
        }},
        {"type":"function","function":{
            "name":"list_audio_outputs",
            "description":"List the sound cards kabelsalat can play to (PipeWire sinks: name to pass, readable label, which is the system default), the one it plays to now, and the user's choice (null = follow the system default).",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_audio_output",
            "description":"Choose the sound card kabelsalat plays to (e.g. the MOTU M2 instead of the laptop speakers). output: a name from list_audio_outputs; null or empty = follow the system default. Moves the live stream without a dropout and is remembered across restarts.",
            "parameters":{"type":"object","properties":{"output":{"type":["string","null"]}}}
        }},
        {"type":"function","function":{
            "name":"list_app_streams",
            "description":"List other apps that are playing audio right now (YouTube in the browser, Spotify, …): index, app, title, volume (1.0 = 100 %), muted. These are the strips in the mixer's System audio section.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_app_stream",
            "description":"Set another app's playback volume and/or mute (e.g. turn YouTube down under the live rigs). index from list_app_streams; volume 0..1.5 (1.0 = 100 %); muted true/false.",
            "parameters":{"type":"object","properties":{"index":{"type":"integer"},"volume":{"type":"number"},"muted":{"type":"boolean"}},"required":["index"]}
        }},
        {"type":"function","function":{
            "name":"list_audio_devices",
            "description":"List live audio input devices (mics, guitars, interfaces) for Audio In units: readable name, PipeWire source name (the device to pass), channel count, and which is default.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"set_audio_in",
            "description":"Configure a live Audio In rack unit (mic/guitar/interface) so the user can sing or play through it. node: the AudioIn node id (get_graph). device: a PipeWire source name from list_audio_devices (null/empty = default input). channels: in_1 (left/mic), in_2 (right/guitar), or stereo. gain: 0..1 input gain. monitor: true wires the input straight to the master for low-latency monitoring, false relies on its OUT jack. Wire the OUT jack to effects/master with connect to shape it.",
            "parameters":{"type":"object","properties":{"node":{"type":"integer"},"device":{"type":"string"},"channels":{"type":"string"},"gain":{"type":"number"},"monitor":{"type":"boolean"}},"required":["node"]}
        }},
        {"type":"function","function":{
            "name":"list_sounds",
            "description":"Search the sound library of ONE instrument by words in its sounds' names, e.g. 'flute', 'pad warm', 'bass', 'jazz'. Each instrument has its own library: Surge patches (~3000, 'Category/Name'), Dexed DX7 cartridge voices, an LV2 instrument's presets and MIDI programmes (setBfree's organ registrations, Vitalium's presets), or a plugin's own preset list (YANES). rig: index of the rig whose instrument is searched (default: the rig in view); node: the instrument's node id, needed when a rig holds several different instruments. Returns up to 25 sounds with the instrument, the library and its size, or says what the instrument offers instead when it has no sounds to browse.",
            "parameters":{"type":"object","properties":{"query":{"type":"string"},"rig":{"type":"integer"},"node":{"type":"integer"}},"required":["query"]}
        }},
        {"type":"function","function":{
            "name":"load_sound",
            "description":"Load a sound into a rig's instrument from that instrument's own library (see list_sounds): a Surge patch, a Dexed voice, an LV2 preset or MIDI programme, a plugin preset. query: words or an entry of list_sounds; rig: index from get_rigs (default: the rig in view); node: the instrument's node id, needed when a rig holds several different instruments. Returns the sound that was loaded and the unit it went into.",
            "parameters":{"type":"object","properties":{"query":{"type":"string"},"rig":{"type":"integer"},"node":{"type":"integer"}},"required":["query"]}
        }},
        {"type":"function","function":{
            "name":"undo",
            "description":"Undo the last agent job (everything one request changed: graph, cables, knobs, synth sounds, matrix, rigs, mixer) in one step. Use when the user says undo/revert/that was wrong, or to back out your own mistake.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"redo",
            "description":"Redo the agent job that undo just reverted.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"get_rigs",
            "description":"List every rig (all of them, not just the one in view): index, name, MIDI device, units (node ids), mixer vol/pan/mute/solo, plus which rig is in view. Every rig sounds at once; get_graph nodes carry their rig index too.",
            "parameters":{"type":"object","properties":{}}
        }},
        {"type":"function","function":{
            "name":"add_rig",
            "description":"Create a new rig (an independent rack: Keys -> Surge -> its own Master, wired and playable; a bare Keys -> Master when all 4 synth voices are taken). Existing rigs keep their patches, knobs and sounds. The view switches to the new rig. sound: optional sound query loaded into the new rig's instrument from its own library (e.g. 'flute' for Surge, 'bass' for YANES). Returns get_rigs.",
            "parameters":{"type":"object","properties":{"name":{"type":"string","description":"Optional rig name (default Rig A/B/C… or the plugin's name)"},"sound":{"type":"string","description":"Optional sound query for the new rig's instrument, e.g. 'flute'"},"plugin_id":{"type":"string","description":"Optional CLAP plugin id from list_plugins (e.g. 'org.yanes.native'): the rig is Keys -> that plugin -> Master instead of Surge"}}}
        }},
        {"type":"function","function":{
            "name":"rename_rig",
            "description":"Rename a rig (index from get_rigs), e.g. 'Bass' or 'Pads'. Up to 24 characters; names are kept in saved sessions.",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"},"name":{"type":"string"}},"required":["rig","name"]}
        }},
        {"type":"function","function":{
            "name":"delete_rig",
            "description":"Delete a rig and everything in it (units, cables, synth settings). DESTRUCTIVE: call it without confirm first (the chat then shows Confirm / Cancel buttons); confirm=true only after the user said yes in text. The last rig cannot be deleted.",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"},"confirm":{"type":"boolean"}},"required":["rig"]}
        }},
        {"type":"function","function":{
            "name":"select_rig",
            "description":"Show a rig in the big rack view (view only — every rig keeps sounding).",
            "parameters":{"type":"object","properties":{"rig":{"type":"integer"}},"required":["rig"]}
        }},
        {"type":"function","function":{
            "name":"save_rig",
            "description":"Save one rig (units, cables, knobs, plugin states, matrix routes, mix, name) as a named rig preset in ~/kabelsalat-rig-presets, independent of whole sessions. rig: 0-based rig index (default: the rig in view).",
            "parameters":{"type":"object","properties":{"name":{"type":"string","description":"Preset name (no slashes)"},"rig":{"type":"integer","description":"Rig index from get_rigs (default: active rig)"}},"required":["name"]}
        }},
        {"type":"function","function":{
            "name":"load_rig",
            "description":"Load a saved rig preset by name, restoring units, cables, knobs, plugin state, matrix routes, mix and rig name. Omit rig to add it as a new rig, or pass rig=<index> to replace an existing rig.",
            "parameters":{"type":"object","properties":{"name":{"type":"string","description":"Saved rig preset name (see list_rig_presets)"},"rig":{"type":"integer","description":"Optional target rig index to replace; omit to load as a new rig"}},"required":["name"]}
        }},
        {"type":"function","function":{
            "name":"list_rig_presets",
            "description":"List saved single-rig preset names in ~/kabelsalat-rig-presets.",
            "parameters":{"type":"object","properties":{}}
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

/// Which chat models can the DeepSeek key use? (once, for the picker)
/// The chat models the picker offers. The Jev router is a chat-model router,
/// not Jev's decision front (that runs before every request on its own): it
/// is offered only where it is the default, with OpenRouter as the only key.
/// The current model is always listed.
fn picker_models(openrouter: bool, deepseek: bool, current: &str) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    if openrouter && !deepseek {
        models.push(llm::JEV_ROUTER.into());
    }
    models.extend([llm::DEEPSEEK_CHAT.to_string(), "deepseek-reasoner".to_string()]);
    if !models.iter().any(|m| m == current) {
        models.insert(0, current.to_string());
    }
    models
}

/// Model choices shown in Settings for `role`: the picker list plus known
/// OpenRouter ids already used in code, keeping `current` present.
pub fn role_model_choices(role: Role, keys: llm::Keys, current: &str) -> Vec<String> {
    let mut list: Vec<String> = match role {
        Role::Decide => vec![llm::JEV.to_string()],
        Role::Act => {
            let mut m = picker_models(keys.openrouter, keys.deepseek, current);
            // Not the Jev router: `picker_models` already offers it where it is
            // the default; beside DeepSeek it read as "turn Jev on" (rule 78).
            for extra in [llm::CLAUDE_OPUS, llm::PERCEIVE_MODEL] {
                if !m.iter().any(|x| x == extra) {
                    m.push(extra.to_string());
                }
            }
            m
        }
        Role::Perceive => vec![
            llm::PERCEIVE_MODEL.to_string(),
            llm::GEMINI_FLASH_25.to_string(),
            llm::CLAUDE_OPUS.to_string(),
        ],
    };
    let cur = current.trim();
    if !cur.is_empty() && !cur.eq_ignore_ascii_case("off") && !list.iter().any(|x| x == cur) {
        list.insert(0, cur.to_string());
    }
    list
}

fn fetch_deepseek_models() -> Option<Vec<String>> {
    let key = Endpoint::DeepSeek.key()?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(20)).build().ok()?;
    let url = format!("{}/models", Endpoint::DeepSeek.spec().base_url);
    let v: Value = client.get(url).bearer_auth(key).send().ok()?.json().ok()?;
    let ids: Vec<String> = v.get("data")?.as_array()?
        .iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect();
    (!ids.is_empty()).then_some(ids)
}

/// Check one tool call from the model against the tool table and return
/// the arguments the tool will read. Err is the message the model gets
/// back, worded so it can correct the call — a call that does not fit is
/// never run with defaults in place of what the model meant.
fn parse_tool_call(tools: &Value, name: &str, raw_args: &str) -> Result<Value, String> {
    let all = tools.as_array().map(Vec::as_slice).unwrap_or_default();
    let tool_name = |t: &Value| t.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let Some(tool) = all.iter().find(|t| tool_name(t) == name) else {
        // "set_tempo" finds set_bpm through its description.
        let words: Vec<&str> = name.split('_').filter(|w| !["set", "get", "list", "add"].contains(w) && w.len() > 2).collect();
        let about = |t: &Value| format!("{} {}", tool_name(t), t.pointer("/function/description").and_then(|d| d.as_str()).unwrap_or("")).to_lowercase();
        let near: Vec<String> = all.iter().filter(|t| words.iter().any(|w| about(t).contains(&w.to_lowercase()))).map(tool_name).take(6).collect();
        let hint = if near.is_empty() { "use only tools from the tool list".to_string() } else { format!("did you mean: {}?", near.join(", ")) };
        return Err(format!("error: there is no tool '{name}' — {hint}"));
    };
    let raw = if raw_args.trim().is_empty() { "{}" } else { raw_args };
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|e| format!("error: the arguments of {name} are not valid JSON ({e}) — send one JSON object"))?;
    let Value::Object(given) = parsed else {
        return Err(format!("error: the arguments of {name} must be one JSON object, got {parsed}"));
    };
    let empty = serde_json::Map::new();
    let props = tool.pointer("/function/parameters/properties").and_then(|p| p.as_object()).unwrap_or(&empty);
    let takes = || if props.is_empty() { "no arguments".to_string() } else { props.keys().cloned().collect::<Vec<_>>().join(", ") };
    let mut args = serde_json::Map::new();
    for (key, value) in given {
        let Some(schema) = props.get(&key) else {
            return Err(format!("error: {name} has no argument '{key}' — it takes: {}", takes()));
        };
        if value.is_null() { continue; } // an explicit null means "not given"
        let value = coerce_arg(schema, value).map_err(|why| format!("error: argument '{key}' of {name} {why}"))?;
        args.insert(key, value);
    }
    let required = tool.pointer("/function/parameters/required").and_then(|r| r.as_array()).map(Vec::as_slice).unwrap_or_default();
    if let Some(missing) = required.iter().filter_map(|r| r.as_str()).find(|r| !args.contains_key(*r)) {
        return Err(format!("error: {name} needs the argument '{missing}' — it takes: {}", takes()));
    }
    Ok(Value::Object(args))
}

/// Fit one argument to its schema type. Models often send "60" or 60.0
/// for an integer; those are read as meant, anything else is an error.
fn coerce_arg(schema: &Value, value: Value) -> Result<Value, String> {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(|t| t.as_str()).collect(),
        _ => return Ok(value),
    };
    let fitted = types.iter().find_map(|t| match (*t, &value) {
        ("integer", Value::Number(n)) if n.is_i64() || n.is_u64() => Some(value.clone()),
        ("integer", Value::Number(n)) => n.as_f64().filter(|f| f.fract() == 0.0).map(|f| json!(f as i64)),
        ("integer", Value::String(s)) => s.trim().parse::<i64>().ok().map(|i| json!(i)),
        ("number", Value::Number(_)) => Some(value.clone()),
        ("number", Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| json!(f)),
        ("boolean", Value::Bool(_)) => Some(value.clone()),
        ("boolean", Value::String(s)) => s.trim().parse::<bool>().ok().map(|b| json!(b)),
        ("string", Value::String(_)) => Some(value.clone()),
        ("string", Value::Number(_) | Value::Bool(_)) => Some(json!(value.to_string())),
        _ => None,
    });
    let fitted = fitted.ok_or_else(|| format!("must be {}, got {value}", types.join(" or ")))?;
    match schema.get("enum").and_then(|e| e.as_array()) {
        Some(allowed) if !allowed.contains(&fitted) => {
            Err(format!("must be one of {}, got {fitted}", Value::Array(allowed.clone())))
        }
        _ => Ok(fitted),
    }
}

/// What a request states: the user's own words, and what Jev read from them.
struct Facts<'a> {
    request: &'a str,
    /// 0-based index of the rig the request names.
    rig: Option<u64>,
    /// 0-based index of the rig in view (from the app's [context] line),
    /// when Jev read that the request names no rig. A rig Jev is unsure of
    /// leaves both empty.
    view: Option<u64>,
    /// Jev's pick for each slot question it is sure of.
    picked: Vec<(&'static str, String)>,
}

impl Facts<'_> {
    fn pick(&self, slot: &Slot) -> Option<&str> {
        self.picked.iter().find(|(key, _)| *key == slot.key).map(|(_, pick)| pick.as_str())
    }

    /// The rig to work on where a request may leave it open: the one it
    /// names, else the one in view.
    fn rig_or_view(&self) -> Option<u64> {
        self.rig.or(self.view)
    }

    fn words(&self) -> Vec<String> {
        self.request.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
    }

    fn numbers(&self) -> Vec<f64> {
        self.request
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter_map(|t| t.trim_matches('.').parse::<f64>().ok())
            .collect()
    }

    /// A tempo the request states, e.g. "100 BPM".
    fn bpm(&self) -> Option<f64> {
        self.numbers().into_iter().find(|n| (20.0..=300.0).contains(n))
    }

    /// A level the request states in percent, as 0..1.
    fn percent(&self) -> Option<f64> {
        let lower = self.request.to_lowercase();
        let at = lower.find('%').or_else(|| lower.find("percent"))?;
        let digits: String = lower[..at].trim_end().chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let pct: f64 = digits.chars().rev().collect::<String>().parse().ok()?;
        (0.0..=100.0).contains(&pct).then_some(pct / 100.0)
    }

    /// A note the request names ("C4", "f#3", "middle C") as a MIDI note.
    fn note(&self) -> Option<u64> {
        let lower = self.request.to_lowercase();
        if lower.contains("middle c") { return Some(60); }
        lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).find_map(|word| {
            let mut chars = word.chars();
            let pitch = match chars.next()? { 'c' => 0, 'd' => 2, 'e' => 4, 'f' => 5, 'g' => 7, 'a' => 9, 'b' => 11, _ => return None };
            let rest = chars.as_str();
            let (accidental, octave) = match rest.strip_prefix('#') {
                Some(o) => (1, o),
                None => match rest.strip_prefix('b').filter(|o| !o.is_empty()) { Some(o) => (-1, o), None => (0, rest) },
            };
            let octave: i64 = octave.parse().ok().filter(|o| (0..=8).contains(o))?;
            Some((12 * (octave + 1) + pitch + accidental) as u64)
        })
    }

    /// The words that describe the sound asked for: "warm" and "pad" in
    /// "Load a warm pad into rig 2."
    fn sound_words(&self) -> Vec<String> {
        const FILLER: &[&str] = &[
            "a", "an", "the", "some", "me", "my", "i", "we", "us", "you", "to", "into", "in", "on", "onto", "for", "of", "with",
            "and", "please", "load", "give", "get", "want", "need", "use", "play", "put", "set", "make", "change", "switch",
            "pick", "choose", "find", "select", "sound", "sounds", "patch", "preset", "instrument", "synth", "surge", "nice",
            "good", "new", "something", "like", "that", "this", "it", "its", "can", "could", "would", "have", "let", "lets",
            "try", "now", "kind", "sort", "type", "one", "up", "s",
        ];
        let words = self.words();
        let mut kept = Vec::new();
        let mut i = 0;
        while i < words.len() {
            if words[i] == "rig" {
                // "the organ rig": the word before names it. "rig 2",
                // "rig B": the word after does.
                if i > 0 && kept.last() == Some(&words[i - 1]) { kept.pop(); }
                let named_after = words.get(i + 1).is_some_and(|w| w.len() == 1 || w.chars().all(|c| c.is_ascii_digit()));
                i += if named_after { 2 } else { 1 };
                continue;
            }
            if !FILLER.contains(&words[i].as_str()) { kept.push(words[i].clone()); }
            i += 1;
        }
        kept
    }
}

/// A typed question with a closed set of answers. Jev picks one; a plan
/// reads the pick.
struct Slot {
    key: &'static str,
    ask: &'static str,
    /// Each option and what it means.
    options: &'static [(&'static str, &'static str)],
}

/// The answer that picks none of a slot's options.
const NO_PICK: &str = "none";

/// The built-in effects, by the name add_node takes.
const EFFECT: Slot = Slot {
    key: "effect",
    ask: "Which ONE built-in effect does the user's request name?",
    options: &[
        ("reverb", "A reverb."), ("delay", "A delay / echo."), ("filter", "A filter."), ("eq3", "An EQ / equalizer."),
        ("limiter", "A limiter."), ("saturation", "A saturation / drive / distortion."), ("gain", "A gain unit."),
        ("pan", "A panner unit."),
    ],
};

/// The kinds of sound a request can ask for, by the word the patch search takes.
const SOUND: Slot = Slot {
    key: "sound",
    ask: "Which ONE kind of synth sound does the user's request ask for?",
    options: &[
        ("pad", "A pad: sustained, atmospheric."), ("bass", "A bass."), ("lead", "A lead or solo synth."),
        ("keys", "Keys: piano, electric piano, Rhodes, Wurlitzer, clavinet."), ("organ", "An organ."),
        ("string", "Strings."), ("brass", "Brass."), ("pluck", "A plucked sound."), ("bell", "Bells, mallets, chimes."),
        ("fx", "A sound effect."),
    ],
};

const SLOTS: &[&Slot] = &[&EFFECT, &SOUND];

/// One step of a shortcut's plan.
#[derive(Debug, PartialEq)]
enum Step {
    /// Tool calls for the app, run as one batch like a model step's.
    Run(Vec<ToolCall>),
    /// The plan is done: what the agent says.
    Say(String),
}

/// A request Jev settles without a chat model: a plan of tool calls built
/// from its typed answers, from what the request spells out and from what
/// the tools report back.
struct Shortcut {
    key: &'static str,
    /// The criterion Jev matches the request against.
    means: &'static str,
    /// The next step, given the results of the calls run so far (none at
    /// first). None = the request or the patch leaves open what a tool
    /// needs: the chat model takes over and sees those calls and results.
    plan: fn(&Facts, &[String]) -> Option<Step>,
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall { name: name.to_string(), args }
}

fn run(name: &str, args: Value) -> Option<Step> {
    Some(Step::Run(vec![call(name, args)]))
}

/// A plan of one tool call: run it, then answer with what it reported.
fn once(done: &[String], args: impl FnOnce() -> Option<ToolCall>, say: impl FnOnce(&str) -> String) -> Option<Step> {
    match done.last() {
        None => Some(Step::Run(vec![args()?])),
        Some(result) => Some(Step::Say(say(result))),
    }
}

/// The tool's own result line as a sentence.
fn say_result(result: &str) -> String {
    let mut chars = result.chars();
    let sentence: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if sentence.ends_with(['.', '!', '?']) { sentence } else { format!("{sentence}.") }
}

/// A mixer change, naming the rig the way the app shows it (1-based). The
/// tool's line is passed on instead when it reports a problem.
fn say_mix(f: &Facts, result: &str, what: &str) -> String {
    match f.rig {
        Some(rig) if result.starts_with(&format!("rig {rig}:")) => format!("Rig {} {what}.", rig + 1),
        _ => say_result(result),
    }
}

/// Set one field of the named rig's mixer strip.
fn mix(f: &Facts, done: &[String], field: &str, value: Value, what: &str) -> Option<Step> {
    once(done, || Some(call("set_rig_mix", json!({ "rig": f.rig?, field: value }))), |r| say_mix(f, r, what))
}

/// "A bit" quieter or louder: about 3 dB.
const VOLUME_STEP: f64 = 0.7;
/// A pan nudge: half the way from the centre to one side.
const PAN_STEP: f64 = 0.5;

fn quieter(vol: f64) -> f64 { vol * VOLUME_STEP }
fn louder(vol: f64) -> f64 { (vol.max(0.1) / VOLUME_STEP).min(1.0) }
fn left(pan: f64) -> f64 { (pan - PAN_STEP).max(-1.0) }
fn right(pan: f64) -> f64 { (pan + PAN_STEP).min(1.0) }

fn say_vol(vol: f64) -> String {
    format!("at {:.0} %", vol * 100.0)
}

fn say_pan(pan: f64) -> String {
    match (pan * 100.0).round() as i64 {
        0 => "panned to the centre".to_string(),
        p if p < 0 => format!("panned {} % left", -p),
        p => format!("panned {p} % right"),
    }
}

/// Move one field of the named rig's mixer strip a step from where it is:
/// read the strip, then set the new value.
fn nudge(f: &Facts, done: &[String], field: &str, step: fn(f64) -> f64, say: fn(f64) -> String) -> Option<Step> {
    let rig = f.rig?;
    let [rigs, set @ ..] = done else { return run("get_rigs", json!({})) };
    let rigs: Value = serde_json::from_str(rigs).ok()?;
    let now = rigs["rigs"].as_array()?.iter().find(|r| r["rig"] == rig)?[field].as_f64()?;
    let next = (step(now) * 100.0).round() / 100.0;
    match set.last() {
        Some(result) => Some(Step::Say(say_mix(f, result, &format!("is now {}", say(next))))),
        None if (next - now).abs() < 0.005 => Some(Step::Say(format!("Rig {} is already {}.", rig + 1, say(now)))),
        None => run("set_rig_mix", json!({ "rig": rig, field: next })),
    }
}

fn graph_of(result: &str) -> Option<Graph> {
    serde_json::from_str(result).ok()
}

fn rig_master(g: &Graph, rig: u64) -> Option<NodeId> {
    g.nodes.iter().find(|n| n.rig as u64 == rig && n.kind == NodeKind::MasterOut).map(|n| n.id)
}

fn is_audio(e: &&Edge) -> bool {
    e.kind == PortKind::Audio
}

/// The id of the node an add_node result names (the app appends it once the
/// engine applied the edit).
fn new_node_id(result: &str) -> Option<u64> {
    result.split("new node id ").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Did every edit of a batch take effect?
fn applied(results: &[String]) -> bool {
    !results.is_empty() && results.iter().all(|r| !r.contains("NOT applied") && !r.starts_with("error"))
}

fn effect_kind(effect: &str) -> Option<(DspKind, &'static str)> {
    Some(match effect {
        "reverb" => (DspKind::Reverb, "Reverb"),
        "delay" => (DspKind::Delay, "Delay"),
        "filter" => (DspKind::Filter, "Filter"),
        "eq3" => (DspKind::Eq3, "EQ"),
        "limiter" => (DspKind::Limiter, "Limiter"),
        "saturation" => (DspKind::Saturation, "Saturation"),
        "gain" => (DspKind::Gain, "Gain"),
        "pan" => (DspKind::Pan, "Panner"),
        _ => return None,
    })
}

/// Where a new effect goes: in front of the rig's Master, taking over every
/// audio cable that feeds it (a metronome keeps its own cable). None when
/// nothing feeds the Master, or when the request asks for a position
/// ("before the delay") in a chain that already has effects.
fn chain_end(g: &Graph, rig: u64, f: &Facts) -> Option<(NodeId, Vec<Edge>)> {
    let master = rig_master(g, rig)?;
    let kind_of = |e: &Edge| g.node(e.from.node).map(|n| &n.kind);
    let feeds: Vec<Edge> = g.edges.iter().filter(is_audio)
        .filter(|e| e.to.node == master && kind_of(e) != Some(&NodeKind::Dsp(DspKind::Metronome)))
        .cloned().collect();
    let only_sources = feeds.iter().all(|e| matches!(kind_of(e), Some(NodeKind::Clap { .. } | NodeKind::AudioIn { .. })));
    let names_a_place = f.words().iter().any(|w| ["after", "before", "between", "behind", "front"].contains(&w.as_str()));
    (!feeds.is_empty() && (only_sources || !names_a_place)).then_some((master, feeds))
}

/// Add one effect to a rig and wire it in front of the Master.
fn add_effect(f: &Facts, done: &[String]) -> Option<Step> {
    let effect = f.pick(&EFFECT)?;
    let (_, name) = effect_kind(effect)?;
    let rig = f.rig_or_view()?;
    match done {
        [] => run("get_graph", json!({})),
        [graph] => {
            chain_end(&graph_of(graph)?, rig, f)?;
            run("add_node", json!({ "kind": "dsp", "dsp": effect, "rig": rig }))
        }
        [graph, added] => {
            let (master, feeds) = chain_end(&graph_of(graph)?, rig, f)?;
            let fx = new_node_id(added)?;
            let mut calls: Vec<ToolCall> = feeds.iter().map(|e| call("disconnect", json!({ "id": e.id }))).collect();
            let mut wire = |c: ToolCall| if !calls.contains(&c) { calls.push(c) };
            for e in &feeds {
                wire(call("connect", json!({ "from_node": e.from.node, "from_port": e.from.port, "to_node": fx })));
            }
            for e in &feeds {
                wire(call("connect", json!({ "from_node": fx, "to_node": master, "to_port": e.to.port })));
            }
            Some(Step::Run(calls))
        }
        [_, _, wired @ ..] => applied(wired).then(|| Step::Say(format!("{name} added to Rig {}, in front of Master.", rig + 1))),
    }
}

/// Remove a rig's one effect of a kind and close the gap it leaves.
fn remove_effect(f: &Facts, done: &[String]) -> Option<Step> {
    let (kind, name) = effect_kind(f.pick(&EFFECT)?)?;
    let rig = f.rig_or_view()?;
    match done {
        [] => run("get_graph", json!({})),
        [graph] => {
            let g = graph_of(graph)?;
            let mut units = g.nodes.iter().filter(|n| n.rig as u64 == rig && n.kind == NodeKind::Dsp(kind.clone()));
            // Which of two delays is meant is the chat model's question.
            let (Some(fx), None) = (units.next().map(|n| n.id), units.next()) else { return None };
            let mut calls = vec![call("remove_node", json!({ "node": fx }))];
            for out in g.edges.iter().filter(is_audio).filter(|e| e.from.node == fx) {
                for src in g.edges.iter().filter(is_audio).filter(|e| e.to.node == fx) {
                    let wired = g.edges.iter().filter(is_audio).any(|e| e.from == src.from && e.to == out.to);
                    let c = call("connect", json!({ "from_node": src.from.node, "from_port": src.from.port, "to_node": out.to.node, "to_port": out.to.port }));
                    if !wired && !calls.contains(&c) { calls.push(c); }
                }
            }
            Some(Step::Run(calls))
        }
        [_, removed @ ..] => applied(removed).then(|| Step::Say(format!("{name} removed from Rig {}.", rig + 1))),
    }
}

/// A running metronome in the rig: start the one that is there (wiring it
/// to the Master if it is not), else add one, wire it and start it.
fn metronome(f: &Facts, done: &[String]) -> Option<Step> {
    let rig = f.rig_or_view()?;
    let [graph, since @ ..] = done else { return run("get_graph", json!({})) };
    let g = graph_of(graph)?;
    let master = rig_master(&g, rig)?;
    let wire = |node: u64| call("connect", json!({ "from_node": node, "to_node": master }));
    let start = |node: u64| call("set_node_playing", json!({ "node": node, "playing": true }));
    let added = since.first().and_then(|r| new_node_id(r));
    match (since, added) {
        ([], _) => match g.nodes.iter().find(|n| n.rig as u64 == rig && n.kind == NodeKind::Dsp(DspKind::Metronome)) {
            Some(m) if g.edges.iter().filter(is_audio).any(|e| e.from.node == m.id && e.to.node == master) => Some(Step::Run(vec![start(m.id as u64)])),
            Some(m) => Some(Step::Run(vec![wire(m.id as u64), start(m.id as u64)])),
            None => run("add_node", json!({ "kind": "dsp", "dsp": "metronome", "rig": rig })),
        },
        ([_], Some(node)) => Some(Step::Run(vec![wire(node), start(node)])),
        ([.., started], _) if applied(since) && started.ends_with(" playing") => Some(Step::Say(
            if added.is_some() { format!("Metronome added to Rig {} and running.", rig + 1) } else { "The metronome is running.".to_string() },
        )),
        _ => None,
    }
}

/// Load a sound of the kind Jev picked from the library of the rig's own
/// instrument. That library is asked for the user's words with the kind
/// ("jazz drawbar organ"), then for the words, then for the kind alone; an
/// instrument without sounds, or one that needs a node named, is the chat
/// model's to explain.
fn load_sound(f: &Facts, done: &[String]) -> Option<Step> {
    // "Pads/Dreamscape" names one sound, not a kind: no other pad will do.
    if f.request.contains('/') { return None; }
    let kind = f.pick(&SOUND)?;
    let rig = f.rig_or_view()?;
    let described = f.sound_words().join(" ");
    let mut queries: Vec<String> = Vec::new();
    for q in [if described.contains(kind) { described.clone() } else { format!("{described} {kind}") }, described.clone(), kind.to_string()] {
        let q = q.trim().to_string();
        if !q.is_empty() && !queries.contains(&q) { queries.push(q); }
    }
    let misses = done.iter().filter(|r| r.starts_with("no ")).count();
    let query = queries.get(misses)?;
    match done.last().map(String::as_str) {
        Some(hits) if hits.starts_with('{') => run("load_sound", json!({ "query": query, "rig": rig })),
        // A sound's name may hold an apostrophe ("8' solo").
        Some(loaded) if loaded.starts_with("loaded '") => Some(Step::Say(format!("Rig {} now plays {}.", rig + 1, loaded.strip_prefix("loaded '")?.rsplit_once("' into ")?.0))),
        Some(other) if !other.starts_with("no ") => None,
        _ => run("list_sounds", json!({ "query": query, "rig": rig })),
    }
}

const SHORTCUTS: &[Shortcut] = &[
    Shortcut { key: "tempo", means: "Set the tempo to a stated BPM value.",
        plan: |f, done| once(done, || Some(call("set_bpm", json!({ "bpm": f.bpm()? }))), say_result) },
    Shortcut { key: "volume", means: "Set one named rig's volume to a stated percentage.",
        plan: |f, done| mix(f, done, "vol", json!(f.percent()?), &format!("is {}", say_vol(f.percent()?))) },
    Shortcut { key: "quieter", means: "Make one named rig quieter, with no amount stated ('a bit quieter', 'turn it down').",
        plan: |f, done| nudge(f, done, "vol", quieter, say_vol) },
    Shortcut { key: "louder", means: "Make one named rig louder, with no amount stated ('a bit louder', 'turn it up').",
        plan: |f, done| nudge(f, done, "vol", louder, say_vol) },
    Shortcut { key: "pan_left", means: "Pan one named rig to the left, with no amount stated.",
        plan: |f, done| nudge(f, done, "pan", left, say_pan) },
    Shortcut { key: "pan_right", means: "Pan one named rig to the right, with no amount stated.",
        plan: |f, done| nudge(f, done, "pan", right, say_pan) },
    Shortcut { key: "mute", means: "Mute one named rig.", plan: |f, done| mix(f, done, "mute", json!(true), "is muted") },
    Shortcut { key: "unmute", means: "Unmute one named rig.", plan: |f, done| mix(f, done, "mute", json!(false), "is unmuted") },
    Shortcut { key: "solo", means: "Solo one named rig.", plan: |f, done| mix(f, done, "solo", json!(true), "is soloed") },
    Shortcut { key: "unsolo", means: "Take one named rig out of solo.", plan: |f, done| mix(f, done, "solo", json!(false), "is out of solo") },
    Shortcut { key: "split", means: "Split the keyboard at a stated note, and nothing more (no word about which instrument plays which half).",
        plan: |f, done| once(done, || Some(call("set_split", json!({ "point": f.note()? }))), say_result) },
    Shortcut { key: "unsplit", means: "Remove the keyboard split.",
        plan: |_, done| once(done, || Some(call("set_split", json!({}))), say_result) },
    Shortcut { key: "add_effect", means: "Add ONE built-in effect (reverb, delay, filter, EQ, limiter, saturation, gain, panner) to a rig and wire it into the signal chain (the request may say 'wire it in', 'after the synth' or 'before the master'), and nothing more (no settings, no second change).",
        plan: add_effect },
    Shortcut { key: "remove_effect", means: "Remove ONE effect unit (e.g. the delay) from a rig, and nothing more.",
        plan: remove_effect },
    Shortcut { key: "load_sound", means: "Load a kind of sound or preset (e.g. a warm pad, a bass, a piano, a jazz organ registration) into an existing rig's instrument, and nothing more (no new rig, no level).",
        plan: load_sound },
    Shortcut { key: "add_rig", means: "Add a new, plain rig (no sound, name or level asked for).",
        // The tool appends the rig list as JSON for a model to read.
        plan: |_, done| once(done, || Some(call("add_rig", json!({}))), |r| say_result(r.split(" {").next().unwrap_or(r).trim())) },
    Shortcut { key: "delete_rig", means: "Delete one named rig.",
        plan: |f, done| once(done, || Some(call("delete_rig", json!({ "rig": f.rig? }))), |r| if r.contains("Confirm / Cancel") {
            "That deletes the rig and everything in it. Confirm or cancel below.".to_string()
        } else {
            say_result(r)
        }) },
    Shortcut { key: "show_rig", means: "Show or switch the view to one named rig.",
        plan: |f, done| once(done, || Some(call("select_rig", json!({ "rig": f.rig? }))), say_result) },
    Shortcut { key: "metronome", means: "Add a metronome (a click), or start the metronome.", plan: metronome },
    Shortcut { key: "play", means: "Start the transport (play), with no word about a metronome.",
        plan: |_, done| once(done, || Some(call("transport_play", json!({}))), say_result) },
    Shortcut { key: "stop", means: "Stop the transport.",
        plan: |_, done| once(done, || Some(call("transport_stop", json!({}))), say_result) },
    Shortcut { key: "record", means: "Start recording.",
        plan: |_, done| once(done, || Some(call("record_start", json!({}))), say_result) },
    Shortcut { key: "record_stop", means: "Stop the recording.",
        plan: |_, done| once(done, || Some(call("record_stop", json!({}))), say_result) },
    Shortcut { key: "undo", means: "Undo the last change.",
        plan: |_, done| once(done, || Some(call("undo", json!({}))), say_result) },
    Shortcut { key: "redo", means: "Redo the change that was just undone.",
        plan: |_, done| once(done, || Some(call("redo", json!({}))), say_result) },
    Shortcut { key: "flip", means: "Flip the rack to its other side (front / back).",
        plan: |_, done| once(done, || Some(call("flip_rack", json!({}))), say_result) },
];

/// The answer that hands the request to the chat model.
const NOT_A_SHORTCUT: &str = "other";
/// Below this probability Jev's pick is not acted on.
const DECISION_CONFIDENCE: f64 = 0.85;
const MAX_RIGS: u64 = 8;
/// Batches one plan may run. The longest plan takes three.
const MAX_PLAN_BATCHES: usize = 6;

/// The typed questions Jev answers about a request.
fn decision_questions() -> Value {
    let mut actions = serde_json::Map::new();
    for s in SHORTCUTS { actions.insert(s.key.to_string(), json!(s.means)); }
    actions.insert(NOT_A_SHORTCUT.to_string(), json!(
        "Anything else: a question, a request for more than one change, wiring or rewiring units, a new rig with a sound \
         or a level, an effect with settings or placed between two other units, or anything unclear."));
    let mut rigs = serde_json::Map::new();
    for n in 1..=MAX_RIGS { rigs.insert(format!("rig_{n}"), json!(format!("The request names rig {n} (by number, letter or name; see 'all rigs' in the context)."))); }
    rigs.insert(NO_PICK.to_string(), json!("The request names no rig, or says 'this rig' / 'the current rig'."));
    let mut questions = json!({
        "action": { "type": "choice", "instructions": "Which ONE action does the user's request (the text after the [context] line) ask for?", "criteria": actions },
        "rig": { "type": "choice", "instructions": "Which rig does the user's request name?", "criteria": rigs },
    });
    for slot in SLOTS {
        let mut options = serde_json::Map::new();
        for (option, means) in slot.options { options.insert(option.to_string(), json!(means)); }
        options.insert(NO_PICK.to_string(), json!("None of these."));
        questions[slot.key] = json!({ "type": "choice", "instructions": slot.ask, "criteria": options });
    }
    questions
}

/// What Jev's answers settle: a shortcut, the facts its plan reads, and the
/// plan's first step — when Jev is sure of the shortcut and the request
/// spells out what that step needs.
fn settle<'a>(decision: &llm::Decisions, user: &'a str) -> Option<(&'static Shortcut, Facts<'a>, Step)> {
    let sure = |question: &str| decision.choice(question).filter(|(_, p)| *p >= DECISION_CONFIDENCE).map(|(pick, _)| pick);
    let action = sure("action")?;
    let shortcut = SHORTCUTS.iter().find(|s| s.key == action)?;
    // The app prepends a [context: …] line; numbers, notes and words come
    // from the user's own words only.
    let (context, request) = match user.split_once('\n') {
        Some((context, rest)) if context.starts_with("[context:") => (context, rest),
        _ => ("", user),
    };
    let facts = Facts {
        request,
        rig: sure("rig").and_then(|rig| rig.strip_prefix("rig_")?.parse::<u64>().ok()).map(|n| n - 1),
        view: decision.choice("rig").filter(|(pick, _)| *pick == NO_PICK)
            .and_then(|_| context.split("(index ").nth(1)?.split(')').next()?.parse().ok()),
        picked: SLOTS.iter()
            .filter_map(|slot| Some((slot.key, sure(slot.key).filter(|pick| *pick != NO_PICK)?.to_string())))
            .collect(),
    };
    let first = (shortcut.plan)(&facts, &[])?;
    Some((shortcut, facts, first))
}

/// Run one shortcut's plan against the app the way the worker does, for
/// tests of what the app's tools answer. `view` is the rig in view, `picks`
/// are Jev's slot answers. Returns what the agent says; None where the plan
/// hands over to the chat model.
#[cfg(test)]
pub fn run_shortcut(key: &str, request: &str, view: u64, picks: &[(&'static str, &str)], mut app: impl FnMut(Vec<ToolCall>) -> Vec<String>) -> Option<String> {
    let shortcut = SHORTCUTS.iter().find(|s| s.key == key)?;
    let facts = Facts { request, rig: None, view: Some(view), picked: picks.iter().map(|(slot, pick)| (*slot, pick.to_string())).collect() };
    let mut done = Vec::new();
    for _ in 0..MAX_PLAN_BATCHES {
        match (shortcut.plan)(&facts, &done)? {
            Step::Say(text) => return Some(text),
            Step::Run(calls) => done.extend(app(calls)),
        }
    }
    None
}

/// Requests `kabelsalat jev-test` asks about.
const JEV_SAMPLES: &[&str] = &[
    "Set the tempo to 100 BPM.", "What tempo are we at?", "Split the keyboard at C4.",
    "Split the keyboard at C4: Surge A plays the low half, Surge B the high half.", "Load a warm pad.",
    "Load a bass sound into rig 2.", "give me a rhodes", "load a flute", "Load the patch Pads/Dreamscape.",
    "Load a bass sound on the YANES rig.", "Give the organ rig a jazz drawbar preset.",
    "Add a reverb to rig 1 and wire it in.", "Add a reverb to this rig", "Insert a reverb into rig 2.",
    "Put a delay after the synth.", "Put a filter between the synth and the delay.", "Add a reverb with a long tail to rig 1.",
    "Add a chorus to rig 1.", "Add a reverb to rig 1 and set the tempo to 120 BPM.", "Remove the delay from rig 1.",
    "Add a metronome and start it.", "Start the metronome.", "play", "stop", "Start recording.", "stop the recording",
    "Add a new rig.", "Make a new rig with a bass sound, quieter than rig 1.", "Set rig 2 volume to 50 %.",
    "Make rig 2 a bit quieter.", "make rig B a bit louder", "turn rig 1 down by 6 dB", "Pan rig 1 to the left.",
    "pan rig 2 right", "Pan rig 1 hard left and rig 2 hard right.", "Mute rig 1.", "unmute rig 1", "solo the second rig",
    "Delete rig 2.", "show rig 2", "undo that", "Why is there no sound?",
];

/// What a decision leads to, for the test printout.
fn settled_as(d: &llm::Decisions, user: &str) -> String {
    match settle(d, user) {
        Some((_, _, Step::Run(calls))) => format!("-> {}", calls.iter().map(|c| format!("{}({})", c.name, c.args)).collect::<Vec<_>>().join(" ")),
        Some((_, _, Step::Say(text))) => format!("-> says: {text}"),
        None => "-> chat model".to_string(),
    }
}

/// `kabelsalat jev-test [--image <file> <request>]`: ask the configured
/// decisions model about sample requests and print what it decides, how
/// sure it is and how long it took. With `--image`, ask about one request
/// twice, without and with the image, to compare what the image adds.
pub fn run_jev_test() -> anyhow::Result<()> {
    let route = Route::configured(Role::Decide);
    let key = route.endpoint.key().ok_or_else(|| anyhow::anyhow!("no {} key in config.json", route.endpoint.spec().label))?;
    let mut client = llm::Client::new(llm::HttpTransport::new().map_err(anyhow::Error::msg)?);
    let context = "[context: rig in view = 1 · Rig A (index 0), units: Keys (node 0), Surge (node 1), Delay (node 3), Master (node 2) · all rigs: 1 · Rig A, 2 · Rig B, 3 · YANES, 4 · Organ]";
    let mut ask = |request: &str, image: Option<&Image>| -> anyhow::Result<String> {
        let user = format!("{context}\n{request}");
        let d = client.decide(&route, &key, &user, image, decision_questions()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let show = |q: &str| d.choice(q).map_or("refused".to_string(), |(c, p)| format!("{c} {p:.2}"));
        Ok(format!("  {:>5} ms  action {:<18} rig {:<10} effect {:<15} sound {:<11} {:<48} {request}  [{}]",
            d.ms, show("action"), show("rig"), show(EFFECT.key), show(SOUND.key), settled_as(&d, &user), d.model))
    };
    let args: Vec<String> = std::env::args().skip(2).collect();
    println!("jev-test: {} on {}", route.model, route.endpoint.spec().label);
    if let Some(at) = args.iter().position(|a| a == "--image") {
        let (Some(path), Some(request)) = (args.get(at + 1), args.get(at + 2)) else { anyhow::bail!("usage: jev-test --image <file> <request>") };
        let image = Image::from_file(path, &std::fs::read(path)?).map_err(anyhow::Error::msg)?;
        println!("image: {}, {} KB", image.describe(), image.bytes.len() / 1000);
        for _ in 0..3 {
            println!("text only{}", ask(request, None)?);
            println!("with image{}", ask(request, Some(&image))?);
        }
        return Ok(());
    }
    for request in JEV_SAMPLES {
        println!("{}", ask(request, None)?);
    }
    Ok(())
}

/// Characters of conversation (beyond the system prompt and the tool
/// table) a request may carry: about 15k tokens.
const CONTEXT_BUDGET: usize = 60_000;
/// What is left of a tool result once it is old. Results describe the patch
/// at the time of the call; the model asks again when it needs fresh data.
const OLD_RESULT_CHARS: usize = 300;

/// What one image counts against the budget: its base64 text is megabytes
/// of characters, but a model reads it as about a thousand tokens.
const IMAGE_CHARS: usize = 4_000;

/// The conversation as whole turns: a user message followed by the model's
/// messages and their tool results. Trimming drops whole turns from the
/// front, so a tool result can never lose the call it answers.
#[derive(Default)]
struct Conversation {
    turns: std::collections::VecDeque<Vec<Value>>,
}

impl Conversation {
    /// A new user message. Tool results of earlier turns are shortened and
    /// their images dropped: an image is sent only during its own turn.
    fn start_turn(&mut self, user: &UserMessage) {
        for turn in &mut self.turns {
            for msg in turn.iter_mut() {
                shorten_tool_result(msg);
                drop_image(msg);
            }
        }
        self.turns.push_back(vec![user_message(&user.text, user.image.as_ref())]);
    }

    /// Append a message to the current turn.
    fn push(&mut self, msg: Value) {
        if let Some(turn) = self.turns.back_mut() { turn.push(msg); }
    }

    /// Does the current turn carry an image (attached, or shown by a tool)?
    fn has_image(&self) -> bool {
        self.turns.back().is_some_and(|turn| turn.iter().any(|m| m["content"].is_array()))
    }

    fn chars(&self) -> usize {
        self.turns.iter().flatten().map(message_chars).sum()
    }

    /// Bring the conversation under `budget`: drop the oldest turns first;
    /// when only the current turn is left, shorten its tool results, oldest
    /// first, sparing those of the latest step.
    fn fit(&mut self, budget: usize) {
        while self.chars() > budget && self.turns.len() > 1 {
            self.turns.pop_front();
        }
        let Some(turn) = self.turns.back_mut() else { return };
        let latest_step = turn.iter().rposition(|m| m["role"] == "assistant").unwrap_or(turn.len());
        for i in 0..latest_step {
            if turn.iter().map(message_chars).sum::<usize>() <= budget { break; }
            shorten_tool_result(&mut turn[i]);
        }
    }

    /// The messages of a request: the system prompt, then every turn.
    fn request(&self, system: &str) -> Vec<Value> {
        std::iter::once(json!({ "role": "system", "content": system }))
            .chain(self.turns.iter().flatten().cloned())
            .collect()
    }
}

/// A user message: plain text, or text and the image as content parts. The
/// text names the image, so the note stays when the image is dropped.
fn user_message(text: &str, image: Option<&Image>) -> Value {
    match image {
        None => json!({ "role": "user", "content": text }),
        Some(image) => json!({ "role": "user", "content": [
            { "type": "text", "text": format!("{text}\n[image: {}]", image.describe()) },
            image.chat_part(),
        ]}),
    }
}

fn message_chars(msg: &Value) -> usize {
    match msg["content"].as_array() {
        None => msg.to_string().len(),
        Some(parts) => parts.iter().map(|p| p["text"].as_str().map_or(IMAGE_CHARS, str::len)).sum(),
    }
}

/// Replace a message's image by one line that says it was there.
fn drop_image(msg: &mut Value) {
    let Some(parts) = msg["content"].as_array() else { return };
    let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
    msg["content"] = json!(format!("{} — no longer attached: an image is sent only during its own turn", text.join("\n")));
}

fn shorten_tool_result(msg: &mut Value) {
    if msg["role"] != "tool" { return; }
    let Some(content) = msg["content"].as_str() else { return };
    if content.chars().count() <= OLD_RESULT_CHARS { return; }
    let head: String = content.chars().take(OLD_RESULT_CHARS).collect();
    msg["content"] = json!(format!("{head}… [shortened — call the tool again for current data]"));
}

/// Model round trips one request may take. A patch built from scratch needs
/// about a dozen; past this the model is going in circles.
const MAX_STEPS: usize = 24;
/// How long the app may take to run one step's tool calls.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How a turn ended.
enum TurnEnd {
    Answered,
    Failed(String),
    /// The user wrote again while tool calls were out: that message is next.
    Interrupted(UserMessage),
    /// The app is gone.
    Closed,
}

/// The agent loop: one turn per user message. A decisions model (Jev) is
/// asked first and settles a simple request with a plan of tool calls;
/// everything else goes to a chat model (the acting model, or the seeing
/// model while the turn carries an image), whose tool calls travel to the
/// app as ONE batch per model step and come back as one batch of results.
fn worker<T: Transport>(
    mut client: llm::Client<T>,
    routes: Routes,
    key_for: fn(Endpoint) -> Option<String>,
    rx: Receiver<ToWorker>,
    tx: Sender<FromWorker>,
) {
    let system = system_prompt();
    let tools = tools();
    let mut convo = Conversation::default();
    let mut next_user: Option<UserMessage> = None;
    loop {
        let user = match next_user.take() {
            Some(message) => message,
            None => match rx.recv() {
                Ok(ToWorker::User(message)) => message,
                Ok(ToWorker::ToolResults(_)) => continue, // results of a turn that already ended
                Err(_) => break,
            },
        };
        convo.start_turn(&user);
        let jev_route = routes.decide.lock().ok().and_then(|d| d.clone());
        let end = match jev_route.as_ref().and_then(|jev| decide_turn(&mut client, jev, key_for, &user, &mut convo, &rx, &tx)) {
            Some(end) => end,
            None => run_turn(&mut client, &routes, key_for, &system, &tools, &mut convo, &rx, &tx),
        };
        match end {
            TurnEnd::Answered => {}
            TurnEnd::Failed(why) => { let _ = tx.send(FromWorker::Error(why)); }
            TurnEnd::Interrupted(message) => { next_user = Some(message); continue; }
            TurnEnd::Closed => break,
        }
        let _ = tx.send(FromWorker::Done);
    }
}

/// Send one batch of tool calls to the app and take its results (one per
/// call, in call order). Err = the turn ends here.
fn run_batch(batch: Vec<ToolCall>, rx: &Receiver<ToWorker>, tx: &Sender<FromWorker>) -> Result<Vec<ToolOutput>, TurnEnd> {
    let _ = tx.send(FromWorker::ToolCalls(batch));
    match rx.recv_timeout(TOOL_TIMEOUT) {
        Ok(ToWorker::ToolResults(ran)) => Ok(ran),
        Ok(ToWorker::User(message)) => Err(TurnEnd::Interrupted(message)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(TurnEnd::Failed(format!("the app did not run the tool calls within {} s", TOOL_TIMEOUT.as_secs())))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(TurnEnd::Closed),
    }
}

const NOT_RUN: &str = "no result: the call did not run";

/// Record one batch in the conversation the way a chat model's step looks:
/// the assistant's tool calls, each call's result, then every image a tool
/// showed (a tool message carries text only). A call without a result gets
/// `NOT_RUN`: an unanswered call makes the next request invalid.
fn record_batch(convo: &mut Conversation, calls: &[(Value, &str)], results: Vec<Option<ToolOutput>>) {
    let mut shown: Vec<Value> = Vec::new();
    for ((id, name), result) in calls.iter().zip(results) {
        let out = result.unwrap_or_else(|| NOT_RUN.to_string().into());
        convo.push(json!({ "role": "tool", "tool_call_id": id, "content": out.text }));
        if let Some(image) = &out.image {
            shown.push(user_message(&format!("[what {name} shows]"), Some(image)));
        }
    }
    for message in shown { convo.push(message); }
}

/// Ask Jev about the request. When it is sure of a shortcut, run that
/// shortcut's plan and answer; None = a chat model takes the turn (also
/// when the decision itself fails: the fast path must never cost a
/// request), from wherever the plan left off.
fn decide_turn<T: Transport>(
    client: &mut llm::Client<T>,
    jev: &Route,
    key_for: fn(Endpoint) -> Option<String>,
    user: &UserMessage,
    convo: &mut Conversation,
    rx: &Receiver<ToWorker>,
    tx: &Sender<FromWorker>,
) -> Option<TurnEnd> {
    // An image turn goes straight to the seeing model. Measured with
    // `jev-test --image`: Jev reads a note in an image, but cannot return a
    // value it states, and what it sees leaks into the rig and slot answers.
    if user.image.is_some() { return None; }
    let key = key_for(jev.endpoint)?;
    let _ = tx.send(FromWorker::Phase(RunPhase::Deciding));
    let decision = client.decide(jev, &key, &user.text, None, decision_questions()).ok()?;
    let _ = tx.send(FromWorker::Step(StepStat { ms: decision.ms, model: decision.model.clone(), attempts: decision.attempts, rejected: 0 }));
    let (shortcut, facts, mut step) = settle(&decision, &user.text)?;
    let mut done: Vec<String> = Vec::new();
    for round in 0..MAX_PLAN_BATCHES {
        let calls = match step {
            Step::Say(text) => {
                convo.push(json!({ "role": "assistant", "content": text }));
                let _ = tx.send(FromWorker::Text(text));
                return Some(TurnEnd::Answered);
            }
            Step::Run(calls) => calls,
        };
        // The conversation records the calls like a chat model's, so a
        // later chat step knows what was done. DeepSeek's thinking models
        // answer 400 to a tool call they did not issue themselves unless
        // it carries `reasoning_content` (empty will do).
        let ids: Vec<(Value, &str)> = calls.iter().enumerate().map(|(i, c)| (json!(format!("jev-{round}-{i}")), c.name.as_str())).collect();
        convo.push(json!({ "role": "assistant", "content": null, "reasoning_content": "", "tool_calls": calls.iter().zip(&ids).map(|(c, (id, _))| json!({
            "id": id, "type": "function", "function": { "name": c.name, "arguments": c.args.to_string() },
        })).collect::<Vec<_>>() }));
        let (ran, stopped) = match run_batch(calls.clone(), rx, tx) {
            Ok(ran) => (ran, None),
            Err(end) => (Vec::new(), Some(end)),
        };
        let mut ran = ran.into_iter();
        let results: Vec<Option<ToolOutput>> = calls.iter().map(|_| ran.next()).collect();
        done.extend(results.iter().map(|r| r.as_ref().map_or(NOT_RUN.to_string(), |out| out.text.clone())));
        record_batch(convo, &ids, results);
        if stopped.is_some() { return stopped; }
        step = (shortcut.plan)(&facts, &done)?;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn run_turn<T: Transport>(
    client: &mut llm::Client<T>,
    routes: &Routes,
    key_for: fn(Endpoint) -> Option<String>,
    system: &str,
    tools: &Value,
    convo: &mut Conversation,
    rx: &Receiver<ToWorker>,
    tx: &Sender<FromWorker>,
) -> TurnEnd {
    for step_idx in 0..MAX_STEPS {
        convo.fit(CONTEXT_BUDGET);
        // The acting model may not see images: from the step an image
        // enters the turn, the seeing model acts.
        let sees = convo.has_image();
        let route = if sees {
            routes.perceive.lock().map(|r| r.clone()).unwrap_or_else(|_| Route::for_model(llm::PERCEIVE_MODEL))
        } else {
            routes.act.lock().map(|r| r.clone()).unwrap_or_else(|_| Route::for_model(llm::DEEPSEEK_CHAT))
        };
        let spec = route.endpoint.spec();
        let Some(key) = key_for(route.endpoint) else {
            let need = if sees { format!("an image needs {} ({})", route.model, spec.label) } else { format!("no {} API key", spec.label) };
            return TurnEnd::Failed(format!("{need}: add {} to {}", spec.key_field, config_path().display()));
        };
        let _ = tx.send(FromWorker::Phase(RunPhase::Thinking {
            step: step_idx + 1,
            model: route.model.clone(),
            seeing: sees,
        }));
        let body = json!({ "messages": convo.request(system), "tools": tools, "tool_choice": "auto" });
        let done = match client.chat(&route, &key, body) {
            Ok(done) => done,
            Err(LlmError::Transient { message, .. }) => {
                return TurnEnd::Failed(format!("{message} — gave up after {} attempts, try again in a moment", llm::MAX_ATTEMPTS));
            }
            Err(e) => return TurnEnd::Failed(e.to_string()),
        };
        let step = |rejected| FromWorker::Step(StepStat { ms: done.ms, model: done.model.clone(), attempts: done.attempts, rejected });
        let calls: Vec<Value> = done.message.get("tool_calls").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        if calls.is_empty() {
            let text = done.message.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            convo.push(json!({ "role": "assistant", "content": text }));
            let _ = tx.send(step(0));
            let _ = tx.send(FromWorker::Text(text));
            return TurnEnd::Answered;
        }
        convo.push(assistant_message(&done.message));

        // Each call is answered either here (it does not fit its tool) or
        // by the app (it ran).
        let mut results: Vec<Option<ToolOutput>> = Vec::with_capacity(calls.len());
        let mut batch: Vec<ToolCall> = Vec::new();
        let mut ids: Vec<(Value, &str)> = Vec::with_capacity(calls.len());
        for call in &calls {
            let name = call.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
            let raw = call.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("");
            ids.push((call.get("id").cloned().unwrap_or(Value::Null), name));
            match parse_tool_call(tools, name, raw) {
                Ok(args) => { batch.push(ToolCall { name: name.to_string(), args }); results.push(None); }
                Err(why) => results.push(Some(why.into())),
            }
        }
        let _ = tx.send(step(calls.len() - batch.len()));
        let (ran, stopped) = match if batch.is_empty() { Ok(Vec::new()) } else { run_batch(batch, rx, tx) } {
            Ok(ran) => (ran, None),
            Err(end) => (Vec::new(), Some(end)),
        };
        let mut ran = ran.into_iter();
        let results = results.into_iter().map(|rejected| rejected.or_else(|| ran.next())).collect();
        record_batch(convo, &ids, results);
        if let Some(end) = stopped { return end; }
    }
    TurnEnd::Failed(format!("stopped after {MAX_STEPS} model steps without a final answer — say \"continue\" to carry on"))
}

/// The model's tool-call message as it goes back into the conversation.
/// Reasoning text is dropped: DeepSeek rejects `reasoning_content` in input.
fn assistant_message(message: &Value) -> Value {
    let mut msg = message.clone();
    if let Some(o) = msg.as_object_mut() {
        o.remove("reasoning_content");
        o.remove("reasoning");
    }
    msg
}

#[cfg(test)]
mod tests {

    /// The picker never offers the Jev router next to DeepSeek: picking it
    /// looked like "turning Jev on" and made every chat step 2-8 s.
    #[test]
    fn picker_offers_the_jev_router_only_without_a_deepseek_key() {
        let both = picker_models(true, true, llm::DEEPSEEK_CHAT);
        assert!(!both.iter().any(|m| m == llm::JEV_ROUTER), "{both:?}");
        assert!(picker_models(true, false, llm::JEV_ROUTER).iter().any(|m| m == llm::JEV_ROUTER));
        // A configured router stays visible (and switchable away from).
        let cur = picker_models(true, true, llm::JEV_ROUTER);
        assert_eq!(cur[0], llm::JEV_ROUTER);
        assert!(cur.iter().any(|m| m == llm::DEEPSEEK_CHAT));
    }
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

    #[test]
    fn reduce_motion_persists_in_config_json_preserving_other_fields() {
        let dir = std::env::temp_dir().join(format!("ksalat-rm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, r#"{"assistant_model":"deepseek-chat"}"#).unwrap();

        assert!(!load_reduce_motion_from(&p));
        save_reduce_motion_to(&p, true).unwrap();
        assert!(load_reduce_motion_from(&p));
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["assistant_model"], json!("deepseek-chat"));
        assert_eq!(raw["ui_reduce_motion"], json!(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::llm::fake::{answer, status, text, FakeTransport};
    use crate::llm::Reply;

    fn calls(calls: &[(&str, &str, &str)]) -> Result<Reply, String> {
        let tool_calls: Vec<Value> = calls.iter()
            .map(|(id, name, args)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}))
            .collect();
        answer("router/picked", json!({"role": "assistant", "content": null, "tool_calls": tool_calls}))
    }

    /// Run the worker to the end of `inbox` on scripted model replies.
    /// Returns what it sent to the app and every request it made.
    fn drive(replies: Vec<Result<Reply, String>>, inbox: Vec<ToWorker>) -> (Vec<FromWorker>, Vec<Value>) {
        drive_with(replies, inbox, None, |_| Some("test-key".to_string()))
    }

    /// The same with Jev in front: the first reply of each turn is its decision.
    fn drive_with_jev(replies: Vec<Result<Reply, String>>, inbox: Vec<ToWorker>) -> (Vec<FromWorker>, Vec<Value>) {
        drive_with(replies, inbox, Some(Route::for_model(llm::JEV)), |_| Some("test-key".to_string()))
    }

    /// The model that acts while a turn carries an image.
    const SEEING: &str = "google/gemini-3.8-flash";

    fn drive_with(replies: Vec<Result<Reply, String>>, inbox: Vec<ToWorker>, decide: Option<Route>, key_for: fn(Endpoint) -> Option<String>) -> (Vec<FromWorker>, Vec<Value>) {
        let probe = FakeTransport::new(replies);
        let (tx_in, rx_in) = channel();
        let (tx_out, rx_out) = channel();
        for msg in inbox { tx_in.send(msg).unwrap(); }
        drop(tx_in);
        let routes = Routes {
            act: std::sync::Arc::new(std::sync::Mutex::new(Route::for_model(llm::JEV_ROUTER))),
            decide: std::sync::Arc::new(std::sync::Mutex::new(decide)),
            perceive: std::sync::Arc::new(std::sync::Mutex::new(Route::for_model(SEEING))),
        };
        worker(llm::Client::new(probe.clone()), routes, key_for, rx_in, tx_out);
        (rx_out.try_iter().collect(), probe.requests().into_iter().map(|(_, _, body)| body).collect())
    }

    fn user(text: &str) -> ToWorker { ToWorker::User(UserMessage { text: text.to_string(), image: None }) }

    fn results(r: &[&str]) -> ToWorker { ToWorker::ToolResults(r.iter().map(|s| s.to_string().into()).collect()) }

    fn image(name: &str) -> Image {
        Image { name: name.into(), format: crate::vision::Format::Png, bytes: vec![1, 2, 3].into(), size: Some([640, 400]) }
    }

    fn user_with_image(text: &str) -> ToWorker {
        ToWorker::User(UserMessage { text: text.to_string(), image: Some(image("note.png")) })
    }

    fn messages(request: &Value) -> &Vec<Value> { request["messages"].as_array().unwrap() }

    /// Every tool message must directly follow the assistant message (or a
    /// sibling tool message) whose tool_calls name its id.
    fn assert_no_orphans(msgs: &[Value]) {
        let mut open: Vec<String> = Vec::new();
        for m in msgs {
            match m["role"].as_str().unwrap() {
                "assistant" => {
                    assert!(open.is_empty(), "calls {open:?} were never answered");
                    open = m["tool_calls"].as_array().map(|c| c.iter().map(|c| c["id"].as_str().unwrap().to_string()).collect()).unwrap_or_default();
                }
                "tool" => {
                    let id = m["tool_call_id"].as_str().unwrap();
                    let at = open.iter().position(|o| o == id).unwrap_or_else(|| panic!("tool message {id} has no call before it"));
                    open.remove(at);
                }
                _ => assert!(open.is_empty(), "calls {open:?} were never answered"),
            }
        }
        assert!(open.is_empty(), "calls {open:?} were never answered");
    }

    #[test]
    fn a_model_step_sends_its_tool_calls_as_one_batch_and_takes_one_batch_back() {
        let (events, requests) = drive(
            vec![calls(&[("c1", "set_bpm", r#"{"bpm":100}"#), ("c2", "set_rig_mix", r#"{"rig":1,"vol":0.5}"#)]), text("router/picked", "Done.")],
            vec![user("faster, and rig 2 quieter"), results(&["tempo set to 100 BPM", "rig 1: vol 0.50"])],
        );
        let batches: Vec<&Vec<ToolCall>> = events.iter().filter_map(|e| match e { FromWorker::ToolCalls(b) => Some(b), _ => None }).collect();
        assert_eq!(batches.len(), 1, "one batch for the step");
        assert_eq!(*batches[0], vec![
            ToolCall { name: "set_bpm".into(), args: json!({"bpm": 100}) },
            ToolCall { name: "set_rig_mix".into(), args: json!({"rig": 1, "vol": 0.5}) },
        ]);
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Text(t), FromWorker::Done] if t == "Done."));
        // The second request carries each result under its call's id.
        let msgs = messages(&requests[1]);
        assert_no_orphans(msgs);
        let tool: Vec<(&str, &str)> = msgs.iter().filter(|m| m["role"] == "tool")
            .map(|m| (m["tool_call_id"].as_str().unwrap(), m["content"].as_str().unwrap())).collect();
        assert_eq!(tool, vec![("c1", "tempo set to 100 BPM"), ("c2", "rig 1: vol 0.50")]);
    }

    #[test]
    fn a_call_that_does_not_fit_its_tool_gets_an_error_the_model_can_correct_and_never_runs() {
        let (events, requests) = drive(
            vec![
                calls(&[
                    ("c1", "set_bpm", r#"{"bpm": 100"#),
                    ("c2", "set_tempo", r#"{"bpm":100}"#),
                    ("c3", "set_rig_mix", r#"{"rig":0,"volume":0.5}"#),
                    ("c4", "set_bpm", "{}"),
                    ("c5", "set_split", r#"{"point":"60"}"#),
                ]),
                text("m", "ok"),
            ],
            vec![user("go"), results(&["split at C4"])],
        );
        let batch = events.iter().find_map(|e| match e { FromWorker::ToolCalls(b) => Some(b.clone()), _ => None }).unwrap();
        assert_eq!(batch, vec![ToolCall { name: "set_split".into(), args: json!({"point": 60}) }], "only the valid call runs, \"60\" read as 60");
        let rejected = events.iter().find_map(|e| match e { FromWorker::Step(s) => Some(s.rejected), _ => None }).unwrap();
        assert_eq!(rejected, 4);
        let msgs = messages(&requests[1]);
        assert_no_orphans(msgs);
        let said: Vec<&str> = msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap()).collect();
        assert!(said[0].starts_with("error:") && said[0].contains("not valid JSON"), "{}", said[0]);
        assert!(said[1].contains("no tool 'set_tempo'") && said[1].contains("set_bpm"), "{}", said[1]);
        assert!(said[2].contains("no argument 'volume'") && said[2].contains("vol"), "{}", said[2]);
        assert!(said[3].contains("needs the argument 'bpm'"), "{}", said[3]);
        assert_eq!(said[4], "split at C4");
    }

    #[test]
    fn a_step_whose_calls_are_all_rejected_does_not_ask_the_app() {
        let (events, requests) = drive(vec![calls(&[("c1", "frobnicate", "{}")]), text("m", "sorry")], vec![user("go")]);
        assert!(!events.iter().any(|e| matches!(e, FromWorker::ToolCalls(_))));
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Text(_), FromWorker::Done]));
        assert_no_orphans(messages(&requests[1]));
    }

    #[test]
    fn arguments_are_read_as_meant_or_refused() {
        let tools = tools();
        let ok = |name: &str, raw: &str| parse_tool_call(&tools, name, raw).unwrap();
        assert_eq!(ok("get_graph", ""), json!({}), "no arguments may come as an empty string");
        assert_eq!(ok("set_rig_mix", r#"{"rig":1.0,"mute":"true","pan":"-0.5"}"#), json!({"rig": 1, "mute": true, "pan": -0.5}));
        assert_eq!(ok("add_node", r#"{"kind":"dsp","dsp":"reverb","rig":null}"#), json!({"kind": "dsp", "dsp": "reverb"}), "null = not given");
        assert_eq!(ok("delete_rig", r#"{"rig":1}"#), json!({"rig": 1}), "the Confirm flow starts without confirm");
        let err = |name: &str, raw: &str| parse_tool_call(&tools, name, raw).unwrap_err();
        assert!(err("set_rig_mix", r#"{"rig":"second"}"#).contains("must be integer"));
        assert!(err("set_rig_mix", r#"{"rig":1.5}"#).contains("must be integer"));
        assert!(err("connect", r#"{"from_node":1,"to_node":2,"kind":"usb"}"#).contains("must be one of"));
        assert!(err("set_bpm", "[100]").contains("one JSON object"));
    }

    /// The argument checker reads the tool table: a required argument that
    /// is not a property could never be given.
    #[test]
    fn the_tool_table_fits_the_argument_checker() {
        for tool in tools().as_array().unwrap() {
            let name = tool["function"]["name"].as_str().unwrap();
            let props = tool["function"]["parameters"]["properties"].as_object().unwrap_or_else(|| panic!("{name}: no properties object"));
            for (key, schema) in props {
                let types: Vec<String> = match &schema["type"] {
                    Value::String(t) => vec![t.clone()],
                    Value::Array(ts) => ts.iter().map(|t| t.as_str().unwrap().to_string()).collect(),
                    other => panic!("{name}.{key}: type {other}"),
                };
                assert!(types.iter().all(|t| ["integer", "number", "boolean", "string", "null"].contains(&t.as_str())), "{name}.{key}: {types:?}");
            }
            for required in tool["function"]["parameters"]["required"].as_array().into_iter().flatten() {
                assert!(props.contains_key(required.as_str().unwrap()), "{name}: required {required} is not a property");
            }
        }
    }

    fn says(text: &str) -> UserMessage {
        UserMessage { text: text.to_string(), image: None }
    }

    fn tool_step(id: &str, result: &str) -> Vec<Value> {
        vec![
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": "get_graph", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": id, "content": result}),
        ]
    }

    #[test]
    fn old_turns_keep_a_short_form_of_their_tool_results() {
        let mut c = Conversation::default();
        c.start_turn(&says("first"));
        for m in tool_step("c1", &"g".repeat(5_000)) { c.push(m); }
        c.push(json!({"role": "assistant", "content": "done"}));
        let result = |msgs: &[Value]| msgs.iter().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
        assert_eq!(result(&c.request("sys")).len(), 5_000, "the current turn keeps its results whole");
        c.start_turn(&says("second"));
        let msgs = c.request("sys");
        assert_no_orphans(&msgs);
        let old = result(&msgs);
        assert!(old.chars().count() < OLD_RESULT_CHARS + 80 && old.contains("shortened"), "{} chars", old.len());
        assert_eq!(msgs.last().unwrap()["content"], "second");
    }

    #[test]
    fn over_budget_the_oldest_turns_go_whole_and_the_current_turn_stays() {
        let mut c = Conversation::default();
        for i in 0..6 {
            c.start_turn(&says(&format!("request {i}")));
            for m in tool_step(&format!("c{i}"), "ok") { c.push(m); }
            c.push(json!({"role": "assistant", "content": "x".repeat(900)}));
        }
        c.fit(2_500);
        let msgs = c.request("sys");
        assert_no_orphans(&msgs);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user", "a kept turn starts with its user message");
        let users: Vec<&str> = msgs.iter().filter(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap()).collect();
        assert_eq!(users, vec!["request 4", "request 5"]);
        assert!(c.chars() <= 2_500);
    }

    #[test]
    fn a_turn_too_big_on_its_own_shortens_its_earlier_results_not_the_latest() {
        let mut c = Conversation::default();
        c.start_turn(&says("build it"));
        for i in 0..3 {
            for m in tool_step(&format!("c{i}"), &format!("{i}").repeat(4_000)) { c.push(m); }
        }
        c.fit(6_000);
        let msgs = c.request("sys");
        assert_no_orphans(&msgs);
        let tool: Vec<&str> = msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap()).collect();
        assert!(tool[0].contains("shortened") && tool[1].contains("shortened"));
        assert_eq!(tool[2].len(), 4_000, "the model still needs the results of its latest step");
    }

    #[test]
    fn a_long_conversation_stays_within_the_budget() {
        let turns = 12;
        let big = "n".repeat(20_000);
        let mut replies = Vec::new();
        let mut inbox = Vec::new();
        for i in 0..turns {
            replies.push(calls(&[(&format!("c{i}"), "get_graph", "{}")]));
            replies.push(text("m", &"a".repeat(6_000)));
            inbox.push(user(&format!("request {i}")));
            inbox.push(results(&[&big]));
        }
        let (_, requests) = drive(replies, inbox);
        assert_eq!(requests.len(), turns * 2);
        for r in &requests {
            let msgs = messages(r);
            assert_eq!(msgs[0]["role"], "system");
            assert_no_orphans(&msgs[1..]);
            let chars: usize = msgs[1..].iter().map(|m| m.to_string().len()).sum();
            assert!(chars <= CONTEXT_BUDGET, "{chars} chars sent");
        }
        let last = messages(requests.last().unwrap());
        assert!(!last.iter().any(|m| m["content"] == "request 0"), "the oldest turn is gone");
        assert!(last.iter().any(|m| m["content"] == "request 11"));
        assert_eq!(last.last().unwrap()["content"].as_str().unwrap().len(), 20_000, "the latest result is whole");
    }

    #[test]
    fn a_turn_stops_at_the_step_cap_and_says_so() {
        let replies = (0..MAX_STEPS).map(|i| calls(&[(&format!("c{i}"), "get_status", "{}")])).collect();
        let mut inbox = vec![user("loop")];
        inbox.extend((0..MAX_STEPS).map(|_| results(&["{}"])));
        let (events, requests) = drive(replies, inbox);
        assert_eq!(requests.len(), MAX_STEPS, "no request past the cap");
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Error(e), FromWorker::Done] if e.contains("24 model steps")), "{events:?}");
    }

    #[test]
    fn a_busy_server_is_retried_inside_the_step() {
        let (events, requests) = drive(vec![status(503, "{}"), text("m", "hello")], vec![user("hi")]);
        assert_eq!(requests.len(), 2);
        let step = events.iter().find_map(|e| match e { FromWorker::Step(s) => Some(s.clone()), _ => None }).unwrap();
        assert_eq!((step.attempts, step.model.as_str()), (2, "m"));
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Text(t), FromWorker::Done] if t == "hello"));
    }

    #[test]
    fn a_request_that_keeps_failing_ends_the_turn_and_the_next_turn_works() {
        let down = || Err("connection refused".to_string());
        let (events, requests) = drive(vec![down(), down(), down(), text("m", "back")], vec![user("one"), user("two")]);
        assert!(matches!(&events[..3], [FromWorker::Phase(_), FromWorker::Error(e), FromWorker::Done] if e.contains("connection refused") && e.contains("3 attempts")), "{events:?}");
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Text(t), FromWorker::Done] if t == "back"));
        let users: Vec<&str> = messages(&requests[3]).iter().filter(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap()).collect();
        assert_eq!(users, vec!["one", "two"], "the failed request stays in the conversation");
    }

    #[test]
    fn run_phase_labels_name_route_and_action() {
        assert_eq!(RunPhase::Deciding.label(), "Jev · deciding");
        assert_eq!(
            RunPhase::Thinking { step: 2, model: "deepseek-chat".into(), seeing: false }.label(),
            "DeepSeek · step 2"
        );
        assert_eq!(
            RunPhase::Thinking { step: 1, model: "google/gemini-3.8-flash".into(), seeing: true }.label(),
            "Gemini · looking at the image"
        );
    }

    #[test]
    fn a_missing_key_is_named_and_nothing_is_sent() {
        let (events, requests) = drive_with(vec![], vec![user("hi")], None, |_| None);
        assert!(requests.is_empty());
        assert!(matches!(&events[..], [FromWorker::Error(e), FromWorker::Done] if e.contains("OpenRouter") && e.contains("openrouter_api_key")), "{events:?}");
    }

    #[test]
    fn a_new_message_while_tool_calls_are_out_leaves_no_call_unanswered() {
        let (events, requests) = drive(
            vec![calls(&[("c1", "get_graph", "{}")]), text("m", "second answer")],
            vec![user("first"), user("second")],
        );
        let msgs = messages(&requests[1]);
        assert_no_orphans(msgs);
        assert!(msgs.iter().any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("did not run")));
        assert_eq!(msgs.last().unwrap()["content"], "second");
        assert!(matches!(&events[events.len() - 2..], [FromWorker::Text(t), FromWorker::Done] if t == "second answer"));
    }

    #[test]
    fn poll_keeps_transcript_and_cost_and_hands_out_tool_batches() {
        let (mut a, worker_tx, _worker_rx) = Assistant::with_test_worker();
        a.send_user_display("hi".into(), "[context] hi".into());
        assert!(a.busy);
        let step = |ms, model: &str| FromWorker::Step(StepStat { ms, model: model.into(), attempts: 1, rejected: 0 });
        worker_tx.send(step(700, "anthropic/claude-opus-5.5")).unwrap();
        worker_tx.send(FromWorker::ToolCalls(vec![ToolCall { name: "set_bpm".into(), args: json!({"bpm": 90}) }])).unwrap();
        let batches = a.poll();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0][0].name, "set_bpm");
        assert!(a.busy, "the turn is still running");
        worker_tx.send(step(500, "anthropic/claude-opus-5.5")).unwrap();
        worker_tx.send(FromWorker::Text("Tempo is 90.".into())).unwrap();
        worker_tx.send(FromWorker::Done).unwrap();
        assert!(a.poll().is_empty());
        assert!(!a.busy);
        assert_eq!(a.history.last().unwrap().text, "Tempo is 90.");
        assert_eq!(a.turn.tools, vec!["set_bpm"]);
        assert_eq!(a.turn.summary().as_deref(), Some("2 steps · 1.2 s · claude-opus-5.5"));
        assert!(a.turn.wall.is_some());
    }

    /// Jev's answer to the decision questions: the action and the rig, and
    /// a pick for each slot named in `slots`.
    fn decision_with(action: &str, p: f64, rig: (&str, f64), slots: &[(&str, &str)]) -> Result<Reply, String> {
        let mut answers = json!({
            "action": {"type": "choice", "choice": action, "probabilities": {action: p}},
            "rig": {"type": "choice", "choice": rig.0, "probabilities": {rig.0: rig.1}},
        });
        for (slot, pick) in slots {
            answers[*slot] = json!({"type": "choice", "choice": pick, "probabilities": {*pick: 0.99}});
        }
        status(200, &json!({"model": "typesafe/jev-1.14", "answers": answers}).to_string())
    }

    fn decision(action: &str, p: f64, rig: &str) -> Result<Reply, String> {
        decision_with(action, p, (rig, 0.97), &[])
    }

    fn asked(context_free: &str) -> ToWorker {
        user(&format!("[context: rig in view = 1 · Rig A (index 0), units: Keys (node 0) · all rigs: 1 · Rig A, 2 · Rig B]\n{context_free}"))
    }

    fn said(events: &[FromWorker]) -> Vec<&str> {
        events.iter().filter_map(|e| match e { FromWorker::Text(t) => Some(t.as_str()), _ => None }).collect()
    }

    fn ran(events: &[FromWorker]) -> Vec<ToolCall> {
        events.iter().filter_map(|e| match e { FromWorker::ToolCalls(b) => Some(b.clone()), _ => None }).flatten().collect()
    }

    #[test]
    fn jev_settles_a_simple_request_with_one_decision_and_no_chat_model() {
        let (events, requests) = drive_with_jev(
            vec![decision("volume", 0.99, "rig_2")],
            vec![asked("Set rig 2 volume to 50 %."), results(&["rig 1: vol 0.50 pan 0.00 mute false solo false"])],
        );
        assert_eq!(requests.len(), 1, "one request for the whole turn");
        assert!(requests[0]["questions"]["action"]["criteria"]["volume"].is_string(), "it is a decisions request");
        assert_eq!(requests[0]["model"], "~typesafe/jev-latest");
        assert_eq!(ran(&events), vec![ToolCall { name: "set_rig_mix".into(), args: json!({"rig": 1, "vol": 0.5}) }]);
        assert_eq!(said(&events), vec!["Rig 2 is at 50 %."]);
        let step = events.iter().find_map(|e| match e { FromWorker::Step(s) => Some(s.model.clone()), _ => None });
        assert_eq!(step.as_deref(), Some("typesafe/jev-1.14"));
        assert!(matches!(events.last(), Some(FromWorker::Done)));
    }

    #[test]
    fn what_jev_does_not_settle_goes_to_the_chat_model() {
        let chat = |replies: Vec<Result<Reply, String>>, request: &str| {
            let (events, requests) = drive_with_jev(replies, vec![asked(request)]);
            assert!(ran(&events).is_empty(), "{request}: nothing runs on a decision that settles nothing");
            assert_eq!(said(&events), vec!["from the chat model"], "{request}");
            assert!(requests.last().unwrap()["messages"].is_array(), "{request}: the last request is a chat completion");
        };
        let answer = || text("m", "from the chat model");
        chat(vec![decision("other", 0.99, "none"), answer()], "Put a filter between the synth and the delay.");
        chat(vec![decision("tempo", 0.6, "none"), answer()], "something about tempo 100");
        chat(vec![decision("volume", 0.99, "rig_2"), answer()], "rig 2 volume, please");
        chat(vec![decision("mute", 0.99, "none"), answer()], "mute it");
        chat(vec![decision("add_effect", 0.99, "rig_1"), answer()], "add a chorus to rig 1");
        chat(vec![decision_with("add_effect", 0.99, ("rig_2", 0.6), &[("effect", "reverb")]), answer()], "add a reverb to, hm, rig 2?");
        chat(vec![decision_with("load_sound", 0.99, ("none", 0.9), &[("sound", "none")]), answer()], "load a flute");
        chat(vec![status(401, "{}"), answer()], "Set the tempo to 100 BPM.");
        chat(vec![status(200, r#"{"answers":{"action":{"type":"refusal"}}}"#), answer()], "Set the tempo to 100 BPM.");
    }

    #[test]
    fn deleting_a_rig_through_jev_still_waits_for_the_users_confirm() {
        let app_says = "not deleted: ask the user first. The chat now shows Confirm / Cancel buttons for this";
        let (events, _) = drive_with_jev(vec![decision("delete_rig", 0.99, "rig_2")], vec![asked("Delete rig 2."), results(&[app_says])]);
        assert_eq!(ran(&events), vec![ToolCall { name: "delete_rig".into(), args: json!({"rig": 1}) }], "no confirm=true from the shortcut");
        assert!(said(&events)[0].contains("Confirm or cancel"));
    }

    #[test]
    fn the_chat_model_later_sees_what_jev_did() {
        let (_, requests) = drive_with_jev(
            vec![decision("tempo", 1.0, "none"), decision("other", 1.0, "none"), text("m", "It is 100.")],
            vec![asked("Set the tempo to 100 BPM."), results(&["tempo set to 100 BPM"]), asked("What tempo are we at?")],
        );
        let msgs = messages(&requests[2]);
        assert_no_orphans(&msgs[1..]);
        assert!(msgs.iter().any(|m| m["role"] == "tool" && m["content"] == "tempo set to 100 BPM"));
        assert!(msgs.iter().any(|m| m["role"] == "assistant" && m["content"] == "Tempo set to 100 BPM."));
    }

    const RIGS: &str = r#"{"rigs":[{"rig":0,"name":"Rig A","vol":1.0,"pan":0.0,"mute":false,"solo":false},{"rig":1,"name":"Rig B","vol":1.0,"pan":0.5,"mute":false,"solo":false}],"active":0}"#;

    #[test]
    fn a_bit_quieter_reads_the_level_and_sets_one_step_less() {
        let (events, requests) = drive_with_jev(
            vec![decision("quieter", 0.99, "rig_2")],
            vec![asked("Make rig 2 a bit quieter."), results(&[RIGS]), results(&["rig 1: vol 0.70 pan 0.50 mute false solo false"])],
        );
        assert_eq!(requests.len(), 1, "no chat model");
        assert_eq!(ran(&events), vec![call("get_rigs", json!({})), call("set_rig_mix", json!({"rig": 1, "vol": 0.7}))]);
        assert_eq!(said(&events), vec!["Rig 2 is now at 70 %."]);
    }

    #[test]
    fn a_nudge_that_has_nowhere_to_go_says_so_and_changes_nothing() {
        let (events, _) = drive_with_jev(vec![decision("louder", 0.99, "rig_1")], vec![asked("rig 1 a bit louder"), results(&[RIGS])]);
        assert_eq!(ran(&events), vec![call("get_rigs", json!({}))]);
        assert_eq!(said(&events), vec!["Rig 1 is already at 100 %."]);
        let (events, _) = drive_with_jev(
            vec![decision("pan_right", 0.99, "rig_2")],
            vec![asked("pan rig 2 right"), results(&[RIGS]), results(&["rig 1: vol 1.00 pan 1.00 mute false solo false"])],
        );
        assert_eq!(ran(&events)[1], call("set_rig_mix", json!({"rig": 1, "pan": 1.0})), "half a side further, capped");
        assert_eq!(said(&events), vec!["Rig 2 is now panned 100 % right."]);
    }

    /// Rig A of the app's two-rig demo: Keys → Surge → Delay → Master (left
    /// jack), with the synth also on the Master's right jack.
    fn rig_with_delay() -> (Graph, [NodeId; 3]) {
        use crate::control::{name_bytes, PortRef};
        let mut g = crate::control::standard_rig_graph(true);
        let synth = g.nodes.iter().find(|n| matches!(n.kind, NodeKind::Clap { .. })).unwrap().id;
        let master = rig_master(&g, 0).unwrap();
        let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
        g.edges.retain(|e| e.id != 2);
        g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: delay, port: 0 }, PortKind::Audio);
        g.connect_with_id(4, PortRef { node: delay, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        (g, [synth, delay, master])
    }

    fn as_json(g: &Graph) -> String {
        serde_json::to_string(g).unwrap()
    }

    #[test]
    fn an_effect_is_added_and_wired_in_front_of_the_master() {
        let g = crate::control::standard_rig_graph(true);
        let (synth, master) = (2, rig_master(&g, 0).unwrap());
        let (events, requests) = drive_with_jev(
            vec![decision_with("add_effect", 0.95, ("none", 0.6), &[("effect", "reverb")])],
            vec![
                asked("Add a reverb to this rig"),
                results(&[&as_json(&g)]),
                results(&["added dsp node to rig 0 'Rig A' (wire it with connect) [applied: new node id 3]"]),
                results(&["disconnected cable 2 [applied: removed cable 2]", "disconnected cable 3 [applied: removed cable 3]",
                    "connected 2 -> 3 (audio) [applied: new cable id 4]", "connected 3 -> 1 (audio) [applied: new cable id 5]",
                    "connected 3 -> 1 (audio) [applied: new cable id 6]"]),
            ],
        );
        assert_eq!(requests.len(), 1, "no chat model");
        assert_eq!(ran(&events), vec![
            call("get_graph", json!({})),
            call("add_node", json!({"kind": "dsp", "dsp": "reverb", "rig": 0})),
            call("disconnect", json!({"id": 2})),
            call("disconnect", json!({"id": 3})),
            call("connect", json!({"from_node": synth, "from_port": 0, "to_node": 3})),
            call("connect", json!({"from_node": 3, "to_node": master, "to_port": 0})),
            call("connect", json!({"from_node": 3, "to_node": master, "to_port": 1})),
        ]);
        let batches = events.iter().filter(|e| matches!(e, FromWorker::ToolCalls(_))).count();
        assert_eq!(batches, 3, "read, add, rewire");
        assert_eq!(said(&events), vec!["Reverb added to Rig 1, in front of Master."]);
    }

    #[test]
    fn a_plan_that_gets_stuck_hands_its_calls_and_results_to_the_chat_model() {
        let g = crate::control::standard_rig_graph(true);
        let not_applied = "added dsp node to rig 0 'Rig A' — NOT applied: the engine changed nothing (check the ids with get_graph)";
        let (events, requests) = drive_with_jev(
            vec![decision_with("add_effect", 0.95, ("rig_1", 0.97), &[("effect", "delay")]), text("m", "The delay could not be added.")],
            vec![asked("Add a delay to rig 1."), results(&[&as_json(&g)]), results(&[not_applied])],
        );
        assert_eq!(ran(&events).len(), 2, "the plan stops at the edit that did nothing");
        assert_eq!(said(&events), vec!["The delay could not be added."]);
        let msgs = messages(&requests[1]);
        assert_no_orphans(&msgs[1..]);
        assert!(msgs.iter().any(|m| m["role"] == "tool" && m["content"] == not_applied), "the chat model sees why");
    }

    #[test]
    fn an_effect_at_a_stated_place_in_a_chain_with_effects_is_left_to_the_chat_model() {
        let (g, _) = rig_with_delay();
        let decided = || decision_with("add_effect", 0.95, ("rig_1", 0.97), &[("effect", "filter")]);
        let (events, _) = drive_with_jev(
            vec![decided(), text("m", "Done.")],
            vec![asked("Add a filter to rig 1 before the delay."), results(&[&as_json(&g)])],
        );
        assert_eq!(ran(&events), vec![call("get_graph", json!({}))], "nothing is added");
        let (events, _) = drive_with_jev(
            vec![decided()],
            vec![asked("Add a filter to rig 1."), results(&[&as_json(&g)]), results(&["added [applied: new node id 9]"]), results(&["ok", "ok", "ok", "ok", "ok", "ok"])],
        );
        let sources: Vec<u64> = ran(&events).iter().filter(|c| c.name == "connect" && c.args["to_node"] == 9).map(|c| c.args["from_node"].as_u64().unwrap()).collect();
        assert_eq!(sources.len(), 2, "with no place stated, everything that fed the Master feeds the filter");
    }

    #[test]
    fn removing_an_effect_closes_the_gap_it_leaves() {
        let (g, [synth, delay, master]) = rig_with_delay();
        let (events, requests) = drive_with_jev(
            vec![decision_with("remove_effect", 0.95, ("rig_1", 0.97), &[("effect", "delay")])],
            vec![asked("Remove the delay from rig 1."), results(&[&as_json(&g)]), results(&["removed node 3 (and its cables) [applied: removed node 3]", "connected [applied: new cable id 5]"])],
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(ran(&events), vec![
            call("get_graph", json!({})),
            call("remove_node", json!({"node": delay})),
            call("connect", json!({"from_node": synth, "from_port": 0, "to_node": master, "to_port": 0})),
        ]);
        assert_eq!(said(&events), vec!["Delay removed from Rig 1."]);
    }

    #[test]
    fn which_of_two_delays_to_remove_is_the_chat_models_question() {
        let (events, _) = drive_with_jev(
            vec![decision_with("remove_effect", 0.95, ("none", 0.9), &[("effect", "delay")]), text("m", "Which one?")],
            vec![asked("Remove the delay."), results(&[&as_json(&Graph::demo_patch())])],
        );
        assert_eq!(ran(&events), vec![call("get_graph", json!({}))]);
        assert_eq!(said(&events), vec!["Which one?"]);
    }

    #[test]
    fn the_metronome_is_started_where_it_is_and_added_where_it_is_not() {
        use crate::control::{name_bytes, PortRef};
        let bare = crate::control::standard_rig_graph(true);
        let master = rig_master(&bare, 0).unwrap();
        let (events, _) = drive_with_jev(
            vec![decision("metronome", 0.95, "none")],
            vec![asked("Add a metronome and start it."), results(&[&as_json(&bare)]), results(&["added dsp node [applied: new node id 3]"]),
                results(&["connected 3 -> 1 (audio) [applied: new cable id 4]", "node 3 playing"])],
        );
        assert_eq!(ran(&events)[1..], [
            call("add_node", json!({"kind": "dsp", "dsp": "metronome", "rig": 0})),
            call("connect", json!({"from_node": 3, "to_node": master})),
            call("set_node_playing", json!({"node": 3, "playing": true})),
        ]);
        assert_eq!(said(&events), vec!["Metronome added to Rig 1 and running."]);

        let mut with_click = bare.clone();
        let click = with_click.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
        with_click.connect_with_id(4, PortRef { node: click, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let (events, _) = drive_with_jev(
            vec![decision("metronome", 0.95, "none")],
            vec![asked("Start the metronome."), results(&[&as_json(&with_click)]), results(&["node 3 playing"])],
        );
        assert_eq!(ran(&events)[1..], [call("set_node_playing", json!({"node": click, "playing": true}))], "no second metronome");
        assert_eq!(said(&events), vec!["The metronome is running."]);
    }

    #[test]
    fn a_sound_is_searched_by_the_users_words_then_by_its_kind() {
        let (events, requests) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("none", 0.9), &[("sound", "pad")])],
            vec![asked("Load a warm pad."), results(&[r#"{"instrument":"Surge","node":1,"rig":0,"library":"Surge patches","total":3008,"sounds":["Pads/Warm Pad"]}"#]),
                results(&["loaded 'Pads/Warm Pad' into Surge (node 1), rig 0 'Rig A'"])],
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(ran(&events), vec![call("list_sounds", json!({"query": "warm pad", "rig": 0})), call("load_sound", json!({"query": "warm pad", "rig": 0}))]);
        assert_eq!(said(&events), vec!["Rig 1 now plays Pads/Warm Pad."]);

        let miss = "no Surge patches of Surge match 'wobbly growling bass' (3008 in its library, e.g. Basses/Attacky): search with other words";
        let (events, _) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("rig_2", 0.99), &[("sound", "bass")])],
            vec![asked("Give rig 2 a wobbly growling bass."), results(&[miss]), results(&[r#"{"instrument":"Surge","node":4,"rig":1,"library":"Surge patches","total":3008,"sounds":["Basses/Attacky"]}"#]),
                results(&["loaded 'Basses/Attacky' into Surge (node 4), rig 1 'Rig B'"])],
        );
        let ran = ran(&events);
        let queries: Vec<&str> = ran.iter().map(|c| c.args["query"].as_str().unwrap()).collect();
        assert_eq!(queries, vec!["wobbly growling bass", "bass", "bass"], "the words, then the kind alone, which is loaded");
        assert!(ran.iter().all(|c| c.args["rig"] == 1), "the search and the load both name the rig: its instrument's library is searched");
        assert_eq!(said(&events), vec!["Rig 2 now plays Basses/Attacky."]);
    }

    #[test]
    fn a_sound_is_searched_in_the_library_of_the_rigs_own_instrument() {
        // Every setBfree programme is an organ and none says so: the kind
        // is one more search word, which the app's search leaves out.
        let (events, _) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("rig_2", 0.99), &[("sound", "organ")])],
            vec![asked("Give the organ rig a jazz drawbar preset."),
                results(&[r#"{"instrument":"Setbfree","node":4,"rig":1,"library":"MIDI programmes","total":110,"sounds":["Jazz 1 all"]}"#]),
                results(&["loaded 'Jazz 1 all' into Setbfree (node 4), rig 1 'Organ'"])],
        );
        assert_eq!(ran(&events), vec![
            call("list_sounds", json!({"query": "jazz drawbar organ", "rig": 1})),
            call("load_sound", json!({"query": "jazz drawbar organ", "rig": 1})),
        ]);
        assert_eq!(said(&events), vec!["Rig 2 now plays Jazz 1 all."]);

        // A request that names one sound by its label is not a kind of sound.
        let (events, requests) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("none", 0.9), &[("sound", "pad")]), text("m", "from the chat model")],
            vec![asked("Load the patch Pads/Dreamscape.")],
        );
        assert!(ran(&events).is_empty() && requests.len() == 2, "the chat model looks for that one patch");

        // A name with an apostrophe is said whole.
        let (events, _) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("none", 0.9), &[("sound", "organ")])],
            vec![asked("Load a solo organ."), results(&[r#"{"instrument":"Setbfree","node":1,"rig":0,"library":"MIDI programmes","total":110,"sounds":["8' solo"]}"#]),
                results(&["loaded '8' solo' into Setbfree (node 1), rig 0 'Rig A'"])],
        );
        assert_eq!(said(&events), vec!["Rig 1 now plays 8' solo."]);

        // An instrument without sounds to browse is the chat model's to explain.
        let nothing = "Lsp osc has no sounds to browse (it has no LV2 presets). It is shaped by its parameters (Frequency, Waveform): see list_params(node=1)";
        let (events, requests) = drive_with_jev(
            vec![decision_with("load_sound", 0.99, ("none", 0.9), &[("sound", "bass")]), text("m", "from the chat model")],
            vec![asked("Load a bass."), results(&[nothing])],
        );
        assert_eq!(ran(&events).len(), 1, "one search, no load");
        assert_eq!(requests.len(), 2, "the chat model takes over and sees the search's answer");
        let handed_over = requests[1]["messages"].as_array().unwrap();
        let planned = handed_over.iter().find(|m| m["tool_calls"][0]["id"] == "jev-0-0").expect("the plan's call is in the conversation");
        assert_eq!(planned["reasoning_content"], "", "DeepSeek's thinking models reject a foreign tool call without it");
        assert_eq!(handed_over.last().unwrap()["content"], nothing);
    }

    #[test]
    fn values_come_from_the_users_own_words() {
        let facts = |request| Facts { request, rig: None, view: None, picked: Vec::new() };
        assert_eq!(facts("Set the tempo to 100 BPM.").bpm(), Some(100.0));
        assert_eq!(facts("faster please").bpm(), None);
        assert_eq!(facts("Set rig 2 volume to 50 %.").percent(), Some(0.5));
        assert_eq!(facts("rig 2 at 75 percent").percent(), Some(0.75));
        assert_eq!(facts("set rig 2 volume to 50").percent(), None, "a bare number is not a level");
        assert_eq!(facts("Split the keyboard at C4.").note(), Some(60));
        assert_eq!(facts("split at f#3").note(), Some(54));
        assert_eq!(facts("split at Bb2").note(), Some(46));
        assert_eq!(facts("split at middle C").note(), Some(60));
        assert_eq!(facts("split the keyboard somewhere").note(), None);
        assert_eq!(facts("Load a warm pad into rig 2, please.").sound_words(), vec!["warm", "pad"]);
        assert_eq!(facts("give me a Rhodes on rig B").sound_words(), vec!["rhodes"]);
        assert_eq!(facts("Give the organ rig a jazz drawbar preset.").sound_words(), vec!["jazz", "drawbar"], "the word before 'rig' names the rig");
        assert_eq!(facts("Load a bass sound on the YANES rig.").sound_words(), vec!["bass"]);
        assert_eq!(facts("give this rig jazz chords").sound_words(), vec!["jazz", "chords"], "only a number or a letter after 'rig' names it");
        // The [context] line holds numbers too; only the request counts.
        let d = llm::Decisions { answers: [("action".to_string(), llm::Answer::Choice { choice: "tempo".into(), p: 1.0 })].into(), model: String::new(), ms: 0, attempts: 1 };
        assert!(settle(&d, "[context: rig in view = 1 · Rig A (index 0), units: Keys (node 100)]\nset the tempo").is_none());
    }

    /// What the app would answer, for a plan run without the app.
    fn canned(call: &ToolCall, graph: &str) -> String {
        match call.name.as_str() {
            "get_rigs" => RIGS.to_string(),
            "get_graph" => graph.to_string(),
            "add_node" => "added [applied: new node id 40]".to_string(),
            "list_sounds" => r#"{"instrument":"Surge","node":4,"rig":1,"library":"Surge patches","total":3008,"sounds":["Pads/Warm Pad"]}"#.to_string(),
            "load_sound" => "loaded 'Pads/Warm Pad' into Surge (node 4), rig 1 'Rig B'".to_string(),
            "set_node_playing" => "node 40 playing".to_string(),
            "set_rig_mix" => "rig 1: vol 0.50 pan 0.00 mute false solo false".to_string(),
            _ => "done [applied: new cable id 41]".to_string(),
        }
    }

    #[test]
    fn every_shortcut_runs_to_an_answer_on_tools_that_exist_with_arguments_they_take() {
        let tools = tools();
        let (rig_a, _) = rig_with_delay();
        let mut rig_b = crate::control::standard_rig_graph(true);
        for n in &mut rig_b.nodes { n.rig = 1; n.id += 10; }
        for e in &mut rig_b.edges { e.id += 10; e.from.node += 10; e.to.node += 10; }
        let mut graph = rig_a;
        graph.nodes.extend(rig_b.nodes);
        graph.edges.extend(rig_b.edges);
        let graph = as_json(&graph);
        for s in SHORTCUTS {
            // Rig 1 has the delay to remove; every other request names rig 2.
            let rig = if s.key == "remove_effect" { 0 } else { 1 };
            let facts = Facts { request: "a warm pad, 50 % at C4, 100 BPM", rig: Some(rig), view: None, picked: vec![("effect", "delay".into()), ("sound", "pad".into())] };
            let mut done: Vec<String> = Vec::new();
            let mut said = None;
            for _ in 0..=MAX_PLAN_BATCHES {
                match (s.plan)(&facts, &done).unwrap_or_else(|| panic!("{}: the plan gives up after {done:?}", s.key)) {
                    Step::Say(text) => { said = Some(text); break; }
                    Step::Run(calls) => for c in calls {
                        let checked = parse_tool_call(&tools, &c.name, &c.args.to_string()).unwrap_or_else(|e| panic!("{}: {e}", s.key));
                        assert_eq!(checked, c.args, "{}: {} takes the arguments as they are", s.key, c.name);
                        done.push(canned(&c, &graph));
                    },
                }
            }
            assert!(said.is_some_and(|text| !text.is_empty()), "{}: no answer within {MAX_PLAN_BATCHES} batches", s.key);
        }
    }

    #[test]
    fn a_turn_with_an_image_goes_to_the_seeing_model_and_skips_the_decision() {
        let (events, requests) = drive_with_jev(vec![text(SEEING, "The note says 90.")], vec![user_with_image("What does the note say?")]);
        assert_eq!(said(&events), vec!["The note says 90."]);
        assert_eq!(requests.len(), 1, "no decisions request");
        assert_eq!(requests[0]["model"], SEEING);
        let sent = messages(&requests[0]).last().unwrap();
        assert_eq!(sent["role"], "user");
        assert_eq!(sent["content"], json!([
            {"type": "text", "text": "What does the note say?\n[image: note.png (640×400 px)]"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQID"}},
        ]));
    }

    #[test]
    fn an_image_is_sent_only_during_its_own_turn() {
        let (_, requests) = drive(
            vec![text(SEEING, "It says 90."), text("m", "Yes.")],
            vec![user_with_image("What does the note say?"), user("Thanks. Still there?")],
        );
        assert_eq!(requests[1]["model"], llm::JEV_ROUTER, "a text turn is back on the acting model");
        let msgs = messages(&requests[1]);
        assert!(!requests[1].to_string().contains("image_url"), "no image in the next turn's request");
        let old = msgs.iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().expect("the old message is plain text again");
        assert!(old.starts_with("What does the note say?\n[image: note.png (640×400 px)] — no longer attached"), "{old}");
        assert!(msgs.iter().any(|m| m["content"] == "It says 90."), "what was seen stays as the answer");
    }

    #[test]
    fn an_image_a_tool_shows_follows_the_tool_results_and_moves_the_turn_to_the_seeing_model() {
        let shot = ToolOutput { text: "screenshot of the kabelsalat window: window (640×400 px)".into(), image: Some(image("window")) };
        let (events, requests) = drive(
            vec![calls(&[("c1", "look_at_rack", "{}"), ("c2", "get_status", "{}")]), text(SEEING, "The mixer is open.")],
            vec![user("What is on screen?"), ToWorker::ToolResults(vec![shot, "{}".to_string().into()])],
        );
        assert_eq!(said(&events), vec!["The mixer is open."]);
        assert_eq!(requests[0]["model"], llm::JEV_ROUTER, "the acting model asks to look");
        assert_eq!(requests[1]["model"], SEEING, "the seeing model reads the screenshot");
        let msgs = messages(&requests[1]);
        assert_no_orphans(&msgs[1..]);
        let roles: Vec<&str> = msgs[1..].iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "tool", "user"], "the image comes after every tool result");
        assert_eq!(msgs[3]["content"], "screenshot of the kabelsalat window: window (640×400 px)");
        assert_eq!(msgs[5]["content"][0]["text"], "[what look_at_rack shows]\n[image: window (640×400 px)]");
        assert_eq!(msgs[5]["content"][1]["type"], "image_url");
    }

    #[test]
    fn an_image_counts_as_a_page_of_text_not_as_its_megabytes() {
        let mut c = Conversation::default();
        c.start_turn(&says("earlier"));
        c.push(json!({"role": "assistant", "content": "ok"}));
        let big = Image { bytes: vec![7; 300_000].into(), ..image("big.png") };
        c.start_turn(&UserMessage { text: "look".into(), image: Some(big) });
        assert!(c.request("sys").last().unwrap().to_string().len() > CONTEXT_BUDGET, "the base64 alone is past the budget");
        c.fit(CONTEXT_BUDGET);
        assert_eq!(c.turns.len(), 2, "the earlier turn stays");
        assert!(c.has_image());
        assert!(c.chars() < 2 * IMAGE_CHARS);
    }

    #[test]
    fn an_image_without_a_key_for_the_seeing_model_is_named_and_nothing_is_sent() {
        let (events, requests) = drive_with(vec![], vec![user_with_image("what is this?")], None, |_| None);
        assert!(requests.is_empty());
        assert!(matches!(&events[..], [FromWorker::Error(e), FromWorker::Done] if e.contains("an image needs google/gemini-3.8-flash") && e.contains("openrouter_api_key")), "{events:?}");
    }

    #[test]
    fn remove_config_field_removes_only_target_and_preserves_mode() {
        let dir = std::env::temp_dir().join(format!("ksalat-rm-field-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        std::fs::write(&p, r#"{"deepseek_api_key":"sk-test","agent_act_model":"deepseek-chat"}"#).unwrap();

        remove_config_field_from(&p, "deepseek_api_key").unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(raw.get("deepseek_api_key").is_none());
        assert_eq!(raw["agent_act_model"], json!("deepseek-chat"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_model_choices_and_role_switching_update_routes() {
        let keys = llm::Keys { openrouter: true, deepseek: true };
        let act_choices = role_model_choices(Role::Act, keys, llm::DEEPSEEK_CHAT);
        assert!(act_choices.iter().any(|m| m == llm::DEEPSEEK_CHAT));
        assert!(act_choices.iter().any(|m| m == "deepseek-reasoner"));
        assert!(!act_choices.iter().any(|m| m == llm::JEV_ROUTER), "no Jev router beside DeepSeek: {act_choices:?}");
        assert!(role_model_choices(Role::Act, llm::Keys { openrouter: true, deepseek: false }, llm::JEV_ROUTER).iter().any(|m| m == llm::JEV_ROUTER));
        assert!(act_choices.iter().any(|m| m == llm::CLAUDE_OPUS));
        assert!(act_choices.iter().any(|m| m == llm::PERCEIVE_MODEL));

        let mut a = Assistant::new();
        a.set_role_model(Role::Act, llm::CLAUDE_OPUS);
        assert_eq!(a.route_for(Role::Act), Route { endpoint: Endpoint::OpenRouter, model: llm::CLAUDE_OPUS.into() });

        a.set_role_model(Role::Perceive, llm::GEMINI_FLASH_25);
        assert_eq!(a.route_for(Role::Perceive), Route { endpoint: Endpoint::OpenRouter, model: llm::GEMINI_FLASH_25.into() });

        a.set_decide_enabled(false);
        assert!(!a.decide_enabled());
        assert!(!a.decides_first());
        a.set_decide_enabled(true);
        assert!(a.decide_enabled());
        assert!(a.decides_first());
        assert_eq!(a.route_for(Role::Decide), Route { endpoint: Endpoint::OpenRouter, model: llm::JEV.into() });

        a.save_api_key(Endpoint::OpenRouter, "or-fake-for-test").unwrap();
        assert!(a.keys().openrouter);
        assert!(a.key_set);
        a.remove_api_key(Endpoint::OpenRouter).unwrap();
        assert!(!a.keys().openrouter);
        assert!(!a.key_set);
    }
}
