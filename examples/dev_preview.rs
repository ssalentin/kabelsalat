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
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{Ignore, MidiInput, MidiOutput};

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
    keys_out: Option<midir::MidiOutputConnection>,
    keys_tx: rtrb::Producer<(u8, bool)>,
    _synth_stream: Option<cpal::Stream>,
    synth_status: String,
    pressed_note: Option<u8>,
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

        // On-screen keys also expose a real ALSA output port, so they can be
        // wired into other apps (e.g. Surge standalone) via aconnect/qpwgraph.
        let keys_out = MidiOutput::new("kabelsalat-keys")
            .ok()
            .and_then(|out| out.create_virtual("kabelsalat-keys").ok());

        // Built-in sine synth, loaded at start: a persistent output stream driven
        // by a lock-free ring of (note, on) messages from the on-screen keys.
        let (keys_tx, mut synth_rx) = rtrb::RingBuffer::<(u8, bool)>::new(64);
        let synth = (|| -> anyhow::Result<cpal::Stream> {
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no output device"))?;
            let cfg = device.default_output_config()?;
            let sr = cfg.sample_rate() as f64;
            let ch = cfg.channels() as usize;
            let mut freq = 440.0f64;
            let mut gate = false;
            let mut env = 0.0f32;
            let mut phase = 0.0f64;
            let mut current_note = 0u8;
            let stream = device.build_output_stream(
                cfg.into(),
                move |data: &mut [f32], _| {
                    while let Ok((note, on)) = synth_rx.pop() {
                        if on {
                            current_note = note;
                            freq = 440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0);
                            gate = true;
                        } else if note == current_note {
                            gate = false;
                        }
                    }
                    let target = if gate { 0.15f32 } else { 0.0 };
                    let coeff = if gate { 0.005f32 } else { 0.0005 };
                    for frame in data.chunks_mut(ch) {
                        env += (target - env) * coeff;
                        phase += freq / sr;
                        if phase >= 1.0 {
                            phase -= 1.0;
                        }
                        let s = (phase * std::f64::consts::TAU).sin() as f32 * env;
                        for x in frame.iter_mut() {
                            *x = s;
                        }
                    }
                },
                |e| eprintln!("synth stream error: {e}"),
                None,
            )?;
            stream.play()?;
            Ok(stream)
        })();
        let (synth_status, synth_stream) = match synth {
            Ok(s) => ("sine synth loaded — click the keys".to_string(), Some(s)),
            Err(e) => (format!("synth failed: {e}"), None),
        };

        Self {
            events,
            midi_status,
            _midi_conn: midi_conn,
            keys_out,
            keys_tx,
            _synth_stream: synth_stream,
            synth_status,
            pressed_note: None,
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

    fn note_event(&mut self, note: u8, on: bool) {
        let _ = self.keys_tx.push((note, on));
        let msg = [if on { 0x90 } else { 0x80 }, note, if on { 100 } else { 0 }];
        if let Some(conn) = &mut self.keys_out {
            let _ = conn.send(&msg);
        }
        let text = format!(
            "[keys] {} key={}",
            if on { "Note on " } else { "Note off" },
            note
        );
        if let Ok(mut q) = self.events.lock() {
            q.push_back(text);
            while q.len() > MAX_EVENTS {
                q.pop_front();
            }
        }
    }

    /// Two octaves of click-to-play piano, C4..C6.
    fn keyboard(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "On-screen keys — click to play the sine synth (also an ALSA output: 'kabelsalat-keys')",
            )
            .small(),
        );
        const WHITE: [u8; 15] = [60, 62, 64, 65, 67, 69, 71, 72, 74, 76, 77, 79, 81, 83, 84];
        const BLACK: [(usize, u8); 10] = [
            (0, 61), (1, 63), (3, 66), (4, 68), (5, 70),
            (7, 73), (8, 75), (10, 78), (11, 80), (12, 82),
        ];
        let width = ui.available_width().min(860.0);
        let height = 110.0;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
        let painter = ui.painter();
        let wk = width / 15.0;
        let bh = height * 0.62;
        let bw = wk * 0.62;

        // Which key is under the pointer right now?
        let mut current: Option<u8> = None;
        if response.is_pointer_button_down_on() {
            if let Some(pos) = response.interact_pointer_pos() {
                if rect.contains(pos) {
                    let x = pos.x - rect.left();
                    let y = pos.y - rect.top();
                    let wi = (x / wk).floor().clamp(0.0, 14.0) as usize;
                    let mut black = None;
                    if y < bh {
                        for (b, note) in BLACK {
                            let cx = (b + 1) as f32 * wk;
                            if (x - cx).abs() < bw / 2.0 {
                                black = Some(note);
                                break;
                            }
                        }
                    }
                    current = Some(black.unwrap_or(WHITE[wi]));
                }
            }
        }

        // White keys.
        for (i, note) in WHITE.iter().enumerate() {
            let r = egui::Rect::from_min_size(
                egui::pos2(rect.left() + i as f32 * wk, rect.top()),
                egui::vec2(wk - 1.0, height),
            );
            let active = current == Some(*note);
            painter.rect_filled(
                r,
                2.0,
                if active { YELLOW } else { egui::Color32::from_rgb(0xe8, 0xe8, 0xec) },
            );
            if note % 12 == 0 {
                painter.text(
                    r.center_bottom() - egui::vec2(0.0, 6.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!("C{}", note / 12 - 1),
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_rgb(0x66, 0x66, 0x77),
                );
            }
        }
        // Black keys.
        for (b, note) in BLACK {
            let cx = rect.left() + (b + 1) as f32 * wk;
            let r = egui::Rect::from_min_size(
                egui::pos2(cx - bw / 2.0, rect.top()),
                egui::vec2(bw, bh),
            );
            let active = current == Some(note);
            painter.rect_filled(
                r,
                2.0,
                if active { YELLOW } else { egui::Color32::from_rgb(0x1a, 0x1b, 0x24) },
            );
        }

        // Note on/off on transitions (glissando-friendly).
        if current != self.pressed_note {
            if let Some(old) = self.pressed_note {
                self.note_event(old, false);
            }
            if let Some(new) = current {
                self.note_event(new, true);
            }
            self.pressed_note = current;
        }
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
                ui.label(format!("Synth: {}", self.synth_status));
                ui.horizontal(|ui| {
                    if ui.button("▶  Play test tone (440 Hz, 1 s)").clicked() {
                        Self::play_tone(self.tone_busy.clone());
                    }
                    ui.label(format!("MIDI: {}", self.midi_status));
                });
                ui.add_space(10.0);
                self.keyboard(ui);
                ui.add_space(6.0);
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
                        "MIDI monitor — on-screen keys + the 'kabelsalat-dev' ALSA port",
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
