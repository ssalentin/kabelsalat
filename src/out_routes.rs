//! The stage output matrix (#399), the parts the app, the assistant and
//! headless MCP share: reading a tool call's arguments, saying where an
//! assignment plays on the interface that is there now, and the list of
//! channels a menu offers. The engine holds the assignments
//! (`Command::SetOutRoute`) and writes the channels; nothing here runs on
//! the audio thread.

use serde_json::{json, Value};

use crate::control::{OutChannels, OutSource, SendSource};
use crate::engine::MAX_OUT_CH;

/// Linear gain as dB, for tool answers and the mixer's field.
pub fn level_db(level: f32) -> f32 {
    if level <= 1e-4 { -80.0 } else { 20.0 * level.log10() }
}

/// dB as the linear gain an assignment holds (−80 dB and below = off).
pub fn db_level(db: f32) -> f32 {
    if db <= -80.0 { 0.0 } else { 10f32.powf(db / 20.0).min(OutChannels::MAX_LEVEL) }
}

/// The source a tool call names: `source` = "master", "click" or "player",
/// or one bus as `rig` (0-based index) or `bus` (an aux bus's node id).
pub fn parse_source(args: &Value) -> Result<OutSource, String> {
    let rig = args.get("rig").and_then(Value::as_u64);
    let bus = args.get("bus").and_then(Value::as_u64);
    let named = args.get("source").and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase());
    match (named.as_deref(), rig, bus) {
        (Some("master"), None, None) => Ok(OutSource::Master),
        (Some("click" | "metronome"), None, None) => Ok(OutSource::Click),
        (Some("player" | "track" | "backing" | "backing_track" | "cue"), None, None) => Ok(OutSource::Player),
        (None | Some("rig"), Some(r), None) => Ok(OutSource::Bus(SendSource::Rig(r.min(255) as u8))),
        (None | Some("bus"), None, Some(n)) => Ok(OutSource::Bus(SendSource::Node(n.min(u32::MAX as u64) as u32))),
        (Some(other), None, None) if !matches!(other, "rig" | "bus") => {
            Err(format!("no source '{other}': source is master, click or player, or give rig (0-based index) or bus (an aux bus's node id)"))
        }
        _ => Err("name exactly one source: source = master, click or player, OR rig (0-based index, that rig's bus), OR bus (an aux bus's node id)".into()),
    }
}

/// The channels a tool call asks for: `output` is the first output as the
/// interface numbers it (1 = the first; 3 with `mono` false = outputs 3-4),
/// null or absent = no assignment. `level_db` (default 0) and `pan`
/// (-1..1, default 0) are optional.
pub fn parse_target(args: &Value) -> Result<Option<OutChannels>, String> {
    let Some(out) = args.get("output").filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(n) = out.as_u64() else {
        return Err("output is the number of the first output channel (1 = the first; 3 = outputs 3-4, or output 3 alone with mono=true), or null for no assignment".into());
    };
    let mono = args.get("mono").and_then(Value::as_bool).unwrap_or(false);
    let width = if mono { 1 } else { 2 };
    if n < 1 || n as usize - 1 + width > MAX_OUT_CH {
        return Err(format!("output {n} is out of range: outputs are 1..{MAX_OUT_CH}"));
    }
    let db = args.get("level_db").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let pan = args.get("pan").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    if !db.is_finite() || !pan.is_finite() {
        return Err("level_db and pan must be numbers".into());
    }
    if db > 6.0 {
        return Err("level_db is at most 6".into());
    }
    Ok(Some(OutChannels { first: (n - 1) as u8, mono, level: db_level(db), pan }.clamped()))
}

/// Where `source` plays without an assignment.
pub fn default_place(source: OutSource) -> &'static str {
    match source {
        OutSource::Master => "outputs 1–2",
        _ => "the master mix",
    }
}

/// "outputs 3–4" / "output 3".
pub fn place(to: &OutChannels) -> String {
    format!("{} {}", if to.mono { "output" } else { "outputs" }, to.label())
}

/// One source of the matrix for a tool answer: its assignment and where it
/// plays on a stream with `channels` outputs.
pub fn route_json(source: OutSource, name: &str, to: Option<OutChannels>, channels: usize) -> Value {
    let mut v = match source {
        OutSource::Master => json!({"source": "master"}),
        OutSource::Click => json!({"source": "click"}),
        OutSource::Player => json!({"source": "player"}),
        OutSource::Bus(SendSource::Rig(r)) => json!({"source": "rig", "rig": r}),
        OutSource::Bus(SendSource::Node(n)) => json!({"source": "bus", "bus": n}),
    };
    v["name"] = json!(name);
    match to {
        None => {
            v["output"] = Value::Null;
            v["plays_on"] = json!(default_place(source));
        }
        Some(to) => {
            v["output"] = json!(to.first as usize + 1);
            v["mono"] = json!(to.mono);
            v["level_db"] = json!((level_db(to.level) * 10.0).round() / 10.0);
            v["pan"] = json!(to.pan);
            let fits = to.fits(channels);
            v["available"] = json!(fits);
            v["plays_on"] = json!(if fits {
                place(&to)
            } else {
                format!("{} (assigned to {}, but this output has {channels} channels)", default_place(source), place(&to))
            });
        }
    }
    v
}

/// The channels a menu offers on an interface with `channels` outputs:
/// every stereo pair that starts on an odd output, then every single one.
/// `keep` (the assignment in place) is listed even when the interface that
/// is there now lacks it.
pub fn menu_options(channels: usize, keep: Option<OutChannels>) -> Vec<OutChannels> {
    let n = channels.min(MAX_OUT_CH);
    let mut v: Vec<OutChannels> = (0..n / 2).map(|p| OutChannels::pair((p * 2) as u8)).collect();
    v.extend((0..n).map(|c| OutChannels { mono: true, ..OutChannels::pair(c as u8) }));
    if let Some(k) = keep {
        if !v.iter().any(|o| o.first == k.first && o.mono == k.mono) {
            v.push(OutChannels { level: 1.0, pan: 0.0, ..k });
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_names_one_source_and_its_channels() {
        assert_eq!(parse_source(&json!({"source": "click"})), Ok(OutSource::Click));
        assert_eq!(parse_source(&json!({"source": "Master"})), Ok(OutSource::Master));
        assert_eq!(parse_source(&json!({"source": "player"})), Ok(OutSource::Player));
        assert_eq!(parse_source(&json!({"rig": 1})), Ok(OutSource::Bus(SendSource::Rig(1))));
        assert_eq!(parse_source(&json!({"source": "bus", "bus": 12})), Ok(OutSource::Bus(SendSource::Node(12))));
        assert!(parse_source(&json!({})).is_err());
        assert!(parse_source(&json!({"source": "click", "rig": 0})).is_err());
        assert!(parse_source(&json!({"rig": 0, "bus": 3})).is_err());
        assert!(parse_source(&json!({"source": "wedge"})).unwrap_err().contains("wedge"));

        assert_eq!(parse_target(&json!({})), Ok(None));
        assert_eq!(parse_target(&json!({"output": null})), Ok(None));
        assert_eq!(parse_target(&json!({"output": 3})), Ok(Some(OutChannels::pair(2))));
        let mono = parse_target(&json!({"output": 5, "mono": true, "level_db": -6.0, "pan": 0.5})).unwrap().unwrap();
        assert_eq!((mono.first, mono.mono, mono.pan), (4, true, 0.5));
        assert!((mono.level - 0.501).abs() < 0.001, "{}", mono.level);
        assert!(parse_target(&json!({"output": 0})).is_err());
        assert!(parse_target(&json!({"output": MAX_OUT_CH})).is_err(), "a pair needs two channels");
        assert!(parse_target(&json!({"output": MAX_OUT_CH, "mono": true})).is_ok());
        assert!(parse_target(&json!({"output": "3-4"})).is_err());
        assert!(parse_target(&json!({"output": 3, "level_db": 12})).is_err());
        assert_eq!(parse_target(&json!({"output": 3, "level_db": -200})).unwrap().unwrap().level, 0.0);
    }

    #[test]
    fn an_assignment_says_where_it_plays_on_the_interface_that_is_there() {
        let click = Some(OutChannels::pair(2));
        let on4 = route_json(OutSource::Click, "Click", click, 4);
        assert_eq!((on4["output"].as_u64(), on4["available"].as_bool(), on4["plays_on"].as_str()), (Some(3), Some(true), Some("outputs 3–4")));
        let on2 = route_json(OutSource::Click, "Click", click, 2);
        assert_eq!(on2["available"], false);
        assert_eq!(on2["plays_on"], "the master mix (assigned to outputs 3–4, but this output has 2 channels)");
        let none = route_json(OutSource::Master, "Master", None, 2);
        assert!(none["output"].is_null());
        assert_eq!(none["plays_on"], "outputs 1–2");
        assert_eq!(route_json(OutSource::Bus(SendSource::Rig(2)), "Rig C", None, 2)["rig"], 2);
    }

    #[test]
    fn the_menu_offers_pairs_then_single_outputs_and_keeps_a_missing_assignment() {
        let labels = |ch, keep| menu_options(ch, keep).iter().map(|o| place(o)).collect::<Vec<_>>();
        assert_eq!(labels(4, None), ["outputs 1–2", "outputs 3–4", "output 1", "output 2", "output 3", "output 4"]);
        assert_eq!(labels(2, Some(OutChannels::pair(2))), ["outputs 1–2", "output 1", "output 2", "outputs 3–4"]);
        assert_eq!(menu_options(4, Some(OutChannels { level: 0.5, ..OutChannels::pair(2) })).len(), 6, "an assignment the interface has is not listed twice");
        assert!((db_level(level_db(0.5)) - 0.5).abs() < 1e-5);
        assert_eq!(db_level(-80.0), 0.0);
    }
}

/// `kabelsalat stage-out-test` (#399): the real PipeWire output stream on a
/// 4-channel null sink plays the click on outputs 3–4 while the master plays
/// a tone on 1–2, with nothing of either on the other pair; then the same
/// engine on a 2-channel null sink plays the assigned click in the master
/// mix. Needs PipeWire with `pactl` and `parec`; loads two temporary null
/// sinks, so nothing is heard.
pub fn run_stage_test() -> anyhow::Result<()> {
    use crate::audio_out::selftest::{pactl, Cleanup};
    use crate::control::{command_bus, name_bytes, AudioInChannel, Command, DspKind, Graph, NodeKind, PortKind, PortRef};
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const RATE: u32 = 48000;
    pactl(&["info"]).ok_or_else(|| anyhow::anyhow!("pactl info failed: no PipeWire/PulseAudio server"))?;
    let mut cleanup = Cleanup::new();
    let pid = std::process::id();
    let stage = format!("kabelsalat_test_stage4_{pid}");
    let laptop = format!("kabelsalat_test_laptop2_{pid}");
    const AUX4: &str = "aux0,aux1,aux2,aux3";
    cleanup.null_sink_with(&stage, "Test-Stage-4ch", &["channels=4", &format!("channel_map={AUX4}")])?;
    cleanup.null_sink(&laptop, "Test-Laptop-2ch")?;
    for (sink, n) in [(&stage, 4), (&laptop, 2)] {
        let got = crate::audio_out::sink_channel_count(&pactl(&["-f", "json", "list", "sinks"]).unwrap_or_default(), sink);
        anyhow::ensure!(got == Some(n), "{sink} lists {got:?} channels, not {n}");
    }

    // The rack: a 440 Hz tone from an audio input and a metronome, both
    // cabled to the master.
    let (mut bus, drain) = command_bus();
    let mut g = Graph::default();
    g.reserve(16, 16);
    let tone_in = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
    let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: tone_in, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(2, PortRef { node: metro, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let mut engine = crate::engine::GraphEngine::new(g, drain, RATE as f64, vec![]);
    let pubd = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
    engine.set_graph_pub(pubd.clone());
    let (mut side_tx, side_rx) = rtrb::RingBuffer::new(4);
    engine.set_audio_in_channel(side_rx);
    let (mut feed, rx) = rtrb::RingBuffer::<f32>::new(4096);
    let _ = side_tx.push((tone_in, rx));
    let engine = Arc::new(Mutex::new(engine));
    // The tone: kept topped up from its own thread while `tone` is on.
    let tone = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (tone, done) = (tone.clone(), done.clone());
        std::thread::spawn(move || {
            let mut k = 0u64;
            while !done.load(Ordering::Relaxed) {
                while feed.slots() >= 2 {
                    let s = if tone.load(Ordering::Relaxed) {
                        0.4 * (std::f64::consts::TAU * 440.0 * k as f64 / RATE as f64).sin() as f32
                    } else {
                        0.0
                    };
                    let _ = feed.push(s);
                    let _ = feed.push(s);
                    k += 1;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let open = |sink: &str| -> anyhow::Result<crate::audio_backend::OpenedStream> {
        let engine = engine.clone();
        crate::audio_backend::open_stream(
            crate::audio_backend::AudioBackendChoice::PipeWire,
            Some(sink),
            Some(RATE as f64),
            256,
            Box::new(move |data: &mut [f32], ch: usize, _| match engine.try_lock() {
                Ok(mut e) => e.render(data, ch),
                Err(_) => data.fill(0.0),
            }),
            Box::new(|e| eprintln!("stage-out-test: stream error: {e}")),
        )
    };
    // Per-channel peaks of what a sink's monitor received for `secs`.
    let record = |sink: &str, channels: usize, map: &str, secs: f64| -> anyhow::Result<Vec<f32>> {
        let mut child = std::process::Command::new("parec")
            .args([
                &format!("--device={sink}.monitor"), &format!("--channels={channels}"), &format!("--channel-map={map}"),
                "--format=float32le", &format!("--rate={RATE}"), "--latency-msec=20", "--client-name=kabelsalat-stage-test",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let mut out = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no parec output"))?;
        // Drop the first 0.2 s (the recorder's own start), keep `secs`.
        let skip = (0.2 * RATE as f64) as usize * channels * 4;
        let mut bytes = vec![0u8; skip + (secs * RATE as f64) as usize * channels * 4];
        let read = out.read_exact(&mut bytes);
        let _ = child.kill();
        let _ = child.wait();
        read?;
        let mut peaks = vec![0.0f32; channels];
        for (i, b) in bytes[skip..].chunks_exact(4).enumerate() {
            let x = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            peaks[i % channels] = peaks[i % channels].max(x.abs());
        }
        Ok(peaks)
    };
    let fmt = |p: &[f32]| p.iter().map(|x| format!("{x:.6}")).collect::<Vec<_>>().join(" ");
    let mut failures = Vec::new();
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures.push(what);
        }
    };
    const SILENT: f32 = 1e-6;

    bus.send(Command::SetBpm { bpm: 240.0 });
    bus.send(Command::SetOutRoute { source: OutSource::Click, to: Some(OutChannels::pair(2)) });
    println!("4-channel interface ({stage}): click assigned to outputs 3–4");
    let stream = open(&stage)?;
    check(stream.channels == 4, format!("the stream opened with {} channels", stream.channels));
    std::thread::sleep(Duration::from_millis(300));
    bus.send(Command::SetNodePlaying { node: metro, playing: true });
    std::thread::sleep(Duration::from_millis(300));
    let p = record(&stage, 4, AUX4, 1.5)?;
    check(p[2] > 0.05 && p[3] > 0.05, format!("click alone: on outputs 3–4 [{}]", fmt(&p)));
    check(p[0] <= SILENT && p[1] <= SILENT, format!("click alone: nothing on outputs 1–2 [{}]", fmt(&p)));
    bus.send(Command::SetNodePlaying { node: metro, playing: false });
    tone.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    let p = record(&stage, 4, AUX4, 1.5)?;
    check(p[0] > 0.1 && p[1] > 0.1, format!("master tone alone: on outputs 1–2 [{}]", fmt(&p)));
    check(p[2] <= SILENT && p[3] <= SILENT, format!("master tone alone: nothing on outputs 3–4 [{}]", fmt(&p)));
    bus.send(Command::SetNodePlaying { node: metro, playing: true });
    std::thread::sleep(Duration::from_millis(300));
    let p = record(&stage, 4, AUX4, 1.5)?;
    check(p.iter().all(|&x| x > 0.05), format!("both: each on its own pair [{}]", fmt(&p)));
    let published = pubd.lock().map(|g| g.out_channels).unwrap_or(0);
    check(published == 4, format!("the engine publishes {published} output channels"));
    drop(stream);

    println!("2-channel output ({laptop}): the same assignment falls back to the master mix");
    tone.store(false, Ordering::Relaxed);
    let stream = open(&laptop)?;
    check(stream.channels == 2, format!("the stream opened with {} channels", stream.channels));
    std::thread::sleep(Duration::from_millis(500));
    let p = record(&laptop, 2, "front-left,front-right", 1.5)?;
    check(p[0] > 0.05 && p[1] > 0.05, format!("click in the master mix on outputs 1–2 [{}]", fmt(&p)));
    let (published, held) = pubd.lock().map(|g| (g.out_channels, g.out_routes.iter().flatten().count())).unwrap_or((0, 0));
    check(published == 2 && held == 1, format!("the engine publishes {published} channels and keeps {held} assignment"));
    drop(stream);

    done.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    drop(cleanup);
    anyhow::ensure!(failures.is_empty(), "stage-out-test: {} check(s) failed: {}", failures.len(), failures.join("; "));
    println!("stage-out-test: all checks passed");
    Ok(())
}
