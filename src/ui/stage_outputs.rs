//! The stage output matrix (#399) in the app: the mirror of the engine's
//! assignments, the Mixer's rows under Master out, the agent tools, and the
//! reopen of the output stream when it lands on an interface with another
//! number of channels. The engine places the audio (`Command::SetOutRoute`);
//! `crate::out_routes` holds what the app shares with headless MCP.

use super::*;
use crate::control::{OutChannels, OutRoute, OutSource};
use crate::out_routes::{db_level, default_place, level_db, menu_options, place, route_json};

/// What the app keeps for the matrix.
pub(super) struct StageOutputs {
    /// The mirror of the engine's table (shares the sends' grace).
    pub(super) routes: Vec<OutRoute>,
    /// Channels of the open output stream, as the engine published them.
    pub(super) channels: usize,
    /// The menus drawn this frame, for tests: (source, rect).
    pub(super) menu_rects: Vec<(OutSource, Rect)>,
    /// The (sink, channels) the stream was last reopened for, so one sink
    /// is tried once.
    reopened_for: Option<(String, usize)>,
}

impl Default for StageOutputs {
    fn default() -> Self {
        Self { routes: Vec::new(), channels: 2, menu_rects: Vec::new(), reopened_for: None }
    }
}

impl DemoApp {
    /// The channels `source` is assigned to; `None` = where it plays by
    /// default.
    pub(super) fn out_route_of(&self, source: OutSource) -> Option<OutChannels> {
        self.stage_out.routes.iter().find(|r| r.source == source).map(|r| r.to)
    }

    /// What the mixer and the tools call a source.
    pub(super) fn out_source_name(&self, source: OutSource) -> String {
        match source {
            OutSource::Master => "Master".into(),
            OutSource::Click => "Click".into(),
            OutSource::Player => "Backing track".into(),
            OutSource::Bus(SendSource::Rig(r)) => format!("{}'s rig bus", self.rig_name(r as usize)),
            OutSource::Bus(SendSource::Node(n)) => self.unit_display_name(n),
        }
    }

    /// Assign `source` to channels of the interface, or take the assignment
    /// away for `None`; in the mirror and in the engine (rule 25). Refuses
    /// what the engine would ignore.
    pub(super) fn set_out_route(&mut self, source: OutSource, to: Option<OutChannels>) -> Result<(), String> {
        match source {
            OutSource::Bus(SendSource::Rig(r)) if r as usize >= self.rig_count() => {
                return Err(format!("no rig {r} (rigs are 0..{})", self.rig_count().saturating_sub(1)));
            }
            OutSource::Bus(SendSource::Node(n)) if !self.bus_units().iter().any(|&u| self.units[u].node_id == n) => {
                return Err(format!("node {n} is not an aux bus (get_output_routes lists the buses; a rig bus is rig=)"));
            }
            _ => {}
        }
        let to = to.map(OutChannels::clamped);
        if let Some(t) = to {
            if !t.fits(crate::engine::MAX_OUT_CH) {
                return Err(format!("outputs are 1..{}", crate::engine::MAX_OUT_CH));
            }
            let held = self.stage_out.routes.iter().any(|r| r.source == source);
            if !held && self.stage_out.routes.len() >= crate::engine::MAX_OUT_ROUTES {
                return Err(format!("{} sources already have outputs of their own, the most kabelsalat serves at once: take one back first", crate::engine::MAX_OUT_ROUTES));
            }
        }
        match (to, self.stage_out.routes.iter_mut().find(|r| r.source == source)) {
            (Some(t), Some(r)) => r.to = t,
            (Some(t), None) => self.stage_out.routes.push(OutRoute { source, to: t }),
            (None, _) => self.stage_out.routes.retain(|r| r.source != source),
        }
        if let (Some(_), OutSource::Bus(bus)) = (to, source) {
            // One place per bus, as in the engine.
            self.bus_outs.retain(|o| o.bus != bus);
        }
        self.ui_send_grace = self.ui_time + 0.6;
        if let Some(b) = &self.bus {
            let _ = b.lock().unwrap().send(Command::SetOutRoute { source, to });
        }
        Ok(())
    }

    /// The assignments a load left without their rig or bus are gone; the
    /// rest go to the engine again, whose table the load dropped.
    pub(super) fn resend_out_routes(&mut self) {
        let rigs = self.rig_count();
        let buses: Vec<u32> = self.bus_units().iter().map(|&u| self.units[u].node_id).collect();
        self.stage_out.routes.retain(|r| match r.source {
            OutSource::Bus(SendSource::Rig(x)) => (x as usize) < rigs,
            OutSource::Bus(SendSource::Node(n)) => buses.contains(&n),
            _ => true,
        });
        if let Some(bus) = &self.bus {
            let mut bus = bus.lock().unwrap();
            for r in &self.stage_out.routes {
                let _ = bus.send(Command::SetOutRoute { source: r.source, to: Some(r.to) });
            }
        }
    }

    /// Every source of the matrix with its assignment, for agents.
    pub(super) fn out_routes_json(&self) -> serde_json::Value {
        let ch = self.stage_out.channels;
        let one = |s: OutSource| route_json(s, &self.out_source_name(s), self.out_route_of(s), ch);
        let mut sources = vec![one(OutSource::Master), one(OutSource::Click), one(OutSource::Player)];
        for r in 0..self.rig_count() {
            let bus = SendSource::Rig(r as u8);
            let mut v = one(OutSource::Bus(bus));
            if let Some(sink) = self.bus_output_of(bus) {
                v["plays_on"] = serde_json::json!(format!("its own sound card, {} (set_bus_output)", sink.as_str()));
            }
            sources.push(v);
        }
        for &u in &self.bus_units() {
            let bus = SendSource::Node(self.units[u].node_id);
            let mut v = one(OutSource::Bus(bus));
            if let Some(sink) = self.bus_output_of(bus) {
                v["plays_on"] = serde_json::json!(format!("its own sound card, {} (set_bus_output)", sink.as_str()));
            }
            sources.push(v);
        }
        serde_json::json!({
            "output_channels": ch,
            "sources": sources,
            "max_assignments": crate::engine::MAX_OUT_ROUTES,
            "note": "An assignment past output_channels stays saved and plays where it would without one until an interface with those channels is the output.",
        })
    }

    /// The `set_output_route` tool: the answer the agent reads.
    pub(super) fn out_route_tool(&mut self, args: &serde_json::Value) -> Result<String, String> {
        let source = crate::out_routes::parse_source(args)?;
        let to = crate::out_routes::parse_target(args)?;
        let who = self.out_source_name(source);
        self.set_out_route(source, to)?;
        Ok(match to {
            None => format!("{who} has no outputs of its own any more: it plays on {}", default_place(source)),
            Some(t) if t.fits(self.stage_out.channels) => {
                let leaves = if source == OutSource::Master { "" } else { " and has left the master mix" };
                format!("{who} plays on {} at {:.1} dB{leaves} (saved with the session)", place(&t), level_db(t.level))
            }
            Some(t) => format!(
                "{who} is assigned to {} (saved with the session), but the output that is there now has {} channels: until an interface with those outputs is connected it plays on {}",
                place(&t), self.stage_out.channels, default_place(source)),
        })
    }

    /// Short text of a menu's button.
    fn out_route_text(&self, source: OutSource, to: Option<OutChannels>) -> String {
        match to {
            Some(t) => format!("→ Out {}", t.label()),
            None if source == OutSource::Master => "→ Out 1–2".into(),
            None => "→ Master mix".into(),
        }
    }

    /// The output menu of one source: where it plays by default, or channels
    /// of the interface. An assignment the interface lacks stays chosen and
    /// says so.
    pub(super) fn out_route_menu_ui(&mut self, ui: &mut egui::Ui, source: OutSource, width: f32) {
        let ch = self.stage_out.channels;
        let now = self.out_route_of(source);
        let missing = now.is_some_and(|t| !t.fits(ch));
        let options: Vec<OutChannels> = menu_options(ch, now).into_iter()
            // Outputs 1-2 are the master's: only the master is offered them.
            .filter(|o| source == OutSource::Master || o.first >= 2 || now.is_some_and(|n| n.first == o.first && n.mono == o.mono))
            .collect();
        let key = match source {
            OutSource::Master => (0u8, 0u32),
            OutSource::Click => (1, 0),
            OutSource::Player => (2, 0),
            OutSource::Bus(SendSource::Rig(r)) => (3, r as u32),
            OutSource::Bus(SendSource::Node(n)) => (4, n),
        };
        let default_item = if source == OutSource::Master { "Outputs 1–2".to_string() } else { "In the master mix".to_string() };
        let mut pick: Option<Option<OutChannels>> = None;
        let resp = theme::Dropdown::new(("dd_out_route", key), self.out_route_text(source, now))
            .width(width)
            .popup_min_width(200.0)
            .text_size(10.5)
            .show_ui(ui, |menu| {
                if menu.item(now.is_none(), &default_item) { pick = Some(None); }
                for o in &options {
                    let label = if o.mono { format!("Output {} (mono)", o.label()) } else { format!("Outputs {}", o.label()) };
                    let selected = now.is_some_and(|n| n.first == o.first && n.mono == o.mono);
                    if menu.item(selected, &label) {
                        // A new place keeps the level and pan set before.
                        pick = Some(Some(OutChannels { first: o.first, mono: o.mono, ..now.unwrap_or(OutChannels::MAIN) }));
                    }
                }
                if ch <= 2 {
                    menu.note("this output has 2 channels: connect and choose the stage interface to use more");
                }
                if let (Some(t), true) = (now, missing) {
                    menu.note(&format!("{} are not there now: plays on {} until they return", place(&t), default_place(source)));
                }
            });
        self.stage_out.menu_rects.push((source, resp.rect));
        if missing {
            ui.painter().rect_stroke(resp.rect, 2.0, Stroke::new(1.5, theme::CABLE_AMBER), egui::StrokeKind::Inside);
        }
        resp.on_hover_text(match now {
            None => format!("{} plays on {}; choose outputs of its own on the audio interface", self.out_source_name(source), default_place(source)),
            Some(t) if missing => format!("assigned to {}, which this output lacks: plays on {} until the stage interface is connected", place(&t), default_place(source)),
            Some(t) if source == OutSource::Master => format!("the master plays on {}", place(&t)),
            Some(t) => format!("plays on {} only, past the master", place(&t)),
        });
        if let Some(p) = pick {
            if self.rack_locked {
                self.notify_rack_locked();
            } else if let Err(e) = self.set_out_route(source, p) {
                self.session_msg = e;
            }
        }
    }

    /// Whether the Mixer shows the matrix: an interface with more than two
    /// outputs is there, or the session assigns something.
    pub(super) fn stage_outputs_shown(&self) -> bool {
        self.stage_out.channels > 2 || !self.stage_out.routes.is_empty()
    }

    /// The matrix rows under Master out: master, click and backing track,
    /// each with its outputs and, once assigned, its level and balance. Bus
    /// strips carry their own menu.
    pub(super) fn stage_outputs_ui(&mut self, ui: &mut egui::Ui) {
        if !self.stage_outputs_shown() {
            return;
        }
        ui.add_space(4.0);
        theme::tracked_label(ui, &format!("STAGE OUTPUTS · {}", self.stage_out.channels), 9.0, 0.14, theme::CREAM_DIM);
        for (source, label) in [(OutSource::Master, "Master"), (OutSource::Click, "Click"), (OutSource::Player, "Track")] {
            ui.horizontal(|ui| {
                ui.set_min_height(22.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add_sized([40.0, 18.0], egui::Label::new(egui::RichText::new(label).size(10.5).color(theme::INK_2)).truncate());
                self.out_route_menu_ui(ui, source, 100.0);
            });
            let Some(mut t) = self.out_route_of(source) else { continue };
            ui.horizontal(|ui| {
                ui.set_min_height(20.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add_space(44.0);
                let mut db = level_db(t.level).max(-60.0);
                let r = drag_only(ui.add(egui::DragValue::new(&mut db).range(-60.0..=6.0).speed(0.25).fixed_decimals(1).suffix(" dB")))
                    .on_hover_text("level on these outputs (drag; −60 dB = off)");
                let mut changed = false;
                if r.changed() {
                    t.level = if db <= -60.0 { 0.0 } else { db_level(db) };
                    changed = true;
                }
                if !t.mono {
                    let mut pan = t.pan;
                    let r = drag_only(ui.add(egui::DragValue::new(&mut pan).range(-1.0..=1.0).speed(0.02).fixed_decimals(2).prefix("bal ")))
                        .on_hover_text("balance between the two outputs (drag)");
                    if r.changed() {
                        t.pan = pan;
                        changed = true;
                    }
                }
                if changed {
                    if self.rack_locked {
                        self.notify_rack_locked();
                    } else if let Err(e) = self.set_out_route(source, Some(t)) {
                        self.session_msg = e;
                    }
                }
            });
        }
    }

    /// Keep the output stream as wide as the sink it plays to (#399): a
    /// stereo stream that was moved to an eight-output interface is reopened
    /// with eight channels, and back. Reads the scan's snapshot only (rule
    /// 48); a sink is tried once.
    pub(super) fn follow_output_channels(&mut self) {
        if self.active_audio_backend != Some(crate::audio_backend::ActiveBackend::PipeWire) {
            return;
        }
        let Some((sink, sink_ch)) = self.audio_out.lock().ok()
            .and_then(|s| Some((s.current.clone()?, s.current_channels?)))
        else { return };
        let want = crate::audio_out::stream_channels(Some(sink_ch));
        if want == self.stage_out.channels {
            self.stage_out.reopened_for = None;
            return;
        }
        if self.stage_out.reopened_for.as_ref() == Some(&(sink.clone(), want)) {
            return;
        }
        self.stage_out.reopened_for = Some((sink.clone(), want));
        let Some(host) = &mut self.audio_host else { return };
        let frames = host.buffer_frames;
        match host.set_buffer_frames(frames) {
            Ok(_) => {
                eprintln!("audio out: reopened the stream for the {sink_ch} channels of {sink}");
                if let Some(tx) = &self.stream_cmds {
                    let _ = tx.send(crate::audio_out::StreamCmd::Resync);
                }
            }
            Err(e) => self.audio_backend_err = Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        app
    }

    fn tool(app: &mut DemoApp, name: &str, args: serde_json::Value) -> String {
        app.exec_tool(name, &args)
    }

    #[test]
    fn the_output_route_tools_set_list_and_refuse() {
        let mut app = app();
        app.stage_out.channels = 4;
        let out = tool(&mut app, "set_output_route", serde_json::json!({ "source": "click", "output": 3 }));
        assert!(out.contains("Click plays on outputs 3–4") && out.contains("left the master mix"), "{out}");
        let out = tool(&mut app, "set_output_route", serde_json::json!({ "rig": 1, "output": 3, "mono": true, "level_db": -6.0 }));
        assert!(out.contains("plays on output 3 at -6.0 dB"), "{out}");
        let listed: serde_json::Value = serde_json::from_str(&tool(&mut app, "get_output_routes", serde_json::json!({}))).unwrap();
        assert_eq!(listed["output_channels"], 4);
        let src = |name: &str| listed["sources"].as_array().unwrap().iter().find(|s| s["source"] == name).unwrap().clone();
        assert_eq!(src("click")["output"], 3);
        assert_eq!(src("click")["plays_on"], "outputs 3–4");
        assert!(src("master")["output"].is_null());
        assert_eq!(listed["sources"].as_array().unwrap().iter().filter(|s| s["source"] == "rig").count(), 2);
        // Refusals name what is wrong, and nothing changes.
        for (args, needle) in [
            (serde_json::json!({ "source": "wedge", "output": 3 }), "wedge"),
            (serde_json::json!({ "rig": 9, "output": 3 }), "no rig 9"),
            (serde_json::json!({ "bus": 9999, "output": 3 }), "not an aux bus"),
            (serde_json::json!({ "source": "click", "output": 0 }), "out of range"),
        ] {
            let out = tool(&mut app, "set_output_route", args);
            assert!(out.contains("NOT applied") && out.contains(needle), "{out}");
        }
        assert_eq!(app.stage_out.routes.len(), 2);
        // On a 2-channel output the assignment stays and says where it plays.
        app.stage_out.channels = 2;
        let out = tool(&mut app, "set_output_route", serde_json::json!({ "source": "player", "output": 5 }));
        assert!(out.contains("has 2 channels") && out.contains("the master mix"), "{out}");
        let listed: serde_json::Value = serde_json::from_str(&tool(&mut app, "get_output_routes", serde_json::json!({}))).unwrap();
        let click = listed["sources"].as_array().unwrap().iter().find(|s| s["source"] == "click").unwrap().clone();
        assert_eq!(click["available"], false);
        // null takes it back.
        let out = tool(&mut app, "set_output_route", serde_json::json!({ "source": "click", "output": null }));
        assert!(out.contains("plays on the master mix"), "{out}");
        assert_eq!(app.out_route_of(OutSource::Click), None);
        for t in ["set_output_route", "get_output_routes"] {
            assert!(crate::assistant::tools().as_array().unwrap().iter().any(|x| x["function"]["name"] == t), "{t} is an assistant tool (rule 6)");
            assert!(UI_BRIDGE_TOOLS.contains(&t), "{t} is served to attached agents (rule 66)");
        }
        assert!(tool_is_read_only("get_output_routes") && !tool_is_read_only("set_output_route"));
    }

    #[test]
    fn a_bus_plays_on_channels_or_on_a_sound_card_never_both() {
        let mut app = app();
        app.stage_out.channels = 4;
        app.bus_outs.push(BusOut { bus: SendSource::Rig(1), sink: SinkName::new("phones") });
        app.set_out_route(OutSource::Bus(SendSource::Rig(1)), Some(OutChannels::pair(2))).unwrap();
        assert_eq!(app.bus_output_of(SendSource::Rig(1)), None, "channels of the interface take the bus off its sound card");
        app.set_bus_output(SendSource::Rig(1), Some("phones")).unwrap();
        assert_eq!(app.out_route_of(OutSource::Bus(SendSource::Rig(1))), None, "and a sound card takes it off the channels");
    }

    #[test]
    fn output_routes_survive_a_session_round_trip_and_a_rig_delete() {
        let mut app = app();
        app.stage_out.channels = 4;
        app.set_out_route(OutSource::Click, Some(OutChannels { level: 0.5, ..OutChannels::pair(2) })).unwrap();
        app.set_out_route(OutSource::Master, Some(OutChannels { pan: -0.25, ..OutChannels::MAIN })).unwrap();
        app.set_out_route(OutSource::Bus(SendSource::Rig(1)), Some(OutChannels { mono: true, ..OutChannels::pair(3) })).unwrap();
        let saved = app.capture_session("Stage").unwrap();
        assert_eq!(saved.out_routes().len(), 3);
        let json = serde_json::to_string(&saved.out_routes()).unwrap();
        assert!(json.contains("\"click\"") && json.contains("\"first\":2"), "{json}");
        // Another patch assigns nothing; the saved one brings all three back.
        let before = app.stage_out.routes.clone();
        app.apply_session(&crate::session::Session::new("Bare", crate::control::standard_rig_graph(true))).unwrap();
        assert!(app.stage_out.routes.is_empty(), "another patch routes nothing: {:?}", app.stage_out.routes);
        app.apply_session(&saved).unwrap();
        assert_eq!(app.stage_out.routes, before);
        // A 2-channel output keeps what the session assigns.
        app.stage_out.channels = 2;
        assert_eq!(app.capture_session("Laptop").unwrap().out_routes().len(), 3, "rehearsing on the laptop loses no assignment");
    }

    /// The Mixer with the click on outputs 3–4 of a four-output interface,
    /// and the same session on a 2-channel output:
    /// `KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat stage_outputs_shots -- --ignored`
    #[test]
    #[ignore = "writes PNGs"]
    fn stage_outputs_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        for (w, h, channels, name) in [
            (1600.0, 1000.0, 4, "stage-outputs.png"),
            (781.0, 950.0, 4, "stage-outputs-781x950.png"),
            (1600.0, 1000.0, 2, "stage-outputs-2ch.png"),
        ] {
            // A fresh app per image (rule 148).
            let mut app = app();
            app.stage_out.channels = channels;
            app.set_out_route(OutSource::Click, Some(OutChannels { level: 0.7, ..OutChannels::pair(2) })).unwrap();
            app.set_out_route(OutSource::Bus(SendSource::Rig(1)), Some(OutChannels { mono: true, ..OutChannels::pair(2) })).unwrap();
            app.stage_view = StageView::Mixer;
            super::super::offline_shots::shot(&mut app, Vec2::new(w, h), &dir.join(name));
        }
    }
}
