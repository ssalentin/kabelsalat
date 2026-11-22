//! Audio output selection — which PipeWire sink kabelsalat plays to.
//!
//! The engine opens one cpal/ALSA output stream through PipeWire's ALSA
//! plugin; PipeWire decides where it goes (the system default sink). The
//! user picks an output in the mixer: the choice is stored in config.json
//! (`audio_output`) and a background thread moves the running stream there
//! with `pactl move-sink-input` — live, no stream reopen, no dropout — and
//! keeps it there across restarts and re-plugs. `None` = follow PipeWire's
//! default (the old behaviour).
//!
//! All `pactl` calls run on the device-scan thread (AGENTS.md rule 48);
//! the UI only reads the shared snapshot and writes the wanted output.

use serde_json::Value;

/// One output, as the mixer and agent tools show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    /// PipeWire sink node name (stable; used to move the stream).
    pub raw: String,
    /// Readable label ("M Series Headphone + Monitor Out").
    pub name: String,
    /// PipeWire's current default sink.
    pub default: bool,
}

/// Another application's playback stream (YouTube in Zen, Spotify, …),
/// shown and controlled in the mixer's "System audio" section.
#[derive(Clone, Debug, PartialEq)]
pub struct AppStream {
    /// PipeWire sink-input index (the handle for volume/mute).
    pub index: u64,
    /// Application ("Zen").
    pub app: String,
    /// What it plays ("Neo Soul Guitar Backing Track … - YouTube").
    pub title: String,
    /// Stream volume, 1.0 = 100 % (average over channels).
    pub volume: f32,
    pub muted: bool,
}

/// Live levels of other apps' streams (the mixer's "System audio" meters):
/// one `parec --monitor-stream` reader per playing stream, the way
/// pavucontrol meters them. 8 kHz mono float is plenty for a meter and costs
/// next to nothing. Readers live on their own threads (rule 48) and park in
/// a blocking read (rule 47); a reader for a stream that is gone is killed.
pub struct StreamMeters {
    readers: std::collections::HashMap<u64, std::process::Child>,
    levels: StreamLevels,
}

/// Peak (linear, smoothed) per sink-input index, with when it was measured.
pub type StreamLevels = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, (f32, std::time::Instant)>>>;

/// Samples per meter reading: 40 ms at 8 kHz (25 readings per second).
const METER_RATE: u32 = 8000;
const METER_CHUNK: usize = 320;

impl StreamMeters {
    pub fn new(levels: StreamLevels) -> Self {
        Self { readers: Default::default(), levels }
    }

    /// Meter exactly these streams: start readers for new ones, stop those
    /// whose stream ended (or whose reader exited, so it restarts).
    pub fn sync(&mut self, streams: &[AppStream]) {
        let wanted: std::collections::HashSet<u64> = streams.iter().map(|s| s.index).collect();
        self.readers.retain(|idx, child| {
            let alive = matches!(child.try_wait(), Ok(None));
            if wanted.contains(idx) && alive { return true; }
            let _ = child.kill();
            let _ = child.wait();
            false
        });
        if let Ok(mut l) = self.levels.lock() { l.retain(|idx, _| wanted.contains(idx)); }
        for &idx in &wanted {
            if self.readers.contains_key(&idx) { continue; }
            if let Some(child) = spawn_meter_reader(idx, self.levels.clone()) {
                self.readers.insert(idx, child);
            }
        }
    }
}

impl Drop for StreamMeters {
    fn drop(&mut self) {
        for (_, mut c) in self.readers.drain() { let _ = c.kill(); let _ = c.wait(); }
    }
}

fn spawn_meter_reader(index: u64, levels: StreamLevels) -> Option<std::process::Child> {
    use std::io::Read;
    let mut child = std::process::Command::new("parec")
        .args([
            &format!("--monitor-stream={index}"), &format!("--rate={METER_RATE}"),
            "--channels=1", "--format=float32le", "--latency-msec=40",
            "--client-name=kabelsalat-meter",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    std::thread::Builder::new()
        .name(format!("kabelsalat-meter-{index}"))
        .spawn(move || {
            let mut buf = vec![0u8; METER_CHUNK * 4];
            let mut level = 0.0f32;
            while out.read_exact(&mut buf).is_ok() {
                level = smooth_meter(level, chunk_peak(&buf));
                if let Ok(mut l) = levels.lock() { l.insert(index, (level, std::time::Instant::now())); }
            }
        })
        .ok()?;
    Some(child)
}

/// Peak |sample| of a chunk of little-endian f32 samples.
pub fn chunk_peak(bytes: &[u8]) -> f32 {
    bytes.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
        .filter(|v| v.is_finite())
        .fold(0.0, f32::max)
}

/// Meter ballistics per reading: instant rise, ~-6 dB per 40 ms fall.
pub fn smooth_meter(prev: f32, peak: f32) -> f32 {
    peak.max(prev * 0.5)
}

/// The level to show for a stream: silent when no reading arrived lately
/// (a paused tab stops delivering data instead of sending zeros).
pub fn stream_level(levels: &StreamLevels, index: u64) -> f32 {
    levels.lock().ok()
        .and_then(|l| l.get(&index).copied())
        .filter(|(_, at)| at.elapsed() < std::time::Duration::from_millis(250))
        .map_or(0.0, |(v, _)| v)
}

/// A change the UI or an agent asks for; applied on the scan thread.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamCmd {
    Volume(u64, f32),
    Mute(u64, bool),
    /// Wake the scan thread to re-apply our chosen output sink right away
    /// (e.g. after reopening the audio stream at a new buffer size).
    Resync,
}

/// What the scan thread last saw: every output, the sink our stream
/// currently plays to (None = stream not found yet), and the other apps'
/// streams that are playing right now.
#[derive(Clone, Debug, Default)]
pub struct OutputState {
    pub outputs: Vec<OutputDevice>,
    pub current: Option<String>,
    pub streams: Vec<AppStream>,
    /// The `streams` the system-audio capture may record (sink-input
    /// indices): those known to belong to another process.
    pub capturable: Vec<u64>,
}

/// Other apps' playing streams from `pactl -f json list sink-inputs`:
/// leaves out our own stream (`ours`), system-internal streams without an
/// application name (speaker DSP chains) and paused ones (corked tabs).
pub fn parse_app_streams(json: &str, ours: Option<u64>) -> Vec<AppStream> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    v.as_array().into_iter().flatten().filter_map(|i| {
        let index = i.get("index")?.as_u64()?;
        let props = i.get("properties")?;
        let app = props.get("application.name").and_then(Value::as_str)?.to_string();
        let corked = i.get("corked").and_then(Value::as_bool).unwrap_or(false);
        let bus_out = props.get("node.name").and_then(Value::as_str).is_some_and(|n| n.starts_with(BUS_OUT_NODE));
        if Some(index) == ours || corked || bus_out || app.contains("[kabelsalat]") { return None; }
        let title = props.get("media.name").and_then(Value::as_str).unwrap_or("").to_string();
        let vols: Vec<f32> = i.get("volume").and_then(Value::as_object).map(|m| m.values()
            .filter_map(|c| c.get("value_percent").and_then(Value::as_str))
            .filter_map(|p| p.trim_end_matches('%').trim().parse::<f32>().ok())
            .map(|p| p / 100.0).collect()).unwrap_or_default();
        let volume = if vols.is_empty() { 1.0 } else { vols.iter().sum::<f32>() / vols.len() as f32 };
        let muted = i.get("mute").and_then(Value::as_bool).unwrap_or(false);
        Some(AppStream { index, app, title, volume, muted })
    }).collect()
}

/// Apply stream commands (latest volume per stream wins). Scan thread only.
pub fn apply_stream_cmds(cmds: Vec<StreamCmd>) {
    let mut last_vol: Vec<(u64, f32)> = Vec::new();
    for c in cmds {
        match c {
            StreamCmd::Volume(i, v) => match last_vol.iter_mut().find(|(x, _)| *x == i) {
                Some(slot) => slot.1 = v,
                None => last_vol.push((i, v)),
            },
            StreamCmd::Mute(i, m) => {
                let _ = std::process::Command::new("pactl")
                    .args(["set-sink-input-mute", &i.to_string(), if m { "1" } else { "0" }]).status();
            }
            StreamCmd::Resync => {}
        }
    }
    for (i, v) in last_vol {
        let pct = format!("{}%", (v.clamp(0.0, 1.5) * 100.0).round() as u32);
        let _ = std::process::Command::new("pactl").args(["set-sink-input-volume", &i.to_string(), &pct]).status();
    }
}

/// Parse `pactl -f json list sinks`.
pub fn parse_sinks(json: &str, default: &str) -> Vec<OutputDevice> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let mut out: Vec<OutputDevice> = v.as_array().into_iter().flatten().filter_map(|s| {
        let raw = s.get("name")?.as_str()?.to_string();
        let name = s.get("description").and_then(Value::as_str).unwrap_or(&raw).to_string();
        Some(OutputDevice { default: raw == default, raw, name })
    }).collect();
    out.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Our playback stream in `pactl -f json list sink-inputs`, matched through
/// its client's process id (`clients_json` = `pactl -f json list clients`):
/// returns (sink-input index, sink index).
pub fn find_our_stream(sink_inputs_json: &str, clients_json: &str, pid: u32) -> Option<(u64, u64)> {
    let clients: Value = serde_json::from_str(clients_json).ok()?;
    // pactl's client "index" is the object serial; a sink-input's top-level
    // "client" field holds the same serial (its "client.id" property is the
    // object id — a different number).
    let ours: Vec<u64> = clients.as_array()?.iter().filter(|c| {
        c.get("properties").and_then(|p| p.get("application.process.id")).and_then(Value::as_str)
            == Some(pid.to_string().as_str())
    }).filter_map(|c| c.get("index").and_then(Value::as_u64)).collect();
    let inputs: Value = serde_json::from_str(sink_inputs_json).ok()?;
    inputs.as_array()?.iter().find_map(|i| {
        // pactl prints this one as a STRING ("29124"), the client index as a number.
        let client = i.get("client").and_then(|c| c.as_u64().or_else(|| c.as_str()?.parse().ok()))?;
        let is_playback = i.get("properties").and_then(|p| p.get("media.class")).and_then(Value::as_str)
            == Some("Stream/Output/Audio");
        (is_playback && ours.contains(&client))
            .then(|| Some((i.get("index")?.as_u64()?, i.get("sink")?.as_u64()?)))?
    })
}

/// Sink-inputs that provably belong to another program: the stream's client
/// is listed with a process id, and `ours` says that process is not us. The
/// system-audio capture records only these, so a stream of ours that the
/// scan has not matched (or cannot match, because `pactl` gave no client
/// list) is never captured and cannot feed back (#27).
pub fn foreign_streams(sink_inputs_json: &str, clients_json: &str, ours: impl Fn(u32) -> bool) -> Vec<u64> {
    let (Ok(clients), Ok(inputs)) = (serde_json::from_str::<Value>(clients_json), serde_json::from_str::<Value>(sink_inputs_json))
    else { return Vec::new() };
    let foreign: Vec<u64> = clients.as_array().into_iter().flatten().filter(|c| {
        c.get("properties").and_then(|p| p.get("application.process.id")).and_then(Value::as_str)
            .and_then(|p| p.parse::<u32>().ok())
            .is_some_and(|p| !ours(p))
    }).filter_map(|c| c.get("index").and_then(Value::as_u64)).collect();
    inputs.as_array().into_iter().flatten().filter_map(|i| {
        let client = i.get("client").and_then(|c| c.as_u64().or_else(|| c.as_str()?.parse().ok()))?;
        foreign.contains(&client).then(|| i.get("index")?.as_u64())?
    }).collect()
}

/// Whether `pid` is this process or one it started (a helper that plays our
/// sound is our output too), by walking the parents in /proc. A process that
/// cannot be read is taken for ours: when in doubt, do not capture.
pub fn is_own_process(pid: u32) -> bool {
    let me = std::process::id();
    let mut p = pid;
    for _ in 0..64 {
        if p == me { return true; }
        if p <= 1 { return false; }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else { return true };
        // "pid (comm) state ppid …": comm may hold spaces and parentheses.
        let Some(ppid) = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok())
        else { return true };
        p = ppid;
    }
    true
}

pub(crate) fn pactl(args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("pactl").args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Where the scan thread should hold our stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// Leave it where PipeWire put it (no choice made).
    Follow,
    /// The user went back to "System default": move it to the default sink
    /// once, then follow.
    Default,
    /// The chosen output; re-applied on every pass (restarts, re-plugs).
    Sink(&'a str),
}

/// What the next pass does, from the stored choice and the one the previous
/// pass saw: a choice that was just dropped moves the stream back once.
pub fn target_for<'a>(wanted: Option<&'a str>, previous: Option<&str>) -> Target<'a> {
    match (wanted, previous) {
        (Some(w), _) => Target::Sink(w),
        (None, Some(_)) => Target::Default,
        (None, None) => Target::Follow,
    }
}

/// The choice the next pass compares against: a dropped choice stays
/// remembered until a pass has seen our stream on the default sink, so the
/// move back is asked for again when PipeWire did not carry it out.
pub fn remember(wanted: Option<String>, previous: Option<String>, seen: &OutputState) -> Option<String> {
    let on_default = seen.current.as_ref()
        .is_some_and(|c| seen.outputs.iter().any(|o| o.default && &o.raw == c));
    match wanted {
        Some(w) => Some(w),
        None if on_default => None,
        None => previous,
    }
}

/// One scan + enforce pass: list outputs, find our stream, move it to the
/// target if it plays elsewhere. Blocking I/O — scan thread only.
pub fn scan_and_apply(target: Target) -> OutputState {
    let default = pactl(&["get-default-sink"]).unwrap_or_default().trim().to_string();
    let sinks_json = pactl(&["-f", "json", "list", "sinks"]).unwrap_or_default();
    let outputs = parse_sinks(&sinks_json, &default);
    let index_to_name = |idx: u64| -> Option<String> {
        let v: Value = serde_json::from_str(&sinks_json).ok()?;
        v.as_array()?.iter().find(|s| s.get("index").and_then(Value::as_u64) == Some(idx))
            .and_then(|s| s.get("name")?.as_str().map(str::to_string))
    };
    let inputs_json = pactl(&["-f", "json", "list", "sink-inputs"]).unwrap_or_default();
    let clients_json = pactl(&["-f", "json", "list", "clients"]).unwrap_or_default();
    let stream = find_our_stream(&inputs_json, &clients_json, std::process::id());
    let streams = parse_app_streams(&inputs_json, stream.map(|(i, _)| i));
    let foreign = foreign_streams(&inputs_json, &clients_json, is_own_process);
    let capturable = streams.iter().map(|s| s.index).filter(|i| foreign.contains(i)).collect();
    let current = stream.and_then(|(_, sink)| index_to_name(sink));
    let wanted = match target {
        Target::Follow => None,
        Target::Default => Some(default.as_str()),
        Target::Sink(w) => Some(w),
    };
    if let (Some((input, _)), Some(w)) = (stream, wanted) {
        let available = outputs.iter().any(|o| o.raw == w);
        if available && current.as_deref() != Some(w) {
            let ok = std::process::Command::new("pactl")
                .args(["move-sink-input", &input.to_string(), w])
                .status().is_ok_and(|s| s.success());
            // `current` stays what pactl listed: PipeWire answers a move with
            // success before it happened, and sometimes without doing it. The
            // next pass reads where the stream really is and asks again.
            if ok { eprintln!("audio out: asked PipeWire to move the stream to {w}"); }
        }
    }
    OutputState { outputs, current, streams, capturable }
}

/// What the self-tests against a real PipeWire share (`audio-out-test`,
/// `capture-test`).
pub(crate) mod selftest {
    pub(crate) use super::pactl;
    use serde_json::Value;
    use std::time::{Duration, Instant};

    pub(crate) const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";

    type Runner = Box<dyn FnMut(&str, &[&str]) -> Option<String>>;

    fn live_run(program: &str, args: &[&str]) -> Option<String> {
        let o = std::process::Command::new(program).args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn non_empty_sink(s: &str) -> Option<String> {
        let t = s.trim();
        (!t.is_empty() && t != "@DEFAULT_SINK@" && t != "@NONE@").then(|| t.to_string())
    }

    /// Parses the sink name from `pw-metadata -n default 0 <key>` output.
    pub(crate) fn parse_pw_metadata_sink(output: &str, key: &str) -> Option<String> {
        let needle = format!("key:'{key}'");
        let line = output.lines().rev().find(|l| l.contains(&needle))?;
        let rest = line.split_once("value:'")?.1.trim_end();
        let json_str = rest
            .rsplit_once("' type:")
            .map(|(v, _)| v)
            .or_else(|| rest.strip_suffix('\''))?;
        let val: Value = serde_json::from_str(json_str).ok()?;
        val.get("name").and_then(Value::as_str).and_then(non_empty_sink)
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum SavedDefault {
        Configured { name: String, fallback: Option<String> },
        Unset { fallback: Option<String> },
        PactlOnly(Option<String>),
    }

    pub(crate) fn classify_saved_default(meta_output: Option<&str>, fallback: Option<String>) -> SavedDefault {
        match meta_output {
            Some(meta) if meta.contains("Found \"default\" metadata") => {
                if let Some(name) = parse_pw_metadata_sink(meta, CONFIGURED_SINK_KEY) {
                    SavedDefault::Configured { name, fallback }
                } else {
                    SavedDefault::Unset { fallback }
                }
            }
            _ => SavedDefault::PactlOnly(fallback),
        }
    }

    /// Unloads the test's null sinks, restores the user's default sink, and
    /// ends the tone, also on a failed check or panic.
    pub(crate) struct Cleanup {
        pub(crate) modules: Vec<String>,
        pub(crate) app: Option<ToneApp>,
        saved_default: SavedDefault,
        default_changed: bool,
        run: Runner,
    }
    impl Cleanup {
        /// Remembers the configured and active default sink, to put it back at the end.
        pub(crate) fn new() -> Self {
            Self::with_runner(Box::new(live_run))
        }
        pub(crate) fn with_runner(mut run: Runner) -> Self {
            let fallback = run("pactl", &["get-default-sink"]).as_deref().and_then(non_empty_sink);
            let meta = run("pw-metadata", &["-n", "default", "0", CONFIGURED_SINK_KEY]);
            let saved_default = classify_saved_default(meta.as_deref(), fallback);
            Self { modules: Vec::new(), app: None, saved_default, default_changed: false, run }
        }
        pub(crate) fn null_sink(&mut self, name: &str, label: &str) -> anyhow::Result<String> {
            let id = (self.run)(
                "pactl",
                &[
                    "load-module",
                    "module-null-sink",
                    &format!("sink_name={name}"),
                    &format!("sink_properties='device.description={label} state.restore-props=false'"),
                ],
            )
            .ok_or_else(|| anyhow::anyhow!("could not load a null sink"))?;
            let id = id.trim().to_string();
            anyhow::ensure!(!id.is_empty(), "empty module id from load-module for {name}");
            // Track before setting properties so Drop still unloads if volume/mute fails.
            self.modules.push(id.clone());
            anyhow::ensure!(
                (self.run)("pactl", &["set-sink-volume", name, "100%"]).is_some(),
                "set-sink-volume 100% failed for {name}"
            );
            anyhow::ensure!(
                (self.run)("pactl", &["set-sink-mute", name, "0"]).is_some(),
                "set-sink-mute 0 failed for {name}"
            );
            Ok(id)
        }
        pub(crate) fn unload_module(&mut self, module_id: &str) -> anyhow::Result<()> {
            anyhow::ensure!(
                (self.run)("pactl", &["unload-module", module_id]).is_some(),
                "unload-module failed for {module_id}"
            );
            self.modules.retain(|m| m != module_id);
            Ok(())
        }
        pub(crate) fn set_default_sink(&mut self, sink: &str) -> anyhow::Result<()> {
            self.default_changed = true;
            anyhow::ensure!(
                (self.run)("pactl", &["set-default-sink", sink]).is_some(),
                "set-default-sink failed"
            );
            Ok(())
        }
        fn restore_default(&mut self) {
            match &self.saved_default {
                SavedDefault::Configured { name, fallback } => {
                    let json = serde_json::json!({ "name": name }).to_string();
                    if (self.run)(
                        "pw-metadata",
                        &["-n", "default", "0", CONFIGURED_SINK_KEY, &json, "Spa:String:JSON"],
                    )
                    .is_none()
                    {
                        if let Some(fb) = fallback {
                            let _ = (self.run)("pactl", &["set-default-sink", fb]);
                        }
                    }
                }
                SavedDefault::Unset { fallback } => {
                    if (self.run)("pw-metadata", &["-n", "default", "-d", "0", CONFIGURED_SINK_KEY]).is_none() {
                        if let Some(fb) = fallback {
                            let _ = (self.run)("pactl", &["set-default-sink", fb]);
                        }
                    }
                }
                SavedDefault::PactlOnly(Some(name)) => {
                    let _ = (self.run)("pactl", &["set-default-sink", name]);
                }
                SavedDefault::PactlOnly(None) => {}
            }
        }
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.app.take();
            if self.default_changed {
                self.restore_default();
            }
            let modules = std::mem::take(&mut self.modules);
            for m in &modules {
                let _ = (self.run)("pactl", &["unload-module", m]);
            }
        }
    }

    pub(crate) fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> anyhow::Result<T> {
        let end = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(v) = f() { return Ok(v); }
            anyhow::ensure!(Instant::now() < end, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// "Another app": a process playing a sine of 0.5 peak to `sink`. It is
    /// started through a shell that exits, so it is not a child of this
    /// process: what we start ourselves counts as our own output.
    pub(crate) struct ToneApp { pid: String, file: std::path::PathBuf }

    impl ToneApp {
        pub(crate) fn stop(self) {}
    }
    impl Drop for ToneApp {
        fn drop(&mut self) {
            let _ = std::process::Command::new("kill").args(["-9", &self.pid]).status();
            let _ = std::fs::remove_file(&self.file);
        }
    }

    pub(crate) fn tone_app(sink: &str, app: &str, title: &str, freq: f32) -> anyhow::Result<ToneApp> {
        // Two minutes of tone, a whole number of cycles per second.
        let second: Vec<u8> = (0..48000).flat_map(|i| {
            let v = (i as f32 * freq / 48000.0).fract();
            (((v * std::f32::consts::TAU).sin() * 0.5 * 32767.0) as i16).to_le_bytes()
        }).collect();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = std::path::PathBuf::from(home).join(".cache");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!("ksalat-tone-{}-{freq}.raw", std::process::id()));
        std::fs::write(&file, second.repeat(120))?;
        let out = std::process::Command::new("sh")
            .args(["-c", r#"pacat --playback --device="$1" --raw --format=s16le --rate=48000 --channels=1 --client-name="$2" --stream-name="$3" --latency-msec=50 "$4" >/dev/null 2>&1 & echo $!"#,
                "sh", sink, app, title])
            .arg(&file)
            .stdin(std::process::Stdio::null())
            .output().map_err(|e| anyhow::anyhow!("pacat: {e}"))?;
        let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        anyhow::ensure!(pid.parse::<u32>().is_ok(), "pacat did not start: {}", String::from_utf8_lossy(&out.stderr));
        Ok(ToneApp { pid, file })
    }
}

// -- buses on their own output (#147) -----------------------------------

use crate::engine::{GraphPub, OutSlot, OutTaps, MAX_OUTS};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Node name prefix of the playback streams of routed buses; the slot
/// number follows. `parse_app_streams` leaves them out by it.
pub const BUS_OUT_NODE: &str = "kabelsalat-bus-out-";

/// Most stereo samples a bus stream lets wait in its ring before it cuts to
/// the newest: the sink runs on its own clock, and a slower one would
/// otherwise let the bus fall further and further behind the master.
fn max_backlog(rate: u32) -> usize {
    (rate as usize / 25) * 2
}

/// Whole frames to throw away so that at most `max` samples wait.
pub fn backlog_cut(waiting: usize, max: usize) -> usize {
    if waiting > max { (waiting - max / 2) & !1 } else { 0 }
}

/// One playing bus stream: `pw-cat` into one sink and the thread that feeds
/// it from the slot's ring.
struct BusStream {
    epoch: u32,
    stop: Arc<AtomicBool>,
    child: std::process::Child,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BusStream {
    /// Has it ended (sink gone, slot handed on)?
    fn ended(&mut self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished()) || !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for BusStream {
    fn drop(&mut self) {
        // Signal stop before killing `pw-cat`, so a feeder parked on an empty
        // ring exits right away and a feeder blocked in `write_all` gets EPIPE.
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

/// Start `pw-cat` on `sink` and feed it slot `k`'s ring while the slot's
/// epoch stays `epoch`. The stream never follows to another sink: when its
/// own goes, PipeWire ends it (`node.dont-reconnect`), and a bus routed to
/// headphones never lands on the PA.
fn start_bus_stream(taps: &OutTaps, k: usize, slot: OutSlot, rate: u32, buffer: u32) -> Option<BusStream> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let sink = slot.sink.as_str().to_string();
    let mut child = std::process::Command::new("pw-cat")
        .args(["--playback", "--raw", "--format", "f32", "--channels", "2"])
        .args(["--rate", &rate.to_string()])
        .args(["--latency", &buffer.max(64).to_string()])
        .args(["--target", &sink])
        .args(["-P", &format!("{{ node.name = {BUS_OUT_NODE}{k} media.name = \"kabelsalat bus\" node.dont-reconnect = true }}")])
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("bus out: cannot start pw-cat: {e}"))
        .ok()?;
    let mut pipe = child.stdin.take()?;
    // The smallest pipe the kernel gives (one page): what waits in it is
    // delay the backlog cut cannot see.
    unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
    let ring = taps.rings[k].clone();
    let epochs = taps.epochs.clone();
    let epoch = slot.epoch;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let thread = std::thread::Builder::new()
        .name(format!("kabelsalat-bus-out-{k}"))
        .spawn(move || {
            // Feeds a sound card: at normal priority a busy desktop delays
            // it past the stream's buffer (rule 29; one promotion, rule 38).
            let _rt = if std::env::var_os("KABELSALAT_NO_RT").is_none() {
                audio_thread_priority::promote_current_thread_to_real_time(rate, rate).ok()
            } else {
                None
            };
            let Ok(mut rx) = ring.lock() else { return };
            let live = || !stop_t.load(Ordering::SeqCst) && epochs[k].load(Ordering::SeqCst) == epoch;
            // What waited in the ring was meant for nobody, or for the sink
            // this slot played before. Whole frames only, here and below:
            // the ring holds L and R in turn.
            let stale = rx.slots() & !1;
            if let Ok(c) = rx.read_chunk(stale) { c.commit_all(); }
            let max = max_backlog(rate);
            let mut bytes: Vec<u8> = Vec::with_capacity(max * 4);
            while live() {
                let waiting = rx.slots() & !1;
                let cut = backlog_cut(waiting, max);
                if cut > 0 {
                    if let Ok(c) = rx.read_chunk(cut) { c.commit_all(); }
                }
                let n = waiting - cut;
                if n == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                let Ok(chunk) = rx.read_chunk(n) else { continue };
                bytes.clear();
                let (a, b) = chunk.as_slices();
                for x in a.iter().chain(b) { bytes.extend_from_slice(&x.to_le_bytes()); }
                chunk.commit_all();
                // Read after the samples left the ring: an unchanged epoch
                // means none of them was pushed for the slot's next sink.
                if !live() || pipe.write_all(&bytes).is_err() { break; }
            }
        })
        .ok()?;
    eprintln!("bus out: output {k} plays to {sink}");
    Some(BusStream { epoch, stop, child, thread: Some(thread) })
}

/// Names of the sinks that exist now. `None` if `pactl` failed. Blocking I/O.
fn sink_names() -> Option<Vec<String>> {
    Some(pactl(&["list", "short", "sinks"])?.lines()
        .filter_map(|l| l.split('\t').nth(1).map(str::to_string)).collect())
}

/// Plays the engine's extra outputs (#147): a thread that keeps one
/// `BusStream` per slot a bus is routed to, for exactly the sink the engine
/// published. A sink that is not there gets no stream (its bus is silent,
/// never on another output); the thread looks again twice a second and
/// starts the stream when the device is back. Dropping it stops everything.
pub struct BusOutputs {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BusOutputs {
    pub fn spawn(taps: OutTaps, graph_pub: Arc<Mutex<GraphPub>>, rate: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("kabelsalat-bus-outs".into())
            .spawn(move || {
                let mut streams: [Option<BusStream>; MAX_OUTS] = std::array::from_fn(|_| None);
                let mut looked = std::time::Instant::now() - std::time::Duration::from_secs(1);
                while !stop_t.load(Ordering::Relaxed) {
                    let slots = graph_pub.lock().map(|g| g.out_slots).unwrap_or([OutSlot::FREE; MAX_OUTS]);
                    for k in 0..MAX_OUTS {
                        let keep = streams[k].as_mut().is_some_and(|s| s.epoch == slots[k].epoch && !s.ended());
                        if !keep { streams[k] = None; }
                    }
                    let active = (0..MAX_OUTS).any(|k| !slots[k].sink.is_none());
                    // pactl only while a bus is routed away, and not in a tight loop:
                    // opens streams whose sink is present and tears down streams whose
                    // sink disappeared (pw-cat with node.dont-reconnect pauses instead
                    // of exiting when its target sink is removed).
                    if active && looked.elapsed() >= std::time::Duration::from_millis(500) {
                        looked = std::time::Instant::now();
                        if let Some(sinks) = sink_names() {
                            for k in 0..MAX_OUTS {
                                if slots[k].sink.is_none() { continue; }
                                let present = sinks.iter().any(|s| s == slots[k].sink.as_str());
                                if !present {
                                    streams[k] = None;
                                } else if streams[k].is_none() {
                                    streams[k] = start_bus_stream(&taps, k, slots[k], rate, crate::bridge::configured_buffer_frames());
                                }
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
            .ok();
        Self { stop, thread }
    }
}

impl Drop for BusOutputs {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

/// `kabelsalat audio-out-test`: the whole of this module against the
/// machine's real PipeWire. It opens a real output stream (silence), starts a
/// second process that plays a tone as "another app", and checks through
/// `pactl` that the stream is found, moved to a chosen output, moved back to
/// the default, left alone for an output that is not there, and that the
/// other app is listed, metered, turned down and muted. Two null sinks are
/// loaded for it and unloaded at the end, so nothing is heard.
pub fn run_test() -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use selftest::{tone_app, until, Cleanup};

    fn sink_of_input(input: u64) -> Option<String> {
        let inputs: Value = serde_json::from_str(&pactl(&["-f", "json", "list", "sink-inputs"])?).ok()?;
        let sink = inputs.as_array()?.iter().find(|i| i.get("index").and_then(Value::as_u64) == Some(input))?
            .get("sink")?.as_u64()?;
        let sinks: Value = serde_json::from_str(&pactl(&["-f", "json", "list", "sinks"])?).ok()?;
        sinks.as_array()?.iter().find(|s| s.get("index").and_then(Value::as_u64) == Some(sink))?
            .get("name")?.as_str().map(str::to_string)
    }

    let info = pactl(&["info"]).ok_or_else(|| anyhow::anyhow!("pactl info failed: no PipeWire/PulseAudio server"))?;
    println!("server: {}", info.lines().find(|l| l.starts_with("Server Name")).unwrap_or("?"));
    let mut cleanup = Cleanup::new();
    let pid = std::process::id();
    let home_s = format!("kabelsalat_test_home_{pid}");
    let other_s = format!("kabelsalat_test_other_{pid}");
    let app_s = format!("Test Browser {pid}");
    let (home, other, app) = (home_s.as_str(), other_s.as_str(), app_s.as_str());
    cleanup.null_sink(home, "Test-Home")?;
    cleanup.null_sink(other, "Test-Other")?;
    cleanup.set_default_sink(home)?;

    // Our own output stream, the way the engine opens it (cpal, default device).
    let device = cpal::default_host().default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let stream = device.build_output_stream(
        config,
        |data: &mut [f32], _: &cpal::OutputCallbackInfo| data.fill(0.0),
        |_err: cpal::Error| {},
        None,
    )?;
    stream.play()?;

    // "Another app": a second process playing a 440 Hz tone.
    cleanup.app = Some(tone_app(home, app, "(2) Backing track - Video", 440.0)?);

    // 1. No choice: our stream is found where PipeWire put it, both outputs are listed.
    let st = until("our stream in the scan", || Some(scan_and_apply(Target::Follow)).filter(|s| s.current.is_some()))?;
    println!("outputs: {:?}", st.outputs.iter().map(|o| (&o.raw, &o.name, o.default)).collect::<Vec<_>>());
    // Not necessarily the default: the session manager restores the sink a
    // stream of this name was last moved to.
    let start = st.current.clone().unwrap_or_default();
    anyhow::ensure!(st.outputs.iter().any(|o| o.raw == start), "our stream plays to a listed sink, got {start:?}");
    anyhow::ensure!(st.outputs.first().is_some_and(|o| o.raw == home && o.default), "the default sink is listed first");
    anyhow::ensure!(st.outputs.iter().any(|o| o.raw == other && o.name == "Test-Other"), "the other sink is listed by its description");
    let clients = pactl(&["-f", "json", "list", "clients"]).unwrap_or_default();
    let (ours, _) = find_our_stream(&pactl(&["-f", "json", "list", "sink-inputs"]).unwrap_or_default(), &clients, std::process::id())
        .ok_or_else(|| anyhow::anyhow!("our stream is not in pactl's list"))?;
    println!("ok: our stream is sink-input {ours} on {start}");

    // The scan thread's loop: one pass per call, with its memory of the choice.
    let mut previous: Option<String> = None;
    let mut pass = |wanted: Option<&str>| {
        let st = scan_and_apply(target_for(wanted, previous.as_deref()));
        previous = remember(wanted.map(str::to_string), previous.take(), &st);
        st
    };

    // 2. A chosen output: the stream moves there, and pactl agrees.
    let mut passes = 0;
    until("pactl showing the stream on the chosen sink", || { passes += 1; pass(Some(other)); (sink_of_input(ours).as_deref() == Some(other)).then_some(()) })?;
    anyhow::ensure!(pass(Some(other)).current.as_deref() == Some(other), "the next pass finds it there");
    println!("ok: chosen output {other}: stream moved after {passes} pass(es) (pactl: {:?})", sink_of_input(ours));

    // 3. An output that is not there: the stream stays.
    let st = pass(Some("kabelsalat_test_unplugged"));
    anyhow::ensure!(st.current.as_deref() == Some(other) && sink_of_input(ours).as_deref() == Some(other),
        "a missing output leaves the stream alone, got {:?}", st.current);
    println!("ok: a missing output leaves the stream on {other}");
    pass(Some(other));

    // 4. Back to "System default": asked for until the stream is there, then left alone.
    let mut passes = 0;
    until("pactl showing the stream back on the default sink", || { passes += 1; pass(None); (sink_of_input(ours).as_deref() == Some(home)).then_some(()) })?;
    pass(None);
    anyhow::ensure!(previous.is_none(), "once it is back, the dropped choice is forgotten");
    anyhow::ensure!(target_for(None, previous.as_deref()) == Target::Follow, "and the stream is left alone from then on");
    println!("ok: back to the system default {home} after {passes} pass(es)");

    // 5. The other app: listed with app and title, never our own stream.
    let theirs = until("the other app's stream", || scan_and_apply(Target::Follow).streams.into_iter().find(|s| s.app == app))?;
    println!("app stream: {theirs:?}");
    anyhow::ensure!(theirs.title == "(2) Backing track - Video", "title, got {:?}", theirs.title);
    anyhow::ensure!((theirs.volume - 1.0).abs() < 0.02 && !theirs.muted, "starts at 100 %, unmuted: {theirs:?}");
    let st = scan_and_apply(Target::Follow);
    anyhow::ensure!(st.streams.iter().all(|s| s.index != ours), "our own stream is not an app strip");
    anyhow::ensure!(st.capturable.contains(&theirs.index) && !st.capturable.contains(&ours),
        "the capture may record the other app and never our own stream, got {:?}", st.capturable);

    // 6. Its meter moves while it plays.
    let levels: StreamLevels = Default::default();
    let mut meters = StreamMeters::new(levels.clone());
    meters.sync(std::slice::from_ref(&theirs));
    let level = until("a meter reading of the tone", || Some(stream_level(&levels, theirs.index)).filter(|l| *l > 0.2))?;
    anyhow::ensure!((level - 0.5).abs() < 0.1, "a 0.5 peak tone meters near 0.5, got {level}");
    println!("ok: meter reads {level:.3} for a 0.5 peak tone");

    // 7. Volume and mute reach the stream.
    apply_stream_cmds(vec![StreamCmd::Volume(theirs.index, 0.9), StreamCmd::Volume(theirs.index, 0.5), StreamCmd::Mute(theirs.index, true)]);
    let now = until("the new volume and mute in the scan", || scan_and_apply(Target::Follow).streams.into_iter()
        .find(|s| s.index == theirs.index).filter(|s| (s.volume - 0.5).abs() < 0.02 && s.muted))?;
    println!("ok: volume {:.2}, muted {} (the latest of two volume changes won)", now.volume, now.muted);
    apply_stream_cmds(vec![StreamCmd::Volume(theirs.index, 1.0), StreamCmd::Mute(theirs.index, false)]);
    until("volume and mute restored", || scan_and_apply(Target::Follow).streams.into_iter()
        .find(|s| s.index == theirs.index).filter(|s| (s.volume - 1.0).abs() < 0.02 && !s.muted))?;

    // 8. The app stops: its strip and its meter reader go.
    if let Some(app) = cleanup.app.take() { app.stop(); }
    let st = until("the stream leaving the scan", || Some(scan_and_apply(Target::Follow)).filter(|s| s.streams.iter().all(|x| x.index != theirs.index)))?;
    meters.sync(&st.streams);
    anyhow::ensure!(!meters.readers.contains_key(&theirs.index), "the meter reader of an ended stream is stopped");
    anyhow::ensure!(stream_level(&levels, theirs.index) == 0.0, "an ended stream reads silent");
    println!("ok: the app stopped, its strip and meter are gone");

    drop(stream);
    bus_output_check(home, other, &mut cleanup)?;
    println!("audio-out-test: PASS");
    Ok(())
}

/// Amplitude of the `hz` component of mono 48 kHz samples (Goertzel).
fn tone_amp(buf: &[f32], hz: f32) -> f32 {
    let w = std::f32::consts::TAU * hz / 48000.0;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, x) in buf.iter().enumerate() {
        let p = (w * i as f32) as f64;
        re += *x as f64 * p.cos();
        im += *x as f64 * p.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / buf.len().max(1) as f64) as f32
}

/// One second of what a sink plays, read from its monitor (mono, 48 kHz).
fn capture_sink(sink: &str, secs: f32) -> anyhow::Result<Vec<f32>> {
    use std::io::Read;
    let mut child = std::process::Command::new("parec")
        .args([&format!("--device={sink}.monitor"), "--format=float32le", "--rate=48000", "--channels=1",
            "--latency-msec=20", "--client-name=kabelsalat-test-probe"])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
        .spawn().map_err(|e| anyhow::anyhow!("parec: {e}"))?;
    let mut out = child.stdout.take().expect("piped stdout");
    // The first 0.3 s are the probe starting up.
    let (skip, want) = (14400usize, (secs * 48000.0) as usize);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![0u8; (skip + want) * 4];
        let _ = tx.send(out.read_exact(&mut bytes).map(|_| bytes));
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(12));
    let _ = child.kill();
    let _ = child.wait();
    let bytes = got.map_err(|_| anyhow::anyhow!("no audio from {sink}.monitor within 12 s"))??;
    Ok(bytes[skip * 4..].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// `audio-out-test`, second half (#147): a real engine on a real output
/// stream, two rigs that play one tone each (440 Hz and 1130 Hz) and an aux
/// bus, and three null sinks read back through their monitors. A bus routed
/// to its own output must arrive there and nowhere else, stay silent while
/// its device is gone, come back with it, and return to the master.
fn bus_output_check(home: &str, other: &str, cleanup: &mut selftest::Cleanup) -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, AudioInChannel, Command, DspKind, Graph, NodeKind, PortKind, PortRef, SendSource, SinkName};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::time::{Duration, Instant};
    const A_HZ: f32 = 440.0;
    const B_HZ: f32 = 1130.0;
    let phones = format!("kabelsalat_test_phones_{}", std::process::id());

    let phones_module = cleanup.null_sink(&phones, "Test-Phones")?;

    // Two rigs, each an Audio In fed its tone in the output callback, wired
    // to its own master; the aux bus comes in by command, as in the app.
    let mut g = Graph::default();
    g.reserve(32, 32);
    let mut ins = [0u32; 2];
    for rig in 0..2u8 {
        let a = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.set_rig(a, rig);
        g.set_rig(m, rig);
        g.connect_with_id(1 + rig as u32, PortRef { node: a, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        ins[rig as usize] = a;
    }
    let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let (rate, ch) = (config.sample_rate as f32, config.channels as usize);
    let (mut bus, drain) = command_bus();
    let mut engine = crate::engine::GraphEngine::new(g, drain, rate as f64, vec![]);
    let graph_pub = Arc::new(Mutex::new(GraphPub::default()));
    engine.set_graph_pub(graph_pub.clone());
    let (mut side_tx, side_rx) = rtrb::RingBuffer::new(8);
    engine.set_audio_in_channel(side_rx);
    let mut feeds = Vec::new();
    for (i, (hz, amp)) in [(A_HZ, 0.3f32), (B_HZ, 0.2)].into_iter().enumerate() {
        let (tx, rx) = rtrb::RingBuffer::<f32>::new(16384);
        let _ = side_tx.push((ins[i], rx));
        feeds.push((tx, hz, amp));
    }
    let taps = engine.take_out_taps().expect("the engine's extra outputs");
    let _player = BusOutputs::spawn(taps, graph_pub.clone(), rate as u32);
    // Rig 1 can play noise bursts instead of its tone (the delay measurement).
    let bursts = Arc::new(AtomicBool::new(false));
    let bursts_cb = bursts.clone();
    let (mut frame_no, mut seed) = (0u64, 0x2545_F491u32);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let burst = bursts_cb.load(Ordering::Relaxed);
            for _ in 0..data.len() / ch {
                for (i, (tx, hz, amp)) in feeds.iter_mut().enumerate() {
                    let mut v = (std::f32::consts::TAU * *hz * frame_no as f32 / rate).sin() * *amp;
                    if burst && i == 0 {
                        // 10 ms of noise four times a second.
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        let noise = (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0;
                        v = if (frame_no % (rate as u64 / 4)) < (rate as u64 / 100) { noise * 0.3 } else { 0.0 };
                    }
                    let _ = tx.push(v);
                    let _ = tx.push(v);
                }
                frame_no += 1;
            }
            engine.render(data, ch);
        },
        |_err: cpal::Error| {},
        None,
    )?;
    stream.play()?;

    fn poll<T>(what: &str, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
        let end = Instant::now() + Duration::from_secs(25);
        loop {
            if let Some(v) = f()? { return Ok(v); }
            anyhow::ensure!(Instant::now() < end, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // (440 Hz, 1130 Hz) amplitude on each of home, other, phones.
    let hear = || -> anyhow::Result<[(f32, f32); 3]> {
        let mut out = [(0.0, 0.0); 3];
        for (i, sink) in [home, other, phones.as_str()].into_iter().enumerate() {
            let buf = capture_sink(sink, 1.0)?;
            out[i] = (tone_amp(&buf, A_HZ), tone_amp(&buf, B_HZ));
        }
        Ok(out)
    };
    const HEARD: f32 = 0.05;
    const SILENT: f32 = 0.002; // -54 dBFS
    let show = |h: &[(f32, f32); 3]| format!("home {:.3}/{:.3}, other {:.3}/{:.3}, phones {:.3}/{:.3} (440 Hz/1130 Hz)",
        h[0].0, h[0].1, h[1].0, h[1].1, h[2].0, h[2].1);
    let bus_streams = || pactl(&["-f", "json", "list", "sink-inputs"]).unwrap_or_default().matches(BUS_OUT_NODE).count();

    // The master stream belongs on `home` for the whole check.
    poll("the engine's stream on the home sink", || Ok((scan_and_apply(Target::Sink(home)).current.as_deref() == Some(home)).then_some(())))?;

    // 9. Nothing routed: both rigs in the master, the other sinks silent.
    let h = poll("both rigs on the home sink", || { let h = hear()?; Ok((h[0].0 > HEARD && h[0].1 > HEARD).then_some(h)) })?;
    anyhow::ensure!(h[1].0 < SILENT && h[1].1 < SILENT && h[2].0 < SILENT && h[2].1 < SILENT, "unrouted: only the home sink plays: {}", show(&h));
    anyhow::ensure!(bus_streams() == 0, "no bus stream without a routed bus");
    println!("ok: nothing routed: {}", show(&h));

    // 10. Rig 2's bus to `other`: it arrives there and leaves the master.
    anyhow::ensure!(bus.send(Command::SetBusOutput { bus: SendSource::Rig(1), sink: Some(SinkName::new(other)) }));
    let h = poll("rig 2 on the other sink only", || { let h = hear()?; Ok((h[1].1 > HEARD && h[0].1 < SILENT).then_some(h)) })?;
    anyhow::ensure!(h[0].0 > HEARD && h[1].0 < SILENT && h[2].0 < SILENT && h[2].1 < SILENT,
        "rig 1 stays in the master and reaches no other sink: {}", show(&h));
    println!("ok: rig 2 routed to {other}: {}", show(&h));

    // 11. An aux bus fed by rig 1 (pre fader) to `phones`: a monitor mix.
    anyhow::ensure!(bus.send(Command::AddNode { kind: NodeKind::Dsp(DspKind::Bus), name: name_bytes("MON"), rig: 0, wire: true }));
    let aux = poll("the aux bus in the engine", || Ok(graph_pub.lock().unwrap().graph.nodes.iter()
        .find(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Bus))).map(|n| n.id)))?;
    bus.send(Command::SetSend { from: SendSource::Rig(0), bus: aux, level: Some(0.5), pre: Some(true) });
    bus.send(Command::SetBusOutput { bus: SendSource::Node(aux), sink: Some(SinkName::new(&phones)) });
    let h = poll("the aux bus on the phones sink", || { let h = hear()?; Ok((h[2].0 > HEARD).then_some(h)) })?;
    anyhow::ensure!(h[2].1 < SILENT && h[1].0 < SILENT && h[1].1 > HEARD && h[0].0 > HEARD && h[0].1 < SILENT,
        "each bus plays on its own sink only: {}", show(&h));
    let dry = h[0].0;
    anyhow::ensure!(bus_streams() == 2, "one stream per routed sink, got {}", bus_streams());
    println!("ok: aux bus routed to {phones}: {}", show(&h));

    // 12. The phones go away: their bus is silent, and nobody else gets it.
    cleanup.unload_module(&phones_module)?;
    poll("the phones stream ending", || Ok((bus_streams() == 1).then_some(())))?;
    std::thread::sleep(Duration::from_millis(1200));
    anyhow::ensure!(bus_streams() == 1, "the stream of a missing output is not reopened elsewhere");
    let (h0, h1) = (capture_sink(home, 1.0)?, capture_sink(other, 1.0)?);
    let (home_a, other_a) = (tone_amp(&h0, A_HZ), tone_amp(&h1, A_HZ));
    anyhow::ensure!((home_a - dry).abs() < dry * 0.1 && other_a < SILENT,
        "the bus of a missing output lands nowhere: home 440 Hz {home_a:.3} (dry {dry:.3}), other {other_a:.3}");
    println!("ok: {phones} unplugged: its bus is silent (home 440 Hz {home_a:.3} = the dry rig, other {other_a:.3})");

    // 13. They come back: the bus plays there again by itself.
    let _ = cleanup.null_sink(&phones, "Test-Phones")?;
    let t0 = Instant::now();
    let h = poll("the aux bus back on the phones sink", || { let h = hear()?; Ok((h[2].0 > HEARD).then_some(h)) })?;
    println!("ok: {phones} re-plugged: heard again within {:.1} s: {}", t0.elapsed().as_secs_f32(), show(&h));

    // 14. How far apart master and bus arrive. Both on one sink: rig 1 plays
    // noise bursts, dry through the master and through the bus into `home`,
    // so the monitor holds two copies and their distance is the peak of its
    // autocorrelation.
    bus.send(Command::SetRigMix { rig: 1, vol: None, pan: None, mute: Some(true), solo: None });
    bus.send(Command::SetBusOutput { bus: SendSource::Node(aux), sink: Some(SinkName::new(home)) });
    bus.send(Command::SetSend { from: SendSource::Rig(0), bus: aux, level: Some(1.0), pre: Some(true) });
    bursts.store(true, Ordering::Relaxed);
    poll("the bus stream on the home sink", || Ok((bus_streams() == 2).then_some(())))?;
    std::thread::sleep(Duration::from_millis(1500));
    let mut gaps = Vec::new();
    for _ in 0..5 {
        let buf = capture_sink(home, 2.0)?;
        let r0: f64 = buf.iter().map(|x| (*x as f64).powi(2)).sum();
        let (mut best, mut best_r) = (0usize, 0.0f64);
        for lag in 12..4800 {
            let r: f64 = buf[lag..].iter().zip(&buf).map(|(a, b)| *a as f64 * *b as f64).sum();
            if r > best_r { (best, best_r) = (lag, r); }
        }
        anyhow::ensure!(r0 > 0.0, "the bursts are not heard on {home}");
        gaps.push((best as f32 / 48.0, (best_r / r0) as f32));
    }
    bursts.store(false, Ordering::Relaxed);
    let clear: Vec<f32> = gaps.iter().filter(|g| g.1 > 0.15).map(|g| g.0).collect();
    if clear.is_empty() {
        println!("note: master and bus arrive less than 0.25 ms apart, or the copies did not correlate: {gaps:?}");
    } else {
        let mut sorted = clear.clone();
        sorted.sort_by(f32::total_cmp);
        println!("measured: master and bus copies arrive {:.1} ms apart on one sink (median of {}; min {:.1}, max {:.1})",
            sorted[sorted.len() / 2], sorted.len(), sorted[0], sorted[sorted.len() - 1]);
    }

    // 15. Everything back into the master: no bus stream is left.
    bus.send(Command::SetRigMix { rig: 1, vol: None, pan: None, mute: Some(false), solo: None });
    bus.send(Command::SetSend { from: SendSource::Rig(0), bus: aux, level: Some(0.0), pre: Some(false) });
    bus.send(Command::SetBusOutput { bus: SendSource::Node(aux), sink: None });
    bus.send(Command::SetBusOutput { bus: SendSource::Rig(1), sink: None });
    let h = poll("both rigs back in the master", || { let h = hear()?; Ok((h[0].1 > HEARD && h[1].1 < SILENT).then_some(h)) })?;
    poll("the bus streams ending", || Ok((bus_streams() == 0).then_some(())))?;
    anyhow::ensure!(h[0].0 > HEARD && h[2].0 < SILENT, "back in the master: {}", show(&h));
    println!("ok: back in the master: {}", show(&h));
    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_meter_peaks_fall_and_go_silent_when_readings_stop() {
        let bytes: Vec<u8> = [0.1f32, -0.5, 0.25, f32::NAN].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(chunk_peak(&bytes), 0.5, "abs peak, NaN ignored");
        assert_eq!(smooth_meter(0.0, 0.5), 0.5, "instant rise");
        assert_eq!(smooth_meter(0.5, 0.0), 0.25, "falls gradually");
        let levels: StreamLevels = Default::default();
        assert_eq!(stream_level(&levels, 7), 0.0, "unknown stream is silent");
        levels.lock().unwrap().insert(7, (0.4, std::time::Instant::now()));
        assert_eq!(stream_level(&levels, 7), 0.4);
        let old = std::time::Instant::now() - std::time::Duration::from_millis(400);
        levels.lock().unwrap().insert(7, (0.4, old));
        assert_eq!(stream_level(&levels, 7), 0.0, "a paused tab stops sending: shown silent, not frozen");
    }

    #[test]
    fn dropping_the_chosen_output_goes_back_to_the_default_once() {
        assert_eq!(target_for(Some("m2"), None), Target::Sink("m2"));
        assert_eq!(target_for(Some("m2"), Some("speakers")), Target::Sink("m2"));
        assert_eq!(target_for(None, Some("m2")), Target::Default, "System default after a choice moves the stream back");
        assert_eq!(target_for(None, None), Target::Follow, "no choice ever made: never move the stream");
        // The dropped choice is kept until the stream is seen on the default.
        let seen = |current: &str| OutputState {
            outputs: vec![
                OutputDevice { raw: "speakers".into(), name: "Speakers".into(), default: true },
                OutputDevice { raw: "m2".into(), name: "M2".into(), default: false },
            ],
            current: Some(current.into()),
            streams: Vec::new(),
            capturable: Vec::new(),
        };
        let m2 = || Some("m2".to_string());
        assert_eq!(remember(None, m2(), &seen("m2")), m2(), "still on the old card: ask again next pass");
        assert_eq!(remember(None, m2(), &OutputState::default()), m2(), "stream not found yet: keep asking");
        assert_eq!(remember(None, m2(), &seen("speakers")), None, "arrived: done");
        assert_eq!(remember(m2(), None, &seen("speakers")), m2());
    }

    #[test]
    fn sinks_parse_default_first() {
        let json = r#"[
          {"index":64,"name":"alsa_output.platform-sound.HiFi__Headphones__sink","description":"Headphones"},
          {"index":25144,"name":"alsa_output.usb-MOTU_M2_M2MA03B7IC-00.HiFi__Line__sink","description":"M Series Headphone + Monitor Out"},
          {"index":85,"name":"audio_effect.j293-convolver","description":"MacBook Speakers"}
        ]"#;
        let outs = parse_sinks(json, "alsa_output.usb-MOTU_M2_M2MA03B7IC-00.HiFi__Line__sink");
        assert_eq!(outs[0].name, "M Series Headphone + Monitor Out");
        assert!(outs[0].default && !outs[1].default);
        assert_eq!(outs.len(), 3);
        assert!(parse_sinks("nope", "").is_empty());
    }

    #[test]
    fn app_streams_list_playing_apps_but_not_us_or_paused_tabs() {
        // Real shape of `pactl -f json list sink-inputs` (trimmed).
        let json = r#"[
          {"index":86,"corked":true,"mute":false,"volume":{"aux0":{"value_percent":"100%"}},"properties":{"media.name":"MacBook Pro J293 Speakers DSP Out"}},
          {"index":11485,"corked":true,"mute":false,"volume":{"front-left":{"value_percent":"100%"},"front-right":{"value_percent":"100%"}},"properties":{"application.name":"Zen","media.name":"Paused tab"}},
          {"index":28159,"corked":false,"mute":false,"volume":{"front-left":{"value_percent":"80%"},"front-right":{"value_percent":"60%"}},"properties":{"application.name":"Zen","media.name":"(2) Neo Soul Guitar Backing Track in A minor"}},
          {"index":28179,"corked":false,"mute":false,"volume":{"front-left":{"value_percent":"100%"}},"properties":{"application.name":"PipeWire ALSA [kabelsalat]","media.name":"ALSA Playback"}},
          {"index":30000,"corked":false,"mute":true,"volume":{"mono":{"value_percent":"100%"}},"properties":{"application.name":"Spotify","media.name":"Song"}}
        ]"#;
        let s = parse_app_streams(json, Some(28179));
        assert_eq!(s.iter().map(|x| x.index).collect::<Vec<_>>(), vec![28159, 30000], "playing apps only");
        assert_eq!(s[0].app, "Zen");
        assert!((s[0].volume - 0.7).abs() < 1e-6, "channel average: {}", s[0].volume);
        assert!(s[1].muted);
        assert!(parse_app_streams("[]", None).is_empty());
    }

    #[test]
    fn our_stream_is_found_through_its_clients_process_id() {
        // Real field names: the stream carries client.id, the client the pid.
        let clients = r#"[
          {"index":26711,"properties":{"object.id":"160","application.process.id":"1868741","application.name":"PipeWire ALSA [kabelsalat]"}},
          {"index":26800,"properties":{"object.id":"171","application.process.id":"999","application.name":"PipeWire ALSA [kabelsalat]"}}
        ]"#;
        let inputs = r#"[
          {"index":26700,"client":"26800","sink":25144,"properties":{"client.id":"171","media.class":"Stream/Output/Audio"}},
          {"index":26713,"client":"26711","sink":25144,"properties":{"client.id":"160","media.class":"Stream/Output/Audio"}}
        ]"#;
        assert_eq!(find_our_stream(inputs, clients, 1868741), Some((26713, 25144)), "the other kabelsalat process is not us");
        assert_eq!(find_our_stream(inputs, clients, 4242), None);
        assert_eq!(find_our_stream("x", clients, 1868741), None);
    }

    #[test]
    fn bus_output_streams_stay_out_of_app_streams_and_backlog_cuts_whole_frames() {
        let json = r#"[
          {"index":31000,"corked":false,"mute":false,"volume":{"front-left":{"value_percent":"100%"}},"properties":{"application.name":"pw-cat","node.name":"kabelsalat-bus-out-0","media.name":"kabelsalat bus"}},
          {"index":31001,"corked":false,"mute":false,"volume":{"front-left":{"value_percent":"100%"}},"properties":{"application.name":"Spotify","media.name":"Song"}}
        ]"#;
        let s = parse_app_streams(json, None);
        assert_eq!(s.iter().map(|x| x.index).collect::<Vec<_>>(), vec![31001]);
        assert_eq!(backlog_cut(100, 200), 0);
        assert_eq!(backlog_cut(200, 200), 0);
        assert_eq!(backlog_cut(205, 200), 104, "cuts whole stereo frames down to half max");
        assert_eq!(backlog_cut(205, 200) % 2, 0);
    }

    #[test]
    fn only_streams_known_to_be_another_process_are_capture_targets() {
        // Real output of `pactl -f json list clients` and `list sink-inputs`
        // during a `capture-test` run (PipeWire 1.6.2, trimmed to the fields
        // read): the cpal output stream of process 312057 and a `pacat` of
        // process 312100. The last client and the last two streams are added
        // by hand: what nobody can place.
        let clients = r#"[
          {"index":33,"driver":"PipeWire","properties":{"application.name":"WirePlumber","application.process.binary":"wireplumber","application.process.id":"1881593"}},
          {"index":39,"driver":"PipeWire","properties":{"application.name":"pipewire","application.process.binary":"pipewire","application.process.id":"1881626"}},
          {"index":6533,"driver":"PipeWire","properties":{"application.name":"PipeWire ALSA [kabelsalat]","application.process.binary":"kabelsalat","application.process.id":"312057"}},
          {"index":6539,"driver":"PipeWire","properties":{"application.name":"Test Browser 312057","application.process.binary":"pacat","application.process.id":"312100"}},
          {"index":6573,"driver":"PipeWire","properties":{"application.name":"kabelsalat-capture","application.process.binary":"pacat","application.process.id":"312291"}},
          {"index":7000,"driver":"PipeWire","properties":{"application.name":"No pid"}}
        ]"#;
        let inputs = r#"[
          {"index":6534,"sink":6526,"client":"6533","corked":false,"properties":{"application.name":"PipeWire ALSA [kabelsalat]","media.class":"Stream/Output/Audio","media.name":"ALSA Playback"}},
          {"index":6540,"sink":6519,"client":"6539","corked":false,"properties":{"application.name":"Test Browser 312057","media.class":"Stream/Output/Audio","media.name":"(2) Backing track - Video"}},
          {"index":7001,"sink":6519,"client":"7000","corked":false,"properties":{"application.name":"No pid"}},
          {"index":7002,"sink":6519,"properties":{"media.name":"no client"}}
        ]"#;
        assert_eq!(foreign_streams(inputs, clients, |p| p == 312057), vec![6540], "not ours, and not what nobody can place");
        assert_eq!(foreign_streams(inputs, clients, |p| p == 312100), vec![6534], "seen from the other process");
        assert!(foreign_streams(inputs, "", |p| p == 312057).is_empty(), "no client list: nothing is known to be foreign");
        assert!(foreign_streams("x", clients, |p| p == 312057).is_empty());
    }

    #[test]
    fn a_process_we_started_is_ours_and_others_are_not() {
        assert!(is_own_process(std::process::id()));
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(is_own_process(child.id()), "a helper process plays our sound");
        let _ = child.kill();
        let _ = child.wait();
        assert!(!is_own_process(1), "init is not ours");
        assert!(!is_own_process(std::os::unix::process::parent_id()), "nor is our parent");
        assert!(is_own_process(u32::MAX), "a process that cannot be read is not captured");
    }

    #[test]
    fn parse_pw_metadata_sink_reads_configured_default_and_handles_unset() {
        use selftest::{classify_saved_default, parse_pw_metadata_sink, SavedDefault, CONFIGURED_SINK_KEY};

        let spaced = "Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.sink' value:'{ \"name\": \"alsa_output.usb-MOTU_M2_M2MA03B7IC-00.HiFi__Line__sink\" }' type:'Spa:String:JSON'\n";
        assert_eq!(
            parse_pw_metadata_sink(spaced, CONFIGURED_SINK_KEY).as_deref(),
            Some("alsa_output.usb-MOTU_M2_M2MA03B7IC-00.HiFi__Line__sink")
        );
        assert_eq!(
            classify_saved_default(Some(spaced), Some("audio_effect.j293-convolver".into())),
            SavedDefault::Configured {
                name: "alsa_output.usb-MOTU_M2_M2MA03B7IC-00.HiFi__Line__sink".into(),
                fallback: Some("audio_effect.j293-convolver".into()),
            }
        );

        let compact = "Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"speakers\"}'\n";
        assert_eq!(parse_pw_metadata_sink(compact, CONFIGURED_SINK_KEY).as_deref(), Some("speakers"));

        let multi = "Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"old\"}' type:'Spa:String:JSON'\nupdate: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"latest\"}' type:'Spa:String:JSON'\n";
        assert_eq!(parse_pw_metadata_sink(multi, CONFIGURED_SINK_KEY).as_deref(), Some("latest"));

        let unset = "Found \"default\" metadata 41\n";
        assert_eq!(parse_pw_metadata_sink(unset, CONFIGURED_SINK_KEY), None);
        assert_eq!(
            classify_saved_default(Some(unset), Some("speakers".into())),
            SavedDefault::Unset { fallback: Some("speakers".into()) }
        );

        for placeholder in ["", "@DEFAULT_SINK@", "@NONE@"] {
            let line = format!("Found \"default\" metadata 41\nupdate: id:0 key:'{CONFIGURED_SINK_KEY}' value:'{{\"name\":\"{placeholder}\"}}' type:'Spa:String:JSON'\n");
            assert_eq!(parse_pw_metadata_sink(&line, CONFIGURED_SINK_KEY), None);
            assert_eq!(
                classify_saved_default(Some(&line), None),
                SavedDefault::Unset { fallback: None }
            );
        }

        assert_eq!(
            classify_saved_default(None, Some("speakers".into())),
            SavedDefault::PactlOnly(Some("speakers".into()))
        );
        assert_eq!(classify_saved_default(None, None), SavedDefault::PactlOnly(None));
    }

    #[test]
    fn selftest_null_sink_sets_full_volume_unmuted_and_cleanup_restores_default_on_drop() {
        use selftest::Cleanup;
        use std::cell::RefCell;
        use std::rc::Rc;

        type Log = Rc<RefCell<Vec<(String, Vec<String>)>>>;

        let make_runner = |log: Log,
                           meta: Option<&'static str>,
                           default_sink: Option<&'static str>,
                           fail_vol: bool,
                           fail_pw_restore: bool| {
            let mut next_id = 10u32;
            Box::new(move |prog: &str, args: &[&str]| -> Option<String> {
                let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
                log.borrow_mut().push((prog.to_string(), owned));
                match (prog, args.first().copied()) {
                    ("pactl", Some("get-default-sink")) => default_sink.map(str::to_string),
                    ("pw-metadata", Some("-n")) if args.contains(&"-d") => (!fail_pw_restore).then(String::new),
                    ("pw-metadata", Some("-n")) if args.len() == 4 => meta.map(str::to_string),
                    ("pw-metadata", Some("-n")) => (!fail_pw_restore).then(String::new),
                    ("pactl", Some("load-module")) => {
                        let id = next_id;
                        next_id += 1;
                        Some(format!("{id}\n"))
                    }
                    ("pactl", Some("set-sink-volume")) if fail_vol => None,
                    ("pactl", _) => Some(String::new()),
                    _ => Some(String::new()),
                }
            })
        };

        // 1. Normal path: null_sink sets state.restore-props=false, 100%, unmuted;
        //    unload_module removes ID; Drop restores pw-metadata default BEFORE unloading.
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        {
            let mut c = Cleanup::with_runner(make_runner(
                log.clone(),
                Some("Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.sink' value:'{ \"name\": \"motu_m2\" }' type:'Spa:String:JSON'\n"),
                Some("speakers"),
                false,
                false,
            ));
            let m1 = c.null_sink("home_1", "Test-Home").unwrap();
            let m2 = c.null_sink("other_1", "Test-Other").unwrap();
            assert_eq!(m1, "10");
            assert_eq!(m2, "11");
            c.set_default_sink("home_1").unwrap();
            c.unload_module(&m2).unwrap();
        }
        let calls = log.borrow();
        let load_call = calls.iter().find(|(p, a)| p == "pactl" && a[0] == "load-module").unwrap();
        assert!(
            load_call.1.iter().any(|a| a == "sink_properties='device.description=Test-Home state.restore-props=false'"),
            "load-module must disable WirePlumber prop restore: {:?}", load_call
        );
        assert!(calls.iter().any(|(p, a)| p == "pactl" && a == &["set-sink-volume", "home_1", "100%"]));
        assert!(calls.iter().any(|(p, a)| p == "pactl" && a == &["set-sink-mute", "home_1", "0"]));
        let restore_pos = calls.iter().position(|(p, a)| {
            p == "pw-metadata" && a == &["-n", "default", "0", "default.configured.audio.sink", "{\"name\":\"motu_m2\"}", "Spa:String:JSON"]
        }).expect("configured default sink restored via pw-metadata");
        let unload_10_pos = calls.iter().position(|(p, a)| p == "pactl" && a == &["unload-module", "10"])
            .expect("remaining module 10 unloaded on drop");
        assert!(restore_pos < unload_10_pos, "default sink restored before unloading modules");
        assert_eq!(
            calls.iter().filter(|(p, a)| p == "pactl" && a == &["unload-module", "11"]).count(),
            1,
            "explicitly unloaded module 11 is not unloaded a second time on drop"
        );
        drop(calls);

        // 2. Error path when set-sink-volume fails: module is still unloaded on drop,
        //    and default restore is skipped because set_default_sink was not called.
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        {
            let mut c = Cleanup::with_runner(make_runner(
                log.clone(),
                Some("Found \"default\" metadata 41\n"),
                Some("speakers"),
                true,
                false,
            ));
            let err = c.null_sink("home_err", "Test-Home").unwrap_err();
            assert!(err.to_string().contains("set-sink-volume 100% failed"));
        }
        let calls = log.borrow();
        assert!(calls.iter().any(|(p, a)| p == "pactl" && a == &["unload-module", "10"]));
        assert!(!calls.iter().any(|(p, a)| p == "pw-metadata" && a.contains(&"-d".to_string())));
        drop(calls);

        // 3. Panic path after set_default_sink: Drop still restores default & unloads.
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut c = Cleanup::with_runner(make_runner(
                log.clone(),
                Some("Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"motu_m2\"}' type:'Spa:String:JSON'\n"),
                Some("speakers"),
                false,
                false,
            ));
            c.null_sink("home_panic", "Test-Home").unwrap();
            c.set_default_sink("home_panic").unwrap();
            panic!("boom");
        }));
        assert!(res.is_err());
        let calls = log.borrow();
        assert!(calls.iter().any(|(p, a)| p == "pw-metadata" && a.contains(&"{\"name\":\"motu_m2\"}".to_string())));
        assert!(calls.iter().any(|(p, a)| p == "pactl" && a == &["unload-module", "10"]));
        drop(calls);

        // 4. Unset configured default: deletes default.configured.audio.sink on drop.
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        {
            let mut c = Cleanup::with_runner(make_runner(
                log.clone(),
                Some("Found \"default\" metadata 41\n"),
                Some("speakers"),
                false,
                false,
            ));
            c.set_default_sink("home_unset").unwrap();
        }
        assert!(log.borrow().iter().any(|(p, a)| {
            p == "pw-metadata" && a == &["-n", "default", "-d", "0", "default.configured.audio.sink"]
        }));

        // 5. PactlOnly(Some(...)) vs PactlOnly(None): restores via pactl or skips gracefully.
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        {
            let mut c = Cleanup::with_runner(make_runner(log.clone(), None, Some("speakers"), false, false));
            c.set_default_sink("home_pa").unwrap();
        }
        assert!(log.borrow().iter().any(|(p, a)| p == "pactl" && a == &["set-default-sink", "speakers"]));

        let log: Log = Rc::new(RefCell::new(Vec::new()));
        {
            let mut c = Cleanup::with_runner(make_runner(log.clone(), None, Some("@DEFAULT_SINK@"), false, false));
            c.set_default_sink("home_none").unwrap();
        }
        assert_eq!(
            log.borrow().iter().filter(|(p, a)| p == "pactl" && a[0] == "set-default-sink").count(),
            1,
            "no meaningful prior default: only the test's own set-default-sink ran"
        );
    }
}
