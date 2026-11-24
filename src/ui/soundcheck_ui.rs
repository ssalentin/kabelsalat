//! The Soundcheck stage view (#450): the screen on top of Virtual Soundcheck
//! (#400). It owns no soundcheck logic: every action goes through
//! `crate::soundcheck::tool` (`list_takes`, `soundcheck_start`, `_seek`,
//! `_loop`, `_stop`, `export_take`), the same calls the assistant and MCP
//! make, and every figure it shows is `SoundcheckShared::status` or
//! `soundcheck::export_state`. Scans of the takes folder and manifests run
//! on a worker thread, the export on the export thread the tool starts, so
//! no frame waits for a disk.

use super::*;
use crate::soundcheck::{self, SoundcheckShared, Status};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// One take of the list (`list_takes` row).
#[derive(Clone, Debug, Default)]
pub(super) struct TakeRow {
    pub(super) folder: String,
    pub(super) name: String,
    pub(super) seconds: f64,
    pub(super) stems: usize,
    pub(super) inputs: usize,
    pub(super) markers: usize,
    pub(super) midi_events: u64,
}

/// A marker row: label and its section in seconds.
#[derive(Clone, Debug, Default)]
pub(super) struct MarkerRow {
    pub(super) label: String,
    pub(super) start: f64,
    pub(super) end: f64,
}

/// What the export dialog asks for.
#[derive(Clone, Debug)]
pub(super) struct ExportDraft {
    pub(super) mixdown: bool,
    pub(super) stems: bool,
    pub(super) inputs: bool,
    /// `None` = the whole take.
    pub(super) section: Option<usize>,
    pub(super) format: &'static str,
}

impl Default for ExportDraft {
    fn default() -> Self {
        Self { mixdown: true, stems: true, inputs: false, section: None, format: "pcm24" }
    }
}

type Slot<T> = Arc<Mutex<Option<T>>>;

/// What the app keeps for the view.
pub(super) struct SoundcheckPanel {
    pub(super) takes: Vec<TakeRow>,
    scan: Slot<Vec<TakeRow>>,
    scanning: Arc<AtomicBool>,
    scanned: bool,
    /// Recorder take count when the list was read: a new take rescans.
    seen_takes: u64,
    pub(super) selected: Option<String>,
    /// Markers of the selected take when none plays, read off the UI thread.
    pub(super) detail: Option<(String, Vec<MarkerRow>)>,
    detail_slot: Slot<(String, Vec<MarkerRow>)>,
    detail_for: Option<String>,
    pub(super) export_open: bool,
    pub(super) draft: ExportDraft,
    /// The last thing a click did that failed or needs saying.
    pub(super) msg: Option<(String, bool)>,
    /// Virtual Soundcheck is on (read once per frame): the inputs hear the take.
    pub(super) engaged: bool,
    /// Tests: the engine's soundcheck, a takes folder, a status to show.
    pub(super) shared: Option<Arc<SoundcheckShared>>,
    pub(super) dir: Option<PathBuf>,
    pub(super) status_override: Option<Status>,
    /// Rects drawn this frame, for tests.
    pub(super) take_rects: Vec<Rect>,
    pub(super) marker_rects: Vec<Rect>,
    pub(super) loop_rects: Vec<Rect>,
    pub(super) start_rect: Rect,
    pub(super) stop_rect: Rect,
    pub(super) bar_rect: Rect,
    pub(super) export_rect: Rect,
    pub(super) run_export_rect: Rect,
    pub(super) section_rects: Vec<Rect>,
}

impl Default for SoundcheckPanel {
    fn default() -> Self {
        Self {
            takes: Vec::new(),
            scan: Default::default(),
            scanning: Default::default(),
            scanned: false,
            seen_takes: 0,
            selected: None,
            detail: None,
            detail_slot: Default::default(),
            detail_for: None,
            export_open: false,
            draft: ExportDraft::default(),
            msg: None,
            engaged: false,
            shared: None,
            dir: None,
            status_override: None,
            take_rects: Vec::new(),
            marker_rects: Vec::new(),
            loop_rects: Vec::new(),
            start_rect: Rect::NOTHING,
            stop_rect: Rect::NOTHING,
            bar_rect: Rect::NOTHING,
            export_rect: Rect::NOTHING,
            run_export_rect: Rect::NOTHING,
            section_rects: Vec::new(),
        }
    }
}

fn mmss(s: f64) -> String {
    let t = s.max(0.0).round() as u64;
    format!("{}:{:02}", t / 60, t % 60)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn take_meta(t: &TakeRow) -> String {
    format!(
        "{} · {} · {} · {}",
        mmss(t.seconds),
        plural(t.stems, "stem", "stems"),
        plural(t.inputs, "raw input", "raw inputs"),
        plural(t.markers, "marker", "markers")
    )
}

fn fit(ui: &egui::Ui, text: &str, size: f32, color: Color32, w: f32) -> Arc<egui::Galley> {
    egui::WidgetText::from(egui::RichText::new(text).size(size).color(color))
        .into_galley(ui, Some(egui::TextWrapMode::Truncate), w.max(1.0), egui::TextStyle::Body)
}

fn parse_takes(v: &Value) -> Vec<TakeRow> {
    v.get("takes")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .map(|t| TakeRow {
                    folder: t["folder"].as_str().unwrap_or_default().into(),
                    name: t["name"].as_str().unwrap_or_default().into(),
                    seconds: t["seconds"].as_f64().unwrap_or(0.0),
                    stems: t["stems"].as_u64().unwrap_or(0) as usize,
                    inputs: t["inputs"].as_u64().unwrap_or(0) as usize,
                    markers: t["markers"].as_u64().unwrap_or(0) as usize,
                    midi_events: t["midi_events"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

impl DemoApp {
    fn sc_shared(&self) -> Option<Arc<SoundcheckShared>> {
        self.soundcheck.shared.clone().or_else(|| soundcheck::LIVE.get().cloned())
    }

    /// Where takes are recorded. Tests never read the user's folder (rule 169).
    fn sc_dir(&self) -> PathBuf {
        match &self.soundcheck.dir {
            Some(d) => d.clone(),
            None if cfg!(test) => std::env::temp_dir().join(format!("ksalat-takes-{}", std::process::id())),
            None => crate::record::record_dir(),
        }
    }

    /// One soundcheck tool call, as the assistant makes it.
    fn sc_call(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let shared = self.sc_shared();
        let dir = self.sc_dir();
        let v = soundcheck::tool(name, &args, shared.as_ref(), &dir);
        match v.get("error").and_then(|e| e.as_str()) {
            Some(e) => {
                self.soundcheck.msg = Some((e.to_string(), true));
                Err(e.to_string())
            }
            None => {
                self.soundcheck.msg = None;
                Ok(v)
            }
        }
    }

    /// The take playing, and where (cheap: the engine's atomics and one lock).
    pub(super) fn sc_status(&self) -> Status {
        if let Some(s) = &self.soundcheck.status_override {
            return s.clone();
        }
        self.sc_shared().map(|s| s.status()).unwrap_or_default()
    }

    /// Read once per frame, before the top bar and the faces draw.
    pub(super) fn sc_refresh_engaged(&mut self) {
        self.soundcheck.engaged = match &self.soundcheck.status_override {
            Some(s) => s.active,
            None => self.sc_shared().is_some_and(|s| s.engaged()),
        };
    }

    /// Stop the soundcheck: the inputs hear the stage again.
    pub(super) fn sc_stop(&mut self) {
        let _ = self.sc_call("soundcheck_stop", json!({}));
        self.soundcheck.engaged = false;
    }

    /// Read the takes folder on a worker thread (never per frame).
    fn sc_rescan(&mut self) {
        if self.soundcheck.scanning.swap(true, Ordering::AcqRel) {
            return;
        }
        let (slot, flag, dir) = (self.soundcheck.scan.clone(), self.soundcheck.scanning.clone(), self.sc_dir());
        let ok = std::thread::Builder::new()
            .name("kabelsalat-takes-scan".into())
            .spawn(move || {
                let rows = parse_takes(&soundcheck::tool("list_takes", &json!({}), None, &dir));
                if let Ok(mut s) = slot.lock() {
                    *s = Some(rows);
                }
                flag.store(false, Ordering::Release);
            })
            .is_ok();
        if !ok {
            self.soundcheck.scanning.store(false, Ordering::Release);
        }
    }

    /// Markers of the selected take, read on a worker thread.
    fn sc_load_detail(&mut self, folder: String) {
        let slot = self.soundcheck.detail_slot.clone();
        self.soundcheck.detail_for = Some(folder.clone());
        let _ = std::thread::Builder::new().name("kabelsalat-take-open".into()).spawn(move || {
            let rows = crate::take::Take::open(std::path::Path::new(&folder))
                .map(|t| {
                    let sr = t.manifest.sample_rate.max(1) as f64;
                    (0..t.manifest.markers.len())
                        .map(|i| MarkerRow {
                            label: t.manifest.markers[i].label(),
                            start: t.manifest.markers[i].frame as f64 / sr,
                            end: t.marker_end(i) as f64 / sr,
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Ok(mut s) = slot.lock() {
                *s = Some((folder, rows));
            }
        });
    }

    /// Adopt what the worker threads read; start the reads that are due.
    fn sc_poll(&mut self, ctx: &egui::Context) {
        let rec_takes = self.recorder.as_ref().map(|r| r.snapshot().takes).unwrap_or(0);
        if !self.soundcheck.scanned || rec_takes != self.soundcheck.seen_takes {
            self.soundcheck.scanned = true;
            self.soundcheck.seen_takes = rec_takes;
            self.sc_rescan();
        }
        let got = self.soundcheck.scan.lock().ok().and_then(|mut s| s.take());
        if let Some(rows) = got {
            self.soundcheck.takes = rows;
            let keep = self.soundcheck.selected.as_ref().is_some_and(|s| self.soundcheck.takes.iter().any(|t| &t.folder == s));
            if !keep {
                self.soundcheck.selected = self.soundcheck.takes.first().map(|t| t.folder.clone());
            }
        }
        let got = self.soundcheck.detail_slot.lock().ok().and_then(|mut s| s.take());
        if let Some(d) = got {
            self.soundcheck.detail = Some(d);
        }
        let want = self.soundcheck.selected.clone();
        if want != self.soundcheck.detail_for {
            if let Some(f) = want {
                self.sc_load_detail(f);
            } else {
                self.soundcheck.detail_for = None;
                self.soundcheck.detail = None;
            }
        }
        if self.soundcheck.scanning.load(Ordering::Acquire) || self.soundcheck.engaged || soundcheck::export_state().running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Open the view (and read the folder again).
    pub(super) fn open_soundcheck(&mut self) {
        if self.stage_view != StageView::Soundcheck {
            self.toggle_stage(StageView::Soundcheck);
        }
        self.soundcheck.scanned = false;
    }

    /// The markers to list: the playing take's, else the selected take's.
    fn sc_markers(&self, st: &Status) -> Vec<MarkerRow> {
        let playing_selected = st.active && st.take.as_ref() == self.soundcheck.selected.as_ref();
        if playing_selected {
            return st.markers.iter().map(|m| MarkerRow { label: m.label.clone(), start: m.seconds, end: m.end_seconds }).collect();
        }
        match (&self.soundcheck.detail, &self.soundcheck.selected) {
            (Some((f, rows)), Some(sel)) if f == sel => rows.clone(),
            _ => Vec::new(),
        }
    }

    pub(super) fn soundcheck_stage(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.sc_poll(&ctx);
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter().clone();
        theme::paper_bg(&painter, rect);
        let (sheet, inner) = self.stage_sheet_rects(rect, 200.0);
        painter.rect_filled(sheet, theme::RADIUS_SHEET, theme::DARK_PANEL);
        let help = self.show_help;
        let st = self.sc_status();
        self.soundcheck.take_rects.clear();
        self.soundcheck.marker_rects.clear();
        self.soundcheck.loop_rects.clear();
        self.soundcheck.section_rects.clear();
        self.soundcheck.start_rect = Rect::NOTHING;
        self.soundcheck.stop_rect = Rect::NOTHING;
        self.soundcheck.bar_rect = Rect::NOTHING;
        self.soundcheck.export_rect = Rect::NOTHING;

        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.shrink_clip_rect(sheet);
            ui.add_enabled_ui(!help, |ui| {
                let notice = self.active_patch_notice().map(str::to_owned);
                let status_line = if st.active {
                    Some(("Virtual Soundcheck is on: the inputs play the take, not the stage", theme::ACCENT_BRIGHT))
                } else {
                    notice.as_deref().map(|n| (n, theme::ACCENT_BRIGHT))
                };
                Self::stage_header(
                    ui,
                    "Soundcheck",
                    "play a recorded gig back through the rack · Esc: back to the rack",
                    status_line,
                );
                let avail_w = ui.available_width();
                let h = ui.available_height().max(80.0);
                let two_col = avail_w >= 700.0;
                egui::ScrollArea::vertical()
                    .id_salt("soundcheck_scroll")
                    .min_scrolled_height(h)
                    .max_height(h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if two_col {
                            let left_w = (avail_w * 0.38).clamp(250.0, 400.0);
                            ui.spacing_mut().item_spacing.x = 25.0;
                            ui.horizontal_top(|ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(left_w, h.max(600.0)),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| self.sc_takes_column(ui, &st, h - 6.0),
                                );
                                let rest = ui.available_width().max(200.0);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(rest, h.max(600.0)),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| self.sc_transport_column(ui, &st),
                                );
                            });
                        } else {
                            self.sc_takes_column(ui, &st, 150.0);
                            ui.add_space(14.0);
                            self.sc_transport_column(ui, &st);
                        }
                    });
            });
        });
    }

    fn sc_takes_column(&mut self, ui: &mut egui::Ui, st: &Status, max_h: f32) {
        let w = ui.available_width();
        theme::section_label(ui, "Takes");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::chip_button(ui, "Refresh", false).on_hover_text("read the recordings folder again").clicked() {
                self.soundcheck.scanned = false;
            }
            if theme::chip_button(ui, "Folder", false).on_hover_text("open the recordings folder").clicked() {
                open_folder(&self.sc_dir());
            }
        });
        ui.add_space(8.0);
        if self.soundcheck.takes.is_empty() {
            let line = if self.soundcheck.scanning.load(Ordering::Acquire) {
                "Looking for takes…"
            } else {
                "No takes yet. A gig take is what REC writes while rig stems are armed (R on a mixer strip): the master, the stems, every Audio In and the MIDI."
            };
            ui.add(egui::Label::new(egui::RichText::new(line).size(12.5).color(theme::INK_2)).wrap());
            return;
        }
        let rows = self.soundcheck.takes.clone();
        let sel = self.soundcheck.selected.clone();
        let mut pick: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("soundcheck_takes_scroll")
            .max_height(max_h.clamp(120.0, 520.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                for t in &rows {
                    let on = sel.as_deref() == Some(t.folder.as_str());
                    let playing = st.active && st.take.as_deref() == Some(t.folder.as_str());
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(w.min(200.0)), 46.0), egui::Sense::click());
                    self.soundcheck.take_rects.push(r);
                    let fill = if on || resp.hovered() { theme::DARK_PANEL_HI } else { theme::DARK_PANEL };
                    let stroke = if on { theme::MUTED } else { theme::HAIRLINE };
                    let p = ui.painter();
                    p.rect(r, 2.0, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
                    if on {
                        p.rect_filled(Rect::from_min_size(r.min, egui::vec2(3.0, r.height())), 1.0, theme::ACCENT);
                    }
                    let tw = r.width() - 26.0;
                    let g1 = fit(ui, &t.name, 13.0, if on { theme::INK } else { theme::INK_2 }, tw - if playing { 70.0 } else { 0.0 });
                    ui.painter().galley(egui::pos2(r.left() + 14.0, r.top() + 7.0), g1, theme::INK);
                    let g2 = fit(ui, &take_meta(t), 11.5, theme::INK_2, tw);
                    ui.painter().galley(egui::pos2(r.left() + 14.0, r.top() + 25.0), g2, theme::INK_2);
                    if playing {
                        let g = fit(ui, "PLAYING", 10.5, theme::ACCENT_BRIGHT, 64.0);
                        let gx = r.right() - 10.0 - g.size().x;
                        ui.painter().galley(egui::pos2(gx, r.top() + 8.0), g, theme::ACCENT_BRIGHT);
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        pick = Some(t.folder.clone());
                    }
                }
            });
        if let Some(f) = pick {
            self.soundcheck.selected = Some(f);
            self.soundcheck.msg = None;
        }
    }

    fn sc_transport_column(&mut self, ui: &mut egui::Ui, st: &Status) {
        let w = ui.available_width();
        let sel = self.soundcheck.selected.clone();
        let row = sel.as_ref().and_then(|f| self.soundcheck.takes.iter().find(|t| &t.folder == f)).cloned();
        theme::section_label(ui, "Transport");
        ui.add_space(8.0);

        if st.active {
            // The easiest mistake at a gig: forgetting the soundcheck is on.
            let (r, _) = ui.allocate_exact_size(egui::vec2(w, 40.0), egui::Sense::hover());
            ui.painter().rect(r, 2.0, theme::DANGER_BG.gamma_multiply(0.0), Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
            let g = fit(ui, "INPUTS FROM TAKE — the stage is not live", 13.0, theme::ACCENT_BRIGHT, r.width() - 24.0);
            ui.painter().galley(egui::pos2(r.left() + 12.0, r.center().y - g.size().y / 2.0), g, theme::ACCENT_BRIGHT);
            ui.add_space(10.0);
            self.sc_position(ui, st, w);
            ui.add_space(12.0);
            let stop = theme::primary_button(ui, "■ Stop — back to the stage").on_hover_text("the Audio In units hear their devices again");
            self.soundcheck.stop_rect = stop.rect;
            if stop.clicked() {
                self.sc_stop();
            }
        } else {
            let what = row.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| "no take selected".into());
            ui.add(egui::Label::new(egui::RichText::new(what).size(14.0).color(theme::INK)).truncate());
            if let Some(t) = &row {
                ui.add_space(2.0);
                ui.add(egui::Label::new(egui::RichText::new(take_meta(t)).size(12.0).color(theme::INK_2)).truncate());
            }
            ui.add_space(10.0);
            let can = row.is_some() && self.sc_shared().is_some();
            ui.add_enabled_ui(can, |ui| {
                let start = theme::primary_button(ui, "▶ Start soundcheck")
                    .on_hover_text("the Audio In units play the take's inputs until you stop it");
                self.soundcheck.start_rect = start.rect;
                if start.clicked() {
                    if let Some(f) = &sel {
                        let _ = self.sc_call("soundcheck_start", json!({"take": f}));
                    }
                }
            });
            ui.add_space(6.0);
            let hint = if self.sc_shared().is_none() {
                "Needs the audio engine."
            } else {
                "The inputs hear the take instead of the stage until you stop."
            };
            ui.add(egui::Label::new(egui::RichText::new(hint).size(12.0).color(theme::INK_2)).wrap());
        }

        if let Some((m, bad)) = self.soundcheck.msg.clone().or_else(|| st.error.clone().map(|e| (e, true))) {
            ui.add_space(8.0);
            ui.add(egui::Label::new(egui::RichText::new(m).size(12.5).color(if bad { theme::CLIP_BRIGHT } else { theme::LIVE_TEAL })).wrap());
        }

        ui.add_space(18.0);
        self.sc_markers_block(ui, st);
        ui.add_space(18.0);
        self.sc_export_block(ui, row.as_ref());
    }

    /// Position line, the seek bar and the loop.
    fn sc_position(&mut self, ui: &mut egui::Ui, st: &Status, w: f32) {
        let tail = match (&st.marker_label, st.ended) {
            (_, true) => "  ·  take ended: seek or stop".to_string(),
            (Some(l), _) => format!("  ·  {l}"),
            _ => String::new(),
        };
        let line = format!("{} / {}{}", mmss(st.seconds), mmss(st.length_seconds), tail);
        ui.add(egui::Label::new(egui::RichText::new(line).size(13.0).color(theme::INK)).truncate());
        ui.add_space(6.0);
        let (bar, resp) = ui.allocate_exact_size(egui::vec2(w, 22.0), egui::Sense::click());
        self.soundcheck.bar_rect = bar;
        let p = ui.painter();
        p.rect_filled(bar, 2.0, theme::PAPER_SINK);
        let len = st.length_seconds.max(0.001);
        let x_of = |s: f64| bar.left() + (s / len).clamp(0.0, 1.0) as f32 * bar.width();
        if let Some((a, b)) = st.loop_seconds {
            p.rect_filled(Rect::from_x_y_ranges(x_of(a)..=x_of(b), bar.y_range()), 0.0, alpha(theme::LIVE_TEAL, 0.28));
        }
        p.rect_filled(Rect::from_x_y_ranges(bar.left()..=x_of(st.seconds), bar.y_range()), 2.0, alpha(theme::ACCENT, 0.55));
        for m in &st.markers {
            let x = x_of(m.seconds);
            p.line_segment([egui::pos2(x, bar.top()), egui::pos2(x, bar.bottom())], Stroke::new(1.0, theme::CREAM_DIM));
        }
        let x = x_of(st.seconds);
        p.line_segment([egui::pos2(x, bar.top() - 2.0), egui::pos2(x, bar.bottom() + 2.0)], Stroke::new(2.0, theme::INK));
        p.rect_stroke(bar, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
        let hit = resp.on_hover_text("click to jump there").clicked_by(egui::PointerButton::Primary);
        if hit {
            if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                let s = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64 * len;
                let _ = self.sc_call("soundcheck_seek", json!({"seconds": s}));
            }
        }
        if st.underruns > 0 {
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(format!("{} — the disk was late; the take paused for it", plural(st.underruns as usize, "underrun", "underruns"))).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
        }
    }

    fn sc_markers_block(&mut self, ui: &mut egui::Ui, st: &Status) {
        let w = ui.available_width();
        let markers = self.sc_markers(st);
        theme::section_label(ui, "Markers");
        ui.add_space(6.0);
        if markers.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new("This take has no song or part switches.").size(12.5).color(theme::INK_2)).wrap());
            return;
        }
        let sel = self.soundcheck.selected.clone();
        let hint = if st.active { "Click a marker to jump there; Loop repeats its section." } else { "Click a marker to start the soundcheck there; Loop starts it looping." };
        ui.add(egui::Label::new(egui::RichText::new(hint).size(12.0).color(theme::INK_2)).wrap());
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        for (i, m) in markers.iter().enumerate() {
            let here = st.active && st.marker == Some(i);
            let looping = st.loop_seconds.is_some_and(|(a, b)| (a - m.start).abs() < 0.01 && (b - m.end).abs() < 0.01);
            let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click());
            self.soundcheck.marker_rects.push(r);
            let chip_w = 56.0;
            let chip = Rect::from_min_size(egui::pos2(r.right() - chip_w - 8.0, r.center().y - 11.0), egui::vec2(chip_w, 22.0));
            self.soundcheck.loop_rects.push(chip);
            let chip_resp = ui.interact(chip, egui::Id::new(("sc_loop", i)), egui::Sense::click());
            let fill = if here || resp.hovered() { theme::DARK_PANEL_HI } else { theme::DARK_PANEL };
            let p = ui.painter();
            p.rect(r, 2.0, fill, Stroke::new(1.0, if here { theme::ACCENT } else { theme::HAIRLINE }), egui::StrokeKind::Inside);
            let range = format!("{}–{}", mmss(m.start), mmss(m.end));
            let rg = fit(ui, &range, 11.5, theme::INK_2, 90.0);
            let tw = r.width() - 24.0 - chip_w - 8.0 - rg.size().x - 8.0;
            let lg = fit(ui, &m.label, 13.0, if here { theme::INK } else { theme::INK_2 }, tw);
            ui.painter().galley(egui::pos2(r.left() + 12.0, r.center().y - lg.size().y / 2.0), lg, theme::INK);
            let rx = r.right() - chip_w - 16.0 - rg.size().x;
            ui.painter().galley(egui::pos2(rx, r.center().y - rg.size().y / 2.0), rg, theme::INK_2);
            let (cf, cc) = if looping { (theme::LIVE_TEAL, theme::DARK_PANEL) } else if chip_resp.hovered() { (theme::DARK_PANEL_HI, theme::INK) } else { (theme::PAPER_SINK, theme::INK_2) };
            ui.painter().rect(chip, 2.0, cf, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
            ui.painter().text(chip.center(), egui::Align2::CENTER_CENTER, "Loop", FontId::proportional(11.5), cc);
            if chip_resp.on_hover_text(if looping { "stop looping this section" } else { "loop this section" }).clicked() {
                if st.active {
                    let args = if looping { json!({"off": true}) } else { json!({"marker": i}) };
                    let _ = self.sc_call("soundcheck_loop", args);
                } else if let Some(f) = &sel {
                    let _ = self.sc_call("soundcheck_start", json!({"take": f, "marker": i, "loop": true}));
                }
            } else if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                if st.active {
                    let _ = self.sc_call("soundcheck_seek", json!({"marker": i}));
                } else if let Some(f) = &sel {
                    let _ = self.sc_call("soundcheck_start", json!({"take": f, "marker": i}));
                }
            }
        }
    }

    fn sc_export_block(&mut self, ui: &mut egui::Ui, row: Option<&TakeRow>) {
        theme::section_label(ui, "Export");
        ui.add_space(6.0);
        ui.add(egui::Label::new(egui::RichText::new("Mixdown, stems and raw inputs as WAV files for a DAW, the whole take or one section.").size(12.0).color(theme::INK_2)).wrap());
        ui.add_space(8.0);
        ui.add_enabled_ui(row.is_some(), |ui| {
            let b = theme::chip_button(ui, "Export…", false);
            self.soundcheck.export_rect = b.rect;
            if b.clicked() {
                self.soundcheck.export_open = true;
                self.soundcheck.draft.section = None;
            }
        });
        ui.add_space(6.0);
        self.sc_export_status(ui);
    }

    /// Progress while one runs; its folder when done; why when it failed.
    fn sc_export_status(&mut self, ui: &mut egui::Ui) {
        let ex = soundcheck::export_state();
        if ex.running {
            let w = ui.available_width();
            let (bar, _) = ui.allocate_exact_size(egui::vec2(w, 12.0), egui::Sense::hover());
            let p = ui.painter();
            p.rect_filled(bar, 2.0, theme::PAPER_SINK);
            p.rect_filled(Rect::from_min_size(bar.min, egui::vec2(bar.width() * ex.progress.clamp(0.0, 1.0), bar.height())), 2.0, theme::ACCENT);
            p.rect_stroke(bar, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
            ui.add_space(4.0);
            ui.label(egui::RichText::new(format!("Exporting… {:.0} %", ex.progress * 100.0)).size(12.0).color(theme::INK_2));
            return;
        }
        match ex.last {
            Some(Ok(x)) => {
                ui.add(egui::Label::new(egui::RichText::new(format!("Exported {} to {}", plural(x.files.len(), "file", "files"), x.folder)).size(12.5).color(theme::LIVE_TEAL)).wrap());
                ui.add_space(4.0);
                if theme::chip_button(ui, "Open folder", false).clicked() {
                    open_folder(std::path::Path::new(&x.folder));
                }
            }
            Some(Err(e)) => {
                ui.add(egui::Label::new(egui::RichText::new(format!("Export failed: {e}")).size(12.5).color(theme::CLIP_BRIGHT)).wrap());
            }
            None => {}
        }
    }

    /// The export dialog. Its state lives until it is decided (rule 153);
    /// Escape closes it through the modal (rule 213).
    pub(super) fn soundcheck_modal(&mut self, ctx: &egui::Context) {
        if !self.soundcheck.export_open {
            return;
        }
        let Some(folder) = self.soundcheck.selected.clone() else {
            self.soundcheck.export_open = false;
            return;
        };
        let row = self.soundcheck.takes.iter().find(|t| t.folder == folder).cloned();
        let st = self.sc_status();
        let markers = self.sc_markers(&st);
        let mut d = self.soundcheck.draft.clone();
        let mut run = false;
        let mut close = false;
        self.soundcheck.section_rects.clear();
        let mut run_rect = Rect::NOTHING;
        let ex = soundcheck::export_state();
        let avail_h = ctx.content_rect().height();
        let list_h = (avail_h * 0.26).clamp(60.0, 190.0);
        let modal = egui::Modal::new(egui::Id::new("soundcheck_export_modal")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 48.0).clamp(280.0, 440.0));
            ui.label(egui::RichText::new("Export take").font(theme::medium_font(16.0)).color(theme::INK));
            if let Some(t) = &row {
                ui.add(egui::Label::new(egui::RichText::new(format!("{} · {}", t.name, take_meta(t))).size(12.0).color(theme::INK_2)).truncate());
            }
            ui.add_space(10.0);
            theme::section_label(ui, "Files");
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if theme::wrapped_chip_button(ui, "Mixdown", d.mixdown).clicked() { d.mixdown = !d.mixdown; }
                if theme::wrapped_chip_button(ui, "Stems", d.stems).clicked() { d.stems = !d.stems; }
                if theme::wrapped_chip_button(ui, "Raw inputs", d.inputs).clicked() { d.inputs = !d.inputs; }
            });
            ui.add_space(10.0);
            theme::section_label(ui, "Range");
            ui.add_space(6.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            egui::ScrollArea::vertical().id_salt("soundcheck_export_range").max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
                let r = Self::load_target_row(ui, "Whole take", d.section.is_none());
                self.soundcheck.section_rects.push(r.rect);
                if r.clicked() { d.section = None; }
                for (i, m) in markers.iter().enumerate() {
                    let label = format!("{} · {}–{}", m.label, mmss(m.start), mmss(m.end));
                    let r = Self::load_target_row(ui, &label, d.section == Some(i));
                    self.soundcheck.section_rects.push(r.rect);
                    if r.clicked() { d.section = Some(i); }
                }
            });
            ui.add_space(10.0);
            theme::section_label(ui, "Bit depth");
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for (id, label) in [("pcm24", "24-bit"), ("pcm16", "16-bit"), ("float32", "32-bit float")] {
                    if theme::wrapped_chip_button(ui, label, d.format == id).clicked() { d.format = id; }
                }
            });
            ui.add_space(12.0);
            if ex.running {
                let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 12.0), egui::Sense::hover());
                ui.painter().rect_filled(bar, 2.0, theme::PAPER_SINK);
                ui.painter().rect_filled(Rect::from_min_size(bar.min, egui::vec2(bar.width() * ex.progress.clamp(0.0, 1.0), bar.height())), 2.0, theme::ACCENT);
                ui.painter().rect_stroke(bar, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("Exporting… {:.0} %", ex.progress * 100.0)).size(12.0).color(theme::INK_2));
                ui.add_space(8.0);
            } else {
                match &ex.last {
                    Some(Ok(x)) => {
                        ui.add(egui::Label::new(egui::RichText::new(format!("Done: {} in {}", plural(x.files.len(), "file", "files"), x.folder)).size(12.0).color(theme::LIVE_TEAL)).wrap());
                        ui.add_space(8.0);
                    }
                    Some(Err(e)) => {
                        ui.add(egui::Label::new(egui::RichText::new(format!("Export failed: {e}")).size(12.0).color(theme::CLIP_BRIGHT)).wrap());
                        ui.add_space(8.0);
                    }
                    None => {}
                }
            }
            if let Some((m, true)) = &self.soundcheck.msg {
                ui.add(egui::Label::new(egui::RichText::new(m.clone()).size(12.0).color(theme::CLIP_BRIGHT)).wrap());
                ui.add_space(8.0);
            }
            let any = d.mixdown || d.stems || d.inputs;
            ui.horizontal(|ui| {
                ui.add_enabled_ui(any && !ex.running, |ui| {
                    let b = theme::primary_button(ui, "Export");
                    run_rect = b.rect;
                    if b.clicked() { run = true; }
                });
                ui.add_space(8.0);
                if theme::chip_button(ui, if matches!(&ex.last, Some(Ok(_))) && !ex.running { "Done" } else { "Close" }, false).clicked() { close = true; }
                if let (Some(Ok(x)), false) = (&ex.last, ex.running) {
                    if theme::chip_button(ui, "Open folder", false).clicked() {
                        open_folder(std::path::Path::new(&x.folder));
                    }
                }
            });
        });
        self.soundcheck.run_export_rect = run_rect;
        self.soundcheck.draft = d.clone();
        if modal.should_close() || close {
            self.soundcheck.export_open = false;
            return;
        }
        if run {
            let mut args = json!({"take": folder, "mixdown": d.mixdown, "stems": d.stems, "inputs": d.inputs, "format": d.format});
            if let Some(i) = d.section {
                args["marker"] = json!(i);
            }
            let _ = self.sc_call("export_take", args);
        }
        if soundcheck::export_state().running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::take::{InputFile, Manifest, Marker, StemFile};
    use super::super::settings_and_dropdown_tests::{click_at, run_frame};

    pub(in crate::ui) const SIZE: Vec2 = Vec2::new(1280.0, 800.0);

    /// A take of `seconds` with a master, a stem, a raw input and three
    /// markers, one second apart.
    pub(in crate::ui) fn write_take(dir: &std::path::Path, name: &str, seconds: usize) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let frames = 48_000 * seconds;
        let wave = |amp: f32| (0..frames).flat_map(|i| { let v = amp * ((i as f32) * 0.05).sin(); [v, -v] }).collect::<Vec<f32>>();
        crate::take::write_test_wav(&folder.join("master.wav"), 48_000, &wave(0.4));
        crate::take::write_test_wav(&folder.join("Rig A.wav"), 48_000, &wave(0.3));
        crate::take::write_test_wav(&folder.join("input-1 Vocal.wav"), 48_000, &wave(0.2));
        let marker = |s: u64, song: &str, part: &str, i: usize| Marker { frame: s * 48_000, part: Some(i), song: song.into(), name: part.into() };
        Manifest {
            version: 1,
            sample_rate: 48_000,
            frames: frames as u64,
            master: Some("master.wav".into()),
            stems: vec![StemFile { rig: 0, name: "Rig A".into(), file: "Rig A.wav".into() }],
            inputs: vec![InputFile { node: 1, name: "Vocal".into(), file: "input-1 Vocal.wav".into() }],
            markers: if seconds >= 3 {
                vec![marker(0, "Song A", "Verse", 0), marker(1, "Song A", "Chorus", 1), marker(2, "Song B", "Intro", 2)]
            } else {
                vec![marker(0, "Song A", "Verse", 0)]
            },
            midi_events: 12,
            ..Manifest::default()
        }
        .write(&folder)
        .unwrap();
        folder
    }

    pub(in crate::ui) fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ksalat-sc-ui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An offline engine with the soundcheck attached, and the shared side.
    pub(in crate::ui) fn engine_with_soundcheck() -> (crate::engine::GraphEngine, crate::control::CommandBus, Arc<SoundcheckShared>) {
        use crate::control::{command_bus, name_bytes, AudioInChannel, Graph, NodeKind, PortKind, PortRef};
        let mut g = Graph::default();
        let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("VOCAL"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: ain, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        let (bus, drain) = command_bus();
        let mut engine = crate::engine::GraphEngine::new(g, drain, 48_000.0, vec![]);
        let (feed, shared) = soundcheck::channel(48_000);
        engine.set_soundcheck(feed);
        (engine, bus, shared)
    }

    /// What the layout audits and the shots show: no thread, no disk.
    #[derive(Clone, Copy, PartialEq)]
    pub(in crate::ui) enum Scene {
        Empty,
        Ready,
        Playing,
        Export,
    }

    pub(in crate::ui) fn seed(app: &mut DemoApp, scene: Scene) {
        app.stage_view = StageView::Soundcheck;
        let p = &mut app.soundcheck;
        p.scanned = true;
        if scene == Scene::Empty {
            return;
        }
        let row = |name: &str, seconds: f64, stems: usize, inputs: usize, markers: usize| TakeRow {
            folder: format!("/takes/{name}"),
            name: name.into(),
            seconds,
            stems,
            inputs,
            markers,
            midi_events: 240,
        };
        p.takes = vec![
            row("2026-11-21 21-14-05", 221.0, 3, 2, 4),
            row("2026-11-21 20-02-40", 1542.0, 3, 2, 3),
            row("2026-11-14 19-48-12", 312.0, 2, 1, 1),
            row("2026-11-07 18-30-00", 95.0, 1, 0, 0),
        ];
        let folder = p.takes[0].folder.clone();
        p.selected = Some(folder.clone());
        let marker = |label: &str, start: f64, end: f64| MarkerRow { label: label.into(), start, end };
        let markers = vec![
            marker("Intro · Count in", 0.0, 24.0),
            marker("Song A · Verse", 24.0, 61.0),
            marker("Song A · Chorus", 61.0, 118.0),
            marker("Song B · A really long part name that has to be cut", 118.0, 221.0),
        ];
        p.detail_for = Some(folder.clone());
        p.detail = Some((folder.clone(), markers.clone()));
        if scene == Scene::Playing || scene == Scene::Export {
            p.status_override = Some(Status {
                active: true,
                playing: true,
                take: Some(folder),
                seconds: 83.0,
                length_seconds: 221.0,
                marker: Some(2),
                marker_label: Some("Song A · Chorus".into()),
                markers: markers
                    .iter()
                    .enumerate()
                    .map(|(index, m)| soundcheck::MarkerStatus { index, label: m.label.clone(), seconds: m.start, end_seconds: m.end })
                    .collect(),
                loop_seconds: Some((61.0, 118.0)),
                inputs: vec!["Vocal".into(), "Keys".into()],
                midi_events: 240,
                ..Status::default()
            });
        }
        p.export_open = scene == Scene::Export;
    }

    fn render(engine: &mut crate::engine::GraphEngine, blocks: usize) {
        let mut out = vec![0.0f32; 512];
        for _ in 0..blocks {
            engine.render(&mut out, 2);
        }
    }

    fn frames(app: &mut DemoApp, ctx: &egui::Context, n: usize) -> Vec<(String, Rect)> {
        let mut t = Vec::new();
        for _ in 0..n {
            t = run_frame(app, ctx, SIZE, vec![]).0;
        }
        t
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let t0 = std::time::Instant::now();
        while !done() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn the_panel_walks_a_take_from_the_list_to_the_export() {
        let dir = scratch("walk");
        let newest = write_take(&dir, "2026-11-24 20-00-00", 3);
        write_take(&dir, "2026-11-24 19-00-00", 2);
        let (mut engine, _bus, shared) = engine_with_soundcheck();
        render(&mut engine, 2);

        let mut app = DemoApp::new_with_sink(None);
        app.soundcheck.dir = Some(dir.clone());
        app.soundcheck.shared = Some(shared.clone());
        app.stage_view = StageView::Soundcheck;
        let ctx = egui::Context::default();

        // List: both takes, newest first, with what each holds (read off the UI thread).
        let mut texts = Vec::new();
        wait_for("the takes scan", || {
            texts = frames(&mut app, &ctx, 1);
            app.soundcheck.takes.len() == 2
        });
        let texts = frames(&mut app, &ctx, 3).into_iter().chain(texts).map(|t| t.0).collect::<Vec<_>>();
        assert_eq!(app.soundcheck.takes[0].name, "2026-11-24 20-00-00", "newest first");
        assert_eq!(app.soundcheck.takes[1].name, "2026-11-24 19-00-00");
        assert!(texts.iter().any(|t| t.starts_with("0:03 · 1 stem · 1 raw input · 3 markers")), "{texts:?}");
        assert_eq!(app.soundcheck.take_rects.len(), 2);
        assert_eq!(app.soundcheck.selected.as_deref(), Some(newest.to_str().unwrap()), "the newest take is selected");

        // The markers of the selected take are listed before it plays (worker thread).
        wait_for("the take's markers", || {
            frames(&mut app, &ctx, 1);
            app.soundcheck.marker_rects.len() == 3
        });
        assert!(!shared.engaged(), "listing starts nothing");

        // Start.
        assert!(app.soundcheck.start_rect.is_positive());
        { let at = app.soundcheck.start_rect.center(); click_at(&mut app, &ctx, SIZE, at); }
        assert!(shared.engaged(), "Start soundcheck starts the take");
        assert_eq!(shared.status().take.as_deref(), Some(newest.to_str().unwrap()));
        let texts = frames(&mut app, &ctx, 3);
        assert!(texts.iter().any(|t| t.0.starts_with("INPUTS FROM TAKE")), "the view says the stage is not live");
        assert!(texts.iter().any(|t| t.0 == "■ Soundcheck"), "the top bar says so too: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());
        assert!(app.soundcheck.stop_rect.is_positive());

        // The engine plays; the panel follows the position.
        assert!(shared.wait_primed(std::time::Duration::from_secs(5)));
        render(&mut engine, 40);
        assert!(shared.status().seconds > 0.1, "the engine advances the take");

        // Seek: a click on a marker jumps to its start.
        assert_eq!(app.soundcheck.marker_rects.len(), 3);
        { let at = app.soundcheck.marker_rects[1].center() - Vec2::new(40.0, 0.0); click_at(&mut app, &ctx, SIZE, at); }
        let st = shared.status();
        assert!((st.seconds - 1.0).abs() < 0.01 && st.marker == Some(1), "seek to marker 1: {st:?}");

        // Loop: the chip loops that marker's section; a second click ends the loop.
        { let at = app.soundcheck.loop_rects[2].center(); click_at(&mut app, &ctx, SIZE, at); }
        let st = shared.status();
        let (a, b) = st.loop_seconds.expect("loop set");
        assert!((a - 2.0).abs() < 0.01 && (b - 3.0).abs() < 0.01, "loop of marker 2: {a}..{b}");
        frames(&mut app, &ctx, 2);
        { let at = app.soundcheck.loop_rects[2].center(); click_at(&mut app, &ctx, SIZE, at); }
        assert!(shared.status().loop_seconds.is_none(), "the loop is off again");

        // A click on the position bar seeks to that second.
        let bar = app.soundcheck.bar_rect;
        { let at = egui::pos2(bar.left() + bar.width() * 0.25, bar.center().y); click_at(&mut app, &ctx, SIZE, at); }
        assert!((shared.status().seconds - 0.75).abs() < 0.05, "bar seek: {}", shared.status().seconds);

        // Stop: the inputs are the stage's again.
        frames(&mut app, &ctx, 2);
        { let at = app.soundcheck.stop_rect.center(); click_at(&mut app, &ctx, SIZE, at); }
        assert!(!shared.engaged(), "Stop hands the inputs back");
        let texts = frames(&mut app, &ctx, 3);
        assert!(!texts.iter().any(|t| t.0 == "■ Soundcheck" || t.0.starts_with("INPUTS FROM TAKE")));

        // Export one section as 24-bit, with the raw input, off the UI thread.
        assert!(app.soundcheck.export_rect.is_positive());
        { let at = app.soundcheck.export_rect.center(); click_at(&mut app, &ctx, SIZE, at); }
        assert!(app.soundcheck.export_open, "Export… opens the dialog");
        frames(&mut app, &ctx, 3);
        assert_eq!(app.soundcheck.section_rects.len(), 4, "whole take + 3 markers");
        { let at = app.soundcheck.section_rects[2].center(); click_at(&mut app, &ctx, SIZE, at); }
        assert_eq!(app.soundcheck.draft.section, Some(1));
        app.soundcheck.draft.inputs = true;
        frames(&mut app, &ctx, 2);
        { let at = app.soundcheck.run_export_rect.center(); click_at(&mut app, &ctx, SIZE, at); }
        wait_for("the export", || {
            frames(&mut app, &ctx, 1);
            matches!(soundcheck::export_state().last, Some(_)) && !soundcheck::export_state().running
        });
        let ex = soundcheck::export_state().last.unwrap().expect("export succeeded");
        let names: Vec<String> = ex.files.iter().map(|f| std::path::Path::new(f).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"mixdown.wav".to_string()) && names.iter().any(|n| n.starts_with("stem")) && names.iter().any(|n| n.starts_with("input")), "{names:?}");
        let len = std::fs::metadata(ex.files.iter().find(|f| f.ends_with("mixdown.wav")).unwrap()).unwrap().len();
        assert_eq!(len, 44 + 48_000 * 2 * 3, "one second of 24-bit stereo");
        assert!((ex.seconds - 1.0).abs() < 1e-6);
        let texts = frames(&mut app, &ctx, 2);
        assert!(texts.iter().any(|t| t.0.starts_with("Done: 3 files")), "the dialog names the result");

        // Escape closes the dialog (rule 213), the stage stays.
        run_frame(&mut app, &ctx, SIZE, super::super::settings_and_dropdown_tests::key(Key::Escape));
        run_frame(&mut app, &ctx, SIZE, vec![]);
        assert!(!app.soundcheck.export_open);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_playing_take_marks_the_top_bar_and_every_audio_in_and_one_click_goes_back() {
        let dir = scratch("onair");
        let take = write_take(&dir, "2026-11-24 21-00-00", 3);
        let (_engine, _bus, shared) = engine_with_soundcheck();
        let mut app = DemoApp::new_with_sink(None);
        let mut g = crate::control::Graph::default();
        let _keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("AUDIO IN"));
        g.connect_with_id(1, PortRef { node: ain, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
        app.build_rack(&g, &[]);
        app.soundcheck.dir = Some(dir.clone());
        app.soundcheck.shared = Some(shared.clone());
        let ctx = egui::Context::default();
        let before = frames(&mut app, &ctx, 4);
        assert!(!before.iter().any(|t| t.0 == "FROM TAKE" || t.0 == "■ Soundcheck"), "live stage: no take marks");
        assert!(before.iter().any(|t| t.0 == "TUNE"), "the Audio In face shows its tuner hint");

        let r = soundcheck::tool("soundcheck_start", &json!({"take": take.to_str().unwrap()}), Some(&shared), &dir);
        assert!(r.get("error").and_then(|e| e.as_str()).is_none(), "{r}");
        let after = frames(&mut app, &ctx, 4);
        assert!(after.iter().any(|t| t.0 == "FROM TAKE"), "every Audio In says its input is the take");
        assert!(!after.iter().any(|t| t.0 == "TUNE"));
        let chip = after.iter().find(|t| t.0 == "■ Soundcheck").expect("top bar chip").1;
        click_at(&mut app, &ctx, SIZE, chip.center());
        assert!(!shared.engaged(), "one click on the chip is back to the stage");
        let back = frames(&mut app, &ctx, 3);
        assert!(!back.iter().any(|t| t.0 == "FROM TAKE" || t.0 == "■ Soundcheck"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_k_and_the_record_menu_open_the_view_and_escape_leaves_it() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        frames(&mut app, &ctx, 3);
        let ev = vec![
            egui::Event::Key { key: Key::K, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND },
            egui::Event::Key { key: Key::K, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::COMMAND },
        ];
        run_frame(&mut app, &ctx, SIZE, ev);
        assert_eq!(app.stage_view, StageView::Soundcheck, "Ctrl+K opens the Soundcheck view");
        let t = frames(&mut app, &ctx, 3);
        assert!(t.iter().any(|t| t.0.starts_with("No takes yet")), "empty folder says how takes come to be");
        run_frame(&mut app, &ctx, SIZE, super::super::settings_and_dropdown_tests::key(Key::Escape));
        run_frame(&mut app, &ctx, SIZE, vec![]);
        assert_eq!(app.stage_view, StageView::Rack, "Esc leaves the view");
    }

    /// Text of the shapes painted after the modal's scrim, with its rect.
    fn modal_texts(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) -> Vec<(String, Rect)> {
        // Text a scroll area cuts away is not on screen: keep what its clip shows whole.
        fn walk(shape: &Shape, clip: Rect, out: &mut Vec<(String, Rect)>) {
            match shape {
                Shape::Text(t) => {
                    let r = t.visual_bounding_rect();
                    if r.is_finite() && clip.expand(0.5).contains_rect(r) {
                        out.push((t.galley.text().to_string(), r));
                    }
                }
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, clip, out)),
                _ => {}
            }
        }
        let mut result = Vec::new();
        for i in 0..4 {
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
            raw.time = Some(i as f64);
            eframe::App::raw_input_hook(app, ctx, &mut raw);
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
            // The dialog is painted after everything under it: its text is
            // what follows its own title in paint order.
            let mut all = Vec::new();
            for cs in &out.shapes {
                walk(&cs.shape, cs.clip_rect, &mut all);
            }
            result = match all.iter().position(|t| t.0 == "Export take") {
                Some(at) => all.split_off(at),
                None => Vec::new(),
            };
        }
        result
    }

    /// The export dialog at every window size: its text inside the window
    /// and clear of other text, with a take that has long marker names.
    #[test]
    fn the_export_dialog_fits_every_window_size() {
        let mut bad = Vec::new();
        for (w, h) in super::super::window_size_layout_tests::SIZES {
            let mut app = DemoApp::new_with_sink(None);
            seed(&mut app, Scene::Export);
            let ctx = egui::Context::default();
            let size = Vec2::new(w, h);
            let t = modal_texts(&mut app, &ctx, size);
            assert!(t.iter().any(|t| t.0 == "Export take"), "[{w}x{h}] the dialog is drawn: {t:?}");
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            for (s, r) in &t {
                if !screen.expand(0.5).contains_rect(*r) {
                    bad.push(format!("[{w}x{h}] {s:?} {r:?} leaves the window"));
                }
            }
            for (i, (a, ra)) in t.iter().enumerate() {
                for (b, rb) in &t[i + 1..] {
                    if ra.shrink(1.0).intersects(rb.shrink(1.0)) && ra.width() > 0.0 && rb.width() > 0.0 {
                        bad.push(format!("[{w}x{h}] {a:?} {ra:?} overlaps {b:?} {rb:?}"));
                    }
                }
            }
            for r in app.soundcheck.section_rects.iter().chain([&app.soundcheck.run_export_rect]) {
                if r.is_positive() && !screen.contains_rect(*r) && r.height() > 0.0 && r.top() < h {
                    // rows scrolled out of the list are clipped, not lost
                    continue;
                }
            }
            assert!(app.soundcheck.run_export_rect.is_positive() && screen.contains_rect(app.soundcheck.run_export_rect), "[{w}x{h}] the Export button stays in the window: {:?}", app.soundcheck.run_export_rect);
        }
        assert!(bad.is_empty(), "{} defects:\n{}", bad.len(), bad.join("\n"));
    }

    /// Images of the stage while a take plays (a rack with an Audio In, the
    /// top bar's chip and the face's "FROM TAKE"):
    /// `KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat soundcheck_stage_shots -- --ignored`
    #[test]
    #[ignore = "writes images"]
    fn soundcheck_stage_shots() {
        let dir = PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        for (w, h) in [(1280.0f32, 800.0f32), (760.0, 520.0)] {
            for playing in [false, true] {
                let mut app = DemoApp::new_with_sink(None);
                let mut g = crate::control::Graph::default();
                let _keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
                let ain = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("VOCAL MIC"));
                g.connect_with_id(1, PortRef { node: ain, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
                app.build_rack(&g, &[]);
                if playing {
                    seed(&mut app, Scene::Playing);
                    app.stage_view = StageView::Rack;
                }
                let name = format!("{w}x{h}-rack-{}.png", if playing { "from-take" } else { "live" });
                super::super::offline_shots::shot(&mut app, Vec2::new(w, h), &dir.join(name));
            }
        }
    }
}
