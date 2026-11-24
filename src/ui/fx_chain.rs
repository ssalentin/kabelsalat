//! Wired FX-chain presets (#429).
//!
//! A chain is the linear run of effect units of one mixer strip: its units,
//! their knob values and plugin states, and the cables between them. It is
//! saved as a rig preset file (`RigPreset`, the one preset format and the one
//! `remap_session` carry the node-id keyed data) whose graph holds only the
//! effects, in its own folder under `.ksalat-fxchain`. Inserting one adds its
//! units to a strip and rewires the strip's chain the way a single insert
//! does (`rewire_chain_edges`), so the chain's open ends end up in the rig's
//! signal path.

use super::*;
use crate::session::RigPreset;

/// Longest saved chain name the dialog takes.
const NAME_MAX: usize = 48;

impl DemoApp {
    #[cfg(test)]
    pub(super) fn default_fx_chains_dir() -> std::path::PathBuf {
        // Tests never write to the user's folders (rules 37, 169).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("test-fx-chains").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[cfg(not(test))]
    pub(super) fn default_fx_chains_dir() -> std::path::PathBuf {
        crate::session::fx_chains_dir()
    }

    /// The strip of `rig` whose insert chain is the default target of a save
    /// (the first with effects) or an insert (the first with a linear chain).
    pub(super) fn fx_chain_source(&self, rig: u8, with_effects: bool) -> Option<u32> {
        self.rig_strip_sources(rig).into_iter()
            .find(|&u| self.strip_chain(u).is_some_and(|c| !with_effects || !c.is_empty()))
            .map(|u| self.units[u].node_id)
    }

    /// Snapshot the insert chain of the strip with node `source` as a preset:
    /// rig-relative ids 0.., units in signal order, one audio cable per hop.
    pub(super) fn capture_fx_chain(&mut self, source: u32, name: &str) -> anyhow::Result<RigPreset> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("The FX chain needs a name before it can be saved.");
        }
        let unit = self.units.iter().position(|u| u.node_id == source)
            .ok_or_else(|| anyhow::anyhow!("no strip with node {source} (see get_graph)"))?;
        let chain = self.strip_chain(unit)
            .ok_or_else(|| anyhow::anyhow!("this strip's wiring is custom — patch it on the rack back, a chain is a linear run of effects"))?;
        if chain.is_empty() {
            anyhow::bail!("this strip has no effects to save as a chain");
        }
        let old: Vec<u32> = chain.iter().map(|&u| self.units[u].node_id).collect();
        let s = self.capture_session(name)?;
        let mut g = Graph::default();
        let mut map = std::collections::HashMap::new();
        for &o in &old {
            let n = s.graph.node(o).ok_or_else(|| anyhow::anyhow!("node {o} is not in the rack"))?;
            let id = g.add_node(n.kind.clone(), n.name);
            map.insert(o, id);
        }
        for pair in old.windows(2) {
            let (from, to) = (map[&pair[0]], map[&pair[1]]);
            let id = g.edges.len() as u32 + 1;
            g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port: 0 }, CPortKind::Audio);
        }
        let mut chain_session = remap_session(&s, g, |o| map.get(&o).copied());
        chain_session.rig_names.clear();
        chain_session.rig_devices.clear();
        chain_session.rig_types.clear();
        let preset = RigPreset::new(name, "", chain_session);
        preset.validate()?;
        preset.fx_chain_order()?;
        Ok(preset)
    }

    pub(super) fn save_fx_chain_to_dir(&mut self, dir: &std::path::Path, source: u32, name: &str) -> anyhow::Result<std::path::PathBuf> {
        let preset = self.capture_fx_chain(source, name)?;
        let path = crate::session::fx_chain_path(dir, name)?;
        preset.save(&path)?;
        Ok(path)
    }

    pub(super) fn load_fx_chain_from_dir(dir: &std::path::Path, name: &str) -> anyhow::Result<RigPreset> {
        let path = crate::session::fx_chain_path(dir, name)?;
        if !path.exists() {
            anyhow::bail!("no FX chain '{name}' (see list_fx_chains)");
        }
        let preset = RigPreset::load(&path)?;
        preset.fx_chain_order()?;
        Ok(preset)
    }

    /// Insert `preset` into `rig`: at the end of the insert chain of strip
    /// `source` (default: the rig's first linear strip), where one effect
    /// would go. A rig with no linear strip gets the units and their own
    /// cables, unconnected to the rest, to patch on the back.
    pub(super) fn insert_fx_chain(&mut self, preset: &RigPreset, rig: u8, source: Option<u32>) -> Result<String, String> {
        if self.rack_locked {
            return Err("rack is locked for editing — unlock it first (Ctrl+L or set_rack_locked)".into());
        }
        if (rig as usize) >= self.rig_count() {
            return Err(format!("no rig {rig}: there are {}", self.rig_count()));
        }
        let order = preset.fx_chain_order().map_err(|e| e.to_string())?;
        let source = source.or_else(|| self.fx_chain_source(rig, false));
        let (mut g, mut fx, tail, old_fx, tag) = match source {
            Some(s) => {
                let (g, fx, tail) = self.strip_chain_graph(s)?;
                let tag = g.node(s).map(|n| n.rig).unwrap_or(rig);
                (g, fx.clone(), tail, fx, tag)
            }
            None => (self.mirror_graph(), Vec::new(), None, Vec::new(), rig),
        };
        let mut map = std::collections::HashMap::new();
        let mut added = Vec::new();
        for &o in &order {
            let n = preset.session.graph.node(o).ok_or_else(|| format!("the chain lost node {o}"))?;
            let id = g.add_node(n.kind.clone(), n.name);
            g.set_rig(id, tag);
            map.insert(o, id);
            added.push(id);
        }
        match source {
            Some(s) => {
                fx.extend(added.iter().copied());
                self.rewire_chain_edges(&mut g, s, &old_fx, &fx, tail);
            }
            None => {
                for pair in added.windows(2) {
                    let id = g.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                    g.connect_with_id(id, PortRef { node: pair[0], port: 0 }, PortRef { node: pair[1], port: 0 }, CPortKind::Audio);
                }
            }
        }
        // Values and states go in the session the reload applies, keyed by
        // the new ids (rule 44); the plugin-state check runs where every
        // state restore runs (rule 118).
        self.reload_with_graph_edit(g, |s| {
            for p in preset.session.params() {
                if let Some(&n) = map.get(&p.node) { s.set_param(n, p.param, p.value); }
            }
            for st in preset.session.plugin_states() {
                if let Some(&n) = map.get(&st.node) { s.set_plugin_state(n, st.plugin_id.clone(), st.data.clone()); }
            }
            for st in preset.session.strips() {
                if let Some(&node) = map.get(&st.node) { s.set_strip(crate::session::StripSaved { node, ..*st }); }
            }
            for (old, label) in preset.session.strip_knobs() {
                if let Some(&n) = map.get(old) { s.set_strip_knob(n, label.clone()); }
            }
        })?;
        let units = order.len();
        let rig_name = self.rig_name(tag as usize);
        Ok(match source {
            Some(_) => format!("Inserted chain '{}' ({units} units) in {rig_name} — before the rig bus", preset.name),
            None => format!("Added chain '{}' ({units} units) to {rig_name} — it has no strip chain to join, patch it on the back (Tab)", preset.name),
        })
    }

    /// The UI's insert of a saved chain by name: undo step, rack lock, notice.
    pub(super) fn ui_insert_fx_chain(&mut self, name: &str, rig: u8, source: Option<u32>) {
        if self.rack_locked {
            self.notify_rack_locked();
            return;
        }
        let msg = match Self::load_fx_chain_from_dir(&self.fx_chains_dir.clone(), name) {
            Ok(preset) => {
                self.push_user_undo();
                match self.insert_fx_chain(&preset, rig, source) {
                    Ok(m) => { self.stage_view = StageView::Rack; m }
                    Err(e) => format!("Insert chain failed: {e}"),
                }
            }
            Err(e) => format!("Insert chain failed: {e}"),
        };
        self.plugin_msg = msg.clone();
        self.session_msg = msg.clone();
        self.notify_patch(&msg);
    }

    /// Names and unit lists of the saved chains, for lists and tools.
    pub(super) fn fx_chain_listing(&self) -> Vec<(String, Vec<String>)> {
        crate::session::list_fx_chains(&self.fx_chains_dir).into_iter().map(|name| {
            let units = Self::load_fx_chain_from_dir(&self.fx_chains_dir, &name).ok()
                .map(|p| {
                    p.fx_chain_order().unwrap_or_default().iter()
                        .filter_map(|id| p.session.graph.node(*id))
                        .map(|n| sentence_case(&name_str(&n.name)))
                        .collect()
                })
                .unwrap_or_default();
            (name, units)
        }).collect()
    }

    /// The agent's chain tools (save_fx_chain, list_fx_chains, insert_fx_chain).
    pub(super) fn fx_chain_tool(&mut self, tool: &str, args: &serde_json::Value) -> String {
        let name = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
        let rig = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize);
        let source = args.get("source").and_then(|v| v.as_u64()).map(|n| n as u32);
        match tool {
            "list_fx_chains" => {
                let chains: Vec<_> = self.fx_chain_listing().into_iter()
                    .map(|(name, units)| serde_json::json!({"name": name, "units": units}))
                    .collect();
                serde_json::json!({"chains": chains}).to_string()
            }
            "save_fx_chain" => {
                let Some(name) = name else { return "save_fx_chain needs 'name'".into() };
                let rig = rig.unwrap_or(self.active_rig);
                let Some(source) = source.or_else(|| self.fx_chain_source(rig as u8, true)) else {
                    return format!("save_fx_chain: rig {rig} has no strip with a linear chain of effects (add some with add_insert)");
                };
                let dir = self.fx_chains_dir.clone();
                match self.save_fx_chain_to_dir(&dir, source, name) {
                    Ok(_) => format!("saved the insert chain of strip node {source} as FX chain '{name}' — insert it anywhere with insert_fx_chain"),
                    Err(e) => format!("save_fx_chain failed: {e}"),
                }
            }
            "insert_fx_chain" => {
                let Some(name) = name else { return "insert_fx_chain needs 'name' (see list_fx_chains)".into() };
                let rig = rig.unwrap_or(self.active_rig);
                let preset = match Self::load_fx_chain_from_dir(&self.fx_chains_dir.clone(), name) {
                    Ok(p) => p,
                    Err(e) => return format!("insert_fx_chain failed: {e}"),
                };
                match self.insert_fx_chain(&preset, rig as u8, source) {
                    Ok(m) => format!("{m}; the rack cables moved with it (the chain is the cables)"),
                    Err(e) => format!("insert_fx_chain failed: {e}"),
                }
            }
            _ => format!("unknown tool {tool}"),
        }
    }

    /// "Save FX chain" dialog (a strip's `save chain`): Enter saves, Esc cancels.
    pub(super) fn save_chain_modal(&mut self, ctx: &egui::Context) {
        let Some((source, mut text)) = self.save_chain_dialog.take() else { return };
        let mut decision: Option<bool> = None;
        let units = self.units.iter().position(|u| u.node_id == source)
            .and_then(|u| self.strip_chain(u)).map(|c| c.len()).unwrap_or(0);
        let modal = egui::Modal::new(egui::Id::new("save_chain_modal")).show(ctx, |ui| {
            ui.set_width(340.0_f32.min(ctx.content_rect().width() - 48.0));
            ui.label(egui::RichText::new("Save FX chain").font(theme::medium_font(16.0)).color(theme::INK));
            ui.add_space(6.0);
            ui.add(egui::Label::new(
                egui::RichText::new(format!("Saves this strip's {units} effects with their knobs, sound and cables.")).size(12.5).color(theme::INK_2),
            ).wrap());
            ui.add_space(8.0);
            let w = ui.available_width() - 2.0;
            let edit = Self::recessed_input_with(ui, &mut text, w, "Chain name…", Some("save_chain_text"), Some(NAME_MAX));
            if !edit.has_focus() && !edit.lost_focus() { edit.request_focus(); }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) { decision = Some(true); }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Save chain").clicked() { decision = Some(true); }
                ui.add_space(8.0);
                if theme::chip_button(ui, "Cancel", false).clicked() { decision = Some(false); }
            });
        });
        if modal.should_close() && decision.is_none() { decision = Some(false); }
        match decision {
            Some(true) => {
                let dir = self.fx_chains_dir.clone();
                let msg = match self.save_fx_chain_to_dir(&dir, source, &text) {
                    Ok(_) => format!("Saved FX chain '{}'", text.trim()),
                    Err(e) => format!("Save chain failed: {e}"),
                };
                self.session_msg = msg.clone();
                self.plugin_msg = msg.clone();
                self.notify_patch(&msg);
            }
            Some(false) => {}
            None => self.save_chain_dialog = Some((source, text)),
        }
    }

    /// The Plugins view's FX chains list: one full-width row per chain.
    pub(super) fn fx_chain_rows(&mut self, ui: &mut egui::Ui, filter: &str) {
        let q = filter.to_lowercase();
        let chains: Vec<(String, Vec<String>)> = self.fx_chain_listing().into_iter()
            .filter(|(n, units)| q.is_empty() || n.to_lowercase().contains(&q)
                || units.iter().any(|u| u.to_lowercase().contains(&q)))
            .collect();
        if chains.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(if q.is_empty() {
                "No FX chains yet. Open the Mixer, pick a strip with two or more inserts and choose “save chain”."
            } else {
                "No FX chain matches."
            }).size(12.5).color(theme::INK_2)).wrap());
            return;
        }
        let rig = self.active_rig as u8;
        let rig_label = self.rig_name(self.active_rig);
        let mut insert: Option<String> = None;
        let mut delete: Option<String> = None;
        for (name, units) in &chains {
            egui::Frame::new()
                .fill(theme::DARK_PANEL_HI)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(Self::BROWSER_CARD_PAD_X, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.set_min_height(26.0);
                        // Buttons first, at the right edge; the text truncates in what is left.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let armed = self.fx_chain_delete_armed.as_deref() == Some(name.as_str());
                            let del = if armed { "Confirm delete" } else { "Delete" };
                            if theme::chip_button(ui, del, armed).clicked() {
                                if armed { delete = Some(name.clone()); } else { self.fx_chain_delete_armed = Some(name.clone()); }
                            }
                            if theme::primary_button(ui, "Insert").on_hover_text(format!("insert into {rig_label}")).clicked() {
                                insert = Some(name.clone());
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.vertical(|ui| {
                                    ui.add(egui::Label::new(egui::RichText::new(name).font(theme::medium_font(13.0)).color(theme::INK)).truncate());
                                    ui.add(egui::Label::new(
                                        egui::RichText::new(format!("{} · {}", units.len(), units.join(" → "))).size(11.0).color(theme::CREAM_DIM),
                                    ).truncate());
                                });
                            });
                        });
                    });
                });
            ui.add_space(6.0);
        }
        if let Some(n) = insert {
            self.ui_insert_fx_chain(&n, rig, None);
        }
        if let Some(n) = delete {
            self.fx_chain_delete_armed = None;
            let msg = match crate::session::delete_fx_chain(&self.fx_chains_dir, &n) {
                Ok(()) => format!("Deleted FX chain '{n}'"),
                Err(e) => format!("Delete failed: {e}"),
            };
            self.plugin_msg = msg;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Command, DspKind};

    const SIZE: Vec2 = Vec2::new(1280.0, 800.0);

    /// Rig A: input → Eq 3 → Saturation → Delay → Master; rig B: click →
    /// Master with one strip setting that differs from the default.
    fn two_rigs() -> DemoApp {
        let mut a = Graph::default();
        let ma = a.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::Stereo }, name_bytes("INPUT"));
        let fx: Vec<u32> = [(DspKind::Eq3, "EQ 3"), (DspKind::Saturation, "SATURATION"), (DspKind::Delay, "DELAY")]
            .into_iter().map(|(k, n)| a.add_node(NodeKind::Dsp(k), name_bytes(n))).collect();
        let master_a = a.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut prev = ma;
        for (i, &f) in fx.iter().chain(std::iter::once(&master_a)).enumerate() {
            a.connect_with_id(i as u32 + 1, PortRef { node: prev, port: 0 }, PortRef { node: f, port: 0 }, CPortKind::Audio);
            prev = f;
        }
        let mut b = Graph::default();
        let mb = b.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::Stereo }, name_bytes("INPUT"));
        let master_b = b.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        b.connect_with_id(1, PortRef { node: mb, port: 0 }, PortRef { node: master_b, port: 0 }, CPortKind::Audio);
        let rigs = vec![crate::control::Rig::new("Lead", a, None), crate::control::Rig::new("Bass", b, None)];
        let merged = crate::control::merge_rigs(&rigs);
        let mut app = DemoApp::new_with_sink(None);
        app.build_rack(&merged, &[]);
        app.session_rig_names = vec!["Lead".into(), "Bass".into()];
        if let Ok(mut s) = app.rig_snapshot.lock() {
            s.rigs = vec![
                crate::control::RigMeta { name: "Lead".into(), midi_device: None },
                crate::control::RigMeta { name: "Bass".into(), midi_device: None },
            ];
            s.active = 0;
        }
        app.layout(Rect::from_min_size(Pos2::ZERO, SIZE));
        // Non-default knob values on every unit of the chain (rule 59).
        for (i, name) in ["EQ 3", "SATURATION", "DELAY"].iter().enumerate() {
            let u = app.units.iter().position(|u| u.name == *name).unwrap();
            let k = app.units[u].knobs.iter().position(|k| k.param.is_some()).unwrap();
            app.set_unit_knob(u, k, 0.31 + 0.2 * i as f32);
        }
        app
    }

    fn click_node(app: &DemoApp, rig: u8) -> u32 {
        app.units.iter().find(|u| u.rig == rig && u.name == "INPUT").unwrap().node_id
    }

    /// (name, knob values) of a rig's chain in signal order, from the saved rig.
    fn chain_of(app: &mut DemoApp, rig: usize) -> Vec<(String, Vec<f64>)> {
        let p = app.capture_rig_preset(rig, "probe").unwrap();
        let g = &p.session.graph;
        let mut at = g.nodes.iter().find(|n| matches!(n.kind, NodeKind::AudioIn { .. })).unwrap().id;
        let mut out = Vec::new();
        while let Some(e) = g.edges.iter().find(|e| e.from.node == at) {
            at = e.to.node;
            let n = g.node(at).unwrap();
            if matches!(n.kind, NodeKind::MasterOut) { break; }
            let mut vals: Vec<f64> = p.session.params().iter().filter(|x| x.node == at).map(|x| x.value).collect();
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            out.push((name_str(&n.name).to_string(), vals));
        }
        out
    }

    /// One second of the rig's master through a real `GraphEngine`, the
    /// rig's own graph and values, the click running.
    fn render_rig(app: &mut DemoApp, rig: usize) -> Vec<f32> {
        let p = app.capture_rig_preset(rig, "render").unwrap();
        // The test has no input device: the audio input's place is taken by a click.
        let mut graph = p.session.graph.clone();
        let metro = graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::AudioIn { .. })).unwrap().id;
        graph.nodes.iter_mut().find(|n| n.id == metro).unwrap().kind = NodeKind::Dsp(DspKind::Metronome);
        let (mut bus, drain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(graph, drain, 48000.0, vec![]);
        for x in p.session.params() {
            bus.send(Command::SetParam { node: x.node, param: x.param, value: x.value });
        }
        bus.send(Command::SetBpm { bpm: 120.0 });
        bus.send(Command::SetNodePlaying { node: metro, playing: true });
        let mut all = Vec::new();
        let mut buf = vec![0.0f32; 512 * 2];
        for _ in 0..(48000 / 512) {
            engine.render(&mut buf, 2);
            all.extend_from_slice(&buf);
        }
        all
    }

    #[test]
    fn a_saved_chain_comes_back_in_another_rig_with_its_values_cables_and_sound() {
        let mut app = two_rigs();
        let before = chain_of(&mut app, 0);
        assert_eq!(before.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["EQ 3", "SATURATION", "DELAY"]);
        assert!(before.iter().all(|c| !c.1.is_empty()), "the chain's knob values are in the session: {before:?}");
        let dir = app.fx_chains_dir.clone();
        let src = click_node(&app, 0);
        app.save_fx_chain_to_dir(&dir, src, "Wide echo").unwrap();
        assert_eq!(crate::session::list_fx_chains(&dir), vec!["Wide echo".to_string()]);

        let preset = DemoApp::load_fx_chain_from_dir(&dir, "Wide echo").unwrap();
        let msg = app.insert_fx_chain(&preset, 1, None).unwrap();
        assert!(msg.contains("3 units") && msg.contains("Bass"), "{msg}");
        // Same nodes in the same order with the same values, wired between
        // rig B's source and its master.
        assert_eq!(chain_of(&mut app, 1), before);
        assert_eq!(chain_of(&mut app, 0), before, "the source rig is untouched");
        // Audio passes through the chain, and sounds like the original.
        let a = render_rig(&mut app, 0);
        let b = render_rig(&mut app, 1);
        assert!(a.iter().fold(0.0f32, |m, s| m.max(s.abs())) > 0.01, "the original chain is audible");
        let diff = a.iter().zip(&b).fold(0.0f32, |m, (x, y)| m.max((x - y).abs()));
        assert!(diff < 1e-4, "rig B through the inserted chain sounds like rig A: max diff {diff}");
    }

    #[test]
    fn inserting_a_chain_is_one_undo_step_that_restores_every_strip_value() {
        let mut app = two_rigs();
        let dir = app.fx_chains_dir.clone();
        let src = click_node(&app, 0);
        app.save_fx_chain_to_dir(&dir, src, "Chain").unwrap();
        let b_click = click_node(&app, 1);
        let amber = egui::Color32::from_rgb(0xD9, 0x8B, 0xA0);
        app.with_strip_row(b_click, |r| { r.mute = true; r.pan = -0.35; r.color = Some(amber); });
        let steps = app.undo_past.len();
        app.ui_insert_fx_chain("Chain", 1, None);
        assert_eq!(app.undo_past.len(), steps + 1, "one insert = one undo step");
        assert_eq!(chain_of(&mut app, 1).len(), 3);
        let out = app.undo_agent(true).unwrap();
        assert!(out.contains("undid"), "{out}");
        assert!(chain_of(&mut app, 1).is_empty(), "the chain's units are gone");
        let b_click = click_node(&app, 1);
        let row = app.strip_row(b_click);
        assert!(row.mute && (row.pan + 0.35).abs() < 1e-6 && row.color == Some(amber), "{row:?}");
        assert_eq!(chain_of(&mut app, 0).len(), 3);
    }

    #[test]
    fn a_locked_rack_inserts_nothing_and_a_custom_strip_saves_nothing() {
        let mut app = two_rigs();
        let dir = app.fx_chains_dir.clone();
        let src = click_node(&app, 0);
        app.save_fx_chain_to_dir(&dir, src, "Chain").unwrap();
        app.set_rack_locked(true);
        app.ui_insert_fx_chain("Chain", 1, None);
        assert!(chain_of(&mut app, 1).is_empty());
        assert!(app.exec_tool("insert_fx_chain", &serde_json::json!({"name": "Chain", "rig": 1})).contains("locked"));
        app.set_rack_locked(false);
        let b = click_node(&app, 1);
        let err = app.save_fx_chain_to_dir(&dir, b, "Empty").unwrap_err().to_string();
        assert!(err.contains("no effects"), "{err}");
        assert!(app.save_fx_chain_to_dir(&dir, src, "a/b").is_err(), "no slashes");
        assert!(DemoApp::load_fx_chain_from_dir(&dir, "Nope").is_err());
    }

    #[test]
    fn the_agent_saves_lists_and_inserts_a_chain() {
        let mut app = two_rigs();
        let out = app.exec_tool("save_fx_chain", &serde_json::json!({"name": "Agent chain", "rig": 0}));
        assert!(out.contains("saved"), "{out}");
        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("list_fx_chains", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["chains"][0]["name"], "Agent chain");
        assert_eq!(listed["chains"][0]["units"].as_array().unwrap().len(), 3);
        let out = app.exec_tool("insert_fx_chain", &serde_json::json!({"name": "Agent chain", "rig": 1}));
        assert!(out.contains("Inserted chain"), "{out}");
        assert_eq!(chain_of(&mut app, 1).len(), 3);
        let bad = app.exec_tool("insert_fx_chain", &serde_json::json!({"name": "Missing"}));
        assert!(bad.contains("failed"), "{bad}");
        for tool in ["save_fx_chain", "insert_fx_chain", "list_fx_chains"] {
            assert!(UI_BRIDGE_TOOLS.contains(&tool), "{tool} is served to attached agents");
        }
    }

    // -- the click-through ---------------------------------------------------

    struct Bench { ctx: egui::Context, t: f64 }

    impl Bench {
        fn frame_with(&mut self, app: &mut DemoApp, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
            self.t += 0.1;
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, SIZE));
            raw.time = Some(self.t);
            raw.focused = true;
            raw.events = events;
            eframe::App::raw_input_hook(app, &self.ctx, &mut raw);
            let mut out = self.ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
            out.shapes.iter().filter_map(|cs| match &cs.shape {
                Shape::Text(t) => Some((t.galley.text().to_owned(), t.visual_bounding_rect())),
                _ => None,
            }).collect()
        }
        fn settle(&mut self, app: &mut DemoApp) -> Vec<(String, Rect)> {
            for _ in 0..3 { self.frame_with(app, vec![]); }
            self.frame_with(app, vec![])
        }
        fn click(&mut self, app: &mut DemoApp, pos: Pos2) {
            let b = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            self.frame_with(app, vec![egui::Event::PointerMoved(pos)]);
            self.frame_with(app, vec![b(true)]);
            self.frame_with(app, vec![b(false)]);
        }
        fn click_text(&mut self, app: &mut DemoApp, text: &str) {
            let texts = self.settle(app);
            let r = texts.iter().find(|(t, _)| t == text)
                .unwrap_or_else(|| panic!("no '{text}' on screen: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>()))
                .1;
            self.click(app, r.center());
        }
        fn type_text(&mut self, app: &mut DemoApp, text: &str) {
            self.frame_with(app, vec![egui::Event::Text(text.into())]);
            self.frame_with(app, vec![]);
        }
    }

    #[test]
    fn the_ui_saves_a_chain_from_the_strip_lists_it_and_inserts_it_through_real_frames() {
        let mut app = two_rigs();
        let mut b = Bench { ctx: egui::Context::default(), t: 0.0 };
        // Mixer: the strip with three inserts offers "save chain".
        app.stage_view = StageView::Mixer;
        b.settle(&mut app);
        b.click_text(&mut app, "save chain");
        assert!(app.save_chain_dialog.is_some(), "the dialog opened");
        b.settle(&mut app);
        b.type_text(&mut app, "Click FX");
        b.click_text(&mut app, "Save chain");
        assert_eq!(crate::session::list_fx_chains(&app.fx_chains_dir), vec!["Click FX".to_string()]);
        assert!(app.save_chain_dialog.is_none());

        // Plugins view lists it with its units; Insert puts it into the rig in view.
        app.set_active_rig(1);
        app.stage_view = StageView::Plugins;
        b.settle(&mut app);
        b.click_text(&mut app, "FX chains (1)");
        let texts = b.settle(&mut app);
        assert!(texts.iter().any(|(t, _)| t == "Click FX"), "listed by name");
        assert!(texts.iter().any(|(t, _)| t.contains("Eq 3 → Saturation → Delay")), "with its units in order");
        b.click_text(&mut app, "Insert");
        assert_eq!(chain_of(&mut app, 1).len(), 3, "inserted into rig B from the Plugins view");
        assert_eq!(app.stage_view, StageView::Rack);

        // Quick-add lists it too: type its name, Enter inserts a second copy into the rig in view.
        app.undo_agent(true).unwrap();
        assert!(chain_of(&mut app, 1).is_empty());
        app.stage_view = StageView::Rack;
        b.settle(&mut app);
        app.open_quick_add(None);
        b.settle(&mut app);
        b.type_text(&mut app, "click fx");
        let texts = b.settle(&mut app);
        assert!(texts.iter().any(|(t, _)| t == "Click FX"), "quick-add shows the chain");
        b.frame_with(&mut app, vec![egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
        b.frame_with(&mut app, vec![egui::Event::Key { key: Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE }]);
        assert_eq!(chain_of(&mut app, 1).len(), 3, "inserted from quick-add");
    }

    /// The mixer's insert picker offers saved chains and inserts one.
    #[test]
    fn the_mixer_insert_picker_inserts_a_saved_chain() {
        let mut app = two_rigs();
        let dir = app.fx_chains_dir.clone();
        let src = click_node(&app, 0);
        app.save_fx_chain_to_dir(&dir, src, "Picker chain").unwrap();
        app.set_active_rig(1);
        let mut b = Bench { ctx: egui::Context::default(), t: 0.0 };
        app.stage_view = StageView::Mixer;
        b.settle(&mut app);
        let texts = b.settle(&mut app);
        // "+ insert" of rig A's strip, of rig B's strip, then the master's.
        let inserts: Vec<Rect> = texts.iter().filter(|(t, _)| t == "+ insert").map(|(_, r)| *r).collect();
        assert_eq!(inserts.len(), 3);
        let rig_b_insert = &inserts[1];
        b.click(&mut app, rig_b_insert.center());
        b.click_text(&mut app, "Picker chain");
        assert_eq!(chain_of(&mut app, 1).len(), 3);
    }

    /// Real frames of the chain flow: the mixer strip's "save chain", the
    /// save dialog, the Plugins list and the quick-add picker.
    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat fx_chain_shots -- --ignored"]
    fn fx_chain_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let key = |key: Key| vec![
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ];
        let saved = || {
            let mut a = two_rigs();
            let src = click_node(&a, 0);
            let d = a.fx_chains_dir.clone();
            a.save_fx_chain_to_dir(&d, src, "Wide echo").unwrap();
            a.set_active_rig(1);
            a
        };
        for (w, h) in [(1280.0, 800.0), (760.0, 520.0)] {
            let size = Vec2::new(w, h);
            let tag = format!("{w}x{h}");
            let mut mixer = two_rigs();
            mixer.stage_view = StageView::Mixer;
            super::super::offline_shots::shot(&mut mixer, size, &dir.join(format!("mixer-save-chain-{tag}.png")));
            let mut dialog = two_rigs();
            dialog.stage_view = StageView::Mixer;
            dialog.save_chain_dialog = Some((click_node(&dialog, 0), "Wide echo".into()));
            super::super::offline_shots::shot(&mut dialog, size, &dir.join(format!("save-chain-dialog-{tag}.png")));
            let mut list = saved();
            list.stage_view = StageView::Plugins;
            list.plugin_chains_view = true;
            super::super::offline_shots::shot(&mut list, size, &dir.join(format!("plugins-chains-{tag}.png")));
            super::super::offline_shots::shot_with(&mut saved(), size, &dir.join(format!("quick-add-chain-{tag}.png")), |_, i| match i {
                3 => key(Key::Enter),
                5 => vec![egui::Event::Text("wide".into())],
                _ => Vec::new(),
            });
            let mut after = saved();
            let d = after.fx_chains_dir.clone();
            let preset = DemoApp::load_fx_chain_from_dir(&d, "Wide echo").unwrap();
            after.insert_fx_chain(&preset, 1, None).unwrap();
            after.stage_view = StageView::Mixer;
            super::super::offline_shots::shot(&mut after, size, &dir.join(format!("mixer-after-insert-{tag}.png")));
        }
    }
}
