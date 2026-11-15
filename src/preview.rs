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

use crate::assistant;
use crate::host::{ClapVoice, ClapVoiceMain, VoiceController};

const MAX_EVENTS: usize = 200;
const KEY_LETTERS: [char; 16] = ['A', 'W', 'S', 'E', 'D', 'F', 'T', 'G', 'Y', 'H', 'U', 'J', 'K', 'O', 'L', 'P'];
const BG: egui::Color32 = egui::Color32::from_rgb(0x15, 0x16, 0x1e);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x20, 0x2e);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(0xff, 0xd6, 0x0a);
const BLUE: egui::Color32 = egui::Color32::from_rgb(0x7a, 0xa2, 0xf7);
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x9e, 0xce, 0x6a);

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 680.0])
            .with_maximized(true)
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
    keys_tx: Arc<Mutex<rtrb::Producer<(u8, bool)>>>,
    _hw_conns: Vec<midir::MidiInputConnection<()>>,
    hw_status: String,
    _synth_stream: Option<cpal::Stream>,
    synth_status: String,
    pressed_note: Option<u8>,
    octave: u8,
    held_keys: Vec<(egui::Key, u8)>,
    tone_busy: Arc<AtomicBool>,
    audio_info: String,
    ui_scale: f32,
    assistant: assistant::Assistant,
    key_input: String,
    chat_input: String,
    note_offs: Vec<(std::time::Instant, u8)>,
    surge_ctrl: Option<VoiceController>,
    // !Send: CLAP main-thread object, stays on the UI thread for state/GUI.
    _surge_main: Option<ClapVoiceMain>,
}

impl PreviewApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // HiDPI: match the compositor's fractional scale exactly (eDP-1 is 1.6
        // on the dev machine). Forcing integer 2.0 oversizes elements; leaving
        // it at 1.0 gets upscaled and blurry.
        let detected = cc.egui_ctx.pixels_per_point();
        let target = if detected > 1.05 { detected } else { 1.6 };
        cc.egui_ctx.set_pixels_per_point(target);
        let ui_scale = cc.egui_ctx.pixels_per_point();

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
        let (keys_tx, mut synth_rx) = rtrb::RingBuffer::<(u8, bool)>::new(64);
        let keys_tx = Arc::new(Mutex::new(keys_tx));

        // Surge XT voice, loaded at start — pre-wired, no setup. Sine fallback
        // if the plugin is unavailable.
        let mut surge_ctrl: Option<VoiceController> = None;
        let mut surge_main: Option<ClapVoiceMain> = None;
        let mut surge_voice: Option<ClapVoice> = None;
        let sample_rate = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.default_output_config().ok())
            .map(|c| c.sample_rate() as f64)
            .unwrap_or(48000.0);
        match ClapVoice::new(sample_rate) {
            Ok((voice, main)) => {
                surge_ctrl = Some(voice.controller());
                surge_voice = Some(voice);
                surge_main = Some(main);
            }
            Err(e) => eprintln!("Surge load failed, sine fallback: {e}"),
        }

        let (midi_status, midi_conn) = match MidiInput::new("kabelsalat-dev") {
            Ok(mut midi_in) => {
                midi_in.ignore(Ignore::None);
                let ev = events.clone();
                let tx = keys_tx.clone();
                let ctl = surge_ctrl.clone();
                match midi_in.create_virtual(
                    "kabelsalat-dev",
                    move |stamp, msg: &[u8], _| route_midi(stamp, msg, &tx, &ev, &ctl),
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

        // Auto-wire any hardware MIDI inputs at startup: plug in a keyboard,
        // launch the app, play. No patching required.
        let mut hw_conns = Vec::new();
        let mut hw_names = Vec::new();
        if let Ok(probe) = MidiInput::new("kabelsalat-probe") {
            for port in probe.ports() {
                let name = probe.port_name(&port).unwrap_or_default();
                if name.contains("kabelsalat")
                    || name.contains("Midi Through")
                    || name == "System"
                    || name == "Announce"
                {
                    continue;
                }
                if let Ok(mi) = MidiInput::new("kabelsalat-hw") {
                    let tx = keys_tx.clone();
                    let ev = events.clone();
                    let ctl = surge_ctrl.clone();
                    if let Ok(conn) = mi.connect(
                        &port,
                        "kabelsalat-hw",
                        move |stamp, msg: &[u8], _| route_midi(stamp, msg, &tx, &ev, &ctl),
                        (),
                    ) {
                        hw_conns.push(conn);
                        hw_names.push(name);
                    }
                }
            }
        }
        let hw_status = if hw_names.is_empty() {
            "no hardware MIDI ports found — laptop keys are live".to_string()
        } else {
            format!("hardware connected: {}", hw_names.join(", "))
        };

        // On-screen keys also expose a real ALSA output port, so they can be
        // wired into other apps (e.g. Surge standalone) via aconnect/qpwgraph.
        let keys_out = MidiOutput::new("kabelsalat-keys")
            .ok()
            .and_then(|out| out.create_virtual("kabelsalat-keys").ok());

        // Built-in synth stream, loaded at start: Surge XT when available,
        // sine fallback otherwise. Driven by the keys/MIDI routing above.
        let synth = (|| -> anyhow::Result<cpal::Stream> {
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no output device"))?;
            let cfg = device.default_output_config()?;
            let sr = cfg.sample_rate() as f64;
            let ch = cfg.channels() as usize;
            let mut config: cpal::StreamConfig = cfg.into();
            config.buffer_size = cpal::BufferSize::Fixed(512);
            let mut scratch = vec![0.0f32; 8192 * 2];
            let mut surge_voice = surge_voice;
            let mut freq = 440.0f64;
            let mut gate = false;
            let mut env = 0.0f32;
            let mut phase = 0.0f64;
            let mut current_note = 0u8;
            let stream = device.build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    if let Some(voice) = &mut surge_voice {
                        let frames = data.len() / ch;
                        if frames * 2 <= scratch.len() {
                            voice.render(&mut scratch[..frames * 2]);
                            for (i, frame) in data.chunks_mut(ch).enumerate() {
                                frame[0] = scratch[i * 2];
                                if ch > 1 {
                                    frame[1] = scratch[i * 2 + 1];
                                }
                                for s in frame.iter_mut().skip(2) {
                                    *s = 0.0;
                                }
                            }
                        } else {
                            for s in data.iter_mut() {
                                *s = 0.0;
                            }
                        }
                        return;
                    }
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
            Ok(s) => (
                if surge_ctrl.is_some() {
                    "Surge loaded — play!".to_string()
                } else {
                    "sine synth (Surge unavailable)".to_string()
                },
                Some(s),
            ),
            Err(e) => (format!("synth failed: {e}"), None),
        };

        Self {
            events,
            midi_status,
            _midi_conn: midi_conn,
            keys_out,
            keys_tx,
            _hw_conns: hw_conns,
            hw_status,
            _synth_stream: synth_stream,
            synth_status,
            pressed_note: None,
            octave: 4,
            held_keys: Vec::new(),
            tone_busy: Arc::new(AtomicBool::new(false)),
            audio_info,
            ui_scale,
            assistant: assistant::Assistant::new(),
            key_input: String::new(),
            chat_input: String::new(),
            note_offs: Vec::new(),
            surge_ctrl,
            _surge_main: surge_main,
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
                let mut config: cpal::StreamConfig = cfg.into();
                config.buffer_size = cpal::BufferSize::Fixed(512);
                let total = sr as usize; // one second
                let mut phase = 0.0f64;
                let inc = 440.0 / sr;
                let mut n = 0usize;
                let stream = device.build_output_stream(
                    config,
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

    fn base_note(&self) -> u8 {
        (self.octave + 1) * 12
    }

    fn key_offset(key: egui::Key) -> Option<u8> {
        use egui::Key::*;
        Some(match key {
            A => 0, W => 1, S => 2, E => 3, D => 4, F => 5, T => 6, G => 7,
            Y => 8, H => 9, U => 10, J => 11, K => 12, O => 13, L => 14, P => 15,
            _ => return None,
        })
    }

    fn shift_octave(&mut self, delta: i8) {
        let held: Vec<_> = self.held_keys.drain(..).collect();
        for (_, note) in held {
            self.note_event(note, false);
        }
        self.octave = (self.octave as i8 + delta).clamp(1, 7) as u8;
    }

    fn handle_key(&mut self, key: egui::Key, pressed: bool, repeat: bool) {
        if pressed && !repeat {
            if key == egui::Key::Z {
                self.shift_octave(-1);
                return;
            }
            if key == egui::Key::X {
                self.shift_octave(1);
                return;
            }
        }
        let Some(off) = Self::key_offset(key) else { return };
        if pressed {
            if repeat || self.held_keys.iter().any(|(k, _)| *k == key) {
                return;
            }
            let note = self.base_note().saturating_add(off).min(127);
            self.held_keys.push((key, note));
            self.note_event(note, true);
        } else if let Some(pos) = self.held_keys.iter().position(|(k, _)| *k == key) {
            let (_, note) = self.held_keys.remove(pos);
            self.note_event(note, false);
        }
    }

    fn assistant_panel(&mut self, root: &mut egui::Ui) {
        egui::Panel::right("assistant")
            .default_size(340.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(10)))
            .show(root, |ui| {
                ui.heading("Assistant");
                ui.label(egui::RichText::new("DeepSeek · bring your own key").small());
                ui.add_space(6.0);
                if !self.assistant.key_set {
                    ui.label("Paste your DeepSeek API key:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.key_input)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    if ui.button("Save key").clicked() && !self.key_input.trim().is_empty() {
                        let k = self.key_input.trim().to_string();
                        match assistant::save_key(&k) {
                            Ok(()) => {
                                self.assistant.set_key(k);
                                self.key_input.clear();
                            }
                            Err(e) => self.assistant.history.push(assistant::ChatLine {
                                role: "error".into(),
                                text: format!("could not save key: {e}"),
                            }),
                        }
                    }
                    ui.label(
                        egui::RichText::new(
                            "Stored locally (~/.config/kabelsalat/config.json, 0600). Sent only to api.deepseek.com.",
                        )
                        .small()
                        .italics(),
                    );
                } else {
                    let avail = ui.available_height();
                    egui::ScrollArea::vertical()
                        .max_height((avail - 90.0).max(100.0))
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.assistant.history {
                                let (label, color) = match line.role.as_str() {
                                    "user" => ("You", YELLOW),
                                    "assistant" => ("Assistant", GREEN),
                                    _ => ("Error", egui::Color32::from_rgb(0xf7, 0x76, 0x8e)),
                                };
                                ui.label(egui::RichText::new(label).small().strong().color(color));
                                ui.label(&line.text);
                                ui.add_space(6.0);
                            }
                            if self.assistant.busy {
                                ui.label(egui::RichText::new("thinking…").italics().small());
                            }
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.chat_input)
                                .desired_width((ui.available_width() - 64.0).max(80.0))
                                .hint_text("Ask or instruct…"),
                        );
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (ui.button("Send").clicked() || enter)
                            && !self.chat_input.trim().is_empty()
                            && !self.assistant.busy
                        {
                            let t = self.chat_input.trim().to_string();
                            self.chat_input.clear();
                            self.assistant.send_user(t);
                        }
                    });
                }
            });
    }

    fn exec_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        match name {
            "get_status" => serde_json::json!({
                "audio": self.audio_info,
                "synth": self.synth_status,
                "midi_in": self.hw_status,
                "midi_monitor": self.midi_status,
                "ui_scale": self.ui_scale,
                "octave": self.octave,
                "recent_events": self.events.lock().map(|q| q.iter().rev().take(10).cloned().collect::<Vec<_>>()).unwrap_or_default(),
            })
            .to_string(),
            "play_note" => {
                let note = args.get("note").and_then(|v| v.as_u64()).unwrap_or(60).min(127) as u8;
                let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(500).min(10000);
                self.note_event(note, true);
                self.note_offs.push((
                    std::time::Instant::now() + std::time::Duration::from_millis(ms),
                    note,
                ));
                format!("played note {note} for {ms} ms")
            }
            "set_octave" => {
                let oct = args.get("octave").and_then(|v| v.as_u64()).unwrap_or(4).clamp(1, 7) as u8;
                self.octave = oct;
                format!("octave set to C{oct}")
            }
            _ => format!("unknown tool: {name}"),
        }
    }

    fn note_event(&mut self, note: u8, on: bool) {
        if let Some(ctrl) = &self.surge_ctrl {
            if on {
                ctrl.note_on(note as u16, 0.8);
            } else {
                ctrl.note_off(note as u16);
            }
        } else {
            let _ = self.keys_tx.lock().unwrap().push((note, on));
        }
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
            egui::RichText::new(format!(
                "Play with mouse or laptop keys: A W S E D F T G Y H U J K O L P · Z/X = octave (now C{}) · ALSA out: 'kabelsalat-keys'",
                self.octave
            ))
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
            let off = *note as i16 - self.base_note() as i16;
            if (0..16).contains(&off) {
                painter.text(
                    r.center_bottom() - egui::vec2(0.0, 20.0),
                    egui::Align2::CENTER_BOTTOM,
                    KEY_LETTERS[off as usize].to_string(),
                    egui::FontId::proportional(9.0),
                    egui::Color32::from_rgb(0x88, 0x88, 0x99),
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
            let off = note as i16 - self.base_note() as i16;
            if (0..16).contains(&off) {
                painter.text(
                    r.center_bottom() - egui::vec2(0.0, 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    KEY_LETTERS[off as usize].to_string(),
                    egui::FontId::proportional(8.0),
                    egui::Color32::from_rgb(0xaa, 0xaa, 0xbb),
                );
            }
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
        // Laptop-keyboard playing.
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key { key, pressed, repeat, .. } = ev {
                self.handle_key(key, pressed, repeat);
            }
        }
        // Scheduled note-offs (assistant play_note tool).
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.note_offs.len() {
            if self.note_offs[i].0 <= now {
                let (_, note) = self.note_offs.remove(i);
                self.note_event(note, false);
            } else {
                i += 1;
            }
        }
        // Assistant events (tool calls execute here, on the UI thread).
        for ev in self.assistant.poll() {
            match ev {
                assistant::FromWorker::Text(t) => self
                    .assistant
                    .history
                    .push(assistant::ChatLine { role: "assistant".into(), text: t }),
                assistant::FromWorker::ToolCall { id, name, args } => {
                    let result = self.exec_tool(&name, &args);
                    self.assistant.send_tool_result(id, result);
                }
                assistant::FromWorker::Done => self.assistant.busy = false,
                assistant::FromWorker::Error(e) => {
                    self.assistant.busy = false;
                    self.assistant
                        .history
                        .push(assistant::ChatLine { role: "error".into(), text: e });
                }
            }
        }
        self.assistant_panel(root);
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
                ui.label(format!(
                    "UI scale: {:.2}× · Synth: {} · MIDI in: {}",
                    self.ui_scale, self.synth_status, self.hw_status
                ));
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

/// Route an incoming MIDI message: log it, and forward note on/off to the synth.
fn route_midi(
    stamp: u64,
    msg: &[u8],
    tx: &Arc<Mutex<rtrb::Producer<(u8, bool)>>>,
    events: &Arc<Mutex<VecDeque<String>>>,
    ctrl: &Option<VoiceController>,
) {
    if msg.len() >= 3 {
        let status = msg[0] & 0xF0;
        if status == 0x90 && msg[2] > 0 {
            if let Some(c) = ctrl {
                c.note_on(msg[1] as u16, msg[2] as f64 / 127.0);
            } else {
                let _ = tx.lock().unwrap().push((msg[1], true));
            }
        } else if status == 0x80 || (status == 0x90 && msg[2] == 0) {
            if let Some(c) = ctrl {
                c.note_off(msg[1] as u16);
            } else {
                let _ = tx.lock().unwrap().push((msg[1], false));
            }
        }
    }
    let text = format_event(stamp, msg);
    if let Ok(mut q) = events.lock() {
        q.push_back(text);
        while q.len() > MAX_EVENTS {
            q.pop_front();
        }
    }
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
