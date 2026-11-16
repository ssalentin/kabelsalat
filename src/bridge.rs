//! Unix-socket control bridge (task-16): let the MCP server and a CLI drive
//! the RUNNING GUI app through the one typed control plane (`control.rs`).
//!
//! The app spawns a listener thread ([`spawn`]) that owns clones of the same
//! `Arc<Mutex<CommandBus>>` the UI publishes through and an `Arc<Mutex<Graph>>`
//! the UI mirrors from its live cables each frame. Commands received here go
//! onto the very bus the engine drains at block boundaries — agents control
//! exactly what the user sees, not a parallel headless engine.
//!
//! Protocol: newline-delimited JSON, one request per line, one reply per line.
//! - `{"op":"cmd","cmd":<Command JSON>}` → `{"ok":true}` (Command is serde,
//!   externally tagged, e.g. `{"SetSystemAudio":{"gain":0.5}}`).
//! - `{"op":"query","name":"get_graph"}` → `{"ok":true,"data":<Graph JSON>}`.
//! - `{"op":"query","name":"get_status"}` → `{"ok":true,"data":{nodes, edges,
//!   sample_rate, output_level, audio_live, rigs, active_rig, ...}}`.
//! - `{"op":"query","name":"get_rigs"}` → `{"ok":true,"data":{rigs:[{name,
//!   midi_device}], active}}` — the engine's shared rig snapshot (empty in
//!   single-graph mode).
//! - Anything else → `{"ok":false,"error":"..."}`. Bad input never panics.
//!
//! Socket: `$XDG_RUNTIME_DIR/kabelsalat.sock`, fallback `~/.cache/kabelsalat.sock`.
//! A stale socket file is removed on start; a LIVE one is never stolen.
//!
//! RT discipline: this thread is a plain frontend. It locks the bus only to
//! push (non-blocking rtrb send) and the graph only to snapshot; the audio
//! thread never touches either lock.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::control::{Command, CommandBus, Graph, RigSnapshot};
use crate::engine::GraphPub;

/// One UI-owned tool call queued for the app frame: (tool, args, reply).
pub type UiRequest = (String, Value, std::sync::mpsc::Sender<String>);

/// Requests the running app executes on its UI thread (`DemoApp::exec_tool`)
/// — e.g. load_sound, whose synth state only the app's main thread owns.
pub static UI_INBOX: Mutex<Vec<UiRequest>> = Mutex::new(Vec::new());
/// Set once a UI frame loop drains `UI_INBOX` (headless MCP has none).
pub static UI_ATTACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn ui_call(tool: String, args: Value) -> Result<String, String> {
    if !UI_ATTACHED.load(Ordering::Relaxed) { return Err("no app UI attached".into()); }
    let (tx, rx) = std::sync::mpsc::channel();
    UI_INBOX.lock().map_err(|_| "ui inbox poisoned".to_string())?.push((tool, args, tx));
    rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the app did not answer in 5 s".to_string())
}

/// Output buffer size in frames (shared with the audio setup in `ui`).
pub const BUFFER_FRAMES: u32 = 256;
/// Count of audio stream errors / xruns reported by cpal since startup.
pub static XRUNS: AtomicU32 = AtomicU32::new(0);
/// The output stream's sample rate in Hz (0 = no audio yet).
pub static SAMPLE_RATE: AtomicU32 = AtomicU32::new(0);
/// Audio callback timing (lock-free, written by the audio thread): worst
/// render time, the callback's frame count, and how many callbacks used
/// more than 80% of their buffer's real-time budget.
pub static RENDER_MAX_US: AtomicU32 = AtomicU32::new(0);
pub static RENDER_LAST_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static RENDER_LATE: AtomicU32 = AtomicU32::new(0);
/// Smoothed (EMA, 1/64) render time in microseconds.
pub static RENDER_AVG_US: AtomicU32 = AtomicU32::new(0);

/// Record one callback's render time (called from the audio thread; atomics only).
pub fn note_render(elapsed: std::time::Duration, frames: usize, sample_rate: f64) {
    let us = elapsed.as_micros().min(u32::MAX as u128) as u32;
    RENDER_MAX_US.fetch_max(us, Ordering::Relaxed);
    RENDER_LAST_FRAMES.store(frames as u32, Ordering::Relaxed);
    let avg = RENDER_AVG_US.load(Ordering::Relaxed);
    RENDER_AVG_US.store(avg - avg / 64 + us / 64, Ordering::Relaxed);
    let budget_us = frames as f64 / sample_rate * 1e6;
    if us as f64 > budget_us * 0.8 {
        RENDER_LATE.fetch_add(1, Ordering::Relaxed);
    }
}
/// Rolling average of the MIDI input→processing delay in microseconds (the
/// hardware-MIDI queue wait; add the audio buffer for total input→sound latency).
pub static MIDI_LATENCY_US: AtomicU32 = AtomicU32::new(0);

/// Update the MIDI-latency rolling average (called when hardware MIDI is drained).
pub fn record_midi_latency(us: u32) {
    let prev = MIDI_LATENCY_US.load(Ordering::Relaxed);
    let next = if prev == 0 { us } else { (prev * 3 + us) / 4 };
    MIDI_LATENCY_US.store(next, Ordering::Relaxed);
}

/// Where the bridge listens (and where clients connect).
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("kabelsalat.sock");
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".cache").join("kabelsalat.sock")
}

/// Start the bridge thread for the running app. `bus` is the UI's shared
/// command bus, `graph` the UI-mirrored live patch, `level` the engine's
/// output-peak handle (f32 bits). Additive: on failure the app keeps running.
///
/// Legacy entry point: `get_rigs` reports an empty rig list. Use
/// [`spawn_with_rigs`] with the engine's shared [`RigSnapshot`] handle to
/// expose live multi-rig state.
pub fn spawn(
    bus: Arc<Mutex<CommandBus>>,
    graph: Arc<Mutex<Graph>>,
    level: Arc<AtomicU32>,
    sample_rate: f64,
) -> anyhow::Result<()> {
    spawn_with_rigs(bus, graph, level, sample_rate, Arc::new(Mutex::new(RigSnapshot::default())))
}

/// Like [`spawn`], plus the shared rig-state snapshot the engine publishes
/// on `LoadRigs`/`LoadGraph` (see `GraphEngine::set_rig_snapshot`). The same
/// `Arc<Mutex<RigSnapshot>>` must be handed to the engine and to this bridge.
pub fn spawn_with_rigs(
    bus: Arc<Mutex<CommandBus>>,
    graph: Arc<Mutex<Graph>>,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    rigs: Arc<Mutex<RigSnapshot>>,
) -> anyhow::Result<()> {
    spawn_at(socket_path(), bus, graph, level, sample_rate, rigs, None)
}

/// Like [`spawn_with_rigs`], plus the engine's `GraphPub` so `get_status`
/// reports live transport, tempo, and count-in state.
pub fn spawn_with_state(
    bus: Arc<Mutex<CommandBus>>,
    graph: Arc<Mutex<Graph>>,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    rigs: Arc<Mutex<RigSnapshot>>,
    graph_pub: Arc<Mutex<GraphPub>>,
) -> anyhow::Result<()> {
    spawn_at(socket_path(), bus, graph, level, sample_rate, rigs, Some(graph_pub))
}

fn spawn_at(
    path: PathBuf,
    bus: Arc<Mutex<CommandBus>>,
    graph: Arc<Mutex<Graph>>,
    level: Arc<AtomicU32>,
    sample_rate: f64,
    rigs: Arc<Mutex<RigSnapshot>>,
    graph_pub: Option<Arc<Mutex<GraphPub>>>,
) -> anyhow::Result<()> {
    if path.exists() {
        // Refuse to steal the socket from a live instance; clean up stale ones.
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!(
                "another kabelsalat instance already owns the bridge at {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .map_err(|e| anyhow::anyhow!("removing stale socket {}: {e}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)
        .map_err(|e| anyhow::anyhow!("binding bridge socket {}: {e}", path.display()))?;
    eprintln!("bridge: listening on {}", path.display());
    std::thread::Builder::new()
        .name("kabelsalat-bridge".into())
        .spawn(move || {
            // Sequential connections: one client at a time, each may pipeline
            // many request lines. Accept errors are logged, never fatal.
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => handle_conn(stream, &bus, &graph, &level, sample_rate, &rigs, graph_pub.as_ref()),
                    Err(e) => eprintln!("bridge: accept error: {e}"),
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("spawning bridge thread: {e}"))?;
    Ok(())
}

fn handle_conn(
    stream: UnixStream,
    bus: &Arc<Mutex<CommandBus>>,
    graph: &Arc<Mutex<Graph>>,
    level: &AtomicU32,
    sample_rate: f64,
    rigs: &Arc<Mutex<RigSnapshot>>,
    graph_pub: Option<&Arc<Mutex<GraphPub>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return, // client gone / timeout — drop the connection
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = handle_line_with_pub(&line, bus, graph, level, sample_rate, rigs, graph_pub);
        if writeln!(writer, "{reply}").is_err() || writer.flush().is_err() {
            return;
        }
    }
}

fn err_reply(msg: impl Into<String>) -> Value {
    json!({"ok": false, "error": msg.into()})
}

/// One request line → one reply value. Pure enough to unit-test; never panics
/// on malformed input (every parse/lock failure becomes an error reply).
#[cfg_attr(not(test), allow(dead_code))]
fn handle_line(
    line: &str,
    bus: &Arc<Mutex<CommandBus>>,
    graph: &Arc<Mutex<Graph>>,
    level: &AtomicU32,
    sample_rate: f64,
    rigs: &Arc<Mutex<RigSnapshot>>,
) -> Value {
    handle_line_with_pub(line, bus, graph, level, sample_rate, rigs, None)
}

fn handle_line_with_pub(
    line: &str,
    bus: &Arc<Mutex<CommandBus>>,
    graph: &Arc<Mutex<Graph>>,
    level: &AtomicU32,
    sample_rate: f64,
    rigs: &Arc<Mutex<RigSnapshot>>,
    graph_pub: Option<&Arc<Mutex<GraphPub>>>,
) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return err_reply(format!("invalid JSON: {e}")),
    };
    match req.get("op").and_then(Value::as_str) {
        Some("cmd") => {
            let Some(cmd_val) = req.get("cmd") else {
                return err_reply("missing 'cmd' field");
            };
            let cmd: Command = match serde_json::from_value(cmd_val.clone()) {
                Ok(c) => c,
                Err(e) => return err_reply(format!("invalid Command: {e}")),
            };
            let mut b = match bus.lock() {
                Ok(b) => b,
                Err(_) => return err_reply("command bus lock poisoned"),
            };
            if b.send(cmd) {
                json!({"ok": true})
            } else {
                err_reply("command bus full — try again")
            }
        }
        // UI-owned tools (synth patches live on the app's main thread):
        // queued for the app frame, answered within 5 s.
        Some("ui") => {
            let tool = req.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
            let args = req.get("args").cloned().unwrap_or(json!({}));
            match ui_call(tool, args) {
                Ok(text) => json!({"ok": true, "data": text}),
                Err(e) => err_reply(e),
            }
        }
        Some("query") => match req.get("name").and_then(Value::as_str) {
            Some("get_graph") => {
                let g = match graph.lock() {
                    Ok(g) => g,
                    Err(_) => return err_reply("graph lock poisoned"),
                };
                match serde_json::to_value(&*g) {
                    Ok(data) => json!({"ok": true, "data": data}),
                    Err(e) => err_reply(format!("serializing graph: {e}")),
                }
            }
            Some("get_rigs") => {
                let s = match rigs.lock() {
                    Ok(s) => s,
                    Err(_) => return err_reply("rig snapshot lock poisoned"),
                };
                match serde_json::to_value(&*s) {
                    Ok(data) => json!({"ok": true, "data": data}),
                    Err(e) => err_reply(format!("serializing rig snapshot: {e}")),
                }
            }
            Some("get_status") => {
                let g = match graph.lock() {
                    Ok(g) => g,
                    Err(_) => return err_reply("graph lock poisoned"),
                };
                // Advisory rig state: a poisoned snapshot lock degrades to
                // zero counts, never to a failed status query.
                let (rig_count, active_rig) = match rigs.lock() {
                    Ok(s) => (s.rigs.len(), s.active),
                    Err(_) => (0, 0),
                };
                let tp = graph_pub
                    .and_then(|p| p.lock().ok())
                    .map(|g| g.transport)
                    .unwrap_or_default();
                json!({"ok": true, "data": {
                    "nodes": g.nodes.len(),
                    "edges": g.edges.len(),
                    "sample_rate": sample_rate,
                    "output_level": f32::from_bits(level.load(Ordering::Relaxed)),
                    "audio_live": true,
                    "transport_playing": tp.playing,
                    "counting_in": tp.counting_in,
                    "count_in_beat": tp.count_in_beat,
                    "count_in_enabled": tp.count_in_enabled,
                    "count_in_bars": tp.count_in_bars,
                    "bpm": tp.bpm,
                    "buffer_frames": BUFFER_FRAMES,
                    "buffer_latency_ms": BUFFER_FRAMES as f64 / sample_rate * 1000.0,
                    "xruns": XRUNS.load(Ordering::Relaxed),
                    "render_max_us": RENDER_MAX_US.load(Ordering::Relaxed),
                    "render_frames": RENDER_LAST_FRAMES.load(Ordering::Relaxed),
                    "render_late": RENDER_LATE.load(Ordering::Relaxed),
                    "render_avg_us": RENDER_AVG_US.load(Ordering::Relaxed),
                    "midi_latency_ms": MIDI_LATENCY_US.load(Ordering::Relaxed) as f64 / 1000.0,
                    "rigs": rig_count,
                    "active_rig": active_rig,
                }})
            }
            other => err_reply(format!(
                "unknown query '{}' (use get_graph|get_status|get_rigs)",
                other.unwrap_or("<missing>")
            )),
        },
        other => err_reply(format!(
            "unknown op '{}' (use cmd|query)",
            other.unwrap_or("<missing>")
        )),
    }
}

// ---------------------------------------------------------------------------
// Client side (CLI `send`, MCP attach)
// ---------------------------------------------------------------------------

/// Connect to the running app's bridge, with an actionable error when the
/// app isn't up.
pub fn connect() -> anyhow::Result<UnixStream> {
    let path = socket_path();
    UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!(
            "cannot reach the kabelsalat bridge at {} ({e}). \
             Is the GUI app running? Start it with `kabelsalat app`.",
            path.display()
        )
    })
}

/// One-shot request: connect, write one JSON line, read one reply line.
/// Tool calls are sequential, so a fresh connection per request is simple
/// and robust (the server handles connections sequentially).
pub fn request(req: &Value) -> anyhow::Result<Value> {
    let stream = connect()?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{}", serde_json::to_string(req)?)?;
    writer.flush()?;
    let mut line = String::new();
    let n = BufReader::new(stream).read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("bridge closed the connection without a reply");
    }
    Ok(serde_json::from_str(&line)?)
}

/// `kabelsalat send '<json>'`: forward one request, print the reply, exit.
pub fn cli_send(payload: &str) -> anyhow::Result<()> {
    let req: Value = serde_json::from_str(payload)
        .map_err(|e| anyhow::anyhow!("argument is not valid JSON: {e}"))?;
    let reply = request(&req)?;
    println!("{}", serde_json::to_string(&reply)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{command_bus, CommandDrain};

    fn fixtures() -> (
        Arc<Mutex<CommandBus>>,
        CommandDrain,
        Arc<Mutex<Graph>>,
        Arc<AtomicU32>,
        Arc<Mutex<RigSnapshot>>,
    ) {
        let (bus, drain) = command_bus();
        (
            Arc::new(Mutex::new(bus)),
            drain,
            Arc::new(Mutex::new(Graph::demo_patch())),
            Arc::new(AtomicU32::new(0.25f32.to_bits())),
            Arc::new(Mutex::new(RigSnapshot::default())),
        )
    }

    #[test]
    fn cmd_forwarded_to_bus() {
        let (bus, mut drain, graph, level, rigs) = fixtures();
        let reply = handle_line(
            r#"{"op":"cmd","cmd":{"SetSystemAudio":{"gain":0.5}}}"#,
            &bus, &graph, &level, 48000.0, &rigs,
        );
        assert_eq!(reply, json!({"ok": true}));
        let cmd = drain.pop().expect("command on the bus");
        assert!(matches!(cmd, Command::SetSystemAudio { gain } if (gain - 0.5).abs() < 1e-6));
    }

    #[test]
    fn queries_return_live_snapshot() {
        let (bus, _drain, graph, level, rigs) = fixtures();
        let reply = handle_line(r#"{"op":"query","name":"get_graph"}"#, &bus, &graph, &level, 48000.0, &rigs);
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["data"]["nodes"].as_array().unwrap().len(), 6);
        assert_eq!(reply["data"]["edges"].as_array().unwrap().len(), 8);

        let reply = handle_line(r#"{"op":"query","name":"get_status"}"#, &bus, &graph, &level, 48000.0, &rigs);
        assert_eq!(reply["data"]["nodes"], json!(6));
        assert_eq!(reply["data"]["edges"], json!(8));
        assert_eq!(reply["data"]["sample_rate"], json!(48000.0));
        assert_eq!(reply["data"]["audio_live"], json!(true));
        assert_eq!(reply["data"]["output_level"], json!(0.25));
        // Single-graph mode: no rigs, active 0.
        assert_eq!(reply["data"]["rigs"], json!(0));
        assert_eq!(reply["data"]["active_rig"], json!(0));
    }

    #[test]
    fn get_rigs_returns_snapshot_and_status_counts() {
        let (bus, _drain, graph, level, rigs) = fixtures();
        {
            let mut s = rigs.lock().unwrap();
            s.rigs = vec![
                crate::control::RigMeta { name: "A".into(), midi_device: Some(2) },
                crate::control::RigMeta { name: "B".into(), midi_device: None },
            ];
            s.active = 1;
        }
        let reply = handle_line(r#"{"op":"query","name":"get_rigs"}"#, &bus, &graph, &level, 48000.0, &rigs);
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["data"]["active"], json!(1));
        let list = reply["data"]["rigs"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], json!("A"));
        assert_eq!(list[0]["midi_device"], json!(2));
        assert_eq!(list[1]["name"], json!("B"));
        assert_eq!(list[1]["midi_device"], json!(Value::Null));

        let reply = handle_line(r#"{"op":"query","name":"get_status"}"#, &bus, &graph, &level, 48000.0, &rigs);
        assert_eq!(reply["data"]["rigs"], json!(2));
        assert_eq!(reply["data"]["active_rig"], json!(1));
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        let (bus, _drain, graph, level, rigs) = fixtures();
        for bad in [
            "not json at all",
            "{}",
            r#"{"op":"frobnicate"}"#,
            r#"{"op":"cmd"}"#,
            r#"{"op":"cmd","cmd":{"NoSuchVariant":{}}}"#,
            r#"{"op":"query","name":"nope"}"#,
            r#"{"op":"query"}"#,
        ] {
            let reply = handle_line(bad, &bus, &graph, &level, 48000.0, &rigs);
            assert_eq!(reply["ok"], json!(false), "input: {bad}");
            assert!(reply["error"].is_string());
        }
    }

    #[test]
    fn socket_roundtrip_and_stale_cleanup() {
        let (bus, mut drain, graph, level, rigs) = fixtures();
        let path = std::env::temp_dir().join(format!("ksalat-bridge-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // A stale (unbound) socket file must be replaced, not fatal.
        std::fs::write(&path, b"").unwrap();
        spawn_at(path.clone(), bus.clone(), graph.clone(), level.clone(), 44100.0, rigs.clone(), None).unwrap();

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        writeln!(stream, r#"{{"op":"cmd","cmd":"TransportPlay"}}"#).unwrap();
        writeln!(stream, r#"{{"op":"query","name":"get_status"}}"#).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut l1 = String::new();
        reader.read_line(&mut l1).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&l1).unwrap(), json!({"ok": true}));
        assert!(matches!(drain.pop(), Some(Command::TransportPlay)));
        let mut l2 = String::new();
        reader.read_line(&mut l2).unwrap();
        let v: Value = serde_json::from_str(&l2).unwrap();
        assert_eq!(v["data"]["sample_rate"], json!(44100.0));

        // A second spawn while the first is live must fail, not steal.
        let (bus2, _d2, g2, lv2, rigs2) = fixtures();
        assert!(spawn_at(path.clone(), bus2, g2, lv2, 48000.0, rigs2, None).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn get_status_includes_transport_pub_state() {
        let (bus, _drain, graph, level, rigs) = fixtures();
        let pub_ = Arc::new(Mutex::new(GraphPub {
            transport: crate::engine::TransportPub {
                playing: false,
                counting_in: true,
                count_in_beat: 3,
                count_in_enabled: true,
                count_in_bars: 2,
                bpm: 124.0,
            },
            ..Default::default()
        }));
        let reply = handle_line_with_pub(
            r#"{"op":"query","name":"get_status"}"#,
            &bus,
            &graph,
            &level,
            48000.0,
            &rigs,
            Some(&pub_),
        );
        assert_eq!(reply["data"]["counting_in"], json!(true));
        assert_eq!(reply["data"]["count_in_beat"], json!(3));
        assert_eq!(reply["data"]["count_in_enabled"], json!(true));
        assert_eq!(reply["data"]["count_in_bars"], json!(2));
        assert_eq!(reply["data"]["bpm"], json!(124.0));
        assert_eq!(reply["data"]["transport_playing"], json!(false));
    }
}