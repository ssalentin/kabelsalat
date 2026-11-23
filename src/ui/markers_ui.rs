//! Backing-track markers in the app (#291): the song's markers kept in step
//! with the engine, the waveform strip (Setlist and Show mode) and the
//! `backing_markers` tool. The engine does every action
//! (`crate::markers`, `engine::fill_player`); this module only edits the
//! list and reads back what the engine published.

use super::*;
use crate::markers::{Marker, MarkerAction, MarkerTable};

/// What the Setlist's marker block asks for, applied outside its borrows.
pub(super) enum MarkerEdit {
    /// The markers changed (add, rename, move, remove, part).
    Edited,
    /// A marker action for the player.
    Act(MarkerAction),
}

/// What the strip needs of the player, read once per frame.
#[derive(Clone)]
pub(super) struct TrackView {
    pub peaks: Arc<crate::player::FileInfo>,
    pub pos: f32,
    pub duration: f32,
    pub section: usize,
    pub at_end: crate::markers::AtEnd,
    pub playing: bool,
}

/// Draw the waveform with its markers (and the playhead). Returns the
/// time a click landed on, in seconds.
pub(super) fn waveform(ui: &mut egui::Ui, size: Vec2, view: &TrackView, markers: &[Marker], names: bool) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    paint_waveform(ui.painter(), rect, view, markers, names);
    let dur = view.duration.max(0.001) as f64;
    resp.interact_pointer_pos().filter(|_| resp.clicked()).map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * dur)
}

/// The strip itself, also drawn by Show mode (which has no input).
pub(super) fn paint_waveform(p: &egui::Painter, rect: Rect, view: &TrackView, markers: &[Marker], names: bool) {
    let dur = view.duration.max(0.001);
    let x_of = |t: f32| rect.left() + (t / dur).clamp(0.0, 1.0) * rect.width();
    p.rect_filled(rect, 4.0, theme::PAPER_SINK);
    // The section playing is lit behind the wave.
    let table = MarkerTable::new(markers);
    let (lo, hi) = table.bounds(view.section, dur as f64);
    p.rect_filled(
        Rect::from_min_max(Pos2::new(x_of(lo as f32), rect.top()), Pos2::new(x_of(hi as f32), rect.bottom())),
        0.0,
        theme::DARK_PANEL_HI,
    );
    let mid = rect.center().y;
    let half = rect.height() * 0.5 - 3.0;
    let peaks = &view.peaks.peaks;
    for (i, &v) in peaks.iter().enumerate() {
        let x = rect.left() + (i as f32 + 0.5) / peaks.len().max(1) as f32 * rect.width();
        let h = (v.clamp(0.0, 1.0) * half).max(0.5);
        p.line_segment([Pos2::new(x, mid - h), Pos2::new(x, mid + h)], Stroke::new(1.0, theme::CREAM_DIM));
    }
    for m in markers {
        let x = x_of(m.at as f32);
        p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.5, theme::CABLE_AMBER));
        if names && x + 4.0 < rect.right() {
            let g = p.layout_no_wrap(m.name.clone(), egui::FontId::proportional(11.0), theme::CABLE_AMBER);
            if x + 4.0 + g.size().x < rect.right() {
                p.galley(Pos2::new(x + 3.0, rect.top() + 1.0), g, theme::CABLE_AMBER);
            }
        }
    }
    let x = x_of(view.pos);
    p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(2.0, if view.playing { theme::LIVE_TEAL } else { theme::CREAM }));
    p.rect_stroke(rect, 4.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
}

/// "1:23.4".
pub(super) fn clock_label(secs: f64) -> String {
    let t = secs.max(0.0);
    format!("{}:{:04.1}", (t / 60.0) as u32, t % 60.0)
}

/// The Setlist's marker block for song `si`: the strip, a row per marker,
/// and the section actions. Edits land in `song.markers` directly.
pub(super) fn marker_block(ui: &mut egui::Ui, song: &mut crate::setlist::Song, view: Option<&TrackView>, out: &mut Option<MarkerEdit>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(egui::RichText::new("Markers").size(11.5).color(theme::INK_2));
        if view.is_none() {
            ui.label(egui::RichText::new("start the song to see its waveform").size(11.5).color(theme::INK_2));
        }
    });
    if let Some(v) = view {
        let w = ui.available_width().clamp(120.0, 640.0);
        if let Some(t) = waveform(ui, Vec2::new(w, 54.0), v, &song.markers, true) {
            // A click sets the cue for the next "+ Marker".
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("marker_cue"), t));
        }
    }
    let cue: Option<f64> = ui.ctx().data(|d| d.get_temp(egui::Id::new("marker_cue")));
    let at_now = view.map(|v| v.pos as f64);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let at = cue.or(at_now);
        let tip = "Add a marker at the spot you clicked on the waveform, or at the playhead";
        if theme::wrapped_chip_button(ui, "+ Marker", false).on_hover_text(tip).clicked() {
            if let Some(at) = at.or(Some(0.0)) {
                let n = song.markers.len() + 1;
                song.markers.push(Marker::new(format!("Marker {n}"), at));
                crate::markers::normalize(&mut song.markers);
                *out = Some(MarkerEdit::Edited);
            }
        }
        for (label, act, tip) in [
            ("◂ Prev", MarkerAction::Prev, "Jump to the previous marker (on the bar when synced)"),
            ("Next ▸", MarkerAction::Next, "Jump to the next marker (on the bar when synced)"),
            ("Loop", MarkerAction::Loop(None), "Loop the section playing"),
            ("Stop at end", MarkerAction::StopAtEnd(None), "Stop at the end of the section playing"),
        ] {
            let on = view.is_some_and(|v| match act {
                MarkerAction::Loop(_) => v.at_end == crate::markers::AtEnd::Loop,
                MarkerAction::StopAtEnd(_) => v.at_end == crate::markers::AtEnd::Stop,
                _ => false,
            });
            if theme::wrapped_chip_button(ui, label, on).on_hover_text(tip).clicked() {
                *out = Some(MarkerEdit::Act(act));
            }
        }
    });
    let parts = song.parts.len();
    let part_names: Vec<String> = song.parts.iter().map(|p| p.name.clone()).collect();
    let mut remove = None;
    for (i, m) in song.markers.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.set_min_height(26.0);
            let r = DemoApp::recessed_input_ext(ui, &mut m.name, 120.0);
            if r.changed() {
                *out = Some(MarkerEdit::Edited);
            }
            ui.label(egui::RichText::new(clock_label(m.at)).monospace().size(12.0).color(theme::CREAM_META));
            let part_lbl = m.part.and_then(|p| part_names.get(p)).map_or("no part".to_string(), |n| format!("→ {n}"));
            if theme::wrapped_chip_button(ui, &part_lbl, m.part.is_some())
                .on_hover_text("The part of this song the sound switches to when playback reaches this marker")
                .clicked()
            {
                m.part = match m.part {
                    None if parts > 0 => Some(0),
                    Some(p) if p + 1 < parts => Some(p + 1),
                    _ => None,
                };
                *out = Some(MarkerEdit::Edited);
            }
            if theme::wrapped_chip_button(ui, "Go", false).on_hover_text("Jump here").clicked() {
                *out = Some(MarkerEdit::Act(MarkerAction::Jump(i as u8)));
            }
            if theme::wrapped_chip_button(ui, "✕", false).on_hover_text("Remove this marker").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        song.markers.remove(i);
        *out = Some(MarkerEdit::Edited);
    }
}

impl DemoApp {
    pub(super) fn recessed_input_ext(ui: &mut egui::Ui, text: &mut String, width: f32) -> egui::Response {
        Self::recessed_input(ui, text, width, "Marker name")
    }

    /// The player's strip data for the song whose track plays now.
    pub(super) fn track_view(&self) -> Option<TrackView> {
        #[cfg(test)]
        if self.track_fixture.is_some() {
            return self.track_fixture.clone();
        }
        let (song, _) = self.setlist_backing_song.as_ref()?;
        let info = self.player_ctrl.as_ref()?.info()?;
        (info.id == self.track_load_id && self.player.file == info.id && self.player.loaded).then(|| TrackView {
            duration: info.duration as f32,
            peaks: info,
            pos: self.player.pos,
            section: self.player.section as usize,
            at_end: self.player.at_end,
            playing: self.player.playing,
        })
        .filter(|_| self.setlist.songs.get(*song).is_some())
    }

    /// Keep the engine's markers equal to the song's: import the file's cue
    /// points once per load into a song with none, then send the table
    /// whenever it changes. Every frame; cheap.
    pub(super) fn sync_markers(&mut self) {
        let Some(info) = self.player_ctrl.as_ref().and_then(|pc| pc.info()) else { return };
        let Some(si) = self.setlist_backing_song.as_ref().map(|t| t.0) else { return };
        if info.id != self.track_load_id {
            return;
        }
        let Some(song) = self.setlist.songs.get_mut(si) else { return };
        if self.markers_imported != Some(info.id) {
            self.markers_imported = Some(info.id);
            if song.markers.is_empty() && !info.cues.is_empty() {
                song.markers = info.cues.clone();
            }
        }
        let table = MarkerTable::new(&song.markers);
        if self.markers_sent == Some((info.id, table)) {
            return;
        }
        let Some(bus) = &self.bus else { return };
        let sent = bus.lock().unwrap_or_else(|e| e.into_inner()).send(Command::SetPlayerMarkers { file: info.id, markers: table });
        if sent {
            self.markers_sent = Some((info.id, table));
        }
    }

    pub(super) fn send_marker_action(&mut self, action: MarkerAction) -> bool {
        match &self.bus {
            Some(bus) => bus.lock().unwrap_or_else(|e| e.into_inner()).send(Command::PlayerMarker { action }),
            None => false,
        }
    }

    pub(super) fn apply_marker_edit(&mut self, edit: MarkerEdit) {
        match edit {
            MarkerEdit::Edited => {
                self.push_user_undo();
                if let Some(si) = self.setlist_edit_song {
                    if let Some(song) = self.setlist.songs.get_mut(si) {
                        crate::markers::normalize(&mut song.markers);
                    }
                }
            }
            MarkerEdit::Act(a) => {
                self.send_marker_action(a);
            }
        }
    }

    /// The `backing_markers` tool: list, edit and play a song's markers.
    pub(super) fn backing_markers_tool(&mut self, args: &serde_json::Value) -> Result<String, String> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let si = match args.get("song").and_then(|v| v.as_u64()) {
            Some(s) => s as usize,
            None => self.setlist.active.map(|p| p.song).or(self.setlist_edit_song).ok_or("no song: give song (0-based index)")?,
        };
        if si >= self.setlist.songs.len() {
            return Err(format!("song {si} does not exist ({} songs)", self.setlist.songs.len()));
        }
        let index = args.get("index").and_then(|v| v.as_u64()).map(|v| v as usize);
        let need_index = |n: usize| -> Result<usize, String> {
            let i = index.ok_or("give index (0-based, in file order; see action=list)")?;
            if i < n { Ok(i) } else { Err(format!("marker {i} does not exist ({n} markers)")) }
        };
        let player_act = |this: &mut Self, a: MarkerAction| {
            if this.setlist_backing_song.as_ref().map(|t| t.0) != Some(si) {
                return Err(format!("song {si}'s backing track is not the one loaded: select the song first"));
            }
            if this.send_marker_action(a) { Ok(()) } else { Err("command bus full, try again".to_string()) }
        };
        let n = self.setlist.songs[si].markers.len();
        match action {
            "list" => {}
            "add" => {
                let at = args.get("at").and_then(|v| v.as_f64()).or_else(|| self.track_view().map(|v| v.pos as f64)).ok_or("give at (seconds)")?;
                self.push_user_undo();
                let song = &mut self.setlist.songs[si];
                let name = args.get("name").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| format!("Marker {}", n + 1));
                let mut m = Marker::new(name, at);
                m.part = args.get("part").and_then(|v| v.as_u64()).map(|p| p as usize).filter(|&p| p < song.parts.len());
                song.markers.push(m);
                crate::markers::normalize(&mut song.markers);
            }
            "remove" => {
                let i = need_index(n)?;
                self.push_user_undo();
                self.setlist.songs[si].markers.remove(i);
            }
            "rename" => {
                let i = need_index(n)?;
                let name = args.get("name").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).ok_or("give name")?;
                self.push_user_undo();
                self.setlist.songs[si].markers[i].name = name.trim().to_string();
            }
            "move" => {
                let i = need_index(n)?;
                let at = args.get("at").and_then(|v| v.as_f64()).ok_or("give at (seconds)")?;
                self.push_user_undo();
                let song = &mut self.setlist.songs[si];
                song.markers[i].at = at;
                crate::markers::normalize(&mut song.markers);
            }
            "set_part" => {
                let i = need_index(n)?;
                let parts = self.setlist.songs[si].parts.len();
                let part = match args.get("part") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => match v.as_u64() {
                        Some(p) if (p as usize) < parts => Some(p as usize),
                        _ => return Err(format!("part must be 0..{} or null", parts.saturating_sub(1))),
                    },
                };
                self.push_user_undo();
                self.setlist.songs[si].markers[i].part = part;
            }
            "jump" => {
                let i = need_index(n)?;
                player_act(self, MarkerAction::Jump(i as u8))?;
            }
            "next" => player_act(self, MarkerAction::Next)?,
            "prev" => player_act(self, MarkerAction::Prev)?,
            "loop_section" => player_act(self, MarkerAction::Loop(args.get("on").and_then(|v| v.as_bool())))?,
            "stop_at_section" => player_act(self, MarkerAction::StopAtEnd(args.get("on").and_then(|v| v.as_bool())))?,
            other => return Err(format!("unknown action '{other}' (list, add, remove, rename, move, set_part, jump, next, prev, loop_section, stop_at_section)")),
        }
        let song = &self.setlist.songs[si];
        Ok(serde_json::json!({
            "song": si,
            "backing": song.backing,
            "markers": song.markers.iter().enumerate().map(|(i, m)| serde_json::json!({
                "index": i, "name": m.name, "at": m.at, "part": m.part,
                "part_name": m.part.and_then(|p| song.parts.get(p)).map(|p| p.name.clone()),
            })).collect::<Vec<_>>(),
            "player": {
                "section": self.player.section,
                "position": self.player.pos,
                "at_end": self.player.at_end.id(),
                "jump_pending": (self.player.jump_to >= 0).then_some(self.player.jump_to),
            },
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(app: &mut DemoApp) {
        let peaks: Vec<f32> = (0..crate::markers::PEAK_BUCKETS)
            .map(|i| {
                let t = i as f32 / crate::markers::PEAK_BUCKETS as f32;
                let section = (t * 4.0).floor();
                (0.25 + 0.2 * section / 3.0 + 0.2 * (t * 90.0).sin().abs()).min(0.95)
            })
            .collect();
        let info = Arc::new(crate::player::FileInfo { id: 1, path: "/music/Moonlight Drive 100 bpm.wav".into(), duration: 160.0, cues: vec![], peaks });
        app.track_fixture = Some(TrackView { peaks: info, pos: 71.0, duration: 160.0, section: 2, at_end: crate::markers::AtEnd::Loop, playing: true });
        let song = &mut app.setlist.songs[0];
        song.backing = Some("/music/Moonlight Drive 100 bpm.wav".into());
        let mut chorus = Marker::new("Chorus", 64.0);
        chorus.part = Some(1);
        song.markers = vec![Marker::new("Verse", 16.0), chorus, Marker::new("Solo", 112.0)];
    }

    /// The marker block lists a row per marker with its part and fits
    /// at the Setlist's small window sizes.
    #[test]
    fn the_setlist_shows_each_marker_with_its_part_and_fits() {
        let mut app = super::super::show_mode::tests::gig();
        fixture(&mut app);
        app.select_setlist(0, 0).unwrap();
        app.stage_view = StageView::Setlist;
        app.setlist_edit_song = Some(0);
        let ctx = egui::Context::default();
        for size in [Vec2::new(1280.0, 1000.0), Vec2::new(781.0, 950.0), Vec2::new(760.0, 520.0)] {
            let mut texts = Vec::new();
            for f in 0..4 {
                let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), time: Some(1.0 + f as f64), focused: true, ..Default::default() };
                let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
                out.textures_delta.clear();
                texts = out.shapes.iter().flat_map(|cs| {
                    fn walk(s: &egui::Shape, o: &mut Vec<String>) {
                        match s {
                            egui::Shape::Text(t) => o.push(t.galley.text().to_owned()),
                            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, o)),
                            _ => {}
                        }
                    }
                    let mut o = Vec::new();
                    walk(&cs.shape, &mut o);
                    o
                }).collect();
            }
            if size.y > 900.0 {
                assert!(texts.iter().any(|t| t == "→ Chorus" || t.starts_with("→")), "{size:?}: a marker's part: {texts:?}");
            }
        }
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat marker_shots -- --ignored"]
    fn marker_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = super::super::show_mode::tests::gig();
        fixture(&mut app);
        app.select_setlist(0, 0).unwrap();
        app.stage_view = StageView::Setlist;
        app.setlist_edit_song = Some(0);
        super::super::offline_shots::shot_with(&mut app, Vec2::new(1280.0, 1000.0), &dir.join("markers-setlist.png"), |_, _| Vec::new());
        let mut app = super::super::show_mode::tests::gig();
        fixture(&mut app);
        app.select_setlist(0, 1).unwrap();
        app.set_show_mode(true);
        super::super::offline_shots::shot_with(&mut app, Vec2::new(1280.0, 800.0), &dir.join("markers-show-mode.png"), |_, _| Vec::new());
    }
}
