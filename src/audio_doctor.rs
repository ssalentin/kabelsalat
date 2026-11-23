//! Audio Doctor (#211): diagnose Linux audio setup, measure round-trip
//! latency, check PipeWire / rtkit / CPU governor / USB autosuspend, apply
//! non-blocking one-click fixes, and explain xruns.
//!
//! Pure parsing, evaluation, latency analysis, and xrun diagnosis live here so
//! they are 100% unit-testable offline. Imperative system probes, fixes, and
//! live latency measurement run off the UI thread (CLI, background worker
//! threads, or headless MCP).

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use serde_json::{json, Value};

use crate::audio_backend::{self, ActiveBackend, AudioBackendChoice};
use crate::audio_in;
use crate::bridge;
use crate::engine_info::EngineStats;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Problem,
}

pub fn symbol(status: Status) -> &'static str {
    match status {
        Status::Ok => "✓",
        Status::Warning => "!",
        Status::Problem => "✗",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsbAudioDevice {
    pub sysfs_id: String,
    pub name: String,
    pub vendor_id: Option<String>,
    pub product_id: Option<String>,
    pub power_control: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixAction {
    SyncPipeWire { quantum: Option<u32>, clear_force_rate: bool },
    EnableRealtime,
    SetCpuGovernor,
    DisableUsbAutosuspend { devices: Vec<UsbAudioDevice> },
    RaiseBufferSize { frames: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixSpec {
    pub label: String,
    pub action: FixAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PipeWireSettings {
    pub available: bool,
    pub rate: Option<u32>,
    pub force_rate: Option<u32>,
    pub quantum: Option<u32>,
    pub min_quantum: Option<u32>,
    pub max_quantum: Option<u32>,
    pub force_quantum: Option<u32>,
}

impl PipeWireSettings {
    pub fn effective_quantum(&self) -> Option<u32> {
        self.force_quantum.filter(|&q| q > 0).or(self.quantum)
    }

    pub fn effective_rate(&self) -> Option<u32> {
        self.force_rate.filter(|&r| r > 0).or(self.rate)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RealtimeFacts {
    pub audio_thread_rt: Option<bool>,
    pub rlimit_rtprio_soft: u64,
    pub rlimit_rtprio_hard: u64,
    pub rtkit_active: Option<bool>,
    pub rtkit_installed: bool,
    pub in_realtime_group: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GovernorFacts {
    pub governors: Vec<String>,
    pub power_profile: Option<String>,
    pub has_powerprofilesctl: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EngineFacts {
    pub backend_choice: String,
    pub active_backend: Option<String>,
    pub configured_frames: u32,
    pub live_frames: u32,
    pub audio_live: bool,
    pub sample_rate: u32,
    pub dsp_percent: f64,
    pub render_max_us: u32,
    pub late_blocks: u32,
    pub xruns: u32,
    pub top_node: Option<(String, f64)>,
}

impl Default for EngineFacts {
    fn default() -> Self {
        Self {
            backend_choice: AudioBackendChoice::Auto.as_str().to_string(),
            active_backend: None,
            configured_frames: bridge::BUFFER_FRAMES,
            live_frames: bridge::BUFFER_FRAMES,
            audio_live: false,
            sample_rate: 48_000,
            dsp_percent: 0.0,
            render_max_us: 0,
            late_blocks: 0,
            xruns: 0,
            top_node: None,
        }
    }
}

impl EngineFacts {
    pub fn from_stats(
        choice: AudioBackendChoice,
        active: Option<ActiveBackend>,
        configured_frames: u32,
        stats: &EngineStats,
        fallback_rate: f64,
    ) -> Self {
        let rate = if stats.sample_rate > 0 {
            stats.sample_rate
        } else {
            fallback_rate.round().max(1.0) as u32
        };
        let live_frames = if stats.audio_live && stats.buffer_frames > 0 {
            stats.buffer_frames
        } else {
            configured_frames
        };
        Self {
            backend_choice: choice.as_str().to_string(),
            active_backend: active.map(|b| b.as_str().to_string()),
            configured_frames,
            live_frames,
            audio_live: stats.audio_live,
            sample_rate: rate,
            dsp_percent: stats.dsp_percent,
            render_max_us: stats.render_max_us,
            late_blocks: stats.late_blocks,
            xruns: stats.xruns,
            top_node: stats.nodes.first().map(|n| (n.name.clone(), n.percent)),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DoctorFacts {
    pub pipewire: PipeWireSettings,
    pub realtime: RealtimeFacts,
    pub governor: GovernorFacts,
    pub usb_audio: Vec<UsbAudioDevice>,
    pub engine: EngineFacts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRow {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub text: String,
    pub fix: Option<FixSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DoctorReport {
    pub checks: Vec<CheckRow>,
    pub facts: DoctorFacts,
    pub took_ms: u64,
}

impl DoctorReport {
    pub fn from_facts(facts: DoctorFacts, took: Duration) -> Self {
        let checks = evaluate(&facts);
        Self {
            checks,
            facts,
            took_ms: took.as_millis() as u64,
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    pub fn worst(&self) -> Status {
        self.checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok)
    }

    pub fn summary(&self) -> String {
        let (p, w) = (self.count(Status::Problem), self.count(Status::Warning));
        let plural = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        match (p, w) {
            (0, 0) => format!("All {} checks passed", self.checks.len()),
            (0, w) => format!("{} · {} ok", plural(w, "warning", "warnings"), self.count(Status::Ok)),
            (p, 0) => format!("{} · {} ok", plural(p, "problem", "problems"), self.count(Status::Ok)),
            (p, w) => format!(
                "{} · {} · {} ok",
                plural(p, "problem", "problems"),
                plural(w, "warning", "warnings"),
                self.count(Status::Ok)
            ),
        }
    }

    pub fn text(&self) -> String {
        let mut out = format!("Audio Doctor — {} ({} ms)\n", self.summary(), self.took_ms);
        let w = self.checks.iter().map(|r| r.title.len()).max().unwrap_or(0);
        for row in &self.checks {
            out += &format!("  {} {:<w$}  {}\n", symbol(row.status), row.title, row.text);
        }
        out
    }

    pub fn json(&self) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "title": c.title,
                    "status": c.status,
                    "text": c.text,
                    "fix_label": c.fix.as_ref().map(|f| f.label.as_str()),
                })
            })
            .collect();
        json!({
            "summary": self.summary(),
            "checks": checks,
            "facts": self.facts,
            "took_ms": self.took_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixOutcome {
    pub applied: bool,
    pub message: String,
    pub needs_manual: bool,
}

impl FixOutcome {
    fn ok(msg: impl Into<String>) -> Self {
        Self { applied: true, message: msg.into(), needs_manual: false }
    }

    fn manual(msg: impl Into<String>) -> Self {
        Self { applied: false, message: msg.into(), needs_manual: true }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self { applied: false, message: msg.into(), needs_manual: false }
    }
}

// ---------------------------------------------------------------------------
// Pure parsers & evaluators (unit-tested offline)
// ---------------------------------------------------------------------------

fn quoted_after<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let start = line.find(field)? + field.len();
    let rest = &line[start..];
    let end = rest.find('\'')?;
    Some(&rest[..end])
}

/// Parse `pw-metadata -n settings` stdout into [`PipeWireSettings`].
pub fn parse_pw_metadata(output: &str) -> PipeWireSettings {
    let mut pw = PipeWireSettings::default();
    if output.trim().is_empty() {
        return pw;
    }
    if output.contains("\"settings\"") || output.contains("key:'") {
        pw.available = true;
    }
    for line in output.lines() {
        let Some(key) = quoted_after(line, "key:'") else { continue };
        let Some(val) = quoted_after(line, "value:'") else { continue };
        let parsed = val.trim().parse::<u32>().ok();
        match key {
            "clock.rate" => pw.rate = parsed,
            "clock.force-rate" => pw.force_rate = parsed,
            "clock.quantum" => pw.quantum = parsed,
            "clock.min-quantum" => pw.min_quantum = parsed,
            "clock.max-quantum" => pw.max_quantum = parsed,
            "clock.force-quantum" => pw.force_quantum = parsed,
            _ => {}
        }
    }
    pw
}

/// Read `scaling_governor` for every CPU under `root` (`/sys/devices/system/cpu`).
pub fn read_cpu_governors_in(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut cpus: Vec<(u32, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(rest) = s.strip_prefix("cpu") else { continue };
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(idx) = rest.parse::<u32>() else { continue };
        let gov_path = entry.path().join("cpufreq").join("scaling_governor");
        if let Ok(gov) = std::fs::read_to_string(gov_path) {
            let trimmed = gov.trim();
            if !trimmed.is_empty() {
                cpus.push((idx, trimmed.to_string()));
            }
        }
    }
    cpus.sort_by_key(|(idx, _)| *idx);
    cpus.into_iter().map(|(_, g)| g).collect()
}

/// Scan `root` (`/sys/bus/usb/devices`) for USB Audio Class (`01`) devices and
/// read their `power/control` autosuspend state (`"auto"` vs `"on"`).
pub fn scan_usb_audio_in(root: &Path) -> Vec<UsbAudioDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let read_opt = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mut seen: std::collections::BTreeMap<String, UsbAudioDevice> = std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let iface_name = entry.file_name();
        let Some(iface_str) = iface_name.to_str() else { continue };
        // USB interface entries look like "1-1:1.0" while the parent device is "1-1".
        let Some((dev_id, _)) = iface_str.split_once(':') else { continue };
        let class = read_opt(&entry.path().join("bInterfaceClass"));
        if class.as_deref() != Some("01") {
            continue;
        }
        if seen.contains_key(dev_id) {
            continue;
        }
        let dev_dir = root.join(dev_id);
        let power_control = read_opt(&dev_dir.join("power").join("control")).unwrap_or_else(|| "unknown".into());
        let product = read_opt(&dev_dir.join("product"));
        let manufacturer = read_opt(&dev_dir.join("manufacturer"));
        let name = match (manufacturer, product) {
            (Some(m), Some(p)) if !p.to_ascii_lowercase().starts_with(&m.to_ascii_lowercase()) => format!("{m} {p}"),
            (_, Some(p)) => p,
            (Some(m), None) => m,
            (None, None) => format!("USB audio {dev_id}"),
        };
        seen.insert(
            dev_id.to_string(),
            UsbAudioDevice {
                sysfs_id: dev_id.to_string(),
                name,
                vendor_id: read_opt(&dev_dir.join("idVendor")),
                product_id: read_opt(&dev_dir.join("idProduct")),
                power_control,
            },
        );
    }
    seen.into_values().collect()
}

pub fn evaluate(facts: &DoctorFacts) -> Vec<CheckRow> {
    vec![
        eval_pipewire(facts),
        eval_realtime(facts),
        eval_governor(facts),
        eval_usb(facts),
        eval_buffer_xruns(facts),
    ]
}

fn eval_pipewire(facts: &DoctorFacts) -> CheckRow {
    let pw = &facts.pipewire;
    let eng = &facts.engine;
    let configured = eng.configured_frames.max(64);
    let eng_rate = if eng.sample_rate > 0 { eng.sample_rate } else { 48_000 };

    if eng.active_backend.as_deref() == Some("jack") {
        let frames = if eng.audio_live { eng.live_frames } else { configured };
        let ms = frames as f64 / eng_rate.max(1) as f64 * 1000.0;
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status: Status::Ok,
            text: format!(
                "JACK server controls timing ({frames} frames at {:.1} kHz, {ms:.1} ms).",
                eng_rate as f64 / 1000.0
            ),
            fix: None,
        };
    }

    if eng.active_backend.as_deref() == Some("alsa") && !pw.available {
        let frames = if eng.audio_live { eng.live_frames } else { configured };
        let ms = frames as f64 / eng_rate.max(1) as f64 * 1000.0;
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status: Status::Ok,
            text: format!(
                "Direct ALSA output at {frames} frames, {:.1} kHz ({ms:.1} ms); PipeWire is not running.",
                eng_rate as f64 / 1000.0
            ),
            fix: None,
        };
    }

    if !pw.available {
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status: Status::Warning,
            text: "Could not read PipeWire settings (pw-metadata not found or PipeWire not running).".into(),
            fix: None,
        };
    }

    let forced_rate = pw.force_rate.filter(|&r| r > 0);
    let rate_mismatch = forced_rate.is_some_and(|fr| fr != eng_rate);
    let forced_q = pw.force_quantum.filter(|&q| q > 0);
    let live_mismatch = eng.audio_live && eng.live_frames != configured;
    let forced_q_mismatch = forced_q.is_some_and(|fq| fq != configured);

    if live_mismatch || forced_q_mismatch {
        let actual_q = if live_mismatch {
            eng.live_frames
        } else {
            forced_q.unwrap_or(configured)
        };
        let eff_rate = pw.effective_rate().unwrap_or(eng_rate).max(1);
        let actual_ms = actual_q as f64 / eff_rate as f64 * 1000.0;
        let target_q = configured.max(pw.min_quantum.unwrap_or(32));
        let status = if actual_ms > 20.0 { Status::Problem } else { Status::Warning };
        let rate_note = match forced_rate {
            Some(fr) if fr != eng_rate => format!(
                " Sample rate is also forced to {fr} Hz while kabelsalat runs at {eng_rate} Hz."
            ),
            _ => String::new(),
        };
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status,
            text: format!(
                "PipeWire forces {actual_q} frames ({actual_ms:.1} ms at {:.1} kHz), while kabelsalat is set to {configured} frames.{rate_note}",
                eff_rate as f64 / 1000.0
            ),
            fix: Some(FixSpec {
                label: format!("Set quantum to {target_q}"),
                action: FixAction::SyncPipeWire {
                    quantum: Some(target_q),
                    clear_force_rate: rate_mismatch,
                },
            }),
        };
    }

    if let Some(fr) = forced_rate.filter(|&r| r != eng_rate) {
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status: Status::Warning,
            text: format!(
                "PipeWire forces {fr} Hz while kabelsalat started at {eng_rate} Hz; this can force resampling or shift pitch."
            ),
            fix: Some(FixSpec {
                label: "Reset PipeWire rate".into(),
                action: FixAction::SyncPipeWire {
                    quantum: None,
                    clear_force_rate: true,
                },
            }),
        };
    }

    if let Some(min_q) = pw.min_quantum.filter(|&mq| mq > configured) {
        return CheckRow {
            id: "pipewire",
            title: "PipeWire quantum & rate",
            status: Status::Warning,
            text: format!(
                "PipeWire's clock.min-quantum ({min_q}) prevents buffers smaller than {min_q} frames (kabelsalat wants {configured})."
            ),
            fix: None,
        };
    }

    let shown_q = if eng.audio_live {
        eng.live_frames
    } else {
        forced_q.unwrap_or(configured)
    };
    let shown_rate = pw.effective_rate().unwrap_or(eng_rate).max(1);
    let ms = shown_q as f64 / shown_rate as f64 * 1000.0;
    CheckRow {
        id: "pipewire",
        title: "PipeWire quantum & rate",
        status: Status::Ok,
        text: format!(
            "PipeWire runs at {:.1} kHz, {shown_q} frames ({ms:.1} ms), matching kabelsalat's {configured}-frame buffer.",
            shown_rate as f64 / 1000.0
        ),
        fix: None,
    }
}

fn eval_realtime(facts: &DoctorFacts) -> CheckRow {
    let rt = &facts.realtime;
    if rt.audio_thread_rt == Some(true) {
        return CheckRow {
            id: "realtime",
            title: "Realtime priority",
            status: Status::Ok,
            text: "Audio render thread runs with realtime priority (SCHED_RR / rtkit).".into(),
            fix: None,
        };
    }
    if rt.rlimit_rtprio_hard >= 50 {
        return CheckRow {
            id: "realtime",
            title: "Realtime priority",
            status: Status::Ok,
            text: format!(
                "Realtime priority is allowed via PAM limits (RLIMIT_RTPRIO {}).",
                rt.rlimit_rtprio_hard
            ),
            fix: None,
        };
    }
    if rt.rtkit_active == Some(true) && rt.audio_thread_rt != Some(false) {
        return CheckRow {
            id: "realtime",
            title: "Realtime priority",
            status: Status::Ok,
            text: "rtkit-daemon is active and grants realtime priority to audio threads.".into(),
            fix: None,
        };
    }
    if rt.rtkit_active == Some(true) && rt.audio_thread_rt == Some(false) {
        return CheckRow {
            id: "realtime",
            title: "Realtime priority",
            status: Status::Warning,
            text: "rtkit-daemon is running, but the audio thread ran at normal priority (sandbox or policy may block RT promotion).".into(),
            fix: Some(FixSpec {
                label: "Check rtkit".into(),
                action: FixAction::EnableRealtime,
            }),
        };
    }
    let why = if !rt.rtkit_installed {
        "rtkit is not installed and RLIMIT_RTPRIO is 0"
    } else {
        "rtkit-daemon is not running and RLIMIT_RTPRIO is 0"
    };
    CheckRow {
        id: "realtime",
        title: "Realtime priority",
        status: Status::Problem,
        text: format!(
            "Realtime priority is unavailable ({why}); UI or background desktop work can preempt audio and cause crackles."
        ),
        fix: Some(FixSpec {
            label: "Enable rtkit".into(),
            action: FixAction::EnableRealtime,
        }),
    }
}

/// Non-blocking startup realtime-priority check (GigChain #71): returns a
/// warning message when the audio thread is running without realtime priority
/// (or when neither PAM `RLIMIT_RTPRIO` nor `rtkit-daemon` is available), and
/// `None` when realtime priority is active or available.
pub fn startup_rt_warning(rt: &RealtimeFacts) -> Option<String> {
    let mut facts = DoctorFacts::default();
    facts.realtime = rt.clone();
    let row = eval_realtime(&facts);
    if row.status != Status::Ok {
        Some(row.text)
    } else {
        None
    }
}

/// Spawn a one-shot non-blocking check after audio stream startup and log a
/// warning to stderr if the audio render thread lacks realtime priority
/// (GigChain #71). Never blocks startup or the audio thread.
pub fn log_startup_rt_warning_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("rt-startup-check".into())
            .spawn(|| {
                // Give the first audio callback a moment to record its scheduling policy.
                std::thread::sleep(Duration::from_millis(250));
                let rt = gather_realtime();
                if let Some(msg) = startup_rt_warning(&rt) {
                    eprintln!("audio: WARNING: {msg} (open Audio Doctor to fix)");
                }
            });
    });
}

fn summarize_governors(govs: &[String]) -> String {
    let mut unique: Vec<&str> = Vec::new();
    for g in govs {
        if !unique.contains(&g.as_str()) {
            unique.push(g.as_str());
        }
    }
    unique.join("/")
}

fn eval_governor(facts: &DoctorFacts) -> CheckRow {
    let g = &facts.governor;
    let throttled = g.power_profile.as_deref() == Some("power-saver")
        || g.governors.iter().any(|s| s == "powersave" || s == "conservative");
    if throttled {
        let all_powersave = !g.governors.is_empty() && g.governors.iter().all(|s| s == "powersave");
        let status = if g.power_profile.as_deref() == Some("power-saver") || all_powersave {
            Status::Problem
        } else {
            Status::Warning
        };
        let detail = match (&g.power_profile, g.governors.is_empty()) {
            (Some(p), false) => format!("power profile '{p}' (governor: {})", summarize_governors(&g.governors)),
            (Some(p), true) => format!("power profile '{p}'"),
            (None, false) => format!("CPU governor '{}'", summarize_governors(&g.governors)),
            (None, true) => "power saving mode".into(),
        };
        return CheckRow {
            id: "governor",
            title: "CPU governor",
            status,
            text: format!("CPU frequency throttling is active ({detail}); switching to performance avoids clock ramp-up crackles."),
            fix: Some(FixSpec {
                label: "Set performance".into(),
                action: FixAction::SetCpuGovernor,
            }),
        };
    }

    if g.power_profile.as_deref() == Some("performance")
        || (!g.governors.is_empty() && g.governors.iter().all(|s| s == "performance"))
    {
        let text = if let Some(p) = &g.power_profile {
            if g.governors.is_empty() {
                format!("Power profile is {p}.")
            } else {
                format!(
                    "Power profile is {p} (governor {} across {} cores).",
                    summarize_governors(&g.governors),
                    g.governors.len()
                )
            }
        } else {
            format!("CPU governor is performance across {} cores.", g.governors.len())
        };
        return CheckRow {
            id: "governor",
            title: "CPU governor",
            status: Status::Ok,
            text,
            fix: None,
        };
    }

    if g.power_profile.as_deref() == Some("balanced") && g.has_powerprofilesctl {
        let gov_note = if g.governors.is_empty() {
            String::new()
        } else {
            format!(" (governor: {})", summarize_governors(&g.governors))
        };
        return CheckRow {
            id: "governor",
            title: "CPU governor",
            status: Status::Warning,
            text: format!(
                "Power profile is balanced{gov_note}; switching to performance avoids CPU ramp-up dropouts."
            ),
            fix: Some(FixSpec {
                label: "Set performance".into(),
                action: FixAction::SetCpuGovernor,
            }),
        };
    }

    if !g.governors.is_empty() {
        return CheckRow {
            id: "governor",
            title: "CPU governor",
            status: Status::Ok,
            text: format!(
                "CPU governor is {} across {} cores (no throttling detected).",
                summarize_governors(&g.governors),
                g.governors.len()
            ),
            fix: None,
        };
    }

    CheckRow {
        id: "governor",
        title: "CPU governor",
        status: Status::Ok,
        text: "CPU frequency scaling is not exposed on this machine.".into(),
        fix: None,
    }
}

fn eval_usb(facts: &DoctorFacts) -> CheckRow {
    if facts.usb_audio.is_empty() {
        return CheckRow {
            id: "usb_autosuspend",
            title: "USB audio autosuspend",
            status: Status::Ok,
            text: "No USB audio interface connected.".into(),
            fix: None,
        };
    }
    let auto_devs: Vec<UsbAudioDevice> = facts
        .usb_audio
        .iter()
        .filter(|d| d.power_control == "auto")
        .cloned()
        .collect();
    if !auto_devs.is_empty() {
        let names: Vec<&str> = auto_devs.iter().map(|d| d.name.as_str()).collect();
        return CheckRow {
            id: "usb_autosuspend",
            title: "USB audio autosuspend",
            status: Status::Warning,
            text: format!(
                "USB autosuspend is set to auto for {}; the kernel can power-gate the interface and drop audio.",
                names.join(", ")
            ),
            fix: Some(FixSpec {
                label: "Disable autosuspend".into(),
                action: FixAction::DisableUsbAutosuspend { devices: auto_devs },
            }),
        };
    }
    let names: Vec<&str> = facts.usb_audio.iter().map(|d| d.name.as_str()).collect();
    CheckRow {
        id: "usb_autosuspend",
        title: "USB audio autosuspend",
        status: Status::Ok,
        text: format!("USB autosuspend is disabled (on) for {}.", names.join(", ")),
        fix: None,
    }
}

fn collect_causes(facts: &DoctorFacts, dsp_percent: f64, late_blocks: u32, xruns: u32, frames: u32) -> Vec<String> {
    let mut causes = Vec::new();
    if let Some((name, pct)) = &facts.engine.top_node {
        if *pct >= 25.0 || dsp_percent >= 70.0 {
            causes.push(format!("{name} uses {pct:.0} % of each audio buffer"));
        }
    }
    if dsp_percent >= 70.0 || (late_blocks > 0 && frames <= 128) || (xruns > 0 && frames <= 128) {
        causes.push(format!("buffer size ({frames} frames) leaves tight headroom"));
    }
    if facts.realtime.rtkit_active != Some(true)
        && facts.realtime.rlimit_rtprio_hard < 50
        && facts.realtime.audio_thread_rt != Some(true)
    {
        causes.push("realtime audio priority (rtkit) is missing".into());
    }
    let throttled = facts.governor.power_profile.as_deref() == Some("power-saver")
        || facts.governor.governors.iter().any(|g| g == "powersave" || g == "conservative");
    if throttled {
        causes.push("CPU governor is throttling clock speed".into());
    }
    if facts.pipewire.available {
        if let Some(fq) = facts.pipewire.force_quantum.filter(|&q| q > 0) {
            if fq != facts.engine.configured_frames {
                causes.push(format!(
                    "PipeWire forces {fq} frames vs configured {}",
                    facts.engine.configured_frames
                ));
            }
        }
    }
    if facts.usb_audio.iter().any(|d| d.power_control == "auto") {
        causes.push("USB audio autosuspend is enabled".into());
    }
    causes
}

fn eval_buffer_xruns(facts: &DoctorFacts) -> CheckRow {
    let eng = &facts.engine;
    let rate = eng.sample_rate.max(1);
    let frames = if eng.audio_live {
        eng.live_frames
    } else {
        eng.configured_frames
    }
    .max(64);
    let ms = frames as f64 / rate as f64 * 1000.0;
    let setup = format!("{frames} frames at {:.1} kHz ({ms:.1} ms)", rate as f64 / 1000.0);

    if !eng.audio_live && eng.xruns == 0 && eng.late_blocks == 0 {
        return CheckRow {
            id: "buffer_xruns",
            title: "Buffer & xruns",
            status: Status::Ok,
            text: format!("{setup}; audio stream not running yet."),
            fix: None,
        };
    }

    if eng.xruns == 0 && eng.late_blocks == 0 && eng.dsp_percent < 70.0 {
        return CheckRow {
            id: "buffer_xruns",
            title: "Buffer & xruns",
            status: Status::Ok,
            text: format!("{setup}: {:.0} % DSP load, 0 xruns, 0 late blocks.", eng.dsp_percent),
            fix: None,
        };
    }

    let status = if eng.xruns > 0 || eng.dsp_percent > 90.0 {
        Status::Problem
    } else {
        Status::Warning
    };
    let causes = collect_causes(facts, eng.dsp_percent, eng.late_blocks, eng.xruns, frames);
    let cause_text = if causes.is_empty() {
        "Raise the buffer size or remove a heavy plugin if dropouts continue.".to_string()
    } else {
        format!("Likely cause: {}.", causes.join("; "))
    };
    let fix = crate::bridge::step_buffer_frames(frames, 1).map(|next| FixSpec {
        label: format!("Raise buffer to {next}"),
        action: FixAction::RaiseBufferSize { frames: next },
    });
    CheckRow {
        id: "buffer_xruns",
        title: "Buffer & xruns",
        status,
        text: format!(
            "{setup}: {:.0} % DSP, {} xrun{}, {} late block{}. {cause_text}",
            eng.dsp_percent,
            eng.xruns,
            if eng.xruns == 1 { "" } else { "s" },
            eng.late_blocks,
            if eng.late_blocks == 1 { "" } else { "s" },
        ),
        fix,
    }
}

// ---------------------------------------------------------------------------
// Live xrun test diagnosis (pure)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct XrunRunSummary {
    pub elapsed_secs: f32,
    pub target_secs: f32,
    pub stopped_early: bool,
    pub xruns: u32,
    pub late_blocks: u32,
    pub peak_dsp_percent: f64,
    pub worst_render_us: u32,
    pub buffer_frames: u32,
    pub sample_rate: u32,
    pub heaviest_node: Option<(String, f64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XrunDiagnosis {
    pub status: Status,
    pub headline: String,
    pub explanation: String,
    pub causes: Vec<String>,
    pub recommended_fix: Option<FixSpec>,
}

pub fn diagnose_xrun_run(run: &XrunRunSummary, facts: &DoctorFacts) -> XrunDiagnosis {
    let mut facts_with_peak = facts.clone();
    if let Some(h) = &run.heaviest_node {
        facts_with_peak.engine.top_node = Some(h.clone());
    }
    let frames = run.buffer_frames.max(64);
    let mut causes = collect_causes(&facts_with_peak, run.peak_dsp_percent, run.late_blocks, run.xruns, frames);
    let budget_us = frames as f64 / run.sample_rate.max(1) as f64 * 1e6;
    if run.worst_render_us as f64 > budget_us && !causes.iter().any(|c| c.contains("buffer")) {
        causes.push(format!(
            "worst callback ({} µs) exceeded the {:.0} µs buffer budget",
            run.worst_render_us, budget_us
        ));
    }

    let suffix = if run.stopped_early {
        format!("after {:.0} s (stopped)", run.elapsed_secs)
    } else {
        format!("over {:.0} s", run.elapsed_secs)
    };

    if run.xruns == 0 && run.late_blocks == 0 && run.peak_dsp_percent < 70.0 {
        return XrunDiagnosis {
            status: Status::Ok,
            headline: format!("Clean run {suffix}: 0 xruns, 0 late blocks"),
            explanation: format!(
                "Peak DSP load stayed at {:.0} % (worst render {} µs at {frames} frames).",
                run.peak_dsp_percent, run.worst_render_us
            ),
            causes: Vec::new(),
            recommended_fix: None,
        };
    }

    let status = if run.xruns > 0 || run.peak_dsp_percent > 90.0 {
        Status::Problem
    } else {
        Status::Warning
    };
    let headline = format!(
        "{} xrun{}, {} late block{} {suffix} (peak DSP {:.0} %)",
        run.xruns,
        if run.xruns == 1 { "" } else { "s" },
        run.late_blocks,
        if run.late_blocks == 1 { "" } else { "s" },
        run.peak_dsp_percent,
    );
    let explanation = if causes.is_empty() {
        "System scheduling jitter caused late audio blocks; try raising the buffer size one step.".into()
    } else {
        format!("Likely cause: {}.", causes.join("; "))
    };

    // Recommend the highest-impact fix from current facts / buffer size.
    let mut recommended_fix = None;
    for row in evaluate(&facts_with_peak) {
        if row.id != "buffer_xruns" && row.status != Status::Ok && row.fix.is_some() {
            recommended_fix = row.fix;
            break;
        }
    }
    if recommended_fix.is_none() {
        recommended_fix = crate::bridge::step_buffer_frames(frames, 1).map(|next| FixSpec {
            label: format!("Raise buffer to {next}"),
            action: FixAction::RaiseBufferSize { frames: next },
        });
    }

    XrunDiagnosis {
        status,
        headline,
        explanation,
        causes,
        recommended_fix,
    }
}

// ---------------------------------------------------------------------------
// Guided round-trip latency analysis (pure, testable offline)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum LatencyResult {
    Measured {
        median_ms: f64,
        min_ms: f64,
        max_ms: f64,
        detected: usize,
        total: usize,
        sample_rate: u32,
        buffer_frames: u32,
    },
    NoLoopback {
        noise_dbfs: f32,
        peak_corr: f32,
        message: String,
    },
    Cancelled,
    Failed(String),
}

/// Find the sample index in `recorded` corresponding to monotonic timestamp `target_ns`,
/// given monotonically increasing `(cumulative_samples, elapsed_ns)` chunk marks.
fn sample_at_ns(chunk_marks: &[(usize, u64)], target_ns: u64) -> Option<usize> {
    if chunk_marks.is_empty() {
        return None;
    }
    let pos = chunk_marks.partition_point(|&(_, t)| t < target_ns);
    if pos == 0 {
        let (s1, t1) = chunk_marks[0];
        if t1 == 0 {
            return Some(0);
        }
        return Some(((target_ns as f64 / t1 as f64) * s1 as f64).round().max(0.0) as usize);
    }
    if pos >= chunk_marks.len() {
        return None;
    }
    let (s0, t0) = chunk_marks[pos - 1];
    let (s1, t1) = chunk_marks[pos];
    if t1 <= t0 {
        return Some(s0);
    }
    let frac = (target_ns - t0) as f64 / (t1 - t0) as f64;
    Some((s0 as f64 + frac * (s1.saturating_sub(s0)) as f64).round() as usize)
}

/// Measure round-trip latency from recorded mono samples, capture chunk
/// timestamps, and output burst emission timestamps using normalized
/// cross-correlation (`crate::audio_in::best_marker_correlation`).
pub fn analyze_round_trip(
    recorded: &[f32],
    chunk_marks: &[(usize, u64)],
    emit_ns: &[u64],
    marker: &[f32],
    rate: u32,
    buffer_frames: u32,
) -> LatencyResult {
    if recorded.is_empty() || marker.is_empty() || rate == 0 {
        return LatencyResult::Failed("No audio samples were captured from the input device.".into());
    }
    let rate_f = rate as f64;
    // Measure ambient input noise before the first burst arrives.
    let noise_len = recorded.len().min((rate as usize) / 4).max(1);
    let noise_rms = (recorded[..noise_len].iter().map(|&x| x * x).sum::<f32>() / noise_len as f32).sqrt();
    let noise_dbfs = 20.0 * noise_rms.max(1e-6).log10();

    let min_off = ((rate as usize) / 1000).max(1); // +1 ms
    let win_len = ((rate as usize) / 4).max(marker.len() + 2); // 250 ms search window
    let mut delays_ms = Vec::new();
    let mut best_corr_seen = 0.0f32;
    let total = emit_ns.iter().filter(|&&t| t > 0).count();

    for &t_emit in emit_ns {
        if t_emit == 0 {
            continue;
        }
        let Some(s_emit) = sample_at_ns(chunk_marks, t_emit) else { continue };
        let start = (s_emit + min_off).min(recorded.len());
        let end = (s_emit + win_len).min(recorded.len());
        if end <= start + marker.len() {
            continue;
        }
        let window = &recorded[start..end];
        if let Some((off, corr)) = audio_in::best_marker_correlation(window, marker) {
            best_corr_seen = best_corr_seen.max(corr);
            let burst_end = (off + marker.len()).min(window.len());
            let burst_slice = &window[off..burst_end];
            let burst_rms = (burst_slice.iter().map(|&x| x * x).sum::<f32>() / burst_slice.len().max(1) as f32).sqrt();
            let burst_peak = burst_slice.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            if corr >= 0.35 && burst_peak >= 0.002 && burst_rms > noise_rms * 2.5 {
                let total_samples = (start + off).saturating_sub(s_emit);
                delays_ms.push(total_samples as f64 / rate_f * 1000.0);
            }
        }
    }

    if delays_ms.len() >= 2 {
        delays_ms.sort_by(|a, b| a.total_cmp(b));
        let median_ms = delays_ms[delays_ms.len() / 2];
        return LatencyResult::Measured {
            median_ms,
            min_ms: delays_ms[0],
            max_ms: *delays_ms.last().unwrap(),
            detected: delays_ms.len(),
            total: total.max(delays_ms.len()),
            sample_rate: rate,
            buffer_frames,
        };
    }

    LatencyResult::NoLoopback {
        noise_dbfs,
        peak_corr: best_corr_seen,
        message: "No loopback signal heard. Plug a cable from output to input, or unmute your speakers and microphone and turn up the volume slightly, then measure again.".into(),
    }
}

// ---------------------------------------------------------------------------
// Imperative system inspection & non-blocking fixes (off UI thread)
// ---------------------------------------------------------------------------

fn cmd_stdout(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which_exists(bin: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .any(|dir| dir.join(bin).is_file())
}

pub fn gather_pipewire() -> PipeWireSettings {
    match cmd_stdout("pw-metadata", &["-n", "settings"]) {
        Some(out) => parse_pw_metadata(&out),
        None => PipeWireSettings::default(),
    }
}

pub fn gather_realtime() -> RealtimeFacts {
    let mut soft = 0u64;
    let mut hard = 0u64;
    #[cfg(target_os = "linux")]
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_RTPRIO, &mut lim) == 0 {
            soft = lim.rlim_cur as u64;
            hard = lim.rlim_max as u64;
        }
    }
    let rtkit_active = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "rtkit-daemon.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success());
    let rtkit_installed = Path::new("/usr/lib/rtkit-daemon").exists()
        || Path::new("/usr/libexec/rtkit-daemon").exists()
        || Path::new("/usr/lib/rtkit/rtkit-daemon").exists()
        || which_exists("rtkitctl");
    let in_realtime_group = cmd_stdout("id", &["-Gn"])
        .map(|g| g.split_whitespace().any(|tok| tok == "realtime" || tok == "audio"))
        .unwrap_or(false);
    RealtimeFacts {
        audio_thread_rt: bridge::audio_thread_rt(),
        rlimit_rtprio_soft: soft,
        rlimit_rtprio_hard: hard,
        rtkit_active,
        rtkit_installed,
        in_realtime_group,
    }
}

pub fn gather_governor() -> GovernorFacts {
    let governors = read_cpu_governors_in(Path::new("/sys/devices/system/cpu"));
    let has_powerprofilesctl = which_exists("powerprofilesctl");
    let power_profile = if has_powerprofilesctl {
        cmd_stdout("powerprofilesctl", &["get"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };
    GovernorFacts {
        governors,
        power_profile,
        has_powerprofilesctl,
    }
}

pub fn gather_usb_audio() -> Vec<UsbAudioDevice> {
    scan_usb_audio_in(Path::new("/sys/bus/usb/devices"))
}

/// Gather all system facts alongside the engine snapshot. Blocking I/O —
/// run from CLI, MCP worker, or a background thread, never on the UI thread.
pub fn gather_facts(engine: EngineFacts) -> DoctorFacts {
    std::thread::scope(|scope| {
        let pw = scope.spawn(gather_pipewire);
        let rt = scope.spawn(gather_realtime);
        let gov = scope.spawn(gather_governor);
        let usb = scope.spawn(gather_usb_audio);
        DoctorFacts {
            pipewire: pw.join().unwrap_or_default(),
            realtime: rt.join().unwrap_or_default(),
            governor: gov.join().unwrap_or_default(),
            usb_audio: usb.join().unwrap_or_default(),
            engine,
        }
    })
}

fn run_non_interactive(bin: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    Err(if detail.is_empty() {
        format!("{bin} exited with {}", out.status)
    } else {
        detail
    })
}

fn running_as_root() -> bool {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::geteuid() == 0
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Apply one system fix without ever blocking on an interactive password prompt.
pub fn apply_fix(action: &FixAction) -> FixOutcome {
    match action {
        FixAction::SyncPipeWire { quantum, clear_force_rate } => {
            if let Some(q) = quantum {
                if let Err(e) = run_non_interactive(
                    "pw-metadata",
                    &["-n", "settings", "0", "clock.force-quantum", &q.to_string()],
                ) {
                    return FixOutcome::err(format!("Could not set PipeWire quantum: {e}"));
                }
            }
            if *clear_force_rate {
                if let Err(e) = run_non_interactive("pw-metadata", &["-n", "settings", "0", "clock.force-rate", "0"]) {
                    return FixOutcome::err(format!("Could not reset PipeWire rate: {e}"));
                }
            }
            match quantum {
                Some(q) => FixOutcome::ok(format!("PipeWire quantum set to {q} frames.")),
                None => FixOutcome::ok("PipeWire forced sample rate cleared."),
            }
        }
        FixAction::EnableRealtime => {
            let rt_ok = if running_as_root() {
                run_non_interactive("systemctl", &["enable", "--now", "rtkit-daemon.service"])
            } else {
                run_non_interactive("sudo", &["-n", "systemctl", "enable", "--now", "rtkit-daemon.service"])
            };
            match rt_ok {
                Ok(()) => FixOutcome::ok("rtkit-daemon started; restart audio or kabelsalat if needed."),
                Err(_) => {
                    let rtkit_present = Path::new("/usr/lib/rtkit-daemon").exists()
                        || Path::new("/usr/libexec/rtkit-daemon").exists()
                        || Path::new("/usr/lib/rtkit/rtkit-daemon").exists()
                        || which_exists("rtkitctl");
                    if rtkit_present {
                        FixOutcome::manual(
                            "Needs administrator permission once: run `sudo systemctl enable --now rtkit-daemon.service`.",
                        )
                    } else {
                        FixOutcome::manual(
                            "Install rtkit and start it: `sudo pacman -S rtkit && sudo systemctl enable --now rtkit-daemon.service` (or `sudo apt install rtkit`).",
                        )
                    }
                }
            }
        }
        FixAction::SetCpuGovernor => {
            if which_exists("powerprofilesctl")
                && run_non_interactive("powerprofilesctl", &["set", "performance"]).is_ok()
            {
                return FixOutcome::ok("Power profile set to performance.");
            }
            // Fallback: direct sysfs write (works if udev/permissions allow) or passwordless sudo.
            let mut wrote_any = false;
            if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
                for e in entries.flatten() {
                    let p = e.path().join("cpufreq").join("scaling_governor");
                    if p.exists() && std::fs::write(&p, b"performance\n").is_ok() {
                        wrote_any = true;
                    }
                }
            }
            if wrote_any {
                return FixOutcome::ok("CPU governor set to performance.");
            }
            if run_non_interactive("sudo", &["-n", "cpupower", "frequency-set", "-g", "performance"]).is_ok() {
                return FixOutcome::ok("CPU governor set to performance.");
            }
            FixOutcome::manual(
                "Needs administrator permission to switch CPU governor: run `sudo cpupower frequency-set -g performance` (or install power-profiles-daemon).",
            )
        }
        FixAction::DisableUsbAutosuspend { devices } => {
            if devices.is_empty() {
                return FixOutcome::ok("No USB audio devices need autosuspend disabled.");
            }
            let mut failed: Vec<&UsbAudioDevice> = Vec::new();
            for dev in devices {
                let path = format!("/sys/bus/usb/devices/{}/power/control", dev.sysfs_id);
                let direct = std::fs::write(&path, b"on\n").is_ok();
                let sudo_ok = direct
                    || (!running_as_root()
                        && run_non_interactive(
                            "sudo",
                            &["-n", "sh", "-c", "printf on > \"$1\"", "_", &path],
                        )
                        .is_ok());
                if !sudo_ok {
                    failed.push(dev);
                }
            }
            if failed.is_empty() {
                return FixOutcome::ok(format!(
                    "Disabled USB autosuspend for {} device(s).",
                    devices.len()
                ));
            }
            let first = failed[0];
            let sysfs_cmd = format!(
                "echo on | sudo tee /sys/bus/usb/devices/{}/power/control",
                first.sysfs_id
            );
            let udev = match (&first.vendor_id, &first.product_id) {
                (Some(v), Some(p)) => format!(
                    " Or make it permanent in udev: ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{v}\", ATTR{{idProduct}}==\"{p}\", TEST==\"power/control\", ATTR{{power/control}}=\"on\"."
                ),
                _ => String::new(),
            };
            FixOutcome::manual(format!(
                "Needs root to disable USB autosuspend: run `{sysfs_cmd}`.{udev}"
            ))
        }
        FixAction::RaiseBufferSize { frames } => FixOutcome::err(format!(
            "Buffer change to {frames} frames must be applied by the app."
        )),
    }
}

// ---------------------------------------------------------------------------
// Live round-trip measurement runner (background thread)
// ---------------------------------------------------------------------------

enum CaptureStop {
    Child(std::process::Child, Option<std::thread::JoinHandle<()>>),
    Cpal(cpal::Stream),
}

impl CaptureStop {
    fn stop(self) {
        match self {
            CaptureStop::Child(mut child, handle) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = handle {
                    let _ = h.join();
                }
            }
            CaptureStop::Cpal(stream) => {
                let _ = stream.pause();
                drop(stream);
            }
        }
    }
}

type CapturedSamples = Arc<Mutex<(Vec<f32>, Vec<(usize, u64)>)>>;

fn start_latency_capture(
    target_input: Option<&str>,
    rate: u32,
    buffer: u32,
    origin: Instant,
) -> Result<(CapturedSamples, CaptureStop), String> {
    let cap_buf: CapturedSamples = Arc::new(Mutex::new((Vec::with_capacity(rate as usize * 3), Vec::new())));

    // Prefer PipeWire `pw-record` when present (handles arbitrary PipeWire sources
    // without ALSA device contention).
    if which_exists("pw-record") {
        let latency_ms = ((buffer.max(64) as f64 * 1000.0 / rate.max(1) as f64).ceil() as u32).max(2);
        let mut cmd = std::process::Command::new("pw-record");
        cmd.args(["--raw", "--format", "f32", "--channels", "1"])
            .args(["--rate", &rate.to_string()])
            .args(["--latency", &format!("{latency_ms}ms")])
            .args(["-P", "{ node.name = kabelsalat-doctor-probe }"]);
        if let Some(d) = target_input.filter(|s| !s.is_empty()) {
            cmd.args(["--target", d]);
        }
        if let Ok(mut child) = cmd
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdout) = child.stdout.take() {
                let buf = cap_buf.clone();
                let handle = std::thread::Builder::new()
                    .name("ksalat-doc-cap".into())
                    .spawn(move || {
                        let mut raw = [0u8; 2048];
                        let mut carry: Vec<u8> = Vec::with_capacity(8);
                        loop {
                            let n = match stdout.read(&mut raw) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            let t_ns = origin.elapsed().as_nanos() as u64;
                            carry.extend_from_slice(&raw[..n]);
                            let whole = (carry.len() / 4) * 4;
                            if whole > 0 {
                                if let Ok(mut g) = buf.lock() {
                                    for c in carry[..whole].chunks_exact(4) {
                                        g.0.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                                    }
                                    let len = g.0.len();
                                    g.1.push((len, t_ns));
                                }
                                carry.drain(..whole);
                            }
                        }
                    })
                    .map_err(|e| format!("capture reader thread: {e}"))?;
                return Ok((cap_buf, CaptureStop::Child(child, Some(handle))));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    // Fallback: cpal default input device.
    let host = cpal::default_host();
    let dev = host
        .default_input_device()
        .ok_or_else(|| "no audio input device found".to_string())?;
    let cfg = dev
        .default_input_config()
        .map_err(|e| format!("input config failed: {e}"))?;
    let ch = cfg.channels().max(1) as usize;
    let mut config: cpal::StreamConfig = cfg.into();
    config.sample_rate = rate;
    config.buffer_size = cpal::BufferSize::Fixed(buffer.max(64));
    let buf = cap_buf.clone();
    let stream = dev
        .build_input_stream(
            config,
            move |data: &[f32], _| {
                let t_ns = origin.elapsed().as_nanos() as u64;
                if let Ok(mut g) = buf.try_lock() {
                    for frame in data.chunks(ch) {
                        let sum: f32 = frame.iter().copied().sum();
                        g.0.push(sum / frame.len().max(1) as f32);
                    }
                    let len = g.0.len();
                    g.1.push((len, t_ns));
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| format!("could not open input stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("could not start input stream: {e}"))?;
    Ok((cap_buf, CaptureStop::Cpal(stream)))
}

/// Measure real audio output → input round-trip latency in milliseconds on a
/// background thread. Plays 4 short chirp bursts while capturing the input.
pub fn measure_round_trip(
    backend: AudioBackendChoice,
    target_output: Option<&str>,
    target_input: Option<&str>,
    rate: u32,
    buffer_frames: u32,
    cancel: Arc<AtomicBool>,
) -> LatencyResult {
    if cancel.load(Ordering::Relaxed) {
        return LatencyResult::Cancelled;
    }
    let rate = rate.max(8000);
    let buffer = buffer_frames.max(64);
    let marker = audio_in::chirp_burst(rate);
    let origin = Instant::now();

    let (cap_buf, cap_stop) = match start_latency_capture(target_input, rate, buffer, origin) {
        Ok(c) => c,
        Err(e) => return LatencyResult::Failed(e),
    };

    // Wait briefly for capture stream to settle before emitting bursts.
    for _ in 0..10 {
        if cancel.load(Ordering::Relaxed) {
            cap_stop.stop();
            return LatencyResult::Cancelled;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    const BURSTS: usize = 4;
    let silence_frames = (rate as usize) / 4; // 250 ms ambient baseline
    let step_frames = ((rate as usize) * 45) / 100; // 450 ms apart
    let tail_frames = (rate as usize) / 3; // 333 ms tail
    let total_frames = silence_frames + (BURSTS - 1) * step_frames + marker.len() + tail_frames;

    let emit_ns: Arc<[AtomicU64; BURSTS]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
    let cursor = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let emit_cb = emit_ns.clone();
    let cursor_cb = cursor.clone();
    let done_cb = done.clone();
    let marker_cb = marker.clone();

    let stream = match audio_backend::open_stream(
        backend,
        target_output,
        Some(rate as f64),
        buffer,
        Box::new(move |out: &mut [f32], ch: usize, _| {
            let frames = out.len() / ch.max(1);
            let pos0 = cursor_cb.fetch_add(frames, Ordering::Relaxed);
            for (fi, frame) in out.chunks_mut(ch).enumerate() {
                let pos = pos0 + fi;
                let mut sample = 0.0f32;
                for (b, slot) in emit_cb.iter().enumerate() {
                    let start = silence_frames + b * step_frames;
                    if pos >= start && pos < start + marker_cb.len() {
                        if pos == start {
                            let t = origin.elapsed().as_nanos().max(1) as u64;
                            let _ = slot.compare_exchange(0, t, Ordering::Relaxed, Ordering::Relaxed);
                        }
                        sample = marker_cb[pos - start];
                        break;
                    }
                }
                for s in frame {
                    *s = sample;
                }
            }
            if pos0 + frames >= total_frames {
                done_cb.store(true, Ordering::Release);
            }
        }),
        Box::new(|_| {}),
    ) {
        Ok(s) => s,
        Err(e) => {
            cap_stop.stop();
            return LatencyResult::Failed(format!("Could not open audio output for measurement: {e}"));
        }
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            drop(stream);
            cap_stop.stop();
            return LatencyResult::Cancelled;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // Give PipeWire/ALSA a moment for the final burst tail to reach capture.
    std::thread::sleep(Duration::from_millis(120));
    drop(stream);
    cap_stop.stop();

    if cancel.load(Ordering::Relaxed) {
        return LatencyResult::Cancelled;
    }

    let (recorded, marks) = match cap_buf.lock() {
        Ok(g) => (g.0.clone(), g.1.clone()),
        Err(_) => return LatencyResult::Failed("Capture buffer lock poisoned.".into()),
    };
    let emitted: Vec<u64> = emit_ns.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    analyze_round_trip(&recorded, &marks, &emitted, &marker, rate, buffer)
}

// ---------------------------------------------------------------------------
// CLI entry point (`kabelsalat audio-doctor [--json]`)
// ---------------------------------------------------------------------------

pub fn run_cli(args: &[String]) -> anyhow::Result<()> {
    let want_json = args.iter().any(|a| a == "--json");
    let choice = audio_backend::effective_choice(crate::assistant::load_audio_backend());
    let target = crate::assistant::load_config_str("audio_output");
    let rate = audio_backend::probe_sample_rate(choice, target.as_deref()).round() as u32;
    let buf = crate::assistant::load_buffer_frames();
    let t = Instant::now();
    let facts = gather_facts(EngineFacts {
        backend_choice: choice.as_str().to_string(),
        active_backend: None,
        configured_frames: buf,
        live_frames: buf,
        audio_live: false,
        sample_rate: rate,
        ..Default::default()
    });
    let report = DoctorReport::from_facts(facts, t.elapsed());
    if want_json {
        println!("{}", serde_json::to_string_pretty(&report.json())?);
    } else {
        print!("{}", report.text());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_facts() -> DoctorFacts {
        DoctorFacts {
            pipewire: PipeWireSettings {
                available: true,
                rate: Some(48_000),
                force_rate: Some(0),
                quantum: Some(1024),
                min_quantum: Some(32),
                max_quantum: Some(2048),
                force_quantum: Some(0),
            },
            realtime: RealtimeFacts {
                audio_thread_rt: Some(true),
                rlimit_rtprio_soft: 0,
                rlimit_rtprio_hard: 0,
                rtkit_active: Some(true),
                rtkit_installed: true,
                in_realtime_group: false,
            },
            governor: GovernorFacts {
                governors: vec!["schedutil".into(); 4],
                power_profile: Some("performance".into()),
                has_powerprofilesctl: true,
            },
            usb_audio: vec![UsbAudioDevice {
                sysfs_id: "1-1".into(),
                name: "MOTU M2".into(),
                vendor_id: Some("07fd".into()),
                product_id: Some("000b".into()),
                power_control: "on".into(),
            }],
            engine: EngineFacts {
                backend_choice: "auto".into(),
                active_backend: Some("pipewire".into()),
                configured_frames: 256,
                live_frames: 256,
                audio_live: true,
                sample_rate: 48_000,
                dsp_percent: 18.0,
                render_max_us: 1100,
                late_blocks: 0,
                xruns: 0,
                top_node: Some(("Surge XT".into(), 12.0)),
            },
        }
    }

    fn check<'a>(rows: &'a [CheckRow], id: &str) -> &'a CheckRow {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn parses_real_pw_metadata_output() {
        let sample = r#"Found "settings" metadata 32
update: id:0 key:'log.level' value:'2' type:''
update: id:0 key:'clock.rate' value:'48000' type:''
update: id:0 key:'clock.allowed-rates' value:'[ 44100, 48000, 96000 ]' type:''
update: id:0 key:'clock.quantum' value:'1024' type:''
update: id:0 key:'clock.min-quantum' value:'32' type:''
update: id:0 key:'clock.max-quantum' value:'2048' type:''
update: id:0 key:'clock.force-quantum' value:'256' type:''
update: id:0 key:'clock.force-rate' value:'0' type:''
"#;
        let pw = parse_pw_metadata(sample);
        assert!(pw.available);
        assert_eq!(pw.rate, Some(48_000));
        assert_eq!(pw.quantum, Some(1024));
        assert_eq!(pw.min_quantum, Some(32));
        assert_eq!(pw.max_quantum, Some(2048));
        assert_eq!(pw.force_quantum, Some(256));
        assert_eq!(pw.force_rate, Some(0));
        assert_eq!(pw.effective_quantum(), Some(256));
        assert_eq!(pw.effective_rate(), Some(48_000));

        assert!(!parse_pw_metadata("").available);
    }

    #[test]
    fn healthy_machine_passes_all_checks() {
        let f = healthy_facts();
        let report = DoctorReport::from_facts(f, Duration::from_millis(12));
        assert!(report.checks.iter().all(|c| c.status == Status::Ok), "{}", report.text());
        assert_eq!(report.summary(), "All 5 checks passed");
        assert!(!report.text().contains("Active:"));
        assert!(!report.text().contains("overriding"));
    }

    #[test]
    fn pipewire_quantum_and_rate_mismatches_offer_fixes() {
        let mut f = healthy_facts();
        f.pipewire.force_quantum = Some(1024);
        f.pipewire.force_rate = Some(44_100);
        f.engine.configured_frames = 256;
        f.engine.live_frames = 1024;
        let rows = evaluate(&f);
        let pw = check(&rows, "pipewire");
        assert_eq!(pw.status, Status::Problem);
        assert!(pw.text.contains("1024 frames"), "{}", pw.text);
        assert!(pw.text.contains("44100 Hz"), "{}", pw.text);
        assert_eq!(
            pw.fix.as_ref().map(|x| &x.action),
            Some(&FixAction::SyncPipeWire {
                quantum: Some(256),
                clear_force_rate: true,
            })
        );

        // Rate mismatch alone also offers a fix.
        let mut f2 = healthy_facts();
        f2.pipewire.force_rate = Some(44_100);
        let rows2 = evaluate(&f2);
        let pw2 = check(&rows2, "pipewire");
        assert_eq!(pw2.status, Status::Warning);
        assert_eq!(
            pw2.fix.as_ref().map(|x| &x.action),
            Some(&FixAction::SyncPipeWire {
                quantum: None,
                clear_force_rate: true,
            })
        );
    }

    #[test]
    fn realtime_priority_check_distinguishes_rt_sources() {
        let mut f = healthy_facts();
        f.realtime.audio_thread_rt = None;
        f.realtime.rtkit_active = Some(false);
        f.realtime.rlimit_rtprio_hard = 0;
        let rows = evaluate(&f);
        let rt = check(&rows, "realtime");
        assert_eq!(rt.status, Status::Problem);
        assert_eq!(rt.fix.as_ref().map(|x| &x.action), Some(&FixAction::EnableRealtime));

        // PAM limits >= 50 pass even without rtkit.
        f.realtime.rlimit_rtprio_hard = 95;
        let rows_pam = evaluate(&f);
        assert_eq!(check(&rows_pam, "realtime").status, Status::Ok);
    }

    #[test]
    fn cpu_governor_reads_sysfs_and_flags_powersave_and_balanced() {
        let dir = std::env::temp_dir().join(format!("ksalat-gov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for c in [0, 1] {
            let p = dir.join(format!("cpu{c}")).join("cpufreq");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("scaling_governor"), "powersave\n").unwrap();
        }
        let govs = read_cpu_governors_in(&dir);
        assert_eq!(govs, vec!["powersave".to_string(), "powersave".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);

        let mut f = healthy_facts();
        f.governor = GovernorFacts {
            governors: govs,
            power_profile: Some("power-saver".into()),
            has_powerprofilesctl: true,
        };
        let rows = evaluate(&f);
        let row = check(&rows, "governor");
        assert_eq!(row.status, Status::Problem);
        assert_eq!(row.fix.as_ref().map(|x| &x.action), Some(&FixAction::SetCpuGovernor));

        f.governor = GovernorFacts {
            governors: vec!["schedutil".into(); 4],
            power_profile: Some("balanced".into()),
            has_powerprofilesctl: true,
        };
        let rows_bal = evaluate(&f);
        assert_eq!(check(&rows_bal, "governor").status, Status::Warning);
    }

    #[test]
    fn usb_audio_scanner_detects_audio_class_and_autosuspend() {
        let dir = std::env::temp_dir().join(format!("ksalat-usb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // USB Audio Class interface ("01") under device 1-2 with autosuspend "auto".
        let dev_a = dir.join("1-2");
        std::fs::create_dir_all(dev_a.join("power")).unwrap();
        std::fs::write(dev_a.join("manufacturer"), "MOTU\n").unwrap();
        std::fs::write(dev_a.join("product"), "M2\n").unwrap();
        std::fs::write(dev_a.join("idVendor"), "07fd\n").unwrap();
        std::fs::write(dev_a.join("idProduct"), "000b\n").unwrap();
        std::fs::write(dev_a.join("power").join("control"), "auto\n").unwrap();
        let if_a0 = dir.join("1-2:1.0");
        std::fs::create_dir_all(&if_a0).unwrap();
        std::fs::write(if_a0.join("bInterfaceClass"), "01\n").unwrap();

        // USB HID device ("03") must be ignored.
        let dev_hid = dir.join("1-3");
        std::fs::create_dir_all(dev_hid.join("power")).unwrap();
        std::fs::write(dev_hid.join("power").join("control"), "auto\n").unwrap();
        let if_hid = dir.join("1-3:1.0");
        std::fs::create_dir_all(&if_hid).unwrap();
        std::fs::write(if_hid.join("bInterfaceClass"), "03\n").unwrap();

        let scanned = scan_usb_audio_in(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].name, "MOTU M2");
        assert_eq!(scanned[0].power_control, "auto");

        let mut f = healthy_facts();
        f.usb_audio = scanned;
        let rows = evaluate(&f);
        let row = check(&rows, "usb_autosuspend");
        assert_eq!(row.status, Status::Warning);
        assert!(row.text.contains("MOTU M2"));
        assert!(matches!(
            row.fix.as_ref().map(|x| &x.action),
            Some(FixAction::DisableUsbAutosuspend { devices }) if devices.len() == 1
        ));
    }

    #[test]
    fn round_trip_analysis_recovers_known_delay_and_detects_missing_loopback() {
        let rate = 48_000u32;
        let marker = audio_in::chirp_burst(rate);
        let delay_samples = (0.0145 * rate as f64).round() as usize; // 14.5 ms = 696 samples
        let total_samples = rate as usize * 3;
        let mut rec = vec![0.0f32; total_samples];
        // Mild ambient floor (~-66 dBFS).
        for (i, s) in rec.iter_mut().enumerate() {
            *s = ((i as f32 * 0.37).sin()) * 0.0005;
        }
        let chunk = 512usize;
        let mut marks = Vec::new();
        for n in (chunk..=total_samples).step_by(chunk) {
            let t_ns = (n as f64 / rate as f64 * 1e9).round() as u64;
            marks.push((n, t_ns));
        }
        let burst_sample_offsets = [12_000usize, 33_600, 55_200, 76_800];
        let mut emit_ns = Vec::new();
        for &off in &burst_sample_offsets {
            emit_ns.push((off as f64 / rate as f64 * 1e9).round() as u64);
            let dst = off + delay_samples;
            for (k, &m) in marker.iter().enumerate() {
                rec[dst + k] += m * 0.6;
            }
        }
        match analyze_round_trip(&rec, &marks, &emit_ns, &marker, rate, 256) {
            LatencyResult::Measured { median_ms, detected, total, .. } => {
                assert_eq!(detected, 4);
                assert_eq!(total, 4);
                assert!((median_ms - 14.5).abs() < 0.2, "got {median_ms} ms");
            }
            other => panic!("expected Measured, got {other:?}"),
        }

        // Silence yields NoLoopback.
        let silent = vec![0.0f32; total_samples];
        assert!(matches!(
            analyze_round_trip(&silent, &marks, &emit_ns, &marker, rate, 256),
            LatencyResult::NoLoopback { .. }
        ));
    }

    #[test]
    fn xrun_diagnosis_identifies_causes_and_recommends_fix() {
        let mut f = healthy_facts();
        f.governor.power_profile = Some("power-saver".into());
        f.governor.governors = vec!["powersave".into(); 4];
        let run = XrunRunSummary {
            elapsed_secs: 60.0,
            target_secs: 60.0,
            stopped_early: false,
            xruns: 3,
            late_blocks: 5,
            peak_dsp_percent: 84.0,
            worst_render_us: 4900,
            buffer_frames: 128,
            sample_rate: 48_000,
            heaviest_node: Some(("Odin 2".into(), 52.0)),
        };
        let diag = diagnose_xrun_run(&run, &f);
        assert_eq!(diag.status, Status::Problem);
        assert!(diag.headline.contains("3 xruns"));
        assert!(diag.causes.iter().any(|c| c.contains("Odin 2")), "{:?}", diag.causes);
        assert!(diag.causes.iter().any(|c| c.contains("CPU governor")), "{:?}", diag.causes);
        assert_eq!(diag.recommended_fix.as_ref().map(|x| &x.action), Some(&FixAction::SetCpuGovernor));

        // Clean run reports Ok with no fix needed.
        let ok_run = XrunRunSummary {
            elapsed_secs: 60.0,
            target_secs: 60.0,
            stopped_early: false,
            xruns: 0,
            late_blocks: 0,
            peak_dsp_percent: 22.0,
            worst_render_us: 1200,
            buffer_frames: 256,
            sample_rate: 48_000,
            heaviest_node: None,
        };
        let ok_diag = diagnose_xrun_run(&ok_run, &healthy_facts());
        assert_eq!(ok_diag.status, Status::Ok);
        assert!(ok_diag.recommended_fix.is_none());
    }

    /// GigChain #71: startup RT check warns when running without realtime
    /// priority and stays silent when RT scheduling is active.
    #[test]
    fn startup_rt_warning_fires_when_audio_thread_lacks_realtime_priority() {
        let rt_ok = RealtimeFacts {
            audio_thread_rt: Some(true),
            rlimit_rtprio_soft: 0,
            rlimit_rtprio_hard: 0,
            rtkit_active: Some(true),
            rtkit_installed: true,
            in_realtime_group: false,
        };
        assert!(startup_rt_warning(&rt_ok).is_none());

        let rt_sandboxed = RealtimeFacts {
            audio_thread_rt: Some(false),
            rlimit_rtprio_soft: 0,
            rlimit_rtprio_hard: 0,
            rtkit_active: Some(true),
            rtkit_installed: true,
            in_realtime_group: false,
        };
        let warn = startup_rt_warning(&rt_sandboxed).expect("normal-priority audio thread must warn");
        assert!(warn.contains("normal priority"), "{warn}");

        let no_rt = RealtimeFacts {
            audio_thread_rt: Some(false),
            rlimit_rtprio_soft: 0,
            rlimit_rtprio_hard: 0,
            rtkit_active: Some(false),
            rtkit_installed: false,
            in_realtime_group: false,
        };
        let prob = startup_rt_warning(&no_rt).expect("missing rtkit + RLIMIT_RTPRIO=0 must warn");
        assert!(prob.contains("Realtime priority is unavailable"), "{prob}");
    }
}
