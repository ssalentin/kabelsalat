//! Audio Doctor UI in Settings → Audio (#211), plus Pre-show jump,
//! background worker polling, and the `audio_doctor` agent / MCP bridge tool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::*;
use crate::audio_doctor::{
    self, DoctorFacts, DoctorReport, EngineFacts, FixAction, FixOutcome, LatencyResult, Status,
    XrunDiagnosis, XrunRunSummary,
};

const XRUN_TEST_SECS: f32 = 60.0;

pub(super) struct ActiveXrunRun {
    started_at: Instant,
    target_secs: f32,
    start_xruns: u32,
    start_late: u32,
    peak_dsp: f64,
    worst_render_us: u32,
    heaviest: Option<(String, f64)>,
    started_demo: bool,
    buffer_frames: u32,
    sample_rate: u32,
}

#[derive(Default)]
pub(super) struct AudioDoctorState {
    pub(super) report: Option<DoctorReport>,
    pub(super) error: Option<String>,
    scanning: Option<mpsc::Receiver<Result<DoctorReport, String>>>,
    waiters: Vec<mpsc::Sender<String>>,
    pub(super) answer: Option<mpsc::Receiver<String>>,
    fixing: Option<(&'static str, mpsc::Receiver<FixOutcome>)>,
    pub(super) fix_msg: Option<(bool, String)>,
    latency_running: Option<(Arc<AtomicBool>, mpsc::Receiver<LatencyResult>)>,
    pub(super) latency_result: Option<LatencyResult>,
    pub(super) xrun_run: Option<ActiveXrunRun>,
    pub(super) xrun_result: Option<(XrunRunSummary, XrunDiagnosis)>,
    pub(super) scroll_to_frames: u8,
}

impl AudioDoctorState {
    pub(super) fn scanning(&self) -> bool {
        self.scanning.is_some()
    }

    pub(super) fn is_busy(&self) -> bool {
        self.scanning.is_some()
            || self.fixing.is_some()
            || self.latency_running.is_some()
            || self.xrun_run.is_some()
    }
}

fn status_color(status: Status) -> Color32 {
    match status {
        Status::Ok => theme::LIVE_TEAL,
        Status::Warning => theme::ACCENT_BRIGHT,
        Status::Problem => theme::CLIP_BRIGHT,
    }
}

impl DemoApp {
    fn doctor_engine_facts(&self) -> EngineFacts {
        EngineFacts::from_stats(
            self.audio_backend,
            self.active_audio_backend,
            self.buffer_frames,
            &self.engine_stats,
            self.sample_rate,
        )
    }

    pub(super) fn start_audio_doctor_scan(&mut self) {
        if self.audio_doctor.scanning() {
            return;
        }
        self.audio_doctor.error = None;
        let engine_facts = self.doctor_engine_facts();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kabelsalat-audio-doctor".into())
            .spawn(move || {
                let t0 = Instant::now();
                let facts = audio_doctor::gather_facts(engine_facts);
                let report = DoctorReport::from_facts(facts, t0.elapsed());
                let _ = tx.send(Ok(report));
            });
        match spawned {
            Ok(_) => self.audio_doctor.scanning = Some(rx),
            Err(e) => self.finish_audio_doctor_scan(Err(format!("could not start Audio Doctor thread: {e}"))),
        }
    }

    fn finish_audio_doctor_scan(&mut self, result: Result<DoctorReport, String>) {
        let answer = match result {
            Ok(report) => {
                let json = report.json().to_string();
                self.audio_doctor.report = Some(report);
                json
            }
            Err(e) => {
                let msg = format!("error: Audio Doctor failed: {e}");
                self.audio_doctor.error = Some(e);
                msg
            }
        };
        for w in self.audio_doctor.waiters.drain(..) {
            let _ = w.send(answer.clone());
        }
    }

    pub(super) fn audio_doctor_request(&mut self, reply: mpsc::Sender<String>) {
        self.audio_doctor.waiters.push(reply);
        self.start_audio_doctor_scan();
    }

    pub(super) fn audio_doctor_tool(&mut self) -> String {
        let (tx, rx) = mpsc::channel();
        self.audio_doctor_request(tx);
        self.audio_doctor.answer = Some(rx);
        "running Audio Doctor".into()
    }

    /// Jump straight to Settings → Audio → Audio Doctor and ensure a scan runs.
    pub(super) fn open_audio_doctor(&mut self) {
        self.stage_view = StageView::Settings;
        self.settings_page = SettingsPage::Audio;
        self.audio_doctor.scroll_to_frames = 2;
        if self.audio_doctor.report.is_none() && !self.audio_doctor.scanning() {
            self.start_audio_doctor_scan();
        }
    }

    fn start_audio_doctor_fix(&mut self, check_id: &'static str, action: FixAction) {
        if self.audio_doctor.fixing.is_some() {
            return;
        }
        self.audio_doctor.fix_msg = None;
        if let FixAction::RaiseBufferSize { frames } = action {
            match self.set_buffer_frames(frames) {
                Ok(msg) => {
                    self.audio_doctor.fix_msg = Some((true, msg));
                    self.start_audio_doctor_scan();
                }
                Err(e) => {
                    self.audio_doctor.fix_msg = Some((false, format!("Buffer change failed: {e}")));
                }
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kabelsalat-doctor-fix".into())
            .spawn(move || {
                let _ = tx.send(audio_doctor::apply_fix(&action));
            });
        match spawned {
            Ok(_) => self.audio_doctor.fixing = Some((check_id, rx)),
            Err(e) => {
                self.audio_doctor.fix_msg = Some((false, format!("Could not start fix thread: {e}")));
            }
        }
    }

    fn start_latency_measure(&mut self) {
        if self.audio_doctor.latency_running.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = cancel.clone();
        let backend = self.audio_backend;
        let out_dev = self.wanted_output.lock().ok().and_then(|w| w.clone());
        let rate = if self.sample_rate > 0.0 {
            self.sample_rate.round() as u32
        } else {
            48_000
        };
        let buf = self.buffer_frames;
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kabelsalat-doctor-latency".into())
            .spawn(move || {
                let res = audio_doctor::measure_round_trip(
                    backend,
                    out_dev.as_deref(),
                    None,
                    rate,
                    buf,
                    cancel_bg,
                );
                let _ = tx.send(res);
            });
        match spawned {
            Ok(_) => self.audio_doctor.latency_running = Some((cancel, rx)),
            Err(e) => {
                self.audio_doctor.latency_result = Some(LatencyResult::Failed(format!(
                    "Could not start latency measurement thread: {e}"
                )));
            }
        }
    }

    fn stop_latency_measure(&mut self) {
        if let Some((cancel, _)) = &self.audio_doctor.latency_running {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn start_xrun_test(&mut self) {
        if self.audio_doctor.xrun_run.is_some() {
            return;
        }
        self.audio_doctor.xrun_result = None;
        let c = self.engine_counters();
        let mut started_demo = false;
        if let Some(bus) = &self.bus {
            let demo = self
                .rig_demo_phrase(self.active_rig)
                .map(|k| (self.active_rig, k))
                .or_else(|| {
                    (0..self.rig_count())
                        .find_map(|r| self.rig_demo_phrase(r).map(|k| (r, k)))
                });
            if let Some((rig, phrase)) = demo {
                if let Ok(mut b) = bus.lock() {
                    b.send(Command::DemoPlay { rig: rig as u8, phrase });
                    started_demo = true;
                }
            }
        }
        let sample_rate = if c.sample_rate > 0 {
            c.sample_rate
        } else {
            self.sample_rate.round().max(1.0) as u32
        };
        let buffer_frames = if c.callback_frames > 0 {
            c.callback_frames
        } else {
            self.buffer_frames
        };
        self.audio_doctor.xrun_run = Some(ActiveXrunRun {
            started_at: Instant::now(),
            target_secs: XRUN_TEST_SECS,
            start_xruns: c.xruns,
            start_late: c.render_late,
            peak_dsp: self.engine_stats.dsp_percent,
            worst_render_us: c.render_max_us,
            heaviest: self.engine_stats.nodes.first().map(|n| (n.name.clone(), n.percent)),
            started_demo,
            buffer_frames,
            sample_rate,
        });
    }

    pub(super) fn finish_xrun_test(&mut self, stopped_early: bool) {
        let Some(run) = self.audio_doctor.xrun_run.take() else { return };
        if run.started_demo {
            if let Some(bus) = &self.bus {
                if let Ok(mut b) = bus.lock() {
                    b.send(Command::DemoStop);
                }
            }
        }
        let c = self.engine_counters();
        let summary = XrunRunSummary {
            elapsed_secs: run.started_at.elapsed().as_secs_f32().min(run.target_secs),
            target_secs: run.target_secs,
            stopped_early,
            xruns: c.xruns.saturating_sub(run.start_xruns),
            late_blocks: c.render_late.saturating_sub(run.start_late),
            peak_dsp_percent: run.peak_dsp.max(self.engine_stats.dsp_percent),
            worst_render_us: run.worst_render_us.max(c.render_max_us),
            buffer_frames: run.buffer_frames,
            sample_rate: run.sample_rate,
            heaviest_node: run.heaviest,
        };
        let facts = self
            .audio_doctor
            .report
            .as_ref()
            .map(|r| r.facts.clone())
            .unwrap_or_else(|| DoctorFacts {
                engine: self.doctor_engine_facts(),
                ..Default::default()
            });
        let diag = audio_doctor::diagnose_xrun_run(&summary, &facts);
        self.audio_doctor.xrun_result = Some((summary, diag));
    }

    pub(super) fn poll_audio_doctor(&mut self) {
        if let Some(rx) = &self.audio_doctor.scanning {
            match rx.try_recv() {
                Ok(res) => {
                    self.audio_doctor.scanning = None;
                    self.finish_audio_doctor_scan(res);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.audio_doctor.scanning = None;
                    self.finish_audio_doctor_scan(Err("Audio Doctor stopped before finishing".into()));
                }
            }
        }

        if let Some((_, rx)) = &self.audio_doctor.fixing {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.audio_doctor.fixing = None;
                    self.audio_doctor.fix_msg = Some((outcome.applied, outcome.message));
                    self.start_audio_doctor_scan();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.audio_doctor.fixing = None;
                    self.audio_doctor.fix_msg = Some((false, "Fix thread ended unexpectedly.".into()));
                }
            }
        }

        if let Some((_, rx)) = &self.audio_doctor.latency_running {
            match rx.try_recv() {
                Ok(res) => {
                    self.audio_doctor.latency_running = None;
                    self.audio_doctor.latency_result = Some(res);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.audio_doctor.latency_running = None;
                    self.audio_doctor.latency_result = Some(LatencyResult::Failed(
                        "Latency measurement stopped unexpectedly.".into(),
                    ));
                }
            }
        }

        let c = self.engine_counters();
        let dsp = self.engine_stats.dsp_percent;
        let top_node = self.engine_stats.nodes.first().map(|n| (n.name.clone(), n.percent));
        let mut finish_auto = false;
        if let Some(run) = &mut self.audio_doctor.xrun_run {
            if dsp > run.peak_dsp {
                run.peak_dsp = dsp;
            }
            if c.render_max_us > run.worst_render_us {
                run.worst_render_us = c.render_max_us;
            }
            if let Some((name, pct)) = top_node {
                let replace = run.heaviest.as_ref().is_none_or(|(_, p)| pct > *p);
                if replace {
                    run.heaviest = Some((name, pct));
                }
            }
            if run.started_at.elapsed().as_secs_f32() >= run.target_secs {
                finish_auto = true;
            }
        }
        if finish_auto {
            self.finish_xrun_test(false);
        }
    }

    pub(super) fn settings_audio_doctor_section(&mut self, ui: &mut egui::Ui) {
        let scanning = self.audio_doctor.scanning();
        let has_report = self.audio_doctor.report.is_some();
        let mut rescan = false;
        let mut pending_fix: Option<(&'static str, FixAction)> = None;
        let mut start_latency = false;
        let mut stop_latency = false;
        let mut start_xrun = false;
        let mut stop_xrun = false;

        let header = ui.horizontal(|ui| {
            ui.set_min_height(26.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if scanning {
                    ui.add(egui::Spinner::new().size(16.0).color(theme::INK_2));
                } else if theme::primary_button(ui, if has_report { "Check again" } else { "Check system" })
                    .on_hover_text("Inspect PipeWire quantum/rate, realtime priority, CPU governor, USB autosuspend, and buffer xruns")
                    .clicked()
                {
                    rescan = true;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    theme::section_label(ui, "Audio doctor");
                });
            });
        });
        if self.audio_doctor.scroll_to_frames > 0 {
            self.audio_doctor.scroll_to_frames -= 1;
            ui.scroll_to_rect_animation(
                header.response.rect,
                Some(egui::Align::TOP),
                egui::style::ScrollAnimation::none(),
            );
        }

        ui.add_space(4.0);
        let summary_text = if scanning {
            egui::RichText::new("Checking PipeWire, realtime priority, CPU governor and USB audio…")
                .color(theme::INK_2)
        } else if let Some(err) = &self.audio_doctor.error {
            egui::RichText::new(format!("Check failed: {err}")).color(theme::CLIP_BRIGHT)
        } else if let Some(r) = &self.audio_doctor.report {
            egui::RichText::new(format!("{} · {} ms", r.summary(), r.took_ms)).color(status_color(r.worst()))
        } else {
            egui::RichText::new(
                "Checks PipeWire quantum, realtime priority, CPU governor and USB autosuspend, measures round-trip latency and diagnoses xruns.",
            )
            .color(theme::INK_2)
        };
        ui.add(egui::Label::new(summary_text.size(11.5)).wrap());

        if let Some((applied, msg)) = &self.audio_doctor.fix_msg {
            ui.add_space(4.0);
            let col = if *applied { theme::LIVE_TEAL } else { theme::ACCENT_BRIGHT };
            ui.add(egui::Label::new(egui::RichText::new(msg).size(11.5).color(col)).wrap());
        }

        if let Some(report) = self.audio_doctor.report.clone() {
            let fixing_id = self.audio_doctor.fixing.as_ref().map(|(id, _)| *id);
            ui.add_space(8.0);
            for row in &report.checks {
                ui.horizontal_top(|ui| {
                    let (mark, _) = ui.allocate_exact_size(egui::vec2(18.0, 22.0), egui::Sense::hover());
                    ui.painter().text(
                        mark.center(),
                        egui::Align2::CENTER_CENTER,
                        audio_doctor::symbol(row.status),
                        egui::FontId::proportional(13.5),
                        status_color(row.status),
                    );
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.set_min_height(26.0);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if let Some(fix) = &row.fix {
                                    if fixing_id == Some(row.id) {
                                        ui.add(egui::Spinner::new().size(14.0).color(theme::INK_2));
                                    } else if theme::chip_button(ui, &fix.label, false).clicked() {
                                        pending_fix = Some((row.id, fix.action.clone()));
                                    }
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(row.title)
                                                .font(theme::medium_font(12.5))
                                                .color(theme::INK),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        });
                        ui.add(egui::Label::new(egui::RichText::new(&row.text).size(11.5).color(theme::INK_2)).wrap());
                    });
                });
                ui.add_space(6.0);
            }
        }

        ui.add_space(8.0);

        // Card 1: Round-trip latency measurement
        let measuring_lat = self.audio_doctor.latency_running.is_some();
        let lat_res = self.audio_doctor.latency_result.clone();
        egui::Frame::new()
            .fill(theme::PAPER_SINK)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(theme::RADIUS_CARD)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if measuring_lat {
                            if theme::danger_button(ui, "Stop", false).clicked() {
                                stop_latency = true;
                            }
                            ui.add(egui::Spinner::new().size(14.0).color(theme::INK_2));
                        } else if theme::chip_button(ui, "Measure", false)
                            .on_hover_text("Play 4 short test chirps through your output and listen on your input")
                            .clicked()
                        {
                            start_latency = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new("Round-trip latency")
                                        .font(theme::medium_font(13.0))
                                        .color(theme::INK),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
                ui.add_space(4.0);
                let (msg, col) = if measuring_lat {
                    (
                        "Playing 4 short test bursts through output and recording input…".to_string(),
                        theme::INK_2,
                    )
                } else if let Some(res) = &lat_res {
                    match res {
                        LatencyResult::Measured {
                            median_ms,
                            min_ms,
                            max_ms,
                            detected,
                            total,
                            sample_rate,
                            buffer_frames,
                        } => (
                            format!(
                                "{median_ms:.1} ms round trip ({min_ms:.1}–{max_ms:.1} ms, {detected}/{total} bursts at {buffer_frames} frames / {:.1} kHz).",
                                *sample_rate as f64 / 1000.0
                            ),
                            theme::LIVE_TEAL,
                        ),
                        LatencyResult::NoLoopback { message, .. } => (message.clone(), theme::ACCENT_BRIGHT),
                        LatencyResult::Cancelled => ("Measurement stopped.".into(), theme::INK_2),
                        LatencyResult::Failed(e) => (e.clone(), theme::CLIP_BRIGHT),
                    }
                } else {
                    (
                        "Connect a cable from output to input, or unmute your speakers and microphone, then press Measure.".into(),
                        theme::INK_2,
                    )
                };
                ui.add(egui::Label::new(egui::RichText::new(msg).size(11.5).color(col)).wrap());
            });

        ui.add_space(8.0);

        // Card 2: Live xrun check (60 s soak, cancellable)
        let running_xrun = self.audio_doctor.xrun_run.as_ref().map(|r| {
            let c = self.engine_counters();
            let elapsed = r.started_at.elapsed().as_secs_f32().min(r.target_secs);
            (
                elapsed,
                r.target_secs,
                c.xruns.saturating_sub(r.start_xruns),
                c.render_late.saturating_sub(r.start_late),
                r.peak_dsp.max(self.engine_stats.dsp_percent),
            )
        });
        let xrun_res = self.audio_doctor.xrun_result.clone();
        egui::Frame::new()
            .fill(theme::PAPER_SINK)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(theme::RADIUS_CARD)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running_xrun.is_some() {
                            if theme::danger_button(ui, "Stop", false).clicked() {
                                stop_xrun = true;
                            }
                        } else if theme::chip_button(ui, "Run 60 s test", false)
                            .on_hover_text("Play a test rig for 60 seconds, count dropouts, and explain any cause")
                            .clicked()
                        {
                            start_xrun = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new("Live xrun check")
                                        .font(theme::medium_font(13.0))
                                        .color(theme::INK),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
                ui.add_space(4.0);
                if let Some((elapsed, target, xruns, late, peak_dsp)) = running_xrun {
                    let frac = (elapsed / target.max(1.0)).clamp(0.0, 1.0);
                    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
                    ui.painter().rect_filled(bar, 1.0, theme::DARK_PANEL_HI);
                    let fill = Rect::from_min_size(bar.min, egui::vec2(bar.width() * frac, bar.height()));
                    ui.painter().rect_filled(fill, 1.0, theme::LIVE_TEAL);
                    ui.add_space(4.0);
                    let status_line = format!(
                        "{elapsed:.0} / {target:.0} s · {xruns} xrun{} · {late} late · peak DSP {peak_dsp:.0} %",
                        if xruns == 1 { "" } else { "s" },
                    );
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(status_line)
                                .font(FontId::monospace(11.5))
                                .color(theme::CREAM_META),
                        )
                        .wrap(),
                    );
                } else if let Some((_, diag)) = &xrun_res {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&diag.headline)
                                .font(theme::medium_font(12.0))
                                .color(status_color(diag.status)),
                        )
                        .wrap(),
                    );
                    ui.add(egui::Label::new(egui::RichText::new(&diag.explanation).size(11.5).color(theme::INK_2)).wrap());
                    if let Some(fix) = &diag.recommended_fix {
                        ui.add_space(4.0);
                        if theme::chip_button(ui, &fix.label, false).clicked() {
                            pending_fix = Some(("xrun_diag", fix.action.clone()));
                        }
                    }
                } else {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                "Plays a test rig for up to 60 s, counts xruns and late buffers, and names the likely cause.",
                            )
                            .size(11.5)
                            .color(theme::INK_2),
                        )
                        .wrap(),
                    );
                }
            });

        if rescan {
            self.start_audio_doctor_scan();
        }
        if let Some((id, action)) = pending_fix {
            self.start_audio_doctor_fix(id, action);
        }
        if start_latency {
            self.start_latency_measure();
        }
        if stop_latency {
            self.stop_latency_measure();
        }
        if start_xrun {
            self.start_xrun_test();
        }
        if stop_xrun {
            self.finish_xrun_test(true);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::ui) fn sample_state() -> AudioDoctorState {
        use crate::audio_doctor::*;
        let facts = DoctorFacts {
            pipewire: PipeWireSettings {
                available: true,
                rate: Some(48_000),
                force_rate: Some(0),
                quantum: Some(1024),
                min_quantum: Some(32),
                max_quantum: Some(2048),
                force_quantum: Some(256),
            },
            realtime: RealtimeFacts {
                audio_thread_rt: Some(true),
                rlimit_rtprio_soft: 0,
                rlimit_rtprio_hard: 0,
                rtkit_active: Some(true),
                rtkit_installed: true,
                in_realtime_group: false,
            },
            governor: GovernorFacts {
                governors: vec!["schedutil".into(); 8],
                power_profile: Some("balanced".into()),
                has_powerprofilesctl: true,
            },
            usb_audio: vec![UsbAudioDevice {
                sysfs_id: "1-2".into(),
                name: "MOTU M2".into(),
                vendor_id: Some("07fd".into()),
                product_id: Some("000b".into()),
                power_control: "auto".into(),
            }],
            engine: EngineFacts {
                backend_choice: "auto".into(),
                active_backend: Some("pipewire".into()),
                configured_frames: 256,
                live_frames: 256,
                audio_live: true,
                sample_rate: 48_000,
                dsp_percent: 18.0,
                render_max_us: 1120,
                late_blocks: 0,
                xruns: 0,
                top_node: Some(("Surge XT".into(), 12.0)),
            },
        };
        let report = DoctorReport::from_facts(facts.clone(), Duration::from_millis(14));
        let run = XrunRunSummary {
            elapsed_secs: 60.0,
            target_secs: 60.0,
            stopped_early: false,
            xruns: 0,
            late_blocks: 0,
            peak_dsp_percent: 24.0,
            worst_render_us: 1480,
            buffer_frames: 256,
            sample_rate: 48_000,
            heaviest_node: Some(("Surge XT".into(), 14.0)),
        };
        let diag = crate::audio_doctor::diagnose_xrun_run(&run, &facts);
        AudioDoctorState {
            report: Some(report),
            latency_result: Some(LatencyResult::Measured {
                median_ms: 14.2,
                min_ms: 13.9,
                max_ms: 14.6,
                detected: 4,
                total: 4,
                sample_rate: 48_000,
                buffer_frames: 256,
            }),
            xrun_result: Some((run, diag)),
            scroll_to_frames: 2,
            ..Default::default()
        }
    }

    fn frame(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        let mut texts = Vec::new();
        fn walk(shape: &Shape, texts: &mut Vec<(String, Rect)>) {
            match shape {
                Shape::Text(t) => texts.push((t.galley.text().to_owned(), t.galley.rect.translate(t.pos.to_vec2()))),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, texts)),
                _ => {}
            }
        }
        for cs in &out.shapes {
            walk(&cs.shape, &mut texts);
        }
        texts
    }

    #[test]
    fn audio_doctor_tool_is_registered_read_only_and_returns_structured_json() {
        let names: Vec<String> = crate::assistant::tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "audio_doctor"), "assistant::tools() missing audio_doctor");
        assert!(UI_BRIDGE_TOOLS.contains(&"audio_doctor"), "UI_BRIDGE_TOOLS missing audio_doctor");
        assert!(tool_is_read_only("audio_doctor"), "audio_doctor must be read-only");
        assert!(!crate::bridge::refused_while_paused(&serde_json::json!({
            "op": "ui",
            "tool": "audio_doctor",
            "args": {}
        })));

        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        assert_eq!(app.exec_tool("audio_doctor", &serde_json::json!({})), "running Audio Doctor");
        let answer = app.audio_doctor.answer.take().expect("assistant waits on audio_doctor");
        let (tx, bridged) = mpsc::channel();
        app.serve_ui_requests(vec![("audio_doctor".into(), serde_json::json!({}), tx)]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.audio_doctor.scanning() {
            assert!(Instant::now() < deadline, "Audio Doctor scan timed out");
            frame(&mut app, &ctx, Vec2::new(1280.0, 800.0), vec![]);
            std::thread::sleep(Duration::from_millis(5));
        }
        for text in [answer.recv().unwrap(), bridged.recv().unwrap()] {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("JSON: {text}"));
            assert_eq!(v["checks"].as_array().map(Vec::len), Some(5), "{v}");
            assert!(v["summary"].is_string(), "{v}");
            assert!(v["facts"]["pipewire"].is_object(), "{v}");
            assert!(v["facts"]["realtime"].is_object(), "{v}");
            assert!(v["facts"]["governor"].is_object(), "{v}");
            assert!(v["facts"]["usb_audio"].is_array(), "{v}");
        }
    }

    #[test]
    fn preshow_audio_warning_links_directly_to_settings_audio_doctor() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        let mut report = super::super::preshow::tests::a_report();
        if let Some(row) = report.rows.iter_mut().find(|r| r.id == "render") {
            row.status = crate::preshow::Status::Warning;
            row.text = "256 frames at 48 kHz (5.3 ms): rendering takes 82 % of a buffer.".into();
        }
        app.stage_view = StageView::Setlist;
        app.preshow.open = true;
        app.preshow.report = Some(report);

        let texts = frame(&mut app, &ctx, Vec2::new(1280.0, 800.0), vec![]);
        let btn_pos = texts
            .iter()
            .find(|(t, _)| t == "Open Audio Doctor")
            .expect("Open Audio Doctor button on buffer warning")
            .1
            .center();
        let evs = vec![
            egui::Event::PointerMoved(btn_pos),
            egui::Event::PointerButton {
                pos: btn_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: btn_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ];
        frame(&mut app, &ctx, Vec2::new(1280.0, 800.0), evs);
        assert_eq!(app.stage_view, StageView::Settings);
        assert_eq!(app.settings_page, SettingsPage::Audio);
    }

    #[test]
    fn xrun_check_can_start_and_stop_early_with_diagnosis() {
        let mut app = DemoApp::new_with_sink(None);
        app.test_counters = Some(crate::engine_info::EngineCounters {
            sample_rate: 48_000,
            callback_frames: 256,
            render_avg_ns: 900_000,
            render_max_us: 1200,
            render_late: 0,
            xruns: 0,
            ..Default::default()
        });
        app.engine_stats = app.engine_stats_now();
        app.start_xrun_test();
        assert!(app.audio_doctor.xrun_run.is_some());
        app.finish_xrun_test(true);
        assert!(app.audio_doctor.xrun_run.is_none());
        let (summary, diag) = app.audio_doctor.xrun_result.as_ref().expect("diagnosis stored");
        assert!(summary.stopped_early);
        assert_eq!(diag.status, Status::Ok);
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat audio_doctor_shots -- --ignored"]
    fn audio_doctor_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        for size in [Vec2::new(1280.0, 800.0), Vec2::new(760.0, 520.0)] {
            let mut app = DemoApp::new_with_sink(None);
            app.stage_view = StageView::Settings;
            app.settings_page = SettingsPage::Audio;
            app.active_audio_backend = Some(crate::audio_backend::ActiveBackend::PipeWire);
            app.test_counters = Some(crate::engine_info::EngineCounters {
                sample_rate: 48_000,
                callback_frames: 256,
                configured_frames: 256,
                render_avg_ns: 950_000,
                render_max_us: 1120,
                ..Default::default()
            });
            app.engine_stats = app.engine_stats_now();
            app.audio_doctor = sample_state();
            super::super::offline_shots::shot(
                &mut app,
                size,
                &dir.join(format!("audio-doctor-{}x{}.png", size.x, size.y)),
            );
        }
    }
}
