//! The audio judge (GitHub #35): a listening model hears a short clip of the
//! master and says in text how well it fits the words of a request.
//!
//! The engine pushes the final master output into a [`Tap`] while a
//! [`Capture`] is armed; nothing is pushed otherwise. The clip goes to an
//! audio-capable chat model on OpenRouter together with what the app measures
//! itself (loudness, peak, bands, low-end mono check). The model answers with
//! a 0–10 score per attribute and one or two sentences of what it heard. That
//! answer is text: the acting model and Jev decide from it, never from the
//! audio (rule 82).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rtrb::{Consumer, Producer, RingBuffer};
use serde_json::{json, Value};

use crate::llm::{self, LlmError, Route, Transport};
use crate::meter::{MasterMeter, MeterSnapshot};

/// The longest clip a listen may take.
pub const MAX_SECS: f32 = 8.0;
pub const DEFAULT_SECS: f32 = 4.0;
/// The config.json field that overrides the listening model.
pub const MODEL_FIELD: &str = "agent_listen_model";
/// The default listening model: its OpenRouter entry lists `audio` in
/// `architecture.input_modalities`.
pub const DEFAULT_MODEL: &str = llm::GEMINI_FLASH;

/// Interleaved audio, mono or stereo.
#[derive(Clone, Debug)]
pub struct Clip {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Clip {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// The clip as the model is sent it: mono at `rate`, low-passed below
    /// the new Nyquist first. Gemini hears audio as 16 kHz mono whatever it
    /// is sent, so a full-rate stereo upload only costs time; stereo and
    /// highs are covered by the measured features.
    pub fn mono_at(&self, rate: u32) -> Clip {
        let ch = self.channels.max(1) as usize;
        let mut lp = Lowpass::at(rate as f32 * 0.45, self.sample_rate);
        let mono: Vec<f32> = self.samples.chunks_exact(ch).map(|f| lp.run(f.iter().sum::<f32>() / ch as f32)).collect();
        let step = self.sample_rate as f64 / rate as f64;
        let n = (mono.len() as f64 / step) as usize;
        let samples = (0..n)
            .map(|i| {
                let x = i as f64 * step;
                let j = x as usize;
                let f = (x - j as f64) as f32;
                let a = mono[j];
                let b = *mono.get(j + 1).unwrap_or(&a);
                a + (b - a) * f
            })
            .collect();
        Clip { sample_rate: rate, channels: 1, samples }
    }

    pub fn secs(&self) -> f32 {
        self.frames() as f32 / self.sample_rate.max(1) as f32
    }

    /// 16-bit PCM WAV, the format every audio model takes.
    pub fn wav(&self) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&(self.sample_rate * 2 * self.channels as u32).to_le_bytes());
        out.extend_from_slice(&(2 * self.channels).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in &self.samples {
            out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
        out
    }
}

/// What the app measures on a clip itself.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub secs: f32,
    pub meter: MeterSnapshot,
    /// Highest sample of the clip, dBFS.
    pub peak_db: f32,
}

impl Features {
    pub fn of(clip: &Clip) -> Self {
        let mut meter = MasterMeter::new(clip.sample_rate as f64);
        for block in clip.samples.chunks(1024) {
            meter.process(block, clip.channels as usize);
        }
        let peak = clip.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        Self { secs: clip.secs(), meter: meter.snapshot(), peak_db: 20.0 * peak.max(1e-6).log10() }
    }

    pub fn silent(&self) -> bool {
        self.peak_db < -60.0
    }

    pub fn describe(&self) -> String {
        let m = &self.meter;
        let lufs = if m.integrated_lufs.is_finite() { format!("{:.1} LUFS", m.integrated_lufs) } else { "below -70 LUFS".into() };
        format!(
            "{:.1} s, {lufs}, peak {:.1} dBFS, bands <120 Hz {:.0} dB, 120-500 Hz {:.0} dB, 500-4k Hz {:.0} dB, >4k Hz {:.0} dB, low end {}",
            self.secs, self.peak_db, m.band_db[0], m.band_db[1], m.band_db[2], m.band_db[3],
            if m.low_end_mono { "mono-safe" } else { "out of phase" },
        )
    }
}

/// What a listen asks about.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The words of the user's request the sound should match ("8-bit",
    /// "heroic", …), each scored 0–10.
    pub attributes: Vec<String>,
    /// A free question to answer besides the scores.
    pub question: Option<String>,
    /// Leave the measured features out of the prompt (listen-test checks
    /// that the model hears the audio, not the numbers).
    pub audio_only: bool,
    pub wire: Wire,
}

/// How a clip travels to the model. The defaults are the fastest settings
/// that still pass `listen-test`; its flags compare others.
#[derive(Clone, Debug)]
pub struct Wire {
    /// Mono sample rate of the upload; None sends the clip as recorded.
    pub rate: Option<u32>,
    /// OpenRouter `reasoning.effort` ("minimal", "low", …); None leaves the
    /// model's default.
    pub reasoning: Option<String>,
}

impl Default for Wire {
    fn default() -> Self {
        Self { rate: Some(16_000), reasoning: Some("low".into()) }
    }
}

/// The listening model's answer.
#[derive(Clone, Debug)]
pub struct Verdict {
    /// Per requested attribute, in request order; None when the model gave
    /// no number for it.
    pub scores: Vec<(String, Option<f32>)>,
    pub heard: String,
    pub model: String,
    pub ms: u64,
}

impl Verdict {
    pub fn score(&self, attribute: &str) -> Option<f32> {
        self.scores.iter().find(|(a, _)| a == attribute).and_then(|(_, s)| *s)
    }
}

const SYSTEM: &str = "You are the listening judge of a software synthesizer rack. You hear a short recording of the \
rack's master output. Judge only what you hear. Answer in exactly this form and nothing else:\n\
SCORES\n<attribute>: <0-10>\n(one line per attribute, in the order given)\n\
HEARD: <one or two sentences: instrument character, timbre, brightness, envelope, space, and what would move the \
sound toward the attributes>\n\
0 means the sound has nothing of the attribute, 10 means it is the attribute's textbook example.";

/// The chat request for one clip.
pub fn body(clip: &Clip, request: &Request) -> Value {
    let mut text = String::new();
    if request.attributes.is_empty() {
        text.push_str("Attributes: none; write SCORES with no lines.\n");
    } else {
        text.push_str(&format!("Attributes: {}\n", request.attributes.join(", ")));
    }
    if !request.audio_only {
        text.push_str(&format!("Measured by the app: {}\n", Features::of(clip).describe()));
    }
    if let Some(q) = request.question.as_deref().filter(|q| !q.trim().is_empty()) {
        text.push_str(&format!("Also answer in HEARD: {q}\n"));
    }
    let sent = match request.wire.rate {
        Some(rate) => clip.mono_at(rate),
        None => clip.clone(),
    };
    let audio = base64::engine::general_purpose::STANDARD.encode(sent.wav());
    let mut body = json!({
        "messages": [
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": [
                { "type": "text", "text": text },
                { "type": "input_audio", "input_audio": { "data": audio, "format": "wav" } },
            ]},
        ],
        "temperature": 0,
        "max_tokens": 4000,
    });
    if let Some(effort) = &request.wire.reasoning {
        body["reasoning"] = json!({ "effort": effort });
    }
    body
}

/// Read the model's answer. Attribute lines are matched by name, ignoring
/// case; a score outside 0–10 is clamped.
pub fn parse(content: &str, request: &Request) -> (Vec<(String, Option<f32>)>, String) {
    let mut found: Vec<(String, f32)> = Vec::new();
    let mut heard = String::new();
    for line in content.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']);
        if let Some(rest) = line.strip_prefix("HEARD:").or_else(|| line.strip_prefix("Heard:")) {
            heard = rest.trim().to_string();
            continue;
        }
        if !heard.is_empty() {
            heard.push(' ');
            heard.push_str(line);
            continue;
        }
        if let Some((name, value)) = line.rsplit_once(':') {
            let number: String = value.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if let Ok(v) = number.parse::<f32>() {
                found.push((name.trim().trim_matches(['*', '"']).to_lowercase(), v.clamp(0.0, 10.0)));
            }
        }
    }
    let scores = request
        .attributes
        .iter()
        .map(|a| (a.clone(), found.iter().find(|(n, _)| *n == a.to_lowercase()).map(|(_, v)| *v)))
        .collect();
    (scores, heard.trim().to_string())
}

/// Ask the listening model about `clip`.
pub fn judge<T: Transport>(client: &mut llm::Client<T>, route: &Route, key: &str, clip: &Clip, request: &Request) -> Result<Verdict, LlmError> {
    let done = client.chat(route, key, body(clip, request))?;
    let content = done.message.get("content").and_then(Value::as_str).unwrap_or("");
    let (scores, heard) = parse(content, request);
    if heard.is_empty() && scores.iter().all(|(_, s)| s.is_none()) {
        return Err(LlmError::Request(format!("{} answered without scores or HEARD: {content:.200}", route.model)));
    }
    Ok(Verdict { scores, heard, model: if done.model.is_empty() { route.model.clone() } else { done.model }, ms: done.ms })
}

/// The listening model config.json asks for, else the default.
pub fn route() -> Route {
    Route::for_model(&crate::assistant::load_config_str(MODEL_FIELD).unwrap_or_else(|| DEFAULT_MODEL.to_string()))
}

// ---------------------------------------------------------------------------
// The master tap
// ---------------------------------------------------------------------------

/// Engine side: pushes the final output while a capture is armed.
pub struct Tap {
    producer: Producer<f32>,
    armed: Arc<AtomicBool>,
}

impl Tap {
    /// RT-safe: no allocation, no lock; a frame that does not fit is dropped.
    pub fn push_stereo(&mut self, data: &[f32], ch: usize) {
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        let ch = ch.max(1);
        for frame in data.chunks_exact(ch) {
            if self.producer.slots() < 2 {
                return;
            }
            let _ = self.producer.push(frame[0]);
            let _ = self.producer.push(if ch > 1 { frame[1] } else { frame[0] });
        }
    }
}

/// UI/agent side: records the next seconds of the master.
pub struct Capture {
    consumer: Mutex<Consumer<f32>>,
    armed: Arc<AtomicBool>,
    sample_rate: u32,
}

pub fn tap(sample_rate: u32) -> (Tap, Arc<Capture>) {
    let capacity = (sample_rate as f32 * (MAX_SECS + 1.0)) as usize * 2;
    let (producer, consumer) = RingBuffer::new(capacity);
    let armed = Arc::new(AtomicBool::new(false));
    (Tap { producer, armed: armed.clone() }, Arc::new(Capture { consumer: Mutex::new(consumer), armed, sample_rate }))
}

impl Capture {
    /// Record `secs` of the master from now. Blocks the calling thread (never
    /// the UI thread) for about that long.
    pub fn record(&self, secs: f32) -> Result<Clip, String> {
        let secs = secs.clamp(0.5, MAX_SECS);
        let mut consumer = self.consumer.try_lock().map_err(|_| "another listen is still recording".to_string())?;
        while consumer.pop().is_ok() {}
        let want = (secs * self.sample_rate as f32) as usize * 2;
        let mut samples = Vec::with_capacity(want);
        let deadline = Instant::now() + Duration::from_secs_f32(secs + 2.0);
        self.armed.store(true, Ordering::Relaxed);
        while samples.len() < want && Instant::now() < deadline {
            let n = consumer.slots().min(want - samples.len());
            if n == 0 {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if let Ok(chunk) = consumer.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                samples.extend_from_slice(a);
                samples.extend_from_slice(b);
                chunk.commit_all();
            }
        }
        self.armed.store(false, Ordering::Relaxed);
        samples.truncate(samples.len() & !1);
        if samples.len() < want / 2 {
            return Err(format!("the engine rendered only {:.1} of {secs:.1} s; is audio output running?", samples.len() as f32 / 2.0 / self.sample_rate as f32));
        }
        Ok(Clip { sample_rate: self.sample_rate, channels: 2, samples })
    }
}

/// The master's capture, installed by whoever runs the engine (the app or
/// headless MCP). One process renders one master.
static CAPTURE: Mutex<Option<Arc<Capture>>> = Mutex::new(None);

pub fn install(capture: Arc<Capture>) {
    if let Ok(mut c) = CAPTURE.lock() {
        *c = Some(capture);
    }
}

/// Parse a listen tool call's arguments (agent and MCP alike).
pub fn request_of(args: &Value) -> (f32, Request) {
    let secs = args.get("seconds").and_then(Value::as_f64).map_or(DEFAULT_SECS, |s| s as f32).clamp(0.5, MAX_SECS);
    let words: Vec<&str> = match args.get("attributes") {
        Some(Value::String(s)) => s.split(',').collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let attributes = words.into_iter().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    let question = args.get("question").and_then(Value::as_str).map(str::to_string);
    (secs, Request { attributes, question, ..Request::default() })
}

/// The `listen` tool: record the master, measure it, ask the listening model.
/// Blocking; run it off the UI thread. The answer is the text the agent reads.
pub fn listen(args: &Value) -> String {
    let (secs, request) = request_of(args);
    let route = route();
    let Some(key) = route.endpoint.key() else {
        return format!(
            "listen: no listening model: {} needs a {} key in config.json ({}). Use measure_mix for loudness, peak and bands.",
            route.model, route.endpoint.spec().label, route.endpoint.spec().key_field
        );
    };
    let capture = CAPTURE.lock().ok().and_then(|c| c.clone());
    let Some(capture) = capture else { return "listen: this process renders no master to listen to".into() };
    let clip = match capture.record(secs) {
        Ok(c) => c,
        Err(e) => return format!("listen: {e}"),
    };
    let features = Features::of(&clip);
    if features.silent() {
        return format!("listen: the master was silent for {secs:.1} s ({}). Play something first (play_note) or check the routing.", features.describe());
    }
    let mut client = match llm::HttpTransport::new() {
        Ok(t) => llm::Client::new(t),
        Err(e) => return format!("listen: {e}"),
    };
    match judge(&mut client, &route, &key, &clip, &request) {
        Ok(v) => report(&v, &features),
        Err(e) => format!("listen: {} failed: {e}. measure_mix still works.", route.model),
    }
}

/// The text the agent reads.
pub fn report(v: &Verdict, features: &Features) -> String {
    let scores: Vec<String> = v
        .scores
        .iter()
        .map(|(a, s)| match s {
            Some(s) => format!("{a} {s:.0}"),
            None => format!("{a} ?"),
        })
        .collect();
    let mut out = format!("listened to {:.1} s of the master ({}, {:.1} s)\n", features.secs, v.model, v.ms as f32 / 1000.0);
    if !scores.is_empty() {
        out.push_str(&format!("scores 0-10: {}\n", scores.join(", ")));
    }
    out.push_str(&format!("heard: {}\nmeasured: {}", v.heard, features.describe()));
    out
}

// ---------------------------------------------------------------------------
// listen-test: can the model tell known clips apart?
// ---------------------------------------------------------------------------

const SR: u32 = 44_100;

fn midi_hz(note: f32) -> f32 {
    440.0 * 2f32.powf((note - 69.0) / 12.0)
}

fn render(secs: f32, mut f: impl FnMut(f32) -> f32) -> Clip {
    let n = (secs * SR as f32) as usize;
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = f(i as f32 / SR as f32);
        samples.push(s);
        samples.push(s);
    }
    Clip { sample_rate: SR, channels: 2, samples }
}

/// A band-limited (PolyBLEP) saw: a naive saw aliases into the buzzy,
/// digital sound the test uses as its 8-bit reference.
fn saw(t: f32, hz: f32) -> f32 {
    let p = (t * hz).fract();
    let dt = hz / SR as f32;
    let blep = if p < dt {
        let x = p / dt;
        x + x - x * x - 1.0
    } else if p > 1.0 - dt {
        let x = (p - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    };
    2.0 * p - 1.0 - blep
}

/// Attack/release gate of a note that lasts `len` seconds.
fn gate(t: f32, len: f32, attack: f32, release: f32) -> f32 {
    if t < 0.0 || t > len + release {
        return 0.0;
    }
    let a = (t / attack.max(1e-4)).min(1.0);
    let r = if t > len { 1.0 - (t - len) / release.max(1e-4) } else { 1.0 };
    a * r
}

/// A four-pole low-pass, run sample by sample.
struct Lowpass {
    g: f32,
    z: [f32; 4],
}

impl Lowpass {
    fn at(cutoff: f32, sample_rate: u32) -> Self {
        Self { g: 1.0 - (-2.0 * std::f32::consts::PI * cutoff / sample_rate as f32).exp(), z: [0.0; 4] }
    }

    fn new(cutoff: f32) -> Self {
        Self::at(cutoff, SR)
    }

    fn run(&mut self, mut x: f32) -> f32 {
        for z in &mut self.z {
            *z += self.g * (x - *z);
            x = *z;
        }
        x
    }
}

/// Square-wave arpeggio at a 4-bit, 11 kHz resolution.
fn chiptune() -> Clip {
    let notes = [72.0, 76.0, 79.0, 84.0, 79.0, 76.0, 74.0, 77.0, 81.0, 86.0, 81.0, 77.0];
    let step = 0.1;
    let mut held = 0.0;
    let mut i = 0usize;
    render(4.0, |t| {
        i += 1;
        if i % 4 == 1 {
            let k = (t / step) as usize;
            let note = notes[k % notes.len()];
            let phase = (t * midi_hz(note)).fract();
            let bass = (t * midi_hz(48.0 + 7.0 * ((k / 6) % 2) as f32)).fract();
            let x = 0.3 * if phase < 0.25 { 1.0 } else { -1.0 } * gate(t % step, step * 0.7, 0.002, 0.01)
                + 0.25 * if bass < 0.5 { 1.0 } else { -1.0 };
            held = (x * 8.0).round() / 8.0;
        }
        held
    })
}

/// A note of a reference phrase: start and length in seconds, MIDI note.
type Phrase = &'static [(f32, f32, u8)];

/// A rising brass call, then a held major chord.
const FANFARE: Phrase = &[
    (0.0, 0.16, 67), (0.2, 0.16, 67), (0.4, 0.16, 67), (0.6, 0.5, 72), (1.2, 0.16, 67), (1.4, 0.5, 72), (2.0, 0.3, 76),
    (2.4, 1.2, 48), (2.4, 1.2, 60), (2.4, 1.2, 64), (2.4, 1.2, 67), (2.4, 1.2, 72),
];
/// A held Cmaj7.
const PAD_CHORD: Phrase = &[(0.0, 3.2, 48), (0.0, 3.2, 55), (0.0, 3.2, 59), (0.0, 3.2, 64)];

/// `phrase` played on a Surge XT factory patch through the real engine, and
/// recorded through the listen tap as the app records it.
fn surge_clip(category: &str, name: &str, phrase: Phrase, secs: f32) -> anyhow::Result<Clip> {
    use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
    use crate::matrix::SourceId;
    const RATE: f64 = 48_000.0;
    const BLOCK: usize = 480;
    let patch = crate::host::list_surge_patches()
        .into_iter()
        .find(|p| p.category == category && p.name == name)
        .ok_or_else(|| anyhow::anyhow!("Surge XT patch {category}/{name} is not installed"))?;
    let (voice, mut main) = crate::host::ClapVoice::new(RATE)?;
    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = crate::engine::GraphEngine::new(g, drain, RATE, vec![(synth, voice)]);
    let (tap, capture) = tap(RATE as u32);
    engine.set_listen_tap(tap);
    let mut buf = [0.0f32; BLOCK * 2];
    main.load_state(&crate::host::fxp_to_state(&std::fs::read(&patch.path)?)?)?;
    for _ in 0..50 {
        engine.render(&mut buf, 2);
    }
    let recording = std::thread::spawn({
        let capture = capture.clone();
        move || capture.record(secs)
    });
    while !capture.armed.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut events: Vec<(usize, u8, bool)> = phrase
        .iter()
        .flat_map(|&(start, len, note)| [((start as f64 * RATE) as usize, note, true), (((start + len) as f64 * RATE) as usize, note, false)])
        .collect();
    events.sort_by_key(|&(at, _, on)| (at, on));
    let mut frame = 0;
    while !recording.is_finished() {
        for &(_, note, on) in events.iter().filter(|(at, _, _)| (frame..frame + BLOCK).contains(at)) {
            bus.send(if on { Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys, channel: 0 } } else { Command::NoteOff { note, source: SourceId::Keys, channel: 0 } });
        }
        engine.render(&mut buf, 2);
        frame += BLOCK;
        if frame % (BLOCK * 20) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    recording.join().map_err(|_| anyhow::anyhow!("capture thread panicked"))?.map_err(anyhow::Error::msg)
}

/// The same saw riff through a closed or open low-pass.
fn saw_riff(cutoff: f32) -> Clip {
    let notes = [57.0, 60.0, 64.0, 62.0];
    let mut lp = Lowpass::new(cutoff);
    render(4.0, move |t| {
        let k = (t / 0.5) as usize;
        let x = saw(t, midi_hz(notes[k % notes.len()])) * gate(t % 0.5, 0.4, 0.01, 0.08) * 0.5;
        lp.run(x)
    })
}

/// (attribute, the clip that should score higher, the one that should score lower)
const EXPECT: &[(&str, &str, &str)] = &[
    ("8-bit", "chiptune", "pad"),
    ("8-bit", "chiptune", "fanfare"),
    ("8-bit", "chiptune", "saw bright"),
    ("soft", "pad", "chiptune"),
    ("soft", "pad", "fanfare"),
    ("soft", "saw dark", "saw bright"),
    ("heroic", "fanfare", "pad"),
    ("heroic", "fanfare", "saw dark"),
    ("bright", "saw bright", "saw dark"),
];

/// The clips listen-test judges. The pad and the fanfare are real Surge XT
/// patches: synthetic ones were heard as chiptune by every model.
fn reference_clips() -> anyhow::Result<Vec<(&'static str, Clip)>> {
    Ok(vec![
        ("chiptune", chiptune()),
        ("pad", surge_clip("Pads", "MKS-70 Warm Pad", PAD_CHORD, 4.0)?),
        ("fanfare", surge_clip("Brass", "JX-10 Double Brass", FANFARE, 4.0)?),
        ("saw dark", saw_riff(500.0)),
        ("saw bright", saw_riff(9000.0)),
    ])
}

/// `kabelsalat listen-test [--model <id>] [--runs N] [--audio-only]
/// [--reasoning <effort|default>] [--rate <Hz|native>] [--save <dir>]`:
/// Phase 0 of the audio judge. Renders reference clips whose differences are
/// known, asks the listening model to score each on the same attributes, and
/// checks that it ranks every expected pair the right way round. Prints the
/// latency of each call, since the agent waits for it.
pub fn run_test() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let route = value("--model").map_or_else(route, |m| Route::for_model(&m));
    let runs: usize = value("--runs").and_then(|r| r.parse().ok()).unwrap_or(2).max(1);
    let mut wire = Wire::default();
    if let Some(r) = value("--reasoning") {
        wire.reasoning = (r != "default").then_some(r);
    }
    if let Some(r) = value("--rate") {
        wire.rate = if r == "native" { None } else { Some(r.parse()?) };
    }
    let request = Request {
        attributes: ["8-bit", "soft", "heroic", "bright"].map(String::from).to_vec(),
        question: None,
        audio_only: args.iter().any(|a| a == "--audio-only"),
        wire,
    };
    let key = route.endpoint.key().ok_or_else(|| anyhow::anyhow!("no {} key in config.json", route.endpoint.spec().label))?;
    let clips = reference_clips()?;
    if let Some(dir) = value("--save") {
        std::fs::create_dir_all(&dir)?;
        for (name, clip) in &clips {
            std::fs::write(format!("{dir}/{}.wav", name.replace(' ', "-")), clip.wav())?;
        }
    }
    let upload_kb = body(&clips[0].1, &request).to_string().len() / 1000;
    println!(
        "listen-test: {} on {}, {runs} run(s), {}, upload {}, reasoning {}, {upload_kb} KB per request",
        route.model,
        route.endpoint.spec().label,
        if request.audio_only { "audio only" } else { "audio + measured features" },
        request.wire.rate.map_or("as recorded".into(), |r| format!("mono {r} Hz")),
        request.wire.reasoning.as_deref().unwrap_or("default"),
    );
    let mut verdicts: Vec<Vec<Verdict>> = vec![Vec::new(); clips.len()];
    for run in 0..runs {
        let answers: Vec<Result<Verdict, String>> = std::thread::scope(|s| {
            let calls: Vec<_> = clips
                .iter()
                .map(|(name, clip)| {
                    let (route, key, request) = (&route, &key, &request);
                    s.spawn(move || {
                        let mut client = llm::Client::new(llm::HttpTransport::new()?);
                        judge(&mut client, route, key, clip, request).map_err(|e| format!("{name}: {e}"))
                    })
                })
                .collect();
            calls.into_iter().map(|c| c.join().unwrap_or_else(|_| Err("judge thread panicked".into()))).collect()
        });
        for (i, answer) in answers.into_iter().enumerate() {
            let v = answer.map_err(anyhow::Error::msg)?;
            let line: Vec<String> = v.scores.iter().map(|(a, s)| format!("{a} {}", s.map_or("?".into(), |s| format!("{s:.0}")))).collect();
            println!("  run {} {:<10} {:>5} ms  {:<36} {}", run + 1, clips[i].0, v.ms, line.join(", "), v.heard);
            verdicts[i].push(v);
        }
    }
    let mut ms: Vec<u64> = verdicts.iter().flatten().map(|v| v.ms).collect();
    ms.sort_unstable();
    let index = |name: &str| clips.iter().position(|(n, _)| *n == name).expect("clip");
    let mut held_total = 0;
    let mut failed = 0;
    for (attribute, high, low) in EXPECT {
        let held = (0..runs)
            .filter(|&r| {
                let s = |c: &str| verdicts[index(c)][r].score(attribute);
                matches!((s(high), s(low)), (Some(h), Some(l)) if h > l)
            })
            .count();
        held_total += held;
        let ok = held * 2 > runs;
        if !ok {
            failed += 1;
        }
        println!("  {} {attribute:<7} {high} > {low}: {held}/{runs}", if ok { "ok  " } else { "FAIL" });
    }
    println!(
        "listen-test: {held_total}/{} pair rankings held; latency median {} ms, max {} ms",
        EXPECT.len() * runs,
        ms[ms.len() / 2],
        ms[ms.len() - 1],
    );
    anyhow::ensure!(failed == 0, "listen-test: {failed} of {} expected rankings did not hold", EXPECT.len());
    println!("listen-test: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{self, FakeTransport};

    fn request(attrs: &[&str]) -> Request {
        Request { attributes: attrs.iter().map(|s| s.to_string()).collect(), ..Request::default() }
    }

    #[test]
    fn a_wav_holds_every_sample_with_a_valid_header() {
        let clip = Clip { sample_rate: 48_000, channels: 2, samples: vec![0.0, 0.5, -0.5, 1.0, 2.0, -2.0] };
        let wav = clip.wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, wav.len() - 8);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 12);
        let samples: Vec<i16> = wav[44..].chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(samples, vec![0, 16384, -16384, 32767, 32767, -32767]);
    }

    #[test]
    fn the_request_carries_the_clip_the_attributes_and_the_measurements() {
        let clip = saw_riff(9000.0);
        let b = body(&clip, &request(&["8-bit", "heroic"]));
        let parts = b.pointer("/messages/1/content").and_then(Value::as_array).unwrap();
        let text = parts[0]["text"].as_str().unwrap();
        assert!(text.contains("Attributes: 8-bit, heroic"), "{text}");
        assert!(text.contains("LUFS") && text.contains("peak"), "{text}");
        assert_eq!(parts[1]["input_audio"]["format"], "wav");
        let wav = base64::engine::general_purpose::STANDARD.decode(parts[1]["input_audio"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(wav, clip.mono_at(16_000).wav());
        assert_eq!(b["reasoning"]["effort"], "low");
        let audio_only = body(&clip, &Request { audio_only: true, ..request(&["soft"]) });
        assert!(!audio_only.pointer("/messages/1/content/0/text").unwrap().as_str().unwrap().contains("LUFS"));
    }

    #[test]
    fn the_upload_is_mono_at_the_wire_rate_and_keeps_the_tone() {
        let clip = render(1.0, |t| (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5);
        let mono = clip.mono_at(16_000);
        assert_eq!((mono.channels, mono.sample_rate, mono.frames()), (1, 16_000, 16_000));
        let crossings = mono.samples.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((439..=441).contains(&crossings), "{crossings} upward zero crossings for 440 Hz");
        let peak = mono.samples[4000..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.45..0.52).contains(&peak), "peak {peak}");
        assert!(mono.wav().len() * 5 < clip.wav().len());
    }

    #[test]
    fn scores_are_read_by_attribute_name_and_heard_keeps_its_lines() {
        let content = "SCORES\n8-Bit: 9\n- heroic: 2/10\nsoft: 14\nHEARD: A square-wave arpeggio,\nvery dry.";
        let (scores, heard) = parse(content, &request(&["8-bit", "heroic", "soft", "layered"]));
        assert_eq!(scores, vec![
            ("8-bit".into(), Some(9.0)),
            ("heroic".into(), Some(2.0)),
            ("soft".into(), Some(10.0)),
            ("layered".into(), None),
        ]);
        assert_eq!(heard, "A square-wave arpeggio, very dry.");
    }

    #[test]
    fn judge_sends_to_the_listening_route_and_reports_text() {
        let mut client = llm::Client::new(FakeTransport::new(vec![fake::text("google/gemini-3.8-flash", "SCORES\nbright: 8\nHEARD: An open saw riff.")]));
        let route = Route::for_model(DEFAULT_MODEL);
        let clip = saw_riff(9000.0);
        let v = judge(&mut client, &route, "k", &clip, &request(&["bright"])).unwrap();
        assert_eq!(v.score("bright"), Some(8.0));
        let text = report(&v, &Features::of(&clip));
        assert!(text.starts_with("listened to 4.0 s"), "{text}");
        assert!(text.contains("scores 0-10: bright 8") && text.contains("heard: An open saw riff.") && text.contains("measured: 4.0 s"), "{text}");
    }

    #[test]
    fn an_answer_with_neither_scores_nor_heard_is_an_error() {
        let mut client = llm::Client::new(FakeTransport::new(vec![fake::text("m", "I cannot hear audio.")]));
        let err = judge(&mut client, &Route::for_model(DEFAULT_MODEL), "k", &chiptune(), &request(&["soft"])).unwrap_err();
        assert!(matches!(err, LlmError::Request(_)), "{err:?}");
    }

    #[test]
    fn a_capture_records_what_the_tap_pushes_only_while_armed() {
        let (mut tap, capture) = tap(1000);
        tap.push_stereo(&[9.0; 200], 2);
        let c = capture.clone();
        let rec = std::thread::spawn(move || c.record(0.5));
        let block: Vec<f32> = (0..100).flat_map(|i| [i as f32 / 100.0, -(i as f32) / 100.0]).collect();
        let start = Instant::now();
        while !rec.is_finished() && start.elapsed() < Duration::from_secs(5) {
            tap.push_stereo(&block, 2);
            std::thread::sleep(Duration::from_millis(5));
        }
        let clip = rec.join().unwrap().unwrap();
        assert_eq!(clip.frames(), 500);
        assert!(clip.samples.iter().all(|s| *s != 9.0), "samples pushed before the capture was armed leaked in");
        assert!(!capture.armed.load(Ordering::Relaxed));
    }

    #[test]
    fn listen_arguments_take_attributes_as_a_list_or_comma_separated() {
        let (secs, r) = request_of(&json!({"attributes": "8-bit, heroic ,", "seconds": 30}));
        assert_eq!((secs, r.attributes), (MAX_SECS, vec!["8-bit".to_string(), "heroic".to_string()]));
        let (secs, r) = request_of(&json!({"attributes": ["warm", " "], "question": "too loud?"}));
        assert_eq!((secs, r.attributes, r.question.as_deref()), (DEFAULT_SECS, vec!["warm".to_string()], Some("too loud?")));
    }

    #[test]
    fn listen_without_a_key_points_at_measure_mix() {
        let text = listen(&json!({"attributes": "soft"}));
        assert!(text.contains("no listening model") && text.contains("measure_mix"), "{text}");
    }

    #[test]
    fn a_capture_without_a_rendering_engine_says_so() {
        let (_tap, capture) = tap(1000);
        let err = capture.record(0.5).unwrap_err();
        assert!(err.contains("is audio output running"), "{err}");
    }

    #[test]
    fn the_reference_clips_differ_where_the_test_expects_it() {
        let bright = |c: &Clip| Features::of(c).meter.band_db[3];
        assert!(bright(&saw_riff(9000.0)) > bright(&saw_riff(500.0)) + 20.0);
        for clip in [chiptune(), saw_riff(500.0), saw_riff(9000.0)] {
            let f = Features::of(&clip);
            assert!(!f.silent() && f.peak_db <= 0.0, "{}", f.describe());
        }
    }
}
