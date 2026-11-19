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
        if Some(index) == ours || corked || app.contains("[kabelsalat]") { return None; }
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

fn pactl(args: &[&str]) -> Option<String> {
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
    let stream = find_our_stream(
        &inputs_json,
        &pactl(&["-f", "json", "list", "clients"]).unwrap_or_default(),
        std::process::id(),
    );
    let streams = parse_app_streams(&inputs_json, stream.map(|(i, _)| i));
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
    OutputState { outputs, current, streams }
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
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Unloads the test's null sinks and ends the tone, also on a failed check.
    struct Cleanup { modules: Vec<String>, app: Option<std::process::Child>, default: String }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(mut c) = self.app.take() { let _ = c.kill(); let _ = c.wait(); }
            if !self.default.is_empty() { let _ = pactl(&["set-default-sink", &self.default]); }
            for m in &self.modules { let _ = pactl(&["unload-module", m]); }
        }
    }
    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> anyhow::Result<T> {
        let end = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(v) = f() { return Ok(v); }
            anyhow::ensure!(Instant::now() < end, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(150));
        }
    }
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
    let mut cleanup = Cleanup {
        modules: Vec::new(), app: None,
        default: pactl(&["get-default-sink"]).unwrap_or_default().trim().to_string(),
    };
    let (home, other) = ("kabelsalat_test_home", "kabelsalat_test_other");
    for (name, label) in [(home, "Test-Home"), (other, "Test-Other")] {
        let id = pactl(&["load-module", "module-null-sink", &format!("sink_name={name}"),
            &format!("sink_properties=device.description={label}")])
            .ok_or_else(|| anyhow::anyhow!("could not load a null sink"))?;
        cleanup.modules.push(id.trim().to_string());
    }
    anyhow::ensure!(pactl(&["set-default-sink", home]).is_some(), "set-default-sink failed");

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
    let mut app = std::process::Command::new("pacat")
        .args(["--playback", &format!("--device={home}"), "--format=float32le", "--rate=48000", "--channels=2",
            "--client-name=Test Browser", "--stream-name=(2) Backing track - Video", "--latency-msec=50"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn().map_err(|e| anyhow::anyhow!("pacat: {e}"))?;
    let mut tone_in = app.stdin.take().expect("piped stdin");
    cleanup.app = Some(app);
    std::thread::spawn(move || {
        let mut phase = 0.0f32;
        let mut buf = Vec::with_capacity(4800 * 8);
        loop {
            buf.clear();
            for _ in 0..4800 {
                phase = (phase + 440.0 / 48000.0).fract();
                let v = ((phase * std::f32::consts::TAU).sin() * 0.5).to_le_bytes();
                buf.extend_from_slice(&v);
                buf.extend_from_slice(&v);
            }
            if tone_in.write_all(&buf).is_err() { break; }
        }
    });

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
    let theirs = until("the other app's stream", || scan_and_apply(Target::Follow).streams.into_iter().find(|s| s.app == "Test Browser"))?;
    println!("app stream: {theirs:?}");
    anyhow::ensure!(theirs.title == "(2) Backing track - Video", "title, got {:?}", theirs.title);
    anyhow::ensure!((theirs.volume - 1.0).abs() < 0.02 && !theirs.muted, "starts at 100 %, unmuted: {theirs:?}");
    anyhow::ensure!(scan_and_apply(Target::Follow).streams.iter().all(|s| s.index != ours), "our own stream is not an app strip");

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
    if let Some(mut c) = cleanup.app.take() { let _ = c.kill(); let _ = c.wait(); }
    let st = until("the stream leaving the scan", || Some(scan_and_apply(Target::Follow)).filter(|s| s.streams.iter().all(|x| x.index != theirs.index)))?;
    meters.sync(&st.streams);
    anyhow::ensure!(!meters.readers.contains_key(&theirs.index), "the meter reader of an ended stream is stopped");
    anyhow::ensure!(stream_level(&levels, theirs.index) == 0.0, "an ended stream reads silent");
    println!("ok: the app stopped, its strip and meter are gone");

    drop(stream);
    println!("audio-out-test: PASS");
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
}
