//! The pre-show check (#278) in the app: the Setlist view's "Check show"
//! panel, the `preshow_check` agent tool, and the thread that finds what the
//! app does not keep (backing tracks, disk, power, the rig probe). The
//! checks are `crate::preshow`; the app hands them its own device lists and
//! plugin scan, so nothing scans on the UI thread (rules 48, 57).

use std::sync::mpsc;

use super::*;
use crate::preshow::{gather, Facts, Installed, Render, Report, Status};

type Reply = Box<dyn FnOnce(Result<Report, String>) + Send>;

enum Job {
    /// The rack as it plays now, with what the app knows of the machine.
    Live { session: crate::session::Session, facts: Facts, frames: u32 },
    /// A saved package, checked as `kabelsalat preshow-check` does.
    Package(std::path::PathBuf),
}

#[derive(Default)]
pub(super) struct PreshowState {
    pub(super) open: bool,
    pub(super) report: Option<Report>,
    pub(super) error: Option<String>,
    running: Option<mpsc::Receiver<Result<Report, String>>>,
    /// Agent calls that wait for the running check of the live show.
    waiters: Vec<mpsc::Sender<String>>,
    /// The in-app assistant's answer, taken by `poll_assistant`.
    pub(super) answer: Option<mpsc::Receiver<String>>,
}

impl PreshowState {
    pub(super) fn running(&self) -> bool {
        self.running.is_some()
    }
}

fn send_job(job: Job, reply: Reply) -> bool {
    gather::submit(move || reply(run_job(job)))
}

fn run_job(job: Job) -> Result<Report, String> {
    match job {
        Job::Live { session, mut facts, frames } => {
            let t = std::time::Instant::now();
            gather::complete(&mut facts, &session, frames);
            Ok(Report::of(&session, &facts, t.elapsed()))
        }
        Job::Package(path) => gather::check_package(&path).map_err(|e| format!("{e:#}")),
    }
}

pub(super) fn status_color(status: Status) -> Color32 {
    match status {
        Status::Ok => theme::LIVE_TEAL,
        Status::Warning => theme::ACCENT_BRIGHT,
        Status::Problem => theme::CLIP_BRIGHT,
    }
}

impl DemoApp {
    /// What the app already knows of the machine: its MIDI devices, outputs,
    /// inputs, plugin scan and the running engine's render time.
    fn preshow_facts(&mut self) -> Facts {
        self.adopt_hw_devices();
        let midi_ports = self
            .hw_devices
            .iter()
            .enumerate()
            .filter(|(i, _)| self.hw_live.get(*i).copied().unwrap_or(true))
            .map(|(_, n)| midi_port_key(n).to_string())
            .collect();
        let c = self.engine_counters();
        let running = c.sample_rate > 0 && c.callback_frames > 0;
        let plugins = self.plugin_lists_ready().then(|| Installed::of(&self.plugins_clap, &self.plugins_lv2));
        Facts {
            midi_ports: Some(midi_ports),
            outputs: self.audio_out.lock().ok().map(|s| s.outputs.clone()).filter(|o| !o.is_empty()),
            chosen_output: self.wanted_output.lock().ok().and_then(|w| w.clone()),
            audio_running: Some(running),
            inputs: self.input_devices.lock().ok().map(|d| d.clone()),
            plugins,
            render: running.then(|| Render {
                sample_rate: c.sample_rate,
                buffer_frames: c.buffer_frames(),
                avg_us: c.render_avg_ns as f64 / 1000.0,
                max_us: c.render_max_us as f64,
                xruns: Some(c.xruns),
            }),
            ..Default::default()
        }
    }

    /// Check the show as it is loaded now. A check already running is the
    /// one every caller gets.
    pub(super) fn start_preshow(&mut self) {
        if self.preshow.running() {
            return;
        }
        self.preshow.error = None;
        let name = self.open_session.clone().unwrap_or_else(|| "Untitled".into());
        let session = match self.capture_session(&name) {
            Ok(s) => s,
            Err(e) => return self.finish_preshow(Err(format!("could not read the rack: {e:#}"))),
        };
        let facts = self.preshow_facts();
        let (tx, rx) = mpsc::channel();
        let job = Job::Live { session, facts, frames: self.buffer_frames };
        if send_job(job, Box::new(move |r| drop(tx.send(r)))) {
            self.preshow.running = Some(rx);
        } else {
            self.finish_preshow(Err("the check thread could not start".into()));
        }
    }

    /// Take the running check's report when it is there (once per frame).
    pub(super) fn poll_preshow(&mut self) {
        let Some(rx) = &self.preshow.running else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("the check stopped before it finished".into()),
        };
        self.preshow.running = None;
        self.finish_preshow(result);
    }

    fn finish_preshow(&mut self, result: Result<Report, String>) {
        let answer = match result {
            Ok(report) => {
                let json = report.json().to_string();
                self.preshow.report = Some(report);
                json
            }
            Err(e) => {
                let text = format!("error: the pre-show check failed: {e}");
                self.preshow.error = Some(e);
                text
            }
        };
        for w in self.preshow.waiters.drain(..) {
            let _ = w.send(answer.clone());
        }
    }

    /// An agent asks for a check: of the live show, or of the saved session
    /// `session` (a name or a package path). `reply` gets the JSON report.
    pub(super) fn preshow_request(&mut self, args: &serde_json::Value, reply: mpsc::Sender<String>) {
        let Some(name) = args.get("session").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
            self.preshow.waiters.push(reply);
            return self.start_preshow();
        };
        let path = if std::path::Path::new(name).exists() {
            Ok(std::path::PathBuf::from(name))
        } else {
            crate::session::Session::package_path(&self.effective_sessions_dir(), name).map_err(|e| format!("{e:#}"))
        };
        let path = match path {
            Ok(p) => p,
            Err(e) => return drop(reply.send(format!("error: {e}"))),
        };
        let answer = reply.clone();
        let sent = send_job(
            Job::Package(path),
            Box::new(move |r| drop(answer.send(r.map_or_else(|e| format!("error: {e}"), |rep| rep.json().to_string())))),
        );
        if !sent {
            let _ = reply.send("error: the check thread could not start".into());
        }
    }

    /// `preshow_check` from the in-app assistant: the turn waits for the
    /// answer (`Waiting::Check`) instead of being told to try again.
    pub(super) fn preshow_tool(&mut self, args: &serde_json::Value) -> String {
        let (tx, rx) = mpsc::channel();
        self.preshow_request(args, tx);
        self.preshow.answer = Some(rx);
        "checking the show".into()
    }

    /// Open or close the Setlist view's check panel; opening it checks.
    pub(super) fn toggle_preshow_panel(&mut self) {
        self.preshow.open = !self.preshow.open;
        if self.preshow.open {
            self.setlist_levels_open = false;
            if self.preshow.report.is_none() {
                self.start_preshow();
            }
        }
    }

    /// The Setlist view's "Check show" panel: one row per check.
    pub(super) fn preshow_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let mut again = false;
        let busy = self.preshow.running();
        ui.horizontal(|ui| {
            ui.set_min_height(26.0);
            theme::section_label(ui, "Pre-show check");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if busy {
                    ui.add(egui::Spinner::new().size(16.0).color(theme::INK_2));
                } else if theme::primary_button(ui, if self.preshow.report.is_some() { "Check again" } else { "Check" })
                    .on_hover_text("devices, outputs, plugins, every rig, backing tracks, disk, buffer and power; changes nothing")
                    .clicked()
                {
                    again = true;
                }
            });
        });
        ui.add_space(4.0);
        let summary = if busy {
            egui::RichText::new("Checking every device, plugin and rig…").color(theme::INK_2)
        } else if let Some(e) = &self.preshow.error {
            egui::RichText::new(format!("The check failed: {e}")).color(theme::CLIP_BRIGHT)
        } else if let Some(r) = &self.preshow.report {
            let worst = r.rows.iter().map(|row| row.status).max().unwrap_or(Status::Ok);
            egui::RichText::new(format!("{} · {:.1} s", r.summary(), r.took_ms as f64 / 1000.0)).color(status_color(worst))
        } else {
            egui::RichText::new("Finds what could go wrong before the first song.").color(theme::INK_2)
        };
        ui.add(egui::Label::new(summary.size(12.5)).wrap());
        ui.add_space(8.0);
        Self::stage_hairline(ui);
        ui.add_space(6.0);
        let rows = self.preshow.report.as_ref().map(|r| r.rows.clone()).unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("preshow_scroll")
            .min_scrolled_height((height - 80.0).clamp(0.0, 120.0))
            .max_height((height - 80.0).max(0.0))
            .show(ui, |ui| {
                for row in &rows {
                    ui.horizontal_top(|ui| {
                        let (mark, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                        ui.painter().text(
                            mark.center(),
                            egui::Align2::CENTER_CENTER,
                            crate::preshow::symbol(row.status),
                            egui::FontId::proportional(14.0),
                            status_color(row.status),
                        );
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width());
                            ui.label(egui::RichText::new(row.title).font(theme::medium_font(12.5)).color(theme::INK));
                            ui.add(egui::Label::new(egui::RichText::new(&row.text).size(12.0).color(theme::INK_2)).wrap());
                        });
                    });
                    ui.add_space(6.0);
                }
            });
        if again {
            self.start_preshow();
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn texts_of(out: &egui::FullOutput) -> Vec<(String, Color32)> {
        fn walk(shape: &Shape, texts: &mut Vec<(String, Color32)>) {
            match shape {
                Shape::Text(t) => texts.push((t.galley.text().to_owned(), t.fallback_color)),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, texts)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for cs in &out.shapes {
            walk(&cs.shape, &mut texts);
        }
        texts
    }

    fn frame(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) -> Vec<(String, Color32)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        texts_of(&out)
    }

    fn frame_texts(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) -> Vec<String> {
        frame(app, ctx, size).into_iter().map(|(t, _)| t).collect()
    }

    /// Frames until the check thread answered (it probes no plugin here:
    /// the test rack's plugins are not in the scan of a machine without them).
    fn wait_for_report(app: &mut DemoApp, ctx: &egui::Context) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while app.preshow.running() {
            assert!(std::time::Instant::now() < deadline, "the check never answered");
            frame_texts(app, ctx, Vec2::new(1280.0, 800.0));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The real checks on a gig whose FM-1 is unplugged, whose lead rig
    /// needs Odin 2 and whose opener's backing track is gone, on battery.
    pub(in crate::ui) fn a_report() -> Report {
        use crate::preshow::tests::{clap, healthy, rig, show, ODIN};
        let mut s = show();
        rig(&mut s.graph, 3, clap(ODIN), "ODIN 2");
        s.rig_names.push("Lead".into());
        s.setlist.songs[0].name = "Opener".into();
        let mut f = healthy(&s);
        f.midi_ports = Some(vec![]);
        f.files = vec![("/gig/intro.wav".into(), crate::preshow::FileFact::Missing)];
        f.power = Some(crate::preshow::Power::Battery { percent: Some(64) });
        f.probe.as_mut().unwrap().rigs[3].1 = 0.0;
        Report::of(&s, &f, std::time::Duration::from_millis(2400))
    }

    /// A rack with the gig's songs, the second one live.
    fn with_songs(app: &mut DemoApp) {
        for name in ["Opener", "Ballad", "Encore"] {
            app.exec_tool("set_setlist_song", &serde_json::json!({"name": name}));
        }
        app.exec_tool("select_setlist_part", &serde_json::json!({"song": 1}));
    }

    #[test]
    fn the_check_show_chip_opens_the_panel_and_runs_the_check() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        app.stage_view = StageView::Setlist;
        app.setlist_levels_open = true;
        app.toggle_preshow_panel();
        assert!(!app.setlist_levels_open, "one panel at a time");
        wait_for_report(&mut app, &ctx);
        let texts = frame_texts(&mut app, &ctx, Vec2::new(1280.0, 800.0));
        let report = app.preshow.report.clone().expect("a report");
        assert_eq!(report.rows.len(), crate::preshow::CHECKS.len());
        for row in &report.rows {
            assert!(texts.iter().any(|t| t == row.title), "{} is shown: {texts:?}", row.title);
        }
        assert!(texts.iter().any(|t| t.starts_with(&report.summary())), "the summary line: {texts:?}");
        assert!(texts.iter().any(|t| t == "Check again"));
    }

    #[test]
    fn the_agent_tool_waits_for_the_report_and_the_bridge_too() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        assert_eq!(app.exec_tool("preshow_check", &serde_json::json!({})), "checking the show");
        let answer = app.preshow.answer.take().expect("the assistant waits on this");
        let (tx, bridged) = mpsc::channel();
        app.serve_ui_requests(vec![("preshow_check".into(), serde_json::json!({}), tx)]);
        assert!(bridged.try_recv().is_err(), "the bridge call waits for the check too");
        wait_for_report(&mut app, &ctx);
        for text in [answer.recv().unwrap(), bridged.recv().unwrap()] {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("JSON: {text}"));
            assert_eq!(v["checks"].as_array().map(Vec::len), Some(crate::preshow::CHECKS.len()), "{v}");
            assert!(v["summary"].is_string());
        }
        assert!(tool_is_read_only("preshow_check"), "no undo snapshot, no action chip");
    }

    #[test]
    fn a_saved_session_that_does_not_exist_is_answered_in_words() {
        let mut app = DemoApp::new_with_sink(None);
        let (tx, rx) = mpsc::channel();
        app.preshow_request(&serde_json::json!({"session": "../outside"}), tx);
        let text = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(text.starts_with("error: "), "{text}");
    }

    #[test]
    fn the_panel_lists_each_check_with_its_status_colour() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        app.stage_view = StageView::Setlist;
        app.preshow.open = true;
        app.preshow.report = Some(a_report());
        let marks: Vec<(String, Color32)> =
            frame(&mut app, &ctx, Vec2::new(1280.0, 800.0)).into_iter().filter(|(t, _)| ["✓", "!", "✗"].contains(&t.as_str())).collect();
        assert_eq!(marks.len(), crate::preshow::CHECKS.len(), "{marks:?}");
        for (mark, colour) in [("✗", theme::CLIP_BRIGHT), ("!", theme::ACCENT_BRIGHT), ("✓", theme::LIVE_TEAL)] {
            assert!(marks.contains(&(mark.into(), colour)), "{mark} in its colour: {marks:?}");
        }
        assert!(!app.preshow.running(), "a report on screen is not checked again by itself");
    }

    /// A 0.1 s stereo float WAV, as the recorder writes them.
    fn write_wav(path: &std::path::Path) {
        let data = vec![0u8; 4800 * 2 * 4];
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(((36 + data.len()) as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(3u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(48000u32.to_le_bytes());
        b.extend((48000u32 * 8).to_le_bytes());
        b.extend(8u16.to_le_bytes());
        b.extend(32u16.to_le_bytes());
        b.extend(b"data");
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn a_song_starts_its_backing_track_and_a_song_without_one_stops_it() {
        let mut app = DemoApp::new_with_sink(None);
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let (tx, mut decoded) = rtrb::RingBuffer::<crate::player::PlayerData>::new(4);
        app.player_ctrl = Some(crate::player::start(tx));
        let wav = std::env::temp_dir().join(format!("ksalat-backing-{}.wav", std::process::id()));
        write_wav(&wav);
        let main = |n: &str| serde_json::json!([{"name": n, "rigs": [{"rig": 0}]}]);
        let mut intro = serde_json::json!({"name": "Intro", "parts": [{"name": "Verse", "rigs": [{"rig": 0}]}, {"name": "Chorus", "rigs": [{"rig": 0}]}]});
        intro["backing"] = serde_json::json!(wav.to_string_lossy());
        app.exec_tool("set_setlist_song", &intro);
        app.exec_tool("set_setlist_song", &serde_json::json!({"name": "Ballad", "parts": main("Main")}));
        let refused = app.exec_tool("set_setlist_song", &serde_json::json!({"song": 1, "backing": "ballad.wav"}));
        assert!(refused.contains("absolute path"), "{refused}");
        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("get_setlist", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["songs"][0]["backing"], serde_json::json!(wav.to_string_lossy()));
        assert!(listed["songs"][1]["backing"].is_null());

        let commands = |drain: &mut crate::control::CommandDrain| {
            let mut out = Vec::new();
            drain.drain(|c| out.push(c));
            out
        };
        app.exec_tool("select_setlist_part", &serde_json::json!({"song": 0}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let file = loop {
            if let Ok(d) = decoded.pop() {
                break d;
            }
            assert!(std::time::Instant::now() < deadline, "the backing track was never decoded");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(file.frames(), 4800);
        assert!(commands(&mut drain).iter().any(|c| matches!(c, Command::SetPlayer { gain } if *gain == 0.8)), "the player was off: it comes up");
        app.exec_tool("select_setlist_part", &serde_json::json!({"action": "next_part"}));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(decoded.pop().is_err(), "the chorus of the same song leaves the track playing");
        assert!(!commands(&mut drain).iter().any(|c| matches!(c, Command::SetPlayerEnable { .. })));
        app.exec_tool("select_setlist_part", &serde_json::json!({"action": "next_part"}));
        assert!(commands(&mut drain).iter().any(|c| matches!(c, Command::SetPlayerEnable { on: false })), "the ballad has no track: the intro's stops");
        let _ = std::fs::remove_file(&wav);
    }

    /// Writes two 20-song shows of two Surge rigs with the app's own capture
    /// and save: one whose devices, plugins and backing track are there,
    /// one that misses the FM-1, Odin 2 and its backing track.
    #[test]
    #[ignore = "writes sessions: KABELSALAT_SHOW=<dir> cargo test --bin kabelsalat write_preshow_shows -- --ignored"]
    fn write_preshow_shows() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOW").expect("KABELSALAT_SHOW=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("intro.wav");
        write_wav(&wav);
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        for i in 0..20 {
            let mut song = serde_json::json!({"name": format!("Song {}", i + 1), "parts": [{"name": "Main", "rigs": [{"rig": i % 2}]}]});
            if i == 0 {
                song["backing"] = serde_json::json!(wav.to_string_lossy());
            }
            app.exec_tool("set_setlist_song", &song);
        }
        let healthy = app.capture_session("Friday gig").unwrap();
        healthy.save(&dir.join("Friday gig.ksalat")).unwrap();
        let mut broken = healthy.clone();
        broken.name = "Friday gig (broken)".into();
        let odin = broken.graph.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new("com.thewavewarden.odin2") }, name_bytes("ODIN 2"));
        broken.graph.nodes.iter_mut().find(|n| n.id == odin).unwrap().rig = 1;
        broken.set_hw_ports([(0, "FM-1:FM-1 MIDI 1".to_string())].into_iter().collect());
        broken.rig_devices = vec![Some(0)];
        broken.setlist.songs[0].backing = Some(dir.join("gone.wav").to_string_lossy().into_owned());
        broken.save(&dir.join("Friday gig (broken).ksalat")).unwrap();
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat preshow_shots -- --ignored"]
    fn preshow_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        // Rule 148: a fresh app per image.
        for size in [Vec2::new(1280.0, 800.0), Vec2::new(781.0, 950.0), Vec2::new(760.0, 520.0)] {
            let mut app = DemoApp::new_with_sink(None);
            with_songs(&mut app);
            app.stage_view = StageView::Setlist;
            app.preshow.open = true;
            app.preshow.report = Some(a_report());
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("preshow-{}x{}.png", size.x, size.y)));
        }
    }
}
