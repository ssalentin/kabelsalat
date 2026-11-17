//! One LLM client for the agent: where a request goes (endpoint + model per
//! role) and how it survives a flaky network (classified errors, retries).
//!
//! Keys live in ~/.config/kabelsalat/config.json and are only ever sent to
//! the endpoint they belong to.

use std::time::Duration;

use serde_json::Value;

/// An OpenAI-compatible chat-completions service.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endpoint {
    OpenRouter,
    DeepSeek,
}

pub struct EndpointSpec {
    pub label: &'static str,
    /// Chat completions live at `{base_url}/chat/completions`.
    pub base_url: &'static str,
    /// Where decisions models (typed questions in, probabilities out) are
    /// asked; None when the endpoint has none.
    pub decisions_url: Option<&'static str>,
    /// The config.json field that holds this endpoint's API key.
    pub key_field: &'static str,
}

impl Endpoint {
    pub fn spec(self) -> EndpointSpec {
        match self {
            Endpoint::OpenRouter => EndpointSpec {
                label: "OpenRouter",
                base_url: "https://openrouter.ai/api/v1",
                decisions_url: Some("https://openrouter.ai/api/alpha/decisions"),
                key_field: "openrouter_api_key",
            },
            Endpoint::DeepSeek => EndpointSpec {
                label: "DeepSeek",
                base_url: "https://api.deepseek.com",
                decisions_url: None,
                key_field: "deepseek_api_key",
            },
        }
    }

    /// This endpoint's key from config.json (None in tests, rule 37).
    pub fn key(self) -> Option<String> {
        crate::assistant::load_config_str(self.spec().key_field)
    }
}

/// What a model is asked to do. Each role has its own route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Decide: a decisions model (Jev) answers typed questions about the
    /// request in one fast call — it writes no text and no tool calls.
    Decide,
    /// Act: a chat model reads the request and calls tools, for everything
    /// the decision alone does not settle.
    Act,
    /// Look and listen: a multimodal model that describes the rack or the
    /// master signal for the acting model.
    Perceive,
}

impl Role {
    /// The config.json field that overrides this role's model.
    pub fn model_field(self) -> &'static str {
        match self {
            Role::Decide => "agent_decide_model",
            Role::Act => "agent_act_model",
            Role::Perceive => "agent_perceive_model",
        }
    }
}

/// Where a role's requests go.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Route {
    pub endpoint: Endpoint,
    pub model: String,
}

/// OpenRouter's alias for the newest Jev, TypeSafe's decisions model.
pub const JEV: &str = "~typesafe/jev-latest";
/// A chat-model router that runs on Jev: the acting model for users who
/// have only an OpenRouter key.
pub const JEV_ROUTER: &str = "typesafe/jev-router";
pub const DEEPSEEK_CHAT: &str = "deepseek-chat";
const PERCEIVE_MODEL: &str = "google/gemini-3.8-flash";

impl Route {
    /// The endpoint follows the id's shape: OpenRouter ids are
    /// "vendor/model", DeepSeek's own ids have no slash.
    pub fn for_model(model: &str) -> Route {
        let endpoint = if model.contains('/') { Endpoint::OpenRouter } else { Endpoint::DeepSeek };
        Route { endpoint, model: model.to_string() }
    }

    /// A role's route: the configured model, else the default. Decisions go
    /// to Jev. Acting stays on DeepSeek's chat model when its key exists
    /// (measured faster per step than the Jev router's picks) and uses the
    /// router when OpenRouter's key is the only one.
    pub fn resolve(role: Role, model_override: Option<&str>, keys: Keys) -> Route {
        if let Some(model) = model_override {
            return Route::for_model(model);
        }
        match role {
            Role::Decide => Route::for_model(JEV),
            Role::Act if keys.openrouter && !keys.deepseek => Route::for_model(JEV_ROUTER),
            Role::Act => Route::for_model(DEEPSEEK_CHAT),
            Role::Perceive => Route::for_model(PERCEIVE_MODEL),
        }
    }

    /// The route config.json asks for.
    pub fn configured(role: Role) -> Route {
        let model = crate::assistant::load_config_str(role.model_field());
        let keys = Keys { openrouter: Endpoint::OpenRouter.key().is_some(), deepseek: Endpoint::DeepSeek.key().is_some() };
        Route::resolve(role, model.as_deref(), keys)
    }
}

/// Which endpoints config.json has a key for.
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    pub openrouter: bool,
    pub deepseek: bool,
}

/// Why a request failed, by what the caller can do about it.
#[derive(Clone, PartialEq, Debug)]
pub enum LlmError {
    /// Timeout, connection error, 429 or 5xx: the same request may work in
    /// a moment (after `retry_after` when the server named one).
    Transient { message: String, retry_after: Option<Duration> },
    /// The key is missing or was refused.
    Auth(String),
    /// The request or the response is wrong; sending it again will not help.
    Request(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Transient { message, .. } => write!(f, "{message}"),
            LlmError::Auth(m) | LlmError::Request(m) => write!(f, "{m}"),
        }
    }
}

/// An HTTP answer as the transport saw it.
pub struct Reply {
    pub status: u16,
    /// The Retry-After header, in seconds.
    pub retry_after: Option<u64>,
    pub body: String,
}

/// The wire under the client. Tests run on a fake; nothing in a test opens
/// a socket or sleeps.
pub trait Transport {
    /// POST a JSON body. Err = the request never got an HTTP answer
    /// (timeout, connection refused, DNS).
    fn post(&mut self, url: &str, key: &str, body: &Value) -> Result<Reply, String>;

    fn sleep(&mut self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// The longest one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HttpTransport(reqwest::blocking::Client);

impl HttpTransport {
    pub fn new() -> Result<Self, String> {
        reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map(HttpTransport)
            .map_err(|e| format!("http client: {e}"))
    }
}

impl Transport for HttpTransport {
    fn post(&mut self, url: &str, key: &str, body: &Value) -> Result<Reply, String> {
        let resp = self.0.post(url).bearer_auth(key).json(body).send().map_err(|e| {
            if e.is_timeout() { format!("no answer within {} s", REQUEST_TIMEOUT.as_secs()) } else { format!("request failed: {e}") }
        })?;
        let status = resp.status().as_u16();
        let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let body = resp.text().map_err(|e| format!("reading the answer failed: {e}"))?;
        Ok(Reply { status, retry_after, body })
    }
}

/// One finished chat completion.
#[derive(Debug)]
pub struct Completion {
    /// The assistant message (content and/or tool_calls).
    pub message: Value,
    /// The model that answered; a router names the one it picked.
    pub model: String,
    /// Wall time including retries and their waits.
    pub ms: u64,
    pub attempts: u32,
}

/// One typed answer of a decisions model.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// Yes/no: the probability of yes.
    Noul(f64),
    /// The option picked among the question's criteria, with its probability.
    Choice { choice: String, p: f64 },
    /// The expected position on the question's ordered scale.
    Score(f64),
    /// The model declined the question.
    Refused,
}

/// The answers to one decisions request, by question key.
#[derive(Debug)]
pub struct Decisions {
    pub answers: std::collections::HashMap<String, Answer>,
    pub model: String,
    pub ms: u64,
    pub attempts: u32,
}

impl Decisions {
    /// The option picked for `question` and its probability.
    pub fn choice(&self, question: &str) -> Option<(&str, f64)> {
        match self.answers.get(question)? {
            Answer::Choice { choice, p } => Some((choice.as_str(), *p)),
            _ => None,
        }
    }
}

fn parse_answer(answer: &Value) -> Answer {
    let number = |key: &str| answer.get(key).and_then(|v| v.as_f64());
    match answer.get("type").and_then(|t| t.as_str()) {
        Some("noul") => number("noul").map_or(Answer::Refused, Answer::Noul),
        Some("score") => number("score").map_or(Answer::Refused, Answer::Score),
        Some("choice") => match answer.get("choice").and_then(|c| c.as_str()) {
            Some(choice) => {
                let p = answer.pointer(&format!("/probabilities/{}", choice.replace('~', "~0").replace('/', "~1")))
                    .and_then(|p| p.as_f64()).unwrap_or(0.0);
                Answer::Choice { choice: choice.to_string(), p }
            }
            None => Answer::Refused,
        },
        _ => Answer::Refused,
    }
}

pub const MAX_ATTEMPTS: u32 = 3;
/// A server may ask for a longer pause than a user will sit through.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(20);

/// The wait before attempt `next` (2 or 3): the server's Retry-After when it
/// named one, else 0.5 s doubling.
fn retry_wait(next: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_millis(500 << (next - 2)))
        .min(MAX_RETRY_WAIT)
}

/// The JSON body of a successful answer, or why there is none.
fn classify(label: &str, reply: Result<Reply, String>) -> Result<Value, LlmError> {
    let reply = reply.map_err(|message| LlmError::Transient { message, retry_after: None })?;
    let body: Option<Value> = serde_json::from_str(&reply.body).ok();
    let api_message = body.as_ref()
        .and_then(|b| b.pointer("/error/message"))
        .and_then(|m| m.as_str())
        .unwrap_or("no error message")
        .to_string();
    // OpenRouter reports an upstream failure as 200 with an error body.
    let code = match reply.status {
        200..=299 => body.as_ref().and_then(|b| b.pointer("/error/code")).and_then(|c| c.as_u64()).map_or(200, |c| c as u16),
        status => status,
    };
    match code {
        200..=299 => body.ok_or_else(|| LlmError::Request(format!("{label} sent an answer that is not JSON"))),
        401 | 403 => Err(LlmError::Auth(format!("{label} refused the API key ({code}): {api_message}"))),
        408 | 429 | 500..=599 => Err(LlmError::Transient {
            message: format!("{label} is busy ({code}): {api_message}"),
            retry_after: reply.retry_after.map(Duration::from_secs),
        }),
        _ => Err(LlmError::Request(format!("{label} rejected the request ({code}): {api_message}"))),
    }
}

pub struct Client<T: Transport> {
    transport: T,
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// POST `body` and return the answer's JSON with the attempts it took.
    /// Transient failures are retried up to `MAX_ATTEMPTS` times; the last
    /// error is returned.
    fn post(&mut self, label: &str, url: &str, key: &str, body: &Value) -> Result<(Value, u32), LlmError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match classify(label, self.transport.post(url, key, body)) {
                Ok(answer) => return Ok((answer, attempts)),
                Err(LlmError::Transient { retry_after, .. }) if attempts < MAX_ATTEMPTS => {
                    self.transport.sleep(retry_wait(attempts + 1, retry_after));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One chat completion on `route`. `body` carries messages and tools;
    /// the route's model is filled in.
    pub fn chat(&mut self, route: &Route, key: &str, mut body: Value) -> Result<Completion, LlmError> {
        let spec = route.endpoint.spec();
        body["model"] = Value::String(route.model.clone());
        let started = std::time::Instant::now();
        let (answer, attempts) = self.post(spec.label, &format!("{}/chat/completions", spec.base_url), key, &body)?;
        let message = answer.pointer("/choices/0/message").cloned()
            .ok_or_else(|| LlmError::Request(format!("{} sent an answer without a message", spec.label)))?;
        Ok(Completion { message, model: model_of(&answer), ms: started.elapsed().as_millis() as u64, attempts })
    }

    /// Ask a decisions model typed `questions` (noul, choice, score) about
    /// `state`: one request, one probability-backed answer per question.
    pub fn decide(&mut self, route: &Route, key: &str, state: Value, questions: Value) -> Result<Decisions, LlmError> {
        let spec = route.endpoint.spec();
        let url = spec.decisions_url
            .ok_or_else(|| LlmError::Request(format!("{} has no decisions endpoint (model {})", spec.label, route.model)))?;
        let body = serde_json::json!({ "model": route.model, "state": state, "questions": questions });
        let started = std::time::Instant::now();
        let (answer, attempts) = self.post(spec.label, url, key, &body)?;
        let answers = answer.get("answers").and_then(|a| a.as_object())
            .ok_or_else(|| LlmError::Request(format!("{} sent a decision without answers", spec.label)))?
            .iter().map(|(key, a)| (key.clone(), parse_answer(a))).collect();
        Ok(Decisions { answers, model: model_of(&answer), ms: started.elapsed().as_millis() as u64, attempts })
    }
}

fn model_of(answer: &Value) -> String {
    answer.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string()
}

/// A scripted transport for tests: answers in order, records what it saw.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Clones share one script and one record, so a test keeps a probe
    /// while the client owns the transport.
    #[derive(Clone, Default)]
    pub struct FakeTransport {
        replies: Arc<Mutex<std::collections::VecDeque<Result<Reply, String>>>>,
        requests: Arc<Mutex<Vec<(String, String, Value)>>>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    impl FakeTransport {
        pub fn new(replies: Vec<Result<Reply, String>>) -> Self {
            Self { replies: Arc::new(Mutex::new(replies.into())), ..Self::default() }
        }

        /// (url, key, body) of every request, in order.
        pub fn requests(&self) -> Vec<(String, String, Value)> {
            self.requests.lock().unwrap().clone()
        }

        pub fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl Transport for FakeTransport {
        fn post(&mut self, url: &str, key: &str, body: &Value) -> Result<Reply, String> {
            self.requests.lock().unwrap().push((url.to_string(), key.to_string(), body.clone()));
            self.replies.lock().unwrap().pop_front().expect("the test scripted too few replies")
        }

        fn sleep(&mut self, wait: Duration) {
            self.sleeps.lock().unwrap().push(wait);
        }
    }

    pub fn status(status: u16, body: &str) -> Result<Reply, String> {
        Ok(Reply { status, retry_after: None, body: body.to_string() })
    }

    /// A 200 whose assistant message is `message`, answered by `model`.
    pub fn answer(model: &str, message: Value) -> Result<Reply, String> {
        status(200, &serde_json::json!({"model": model, "choices": [{"message": message}]}).to_string())
    }

    pub fn text(model: &str, content: &str) -> Result<Reply, String> {
        answer(model, serde_json::json!({"role": "assistant", "content": content}))
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use serde_json::json;

    fn chat(replies: Vec<Result<Reply, String>>) -> (Result<Completion, LlmError>, FakeTransport) {
        let probe = FakeTransport::new(replies);
        let out = Client::new(probe.clone()).chat(&Route::for_model(JEV_ROUTER), "k", json!({"messages": []}));
        (out, probe)
    }

    const BOTH: Keys = Keys { openrouter: true, deepseek: true };
    const ONLY_OPENROUTER: Keys = Keys { openrouter: true, deepseek: false };
    const NONE: Keys = Keys { openrouter: false, deepseek: false };

    #[test]
    fn each_role_has_its_default_route() {
        assert_eq!(Route::resolve(Role::Decide, None, BOTH), Route { endpoint: Endpoint::OpenRouter, model: "~typesafe/jev-latest".into() });
        assert_eq!(Route::resolve(Role::Act, None, BOTH), Route { endpoint: Endpoint::DeepSeek, model: DEEPSEEK_CHAT.into() });
        assert_eq!(Route::resolve(Role::Act, None, ONLY_OPENROUTER), Route { endpoint: Endpoint::OpenRouter, model: JEV_ROUTER.into() });
        assert_eq!(Route::resolve(Role::Act, None, NONE).endpoint, Endpoint::DeepSeek, "the key box asks for a DeepSeek key");
        assert_eq!(Route::resolve(Role::Perceive, None, BOTH), Route { endpoint: Endpoint::OpenRouter, model: "google/gemini-3.8-flash".into() });
    }

    #[test]
    fn jev_answers_typed_questions_on_the_decisions_endpoint() {
        // The response shape of OpenRouter's API reference (trimmed).
        let body = r#"{"answers":{
            "is_bug":{"noul":0.96,"type":"noul"},
            "team":{"choice":"payments","confidence":0.75,"probabilities":{"account":0,"frontend":0.16,"payments":0.84},"type":"choice"},
            "urgency":{"confidence":0.99,"probabilities":{"0":0,"1":0.01,"2":0.99},"score":1.99,"type":"score"}},
            "id":"gen-dec-1","model":"typesafe/jev-1.13-20260917","provider":"TypeSafe","usage":{"cost":0.00002,"input_tokens":476,"output_tokens":70}}"#;
        let probe = FakeTransport::new(vec![status(503, "{}"), status(200, body)]);
        let out = Client::new(probe.clone())
            .decide(&Route::for_model(JEV), "k", json!("My checkout page is blank."), json!({"is_bug": {"type": "noul", "instructions": "A defect?"}}))
            .unwrap();
        assert_eq!(out.answers["is_bug"], Answer::Noul(0.96));
        assert_eq!(out.choice("team"), Some(("payments", 0.84)));
        assert_eq!(out.answers["urgency"], Answer::Score(1.99));
        assert_eq!((out.model.as_str(), out.attempts), ("typesafe/jev-1.13-20260917", 2), "a busy server is retried");
        let (url, _, sent) = &probe.requests()[1];
        assert_eq!(url, "https://openrouter.ai/api/alpha/decisions");
        assert_eq!(sent["model"], "~typesafe/jev-latest");
        assert_eq!(sent["state"], "My checkout page is blank.");
        assert_eq!(sent["questions"]["is_bug"]["type"], "noul");
    }

    #[test]
    fn a_decision_needs_an_endpoint_that_has_decisions() {
        let probe = FakeTransport::new(vec![]);
        let out = Client::new(probe.clone()).decide(&Route::for_model(DEEPSEEK_CHAT), "k", json!("x"), json!({}));
        assert!(matches!(out, Err(LlmError::Request(m)) if m.contains("no decisions endpoint")));
        assert!(probe.requests().is_empty());
    }

    #[test]
    fn a_configured_model_overrides_the_default_and_picks_its_endpoint() {
        assert_eq!(Route::resolve(Role::Act, Some("deepseek-reasoner"), ONLY_OPENROUTER).endpoint, Endpoint::DeepSeek);
        let r = Route::resolve(Role::Perceive, Some("anthropic/claude-opus-5.5"), NONE);
        assert_eq!((r.endpoint, r.model.as_str()), (Endpoint::OpenRouter, "anthropic/claude-opus-5.5"));
    }

    #[test]
    fn a_request_goes_to_the_routes_endpoint_with_its_model_and_key() {
        let (out, t) = chat(vec![text("anthropic/claude-opus-5.5", "hi")]);
        let out = out.unwrap();
        assert_eq!(out.model, "anthropic/claude-opus-5.5", "the router's pick is reported");
        assert_eq!(out.message["content"], "hi");
        assert_eq!(out.attempts, 1);
        let (url, key, body) = &t.requests()[0];
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(key, "k");
        assert_eq!(body["model"], JEV_ROUTER);
    }

    #[test]
    fn transient_failures_are_retried_with_backoff_until_one_works() {
        let (out, t) = chat(vec![Err("connection reset".into()), status(503, "{}"), text("m", "ok")]);
        assert_eq!(out.unwrap().attempts, 3);
        assert_eq!(t.requests().len(), 3);
        assert_eq!(t.sleeps(), vec![Duration::from_millis(500), Duration::from_millis(1000)]);
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_the_server_asks() {
        let limited = Ok(Reply { status: 429, retry_after: Some(3), body: r#"{"error":{"message":"slow down"}}"#.into() });
        let (out, t) = chat(vec![limited, text("m", "ok")]);
        assert!(out.is_ok());
        assert_eq!(t.sleeps(), vec![Duration::from_secs(3)]);
    }

    #[test]
    fn three_transient_failures_give_up_with_the_last_error() {
        let (out, t) = chat(vec![status(500, "{}"), status(502, "{}"), Err("timed out".into())]);
        assert_eq!(out.unwrap_err(), LlmError::Transient { message: "timed out".into(), retry_after: None });
        assert_eq!(t.requests().len(), MAX_ATTEMPTS as usize, "no fourth attempt");
    }

    #[test]
    fn a_refused_key_and_a_bad_request_are_not_retried() {
        let (out, t) = chat(vec![status(401, r#"{"error":{"message":"No auth credentials found"}}"#)]);
        assert!(matches!(out, Err(LlmError::Auth(m)) if m.contains("No auth credentials found")));
        assert_eq!(t.requests().len(), 1);
        let (out, t) = chat(vec![status(400, r#"{"error":{"message":"tool_call_id missing"}}"#)]);
        assert!(matches!(out, Err(LlmError::Request(m)) if m.contains("tool_call_id missing")));
        assert_eq!(t.requests().len(), 1);
    }

    #[test]
    fn an_upstream_error_inside_a_200_is_retried() {
        let upstream = status(200, r#"{"error":{"code":502,"message":"provider returned error"}}"#);
        let (out, t) = chat(vec![upstream, text("m", "ok")]);
        assert_eq!(out.unwrap().attempts, 2);
        assert_eq!(t.requests().len(), 2);
    }

    #[test]
    fn an_answer_without_a_message_is_a_request_error() {
        let (out, _) = chat(vec![status(200, r#"{"choices":[]}"#)]);
        assert!(matches!(out, Err(LlmError::Request(_))));
    }
}
