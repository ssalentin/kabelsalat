//! `kabelsalat app --headless`: the app without a window or sound card, for
//! `kabelsalat crash-test` (#279). It runs the app's frame path on a
//! headless egui context and an offline engine in real time, under the same
//! watchdog, crash handlers and recovery as the windowed app. Lines on stdin
//! drive it; it answers on stdout with lines that start with `headless:`.
//!
//! - `ready pid=… recovered=0|1 report=<path>|-` and a `state` line, once up
//! - `open <session>` / `select <song> <part>` / `state` → `state session=… song=… part=… bpm=…`
//! - `play` → transport on, then `sound peak=…` once the master is not silent; `stop`
//! - `panic` / `segv` → a panic or a segfault on the UI thread; `quit` → exit 0

use super::*;
use std::io::BufRead;

const SR: f64 = 48000.0;
const BLOCK: usize = 256;
/// Blocks per UI frame: one frame per 16 ms, as on a 60 Hz display.
const BLOCKS_PER_FRAME: usize = 3;
const SILENT: f32 = 0.01;

struct Headless {
    engine: GraphEngine,
    app: DemoApp,
    ctx: egui::Context,
    buf: Vec<f32>,
    started: std::time::Instant,
    next_block: std::time::Instant,
    listening: bool,
}

impl Headless {
    fn new() -> anyhow::Result<Self> {
        let (voices, mains) = startup_voices(SR).unwrap_or_else(|e| {
            eprintln!("headless: no startup synth ({e})");
            (Vec::new(), Vec::new())
        });
        let controllers = voices.iter().map(|(_, v)| v.controller()).collect();
        let voice_ids = voices.iter().map(|(_, v)| v.voice_id()).collect();
        let (bus, drain) = crate::control::command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, voices);
        let mut app = DemoApp::new_app(None, false, Assistant::new());
        engine.set_rig_snapshot(app.rig_snapshot.clone());
        let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
        engine.set_graph_pub(graph_pub.clone());
        let (voice_tx, voice_rx) = rtrb::RingBuffer::new(VOICE_RING);
        let (retire_tx, retire_rx) = rtrb::RingBuffer::new(VOICE_RING);
        engine.set_voice_channels(voice_rx, retire_tx);
        engine.spawn_lv2_loader();
        app.graph_pub = Some(graph_pub);
        app.attach_engine(mains, controllers, voice_ids, SR, voice_tx, retire_rx, Arc::new(Mutex::new(bus)));
        let now = std::time::Instant::now();
        Ok(Self { engine, app, ctx: egui::Context::default(), buf: vec![0.0; BLOCK * 2], started: now, next_block: now, listening: false })
    }

    /// One UI frame, then the blocks that play while it shows, in real time.
    fn frame(&mut self) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.time = Some(self.started.elapsed().as_secs_f64());
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        for _ in 0..BLOCKS_PER_FRAME {
            self.engine.render(&mut self.buf, 2);
            let peak = self.buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            if self.listening && peak > SILENT {
                self.listening = false;
                say(&format!("sound peak={peak:.3}"));
            }
            self.next_block += std::time::Duration::from_secs_f64(BLOCK as f64 / SR);
            let now = std::time::Instant::now();
            if self.next_block > now {
                std::thread::sleep(self.next_block - now);
            } else {
                self.next_block = now;
            }
        }
    }

    fn state(&self) -> String {
        let (song, part) = match self.app.setlist.active {
            Some(p) => (p.song.to_string(), p.part.to_string()),
            None => ("-".into(), "-".into()),
        };
        format!("state session={} song={song} part={part} bpm={:.0}", self.app.open_session.as_deref().unwrap_or("-"), self.app.bpm)
    }

    fn command(&mut self, line: &str) -> Result<bool, String> {
        let (verb, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        match verb {
            "open" => {
                let dir = self.app.effective_sessions_dir();
                self.app.open_saved_session(&dir, rest)?;
            }
            "select" => {
                let mut n = rest.split_whitespace().map(|s| s.parse::<usize>().map_err(|e| e.to_string()));
                let (song, part) = (n.next().ok_or("select <song> <part>")??, n.next().ok_or("select <song> <part>")??);
                self.app.select_setlist(song, part)?;
            }
            "state" => {}
            "play" => {
                self.app.master_transport(true);
                self.listening = true;
                return Ok(true);
            }
            "stop" => {
                self.app.master_transport(false);
                self.listening = false;
            }
            "panic" => panic!("crash-test: a panic on the UI thread"),
            // Non-null, so the debug precondition check does not abort first.
            "segv" => unsafe { (16 as *mut u8).write_volatile(1) },
            "quit" => return Ok(false),
            other => return Err(format!("unknown command {other:?}")),
        }
        // The engine applies the change and the marker follows in a frame.
        for _ in 0..4 {
            self.frame();
        }
        say(&self.state());
        Ok(true)
    }
}

fn say(line: &str) {
    println!("headless: {line}");
}

pub fn run() -> anyhow::Result<()> {
    let mut h = Headless::new()?;
    let restored = h.app.start_recovery();
    for _ in 0..8 {
        h.frame();
    }
    let report = std::env::var(crate::watchdog::CRASH_REPORT).unwrap_or_else(|_| "-".into());
    say(&format!("ready pid={} recovered={} report={report}", std::process::id(), restored as u8));
    say(&h.state());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("headless-stdin".into()).spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    })?;
    loop {
        match rx.try_recv() {
            Ok(line) => match h.command(&line) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => say(&format!("error {e}")),
            },
            // stdin closed: whoever drove this is gone.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Empty) => h.frame(),
        }
    }
}
