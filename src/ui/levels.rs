//! Show, set and song levels (#273) in the app: the level dropdown on the
//! mixer's bus strips, the Setlist view's "Sets & levels" panel, and the
//! agent tools that read and set them. The model is `crate::setlist`.

use super::*;
use crate::control::NodeId;
use crate::setlist::{Binding, Level, SetId};

/// One thing that has a level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum LevelItem {
    Rig(u8),
    Bus(NodeId),
    Binding(Binding, CcTarget),
}

/// What the Sets & levels panel asks for, applied after it is drawn.
enum SetEdit {
    Add,
    Rename(SetId, String),
    Skip(SetId, bool),
    Move(SetId, bool),
    Remove(SetId),
    Member(SetId, usize, bool),
}

impl DemoApp {
    /// Every controller binding of the matrix, with its target.
    pub(super) fn show_bindings(&self) -> Vec<(Binding, CcTarget)> {
        let mut out: Vec<(Binding, CcTarget)> = self
            .rule_edits
            .iter()
            .flat_map(|(&(source, dest), e)| e.cc_map.iter().map(move |m| (Binding { source, dest, cc: m.cc }, m.target)))
            .collect();
        out.sort_by_key(|(b, _)| (b.dest, b.cc, crate::setlist::source_str(b.source)));
        out
    }

    /// Put the active part's zones, closed buses and pinned bindings into the
    /// engine (everything open when no part is active).
    pub(super) fn send_show_gates(&mut self) {
        self.ui_zone_grace = self.ui_time + 0.6;
        self.send_gates(true);
    }

    /// The gates of a part the engine stepped to on a foot switch (#261):
    /// its closed buses and pinned bindings, one UI frame after the step.
    /// The engine switched the zones itself, and sending them again could
    /// undo a second step it took meanwhile.
    pub(super) fn send_show_gates_after_engine_step(&mut self) {
        self.send_gates(false);
    }

    fn send_gates(&mut self, zones: bool) {
        let gates = self.setlist.gates(&self.mirror_graph());
        self.rig_zones = gates.zones;
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap_or_else(|e| e.into_inner());
            for c in gates.commands() {
                if zones || !matches!(c, Command::SetRigZones { .. }) {
                    let _ = b.send(c);
                }
            }
        }
    }

    /// The gates the live part has now; `None` while no part is active.
    pub(super) fn show_gates_now(&self) -> Option<crate::setlist::Gates> {
        self.setlist.active.is_some().then(|| self.setlist.gates(&self.mirror_graph()))
    }

    /// The rig a new song or part starts with (`Setlist::default_part_rig`),
    /// at its current fader level.
    pub(super) fn default_part_rigs(&self) -> Vec<crate::setlist::PartRig> {
        let n = self.rig_count();
        let preferred = self.active_rig.min(n.saturating_sub(1)) as u8;
        self.setlist
            .default_part_rig(preferred, n)
            .map(|rig| crate::setlist::PartRig { rig, level: self.rig_mix[rig as usize].vol, ..Default::default() })
            .into_iter()
            .collect()
    }

    pub(super) fn level_of(&self, item: LevelItem) -> Level {
        match item {
            LevelItem::Rig(r) => self.setlist.rig_level(r),
            LevelItem::Bus(n) => self.setlist.bus_level(n),
            LevelItem::Binding(b, t) => self.setlist.binding_level(b, t, &self.mirror_graph()),
        }
    }

    /// Pin `item` to `level`; a live part takes the change at once.
    pub(super) fn set_level(&mut self, item: LevelItem, level: Level) {
        match item {
            LevelItem::Rig(r) => self.setlist.pin_rig(r, level),
            LevelItem::Bus(n) => self.setlist.pin_bus(n, level),
            LevelItem::Binding(b, _) => self.setlist.pin_binding(b, level),
        }
        if self.setlist.active.is_some() {
            self.send_show_gates();
        }
        self.sync_setlist_window();
    }

    /// A dropdown that shows and sets one item's level: Show, Song or a set.
    pub(super) fn level_dropdown_ui(&mut self, ui: &mut egui::Ui, item: LevelItem, width: f32) -> egui::Response {
        let now = self.level_of(item);
        let label = match now {
            Level::Show => "Show level".to_string(),
            Level::Song => "Song level".to_string(),
            Level::Set(_) => format!("Set · {}", self.setlist.level_label(now)),
        };
        let key = match item {
            LevelItem::Rig(r) => (0u8, r as u32, 0u8, String::new()),
            LevelItem::Bus(n) => (1, n, 0, String::new()),
            LevelItem::Binding(b, _) => (2, b.dest, b.cc, crate::setlist::source_str(b.source)),
        };
        let sets: Vec<(SetId, String)> = self.setlist.sets.iter().map(|s| (s.id, s.name.clone())).collect();
        let mut pick = None;
        let resp = theme::Dropdown::new(("dd_level", key), &label)
            .width(width)
            .popup_min_width(200.0)
            .text_size(10.5)
            .show_ui(ui, |menu| {
                if menu.item(now == Level::Show, "Show: every song, never switched") {
                    pick = Some(Level::Show);
                }
                if menu.item(now == Level::Song, "Song: where the parts use it") {
                    pick = Some(Level::Song);
                }
                for (id, name) in &sets {
                    if menu.item(now == Level::Set(*id), &format!("Set · {name}")) {
                        pick = Some(Level::Set(*id));
                    }
                }
                if sets.is_empty() {
                    menu.note("Sets group songs: make one under Setlist → Sets & levels");
                }
            });
        if let Some(level) = pick {
            self.set_level(item, level);
        }
        resp.on_hover_text(match now {
            Level::Show => "no song switch touches it",
            Level::Song => "plays only where the setlist's parts use it",
            Level::Set(_) => "plays through the songs of its set, untouched by their switches",
        })
    }

    /// The Setlist view's "Sets & levels" panel: sets (name, songs, skip,
    /// order) and the level of every rig, bus and binding.
    pub(super) fn setlist_levels_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let mut edits: Vec<SetEdit> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("setlist_levels_scroll")
            .min_scrolled_height(height.min(120.0))
            .max_height(height)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::section_label(ui, "Sets");
                    ui.add_space(8.0);
                    if theme::chip_button(ui, "+ Set", false).on_hover_text("a named group of songs, moved or skipped as a whole").clicked() {
                        edits.push(SetEdit::Add);
                    }
                });
                ui.add_space(6.0);
                if self.setlist.sets.is_empty() {
                    ui.label(egui::RichText::new("No sets: every song stands on its own.").size(12.0).color(theme::INK_2));
                }
                for i in 0..self.setlist.sets.len() {
                    let (id, mut name, skip) = {
                        let s = &self.setlist.sets[i];
                        (s.id, s.name.clone(), s.skip)
                    };
                    let members: Vec<usize> = (0..self.setlist.songs.len()).filter(|&j| self.setlist.songs[j].set == Some(id)).collect();
                    let can_up = members.first().is_some_and(|&f| f > 0);
                    let can_down = members.last().is_some_and(|&l| l + 1 < self.setlist.songs.len());
                    egui::Frame::new()
                        .fill(theme::DARK_PANEL_HI)
                        .stroke(Stroke::new(1.0, theme::HAIRLINE))
                        .corner_radius(theme::RADIUS_CARD)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let w = (ui.available_width() - 150.0).clamp(80.0, 200.0);
                                if Self::recessed_input(ui, &mut name, w, "Set name").changed() {
                                    edits.push(SetEdit::Rename(id, name.clone()));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    if theme::ghost_icon_chip(ui, "×", false).on_hover_text("remove the set (its songs stay)").clicked() {
                                        edits.push(SetEdit::Remove(id));
                                    }
                                    if can_down && theme::ghost_icon_chip(ui, "↓", false).on_hover_text("move the set down").clicked() {
                                        edits.push(SetEdit::Move(id, false));
                                    }
                                    if can_up && theme::ghost_icon_chip(ui, "↑", false).on_hover_text("move the set up").clicked() {
                                        edits.push(SetEdit::Move(id, true));
                                    }
                                    if theme::live_toggle_chip(ui, "Skip", skip).on_hover_text("stepping jumps over this set's songs").clicked() {
                                        edits.push(SetEdit::Skip(id, !skip));
                                    }
                                });
                            });
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                for (si, song) in self.setlist.songs.iter().enumerate() {
                                    let on = song.set == Some(id);
                                    if song.set.is_some() && !on {
                                        continue;
                                    }
                                    let label = format!("{}. {}", si + 1, song.name);
                                    if theme::wrapped_live_toggle_chip(ui, &label, on).on_hover_text(if on { "take out of the set" } else { "put into the set" }).clicked() {
                                        edits.push(SetEdit::Member(id, si, !on));
                                    }
                                }
                            });
                        });
                    ui.add_space(4.0);
                }

                ui.add_space(10.0);
                Self::stage_hairline(ui);
                ui.add_space(8.0);
                theme::section_label(ui, "Levels");
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Show: no song switch touches it · Set: through one set's songs · Song: where the parts use it")
                        .size(11.5)
                        .color(theme::INK_2),
                );
                ui.add_space(6.0);
                let row = |app: &mut Self, ui: &mut egui::Ui, label: String, item: LevelItem| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(24.0);
                        let w = (ui.available_width() - 170.0).max(60.0);
                        ui.allocate_ui_with_layout(egui::vec2(w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(w);
                            ui.add(egui::Label::new(egui::RichText::new(label.as_str()).size(12.0).color(theme::INK)).truncate())
                                .on_hover_text(label.as_str());
                        });
                        app.level_dropdown_ui(ui, item, 160.0);
                    });
                };
                for r in 0..self.rig_count().min(crate::setlist::MAX_RIGS) {
                    let label = format!("Rig {} · {}", r + 1, self.rig_name(r));
                    row(self, ui, label, LevelItem::Rig(r as u8));
                }
                let buses: Vec<(NodeId, String)> = self.bus_units().into_iter().map(|u| (self.units[u].node_id, sentence_case(&self.units[u].name))).collect();
                for (node, name) in buses {
                    row(self, ui, format!("Bus · {name}"), LevelItem::Bus(node));
                }
                for (b, target) in self.show_bindings() {
                    let dest = self.units.iter().find(|u| u.node_id == b.dest).map_or_else(|| format!("node {}", b.dest), |u| sentence_case(&u.name));
                    let src = match b.source {
                        crate::matrix::SourceId::Keys => "Keyboard".to_string(),
                        crate::matrix::SourceId::Hardware(d) => self.hw_device_label(d),
                        _ => crate::setlist::source_str(b.source),
                    };
                    let label = format!("CC {} · {src} → {dest}", b.cc);
                    row(self, ui, label, LevelItem::Binding(b, target));
                }
            });

        let mut window_changed = false;
        for e in edits {
            match e {
                SetEdit::Add => {
                    let id = self.setlist.add_set("");
                    if let Some(si) = self.setlist_edit_song {
                        self.setlist_edit_song = self.setlist.assign_set(si, Some(id));
                    }
                    window_changed = true;
                }
                SetEdit::Rename(id, name) => {
                    if let Some(s) = self.setlist.sets.iter_mut().find(|s| s.id == id) {
                        s.name = name;
                    }
                }
                SetEdit::Skip(id, skip) => {
                    if let Some(s) = self.setlist.sets.iter_mut().find(|s| s.id == id) {
                        s.skip = skip;
                    }
                    window_changed = true;
                }
                SetEdit::Move(id, up) => {
                    if let Some(order) = self.setlist.move_set(id, up) {
                        self.setlist_edit_song = self.setlist_edit_song.and_then(|old| order.iter().position(|&o| o == old));
                        window_changed = true;
                    }
                }
                SetEdit::Remove(id) => {
                    self.setlist.remove_set(id);
                    window_changed = true;
                }
                SetEdit::Member(id, song, on) => {
                    let new = self.setlist.assign_set(song, on.then_some(id));
                    if self.setlist_edit_song == Some(song) {
                        self.setlist_edit_song = new;
                    }
                    window_changed = true;
                }
            }
        }
        if window_changed {
            if self.setlist.active.is_some() {
                self.send_show_gates();
            }
            self.sync_setlist_window();
        }
    }

    /// `set_level` from the assistant or MCP.
    pub(super) fn exec_set_level(&mut self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        let bindings = self.show_bindings();
        let msg = self.setlist.apply_set_level(&self.mirror_graph(), &bindings, args)?;
        if self.setlist.active.is_some() {
            self.send_show_gates();
        }
        self.sync_setlist_window();
        let mut out = self.setlist_json();
        out["result"] = serde_json::json!(msg);
        Ok(out)
    }

    /// `set_setlist_set` from the assistant or MCP.
    pub(super) fn exec_setlist_set(&mut self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
        let msg = self.setlist.apply_setlist_set(args)?;
        if self.setlist.active.is_some() {
            self.send_show_gates();
        }
        self.sync_setlist_window();
        let mut out = self.setlist_json();
        out["result"] = serde_json::json!(msg);
        Ok(out)
    }

    /// Sets and levels for `get_setlist`.
    pub(super) fn show_json(&self) -> serde_json::Value {
        let graph = self.mirror_graph();
        let bus_name = |n: NodeId| self.units.iter().find(|u| u.node_id == n).map_or_else(String::new, |u| sentence_case(&u.name));
        self.setlist.show_json(&graph, &self.show_bindings(), bus_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::GraphEngine;

    const SURGE: &str = "org.surge-synth-team.surge-xt";

    /// A small gig: Vocals (a mic and a Hall bus), Piano and Organ.
    fn gig() -> DemoApp {
        let mut vocals = Graph::default();
        vocals.reserve(8, 8);
        let mic = vocals.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::MonoL }, name_bytes("VOCAL MIC"));
        let hall = vocals.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("HALL"));
        let mv = vocals.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        vocals.connect_with_id(1, PortRef { node: mic, port: 0 }, PortRef { node: mv, port: 0 }, CPortKind::Audio);
        vocals.connect_with_id(2, PortRef { node: hall, port: 0 }, PortRef { node: mv, port: 0 }, CPortKind::Audio);
        let synth = |name: &str| {
            let mut g = Graph::default();
            g.reserve(8, 8);
            let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let s = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE) }, name_bytes(name));
            let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, CPortKind::Midi);
            g.connect_with_id(2, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
            g
        };
        let rigs = vec![
            crate::control::Rig::new("Vocals", vocals, None),
            crate::control::Rig::new("Piano", synth("PIANO"), None),
            crate::control::Rig::new("Organ", synth("ORGAN"), None),
        ];
        let merged = crate::control::merge_rigs(&rigs);
        let mut app = DemoApp::new_with_sink(None);
        app.build_rack(&merged, &[]);
        app.session_rig_names = vec!["Vocals".into(), "Piano".into(), "Organ".into()];
        if let Ok(mut s) = app.rig_snapshot.lock() {
            s.rigs = ["Vocals", "Piano", "Organ"].map(|name| crate::control::RigMeta { name: name.into(), midi_device: None }).to_vec();
        }
        app
    }

    fn node(app: &DemoApp, name: &str) -> NodeId {
        app.units.iter().find(|u| u.name == name).unwrap_or_else(|| panic!("no unit {name}")).node_id
    }

    fn tool(app: &mut DemoApp, name: &str, args: serde_json::Value) -> serde_json::Value {
        let out = app.exec_tool(name, &args);
        serde_json::from_str(&out).unwrap_or_else(|_| panic!("{name}: {out}"))
    }

    /// Ten songs taking turns on Piano and Organ.
    fn ten_songs(app: &mut DemoApp) {
        for i in 0..10 {
            let rig = 1 + i % 2;
            tool(app, "set_setlist_song", serde_json::json!({
                "name": format!("Song {}", i + 1),
                "parts": [{"name": "Main", "rigs": [{"rig": rig, "level": 0.6}]}],
            }));
        }
    }

    #[test]
    fn the_level_tools_are_in_every_table() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for name in ["set_level", "set_setlist_set"] {
            assert!(names.contains(&name), "assistant tools miss {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS miss {name}");
            assert!(!tool_is_read_only(name));
        }
    }

    /// The app sends what the setlist says at every switch: the vocal rig at
    /// show level keeps the whole keyboard and its fader, the Hall at song
    /// level closes where no part names it.
    #[test]
    fn a_switch_leaves_show_level_rigs_alone_and_closes_song_level_buses() {
        let mut app = gig();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let hall = node(&app, "HALL");
        ten_songs(&mut app);
        // The first song lists the vocals too, at a low fader: as show level that entry plays no part.
        tool(&mut app, "set_setlist_song", serde_json::json!({
            "song": 0,
            "parts": [{"name": "Main", "rigs": [{"rig": 0, "level": 0.1}, {"rig": 1, "level": 0.6}], "buses": [hall]}],
        }));
        let v = tool(&mut app, "set_level", serde_json::json!({"rig": 0, "level": "show"}));
        assert_eq!(v["levels"]["rigs"][0]["level"], "show", "{v}");
        assert_eq!(v["levels"]["buses"][0]["level"], "song", "a bus a part names is at song level: {v}");
        drain.drain(|_| {});
        for song in 0..10 {
            tool(&mut app, "select_setlist_part", serde_json::json!({"song": song}));
            let mut cmds = Vec::new();
            drain.drain(|c| cmds.push(c));
            let zones = cmds.iter().find_map(|c| match c { Command::SetRigZones { zones } => Some(*zones), _ => None }).expect("zones");
            assert_eq!(zones[0], crate::control::RigZone::OPEN, "song {}: the vocal rig takes the whole keyboard", song + 1);
            assert!(!cmds.iter().any(|c| matches!(c, Command::SetRigMix { rig: 0, .. })), "song {}: its fader is not recalled", song + 1);
            let closed = cmds.iter().any(|c| matches!(c, Command::SetBusLive { bus, live: false } if *bus == hall));
            assert_eq!(closed, song != 0, "song {}: the Hall takes signal only where a part names it", song + 1);
        }
    }

    /// Done-when #273: a volume pedal at show level works in every song
    /// without being bound again, through the app and the engine. The
    /// organ's own pedal, unpinned, follows its rig.
    #[test]
    fn a_show_level_volume_pedal_works_in_every_song_through_the_engine() {
        let mut app = gig();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (piano, organ) = (node(&app, "PIANO"), node(&app, "ORGAN"));
        let (mut ebus, edrain) = crate::control::command_bus();
        let mut engine = GraphEngine::new(app.mirror_graph(), edrain, 48000.0, vec![]);
        let mut pump = |drain: &mut crate::control::CommandDrain, engine: &mut GraphEngine, ebus: &mut crate::control::CommandBus| {
            drain.drain(|c| { ebus.send(c); });
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
        };
        let organ_master = app.units.iter().find(|u| u.rig == 2 && matches!(u.node_kind, UnitKind::Master)).unwrap().node_id;
        ten_songs(&mut app);
        for (dest, cc, knob) in [(piano, 11, "LEVEL"), (organ, 4, "LEVEL"), (organ_master, 7, "VOLUME")] {
            let out = app.exec_tool("set_cc_map", &serde_json::json!({"node": dest, "source": "hw:0", "cc": cc, "knob": knob}));
            assert!(!out.starts_with("error"), "{out}");
        }
        let v = tool(&mut app, "set_level", serde_json::json!({"node": piano, "cc": 11, "source": "hw:0", "level": "show"}));
        let lvl = |cc: u64| v["levels"]["bindings"].as_array().unwrap().iter().find(|b| b["cc"] == cc).unwrap()["level"].clone();
        assert_eq!((lvl(11), lvl(4)), (serde_json::json!("show"), serde_json::json!("song")), "{v}");
        pump(&mut drain, &mut engine, &mut ebus);
        for song in 0..10 {
            tool(&mut app, "select_setlist_part", serde_json::json!({"song": song}));
            pump(&mut drain, &mut engine, &mut ebus);
            engine.cc_log.clear();
            for cc in [11, 4, 7] {
                ebus.send(Command::Cc { source: SourceId::Hardware(0), channel: 0, cc, value: 90 });
            }
            pump(&mut drain, &mut engine, &mut ebus);
            let moved = |n: NodeId| engine.cc_log.iter().any(|e| e.0 == n);
            assert!(moved(piano), "song {}: the show-level pedal reaches the piano", song + 1);
            assert!(moved(organ_master), "song {}: a master volume pedal is no rig's, whichever master it was learned on", song + 1);
            assert_eq!(moved(organ), song % 2 == 1, "song {}: the organ's pedal works only while the organ plays", song + 1);
        }
    }

    /// The knobs on screen follow a pedal exactly where the engine does.
    #[test]
    fn a_pedal_moves_the_knob_on_screen_only_where_the_engine_applies_it() {
        let mut app = gig();
        let organ = node(&app, "ORGAN");
        ten_songs(&mut app);
        app.exec_tool("set_cc_map", &serde_json::json!({"node": organ, "source": "hw:0", "cc": 4, "knob": "LEVEL"}));
        let level = |app: &DemoApp| {
            let u = app.units.iter().find(|u| u.node_id == organ).unwrap();
            u.knobs[u.knob_index("LEVEL").unwrap()].display_value
        };
        tool(&mut app, "select_setlist_part", serde_json::json!({"song": 0}));
        let before = level(&app);
        app.apply_cc_to_knobs(SourceId::Hardware(0), 0, 4, 20, false);
        assert_eq!(level(&app), before, "song 1 has no organ: the engine drops the pedal, the knob stays");
        tool(&mut app, "select_setlist_part", serde_json::json!({"song": 1}));
        app.apply_cc_to_knobs(SourceId::Hardware(0), 0, 4, 20, false);
        assert!((level(&app) - 20.0 / 127.0).abs() < 1e-3, "song 2 plays the organ: the knob follows");
    }

    /// A new song never names a rig that is at show level by default, and an
    /// edit to a song that is not playing reaches the live part at once.
    #[test]
    fn adding_and_editing_songs_keeps_the_show_level_vocals_and_resends_the_gates() {
        let mut app = gig();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        ten_songs(&mut app);
        app.active_rig = 0;
        let v = tool(&mut app, "set_setlist_song", serde_json::json!({"name": "Encore"}));
        assert_eq!(v["songs"][10]["parts"][0]["rigs"][0]["rig"], 1, "the vocals in view stay out of it: {v}");
        assert_eq!(v["levels"]["rigs"][0]["level"], "show");
        tool(&mut app, "select_setlist_part", serde_json::json!({"song": 2}));
        drain.drain(|_| {});
        tool(&mut app, "set_setlist_song", serde_json::json!({"song": 5, "parts": [{"name": "Main", "rigs": [{"rig": 0}]}]}));
        let mut zones = None;
        drain.drain(|c| if let Command::SetRigZones { zones: z } = c { zones = Some(z) });
        assert_eq!(zones.map(|z| z[0].live), Some(false), "song 6 now uses the vocals: song 3, playing, drops them at once");
    }

    #[test]
    fn sets_and_levels_survive_a_session_round_trip() {
        let mut app = gig();
        let hall = node(&app, "HALL");
        ten_songs(&mut app);
        let piano = node(&app, "PIANO");
        app.exec_tool("set_cc_map", &serde_json::json!({"node": piano, "source": "hw:0", "cc": 11, "knob": "LEVEL"}));
        tool(&mut app, "set_setlist_set", serde_json::json!({"name": "Encore", "songs": "8,9", "skip": true}));
        tool(&mut app, "set_level", serde_json::json!({"rig": 0, "level": "show"}));
        tool(&mut app, "set_level", serde_json::json!({"bus": hall, "level": "set", "set": "Encore"}));
        tool(&mut app, "set_level", serde_json::json!({"node": piano, "cc": 11, "source": "hw:0", "level": "show"}));
        let before = app.setlist.clone();
        let s = app.capture_session("Levels").unwrap();
        let mut back = gig();
        back.apply_session(&s).unwrap();
        assert_eq!(back.setlist, before);
        let v: serde_json::Value = serde_json::from_str(&back.exec_tool("get_setlist", &serde_json::json!({}))).unwrap();
        assert_eq!(v["sets"][0]["name"], "Encore");
        assert_eq!(v["sets"][0]["skip"], true);
        assert_eq!(v["songs"][9]["set"], "Encore");
        let bus = &v["levels"]["buses"][0];
        assert_eq!((bus["level"].as_str(), bus["set"].as_str()), (Some("set"), Some("Encore")), "{v}");
        assert_eq!(v["levels"]["bindings"][0]["level"], "show");
        // A binding that is cleared takes its pin with it at the next save.
        back.exec_tool("clear_cc_map", &serde_json::json!({"node": piano, "source": "hw:0", "cc": 11}));
        assert!(back.capture_session("Levels").unwrap().setlist.levels.bindings.is_empty());
    }

    fn frame_texts(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) -> Vec<String> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        fn walk(shape: &Shape, texts: &mut Vec<String>) {
            match shape {
                Shape::Text(t) => texts.push(t.galley.text().to_owned()),
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

    #[test]
    fn the_mixer_and_the_setlist_show_each_level() {
        let size = Vec2::new(1600.0, 1000.0);
        let mut app = gig();
        let ctx = egui::Context::default();
        app.stage_view = StageView::Mixer;
        let texts = frame_texts(&mut app, &ctx, size);
        assert!(!texts.iter().any(|t| t.contains("Show level")), "no setlist: no level row");
        ten_songs(&mut app);
        tool(&mut app, "set_setlist_set", serde_json::json!({"name": "Encore", "songs": "9"}));
        let texts = frame_texts(&mut app, &ctx, size);
        assert!(texts.iter().filter(|t| t.as_str() == "Show level").count() >= 2, "the vocal rig bus and the Hall: {texts:?}");
        assert!(texts.iter().any(|t| t == "Song level"), "piano and organ, which the parts use");
        app.stage_view = StageView::Setlist;
        app.setlist_levels_open = true;
        let texts = frame_texts(&mut app, &ctx, size);
        let vocals = format!("Rig 1 · {}", app.rig_name(0));
        for want in ["+ Set", "Encore", vocals.as_str(), "Bus · Hall"] {
            assert!(texts.iter().any(|t| t == want), "the Sets & levels panel shows {want}: {texts:?}");
        }
        app.setlist_levels_open = false;
        app.setlist_edit_song = Some(9);
        let texts = frame_texts(&mut app, &ctx, size);
        assert!(texts.iter().any(|t| t.contains("Encore")), "a song's set is in its row");
        assert!(texts.iter().any(|t| t == "Show level: plays in every part"), "the part editor marks the vocal rig: {texts:?}");
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat levels_shots -- --ignored"]
    fn levels_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let setup = || {
            let mut app = gig();
            let hall = node(&app, "HALL");
            ten_songs(&mut app);
            tool(&mut app, "set_setlist_set", serde_json::json!({"name": "First set", "songs": "0,1,2,3,4"}));
            tool(&mut app, "set_setlist_set", serde_json::json!({"name": "Encore", "songs": "8,9", "skip": true}));
            tool(&mut app, "set_level", serde_json::json!({"bus": hall, "level": "show"}));
            let piano = node(&app, "PIANO");
            app.exec_tool("set_cc_map", &serde_json::json!({"node": piano, "source": "hw:0", "cc": 11, "knob": "LEVEL"}));
            tool(&mut app, "set_level", serde_json::json!({"node": piano, "cc": 11, "source": "hw:0", "level": "show"}));
            tool(&mut app, "select_setlist_part", serde_json::json!({"song": 1}));
            app
        };
        // Rule 148: a fresh app per image.
        for size in [Vec2::new(1280.0, 800.0), Vec2::new(781.0, 950.0)] {
            let tag = format!("{}x{}", size.x, size.y);
            let mut app = setup();
            app.stage_view = StageView::Setlist;
            app.setlist_levels_open = true;
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("setlist-levels-{tag}.png")));
            let mut app = setup();
            app.stage_view = StageView::Setlist;
            app.setlist_edit_song = Some(1);
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("setlist-song-{tag}.png")));
            let mut app = setup();
            app.stage_view = StageView::Mixer;
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("mixer-levels-{tag}.png")));
        }
    }
}
