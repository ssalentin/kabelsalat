//! Show controls in the app (#277): learning a pedal, key or button for a
//! show action, the hardware messages a show control takes (no key, knob or
//! knob learn moves on them), the actions that are the app's own (the tuner,
//! recording), the one list of actions in the Setlist view and the tuner.
//! The engine consumes the messages and does the rest (`crate::show`).

use super::*;
use crate::show::{ShowAction, ShowMsg};

/// One row of the action list, and the width of its action's name.
const ROW_H: f32 = 28.0;
const ACTION_W: f32 = 100.0;
/// The tuner panel's size and the cents its needle spans either way.
const TUNER_SIZE: Vec2 = Vec2::new(340.0, 168.0);
const TUNER_SPAN_CENTS: f32 = 50.0;
/// In tune: the needle and the note turn teal within this many cents.
const IN_TUNE_CENTS: f32 = 3.0;

/// What the tuner shows.
enum TunerReading {
    /// No Audio In unit in the rack.
    NoInput,
    /// An input, and the pitch it plays (none while it is quiet).
    Input { name: String, pitch: Option<crate::audio_in::Pitch> },
}

impl DemoApp {
    /// A hardware message the app sees after the engine took it: learned
    /// for the armed action on a press, or one a show control takes. True
    /// when the message is a show control's, so no key, knob or knob learn
    /// moves on it (the engine gave it to no instrument).
    pub(super) fn show_control_msg(&mut self, source: SourceId, channel: u8, msg: ShowMsg, press: bool) -> bool {
        let SourceId::Hardware(device) = source else { return false };
        if let Some(action) = self.show_learn.filter(|_| press) {
            self.show_learn = None;
            let what = crate::setlist::msg_label(msg);
            self.session_msg = match self.setlist.learn(action, Some(device), msg) {
                Some(other) => format!("Learned {what} for {} (no longer {})", action.label(), other.label()),
                None => format!("Learned {what} for {}", action.label()),
            };
            return true;
        }
        self.setlist.show_controls().iter().flatten().any(|c| c.takes(device, channel, msg))
    }

    /// Do what the engine counted presses of and only the app can do: the
    /// tuner and recording; a Panic or a tap also shows here.
    pub(super) fn follow_show_presses(&mut self) {
        let Some(p) = &self.graph_pub else { return };
        let Ok(g) = p.lock() else { return };
        let now = g.show_presses;
        drop(g);
        let seen = std::mem::replace(&mut self.show_presses_seen, now);
        let moved = |a: ShowAction| now[a as usize].wrapping_sub(seen[a as usize]);
        if moved(ShowAction::Tuner) % 2 == 1 {
            self.tuner_on = !self.tuner_on;
        }
        if moved(ShowAction::Record) % 2 == 1 {
            self.toggle_recording();
        }
        if moved(ShowAction::Panic) > 0 {
            self.panicked();
        }
        if moved(ShowAction::TapTempo) > 0 {
            self.tap_flash_t = 1.0;
        }
    }

    /// The Setlist view's show controls: every action with its control and
    /// Learn / Clear, then each device whose program change can pick the song.
    pub(super) fn show_controls_rows(
        ui: &mut egui::Ui,
        setlist: &mut crate::setlist::Setlist,
        learn: &mut Option<ShowAction>,
        devices: &[(u32, String, bool)],
    ) {
        let device_name = |d: u32| devices.iter().find(|r| r.0 == d).map_or_else(|| format!("Device {d}"), |r| r.1.clone());
        theme::section_label(ui, "Show controls");
        ui.add_space(6.0);
        for action in ShowAction::LEARNED {
            let learning = *learn == Some(action);
            let control = setlist.control_of(action);
            ui.horizontal(|ui| {
                ui.set_min_height(ROW_H - 4.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // Learn keeps its column whether or not there is a Clear.
                    if control.is_some() {
                        if theme::chip_button(ui, "Clear", false).clicked() {
                            setlist.forget(action);
                        }
                    } else {
                        ui.add_space(theme::chip_button_width(ui, "Clear") + 4.0);
                    }
                    if theme::chip_button(ui, if learning { "Cancel" } else { "Learn" }, learning).clicked() {
                        *learn = if learning { None } else { Some(action) };
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.allocate_ui_with_layout(Vec2::new(ACTION_W, ROW_H - 4.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(ACTION_W);
                            ui.add(egui::Label::new(egui::RichText::new(action.label()).size(12.0).color(theme::INK)).truncate());
                        });
                        let (text, color) = match (learning, control) {
                            (true, _) => ("Press a pedal, key or button…".to_string(), theme::ACCENT),
                            (false, Some(c)) => {
                                let what = crate::setlist::msg_label(c.msg);
                                (c.device.map_or(what.clone(), |d| format!("{what} · {}", device_name(d))), theme::INK)
                            }
                            (false, None) => ("Unbound".to_string(), theme::INK_2),
                        };
                        ui.add(egui::Label::new(egui::RichText::new(text).size(11.5).color(color)).truncate());
                    });
                });
            });
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new(ShowAction::SongOfProgram.label()).size(12.0).color(theme::INK));
        if devices.is_empty() {
            ui.label(egui::RichText::new("Plug in a MIDI device to pick songs with its program buttons.").size(11.5).color(theme::INK_2));
        }
        for (d, name, live) in devices {
            let on = setlist.program_selects_song(*d);
            let full = !on && setlist.program_devices().count() >= crate::setlist::MAX_PROGRAM_DEVICES;
            ui.horizontal(|ui| {
                ui.set_min_height(ROW_H - 4.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let hint = if full {
                        format!("At most {} devices pick songs", crate::setlist::MAX_PROGRAM_DEVICES)
                    } else {
                        "Program N selects song N+1; this device's program changes no longer reach its instruments".to_string()
                    };
                    let chip = ui.add_enabled_ui(!full, |ui| theme::chip_button(ui, if on { "On" } else { "Off" }, on)).inner;
                    if chip.on_hover_text(hint).clicked() {
                        setlist.set_program_selects_song(*d, !on);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let color = if *live { theme::INK } else { theme::INK_2 };
                        ui.add(egui::Label::new(egui::RichText::new(name).size(12.0).color(color)).truncate());
                    });
                });
            });
        }
    }

    /// `get_show_controls`: every action, its control and the devices that
    /// can pick songs, with this run's device names.
    pub(super) fn show_controls_json(&self) -> serde_json::Value {
        let devices: Vec<u32> = (0..self.hw_devices.len() as u32).collect();
        self.setlist.show_controls_json(&devices, &|d| self.hw_device_label(d))
    }

    fn tuner_reading(&self) -> TunerReading {
        let Some(u) = self.units.iter().find(|u| matches!(u.node_kind, UnitKind::AudioIn)) else { return TunerReading::NoInput };
        let pitch = self.audio_in_tuners.get(&u.node_id).and_then(|(_, _, p)| *p);
        let device = u.device.as_deref().map_or_else(|| "default input".to_string(), crate::audio_in::pretty_device_name);
        TunerReading::Input { name: format!("{} · {device}", sentence_case(&u.name)), pitch }
    }

    /// The tuner a show control turns on (#277), over whatever screen is
    /// open: the note the first Audio In plays and how far off it is.
    /// Clicking it closes it.
    pub(super) fn tuner_panel(&mut self, ctx: &egui::Context) {
        if !self.tuner_on {
            return;
        }
        let reading = self.tuner_reading();
        let area = egui::Area::new(egui::Id::new("show_tuner")).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 72.0));
        let closed = area
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(TUNER_SIZE, egui::Sense::click());
                let p = ui.painter_at(rect);
                p.rect(rect, 18.0, theme::DARK_PANEL, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
                let inner = rect.shrink2(Vec2::new(22.0, 16.0));
                let cap = p.fonts_mut(|f| theme::layout_tracked(f, "TUNER", FontId::proportional(12.0), 0.14));
                theme::paint_tracked(&p, inner.left_top(), &cap, theme::CREAM_DIM);
                let close = p.layout_no_wrap("click to close".into(), FontId::proportional(11.0), theme::CREAM_DIM);
                p.galley(Pos2::new(inner.right() - close.size().x, inner.top()), close, theme::CREAM_DIM);
                let (note, cents, line) = match &reading {
                    TunerReading::NoInput => (None, None, "No Audio In unit in the rack".to_string()),
                    TunerReading::Input { name, pitch: None } => (None, None, format!("{name} · play a note")),
                    TunerReading::Input { name, pitch: Some(pitch) } => (Some(crate::audio_in::note_name(pitch.note)), Some(pitch.cents), name.clone()),
                };
                let in_tune = cents.is_some_and(|c| c.abs() <= IN_TUNE_CENTS);
                let tone = if in_tune { theme::LIVE_TEAL } else { theme::CREAM };
                let big = p.layout_no_wrap(note.unwrap_or_else(|| "–".into()), theme::medium_font(48.0), tone);
                let big_top = inner.top() + cap.size.y + 6.0;
                p.galley(Pos2::new(inner.left(), big_top), big.clone(), tone);
                if let Some(c) = cents {
                    let txt = p.layout_no_wrap(format!("{c:+.0} ¢"), theme::medium_font(24.0), tone);
                    p.galley(Pos2::new(inner.right() - txt.size().x, big_top + big.size().y - txt.size().y - 6.0), txt, tone);
                }
                let bar = Rect::from_min_size(Pos2::new(inner.left(), big_top + big.size().y + 8.0), Vec2::new(inner.width(), 8.0));
                p.rect_filled(bar, 4.0, theme::METER_TRACK);
                p.rect_filled(Rect::from_center_size(bar.center(), Vec2::new(2.0, 16.0)), 1.0, theme::CREAM_DIM);
                if let Some(c) = cents {
                    let x = bar.center().x + (c / TUNER_SPAN_CENTS).clamp(-1.0, 1.0) * bar.width() / 2.0;
                    p.rect_filled(Rect::from_center_size(Pos2::new(x, bar.center().y), Vec2::new(6.0, 20.0)), 3.0, if in_tune { theme::LIVE_TEAL } else { theme::ACCENT });
                }
                let mut job = egui::text::LayoutJob::single_section(line, egui::TextFormat::simple(FontId::proportional(12.0), theme::CREAM_DIM));
                job.wrap = egui::text::TextWrapping::truncate_at_width(inner.width());
                let line = p.layout_job(job);
                p.galley(Pos2::new(inner.left(), inner.bottom() - line.size().y), line, theme::CREAM_DIM);
                resp.clicked()
            })
            .inner;
        if closed {
            self.tuner_on = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::show::{ShowAction, ShowMsg};
    use std::sync::{Arc, Mutex};

    const PEDAL: u32 = 1;

    /// Tall enough for the whole list: in a shorter window it scrolls.
    const SIZE: Vec2 = Vec2::new(1280.0, 1400.0);

    fn frame(app: &mut DemoApp, ctx: &egui::Context) {
        let mut raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)), ..Default::default() };
        raw.time = Some(app.ui_time as f64 + 1.0);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
    }

    /// An app with a gig of three songs, wired to a real engine the way the
    /// running app is: the MIDI thread sends to the engine first
    /// (`hw_direct`) and queues for the app, and what the app sends goes
    /// through `sent` on its way to the engine.
    struct Stage {
        app: DemoApp,
        ctx: egui::Context,
        engine: crate::engine::GraphEngine,
        pub_: Arc<Mutex<crate::engine::GraphPub>>,
        engine_bus: Arc<Mutex<crate::control::CommandBus>>,
        app_drain: crate::control::CommandDrain,
        sent: Vec<Command>,
        queues: HwQueues,
    }

    impl Stage {
        fn new() -> Self {
            let mut app = DemoApp::new_with_sink(None);
            for (name, n) in [("One", 2), ("Two", 1), ("Three", 1)] {
                let parts = (0..n).map(|i| crate::setlist::Part::new(format!("{name} {i}"), vec![crate::setlist::PartRig::new(0)])).collect();
                app.setlist.songs.push(crate::setlist::Song::new(name, parts));
            }
            let (tx, drain) = crate::control::command_bus();
            let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), drain, 48000.0, vec![]);
            let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
            engine.set_graph_pub(pub_.clone());
            let engine_bus = Arc::new(Mutex::new(tx));
            *app.hw_direct.bus.lock().unwrap() = Some(engine_bus.clone());
            let (app_tx, app_drain) = crate::control::command_bus();
            app.bus = Some(Arc::new(Mutex::new(app_tx)));
            app.graph_pub = Some(pub_.clone());
            let queues = app.hw_queues();
            let mut s = Stage { app, ctx: egui::Context::default(), engine, pub_, engine_bus, app_drain, sent: Vec::new(), queues };
            s.tick();
            s
        }

        /// One UI frame, then one engine block with what the frame sent.
        fn tick(&mut self) {
            frame(&mut self.app, &self.ctx);
            let mut sent = Vec::new();
            self.app_drain.drain(|c| sent.push(c));
            for c in &sent {
                self.engine_bus.lock().unwrap().send(c.clone());
            }
            self.sent.extend(sent);
            let mut buf = vec![0.0f32; 256 * 2];
            self.engine.render(&mut buf, 2);
        }

        /// Raw bytes from the pedal, as the MIDI thread hands them on; then
        /// the engine's block and the app's frame.
        fn midi(&mut self, bytes: &[u8]) {
            hw_midi_in(PEDAL, bytes, &self.queues, &self.app.hw_direct);
            let mut buf = vec![0.0f32; 256 * 2];
            self.engine.render(&mut buf, 2);
            self.tick();
        }

        fn tap(&mut self, press: &[u8]) {
            self.midi(press);
            match press[0] & 0xF0 {
                0x90 => self.midi(&[0x80 | (press[0] & 0x0F), press[1], 0]),
                0xB0 => self.midi(&[press[0], press[1], 0]),
                _ => {}
            }
        }

        fn presses(&self, a: ShowAction) -> u64 {
            self.pub_.lock().unwrap().show_presses[a as usize]
        }
    }

    /// The press of each action's control, as raw bytes: notes, CCs and
    /// program changes on several channels.
    fn press_of(i: usize) -> Vec<u8> {
        match i % 3 {
            0 => vec![0x90 | (i as u8 % 4), 36 + i as u8, 100],
            1 => vec![0xB0 | (i as u8 % 4), 80 + i as u8, 127],
            _ => vec![0xC0 | (i as u8 % 4), 10 + i as u8],
        }
    }

    #[test]
    fn every_action_is_learned_from_raw_midi_bytes_and_triggered_by_them() {
        let mut s = Stage::new();
        for (i, action) in ShowAction::LEARNED.into_iter().enumerate() {
            s.app.show_learn = Some(action);
            s.tap(&press_of(i));
            let c = s.app.setlist.control_of(action).unwrap_or_else(|| panic!("{action:?} learned nothing"));
            assert_eq!((c.device, c.channel), (Some(PEDAL), None), "{action:?}: learned from the pedal, on any channel");
        }
        let learned: Vec<ShowMsg> = ShowAction::LEARNED.iter().map(|&a| s.app.setlist.control_of(a).unwrap().msg).collect();
        assert_eq!(learned[..3], [ShowMsg::Note(36), ShowMsg::Cc(81), ShowMsg::Program(12)], "a note, a CC and a program change each bind");
        s.tick();
        assert!(s.sent.iter().any(|c| matches!(c, Command::SetShowControls { controls } if controls.iter().flatten().count() == ShowAction::LEARNED.len())), "the engine got every control");

        s.app.select_setlist(0, 0).unwrap();
        s.tick();
        let at = |s: &Stage| s.app.setlist.active.map(|p| (p.song, p.part));
        let idx = |a: ShowAction| ShowAction::LEARNED.iter().position(|&x| x == a).unwrap();
        s.tap(&press_of(idx(ShowAction::NextPart)));
        assert_eq!(at(&s), Some((0, 1)), "next part");
        s.tap(&press_of(idx(ShowAction::NextSong)));
        assert_eq!(at(&s), Some((1, 0)), "next song");
        s.tap(&press_of(idx(ShowAction::PrevSong)));
        assert_eq!(at(&s), Some((0, 0)), "previous song, at its first part");
        s.tap(&press_of(idx(ShowAction::NextSong)));
        s.tap(&press_of(idx(ShowAction::PrevPart)));
        assert_eq!(at(&s), Some((0, 1)), "previous part, across the song boundary");

        s.app.notes_down[60] = 1;
        s.tap(&press_of(idx(ShowAction::Panic)));
        assert_eq!(s.presses(ShowAction::Panic), 1);
        assert_eq!(s.app.notes_down[60], 0, "the app's keys go dark with the engine's panic");

        for _ in 0..4 {
            s.tap(&press_of(idx(ShowAction::TapTempo)));
            let mut buf = vec![0.0f32; 256 * 2];
            for _ in 0..(48000 / 2 / 256) - 2 {
                s.engine.render(&mut buf, 2);
            }
        }
        s.tick();
        assert!((s.app.bpm - 120.0).abs() <= 2.0, "four taps half a second apart: {} BPM", s.app.bpm);

        s.tap(&press_of(idx(ShowAction::MasterMute)));
        assert!(s.pub_.lock().unwrap().master_mute);
        s.tap(&press_of(idx(ShowAction::Tuner)));
        assert!(s.app.tuner_on, "the tuner opens");
        s.tap(&press_of(idx(ShowAction::Tuner)));
        assert!(!s.app.tuner_on, "and closes");
        s.sent.clear();
        s.tap(&press_of(idx(ShowAction::Record)));
        assert!(s.sent.iter().any(|c| matches!(c, Command::RecordStart)), "the app starts recording: {:?}", s.sent);
        s.tap(&press_of(idx(ShowAction::BackingTrack)));
        assert_eq!(s.presses(ShowAction::BackingTrack), 1, "the backing track's press reached the engine");
        for a in ShowAction::LEARNED {
            assert!(s.presses(a) >= 1, "{a:?} was never pressed");
        }
    }

    #[test]
    fn a_show_controls_messages_move_no_key_and_no_knob_learn() {
        let mut s = Stage::new();
        s.app.show_learn = Some(ShowAction::NextPart);
        s.midi(&[0x90, 36, 100]);
        assert_eq!(s.app.notes_down[36], 0, "the key a learn binds does not light");
        s.midi(&[0x80, 36, 0]);
        s.midi(&[0x90, 36, 100]);
        assert_eq!(s.app.notes_down[36], 0, "nor does its next press");
        s.midi(&[0x90, 37, 100]);
        assert_ne!(s.app.notes_down[37], 0, "another key plays as before");
    }

    fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        fn walk(shape: &Shape, out: &mut Vec<String>) {
            match shape {
                Shape::Text(t) => out.push(t.galley.text().to_owned()),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        shapes.iter().for_each(|cs| walk(&cs.shape, &mut out));
        out
    }

    #[test]
    fn the_setlist_view_lists_every_action_and_the_tuner_shows_over_it() {
        let mut app = DemoApp::new_with_sink(None);
        app.stage_view = StageView::Setlist;
        app.tuner_on = true;
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)), time: Some(5.0), ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        let texts = texts(&out.shapes);
        for a in ShowAction::LEARNED {
            assert!(texts.iter().any(|t| t == a.label()), "{} is listed: {texts:?}", a.label());
        }
        assert!(texts.iter().any(|t| t == ShowAction::SongOfProgram.label()), "and the program devices' row");
        assert!(texts.iter().any(|t| t == "No Audio In unit in the rack"), "the tuner says what it has to tune: {texts:?}");
    }

    #[test]
    fn the_show_control_tools_are_in_every_table_and_cover_program_devices() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for name in ["get_show_controls", "set_show_control"] {
            assert!(names.contains(&name), "assistant tools miss {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS miss {name}");
            assert!(include_str!("../mcp.rs").contains(&format!("#[tool(name = \"{name}\"")), "MCP misses {name}");
        }
        assert!(!names.contains(&"set_setlist_footswitch"), "one tool for every action");
        let mut app = DemoApp::new_with_sink(None);
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("set_show_control", &serde_json::json!({"action": "record", "note": 40, "device": 0}))).unwrap();
        assert_eq!(v["actions"].as_array().unwrap().iter().find(|r| r["action"] == "record").unwrap()["control"]["note"], 40, "{v}");
        app.exec_tool("set_show_control", &serde_json::json!({"action": "song_of_program", "device": 3}));
        assert!(app.setlist.program_selects_song(3));
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_show_controls", &serde_json::json!({}))).unwrap();
        assert!(v["song_of_program"].as_array().unwrap().iter().any(|r| r["device"] == 3 && r["on"] == true), "{v}");
        assert!(app.exec_tool("set_show_control", &serde_json::json!({"action": "nope"})).starts_with("error"));
    }
    /// An Audio In unit whose tuner hears a steady 442 Hz sine (A4, +8 ¢),
    /// through the real ring and estimator the input feeds.
    fn tuning(app: &mut DemoApp) {
        let mut g = app.base_graph.clone();
        let node = g.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::MonoL }, crate::control::name_bytes("GUITAR"));
        app.build_rack(&g, &[]);
        let (mut tx, rx) = rtrb::RingBuffer::new(48000);
        for i in 0..24000 {
            let _ = tx.push((std::f32::consts::TAU * 442.0 * i as f32 / 48000.0).sin() * 0.5);
        }
        app.audio_in_tuners.insert(node, (rx, crate::audio_in::Tuner::new(48000), None));
        app.tuner_on = true;
    }

    #[test]
    fn the_tuner_shows_the_note_the_audio_in_plays_and_a_click_closes_it() {
        let mut app = DemoApp::new_with_sink(None);
        tuning(&mut app);
        let ctx = egui::Context::default();
        let mut texts_now = |app: &mut DemoApp, events: Vec<egui::Event>| {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)), time: Some(app.ui_time as f64 + 1.0), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
            texts(&out.shapes)
        };
        texts_now(&mut app, vec![]);
        let shown = texts_now(&mut app, vec![]);
        assert!(shown.iter().any(|t| t == "A4") && shown.iter().any(|t| t == "+8 ¢"), "{shown:?}");
        let at = Pos2::new(SIZE.x / 2.0, 72.0 + super::TUNER_SIZE.y / 2.0);
        texts_now(&mut app, vec![egui::Event::PointerMoved(at)]);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        texts_now(&mut app, vec![press(true)]);
        texts_now(&mut app, vec![press(false)]);
        assert!(!app.tuner_on, "a click on the tuner closes it");
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat show_controls_shots -- --ignored"]
    fn show_controls_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let gig = |tune: bool| {
            let mut app = DemoApp::new_with_sink(None);
            if tune {
                tuning(&mut app);
            }
            for (name, bpm) in [("Opener", 124), ("Ballad", 72), ("Closer", 132)] {
                let parts = serde_json::json!([{"name": "Verse", "rigs": [{"rig": 0}]}, {"name": "Chorus", "rigs": [{"rig": 0}]}]);
                app.exec_tool("set_setlist_song", &serde_json::json!({"name": name, "bpm": bpm, "parts": parts.to_string()}));
            }
            for (action, msg) in [
                (ShowAction::NextPart, ShowMsg::Cc(80)),
                (ShowAction::PrevPart, ShowMsg::Cc(81)),
                (ShowAction::NextSong, ShowMsg::Note(36)),
                (ShowAction::Panic, ShowMsg::Note(37)),
                (ShowAction::Tuner, ShowMsg::Program(9)),
                (ShowAction::Record, ShowMsg::Cc(85)),
            ] {
                app.setlist.learn(action, Some(1), msg);
            }
            app.select_setlist(0, 1).unwrap();
            app
        };
        // Rule 148: a fresh app per image.
        for size in [Vec2::new(1280.0, 1000.0), Vec2::new(781.0, 950.0)] {
            let mut app = gig(false);
            app.stage_view = StageView::Setlist;
            app.show_learn = Some(ShowAction::TapTempo);
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("show-controls-{}x{}.png", size.x, size.y)));
        }
        let mut app = gig(true);
        app.set_show_mode(true);
        super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("show-tuner-1280x800.png"));
    }
}
