//! Show mode (#210): the locked stage screen. From two metres away it says
//! which song and part this is, what comes next and whether sound goes out,
//! and Panic silences every rig. While it is open `frame_ui` runs
//! [`DemoApp::show_frame`] in place of the editing frame, so no key, click or
//! dropped file reaches the patch, the mix or the setlist. Its own keyboard
//! controls are `←` / `→` and `PageUp` / `PageDown` for parts (`Shift` for
//! songs), `↑` / `↓` to scroll song notes, `Space` for master transport /
//! click, `Shift+T` for tap tempo, `Ctrl+T` for the tuner (short `Esc` closes
//! it), the Panic button, and `Esc` held until its line fills and then let go
//! to leave. Hardware MIDI, the engine and agents keep running: a foot switch
//! still steps the part, and the screen follows.

use super::*;

/// How long Esc is held before letting go of it leaves.
pub(super) const LEAVE_HOLD_SECS: f32 = 1.0;
/// The floors of the master meter (`meter_db_pos`) and of a rig meter
/// (`edge_display_level`), in dBFS.
const MASTER_FLOOR_DB: f32 = -60.0;
const RIG_FLOOR_DB: f32 = -48.0;
/// How long the screen says "All notes off" after a Panic.
const PANIC_NOTE_SECS: f32 = 2.0;
/// One meter row: a rig or the master.
const METER_ROW_H: f32 = 44.0;
const METER_GAP: f32 = 8.0;

#[derive(Default)]
pub(super) struct ShowMode {
    /// `ui_time` Esc went down at.
    esc_since: Option<f32>,
    /// `ui_time` of the last Panic.
    panic_at: Option<f32>,
    /// Where Panic was drawn last.
    pub(super) panic_rect: Option<Rect>,
    /// Vertical scroll offset (points) for the active song's notes/chart.
    notes_scroll_y: f32,
    /// Song index that `notes_scroll_y` belongs to (resets on song switch).
    notes_song: Option<usize>,
}

/// What the screen shows, read from the app once a frame.
struct Board {
    song_idx: Option<usize>,
    caption: String,
    song: String,
    part: Option<String>,
    next_part: Option<String>,
    next: Option<String>,
    tempo: String,
    notes: Option<String>,
    loading: bool,
    indicators: Vec<(&'static str, Color32)>,
    /// Live rigs: name and level (`rig_levels`, −48…0 dBFS).
    rigs: Vec<(String, f32)>,
    master: f32,
    master_peak: f32,
}

/// The hero's text laid out at one scale, and the height it takes.
struct HeroBlock {
    inner: Rect,
    height: f32,
    gap: f32,
    caption: theme::TrackedLabel,
    song: Arc<egui::Galley>,
    part: Option<Arc<egui::Galley>>,
    tempo: Arc<egui::Galley>,
    /// The tempo goes under the part: both do not fit on one line.
    stacked: bool,
}

/// The screen's bands, top to bottom.
struct Regions {
    top: Rect,
    hero: Rect,
    next: Rect,
    meters: Rect,
    bottom: Rect,
}

fn regions(screen: Rect, meter_rows: usize) -> Regions {
    let margin = if screen.width() < 900.0 { 16.0 } else { 24.0 };
    let gap = 16.0;
    let r = screen.shrink(margin);
    let (top_h, next_h, bottom_h) = (48.0, 64.0, 80.0);
    let meters_h = meter_rows as f32 * (METER_ROW_H + METER_GAP) - METER_GAP;
    let top = Rect::from_min_size(r.min, Vec2::new(r.width(), top_h));
    let bottom = Rect::from_min_max(Pos2::new(r.left(), r.bottom() - bottom_h), r.max);
    let meters = Rect::from_min_max(Pos2::new(r.left(), bottom.top() - gap - meters_h), Pos2::new(r.right(), bottom.top() - gap));
    let next = Rect::from_min_max(Pos2::new(r.left(), meters.top() - gap - next_h), Pos2::new(r.right(), meters.top() - gap));
    let hero = Rect::from_min_max(Pos2::new(r.left(), top.bottom() + gap), Pos2::new(r.right(), next.top() - gap));
    Regions { top, hero, next, meters, bottom }
}

/// The local wall-clock time, "21:47" (rule 54).
fn clock_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let day = (secs + crate::record::local_utc_offset(secs)).rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, day % 3600 / 60)
}

/// `text` in the medium face, as large as fits `width` between `min` and
/// `max` points (in 0.5 pt steps, rule 17), truncated below `min`.
fn fitted(p: &egui::Painter, text: &str, max: f32, min: f32, width: f32, color: Color32) -> Arc<egui::Galley> {
    let natural = p.layout_no_wrap(text.to_owned(), theme::medium_font(max), color).size().x;
    let size = if natural <= width { max } else { ((max * width / natural) * 2.0).floor() / 2.0 }.max(min);
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(theme::medium_font(size), color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    p.layout_job(job)
}

/// Tracked upper-case micro-caps (§2.2).
fn tracked(p: &egui::Painter, text: &str, size: f32) -> theme::TrackedLabel {
    p.fonts_mut(|f| theme::layout_tracked(f, &text.to_uppercase(), FontId::proportional(size), 0.14))
}

/// [`tracked`] no wider than `width`: smaller down to `min` points, then cut
/// short with an ellipsis.
fn tracked_fit(p: &egui::Painter, text: &str, max: f32, min: f32, width: f32) -> theme::TrackedLabel {
    let label = tracked(p, text, max);
    if label.size.x <= width {
        return label;
    }
    let size = (((max * width / label.size.x) * 2.0).floor() / 2.0).max(min);
    let mut chars: Vec<char> = text.chars().collect();
    loop {
        let shown: String = if chars.len() < text.chars().count() { chars.iter().collect::<String>() + "…" } else { text.to_owned() };
        let label = tracked(p, &shown, size);
        if label.size.x <= width || chars.is_empty() {
            return label;
        }
        chars.pop();
    }
}

/// [`tracked`] painted at `pos`; returns its size.
fn caption(p: &egui::Painter, pos: Pos2, text: &str, size: f32, color: Color32) -> Vec2 {
    let label = tracked(p, text, size);
    theme::paint_tracked(p, pos, &label, color);
    label.size
}

/// A horizontal stepped meter (flat segments, cream → tomato → clip) from
/// `floor_db` to 0 dBFS; `pos` and `peak` are positions on that scale.
fn paint_bar(p: &egui::Painter, r: Rect, pos: f32, peak: Option<f32>, floor_db: f32) {
    let segs = ((r.width() / 12.0) as usize).max(8);
    let seg_w = r.width() / segs as f32;
    let lit = lit_segments(pos, segs);
    let peak_i = peak.map(|v| ((v.clamp(0.0, 1.0) * segs as f32).ceil() as usize).min(segs));
    for i in 0..segs {
        let sr = Rect::from_min_size(Pos2::new(r.left() + i as f32 * seg_w + 1.0, r.top()), Vec2::new(seg_w - 2.0, r.height()));
        let db = floor_db * (1.0 - (i as f32 + 0.5) / segs as f32);
        let on = i < lit || peak_i.is_some_and(|k| k > 0 && i + 1 == k);
        p.rect_filled(sr, 1.0, if on { theme::meter_color((db - MASTER_FLOOR_DB) / -MASTER_FLOOR_DB) } else { theme::METER_TRACK });
    }
}

/// The hero's text at `scale`.
fn hero_block(p: &egui::Painter, board: &Board, hero: Rect, scale: f32) -> HeroBlock {
    let q = |v: f32| (v * scale * 2.0).round() / 2.0;
    let pad = q(36.0);
    let inner = hero.shrink2(Vec2::new(pad, pad * 0.8));
    let gap = q(10.0);
    let caption = tracked_fit(p, &board.caption, q(20.0), q(12.0).min(12.0), inner.width());
    let song = fitted(p, &board.song, q(112.0), q(44.0), inner.width(), theme::CREAM);
    let tempo = p.layout_no_wrap(board.tempo.clone(), theme::medium_font(q(44.0)), theme::CREAM_META);
    let beside = inner.width() - tempo.size().x - q(48.0);
    let stacked = board.part.as_ref().is_some_and(|t| p.layout_no_wrap(t.clone(), theme::medium_font(q(64.0)), theme::LIVE_TEAL).size().x > beside);
    let part = board.part.as_ref().map(|t| fitted(p, t, q(64.0), q(32.0), if stacked { inner.width() } else { beside }, theme::LIVE_TEAL));
    let lines = match (&part, stacked) {
        (Some(g), false) => g.size().y,
        (Some(g), true) => g.size().y + tempo.size().y,
        (None, _) => tempo.size().y,
    };
    let height = caption.size.y + gap + song.size().y + gap + lines;
    HeroBlock { inner, height, gap, caption, song, part, tempo, stacked }
}

impl DemoApp {
    /// Open or close Show mode. Opening lets go of what a key or the pointer
    /// holds: its release would never reach the editing frame.
    pub(super) fn set_show_mode(&mut self, on: bool) {
        if on == self.show_mode.is_some() {
            return;
        }
        if on {
            self.release_kb_drag_note();
            self.current_source = SourceId::Keys;
            for (_, semi) in std::mem::take(&mut self.held_keys) {
                self.fire_keys(semi, 0.0);
            }
            self.drag = Drag::None;
            for c in &mut self.cables {
                (c.over_a, c.over_b) = (None, None);
            }
            self.press = None;
            self.pending = None;
            self.hover = None;
            self.drag_cursor = None;
            self.drop_ghost = None;
            self.send_drag = None;
            self.mixer_knob_drag = None;
            self.mixer_strip_drag = None;
            self.cc_learn = None;
            self.show_learn = None;
            self.show_mode = Some(ShowMode::default());
        } else {
            self.show_mode = None;
        }
    }

    /// All notes off on every rig: the engine releases them, the keys go dark.
    pub(super) fn panic(&mut self) {
        if let Some(bus) = &self.bus {
            bus.lock().unwrap_or_else(|e| e.into_inner()).send(Command::Panic);
        }
        self.panicked();
    }

    /// What the app shows of a Panic the engine did: the keys go dark and
    /// Show mode says so.
    pub(super) fn panicked(&mut self) {
        self.release_kb_drag_note();
        self.held_keys.clear();
        self.notes_down = [0; 128];
        self.note_offs.clear();
        self.kb_sustain = false;
        self.kb_sustained = [false; 128];
        let now = self.ui_time;
        if let Some(show) = &mut self.show_mode {
            show.panic_at = Some(now);
        }
    }

    /// The frame while Show mode is open: the engine, agents and hardware are
    /// served as in the editing frame, and input reaches only this screen.
    pub(super) fn show_frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.flip_requested = false;
        self.frame_feeds(&ctx);
        let dt = ctx.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0);
        self.sync_main_knobs();
        self.sync_midi_knobs();
        self.sync_aeolus_faces();
        // Shift variants are checked before NONE (rule 143), and plain/command
        // controls reject extra Alt/Shift modifiers so stray chords stay locked.
        let (step, toggle_play, tap_tempo, toggle_tuner, scroll_delta) = ctx.input_mut(|i| {
            if i.modifiers.alt {
                return (None, false, false, false, 0.0);
            }
            let step = [
                (Modifiers::SHIFT, Key::PageUp, "prev_song"),
                (Modifiers::SHIFT, Key::PageDown, "next_song"),
                (Modifiers::SHIFT, Key::ArrowLeft, "prev_song"),
                (Modifiers::SHIFT, Key::ArrowRight, "next_song"),
                (Modifiers::NONE, Key::PageUp, "prev_part"),
                (Modifiers::NONE, Key::PageDown, "next_part"),
                (Modifiers::NONE, Key::ArrowLeft, "prev_part"),
                (Modifiers::NONE, Key::ArrowRight, "next_part"),
            ]
            .into_iter()
            .find(|&(m, k, _)| i.consume_key(m, k))
            .map(|(_, _, a)| a);
            let play = !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Space);
            let tap = i.consume_key(Modifiers::SHIFT, Key::T);
            let tuner = !i.modifiers.shift && i.consume_key(Modifiers::COMMAND, Key::T);
            let scroll = if !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                72.0
            } else if !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                -72.0
            } else {
                0.0
            };
            (step, play, tap, tuner, scroll)
        });
        if let Some(action) = step {
            let _ = self.step_setlist(action);
        }
        if toggle_play {
            self.toggle_play();
        }
        if tap_tempo {
            self.ui_transport_grace = self.ui_time + 0.6;
            self.tap_tempo_at(self.tap_origin.elapsed().as_secs_f64());
        }
        if toggle_tuner {
            self.tuner_on = !self.tuner_on;
            if !self.tuner_on {
                self.tuner_rect = Rect::NOTHING;
            }
        }
        if scroll_delta != 0.0 {
            if let Some(show) = &mut self.show_mode {
                show.notes_scroll_y = (show.notes_scroll_y + scroll_delta).max(0.0);
            }
        }
        self.update(dt);
        self.publish_patch();
        self.publish_pins();
        self.show_paint(ui);
        self.tuner_panel(&ctx);
        // Leaving waits for the release: an Esc still held, and its key
        // repeats, would otherwise reach the editing frame and close the
        // view Show mode was opened from. A short Esc press closes an open
        // tuner without leaving Show mode.
        let (esc, esc_released) = ctx.input(|i| (i.key_down(Key::Escape), i.key_released(Key::Escape)));
        let now = self.ui_time;
        let mut close_tuner = false;
        let leave = self.show_mode.as_mut().is_some_and(|show| {
            let was_down = show.esc_since.is_some();
            let held_out = show.esc_since.is_some_and(|t| now - t >= LEAVE_HOLD_SECS);
            show.esc_since = if esc { Some(show.esc_since.unwrap_or(now)) } else { None };
            if !esc && (was_down || esc_released) && !held_out && self.tuner_on {
                close_tuner = true;
            }
            !esc && held_out
        });
        if close_tuner {
            self.tuner_on = false;
            self.tuner_rect = Rect::NOTHING;
        }
        if leave {
            self.set_show_mode(false);
        }
        self.frame_end(&ctx);
    }

    fn board(&self) -> Board {
        let sl = &self.setlist;
        let loading = self.active_song_loading_notice().is_some();
        let (caption, song, part) = match sl.current() {
            Some((s, p, pos)) => {
                let mut caption = format!("Song {} of {} · part {} of {}", pos.song + 1, sl.songs.len(), pos.part + 1, s.parts.len());
                if let Some(set) = sl.set_of(pos.song) {
                    caption = format!("{caption} · {}", set.name);
                }
                if loading {
                    caption = format!("Loading… · {caption}");
                }
                (caption, s.name.clone(), Some(p.name.clone()))
            }
            None if sl.songs.is_empty() => ("No setlist".into(), "Every rig plays".into(), None),
            None => ("Setlist not started · → or PageDown starts".into(), "Every rig plays".into(), None),
        };
        let rigs = (0..self.rig_count())
            .filter(|&r| self.rig_zones.get(r).is_none_or(|z| z.live))
            .map(|r| (self.rig_name(r), self.rig_levels.get(r).copied().unwrap_or(0.0)))
            .collect();
        let next_part = sl.current().and_then(|(s, _, pos)| s.parts.get(pos.part + 1).map(|p| p.name.clone()));
        let mut indicators = Vec::new();
        if self.recorder.as_ref().is_some_and(|r| r.is_recording()) {
            indicators.push(("● REC", theme::CLIP_BRIGHT));
        }
        if self.player.playing || self.player.pending {
            indicators.push(("♫ TRACK", theme::LIVE_TEAL));
        }
        if self.playing {
            indicators.push(("CLICK", theme::CREAM_META));
        }
        if self.recovery.has_notice() {
            indicators.push(("Restarted after crash", theme::ACCENT_BRIGHT));
        }
        indicators.extend(self.hotplug_indicators());
        let (song_idx, notes) = sl.current().map_or((None, None), |(s, _, pos)| {
            (Some(pos.song), (!s.notes.trim().is_empty()).then(|| s.notes.clone()))
        });
        Board {
            song_idx,
            caption,
            song,
            part,
            next_part,
            next: sl.upcoming_song().map(|i| sl.songs[i].name.clone()),
            tempo: format!("{} BPM · {}", self.bpm.round() as u32, self.time_sig),
            notes,
            loading,
            indicators,
            rigs,
            master: meter_db_pos(self.meter),
            master_peak: meter_db_pos(self.meter_peak),
        }
    }

    fn show_paint(&mut self, ui: &mut egui::Ui) {
        let screen = ui.ctx().content_rect();
        let p = ui.painter().clone();
        let board = self.board();
        theme::paper_bg(&p, screen);

        let cols = ((screen.width() - 48.0) / 440.0).floor().clamp(1.0, 3.0) as usize;
        let fit_rows = ((screen.height() - 520.0) / (METER_ROW_H + METER_GAP)).floor().max(1.0) as usize;
        let meters: Vec<(String, f32, Option<f32>)> = std::iter::once(("Master".to_string(), board.master, Some(board.master_peak)))
            .chain(board.rigs.iter().map(|(n, l)| (n.clone(), *l, None)))
            .take(cols * fit_rows)
            .collect();
        let mut reg = regions(screen, meters.len().div_ceil(cols));
        // The backing track's waveform with its markers (#291): a strip
        // under the hero while the song's track plays.
        let track = self.track_view().filter(|_| reg.hero.height() > 260.0);
        let track_strip = track.as_ref().map(|_| {
            let strip = Rect::from_min_max(Pos2::new(reg.hero.left(), reg.hero.bottom() - 64.0), reg.hero.max);
            reg.hero.max.y -= 64.0 + 12.0;
            strip
        });

        // Top: what this screen is, live status indicators, and the clock.
        let top_cap = caption(&p, reg.top.left_center() - Vec2::new(0.0, 9.0), "Show mode · editing locked", 18.0, theme::INK_2);
        let clock = p.layout_no_wrap(clock_now(), theme::medium_font(40.0), theme::INK);
        let clock_x = reg.top.right() - clock.size().x;
        p.galley(Pos2::new(clock_x, reg.top.center().y - clock.size().y / 2.0), clock, theme::INK);
        if !board.indicators.is_empty() {
            let mut ix = reg.top.left() + top_cap.x + 18.0;
            for &(text, col) in &board.indicators {
                let g = p.layout_no_wrap(text.into(), theme::medium_font(14.0), col);
                let pw = g.size().x + 16.0;
                if ix + pw > clock_x - 16.0 {
                    break;
                }
                let pill = Rect::from_min_size(Pos2::new(ix, reg.top.center().y - 13.0), Vec2::new(pw, 26.0));
                p.rect_filled(pill, 4.0, theme::DARK_PANEL);
                p.galley(pill.center() - g.size() / 2.0, g, col);
                ix = pill.right() + 8.0;
            }
        }

        if let (Some(view), Some(strip)) = (&track, track_strip) {
            let markers = self.setlist.current().map(|(s, _, _)| s.markers.clone()).unwrap_or_default();
            super::markers_ui::paint_waveform(&p, strip, view, &markers, true);
            let (name, mode) = {
                let sec = view.section.checked_sub(1).and_then(|i| markers.get(i)).map(|m| m.name.clone());
                (sec.unwrap_or_else(|| "Start".into()), view.at_end)
            };
            let tag = match mode {
                crate::markers::AtEnd::Loop => format!("{name} · LOOP"),
                crate::markers::AtEnd::Stop => format!("{name} · STOPS AT END"),
                crate::markers::AtEnd::Continue => name,
            };
            let g = p.layout_no_wrap(tag, theme::medium_font(14.0), theme::CREAM);
            let r = Rect::from_min_size(Pos2::new(strip.right() - g.size().x - 14.0, strip.bottom() - g.size().y - 8.0), g.size() + Vec2::new(10.0, 4.0));
            p.rect_filled(r, 4.0, theme::DARK_PANEL);
            p.galley(r.min + Vec2::new(5.0, 2.0), g, theme::CREAM);
        }

        // The hero: song, part and tempo, as large as the panel allows,
        // centred in it. When the active song has notes/chords, share the
        // hero band with a scrollable chart card without crowding the title.
        let (hero, notes_rect) = if board.notes.is_some() {
            if reg.hero.width() >= 760.0 {
                let split_gap = 16.0;
                let hero_w = ((reg.hero.width() - split_gap) * 0.56).round();
                let h = Rect::from_min_size(reg.hero.min, Vec2::new(hero_w, reg.hero.height()));
                let n = Rect::from_min_max(Pos2::new(h.right() + split_gap, reg.hero.top()), reg.hero.max);
                (h, Some(n))
            } else if reg.hero.height() >= 280.0 {
                let split_gap = 12.0;
                let hero_h = ((reg.hero.height() - split_gap) * 0.48).round();
                let h = Rect::from_min_size(reg.hero.min, Vec2::new(reg.hero.width(), hero_h));
                let n = Rect::from_min_max(Pos2::new(reg.hero.left(), h.bottom() + split_gap), reg.hero.max);
                (h, Some(n))
            } else {
                (reg.hero, None)
            }
        } else {
            (reg.hero, None)
        };
        let hero_radius = if notes_rect.is_some() { 24.0 } else { 36.0 };
        p.rect_filled(hero, hero_radius, theme::DARK_PANEL);
        let mut scale = (hero.height() / 330.0).clamp(0.5, 1.4);
        let mut block = hero_block(&p, &board, hero, scale);
        if block.height > block.inner.height() {
            scale *= (block.inner.height() / block.height).clamp(0.5, 1.0);
            block = hero_block(&p, &board, hero, scale);
        }
        let HeroBlock { inner, height, gap, caption: cap, song, part, tempo, stacked } = block;
        let top = (hero.center().y - height / 2.0).max(inner.top());
        let cap_col = if board.loading { theme::CABLE_AMBER } else { theme::CREAM_DIM };
        theme::paint_tracked(&p, Pos2::new(inner.left(), top), &cap, cap_col);
        let song_top = top + cap.size.y + gap;
        let line_top = song_top + song.size().y + gap;
        p.galley(Pos2::new(inner.left(), song_top), song, theme::CREAM);
        let tempo_pos = match part {
            Some(part) => {
                let part_h = part.size().y;
                p.galley(Pos2::new(inner.left(), line_top), part, theme::LIVE_TEAL);
                if stacked {
                    Pos2::new(inner.left(), line_top + part_h)
                } else {
                    Pos2::new(inner.right() - tempo.size().x, line_top + part_h - tempo.size().y)
                }
            }
            None => Pos2::new(inner.left(), line_top),
        };
        p.galley(tempo_pos, tempo, theme::CREAM_META);

        if let Some(show) = &mut self.show_mode {
            if show.notes_song != board.song_idx || board.notes.is_none() {
                show.notes_song = board.song_idx;
                show.notes_scroll_y = 0.0;
            }
        }
        if let (Some(notes), Some(nrect)) = (&board.notes, notes_rect) {
            let scroll_y = self.show_mode.as_ref().map_or(0.0, |s| s.notes_scroll_y);
            p.rect_filled(nrect, 24.0, theme::DARK_PANEL);
            p.rect_stroke(nrect, 24.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
            let pad = if nrect.width() < 400.0 || nrect.height() < 180.0 { 16.0 } else { 24.0 };
            let ninner = nrect.shrink(pad);
            let hdr_sz = caption(&p, ninner.left_top(), "Song notes", 14.0, theme::CREAM_DIM);
            let body = Rect::from_min_max(Pos2::new(ninner.left(), ninner.top() + hdr_sz.y + 10.0), ninner.max);
            if body.width() > 40.0 && body.height() > 24.0 {
                let lines = crate::setlist::parse_chart(notes);
                let mut new_scroll_y = scroll_y;
                ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
                    ui.shrink_clip_rect(body);
                    let out = egui::ScrollArea::vertical()
                        .id_salt("show_mode_notes_scroll")
                        .vertical_scroll_offset(scroll_y)
                        .max_height(body.height())
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            for line in &lines {
                                match line {
                                    crate::setlist::ChartLine::Blank => {
                                        ui.add_space(8.0);
                                    }
                                    crate::setlist::ChartLine::Directive { kind, text } => match kind {
                                        crate::setlist::ChartDirective::Title => {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(text).font(theme::medium_font(20.0)).color(theme::CREAM),
                                                )
                                                .wrap(),
                                            );
                                        }
                                        crate::setlist::ChartDirective::Subtitle => {
                                            ui.add(
                                                egui::Label::new(egui::RichText::new(text).size(15.0).color(theme::CREAM_META)).wrap(),
                                            );
                                        }
                                        crate::setlist::ChartDirective::Section => {
                                            ui.add_space(6.0);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(text).font(theme::medium_font(15.0)).color(theme::LIVE_TEAL),
                                                )
                                                .wrap(),
                                            );
                                        }
                                    },
                                    crate::setlist::ChartLine::Line(segs) => {
                                        let mut job = egui::text::LayoutJob::default();
                                        let mut prev_chord = false;
                                        for seg in segs {
                                            match seg {
                                                crate::setlist::ChartSegment::Chord(c) => {
                                                    if prev_chord {
                                                        job.append(" ", 0.0, egui::TextFormat::simple(FontId::proportional(18.0), theme::CREAM));
                                                    }
                                                    job.append(
                                                        &format!("[{c}]"),
                                                        0.0,
                                                        egui::TextFormat::simple(theme::medium_font(18.0), theme::CABLE_AMBER),
                                                    );
                                                    prev_chord = true;
                                                }
                                                crate::setlist::ChartSegment::Text(t) => {
                                                    job.append(t, 0.0, egui::TextFormat::simple(FontId::proportional(18.0), theme::CREAM));
                                                    prev_chord = false;
                                                }
                                            }
                                        }
                                        ui.add(egui::Label::new(job).wrap());
                                    }
                                }
                            }
                        });
                    new_scroll_y = out.state.clamped_offset().y;
                });
                if let Some(show) = &mut self.show_mode {
                    show.notes_scroll_y = new_scroll_y;
                }
            }
        }

        // Next part / next song.
        let next = reg.next;
        match (&board.next_part, &board.next) {
            (Some(np), Some(ns)) => {
                let split_x = next.left() + (next.width() * 0.44).round();
                let l_cap = caption(&p, Pos2::new(next.left(), next.center().y - 9.0), "Next part", 16.0, theme::INK_2);
                let np_x = next.left() + l_cap.x + 12.0;
                let np_w = (split_x - 16.0 - np_x).max(40.0);
                let np_g = fitted(&p, np, 38.0, 20.0, np_w, theme::LIVE_TEAL);
                p.galley(Pos2::new(np_x, next.center().y - np_g.size().y / 2.0), np_g, theme::LIVE_TEAL);

                let r_x = split_x + 8.0;
                let r_cap = caption(&p, Pos2::new(r_x, next.center().y - 9.0), "Next song", 16.0, theme::INK_2);
                let ns_x = r_x + r_cap.x + 12.0;
                let ns_w = (next.right() - ns_x).max(40.0);
                let ns_g = fitted(&p, ns, 38.0, 20.0, ns_w, theme::INK);
                p.galley(Pos2::new(ns_x, next.center().y - ns_g.size().y / 2.0), ns_g, theme::INK);
            }
            (Some(np), None) => {
                let label = caption(&p, Pos2::new(next.left(), next.center().y - 11.0), "Next part", 20.0, theme::INK_2);
                let name_x = next.left() + label.x + 20.0;
                let name = fitted(&p, np, 44.0, 24.0, (next.right() - name_x).max(40.0), theme::LIVE_TEAL);
                p.galley(Pos2::new(name_x, next.center().y - name.size().y / 2.0), name, theme::LIVE_TEAL);
            }
            (None, _) => {
                let label = caption(&p, Pos2::new(next.left(), next.center().y - 11.0), "Next", 20.0, theme::INK_2);
                let name_x = next.left() + label.x + 20.0;
                let (text, color) = match &board.next {
                    Some(n) => (n.as_str(), theme::INK),
                    None => ("End of the setlist", theme::INK_2),
                };
                let name = fitted(&p, text, 44.0, 24.0, next.right() - name_x, color);
                p.galley(Pos2::new(name_x, next.center().y - name.size().y / 2.0), name, color);
            }
        }

        // Meters: the master, then every rig that plays in this part.
        let col_w = (reg.meters.width() - (cols - 1) as f32 * 24.0) / cols as f32;
        for (i, (name, pos, peak)) in meters.iter().enumerate() {
            let (row, col) = (i / cols, i % cols);
            let cell = Rect::from_min_size(
                Pos2::new(reg.meters.left() + col as f32 * (col_w + 24.0), reg.meters.top() + row as f32 * (METER_ROW_H + METER_GAP)),
                Vec2::new(col_w, METER_ROW_H),
            );
            let label_w = (cell.width() * 0.4).min(200.0);
            let g = fitted(&p, name, 28.0, 18.0, label_w - 12.0, theme::INK);
            p.galley(Pos2::new(cell.left(), cell.center().y - g.size().y / 2.0), g, theme::INK);
            let bar = Rect::from_min_max(Pos2::new(cell.left() + label_w, cell.center().y - 10.0), Pos2::new(cell.right(), cell.center().y + 10.0));
            let floor = if peak.is_some() { MASTER_FLOOR_DB } else { RIG_FLOOR_DB };
            paint_bar(&p, bar, *pos, *peak, floor);
        }

        // Panic (destructive, rule 85) and the way out.
        let bottom = reg.bottom;
        let narrow = (bottom.width() / 900.0).clamp(0.72, 1.0);
        let qsize = |v: f32| ((v * narrow) * 2.0).round() / 2.0;
        let panic_rect = Rect::from_min_size(bottom.left_top(), Vec2::new((bottom.width() * 0.34).clamp(180.0, 300.0), bottom.height()));
        let resp = ui.interact(panic_rect, egui::Id::unique("show_mode_panic"), egui::Sense::click());
        let (fill, edge) = if resp.hovered() {
            (mix(theme::DANGER_BG, theme::PAPER_SINK, 0.25), theme::CLIP_BRIGHT)
        } else {
            (theme::DANGER_BG, theme::CLIP_RED)
        };
        p.rect_filled(panic_rect, 4.0, fill);
        p.rect_stroke(panic_rect, 4.0, Stroke::new(2.0, edge), egui::StrokeKind::Inside);
        let word = p.layout_no_wrap("Panic".into(), theme::medium_font(qsize(36.0)), theme::CLIP_BRIGHT);
        p.galley(panic_rect.center() - word.size() / 2.0, word, theme::CLIP_BRIGHT);
        if resp.clicked() {
            self.panic();
        }
        let now = self.ui_time;
        let Some(show) = &mut self.show_mode else { return };
        show.panic_rect = Some(panic_rect);
        let held_out = show.esc_since.is_some_and(|t| now - t >= LEAVE_HOLD_SECS);
        let hint_str = if held_out { "Let go of Esc to leave" } else { "Hold Esc to leave" };
        let hint = p.layout_no_wrap(hint_str.into(), theme::medium_font(qsize(28.0)), theme::INK_2);
        let hint_w = hint.size().x;
        let hint_pos = Pos2::new(bottom.right() - hint_w, bottom.center().y - hint.size().y / 2.0 - 4.0);
        if show.panic_at.is_some_and(|t| now - t < PANIC_NOTE_SECS) {
            let note_avail = hint_pos.x - panic_rect.right() - 28.0;
            if note_avail >= 60.0 {
                let note = fitted(&p, "All notes off", qsize(28.0), 14.0, note_avail, theme::INK_2);
                p.galley(Pos2::new(panic_rect.right() + 16.0, panic_rect.center().y - note.size().y / 2.0), note, theme::INK_2);
            }
        }
        let track = Rect::from_min_size(Pos2::new(hint_pos.x, hint_pos.y + hint.size().y + 6.0), Vec2::new(hint_w, 4.0));
        p.galley(hint_pos, hint, theme::INK_2);
        p.rect_filled(track, 2.0, theme::METER_TRACK);
        if let Some(t) = show.esc_since {
            let held = ((now - t) / LEAVE_HOLD_SECS).clamp(0.0, 1.0);
            p.rect_filled(Rect::from_min_size(track.min, Vec2::new(hint_w * held, 4.0)), 2.0, theme::CREAM);
        }
    }

    /// The Show mode tools' answer: whether it is open and what it shows.
    pub(super) fn show_mode_json(&self) -> serde_json::Value {
        let b = self.board();
        serde_json::json!({
            "show_mode": self.show_mode.is_some(),
            "song": b.song,
            "part": b.part,
            "next_part": b.next_part,
            "next_song": b.next,
            "tempo": b.tempo,
            "notes": b.notes,
            "loading": b.loading,
            "live_rigs": b.rigs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::control::{name_bytes, Graph, NodeKind, PortRef};

    const SURGE: &str = "org.surge-synth-team.surge-xt";
    const WIDE: Vec2 = Vec2::new(1280.0, 800.0);
    const TALL: Vec2 = Vec2::new(781.0, 950.0);

    /// Piano and Organ, and three songs: a verse on the piano, a chorus on
    /// the organ, then two songs of one part each.
    pub(in crate::ui) fn gig() -> DemoApp {
        let synth = |name: &str| {
            let mut g = Graph::default();
            g.reserve(8, 8);
            let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let s = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE) }, name_bytes(name));
            let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, crate::control::PortKind::Midi);
            g.connect_with_id(2, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, crate::control::PortKind::Audio);
            g
        };
        let rigs = vec![
            crate::control::Rig::new("Piano", synth("PIANO"), None),
            crate::control::Rig::new("Organ", synth("ORGAN"), None),
        ];
        let mut app = DemoApp::new_with_sink(None);
        app.build_rack(&crate::control::merge_rigs(&rigs), &[]);
        app.session_rig_names = vec!["Piano".into(), "Organ".into()];
        if let Ok(mut s) = app.rig_snapshot.lock() {
            s.rigs = ["Piano", "Organ"].map(|name| crate::control::RigMeta { name: name.into(), midi_device: None }).to_vec();
        }
        for (name, bpm, parts) in [
            ("Moonlight Drive", 118, serde_json::json!([
                {"name": "Verse", "rigs": [{"rig": 0, "level": 0.9}]},
                {"name": "Chorus", "rigs": [{"rig": 1, "level": 0.7}]},
            ])),
            ("Riders on the Storm", 104, serde_json::json!([{"name": "Main", "rigs": [{"rig": 1}]}])),
            ("Light My Fire", 126, serde_json::json!([{"name": "Solo", "rigs": [{"rig": 0}, {"rig": 1}]}])),
        ] {
            let out = app.exec_tool("set_setlist_song", &serde_json::json!({"name": name, "bpm": bpm, "parts": parts.to_string()}));
            assert!(!out.starts_with("error"), "{out}");
        }
        app.select_setlist(0, 0).unwrap();
        app
    }

    fn texts_of(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, Rect)> {
        fn walk(shape: &Shape, out: &mut Vec<(String, Rect)>) {
            match shape {
                Shape::Text(t) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for cs in shapes {
            walk(&cs.shape, &mut out);
        }
        out
    }

    /// One real frame; returns the texts it painted.
    fn frame(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        raw.focused = true;
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        texts_of(&out.shapes)
    }

    /// Frames until the window has its size in points: the frame after the
    /// app sets its 1.6 scale is laid out at the old zoom.
    fn settle(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) {
        for _ in 0..3 {
            frame(app, ctx, size, vec![]);
        }
    }

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers }
    }

    fn tap(app: &mut DemoApp, ctx: &egui::Context, k: Key, m: Modifiers) {
        frame(app, ctx, WIDE, vec![egui::Event::ModifiersChanged(m), key(k, true, m)]);
        frame(app, ctx, WIDE, vec![key(k, false, m), egui::Event::ModifiersChanged(Modifiers::NONE)]);
    }

    fn click(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, pos: Pos2, button: egui::PointerButton) {
        let b = |pressed| egui::Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        frame(app, ctx, size, vec![egui::Event::PointerMoved(pos)]);
        frame(app, ctx, size, vec![b(true)]);
        frame(app, ctx, size, vec![b(false)]);
    }

    /// What the lock protects: the patch (graph, knobs, plugin states), the
    /// mix and the setlist, all saved in the session.
    fn protected(app: &mut DemoApp) -> String {
        format!("{:?}", app.capture_session("lock").unwrap())
    }

    fn song_order(app: &DemoApp) -> Vec<String> {
        app.setlist.songs.iter().map(|s| s.name.clone()).collect()
    }

    /// Done when #210: with Show mode open, no click or key other than its
    /// own changes the patch, the mix or the setlist order.
    #[test]
    fn show_mode_locks_every_key_and_click_out_of_the_patch_the_mix_and_the_setlist() {
        let mut app = gig();
        let ctx = egui::Context::default();
        app.stage_view = StageView::Setlist;
        settle(&mut app, &ctx, WIDE);
        app.exec_tool("set_show_mode", &serde_json::json!({"on": true}));
        frame(&mut app, &ctx, WIDE, vec![]);
        let (before, order, mix, active) = (protected(&mut app), song_order(&app), format!("{:?}", app.rig_mix), app.setlist.active);
        let (playing, tuner, bpm) = (app.playing, app.tuner_on, app.bpm);
        let screen = (app.stage_view, app.dock, app.kb_expanded, app.nav_open, app.show_help);

        let own = |k: Key, m: Modifiers| {
            let plain_or_shift = m == Modifiers::NONE || m == Modifiers::SHIFT;
            match k {
                Key::PageUp | Key::PageDown | Key::ArrowLeft | Key::ArrowRight => plain_or_shift,
                Key::ArrowUp | Key::ArrowDown | Key::Space => m == Modifiers::NONE,
                Key::T => m == Modifiers::SHIFT || m == Modifiers::COMMAND,
                _ => false,
            }
        };
        let mods = [Modifiers::NONE, Modifiers::SHIFT, Modifiers::CTRL, Modifiers::ALT, Modifiers::CTRL | Modifiers::SHIFT, Modifiers::COMMAND];
        for &m in &mods {
            for &k in Key::ALL.iter().filter(|&&k| !own(k, m)) {
                tap(&mut app, &ctx, k, m);
            }
        }
        for ev in [
            egui::Event::Text("qwerty 1234".into()),
            egui::Event::Paste("{\"nodes\":[]}".into()),
            egui::Event::Copy,
            egui::Event::Cut,
            egui::Event::Zoom(1.5),
        ] {
            frame(&mut app, &ctx, WIDE, vec![ev]);
        }
        for y in (6..WIDE.y as i32).step_by(40) {
            for x in (6..WIDE.x as i32).step_by(40) {
                let pos = Pos2::new(x as f32, y as f32);
                click(&mut app, &ctx, WIDE, pos, egui::PointerButton::Primary);
                if x % 200 == 6 {
                    click(&mut app, &ctx, WIDE, pos, egui::PointerButton::Secondary);
                    let wheel = egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: Vec2::new(0.0, -120.0),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: Modifiers::NONE,
                    };
                    frame(&mut app, &ctx, WIDE, vec![wheel]);
                }
            }
        }
        // Drags across the whole screen, as a stray hand on a trackpad.
        for (a, b) in [(Pos2::new(40.0, 120.0), Pos2::new(1200.0, 700.0)), (Pos2::new(1200.0, 100.0), Pos2::new(60.0, 760.0))] {
            let b_ev = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            frame(&mut app, &ctx, WIDE, vec![egui::Event::PointerMoved(a), b_ev(a, true)]);
            for i in 1..=20 {
                frame(&mut app, &ctx, WIDE, vec![egui::Event::PointerMoved(a + (b - a) * (i as f32 / 20.0))]);
            }
            frame(&mut app, &ctx, WIDE, vec![b_ev(b, false)]);
        }

        assert!(app.show_mode.is_some(), "only a held Esc leaves");
        assert_eq!(song_order(&app), order, "the setlist order");
        assert_eq!(format!("{:?}", app.rig_mix), mix, "the mix");
        assert_eq!(app.setlist.active, active, "no key but its own steps the part");
        assert_eq!(app.playing, playing, "no key other than Space toggles transport");
        assert_eq!(app.tuner_on, tuner, "no key other than Ctrl+T toggles tuner");
        assert!((app.bpm - bpm).abs() < 1e-6, "no key other than Shift+T changes tempo");
        assert!(protected(&mut app) == before, "the patch, mix and setlist are as they were");
        assert_eq!((app.stage_view, app.dock, app.kb_expanded, app.nav_open, app.show_help), screen, "nothing opened behind the screen");

        // The same harness reaches the editing frame: F4 loads the
        // multi-rig demo there.
        app.set_show_mode(false);
        tap(&mut app, &ctx, Key::F4, Modifiers::NONE);
        assert!(protected(&mut app) != before, "the barrage would have been seen without Show mode");
    }

    #[test]
    fn show_mode_shows_the_song_the_part_the_next_song_tempo_levels_and_the_clock() {
        for size in [WIDE, TALL] {
            let mut app = gig();
            let ctx = egui::Context::default();
            app.set_show_mode(true);
            settle(&mut app, &ctx, size);
            let texts = frame(&mut app, &ctx, size, vec![]);
            let has = |want: &str| texts.iter().any(|(t, _)| t == want);
            for want in ["Moonlight Drive", "Verse", "Chorus", "Riders on the Storm", "118 BPM · 4/4", "Master", "Piano", "Panic", "Hold Esc to leave"] {
                assert!(has(want), "{size:?}: the screen shows {want}: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());
            }
            assert!(!has("Organ"), "{size:?}: the organ does not play in the verse, so it has no meter");
            assert!(texts.iter().any(|(t, _)| t.len() == 5 && t.as_bytes()[2] == b':' && t.bytes().filter(u8::is_ascii_digit).count() == 4), "{size:?}: a clock");
            let window = Rect::from_min_size(Pos2::ZERO, size);
            // Tracked captions are one galley per letter: letters may touch
            // each other, never a word.
            let inked: Vec<&(String, Rect)> = texts.iter().filter(|(t, _)| !t.trim().is_empty()).collect();
            for (i, (a, ra)) in inked.iter().enumerate() {
                assert!(window.contains_rect(*ra), "{size:?}: '{a}' at {ra:?} leaves the window");
                for (b, rb) in &inked[i + 1..] {
                    let letters = a.chars().count() == 1 && b.chars().count() == 1;
                    assert!(letters || !ra.shrink(1.0).intersects(rb.shrink(1.0)), "{size:?}: '{a}' {ra:?} overlaps '{b}' {rb:?}");
                }
            }
        }
    }

    #[test]
    fn show_mode_narrow_bottom_bar_and_indicators_fit_without_overlap() {
        let size = Vec2::new(760.0, 520.0);
        let mut app = gig();
        let ctx = egui::Context::default();
        app.playing = true;
        app.player.playing = true;
        app.set_show_mode(true);
        settle(&mut app, &ctx, size);
        app.panic();
        if let Some(s) = &mut app.show_mode {
            s.esc_since = Some(app.ui_time - LEAVE_HOLD_SECS - 0.1);
        }
        let texts = frame(&mut app, &ctx, size, vec![]);
        let has = |want: &str| texts.iter().any(|(t, _)| t == want);
        for want in ["♫ TRACK", "CLICK", "Moonlight Drive", "Verse", "Chorus", "Riders on the Storm", "Master", "Panic", "All notes off", "Let go of Esc to leave"] {
            assert!(has(want), "narrow show mode shows {want}: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());
        }
        let window = Rect::from_min_size(Pos2::ZERO, size);
        let inked: Vec<&(String, Rect)> = texts.iter().filter(|(t, _)| !t.trim().is_empty()).collect();
        for (i, (a, ra)) in inked.iter().enumerate() {
            assert!(window.contains_rect(*ra), "'{a}' at {ra:?} leaves the window");
            for (b, rb) in &inked[i + 1..] {
                let letters = a.chars().count() == 1 && b.chars().count() == 1;
                assert!(letters || !ra.shrink(1.0).intersects(rb.shrink(1.0)), "'{a}' {ra:?} overlaps '{b}' {rb:?}");
            }
        }
    }

    /// Rules 30 and 53: a rig whose level decays towards silence reads dark.
    #[test]
    fn a_silent_rig_reads_dark() {
        let mut app = gig();
        let ctx = egui::Context::default();
        app.set_show_mode(true);
        settle(&mut app, &ctx, WIDE);
        for u in &mut app.units {
            u.led = 0.0;
        }
        app.rig_levels = vec![1e-4, 0.0];
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, WIDE));
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        fn lit(shape: &Shape, n: &mut usize) {
            match shape {
                Shape::Rect(r) if [theme::CREAM, theme::ACCENT, theme::CLIP_RED].contains(&r.fill) => *n += 1,
                Shape::Vec(v) => v.iter().for_each(|s| lit(s, n)),
                _ => {}
            }
        }
        let mut n = 0;
        out.shapes.iter().for_each(|cs| lit(&cs.shape, &mut n));
        assert_eq!(n, 0, "no lit meter segment in silence");
    }

    #[test]
    fn page_keys_step_the_part_and_song_and_a_held_esc_leaves() {
        let mut app = gig();
        let ctx = egui::Context::default();
        app.stage_view = StageView::Setlist;
        app.set_show_mode(true);
        settle(&mut app, &ctx, WIDE);
        tap(&mut app, &ctx, Key::PageDown, Modifiers::NONE);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }));
        assert!(texts.iter().any(|(t, _)| t == "Chorus") && texts.iter().any(|(t, _)| t == "Organ"), "the chorus plays the organ");
        assert!(!texts.iter().any(|(t, _)| t == "Piano"), "the piano is out of the chorus");
        assert!((app.rig_mix[1].vol - 0.7).abs() < 1e-6, "the part's level is recalled");
        tap(&mut app, &ctx, Key::PageDown, Modifiers::SHIFT);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 1, part: 0 }), "Shift steps songs");
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert!(texts.iter().any(|(t, _)| t == "Light My Fire"), "and the next song follows");
        tap(&mut app, &ctx, Key::PageUp, Modifiers::NONE);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }));

        // 1/60 s a frame, with the OS repeating the held key: half the hold
        // does nothing; a full hold leaves when Esc is let go, and none of
        // its repeats reaches the editing frame.
        let frames = (LEAVE_HOLD_SECS * 60.0) as usize;
        let repeat = egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: true, modifiers: Modifiers::NONE };
        frame(&mut app, &ctx, WIDE, vec![key(Key::Escape, true, Modifiers::NONE)]);
        for _ in 0..frames / 2 {
            frame(&mut app, &ctx, WIDE, vec![repeat.clone()]);
        }
        frame(&mut app, &ctx, WIDE, vec![key(Key::Escape, false, Modifiers::NONE)]);
        assert!(app.show_mode.is_some(), "a short press does not leave");
        frame(&mut app, &ctx, WIDE, vec![key(Key::Escape, true, Modifiers::NONE)]);
        let mut texts = Vec::new();
        for _ in 0..frames * 2 {
            texts = frame(&mut app, &ctx, WIDE, vec![repeat.clone()]);
        }
        assert!(app.show_mode.is_some(), "still held: still on stage");
        assert!(texts.iter().any(|(t, _)| t == "Let go of Esc to leave"), "the screen says what leaves");
        frame(&mut app, &ctx, WIDE, vec![key(Key::Escape, false, Modifiers::NONE)]);
        assert!(app.show_mode.is_none(), "letting go after a full hold leaves");
        frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.stage_view, StageView::Setlist, "back where Show mode was opened, not closed by the held Esc");
    }

    #[test]
    fn arrow_keys_step_the_part_and_song_in_show_mode() {
        let mut app = gig();
        app.setlist.songs[0].notes = (1..=40).map(|i| format!("Line {i}")).collect::<Vec<_>>().join("\n");
        let ctx = egui::Context::default();
        app.set_show_mode(true);
        settle(&mut app, &ctx, WIDE);

        // Plain ArrowRight / ArrowLeft step parts across song boundaries.
        tap(&mut app, &ctx, Key::ArrowRight, Modifiers::NONE);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }));
        assert!(texts.iter().any(|(t, _)| t == "Chorus") && texts.iter().any(|(t, _)| t == "Organ"), "ArrowRight steps to Chorus");
        assert!(!texts.iter().any(|(t, _)| t == "Piano"), "the piano is out of the chorus");
        assert!((app.rig_mix[1].vol - 0.7).abs() < 1e-6, "the part's level is recalled");

        tap(&mut app, &ctx, Key::ArrowRight, Modifiers::NONE);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 1, part: 0 }), "ArrowRight crosses into next song");
        tap(&mut app, &ctx, Key::ArrowLeft, Modifiers::NONE);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }), "ArrowLeft crosses back into previous song's last part");

        // Up/Down scroll notes without changing active part.
        tap(&mut app, &ctx, Key::ArrowDown, Modifiers::NONE);
        assert!(app.show_mode.as_ref().is_some_and(|s| s.notes_scroll_y > 0.0), "ArrowDown scrolls notes");
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }), "ArrowDown does not step");

        // Shift+ArrowRight / Shift+ArrowLeft jump whole songs and reset scroll.
        tap(&mut app, &ctx, Key::ArrowRight, Modifiers::SHIFT);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 1, part: 0 }), "Shift+ArrowRight jumps to next song");
        assert_eq!(app.show_mode.as_ref().map(|s| s.notes_scroll_y), Some(0.0), "song switch resets notes scroll");
        assert!(texts.iter().any(|(t, _)| t == "Light My Fire"), "next song preview updates");

        tap(&mut app, &ctx, Key::ArrowRight, Modifiers::SHIFT);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 2, part: 0 }));
        tap(&mut app, &ctx, Key::ArrowLeft, Modifiers::SHIFT);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 1, part: 0 }));
        tap(&mut app, &ctx, Key::ArrowLeft, Modifiers::SHIFT);
        frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 0 }), "Shift+ArrowLeft jumps to song's first part");
        assert_eq!(app.show_mode.as_ref().map(|s| s.notes_scroll_y), Some(0.0), "returning to song 0 starts at top");

        // Extra modifiers on arrows never step.
        for m in [Modifiers::CTRL, Modifiers::ALT, Modifiers::COMMAND, Modifiers::CTRL | Modifiers::SHIFT, Modifiers::ALT | Modifiers::SHIFT] {
            tap(&mut app, &ctx, Key::ArrowRight, m);
            tap(&mut app, &ctx, Key::ArrowLeft, m);
        }
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 0 }), "modified arrows stay locked");

        // Starting from an inactive setlist shows the arrow hint and starts on ArrowRight.
        app.step_setlist("clear").unwrap();
        assert_eq!(app.board().caption, "Setlist not started · → or PageDown starts");
        tap(&mut app, &ctx, Key::ArrowRight, Modifiers::NONE);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 0 }));
    }

    #[test]
    fn stage_keyboard_controls_space_tap_tempo_and_tuner_work_in_show_mode() {
        let mut app = gig();
        let ctx = egui::Context::default();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        app.set_show_mode(true);
        settle(&mut app, &ctx, WIDE);
        drain.drain(|_| {});

        // Space toggles master transport (CLICK indicator) and emits TransportPlay / TransportStop.
        assert!(!app.playing);
        tap(&mut app, &ctx, Key::Space, Modifiers::NONE);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(app.playing, "Space starts transport in Show mode");
        assert!(texts.iter().any(|(t, _)| t == "CLICK"), "Show mode displays CLICK indicator");
        assert!(cmds.iter().any(|c| matches!(c, Command::TransportPlay)), "{cmds:?}");

        tap(&mut app, &ctx, Key::Space, Modifiers::NONE);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        assert!(!app.playing, "Space stops transport in Show mode");
        assert!(!texts.iter().any(|(t, _)| t == "CLICK"), "CLICK indicator disappears on stop");
        assert!(cmds.iter().any(|c| matches!(c, Command::TransportStop)), "{cmds:?}");

        // Modified Space stays locked.
        for m in [Modifiers::SHIFT, Modifiers::ALT, Modifiers::CTRL] {
            tap(&mut app, &ctx, Key::Space, m);
        }
        assert!(!app.playing, "modified Space does not toggle transport");

        // Shift+T taps tempo without wall-clock races (rule 184).
        app.bpm = 90.0;
        app.tap_tempo.reset();
        let first_tap = app.tap_origin.elapsed().as_secs_f64() - 0.5;
        assert_eq!(app.tap_tempo.tap(first_tap), None);
        drain.drain(|_| {});
        tap(&mut app, &ctx, Key::T, Modifiers::SHIFT);
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        assert!(app.tap_flash_t > 0.0, "Shift+T flashes tap tempo");
        assert!((110.0..=125.0).contains(&app.bpm), "Shift+T updates BPM from interval (~120 BPM), got {}", app.bpm);
        assert!(cmds.iter().any(|c| matches!(c, Command::SetBpm { bpm } if (110.0..=125.0).contains(bpm))), "{cmds:?}");

        // Ctrl+T toggles the tuner overlay, and a short Esc closes it without leaving Show mode.
        assert!(!app.tuner_on);
        tap(&mut app, &ctx, Key::T, Modifiers::COMMAND);
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert!(app.tuner_on, "Ctrl+T opens tuner in Show mode");
        assert!(texts.iter().any(|(t, _)| t == "click to close"), "tuner overlay renders in Show mode");

        tap(&mut app, &ctx, Key::Escape, Modifiers::NONE);
        assert!(!app.tuner_on, "short Esc closes tuner in Show mode");
        assert!(app.show_mode.is_some(), "short Esc stays in Show mode");

        tap(&mut app, &ctx, Key::T, Modifiers::COMMAND);
        assert!(app.tuner_on, "Ctrl+T opens tuner again");
        tap(&mut app, &ctx, Key::T, Modifiers::COMMAND);
        assert!(!app.tuner_on, "Ctrl+T closes tuner");

        // Plain T, Ctrl+Shift+T, and Alt+T variants stay locked.
        let prev_bpm = app.bpm;
        for m in [Modifiers::NONE, Modifiers::COMMAND | Modifiers::SHIFT, Modifiers::ALT | Modifiers::SHIFT, Modifiers::ALT | Modifiers::COMMAND] {
            tap(&mut app, &ctx, Key::T, m);
        }
        assert!(!app.tuner_on, "plain/Shift/Alt T variants do not open tuner");
        assert!((app.bpm - prev_bpm).abs() < 1e-6, "non-Shift+T variants do not tap tempo");
    }

    #[test]
    fn panic_sends_all_notes_off_and_says_so() {
        let mut app = gig();
        let ctx = egui::Context::default();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        app.set_show_mode(true);
        settle(&mut app, &ctx, WIDE);
        app.notes_down[60] = 1;
        drain.drain(|_| {});
        let panic = app.show_mode.as_ref().and_then(|s| s.panic_rect).expect("Panic is on screen");
        click(&mut app, &ctx, WIDE, panic.center(), egui::PointerButton::Primary);
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c));
        assert!(sent.iter().any(|c| matches!(c, Command::Panic)), "{sent:?}");
        assert_eq!(app.notes_down[60], 0, "the key goes dark");
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert!(texts.iter().any(|(t, _)| t == "All notes off"));

        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("panic", &serde_json::json!({}))).unwrap();
        assert_eq!(v["panic"], true);
        drain.drain(|c| sent.push(c));
        assert_eq!(sent.iter().filter(|c| matches!(c, Command::Panic)).count(), 2, "the agent's panic too");
    }

    /// A foot switch (hardware MIDI) still steps the part: the lock is for
    /// the screen. The CC reaches the engine and the app, as `hw_direct` and
    /// the MIDI thread deliver it.
    #[test]
    fn a_foot_switch_steps_the_part_under_show_mode_and_the_screen_follows() {
        let mut app = gig();
        let ctx = egui::Context::default();
        let (tx, drain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), drain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
        engine.set_graph_pub(pub_.clone());
        let bus = Arc::new(Mutex::new(tx));
        app.bus = Some(bus.clone());
        app.graph_pub = Some(pub_);
        app.exec_tool("set_show_control", &serde_json::json!({"action": "next_part", "cc": 80}));
        app.select_setlist(0, 0).unwrap();
        app.set_show_mode(true);
        let mut buf = vec![0.0f32; 64 * 2];
        frame(&mut app, &ctx, WIDE, vec![]);
        engine.render(&mut buf, 2);
        for value in [127, 0] {
            let (source, channel, cc) = (SourceId::Hardware(0), 0, 80);
            assert!(bus.lock().unwrap().send(Command::Cc { source, channel, cc, value }));
            app.hw_cc_queue.lock().unwrap().push_back((source, channel, cc, value));
            engine.render(&mut buf, 2);
            frame(&mut app, &ctx, WIDE, vec![]);
        }
        let texts = frame(&mut app, &ctx, WIDE, vec![]);
        assert_eq!(app.setlist.active, Some(crate::setlist::SetlistPos { song: 0, part: 1 }), "one press, one step");
        assert!(texts.iter().any(|(t, _)| t == "Chorus"), "the screen shows the part the pedal stepped to");
    }

    #[test]
    fn the_show_tools_are_in_every_table() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for name in ["set_show_mode", "panic"] {
            assert!(names.contains(&name), "assistant tools miss {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS miss {name}");
            assert!(include_str!("../mcp.rs").contains(&format!("#[tool(name = \"{name}\"")), "MCP misses {name}");
        }
        let mut app = gig();
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("set_show_mode", &serde_json::json!({"on": true}))).unwrap();
        assert_eq!((v["show_mode"].as_bool(), v["song"].as_str(), v["part"].as_str(), v["next_song"].as_str()), (Some(true), Some("Moonlight Drive"), Some("Verse"), Some("Riders on the Storm")), "{v}");
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("set_show_mode", &serde_json::json!({"on": false}))).unwrap();
        assert_eq!(v["show_mode"], false);
        assert!(app.exec_tool("set_show_mode", &serde_json::json!({})).starts_with("error"));
    }

    #[test]
    fn show_mode_renders_song_notes_and_chordpro_without_overlap() {
        for size in [WIDE, TALL] {
            let mut app = gig();
            app.setlist.songs[0].notes = "{c: Verse 1}\n[Am]Let's swim to the [G]moon\n{c: Chorus}\n[F]Tonight we [E7]drive".into();
            let ctx = egui::Context::default();
            app.set_show_mode(true);
            settle(&mut app, &ctx, size);
            let texts = frame(&mut app, &ctx, size, vec![]);
            let has_substr = |want: &str| texts.iter().any(|(t, _)| t.contains(want));
            for want in ["Moonlight Drive", "Verse", "Riders on the Storm", "Panic", "Verse 1", "[Am]", "swim to the", "[G]", "Chorus"] {
                assert!(has_substr(want), "{size:?}: show mode with notes should show {want:?}, got {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());
            }
            let window = Rect::from_min_size(Pos2::ZERO, size);
            let inked: Vec<&(String, Rect)> = texts.iter().filter(|(t, _)| !t.trim().is_empty()).collect();
            for (i, (a, ra)) in inked.iter().enumerate() {
                assert!(window.contains_rect(*ra), "{size:?}: '{a}' at {ra:?} leaves the window");
                for (b, rb) in &inked[i + 1..] {
                    let letters = a.chars().count() == 1 && b.chars().count() == 1;
                    assert!(letters || !ra.shrink(1.0).intersects(rb.shrink(1.0)), "{size:?}: '{a}' {ra:?} overlaps '{b}' {rb:?}");
                }
            }
            assert_eq!(app.show_mode_json()["notes"].as_str(), Some(app.setlist.songs[0].notes.as_str()));
        }
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat show_mode_shots -- --ignored"]
    fn show_mode_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        for size in [WIDE, TALL] {
            // Rule 148: a fresh app per image.
            let mut app = gig();
            app.select_setlist(0, 1).unwrap();
            app.set_show_mode(true);
            // The organ plays the chorus at about −10 dBFS on the master.
            app.out_level = Some(Arc::new(std::sync::atomic::AtomicU32::new(0.32f32.to_bits())));
            let path = dir.join(format!("show-mode-{}x{}.png", size.x, size.y));
            super::super::offline_shots::shot_with(&mut app, size, &path, |a, _| {
                a.rig_levels = vec![0.0, 0.85];
                Vec::new()
            });
        }
    }
}
