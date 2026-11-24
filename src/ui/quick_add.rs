//! Favorites, recents and the quick-add picker (#307).
//!
//! The picker opens over the rack with Enter or a right-click on empty rack
//! space and adds a plugin without leaving the Rack view. It is a second view
//! of the one classified plugin list (`plugins::entries`, rule 170) and runs
//! the Plugins browser's own actions (`run_browser_action`), so both add the
//! same way, honour the rack lock (rule 214) and feed the same recents.

use super::*;
use crate::plugins::{quick_search, Entry, ShelfView};

/// One row of the picker's list is this tall.
const ROW_H: f32 = 28.0;
/// Rows laid out per frame; a longer list is narrowed by typing.
const MAX_ROWS: usize = 80;

/// The open quick-add picker.
pub(super) struct QuickAdd {
    query: String,
    /// The highlighted row.
    sel: usize,
    /// Where a right-click opened it; None = opened by key, placed centred.
    pos: Option<Pos2>,
    view: Option<ShelfView>,
    ty: Option<PluginType>,
    /// Set in the frame it opens: the click or key that opened it is not its
    /// own input, so it cannot close or pick in that frame (rule 153).
    fresh: bool,
    /// Bring the highlighted row into view on the next layout.
    reveal: bool,
    /// Where it was drawn last frame.
    pub(super) rect: Rect,
}

/// Row keys of saved FX chains start with this; plugin keys never do.
const CHAIN_KEY: &str = "chain:";

/// What the list shows for one plugin, detached from the scan's borrow.
struct Row {
    key: String,
    name: String,
    badge: String,
    badge_col: Color32,
    favorite: bool,
}

fn type_color(ty: PluginType) -> Color32 {
    match ty {
        PluginType::Instrument => theme::ACCENT_BRIGHT,
        PluginType::Effect => theme::LIVE_TEAL,
        PluginType::MidiFx => theme::MIDI_BLUE,
        PluginType::Utility => theme::CREAM_META,
    }
}

fn format_tag(e: &Entry) -> &'static str {
    match e.source {
        crate::plugins::Source::Builtin(_) => "BUILT-IN",
        crate::plugins::Source::Clap(_) => "CLAP",
        crate::plugins::Source::Vst3(_) => "VST3",
        crate::plugins::Source::Lv2(_) => "LV2",
    }
}

fn cycle_quick_add_filter(
    view: Option<ShelfView>,
    ty: Option<PluginType>,
    step: isize,
) -> (Option<ShelfView>, Option<PluginType>) {
    let mut filters: Vec<(Option<ShelfView>, Option<PluginType>)> = Vec::with_capacity(3 + PluginType::ALL.len());
    filters.push((None, None));
    filters.push((Some(ShelfView::Favorites), None));
    filters.push((Some(ShelfView::Recent), None));
    for t in PluginType::ALL {
        filters.push((None, Some(t)));
    }
    let cur = filters
        .iter()
        .position(|&(v, t)| v == view && t == ty)
        .or_else(|| view.and_then(|v| filters.iter().position(|&(fv, _)| fv == Some(v))))
        .or_else(|| ty.and_then(|t| filters.iter().position(|&(_, ft)| ft == Some(t))))
        .unwrap_or(0);
    let next = (cur as isize + step).rem_euclid(filters.len() as isize) as usize;
    filters[next]
}

impl DemoApp {
    /// The star on a plugin card: filled when the plugin is a favorite.
    /// Returns whether it was clicked.
    pub(super) fn favorite_star(ui: &mut egui::Ui, starred: bool) -> bool {
        let (r, resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
        let col = match (starred, resp.hovered()) {
            (true, _) => theme::CABLE_AMBER,
            (false, true) => theme::CREAM,
            (false, false) => theme::CREAM_DIM,
        };
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            if starred { "★" } else { "☆" },
            FontId::proportional(14.0),
            col,
        );
        let resp = resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(if starred { "remove from favorites" } else { "add to favorites" });
        resp.clicked()
    }

    /// Star or unstar a plugin by its shelf key and write the config file.
    pub(super) fn set_plugin_favorite(&mut self, key: &str, on: bool) {
        if self.plugin_shelf.set_favorite(key, on) {
            if let Err(e) = self.plugin_shelf.save() {
                self.plugin_msg = format!("Favorites not saved: {e}");
            }
        }
    }

    /// Run what a Plugins-browser card or the quick-add picker asked for.
    /// `key` is the shelf key of the plugin an add action is for: an add
    /// that went through makes it the newest recent.
    pub(super) fn run_browser_action(&mut self, act: BrowserAction, key: Option<String>) {
        match act {
            BrowserAction::Install(id) => {
                if let Err(e) = self.start_install(&id) {
                    self.plugin_msg = format!("Install: {e}");
                }
                return;
            }
            BrowserAction::ShowType(ty) => {
                self.plugin_category = PluginCategory::of_type(ty);
                self.plugin_subtype = None;
                return;
            }
            BrowserAction::ToggleFavorite(key) => {
                let on = !self.plugin_shelf.keys(ShelfView::Favorites).contains(&key);
                self.set_plugin_favorite(&key, on);
                return;
            }
            _ => {}
        }
        if self.rack_locked {
            self.plugin_msg = "Rack is locked for editing — unlock (Ctrl+L) to add plugins".into();
            self.notify_rack_locked();
            return;
        }
        match act {
            BrowserAction::NewClapRig(id, name) => self.add_clap_rig(&id, &name),
            BrowserAction::NewVst3Rig(cid, name) => self.add_vst3_rig(&cid, &name),
            BrowserAction::NewLv2Rig(uri, name) => self.add_lv2_instrument_rig(&uri, &name),
            BrowserAction::AddInstrument(kind, name) => self.add_instrument_to_active_rig(kind, &name),
            BrowserAction::AddEffect(kind, name) => self.add_effect_to_active_rig(kind, &name),
            BrowserAction::AddMasterEffect(kind, name) => self.add_effect_to_master(kind, &name),
            BrowserAction::AddUnwired(kind, name) => self.add_unwired_to_active_rig(kind, &name),
            BrowserAction::AddArp => self.add_arp_to_active_rig(),
            BrowserAction::AddMetronome => self.add_metronome_to_active_rig(),
            BrowserAction::AddMidiFx(uri, name) => self.add_midi_fx_to_active_rig(uri.as_deref(), &name),
            BrowserAction::Install(_) | BrowserAction::ShowType(_) | BrowserAction::ToggleFavorite(_) => {}
        }
        let failed = self.plugin_msg.starts_with("Add failed") || self.plugin_msg.starts_with("At most");
        if let (Some(key), false) = (key, failed) {
            self.plugin_shelf.note_used(&key);
            if let Err(e) = self.plugin_shelf.save() {
                eprintln!("plugin recents not saved: {e}");
            }
        }
    }

    /// The agent's `set_plugin_favorite`: star or unstar an installed plugin.
    pub(super) fn plugin_favorite_tool(&mut self, args: &serde_json::Value) -> String {
        let Some(wanted) = args.get("plugin").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else {
            return "error: give plugin = an id, cid or uri from list_plugins (e.g. builtin.delay)".into();
        };
        let Some(on) = args.get("favorite").and_then(|v| v.as_bool()) else {
            return "error: give favorite=true to star the plugin or favorite=false to unstar it".into();
        };
        if !self.plugin_lists_ready() {
            return "scanning installed plugins in the background (first run) — ask again in a few seconds".into();
        }
        let found = {
            let all = crate::plugins::entries(&self.plugins_clap, &self.plugins_lv2, &self.plugins_vst3);
            crate::plugins::find_plugin(&all, wanted).map(|e| (e.key(), e.name().to_string()))
        };
        match found {
            Ok((key, name)) => {
                self.set_plugin_favorite(&key, on);
                serde_json::json!({
                    "plugin": key,
                    "name": name,
                    "favorite": on,
                    "favorites": self.plugin_shelf.keys(ShelfView::Favorites),
                })
                .to_string()
            }
            Err(e) => format!("error: {e}"),
        }
    }

    /// Open the quick-add picker: at `pos` after a right-click, centred over
    /// the rack after Enter. A locked rack says so instead (rule 214).
    pub(super) fn open_quick_add(&mut self, pos: Option<Pos2>) {
        if self.rack_locked {
            self.notify_rack_locked();
            return;
        }
        // Start the background scan if it has not run (rule 57); built-in
        // units are listed at once.
        let _ = self.plugin_lists_ready();
        self.knob_menu = None;
        self.unit_menu = None;
        self.cable_menu = None;
        self.scale_menu = None;
        self.quick_add = Some(QuickAdd {
            query: String::new(),
            sel: 0,
            pos,
            view: None,
            ty: None,
            fresh: true,
            reveal: false,
            rect: Rect::NOTHING,
        });
    }

    fn close_quick_add(&mut self, ctx: &egui::Context) {
        self.quick_add = None;
        // The search field is gone: give the keyboard back to the rack.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }

    /// Add the plugin with shelf key `key` from the picker. Enter adds to
    /// the rig in view (an instrument is layered into it); `alt`
    /// (Shift+Enter) takes the card's other action: a new rig for an
    /// instrument, a master insert for an effect.
    fn quick_add_pick(&mut self, key: &str, alt: bool) {
        // A saved FX chain (#429) goes into the rig in view, where one effect would.
        if let Some(name) = key.strip_prefix(CHAIN_KEY) {
            self.ui_insert_fx_chain(name, self.active_rig as u8, None);
            return;
        }
        let act = {
            let all = crate::plugins::entries(&self.plugins_clap, &self.plugins_lv2, &self.plugins_vst3);
            let Some(e) = all.iter().find(|e| e.has_key(key)) else { return };
            let mut actions: Vec<BrowserAction> = BrowserCard::of(e).actions.into_iter().map(|(_, _, a)| a).collect();
            let elsewhere = |a: &BrowserAction| {
                matches!(
                    a,
                    BrowserAction::NewClapRig(..)
                        | BrowserAction::NewVst3Rig(..)
                        | BrowserAction::NewLv2Rig(..)
                        | BrowserAction::AddMasterEffect(..)
                )
            };
            let at = actions.iter().position(|a| elsewhere(a) == alt).or(if actions.is_empty() { None } else { Some(0) });
            let Some(at) = at else { return };
            actions.swap_remove(at)
        };
        self.run_browser_action(act, Some(key.to_string()));
        // The Plugins view shows `plugin_msg`; on the rack it is a notice.
        let msg = self.plugin_msg.clone();
        if !self.rack_locked {
            self.notify_patch(&msg);
        }
    }

    /// The quick-add picker, drawn over the rack while it is open.
    pub(super) fn quick_add_ui(&mut self, ctx: &egui::Context) {
        let Some(mut qa) = self.quick_add.take() else { return };
        if self.stage_view != StageView::Rack || (self.dock == DockView::Keyboard && self.kb_expanded) {
            self.close_quick_add(ctx);
            return;
        }
        let fresh = std::mem::take(&mut qa.fresh);

        // Keys before the search field sees them. Shift+Enter / Shift+Tab are
        // asked before unmodified Enter / Tab (rule 143).
        let (up, down, page_up, page_down, home, end, prev_tab, next_tab, alt_enter, enter, esc) = if fresh {
            (false, false, false, false, false, false, false, false, false, false, false)
        } else {
            ctx.input_mut(|i| {
                let prev_tab = i.consume_key(Modifiers::SHIFT, Key::Tab);
                let next_tab = !prev_tab && i.consume_key(Modifiers::NONE, Key::Tab);
                if prev_tab || next_tab {
                    i.events.retain(|ev| !matches!(ev, egui::Event::Text(t) if t == "\t"));
                }
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::PageUp),
                    i.consume_key(Modifiers::NONE, Key::PageDown),
                    i.consume_key(Modifiers::NONE, Key::Home),
                    i.consume_key(Modifiers::NONE, Key::End),
                    prev_tab,
                    next_tab,
                    i.consume_key(Modifiers::SHIFT, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Escape),
                )
            })
        };

        if prev_tab || next_tab {
            let step = if prev_tab { -1 } else { 1 };
            (qa.view, qa.ty) = cycle_quick_add_filter(qa.view, qa.ty, step);
            qa.sel = 0;
            qa.reveal = true;
        }

        let stage = self.stage_rect;
        let width = 380.0_f32.min(stage.width() - 24.0).max(240.0);
        let list_h = ((stage.height() - 190.0) / ROW_H).floor().clamp(3.0, 9.0) * ROW_H;
        let page_rows = ((list_h / ROW_H).round() as usize).max(1);

        let scanning = !self.plugins_loaded;
        // Saved FX chains (#429) lead the list when they match the words typed,
        // in the All and Effects views.
        let chain_rows: Vec<Row> = if qa.view.is_none() && qa.ty.is_none_or(|t| t == PluginType::Effect) {
            let words: Vec<String> = qa.query.split_whitespace().map(str::to_lowercase).collect();
            self.fx_chain_listing().into_iter()
                .filter(|(name, units)| {
                    let hay = format!("{} {} fx chain", name.to_lowercase(), units.join(" ").to_lowercase());
                    words.iter().all(|w| hay.contains(w.as_str()))
                })
                .map(|(name, units)| Row {
                    key: format!("{CHAIN_KEY}{name}"),
                    name,
                    badge: format!("FX CHAIN · {} units", units.len()),
                    badge_col: type_color(PluginType::Effect),
                    favorite: false,
                })
                .collect()
        } else {
            Vec::new()
        };
        let (rows, total): (Vec<Row>, usize) = {
            let all = crate::plugins::entries(&self.plugins_clap, &self.plugins_lv2, &self.plugins_vst3);
            let hits = quick_search(&all, &qa.query, &self.plugin_shelf, qa.view, qa.ty);
            let rows = hits
                .iter()
                .take(MAX_ROWS)
                .map(|e| Row {
                    key: e.key(),
                    name: e.name().to_string(),
                    badge: format!("{} · {}", format_tag(e), e.badge()),
                    badge_col: type_color(e.ty()),
                    favorite: self.plugin_shelf.is_favorite(e),
                })
                .collect();
            (rows, hits.len())
        };
        let total = total + chain_rows.len();
        let rows: Vec<Row> = chain_rows.into_iter().chain(rows).collect();
        if rows.is_empty() {
            qa.sel = 0;
        } else {
            if down {
                qa.sel = (qa.sel + 1) % rows.len();
                qa.reveal = true;
            }
            if up {
                qa.sel = (qa.sel + rows.len() - 1) % rows.len();
                qa.reveal = true;
            }
            if page_down {
                qa.sel = (qa.sel + page_rows).min(rows.len() - 1);
                qa.reveal = true;
            }
            if page_up {
                qa.sel = qa.sel.saturating_sub(page_rows);
                qa.reveal = true;
            }
            if home {
                qa.sel = 0;
                qa.reveal = true;
            }
            if end {
                qa.sel = rows.len() - 1;
                qa.reveal = true;
            }
            qa.sel = qa.sel.min(rows.len() - 1);
        }

        let pos = match qa.pos {
            Some(p) => p + Vec2::new(6.0, 6.0),
            None => Pos2::new(stage.center().x - width * 0.5, stage.top() + (stage.height() * 0.1).max(8.0)),
        };
        let rig = self.rig_name(self.active_rig);
        let mut picked: Option<(String, bool)> = None;
        let pointer_moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO);
        let shift = ctx.input(|i| i.modifiers.shift);

        let area = egui::Area::new(egui::Id::new("quick_add"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain_to(stage)
            .show(ctx, |ui| {
                theme::dropdown_popup_frame().inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                    let inner_w = width - 20.0;
                    ui.set_width(inner_w);
                    // The hint first, at the right edge; the title truncates
                    // in what is left (rule 196).
                    ui.horizontal(|ui| {
                        ui.set_min_height(18.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new("Esc closes").size(10.5).color(theme::CREAM_DIM));
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("Add to {rig}"))
                                            .font(theme::medium_font(12.5))
                                            .color(theme::CREAM),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                    });
                    ui.add_space(6.0);
                    let field = Self::recessed_input(ui, &mut qa.query, inner_w - 18.0, "Search plugins to add…");
                    // The field keeps the keyboard while the picker is open,
                    // so no rack shortcut or laptop key fires under it (rule 12).
                    if !field.has_focus() {
                        field.request_focus();
                    }
                    if field.changed() {
                        qa.sel = 0;
                        qa.reveal = true;
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        let mut changed = false;
                        if theme::wrapped_quiet_filter_chip(ui, "All", qa.view.is_none() && qa.ty.is_none()).clicked() {
                            qa.view = None;
                            qa.ty = None;
                            changed = true;
                        }
                        for (view, label) in [(ShelfView::Favorites, "★ Favorites"), (ShelfView::Recent, "Recent")] {
                            let on = qa.view == Some(view);
                            if theme::wrapped_quiet_filter_chip(ui, label, on).clicked() {
                                qa.view = if on { None } else { Some(view) };
                                changed = true;
                            }
                        }
                        for ty in PluginType::ALL {
                            let on = qa.ty == Some(ty);
                            if theme::wrapped_quiet_filter_chip(ui, ty.label(), on).clicked() {
                                qa.ty = if on { None } else { Some(ty) };
                                changed = true;
                            }
                        }
                        if changed {
                            qa.sel = 0;
                            qa.reveal = true;
                        }
                    });
                    ui.add_space(6.0);
                    Self::stage_hairline(ui);
                    ui.add_space(4.0);

                    egui::ScrollArea::vertical()
                        .id_salt("quick_add_scroll")
                        .min_scrolled_height(list_h)
                        .max_height(list_h)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for (i, row) in rows.iter().enumerate() {
                                let w = ui.available_width();
                                let (r, resp) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::click());
                                if resp.hovered() && pointer_moved {
                                    qa.sel = i;
                                }
                                if resp.hovered() {
                                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                }
                                let selected = qa.sel == i;
                                if selected {
                                    ui.painter().rect_filled(r, 2.0, theme::DARK_PANEL_HI);
                                    if std::mem::take(&mut qa.reveal) {
                                        ui.scroll_to_rect(r, None);
                                    }
                                }
                                let p = ui.painter();
                                let badge = p.layout_no_wrap(row.badge.clone(), FontId::proportional(9.5), row.badge_col);
                                let badge_w = badge.size().x;
                                p.galley(
                                    Pos2::new(r.right() - 8.0 - badge_w, r.center().y - badge.size().y * 0.5),
                                    badge,
                                    row.badge_col,
                                );
                                let mut x = r.left() + 8.0;
                                if row.favorite {
                                    p.text(
                                        Pos2::new(x, r.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        "★",
                                        FontId::proportional(11.0),
                                        theme::CABLE_AMBER,
                                    );
                                    x += 16.0;
                                }
                                let name_w = (r.right() - 8.0 - badge_w - 12.0 - x).max(40.0);
                                let col = if selected { theme::CREAM } else { theme::INK };
                                let name = Self::ellipsized(p, &row.name, FontId::proportional(12.5), col, name_w);
                                p.galley(Pos2::new(x, r.center().y - name.size().y * 0.5), name, col);
                                if resp.clicked() {
                                    picked = Some((row.key.clone(), shift));
                                }
                            }
                            if rows.is_empty() {
                                let query = qa.query.trim();
                                let empty = match (query.is_empty(), qa.view) {
                                    (false, _) => format!("No plugin matches '{query}'."),
                                    (true, Some(ShelfView::Favorites)) => {
                                        "No favorites yet. Star a plugin in the Plugins view (B).".to_string()
                                    }
                                    (true, Some(ShelfView::Recent)) => "Nothing added yet.".to_string(),
                                    (true, None) => "Nothing to add in this category.".to_string(),
                                };
                                ui.add_space(6.0);
                                ui.add(egui::Label::new(egui::RichText::new(empty).size(12.0).color(theme::INK_2)).wrap());
                            }
                        });
                    ui.add_space(4.0);
                    Self::stage_hairline(ui);
                    ui.add_space(4.0);
                    let foot = if scanning {
                        "Scanning installed plugins…".to_string()
                    } else if total > rows.len() {
                        format!("{} of {total} shown · type to narrow", rows.len())
                    } else {
                        "↑↓ choose · Enter adds to the rig · Shift+Enter: new rig or master FX".to_string()
                    };
                    ui.add(egui::Label::new(egui::RichText::new(foot).size(10.5).color(theme::CREAM_DIM)).truncate());
                });
            });
        qa.rect = area.response.rect;

        if (enter || alt_enter) && picked.is_none() {
            if let Some(row) = rows.get(qa.sel) {
                picked = Some((row.key.clone(), alt_enter));
            }
        }
        let outside = !fresh
            && ctx.input(|i| i.pointer.primary_clicked() || i.pointer.secondary_clicked())
            && !ctx.pointer_interact_pos().is_some_and(|p| qa.rect.contains(p));
        if let Some((key, alt)) = picked {
            self.close_quick_add(ctx);
            self.quick_add_pick(&key, alt);
        } else if esc || outside {
            self.close_quick_add(ctx);
        } else {
            self.quick_add = Some(qa);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Vec2 = Vec2::new(1280.0, 800.0);
    const DEXED: &str = "clap:com.digital-suburban.dexed";

    /// The demo rack with a finished plugin scan: Dexed (CLAP instrument)
    /// and one LV2 reverb next to the built-in units.
    fn app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.plugins_loaded = true;
        app.plugins_clap = vec![ClapPluginInfo {
            id: "com.digital-suburban.dexed".into(),
            name: "Dexed".into(),
            vendor: "Digital Suburban".into(),
            features: vec!["instrument".into(), "synthesizer".into()],
        }];
        app.plugins_lv2 = vec![Lv2PluginInfo {
            uri: "http://example.org/roomverb".into(),
            name: "Room Reverb".into(),
            author: "Example".into(),
            lv2_class: "ReverbPlugin".into(),
            num_ports: 4,
            is_instrument: false,
            is_midi_fx: false,
            has_ui: false,
            bundle_path: String::new(),
        }];
        app
    }

    /// Real frames through `frame_ui`, with a clock that moves (popups fade
    /// in, rule 101).
    struct Bench {
        ctx: egui::Context,
        size: Vec2,
        t: f64,
    }

    impl Bench {
        fn new(size: Vec2) -> Self {
            Self { ctx: egui::Context::default(), size, t: 0.0 }
        }

        fn frame_with(&mut self, app: &mut DemoApp, events: Vec<egui::Event>, modifiers: Modifiers) -> Vec<(String, Rect)> {
            self.t += 0.1;
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, self.size));
            raw.time = Some(self.t);
            raw.focused = true;
            raw.events = std::iter::once(egui::Event::ModifiersChanged(modifiers)).chain(events).collect();
            eframe::App::raw_input_hook(app, &self.ctx, &mut raw);
            let mut out = self.ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear(); // rule 11
            out.shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    Shape::Text(t) => Some((t.galley.text().to_owned(), t.visual_bounding_rect())),
                    _ => None,
                })
                .collect()
        }

        fn frame(&mut self, app: &mut DemoApp) -> Vec<(String, Rect)> {
            self.frame_with(app, vec![], Modifiers::NONE)
        }

        /// Settle the layout (rule 212), then one more frame whose texts are returned.
        fn settle(&mut self, app: &mut DemoApp) -> Vec<(String, Rect)> {
            for _ in 0..3 {
                self.frame(app);
            }
            self.frame(app)
        }

        fn key(&mut self, app: &mut DemoApp, key: Key, modifiers: Modifiers) {
            let ev = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
            self.frame_with(app, vec![ev(true)], modifiers);
            self.frame_with(app, vec![ev(false)], Modifiers::NONE);
        }

        fn type_text(&mut self, app: &mut DemoApp, text: &str) {
            self.frame_with(app, vec![egui::Event::Text(text.into())], Modifiers::NONE);
            self.frame(app);
        }

        /// Move, press and release on frames of their own, as a hand does.
        fn click(&mut self, app: &mut DemoApp, pos: Pos2, button: egui::PointerButton) {
            let b = |pressed| egui::Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
            self.frame_with(app, vec![egui::Event::PointerMoved(pos)], Modifiers::NONE);
            self.frame_with(app, vec![b(true)], Modifiers::NONE);
            self.frame_with(app, vec![b(false)], Modifiers::NONE);
        }
    }

    fn find<'a>(texts: &'a [(String, Rect)], text: &str) -> Option<&'a Rect> {
        texts.iter().find(|(t, _)| t == text).map(|(_, r)| r)
    }

    /// The star on the card titled `title`: the star glyph on the title's row.
    fn star_of(texts: &[(String, Rect)], title: &str) -> Pos2 {
        let row = find(texts, title).unwrap_or_else(|| panic!("no card titled {title}"));
        texts
            .iter()
            .filter(|(t, r)| (t == "☆" || t == "★") && (r.center().y - row.center().y).abs() < 14.0 && r.left() > row.left())
            .min_by(|a, b| a.1.left().total_cmp(&b.1.left()))
            .unwrap_or_else(|| panic!("no star on the row of {title}"))
            .1
            .center()
    }

    /// Empty rack space: a stage point the rack's hit-test calls background,
    /// away from `avoid`.
    fn empty_rack_point(app: &DemoApp, avoid: Rect) -> Pos2 {
        let s = app.stage_rect;
        for iy in 1..40 {
            for ix in 1..40 {
                let p = Pos2::new(s.left() + s.width() * ix as f32 / 40.0, s.bottom() - s.height() * iy as f32 / 40.0);
                if matches!(app.hit_test(p), Hit::Background) && !avoid.expand(20.0).contains(p) && !app.back_controls_rect.contains(p) {
                    return p;
                }
            }
        }
        panic!("no empty rack space in {s:?}");
    }

    fn count(app: &DemoApp, kind: DspKind) -> usize {
        app.units.iter().filter(|u| matches!(&u.node_kind, UnitKind::Dsp(k) if *k == kind)).count()
    }

    #[test]
    fn a_starred_plugin_is_under_favorites_and_an_added_one_under_recent() {
        let mut app = app();
        app.stage_view = StageView::Plugins;
        let mut b = Bench::new(Vec2::new(1600.0, 1000.0));
        let texts = b.settle(&mut app);
        assert!(find(&texts, "★ Favorites (0)").is_some(), "the Favorites chip shows its count");
        assert!(find(&texts, "Recent (0)").is_some());

        // Star Dexed on its card.
        b.click(&mut app, star_of(&texts, "Dexed"), egui::PointerButton::Primary);
        assert_eq!(app.plugin_shelf.keys(ShelfView::Favorites), [DEXED]);
        assert_eq!(app.stage_view, StageView::Plugins, "starring adds nothing to the rack");
        let texts = b.settle(&mut app);
        let chip = find(&texts, "★ Favorites (1)").expect("the chip counts the favorite").center();

        // The Favorites filter shows it alone, and survives its opening click.
        b.click(&mut app, chip, egui::PointerButton::Primary);
        assert_eq!(app.plugin_shelf_view, Some(ShelfView::Favorites));
        let texts = b.settle(&mut app);
        assert!(find(&texts, "Dexed").is_some());
        assert!(find(&texts, "Delay").is_none() && find(&texts, "Room Reverb").is_none(), "only favorites are listed");

        // Unstar it there: the shelf says how to fill it.
        b.click(&mut app, star_of(&texts, "Dexed"), egui::PointerButton::Primary);
        assert!(app.plugin_shelf.keys(ShelfView::Favorites).is_empty());
        let texts = b.settle(&mut app);
        assert!(texts.iter().any(|(t, _)| t.contains("No favorites here yet")), "an empty shelf explains itself");

        // Add the Limiter from its card: it becomes the newest recent.
        app.plugin_shelf_view = None;
        app.plugin_filter = "limiter".into();
        let texts = b.settle(&mut app);
        let add = find(&texts, "Add to rig").expect("the Limiter card's button").center();
        let before = count(&app, DspKind::Limiter);
        b.click(&mut app, add, egui::PointerButton::Primary);
        assert_eq!(count(&app, DspKind::Limiter), before + 1, "{}", app.plugin_msg);
        assert_eq!(app.plugin_shelf.keys(ShelfView::Recent), ["builtin:builtin.limiter"]);

        app.stage_view = StageView::Plugins;
        app.plugin_filter.clear();
        let texts = b.settle(&mut app);
        let chip = find(&texts, "Recent (1)").expect("the Recent chip counts it").center();
        b.click(&mut app, chip, egui::PointerButton::Primary);
        let texts = b.settle(&mut app);
        assert!(find(&texts, "Limiter").is_some() && find(&texts, "Dexed").is_none());
    }

    #[test]
    fn a_locked_rack_still_stars_but_adds_nothing_and_keeps_recents() {
        let mut app = app();
        app.set_rack_locked(true);
        app.run_browser_action(BrowserAction::ToggleFavorite(DEXED.into()), None);
        assert_eq!(app.plugin_shelf.keys(ShelfView::Favorites), [DEXED], "a favorite is no rack edit");
        let before = app.units.len();
        app.run_browser_action(
            BrowserAction::AddEffect(NodeKind::Dsp(DspKind::Reverb), "Reverb".into()),
            Some("builtin:builtin.reverb".into()),
        );
        assert_eq!(app.units.len(), before);
        assert!(app.plugin_shelf.keys(ShelfView::Recent).is_empty(), "a refused add is no recent");
        assert!(app.plugin_msg.contains("locked"), "{}", app.plugin_msg);
    }

    #[test]
    fn enter_opens_quick_add_and_enter_adds_the_best_match_to_the_rig_in_view() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);
        let (dock, reverbs) = (app.dock, count(&app, DspKind::Reverb));
        assert!(app.quick_add.is_none());

        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_some(), "Enter on the rack opens the picker");
        // It survives the key that opened it and the frames after (rule 153).
        let texts = b.settle(&mut app);
        assert!(app.quick_add.is_some());
        assert!(typing(&b.ctx), "the search field has the keyboard");
        assert!(find(&texts, &format!("Add to {}", app.rig_name(app.active_rig))).is_some(), "it names the rig it adds to");
        let qa = app.quick_add.as_ref().unwrap().rect;
        assert!(app.stage_rect.expand(1.0).contains_rect(qa), "over the rack: {qa:?} in {:?}", app.stage_rect);

        // Typed letters go to the search, not to rack shortcuts (V is the
        // keyboard dock, R/E are laptop keys).
        b.type_text(&mut app, "rev");
        assert_eq!(app.quick_add.as_ref().unwrap().query, "rev");
        assert_eq!(app.dock, dock, "V did not toggle the dock");
        assert_eq!(app.stage_view, StageView::Rack);
        let texts = b.frame(&mut app);
        let (reverb, room) = (find(&texts, "Reverb").expect("Reverb listed"), find(&texts, "Room Reverb").expect("Room Reverb listed"));
        assert!(reverb.top() < room.top(), "the closest name is the first row");
        assert!(find(&texts, "Dexed").is_none(), "what does not match is not listed");

        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_none(), "picking closes the picker");
        assert_eq!(count(&app, DspKind::Reverb), reverbs + 1, "{}", app.plugin_msg);
        assert_eq!(app.stage_view, StageView::Rack, "without leaving the rack");
        assert_eq!(app.plugin_shelf.keys(ShelfView::Recent), ["builtin:builtin.reverb"]);
        b.frame(&mut app);
        assert!(!typing(&b.ctx), "the rack has the keyboard back");
        assert!(app.active_patch_notice().is_some_and(|n| n.contains("Reverb")), "the rack says what was added");

        // The next Enter opens it again, with the recent on top.
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        let texts = b.settle(&mut app);
        let first = texts
            .iter()
            .filter(|(_, r)| app.quick_add.as_ref().unwrap().rect.contains_rect(*r) && r.height() > 8.0)
            .filter(|(t, _)| ["Reverb", "Delay", "Dexed", "Filter"].contains(&t.as_str()))
            .min_by(|a, c| a.1.top().total_cmp(&c.1.top()))
            .map(|(t, _)| t.clone());
        assert_eq!(first.as_deref(), Some("Reverb"), "the plugin added last heads the list");
    }

    #[test]
    fn arrows_choose_a_row_and_shift_enter_takes_the_other_action() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);
        let rigs = app.rig_count();

        // Enter layers an instrument into the rig in view.
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        b.frame(&mut app);
        b.type_text(&mut app, "dxd");
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert_eq!(app.rig_count(), rigs, "{}", app.plugin_msg);
        assert_eq!(app.units.iter().filter(|u| u.name == "DEXED").count(), 1, "{}", app.plugin_msg);

        // Shift+Enter makes a new rig of it.
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        b.frame(&mut app);
        b.type_text(&mut app, "dexed");
        b.key(&mut app, Key::Enter, Modifiers::SHIFT);
        assert!(app.quick_add.is_none());
        assert_eq!(app.rig_count(), rigs + 1, "{}", app.plugin_msg);

        // Arrow down moves to the second row; up wraps back.
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        b.frame(&mut app);
        b.type_text(&mut app, "reverb");
        assert_eq!(app.quick_add.as_ref().unwrap().sel, 0);
        b.key(&mut app, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(app.quick_add.as_ref().unwrap().sel, 1);
        b.key(&mut app, Key::ArrowUp, Modifiers::NONE);
        b.key(&mut app, Key::ArrowUp, Modifiers::NONE);
        assert!(app.quick_add.as_ref().unwrap().sel >= 2, "up from the first row wraps to the last");
        b.key(&mut app, Key::ArrowDown, Modifiers::NONE);
        b.key(&mut app, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(app.quick_add.as_ref().unwrap().sel, 1, "down from the last wraps to the first, then the second");
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.plugin_msg.contains("Room reverb"), "{}", app.plugin_msg);
        assert_eq!(app.plugin_shelf.keys(ShelfView::Recent)[0], "lv2:http://example.org/roomverb");
    }

    #[test]
    fn a_right_click_on_empty_rack_space_opens_quick_add_and_a_click_outside_closes_it() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);
        let empty = empty_rack_point(&app, Rect::NOTHING);

        b.click(&mut app, empty, egui::PointerButton::Secondary);
        assert!(app.quick_add.is_some(), "right-click on empty rack space opens the picker");
        // The click that opened it does not close it (rule 153).
        b.settle(&mut app);
        let qa = app.quick_add.as_ref().expect("still open after its opening click").rect;
        assert!(app.stage_rect.expand(1.0).contains_rect(qa), "{qa:?}");
        assert!(qa.expand(12.0).contains(empty) || !app.stage_rect.contains(empty + Vec2::splat(200.0)), "it opens at the click: {qa:?} / {empty:?}");

        // A row click adds: the Delay row.
        b.type_text(&mut app, "limiter");
        let texts = b.frame(&mut app);
        let row = find(&texts, "Limiter").expect("the Limiter row").center();
        let before = count(&app, DspKind::Limiter);
        b.click(&mut app, row, egui::PointerButton::Primary);
        assert!(app.quick_add.is_none());
        assert_eq!(count(&app, DspKind::Limiter), before + 1, "{}", app.plugin_msg);

        // A click outside closes it and selects nothing on the rack.
        b.settle(&mut app);
        let empty = empty_rack_point(&app, Rect::NOTHING);
        b.click(&mut app, empty, egui::PointerButton::Secondary);
        b.settle(&mut app);
        let qa = app.quick_add.as_ref().expect("open again").rect;
        let units = app.units.len();
        let away = empty_rack_point(&app, qa);
        b.click(&mut app, away, egui::PointerButton::Primary);
        assert!(app.quick_add.is_none(), "a click outside closes it");
        assert_eq!(app.units.len(), units);

        // Escape closes it too, and stays on the rack.
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_some());
        b.frame(&mut app);
        b.key(&mut app, Key::Escape, Modifiers::NONE);
        assert!(app.quick_add.is_none());
        assert_eq!(app.stage_view, StageView::Rack);
    }

    #[test]
    fn a_right_click_on_a_unit_opens_its_menu_not_quick_add() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);
        let ui = app.units.iter().position(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay))).unwrap();
        let on_unit = (0..40)
            .flat_map(|iy| (0..40).map(move |ix| (ix, iy)))
            .map(|(ix, iy)| {
                let s = app.stage_rect;
                Pos2::new(s.left() + s.width() * ix as f32 / 40.0, s.top() + s.height() * iy as f32 / 40.0)
            })
            .find(|p| matches!(app.hit_test(*p), Hit::Unit(u) if u == ui))
            .expect("a point on the Delay's plate");
        b.click(&mut app, on_unit, egui::PointerButton::Secondary);
        assert!(app.unit_menu.is_some());
        assert!(app.quick_add.is_none(), "a unit keeps its own menu");
        // A right-click on empty space now only dismisses that menu.
        b.frame(&mut app);
        let empty = empty_rack_point(&app, app.unit_menu_rect);
        b.click(&mut app, empty, egui::PointerButton::Secondary);
        assert!(app.unit_menu.is_none());
        assert!(app.quick_add.is_none(), "the click that dismisses a menu opens nothing");
    }

    #[test]
    fn a_locked_rack_says_so_instead_of_opening_quick_add() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);
        app.set_rack_locked(true);
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_none());
        assert!(app.active_patch_notice().is_some_and(|n| n.contains("locked")), "{:?}", app.active_patch_notice());
        let empty = empty_rack_point(&app, Rect::NOTHING);
        b.click(&mut app, empty, egui::PointerButton::Secondary);
        assert!(app.quick_add.is_none());

        // Locked while the picker is open (an agent's set_rack_locked): the
        // pick is refused.
        app.set_rack_locked(false);
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        b.frame(&mut app);
        b.type_text(&mut app, "reverb");
        app.set_rack_locked(true);
        let before = app.units.len();
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert_eq!(app.units.len(), before, "no unit is added to a locked rack");
        assert!(app.plugin_shelf.keys(ShelfView::Recent).is_empty());
    }

    #[test]
    fn enter_opens_nothing_outside_the_rack_view_or_while_a_text_field_has_focus() {
        let mut app = app();
        let mut b = Bench::new(SIZE);
        app.stage_view = StageView::Mixer;
        b.settle(&mut app);
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_none(), "Enter in the Mixer is not quick-add");
        // Leaving the rack closes an open picker.
        app.stage_view = StageView::Rack;
        b.settle(&mut app);
        b.key(&mut app, Key::Enter, Modifiers::NONE);
        assert!(app.quick_add.is_some());
        app.stage_view = StageView::Plugins;
        b.frame(&mut app);
        assert!(app.quick_add.is_none());
        b.frame(&mut app);
        b.frame(&mut app);
        assert!(!typing(&b.ctx), "the picker's field does not keep the keyboard");
    }

    #[test]
    fn quick_add_fits_small_windows_and_keeps_its_text_inside() {
        for size in [Vec2::new(760.0, 520.0), Vec2::new(1000.0, 700.0), SIZE] {
            for query in ["", "re"] {
                let mut app = app();
                app.plugin_shelf.set_favorite(DEXED, true);
                let mut b = Bench::new(size);
                b.settle(&mut app);
                b.key(&mut app, Key::Enter, Modifiers::NONE);
                b.frame(&mut app);
                if !query.is_empty() {
                    b.type_text(&mut app, query);
                }
                let texts = b.settle(&mut app);
                let qa = app.quick_add.as_ref().expect("open").rect;
                let window = Rect::from_min_size(Pos2::ZERO, size);
                assert!(window.contains_rect(qa), "{size:?} '{query}': {qa:?} leaves the window");
                assert!(app.stage_rect.expand(1.0).contains_rect(qa), "{size:?} '{query}': {qa:?} leaves the stage {:?}", app.stage_rect);
                let inside: Vec<&(String, Rect)> = texts
                    .iter()
                    .filter(|(_, r)| r.left() >= qa.left() - 0.5 && r.left() <= qa.right() && qa.contains(r.center()))
                    .collect();
                assert!(!query.is_empty() || inside.iter().any(|(t, _)| t == "Dexed"), "{size:?} '{query}': the favorite is listed");
                for (t, r) in &inside {
                    assert!(
                        r.left() >= qa.left() - 0.5 && r.right() <= qa.right() + 0.5,
                        "{size:?} '{query}': '{t}' {r:?} runs out of the picker {qa:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_agent_stars_a_plugin_and_lists_the_shelves() {
        let mut app = app();
        let out = app.exec_tool("set_plugin_favorite", &serde_json::json!({"plugin": "builtin.delay", "favorite": true}));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}"));
        assert_eq!(v["plugin"], "builtin:builtin.delay", "{v}");
        assert_eq!(v["favorite"], true);
        assert_eq!(v["favorites"], serde_json::json!(["builtin:builtin.delay"]));
        // By name, and while the rack is locked: a favorite changes no patch.
        app.set_rack_locked(true);
        let out = app.exec_tool("set_plugin_favorite", &serde_json::json!({"plugin": "Dexed", "favorite": true}));
        assert!(out.contains(DEXED), "{out}");
        app.set_rack_locked(false);
        assert_eq!(app.plugin_shelf.keys(ShelfView::Favorites), ["builtin:builtin.delay", DEXED]);

        let listed = app.exec_tool("list_plugins", &serde_json::json!({"shelf": "favorites"}));
        assert!(listed.contains("Delay (builtin.delay)") && listed.contains("Dexed"), "{listed}");
        assert!(!listed.contains("builtin.reverb") && !listed.contains("Room Reverb"), "{listed}");

        // What an agent adds from the quick-add path is a recent it can list.
        app.run_browser_action(
            BrowserAction::AddEffect(NodeKind::Dsp(DspKind::Reverb), "Reverb".into()),
            Some("builtin:builtin.reverb".into()),
        );
        let recent = app.exec_tool("list_plugins", &serde_json::json!({"shelf": "recent"}));
        assert!(recent.contains("builtin.reverb") && !recent.contains("builtin.delay"), "{recent}");

        let out = app.exec_tool("set_plugin_favorite", &serde_json::json!({"plugin": "builtin.delay", "favorite": false}));
        assert!(out.contains("\"favorite\":false"), "{out}");
        assert_eq!(app.plugin_shelf.keys(ShelfView::Favorites), [DEXED]);
        // Refused, not guessed (rule 77).
        for bad in [
            serde_json::json!({"plugin": "Nonesuch", "favorite": true}),
            serde_json::json!({"plugin": "builtin.delay"}),
            serde_json::json!({"favorite": true}),
        ] {
            let out = app.exec_tool("set_plugin_favorite", &bad);
            assert!(out.starts_with("error"), "{bad}: {out}");
        }
        assert_eq!(app.plugin_shelf.keys(ShelfView::Favorites), [DEXED], "a refused call changes nothing");
        let bad = app.exec_tool("list_plugins", &serde_json::json!({"shelf": "loved"}));
        assert!(bad.contains("favorites | recent"), "{bad}");
    }

    /// Before/after images of the Plugins view's stars and shelves and of the
    /// picker over the rack, opened and typed into through the real key path.
    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat quick_add_shots -- --ignored"]
    fn quick_add_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let key = |key: Key| {
            vec![
                egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
                egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
            ]
        };
        let shelved = || {
            let mut a = app();
            a.plugin_shelf.set_favorite(DEXED, true);
            a.plugin_shelf.set_favorite("builtin:builtin.delay", true);
            a.plugin_shelf.note_used("builtin:builtin.eq3");
            a.plugin_shelf.note_used("lv2:http://example.org/roomverb");
            a
        };
        for (w, h) in [(1280.0, 800.0), (760.0, 520.0)] {
            let size = Vec2::new(w, h);
            let tag = format!("{w}x{h}");
            // A fresh app per image (rule 148).
            super::super::offline_shots::shot(&mut app(), size, &dir.join(format!("rack-before-{tag}.png")));
            super::super::offline_shots::shot_with(&mut shelved(), size, &dir.join(format!("quick-add-open-{tag}.png")), |_, i| {
                if i == 3 { key(Key::Enter) } else { Vec::new() }
            });
            super::super::offline_shots::shot_with(&mut shelved(), size, &dir.join(format!("quick-add-typed-{tag}.png")), |_, i| match i {
                3 => key(Key::Enter),
                5 => vec![egui::Event::Text("rev".into())],
                _ => Vec::new(),
            });
            let mut plain = app();
            plain.stage_view = StageView::Plugins;
            super::super::offline_shots::shot(&mut plain, size, &dir.join(format!("plugins-before-stars-{tag}.png")));
            let mut starred = shelved();
            starred.stage_view = StageView::Plugins;
            super::super::offline_shots::shot(&mut starred, size, &dir.join(format!("plugins-starred-{tag}.png")));
            let mut favs = shelved();
            favs.stage_view = StageView::Plugins;
            favs.plugin_shelf_view = Some(ShelfView::Favorites);
            super::super::offline_shots::shot(&mut favs, size, &dir.join(format!("plugins-favorites-{tag}.png")));
            let mut recent = shelved();
            recent.stage_view = StageView::Plugins;
            recent.plugin_shelf_view = Some(ShelfView::Recent);
            super::super::offline_shots::shot(&mut recent, size, &dir.join(format!("plugins-recent-{tag}.png")));
        }
    }

    #[test]
    fn page_keys_home_end_and_tab_navigate_without_inserting_tab_chars() {
        let mut app = app();
        for i in 0..20 {
            app.plugins_lv2.push(Lv2PluginInfo {
                uri: format!("http://example.org/fx/{i}"),
                name: format!("Extra FX {i:02}"),
                author: "Example".into(),
                lv2_class: "FilterPlugin".into(),
                num_ports: 4,
                is_instrument: false,
                is_midi_fx: false,
                has_ui: false,
                bundle_path: String::new(),
            });
        }
        let mut b = Bench::new(SIZE);
        b.settle(&mut app);

        b.key(&mut app, Key::Enter, Modifiers::NONE);
        b.frame(&mut app);
        let side_before = app.side;
        assert_eq!(app.quick_add.as_ref().expect("open").sel, 0);

        b.key(&mut app, Key::PageDown, Modifiers::NONE);
        let after_pgdn = app.quick_add.as_ref().unwrap().sel;
        assert!(after_pgdn > 1, "PageDown must advance multiple rows (got {after_pgdn})");

        b.key(&mut app, Key::End, Modifiers::NONE);
        let at_end = app.quick_add.as_ref().unwrap().sel;
        assert!(at_end > after_pgdn, "End must jump to last visible row (got {at_end})");

        b.key(&mut app, Key::PageUp, Modifiers::NONE);
        let after_pgup = app.quick_add.as_ref().unwrap().sel;
        assert!(after_pgup < at_end, "PageUp must move back up (got {after_pgup} vs {at_end})");

        b.key(&mut app, Key::Home, Modifiers::NONE);
        assert_eq!(app.quick_add.as_ref().unwrap().sel, 0);

        // Simulate real Tab + Text("\t"): cycles filter chip, does not flip rack or insert '\t'.
        b.frame_with(
            &mut app,
            vec![
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                egui::Event::Text("\t".into()),
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            Modifiers::NONE,
        );
        b.frame(&mut app);
        let qa = app.quick_add.as_ref().unwrap();
        assert_eq!(qa.view, Some(ShelfView::Favorites), "Tab advances filter to Favorites");
        assert!(qa.query.is_empty(), "Tab text event must not enter query: {:?}", qa.query);
        assert!(!app.flip_requested && app.side == side_before, "Tab inside picker must not flip rack");

        b.frame_with(
            &mut app,
            vec![
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::SHIFT,
                },
                egui::Event::Text("\t".into()),
                egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: Modifiers::SHIFT,
                },
            ],
            Modifiers::SHIFT,
        );
        b.frame(&mut app);
        let qa = app.quick_add.as_ref().unwrap();
        assert_eq!((qa.view, qa.ty), (None, None), "Shift+Tab cycles back to All");
        assert!(qa.query.is_empty(), "Shift+Tab text event must not enter query: {:?}", qa.query);
    }
}
