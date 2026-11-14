//! Dev preview v0.0.1 — the first local build to try.
//!
//! An interactive shell while the P0 workstreams land: test tone through
//! PipeWire, a live MIDI monitor on a virtual ALSA port, and the rack taking
//! shape. The rack flip + physics cables arrive with the ui-cables workstream;
//! Surge hosting arrives with clap-host. This module is lead-owned integration
//! scaffolding and will evolve into the real app shell.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use midir::os::unix::VirtualInput;
use midir::{Ignore, MidiInput};

const MAX_EVENTS: usize = 200;
const BG: egui::Color32 = egui::Color32::from_rgb(0x15, 0x16, 0x1e);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x20, 0x2e);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(0xff, 0xd6, 0x0a);
const BLUE: egui::Color32 = egui::Color32::from_rgb(0x7a, 0xa2, 0xf7);
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x9e, 0xce, 0x6a);

fn main() -> anyhow::Result<()> {
    run()
}

fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 680.0])
            .with_title("kabelsalat — dev preview"),
        ..Default::default()
    };
    eframe::run_native(
        "kabelsalat-dev",
        options,
        Box::new(|cc| Ok(Box::new(PreviewApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct PreviewApp {
    events: Arc<Mutex<VecDeque<String>>>,
    midi_status: String,
    // Kept alive so the virtual port stays open.
    _midi_conn: Option<midir::MidiInputConnection<()>>,
    tone_busy: Arc<AtomicBool>,
    audio_info: String,
}

impl PreviewApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let audio_info = match cpal::default_host().default_output_device() {
            Some(dev) => {
                let name = dev
                    .description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "?".into());
                match dev.default_output_config() {
                    Ok(cfg) => format!(
                        "{name} · {} Hz · {} ch",
                        cfg.sample_rate(),
                        cfg.channels()
                    ),
                    Err(e) => format!("{name} · config error: {e}"),
                }
            }
            None => "no output device found".into(),
        };

        let events = Arc::new(Mutex::new(VecDeque::new()));
        let (midi_status, midi_conn) = match MidiInput::new("kabelsalat-dev") {
            Ok(mut midi_in) => {
                midi_in.ignore(Ignore::None);
                let ev = events.clone();
                match midi_in.create_virtual(
                    "kabelsalat-dev",
                    move |stamp, msg: &[u8], _| {
                        let text = format_event(stamp, msg);
                        if let Ok(mut q) = ev.lock() {
                            q.push_back(text);
                            while q.len() > MAX_EVENTS {
                                q.pop_front();
                            }
                        }
                    },
                    (),
                ) {
                    Ok(conn) => (
                        "listening on virtual ALSA port 'kabelsalat-dev'".to_string(),
                        Some(conn),
                    ),
                    Err(e) => (format!("virtual port failed: {e}"), None),
                }
            }
            Err(e) => (format!("MIDI unavailable: {e}"), None),
        };

        Self {
            events,
            midi_status,
            _midi_conn: midi_conn,
            tone_busy: Arc::new(AtomicBool::new(false)),
            audio_info,
        }
    }

    fn play_tone(busy: Arc<AtomicBool>) {
        if busy.swap(true, Ordering::SeqCst) {
            return; // already playing
        }
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("no output device"))?;
                let cfg = device.default_output_config()?;
                let sr = cfg.sample_rate() as f64;
                let ch = cfg.channels() as usize;
                let total = sr as usize; // one second
                let mut phase = 0.0f64;
                let inc = 440.0 / sr;
                let mut n = 0usize;
                let stream = device.build_output_stream(
                    cfg.into(),
                    move |data: &mut [f32], _| {
                        for frame in data.chunks_mut(ch) {
                            phase += inc;
                            if phase >= 1.0 {
                                phase -= 1.0;
                            }
                            // 10 ms fade in/out to avoid clicks.
                            let env = (n as f32 / 480.0)
                                .min(1.0)
                                .min((total.saturating_sub(n) as f32) / 480.0);
                            n += 1;
                            let s = (phase * std::f64::consts::TAU).sin() as f32 * 0.15 * env;
                            for x in frame.iter_mut() {
                                *x = s;
                            }
                        }
                    },
                    |e| eprintln!("tone stream error: {e}"),
                    None,
                )?;
                stream.play()?;
                std::thread::sleep(std::time::Duration::from_secs(1));
                Ok(())
            })();
            if let Err(e) = result {
                eprintln!("tone failed: {e}");
            }
            busy.store(false, Ordering::SeqCst);
        });
    }
}

impl eframe::App for PreviewApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep the MIDI monitor fresh without burning CPU at full frame rate.
        let ctx = root.ctx().clone();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(16)))
            .show(root, |ui| {
                ui.heading("kabelsalat");
                ui.label(
                    egui::RichText::new("dev preview v0.0.1 — live audio/MIDI wiring, alive")
                        .italics(),
                );
                ui.add_space(10.0);
                ui.label(format!("Audio: {}", self.audio_info));
                ui.horizontal(|ui| {
                    if ui.button("▶  Play test tone (440 Hz, 1 s)").clicked() {
                        Self::play_tone(self.tone_busy.clone());
                    }
                    ui.label(format!("MIDI: {}", self.midi_status));
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(egui::RichText::new("The rack — preview").strong());
                ui.add_space(4.0);
                rack_unit(ui, YELLOW, "SURGE XT", "synth · CLAP hosting in progress");
                rack_unit(ui, BLUE, "SYSTEM AUDIO", "YouTube capture · coming in P2");
                rack_unit(ui, GREEN, "MASTER", "output · live");
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Tab ⇄ flip the rack to patch the cables — landing with the UI workstream",
                    )
                    .small()
                    .italics(),
                );
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "MIDI monitor — send notes to the 'kabelsalat-dev' ALSA port",
                    )
                    .strong(),
                );
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let q = self.events.lock().unwrap();
                        if q.is_empty() {
                            ui.label("(waiting for events…)");
                        }
                        for e in q.iter() {
                            ui.monospace(e);
                        }
                    });
            });
    }
}

fn rack_unit(ui: &mut egui::Ui, accent: egui::Color32, name: &str, desc: &str) {
    egui::Frame::new()
        .fill(PANEL)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(8.0, 34.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, accent);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(name).strong().color(accent));
                    ui.label(egui::RichText::new(desc).small());
                });
            });
        });
    ui.add_space(4.0);
}

fn format_event(stamp: u64, msg: &[u8]) -> String {
    let t = format!("[{:.3}s]", stamp as f64 / 1_000_000.0);
    if msg.is_empty() {
        return format!("{t} (empty)");
    }
    let status = msg[0] & 0xF0;
    let ch = (msg[0] & 0x0F) + 1;
    match (status, msg.len()) {
        (0x90, 3..) if msg[2] > 0 => {
            format!("{t} Note on  ch{ch} key={} vel={}", msg[1], msg[2])
        }
        (0x80, 3..) | (0x90, 3..) => format!("{t} Note off ch{ch} key={}", msg[1]),
        (0xB0, 3..) => format!("{t} CC      ch{ch} #{}={}", msg[1], msg[2]),
        (0xE0, 3..) => format!("{t} Pitch   ch{ch}"),
        _ => format!(
            "{t} {}",
            msg.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }
}
