//! Audio In — live microphone / instrument input as a rack-able source.
//! (GitHub issue #28: a real audio input, AUM-style.)
//!
//! Shape: each `NodeKind::AudioIn` node captures one PipeWire source with a
//! `pw-record` process (PipeWire-native: sources by name, no busy ALSA card;
//! cpal's ALSA host listed duplicate, unusable devices and can't target a
//! PipeWire node). A reader thread pushes *stereo-interleaved* `f32` into two
//! lock-free `rtrb` rings:
//!
//! - the **engine ring**, consumed by the audio render callback at block
//!   boundaries (allocation- and lock-free, AGENTS.md rule 29), so an Audio In
//!   node routes into effects and Master exactly like a voice;
//! - the **tuner ring**, drained by the UI thread for pitch detection (the
//!   YIN/autocorrelation tuner here), never on the audio path.
//!
//! Input and output clocks differ slightly, so the render side keeps the ring
//! around a small target fill and drops (or holds) one sample when it drifts —
//! a single-sample drop every few hundred ms is inaudible, far cheaper than
//! resampling. Measured, not assumed: [`run_test`] reports the real drift.
//!
//! Device discovery runs on its own thread (rule 48) and rescans every 2 s so a
//! hot-plugged interface shows up. Unit tests never scan real hardware (rule
//! 48/37): only the pure tuner + drift logic are tested here, with synthetic
//! sines, never a live microphone.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// One input device, as the device-selection UI/MCP surface it. `raw` is the
/// PipeWire source node name (stable, used to open it); `name` is the
/// readable label from PipeWire's description ("MOTU M2", "MacBook
/// Microphone"); `default` marks PipeWire's default source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    pub raw: String,
    pub name: String,
    pub channels: u16,
    pub default: bool,
}

/// Readable names learned from the last device scan (raw → label), so faces
/// and subtitles can show "MacBook Microphone" for a node name without I/O.
static LABELS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Turn a PipeWire source name into a readable label: the description from
/// the last scan when known, else a heuristic for ALSA-style node names
/// ("alsa_input.usb-MOTU_M2_00HF-00.analog-stereo" → "MOTU M2").
pub fn pretty_device_name(raw: &str) -> String {
    if let Some(l) = LABELS.lock().ok().and_then(|v| v.iter().find(|(r, _)| r == raw).map(|(_, l)| l.clone())) {
        return l;
    }
    let body = raw
        .strip_prefix("alsa_input.")
        .or_else(|| raw.strip_prefix("alsa_output."))
        .or_else(|| raw.strip_prefix("alsa_pcm."))
        .unwrap_or(raw);
    let body = body.strip_prefix("usb-").or_else(|| body.strip_prefix("pci-")).unwrap_or(body);
    let body = body.split('.').next().unwrap_or(body); // drop ".analog-stereo"
    let parts: Vec<&str> = body.split(['_', '-']).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    // "VENDOR_MODEL_<serial>_…" → keep the first two tokens verbatim (MOTU M2).
    let keep = if parts.len() >= 2 { 2 } else { parts.len() };
    parts[..keep].join(" ")
}

/// Short label from a PipeWire description: profile suffixes are noise on a
/// rack face ("M2 Analog Stereo" → "M2").
fn short_description(desc: &str) -> String {
    let mut d = desc.trim().to_string();
    for suffix in [" Analog Stereo", " Digital Stereo (IEC958)", " Multichannel", " Pro", " Mono"] {
        if let Some(x) = d.strip_suffix(suffix) { d = x.to_string(); }
    }
    d
}

/// Parse `pactl -f json list sources` into real inputs: monitors (playback
/// taps, that is system audio) and our own nodes are left out.
fn parse_sources(json: &str, default: &str) -> Vec<InputDevice> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let mut out = Vec::new();
    for s in v.as_array().into_iter().flatten() {
        let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let props = s.get("properties");
        let class = props.and_then(|p| p.get("device.class")).and_then(|x| x.as_str()).unwrap_or("");
        if name.is_empty() || name.ends_with(".monitor") || class == "monitor" || name.contains("kabelsalat") {
            continue;
        }
        let desc = s.get("description").and_then(|x| x.as_str()).unwrap_or(name);
        let spec = s.get("sample_specification").and_then(|x| x.as_str()).unwrap_or("");
        let channels = spec.split_whitespace().find_map(|t| t.strip_suffix("ch")).and_then(|n| n.parse().ok()).unwrap_or(2);
        out.push(InputDevice { raw: name.to_string(), name: short_description(desc), channels, default: name == default });
    }
    // Default first (the "no selection" case), then alphabetical.
    out.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Enumerate PipeWire input sources. Runs `pactl` — call it from the
/// background discovery thread, never per UI frame (rule 48).
pub fn list_input_devices() -> Vec<InputDevice> {
    let run = |args: &[&str]| std::process::Command::new("pactl").args(args).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let default = run(&["get-default-source"]).unwrap_or_default().trim().to_string();
    let devs = run(&["-f", "json", "list", "sources"]).map(|j| parse_sources(&j, &default)).unwrap_or_default();
    if let Ok(mut l) = LABELS.lock() {
        *l = devs.iter().map(|d| (d.raw.clone(), d.name.clone())).collect();
    }
    devs
}

/// A running capture process (`pw-record`); killed when dropped.
pub struct CaptureProc(std::process::Child);

impl Drop for CaptureProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A live input capture: the capture process plus the two ring consumers.
/// The owner (the UI) keeps `stream` alive; dropping it stops the capture.
pub struct InputCapture {
    pub stream: CaptureProc,
    /// Engine read end (stereo-interleaved f32), handed to the render thread.
    pub engine_rx: rtrb::Consumer<f32>,
    /// Tuner read end (same samples), drained by the UI thread for YIN.
    pub tuner_rx: rtrb::Consumer<f32>,
    /// Sample rate the capture runs at.
    pub sample_rate: u32,
    /// Cleared when the capture ends (device unplugged, process gone); the UI
    /// polls it and re-opens when the device returns.
    pub live: Arc<AtomicBool>,
}

/// Open a capture of PipeWire source `device` (None = the default source) at
/// `rate`, stereo f32, asking PipeWire for a node latency of `buffer` frames.
/// PipeWire-native (`pw-record`): no busy ALSA card, any source by name, and
/// an interface used for input and output shares one graph clock. A named
/// device that is missing returns None — never silently another input.
pub fn open_capture(device: Option<&str>, rate: u32, buffer: u32) -> Option<InputCapture> {
    use std::io::Read;
    let rate = rate.max(1);
    let latency_ms = ((buffer.max(64) as f64 * 1000.0 / rate as f64).ceil() as u32).max(2);
    // Known from the background scan (no I/O here: this runs on the UI
    // thread, rule 48). Unknown = not plugged in yet; the periodic re-sync
    // opens it once a scan has seen it.
    if let Some(d) = device {
        if !LABELS.lock().map_or(false, |l| l.iter().any(|(r, _)| r == d)) {
            return None;
        }
    }
    let mut cmd = std::process::Command::new("pw-record");
    cmd.args(["--raw", "--format", "f32", "--channels", "2"])
        .args(["--rate", &rate.to_string()])
        .args(["--latency", &format!("{latency_ms}ms")])
        .args(["-P", "{ node.name = kabelsalat-audio-in }"]);
    if let Some(d) = device { cmd.args(["--target", d]); }
    let mut child = cmd.arg("-")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("audio in: cannot start pw-record: {e}"))
        .ok()?;
    let mut out = child.stdout.take()?;
    let cap = (rate as usize) * 2;
    let (mut engine_p, engine_rx) = rtrb::RingBuffer::<f32>::new(cap);
    let (mut tuner_p, tuner_rx) = rtrb::RingBuffer::<f32>::new(cap);
    let live = Arc::new(AtomicBool::new(true));
    let live_r = live.clone();
    std::thread::Builder::new()
        .name("kabelsalat-audio-in".into())
        .spawn(move || {
            let mut bytes = [0u8; 4096];
            let mut carry: Vec<u8> = Vec::with_capacity(8);
            loop {
                let n = match out.read(&mut bytes) { Ok(0) | Err(_) => break, Ok(n) => n };
                carry.extend_from_slice(&bytes[..n]);
                let whole = carry.len() / 4 * 4;
                for c in carry[..whole].chunks_exact(4) {
                    let s = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    let _ = engine_p.push(s);
                    let _ = tuner_p.push(s);
                }
                carry.drain(..whole);
            }
            live_r.store(false, Ordering::Relaxed); // EOF: device gone or process ended
        })
        .ok()?;
    Some(InputCapture { stream: CaptureProc(child), engine_rx, tuner_rx, sample_rate: rate, live })
}

// ---------------------------------------------------------------------------
// Tuner — YIN-style pitch detection (off the audio thread)
// ---------------------------------------------------------------------------

/// A detected pitch: nearest MIDI note + signed cents offset (−50..=+50).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pitch {
    pub note: u8,
    pub cents: f32,
    /// Fundamental frequency in Hz.
    pub freq: f32,
}

/// YIN pitch detector over a fixed window. `0.0` output = no confident pitch.
pub struct Tuner {
    buffer: Vec<f32>,
    /// Window: ≥ 2 periods of the lowest tracked note (E2 82.41 Hz @48k ≈ 582
    /// samples/period) with margin.
    window: usize,
    sr: f32,
}

impl Tuner {
    pub fn new(sample_rate: u32) -> Self {
        // 4096 samples @48k = 85 ms, ~3.5 periods of E2 and ~1.4 of a low B0.
        let window = 4096usize.max((sample_rate as usize / 12) * 4); // ≥ 4 periods @ ~12 Hz floor
        Self { buffer: vec![0.0; window], window, sr: sample_rate as f32 }
    }

    /// Feed a slice of (mono) samples; returns the current pitch estimate.
    /// Call whenever new samples are available; the detector keeps its own
    /// rolling window so callers just pump.
    pub fn process(&mut self, input: &[f32]) -> Option<Pitch> {
        if input.is_empty() {
            return None;
        }
        let w = self.window;
        // Shift old samples out, append new (rolling window).
        let n = input.len();
        if n >= w {
            self.buffer.copy_from_slice(&input[n - w..]);
        } else {
            self.buffer.copy_within(n.., 0);
            self.buffer[w - n..].copy_from_slice(input);
        }
        self.detect()
    }

    fn detect(&self) -> Option<Pitch> {
        let w = self.window;
        let x = &self.buffer[..w];
        // Energy gate: silence (or a nearly-DC floor) has no pitch.
        let mut energy = 0.0f32;
        for &s in x.iter().take(w) {
            energy += s * s;
        }
        if (energy / w as f32) < 1e-6 {
            return None;
        }
        let sr = self.sr;
        // Detectable range: E1 (~41 Hz) up to ~1.2 kHz (voice/guitar/tone).
        let min_period = (sr / 1200.0).floor().max(2.0) as usize; // ≈ 40 samples
        let max_period = (sr / 30.0).floor() as usize; // ≈ 1600 samples @48k
        if max_period + 1 >= w {
            return None; // window too small for the range
        }
        // YIN difference function d(tau) = sum (x[i] - x[i+tau])^2.
        let mut d = vec![0.0f32; max_period + 1];
        // Cumulative-mean-normalized difference on the fly to keep this cheap.
        let mut cmnd = vec![0.0f32; max_period + 1];
        let mut running = 0.0f32;
        for tau in 1..=max_period {
            let mut acc = 0.0f32;
            for i in 0..(w - max_period) {
                let diff = x[i] - x[i + tau];
                acc += diff * diff;
            }
            running += acc;
            d[tau] = acc;
            cmnd[tau] = if running > 0.0 { acc * tau as f32 / running } else { 0.0 };
        }
        // Absolute threshold + parabolic interpolation around the first dip.
        let threshold = 0.12f32;
        let mut tau = min_period;
        while tau <= max_period {
            if cmnd[tau] < threshold {
                // Look ahead for a local minimum.
                while tau + 1 < cmnd.len() && cmnd[tau + 1] < cmnd[tau] {
                    tau += 1;
                }
                break;
            }
            tau += 1;
        }
        let tau = tau.min(max_period);
        if tau == max_period && cmnd[tau] >= threshold {
            return None; // no confident dip
        }
        // Parabolic interpolation around tau (t0 = tau-1, t1 = tau, t2 = tau+1).
        let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
        let denom = (a - 2.0 * b + c) * 2.0;
        let shift = if denom.abs() > 1e-9 { (a - c) / denom } else { 0.0 };
        let period = (tau as f32 + shift).max(1.0);
        let freq = (sr / period).clamp(20.0, 2000.0);
        let (note, cents) = pitch_to_note(freq);
        Some(Pitch { note, cents, freq })
    }
}

/// Nearest MIDI note and signed cents for a frequency (A4 = 440 Hz = note 69).
pub fn pitch_to_note(freq: f32) -> (u8, f32) {
    let midi = 69.0 + 12.0 * (freq / 440.0).log2();
    let note = midi.round().clamp(0.0, 127.0) as u8;
    let cents = ((midi - note as f32) * 100.0).clamp(-50.0, 50.0);
    (note, cents)
}

/// Note name for a MIDI note (60 = C4), matching the UI readout.
pub fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[(n % 12) as usize], n as i32 / 12 - 1)
}

// ---------------------------------------------------------------------------
// Drift control — reconcile input vs output clocks around a target fill
// ---------------------------------------------------------------------------

/// Ring-fill tracker for one input ring. The render side reads `frames` stereo
/// samples each block and asks this what to do about the leftover fill.
#[derive(Clone, Copy, Debug)]
pub struct Drift {
    low: usize,
    high: usize,
}

/// What the consumer should do after reading a block, given `available`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftAction {
    /// Fill is healthy: nothing to do.
    Hold,
    /// Input is running fast: discard one sample to pull the ring back down.
    DropOne,
    /// Input is running slow (or starved): hold the last sample one slot.
    InsertOne,
}

impl Drift {
    /// `block` = samples the consumer reads per render block (mono count).
    pub fn new(block: usize) -> Self {
        let target = block * 2;
        Self { low: target.saturating_sub(block / 2), high: target.saturating_add(block / 2) }
    }

    pub fn action(&self, available: usize) -> DriftAction {
        if available > self.high {
            DriftAction::DropOne
        } else if available < self.low {
            DriftAction::InsertOne
        } else {
            DriftAction::Hold
        }
    }
}

// ---------------------------------------------------------------------------
// Latency / drift measurement (read-only publish)
// ---------------------------------------------------------------------------

/// Live input↔output latency stats, written by callbacks and read by the UI.
#[derive(Default)]
pub struct LatencyMeter {
    pub in_calls: AtomicU64,
    pub in_total_ns: AtomicU64,
    pub out_calls: AtomicU64,
    pub out_total_ns: AtomicU64,
    pub dropped: AtomicU64,
    pub starved: AtomicU64,
    pub drift_events: AtomicU64,
    pub fill: AtomicUsize,
}

impl LatencyMeter {
    fn record_input(&self, ns: u64) {
        self.in_calls.fetch_add(1, Ordering::Relaxed);
        self.in_total_ns.fetch_add(ns, Ordering::Relaxed);
    }
    fn record_output(&self, ns: u64) {
        self.out_calls.fetch_add(1, Ordering::Relaxed);
        self.out_total_ns.fetch_add(ns, Ordering::Relaxed);
    }

    fn avg(n: u64, total: u64) -> String {
        if n == 0 {
            "n/a".into()
        } else {
            format!("{:.2} ms", total as f64 / n as f64 / 1e6)
        }
    }

    pub fn report(&self) {
        println!(
            "input stream latency (callback − capture):  {} ({} callbacks)",
            Self::avg(self.in_calls.load(Ordering::Relaxed), self.in_total_ns.load(Ordering::Relaxed)),
            self.in_calls.load(Ordering::Relaxed),
        );
        println!(
            "output stream latency (playback − callback): {} ({} callbacks)",
            Self::avg(self.out_calls.load(Ordering::Relaxed), self.out_total_ns.load(Ordering::Relaxed)),
            self.out_calls.load(Ordering::Relaxed),
        );
        let (i, o) = (self.in_total_ns.load(Ordering::Relaxed), self.out_total_ns.load(Ordering::Relaxed));
        let (ic, oc) = (self.in_calls.load(Ordering::Relaxed), self.out_calls.load(Ordering::Relaxed));
        if ic > 0 && oc > 0 {
            let round_trip = i as f64 / ic as f64 / 1e6 + o as f64 / oc as f64 / 1e6;
            println!("estimated input→output round trip: {round_trip:.2} ms (in + out stream latency)");
        }
        println!(
            "ring: {} dropped (full), {} starved (empty), {} drift corrections, fill {}",
            self.dropped.load(Ordering::Relaxed),
            self.starved.load(Ordering::Relaxed),
            self.drift_events.load(Ordering::Relaxed),
            self.fill.load(Ordering::Relaxed),
        );
    }
}

// ---------------------------------------------------------------------------
// Self-test: open the default input, loop it to the output, measure latency.
// ---------------------------------------------------------------------------

/// `kabelsalat audio-in-test`: a software loopback that measures the real
/// input→output round trip (via cpal stream timestamps) and the ring drift.
/// Skips cleanly when there is no input device (not an error — a headless
/// machine).
pub fn run_test() -> anyhow::Result<()> {
    let host = crate::ui::preferred_host();
    println!("audio host: {}", host.id().name());

    let Some(dev) = host.default_input_device() else {
        println!("audio-in-test: SKIP — no default input device on this machine");
        return Ok(());
    };
    let in_cfg = dev.default_input_config()?;
    let out_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let out_cfg = out_dev.default_output_config()?;
    let rate = out_cfg.sample_rate().max(1);
    let buffer = crate::bridge::BUFFER_FRAMES;
    println!(
        "input: {} ({} ch)", dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        in_cfg.channels()
    );

    // Ring input → output (loopback at low gain, clamped).
    let cap = (rate as usize) * 2;
    let (mut ib_prod, mut ib_cons) = rtrb::RingBuffer::<f32>::new(cap);
    let meter = std::sync::Arc::new(LatencyMeter::default());
    let drift = Drift::new(buffer as usize * 2);

    let nch = in_cfg.channels().max(1) as usize;
    let meter2 = meter.clone();
    let mut in_config: cpal::StreamConfig = in_cfg.clone().into();
    in_config.buffer_size = cpal::BufferSize::Fixed(buffer);
    in_config.sample_rate = rate;
    let in_stream = dev.build_input_stream(
        in_config,
        move |data: &[f32], info: &cpal::InputCallbackInfo| {
            // Downmix device channels to mono and push into the ring.
            for frame in data.chunks(nch) {
                let s = frame.iter().copied().sum::<f32>() / nch as f32;
                if ib_prod.push(s).is_err() {
                    meter2.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Input latency: callback − capture (cpal timestamps share a clock).
            let ts = info.timestamp();
            if let Some(d) = ts.callback.checked_duration_since(ts.capture) {
                meter2.record_input(d.as_nanos() as u64);
            }
        },
        |_e| {},
        None,
    )?;
    in_stream.play()?;

    let meter3 = meter.clone();
    let mut out_config: cpal::StreamConfig = out_cfg.clone().into();
    out_config.buffer_size = cpal::BufferSize::Fixed(buffer);
    let out_stream = out_dev.build_output_stream(
        out_config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            for s in data.iter_mut() {
                let v = match ib_cons.pop() {
                    Ok(v) => v.clamp(-1.0, 1.0),
                    Err(_) => {
                        meter3.starved.fetch_add(1, Ordering::Relaxed);
                        0.0
                    }
                };
                *s = v * 0.1;
            }
            // Output latency: playback − callback (same clock as input).
            let ts = info.timestamp();
            if let Some(d) = ts.playback.checked_duration_since(ts.callback) {
                meter3.record_output(d.as_nanos() as u64);
            }
            let avail = ib_cons.slots();
            meter3.fill.store(avail, Ordering::Relaxed);
            if drift.action(avail) == DriftAction::DropOne {
                let _ = ib_cons.pop();
                meter3.drift_events.fetch_add(1, Ordering::Relaxed);
            }
        },
        |_e| {},
        None,
    )?;
    out_stream.play()?;

    println!("looping input → output for 3 s (measure latency + drift)...");
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(in_stream);
    drop(out_stream);
    meter.report();
    println!("audio-in-test: OK — input capture + loopback stable");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: f32, freq: f32, secs: f32) -> Vec<f32> {
        let n = (sr * secs) as usize;
        (0..n).map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin() * 0.5).collect()
    }

    fn probe(tuner: &mut Tuner, sig: &[f32]) -> Pitch {
        // Pump a few steady windows so the rolling buffer is fully the tone.
        for _ in 0..6 {
            tuner.process(sig);
        }
        tuner.process(sig).expect("confident pitch for a clean sine")
    }

    #[test]
    fn tuner_reads_clean_sines_within_three_cents() {
        let mut t = Tuner::new(48_000);
        // Exact 12-TET note frequencies (A4 = 440): cents ≈ 0.
        for freq in [82.41, 110.0, 196.0, 293.66, 440.0, 987.77] {
            let sig = sine(48_000.0, freq, 0.5);
            let p = probe(&mut t, &sig);
            let midi = 69.0 + 12.0 * (freq / 440.0).log2();
            assert_eq!(p.note, midi.round() as u8, "note for {freq} Hz");
            assert!(p.cents.abs() <= 3.0, "within ±3 cents at {freq} Hz: {:.2}", p.cents);
            assert!((p.freq - freq).abs() / freq < 0.01, "freq for {freq} Hz: {:.2}", p.freq);
        }
    }

    #[test]
    fn tuner_reads_guitar_low_e_exactly() {
        let mut t = Tuner::new(48_000);
        let sig = sine(48_000.0, 82.41, 0.6);
        let p = probe(&mut t, &sig);
        assert_eq!(p.note, 40, "E2 (MIDI 40) is the guitar's low E string");
        assert!(p.cents.abs() <= 3.0, "low E within ±3 cents: {:.2}", p.cents);
        assert!((p.freq - 82.41).abs() < 1.5, "low E freq ≈ 82.41 Hz: {:.2}", p.freq);
    }

    #[test]
    fn tuner_reports_detuning_correctly() {
        let mut t = Tuner::new(48_000);
        // 1000 Hz is ~21 cents sharp of B5 (987.77 Hz) — a deliberately
        // detuned tone must READ detuned, not be forced onto the nearest note.
        let sig = sine(48_000.0, 1000.0, 0.5);
        let p = probe(&mut t, &sig);
        assert_eq!(p.note, 83, "1000 Hz is closest to B5");
        assert!((p.cents - 21.2).abs() < 3.0, "1000 Hz ≈ +21 cents sharp of B5: {:.2}", p.cents);
        assert!((p.freq - 1000.0).abs() < 20.0, "freq ≈ 1000 Hz: {:.2}", p.freq);
    }

    #[test]
    fn tuner_is_silent_on_silence() {
        let mut t = Tuner::new(48_000);
        for _ in 0..6 {
            assert!(t.process(&vec![0.0; 512]).is_none(), "silence has no pitch");
        }
    }

    #[test]
    fn pitch_to_note_maps_a4_and_offsets() {
        let (n, c) = pitch_to_note(440.0);
        assert_eq!(n, 69);
        assert!(c.abs() < 1e-3, "A4 is exact");
        let (n, c) = pitch_to_note(466.16); // A#4
        assert_eq!(n, 70);
        assert!(c.abs() <= 3.0);
        // A quarter-tone flat A4 sits at −50 cents.
        let (n, c) = pitch_to_note(440.0 * 2f32.powf(-0.5 / 12.0));
        assert_eq!(n, 69);
        assert!((c + 50.0).abs() < 2.0);
    }

    #[test]
    fn note_name_matches_ui_convention() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(40), "E2");
        assert_eq!(note_name(69), "A4");
    }

    #[test]
    fn drift_holds_in_band_and_corrects_outside() {
        let d = Drift::new(256); // 256-sample block
        let target = 256 * 2;
        assert_eq!(d.action(target), DriftAction::Hold, "on target = hold");
        assert_eq!(d.action(target + 100), DriftAction::Hold, "small overshoot stays in band");
        assert_eq!(d.action(target + 1000), DriftAction::DropOne, "fast input → drop");
        assert_eq!(d.action(0), DriftAction::InsertOne, "starved → hold last");
        assert_eq!(d.action(2), DriftAction::InsertOne, "near-empty → hold last");
    }

    #[test]
    fn drift_band_scales_with_block() {
        let big = Drift::new(512);
        let small = Drift::new(256);
        assert!(big.high > small.high, "larger block → larger band");
        assert!(big.low > small.low);
    }

    #[test]
    fn pretty_device_name_reads_brand_and_model() {
        assert_eq!(pretty_device_name("alsa_input.usb-MOTU_M2_M2MC0123-00.analog-stereo"), "MOTU M2");
        assert_eq!(pretty_device_name("alsa_input.usb-Focusrite_Scarlett_2i2_USB_ABCD-00.analog-stereo"), "Focusrite Scarlett");
        assert_eq!(pretty_device_name(""), "");
    }

    #[test]
    fn pipewire_sources_list_real_inputs_with_readable_names() {
        // Shape of `pactl -f json list sources` (trimmed, real field names).
        let json = r#"[
          {"name":"alsa_output.platform-sound.HiFi__Headphones__sink.monitor","description":"Monitor of Headphones","sample_specification":"s32le 2ch 48000Hz","properties":{"device.class":"monitor"}},
          {"name":"omarchy_asahi_mic","description":"MacBook Microphone","sample_specification":"float32le 2ch 48000Hz","properties":{"device.class":"filter"}},
          {"name":"alsa_input.usb-MOTU_M2_M2MC0123-00.analog-stereo","description":"M2 Analog Stereo","sample_specification":"s32le 2ch 48000Hz","properties":{"device.class":"sound"}},
          {"name":"effect_output.j293-mic","description":"MacBook Microphone (Mono)","sample_specification":"float32le 1ch 48000Hz","properties":{}}
        ]"#;
        let devs = parse_sources(json, "omarchy_asahi_mic");
        let names: Vec<&str> = devs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["MacBook Microphone", "M2", "MacBook Microphone (Mono)"], "default first, monitors gone");
        assert!(devs[0].default && !devs[1].default);
        assert_eq!(devs[1].raw, "alsa_input.usb-MOTU_M2_M2MC0123-00.analog-stereo", "opened by its stable node name");
        assert_eq!((devs[1].channels, devs[2].channels), (2, 1));
        assert!(parse_sources("not json", "").is_empty());
    }
}