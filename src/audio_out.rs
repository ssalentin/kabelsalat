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

/// A change the UI or an agent asks for; applied on the scan thread.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamCmd {
    Volume(u64, f32),
    Mute(u64, bool),
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

/// One scan + enforce pass: list outputs, find our stream, move it to
/// `wanted` if it plays elsewhere. Blocking I/O — scan thread only.
pub fn scan_and_apply(wanted: Option<&str>) -> OutputState {
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
    let mut current = stream.and_then(|(_, sink)| index_to_name(sink));
    if let (Some((input, _)), Some(w)) = (stream, wanted) {
        let available = outputs.iter().any(|o| o.raw == w);
        if available && current.as_deref() != Some(w) {
            let ok = std::process::Command::new("pactl")
                .args(["move-sink-input", &input.to_string(), w])
                .status().is_ok_and(|s| s.success());
            if ok {
                eprintln!("audio out: moved to {w}");
                current = Some(w.to_string());
            }
        }
    }
    OutputState { outputs, current, streams }
}

#[cfg(test)]
mod tests {
    use super::*;

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
