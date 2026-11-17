//! egui UI — the signature animated wiring (docs/ARCHITECTURE.md §2), Reason-style rack.
//! Owned by the ui-cables workstream. P0: standalone demo (`ui-demo`).
//!
//! One window, two sides of a mini rack:
//! * FRONT — perform: draggable knobs, a clickable mini keyboard, live meter.
//! * BACK  — patch: jacks and sagging cables. Tab flips the rack with an
//!   eased squash-swap-expand animation.
//!
//! Cable model
//! -----------
//! Each cable is a cubic Bézier whose two control points are damped-spring
//! particles anchored to a distance-aware rest shape (downward sag + lateral
//! bulge for vertical runs). Grabbing a cable injects forces into the
//! particles; dragging a plug moves an endpoint — the cable whips and settles.
//! MIDI cables carry pulses fired by a simulated 96 BPM band; audio cables
//! breathe with envelopes triggered when pulses arrive.
//!
//! Performance: `epaint::CubicBezierShape` (4 points, zero heap), fixed-size
//! arc-length LUTs, cached galleys, fixed pulse arrays — no per-frame
//! allocation storms. Pure snapshot-style simulation; never touches audio.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui::{
    self, Color32, CursorIcon, FontId, Galley, Key, Modifiers, Pos2, Rect, Shape, Stroke, Vec2,
};

use crate::assistant::{Assistant, ChatLine, ToolCall};
use crate::control::{
    name_bytes, name_str, split_port, AudioInChannel, Command, CommandBus, DspKind, Graph, Lv2Uri,
    Node, NodeKind, PortKind as CPortKind, PortRef, DEFAULT_SPLIT,
};
use crate::engine::{
    AudioInLevels, EdgeLevels, GraphEngine, P_AUDIO_IN_GAIN, P_AUDIO_IN_MONITOR, P_LEVEL, P_MIX,
    P_TIME, P_VOL, P_WARM,
};
use crate::host::{
    list_clap_plugins, ClapPluginInfo, ClapVoice, ClapVoiceMain, ParamDesc, VoiceController,
};
use clack_host::prelude::ClapId;
use crate::lv2::{discover as lv2_discover, Lv2PluginInfo};
use crate::matrix::SourceId;
use crate::session::Template;
use crate::theme;

// ---------------------------------------------------------------------------
// Palette — Hurricane design language (src/theme.rs,
// docs/design-reference-analysis.md §3.2). The legacy names stay as aliases
// so the drawing code reads naturally; all values come from the single theme
// source. Rack painting is on the dark hero panel; egui chrome is on paper.
// ---------------------------------------------------------------------------

const YELLOW: Color32 = theme::MIDI_BLUE; // MIDI
const GREEN: Color32 = theme::LIVE_TEAL; // active / playing / sources
const ORANGE: Color32 = theme::ACCENT; // cable warming
const CHASSIS: Color32 = theme::DARK_PANEL;
const RAIL: Color32 = theme::DARK_PANEL_HI;
const BORDER: Color32 = theme::HAIRLINE_DARK;
const RING: Color32 = theme::CREAM_DIM;
const PLUG: Color32 = theme::DARK_PANEL_HI;
const TEXT: Color32 = theme::CREAM;
const SUBTEXT: Color32 = theme::CREAM_MID;
/// Knob radius on unit faces (labels and display captions share its baseline).
const KNOB_R: f32 = 17.0;
const METER_SEGS: usize = 14;
/// Face grid (rack-local px), shared by every unit face: content starts on
/// the title's left edge, keeps FACE_PAD_R from the right border and
/// FACE_PAD_B under the lowest label. The knob row sits on the bottom of
/// every unit, so rows line up across columns and tall units (Keys, Master)
/// share the bottom stack row's knob line.
const FACE_X: f32 = 34.0;
const FACE_PAD_R: f32 = 20.0;
const FACE_PAD_B: f32 = 14.0;
/// Knob label: top sits KNOB_R + KNOB_LABEL_GAP under the knob center.
const KNOB_LABEL_GAP: f32 = 10.0;
const KNOB_LABEL_H: f32 = 12.0;
/// Horizontal knob pitch and the half-slot reserved right of the last knob
/// (wide labels like VOLUME stay clear of the border).
const KNOB_PITCH: f32 = 56.0;
const KNOB_SLOT_HALF: f32 = 26.0;
const WHITE: Color32 = theme::ON_ACCENT; // #FFFBFA
const BLACK: Color32 = theme::DARK_PANEL; // true dark for glyphs on teal/lit fills

/// The reference CLAP instrument (docs/ARCHITECTURE.md §1). The engine's two voices are Surge,
/// so it is the only CLAP plugin that can currently be added from the browser.
const SURGE_ID: &str = "org.surge-synth-team.surge-xt";
const DEXED_ID: &str = "com.digital-suburban.dexed";

const MAX_SAMPLES: usize = 49; // arc-length LUT cap (48 segments per cable)
const MAX_PULSES: usize = 12;
/// Audio cables flow while their smoothed level exceeds this (docs/ARCHITECTURE.md §2).
const AUDIO_PULSE_MIN: f32 = 0.05;
/// Seconds between traveling pulses on a sounding audio cable.
const AUDIO_PULSE_EVERY: f32 = 0.25;
/// Key LED decay τ (s): 1.0 → the 0.02 lit threshold in ≈0.35 s.
const KEY_LED_TAU: f32 = 0.09;
/// How long a mouse-clicked on-screen key sounds.
const CLICK_NOTE_MS: u64 = 450;
/// How often hardware MIDI inputs are rescanned for hot-plugged devices.
const HW_RESCAN_SECS: f32 = 2.0;
/// Unit activity LED decay τ (s).
const UNIT_LED_TAU: f32 = 0.12;

/// Horizontal space a unit name may not use: 34px left text inset + the
/// activity LED (+halo) on the same row at the right edge.
const UNIT_NAME_INSET: f32 = 34.0 + 28.0;
/// Subtitle sits below the LED row: left inset + 12px right margin.
const UNIT_SUB_INSET: f32 = 34.0 + 12.0;

/// Shrink-to-fit for one-line unit labels (C4): scale the `natural` px size
/// down so `text` fits `avail` (floor `floor`); if it still overflows at the
/// floor, drop trailing characters and append an ellipsis. `measure(text,
/// size)` returns the laid-out width. Returns (text, size).
fn fit_label(text: &str, natural: f32, floor: f32, avail: f32, measure: impl Fn(&str, f32) -> f32) -> (String, f32) {
    let w = measure(text, natural);
    if !(avail < f32::INFINITY) || w <= avail { return (text.to_string(), natural); }
    let avail = avail.max(0.0);
    // Width scales ~linearly with size; step down to absorb hinting error.
    // Sizes are quantized to 0.5px so window resizes can't grow the font
    // atlas with a glyph set per fractional size.
    let mut size = ((natural * avail / w.max(1e-3)) * 2.0).floor() / 2.0;
    size = size.clamp(floor, natural);
    while size > floor && measure(text, size) > avail { size = (size - 0.5).max(floor); }
    if measure(text, size) <= avail { return (text.to_string(), size); }
    let chars: Vec<char> = text.chars().collect();
    for n in (0..chars.len()).rev() {
        let mut t: String = chars[..n].iter().collect::<String>().trim_end().to_string();
        t.push('…');
        if measure(&t, size) <= avail { return (t, size); }
    }
    ("…".to_string(), size)
}

/// Engine block peak → visual cable level 0..1 (meter-style dB map:
/// −48 dBFS → 0, 0 dBFS → 1), so quiet-but-audible signal still breathes.
fn edge_display_level(peak: f32) -> f32 {
    if !(peak > 0.0) { return 0.0; }
    ((20.0 * peak.log10() + 48.0) / 48.0).clamp(0.0, 1.0)
}
const MAX_CABLES: usize = 64;
const FLIP_SECS: f32 = 0.35;
// Damped spring: omega ≈ 11.4 rad/s (~1.8 Hz), zeta ≈ 0.3 → a few visible
// swings before settling. Semi-implicit Euler at ≤ 1/240 s substeps.
const SPRING_K: f32 = 130.0;
const SPRING_D: f32 = 6.8;

const WHITE_SEMI: [usize; 8] = [0, 2, 4, 5, 7, 9, 11, 12];
const BLACK_SEMI: [usize; 5] = [1, 3, 6, 8, 10];
const BLACK_AFTER: [usize; 5] = [0, 1, 3, 4, 5]; // white-key index each black key follows

// ---------------------------------------------------------------------------
// Small helpers (no allocation)
// ---------------------------------------------------------------------------

// alpha/mix live in the theme module (single source for the Signal palette).
use crate::theme::{alpha, mix};

#[inline]
fn clamp_len(v: Vec2, max: f32) -> Vec2 {
    let len = v.length();
    if len > max && len > 0.0 { v * (max / len) } else { v }
}

#[inline]
fn cubic(p: &[Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Pos2::new(
        w0 * p[0].x + w1 * p[1].x + w2 * p[2].x + w3 * p[3].x,
        w0 * p[0].y + w1 * p[1].y + w2 * p[2].y + w3 * p[3].y,
    )
}

fn bezier_shape(p: [Pos2; 4], width: f32, color: Color32) -> Shape {
    Shape::CubicBezier(egui::epaint::CubicBezierShape::from_points_stroke(
        p, false, Color32::TRANSPARENT, Stroke::new(width, color),
    ))
}

fn dist_sq_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_sq();
    let t = if len_sq > 1e-9 { ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0) } else { 0.0 };
    (p - (a + ab * t)).length_sq()
}

#[inline]
fn rng01(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32) / (u32::MAX as f32)
}

fn ease_in_cubic(x: f32) -> f32 { x * x * x }

fn ease_out_back(x: f32) -> f32 {
    let c1 = 1.70158f32;
    let c3 = c1 + 1.0;
    1.0 + c3 * (x - 1.0).powi(3) + c1 * (x - 1.0).powi(2)
}

// ---------------------------------------------------------------------------
// Audio-integration seam (task-4)
// ---------------------------------------------------------------------------

/// A note-on event emitted by the UI when a key is played. `note` is a
/// semitone offset (0 = C), `velocity` is normalized 0..1. The audio engine
/// subscribes via [`run_demo_with_note_sink`]; the render path never blocks on
/// it (fire-and-forget), so animation load cannot affect sound.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // fields are read by the audio engine's sink (task-4)
pub struct NoteEvent {
    pub note: u8,
    pub velocity: f32,
    pub source: SourceId,
    /// Arrival timestamp (hardware MIDI latency measurement, task-23).
    pub created: std::time::Instant,
}

/// Hardware MIDI → engine from the midir thread (see `DemoApp::hw_direct`).
/// Same mapping as the note sink: velocity scaled by the Keys VEL knob
/// (published lock-free by the UI), velocity 0 = NoteOff.
/// Hardware MIDI devices as the MIDI thread sees them: (port name, plugged
/// in) per device index = `SourceId::Hardware(index)`. An unplugged device
/// keeps its index, so rig bindings and learned CCs survive a re-plug.
#[derive(Default)]
struct HwMidi {
    devices: Mutex<Vec<(String, bool)>>,
    rev: std::sync::atomic::AtomicU64,
}

/// The MIDI thread: connects every hardware input, rescans every
/// `HW_RESCAN_SECS` for hot-plugged devices, and owns the connections, so
/// sound never depends on UI frames (rule 31). Ends with the app.
fn spawn_hw_midi(
    hw: Arc<HwMidi>,
    q: Arc<Mutex<VecDeque<(NoteEvent, bool)>>>,
    cc_q: Arc<Mutex<VecDeque<(SourceId, u8, u8, u8)>>>,
    direct: Arc<HwDirect>,
) {
    let spawned = std::thread::Builder::new().name("kabelsalat-midi".into()).spawn(move || {
        let mut conns: Vec<Option<midir::MidiInputConnection<()>>> = Vec::new();
        while Arc::strong_count(&hw) > 1 {
            rescan_hw_midi(&hw, &mut conns, &q, &cc_q, &direct);
            std::thread::sleep(std::time::Duration::from_secs_f32(HW_RESCAN_SECS));
        }
    });
    if let Err(e) = spawned {
        eprintln!("midi: hot-plug thread failed to start: {e}");
    }
}

/// A MIDI port name without ALSA's trailing "client:port" numbers
/// ("FM-1:FM-1 MIDI 1 24:0" → "FM-1:FM-1 MIDI 1"): stable across re-plugs.
fn midi_port_key(name: &str) -> &str {
    match name.rsplit_once(' ') {
        Some((head, tail)) if tail.split_once(':').is_some_and(|(c, p)| {
            !c.is_empty() && !p.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) && p.bytes().all(|b| b.is_ascii_digit())
        }) => head,
        _ => name,
    }
}

/// Which known device index a port belongs to (#16). `Err(())` = already
/// connected (live under this exact name), `Ok(None)` = a new device.
/// A re-plug can get another ALSA client number: an OFFLINE device with the
/// same name minus that number is the same keyboard, so it keeps its index
/// (and with it its rig binding and matrix row).
fn match_midi_device(devices: &[(String, bool)], name: &str) -> Result<Option<usize>, ()> {
    if let Some(i) = devices.iter().position(|(n, _)| n == name) {
        return if devices[i].1 { Err(()) } else { Ok(Some(i)) };
    }
    let key = midi_port_key(name);
    Ok(devices.iter().position(|(n, live)| !*live && midi_port_key(n) == key))
}

/// Is ALSA port `name` ("…  28:0") still subscribed by one of our
/// `kabelsalat-hw` clients, per the text of /proc/asound/seq/clients?
/// None = cannot tell (no "client:port" suffix, port not listed). A USB
/// re-plug that brings the device back under the SAME name drops the
/// subscription silently; the name alone said "still connected" and the
/// keyboard stayed dead.
fn alsa_port_subscribed(seq_clients: &str, name: &str) -> Option<bool> {
    let (c, p) = name.rsplit_once(' ')?.1.split_once(':')?;
    let (c, p): (u32, u32) = (c.parse().ok()?, p.parse().ok()?);
    let mut ours = Vec::new();
    let mut client: Option<u32> = None;
    for line in seq_clients.lines() {
        if let Some(rest) = line.strip_prefix("Client") {
            let n = rest.trim_start().split(|ch: char| !ch.is_ascii_digit()).next().and_then(|x| x.parse().ok());
            if let Some(n) = n { if line.contains("\"kabelsalat-hw\"") { ours.push(n); } }
            client = n;
        }
    }
    let mut in_port = false;
    let mut found = false;
    for line in seq_clients.lines() {
        if let Some(rest) = line.strip_prefix("Client") {
            let n: Option<u32> = rest.trim_start().split(|ch: char| !ch.is_ascii_digit()).next().and_then(|x| x.parse().ok());
            client = n;
            in_port = false;
            continue;
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Port") {
            let n: Option<u32> = rest.trim_start().split(|ch: char| !ch.is_ascii_digit()).next().and_then(|x| x.parse().ok());
            in_port = client == Some(c) && n == Some(p);
            if in_port { found = true; }
            continue;
        }
        if in_port {
            if let Some(targets) = t.strip_prefix("Connecting To:") {
                let to_us = targets.split(',').any(|x| {
                    x.trim().split(':').next().and_then(|n| n.trim().parse::<u32>().ok()).is_some_and(|n| ours.contains(&n))
                });
                if to_us { return Some(true); }
            }
        }
    }
    found.then_some(false)
}

fn rescan_hw_midi(
    hw: &HwMidi,
    conns: &mut Vec<Option<midir::MidiInputConnection<()>>>,
    q: &Arc<Mutex<VecDeque<(NoteEvent, bool)>>>,
    cc_q: &Arc<Mutex<VecDeque<(SourceId, u8, u8, u8)>>>,
    direct: &Arc<HwDirect>,
) {
    let Ok(probe) = midir::MidiInput::new("kabelsalat-probe") else { return };
    // KABELSALAT_MIDI_THROUGH=1 accepts the kernel's Midi Through port — a
    // hardware-path self-test feed (aseqsend -p 14:0).
    let through_ok = std::env::var("KABELSALAT_MIDI_THROUGH").is_ok_and(|v| !v.is_empty() && v != "0");
    let ports: Vec<(String, midir::MidiInputPort)> = probe.ports().into_iter()
        .map(|p| (probe.port_name(&p).unwrap_or_default(), p))
        .filter(|(name, _)| !(name.contains("kabelsalat")
            || (name.contains("Midi Through") && !through_ok)
            || name == "System"
            || name == "Announce"))
        .collect();
    // Decide under the lock, connect/close outside it: the UI frame reads
    // the same list and must never wait for an ALSA port to open.
    let (unplugged, to_connect) = {
        let Ok(mut devices) = hw.devices.lock() else { return };
        let mut unplugged = Vec::new();
        // A device re-plugged under the same name lost our subscription:
        // treat it as unplugged so this pass reconnects it.
        let seq = std::fs::read_to_string("/proc/asound/seq/clients").unwrap_or_default();
        for (i, (name, live)) in devices.iter_mut().enumerate() {
            let stale = *live && alsa_port_subscribed(&seq, name) == Some(false);
            if stale { eprintln!("midi: {name} lost its subscription (re-plugged?) — reconnecting"); }
            if *live && (stale || !ports.iter().any(|(n, _)| n == name)) {
                *live = false;
                unplugged.push(i);
            }
        }
        let mut to_connect = Vec::new();
        for (name, port) in &ports {
            let dev = match match_midi_device(&devices, name) {
                Err(()) => continue,
                Ok(Some(i)) => {
                    devices[i].0 = name.clone(); // the new client number
                    i
                }
                Ok(None) if devices.len() >= crate::engine::MAX_HW_DEVICES as usize => {
                    // The engine routes Hardware(0..16) only: a 17th id would be mute.
                    eprintln!("midi: {name} ignored — {} hardware devices is the limit", crate::engine::MAX_HW_DEVICES);
                    continue;
                }
                Ok(None) => {
                    devices.push((name.clone(), false));
                    devices.len() - 1
                }
            };
            to_connect.push((dev, name.clone(), port.clone()));
        }
        (unplugged, to_connect)
    };
    let mut changed = !unplugged.is_empty();
    for i in unplugged {
        if let Some(c) = conns.get_mut(i).and_then(Option::take) { c.close(); }
    }
    for (dev, name, port) in to_connect {
        if conns.len() <= dev { conns.resize_with(dev + 1, || None); }
        conns[dev] = connect_hw_input(&port, dev as u32, q, cc_q, direct);
        if conns[dev].is_some() {
            if let Ok(mut devices) = hw.devices.lock() {
                if let Some(d) = devices.get_mut(dev) { d.1 = true; }
            }
            eprintln!("midi: connected {name}");
            changed = true;
        }
    }
    if changed {
        hw.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// Open one hardware input. Events sound from the MIDI thread (rule 31) and
/// queue for the UI animation.
fn connect_hw_input(
    port: &midir::MidiInputPort,
    dev: u32,
    q: &Arc<Mutex<VecDeque<(NoteEvent, bool)>>>,
    cc_q: &Arc<Mutex<VecDeque<(SourceId, u8, u8, u8)>>>,
    direct: &Arc<HwDirect>,
) -> Option<midir::MidiInputConnection<()>> {
    let mi = midir::MidiInput::new("kabelsalat-hw").ok()?;
    let (q, cc_q, direct) = (q.clone(), cc_q.clone(), direct.clone());
    mi.connect(
        port,
        "kabelsalat-hw",
        move |_stamp, msg: &[u8], _| {
            if msg.len() >= 3 {
                let st = msg[0] & 0xF0;
                if st == 0x90 || st == 0x80 {
                    let vel = if st == 0x90 { msg[2] as f32 / 127.0 } else { 0.0 };
                    let ev = NoteEvent { note: msg[1], velocity: vel, source: SourceId::Hardware(dev), created: std::time::Instant::now() };
                    let sounded = direct.send(&ev);
                    if let Ok(mut q) = q.lock() {
                        q.push_back((ev, sounded));
                        while q.len() > 256 {
                            q.pop_front();
                        }
                    }
                } else if st == 0xB0 {
                    let ch = msg[0] & 0x0F;
                    let cc = msg[1] & 0x7F;
                    let val = msg[2] & 0x7F;
                    let src = SourceId::Hardware(dev);
                    let _ = direct.send_cc(src, ch, cc, val, std::time::Instant::now());
                    if let Ok(mut cq) = cc_q.lock() {
                        cq.push_back((src, ch, cc, val));
                        while cq.len() > 256 {
                            cq.pop_front();
                        }
                    }
                }
            }
        },
        (),
    ).ok()
}

struct HwDirect {
    bus: Mutex<Option<Arc<Mutex<CommandBus>>>>,
    /// Keys VEL knob as f32 bits (starts at the knob's 0.8 default).
    keys_vel: std::sync::atomic::AtomicU32,
}

impl Default for HwDirect {
    fn default() -> Self {
        Self { bus: Mutex::new(None), keys_vel: std::sync::atomic::AtomicU32::new(0.8f32.to_bits()) }
    }
}

impl HwDirect {
    fn set_keys_vel(&self, v: f32) { self.keys_vel.store(v.to_bits(), Ordering::Relaxed); }

    /// Send one hardware note to the engine. False when no engine is attached
    /// (the UI frame then sounds it through the note sink).
    fn send(&self, ev: &NoteEvent) -> bool {
        let Ok(guard) = self.bus.lock() else { return false };
        let Some(bus) = guard.as_ref() else { return false };
        let kv = f32::from_bits(self.keys_vel.load(Ordering::Relaxed));
        let cmd = if ev.velocity > 0.0 {
            Command::NoteOn { note: ev.note, velocity: (ev.velocity * (0.35 + kv * 0.65)).clamp(0.0, 1.0), source: ev.source }
        } else {
            Command::NoteOff { note: ev.note, source: ev.source }
        };
        let ok = bus.lock().map(|mut b| b.send(cmd)).unwrap_or(false);
        crate::bridge::record_midi_latency(ev.created.elapsed().as_micros() as u32);
        ok
    }

    /// Send one hardware CC straight to the engine from the midir thread.
    fn send_cc(&self, source: SourceId, channel: u8, cc: u8, value: u8, created: std::time::Instant) -> bool {
        let Ok(guard) = self.bus.lock() else { return false };
        let Some(bus) = guard.as_ref() else { return false };
        let cmd = Command::Cc {
            source,
            channel: channel & 0x0F,
            cc: cc & 0x7F,
            value: value & 0x7F,
        };
        let ok = bus.lock().map(|mut b| b.send(cmd)).unwrap_or(false);
        crate::bridge::record_midi_latency(created.elapsed().as_micros() as u32);
        ok
    }
}

/// Sink for [`NoteEvent`]s. `Send` so the engine can move it to its own thread.
pub type NoteSink = Box<dyn FnMut(NoteEvent) + Send>;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PortKind { Midi, Audio }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir { In, Out }

impl PortKind {
    fn color(self) -> Color32 {
        match self { PortKind::Midi => YELLOW, PortKind::Audio => theme::AUDIO_CREAM }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side { Front, Back }

struct Knob {
    label: String,
    value: f32,
    default: f32,
    /// Engine param id this knob publishes (None = visual-only knob).
    param: Option<u32>,
    /// Hidden from the face (still settable/bindable — e.g. AudioIn MON, shown
    /// as a chip instead of a knob). Skipped by draw + face-layout audit.
    hidden: bool,
    /// Last value sent via `Command::SetParam` so `publish_patch` only sends on change
    /// and never fights incoming hardware CCs.
    sent: Option<f32>,
    /// A Surge parameter pinned from the synth editor: the knob drives the
    /// PLUGIN parameter (0..1 mapped onto min..max), sent only on change.
    clap: Option<ClapPin>,
    label_g: Option<Arc<theme::TrackedLabel>>,
}

impl Knob {
    /// Engine or plugin target this knob controls, if bindable to MIDI CC.
    fn cc_target(&self) -> Option<(u32, crate::matrix::CcTarget)> {
        if let Some(pin) = self.clap {
            Some((pin.id, crate::matrix::CcTarget::PluginParam))
        } else {
            self.param.map(|p| (p, crate::matrix::CcTarget::Knob))
        }
    }
}

#[derive(Clone, Copy)]
struct ClapPin { id: u32, min: f64, max: f64, sent: f32 }

/// Most Surge parameters pinnable to one synth face (2 built-in + 4).
const MAX_PINS: usize = 4;

/// A jack on the back panel: direction, port index, x fraction of the unit.
struct Jack {
    dir: Dir,
    port: usize,
    xf: f32,
    label: &'static str,
    label_g: Option<Arc<theme::TrackedLabel>>,
}

/// What a rack unit IS — drives its face, knobs, and layout grouping.
#[derive(Clone)]
enum UnitKind {
    Keys,
    Clap,
    Dsp(crate::control::DspKind),
    Master,
    /// Live audio input (mic/guitar/interface): a source with an audio OUT.
    AudioIn,
}

struct Unit {
    /// Engine node id (control-plane Graph). Unit index ≠ node id in general.
    node_id: u32,
    /// Rig tag from the control-plane node (merge_rigs). The big view shows
    /// one rig at a time; single-graph mode is all-rig-0.
    rig: u8,
    /// LV2 instrument (vs effect) — matrix sink column. Classified in build_rack.
    lv2_inst: bool,
    node_kind: UnitKind,
    name: String,
    sub: String,
    ins: [PortKind; 3],
    outs: [PortKind; 3],
    knobs: Vec<Knob>,
    jacks: Vec<Jack>,
    led: f32,
    /// The node owns a start/stop transport (`NodeKind::supports_transport`:
    /// metronome today, the file player later) → its face gets a Play/Stop chip.
    transport: bool,
    /// Mirror of the engine's `Node::playing` (GraphPub), never a local guess
    /// while an engine is attached.
    playing: bool,
    /// Keys only: mirror of `Node::split`. Some = TWO MIDI OUT jacks (LOW on
    /// port 0, HIGH on port 1); None = one MIDI OUT. Never a local guess
    /// while an engine is attached (adopted from GraphPub).
    split: Option<u8>,
    /// AudioIn only: mirror of the node's device selection (None = default).
    device: Option<String>,
    /// AudioIn only: which device channels fill the stereo OUT.
    channels: AudioInChannel,
    name_g: Option<Arc<Galley>>,
    sub_g: Option<Arc<theme::TrackedLabel>>,
    /// Unit width the name/subtitle galleys were fitted to (C4 shrink-to-
    /// fit); INFINITY = natural size (rects not laid out yet).
    label_fit_w: f32,
    back_g: Option<Arc<theme::TrackedLabel>>,
}

impl Unit {
    fn jack_local(&self, unit_rect: Rect, ji: usize) -> Vec2 {
        let j = &self.jacks[ji];
        Vec2::new(unit_rect.left() + unit_rect.width() * j.xf, unit_rect.top() + unit_rect.height() * 0.54)
    }
    fn jack_index(&self, dir: Dir, port: usize) -> Option<usize> {
        self.jacks.iter().position(|j| j.dir == dir && j.port == port)
    }
    fn kind(&self, dir: Dir, port: usize) -> PortKind {
        if dir == Dir::In {
            self.ins.get(port).copied().unwrap_or(PortKind::Audio)
        } else {
            self.outs.get(port).copied().unwrap_or(PortKind::Audio)
        }
    }
}

/// Build a knob. `param` = engine param id it publishes (None = visual-only).
fn knob(label: &'static str, default: f32, param: Option<u32>) -> Knob {
    Knob { label: label.to_string(), value: default, default, param, hidden: false, sent: None, clap: None, label_g: None }
}

/// A hidden knob (drive-by chip/param, not drawn on the face). Still bindable
/// and persisted; the face renders its state as a chip instead.
fn hidden_knob(label: &'static str, default: f32, param: Option<u32>) -> Knob {
    Knob { label: label.to_string(), value: default, default, param, hidden: true, sent: None, clap: None, label_g: None }
}

/// Keys back-panel jacks: one MIDI OUT unsplit; LOW (port 0) + HIGH
/// (port 1) when split (AUM model: a split keyboard is two MIDI sources).
fn keys_jacks(split: Option<u8>) -> Vec<(Dir, usize, f32, &'static str)> {
    match split {
        None => vec![(Dir::Out, 0, 0.62, "MIDI OUT")],
        Some(_) => vec![(Dir::Out, 0, 0.44, "LOW"), (Dir::Out, 1, 0.80, "HIGH")],
    }
}

/// Put a Keys unit's split state + matching jack set in place. Jack label
/// galleys refill lazily in `ensure_galleys`.
fn set_unit_split(u: &mut Unit, split: Option<u8>) {
    if u.split == split && u.jacks.len() == keys_jacks(split).len() { return; }
    u.split = split;
    u.jacks = keys_jacks(split).into_iter()
        .map(|(dir, port, xf, label)| Jack { dir, port, xf, label, label_g: None })
        .collect();
}

/// Value of the knob bound to `param`, or `default` if the unit has no such knob.
fn knob_val(u: &Unit, param: u32, default: f32) -> f32 {
    u.knobs.iter().find(|k| k.param == Some(param)).map(|k| k.value).unwrap_or(default)
}

/// Visual breathing factor for audio leaving a unit (the engine does the real
/// audio; this only drives the cable width breathing). Clap → LEVEL, Delay →
/// MIX, other DSP → its first knob, everything else → 1.
fn unit_out_gain(u: &Unit) -> f32 {
    match &u.node_kind {
        UnitKind::Clap => 0.25 + knob_val(u, P_LEVEL, 0.85) * 0.75,
        UnitKind::Dsp(DspKind::Delay) => 0.15 + knob_val(u, P_MIX, 0.5) * 0.85,
        UnitKind::Dsp(_) => 0.15 + knob_val(u, 0, 0.5) * 0.85,
        _ => 1.0,
    }
}

/// Pulse-speed factor for cables leaving a unit (Delay TIME slows/quickens).
fn unit_time_mul(u: &Unit) -> f32 {
    match &u.node_kind {
        UnitKind::Dsp(DspKind::Delay) => 0.55 + knob_val(u, P_TIME, 0.4) * 1.3,
        _ => 1.0,
    }
}

/// One row of the hot-wire MIDI matrix: a MIDI source that can
/// feed any instrument rig. `Keys` = the computer keyboard (on-screen clicks
/// and laptop typing are ONE source); `HwAll` = any hardware device (omni).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MatrixRow { Keys, HwAll, HwDev(u32) }

impl MatrixRow {
    /// The concrete source a row edits (HwAll is an aggregate — no single rule).
    fn source(self) -> Option<SourceId> {
        match self {
            MatrixRow::Keys => Some(SourceId::Keys),
            MatrixRow::HwAll => None,
            MatrixRow::HwDev(d) => Some(SourceId::Hardware(d)),
        }
    }
    fn label(self) -> String {
        match self {
            MatrixRow::Keys => "Keyboard".into(),
            MatrixRow::HwAll => "HW all".into(),
            MatrixRow::HwDev(d) => format!("HW {d}"),
        }
    }
}

/// UI-side mirror of one matrix rule's editable fields (Command::MatrixRule + MatrixCcMap).
/// The engine owns the truth; this drives the editor + cell indicators.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RuleEdit {
    channel: Option<u8>,
    transpose: i8,
    key_lo: u8,
    key_hi: u8,
    vel_lo: u8,
    vel_hi: u8,
    enabled: bool,
    cc_map: Vec<crate::matrix::CcMapping>,
    /// The engine rule exists only for CC bindings (no notes routed).
    #[serde(default)]
    cc_only: bool,
}

/// One shaped matrix route as a session stores it (`Session::matrix`).
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedRule {
    source: SourceId,
    dest: u32,
    #[serde(flatten)]
    edit: RuleEdit,
}

/// `s` moved onto `graph`, every node id mapped by `remap` (None drops it):
/// knobs, plugin states and matrix routes follow their node.
fn remap_session(
    s: &crate::session::Session,
    graph: Graph,
    remap: impl Fn(u32) -> Option<u32>,
) -> crate::session::Session {
    let mut ns = crate::session::Session::new(s.name.clone(), graph);
    ns.rig_names = s.rig_names.clone(); // names follow their rig (#33)
    for p in s.params() {
        if let Some(nid) = remap(p.node) { ns.set_param(nid, p.param, p.value); }
    }
    for st in s.plugin_states() {
        if let Some(nid) = remap(st.node) { ns.set_plugin_state(nid, st.plugin_id.clone(), st.data.clone()); }
    }
    let rules: Vec<SavedRule> = s.matrix()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let rules: Vec<SavedRule> = rules.into_iter()
        .filter_map(|mut r| { r.dest = remap(r.dest)?; Some(r) })
        .collect();
    if !rules.is_empty() {
        ns.set_matrix(serde_json::to_value(&rules).ok());
    }
    ns
}

/// A multi-rig session in the id layout the engine's LoadRigs produces
/// (split → merge). Gaps left by removed or added nodes shift a later rig's
/// ids there, so everything keyed on node ids is remapped first.
fn canonical_rig_session(s: &crate::session::Session) -> crate::session::Session {
    if s.graph.nodes.iter().all(|n| n.rig == 0) {
        return s.clone();
    }
    let (graphs, old_bases) = crate::control::split_rigs(&s.graph);
    let rigs: Vec<crate::control::Rig> = graphs.into_iter()
        .map(|g| crate::control::Rig::new(String::new(), g, None))
        .collect();
    let new_bases = crate::control::rig_bases(&rigs);
    let merged = crate::control::merge_rigs(&rigs);
    remap_session(s, merged, |old| {
        let rig = s.graph.node(old)?.rig as usize;
        Some(new_bases[rig] + (old - old_bases[rig]))
    })
}

impl Default for RuleEdit {
    fn default() -> Self {
        Self {
            channel: None,
            transpose: 0,
            key_lo: 0,
            key_hi: 127,
            vel_lo: 0,
            vel_hi: 127,
            enabled: true,
            cc_map: Vec::new(),
            cc_only: false,
        }
    }
}

impl RuleEdit {
    fn is_default(&self) -> bool {
        self.channel.is_none()
            && self.transpose == 0
            && self.key_lo == 0
            && self.key_hi == 127
            && self.vel_lo == 0
            && self.vel_hi == 127
            && self.enabled
            && self.cc_map.is_empty()
            && !self.cc_only
    }
    fn set_cc(&mut self, cc: u8, param: u32, target: crate::matrix::CcTarget) {
        let cc = cc & 0x7F;
        if let Some(m) = self.cc_map.iter_mut().find(|m| m.cc == cc) {
            m.param = param;
            m.target = target;
        } else {
            self.cc_map.push(crate::matrix::CcMapping { cc, param, target });
        }
    }
    fn unmap_cc(&mut self, cc: u8) {
        let cc = cc & 0x7F;
        self.cc_map.retain(|m| m.cc != cc);
    }
    fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(ch) = self.channel { parts.push(format!("ch {}", ch + 1)); }
        if self.transpose != 0 { parts.push(format!("{:+} st", self.transpose)); }
        if (self.key_lo, self.key_hi) != (0, 127) { parts.push(format!("keys {}–{}", self.key_lo, self.key_hi)); }
        if (self.vel_lo, self.vel_hi) != (0, 127) { parts.push(format!("vel {}–{}", self.vel_lo, self.vel_hi)); }
        if !self.cc_map.is_empty() { parts.push(format!("{} CC", self.cc_map.len())); }
        if !self.enabled { parts.push("muted".into()); }
        if parts.is_empty() { "transparent".into() } else { parts.join(" · ") }
    }
    /// In-cell code for a routed matrix cell: the rule's defining change,
    /// so a shaped route reads differently from a plain one at a glance.
    fn cell_code(&self) -> String {
        if !self.enabled { return "off".into(); }
        if self.transpose != 0 { return format!("{:+}", self.transpose); }
        if let Some(ch) = self.channel { return format!("ch {}", ch + 1); }
        if (self.key_lo, self.key_hi) != (0, 127) { return "keys".into(); }
        if (self.vel_lo, self.vel_hi) != (0, 127) { return "vel".into(); }
        if !self.cc_map.is_empty() { return format!("{} CC", self.cc_map.len()); }
        "ALL".into()
    }
    fn to_command(&self, source: SourceId, dest: u32) -> Command {
        Command::MatrixRule {
            source, dest,
            channel: self.channel,
            transpose: self.transpose,
            key_lo: self.key_lo, key_hi: self.key_hi,
            vel_lo: self.vel_lo, vel_hi: self.vel_hi,
            enabled: self.enabled,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CcLearn {
    source: SourceId,
    dest: u32,
    param: u32,
    target: crate::matrix::CcTarget,
    /// When armed from a rack knob, replace any prior binding for that knob on completion.
    replace_existing: bool,
}

/// The knob context menu, keyed by what the knob drives — never unit/knob
/// indices, which a rebuild can shift under an open menu (rule 10, #24).
/// A destructive tool the assistant asked about: what to run on Confirm.
#[derive(Clone, Debug)]
struct PendingConfirm {
    tool: String,
    args: serde_json::Value,
    /// Button text, e.g. "Delete rig 2 · Bass".
    label: String,
}

/// Tools that change things the user cannot undo — they need confirm=true
/// after asking (rule 23), and the in-app chat offers Confirm / Cancel.
const CONFIRM_TOOLS: &[&str] = &["delete_rig", "install_instrument"];

/// Tools whose edit the ENGINE applies: the rack only follows once the
/// engine published it (rule 70).
const ENGINE_EDIT_TOOLS: &[&str] = &["add_node", "connect", "disconnect", "remove_node"];
/// How long a tool batch waits for the engine to publish one edit.
const ENGINE_EDIT_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// One model step's tool calls, run in call order. An engine edit is a
/// barrier: the next call runs only after the engine published the edit and
/// the rack adopted it, so every later call — and the model's next step —
/// sees what the calls before it really did.
struct ToolBatch {
    todo: VecDeque<ToolCall>,
    results: Vec<String>,
    waiting: Option<EngineEdit>,
}

/// An engine edit whose effect on the rack is still awaited.
struct EngineEdit {
    /// What the tool said when it sent the command.
    result: String,
    /// Node and cable ids before the edit.
    before: (Vec<u32>, Vec<u32>),
    deadline: std::time::Instant,
}

#[derive(Clone, Copy)]
struct KnobMenu {
    node: u32,
    param: u32,
    target: crate::matrix::CcTarget,
    pos: Pos2,
}

/// MIDI note name for editor readouts (60 = C4).
fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[(n % 12) as usize], n as i32 / 12 - 1)
}

/// Effective cell state for a CLAP rig: the MIDI cable gate AND the source
/// filter/device binding (mirrors engine::source_accepted + keys_gate).
/// `src` = the rig's (keyboard, hardware) source pair.
fn clap_cell(src: (bool, bool), dev: Option<u32>, gate: bool, row: MatrixRow) -> bool {
    if !gate { return false; }
    match row {
        MatrixRow::Keys => src.0,
        MatrixRow::HwAll => src.1 && dev.is_none(),
        MatrixRow::HwDev(d) => src.1 && dev == Some(d),
    }
}

/// Parse an AudioIn channel name (L / R / ST / mono_l / mono_r / stereo).
fn parse_audio_channel(s: &str) -> AudioInChannel {
    match s.trim().to_lowercase().as_str() {
        "r" | "right" | "mono_r" | "monor" => AudioInChannel::MonoR,
        "st" | "stereo" | "s" => AudioInChannel::Stereo,
        _ => AudioInChannel::MonoL,
    }
}

/// Readable AudioIn subtitle: "MOTU M2 · In 1" (device · channel).
fn audio_in_sub(device: Option<&str>, ch: AudioInChannel) -> String {
    let dev = device
        .map(crate::audio_in::pretty_device_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Default input".to_string());
    format!("{dev} · {}", ch.label())
}

/// Sentence case for unit names (§4.3): "SURGE A" → "Surge A", "MASTER OUT"
/// → "Master out". Single-letter words (rig tags A/B) stay uppercase.
/// Runs once per unit at galley-build time, never on the frame path.
fn sentence_case(s: &str) -> String {
    s.split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            if w.chars().count() == 1 { return w.to_uppercase(); }
            let mut c = w.chars();
            match c.next() {
                Some(f) if i == 0 => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                _ => w.to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Real chip toggle (user request 2026-11-15: buttons, not underlined text).
/// On = near-black chip + cream text; off = recessed paper chip + hairline.
fn text_toggle(ui: &mut egui::Ui, name: &str, on: bool) -> bool {
    theme::chip_button(ui, name, on).clicked()
}

/// Nav item as a real button chip (§4.14, updated 2026-11-15): active/open =
/// near-black fill + cream text; closed = recessed paper chip. No underlines.
use crate::control::VOICE_POOL;

fn nav_button(ui: &mut egui::Ui, name: &str, active: bool) -> bool {
    // Hover names the letter shortcut (M / N / C / B).
    let key = match name { "Mixer" => Some("M"), "Matrix" => Some("N"), "Assistant" => Some("C"), "Keyboard" => Some("V"), "Plugins" => Some("B"), _ => None };
    let r = theme::chip_button(ui, name, active);
    match key { Some(k) => r.on_hover_text(format!("{name} · key {k}")).clicked(), None => r.clicked() }
}

/// 1px hairline vertical rule between nav items (§4.14).
/// Open a folder in the desktop's file manager (detached; never in tests).
fn open_folder(dir: &std::path::Path) {
    if cfg!(test) { return; } // tests never touch the user's folders (rule 37)
    let _ = std::fs::create_dir_all(dir);
    let _ = std::process::Command::new("xdg-open").arg(dir)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

fn nav_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, 14.0), egui::Sense::hover());
    ui.painter().line_segment(
        [Pos2::new(rect.center().x, rect.top()), Pos2::new(rect.center().x, rect.bottom())],
        Stroke::new(1.0, theme::HAIRLINE),
    );
}

/// The REC chip: tomato fill + a filled dot only while recording (Hurricane:
/// tomato is reserved for the active record state); a calm dark chip with a
/// dim open dot when idle. `secs` shows elapsed time while recording.
fn rec_chip(ui: &mut egui::Ui, recording: bool, secs: f64) -> egui::Response {
    let (fill, stroke, txt) = if recording {
        (theme::ACCENT_DEEP, theme::ACCENT_DEEP, theme::ON_ACCENT)
    } else {
        (theme::DARK_PANEL_HI, theme::HAIRLINE, theme::CREAM_MID)
    };
    let dot = if recording { "●" } else { "○" };
    let label = if recording { format!("{dot} REC {}:{:02}", (secs as u64) / 60, (secs as u64) % 60) } else { format!("{dot} REC") };
    ui.scope(|ui| {
        let base = egui::style::WidgetVisuals {
            bg_fill: fill,
            weak_bg_fill: fill,
            bg_stroke: Stroke::new(1.0, stroke),
            corner_radius: 2.0.into(),
            fg_stroke: Stroke::new(1.0, txt),
            expansion: 0.0,
        };
        let hot = egui::style::WidgetVisuals {
            bg_fill: if recording { theme::mix(theme::ACCENT_DEEP, theme::INK, 0.08) } else { theme::mix(theme::DARK_PANEL_HI, theme::CREAM, 0.07) },
            weak_bg_fill: fill,
            ..base
        };
        let v = ui.visuals_mut();
        v.widgets.inactive = base;
        v.widgets.hovered = hot;
        v.widgets.active = hot;
        v.widgets.open = base;
        ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
        ui.add(
            egui::Button::new(egui::RichText::new(label).size(12.5).color(txt))
                .corner_radius(2.0)
                .min_size(egui::vec2(0.0, 26.0)),
        )
    }).inner
}

/// Build a rack unit from a control-plane node. The face (jacks + knobs) is
/// chosen by node kind; known kinds keep their bespoke faces, other DSP kinds
/// get a generic labeled box with 1–2 knobs bound to engine params 0/1.
/// Unit identity is carried by typography and position — no per-unit hues.
fn make_unit(node: &Node) -> Unit {
    let name = name_str(&node.name).to_string();
    // Knobs come from the one table agents resolve names against (#19).
    let knobs: Vec<Knob> = crate::engine::knob_specs(&node.kind)
        .iter()
        .map(|k| if k.hidden { hidden_knob(k.label, k.default, k.param) } else { knob(k.label, k.default, k.param) })
        .collect();
    let (node_kind, sub, ins, outs, jacks): (
        UnitKind, String, Vec<PortKind>, Vec<PortKind>, Vec<(Dir, usize, f32, &'static str)>,
    ) = match &node.kind {
        NodeKind::Keys => (
            UnitKind::Keys, "midi source".into(),
            vec![], vec![PortKind::Midi, PortKind::Midi],
            keys_jacks(node.split),
        ),
        NodeKind::Clap { .. } => (
            UnitKind::Clap, "synth · clap".into(),
            vec![PortKind::Midi], vec![PortKind::Audio],
            vec![(Dir::In, 0, 0.30, "MIDI IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::MasterOut => (
            UnitKind::Master, "stereo + mono bus".into(),
            vec![PortKind::Audio, PortKind::Audio, PortKind::Audio], vec![],
            // L · R as a stereo pair, MONO apart (between them it read as a
            // center channel — vision review).
            vec![(Dir::In, 0, 0.22, "IN L"), (Dir::In, 1, 0.44, "IN R"), (Dir::In, 2, 0.74, "IN MONO")],
        ),
        NodeKind::Dsp(DspKind::Delay) => (
            UnitKind::Dsp(DspKind::Delay), "delay fx · midi tap".into(),
            vec![PortKind::Audio, PortKind::Midi], vec![PortKind::Audio],
            // IN sits under Surge's OUT (right) and MIDI IN faces Keys (left):
            // the audio cable no longer crosses the MIDI jack (vision review).
            vec![(Dir::In, 1, 0.22, "MIDI IN"), (Dir::Out, 0, 0.48, "OUT"), (Dir::In, 0, 0.74, "IN")],
        ),
        NodeKind::Dsp(kind) => {
            let sub: &str = match kind {
                DspKind::Reverb => "reverb",
                DspKind::Gain => "gain",
                DspKind::Filter => "filter",
                DspKind::Eq3 => "3-band eq",
                DspKind::Limiter => "limiter",
                DspKind::Saturation => "saturation",
                DspKind::Pan => "panner",
                DspKind::Metronome => "metronome · transport click",
                DspKind::Delay => unreachable!(),
            };
            (
                UnitKind::Dsp(kind.clone()), sub.into(),
                vec![PortKind::Audio], vec![PortKind::Audio],
                vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
            )
        }
        NodeKind::Lv2 { .. } => (
            UnitKind::Dsp(DspKind::Gain), "lv2 effect".into(),
            vec![PortKind::Audio], vec![PortKind::Audio],
            vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::SystemAudio => (
            UnitKind::Dsp(DspKind::Gain), "system audio".into(),
            vec![], vec![PortKind::Audio],
            vec![(Dir::Out, 0, 0.62, "OUT")],
        ),
        NodeKind::AudioIn { device, channels } => (
            UnitKind::AudioIn, audio_in_sub(device.as_deref(), *channels),
            vec![], vec![PortKind::Audio],
            vec![(Dir::Out, 0, 0.62, "OUT")],
        )
    };

    let mut ins_arr = [PortKind::Audio; 3];
    let mut outs_arr = [PortKind::Audio; 3];
    for (i, k) in ins.iter().take(3).enumerate() { ins_arr[i] = *k; }
    for (i, k) in outs.iter().take(3).enumerate() { outs_arr[i] = *k; }
    let (device, channels) = match &node.kind {
        NodeKind::AudioIn { device, channels } => (device.clone(), *channels),
        _ => (None, AudioInChannel::MonoL),
    };
    Unit {
        node_id: node.id,
        rig: node.rig,
        lv2_inst: false,
        node_kind,
        name,
        sub,
        ins: ins_arr,
        outs: outs_arr,
        knobs,
        jacks: jacks.iter().map(|&(dir, port, xf, label)| Jack { dir, port, xf, label, label_g: None }).collect(),
        led: 0.0,
        transport: node.kind.supports_transport(),
        playing: node.playing && node.kind.supports_transport(),
        split: if matches!(node.kind, NodeKind::Keys) { node.split } else { None },
        device,
        channels,
        name_g: None, sub_g: None, back_g: None, label_fit_w: f32::INFINITY,
    }
}

#[derive(Clone, Copy)]
struct Pulse {
    active: bool,
    t: f32,     // arc-length fraction 0..1 (may start negative = delayed)
    speed: f32,
    amp: f32,
}

struct Cable {
    id: u32,
    src: usize,
    src_port: usize,
    dst: usize,
    dst_port: usize,
    kind: PortKind,
    color: Color32,
    pattern: u8, // 0 = melody, 1 = tap (MIDI only)

    // Spring particles: control-point offsets relative to each endpoint.
    c1: Vec2, v1: Vec2,
    c2: Vec2, v2: Vec2,
    prev_a: Vec2, prev_b: Vec2,
    // Endpoint overrides while a plug is being dragged.
    over_a: Option<Vec2>,
    over_b: Option<Vec2>,

    bulge_side: f32,
    bulge_var: f32,

    // Local-space samples + cumulative arc length, rebuilt each frame.
    pts: [Pos2; MAX_SAMPLES],
    lut: [f32; MAX_SAMPLES],
    n: usize,
    total_len: f32,
    curve: [Pos2; 4],

    pulses: [Pulse; MAX_PULSES],
    level: f32,
    env: f32,
    phase: f32,
    /// Audio only: countdown to the next level-driven traveling pulse.
    audio_tick: f32,

    age: f32,
    dying: f32, // < 0 alive; else unplug-fall progress (seconds)
    detach_dst: bool,
    drift: f32,

    next_event: f64,
    pat_idx: usize,
    rng: u32,
}

impl Cable {
    fn new(id: u32, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind, color: Color32, pattern: u8, seed: u32) -> Self {
        let dead = Pulse { active: false, t: 0.0, speed: 0.0, amp: 0.0 };
        Self {
            id, src, src_port, dst, dst_port, kind, color, pattern,
            c1: Vec2::ZERO, v1: Vec2::ZERO, c2: Vec2::ZERO, v2: Vec2::ZERO,
            prev_a: Vec2::ZERO, prev_b: Vec2::ZERO,
            over_a: None, over_b: None,
            bulge_side: 1.0, bulge_var: 1.0,
            pts: [Pos2::ZERO; MAX_SAMPLES], lut: [0.0; MAX_SAMPLES],
            n: 0, total_len: 0.0, curve: [Pos2::ZERO; 4],
            pulses: [dead; MAX_PULSES],
            level: 0.0, env: 0.0, phase: (seed % 628) as f32 / 100.0, audio_tick: 0.0,
            age: 0.0, dying: -1.0, detach_dst: true, drift: 0.0,
            next_event: 0.0, pat_idx: 0, rng: seed | 1,
        }
    }

    fn spawn_pulse(&mut self, t: f32, amp: f32) {
        let speed = 1.55 * (0.9 + 0.25 * rng01(&mut self.rng));
        let slot = self.pulses.iter().position(|p| !p.active).unwrap_or(0);
        self.pulses[slot] = Pulse { active: true, t, speed, amp };
    }

    fn arc_to_param(&self, frac: f32) -> f32 {
        let s = frac.clamp(0.0, 1.0) * self.total_len;
        let n = self.n;
        if n < 2 { return 0.0; }
        for i in 0..n - 1 {
            if s <= self.lut[i + 1] || i == n - 2 {
                let span = self.lut[i + 1] - self.lut[i];
                let f = if span > 1e-6 { (s - self.lut[i]) / span } else { 0.0 };
                return (i as f32 + f.clamp(0.0, 1.0)) / (n - 1) as f32;
            }
        }
        1.0
    }

    fn point_at_arc(&self, frac: f32) -> Pos2 {
        cubic(&self.curve, self.arc_to_param(frac))
    }
}

struct Pending {
    unit: usize,
    jack: usize,
    dir: Dir,
    kind: PortKind,
    cursor: Vec2, // rack-local
    c1: Vec2, v1: Vec2, c2: Vec2, v2: Vec2,
    prev_b: Vec2,
}

/// Outcome of dropping a held plug on a jack (`replug_cable`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Replug { Moved, Refused, Incompatible }

/// Help card row: key chips (cream Medium 12px) + a caption line.
struct HelpRow {
    keys: Vec<Arc<Galley>>,
    desc: Arc<Galley>,
}

/// Key chip geometry (AGENTS.md rule 7: ≥ 8px horizontal / ≥ 4px vertical).
/// Face controls (Keys SPLIT chip + point readout, metronome BPM): 24px
/// tall keeps ≥ 6px vertical padding around 12px labels (rule 7); widths
/// leave ≥ 8px horizontal padding around their widest text.
const FACE_CTL_H: f32 = 24.0;
const SPLIT_CHIP_W: f32 = 58.0;
const SPLIT_READOUT_W: f32 = 46.0;
const BPM_STEP_W: f32 = 22.0;
const BPM_VALUE_W: f32 = 66.0;
const METRO_TAP_W: f32 = 48.0;
const METRO_COUNT_IN_W: f32 = 84.0;

/// Metronome-face BPM control rects (rack-local).
#[derive(Clone, Copy, Debug)]
struct BpmRects {
    minus: Option<Rect>,
    value: Rect,
    plus: Option<Rect>,
}

/// Metronome-face tap-tempo + count-in chip rects (rack-local).
#[derive(Clone, Copy, Debug)]
struct MetroActionRects {
    tap: Rect,
    count_in: Rect,
    compact: bool,
}

const HELP_CHIP_PAD: Vec2 = Vec2::new(8.0, 4.0);
const HELP_CHIP_GAP: f32 = 5.0;
const HELP_KEY_GAP: f32 = 14.0;

impl HelpRow {
    fn chip_w(g: &Galley) -> f32 { (g.size().x + 2.0 * HELP_CHIP_PAD.x).max(26.0) }
    fn keys_width(&self) -> f32 {
        let w: f32 = self.keys.iter().map(|g| Self::chip_w(g)).sum();
        w + HELP_CHIP_GAP * self.keys.len().saturating_sub(1) as f32
    }
}

/// One help column: tracked section header + rows, widths measured once.
struct HelpCol {
    header: Arc<theme::TrackedLabel>,
    rows: Vec<HelpRow>,
    keys_w: f32,
    width: f32,
}

#[derive(Clone, Copy)]
enum Hit {
    Play,
    Knob { unit: usize, idx: usize },
    Jack { unit: usize, jack: usize },
    /// Cable id (not index — `update()` retains dying cables, which shifts
    /// indices mid-gesture; AGENTS.md rule 10) + grab parameter.
    Cable(u32, f32),
    Unit(usize),
    Background,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum StageView {
    #[default]
    Rack,
    Matrix,
    Mixer,
    Sessions,
    Plugins,
}

/// The bottom dock holds exactly one occupant at a time: no dock, the full-width
/// software keyboard, or the assistant chat. One owner of the computer keyboard
/// (rule: the assistant captures typing; the keyboard plays notes; neither when
/// the dock is closed).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum DockView {
    #[default]
    None,
    Keyboard,
    Assistant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum PluginCategory {
    #[default]
    All,
    Clap,
    Lv2,
    BuiltIn,
    /// The instrument browser's installable catalog (issue #10).
    Install,
}

/// One progress event from an install worker thread.
enum InstallEvent {
    Log(String),
    Done(Result<(), String>),
}

/// State of the running (or last-finished) instrument install.
struct InstallState {
    id: String,
    rx: Option<std::sync::mpsc::Receiver<InstallEvent>>,
    log: Vec<String>,
    running: bool,
    error: Option<String>,
}

enum Drag {
    None,
    Knob { unit: usize, idx: usize, start_val: f32, press_y: f32 },
    /// Holding a plug: `id` is the cable id, `end` the held end.
    Plug { id: u32, end: Dir },
    Cable { id: u32, u: f32 },
    /// Ctrl+drag a rack unit to rearrange it (task-21). `grab` is the pointer's
    /// rack-local offset inside the unit at press, so the box tracks the cursor.
    Unit { unit: usize, grab: Vec2 },
    /// Dragging a Keys face's split-point readout: `cur` is the point last
    /// sent, shown while dragging (the engine publication lags a block).
    Split { unit: usize, start: u8, press: Pos2, cur: u8 },
    /// Dragging a metronome face's BPM readout (the one global tempo).
    Bpm { start: f32, press: Pos2 },
}

/// Rack-local → screen transform for the flip (squash around the horizontal
/// axis). All rack drawing goes through this.
#[derive(Clone, Copy)]
struct Xform {
    center: Pos2, // screen
    lc: Vec2,     // rack-local center
    sx: f32,
    sy: f32,
}

impl Xform {
    fn p(&self, l: Vec2) -> Pos2 {
        Pos2::new(
            self.center.x + (l.x - self.lc.x) * self.sx,
            self.center.y + (l.y - self.lc.y) * self.sy,
        )
    }
    fn rect(&self, r: Rect) -> Rect {
        Rect::from_min_max(self.p(r.min.to_vec2()), self.p(r.max.to_vec2()))
    }
    fn inv(&self, s: Pos2) -> Vec2 {
        Vec2::new(
            self.lc.x + (s.x - self.center.x) / self.sx,
            self.lc.y + (s.y - self.center.y) / self.sy,
        )
    }
}

// ---------------------------------------------------------------------------
// Agent activity tracking (rack / cable / matrix / assistant highlights)
// ---------------------------------------------------------------------------

/// Duration (seconds) an agent-touched unit/cable/cell remains highlighted
/// before fading completely away.
const AGENT_TOUCH_SECS: f32 = 2.6;
/// Fade-out tail (seconds) at the end of [`AGENT_TOUCH_SECS`].
const AGENT_FADE_SECS: f32 = 1.2;

/// One agent-undo step: everything a rig edit or session load restores
/// (graph + knobs + plugin states + matrix via the session, plus the rig
/// list, mixer, rig in view and patch names that the session does not carry).
struct UndoSnap {
    session: crate::session::Session,
    rigs: Vec<crate::control::RigMeta>,
    mix: [crate::engine::RigMix; 8],
    active: usize,
    patch_names: [Option<String>; VOICE_POOL],
    /// Per-voice keyboard/hardware source switches (not in the session).
    rig_sources: [(bool, bool); VOICE_POOL],
}

/// Tools the running app serves to `kabelsalat mcp --attach` over the UI
/// bridge (they need the app's UI-thread state). Every MCP tool that calls
/// `ui_bridge_tool("…")` must be listed — `ui_bridge_tools_cover_mcp` checks.
const UI_BRIDGE_TOOLS: &[&str] = &[
    "load_sound", "list_sounds", "list_params", "list_cc_maps", "set_cc_map", "clear_cc_map",
    "learn_cc", "get_matrix", "undo", "redo", "rename_rig", "list_audio_outputs",
    "set_audio_output", "list_app_streams", "set_app_stream", "add_rig",
    "save_rig", "load_rig", "list_rig_presets",
    "list_instruments", "install_instrument", "play_file", "stop_file",
];

/// Agent undo depth (snapshots hold plugin states — keep it bounded).
const UNDO_LIMIT: usize = 32;
/// Remote (MCP) calls closer than this belong to one job / one undo step.
const REMOTE_JOB_GAP: f32 = 3.0;

/// Tools that only read: they never open an undo step.
fn tool_is_read_only(name: &str) -> bool {
    name.starts_with("get_") || name.starts_with("list_") || name.starts_with("search_")
        || matches!(name, "record_status" | "measure_mix" | "undo" | "redo")
}

/// Tools that change the machine, not the patch: no agent undo snapshot (an
/// install never touches the graph/session), but still a mutation for the
/// agent-paused gate (the bridge treats them as changing, not read-only).
fn tool_changes_only_system(name: &str) -> bool {
    matches!(name, "install_instrument" | "save_rig")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AgentOrigin {
    Assistant,
    Remote,
}

impl AgentOrigin {
    fn tag(self) -> &'static str {
        match self {
            AgentOrigin::Assistant => "ASST",
            AgentOrigin::Remote => "MCP",
        }
    }
}

#[derive(Clone, Debug)]
struct AgentAction {
    origin: AgentOrigin,
    summary: String,
    at: f32,
    nodes: Vec<u32>,
}

/// Model-first store of recent agent edits, keyed by stable domain IDs
/// (`NodeId`, `EdgeId`, `(SourceId, NodeId)`, rig index) so `build_rack`
/// rebuilds never drop or shift highlights.
#[derive(Default)]
struct AgentActivity {
    nodes: std::collections::HashMap<u32, f32>,
    edges: std::collections::HashMap<u32, f32>,
    matrix_cells: std::collections::HashMap<(SourceId, u32), f32>,
    rigs: std::collections::HashMap<usize, f32>,
    log: VecDeque<AgentAction>,
}

impl AgentActivity {
    fn touch_node(&mut self, node: u32, now: f32) {
        self.nodes.insert(node, now + AGENT_TOUCH_SECS);
    }

    fn touch_edge(&mut self, edge: u32, now: f32) {
        self.edges.insert(edge, now + AGENT_TOUCH_SECS);
    }

    fn touch_cell(&mut self, src: SourceId, node: u32, now: f32) {
        self.matrix_cells.insert((src, node), now + AGENT_TOUCH_SECS);
    }

    fn touch_rig(&mut self, rig: usize, now: f32) {
        self.rigs.insert(rig, now + AGENT_TOUCH_SECS);
    }

    fn record(&mut self, origin: AgentOrigin, summary: impl Into<String>, nodes: Vec<u32>, now: f32) {
        for &n in &nodes {
            self.touch_node(n, now);
        }
        self.log.push_front(AgentAction {
            origin,
            summary: summary.into(),
            at: now,
            nodes,
        });
        while self.log.len() > 24 {
            self.log.pop_back();
        }
    }

    fn strength(expiry: Option<&f32>, now: f32) -> f32 {
        match expiry {
            Some(&exp) if exp > now => ((exp - now) / AGENT_FADE_SECS).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    fn node_alpha(&self, node: u32, now: f32) -> f32 {
        Self::strength(self.nodes.get(&node), now)
    }

    fn edge_alpha(&self, edge: u32, now: f32) -> f32 {
        Self::strength(self.edges.get(&edge), now)
    }

    fn cell_alpha(&self, src: SourceId, node: u32, now: f32) -> f32 {
        Self::strength(self.matrix_cells.get(&(src, node)), now)
    }

    fn rig_alpha(&self, rig: usize, now: f32) -> f32 {
        Self::strength(self.rigs.get(&rig), now)
    }

    fn has_active(&self, now: f32) -> bool {
        self.nodes.values().any(|&e| e > now)
            || self.edges.values().any(|&e| e > now)
            || self.matrix_cells.values().any(|&e| e > now)
            || self.rigs.values().any(|&e| e > now)
    }

    fn prune(&mut self, now: f32) {
        self.nodes.retain(|_, exp| *exp > now);
        self.edges.retain(|_, exp| *exp > now);
        self.matrix_cells.retain(|_, exp| *exp > now);
        self.rigs.retain(|_, exp| *exp > now);
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct DemoApp {
    units: Vec<Unit>,
    cables: Vec<Cable>,
    next_id: u32,

    side: Side,
    flipping: bool,
    flip_t: f32,
    flip_to: Side,

    time: f64,    // musical clock (pausable)
    ui_time: f32, // wall clock for idle breathing / affordances
    playing: bool, // transport state: engine metronome running
    bpm: f32,      // metronome tempo (20..=300), mirrors Command::SetBpm
    tap_tempo: crate::transport::TapTempo,
    tap_origin: std::time::Instant,
    tap_flash_t: f32,
    count_in_enabled: bool,
    count_in_bars: u8,
    counting_in: bool,
    count_in_beat: u8,
    last_transport_rev: u64,

    drag: Drag,
    press: Option<(Hit, Pos2)>,
    pending: Option<Pending>,
    selected: Option<u32>,
    hover: Option<Hit>,

    key_leds: [f32; 13],
    /// Per-MIDI-note LEDs for the multi-octave bottom keyboard (key_leds is
    /// per pitch class and would light every octave).
    note_leds: [f32; 128],
    meter: f32,
    /// Peak-hold for the master meter (slow release).
    meter_peak: f32,

    show_help: bool,
    show_stats: bool,
    session_name: String,
    session_msg: String,
    confirm_delete_session: Option<String>,
    /// Short on-stage patch notice (refused cable, …) + expiry in `ui_time`.
    patch_notice: Option<(String, f32)>,
    /// Last frame's rig-chip rects in the overview strip (points), per rig.
    rig_chip_rects: Vec<Rect>,
    /// Last frame's "+ Rig" button rect in the overview strip (points).
    add_rig_rect: Rect,
    /// Top-bar external-agent chip (tests).
    agent_chip_rect: Rect,
    /// Agent undo/redo (one step per agent job) and the open job.
    undo_past: Vec<UndoSnap>,
    undo_future: Vec<UndoSnap>,
    undo_job: Option<(AgentOrigin, f32)>,
    /// UI time until which the last take is playing (Play/Stop chip state).
    take_play_until: f32,
    /// Top-bar groups (tests): the View segment and the Bottom panel toggles.
    view_group_rect: Rect,
    dock_group_rect: Rect,
    // Plugin browser (task-18): add hosted LV2/CLAP plugins to a rig.
    plugins_loaded: bool,
    plugins_clap: Vec<ClapPluginInfo>,
    plugins_lv2: Vec<Lv2PluginInfo>,
    /// Plugin discovery running on a background thread (CLAP scan + LV2
    /// lilv load take seconds); adopted by `poll_plugin_scan`.
    plugin_scan: Option<std::sync::mpsc::Receiver<(Vec<ClapPluginInfo>, Vec<Lv2PluginInfo>)>>,
    plugin_filter: String,
    plugin_category: PluginCategory,
    plugin_msg: String,
    /// Instrument-browser install (issue #10): the running/last worker.
    install: Option<InstallState>,
    /// Test-only override of the bundle-probe roots (None = real home + system
    /// dirs), so catalog "installed" assertions never depend on this machine.
    catalog_roots: Option<Vec<std::path::PathBuf>>,
    fps: f32,
    stats_timer: f32,
    stats_line: String,

    // Per-frame layout / transform.
    rack_w: f32,
    rack_h: f32,
    /// Big patches keep readable units and PAN instead of shrinking
    /// (2026-11-15, user: "racks are squished"): the overflow beyond the
    /// stage view, the current pan offset, and the rack's clip (Some only
    /// while overflowing).
    rack_overflow: Vec2,
    rack_pan: Vec2,
    rack_clip: Option<Rect>,
    unit_h: f32,
    unit_rects: Vec<Rect>, // rack-local, indexed by unit
    /// Rack structure derived from the loaded graph (stable while patching).
    keys_unit: Option<usize>,
    master_unit: Option<usize>,
    /// Middle columns, each a top→bottom list of unit indices (instrument+FX).
    stack_cols: Vec<Vec<usize>>,
    layout_rows: usize,
    /// The loaded graph's nodes (edges are mirrored from the living cables).
    base_graph: Graph,
    xf: Xform,
    /// Y origin of the central rack area (below the top menu panel); the
    /// floating toolbar pill is positioned relative to it so it never overlaps
    /// the rig-inputs menu.
    toolbar_top: f32,
    text_alpha: f32,

    // Cached text.
    wordmark_g: Option<Arc<theme::TrackedLabel>>,
    /// The wordmark at top-bar size (the brand moved into the top bar).
    topbar_word_g: Option<Arc<theme::TrackedLabel>>,
    state_play_g: Option<Arc<theme::TrackedLabel>>,
    /// Captions for the face activity displays (Surge output bars, Delay
    /// echo dots) — unlabeled they read as broken meters (vision review).
    caption_output_g: Option<Arc<theme::TrackedLabel>>,
    caption_echoes_g: Option<Arc<theme::TrackedLabel>>,
    caption_notes_g: Option<Arc<theme::TrackedLabel>>,
    state_pause_g: Option<Arc<theme::TrackedLabel>>,
    hint_front_g: Option<Arc<Galley>>,
    hint_back_g: Option<Arc<Galley>>,
    hint_wire_g: Option<Arc<Galley>>,
    help_title_g: Option<Arc<theme::TrackedLabel>>,
    help_cols: Vec<HelpCol>,
    help_foot_g: Option<Arc<theme::TrackedLabel>>,

    arrivals: [(usize, f32); 32],

    /// Audio-integration seam: interactive key presses are delivered here.
    note_sink: Option<NoteSink>,

    /// Set by raw_input_hook when Tab is pressed (stripped from egui's input
    /// so focus navigation never sees it); consumed in ui().
    flip_requested: bool,

    /// Source tag applied to the next fire_keys (set by each input path).
    current_source: SourceId,
    /// Multi-rig UI: the rig shown in the big view. Visual only —
    /// every rig keeps sounding; switching never touches the audio path.
    active_rig: usize,
    /// Smoothed per-rig activity for the overview strip (max unit LED).
    rig_levels: Vec<f32>,
    /// Active main-stage view: Rack, Matrix, Mixer, Sessions, or Plugins.
    stage_view: StageView,
    /// Ctrl-drag of a unit: last rack-local cursor (drives the live layout
    /// preview) and the slot rect the dragged unit would land in.
    drag_cursor: Option<Vec2>,
    drop_ghost: Option<Rect>,
    /// Committed mid-column centers + stack height (drop-target geometry).
    col_x: Vec<f32>,
    stack_tall_h: f32,
    /// Installed Surge patches, scanned on first use (load_sound/list_sounds).
    surge_patch_cache: Option<Vec<crate::host::SurgePatch>>,
    dexed_patch_cache: Option<Vec<crate::host::DexedPatch>>,
    /// Current patch per synth voice slot (None = Surge's init patch).
    patch_names: [Option<String>; VOICE_POOL],
    /// Sound browser search text (synth editor).
    sound_filter: String,
    /// Last frame's MIDI-matrix block width (table + legend), for centering.
    matrix_block_w: f32,
    matrix_block_h: f32,
    /// Last frame's stage rect (rack/matrix/mixer area) and assistant panel
    /// rect — the hint and the rack must stay inside the stage.
    stage_rect: Rect,
    assistant_rect: Rect,
    /// Last frame's assistant input-row height (+ spacing): the chat history
    /// gets exactly the rest, so the panel never overflows its rect.
    assistant_input_h: f32,
    /// Last frame's assistant input-row rect (tests verify it stays visible).
    assistant_input_rect: Rect,
    /// Agent touch highlights + recent tool/remote action log.
    agent_activity: AgentActivity,
    /// Grace deadlines (`ui_time`) so echoes of direct UI edits never light
    /// up agent highlights.
    ui_mix_grace: [f32; 8],
    ui_transport_grace: f32,
    ui_node_grace: std::collections::HashMap<u32, f32>,
    /// Attribution hint when `exec_tool` triggers a bus command that lands back
    /// through `sync_from_engine`.
    pending_agent_origin: Option<(AgentOrigin, f32)>,
    /// Last frame's rig-bus fader rects in the mixer stage (tests, per rig).
    mixer_fader_rects: Vec<Rect>,
    /// Bottom dock occupant (None / Keyboard / Assistant). Hidden by default.
    dock: DockView,
    /// Set when the assistant opens: the chat field takes focus on the next
    /// frame so typing goes to the agent instead of the laptop piano.
    focus_chat_request: bool,
    /// `--demo-agent` screenshot hook: keeps the demo agent's touch fresh every
    /// frame so the rack highlight stays visible instead of fading after 2.6s.
    demo_agent_hold: bool,
    /// Last frame's dock panel rect (tests + keyboard hit-testing).
    dock_rect: Rect,
    /// Last frame's key area inside the dock keyboard (points).
    dock_kb_rect: Rect,
    /// Per-hardware-device activity LEDs for the matrix row headers.
    hw_leds: Vec<f32>,
    /// Motion (design polish): staged fade-in after a template/session load.
    load_fade: f32,
    /// Per-unit hover easing (0..1, ~120ms) for the hairline rule brighten.
    unit_hover: Vec<f32>,
    /// Toolbar hover easing (~120ms).
    play_hover_t: f32,
    /// Matrix rule edits keyed by (source, dest node) — UI mirror of the
    /// engine's rules, drives the editor + cell indicators.
    rule_edits: std::collections::HashMap<(SourceId, u32), RuleEdit>,
    /// The matrix cell whose rule editor is open: (sink unit, row).
    rule_editor: Option<(usize, MatrixRow)>,
    cc_learn: Option<CcLearn>,
    knob_menu: Option<KnobMenu>,
    knob_menu_rect: Rect,
    rule_cc_num: u8,
    rule_param_id: u32,
    rule_param_filter: String,
    rule_param_cache: Option<(u32, Vec<(u32, String)>)>,
    /// Per-rig mixer mirror (AUM overview strip): vol/pan/mute/solo.
    rig_mix: [crate::engine::RigMix; 8],
    /// Last adopted `GraphPub::mix_rev` (engine mixer publication).
    last_mix_rev: u64,
    /// Last adopted `GraphPub::meter_rev` (engine master-meter publication,
    /// Bravo 1) — updated every block by `sync_from_engine`.
    last_meter_rev: u64,
    /// Mirrored master-bus meter snapshot (read by `measure_mix`, `get_status`).
    master_meter: crate::meter::MeterSnapshot,
    /// Rig awaiting "Delete rig?" confirmation (overview strip × button).
    confirm_delete_rig: Option<usize>,
    /// A destructive assistant action waiting for the chat's Confirm /
    /// Cancel buttons (#32).
    pending_confirm: Option<PendingConfirm>,
    /// The assistant's tool calls in flight (one model step).
    tool_batch: Option<ToolBatch>,
    /// Rename dialog: (rig, the name being edited). Double-click a rig chip.
    rename_rig_dialog: Option<(usize, String)>,
    /// Save-rig-preset modal: (rig index, preset name being edited).
    save_rig_dialog: Option<(usize, String)>,
    /// Load-rig-preset modal: `Some(None)` = as new rig, `Some(Some(rig))` = replace `rig`.
    load_rig_dialog: Option<Option<usize>>,
    /// Rig preset storage dir (isolated per test under `cfg(test)`).
    rig_presets_dir: std::path::PathBuf,
    /// Rig names of the session being applied (load path names its rigs).
    session_rig_names: Vec<String>,
    /// Set by edit_rigs: the next engine load is LoadRigs, not LoadGraph.
    load_as_rigs: Option<(Vec<crate::control::Rig>, usize)>,
    /// Previous frame's keyboard-focus state: losing focus drains held keys
    /// so note-offs can't orphan (vibez lesson, docs/vibez-notes.md).
    had_kb_focus: bool,

    /// Per-rig input selection (keyboard, hardware) for rigs A=node1, B=node4.
    /// The keyboard is ONE source: on-screen clicks and laptop typing.
    rig_sources: [(bool, bool); VOICE_POOL],

    // --- Integrated app (task-4 merge): audio, assistant, input routing ---
    assistant: Assistant,
    key_input: String,
    chat_input: String,
    octave: u8,
    held_keys: Vec<(Key, usize)>,
    note_offs: Vec<(std::time::Instant, usize)>,
    /// Hardware notes for the UI (key LEDs, cable pulses). `true` = the MIDI
    /// thread already sent it to the engine (`hw_direct`), so the frame only
    /// animates it.
    hw_queue: Arc<Mutex<VecDeque<(NoteEvent, bool)>>>,
    hw_cc_queue: Arc<Mutex<VecDeque<(SourceId, u8, u8, u8)>>>,
    /// Hardware MIDI → engine straight from the midir thread: sound never
    /// waits for a UI frame (16 ms focused, 33 ms when throttled unfocused).
    hw_direct: Arc<HwDirect>,
    /// Hardware MIDI inputs, connected and hot-plugged by the MIDI thread.
    hw_midi: Arc<HwMidi>,
    hw_midi_rev: u64,
    hw_status: String,
    /// Connected hardware MIDI device names, indexed by device id.
    hw_devices: Vec<String>,
    /// Last hardware MIDI event for the info line ("eX MIDI 1 · C4 v100").
    last_midi: Option<String>,
    /// The info line is shown in the hint row (no room in the top bar).
    info_in_hint: bool,
    /// Per-rig hardware device selection (None = any), rigs A=node1, B=node4.
    rig_device: [Option<u32>; VOICE_POOL],
    surge_status: String,
    // !Send: CLAP main-thread objects; DemoApp never leaves the UI thread.
    _surge_main: Vec<ClapVoiceMain>,
    /// RT-safe parameter setters, one per hosted voice. Grabbed
    /// before the voices move into the engine; used by the native editor.
    surge_controllers: Vec<VoiceController>,
    clap_plugin_ids: Vec<String>,
    clap_voice_ids: Vec<u64>,
    clap_slot_map: Vec<Option<usize>>,
    sample_rate: f64,
    voice_tx: Option<rtrb::Producer<(u32, ClapVoice, Vec<f32>)>>,
    voice_retire_rx: Option<rtrb::Consumer<ClapVoice>>,
    _audio_stream: Option<cpal::Stream>,
    bus: Option<Arc<Mutex<CommandBus>>>,
    out_level: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Per-edge audio levels from the engine (lock-free, block rate). Drives
    /// audio cable breathing + flowing pulses (docs/ARCHITECTURE.md §2). None = no engine:
    /// MIDI arrivals then fake the downstream envelope instead.
    edge_levels: Option<Arc<EdgeLevels>>,

    // --- Audio In (live mic/guitar/interface, issue #28) ---
    /// Per-node input level (pre-gain peak), engine → UI, lock-free.
    audio_in_levels: Option<Arc<AudioInLevels>>,
    /// UI → engine: delivers a freshly-opened input ring's consumer to a slot.
    audio_in_tx: Option<rtrb::Producer<(u32, rtrb::Consumer<f32>)>>,
    /// Open input captures kept alive on the UI thread, keyed by node id.
    audio_in_streams: std::collections::HashMap<u32, crate::audio_in::CaptureProc>,
    /// Device name each node's stream is actually open for (change detection).
    audio_in_opened: std::collections::HashMap<u32, Option<String>>,
    /// Per-node tuner (owns the tuner ring consumer + pitch cache).
    audio_in_tuners: std::collections::HashMap<u32, (rtrb::Consumer<f32>, crate::audio_in::Tuner, Option<crate::audio_in::Pitch>)>,
    /// Discovered input devices (name, channels, default), rescanned off-frame.
    input_devices: std::sync::Arc<Mutex<Vec<crate::audio_in::InputDevice>>>,
    /// Outputs, our current sink and other apps' streams (scan thread).
    audio_out: std::sync::Arc<Mutex<crate::audio_out::OutputState>>,
    /// Live levels of other apps' streams (mixer "System audio" meters).
    stream_levels: crate::audio_out::StreamLevels,
    /// The output the user chose (None = follow PipeWire's default).
    wanted_output: std::sync::Arc<Mutex<Option<String>>>,
    /// Volume/mute changes for other apps' streams → scan thread.
    stream_cmds: Option<std::sync::mpsc::Sender<crate::audio_out::StreamCmd>>,
    /// Fader values the user is moving (stream index → (value, until)), so
    /// a fader does not jump back before the next scan sees the change.
    stream_grace: Vec<(u64, f32, f32)>,
    /// Time accumulator for the periodic capture re-sync (hot-plug, rule 48).
    audio_in_sync_timer: f32,
    /// Per-node "stream is alive" flag (false after cpal's error callback).
    audio_in_live: std::collections::HashMap<u32, Arc<std::sync::atomic::AtomicBool>>,

    /// Live patch mirrored for the control bridge (task-16): MCP/CLI queries
    /// read this snapshot. Rebuilt from the living cables whenever they change.
    shared_graph: Arc<Mutex<Graph>>,
    /// Multi-rig list/active published by the engine (for the rig overview UI).
    rig_snapshot: Arc<Mutex<crate::control::RigSnapshot>>,
    /// Engine graph publication (MCP→UI live sync). None in silent/demo mode.
    graph_pub: Option<Arc<Mutex<crate::engine::GraphPub>>>,
    /// Last engine graph revision the UI has seen/adopted.
    last_graph_rev: u64,
    /// File-player control (decode thread); engine owns the ring consumer.
    player_ctrl: Option<crate::player::PlayerControl>,
    /// Master recorder status (engine writes `recording`/`dropped`; the writer
    /// thread owns the path/seconds). Read-only here — the REC button sends
    /// RecordStart/Stop through the command bus, the engine stays authoritative.
    recorder: Option<std::sync::Arc<crate::record::RecorderShared>>,
    /// "Sync to transport" toggle mirror (the engine is authoritative via
    /// Command::SetRecordSync; this only drives the chip's display).
    record_sync: bool,
    /// Signature of the living cable set behind `shared_graph` (change detect).
    graph_sig: Vec<(u32, u32, u8, u32, u8, CPortKind)>,

    // --- Native egui parameter editor (task-26): NO X11 child window. Opens
    // on double-click of a Clap unit; a scrollable, searchable, module-grouped
    // slider pane driven by the voice's RT-safe controller. One at a time.
    editor_open: bool,
    /// Which rig's editor is open: 0 = Surge A (unit 1), 1 = Surge B (unit 4).
    editor_rig: usize,
    /// Cached parameter list for the open editor (refreshed on open/Refresh).
    editor_params: Vec<ParamDesc>,
    /// Search/filter text for the parameter list.
    editor_filter: String,
    /// Pending open/close request from a unit double-click (task-26).
    editor_request: Option<usize>,
    /// Open/pending LV2 plugin editor (node id, name, controls, presets).
    lv2_editor_node: Option<u32>,
    lv2_editor_name: String,
    lv2_editor_controls: Vec<crate::lv2::Lv2ControlDesc>,
    lv2_editor_presets: Vec<crate::lv2::Lv2Preset>,
    lv2_preset_name: Option<String>,
    lv2_editor_request: Option<(u32, String, String)>,
    editor_status: String,
    /// Last single click on a unit (unit, time) for double-click detection.
    last_unit_click: Option<(usize, std::time::Instant)>,
}

impl DemoApp {
    fn new_with_sink(note_sink: Option<NoteSink>) -> Self {
        Self::new_app(note_sink, !cfg!(test), Assistant::new())
    }

    /// `hw_scan` false = no hardware MIDI thread (tests and agent-eval never
    /// scan the machine's devices, rule 48).
    fn new_app(note_sink: Option<NoteSink>, hw_scan: bool, assistant: Assistant) -> Self {
        let mut app = Self {
            units: Vec::new(),
            cables: Vec::with_capacity(MAX_CABLES),
            next_id: 1,
            side: Side::Front,
            flipping: false,
            flip_t: 1.0,
            flip_to: Side::Front,
            time: 0.0,
            ui_time: 0.0,
            playing: false,
            bpm: 96.0,
            tap_tempo: crate::transport::TapTempo::new(),
            tap_origin: std::time::Instant::now(),
            tap_flash_t: 0.0,
            count_in_enabled: false,
            count_in_bars: 1,
            counting_in: false,
            count_in_beat: 0,
            last_transport_rev: 0,
            drag: Drag::None,
            press: None,
            pending: None,
            selected: None,
            hover: None,
            key_leds: [0.0; 13],
            note_leds: [0.0; 128],
            meter: 0.0,
            meter_peak: 0.0,
            show_help: false,
            show_stats: false,
            session_name: String::new(),
            session_msg: String::new(),
            confirm_delete_session: None,
            patch_notice: None,
            rig_chip_rects: Vec::new(),
            add_rig_rect: Rect::NOTHING,
            agent_chip_rect: Rect::NOTHING,
            undo_past: Vec::new(),
            undo_future: Vec::new(),
            undo_job: None,
            take_play_until: 0.0,
            view_group_rect: Rect::NOTHING,
            dock_group_rect: Rect::NOTHING,
            plugins_loaded: false,
            plugins_clap: Vec::new(),
            plugins_lv2: Vec::new(),
            plugin_scan: None,
            plugin_filter: String::new(),
            plugin_category: PluginCategory::All,
            plugin_msg: String::new(),
            install: None,
            catalog_roots: None,
            fps: 60.0,
            stats_timer: 0.0,
            stats_line: String::new(),
            rack_w: 640.0,
            rack_overflow: Vec2::ZERO,
            rack_pan: Vec2::ZERO,
            rack_clip: None,
            rack_h: 560.0,
            unit_h: 120.0,
            unit_rects: Vec::new(),
            keys_unit: None,
            master_unit: None,
            stack_cols: Vec::new(),
            layout_rows: 1,
            base_graph: Graph::demo_patch(),
            xf: Xform { center: Pos2::ZERO, lc: Vec2::ZERO, sx: 1.0, sy: 1.0 },
            toolbar_top: 0.0,
            text_alpha: 1.0,
            wordmark_g: None,
            topbar_word_g: None,
            state_play_g: None,
            caption_output_g: None,
            caption_echoes_g: None,
            caption_notes_g: None,
            state_pause_g: None,
            hint_front_g: None,
            hint_back_g: None,
            hint_wire_g: None,
            help_title_g: None,
            help_cols: Vec::new(),
            help_foot_g: None,
            arrivals: [(0, 0.0); 32],
            note_sink,
            flip_requested: false,
            current_source: SourceId::Keys,
            active_rig: 0,
            rig_levels: Vec::new(),
            stage_view: StageView::Rack,
            drag_cursor: None,
            drop_ghost: None,
            col_x: Vec::new(),
            stack_tall_h: 0.0,
            surge_patch_cache: None,
            dexed_patch_cache: None,
            patch_names: Default::default(),
            sound_filter: String::new(),
            matrix_block_w: 0.0,
            matrix_block_h: 0.0,
            stage_rect: Rect::NOTHING,
            assistant_rect: Rect::NOTHING,
            assistant_input_h: 44.0,
            assistant_input_rect: Rect::NOTHING,
            agent_activity: AgentActivity::default(),
            ui_mix_grace: [-1.0; 8],
            ui_transport_grace: -1.0,
            ui_node_grace: std::collections::HashMap::new(),
            pending_agent_origin: None,
            mixer_fader_rects: Vec::new(),
            dock: DockView::None,
            focus_chat_request: false,
            demo_agent_hold: false,
            dock_rect: Rect::NOTHING,
            dock_kb_rect: Rect::NOTHING,
            hw_leds: Vec::new(),
            load_fade: 1.0,
            unit_hover: Vec::new(),
            play_hover_t: 0.0,
            rule_edits: std::collections::HashMap::new(),
            rule_editor: None,
            cc_learn: None,
            knob_menu: None,
            knob_menu_rect: Rect::NOTHING,
            rule_cc_num: 74,
            rule_param_id: 0,
            rule_param_filter: String::new(),
            rule_param_cache: None,
            rig_mix: [crate::engine::RigMix::DEFAULT; 8],
            last_mix_rev: 0,
            last_meter_rev: 0,
            master_meter: crate::meter::MeterSnapshot::default(),
            confirm_delete_rig: None,
            pending_confirm: None,
            tool_batch: None,
            rename_rig_dialog: None,
            save_rig_dialog: None,
            load_rig_dialog: None,
            rig_presets_dir: Self::default_rig_presets_dir(),
            session_rig_names: Vec::new(),
            load_as_rigs: None,
            had_kb_focus: true,
            rig_sources: [(true, true); VOICE_POOL],
            assistant,
            key_input: String::new(),
            chat_input: String::new(),
            octave: 4,
            held_keys: Vec::new(),
            note_offs: Vec::new(),
            hw_queue: Arc::new(Mutex::new(VecDeque::new())),
            hw_cc_queue: Arc::new(Mutex::new(VecDeque::new())),
            hw_direct: Arc::new(HwDirect::default()),
            hw_midi: Arc::new(HwMidi::default()),
            hw_midi_rev: u64::MAX,
            hw_status: String::new(),
            hw_devices: Vec::new(),
            last_midi: None,
            info_in_hint: false,
            rig_device: [None; VOICE_POOL],
            surge_status: "no synth loaded".into(),
            _surge_main: Vec::new(),
            surge_controllers: Vec::new(),
            clap_plugin_ids: Vec::new(),
            clap_voice_ids: Vec::new(),
            clap_slot_map: Vec::new(),
            sample_rate: 48000.0,
            voice_tx: None,
            voice_retire_rx: None,
            _audio_stream: None,
            bus: None,
            out_level: None,
            edge_levels: None,
            audio_in_levels: None,
            audio_in_tx: None,
            audio_in_streams: std::collections::HashMap::new(),
            audio_in_opened: std::collections::HashMap::new(),
            audio_in_tuners: std::collections::HashMap::new(),
            input_devices: std::sync::Arc::new(Mutex::new(Vec::new())),
            audio_out: std::sync::Arc::new(Mutex::new(Default::default())),
            stream_levels: Default::default(),
            wanted_output: std::sync::Arc::new(Mutex::new(crate::assistant::load_config_str("audio_output"))),
            stream_cmds: None,
            stream_grace: Vec::new(),
            audio_in_sync_timer: 0.0,
            audio_in_live: std::collections::HashMap::new(),
            shared_graph: Arc::new(Mutex::new(Graph::demo_patch())),
            rig_snapshot: Arc::new(Mutex::new(crate::control::RigSnapshot::default())),
            graph_pub: None,
            last_graph_rev: 0,
            player_ctrl: None,
            recorder: None,
            record_sync: false,
            graph_sig: Vec::new(),
            editor_open: false,
            editor_rig: 0,
            editor_params: Vec::new(),
            editor_filter: String::new(),
            editor_request: None,
            lv2_editor_node: None,
            lv2_editor_name: String::new(),
            lv2_editor_controls: Vec::new(),
            lv2_editor_presets: Vec::new(),
            lv2_preset_name: None,
            lv2_editor_request: None,
            editor_status: String::new(),
            last_unit_click: None,
        };

        // Hardware MIDI: a thread connects what is plugged in now and keeps
        // rescanning, so devices plugged in later are picked up too.
        if hw_scan {
            spawn_hw_midi(app.hw_midi.clone(), app.hw_queue.clone(), app.hw_cc_queue.clone(), app.hw_direct.clone());
        }

        // Default rack = the engine's demo patch (2 stacks). build_rack mirrors
        // its nodes/edges into units/cables; ids match so per-frame Connect
        // publishes are engine-side duplicates.
        let (ci_enabled, ci_bars) = crate::assistant::load_count_in();
        app.count_in_enabled = ci_enabled;
        app.count_in_bars = ci_bars;
        app.build_rack(&Graph::demo_patch(), &[]);
        app.begin_load_fade(); // startup rack fades in
        app
    }

    /// Rebuild units + cables + rack structure from a control-plane graph, then
    /// apply default knob values. This is the single path used by the demo,
    /// starter templates, and saved sessions.
    fn build_rack(&mut self, graph: &Graph, params: &[crate::session::ParamValue]) {
        self.units.clear();
        self.cables.clear();
        self.base_graph = graph.clone();
        for node in &graph.nodes {
            self.units.push(make_unit(node));
        }
        // Classify LV2 instruments (hot-wire matrix sink columns). Rare
        // structural op: the discovery scan runs only when LV2 nodes exist.
        if graph.nodes.iter().any(|n| matches!(n.kind, NodeKind::Lv2 { .. })) {
            let infos = lv2_discover();
            for u in &mut self.units {
                if let Some(NodeKind::Lv2 { uri }) = graph.nodes.iter().find(|n| n.id == u.node_id).map(|n| &n.kind) {
                    u.lv2_inst = infos.iter().any(|p| p.uri == uri.as_str() && p.is_instrument);
                    if u.lv2_inst {
                        u.sub = "lv2 synth".into();
                        u.sub_g = None;
                    }
                }
            }
        }
        // Size the rect slots before adding cables (add_cable_id reads them to
        // seed the spring rest shape; layout() fills real rects on frame 1).
        self.unit_rects = vec![Rect::NOTHING; self.units.len()];
        // node id → unit index (units are built in graph.nodes order).
        let mut max_id = 0u32;
        for e in &graph.edges {
            let s = self.units.iter().position(|u| u.node_id == e.from.node);
            let d = self.units.iter().position(|u| u.node_id == e.to.node);
            if let (Some(s), Some(d)) = (s, d) {
                let kind = match e.kind {
                    CPortKind::Midi => PortKind::Midi,
                    CPortKind::Audio => PortKind::Audio,
                };
                self.add_cable_id(e.id, s, e.from.port as usize, d, e.to.port as usize, kind);
                max_id = max_id.max(e.id);
            }
        }
        self.next_id = max_id + 1;
        // Apply default params to the matching knobs.
        for p in params {
            if let Some(u) = self.units.iter_mut().find(|u| u.node_id == p.node) {
                if let Some(k) = u.knobs.iter_mut().find(|k| k.param == Some(p.param)) {
                    k.value = p.value as f32;
                    k.default = p.value as f32;
                }
            }
        }
        self.active_rig = 0;
        self.compute_groups();
        // Reset interaction state that referenced the old rack.
        self.selected = None;
        self.pending = None;
        self.drag = Drag::None;
        self.press = None;
        self.knob_menu = None;
        self.knob_menu_rect = Rect::NOTHING;
        self.hover = None;
        self.graph_sig.clear(); // force shared-graph re-mirror
        // No load_fade reset here: build_rack also serves sync adoptions
        // (agent/MCP add_node/connect), which must appear without a flash.
        // Explicit loads call begin_load_fade().
        self.unit_hover = vec![0.0; self.units.len()];
        // The engine rebuilds its matrix on loads — rule edits start fresh.
        self.rule_edits.clear();
        self.rule_editor = None;
        self.cc_learn = None;
        self.rule_param_cache = None;
        self.rig_mix = [crate::engine::RigMix::DEFAULT; 8];
        // Open/refresh input captures for every AudioIn node (structural only).
        self.sync_audio_in_captures();
    }

    /// Staged ~0.45s fade-in for an explicitly loaded rack (template,
    /// session, demo, multi-rig demo, plugin-browser rig). Never for sync
    /// adoptions — incremental remote edits must not flash the screen.
    fn begin_load_fade(&mut self) {
        self.load_fade = 0.0;
    }

    /// Derive rack structure FOR THE ACTIVE RIG: Keys (left), Master (right),
    /// and one middle column per instrument (Clap) followed by its downstream
    /// DSP chain. Other rigs' units keep Rect::NOTHING (hidden, still sounding).
    fn compute_groups(&mut self) {
        let rig = self.active_rig as u8;
        let multi = self.rig_count() > 1;
        let in_rig = |u: &Unit| !multi || u.rig == rig;
        self.keys_unit = self.units.iter().position(|u| in_rig(u) && matches!(u.node_kind, UnitKind::Keys));
        self.master_unit = self.units.iter().position(|u| in_rig(u) && matches!(u.node_kind, UnitKind::Master));
        let cols = self.stack_columns(rig, multi);
        // Every rig is laid out with the TALLEST rig's row count (user
        // 2026-11-15: "rig B is smaller than rig A") — the rack keeps one
        // size across rig switches; shorter stacks stretch to fill (layout).
        let rows_all = if multi {
            (0..self.rig_count()).map(|r| self.stack_columns(r as u8, true).iter().map(|c| c.len()).max().unwrap_or(1)).max().unwrap_or(1)
        } else {
            cols.iter().map(|c| c.len()).max().unwrap_or(1)
        };
        self.layout_rows = rows_all.max(1);
        self.stack_cols = cols;
        // Hidden units (other rigs) keep no rect: layout/draw/hit skip them.
        for (i, u) in self.units.iter().enumerate() {
            if !in_rig(u) {
                if let Some(r) = self.unit_rects.get_mut(i) { *r = Rect::NOTHING; }
            }
        }
    }

    /// One rig's middle stacks: each instrument followed down its audio
    /// chain (instrument → DSP → DSP …), then any unclaimed DSP alone.
    /// `multi = false`: the whole patch is one rig.
    fn stack_columns(&self, rig: u8, multi: bool) -> Vec<Vec<usize>> {
        let in_rig = |u: &Unit| !multi || u.rig == rig;
        let mut claimed = vec![false; self.units.len()];
        for (i, u) in self.units.iter().enumerate() {
            // Never group hidden units, nor this rig's Keys/Master anchors.
            if !in_rig(u) || matches!(u.node_kind, UnitKind::Keys | UnitKind::Master) { claimed[i] = true; }
        }
        let mut cols: Vec<Vec<usize>> = Vec::new();
        for i in 0..self.units.len() {
            // Skip claimed units: another rig's synth must not open an
            // (invisible) column in this rig's view.
            if claimed[i] || !matches!(self.units[i].node_kind, UnitKind::Clap | UnitKind::AudioIn) { continue; }
            let mut col = vec![i];
            claimed[i] = true;
            let mut cur = i;
            loop {
                let next = self.cables.iter().find(|c| {
                    c.dying < 0.0 && c.kind == PortKind::Audio && c.src == cur
                        && !claimed[c.dst]
                        && matches!(self.units[c.dst].node_kind, UnitKind::Dsp(_))
                }).map(|c| c.dst);
                match next {
                    Some(d) => { claimed[d] = true; col.push(d); cur = d; }
                    None => break,
                }
            }
            cols.push(col);
        }
        for i in 0..self.units.len() {
            if !claimed[i] && matches!(self.units[i].node_kind, UnitKind::Dsp(_)) {
                claimed[i] = true;
                cols.push(vec![i]);
            }
        }
        cols
    }

    /// Number of rigs in the current rack (merge_rigs tags nodes). Single-graph
    /// mode = 1. The overview strip and rig switching only appear above 1.
    fn rig_count(&self) -> usize {
        self.units.iter().map(|u| u.rig as usize).max().map_or(1, |m| m + 1)
    }

    /// Is this unit part of the big view (the active rig)?
    fn visible(&self, unit: usize) -> bool {
        self.rig_count() <= 1 || self.units.get(unit).map_or(false, |u| u.rig == self.active_rig as u8)
    }

    /// Switch the big view to another rig. VISUAL ONLY — all rigs keep sounding
    /// (the engine matrix routes by source, never by UI selection; vibez lesson).
    fn set_active_rig(&mut self, rig: usize) {
        let n = self.rig_count();
        let rig = rig.min(n.saturating_sub(1));
        if rig == self.active_rig { return; }
        self.active_rig = rig;
        self.compute_groups();
        // Interaction state referenced the old view's geometry.
        self.selected = None;
        self.pending = None;
        self.drag = Drag::None;
        self.press = None;
        self.knob_menu = None;
        self.knob_menu_rect = Rect::NOTHING;
        self.hover = None;
        // Keep MCP/bridge get_rigs consistent with the UI selection (the audio
        // render path never locks this snapshot).
        if let Ok(mut s) = self.rig_snapshot.lock() {
            if !s.rigs.is_empty() { s.active = rig; }
        }
    }

    /// Display name for a rig: engine-published snapshot first, else Rig A/B/C.
    fn rig_name(&self, rig: usize) -> String {
        if let Ok(s) = self.rig_snapshot.lock() {
            if let Some(m) = s.rigs.get(rig) {
                if !m.name.is_empty() { return m.name.clone(); }
            }
        }
        let letter = (b'A' + (rig.min(25)) as u8) as char;
        format!("Rig {letter}")
    }

    /// Overview device line: the rig's bound hardware MIDI device, or ALL.
    fn rig_device_label(&self, rig: usize) -> String {
        let dev = self.rig_snapshot.lock().ok().and_then(|s| s.rigs.get(rig).and_then(|m| m.midi_device));
        match dev {
            None => "ALL SOURCES".into(),
            Some(d) => match self.hw_devices.get(d as usize) {
                Some(name) => format!("HW: {}", name.to_uppercase()),
                None => format!("HW: DEV {d}"),
            },
        }
    }

    /// The movable (middle-stack) unit under a rack-local point, if any.
    /// KEYS and MASTER are fixed anchors and are never returned (task-21).
    fn stack_unit_at(&self, l: Vec2) -> Option<usize> {
        let lp = l.to_pos2();
        for ui in 0..self.units.len() {
            if self.keys_unit == Some(ui) || self.master_unit == Some(ui) { continue; }
            if self.unit_rects[ui].contains(lp) { return Some(ui); }
        }
        None
    }

    /// Where a Ctrl-dragged `unit` would land at rack-local `drop`:
    /// (target column, target row). Columns are judged from the COMMITTED
    /// layout's centers (`col_x`), rows from the drop height.
    fn drop_target(&self, unit: usize, drop: Vec2) -> Option<(usize, usize)> {
        if self.keys_unit == Some(unit) || self.master_unit == Some(unit) { return None; }
        self.stack_cols.iter().flatten().position(|&u| u == unit)?;
        let tj = self.col_x.iter().enumerate()
            .min_by(|a, b| (drop.x - a.1).abs().total_cmp(&(drop.x - b.1).abs()))
            .map(|(j, _)| j)?;
        let others = self.stack_cols.get(tj)?.iter().filter(|&&u| u != unit).count();
        let pad = 20.0;
        let slot = self.stack_tall_h / (others + 1) as f32;
        let tr = (((drop.y - pad) / slot.max(1.0)).floor() as i32).clamp(0, others as i32) as usize;
        Some((tj, tr))
    }

    /// The stack columns after moving `unit` to `(tj, tr)` — empty columns
    /// are KEPT (stable geometry while previewing); `drop_unit` prunes them.
    fn cols_after(&self, unit: usize, (tj, tr): (usize, usize)) -> Vec<Vec<usize>> {
        let mut cols = self.stack_cols.clone();
        for c in cols.iter_mut() { c.retain(|&u| u != unit); }
        if let Some(c) = cols.get_mut(tj) { let tr = tr.min(c.len()); c.insert(tr, unit); }
        cols
    }

    /// Ctrl+drag release (task-21): commit exactly the previewed order;
    /// layout() then reflows. Unit indices (cable endpoints) are untouched.
    fn drop_unit(&mut self, unit: usize, drop: Vec2) {
        let Some(t) = self.drop_target(unit, drop) else { return };
        let mut cols = self.cols_after(unit, t);
        cols.retain(|c| !c.is_empty());
        self.stack_cols = cols;
        self.layout_rows = self.layout_rows.max(self.stack_cols.iter().map(|c| c.len()).max().unwrap_or(1)).max(1);
    }

    /// Galleys are laid out on the first frame: egui loads fonts only once
    /// `Context::run` has started, so the app creator cannot do this.
    fn ensure_galleys(&mut self, ctx: &egui::Context) {
        let layout = |text: &str, font: FontId, color: Color32, wrap: f32| -> Arc<Galley> {
            ctx.fonts_mut(|f| f.layout(text.to_owned(), font, color, wrap))
        };
        // Tracked micro-caps (§4.5): per-char galleys, cached like the rest.
        let tracked = |text: &str, size: f32, tracking_em: f32| -> Arc<theme::TrackedLabel> {
            let font = FontId::proportional(size);
            Arc::new(ctx.fonts_mut(|f| theme::layout_tracked(f, text, font, tracking_em)))
        };
        // Unit galleys: (re)fill any that are missing — templates rebuild units.
        // Name + subtitle are shrink-to-fit (C4): refitted whenever the unit
        // width changes (rack compression), measured under the fonts lock
        // only — painting happens later in draw_front.
        for (ui, u) in self.units.iter_mut().enumerate() {
            let w = self.unit_rects.get(ui).map_or(f32::NAN, |r| r.width());
            let fit_w = if w.is_finite() && w > 0.0 { w } else { f32::INFINITY };
            let refit = u.name_g.is_none() || u.sub_g.is_none() || (fit_w - u.label_fit_w).abs() > 0.5;
            if refit {
                // Name: Medium 16px, sentence case (§4.3), beside the LED.
                // Subtitle: tracked micro-caps 10px (+0.14em), below it.
                let name = sentence_case(&u.name);
                let (name, size) = fit_label(&name, 16.0, 8.0, fit_w - UNIT_NAME_INSET, |t, sz| {
                    layout(t, theme::medium_font(sz), TEXT, f32::INFINITY).size().x
                });
                u.name_g = Some(layout(&name, theme::medium_font(size), TEXT, f32::INFINITY));
                let sub = u.sub.to_uppercase();
                let (sub, size) = fit_label(&sub, 10.0, 8.0, fit_w - UNIT_SUB_INSET, |t, sz| tracked(t, sz, 0.14).size.x);
                u.sub_g = Some(tracked(&sub, size, 0.14));
                u.label_fit_w = fit_w;
            }
            if u.back_g.is_none() {
                // Same display name as the front (rule 28), still small + tracked.
                u.back_g = Some(tracked(&sentence_case(&u.name), 11.0, 0.06));
            }
            for k in &mut u.knobs {
                if k.label_g.is_none() {
                    k.label_g = Some(tracked(&k.label, 10.0, 0.14));
                }
            }
            for j in &mut u.jacks {
                if j.label_g.is_none() {
                    j.label_g = Some(tracked(j.label, 9.0, 0.14));
                }
            }
        }
        if self.wordmark_g.is_some() { return; }
        // Masthead (§4.13): Medium 22px wordmark, display tracking −0.03em;
        // state line in tracked micro-caps.
        self.wordmark_g = Some(Arc::new(ctx.fonts_mut(|f| {
            theme::layout_tracked(f, "kabelsalat", theme::medium_font(30.0), -0.03)
        })));
        self.topbar_word_g = Some(Arc::new(ctx.fonts_mut(|f| {
            theme::layout_tracked(f, "kabelsalat", theme::medium_font(20.0), -0.03)
        })));
        self.state_play_g = Some(tracked("PLAYING", 10.0, 0.14));
        self.caption_output_g = Some(tracked("OUTPUT", 10.0, 0.14));
        self.caption_echoes_g = Some(tracked("ECHOES", 10.0, 0.14));
        self.caption_notes_g = Some(tracked("NOTES", 10.0, 0.14));
        self.state_pause_g = Some(tracked("STOPPED", 10.0, 0.14));
        // Hints (§4.15): plain caption lines on paper, sentence case.
        self.hint_front_g = Some(layout(
            "perform: drag knobs · play on the keyboard (V) · Space starts/stops · Tab flips the rack to patch",
            FontId::proportional(12.5), theme::INK_2, f32::INFINITY));
        self.hint_back_g = Some(layout(
            "patch: drag jack to jack · drag a plug to another jack to move it · tug off to unplug · Tab flips back",
            FontId::proportional(12.5), theme::INK_2, f32::INFINITY));
        self.hint_wire_g = Some(layout(
            "now click a highlighted jack to connect · esc or right-click cancels",
            FontId::proportional(12.5), theme::ACCENT_DEEP, f32::INFINITY));
        // Help card (§4.16): Medium 22px title, two columns of key chips +
        // captions under tracked section labels, tracked micro-caps footer.
        self.help_title_g = Some(Arc::new(ctx.fonts_mut(|f| {
            theme::layout_tracked(f, "kabelsalat — the cables are the show", theme::medium_font(22.0), -0.03)
        })));
        let col = |title: &str, rows: &[(&[&str], &str)]| -> HelpCol {
            let rows: Vec<HelpRow> = rows.iter().map(|(keys, desc)| HelpRow {
                keys: keys.iter().map(|k| layout(k, theme::medium_font(12.0), theme::CREAM, f32::INFINITY)).collect(),
                desc: layout(desc, FontId::proportional(13.5), theme::CREAM_MID, f32::INFINITY),
            }).collect();
            let keys_w = rows.iter().map(|r| r.keys_width()).fold(0.0, f32::max);
            let desc_w = rows.iter().map(|r| r.desc.size().x).fold(0.0, f32::max);
            HelpCol { header: tracked(&title.to_uppercase(), 10.0, 0.14), rows, keys_w, width: keys_w + HELP_KEY_GAP + desc_w }
        };
        self.help_cols = vec![
            col("Play", &[
                (&["Tab"], "flip the rack · front plays, back patches"),
                (&["Space"], "master play / stop (every metronome)"),
                (&["Shift", "T"], "tap tempo (or TAP on a metronome)"),
                (&["drag"], "the BPM on a metronome to set the tempo"),
                (&["SPLIT"], "on the keys: LOW / HIGH MIDI outs"),
                (&["drag"], "a knob to shape the sound"),
                (&["R"], "reset the knobs"),
                (&["double-click"], "a synth to open its editor"),
                (&["1–9", "←", "→"], "switch the rig in view"),
                (&["F4"], "multi-rig demo"),
                (&["F5–F8"], "starter templates"),
            ]),
            col("Patch & views", &[
                (&["drag"], "jack to jack to patch a cable"),
                (&["drag plug"], "onto a jack to move it · tug off to unplug"),
                (&["Ctrl", "drag"], "a unit to rearrange the rack"),
                (&["click"], "a cable to select its path"),
                (&["Del"], "remove the selected cable"),
                (&["F9"], "plugin browser"),
                (&["M", "F10"], "mixer view (sources → rig bus → master)"),
                (&["N"], "MIDI matrix view (also F11)"),
                (&["C"], "chat with the assistant (also F12)"),
                (&["B"], "plugin browser (also F9)"),
                (&["F11"], "MIDI matrix view"),
                (&["F12"], "assistant"),
                (&["I"], "MIDI + performance info"),
            ]),
        ];
        self.help_foot_g = Some(tracked("ESC OR ? TO CLOSE", 10.0, 0.14));
    }

    // -- layout -----------------------------------------------------------

    /// Stage height the rack needs for unhurried faces: masthead + hint
    /// overhead (as in `layout`) plus `rows` units at COMFORT_UNIT_H.
    fn rack_need_h(&self) -> f32 {
        const COMFORT_UNIT_H: f32 = 150.0;
        let rows = self.layout_rows.max(1) as f32;
        150.0 + rows * COMFORT_UNIT_H + (rows - 1.0) * 12.0
    }

    fn layout(&mut self, rect: Rect) {
        self.toolbar_top = rect.top();
        // Adaptive rack: KEYS (left, full height) | one column per instrument
        // stack (instrument + its FX chain) | MASTER (right, full height).
        let rows = self.layout_rows.max(1);
        let n_mid = self.stack_cols.len();
        // Readable floors: below them the rack grows past the stage and pans
        // (wheel / trackpad) instead of squishing faces.
        const MIN_UNIT_H: f32 = 140.0;
        const MIN_MID_W: f32 = 200.0;
        const MIN_SIDE_W: f32 = 220.0;
        // Caps sized for the 1.6x design scale (1600x1000 points): the rack
        // uses the stage instead of floating small in it.
        let fit_w = (rect.width() - 60.0).clamp(560.0, 1480.0);
        // No masthead any more (user 2026-11-15: Play button removed, brand in
        // the top bar) — the stage overhead is just margins + the hint line.
        self.unit_h = ((rect.height() - 150.0) / rows as f32).clamp(MIN_UNIT_H, 250.0);
        let gap = 12.0;
        let pad = 20.0;
        let rail = 14.0;
        self.rack_h = pad * 2.0 + rows as f32 * self.unit_h + (rows as f32 - 1.0) * gap;
        let n_cols = n_mid + 2; // keys + stacks + master
        let gaps = (n_cols as f32 - 1.0) * gap;
        let usable_fit = fit_w - 2.0 * (pad + rail) - gaps;
        let side_frac = if n_mid == 0 { 0.5 } else if n_mid <= 2 { 0.26 } else { 0.20 };
        let mid_frac = if n_mid == 0 { 0.0 } else { (1.0 - 2.0 * side_frac) / n_mid as f32 };
        let (mut side_w, mut mid_w) = (usable_fit * side_frac, usable_fit * mid_frac);
        if n_mid > 0 && mid_w < MIN_MID_W {
            mid_w = MIN_MID_W;
            side_w = side_w.max(MIN_SIDE_W);
        }
        // A lone stack must not stretch across half the stage (a one-synth
        // rig read as a mostly empty face): cap the column, the rack centers.
        const MAX_MID_W: f32 = 420.0;
        if n_mid > 0 && mid_w > MAX_MID_W {
            mid_w = MAX_MID_W;
            side_w = side_w.min(MAX_MID_W * 1.1);
        }
        self.rack_w = 2.0 * (pad + rail) + 2.0 * side_w + n_mid as f32 * mid_w + gaps;
        let mut widths: Vec<f32> = Vec::with_capacity(n_cols);
        widths.push(side_w); // keys
        for _ in 0..n_mid { widths.push(mid_w); }
        widths.push(side_w); // master
        let mut xs: Vec<f32> = Vec::with_capacity(n_cols);
        let mut x = pad + rail;
        for w in &widths { xs.push(x); x += w + gap; }
        let tall_h = rows as f32 * self.unit_h + (rows as f32 - 1.0) * gap;
        let mk = |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
        if self.unit_rects.len() != self.units.len() {
            self.unit_rects = vec![Rect::NOTHING; self.units.len()];
        }
        if let Some(k) = self.keys_unit {
            self.unit_rects[k] = mk(xs[0], pad, widths[0], tall_h);
        }
        if let Some(m) = self.master_unit {
            self.unit_rects[m] = mk(xs[n_cols - 1], pad, widths[n_cols - 1], tall_h);
        }
        // Column centers of the COMMITTED layout: the drop target is judged
        // from these, so the live preview below cannot feed back into it.
        self.col_x = (0..n_mid).map(|j| xs[j + 1] + widths[j + 1] / 2.0).collect();
        self.stack_tall_h = tall_h;
        // While Ctrl-dragging a unit, lay the others out AS IF it were
        // released here (user: show the layout before release); the ghost
        // rect marks where the dragged unit lands.
        let preview = match (&self.drag, self.drag_cursor) {
            (&Drag::Unit { unit, .. }, Some(at)) => self.drop_target(unit, at).map(|t| (unit, self.cols_after(unit, t))),
            _ => None,
        };
        let cols = preview.as_ref().map(|(_, c)| c.clone()).unwrap_or_else(|| self.stack_cols.clone());
        self.drop_ghost = None;
        for j in 0..n_mid.min(cols.len()) {
            let cx = xs[j + 1];
            let cw = widths[j + 1];
            // A shorter stack stretches its units to the full column height
            // (one rack size for every rig, no empty rows).
            let len = cols[j].len().max(1);
            let uh = (tall_h - (len as f32 - 1.0) * gap) / len as f32;
            for r in 0..cols[j].len() {
                let u = cols[j][r];
                let y = pad + r as f32 * (uh + gap);
                let rr = mk(cx, y, cw, uh);
                if preview.as_ref().map_or(false, |(d, _)| *d == u) {
                    self.drop_ghost = Some(rr); // the dragged box itself follows the cursor
                } else {
                    self.unit_rects[u] = rr;
                }
            }
        }
        // Stage view for the rack: below the masthead, above the hint line.
        let view = Rect::from_min_max(
            Pos2::new(rect.left() + 16.0, rect.top() + 40.0),
            Pos2::new(rect.right() - 16.0, rect.bottom() - 36.0),
        );
        let ov = Vec2::new((self.rack_w - view.width()).max(0.0), (self.rack_h - view.height()).max(0.0));
        if ov != self.rack_overflow {
            eprintln!("rack layout: stage {:.0}x{:.0} rack {:.0}x{:.0} unit_h {:.0} rows {} → overflow {:.0},{:.0}",
                rect.width(), rect.height(), self.rack_w, self.rack_h, self.unit_h, rows, ov.x, ov.y);
        }
        self.rack_overflow = ov;
        self.rack_pan = Vec2::new(
            self.rack_pan.x.clamp(-ov.x / 2.0, ov.x / 2.0),
            self.rack_pan.y.clamp(-ov.y / 2.0, ov.y / 2.0),
        );
        self.rack_clip = (ov.x > 0.0 || ov.y > 0.0).then_some(view.expand(8.0));
        let origin = Pos2::new(
            rect.center().x - self.rack_w / 2.0 + self.rack_pan.x,
            if ov.y > 0.0 { view.center().y - self.rack_h / 2.0 + self.rack_pan.y }
            else { rect.center().y - self.rack_h / 2.0 - 8.0 },
        );
        // Flip transform.
        let mut sy = 1.0f32;
        if self.flipping {
            let p = self.flip_t.clamp(0.0, 1.0);
            sy = if p < 0.5 { 1.0 - ease_in_cubic(p * 2.0) }
                 else { ease_out_back((p - 0.5) * 2.0) };
            sy = sy.clamp(0.015, 1.06);
        }
        let sx = 1.0 + 0.06 * (1.0 - sy.min(1.0));
        self.xf = Xform {
            center: origin + Vec2::new(self.rack_w / 2.0, self.rack_h / 2.0),
            lc: Vec2::new(self.rack_w / 2.0, self.rack_h / 2.0),
            sx, sy,
        };
        self.text_alpha = ((sy - 0.30) / 0.45).clamp(0.0, 1.0);
    }

    fn jack_pos(&self, unit: usize, jack: usize) -> Vec2 {
        self.units[unit].jack_local(self.unit_rects[unit], jack)
    }

    fn endpoint(&self, c: &Cable, end: Dir) -> Vec2 {
        let (u, p) = if end == Dir::Out { (c.src, c.src_port) } else { (c.dst, c.dst_port) };
        let ji = self.units[u].jack_index(end, p).unwrap_or(0);
        self.jack_pos(u, ji)
    }

    // -- graph edits --------------------------------------------------------

    /// Add a user cable with a fresh id. Returns the new id, or `None` when
    /// the cable was refused (see `cable_refusal`) — callers surface that.
    fn add_cable(&mut self, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind, pattern: u8) -> Option<u32> {
        let _ = pattern; // patterns retired with the demo band
        if self.cable_refusal(src, src_port, dst, dst_port, None).is_some() { return None; }
        let id = self.next_id;
        self.next_id += 1;
        self.add_cable_id(id, src, src_port, dst, dst_port, kind);
        Some(id)
    }

    /// Why a cable src:sp → dst:dp would be refused, ignoring cable `except`
    /// (the one being re-plugged). Mirrors `add_cable_id`'s guards.
    fn cable_refusal(&self, src: usize, src_port: usize, dst: usize, dst_port: usize, except: Option<u32>) -> Option<&'static str> {
        if src == dst { return Some("can't patch a unit into itself"); }
        let others = self.cables.iter().filter(|c| Some(c.id) != except).count();
        if others >= MAX_CABLES { return Some("cable limit reached — unplug one first"); }
        let dup = self.cables.iter().any(|c| {
            Some(c.id) != except && c.dying < 0.0
                && c.src == src && c.src_port == src_port && c.dst == dst && c.dst_port == dst_port
        });
        if dup { return Some("those jacks are already patched"); }
        None
    }

    fn notify_patch(&mut self, msg: &str) {
        self.patch_notice = Some((msg.to_owned(), self.ui_time + 2.5));
    }

    /// Index of the living cable with this id (AGENTS.md rule 10).
    fn live_cable(&self, id: u32) -> Option<usize> {
        self.cables.iter().position(|c| c.id == id && c.dying < 0.0)
    }

    /// Move the held `end` of cable `id` onto jack (`unit`, `port`). The jack
    /// must match the held end's direction and the cable's kind and give a
    /// different endpoint pair. The engine sees `Disconnect(old)` now and
    /// `Connect(new)` on the next patch publish — FIFO bus, order holds.
    fn replug_cable(&mut self, id: u32, end: Dir, unit: usize, dir: Dir, port: usize) -> Replug {
        let Some(ci) = self.live_cable(id) else { return Replug::Incompatible };
        let (src, sp, dst, dp, kind) = {
            let c = &self.cables[ci];
            (c.src, c.src_port, c.dst, c.dst_port, c.kind)
        };
        if dir != end || self.units[unit].kind(dir, port) != kind { return Replug::Incompatible; }
        let (ns, nsp, nd, ndp) = if end == Dir::Out { (unit, port, dst, dp) } else { (src, sp, unit, port) };
        if (ns, nsp, nd, ndp) == (src, sp, dst, dp) { return Replug::Incompatible; }
        if let Some(why) = self.cable_refusal(ns, nsp, nd, ndp, Some(id)) {
            self.notify_patch(why);
            return Replug::Refused;
        }
        let was_selected = self.selected == Some(id);
        self.unplug_cable(ci, end == Dir::In);
        // A moved plug doesn't fall: drop the old cable outright.
        self.cables.remove(ci);
        let new_id = self.add_cable(ns, nsp, nd, ndp, kind, 0);
        if was_selected { self.selected = new_id; }
        Replug::Moved
    }

    /// Add a cable with an explicit id (demo patch mirrors engine edge ids).
    /// Inputs fan in: several cables may share one input jack (master bus).
    fn add_cable_id(&mut self, id: u32, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind) {
        if self.cables.len() >= MAX_CABLES || src == dst { return; }
        if self.cables.iter().any(|c| c.dying < 0.0 && c.src == src && c.src_port == src_port && c.dst == dst && c.dst_port == dst_port) {
            return;
        }
        // Cable salad (user request 2026-11-15): warm jackets for audio, cool
        // for MIDI — family hue semantics survive, shape stays the primary
        // separator. The key uses RIG-RELATIVE node ids so a patch keeps its
        // colors across reloads even when other rigs grow (merge_rigs offsets
        // absolute ids; relative ids within a rig are stable).
        let rig = self.units[src].rig;
        let base = self.units.iter().filter(|u| u.rig == rig).map(|u| u.node_id).min().unwrap_or(0);
        let ls = self.units[src].node_id.saturating_sub(base) as usize;
        let ld = self.units[dst].node_id.saturating_sub(base) as usize;
        let key = ls * 7 + ld * 13 + src_port * 3 + dst_port;
        let color = match kind {
            PortKind::Audio => theme::CABLE_AUDIO_PALETTE[key % theme::CABLE_AUDIO_PALETTE.len()],
            PortKind::Midi => theme::CABLE_MIDI_PALETTE[key % theme::CABLE_MIDI_PALETTE.len()],
        };
        let mut c = Cable::new(id, src, src_port, dst, dst_port, kind, color, 0, id * 7919 + 13);
        c.next_event = self.time + 0.2 + rng01(&mut c.rng) as f64 * 0.4;
        // Bulge away from the rack centerline; vary per cable for organic look.
        let a = self.endpoint(&c, Dir::Out);
        let b = self.endpoint(&c, Dir::In);
        let mid_x = (a.x + b.x) * 0.5;
        c.bulge_side = if (mid_x - self.rack_w / 2.0).abs() < 26.0 {
            if id % 2 == 0 { 1.0 } else { -1.0 }
        } else if mid_x < self.rack_w / 2.0 { -1.0 } else { 1.0 };
        c.bulge_var = 0.85 + 0.3 * rng01(&mut c.rng);
        let dist = (b - a).length();
        let sag = (18.0 + dist * 0.20).min(170.0);
        c.c1 = Vec2::new(0.0, sag - 90.0);
        c.c2 = Vec2::new(0.0, sag - 90.0);
        c.v1 = Vec2::new(0.0, 260.0);
        c.v2 = Vec2::new(0.0, 260.0);
        c.prev_a = a;
        c.prev_b = b;
        self.units[src].led = 1.0;
        self.units[dst].led = 1.0;
        self.cables.push(c);
    }

    fn unplug_cable(&mut self, idx: usize, detach_dst: bool) {
        let c = &mut self.cables[idx];
        if c.dying < 0.0 {
            // The patch is real: tell the engine to drop this route.
            if let Some(bus) = &self.bus {
                let _ = bus.lock().unwrap().send(Command::Disconnect(c.id));
            }
            c.dying = 0.0;
            c.detach_dst = detach_dst;
            c.over_a = None;
            c.over_b = None;
            c.drift = if rng01(&mut c.rng) > 0.5 { 34.0 } else { -34.0 };
            c.v1 += Vec2::new(0.0, -120.0);
            c.v2 += Vec2::new(0.0, -120.0);
        }
    }

    /// End a plug drag on cable `id` (held `end`) released at rack-local `l`
    /// over `hit`. A real drag (≥ 6px) ending on a compatible jack moves the
    /// plug; otherwise far from its jack (or a plain click) unplugs, and near
    /// it snaps back.
    fn release_plug(&mut self, id: u32, end: Dir, l: Vec2, moved: f32, hit: Hit) {
        let Some(ci) = self.live_cable(id) else { return };
        let jack = self.endpoint(&self.cables[ci], end);
        let far = (l - jack).length() > 42.0;
        if moved >= 6.0 && far && self.side == Side::Back {
            if let Hit::Jack { unit, jack } = hit {
                let (dir, port) = { let j = &self.units[unit].jacks[jack]; (j.dir, j.port) };
                match self.replug_cable(id, end, unit, dir, port) {
                    Replug::Moved => return,
                    // A fitting jack that can't take it: keep the cable (snap back).
                    Replug::Refused => {
                        let c = &mut self.cables[ci];
                        if end == Dir::Out { c.over_a = None; } else { c.over_b = None; }
                        return;
                    }
                    Replug::Incompatible => {}
                }
            }
        }
        if moved < 6.0 || far {
            self.unplug_cable(ci, end == Dir::In);
        } else {
            let c = &mut self.cables[ci];
            if end == Dir::Out { c.over_a = None; } else { c.over_b = None; }
        }
    }

    fn cable_at_jack(&self, unit: usize, dir: Dir, port: usize) -> Option<usize> {
        // Last plug wins: with fan-in, click/drag grabs the most recent cable.
        self.cables.iter().rposition(|c| {
            c.dying < 0.0
                && if dir == Dir::In { c.dst == unit && c.dst_port == port }
                   else { c.src == unit && c.src_port == port }
        })
    }

    /// Play a key: light it, launch visual pulses, and emit a note event to the
    /// audio seam. This is the single entry point for on-screen keys, laptop
    /// keys, and (later) hardware MIDI — task-4 routes sound from here.
    fn with_audio(
        mut self,
        mains: Vec<ClapVoiceMain>,
        controllers: Vec<VoiceController>,
        voice_ids: Vec<u64>,
        sample_rate: f64,
        voice_tx: rtrb::Producer<(u32, ClapVoice, Vec<f32>)>,
        voice_retire_rx: rtrb::Consumer<ClapVoice>,
        stream: cpal::Stream,
        bus: Arc<Mutex<CommandBus>>,
        level: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        self._audio_stream = Some(stream);
        if let Ok(mut b) = self.hw_direct.bus.lock() { *b = Some(bus.clone()); }
        if self.count_in_enabled || self.count_in_bars != 1 {
            let _ = bus.lock().unwrap().send(Command::SetCountIn {
                enabled: self.count_in_enabled,
                bars: self.count_in_bars,
            });
        }
        self.attach_engine(mains, controllers, voice_ids, sample_rate, voice_tx, voice_retire_rx, bus);
        self.out_level = Some(level);
        self.start_input_device_scan();
        self.start_audio_out_thread();
        self
    }

    /// Bind the app to an engine and its synth voice pool: everything the
    /// agent's tools need (command bus, voice mains for load_sound/sessions).
    /// The real app adds the audio stream and device threads on top;
    /// agent-eval runs on exactly this with an offline engine.
    fn attach_engine(
        &mut self,
        mains: Vec<ClapVoiceMain>,
        controllers: Vec<VoiceController>,
        voice_ids: Vec<u64>,
        sample_rate: f64,
        voice_tx: rtrb::Producer<(u32, ClapVoice, Vec<f32>)>,
        voice_retire_rx: rtrb::Consumer<ClapVoice>,
        bus: Arc<Mutex<CommandBus>>,
    ) {
        self.clap_plugin_ids = vec![SURGE_ID.to_string(); mains.len()];
        self.clap_voice_ids = voice_ids;
        self.clap_slot_map = (0..mains.len()).map(Some).collect();
        self._surge_main = mains;
        self.surge_controllers = controllers;
        self.sample_rate = sample_rate;
        self.voice_tx = Some(voice_tx);
        self.voice_retire_rx = Some(voice_retire_rx);
        self.bus = Some(bus);
        self.surge_status = "Surge loaded — play!".into();
    }

    /// Output selection + other apps' streams: scan every second (sooner when
    /// a command arrives), move our stream to the chosen output, apply
    /// volume/mute changes. All pactl I/O lives here (rule 48).
    fn start_audio_out_thread(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel::<crate::audio_out::StreamCmd>();
        self.stream_cmds = Some(tx);
        let state = self.audio_out.clone();
        let wanted = self.wanted_output.clone();
        let mut meters = crate::audio_out::StreamMeters::new(self.stream_levels.clone());
        std::thread::Builder::new()
            .name("kabelsalat-audio-out".into())
            .spawn(move || loop {
                let mut cmds = Vec::new();
                match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(c) => { cmds.push(c); while let Ok(c) = rx.try_recv() { cmds.push(c); } }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                crate::audio_out::apply_stream_cmds(cmds);
                let w = wanted.lock().ok().and_then(|w| w.clone());
                let st = crate::audio_out::scan_and_apply(w.as_deref());
                meters.sync(&st.streams);
                if let Ok(mut s) = state.lock() { *s = st; }
            })
            .ok();
    }

    /// Choose the output (None = PipeWire default); remembered in config.
    fn set_audio_output(&mut self, output: Option<String>) {
        let _ = crate::assistant::save_config_field("audio_output", output.as_deref().unwrap_or(""));
        if let Ok(mut w) = self.wanted_output.lock() { *w = output; }
    }

    /// Set another app's stream volume/mute (applied by the scan thread).
    fn set_app_stream(&mut self, index: u64, volume: Option<f32>, muted: Option<bool>) {
        let until = self.ui_time + 2.5;
        if let Some(v) = volume {
            let v = v.clamp(0.0, 1.5);
            self.stream_grace.retain(|(i, _, _)| *i != index);
            self.stream_grace.push((index, v, until));
            if let Some(tx) = &self.stream_cmds { let _ = tx.send(crate::audio_out::StreamCmd::Volume(index, v)); }
        }
        if let Some(m) = muted {
            if let Some(tx) = &self.stream_cmds { let _ = tx.send(crate::audio_out::StreamCmd::Mute(index, m)); }
            if let Ok(mut st) = self.audio_out.lock() {
                if let Some(s) = st.streams.iter_mut().find(|s| s.index == index) { s.muted = m; }
            }
        }
    }

    /// Other apps' streams as shown: the scan's values, with faders the user
    /// is moving held at their new value until the scan catches up.
    fn app_streams_shown(&mut self) -> Vec<crate::audio_out::AppStream> {
        let now = self.ui_time;
        self.stream_grace.retain(|(_, _, until)| *until > now);
        let mut v = self.audio_out.lock().map(|s| s.streams.clone()).unwrap_or_default();
        for s in &mut v {
            if let Some((_, val, _)) = self.stream_grace.iter().find(|(i, _, _)| *i == s.index) { s.volume = *val; }
        }
        v
    }

    fn laptop_key_offset(key: Key) -> Option<usize> {
        Some(match key {
            Key::A => 0, Key::W => 1, Key::S => 2, Key::E => 3, Key::D => 4,
            Key::F => 5, Key::T => 6, Key::G => 7, Key::Y => 8, Key::H => 9,
            Key::U => 10, Key::J => 11, Key::K => 12, Key::O => 13, Key::L => 14,
            Key::P => 15,
            _ => return None,
        })
    }

    fn handle_laptop_key(&mut self, key: Key, pressed: bool, repeat: bool) {
        // Laptop typing and on-screen clicks are one Keyboard source.
        self.current_source = SourceId::Keys;
        if pressed && !repeat {
            if key == Key::Z {
                let held: Vec<_> = self.held_keys.drain(..).collect();
                for (_, semi) in held {
                    self.fire_keys(semi, 0.0);
                }
                self.octave = self.octave.saturating_sub(1).max(1);
                return;
            }
            if key == Key::X {
                let held: Vec<_> = self.held_keys.drain(..).collect();
                for (_, semi) in held {
                    self.fire_keys(semi, 0.0);
                }
                self.octave = (self.octave + 1).min(7);
                return;
            }
        }
        let Some(off) = Self::laptop_key_offset(key) else { return };
        let semi = (off as i16 + 12 * (self.octave as i16 + 1)).clamp(0, 127) as usize;
        if pressed {
            if repeat || self.held_keys.iter().any(|(k, _)| *k == key) {
                return;
            }
            self.held_keys.push((key, semi));
            self.fire_keys(semi, 0.8);
        } else if let Some(pos) = self.held_keys.iter().position(|(k, _)| *k == key) {
            let (_, semi) = self.held_keys.remove(pos);
            self.fire_keys(semi, 0.0);
        }
    }

    fn unit_display_name(&self, node: u32) -> String {
        self.units
            .iter()
            .find(|u| u.node_id == node)
            .map(|u| sentence_case(&u.name))
            .unwrap_or_else(|| format!("node {node}"))
    }

    fn take_pending_agent_origin(&mut self) -> Option<AgentOrigin> {
        let (origin, exp) = self.pending_agent_origin?;
        if self.ui_time <= exp {
            self.pending_agent_origin = None;
            Some(origin)
        } else {
            self.pending_agent_origin = None;
            None
        }
    }

    /// MCP→UI live sync: adopt the engine's graph when a remote
    /// structural change (MCP/CLI load_graph/load_rigs/add_node/connect/…)
    /// diverges from the UI's own mirror. UI-originated edits converge (the
    /// per-frame Connect mirror is idempotent engine-side), so they never
    /// trigger a rebuild — animations and interaction state survive.
    fn sync_from_engine(&mut self) {
        if self.bus.is_some() {
            let remote_cmds: Vec<Command> = crate::bridge::REMOTE_CMDS
                .lock()
                .map(|mut q| q.drain(..).collect())
                .unwrap_or_default();
            let now = self.ui_time;
            for cmd in remote_cmds {
                match cmd {
                    Command::SetParam { node, param, value } => {
                        let v = value as f32;
                        if let Some(u) = self.units.iter_mut().find(|u| u.node_id == node) {
                            for k in &mut u.knobs {
                                if k.param == Some(param) {
                                    k.value = v.clamp(0.0, 1.0);
                                }
                            }
                        }
                        let uname = self.unit_display_name(node);
                        self.agent_activity.record(AgentOrigin::Remote, format!("Param → {uname}"), vec![node], now);
                    }
                    Command::SetRigSource { node, keyboard, hardware } => {
                        let rig = self.rig_of_node(node);
                        if let Some(s) = self.rig_sources.get_mut(rig) {
                            *s = (keyboard, hardware);
                        }
                        self.agent_activity.touch_cell(SourceId::Keys, node, now);
                        let uname = self.unit_display_name(node);
                        self.agent_activity.record(AgentOrigin::Remote, format!("Sources → {uname}"), vec![node], now);
                    }
                    Command::SetRigDevice { node, device } => {
                        let rig = self.rig_of_node(node);
                        if let Some(d) = self.rig_device.get_mut(rig) {
                            *d = device;
                        }
                        if let Some(d) = device {
                            self.agent_activity.touch_cell(SourceId::Hardware(d), node, now);
                        }
                        let uname = self.unit_display_name(node);
                        self.agent_activity.record(AgentOrigin::Remote, format!("Device → {uname}"), vec![node], now);
                    }
                    _ => {}
                }
            }
        }
        let Some(pub_) = &self.graph_pub else { return };
        let Ok(g) = pub_.lock() else { return };
        let now = self.ui_time;
        // Mixer mirror: adopt remote (MCP/CLI/agent) SetRigMix changes —
        // the Mixer showed stale vol/pan after a bridge edit (rule 25 class).
        let mut mix_diff_rigs: Vec<usize> = Vec::new();
        if g.mix_rev != self.last_mix_rev {
            self.last_mix_rev = g.mix_rev;
            for r in 0..8 {
                if g.mix[r] != self.rig_mix[r] && now >= self.ui_mix_grace[r] {
                    mix_diff_rigs.push(r);
                }
            }
            self.rig_mix = g.mix;
            // System-audio GAIN knobs follow SetSystemAudio from any frontend
            // (one engine-wide gain; `sent` matches so it is not echoed back).
            if !matches!(self.drag, Drag::Knob { .. }) {
                for u in &mut self.units {
                    for k in &mut u.knobs {
                        if k.param == Some(crate::engine::P_SYSTEM_GAIN) && (k.value - g.system_gain).abs() > 1e-4 {
                            k.value = g.system_gain;
                            k.sent = Some(g.system_gain);
                        }
                    }
                }
            }
        }
        // Master meter mirror (Bravo 1): the engine publishes a fresh snapshot
        // every block.
        if g.meter_rev != self.last_meter_rev {
            self.last_meter_rev = g.meter_rev;
            self.master_meter = g.meter;
        }
        let mut transport_changed = false;
        if g.transport_rev != self.last_transport_rev {
            self.last_transport_rev = g.transport_rev;
            let was_counting = self.counting_in;
            let now_playing = g.transport.playing || g.transport.counting_in;
            let bpm_diff = !matches!(self.drag, Drag::Bpm { .. }) && (g.transport.bpm - self.bpm).abs() > 0.05;
            if now >= self.ui_transport_grace
                && (now_playing != self.playing
                    || bpm_diff
                    || g.transport.count_in_enabled != self.count_in_enabled
                    || g.transport.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS) != self.count_in_bars
                    || g.transport.record_sync != self.record_sync)
            {
                transport_changed = true;
            }
            self.counting_in = g.transport.counting_in;
            self.count_in_beat = g.transport.count_in_beat;
            self.count_in_enabled = g.transport.count_in_enabled;
            self.count_in_bars = g.transport.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS);
            self.record_sync = g.transport.record_sync;
            if !matches!(self.drag, Drag::Bpm { .. }) {
                self.bpm = g.transport.bpm;
            }
            if (now_playing && !self.playing) || (was_counting && !self.counting_in && g.transport.playing) {
                self.time = 0.0;
            }
            self.playing = now_playing;
        }
        let graph_rev_changed = g.rev != self.last_graph_rev;
        if !graph_rev_changed {
            drop(g);
            if !mix_diff_rigs.is_empty() || transport_changed {
                let pending_origin = self.take_pending_agent_origin();
                let n_rigs = self.rig_count();
                for r in mix_diff_rigs {
                    self.agent_activity.touch_rig(r, now);
                    let nodes: Vec<u32> = self.units.iter()
                        .filter(|u| n_rigs <= 1 || u.rig as usize == r)
                        .map(|u| u.node_id)
                        .collect();
                    if pending_origin.is_none() {
                        self.agent_activity.record(AgentOrigin::Remote, format!("Mix → {}", self.rig_name(r)), nodes, now);
                    } else {
                        for n in nodes { self.agent_activity.touch_node(n, now); }
                    }
                }
                if transport_changed {
                    let nodes: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
                    if pending_origin.is_none() && !nodes.is_empty() {
                        self.agent_activity.record(AgentOrigin::Remote, format!("Transport ({:.0} BPM)", self.bpm), nodes, now);
                    } else {
                        for n in nodes { self.agent_activity.touch_node(n, now); }
                    }
                }
            }
            return;
        }
        self.last_graph_rev = g.rev;
        // Mirror the engine's rules (covers remote MCP rule edits too).
        let mut edits = std::collections::HashMap::new();
        for r in &g.rules {
            let e = RuleEdit {
                channel: r.channel,
                transpose: r.transpose,
                key_lo: r.key_range.0, key_hi: r.key_range.1,
                vel_lo: r.vel_range.0, vel_hi: r.vel_range.1,
                enabled: r.enabled,
                cc_map: r.cc_map.clone(),
                cc_only: r.cc_only,
            };
            if !e.is_default() { edits.insert((r.source, r.dest), e); }
        }
        let mut changed_rules: Vec<(SourceId, u32)> = Vec::new();
        for (&k, v) in &edits {
            if self.rule_edits.get(&k) != Some(v) {
                changed_rules.push(k);
            }
        }
        for &k in self.rule_edits.keys() {
            if !edits.contains_key(&k) {
                changed_rules.push(k);
            }
        }
        self.rule_edits = edits;
        // Per-instrument transport: adopt the engine's playing flags into the
        // base graph + unit faces + bridge mirror BEFORE the divergence check,
        // so a Play/Stop (local chip, MCP, master) never rebuilds the rack.
        let mut touched_nodes: Vec<u32> = Vec::new();
        let mut shared = self.shared_graph.lock().ok();
        for n in g.graph.nodes.iter().filter(|n| n.kind.supports_transport()) {
            let was = self.base_graph.node(n.id).map(|old| old.playing);
            if was.is_some_and(|w| w != n.playing) && now >= self.ui_node_grace.get(&n.id).copied().unwrap_or(-1.0) {
                touched_nodes.push(n.id);
            }
            self.base_graph.set_playing(n.id, n.playing);
            if let Some(sg) = shared.as_mut() { sg.set_playing(n.id, n.playing); }
            for u in self.units.iter_mut().filter(|u| u.node_id == n.id && u.transport) {
                u.playing = n.playing;
            }
        }
        // Keys split (a node property): adopt the engine's point the same way
        // — jacks swap 1 ↔ 2 MIDI outs without a rebuild. A vanished HIGH
        // jack can't stay referenced by in-flight interaction state.
        for n in g.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Keys)) {
            let was = self.base_graph.node(n.id).map(|old| old.split);
            if was.is_some_and(|w| w != n.split) && now >= self.ui_node_grace.get(&n.id).copied().unwrap_or(-1.0) {
                touched_nodes.push(n.id);
            }
            self.base_graph.set_split(n.id, n.split);
            if let Some(sg) = shared.as_mut() { sg.set_split(n.id, n.split); }
            for (i, u) in self.units.iter_mut().enumerate() {
                if u.node_id != n.id || !matches!(u.node_kind, UnitKind::Keys) || u.split == n.split { continue; }
                set_unit_split(u, n.split);
                if self.pending.as_ref().map_or(false, |p| p.unit == i) { self.pending = None; }
                if matches!(self.hover, Some(Hit::Jack { unit, .. }) if unit == i) { self.hover = None; }
                if matches!(self.press, Some((Hit::Jack { unit, .. }, _)) if unit == i) { self.press = None; }
            }
        }
        drop(shared);
        // Audio In (a node property): adopt the engine's device/channels in
        // place so a remote MCP/CLI set_audio_in flips the face without a
        // rebuild; the capture (re)open is deferred past `drop(g)` below.
        let mut audio_resync: Vec<u32> = Vec::new();
        for n in g.graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::AudioIn { .. })) {
            let NodeKind::AudioIn { device, channels } = &n.kind else { continue };
            let prev = self.base_graph.node(n.id).and_then(|m| match &m.kind {
                NodeKind::AudioIn { device, channels } => Some((device.clone(), *channels)),
                _ => None,
            });
            if prev.as_ref().map(|(d, c)| d == device && c == channels) == Some(true) {
                continue;
            }
            if let Some(m) = self.base_graph.nodes.iter_mut().find(|m| m.id == n.id) {
                if let NodeKind::AudioIn { device: d, channels: c } = &mut m.kind {
                    *d = device.clone();
                    *c = *channels;
                }
            }
            for u in self.units.iter_mut().filter(|u| u.node_id == n.id) {
                u.device = device.clone();
                u.channels = *channels;
                u.sub = audio_in_sub(u.device.as_deref(), *channels);
                u.sub_g = None;
            }
            audio_resync.push(n.id);
        }
        // Build the UI's current mirror: base nodes + living cables.
        let mut mirror = self.base_graph.clone();
        mirror.edges.clear();
        for c in &self.cables {
            if c.dying >= 0.0 { continue; }
            let (Some(sn), Some(dn)) = (
                self.units.get(c.src).map(|u| u.node_id),
                self.units.get(c.dst).map(|u| u.node_id),
            ) else { continue };
            let kind = match c.kind {
                PortKind::Midi => CPortKind::Midi,
                PortKind::Audio => CPortKind::Audio,
            };
            mirror.connect_with_id(
                c.id,
                PortRef { node: sn, port: c.src_port as u8 },
                PortRef { node: dn, port: c.dst_port as u8 },
                kind,
            );
        }
        // Full structural comparison (rare path — only when the rev changed).
        let same = serde_json::to_value(&mirror.nodes).ok() == serde_json::to_value(&g.graph.nodes).ok()
            && serde_json::to_value(&mirror.edges).ok() == serde_json::to_value(&g.graph.edges).ok();
        let mut diff_edges: Vec<u32> = Vec::new();
        let mut diff_rigs: Vec<usize> = Vec::new();
        let mut diff_summary: Option<String> = None;
        if !same {
            let mut added = 0usize;
            for n in &g.graph.nodes {
                let prev = mirror.nodes.iter().find(|m| m.id == n.id);
                if prev.is_none() {
                    added += 1;
                    touched_nodes.push(n.id);
                    diff_rigs.push(n.rig as usize);
                } else if prev.is_some_and(|m| m.rig != n.rig || m.name != n.name) {
                    touched_nodes.push(n.id);
                }
            }
            let mut removed = 0usize;
            for m in &mirror.nodes {
                if !g.graph.nodes.iter().any(|n| n.id == m.id) {
                    removed += 1;
                    diff_rigs.push(m.rig as usize);
                    for e in &mirror.edges {
                        if e.from.node == m.id && e.to.node != m.id { touched_nodes.push(e.to.node); }
                        if e.to.node == m.id && e.from.node != m.id { touched_nodes.push(e.from.node); }
                    }
                }
            }
            let mut wired = 0usize;
            for e in &g.graph.edges {
                if !mirror.edges.iter().any(|me| me.id == e.id && me.from == e.from && me.to == e.to && me.kind == e.kind) {
                    wired += 1;
                    diff_edges.push(e.id);
                    touched_nodes.push(e.from.node);
                    touched_nodes.push(e.to.node);
                }
            }
            let mut unwired = 0usize;
            for me in &mirror.edges {
                if !g.graph.edges.iter().any(|e| e.id == me.id && e.from == me.from && e.to == me.to && e.kind == me.kind) {
                    unwired += 1;
                    touched_nodes.push(me.from.node);
                    touched_nodes.push(me.to.node);
                }
            }
            diff_summary = Some(match (added, removed, wired, unwired) {
                (a, 0, _, _) if a > 0 => format!("Added {a} unit{}", if a == 1 { "" } else { "s" }),
                (0, r, _, _) if r > 0 => format!("Removed {r} unit{}", if r == 1 { "" } else { "s" }),
                (0, 0, w, 0) if w > 0 => format!("Connected {w} cable{}", if w == 1 { "" } else { "s" }),
                (0, 0, 0, u) if u > 0 => format!("Disconnected {u} cable{}", if u == 1 { "" } else { "s" }),
                _ => "Patch updated".into(),
            });
        }
        let adopted = g.graph.clone();
        let mix = g.mix;
        drop(g);
        // (Re)open input captures after releasing the graph publication lock —
        // cpal open is I/O and must not hold the frontend snapshot lock.
        for node in audio_resync.drain(..) {
            self.resync_audio_in_capture(node);
        }

        let pending_origin = self.take_pending_agent_origin();
        let origin = pending_origin.unwrap_or(AgentOrigin::Remote);
        for r in &mix_diff_rigs {
            self.agent_activity.touch_rig(*r, now);
            let n_rigs = self.rig_count();
            for u in &self.units {
                if n_rigs <= 1 || u.rig as usize == *r {
                    touched_nodes.push(u.node_id);
                }
            }
        }
        if transport_changed {
            for u in self.units.iter().filter(|u| u.transport) {
                touched_nodes.push(u.node_id);
            }
        }
        for &(src, node) in &changed_rules {
            if now >= self.ui_node_grace.get(&node).copied().unwrap_or(-1.0) {
                self.agent_activity.touch_cell(src, node, now);
                touched_nodes.push(node);
            }
        }
        for &eid in &diff_edges {
            self.agent_activity.touch_edge(eid, now);
        }
        for &r in &diff_rigs {
            self.agent_activity.touch_rig(r, now);
        }
        if !touched_nodes.is_empty() {
            touched_nodes.sort_unstable();
            touched_nodes.dedup();
            if pending_origin.is_none() {
                let summary = diff_summary.unwrap_or_else(|| {
                    if !changed_rules.is_empty() {
                        format!("Rule → {}", self.unit_display_name(changed_rules[0].1))
                    } else {
                        format!("Updated {}", self.unit_display_name(touched_nodes[0]))
                    }
                });
                self.agent_activity.record(origin, summary, touched_nodes, now);
            } else {
                for n in touched_nodes {
                    self.agent_activity.touch_node(n, now);
                }
            }
        }

        if same {
            self.sync_clap_slots(&adopted);
            return;
        }
        self.build_rack(&adopted, &[]);
        self.sync_clap_slots(&adopted);
        self.rig_mix = mix; // build_rack resets the mixer (rule 20)
        // Reset per-rig source filters to the engine's rig bindings.
        self.rig_sources = [(true, true); VOICE_POOL];
        self.rig_device = [None; VOICE_POOL];
        self.adopt_rig_devices();
        // build_rack resets the view to rig 0 — keep the engine's active rig.
        let active = self.rig_snapshot.lock().map(|s| s.active).unwrap_or(0);
        self.active_rig = usize::MAX;
        self.set_active_rig(active);
        self.editor_open = false;
        self.editor_params.clear();
        self.session_msg = "Patch updated from MCP/CLI".into();
    }

    /// Take in the worker's events and run its tool calls. The worker sends
    /// one batch per model step and waits for all of its results.
    fn poll_assistant(&mut self) {
        if let Some(batch) = self.assistant.poll().pop() {
            self.tool_batch = Some(ToolBatch { todo: batch.into(), results: Vec::new(), waiting: None });
        }
        let Some(mut batch) = self.tool_batch.take() else { return };
        if let Some(edit) = batch.waiting.take() {
            match self.engine_edit_outcome(&edit) {
                Some(result) => batch.results.push(result),
                None => {
                    batch.waiting = Some(edit);
                    self.tool_batch = Some(batch);
                    return;
                }
            }
        }
        while let Some(call) = batch.todo.pop_front() {
            let engine_edit = self.graph_pub.is_some() && ENGINE_EDIT_TOOLS.contains(&call.name.as_str());
            let before = engine_edit.then(|| self.rack_ids());
            let result = self.run_assistant_tool(&call.name, &call.args);
            let Some(before) = before else {
                batch.results.push(result);
                continue;
            };
            batch.waiting = Some(EngineEdit { result, before, deadline: std::time::Instant::now() + ENGINE_EDIT_WAIT });
            self.tool_batch = Some(batch);
            return;
        }
        self.assistant.send_tool_results(batch.results);
    }

    /// Node ids and living cable ids of the rack.
    fn rack_ids(&self) -> (Vec<u32>, Vec<u32>) {
        (
            self.units.iter().map(|u| u.node_id).collect(),
            self.cables.iter().filter(|c| c.dying < 0.0).map(|c| c.id).collect(),
        )
    }

    /// What an engine edit did to the rack, appended to the tool's result:
    /// the ids it created or removed, or that nothing changed. None = the
    /// engine has not published it yet (and may still).
    fn engine_edit_outcome(&self, edit: &EngineEdit) -> Option<String> {
        let (nodes, cables) = self.rack_ids();
        let (nodes_before, cables_before) = &edit.before;
        let list = |what: &str, now: &[u32], then: &[u32]| {
            let ids: Vec<String> = now.iter().filter(|id| !then.contains(id)).map(|id| id.to_string()).collect();
            (!ids.is_empty()).then(|| format!("{what} {}", ids.join(", ")))
        };
        let changes: Vec<String> = [
            list("new node id", &nodes, nodes_before),
            list("removed node", nodes_before, &nodes),
            list("new cable id", &cables, cables_before),
            list("removed cable", cables_before, &cables),
        ].into_iter().flatten().collect();
        if !changes.is_empty() {
            Some(format!("{} [applied: {}]", edit.result, changes.join("; ")))
        } else if std::time::Instant::now() < edit.deadline {
            None
        } else {
            Some(format!("{} — NOT applied: the engine changed nothing (check the ids with get_graph)", edit.result))
        }
    }

    /// Run one assistant tool call. A destructive tool called without
    /// confirm=true is refused by `exec_tool` and parked as a Confirm /
    /// Cancel prompt in the chat (#32); the model is told to ask and wait.
    fn run_assistant_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        let result = self.exec_tool_with_origin(name, args, AgentOrigin::Assistant);
        let confirmed = args.get("confirm").and_then(|v| v.as_bool()) == Some(true);
        if CONFIRM_TOOLS.contains(&name) && !confirmed && result.starts_with("not ") {
            let label = match name {
                "delete_rig" => {
                    let rig = args.get("rig").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    format!("Delete rig {} · {}", rig + 1, self.rig_name(rig))
                }
                _ => format!("Install {}", args.get("id").and_then(|v| v.as_str()).unwrap_or("instrument")),
            };
            self.pending_confirm = Some(PendingConfirm { tool: name.to_string(), args: args.clone(), label });
            return format!("{result}. The chat now shows Confirm / Cancel buttons for this: ask the user in one short sentence and wait — their click runs or cancels it (do not call it again with confirm=true unless they answer in text).");
        }
        result
    }

    /// The user clicked Confirm (true) or Cancel (false) on the pending
    /// destructive action. Confirm runs it with confirm=true; either way the
    /// model hears what happened.
    fn resolve_confirm(&mut self, confirm: bool) {
        let Some(p) = self.pending_confirm.take() else { return };
        if confirm {
            let mut args = p.args.clone();
            if let Some(o) = args.as_object_mut() { o.insert("confirm".into(), serde_json::Value::Bool(true)); }
            let result = self.exec_tool_with_origin(&p.tool, &args, AgentOrigin::Assistant);
            self.assistant.send_user_display(
                format!("Confirm · {}", p.label),
                format!("[The user clicked Confirm. The app ran {} with confirm=true: {result}] Report the outcome briefly; do not call it again.", p.tool),
            );
        } else {
            self.assistant.send_user_display(
                format!("Cancel · {}", p.label),
                format!("[The user clicked Cancel: do not run {} ({}).] Acknowledge briefly.", p.tool, p.label),
            );
        }
    }

    fn sessions_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = std::path::PathBuf::from(home).join("kabelsalat-sessions");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn list_saved_sessions(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().map(|x| x == "ksalat").unwrap_or(false) {
                            p.file_stem().map(|s| s.to_string_lossy().into_owned())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn delete_saved_session(dir: &std::path::Path, name: &str) -> Result<(), String> {
        let path = crate::session::Session::package_path(dir, name).map_err(|e| e.to_string())?;
        if !path.exists() {
            return Err(format!("session '{name}' does not exist"));
        }
        std::fs::remove_dir_all(&path).map_err(|e| e.to_string())
    }

    fn toggle_stage(&mut self, view: StageView) {
        self.stage_view = if self.stage_view == view { StageView::Rack } else { view };
        if self.stage_view != StageView::Sessions {
            self.confirm_delete_session = None;
        }
        if self.stage_view == StageView::Plugins && !self.plugins_loaded {
            self.refresh_plugins();
        }
    }

    // -- agent undo -------------------------------------------------------

    fn capture_undo(&mut self) -> Option<UndoSnap> {
        let session = self.capture_session("Undo").ok()?;
        let rigs = self.rig_snapshot.lock().map(|s| s.rigs.clone()).unwrap_or_default();
        Some(UndoSnap {
            session, rigs, mix: self.rig_mix, active: self.active_rig,
            patch_names: self.patch_names.clone(), rig_sources: self.rig_sources,
        })
    }

    /// Before an agent's first change in a job, snapshot the rack. A job is
    /// one assistant request, or a burst of remote calls (gaps < 3 s).
    fn note_agent_change(&mut self, origin: AgentOrigin) {
        let now = self.ui_time;
        let same_job = match self.undo_job {
            Some((o, last)) => o == origin && (origin == AgentOrigin::Assistant || now - last < REMOTE_JOB_GAP),
            None => false,
        };
        if !same_job {
            if let Some(snap) = self.capture_undo() {
                self.undo_past.push(snap);
                if self.undo_past.len() > UNDO_LIMIT { self.undo_past.remove(0); }
                self.undo_future.clear();
            }
        }
        self.undo_job = Some((origin, now));
    }

    /// Restore a snapshot exactly the way a rig edit reloads (rule 20:
    /// mixer and rig in view are applied AFTER the reload).
    fn restore_undo(&mut self, snap: UndoSnap) -> anyhow::Result<()> {
        let (graphs, _) = crate::control::split_rigs(&snap.session.graph);
        let list: Vec<crate::control::Rig> = graphs.into_iter().enumerate().map(|(i, g)| {
            let meta = snap.rigs.get(i);
            let name = meta.map(|m| m.name.clone()).filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Rig {}", (b'A' + i.min(25) as u8) as char));
            crate::control::Rig::new(name, g, meta.and_then(|m| m.midi_device))
        }).collect();
        let active = snap.active.min(list.len().saturating_sub(1));
        self.load_as_rigs = Some((list.clone(), active));
        self.apply_session(&snap.session)?;
        if let Ok(mut s) = self.rig_snapshot.lock() {
            s.rigs = list.iter().map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
            s.active = active;
        }
        self.adopt_rig_devices();
        self.patch_names = snap.patch_names;
        // apply_session reset the source switches: restore and re-send them.
        self.rig_sources = snap.rig_sources;
        for (_, node) in self.clap_rigs() { self.send_rig_source(node); }
        self.rig_mix = snap.mix;
        self.active_rig = usize::MAX; // force set_active_rig to regroup
        self.set_active_rig(active);
        for i in 0..list.len() { self.send_rig_mix(i); }
        self.editor_status.clear();
        Ok(())
    }

    /// Undo (or redo) the last agent job. Returns a line for the agent/UI.
    fn undo_agent(&mut self, undo: bool) -> Result<String, String> {
        let from = if undo { &mut self.undo_past } else { &mut self.undo_future };
        let Some(target) = from.pop() else {
            return Err(if undo { "nothing to undo".into() } else { "nothing to redo".into() });
        };
        let Some(current) = self.capture_undo() else { return Err("could not capture the current rack".into()) };
        if undo { self.undo_future.push(current) } else { self.undo_past.push(current) }
        self.undo_job = None; // the next agent change starts a fresh step
        self.restore_undo(target).map_err(|e| e.to_string())?;
        let what = if undo { "undid" } else { "redid" };
        self.session_msg = format!("{what} the last agent change");
        Ok(format!("{what} the last agent change · undo left {} · redo left {}", self.undo_past.len(), self.undo_future.len()))
    }

    fn undo_agent_tool(&mut self, undo: bool) -> String {
        match self.undo_agent(undo) { Ok(s) | Err(s) => s }
    }

    /// Play (or stop) the last recorded take through the file player.
    fn toggle_take_playback(&mut self, path: &str, secs: f64) {
        let playing = self.take_play_until > self.ui_time;
        let gain = if playing { 0.0 } else { 0.8 };
        if let Some(pc) = &self.player_ctrl {
            if playing { pc.stop() } else { pc.play(path) }
        }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetPlayer { gain });
        }
        self.take_play_until = if playing { 0.0 } else { self.ui_time + secs as f32 + 0.2 };
    }

    fn assistant_open(&self) -> bool { self.dock == DockView::Assistant }

    fn keyboard_open(&self) -> bool { self.dock == DockView::Keyboard }

    /// Toggle a dock occupant (one at a time). Opening the assistant also asks
    /// the chat field to take keyboard focus on the next frame, so the agent
    /// owns the computer keyboard instead of the laptop piano.
    fn toggle_dock(&mut self, d: DockView) {
        self.dock = if self.dock == d { DockView::None } else { d };
        if self.dock == DockView::Assistant {
            self.focus_chat_request = true;
        }
    }

    fn stage_sheet_rects(&self, rect: Rect, min_h: f32) -> (Rect, Rect) {
        let top = self.toolbar_top + 20.0;
        let w = (rect.width() - 60.0).clamp(560.0, 1240.0).min((rect.width() - 16.0).max(200.0));
        let sheet = Rect::from_min_max(
            Pos2::new(rect.center().x - w / 2.0, top),
            Pos2::new(rect.center().x + w / 2.0, (rect.bottom() - 20.0).max(top + min_h)),
        );
        (sheet, sheet.shrink2(Vec2::new(32.0, 26.0)))
    }

    /// Snapshot the current patch + knobs + plugin state into a Session.
    fn capture_session(&mut self, name: &str) -> anyhow::Result<crate::session::Session> {
        // Nodes come from the loaded graph; edges from the living cables.
        let mut graph = self.base_graph.clone();
        graph.edges.clear();
        for c in &self.cables {
            if c.dying >= 0.0 { continue; }
            let kind = match c.kind {
                PortKind::Midi => CPortKind::Midi,
                PortKind::Audio => CPortKind::Audio,
            };
            let (Some(sn), Some(dn)) = (
                self.units.get(c.src).map(|u| u.node_id),
                self.units.get(c.dst).map(|u| u.node_id),
            ) else { continue };
            graph.connect_with_id(
                c.id,
                PortRef { node: sn, port: c.src_port as u8 },
                PortRef { node: dn, port: c.dst_port as u8 },
                kind,
            );
        }
        let mut s = crate::session::Session::new(name, graph);
        for u in &self.units {
            for k in &u.knobs {
                if let Some(p) = k.param {
                    s.set_param(u.node_id, p, k.value as f64);
                }
            }
        }
        let mut rig = 0;
        for u in &self.units {
            if matches!(u.node_kind, UnitKind::Clap) {
                if let Some(idx) = self.clap_pool_idx(rig) {
                    let pid = self.clap_plugin_ids.get(idx).cloned().unwrap_or_else(|| SURGE_ID.to_string());
                    if let Some(m) = self._surge_main.get_mut(idx) {
                        if let Ok(blob) = m.save_state() {
                            s.set_plugin_state(u.node_id, &pid, blob);
                        }
                    }
                }
                rig += 1;
            }
        }
        // Shaped matrix routes and CC/MIDI-learn bindings (rule 33 mirror).
        let mut rules: Vec<SavedRule> = self.rule_edits.iter()
            .filter(|((_, dest), _)| s.graph.node(*dest).is_some())
            .map(|(&(source, dest), e)| SavedRule { source, dest, edit: e.clone() })
            .collect();
        rules.sort_by_key(|r| (format!("{:?}", r.source), r.dest));
        if !rules.is_empty() {
            s.set_matrix(Some(serde_json::to_value(&rules)?));
        }
        s.rig_names = (0..self.rig_count()).map(|r| self.rig_name(r)).collect();
        Ok(s)
    }

    /// Re-apply a session's shaped matrix routes after its graph is loaded.
    fn restore_session_rules(&mut self, s: &crate::session::Session) {
        let rules: Vec<SavedRule> = s.matrix()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        self.rule_edits.clear();
        for r in rules.into_iter().filter(|r| s.graph.node(r.dest).is_some()) {
            if let Some(bus) = &self.bus {
                let mut b = bus.lock().unwrap();
                if !r.edit.cc_only {
                    let e = &r.edit;
                    let _ = b.send(Command::MatrixRule {
                        source: r.source, dest: r.dest, channel: e.channel, transpose: e.transpose,
                        key_lo: e.key_lo, key_hi: e.key_hi, vel_lo: e.vel_lo, vel_hi: e.vel_hi, enabled: e.enabled,
                    });
                }
                for m in &r.edit.cc_map {
                    let _ = b.send(Command::MatrixCcMap { source: r.source, dest: r.dest, cc: m.cc, param: Some(m.param), target: m.target });
                }
            }
            self.rule_edits.insert((r.source, r.dest), r.edit);
        }
    }

    /// Pool index in `_surge_main` / `surge_controllers` for CLAP rig slot `rig`.
    fn clap_pool_idx(&self, rig: usize) -> Option<usize> {
        self.clap_slot_map.get(rig).copied().flatten()
    }

    /// Drain retired CLAP voices and match every `UnitKind::Clap` in `self.units`
    /// to a `ClapVoiceMain` of matching `plugin_id`, instantiating new CLAP
    /// plugins (e.g. Dexed) on the UI thread and delivering them to the engine.
    fn sync_clap_slots(&mut self, graph: &Graph) {
        while let Some(rx) = self.voice_retire_rx.as_mut() {
            let Ok(voice) = rx.pop() else { break };
            if let Some(pos) = self.clap_voice_ids.iter().position(|&id| id == voice.voice_id()) {
                let main = self._surge_main.remove(pos);
                self.surge_controllers.remove(pos);
                self.clap_plugin_ids.remove(pos);
                self.clap_voice_ids.remove(pos);
                main.retire(voice);
            }
        }
        let clap_nodes: Vec<(u32, String)> = self
            .units
            .iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .map(|u| {
                let pid = graph
                    .node(u.node_id)
                    .and_then(|n| match &n.kind {
                        NodeKind::Clap { plugin_id } => Some(crate::control::name_str(plugin_id).to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| SURGE_ID.to_string());
                (u.node_id, pid)
            })
            .collect();
        let mut used = vec![false; self._surge_main.len()];
        let mut new_map = Vec::with_capacity(clap_nodes.len());
        for (node_id, pid) in clap_nodes {
            let found = (0..self._surge_main.len()).find(|&i| {
                !used[i]
                    && (self.clap_plugin_ids[i] == pid
                        || ((pid.is_empty() || pid == "Surge XT") && self.clap_plugin_ids[i] == SURGE_ID))
            });
            if let Some(i) = found {
                used[i] = true;
                new_map.push(Some(i));
            } else if self.voice_tx.is_some() {
                match ClapVoice::new_plugin(self.sample_rate, &pid) {
                    Ok((voice, main)) => {
                        let ctrl = voice.controller();
                        let vid = voice.voice_id();
                        if let Some(tx) = self.voice_tx.as_mut() {
                            if tx.push((node_id, voice, vec![0.0; 8192 * 2])).is_err() {
                                self.plugin_msg = format!("Voice channel full for {pid}");
                                new_map.push(None);
                                continue;
                            }
                        }
                        self._surge_main.push(main);
                        self.surge_controllers.push(ctrl);
                        self.clap_plugin_ids.push(pid);
                        self.clap_voice_ids.push(vid);
                        used.push(true);
                        new_map.push(Some(self._surge_main.len() - 1));
                    }
                    Err(e) => {
                        self.plugin_msg = format!("Load {pid} failed: {e}");
                        new_map.push(None);
                    }
                }
            } else {
                new_map.push(None);
            }
        }
        self.clap_slot_map = new_map;
        self.rule_param_cache = None;
    }

    /// Load a Session: rebuild the rack from its graph + params, restore plugin
    /// state, and rewire the engine. Works for any topology (templates, saved
    /// sessions, the demo patch).
    fn apply_session(&mut self, s: &crate::session::Session) -> anyhow::Result<()> {
        let canonical = canonical_rig_session(s);
        let s = &canonical;
        self.session_rig_names = s.rig_names.clone();
        self.build_rack(&s.graph, s.params());
        self.begin_load_fade();
        self.sync_clap_slots(&s.graph);
        // Restore plugin state into the remapped voices (rig order).
        let clap_nodes: Vec<u32> = self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .map(|u| u.node_id).collect();
        for (rig, node) in clap_nodes.iter().enumerate() {
            if let Some(st) = s.plugin_state(*node) {
                if let Some(idx) = self.clap_pool_idx(rig) {
                    if let Some(m) = self._surge_main.get_mut(idx) {
                        let _ = m.load_state(&st.data);
                    }
                }
            }
        }
        // Rewire the engine, then publish every knob value.
        self.ui_transport_grace = self.ui_time + 0.6;
        self.load_graph_into_engine(&s.graph);
        // The editor referenced the old voice mapping; close it.
        self.editor_open = false;
        self.editor_params.clear();
        self.editor_status = "editor closed (rack reloaded)".into();
        // Reset per-rig filters for the new graph (Keys splits travel IN the
        // graph — Node::split — so LoadGraph restores them).
        self.rig_sources = [(true, true); VOICE_POOL];
        self.rig_device = [None; VOICE_POOL];
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap();
            for u in &mut self.units {
                for k in &mut u.knobs {
                    if let Some(p) = k.param {
                        if b.send(Command::SetParam { node: u.node_id, param: p, value: k.value as f64 }) {
                            k.sent = Some(k.value);
                        }
                    }
                }
            }
        }
        self.restore_session_rules(s);
        Ok(())
    }

    /// Send the engine a full graph replacement (voices remap, DSP rebuild).
    fn load_graph_into_engine(&mut self, graph: &Graph) {
        // A rig add/delete (edit_rigs) loads the same merged graph as rigs;
        // so does any graph that carries several rig tags (a session saved
        // from a multi-rig rack) — LoadGraph would drop the engine's rig list.
        // Names from the session being applied, used once (never leak into a
        // later load).
        let names = std::mem::take(&mut self.session_rig_names);
        let rigs = self.load_as_rigs.take().or_else(|| {
            // A single rig keeps LoadGraph — unless the session renamed it,
            // which only the rig list (LoadRigs) can carry (#33).
            let renamed_single = names.first().is_some_and(|n| !n.trim().is_empty() && n != "Rig A");
            if graph.nodes.iter().all(|n| n.rig == 0) && !renamed_single { return None; }
            let (graphs, _) = crate::control::split_rigs(graph);
            let rigs: Vec<crate::control::Rig> = graphs.into_iter().enumerate()
                .map(|(i, g)| {
                    let name = names.get(i).filter(|n| !n.trim().is_empty()).cloned()
                        .unwrap_or_else(|| format!("Rig {}", (b'A' + i.min(25) as u8) as char));
                    crate::control::Rig::new(name, g, None)
                })
                .collect();
            Some((rigs, 0))
        });
        // Mirror the rig list now (names, devices), so chips, mixer and
        // get_rigs show it on the next frame — and without an engine (rule 25).
        if let Ok(mut snap) = self.rig_snapshot.lock() {
            match &rigs {
                Some((rigs, active)) => {
                    snap.rigs = rigs.iter().map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
                    snap.active = *active;
                }
                // LoadGraph: the engine drops its rig list; so does the mirror.
                None => { snap.rigs.clear(); snap.active = 0; }
            }
        }
        if let Some(bus) = &self.bus {
            let cmd = match rigs {
                Some((rigs, active)) => Command::LoadRigs { rigs, active },
                None => Command::LoadGraph { graph: graph.clone() },
            };
            let _ = bus.lock().unwrap().send(cmd);
        }
    }

    /// Load a starter template (playable pre-wired rack + default knobs).
    fn load_template(&mut self, t: Template) {
        let s = t.session();
        match self.apply_session(&s) {
            Ok(_) => self.session_msg = format!("Loaded template '{}'", t.title()),
            Err(e) => self.session_msg = format!("Template failed: {e}"),
        }
    }

    /// Reload the default two-stack demo patch.
    fn load_demo_patch(&mut self) {
        let s = crate::session::Session::new("Demo patch", Graph::demo_patch());
        match self.apply_session(&s) {
            Ok(_) => self.session_msg = "Loaded demo patch".into(),
            Err(e) => self.session_msg = format!("Demo failed: {e}"),
        }
    }

    /// Load two independent rigs (AUM operating model): each a
    /// full rack with its own MasterOut, merged via merge_rigs and sent to the
    /// engine as LoadRigs. Rig A: Keys→Surge→Delay→Master; Rig B: Keys→Surge→
    /// Master. Both sound at once; the big view shows one (1–9 / ←→ switch).
    fn load_multi_rig_demo(&mut self) {
        fn mk_rig(name: &str, with_delay: bool) -> crate::control::Rig {
            let mut g = Graph::default();
            g.reserve(8, 8);
            let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_ID) }, name_bytes("SURGE"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, CPortKind::Midi);
            if with_delay {
                let fx = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
                g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: fx, port: 0 }, CPortKind::Audio);
                g.connect_with_id(3, PortRef { node: fx, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
                g.connect_with_id(4, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, CPortKind::Audio);
            } else {
                g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
                g.connect_with_id(3, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, CPortKind::Audio);
            }
            crate::control::Rig::new(name, g, None)
        }
        let rigs = vec![mk_rig("Rig A", true), mk_rig("Rig B", false)];
        let merged = crate::control::merge_rigs(&rigs);
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::LoadRigs { rigs, active: 0 });
        }
        self.build_rack(&merged, &[]);
        self.begin_load_fade();
        self.editor_open = false;
        self.editor_params.clear();
        self.rig_sources = [(true, true); VOICE_POOL];
        self.rig_device = [None; VOICE_POOL];
        self.session_msg = "Loaded multi-rig demo — 1–9 / ←→ switch rigs".into();
    }

    fn exec_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        self.exec_tool_with_origin(name, args, AgentOrigin::Assistant)
    }

    fn exec_tool_with_origin(&mut self, name: &str, args: &serde_json::Value, origin: AgentOrigin) -> String {
        let now = self.ui_time;
        if !tool_is_read_only(name) && !tool_changes_only_system(name) { self.note_agent_change(origin); }
        match name {
            "undo" => return self.undo_agent_tool(true),
            "redo" => return self.undo_agent_tool(false),
            "get_graph" => match self.shared_graph.lock() {
                Ok(g) => serde_json::to_value(&*g)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|e| format!("graph serialize: {e}")),
                Err(_) => "graph lock poisoned".into(),
            },
            "get_status" => serde_json::json!({
                "synth": self.surge_status,
                "midi_in": self.hw_status,
                "octave": self.octave,
                "rack_side": match self.side { Side::Front => "front", Side::Back => "back" },
                "playing": self.playing && !self.counting_in,
                "counting_in": self.counting_in,
                "count_in_beat": self.count_in_beat,
                "count_in_enabled": self.count_in_enabled,
                "count_in_bars": self.count_in_bars,
                "record_sync": self.record_sync,
                "bpm": self.bpm,
                "cables": self.cables.len(),
                "fps": self.fps,
                "buffer_frames": crate::bridge::BUFFER_FRAMES,
                "buffer_latency_ms": crate::bridge::BUFFER_FRAMES as f64 / 48000.0 * 1000.0,
                "xruns": crate::bridge::XRUNS.load(std::sync::atomic::Ordering::Relaxed),
                "midi_latency_ms": crate::bridge::MIDI_LATENCY_US.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
                "layout": {
                    "stage_h": self.stage_rect.height(), "unit_h": self.unit_h, "rows": self.layout_rows,
                    "rack_w": self.rack_w, "rack_h": self.rack_h,
                    "overflow": [self.rack_overflow.x, self.rack_overflow.y],
                    "assistant_h": if self.assistant_open() { self.assistant_rect.height() } else { 0.0 },
                },
                "meter": serde_json::to_value(self.master_meter).unwrap_or(serde_json::json!({})),
                "audio_in": self.units.iter().filter(|u| matches!(u.node_kind, UnitKind::AudioIn)).map(|u| serde_json::json!({
                    "node": u.node_id,
                    "device": u.device,
                    "channels": u.channels.label(),
                    "level": self.audio_in_level(u.node_id),
                })).collect::<Vec<_>>(),
            })
            .to_string(),
            "measure_mix" => {
                if args.get("reset").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(bus) = &self.bus {
                        let _ = bus.lock().unwrap().send(Command::ResetMeter);
                    }
                }
                serde_json::to_value(self.master_meter)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|e| format!("meter: {e}"))
            }
            "play_note" => {
                let note = args.get("note").and_then(|v| v.as_u64()).unwrap_or(60).min(127);
                let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(500).min(10000);
                let semi = note as usize;
                self.current_source = SourceId::Keys;
                self.fire_keys(semi, 0.8);
                self.note_offs.push((
                    std::time::Instant::now() + std::time::Duration::from_millis(ms),
                    semi,
                ));
                if let Some(k) = self.keys_unit.and_then(|u| self.units.get(u)).map(|u| u.node_id) {
                    self.agent_activity.record(origin, format!("Play note {note}"), vec![k], now);
                }
                format!("played note {note} for {ms} ms")
            }
            "set_octave" => {
                let oct = args.get("octave").and_then(|v| v.as_u64()).unwrap_or(4).clamp(1, 7) as u8;
                self.octave = oct;
                if let Some(k) = self.keys_unit.and_then(|u| self.units.get(u)).map(|u| u.node_id) {
                    self.agent_activity.record(origin, format!("Octave C{oct}"), vec![k], now);
                }
                format!("octave set to C{oct}")
            }
            "flip_rack" => {
                self.start_flip();
                self.agent_activity.record(origin, "Flip rack", vec![], now);
                "rack flipped".into()
            }
            "set_rig_source" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let flag = |k: &str| args.get(k).and_then(|v| v.as_bool());
                // `keys`/`laptop` are the pre-unification spellings: keyboard = keys || laptop.
                let keyboard = flag("keyboard").unwrap_or_else(|| match (flag("keys"), flag("laptop")) {
                    (None, None) => true,
                    (k, l) => k.unwrap_or(false) || l.unwrap_or(false),
                });
                let hardware = flag("hardware").unwrap_or(true);
                let rig = self.rig_of_node(node);
                self.rig_sources[rig] = (keyboard, hardware);
                self.send_rig_source(node);
                self.agent_activity.touch_cell(SourceId::Keys, node, now);
                self.agent_activity.record(origin, format!("Sources → {}", self.unit_display_name(node)), vec![node], now);
                format!("rig {node} sources: keyboard={keyboard} hardware={hardware}")
            }
            "set_split" => {
                let point = args.get("point").and_then(|v| v.as_u64()).map(|p| p.min(127) as u8);
                // Default: the Keys node in view (each rig has its own).
                let unit = match args.get("node").and_then(|v| v.as_u64()) {
                    Some(n) => self.units.iter().position(|u| u.node_id == n as u32),
                    None => self.keys_unit,
                };
                let Some(unit) = unit.filter(|&u| matches!(self.units[u].node_kind, UnitKind::Keys)) else {
                    return "set_split needs a Keys node (get_graph kind Keys) — only keyboards split".into();
                };
                let node = self.units[unit].node_id;
                self.set_keys_split(unit, point);
                if self.graph_pub.is_some() {
                    self.pending_agent_origin = Some((origin, now + 1.0));
                }
                let summary = match point {
                    Some(p) => format!("Split Keys at {}", note_name(p)),
                    None => "Unsplit Keys".into(),
                };
                self.agent_activity.record(origin, summary, vec![node], now);
                match point {
                    Some(p) => format!("keys node {node} split at note {p} ({}): port 0 = LOW (below), port 1 = HIGH (at/above) — wire HIGH with connect from_port 1", note_name(p)),
                    None => format!("keys node {node} unsplit — one MIDI OUT (port 0), HIGH cables removed"),
                }
            }
            "save_session" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled");
                match self.capture_session(name) {
                    Ok(s) => {
                        let path = crate::session::Session::package_path(&Self::sessions_dir(), name);
                        match path.and_then(|p| s.save(&p).map(|_| p)) {
                            Ok(p) => format!("saved session '{}' to {}", name, p.display()),
                            Err(e) => format!("save failed: {e}"),
                        }
                    }
                    Err(e) => format!("capture failed: {e}"),
                }
            }
            "load_session" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled");
                let path = crate::session::Session::package_path(&Self::sessions_dir(), name);
                match path.and_then(|p| crate::session::Session::load(&p)) {
                    Ok(s) => match self.apply_session(&s) {
                        Ok(_) => format!("loaded session '{}' — knobs + plugin state restored", name),
                        Err(e) => format!("apply failed: {e}"),
                    },
                    Err(e) => format!("load failed: {e}"),
                }
            }
            "list_sessions" => {
                let names = Self::list_saved_sessions(&Self::sessions_dir());
                if names.is_empty() {
                    "no saved sessions yet".into()
                } else {
                    format!("sessions: {}", names.join(", "))
                }
            }
            "system_audio" => {
                let gain = args.get("gain").and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetSystemAudio { gain });
                }
                if gain > 0.0 {
                    format!("system audio mixed in at gain {gain:.2} — captures the default output monitor (e.g. YouTube). Beware feedback if kabelsalat plays to the same output.")
                } else {
                    "system audio muted".into()
                }
            }
            "list_audio_outputs" => {
                let st = self.audio_out.lock().map(|s| s.clone()).unwrap_or_default();
                let wanted = self.wanted_output.lock().ok().and_then(|w| w.clone());
                serde_json::json!({
                    "outputs": st.outputs.iter().map(|o| serde_json::json!({"name": o.raw, "label": o.name, "default": o.default})).collect::<Vec<_>>(),
                    "current": st.current,
                    "selected": wanted,
                }).to_string()
            }
            "set_audio_output" => {
                let out = args.get("output").and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
                let known = self.audio_out.lock().map(|s| s.outputs.clone()).unwrap_or_default();
                if let Some(o) = &out {
                    if !known.iter().any(|k| &k.raw == o) {
                        return format!("no output named '{o}' (see list_audio_outputs)");
                    }
                }
                self.set_audio_output(out.clone());
                match out {
                    Some(o) => format!("kabelsalat plays to {o} (moved within a second; remembered)"),
                    None => "kabelsalat follows the system default output".into(),
                }
            }
            "list_app_streams" => {
                let streams = self.app_streams_shown();
                serde_json::json!({"streams": streams.iter().map(|s| serde_json::json!({
                    "index": s.index, "app": s.app, "title": s.title, "volume": s.volume, "muted": s.muted,
                })).collect::<Vec<_>>()}).to_string()
            }
            "set_app_stream" => {
                let Some(index) = args.get("index").and_then(|v| v.as_u64()) else {
                    return "set_app_stream needs 'index' (see list_app_streams)".into();
                };
                if !self.app_streams_shown().iter().any(|s| s.index == index) {
                    return format!("no playing app stream {index} (see list_app_streams)");
                }
                let volume = args.get("volume").and_then(|v| v.as_f64()).map(|v| v as f32);
                let muted = args.get("muted").and_then(|v| v.as_bool());
                self.set_app_stream(index, volume, muted);
                format!("app stream {index}: volume {volume:?} muted {muted:?}")
            }
            "list_audio_devices" => {
                let devs: Vec<crate::audio_in::InputDevice> = self.input_devices.lock().map(|d| d.clone()).unwrap_or_default();
                serde_json::json!({"devices": devs.iter().map(|d| serde_json::json!({
                    "name": d.name, "raw": d.raw, "channels": d.channels, "default": d.default,
                })).collect::<Vec<_>>()}).to_string()
            }
            "set_audio_in" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(u32::MAX as u64) as u32;
                let Some(unit) = self.units.iter().position(|u| u.node_id == node && matches!(u.node_kind, UnitKind::AudioIn)) else {
                    return format!("no Audio In node with id {node} (add one with add_node kind audio_in, then get_graph)");
                };
                // device: present + non-empty = select; present + null/empty = default.
                let device = args.get("device").map(|v| v.as_str().map(String::from)).unwrap_or_else(|| self.units[unit].device.clone());
                let channels = args.get("channels").and_then(|v| v.as_str()).map(parse_audio_channel);
                if let Some(gain) = args.get("gain").and_then(|v| v.as_f64()) {
                    let g = gain.clamp(0.0, 1.0);
                    if let Some(k) = self.units[unit].knobs.iter_mut().find(|k| k.param == Some(P_AUDIO_IN_GAIN)) {
                        k.value = g as f32;
                    }
                    if let Some(bus) = &self.bus {
                        let _ = bus.lock().unwrap().send(Command::SetParam { node, param: P_AUDIO_IN_GAIN, value: g });
                    }
                }
                if let Some(mon) = args.get("monitor").and_then(|v| v.as_bool()) {
                    self.set_audio_in_monitor(unit, mon);
                }
                self.set_audio_in_config(unit, device.filter(|d| !d.is_empty()), channels);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                self.agent_activity.record(origin, format!("Audio In → {}", sentence_case(&self.units[unit].name)), vec![node], now);
                let dev = self.units[unit].device.as_deref().unwrap_or("DEFAULT");
                format!("audio in node {node}: device={dev}, channel={}", self.units[unit].channels.label())
            }
            "play_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let gain = args.get("gain").and_then(|v| v.as_f64()).unwrap_or(0.8).clamp(0.0, 1.0) as f32;
                if path.is_empty() {
                    return "play_file needs a path to a WAV file (16/24-bit PCM or 32-bit float)".into();
                }
                if !std::path::Path::new(path).is_file() {
                    return format!("play_file: no file at '{path}'");
                }
                if let Some(pc) = &self.player_ctrl {
                    pc.play(path);
                    if let Some(bus) = &self.bus {
                        let _ = bus.lock().unwrap().send(Command::SetPlayer { gain });
                    }
                    format!("playing '{path}' at gain {gain:.2} (backing track mixed to master)")
                } else {
                    "file player unavailable (no audio)".into()
                }
            }
            "stop_file" => {
                if let Some(pc) = &self.player_ctrl {
                    pc.stop();
                }
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetPlayer { gain: 0.0 });
                }
                "file player stopped".into()
            }
            "set_player" => {
                let gain = args.get("gain").and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetPlayer { gain });
                }
                format!("file player gain {gain:.2}")
            }
            "set_rig_device" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let device = args.get("device").and_then(|v| v.as_u64()).map(|d| d as u32);
                let rig = self.rig_of_node(node);
                self.rig_device[rig] = device;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetRigDevice { node, device });
                }
                if let Some(d) = device {
                    self.agent_activity.touch_cell(SourceId::Hardware(d), node, now);
                }
                self.agent_activity.record(origin, format!("Device → {}", self.unit_display_name(node)), vec![node], now);
                match device {
                    Some(d) => format!("rig {node} now listens only to hardware device {d}"),
                    None => format!("rig {node} listens to any hardware device"),
                }
            }
            "get_matrix" => {
                let sinks: Vec<serde_json::Value> = self.matrix_sinks().iter().map(|&s| {
                    let u = &self.units[s];
                    let gate = self.midi_cable_to(s).is_some();
                    let is_clap = matches!(u.node_kind, UnitKind::Clap);
                    let rig = self.rig_of_unit(s).unwrap_or(0);
                    let src = self.rig_sources.get(rig).copied().unwrap_or((true, true));
                    let dev = self.rig_device.get(rig).copied().flatten();
                    let cell = |row: MatrixRow| if is_clap { clap_cell(src, dev, gate, row) } else { gate };
                    serde_json::json!({
                        "node": u.node_id, "name": u.name, "rig": u.rig,
                        "kind": if is_clap { "clap" } else { "lv2" },
                        "midi_cable": gate,
                        "keys": cell(MatrixRow::Keys),
                        "hw_all": cell(MatrixRow::HwAll),
                        "hw_devices": (0..self.hw_devices.len())
                            .map(|d| cell(MatrixRow::HwDev(d as u32))).collect::<Vec<_>>(),
                    })
                }).collect();
                serde_json::json!({
                    "sources": ["keys", "hw_all", "hw:<device index>"],
                    "keys": "the computer keyboard: on-screen clicks and laptop typing are one source",
                    "hw_devices": self.hw_devices,
                    "split": self.split_report(),
                    "sinks": sinks,
                    "rule_edits": self.rule_edits.iter().map(|((src, node), e)| {
                        serde_json::json!({
                            "source": format!("{src:?}"), "node": node,
                            "channel": e.channel, "transpose": e.transpose,
                            "key_range": [e.key_lo, e.key_hi], "vel_range": [e.vel_lo, e.vel_hi],
                            "enabled": e.enabled,
                            "cc_map": e.cc_map.iter().map(|m| serde_json::json!({"cc": m.cc, "param": m.param, "target": m.target.as_str()})).collect::<Vec<_>>(),
                        })
                    }).collect::<Vec<_>>(),
                    "cc_learn": self.cc_learn.as_ref().map(|l| serde_json::json!({
                        "source": format!("{:?}", l.source), "node": l.dest, "param": l.param, "target": l.target.as_str(),
                    })),
                }).to_string()
            }
            "set_matrix_cell" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("keys");
                let on = args.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
                let Some(inst) = self.units.iter().position(|u| u.node_id == node) else {
                    return format!("no unit with node id {node}");
                };
                let row = match source {
                    // "laptop" is the legacy spelling of the one keyboard row.
                    "keys" | "keyboard" | "laptop" => MatrixRow::Keys,
                    "hw_all" => MatrixRow::HwAll,
                    s if s.starts_with("hw:") => match s[3..].parse::<u32>() {
                        Ok(d) => MatrixRow::HwDev(d),
                        Err(_) => return format!("bad source '{source}'"),
                    },
                    _ => return format!("unknown source '{source}' (keys|hw_all|hw:<d>)"),
                };
                self.matrix_toggle(inst, row, on);
                if let Some(src) = row.source() {
                    self.agent_activity.touch_cell(src, node, now);
                }
                if let Some(ci) = self.midi_cable_to(inst) {
                    self.agent_activity.touch_edge(self.cables[ci].id, now);
                }
                self.agent_activity.record(origin, format!("Matrix {source} → {}", self.unit_display_name(node)), vec![node], now);
                format!("matrix cell {source} → node {node} = {on}")
            }
            "set_matrix_rule" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("keys");
                if !self.units.iter().any(|u| u.node_id == node) {
                    return format!("no unit with node id {node}");
                }
                let src = match self.parse_rule_source(source) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let mut e = RuleEdit::default();
                if let Some(prev) = self.rule_edits.get(&(src, node)) {
                    e.cc_map = prev.cc_map.clone();
                }
                if let Some(ch) = args.get("channel").and_then(|v| v.as_u64()) { e.channel = Some(ch.min(15) as u8); }
                if let Some(t) = args.get("transpose").and_then(|v| v.as_i64()) { e.transpose = t.clamp(-24, 24) as i8; }
                if let Some(v) = args.get("key_lo").and_then(|v| v.as_u64()) { e.key_lo = v.min(127) as u8; }
                if let Some(v) = args.get("key_hi").and_then(|v| v.as_u64()) { e.key_hi = v.min(127) as u8; }
                if let Some(v) = args.get("vel_lo").and_then(|v| v.as_u64()) { e.vel_lo = v.min(127) as u8; }
                if let Some(v) = args.get("vel_hi").and_then(|v| v.as_u64()) { e.vel_hi = v.min(127) as u8; }
                if let Some(v) = args.get("enabled").and_then(|v| v.as_bool()) { e.enabled = v; }
                let summary = e.summary();
                let cmd = e.to_command(src, node);
                if e.is_default() { self.rule_edits.remove(&(src, node)); }
                else { self.rule_edits.insert((src, node), e); }
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(cmd);
                    if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                }
                self.agent_activity.touch_cell(src, node, now);
                self.agent_activity.record(origin, format!("Rule → {}: {summary}", self.unit_display_name(node)), vec![node], now);
                format!("rule {source} → node {node}: {summary}")
            }
            "set_cc_map" => {
                let Some(node) = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32) else {
                    return "set_cc_map needs 'node'".into();
                };
                if !self.units.iter().any(|u| u.node_id == node) {
                    return format!("no unit with node id {node}");
                }
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("keys");
                let src = match self.parse_rule_source(source) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let Some(cc) = args.get("cc").and_then(|v| v.as_u64()).filter(|&c| c <= 127).map(|c| c as u8) else {
                    return "set_cc_map needs 'cc' (0..=127)".into();
                };
                let mut target = match Self::parse_cc_target(args.get("target").and_then(|v| v.as_str())) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let param = match args.get("knob").and_then(|v| v.as_str()) {
                    Some(k) => match self.resolve_unit_knob(node, k) {
                        Ok((p, t)) => { target = t; Some(p) }
                        Err(e) => return e,
                    },
                    None => args.get("param").and_then(|v| v.as_u64()).map(|p| p as u32),
                };
                self.set_cc_binding(src, node, cc, param, target);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                self.agent_activity.touch_cell(src, node, now);
                self.agent_activity.record(origin, format!("CC {cc} → {}", self.unit_display_name(node)), vec![node], now);
                let map: Vec<_> = self.rule_edits.get(&(src, node))
                    .map(|e| e.cc_map.iter().map(|m| serde_json::json!({"cc": m.cc, "param": m.param, "target": m.target.as_str()})).collect())
                    .unwrap_or_default();
                serde_json::json!({"node": node, "source": source, "cc": cc, "param": param, "target": target.as_str(), "cc_map": map}).to_string()
            }
            "clear_cc_map" => {
                let Some(node) = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32) else {
                    return "clear_cc_map needs 'node'".into();
                };
                if !self.units.iter().any(|u| u.node_id == node) {
                    return format!("no unit with node id {node}");
                }
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("keys");
                let src = match self.parse_rule_source(source) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let target_filter = match args.get("target").and_then(|v| v.as_str()) {
                    Some(t) => match Self::parse_cc_target(Some(t)) {
                        Ok(tt) => Some(tt),
                        Err(e) => return e,
                    },
                    None => None,
                };
                let knob_filter = match args.get("knob").and_then(|v| v.as_str()) {
                    Some(k) => match self.resolve_unit_knob(node, k) {
                        Ok(pair) => Some(pair),
                        Err(e) => return e,
                    },
                    None => None,
                };
                if let Some(cc) = args.get("cc").and_then(|v| v.as_u64()) {
                    if cc > 127 { return "cc must be 0..=127".into(); }
                    self.set_cc_binding(src, node, cc as u8, None, crate::matrix::CcTarget::PluginParam);
                } else {
                    let ccs: Vec<(u8, crate::matrix::CcTarget)> = self.rule_edits.get(&(src, node))
                        .map(|e| {
                            e.cc_map.iter()
                                .filter(|m| {
                                    knob_filter.map_or(true, |(kp, kt)| m.param == kp && m.target == kt)
                                        && target_filter.map_or(true, |tf| m.target == tf)
                                })
                                .map(|m| (m.cc, m.target))
                                .collect()
                        })
                        .unwrap_or_default();
                    for (cc, tgt) in ccs {
                        self.set_cc_binding(src, node, cc, None, tgt);
                    }
                }
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                self.agent_activity.touch_cell(src, node, now);
                self.agent_activity.record(origin, format!("Clear CC → {}", self.unit_display_name(node)), vec![node], now);
                let map: Vec<_> = self.rule_edits.get(&(src, node))
                    .map(|e| e.cc_map.iter().map(|m| serde_json::json!({"cc": m.cc, "param": m.param, "target": m.target.as_str()})).collect())
                    .unwrap_or_default();
                serde_json::json!({"node": node, "source": source, "cc_map": map}).to_string()
            }
            "list_cc_maps" => {
                let filter_node = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32);
                let filter_src = match args.get("source").and_then(|v| v.as_str()) {
                    Some(s) => match self.parse_rule_source(s) {
                        Ok(src) => Some(src),
                        Err(e) => return e,
                    },
                    None => None,
                };
                let keys: Vec<(SourceId, u32)> = self.rule_edits.iter()
                    .filter(|((src, node), e)| {
                        !e.cc_map.is_empty()
                            && filter_node.map_or(true, |n| n == *node)
                            && filter_src.map_or(true, |s| s == *src)
                    })
                    .map(|(k, _)| *k)
                    .collect();
                let mut entries = Vec::with_capacity(keys.len());
                for (src, node) in keys {
                    let choices = self.node_param_choices(node);
                    if let Some(e) = self.rule_edits.get(&(src, node)) {
                        let bindings: Vec<_> = e.cc_map.iter().map(|m| {
                            let name = match m.target {
                                crate::matrix::CcTarget::Knob => self.knob_param_label(node, m.param),
                                crate::matrix::CcTarget::PluginParam => choices.iter().find(|(id, _)| *id == m.param).map(|(_, n)| n.clone()),
                            };
                            serde_json::json!({"cc": m.cc, "param": m.param, "target": m.target.as_str(), "name": name})
                        }).collect();
                        entries.push(serde_json::json!({
                            "source": Self::source_str(src),
                            "node": node,
                            "bindings": bindings,
                        }));
                    }
                }
                serde_json::json!({
                    "cc_maps": entries,
                    "cc_learn": self.cc_learn.as_ref().map(|l| serde_json::json!({
                        "source": Self::source_str(l.source), "node": l.dest, "param": l.param, "target": l.target.as_str(),
                    })),
                }).to_string()
            }
            "list_params" => {
                let Some(node) = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32) else {
                    return "list_params needs 'node'".into();
                };
                if !self.units.iter().any(|u| u.node_id == node) {
                    return format!("no unit with node id {node}");
                }
                let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let words: Vec<&str> = q.split_whitespace().collect();
                let choices = self.node_param_choices(node);
                let total = choices.len();
                let params: Vec<_> = choices.into_iter()
                    .filter(|(_, name)| {
                        let low = name.to_lowercase();
                        words.iter().all(|w| low.contains(w))
                    })
                    .take(50)
                    .map(|(id, name)| serde_json::json!({"id": id, "name": name}))
                    .collect();
                let knobs: Vec<_> = self.units.iter()
                    .find(|u| u.node_id == node)
                    .map(|u| {
                        u.knobs.iter()
                            .filter_map(|k| k.cc_target().map(|(id, tgt)| {
                                serde_json::json!({"label": k.label, "param": id, "target": tgt.as_str()})
                            }))
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({"node": node, "total": total, "params": params, "knobs": knobs}).to_string()
            }
            "learn_cc" => {
                if args.get("cancel").and_then(|v| v.as_bool()) == Some(true) {
                    self.cc_learn = None;
                    return serde_json::json!({"armed": false}).to_string();
                }
                let Some(node) = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32) else {
                    return "learn_cc needs 'node'".into();
                };
                if !self.units.iter().any(|u| u.node_id == node) {
                    return format!("no unit with node id {node}");
                }
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("keys");
                let src = match self.parse_rule_source(source) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let mut target = match Self::parse_cc_target(args.get("target").and_then(|v| v.as_str())) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let param = match args.get("knob").and_then(|v| v.as_str()) {
                    Some(k) => match self.resolve_unit_knob(node, k) {
                        Ok((p, t)) => { target = t; p }
                        Err(e) => return e,
                    },
                    None => match args.get("param").and_then(|v| v.as_u64()).map(|p| p as u32) {
                        Some(p) => p,
                        None => return "learn_cc needs 'param' or 'knob'".into(),
                    },
                };
                let replace_existing = target == crate::matrix::CcTarget::Knob || args.get("knob").is_some();
                self.cc_learn = Some(CcLearn { source: src, dest: node, param, target, replace_existing });
                serde_json::json!({"armed": true, "node": node, "source": Self::source_str(src), "param": param, "target": target.as_str()}).to_string()
            }
            "set_rig_mix" => {
                let rig = args.get("rig").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let n_rigs = self.rig_count();
                if rig >= n_rigs { return format!("rig {rig} out of range ({n_rigs} rigs)"); }
                let m = &mut self.rig_mix[rig.min(7)];
                if let Some(v) = args.get("vol").and_then(|v| v.as_f64()) { m.vol = v.clamp(0.0, 1.0) as f32; }
                if let Some(v) = args.get("pan").and_then(|v| v.as_f64()) { m.pan = v.clamp(-1.0, 1.0) as f32; }
                if let Some(v) = args.get("mute").and_then(|v| v.as_bool()) { m.mute = v; }
                if let Some(v) = args.get("solo").and_then(|v| v.as_bool()) { m.solo = v; }
                self.send_rig_mix(rig);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let m = self.rig_mix[rig.min(7)];
                let nodes: Vec<u32> = self.units.iter()
                    .filter(|u| n_rigs <= 1 || u.rig as usize == rig)
                    .map(|u| u.node_id)
                    .collect();
                self.agent_activity.touch_rig(rig, now);
                self.agent_activity.record(origin, format!("Mix → {} (vol {:.2})", self.rig_name(rig), m.vol), nodes, now);
                format!("rig {rig}: vol {:.2} pan {:.2} mute {} solo {}", m.vol, m.pan, m.mute, m.solo)
            }
            "list_plugins" => {
                if !self.plugin_lists_ready() {
                    return "scanning installed plugins in the background (first run) — ask again in a few seconds".into();
                }
                let clap = self.plugins_clap.clone();
                let lv2 = self.plugins_lv2.clone();
                let mut s = format!("CLAP plugins ({}):\n", clap.len());
                for p in clap.iter().take(20) {
                    s.push_str(&format!("  {} [{}] {}\n", p.name, p.id, p.features.join(",")));
                }
                s.push_str(&format!("LV2 plugins ({}):\n", lv2.len()));
                for p in lv2.iter().take(30) {
                    let kind = if p.is_instrument { " [instrument]" } else { "" };
                    s.push_str(&format!("  {}{} — {}\n", p.name, kind, p.uri));
                }
                s
            }
            "list_instruments" => {
                let installing_id = self.install.as_ref().map(|i| i.id.clone());
                let entries: Vec<serde_json::Value> = crate::catalog::CATALOG
                    .iter()
                    .map(|e| {
                        let inst = self.catalog_installed(e);
                        let (route, pkg) = match e.route {
                            crate::catalog::InstallRoute::DistroPackage { pkg } => ("distro", Some(pkg)),
                            crate::catalog::InstallRoute::Source { repo, .. } => ("source", Some(repo)),
                        };
                        serde_json::json!({
                            "id": e.id,
                            "name": e.name,
                            "format": e.format.label(),
                            "description": e.description,
                            "repo": e.repo,
                            "route": route,
                            "pkg_or_repo": pkg,
                            "installed": inst,
                            "installing": installing_id.as_deref() == Some(e.id),
                            "url": crate::catalog::repo_url(e.repo),
                        })
                    })
                    .collect();
                serde_json::json!({ "instruments": entries }).to_string()
            }
            "install_instrument" => {
                let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    return "install_instrument needs 'id' (from list_instruments)".into();
                }
                if args.get("confirm").and_then(|v| v.as_bool()) != Some(true) {
                    return format!("not installed: installing '{id}' changes the system — ask the user first, then call install_instrument with confirm=true (id '{id}')");
                }
                match self.start_install(id) {
                    Ok(()) => {
                        self.plugin_msg = format!("Installing '{id}' — see the log below");
                        format!("installing '{id}' (see the Plugins view for the live log)")
                    }
                    Err(e) => format!("install failed to start: {e}"),
                }
            }
            "transport_play" => {
                if !self.playing {
                    self.playing = true;
                    self.time = 0.0;
                    if self.graph_pub.is_none() && self.count_in_enabled {
                        self.counting_in = true;
                        self.count_in_beat = 1;
                    }
                    self.master_transport(true);
                }
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let nodes: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, "Transport play", nodes, now);
                if self.count_in_enabled {
                    format!("master transport count-in started ({} bar)", self.count_in_bars)
                } else {
                    "master transport started (every metronome plays)".into()
                }
            }
            "transport_stop" => {
                self.playing = false;
                self.counting_in = false;
                self.count_in_beat = 0;
                self.master_transport(false);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let nodes: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, "Transport stop", nodes, now);
                "master transport stopped (every metronome stops)".into()
            }
            "record_start" => {
                if self.recorder.is_none() || self.bus.is_none() {
                    return "recording unavailable: no audio engine (recorder/bus missing)".into();
                }
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::RecordStart);
                }
                let masters: Vec<u32> = self.units.iter().filter(|u| matches!(u.node_kind, UnitKind::Master)).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, "Record start", masters, now);
                "recording started (master output → ~/Music/kabelsalat/)".into()
            }
            "record_stop" => {
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::RecordStop);
                }
                let masters: Vec<u32> = self.units.iter().filter(|u| matches!(u.node_kind, UnitKind::Master)).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, "Record stop", masters, now);
                "recording stopped".into()
            }
            "record_status" => match &self.recorder {
                Some(r) => serde_json::to_value(r.snapshot())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|e| format!("record status: {e}")),
                None => "{\"recording\":false,\"available\":false}".into(),
            },
            "set_record_sync" => {
                let on = args.get("on").and_then(|v| v.as_bool()).unwrap_or(!self.record_sync);
                self.record_sync = on;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetRecordSync { on });
                    if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                }
                self.agent_activity.record(origin, format!("Record sync = {on}"), vec![], now);
                serde_json::json!({"record_sync": self.record_sync}).to_string()
            }
            "set_node_playing" => {
                let Some(node) = args.get("node").and_then(|v| v.as_u64()) else {
                    return "set_node_playing needs 'node' (id from get_graph)".into();
                };
                let playing = args.get("playing").and_then(|v| v.as_bool()).unwrap_or(true);
                if self.set_node_playing(node as u32, playing) {
                    if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                    let nid = node as u32;
                    let uname = self.unit_display_name(nid);
                    self.agent_activity.record(origin, format!("{uname} {}", if playing { "play" } else { "stop" }), vec![nid], now);
                    format!("node {node} {}", if playing { "playing" } else { "stopped" })
                } else {
                    format!("node {node} has no transport (only metronomes start/stop) or does not exist")
                }
            }
            "set_bpm" => {
                let bpm = args.get("bpm").and_then(|v| v.as_f64()).unwrap_or(96.0).clamp(20.0, 300.0) as f32;
                self.set_bpm(bpm);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let nodes: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, format!("Tempo {bpm:.0} BPM"), nodes, now);
                format!("tempo set to {bpm:.0} BPM")
            }
            "set_count_in" => {
                let bars_arg = args.get("bars").and_then(|v| v.as_u64()).map(|b| b.min(255) as u8);
                let enabled_arg = args.get("enabled").and_then(|v| v.as_bool());
                let (enabled, bars) = match bars_arg {
                    Some(0) => (false, self.count_in_bars),
                    Some(b) => (enabled_arg.unwrap_or(true), b.clamp(1, crate::transport::MAX_COUNT_IN_BARS)),
                    None => (enabled_arg.unwrap_or(!self.count_in_enabled), self.count_in_bars),
                };
                self.set_count_in(enabled, bars);
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let nodes: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
                self.agent_activity.record(origin, format!("Count-in {enabled} ({bars}b)"), nodes, now);
                serde_json::json!({"enabled": self.count_in_enabled, "bars": self.count_in_bars}).to_string()
            }
            "load_template" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("demo").to_lowercase();
                let res = match name.as_str() {
                    "lead_reverb" | "lead" => { self.load_template(Template::LeadReverb); Some("Loaded template 'Lead + Reverb'".to_string()) }
                    "drums_bass" | "drums" => { self.load_template(Template::DrumsBass); Some("Loaded template 'Drums + Bass'".to_string()) }
                    "ambient_pad" | "ambient" | "pad" => { self.load_template(Template::AmbientPad); Some("Loaded template 'Ambient Pad'".to_string()) }
                    "demo" => { self.load_demo_patch(); Some("Loaded the demo patch".to_string()) }
                    "multi_rig" | "rigs" => { self.load_multi_rig_demo(); Some("Loaded the multi-rig demo (2 rigs) — switch with set_active_rig or 1–9".to_string()) }
                    _ => None,
                };
                match res {
                    Some(msg) => {
                        let nodes: Vec<u32> = self.units.iter().map(|u| u.node_id).collect();
                        self.agent_activity.record(origin, format!("Template: {name}"), nodes, now);
                        msg
                    }
                    None => format!("unknown template '{name}' (use lead_reverb, drums_bass, ambient_pad, demo, multi_rig)"),
                }
            }
            "add_node" => {
                let kind_str = args.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let name = name_bytes(args.get("name").and_then(|v| v.as_str()).unwrap_or(&kind_str));
                let kind = match kind_str.as_str() {
                    "keys" => Some(NodeKind::Keys),
                    "clap" => Some(NodeKind::Clap { plugin_id: name_bytes(args.get("plugin_id").and_then(|v| v.as_str()).unwrap_or("org.surge-synth-team.surge-xt")) }),
                    "dsp" => match args.get("dsp").and_then(|v| v.as_str()).unwrap_or("").to_lowercase().as_str() {
                        "gain" => Some(NodeKind::Dsp(DspKind::Gain)),
                        "pan" => Some(NodeKind::Dsp(DspKind::Pan)),
                        "eq3" => Some(NodeKind::Dsp(DspKind::Eq3)),
                        "filter" => Some(NodeKind::Dsp(DspKind::Filter)),
                        "limiter" => Some(NodeKind::Dsp(DspKind::Limiter)),
                        "saturation" => Some(NodeKind::Dsp(DspKind::Saturation)),
                        "delay" => Some(NodeKind::Dsp(DspKind::Delay)),
                        "reverb" => Some(NodeKind::Dsp(DspKind::Reverb)),
                        "metronome" => Some(NodeKind::Dsp(DspKind::Metronome)),
                        _ => None,
                    },
                    "lv2" => args.get("uri").and_then(|v| v.as_str()).map(|u| NodeKind::Lv2 { uri: Lv2Uri::new(u) }),
                    "audio_in" => Some(NodeKind::AudioIn {
                        device: args.get("device").and_then(|v| v.as_str()).map(String::from),
                        channels: parse_audio_channel(args.get("channels").and_then(|v| v.as_str()).unwrap_or("")),
                    }),
                    "system_audio" => Some(NodeKind::SystemAudio),
                    "master_out" => Some(NodeKind::MasterOut),
                    _ => None,
                };
                // Global: any rig by index; default = the rig in view.
                let n_rigs = self.rig_count();
                let rig = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize).unwrap_or(self.active_rig);
                if rig >= n_rigs {
                    return format!("no rig {rig}: there are {n_rigs} (0..{}) — add one with add_rig", n_rigs - 1);
                }
                match kind {
                    Some(k) => {
                        if let Some(bus) = &self.bus {
                            let _ = bus.lock().unwrap().send(Command::AddNode { kind: k, name, rig: rig as u8 });
                            if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                        }
                        self.agent_activity.touch_rig(rig, now);
                        self.agent_activity.record(origin, format!("Add {} → {}", name_str(&name), self.rig_name(rig)), vec![], now);
                        format!("added {kind_str} node to rig {rig} '{}' (wire it with connect)", self.rig_name(rig))
                    }
                    None => format!("invalid node kind/dsp/uri for '{kind_str}'"),
                }
            }
            "get_rigs" => self.rigs_json().to_string(),
            "list_sounds" => {
                let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let is_dexed = self.clap_pool_idx(self.active_rig)
                    .and_then(|i| self.clap_plugin_ids.get(i))
                    .map(|id| id == DEXED_ID)
                    .unwrap_or(false);
                if is_dexed {
                    let hits = crate::host::search_dexed_patches(self.dexed_patches(), q, 25);
                    if hits.is_empty() { format!("no Dexed patch matches '{q}'") } else { serde_json::json!({"patches": hits}).to_string() }
                } else {
                    let hits = crate::host::search_surge_patches(self.surge_patches(), q, 25);
                    if hits.is_empty() { format!("no Surge patch matches '{q}'") } else { serde_json::json!({"patches": hits}).to_string() }
                }
            }
            "load_sound" => {
                let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let rig = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize).unwrap_or(self.active_rig);
                match self.load_sound(rig, q) {
                    Ok(p) => {
                        let multi = self.rig_count() > 1;
                        let nodes: Vec<u32> = self.units.iter()
                            .filter(|u| matches!(u.node_kind, UnitKind::Clap) && (!multi || u.rig as usize == rig))
                            .map(|u| u.node_id)
                            .collect();
                        self.agent_activity.touch_rig(rig, now);
                        self.agent_activity.record(origin, format!("Sound → {p}"), nodes, now);
                        format!("loaded '{p}' into rig {rig} '{}'", self.rig_name(rig))
                    }
                    Err(e) => format!("load_sound failed: {e}"),
                }
            }
            "add_rig" => {
                let n = self.rig_count();
                if n >= Self::MAX_RIGS { return format!("at most {} rigs", Self::MAX_RIGS); }
                let name = args.get("name").and_then(|v| v.as_str());
                match args.get("plugin_id").and_then(|v| v.as_str()) {
                    Some(id) => {
                        if !self.plugin_lists_ready() {
                            return "scanning installed plugins in the background (first run) — try add_rig again in a few seconds".into();
                        }
                        let known = self.plugins_clap.iter().find(|p| p.id == id).map(|p| p.name.clone());
                        let Some(label) = known else {
                            return format!("add_rig failed: no installed CLAP plugin with id '{id}' (see list_plugins)");
                        };
                        self.edit_rigs_with(None, Some(name.unwrap_or(&label)), Some((id, &label)));
                    }
                    None => self.edit_rigs_named(None, name),
                }
                if self.rig_count() == n { return format!("add_rig failed: {}", self.session_msg); }
                // A Surge patch query means nothing to another plugin.
                let new_rig = self.active_rig;
                let sound_query = if args.get("plugin_id").is_some() { None } else { args.get("sound").and_then(|v| v.as_str()) };
                let sound = match sound_query {
                    Some(q) => match self.load_sound(new_rig, q) {
                        Ok(p) => format!(" Sound: {p}."),
                        Err(e) => format!(" Sound not loaded: {e}."),
                    },
                    None => String::new(),
                };
                let nodes: Vec<u32> = self.units.iter()
                    .filter(|u| u.rig as usize == new_rig)
                    .map(|u| u.node_id)
                    .collect();
                let edges: Vec<u32> = self.cables.iter()
                    .filter(|c| self.units.get(c.src).is_some_and(|u| u.rig as usize == new_rig))
                    .map(|c| c.id)
                    .collect();
                for eid in edges { self.agent_activity.touch_edge(eid, now); }
                self.agent_activity.touch_rig(new_rig, now);
                self.agent_activity.record(origin, format!("Add {}", self.rig_name(new_rig)), nodes, now);
                format!("{} — now in view.{sound} {}", self.session_msg, self.rigs_json())
            }
            "delete_rig" => {
                let n = self.rig_count();
                let Some(rig) = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize) else {
                    return "delete_rig needs rig (index from get_rigs)".into();
                };
                if args.get("confirm").and_then(|v| v.as_bool()) != Some(true) {
                    return format!("not deleted: ask the user first, then call delete_rig with confirm=true (rig {rig} '{}')", self.rig_name(rig));
                }
                if n <= 1 { return "not deleted: the last rig cannot be deleted".into(); }
                if rig >= n { return format!("no rig {rig}: there are {n}"); }
                let deleted_name = self.rig_name(rig);
                self.edit_rigs(Some(rig));
                self.agent_activity.record(origin, format!("Delete {deleted_name}"), vec![], now);
                format!("{}. {}", self.session_msg, self.rigs_json())
            }
            "rename_rig" => {
                let Some(rig) = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize) else {
                    return "rename_rig needs rig (index from get_rigs) and name".into();
                };
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                match self.rename_rig(rig, name) {
                    Ok(n) => {
                        self.agent_activity.touch_rig(rig, now);
                        self.agent_activity.record(origin, format!("Rename rig {} → {n}", rig + 1), vec![], now);
                        format!("rig {rig} is now '{n}'. {}", self.rigs_json())
                    }
                    Err(e) => e,
                }
            }
            "select_rig" => {
                let n = self.rig_count();
                let rig = args.get("rig").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if rig >= n { return format!("no rig {rig}: there are {n}"); }
                self.set_active_rig(rig);
                self.agent_activity.touch_rig(rig, now);
                self.agent_activity.record(origin, format!("View {}", self.rig_name(rig)), vec![], now);
                format!("showing rig {rig} '{}' (view only — every rig keeps sounding)", self.rig_name(rig))
            }
            "save_rig" => {
                let Some(name) = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    return "save_rig needs 'name'".into();
                };
                let rig = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize).unwrap_or(self.active_rig);
                let dir = self.rig_presets_dir.clone();
                match self.save_rig_preset_to_dir(&dir, rig, name) {
                    Ok(p) => {
                        self.agent_activity.touch_rig(rig, now);
                        self.agent_activity.record(origin, format!("Save rig → {name}"), vec![], now);
                        format!("saved rig {rig} '{}' as preset '{name}' to {}", self.rig_name(rig), p.display())
                    }
                    Err(e) => format!("save_rig failed: {e}"),
                }
            }
            "load_rig" => {
                let Some(name) = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    return "load_rig needs 'name'".into();
                };
                let target = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize);
                let dir = self.rig_presets_dir.clone();
                match self.load_rig_preset_from_dir(&dir, name, target) {
                    Ok(slot) => {
                        let nodes: Vec<u32> = self.units.iter().filter(|u| u.rig as usize == slot).map(|u| u.node_id).collect();
                        self.agent_activity.touch_rig(slot, now);
                        self.agent_activity.record(origin, format!("Load rig ← {name}"), nodes, now);
                        format!("loaded rig preset '{name}' into rig {slot} '{}' — knobs, cables, mix and synth state restored. {}", self.rig_name(slot), self.rigs_json())
                    }
                    Err(e) => format!("load_rig failed: {e}"),
                }
            }
            "list_rig_presets" => {
                let presets = crate::session::list_rig_presets(&self.rig_presets_dir);
                serde_json::json!({"presets": presets}).to_string()
            }
            "connect" => {
                let from_node = args.get("from_node").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let to_node = args.get("to_node").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let from_port = args.get("from_port").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                let to_port = args.get("to_port").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                let midi = args.get("kind").and_then(|v| v.as_str()).unwrap_or("audio").eq_ignore_ascii_case("midi");
                let kind = if midi { CPortKind::Midi } else { CPortKind::Audio };
                let id = self.shared_graph.lock().map(|g| g.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1).unwrap_or(1);
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::Connect { id, from: PortRef { node: from_node, port: from_port }, to: PortRef { node: to_node, port: to_port }, kind });
                    if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                }
                self.agent_activity.touch_edge(id, now);
                let sname = self.unit_display_name(from_node);
                let dname = self.unit_display_name(to_node);
                self.agent_activity.record(origin, format!("Wire {sname} → {dname}"), vec![from_node, to_node], now);
                format!("connected {from_node} -> {to_node} ({})", if midi { "midi" } else { "audio" })
            }
            "disconnect" => {
                let id = args.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let endpoints: Vec<u32> = self.cables.iter().find(|c| c.id == id)
                    .map(|c| {
                        let mut v = Vec::new();
                        if let Some(u) = self.units.get(c.src) { v.push(u.node_id); }
                        if let Some(u) = self.units.get(c.dst) { v.push(u.node_id); }
                        v
                    })
                    .unwrap_or_default();
                // Unplug it in the rack too: a cable the rack still shows is
                // re-sent to the engine every frame (publish_patch).
                match self.live_cable(id) {
                    Some(ci) => self.unplug_cable(ci, true),
                    None => if let Some(bus) = &self.bus {
                        let _ = bus.lock().unwrap().send(Command::Disconnect(id));
                    },
                }
                if self.bus.is_some() && self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                self.agent_activity.record(origin, format!("Unplug cable {id}"), endpoints, now);
                format!("disconnected cable {id}")
            }
            "remove_node" => {
                let id = args.get("node").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let uname = self.unit_display_name(id);
                let neighbors: Vec<u32> = self.cables.iter()
                    .filter(|c| c.dying < 0.0)
                    .filter_map(|c| {
                        let s = self.units.get(c.src).map(|u| u.node_id);
                        let d = self.units.get(c.dst).map(|u| u.node_id);
                        match (s, d) {
                            (Some(sn), Some(dn)) if sn == id && dn != id => Some(dn),
                            (Some(sn), Some(dn)) if dn == id && sn != id => Some(sn),
                            _ => None,
                        }
                    })
                    .collect();
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::RemoveNode(id));
                    if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                }
                self.agent_activity.record(origin, format!("Remove {uname}"), neighbors, now);
                format!("removed node {id} (and its cables)")
            }
            _ => format!("unknown tool: {name}"),
        }
    }

    /// Flip one of a rig's two input chips (0 = keyboard, 1 = hardware)
    /// and publish the (keyboard, hardware) pair.
    fn toggle_rig_source(&mut self, rig: usize, node: u32, idx: usize) {
        let Some(s) = self.rig_sources.get_mut(rig) else { return };
        if idx == 0 { s.0 = !s.0 } else { s.1 = !s.1 }
        self.send_rig_source(node);
    }

    /// The Keyboard matrix row's activity LED: on-screen clicks and laptop
    /// typing both light `key_leds`, so the row LED is their max.
    fn keyboard_led(&self) -> f32 {
        self.key_leds.iter().cloned().fold(0.0f32, f32::max)
    }

    fn send_rig_source(&self, node: u32) {
        let rig = self.rig_of_node(node);
        let (keyboard, hardware) = self.rig_sources[rig];
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetRigSource { node, keyboard, hardware });
        }
    }

    // -- native egui parameter editor (task-26) ------------------------------
    //
    // NO X11 child window. Double-clicking a Clap unit opens a scrollable,
    // searchable, module-grouped slider pane. Sliders drive the voice's
    // RT-safe VoiceController (ParamValueEvent at the next block); values are
    // formatted by the plugin itself via param_text. One pane at a time.

    /// Open/close a rig's native parameter editor. Double-clicking the same
    /// unit again closes it; opening another rig's editor replaces it.
    fn toggle_param_editor(&mut self, rig: usize) {
        if self.editor_open && self.lv2_editor_node.is_none() && self.editor_rig == rig {
            self.editor_open = false;
            return;
        }
        self.lv2_editor_node = None;
        self.editor_rig = rig;
        self.editor_filter.clear();
        let pool_idx = self.clap_pool_idx(rig);
        self.editor_params = pool_idx
            .and_then(|i| self._surge_main.get_mut(i))
            .map(|m| m.param_list())
            .unwrap_or_default();
        // Current sound = the patch named IN the synth's own state (correct
        // across rig edits and session loads, unlike a slot-keyed memo).
        let is_surge = pool_idx
            .and_then(|i| self.clap_plugin_ids.get(i))
            .map(|id| id == SURGE_ID)
            .unwrap_or(false);
        if is_surge {
            if let Some(name) = pool_idx
                .and_then(|i| self._surge_main.get_mut(i))
                .and_then(|m| m.save_state().ok())
                .and_then(|st| crate::host::patch_name_from_state(&st))
            {
                if let Some(n) = self.patch_names.get_mut(rig) { *n = Some(name); }
            }
        }
        self.editor_open = !self.editor_params.is_empty();
        self.editor_status = if self.editor_open {
            String::new()
        } else {
            "no synth loaded — editor unavailable".into()
        };
    }

    /// Open or close the native parameter & preset editor for an LV2 unit.
    fn toggle_lv2_editor(&mut self, node_id: u32, uri: &str, name: &str) {
        if self.editor_open && self.lv2_editor_node == Some(node_id) {
            self.editor_open = false;
            self.lv2_editor_node = None;
            return;
        }
        self.lv2_editor_node = Some(node_id);
        self.lv2_editor_name = sentence_case(name);
        self.lv2_editor_controls = crate::lv2::control_descs_for_uri(uri);
        self.lv2_editor_presets = crate::lv2::discover_presets(uri);
        self.lv2_preset_name = None;
        self.editor_filter.clear();
        self.sound_filter.clear();
        self.editor_open = true;
        self.editor_status.clear();
    }

    /// Re-read the open rig's parameter list (on open / Refresh button).
    fn refresh_editor_params(&mut self) {
        let rig = self.editor_rig;
        if let Some(idx) = self.clap_pool_idx(rig) {
            if let Some(main) = self._surge_main.get_mut(idx) {
                self.editor_params = main.param_list();
            }
        }
    }

    /// The native parameter editor window: search box + module-grouped
    /// sliders. Changes are collected and applied after rendering so the
    /// cached list and the RT-safe controller stay in sync.
    fn param_editor_window(&mut self, ctx: &egui::Context) {
        if !self.editor_open {
            return;
        }
        if let Some(lv2_node) = self.lv2_editor_node {
            let mut open = self.editor_open;
            let mut chosen_preset: Option<crate::lv2::Lv2Preset> = None;
            let mut ctrl_changes: Vec<(usize, f32, f32)> = Vec::new();
            egui::Window::new(egui::RichText::new(format!("{} — presets & parameters", self.lv2_editor_name))
                    .font(theme::medium_font(16.0)).color(theme::INK))
                .open(&mut open)
                .collapsible(false)
                .fixed_size([480.0, 700.0])
                .show(ctx, |ui| {
                    theme::section_label(ui, "Presets");
                    let current = self.lv2_preset_name.clone().unwrap_or_else(|| "Default".into());
                    ui.label(egui::RichText::new(current).font(theme::medium_font(15.0)).color(theme::INK));
                    ui.add_sized([ui.available_width(), 20.0],
                        egui::TextEdit::singleline(&mut self.sound_filter).hint_text("search presets…"));
                    let words: Vec<String> = self.sound_filter.to_lowercase().split_whitespace().map(str::to_string).collect();
                    egui::ScrollArea::vertical().id_salt("lv2_preset_list").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                        let mut shown = 0usize;
                        for p in &self.lv2_editor_presets {
                            let hay = p.name.to_lowercase();
                            if !words.iter().all(|w| hay.contains(w.as_str())) {
                                continue;
                            }
                            shown += 1;
                            let selected = self.lv2_preset_name.as_deref() == Some(p.name.as_str());
                            let resp = ui.selectable_label(selected, egui::RichText::new(&p.name).size(12.5).color(theme::CREAM));
                            if resp.clicked() {
                                chosen_preset = Some(p.clone());
                            }
                        }
                        if shown == 0 {
                            ui.label(egui::RichText::new("no matching preset").size(12.0).color(theme::INK_2));
                        }
                    });
                    ui.separator();
                    theme::section_label(ui, "Parameters");
                    ui.horizontal(|ui| {
                        ui.label("Filter:");
                        ui.add_sized([ui.available_width() - 10.0, 20.0],
                            egui::TextEdit::singleline(&mut self.editor_filter).hint_text("search parameters…"));
                    });
                    ui.separator();
                    let filter = self.editor_filter.to_lowercase();
                    egui::ScrollArea::vertical().id_salt("lv2_param_list").auto_shrink([false, false]).show(ui, |ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.bg_fill = theme::HAIRLINE;
                        v.widgets.inactive.fg_stroke = Stroke::new(1.0, theme::INK);
                        v.widgets.hovered.bg_fill = theme::ACCENT;
                        v.widgets.hovered.fg_stroke = Stroke::new(1.0, theme::INK);
                        v.widgets.active.bg_fill = theme::ACCENT;
                        v.widgets.active.fg_stroke = Stroke::new(1.0, theme::INK);
                        v.selection.bg_fill = theme::ACCENT;
                        let mut shown = 0usize;
                        for (ci, c) in self.lv2_editor_controls.iter().enumerate() {
                            if !filter.is_empty()
                                && !c.name.to_lowercase().contains(&filter)
                                && !c.symbol.to_lowercase().contains(&filter)
                            {
                                continue;
                            }
                            shown += 1;
                            let mut val = c.value;
                            let hi = if c.max > c.min { c.max } else { c.min + 1.0 };
                            let resp = ui.add_sized(
                                [ui.available_width() - 8.0, 18.0],
                                egui::Slider::new(&mut val, c.min..=hi)
                                    .text(egui::RichText::new(c.name.to_uppercase()).size(10.0).color(theme::INK_2)),
                            );
                            if resp.changed() {
                                let norm = ((val - c.min) / (hi - c.min).max(1e-6)).clamp(0.0, 1.0);
                                ctrl_changes.push((ci, val, norm));
                            }
                        }
                        if shown == 0 {
                            ui.label(egui::RichText::new("no matching parameters").size(12.0).color(theme::INK_2));
                        }
                    });
                });
            self.editor_open = open;
            if !open {
                self.lv2_editor_node = None;
            }
            if let Some(preset) = chosen_preset {
                if let Some(bus) = &self.bus {
                    bus.lock().unwrap().send(Command::LoadLv2Preset { node: lv2_node, preset_uri: preset.uri.clone() });
                }
                self.lv2_preset_name = Some(preset.name.clone());
                self.editor_status = format!("loaded {}", preset.name);
                for (sym, val) in &preset.values {
                    if let Some(c) = self.lv2_editor_controls.iter_mut().find(|c| &c.symbol == sym) {
                        let hi = if c.max > c.min { c.max } else { c.min + 1.0 };
                        c.value = val.clamp(c.min, hi);
                    }
                }
            }
            for (ci, val, norm) in ctrl_changes {
                if let Some(c) = self.lv2_editor_controls.get_mut(ci) {
                    c.value = val;
                    if let Some(bus) = &self.bus {
                        bus.lock().unwrap().send(Command::SetParam {
                            node: lv2_node,
                            param: c.index as u32,
                            value: norm as f64,
                        });
                    }
                }
            }
            return;
        }
        let rig = self.editor_rig;
        let name = self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .nth(rig)
            .map(|u| sentence_case(&u.name))
            .unwrap_or_else(|| if rig == 0 { "Surge A".to_string() } else { "Surge B".to_string() });
        let mut open = self.editor_open;
        let mut refresh = false;
        let mut changes: Vec<(u32, f64)> = Vec::new();
        let mut pin_req: Option<crate::host::ParamDesc> = None;
        egui::Window::new(egui::RichText::new(format!("{name} — sound & parameters"))
                .font(theme::medium_font(16.0)).color(theme::INK))
            .open(&mut open)
            .collapsible(false)
            .fixed_size([480.0, 700.0])
            .show(ctx, |ui| {
                let pool_idx = self.clap_pool_idx(rig);
                let plugin_id = pool_idx.and_then(|i| self.clap_plugin_ids.get(i)).map(|s| s.as_str());
                let is_surge = plugin_id == Some(SURGE_ID);
                let is_dexed = plugin_id == Some(DEXED_ID);
                if is_surge {
                    // Sound: exchange the instrument (Surge patch) of THIS synth.
                    theme::section_label(ui, "Sound");
                    let current = self.patch_names.get(rig).cloned().flatten().unwrap_or_else(|| "Init (Surge default)".into());
                    ui.label(egui::RichText::new(current).font(theme::medium_font(15.0)).color(theme::INK));
                    ui.add_sized([ui.available_width(), 20.0],
                        egui::TextEdit::singleline(&mut self.sound_filter).hint_text("search sounds… e.g. flute, warm pad, bass"));
                    let words: Vec<String> = self.sound_filter.to_lowercase().split_whitespace().map(str::to_string).collect();
                    let mut chosen: Option<crate::host::SurgePatch> = None;
                    {
                        let patches = self.surge_patches().to_vec();
                        egui::ScrollArea::vertical().id_salt("sound_list").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                            let mut shown = 0;
                            for p in patches.iter().filter(|p| {
                                let f = format!("{}/{}", p.category, p.name).to_lowercase();
                                words.iter().all(|w| f.contains(w.as_str()))
                            }) {
                                if shown >= 150 { ui.label(egui::RichText::new("… refine the search").size(11.0).color(theme::INK_2)); break; }
                                shown += 1;
                                let resp = ui.selectable_label(false, egui::RichText::new(format!("{} · {}", p.category, p.name)).size(12.5).color(theme::CREAM));
                                if resp.clicked() { chosen = Some(p.clone()); }
                            }
                            if shown == 0 { ui.label(egui::RichText::new("no matching sound").size(12.0).color(theme::INK_2)); }
                        });
                    }
                    if let Some(p) = chosen {
                        match self.load_patch_into_slot(rig, &p) {
                            Ok(n) => self.editor_status = format!("loaded {n}"),
                            Err(e) => self.editor_status = format!("sound failed: {e}"),
                        }
                    }
                    ui.separator();
                } else if is_dexed {
                    theme::section_label(ui, "Sound");
                    let current = self.patch_names.get(rig).cloned().flatten().unwrap_or_else(|| "Init (Dexed default)".into());
                    ui.label(egui::RichText::new(current).font(theme::medium_font(15.0)).color(theme::INK));
                    ui.add_sized([ui.available_width(), 20.0],
                        egui::TextEdit::singleline(&mut self.sound_filter).hint_text("search DX7 sounds… e.g. piano, bass, brass"));
                    let words: Vec<String> = self.sound_filter.to_lowercase().split_whitespace().map(str::to_string).collect();
                    let mut chosen: Option<crate::host::DexedPatch> = None;
                    {
                        let patches = self.dexed_patches().to_vec();
                        egui::ScrollArea::vertical().id_salt("dexed_sound_list").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                            let mut shown = 0usize;
                            for p in patches.iter().filter(|p| {
                                let f = format!("{}/{}", p.bank, p.name).to_lowercase();
                                words.iter().all(|w| f.contains(w.as_str()))
                            }) {
                                if shown >= 150 { ui.label(egui::RichText::new("… refine the search").size(11.0).color(theme::INK_2)); break; }
                                shown += 1;
                                let resp = ui.selectable_label(false, egui::RichText::new(format!("{} · {:02} {}", p.bank, p.program + 1, p.name)).size(12.5).color(theme::CREAM));
                                if resp.clicked() { chosen = Some(p.clone()); }
                            }
                            if shown == 0 { ui.label(egui::RichText::new("no matching sound").size(12.0).color(theme::INK_2)); }
                        });
                    }
                    if let Some(p) = chosen {
                        match self.load_dexed_patch_into_slot(rig, &p) {
                            Ok(n) => self.editor_status = format!("loaded {n}"),
                            Err(e) => self.editor_status = format!("sound failed: {e}"),
                        }
                    }
                    ui.separator();
                }
                theme::section_label(ui, "Parameters");
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add_sized([ui.available_width() - 70.0, 20.0],
                        egui::TextEdit::singleline(&mut self.editor_filter).hint_text("search parameters…"));
                    if ui.button("Refresh").on_hover_text("re-read the plugin's parameters").clicked() {
                        refresh = true;
                    }
                });
                ui.separator();
                let filter = self.editor_filter.to_lowercase();
                // Which params are pinned on this synth's face (for the toggles).
                let pin_node = self.clap_rigs().into_iter().find(|(s, _)| *s == rig).map(|(_, n)| n);
                let pinned: Vec<u32> = self.units.iter().find(|u| Some(u.node_id) == pin_node)
                    .map(|u| u.knobs.iter().filter_map(|k| k.clap.map(|c| c.id)).collect()).unwrap_or_default();
                // Split borrows: cached params (read) vs voice mains (param_text).
                let params = &self.editor_params;
                let pool_idx = self.clap_pool_idx(rig);
                let mains = &mut self._surge_main;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Sliders: accent fill on a hairline track (§4.18). Scoped
                    // once for the whole list, not per row.
                    let v = ui.visuals_mut();
                    v.widgets.inactive.bg_fill = theme::HAIRLINE;
                    v.widgets.inactive.fg_stroke = Stroke::new(1.0, theme::INK);
                    v.widgets.hovered.bg_fill = theme::ACCENT;
                    v.widgets.hovered.fg_stroke = Stroke::new(1.0, theme::INK);
                    v.widgets.active.bg_fill = theme::ACCENT;
                    v.widgets.active.fg_stroke = Stroke::new(1.0, theme::INK);
                    v.selection.bg_fill = theme::ACCENT;
                    let mut modules: Vec<&str> = Vec::new();
                    for p in params {
                        if !modules.contains(&p.module.as_str()) {
                            modules.push(p.module.as_str());
                        }
                    }
                    let mut shown = 0usize;
                    for module in modules {
                        let group: Vec<&ParamDesc> = params.iter()
                            .filter(|p| p.module == module && p.automatable && !p.hidden && !p.readonly
                                && (filter.is_empty()
                                    || p.name.to_lowercase().contains(&filter)
                                    || p.module.to_lowercase().contains(&filter)))
                            .collect();
                        if group.is_empty() {
                            continue;
                        }
                        // Force modules open while searching so matches show.
                        let force_open = if filter.is_empty() { None } else { Some(true) };
                        egui::CollapsingHeader::new(module)
                            .open(force_open)
                            .show(ui, |ui| {
                                for p in group {
                                    shown += 1;
                                    let mut value = p.value;
                                    ui.horizontal(|ui| {
                                        let w = ui.available_width();
                                        // Param name: micro-caps INK_2 (§4.18).
                                        let resp = ui.add_sized(
                                            [(w - 96.0).max(120.0), 18.0],
                                            egui::Slider::new(&mut value, p.min..=p.max)
                                                .text(egui::RichText::new(p.name.to_uppercase())
                                                    .size(10.0).color(theme::INK_2)),
                                        );
                                        if resp.changed() {
                                            changes.push((p.id, value));
                                        }
                                        let text = pool_idx
                                            .and_then(|i| mains.get_mut(i))
                                            .map(|m| m.param_text(p.id, value))
                                            .unwrap_or_default();
                                        ui.label(egui::RichText::new(text)
                                            .font(theme::medium_font(13.0)).color(theme::INK));
                                        // Pin this parameter as a knob on the synth's face.
                                        let is_pinned = pinned.contains(&p.id);
                                        if theme::chip_button(ui, if is_pinned { "− knob" } else { "+ knob" }, is_pinned)
                                            .on_hover_text(if is_pinned { "remove this knob from the Surge face" } else { "put this parameter as a knob on the Surge face" })
                                            .clicked() { pin_req = Some(p.clone()); }
                                    });
                                }
                            });
                    }
                    if shown == 0 {
                        ui.label(egui::RichText::new("no matching parameters").size(12.0).color(theme::INK_2));
                    }
                });
            });
        self.editor_open = open;
        if let Some(p) = pin_req { self.editor_status = self.toggle_pin(rig, &p); }
        // Apply slider changes: update the cache + schedule RT-safe param events.
        for (id, value) in changes {
            if let Some(p) = self.editor_params.iter_mut().find(|p| p.id == id) {
                p.value = value;
            }
            if let Some(ctrl) = self.clap_pool_idx(rig).and_then(|i| self.surge_controllers.get(i)) {
                ctrl.set_param(ClapId::new(id), value);
            }
        }
        if refresh {
            self.refresh_editor_params();
        }
    }

    // -- hot-wire MIDI matrix ------------------------------------

    /// Matrix sink columns: instrument units (CLAP voices + LV2 instruments).
    fn matrix_sinks(&self) -> Vec<usize> {
        self.units.iter().enumerate()
            .filter(|(_, u)| matches!(u.node_kind, UnitKind::Clap) || u.lv2_inst)
            .map(|(i, _)| i)
            .collect()
    }

    /// The Keys unit belonging to a rig tag (each rig carries its own Keys
    /// after LoadRigs; single-graph patches have exactly one).
    fn keys_unit_for_rig(&self, tag: u8) -> Option<usize> {
        self.units.iter().position(|u| matches!(u.node_kind, UnitKind::Keys) && u.rig == tag)
    }

    /// Live MIDI cable from any Keys unit into this instrument's MIDI in.
    fn midi_cable_to(&self, inst: usize) -> Option<usize> {
        self.cables.iter().position(|c| {
            c.dying < 0.0 && c.kind == PortKind::Midi && c.dst == inst
                && matches!(self.units.get(c.src).map(|u| &u.node_kind), Some(UnitKind::Keys))
        })
    }

    /// Hot-wire: plug the rig's Keys → instrument MIDI cable if missing.
    fn ensure_midi_cable(&mut self, inst: usize) {
        if self.midi_cable_to(inst).is_some() { return; }
        let tag = self.units.get(inst).map(|u| u.rig).unwrap_or(0);
        if let Some(k) = self.keys_unit_for_rig(tag) {
            self.add_cable(k, 0, inst, 0, PortKind::Midi, 0);
        }
    }

    /// Toggle one matrix cell live. CLAP columns map to the engine's source
    /// filters + device binding (SetRigSource/SetRigDevice); LV2 instrument
    /// columns are gated by the MIDI cable alone. Turning a cell ON also
    /// hot-wires the MIDI cable if it was unplugged.
    fn matrix_toggle(&mut self, inst: usize, row: MatrixRow, on: bool) {
        let is_clap = matches!(self.units.get(inst).map(|u| &u.node_kind), Some(UnitKind::Clap));
        if !is_clap {
            // LV2 instrument: the cable IS the route.
            if on { self.ensure_midi_cable(inst); }
            else if let Some(ci) = self.midi_cable_to(inst) { self.unplug_cable(ci, true); }
            return;
        }
        let Some(rig) = self.rig_of_unit(inst) else { return };
        if rig >= self.rig_sources.len() { return; } // beyond the voice pool
        if on { self.ensure_midi_cable(inst); }
        let node = self.units[inst].node_id;
        {
            let s = &mut self.rig_sources[rig];
            let d = &mut self.rig_device[rig];
            match row {
                MatrixRow::Keys => s.0 = on,
                MatrixRow::HwAll => { s.1 = on; if on { *d = None; } }
                MatrixRow::HwDev(dev) => {
                    if on { s.1 = true; *d = Some(dev); }
                    else if *d == Some(dev) { s.1 = false; }
                }
            }
        }
        self.send_rig_source(node);
        if matches!(row, MatrixRow::HwAll | MatrixRow::HwDev(_)) {
            let device = self.rig_device[rig];
            if let Some(bus) = &self.bus {
                let _ = bus.lock().unwrap().send(Command::SetRigDevice { node, device });
            }
        }
    }

    /// Matrix source rows with their header label and activity LED: ONE
    /// Keyboard row (on-screen clicks + laptop typing), hardware omni, then
    /// one row per detected device.
    fn matrix_rows(&self) -> Vec<(MatrixRow, String, f32)> {
        let mut rows: Vec<(MatrixRow, String, f32)> = vec![
            (MatrixRow::Keys, "Keyboard".into(), self.keyboard_led()),
            (MatrixRow::HwAll, "All Hardware".into(), self.hw_leds.iter().cloned().fold(0.0f32, f32::max)),
        ];
        for (d, name) in self.hw_devices.iter().enumerate() {
            rows.push((MatrixRow::HwDev(d as u32), format!("Hardware: {name}"),
                self.hw_leds.get(d).copied().unwrap_or(0.0)));
        }
        rows
    }

    /// The hot-wire MIDI matrix grid: sources × instrument rigs, live.
    /// Hurricane language: hairline cells, teal = routed. Rendered inline in
    /// the main stage (one-window rule) — click toggles a route, right-click
    /// opens the inline rule editor.
    fn matrix_grid_ui(&mut self, ui: &mut egui::Ui) {
        let sinks = self.matrix_sinks();
        if sinks.is_empty() {
            ui.label(egui::RichText::new("no instruments in the patch — add a rig first").size(12.0).color(theme::INK_2));
            return;
        }
        let mut toggles: Vec<(usize, MatrixRow, bool)> = Vec::new();
        let mut edit_requests: Vec<(usize, MatrixRow)> = Vec::new();
        let rows = self.matrix_rows();
        // Header cells never wrap ("Surge A" split across lines): Extend
        // labels + a min column width keep names and status on fixed rows.
        let no_wrap = |ui: &mut egui::Ui, text: egui::RichText| {
            ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend));
        };
        egui::Grid::new("midi_matrix_grid").num_columns(sinks.len() + 1).spacing([20.0, 10.0])
            .min_col_width(120.0).show(ui, |ui| {
            // Header: sink names + rig tags + cable state.
            theme::section_label(ui, "Source");
            for &s in &sinks {
                let u = &self.units[s];
                let gate = self.midi_cable_to(s).is_some();
                let node_at = self.agent_activity.node_alpha(u.node_id, self.ui_time);
                ui.vertical(|ui| {
                    ui.set_min_width(90.0);
                    // Multi-rig: "2 · Surge" like the RIG INPUTS labels — two
                    // columns both headed "Surge" were indistinguishable.
                    let head = if self.rig_count() > 1 { format!("{} · {}", u.rig as usize + 1, sentence_case(&u.name)) } else { sentence_case(&u.name) };
                    ui.horizontal(|ui| {
                        no_wrap(ui, egui::RichText::new(head).font(theme::medium_font(15.0)).color(theme::INK));
                        if node_at > 0.01 {
                            let (br, _) = ui.allocate_exact_size(egui::vec2(44.0, 15.0), egui::Sense::hover());
                            ui.painter().rect_filled(br, 2.0, theme::alpha(theme::AGENT_COLOR, node_at));
                            ui.painter().text(br.center(), egui::Align2::CENTER_CENTER, "AGENT",
                                theme::medium_font(9.0), theme::alpha(theme::AGENT_COLOR_TEXT, node_at));
                        }
                    });
                    // Single-graph mode: all nodes share rig 0 — don't label
                    // every column "rig A" (user report 2026-11-15).
                    // Same rig naming as the RIGS strip, same "MIDI" casing as everywhere.
                    let rig_txt = if self.rig_count() > 1 { format!("{} · ", self.rig_name(u.rig as usize)) } else { String::new() };
                    no_wrap(ui, egui::RichText::new(format!("{rig_txt}MIDI {}", if gate { "IN ✓" } else { "unplugged" }))
                        .size(11.5).color(if gate { theme::LIVE_TEAL } else { theme::INK_2 }));
                });
            }
            ui.end_row();
            for (row, label, led) in &rows {
                // Row header with activity dot.
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 3.0,
                        theme::alpha(theme::LIVE_TEAL, 0.25 + 0.75 * led.clamp(0.0, 1.0)));
                    ui.add_space(4.0);
                    no_wrap(ui, egui::RichText::new(label).size(14.0).color(theme::INK));
                });
                for &s in &sinks {
                    let u = &self.units[s];
                    let is_clap = matches!(u.node_kind, UnitKind::Clap);
                    let gate = self.midi_cable_to(s).is_some();
                    let on = if is_clap {
                        let rig = self.rig_of_unit(s).unwrap_or(0);
                        let src = self.rig_sources.get(rig).copied().unwrap_or((true, true));
                        let dev = self.rig_device.get(rig).copied().flatten();
                        clap_cell(src, dev, gate, *row)
                    } else {
                        gate // LV2: the cable is the route (all sources)
                    };
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(68.0, 32.0), egui::Sense::click());
                    let p = ui.painter();
                    let cell = rect.shrink(3.0);
                    let node = self.units[s].node_id;
                    let edit = row.source().and_then(|src| self.rule_edits.get(&(src, node)).cloned());
                    let shaped = edit.filter(|e| !e.is_default());
                    if on {
                        // Routed: teal fill + an in-cell code — "ALL" (every
                        // note passes) or the shaped rule's change ("+12",
                        // "ch 2", "keys", "vel"); explicit colors (rule 1).
                        p.rect_filled(cell, 2.0, theme::LIVE_TEAL);
                        let code = shaped.as_ref().map_or_else(|| "ALL".to_string(), |e| e.cell_code());
                        p.text(cell.center(), egui::Align2::CENTER_CENTER, code, theme::medium_font(12.5), theme::DARK_PANEL);
                    } else {
                        p.rect_stroke(cell, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
                    }
                    // Shaped rule: the in-cell code says so (no extra accent
                        // dot — tomato stays CTA/selection-only); hover = summary.
                    let resp = if let Some(e) = &shaped {
                        resp.on_hover_text(format!("rule: {}\nright-click to edit", e.summary()))
                    } else if row.source().is_some() {
                        resp.on_hover_text("click: route on/off · right-click: edit rule")
                    } else {
                        resp
                    };
                    if resp.hovered() {
                        p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    } else if let Some(src) = row.source() {
                        let cell_at = self.agent_activity.cell_alpha(src, node, self.ui_time);
                        if cell_at > 0.01 {
                            p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.5, theme::alpha(theme::AGENT_COLOR, cell_at)), egui::StrokeKind::Inside);
                        }
                    }
                    if resp.clicked() { toggles.push((s, *row, !on)); }
                    if resp.secondary_clicked() && row.source().is_some() {
                        edit_requests.push((s, *row));
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new("ALL = every note passes · +12 / ch 2 / keys / vel / off = a shaped rule (right-click a cell) · empty = not routed")
            .size(12.5).color(theme::INK_2));
        ui.label(egui::RichText::new(self.split_footer()).size(12.5).color(theme::INK_2));
        if !self.hw_devices.is_empty() {
            ui.label(egui::RichText::new("Hardware rows are a radio group per rig: one device, all devices, or off.")
                .size(11.0).color(theme::INK_2));
        }
        for (s, row, on) in toggles { self.matrix_toggle(s, row, on); }
        if let Some(cell) = edit_requests.last() { self.rule_editor = Some(*cell); }
    }

    fn parse_rule_source(&self, source: &str) -> Result<SourceId, String> {
        let row = match source {
            "keys" | "keyboard" | "laptop" => MatrixRow::Keys,
            s if s.starts_with("hw:") => match s[3..].parse::<u32>() {
                Ok(d) => MatrixRow::HwDev(d),
                Err(_) => return Err(format!("bad source '{source}'")),
            },
            _ => return Err(format!("unknown source '{source}' (keys|hw:<d>)")),
        };
        row.source().ok_or_else(|| "hw:all has no single rule — use hw:<d>".to_string())
    }

    fn source_str(src: SourceId) -> String {
        match src {
            SourceId::Keys => "keys".into(),
            SourceId::Hardware(d) => format!("hw:{d}"),
            other => format!("{other:?}"),
        }
    }

    fn node_param_choices(&mut self, node_id: u32) -> Vec<(u32, String)> {
        let Some(u_idx) = self.units.iter().position(|u| u.node_id == node_id) else {
            return Vec::new();
        };
        match &self.units[u_idx].node_kind {
            UnitKind::Clap => {
                if let Some(slot) = self.rig_of_unit(u_idx) {
                    if let Some(idx) = self.clap_pool_idx(slot) {
                        if let Some(main) = self._surge_main.get_mut(idx) {
                            return main
                                .param_list()
                                .into_iter()
                                .filter(|p| p.automatable && !p.hidden && !p.readonly)
                                .map(|p| {
                                    let label = if p.module.is_empty() {
                                        p.name
                                    } else {
                                        format!("{} / {}", p.module, p.name)
                                    };
                                    (p.id, label)
                                })
                                .collect();
                        }
                    }
                }
                Vec::new()
            }
            UnitKind::Dsp(dk) => {
                let kind = self.base_graph.node(node_id).map(|n| &n.kind);
                if let Some(NodeKind::Lv2 { uri }) = kind {
                    crate::lv2::control_descs_for_uri(uri.as_str())
                        .into_iter()
                        .map(|c| (c.index as u32, c.name))
                        .collect()
                } else if matches!(kind, Some(NodeKind::SystemAudio)) {
                    vec![(crate::engine::P_SYSTEM_GAIN, "Gain".into())]
                } else {
                    crate::engine::dsp_param_choices(dk)
                        .iter()
                        .map(|&(id, name)| (id, name.to_string()))
                        .collect()
                }
            }
            UnitKind::Master => vec![(P_VOL, "Volume".into()), (P_WARM, "Warmth".into())],
            UnitKind::Keys => Vec::new(),
            UnitKind::AudioIn => vec![(P_AUDIO_IN_GAIN, "Gain".into()), (P_AUDIO_IN_MONITOR, "Monitor".into())],
        }
    }

    fn parse_cc_target(target: Option<&str>) -> Result<crate::matrix::CcTarget, String> {
        match target.map(str::trim).map(str::to_lowercase).as_deref() {
            None | Some("plugin") | Some("plugin_param") | Some("pluginparam") => {
                Ok(crate::matrix::CcTarget::PluginParam)
            }
            Some("knob") => Ok(crate::matrix::CcTarget::Knob),
            Some(other) => Err(format!("unknown target '{other}' (expected 'plugin' or 'knob')")),
        }
    }

    fn resolve_unit_knob(&self, node_id: u32, knob: &str) -> Result<(u32, crate::matrix::CcTarget), String> {
        let u = self.units.iter().find(|u| u.node_id == node_id)
            .ok_or_else(|| format!("no unit with node id {node_id}"))?;
        // Long names ("RESONANCE") resolve through the shared table's aliases.
        let canonical = self.base_graph.node(node_id)
            .and_then(|n| crate::engine::find_knob_spec(&n.kind, knob))
            .map(|spec| spec.label);
        let q = canonical.unwrap_or(knob).trim().to_uppercase();
        for k in &u.knobs {
            if k.label.to_uppercase() == q {
                return k.cc_target().ok_or_else(|| format!("knob '{}' on {} is local-only", k.label, u.name));
            }
        }
        Err(format!("no knob '{knob}' on node {node_id}"))
    }

    fn knob_param_label(&self, node_id: u32, param: u32) -> Option<String> {
        self.units.iter()
            .find(|u| u.node_id == node_id)
            .and_then(|u| u.knobs.iter().find(|k| k.param == Some(param)))
            .map(|k| format!("{} (knob)", k.label))
    }

    fn source_sort_key(src: SourceId) -> (u8, u32) {
        match src {
            SourceId::Hardware(d) => (0, d),
            SourceId::Keys => (1, 0),
            SourceId::Virtual(v) => (2, v),
            SourceId::FilePlayer(f) => (3, f),
        }
    }

    /// Active CC binding for a rack knob (if any): `(source, cc)`.
    fn knob_cc_binding(&self, node_id: u32, param: u32, target: crate::matrix::CcTarget) -> Option<(SourceId, u8)> {
        let mut best: Option<(SourceId, u8)> = None;
        for (&(src, dest), edit) in &self.rule_edits {
            if dest != node_id { continue; }
            for m in &edit.cc_map {
                if m.param == param && m.target == target {
                    if best.map_or(true, |(bs, bc)| (Self::source_sort_key(bs), bc) > (Self::source_sort_key(src), m.cc)) {
                        best = Some((src, m.cc));
                    }
                }
            }
        }
        best
    }

    /// Clear every CC binding targeting this knob across all sources.
    fn clear_knob_bindings(&mut self, node_id: u32, param: u32, target: crate::matrix::CcTarget) {
        let matches: Vec<(SourceId, u8)> = self.rule_edits.iter()
            .filter(|(&(_, dest), _)| dest == node_id)
            .flat_map(|(&(src, _), edit)| {
                edit.cc_map.iter()
                    .filter(move |m| m.param == param && m.target == target)
                    .map(move |m| (src, m.cc))
            })
            .collect();
        for (src, cc) in matches {
            self.set_cc_binding(src, node_id, cc, None, target);
        }
    }

    /// Mirror an incoming hardware CC onto matching rack knobs. Normally the
    /// engine already applied it, so `sent` is updated and nothing is re-sent
    /// (no feedback loop). `resend` is for the CC that completed a learn: the
    /// engine saw it before the binding existed, so publish must catch it up.
    fn apply_cc_to_knobs(&mut self, src: SourceId, ch: u8, cc: u8, val: u8, resend: bool) {
        let norm = (val as f32 / 127.0).clamp(0.0, 1.0);
        for ((rule_src, dest), edit) in &self.rule_edits {
            if *rule_src != src || !edit.enabled { continue; }
            if let Some(rule_ch) = edit.channel {
                if rule_ch != ch { continue; }
            }
            for m in &edit.cc_map {
                if m.cc != cc { continue; }
                for u in &mut self.units {
                    if u.node_id != *dest { continue; }
                    // On non-CLAP units plugin and knob params are the same
                    // engine path (apply_engine_param), so both targets move the knob.
                    let is_clap = matches!(u.node_kind, UnitKind::Clap);
                    for k in &mut u.knobs {
                        let by_param = matches!(m.target, crate::matrix::CcTarget::Knob) || !is_clap;
                        if by_param && k.param == Some(m.param) {
                            k.value = norm;
                            if !resend { k.sent = Some(norm); }
                        } else if matches!(m.target, crate::matrix::CcTarget::PluginParam) {
                            if let Some(pin) = k.clap.as_mut() {
                                if pin.id == m.param {
                                    k.value = norm;
                                    if !resend { pin.sent = norm; }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn set_cc_binding(&mut self, source: SourceId, dest: u32, cc: u8, param: Option<u32>, target: crate::matrix::CcTarget) {
        let cc = cc & 0x7F;
        let mut e = self.rule_edits.get(&(source, dest)).cloned().unwrap_or_else(|| {
            // Mirror what the engine will do: binding on a pair without a rule
            // creates a CC-only rule (MatrixCcMap), never a note route.
            let routed = match self.graph_pub.as_ref().and_then(|p| p.lock().ok()) {
                Some(g) => g.rules.iter().any(|r| r.source == source && r.dest == dest),
                None => true,
            };
            RuleEdit { cc_only: !routed, ..RuleEdit::default() }
        });
        match param {
            Some(p) => e.set_cc(cc, p, target),
            None => e.unmap_cc(cc),
        }
        if e.is_default() {
            self.rule_edits.remove(&(source, dest));
        } else {
            self.rule_edits.insert((source, dest), e);
        }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::MatrixCcMap { source, dest, cc, param, target });
        }
    }

    fn complete_cc_learn(&mut self, incoming_src: SourceId, cc: u8) -> bool {
        let Some(learn) = self.cc_learn.clone() else { return false };
        let bind_src = match learn.source {
            SourceId::Hardware(d) if incoming_src == SourceId::Hardware(d) => incoming_src,
            SourceId::Hardware(_) => return false,
            SourceId::Keys => incoming_src,
            other if other == incoming_src => incoming_src,
            _ => return false,
        };
        if learn.replace_existing {
            self.clear_knob_bindings(learn.dest, learn.param, learn.target);
        }
        self.set_cc_binding(bind_src, learn.dest, cc, Some(learn.param), learn.target);
        if learn.source == SourceId::Keys {
            if let SourceId::Hardware(d) = bind_src {
                if let Some((unit, _)) = self.rule_editor {
                    self.rule_editor = Some((unit, MatrixRow::HwDev(d)));
                }
            }
        }
        self.rule_cc_num = cc & 0x7F;
        self.cc_learn = None;
        true
    }

    /// Per-cell rule editor (right-click a matrix cell), inline beside the
    /// grid: channel filter, transpose, key/velocity ranges, enabled, CC maps +
    /// MIDI-learn — sent live as MatrixRule / MatrixCcMap.
    fn rule_editor_ui(&mut self, ui: &mut egui::Ui) {
        let Some((unit, row)) = self.rule_editor else { return };
        let Some(src) = row.source() else { self.rule_editor = None; self.cc_learn = None; return };
        let Some(u) = self.units.get(unit) else { self.rule_editor = None; self.cc_learn = None; return };
        let node = u.node_id;
        let title = format!("{} → {}", row.label(), sentence_case(&u.name));
        if self.rule_param_cache.as_ref().map(|(id, _)| *id) != Some(node) {
            let choices = self.node_param_choices(node);
            if let Some(&(first_id, _)) = choices
                .iter()
                .find(|(_, n)| n.to_lowercase().contains("cutoff"))
                .or_else(|| choices.first())
            {
                self.rule_param_id = first_id;
            }
            self.rule_param_cache = Some((node, choices));
        }
        let mut edit = self.rule_edits.get(&(src, node)).cloned().unwrap_or_default();
        let mut close = false;
        let mut changed = false;
        let mut reset = false;
        let mut add_cc: Option<(u8, u32)> = None;
        let mut remove_ccs: Vec<(u8, crate::matrix::CcTarget)> = Vec::new();
        ui.label(egui::RichText::new(title).font(theme::medium_font(15.0)).color(theme::INK));
        ui.add_space(2.0);
        ui.label(egui::RichText::new("shapes this route live · releases held notes on change")
            .size(11.0).color(theme::INK_2));
        ui.add_space(4.0);
        let mut en = edit.enabled;
        if ui.checkbox(&mut en, "Route enabled").changed() { edit.enabled = en; changed = true; }
        let mut omni = edit.channel.is_none();
        if ui.checkbox(&mut omni, "Omni (any MIDI channel)").changed() {
            edit.channel = if omni { None } else { Some(0) };
            changed = true;
        }
        if let Some(ch) = edit.channel {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Channel").size(12.0));
                let mut c = ch as i32 + 1;
                if ui.add(egui::DragValue::new(&mut c).range(1..=16)).changed() {
                    edit.channel = Some((c - 1).clamp(0, 15) as u8);
                    changed = true;
                }
            });
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Transpose").size(12.0));
            let mut t = edit.transpose as i32;
            if ui.add(egui::DragValue::new(&mut t).range(-24..=24).suffix(" st")).changed() {
                edit.transpose = t.clamp(-24, 24) as i8;
                changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Key range").size(12.0));
            let mut lo = edit.key_lo as i32;
            let mut hi = edit.key_hi as i32;
            if ui.add(egui::DragValue::new(&mut lo).range(0..=127)).changed() { edit.key_lo = lo as u8; changed = true; }
            ui.label(egui::RichText::new("–").size(12.0));
            if ui.add(egui::DragValue::new(&mut hi).range(0..=127)).changed() { edit.key_hi = hi as u8; changed = true; }
            ui.label(egui::RichText::new(format!("{} – {}", note_name(edit.key_lo), note_name(edit.key_hi)))
                .size(11.0).color(theme::INK_2));
        });
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Velocity").size(12.0));
            let mut lo = edit.vel_lo as i32;
            let mut hi = edit.vel_hi as i32;
            if ui.add(egui::DragValue::new(&mut lo).range(0..=127)).changed() { edit.vel_lo = lo as u8; changed = true; }
            ui.label(egui::RichText::new("–").size(12.0));
            if ui.add(egui::DragValue::new(&mut hi).range(0..=127)).changed() { edit.vel_hi = hi as u8; changed = true; }
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        theme::section_label(ui, "CC Maps");
        let cc_hint = if edit.cc_map.is_empty() {
            "empty = all CCs pass through"
        } else {
            "whitelist active · unmapped CCs filtered"
        };
        ui.label(egui::RichText::new(cc_hint).size(11.0).color(theme::INK_2));
        ui.add_space(4.0);
        let choices = self.rule_param_cache.as_ref().map(|(_, c)| c.as_slice()).unwrap_or(&[]);
        for m in &edit.cc_map {
            let pname = match m.target {
                crate::matrix::CcTarget::Knob => self.knob_param_label(node, m.param)
                    .unwrap_or_else(|| format!("Knob {}", m.param)),
                crate::matrix::CcTarget::PluginParam => choices
                    .iter()
                    .find(|(id, _)| *id == m.param)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| format!("Param {}", m.param)),
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("CC {:>3}", m.cc)).size(12.0).color(theme::MIDI_BLUE));
                ui.label(egui::RichText::new("→").size(12.0).color(theme::INK_2));
                let label_w = (ui.available_width() - 42.0).max(60.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(label_w, 24.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(pname).size(12.0).color(theme::INK)).truncate());
                    },
                );
                if theme::chip_button(ui, "×", false).on_hover_text("remove this CC binding").clicked() {
                    remove_ccs.push((m.cc, m.target));
                }
            });
        }
        if !choices.is_empty() {
            ui.add_sized(
                [ui.available_width() - 4.0, 20.0],
                egui::TextEdit::singleline(&mut self.rule_param_filter).hint_text("filter param…"),
            );
            let words: Vec<String> = self.rule_param_filter.to_lowercase().split_whitespace().map(str::to_string).collect();
            let selected_label = choices
                .iter()
                .find(|(id, _)| *id == self.rule_param_id)
                .map(|(_, n)| n.as_str())
                .unwrap_or("select param…");
            egui::ComboBox::from_id_salt("rule_cc_param_pick")
                .width((ui.available_width() - 4.0).max(160.0))
                .selected_text(egui::RichText::new(selected_label).size(12.0).color(theme::INK))
                .show_ui(ui, |ui| {
                    let mut shown = 0usize;
                    for (id, name) in choices {
                        let low = name.to_lowercase();
                        if !words.iter().all(|w| low.contains(w.as_str())) {
                            continue;
                        }
                        if shown >= 80 {
                            ui.label(egui::RichText::new("… refine filter").size(11.0).color(theme::INK_2));
                            break;
                        }
                        shown += 1;
                        ui.selectable_value(&mut self.rule_param_id, *id, egui::RichText::new(name).size(12.0).color(theme::INK));
                    }
                    if shown == 0 {
                        ui.label(egui::RichText::new("no matching parameter").size(11.0).color(theme::INK_2));
                    }
                });
        } else {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Param ID").size(12.0).color(theme::INK));
                let mut pid = self.rule_param_id as i64;
                if ui.add(egui::DragValue::new(&mut pid).range(0..=u32::MAX as i64)).changed() {
                    self.rule_param_id = pid.max(0) as u32;
                }
            });
        }
        ui.add_space(4.0);
        let learning_here = self.cc_learn.as_ref().is_some_and(|l| l.source == src && l.dest == node);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("CC").size(12.0).color(theme::INK));
            let mut cc_i = self.rule_cc_num as i32;
            if ui.add(egui::DragValue::new(&mut cc_i).range(0..=127)).changed() {
                self.rule_cc_num = cc_i.clamp(0, 127) as u8;
            }
            if theme::chip_button(ui, "Add", false)
                .on_hover_text("bind this CC number to the chosen parameter")
                .clicked()
            {
                add_cc = Some((self.rule_cc_num, self.rule_param_id));
            }
            if learning_here {
                if theme::primary_button(ui, "Learning…")
                    .on_hover_text("move a hardware knob to bind it (click or Esc to cancel)")
                    .clicked()
                {
                    self.cc_learn = None;
                }
            } else if theme::chip_button(ui, "Learn", false)
                .on_hover_text("arm MIDI-learn: the next hardware CC binds to the chosen parameter")
                .clicked()
            {
                self.cc_learn = Some(CcLearn {
                    source: src,
                    dest: node,
                    param: self.rule_param_id,
                    target: crate::matrix::CcTarget::PluginParam,
                    replace_existing: false,
                });
            }
        });
        if learning_here {
            ui.label(egui::RichText::new("Move a hardware knob to bind… (Esc cancels)")
                .size(11.0).color(theme::ACCENT_BRIGHT));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::chip_button(ui, "Reset rule", false).clicked() { reset = true; }
            ui.add_space(6.0);
            if theme::chip_button(ui, "Close", false).on_hover_text("close the rule editor (Esc)").clicked() { close = true; }
        });
        if close {
            self.rule_editor = None;
            self.cc_learn = None;
        }
        if reset {
            let old_ccs: Vec<(u8, crate::matrix::CcTarget)> = edit.cc_map.iter().map(|m| (m.cc, m.target)).collect();
            for (cc, tgt) in old_ccs {
                self.set_cc_binding(src, node, cc, None, tgt);
            }
            edit = RuleEdit::default();
            changed = true;
            self.cc_learn = None;
        }
        for (cc, tgt) in remove_ccs {
            self.set_cc_binding(src, node, cc, None, tgt);
        }
        if let Some((cc, param)) = add_cc {
            self.set_cc_binding(src, node, cc, Some(param), crate::matrix::CcTarget::PluginParam);
        }
        if changed {
            edit.cc_only = false; // a shaped route routes notes (engine MatrixRule)
            let cmd = edit.to_command(src, node);
            if edit.is_default() { self.rule_edits.remove(&(src, node)); }
            else { self.rule_edits.insert((src, node), edit); }
            if let Some(bus) = &self.bus {
                let _ = bus.lock().unwrap().send(cmd);
            }
        }
    }

    /// Publish the full per-rig mixer state to the engine (idempotent).
    fn send_rig_mix(&self, rig: usize) {
        let m = self.rig_mix[rig.min(7)];
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetRigMix {
                rig: rig as u8,
                vol: Some(m.vol), pan: Some(m.pan), mute: Some(m.mute), solo: Some(m.solo),
            });
        }
    }

    /// Mirror the rig snapshot's device bindings into the per-synth source
    /// filters: every synth voice of a device-bound rig takes only that
    /// device (H on, K off). Indexes by VOICE slot, not rig index.
    fn adopt_rig_devices(&mut self) {
        let devs: Vec<Option<u32>> = self.rig_snapshot.lock()
            .map(|s| s.rigs.iter().map(|m| m.midi_device).collect()).unwrap_or_default();
        for (slot, node) in self.clap_rigs() {
            let rig = self.units.iter().find(|u| u.node_id == node).map_or(0, |u| u.rig as usize);
            if let Some(Some(d)) = devs.get(rig) {
                self.rig_sources[slot] = (false, true);
                self.rig_device[slot] = Some(*d);
            }
        }
    }

    /// Answer bridge/MCP requests for UI-owned tools (load_sound, …) with the
    /// SAME exec_tool the in-app agent uses (rule 6 parity).
    fn serve_ui_inbox(&mut self) {
        if self.bus.is_none() { return; }
        crate::bridge::UI_ATTACHED.store(true, std::sync::atomic::Ordering::Relaxed);
        let reqs: Vec<crate::bridge::UiRequest> = crate::bridge::UI_INBOX.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
        for (tool, args, reply) in reqs {
            let out = match tool.as_str() {
                t if UI_BRIDGE_TOOLS.contains(&t) => self.exec_tool_with_origin(&tool, &args, AgentOrigin::Remote),
                t if t == crate::bridge::BEFORE_REMOTE_EDIT => {
                    self.note_agent_change(AgentOrigin::Remote);
                    "ok".into()
                }
                other => format!("tool '{other}' is not served over the UI bridge"),
            };
            let _ = reply.send(out);
        }
    }

    /// Installed Surge patches (scanned once, cached).
    fn surge_patches(&mut self) -> &[crate::host::SurgePatch] {
        if self.surge_patch_cache.is_none() { self.surge_patch_cache = Some(crate::host::list_surge_patches()); }
        self.surge_patch_cache.as_deref().unwrap_or(&[])
    }

    /// Installed Dexed DX7 `.syx` cartridge patches (scanned once, cached).
    fn dexed_patches(&mut self) -> &[crate::host::DexedPatch] {
        if self.dexed_patch_cache.is_none() { self.dexed_patch_cache = Some(crate::host::discover_dexed_patches()); }
        self.dexed_patch_cache.as_deref().unwrap_or(&[])
    }

    /// Load the best-matching Surge or Dexed patch into `rig`'s (first) synth.
    fn load_sound(&mut self, rig: usize, query: &str) -> Result<String, String> {
        if rig >= self.rig_count() { return Err(format!("no rig {rig}")); }
        let multi = self.rig_count() > 1;
        let node = self.units.iter().find(|u| matches!(u.node_kind, UnitKind::Clap) && (!multi || u.rig as usize == rig))
            .map(|u| u.node_id).ok_or_else(|| format!("rig {rig} has no CLAP synth"))?;
        let slot = self.clap_rigs().into_iter().find(|(_, n)| *n == node).map(|(s, _)| s)
            .ok_or_else(|| "that synth has no voice slot".to_string())?;
        let is_dexed = self.clap_pool_idx(slot)
            .and_then(|i| self.clap_plugin_ids.get(i))
            .map(|id| id == DEXED_ID)
            .unwrap_or(false);
        if is_dexed {
            let words: Vec<String> = query.trim().to_lowercase().split_whitespace().map(str::to_string).collect();
            let patch = self.dexed_patches().iter().find(|p| {
                let hay = format!("{}/{}", p.bank, p.name).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            }).cloned().ok_or_else(|| format!("no Dexed patch matches '{query}'"))?;
            return self.load_dexed_patch_into_slot(slot, &patch);
        }
        let patch = {
            let ps = self.surge_patches();
            crate::host::find_surge_patch(ps, query).cloned().ok_or_else(|| format!("no Surge patch matches '{query}'"))?
        };
        self.load_patch_into_slot(slot, &patch)
    }

    /// Load one patch file into voice `slot` (agent load_sound and the synth
    /// editor's Sound browser share this). Records the patch name and, if
    /// that synth's editor is open, re-reads its parameters.
    fn load_patch_into_slot(&mut self, slot: usize, patch: &crate::host::SurgePatch) -> Result<String, String> {
        let data = std::fs::read(&patch.path).map_err(|e| format!("reading patch: {e}"))?;
        let state = crate::host::fxp_to_state(&data).map_err(|e| e.to_string())?;
        let idx = self.clap_pool_idx(slot).ok_or_else(|| "no audio engine (headless)".to_string())?;
        let main = self._surge_main.get_mut(idx).ok_or_else(|| "no audio engine (headless)".to_string())?;
        main.load_state(&state).map_err(|e| e.to_string())?;
        if let Some(n) = self.patch_names.get_mut(slot) { *n = Some(patch.name.clone()); }
        if self.editor_open && self.editor_rig == slot { self.refresh_editor_params(); }
        Ok(format!("{}/{}", patch.category, patch.name))
    }

    /// Load a DX7 `.syx` cartridge patch into a Dexed voice slot.
    fn load_dexed_patch_into_slot(&mut self, slot: usize, patch: &crate::host::DexedPatch) -> Result<String, String> {
        let idx = self.clap_pool_idx(slot).ok_or_else(|| "no audio engine (headless)".to_string())?;
        let main = self._surge_main.get_mut(idx).ok_or_else(|| "no audio engine (headless)".to_string())?;
        crate::host::load_dexed_patch(main, patch).map_err(|e| e.to_string())?;
        if let Some(n) = self.patch_names.get_mut(slot) { *n = Some(patch.name.clone()); }
        if self.editor_open && self.lv2_editor_node.is_none() && self.editor_rig == slot {
            self.refresh_editor_params();
        }
        Ok(format!("{}/{}", patch.bank, patch.name))
    }

    /// Names worth highlighting in chat replies: rigs ("Rig A", "Rig 1",
    /// "Rack 1"), units ("Surge B", "Delay"), and effect kinds.
    fn chat_entities(&self) -> Vec<String> {
        let mut e: Vec<String> = Vec::new();
        for r in 0..self.rig_count() {
            e.push(self.rig_name(r));
            e.push(format!("Rig {}", r + 1));
            e.push(format!("Rack {}", r + 1));
        }
        for u in &self.units { e.push(sentence_case(&u.name)); }
        for w in ["Surge", "Reverb", "Delay", "Filter", "Gain", "EQ", "Limiter", "Saturation", "Panner", "Metronome", "Master", "Keys", "Keyboard"] {
            e.push(w.into());
        }
        e.retain(|s| s.chars().count() >= 2);
        e.sort_by(|a, b| b.len().cmp(&a.len())); // longest first
        e.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        e
    }

    /// Short state header prepended to each chat message so the agent works
    /// where the user is (the rig in view) without asking.
    fn chat_context(&self) -> String {
        let n = self.rig_count();
        let units: Vec<String> = self.units.iter().filter(|u| n <= 1 || u.rig as usize == self.active_rig)
            .map(|u| format!("{} (node {})", sentence_case(&u.name), u.node_id)).collect();
        let rigs: Vec<String> = (0..n).map(|r| format!("{} · {}", r + 1, self.rig_name(r))).collect();
        format!("[context: rig in view = {} · {} (index {}), units: {} · all rigs: {}]",
            self.active_rig + 1, self.rig_name(self.active_rig), self.active_rig, units.join(", "), rigs.join(", "))
    }

    /// Chat submit: `/model` commands stay local; everything else goes to the
    /// agent with the state header.
    fn submit_chat(&mut self, t: String) {
        if let Some(rest) = t.strip_prefix("/model") {
            let m = rest.trim();
            let line = if m.is_empty() {
                let perceive = crate::llm::Route::configured(crate::llm::Role::Perceive).model;
                format!("model: {} · available: {} · perception model: {perceive}", self.assistant.model(), self.assistant.models.join(", "))
            } else { self.switch_model(m) };
            self.assistant.history.push(ChatLine { role: "system".into(), text: line });
            return;
        }
        let msg = format!("{}\n{}", self.chat_context(), t);
        self.undo_job = None; // a new request = a new undo step
        self.pending_confirm = None; // a typed answer replaces the buttons
        self.assistant.send_user_display(t, msg);
    }

    fn switch_model(&mut self, m: &str) -> String {
        self.assistant.set_model(m)
    }

    /// Every rig for the agent: index, name, device, units (node ids), mix.
    fn rigs_json(&self) -> serde_json::Value {
        let rigs: Vec<serde_json::Value> = (0..self.rig_count()).map(|r| {
            let m = self.rig_mix[r.min(7)];
            let units: Vec<serde_json::Value> = self.units.iter().filter(|u| u.rig as usize == r)
                .map(|u| serde_json::json!({"node": u.node_id, "name": u.name})).collect();
            serde_json::json!({
                "rig": r, "name": self.rig_name(r), "device": self.rig_device_label(r),
                "units": units, "vol": m.vol, "pan": m.pan, "mute": m.mute, "solo": m.solo,
            })
        }).collect();
        serde_json::json!({"rigs": rigs, "active": self.active_rig, "max_rigs": Self::MAX_RIGS})
    }

    /// Most rigs the UI manages (the per-rig mixer mirror is `[RigMix; 8]`).
    const MAX_RIGS: usize = 8;

    /// A fresh rig's patch: Keys→Surge→Master (stereo) while a pooled synth
    /// voice is free (VOICE_POOL), else Keys→Master — a skeleton the user
    /// fills from the Plugins browser.
    fn new_rig_graph(&self) -> Graph {
        let claps = self.units.iter().filter(|u| matches!(u.node_kind, UnitKind::Clap)).count();
        let voices = if self._surge_main.is_empty() { VOICE_POOL } else { self._surge_main.len() };
        crate::control::standard_rig_graph(claps < voices)
    }

    /// Add a rig (`None`) or delete rig `Some(r)`, keeping every surviving
    /// rig's patch, knob values, synth state, device binding and mixer.
    /// The live patch is split per rig (rig-relative ids, rule 4), edited,
    /// re-merged and sent to the engine as ONE LoadRigs.
    fn edit_rigs(&mut self, delete: Option<usize>) { self.edit_rigs_named(delete, None); }

    /// `edit_rigs` with an explicit name for an added rig (agent `add_rig`).
    fn edit_rigs_named(&mut self, delete: Option<usize>, add_name: Option<&str>) {
        self.edit_rigs_with(delete, add_name, None);
    }

    /// `edit_rigs_named`; an added rig is built around `plugin` = (CLAP id,
    /// display name) instead of the default Surge rig.
    fn edit_rigs_with(&mut self, delete: Option<usize>, add_name: Option<&str>, plugin: Option<(&str, &str)>) {
        let n = self.rig_count();
        match delete {
            None if n >= Self::MAX_RIGS => {
                self.session_msg = format!("At most {} rigs", Self::MAX_RIGS);
                return;
            }
            Some(r) if r >= n || n <= 1 => return,
            _ => {}
        }
        let s = match self.capture_session("Rigs") {
            Ok(s) => s,
            Err(e) => { self.session_msg = format!("Rig edit failed: {e}"); return; }
        };
        let (graphs, old_bases) = crate::control::split_rigs(&s.graph);
        let devices: Vec<Option<u32>> = {
            let snap = self.rig_snapshot.lock().ok();
            (0..graphs.len()).map(|i| snap.as_ref().and_then(|sn| sn.rigs.get(i).and_then(|m| m.midi_device))).collect()
        };
        // (old rig index, rig) — None marks the brand-new rig.
        let mut rigs: Vec<(Option<usize>, crate::control::Rig)> = graphs.into_iter().enumerate()
            .map(|(i, g)| (Some(i), crate::control::Rig::new(self.rig_name(i), g, devices[i])))
            .collect();
        let msg = match delete {
            Some(r) => {
                let name = rigs[r].1.name.clone();
                rigs.remove(r);
                format!("Deleted {name}")
            }
            None => {
                let used: Vec<String> = rigs.iter().map(|(_, r)| r.name.clone()).collect();
                let name = add_name.map(|s| s.trim().chars().take(24).collect::<String>()).filter(|s| !s.is_empty())
                    .or_else(|| (b'A'..=b'Z').map(|c| format!("Rig {}", c as char)).find(|n| !used.contains(n)))
                    .unwrap_or_else(|| format!("Rig {}", n + 1));
                let graph = match plugin {
                    Some((id, label)) => crate::control::plugin_rig_graph(id, label),
                    None => self.new_rig_graph(),
                };
                rigs.push((None, crate::control::Rig::new(name.clone(), graph, None)));
                format!("Added {name}")
            }
        };
        let list: Vec<crate::control::Rig> = rigs.iter().map(|(_, r)| r.clone()).collect();
        let new_bases = crate::control::rig_bases(&list);
        let merged = crate::control::merge_rigs(&list);
        // Old absolute node id → new absolute id (None: the node's rig is gone).
        let remap = |old: u32| -> Option<u32> {
            let o = s.graph.node(old)?.rig as usize;
            let i = rigs.iter().position(|(k, _)| *k == Some(o))?;
            Some(new_bases[i] + (old - old_bases[o]))
        };
        let ns = remap_session(&s, merged, remap);
        // Mixer follows its rig; the new rig starts at unity (applied after
        // the reload — build_rack resets the mixer).
        let mut mix = [crate::engine::RigMix::DEFAULT; 8];
        for (i, (k, _)) in rigs.iter().enumerate() {
            mix[i] = k.map_or(crate::engine::RigMix::DEFAULT, |o| self.rig_mix[o.min(7)]);
        }
        let active = match delete {
            None => rigs.len() - 1,
            Some(r) if self.active_rig > r => self.active_rig - 1,
            Some(_) => self.active_rig.min(rigs.len() - 1),
        };
        self.load_as_rigs = Some((list.clone(), active));
        if let Err(e) = self.apply_session(&ns) {
            self.session_msg = format!("Rig edit failed: {e}");
            return;
        }
        // The engine republishes this on LoadRigs; mirror it now so names and
        // devices are right on the very next frame (and without an engine).
        if let Ok(mut snap) = self.rig_snapshot.lock() {
            snap.rigs = list.iter().map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device }).collect();
            snap.active = active;
        }
        self.adopt_rig_devices();
        self.rig_mix = mix;
        self.active_rig = usize::MAX; // force set_active_rig to regroup
        self.set_active_rig(active);
        for i in 0..list.len() { self.send_rig_mix(i); }
        self.session_msg = msg;
        self.editor_status.clear(); // not "rack reloaded" noise for a rig edit
    }

    /// "Delete rig?" confirmation (destructive: the rig's units, cables and
    /// synth state go away). Cancel / Esc keep everything.
    fn delete_rig_modal(&mut self, ctx: &egui::Context) {
        let Some(rig) = self.confirm_delete_rig else { return };
        if rig >= self.rig_count() || self.rig_count() <= 1 { self.confirm_delete_rig = None; return; }
        let name = self.rig_name(rig);
        let units = self.units.iter().filter(|u| u.rig as usize == rig).count();
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::unique("delete_rig_modal")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(egui::RichText::new(format!("Delete {name}?")).size(17.0).strong().color(theme::INK));
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!(
                "Its {units} units, their cables and synth settings are removed. The other rigs keep playing."))
                .size(13.0).color(theme::INK_2));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Delete rig").clicked() { decision = Some(true); }
                ui.add_space(8.0);
                if theme::chip_button(ui, "Cancel", false).clicked() { decision = Some(false); }
            });
        });
        if modal.should_close() && decision.is_none() { decision = Some(false); }
        match decision {
            Some(true) => { self.confirm_delete_rig = None; self.edit_rigs(Some(rig)); }
            Some(false) => self.confirm_delete_rig = None,
            None => {}
        }
    }

    /// Rename rig `rig` (#33): the engine owns the name (RenameRig, published
    /// with the rig snapshot); the snapshot is mirrored at once so chips,
    /// mixer and get_rigs show it on the next frame (rule 25).
    fn rename_rig(&mut self, rig: usize, name: &str) -> Result<String, String> {
        let name: String = name.trim().chars().take(24).collect();
        if name.is_empty() { return Err("a rig name cannot be empty".into()); }
        let n = self.rig_count();
        if rig >= n { return Err(format!("no rig {rig}: there are {n}")); }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::RenameRig { rig: rig as u8, name: name_bytes(&name) });
        }
        if let Ok(mut snap) = self.rig_snapshot.lock() {
            while snap.rigs.len() < n {
                let i = snap.rigs.len();
                snap.rigs.push(crate::control::RigMeta { name: format!("Rig {}", (b'A' + i.min(25) as u8) as char), midi_device: None });
            }
            snap.rigs[rig].name = name.clone();
        }
        Ok(name)
    }

    /// "Rename rig" dialog (double-click a rig chip): Enter saves, Esc cancels.
    fn rename_rig_modal(&mut self, ctx: &egui::Context) {
        let Some((rig, mut text)) = self.rename_rig_dialog.take() else { return };
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new("rename_rig_modal")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(egui::RichText::new("Rename rig").size(17.0).strong().color(theme::INK));
            ui.add_space(8.0);
            let edit = ui.add(egui::TextEdit::singleline(&mut text).id_salt("rename_rig_text").desired_width(300.0).char_limit(24));
            if !edit.has_focus() && !edit.lost_focus() { edit.request_focus(); }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) { decision = Some(true); }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Rename").clicked() { decision = Some(true); }
                ui.add_space(8.0);
                if theme::chip_button(ui, "Cancel", false).clicked() { decision = Some(false); }
            });
        });
        if modal.should_close() && decision.is_none() { decision = Some(false); }
        match decision {
            Some(true) => { let _ = self.rename_rig(rig, &text); }
            Some(false) => {}
            None => self.rename_rig_dialog = Some((rig, text)),
        }
    }

    #[cfg(test)]
    fn default_rig_presets_dir() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("test-rig-presets").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[cfg(not(test))]
    fn default_rig_presets_dir() -> std::path::PathBuf {
        crate::session::rig_presets_dir()
    }

    /// Snapshot ONE rig (units, cables, knobs, plugin states, matrix routes,
    /// mix, name, device, sources, patch names) with rig-relative node ids.
    fn capture_rig_preset(&mut self, rig: usize, preset_name: &str) -> anyhow::Result<crate::session::RigPreset> {
        let n = self.rig_count();
        if rig >= n {
            anyhow::bail!("no rig {rig}: there are {n} (0..{})", n.saturating_sub(1));
        }
        let s = self.capture_session(preset_name)?;
        let (graphs, bases) = crate::control::split_rigs(&s.graph);
        let rig_graph = graphs.get(rig).cloned()
            .ok_or_else(|| anyhow::anyhow!("rig {rig} has no graph"))?;
        let base = bases[rig];
        let mut rig_session = remap_session(&s, rig_graph, |old| {
            let node = s.graph.node(old)?;
            if node.rig as usize == rig { Some(old - base) } else { None }
        });
        rig_session.rig_names.clear();
        let midi_device = self.rig_snapshot.lock().ok()
            .and_then(|sn| sn.rigs.get(rig).and_then(|m| m.midi_device));
        let mut clap_meta = Vec::new();
        for (slot, nid) in self.clap_rigs() {
            if self.units.iter().find(|u| u.node_id == nid).is_some_and(|u| u.rig as usize == rig) {
                let (keyboard, hardware) = self.rig_sources.get(slot).copied().unwrap_or((true, true));
                let patch_name = self.patch_names.get(slot).and_then(|p| p.clone());
                clap_meta.push(crate::session::RigClapMeta {
                    node: nid - base,
                    keyboard,
                    hardware,
                    patch_name,
                });
            }
        }
        let mut preset = crate::session::RigPreset::new(preset_name.trim(), self.rig_name(rig), rig_session);
        preset.mix = self.rig_mix[rig.min(7)];
        preset.midi_device = midi_device;
        preset.clap_meta = clap_meta;
        preset.validate()?;
        Ok(preset)
    }

    /// Load a single-rig preset either as a new rig (`target_rig = None`) or
    /// replacing `Some(target_rig)`, keeping every other rig's state intact
    /// and remapping all node-id keyed data through `remap_session`.
    fn apply_rig_preset(
        &mut self,
        preset: &crate::session::RigPreset,
        target_rig: Option<usize>,
    ) -> anyhow::Result<usize> {
        preset.validate()?;
        let n = self.rig_count();
        match target_rig {
            None if n >= Self::MAX_RIGS => anyhow::bail!("at most {} rigs", Self::MAX_RIGS),
            Some(t) if t >= n => anyhow::bail!("no rig {t}: there are {n}"),
            _ => {}
        }

        let current = self.capture_session("Rigs")?;
        let (existing_graphs, old_bases) = crate::control::split_rigs(&current.graph);
        let existing_devices: Vec<Option<u32>> = self.rig_snapshot.lock().ok()
            .map(|sn| (0..existing_graphs.len()).map(|i| sn.rigs.get(i).and_then(|m| m.midi_device)).collect())
            .unwrap_or_else(|| vec![None; existing_graphs.len()]);
        let existing_names: Vec<String> = (0..existing_graphs.len()).map(|i| self.rig_name(i)).collect();
        let existing_mix = self.rig_mix;

        let mut old_sources_by_node = std::collections::HashMap::new();
        let mut old_patches_by_node = std::collections::HashMap::new();
        for (slot, nid) in self.clap_rigs() {
            old_sources_by_node.insert(nid, self.rig_sources.get(slot).copied().unwrap_or((true, true)));
            if let Some(Some(p)) = self.patch_names.get(slot) {
                old_patches_by_node.insert(nid, p.clone());
            }
        }

        #[derive(Clone, Copy)]
        enum RigSlot { Existing(usize), Preset }
        let mut slots: Vec<RigSlot> = (0..existing_graphs.len()).map(RigSlot::Existing).collect();
        let active = match target_rig {
            None => { slots.push(RigSlot::Preset); slots.len() - 1 }
            Some(t) => { slots[t] = RigSlot::Preset; t }
        };

        let preset_name = if preset.rig_name.trim().is_empty() {
            preset.name.clone()
        } else {
            preset.rig_name.clone()
        };
        let list: Vec<crate::control::Rig> = slots.iter().map(|slot| match *slot {
            RigSlot::Existing(o) => crate::control::Rig::new(
                existing_names[o].clone(),
                existing_graphs[o].clone(),
                existing_devices[o],
            ),
            RigSlot::Preset => crate::control::Rig::new(
                preset_name.clone(),
                preset.session.graph.clone(),
                preset.midi_device,
            ),
        }).collect();

        let new_bases = crate::control::rig_bases(&list);
        let merged = crate::control::merge_rigs(&list);
        let preset_slot = slots.iter().position(|s| matches!(s, RigSlot::Preset)).unwrap();
        let mut existing_to_new: Vec<Option<usize>> = vec![None; existing_graphs.len()];
        for (new_i, slot) in slots.iter().enumerate() {
            if let RigSlot::Existing(o) = *slot {
                existing_to_new[o] = Some(new_i);
            }
        }
        let remap_existing = |old: u32| -> Option<u32> {
            let o = current.graph.node(old)?.rig as usize;
            let new_i = *existing_to_new.get(o)?.as_ref()?;
            Some(new_bases[new_i] + (old - old_bases[o]))
        };
        let remap_preset = |old: u32| -> Option<u32> {
            if preset.session.graph.node(old).is_some() {
                Some(new_bases[preset_slot] + old)
            } else {
                None
            }
        };

        let ns_existing = remap_session(&current, merged.clone(), remap_existing);
        let ns_preset = remap_session(&preset.session, merged.clone(), remap_preset);
        let mut combined = crate::session::Session::new("Rigs", merged);
        combined.rig_names = list.iter().map(|r| r.name.clone()).collect();
        for src_sess in [&ns_existing, &ns_preset] {
            for p in src_sess.params() {
                combined.set_param(p.node, p.param, p.value);
            }
            for st in src_sess.plugin_states() {
                combined.set_plugin_state(st.node, &st.plugin_id, st.data.clone());
            }
        }
        let mut rules: Vec<SavedRule> = Vec::new();
        for src_sess in [&ns_existing, &ns_preset] {
            if let Some(v) = src_sess.matrix() {
                if let Ok(r) = serde_json::from_value::<Vec<SavedRule>>(v.clone()) {
                    rules.extend(r);
                }
            }
        }
        if !rules.is_empty() {
            combined.set_matrix(serde_json::to_value(&rules).ok());
        }

        let mut mix = [crate::engine::RigMix::DEFAULT; 8];
        for (i, slot) in slots.iter().enumerate() {
            mix[i] = match *slot {
                RigSlot::Existing(o) => existing_mix[o.min(7)],
                RigSlot::Preset => preset.mix,
            };
        }
        let mut new_sources_by_node = std::collections::HashMap::new();
        let mut new_patches_by_node = std::collections::HashMap::new();
        for (old_nid, src) in old_sources_by_node {
            if let Some(new_nid) = remap_existing(old_nid) {
                new_sources_by_node.insert(new_nid, src);
            }
        }
        for (old_nid, patch) in old_patches_by_node {
            if let Some(new_nid) = remap_existing(old_nid) {
                new_patches_by_node.insert(new_nid, patch);
            }
        }
        for m in &preset.clap_meta {
            if let Some(new_nid) = remap_preset(m.node) {
                new_sources_by_node.insert(new_nid, (m.keyboard, m.hardware));
                if let Some(p) = &m.patch_name {
                    new_patches_by_node.insert(new_nid, p.clone());
                }
            }
        }

        self.load_as_rigs = Some((list.clone(), active));
        self.apply_session(&combined)?;
        if let Ok(mut snap) = self.rig_snapshot.lock() {
            snap.rigs = list.iter().map(|r| crate::control::RigMeta {
                name: r.name.clone(),
                midi_device: r.midi_device,
            }).collect();
            snap.active = active;
        }
        self.adopt_rig_devices();
        // Restore per-voice source switches and patch names AFTER apply_session (rule 20/59).
        for (slot, nid) in self.clap_rigs() {
            if let Some(&src) = new_sources_by_node.get(&nid) {
                if let Some(s) = self.rig_sources.get_mut(slot) {
                    *s = src;
                }
                self.send_rig_source(nid);
            }
            if let Some(p) = new_patches_by_node.get(&nid) {
                if let Some(pn) = self.patch_names.get_mut(slot) {
                    *pn = Some(p.clone());
                }
            }
        }
        self.rig_mix = mix;
        self.active_rig = usize::MAX;
        self.set_active_rig(active);
        for i in 0..list.len() {
            self.ui_mix_grace[i.min(7)] = self.ui_time + 0.6;
            self.send_rig_mix(i);
        }
        self.editor_status.clear();
        Ok(active)
    }

    fn save_rig_preset_to_dir(&mut self, dir: &std::path::Path, rig: usize, name: &str) -> anyhow::Result<std::path::PathBuf> {
        let preset = self.capture_rig_preset(rig, name)?;
        let path = crate::session::RigPreset::package_path(dir, name)?;
        preset.save(&path)?;
        Ok(path)
    }

    fn load_rig_preset_from_dir(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        target_rig: Option<usize>,
    ) -> anyhow::Result<usize> {
        let path = crate::session::RigPreset::package_path(dir, name)?;
        let preset = crate::session::RigPreset::load(&path)?;
        self.apply_rig_preset(&preset, target_rig)
    }

    /// "Save rig preset" dialog: Enter saves, Esc cancels.
    fn save_rig_modal(&mut self, ctx: &egui::Context) {
        let Some((rig, mut text)) = self.save_rig_dialog.take() else { return };
        if rig >= self.rig_count() { return; }
        let mut decision: Option<bool> = None;
        let rig_label = self.rig_name(rig);
        let modal = egui::Modal::new(egui::Id::unique("save_rig_modal")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(egui::RichText::new(format!("Save rig {} · {}", rig + 1, rig_label)).size(17.0).strong().color(theme::INK));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Saves units, cables, knobs, sound, matrix routes and mix.").size(12.5).color(theme::INK_2));
            ui.add_space(8.0);
            let edit = ui.add(egui::TextEdit::singleline(&mut text).id_salt("save_rig_text").desired_width(320.0).char_limit(48));
            if !edit.has_focus() && !edit.lost_focus() { edit.request_focus(); }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) { decision = Some(true); }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Save preset").clicked() { decision = Some(true); }
                ui.add_space(8.0);
                if theme::chip_button(ui, "Cancel", false).clicked() { decision = Some(false); }
            });
        });
        if modal.should_close() && decision.is_none() { decision = Some(false); }
        match decision {
            Some(true) => {
                let dir = self.rig_presets_dir.clone();
                match self.save_rig_preset_to_dir(&dir, rig, &text) {
                    Ok(_) => self.session_msg = format!("Saved rig preset '{}'", text.trim()),
                    Err(e) => self.session_msg = format!("Save rig failed: {e}"),
                }
            }
            Some(false) => {}
            None => self.save_rig_dialog = Some((rig, text)),
        }
    }

    /// "Load rig preset" dialog: load as a new rig or replace an existing one.
    fn load_rig_modal(&mut self, ctx: &egui::Context) {
        let Some(mut target) = self.load_rig_dialog.take() else { return };
        let n = self.rig_count();
        if target.is_some_and(|t| t >= n) { target = None; }
        let dir = self.rig_presets_dir.clone();
        let presets = crate::session::list_rig_presets(&dir);
        let mut close = false;
        let mut load_name: Option<String> = None;
        let mut delete_name: Option<String> = None;
        let modal = egui::Modal::new(egui::Id::unique("load_rig_modal")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(egui::RichText::new("Load rig preset").size(17.0).strong().color(theme::INK));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                if n < Self::MAX_RIGS && theme::chip_button(ui, "As new rig", target.is_none()).clicked() {
                    target = None;
                }
                for r in 0..n {
                    let lbl = format!("Replace {} · {}", r + 1, self.rig_name(r));
                    if theme::chip_button(ui, &lbl, target == Some(r)).clicked() {
                        target = Some(r);
                    }
                }
            });
            ui.add_space(10.0);
            Self::stage_hairline(ui);
            ui.add_space(8.0);
            let list_h = 220.0;
            egui::ScrollArea::vertical()
                .id_salt("rig_presets_list_scroll")
                .min_scrolled_height(list_h)
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if presets.is_empty() {
                        ui.label(
                            egui::RichText::new("No saved rig presets yet — click Save rig to save one.")
                                .size(13.0)
                                .color(theme::INK_2),
                        );
                        return;
                    }
                    for (i, name) in presets.iter().enumerate() {
                        if i > 0 { Self::stage_hairline(ui); }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(name).font(theme::medium_font(14.0)).color(theme::INK));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if Self::danger_button(ui, "Delete", false).clicked() {
                                    delete_name = Some(name.clone());
                                }
                                ui.add_space(6.0);
                                if theme::primary_button(ui, "Load").clicked() {
                                    load_name = Some(name.clone());
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }
                });
            ui.add_space(10.0);
            if theme::chip_button(ui, "Close", false).clicked() { close = true; }
        });
        if modal.should_close() { close = true; }
        if let Some(name) = delete_name {
            let _ = crate::session::delete_rig_preset(&dir, &name);
        }
        if let Some(name) = load_name {
            match self.load_rig_preset_from_dir(&dir, &name, target) {
                Ok(slot) => self.session_msg = format!("Loaded rig '{name}' into {}", self.rig_name(slot)),
                Err(e) => self.session_msg = format!("Load rig failed: {e}"),
            }
            return;
        }
        if !close {
            self.load_rig_dialog = Some(target);
        }
    }

    /// Multi-rig overview strip: always visible above the rack.
    /// Every control (RIGS label, "+ Rig" button, separator, rig chips, delete
    /// button, source label, level bar) shares one 26px centered baseline.
    fn rig_overview_panel(&mut self, root: &mut egui::Ui) {
        // Always shown (2026-11-15, user): the strip owns "+ Rig" and the
        // per-rig delete, so a single-rig rack can grow into several.
        const STRIP_ROW_H: f32 = 26.0;
        let n = self.rig_count();
        let mut switch: Option<usize> = None;
        let mut add_rig = false;
        self.rig_chip_rects.clear();
        self.add_rig_rect = Rect::NOTHING;
        egui::Panel::top("rig_overview")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(root, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), STRIP_ROW_H),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        theme::section_label(ui, "Rigs");
                        // "+ Rig" sits up front so it never scrolls out of reach.
                        if n < Self::MAX_RIGS {
                            let add = theme::add_rig_button(ui, "Rig")
                                .on_hover_text("add a rig: its own keys, synth and master — all rigs sound together");
                            self.add_rig_rect = add.rect;
                            if add.clicked() { add_rig = true; }
                        }
                        nav_separator(ui);
                        let scroll_w = ui.available_width().max(1.0);
                        egui::ScrollArea::horizontal()
                            .id_salt("rig_strip")
                            .max_width(scroll_w)
                            .max_height(STRIP_ROW_H)
                            .show(ui, |ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(scroll_w, STRIP_ROW_H),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        for rig in 0..n.min(9) {
                                            if rig > 0 {
                                                ui.add_space(4.0);
                                                nav_separator(ui);
                                                ui.add_space(4.0);
                                            }
                                            let active = rig == self.active_rig;
                                            let name = self.rig_name(rig);
                                            let dev = self.rig_device_label(rig);
                                            let level = self.rig_levels.get(rig).copied().unwrap_or(0.0);
                                            let chip = theme::chip_button(ui, &format!("{} · {}", rig + 1, name), active);
                                            let agent_t = self.agent_activity.rig_alpha(rig, self.ui_time);
                                            if agent_t > 0.01 {
                                                ui.painter().rect_stroke(
                                                    chip.rect.expand(1.5),
                                                    3.0,
                                                    Stroke::new(1.5, theme::alpha(theme::AGENT_COLOR, agent_t)),
                                                    egui::StrokeKind::Outside,
                                                );
                                            }
                                            if chip.clicked() { switch = Some(rig); }
                                            if chip.double_clicked() { self.rename_rig_dialog = Some((rig, name.clone())); }
                                            chip.context_menu(|ui| {
                                                if ui.button("Rename…").clicked() {
                                                    self.rename_rig_dialog = Some((rig, name.clone()));
                                                    ui.close();
                                                }
                                                if ui.button("Save rig preset…").clicked() {
                                                    self.save_rig_dialog = Some((rig, name.clone()));
                                                    ui.close();
                                                }
                                                if ui.button("Replace from preset…").clicked() {
                                                    self.load_rig_dialog = Some(Some(rig));
                                                    ui.close();
                                                }
                                                if n > 1 && ui.button("Delete rig…").clicked() {
                                                    self.confirm_delete_rig = Some(rig);
                                                    ui.close();
                                                }
                                            });
                                            self.rig_chip_rects.push(chip.rect);
                                            if n > 1 {
                                                let (dr, dresp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
                                                let hov = dresp.hovered();
                                                if hov {
                                                    ui.painter().rect_filled(dr, 3.0, theme::DARK_PANEL_HI);
                                                    ui.painter().rect_stroke(dr, 3.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
                                                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                                }
                                                ui.painter().text(dr.center(), egui::Align2::CENTER_CENTER, "×", FontId::proportional(15.0),
                                                    if hov { theme::CLIP_BRIGHT } else { theme::INK_2 });
                                                if dresp.on_hover_text(format!("Delete {name}… (asks first)")).clicked() {
                                                    self.confirm_delete_rig = Some(rig);
                                                }
                                            }
                                            ui.add_space(2.0);
                                            theme::tracked_label(ui, &dev, 9.0, 0.14, theme::INK_2);
                                            ui.add_space(2.0);
                                            let (r, _) = ui.allocate_exact_size(egui::vec2(36.0, 3.0), egui::Sense::hover());
                                            let p = ui.painter();
                                            p.rect_filled(r, 1.0, theme::HAIRLINE);
                                            let w = 36.0 * level.clamp(0.0, 1.0);
                                            if w > 0.5 {
                                                p.rect_filled(Rect::from_min_size(r.min, egui::vec2(w, 3.0)), 1.0, theme::LIVE_TEAL);
                                            }
                                        }
                                        ui.add_space(4.0);
                                        nav_separator(ui);
                                        ui.add_space(4.0);
                                        if theme::chip_button(ui, "Save rig", false)
                                            .on_hover_text("save the active rig as a reusable preset")
                                            .clicked()
                                        {
                                            let r = self.active_rig;
                                            self.save_rig_dialog = Some((r, self.rig_name(r)));
                                        }
                                        if theme::chip_button(ui, "Load rig", false)
                                            .on_hover_text("load a saved rig preset (new rig or replace existing)")
                                            .clicked()
                                        {
                                            self.load_rig_dialog = Some(None);
                                        }
                                        if n > 1 {
                                            ui.add_space(8.0);
                                            let keys = if n.min(9) == 2 { "keys 1–2".to_string() } else { format!("keys 1–{}", n.min(9)) };
                                            ui.label(egui::RichText::new(format!("{keys} or arrow keys switch the view · all rigs keep sounding"))
                                                .size(11.0).color(theme::INK_2));
                                        }
                                    },
                                );
                            });
                    },
                );
            });
        if let Some(r) = switch { self.set_active_rig(r); }
        if add_rig { self.edit_rigs(None); }
    }

    /// AUM-style per-rig input selection: which sources feed each rig.
    /// Hurricane top strip (§4.14): paper, plain-text toggles, hairline nav.
    fn rig_inputs_panel(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("rig_inputs")
            .default_size(46.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    // Brand in the top bar (user 2026-11-15: the RIG INPUTS
                    // chips are not needed here — the logo goes in their place,
                    // without the play button). Per-synth Keyboard/Hardware
                    // routing lives in the MIDI matrix (same toggles).
                    ui.add_space(4.0);
                    // Narrow window (< 1000 px): drop the wordmark text and the
                    // group captions so REC/SYNC never run past the window
                    // edge; the framed groups still say what belongs together.
                    // The WINDOW width decides, never ui.available_width(): the
                    // latter follows the bar's previous-frame width, so the mode
                    // flipped between frames and buttons jumped under the mouse.
                    let compact = ui.ctx().content_rect().width() < 1000.0;
                    let (mark, _) = ui.allocate_exact_size(egui::vec2(18.0, 22.0), egui::Sense::hover());
                    ui.add_space(4.0);
                    if compact {
                        theme::crossed_wires(ui.painter(), mark.center(), 16.0, theme::PAPER, theme::MarkStyle::Mono);
                    } else if let Some(g) = self.topbar_word_g.clone() {
                        let (r, _) = ui.allocate_exact_size(g.size, egui::Sense::hover());
                        theme::paint_tracked(ui.painter(), r.min, &g, theme::INK);
                        // Optical alignment: center the mark on the wordmark's
                        // LETTER BODY (baseline − half the cap height), not on
                        // the galley box — the box includes line spacing, which
                        // left the mark ~2px high (user: "not properly aligned").
                        let base = g.chars.first()
                            .and_then(|(c, _)| c.rows.first().and_then(|row| row.glyphs.first().map(|gl| row.pos.y + gl.pos.y)))
                            .unwrap_or(g.size.y * 0.8);
                        let body_mid = r.top() + base - 20.0 * 0.36;
                        theme::crossed_wires(ui.painter(), Pos2::new(mark.center().x, body_mid), 16.0, theme::PAPER, theme::MarkStyle::Mono);
                    }
                    ui.add_space(14.0);
                    ui.add_space(6.0);
                    if !self.editor_open && !self.editor_status.is_empty() {
                        ui.label(egui::RichText::new(&self.editor_status).size(12.0).color(theme::ACCENT_DEEP));
                        ui.add_space(8.0);
                    }
                    nav_separator(ui);
                    ui.add_space(6.0);
                    // Two explicit groups (user): the VIEW decides what the
                    // main stage shows — exactly one, Rack included — while
                    // the BOTTOM PANEL toggles what docks under it.
                    if !compact { theme::section_label(ui, "View"); }
                    ui.add_space(2.0);
                    let views = [
                        ("Rack", StageView::Rack),
                        ("Sessions", StageView::Sessions),
                        ("Plugins", StageView::Plugins),
                        ("Matrix", StageView::Matrix),
                        ("Mixer", StageView::Mixer),
                    ];
                    let mut pick = None;
                    let group = ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (name, view) in views {
                            if nav_button(ui, name, self.stage_view == view) { pick = Some(view); }
                        }
                    }).response.rect;
                    ui.painter().rect_stroke(group.expand(3.0), 4.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Outside);
                    self.view_group_rect = group;
                    if let Some(view) = pick {
                        if view == StageView::Rack || view != self.stage_view { self.toggle_stage(view); }
                    }
                    ui.add_space(12.0);
                    nav_separator(ui);
                    ui.add_space(10.0);
                    if !compact { theme::section_label(ui, "Bottom panel"); }
                    ui.add_space(2.0);
                    let mut dock = None;
                    let group = ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if nav_button(ui, "Keyboard", self.keyboard_open()) { dock = Some(DockView::Keyboard); }
                        if nav_button(ui, "Assistant", self.assistant_open()) { dock = Some(DockView::Assistant); }
                    }).response.rect;
                    ui.painter().rect_stroke(group.expand(3.0), 4.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Outside);
                    self.dock_group_rect = group;
                    if let Some(d) = dock { self.toggle_dock(d); }
                    // External agent (MCP --attach): visible while it acts,
                    // one click pauses it (Bravo item 7, without a per-start
                    // consent prompt — the user drives the app from outside).
                    {
                        use std::sync::atomic::Ordering::Relaxed;
                        let paused = crate::bridge::REMOTE_PAUSED.load(Relaxed);
                        let last = crate::bridge::LAST_REMOTE_MS.load(Relaxed);
                        let recent = last > 0 && crate::bridge::now_ms().saturating_sub(last) < 15_000;
                        if paused || recent {
                            ui.add_space(10.0);
                            let (label, tip) = match (paused, compact) {
                                (true, false) => ("Agent paused", "external agents (MCP) may not change this rack — click to allow"),
                                (true, true) => ("Paused", "external agents (MCP) may not change this rack — click to allow"),
                                (false, false) => ("● Agent connected", "an external agent (MCP) is working on this rack — click to pause it"),
                                (false, true) => ("●", "an external agent (MCP) is working on this rack — click to pause it"),
                            };
                            let r = theme::chip_button(ui, label, paused).on_hover_text(tip);
                            if !paused {
                                ui.painter().rect_stroke(r.rect, 2.0, Stroke::new(1.2, theme::AGENT_COLOR), egui::StrokeKind::Inside);
                            }
                            self.agent_chip_rect = r.rect;
                            if r.clicked() { crate::bridge::REMOTE_PAUSED.store(!paused, Relaxed); }
                        }
                    }
                    // Master recording (GitHub #6): a REC chip (tomato only
                    // while recording) + a sync-to-transport toggle + elapsed
                    // time while active. Start/stop go through the command bus.
                    ui.add_space(6.0);
                    nav_separator(ui);
                    ui.add_space(6.0);
                    let recording = self.recorder.as_ref().map(|r| r.is_recording()).unwrap_or(false);
                    let snap = self.recorder.as_ref().map(|r| r.snapshot());
                    let secs = snap.as_ref().map(|s| s.seconds).unwrap_or(0.0);
                    let rec_err = snap.as_ref().and_then(|s| s.error.clone());
                    if rec_chip(ui, recording, secs).on_hover_text("record the master output to ~/Music/kabelsalat/").clicked() {
                        self.toggle_recording();
                    }
                    if let Some(err) = rec_err {
                        ui.label(egui::RichText::new(err).size(11.0).color(theme::ACCENT_DEEP))
                            .on_hover_text("recording failed to start or write");
                    }
                    ui.add_space(6.0);
                    let sync = self.record_sync;
                    if theme::chip_button(ui, "SYNC", sync)
                        .on_hover_text("start/stop recording with the master transport (Space)")
                        .clicked()
                    {
                        self.record_sync = !sync;
                        self.ui_transport_grace = self.ui_time + 0.6;
                        if let Some(bus) = &self.bus {
                            let _ = bus.lock().unwrap().send(Command::SetRecordSync { on: self.record_sync });
                        }
                    }
                    // Last take (user: "what does record even do?"): after a
                    // stop, name the file and offer Play (through the file
                    // player, into the master) and the folder.
                    if !recording {
                        if let Some(path) = snap.as_ref().filter(|s| s.takes > 0 && s.finalized).and_then(|s| s.path.clone()) {
                            ui.add_space(6.0);
                            let name = std::path::Path::new(&path).file_name()
                                .map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
                            ui.label(egui::RichText::new(format!("saved {name}")).size(11.5).color(theme::INK_2))
                                .on_hover_text(path.clone());
                            let playing = self.take_play_until > self.ui_time;
                            if theme::chip_button(ui, if playing { "■ Stop" } else { "▶ Play" }, playing)
                                .on_hover_text("play the last take through the master")
                                .clicked()
                            {
                                self.toggle_take_playback(&path, secs);
                            }
                            if theme::chip_button(ui, "Folder", false).on_hover_text("open the recordings folder").clicked() {
                                open_folder(&crate::record::record_dir());
                            }
                        }
                    }
                    // MIDI + performance info IN the page (I toggles): right-aligned bar text.
                    // Too little room left in the bar (narrow window): the line
                    // moves to the rack's hint row instead of overflowing.
                    self.info_in_hint = self.show_stats && ui.available_width() < 140.0;
                    if self.info_in_hint { let _ = self.stats_text(); }
                    if self.show_stats && !self.info_in_hint {
                        let line = self.stats_text();
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(6.0);
                            // Truncated to the room left in the bar (it ran over the
                            // nav buttons on narrow windows); the hover has it all.
                            ui.add(egui::Label::new(egui::RichText::new(&line).monospace().size(11.0).color(theme::INK_2)).truncate())
                                .on_hover_text(format!("{line}\n(I hides)"));
                        });
                    }
                });
            });
    }

    fn recessed_input(ui: &mut egui::Ui, text: &mut String, width: f32, hint: &str) -> egui::Response {
        egui::Frame::new()
            .fill(theme::PAPER_SINK)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(2.0)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(text)
                        .frame(egui::Frame::NONE)
                        .desired_width(width)
                        .hint_text(hint),
                )
            })
            .inner
    }

    fn danger_button(ui: &mut egui::Ui, text: &str, filled: bool) -> egui::Response {
        ui.scope(|ui| {
            let (fill, stroke, txt) = if filled {
                (theme::CLIP_RED, theme::CLIP_RED, theme::ON_ACCENT)
            } else {
                (theme::DARK_PANEL_HI, theme::HAIRLINE, theme::CLIP_BRIGHT)
            };
            let base = egui::style::WidgetVisuals {
                bg_fill: fill,
                weak_bg_fill: fill,
                bg_stroke: Stroke::new(1.0, stroke),
                corner_radius: 2.0.into(),
                fg_stroke: Stroke::new(1.0, txt),
                expansion: 0.0,
            };
            let hot_fill = if filled {
                theme::mix(theme::CLIP_RED, theme::INK, 0.08)
            } else {
                theme::mix(theme::DARK_PANEL_HI, theme::CLIP_RED, 0.14)
            };
            let hot = egui::style::WidgetVisuals {
                bg_fill: hot_fill,
                weak_bg_fill: hot_fill,
                bg_stroke: Stroke::new(1.0, if filled { hot_fill } else { theme::CLIP_RED }),
                ..base
            };
            let v = ui.visuals_mut();
            v.widgets.inactive = base;
            v.widgets.hovered = hot;
            v.widgets.active = hot;
            v.widgets.open = base;
            ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
            ui.add(
                egui::Button::new(egui::RichText::new(text).size(12.5).color(txt))
                    .corner_radius(2.0)
                    .min_size(egui::vec2(0.0, 26.0)),
            )
        })
        .inner
    }

    fn stage_hairline(ui: &mut egui::Ui) {
        let w = ui.available_width().max(1.0);
        let (r, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0, theme::HAIRLINE));
    }

    fn save_current_session(&mut self, dir: &std::path::Path) {
        let name = if self.session_name.trim().is_empty() {
            "Untitled".to_string()
        } else {
            self.session_name.trim().to_string()
        };
        match self.capture_session(&name) {
            Ok(s) => {
                let path = crate::session::Session::package_path(dir, &name);
                match path.and_then(|p| s.save(&p).map(|_| p)) {
                    Ok(_) => {
                        self.confirm_delete_session = None;
                        self.session_msg = format!("Saved '{name}'");
                    }
                    Err(e) => self.session_msg = format!("Save failed: {e}"),
                }
            }
            Err(e) => self.session_msg = format!("Capture failed: {e}"),
        }
    }

    fn sessions_stage(&mut self, ui: &mut egui::Ui) {
        let dir = Self::sessions_dir();
        self.sessions_stage_in(ui, &dir);
    }

    /// Main-stage view for sessions and starter templates (one-window rule).
    fn sessions_stage_in(&mut self, ui: &mut egui::Ui, dir: &std::path::Path) {
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter().clone();
        theme::paper_bg(&painter, rect);
        let (sheet, inner) = self.stage_sheet_rects(rect, 200.0);
        painter.rect_filled(sheet, 28.0, theme::DARK_PANEL);
        let help = self.show_help;
        let names = Self::list_saved_sessions(dir);

        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.add_enabled_ui(!help, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Sessions").font(theme::medium_font(18.0)).color(theme::INK));
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("save and load patches · starter templates · Tab / Esc: back to the rack")
                            .size(13.0)
                            .color(theme::INK_2),
                    );
                    if !self.session_msg.is_empty() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let col = if self.session_msg.contains("failed") {
                                theme::CLIP_BRIGHT
                            } else {
                                theme::LIVE_TEAL
                            };
                            ui.label(egui::RichText::new(&self.session_msg).size(12.5).color(col));
                        });
                    }
                });
                ui.add_space(12.0);
                Self::stage_hairline(ui);
                ui.add_space(14.0);

                let avail_w = ui.available_width();
                let h = ui.available_height().max(80.0);
                let two_col = avail_w >= 680.0;
                let left_w = if two_col { (avail_w * 0.40).clamp(270.0, 410.0) } else { avail_w };
                let right_w = if two_col { (avail_w - left_w - 25.0).max(240.0) } else { avail_w };

                let mut load_session: Option<String> = None;
                let mut delete_session: Option<String> = None;
                let mut do_save = false;
                let mut load_tpl: Option<Template> = None;
                let mut load_demo = false;
                let mut load_multi = false;

                let draw_left = |ui: &mut egui::Ui,
                                 session_name: &mut String,
                                 do_save: &mut bool,
                                 load_tpl: &mut Option<Template>,
                                 load_demo: &mut bool,
                                 load_multi: &mut bool| {
                    theme::section_label(ui, "Save current patch");
                    ui.add_space(8.0);
                    let field_w = (ui.available_width() - 12.0).max(120.0);
                    let edit = Self::recessed_input(ui, session_name, field_w, "Session name…");
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        *do_save = true;
                    }
                    ui.add_space(8.0);
                    if theme::primary_button(ui, "Save current patch").clicked() {
                        *do_save = true;
                    }
                    ui.add_space(16.0);
                    Self::stage_hairline(ui);
                    ui.add_space(14.0);
                    theme::section_label(ui, "Starter templates");
                    ui.add_space(8.0);

                    let template_row = |ui: &mut egui::Ui, title: &str, desc: &str, hover: &str| -> bool {
                        let mut clicked = false;
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(title).font(theme::medium_font(13.5)).color(theme::INK));
                                ui.label(egui::RichText::new(desc).size(11.5).color(theme::INK_2));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::chip_button(ui, "Load", false).on_hover_text(hover).clicked() {
                                    clicked = true;
                                }
                            });
                        });
                        clicked
                    };

                    for (idx, t) in Template::ALL.iter().enumerate() {
                        if idx > 0 {
                            ui.add_space(6.0);
                            Self::stage_hairline(ui);
                            ui.add_space(6.0);
                        }
                        let (desc, key_hint) = match t {
                            Template::LeadReverb => ("Keys → Surge → Reverb", "F5"),
                            Template::DrumsBass => ("Dual Surge layers", "F6"),
                            Template::AmbientPad => ("Surge → Delay → Reverb", "F7"),
                        };
                        if template_row(ui, t.title(), &format!("{desc} · {key_hint}"), "load a playable pre-wired rack") {
                            *load_tpl = Some(*t);
                        }
                    }
                    ui.add_space(6.0);
                    Self::stage_hairline(ui);
                    ui.add_space(6.0);
                    if template_row(ui, "Demo patch (2 stacks)", "2 Surge + Delay stacks · F8", "restore the two-stack demo patch") {
                        *load_demo = true;
                    }
                    ui.add_space(6.0);
                    Self::stage_hairline(ui);
                    ui.add_space(6.0);
                    if template_row(
                        ui,
                        "Multi-rig demo (2 rigs)",
                        "2 independent rigs, 1–9 switch · F4",
                        "two independent rigs, both sounding — switch the view with 1–9 / ←→ (F4)",
                    ) {
                        *load_multi = true;
                    }
                };

                let draw_right = |ui: &mut egui::Ui,
                                  names: &[String],
                                  confirm_delete: &mut Option<String>,
                                  load_session: &mut Option<String>,
                                  delete_session: &mut Option<String>,
                                  list_h: f32| {
                    theme::section_label(ui, &format!("Saved sessions ({})", names.len()));
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .id_salt("sessions_list_scroll")
                        .min_scrolled_height(list_h)
                        .max_height(list_h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if names.is_empty() {
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new("No saved sessions yet — name your patch on the left and click Save.")
                                        .size(13.0)
                                        .color(theme::INK_2),
                                );
                                return;
                            }
                            for (i, name) in names.iter().enumerate() {
                                if i > 0 {
                                    Self::stage_hairline(ui);
                                }
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!("{:02}", i + 1))
                                            .font(theme::medium_font(12.5))
                                            .color(theme::INK_2),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new(name)
                                            .font(theme::medium_font(14.5))
                                            .color(theme::INK),
                                    );
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        let confirming = confirm_delete.as_deref() == Some(name.as_str());
                                        if confirming {
                                            if theme::chip_button(ui, "Cancel", false).clicked() {
                                                *confirm_delete = None;
                                            }
                                            ui.add_space(6.0);
                                            if Self::danger_button(ui, "Confirm delete", true).clicked() {
                                                *delete_session = Some(name.clone());
                                            }
                                        } else {
                                            if Self::danger_button(ui, "Delete", false)
                                                .on_hover_text(format!("delete '{name}' from disk"))
                                                .clicked()
                                            {
                                                *confirm_delete = Some(name.clone());
                                            }
                                            ui.add_space(6.0);
                                            if theme::primary_button(ui, "Load").clicked() {
                                                *load_session = Some(name.clone());
                                            }
                                        }
                                    });
                                });
                                ui.add_space(8.0);
                            }
                        });
                };

                if two_col {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(left_w, h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("sessions_left_scroll")
                                    .min_scrolled_height(h)
                                    .max_height(h)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        draw_left(
                                            ui,
                                            &mut self.session_name,
                                            &mut do_save,
                                            &mut load_tpl,
                                            &mut load_demo,
                                            &mut load_multi,
                                        );
                                    });
                            },
                        );
                        ui.add_space(12.0);
                        let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, h), egui::Sense::hover());
                        ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE));
                        ui.add_space(12.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(right_w, h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                let list_h = (h - 24.0).max(64.0);
                                draw_right(
                                    ui,
                                    &names,
                                    &mut self.confirm_delete_session,
                                    &mut load_session,
                                    &mut delete_session,
                                    list_h,
                                );
                            },
                        );
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("sessions_narrow_scroll")
                        .min_scrolled_height(h)
                        .max_height(h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            draw_left(
                                ui,
                                &mut self.session_name,
                                &mut do_save,
                                &mut load_tpl,
                                &mut load_demo,
                                &mut load_multi,
                            );
                            ui.add_space(14.0);
                            Self::stage_hairline(ui);
                            ui.add_space(14.0);
                            draw_right(
                                ui,
                                &names,
                                &mut self.confirm_delete_session,
                                &mut load_session,
                                &mut delete_session,
                                200.0,
                            );
                        });
                }

                if do_save {
                    self.save_current_session(dir);
                }
                if let Some(t) = load_tpl {
                    self.load_template(t);
                }
                if load_demo {
                    self.load_demo_patch();
                }
                if load_multi {
                    self.load_multi_rig_demo();
                }
                if let Some(name) = load_session {
                    self.confirm_delete_session = None;
                    let path = crate::session::Session::package_path(dir, &name);
                    match path.and_then(|p| crate::session::Session::load(&p)) {
                        Ok(s) => match self.apply_session(&s) {
                            Ok(_) => self.session_msg = format!("Loaded '{name}'"),
                            Err(e) => self.session_msg = format!("Apply failed: {e}"),
                        },
                        Err(e) => self.session_msg = format!("Load failed: {e}"),
                    }
                }
                if let Some(name) = delete_session {
                    self.confirm_delete_session = None;
                    match Self::delete_saved_session(dir, &name) {
                        Ok(()) => self.session_msg = format!("Deleted '{name}'"),
                        Err(e) => self.session_msg = format!("Delete failed: {e}"),
                    }
                }
            });
        });

        if self.show_help { self.draw_help(&painter, rect); }
    }

    // -- plugin browser (task-18) -------------------------------------------

    /// Scan installed CLAP + LV2 plugins. Filesystem / lilv work — done only on
    /// open / refresh, never on the per-frame path.
    fn refresh_plugins(&mut self) {
        self.refresh_plugins_with(false);
    }

    /// `retry_failed`: rescan CLAP bundles whose failure is cached (Refresh).
    fn refresh_plugins_with(&mut self, retry_failed: bool) {
        self.start_plugin_scan(retry_failed);
        self.poll_plugin_scan();
    }

    /// Discover CLAP + LV2 plugins off the UI thread (inline under test).
    fn start_plugin_scan(&mut self, retry_failed: bool) {
        let scan = move || {
            let clap = if retry_failed { crate::host::list_clap_plugins_retry_failed() } else { list_clap_plugins() };
            (clap, lv2_discover())
        };
        if cfg!(test) {
            (self.plugins_clap, self.plugins_lv2) = scan();
            self.plugins_loaded = true;
            return;
        }
        if self.plugin_scan.is_some() && !retry_failed {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("kabelsalat-plugin-scan".into())
            .spawn(move || { let _ = tx.send(scan()); })
            .expect("spawn plugin scan");
        self.plugin_scan = Some(rx);
    }

    /// Plugin lists for UI-thread callers (agent tools): never scan here —
    /// a cold CLAP scan blocked the UI for up to 5 s per wave (#15). Starts
    /// the background scan if needed; false = not ready yet, ask again.
    fn plugin_lists_ready(&mut self) -> bool {
        self.poll_plugin_scan();
        if !self.plugins_loaded { self.start_plugin_scan(false); self.poll_plugin_scan(); }
        self.plugins_loaded
    }

    /// Adopt a finished background scan (cheap; called every frame).
    fn poll_plugin_scan(&mut self) {
        let Some(rx) = &self.plugin_scan else { return };
        match rx.try_recv() {
            Ok((clap, lv2)) => {
                self.plugins_clap = clap;
                self.plugins_lv2 = lv2;
                self.plugins_loaded = true;
                self.plugin_scan = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.plugin_scan = None,
        }
    }

    /// Distill the installed-plugin scan into the catalog's `ScanSet` (owned,
    /// so `catalog` stays free of `host`/`lv2` types).
    fn catalog_scan(&self) -> crate::catalog::ScanSet {
        crate::catalog::ScanSet {
            clap_ids: self.plugins_clap.iter().map(|p| p.id.clone()).collect(),
            lv2_uris: self.plugins_lv2.iter().map(|p| p.uri.clone()).collect(),
        }
    }

    /// Bundle-probe roots: the test override when set, else the real home +
    /// system dirs.
    fn catalog_root_paths(&self) -> Vec<std::path::PathBuf> {
        self.catalog_roots.clone()
            .unwrap_or_else(|| crate::catalog::plugin_roots(&crate::catalog::home_dir()))
    }

    /// Is a catalog entry installed (scan evidence OR a bundle path on disk)?
    fn catalog_installed(&self, entry: &crate::catalog::CatalogEntry) -> bool {
        let roots = self.catalog_root_paths();
        let root_refs: Vec<&std::path::Path> = roots.iter().map(|p| p.as_path()).collect();
        crate::catalog::installed(entry, &self.catalog_scan(), &root_refs)
    }

    /// Start installing a catalog instrument on a worker thread (rule 46:
    /// never pacman/git/cmake on the UI thread). Streams a log the Plugins
    /// view shows; a finished install triggers a plugin rescan.
    fn start_install(&mut self, id: &str) -> Result<(), String> {
        let entry = crate::catalog::find(id).ok_or_else(|| format!("no catalog instrument '{id}' (see list_instruments)"))?;
        if let Some(inst) = &self.install {
            if inst.running {
                return Err(format!("'{}' is already installing — wait for it to finish", inst.id));
            }
        }
        let cache = crate::catalog::cache_root();
        let home = crate::catalog::home_dir();
        if cfg!(test) {
            // Tests never run subprocesses (rules 37/48): record the request.
            // `poll_install` then deactivates it, so a later frame is idle.
            self.install = Some(InstallState {
                id: id.to_string(),
                rx: None,
                log: vec![format!("(test) would install '{}' — no subprocess runs here", id)],
                running: true,
                error: None,
            });
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_id = id.to_string();
        std::thread::Builder::new()
            .name(format!("kabelsalat-install-{thread_id}"))
            .spawn(move || {
                let mut log = |line: String| { let _ = tx.send(InstallEvent::Log(line)); };
                let res = crate::catalog::run_install(entry, &cache, &home, &mut log);
                let _ = tx.send(InstallEvent::Done(res));
            })
            .map_err(|e| format!("spawn install: {e}"))?;
        self.install = Some(InstallState {
            id: id.to_string(),
            rx: Some(rx),
            log: Vec::new(),
            running: true,
            error: None,
        });
        Ok(())
    }

    /// Adopt install-worker progress (cheap; called every frame). On success or
    /// failure the worker is dropped and a plugin rescan picks up new bundles.
    fn poll_install(&mut self) {
        let Some(inst) = &mut self.install else { return };
        let mut done: Option<Result<(), String>> = None;
        if let Some(rx) = &inst.rx {
            loop {
                match rx.try_recv() {
                    Ok(InstallEvent::Log(line)) => inst.log.push(line),
                    Ok(InstallEvent::Done(res)) => { done = Some(res); break; }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = Some(Err("install worker stopped without a result".into()));
                        break;
                    }
                }
            }
        } else if inst.running && cfg!(test) {
            // The test-stub install finishes on the next poll.
            done = Some(Ok(()));
        }
        if let Some(res) = done {
            inst.rx = None;
            inst.running = false;
            match res {
                Ok(()) => {}
                Err(e) => inst.error = Some(e),
            }
            let keep = 64;
            if inst.log.len() > keep {
                let drain = inst.log.len() - keep;
                inst.log.drain(..drain);
            }
            // Rule 46: rescan off-thread so a fresh setBfree/YANES shows up.
            self.start_plugin_scan(true);
        }
    }

    /// The proven LV2 effect chain: Keys → Surge → <lv2> → Master (+ dry path).
    /// Mirrors engine::lv2_engine_test.
    fn build_lv2_chain_graph(uri: &str, name: &str) -> Graph {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_ID) }, name_bytes("SURGE"));
        let fx = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name_bytes(name));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, CPortKind::Midi);
        g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: fx, port: 0 }, CPortKind::Audio);
        g.connect_with_id(3, PortRef { node: fx, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
        g.connect_with_id(4, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, CPortKind::Audio);
        g
    }

    /// Add an LV2 effect: rebuild the rack as Keys→Surge→effect→Master and
    /// rewire the engine (same path as loading a template).
    fn add_lv2_effect(&mut self, uri: &str, name: &str) {
        let g = Self::build_lv2_chain_graph(uri, name);
        let s = crate::session::Session::new(format!("LV2: {name}"), g);
        match self.apply_session(&s) {
            Ok(_) => self.plugin_msg = format!("Added '{name}' — Keys→Surge→{name}→Master"),
            Err(e) => self.plugin_msg = format!("Add failed: {e}"),
        }
    }

    /// The LV2 instrument rig: Keys → <lv2 instrument> → Master. Mirrors
    /// engine::lv2_instrument_engine_test (MIDI to the instrument, audio out).
    fn build_lv2_instrument_graph(uri: &str, name: &str) -> Graph {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let inst = g.add_node(NodeKind::Lv2 { uri: Lv2Uri::new(uri) }, name_bytes(name));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: inst, port: 0 }, CPortKind::Midi);
        g.connect_with_id(2, PortRef { node: inst, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
        g
    }

    /// Add an LV2 instrument as a rig: rebuild the rack as Keys→LV2→Master and
    /// rewire the engine (same apply_session path as add-effect).
    fn add_lv2_instrument(&mut self, uri: &str, name: &str) {
        let g = Self::build_lv2_instrument_graph(uri, name);
        let s = crate::session::Session::new(format!("LV2 inst: {name}"), g);
        match self.apply_session(&s) {
            Ok(_) => self.plugin_msg = format!("Added '{name}' as instrument — Keys→{name}→Master"),
            Err(e) => self.plugin_msg = format!("Add failed: {e}"),
        }
    }

    /// Start a fresh single-instrument CLAP rig for any CLAP plugin.
    fn add_clap_rig(&mut self, plugin_id: &str, name: &str) {
        let g = crate::control::plugin_rig_graph(plugin_id, name);
        let s = crate::session::Session::new(&format!("{name} rig"), g);
        match self.apply_session(&s) {
            Ok(_) => self.plugin_msg = format!("New {name} rig — Keys→{name}→Master"),
            Err(e) => self.plugin_msg = format!("Add failed: {e}"),
        }
    }

    /// Add a Metronome node to the active rig and wire it to that rig's MasterOut.
    fn add_metronome_to_active_rig(&mut self) {
        let rig = self.active_rig as u8;
        let master = self.units.iter()
            .find(|u| matches!(u.node_kind, UnitKind::Master) && (self.rig_count() <= 1 || u.rig == rig))
            .map(|u| u.node_id);
        let mut g = self.base_graph.clone();
        // Rebuild edges from living cables so user rewires survive.
        g.edges.clear();
        for c in &self.cables {
            if c.dying >= 0.0 { continue; }
            if let (Some(sn), Some(dn)) = (self.units.get(c.src).map(|u| u.node_id), self.units.get(c.dst).map(|u| u.node_id)) {
                let kind = match c.kind { PortKind::Midi => CPortKind::Midi, PortKind::Audio => CPortKind::Audio };
                g.connect_with_id(c.id, PortRef { node: sn, port: c.src_port as u8 }, PortRef { node: dn, port: c.dst_port as u8 }, kind);
            }
        }
        let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
        g.set_rig(metro, rig);
        if let Some(m) = master {
            let eid = g.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1;
            g.connect_with_id(eid, PortRef { node: metro, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
        }
        let keep_rig = self.active_rig;
        let keep_mix = self.rig_mix;
        self.load_graph_into_engine(&g);
        self.build_rack(&g, &[]);
        self.rig_mix = keep_mix;
        self.set_active_rig(keep_rig);
        self.plugin_msg = "Added Metronome — wired to Master".into();
    }

    /// Start a fresh single-Surge rig.
    fn add_surge_rig(&mut self) {
        self.add_clap_rig(SURGE_ID, "Surge");
    }

    fn plugin_card(ui: &mut egui::Ui, width: f32, body: impl FnOnce(&mut egui::Ui, f32)) {
        let inner_w = (width - 30.0).max(160.0);
        egui::Frame::new()
            .fill(theme::DARK_PANEL_HI)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(inner_w, 92.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(inner_w);
                        ui.set_min_height(92.0);
                        body(ui, inner_w);
                    },
                );
            });
    }

    /// A taller [`plugin_card`] for catalog entries: name, one-line
    /// description, repo link and an Install button / Installed badge.
    fn catalog_card(ui: &mut egui::Ui, width: f32, body: impl FnOnce(&mut egui::Ui, f32)) {
        let inner_w = (width - 30.0).max(160.0);
        egui::Frame::new()
            .fill(theme::DARK_PANEL_HI)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(inner_w, 118.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(inner_w);
                        ui.set_min_height(118.0);
                        body(ui, inner_w);
                    },
                );
            });
    }

    /// Main-stage plugin browser (one-window rule, F9 / B / Plugins chip).
    fn plugins_stage(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter().clone();
        theme::paper_bg(&painter, rect);
        let (sheet, inner) = self.stage_sheet_rects(rect, 200.0);
        painter.rect_filled(sheet, 28.0, theme::DARK_PANEL);
        let help = self.show_help;

        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.add_enabled_ui(!help, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Plugins").font(theme::medium_font(18.0)).color(theme::INK));
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("CLAP instruments, LV2 plugins and built-ins · Tab / Esc: back to the rack")
                            .size(13.0)
                            .color(theme::INK_2),
                    );
                    if !self.plugin_msg.is_empty() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let col = if self.plugin_msg.contains("failed") {
                                theme::CLIP_BRIGHT
                            } else {
                                theme::LIVE_TEAL
                            };
                            ui.label(egui::RichText::new(&self.plugin_msg).size(12.5).color(col));
                        });
                    }
                });
                ui.add_space(12.0);

                let mut do_refresh = false;
                ui.horizontal_wrapped(|ui| {
                    let search_w = (ui.available_width() * 0.28).clamp(180.0, 320.0);
                    Self::recessed_input(ui, &mut self.plugin_filter, search_w, "Search plugins by name or vendor…");
                    if !self.plugin_filter.is_empty() && theme::chip_button(ui, "Clear", false).clicked() {
                        self.plugin_filter.clear();
                    }
                    ui.add_space(6.0);
                    for (cat, label) in [
                        (PluginCategory::All, "All".to_string()),
                        (PluginCategory::BuiltIn, "Built-in (1)".to_string()),
                        (PluginCategory::Clap, format!("CLAP ({})", self.plugins_clap.len())),
                        (PluginCategory::Lv2, format!("LV2 ({})", self.plugins_lv2.len())),
                        (PluginCategory::Install, format!("Install ({})", crate::catalog::CATALOG.len())),
                    ] {
                        if theme::chip_button(ui, &label, self.plugin_category == cat).clicked() {
                            self.plugin_category = cat;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::chip_button(ui, "Refresh", false)
                            .on_hover_text("rescan, retrying plugins that failed before")
                            .clicked()
                        {
                            do_refresh = true;
                        }
                    });
                });
                if do_refresh {
                    self.refresh_plugins_with(true);
                }

                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Adding a CLAP or LV2 plugin rebuilds the current patch · Metronome adds to the active rig")
                        .size(12.0)
                        .color(theme::INK_2),
                );
                if self.plugin_scan.is_some() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Scanning plugins…").size(12.0).color(theme::CREAM_DIM));
                }
                ui.add_space(10.0);
                Self::stage_hairline(ui);
                ui.add_space(12.0);

                let filter = self.plugin_filter.trim().to_lowercase();
                let list_h = ui.available_height().max(80.0);
                let mut add_metro = false;
                let mut add_clap: Option<(String, String)> = None;
                let mut add_lv2: Option<(String, String, bool)> = None;
                let mut install_request: Option<String> = None;

                egui::ScrollArea::vertical()
                    .id_salt("plugins_scroll")
                    .min_scrolled_height(list_h)
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let avail_w = (ui.available_width() - 14.0).max(200.0);
                        let gap = 12.0;
                        let cols = ((avail_w + gap) / 340.0).floor().clamp(1.0, 3.0) as usize;
                        let card_w = ((avail_w - gap * (cols as f32 - 1.0)) / cols as f32).max(200.0);

                        if self.plugin_category == PluginCategory::Install {
                            let roots_raw = self.catalog_root_paths();
                            let roots: Vec<&std::path::Path> = roots_raw.iter().map(|p| p.as_path()).collect();
                            let scan = self.catalog_scan();
                            let matches: Vec<&crate::catalog::CatalogEntry> = crate::catalog::CATALOG
                                .iter()
                                .filter(|e| {
                                    filter.is_empty()
                                        || e.name.to_lowercase().contains(&filter)
                                        || e.description.to_lowercase().contains(&filter)
                                        || e.id.to_lowercase().contains(&filter)
                                        || e.repo.to_lowercase().contains(&filter)
                                })
                                .collect();

                            theme::section_label(ui, &format!("Install from the catalog ({})", matches.len()));
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("Hand-picked instruments from GitHub — a distro package where one exists, else built into ~/.clap or ~/.lv2")
                                    .size(12.0)
                                    .color(theme::INK_2),
                            );
                            ui.add_space(10.0);

                            if let Some(inst) = &self.install {
                                if !inst.log.is_empty() {
                                    Self::stage_hairline(ui);
                                    ui.add_space(10.0);
                                    ui.horizontal(|ui| {
                                        theme::section_label(ui, "Install log");
                                        if let Some(err) = &inst.error {
                                            ui.add_space(8.0);
                                            ui.label(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT));
                                        } else if inst.running {
                                            ui.add_space(8.0);
                                            ui.label(egui::RichText::new("running…").size(11.5).color(theme::AGENT_COLOR));
                                        } else {
                                            ui.add_space(8.0);
                                            ui.label(egui::RichText::new("done — plugins rescanned").size(11.5).color(theme::LIVE_TEAL));
                                        }
                                    });
                                    ui.add_space(4.0);
                                    let tail = inst.log.iter().rev().take(10).rev().cloned().collect::<Vec<_>>();
                                    let body = if tail.is_empty() { "…".to_string() } else { tail.join("\n") };
                                    ui.label(egui::RichText::new(body).size(11.0).monospace().color(theme::INK_2));
                                    ui.add_space(10.0);
                                }
                            }

                            if matches.is_empty() {
                                ui.add_space(12.0);
                                ui.label(egui::RichText::new("No catalog entries match.").size(13.0).color(theme::INK_2));
                                return;
                            }

                            for row in matches.chunks(cols) {
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = gap;
                                    for e in row {
                                        let is_inst = crate::catalog::installed(e, &scan, &roots);
                                        let active = self.install.as_ref().filter(|i| i.id == e.id);
                                        let running = active.is_some_and(|i| i.running);
                                        let failed = active.and_then(|i| i.error.as_deref());
                                        Self::catalog_card(ui, card_w, |ui, iw| {
                                            ui.horizontal(|ui| {
                                                ui.set_width(iw);
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    theme::tracked_label(ui, e.format.label(), 9.5, 0.12, theme::ACCENT_BRIGHT);
                                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                        ui.add(
                                                            egui::Label::new(
                                                                egui::RichText::new(e.name)
                                                                    .font(theme::medium_font(14.0))
                                                                    .color(theme::INK),
                                                            )
                                                            .truncate(),
                                                        );
                                                    });
                                                });
                                            });
                                            ui.add_space(4.0);
                                            ui.add(
                                                egui::Label::new(egui::RichText::new(e.description).size(12.0).color(theme::INK_2))
                                                    .truncate(),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(format!("github.com/{}", e.repo))
                                                        .size(11.0)
                                                        .color(theme::MIDI_BLUE),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(crate::catalog::repo_url(e.repo));
                                            ui.add_space(10.0);
                                            if is_inst {
                                                ui.label(egui::RichText::new("Installed").size(11.0).color(theme::LIVE_TEAL));
                                            } else if running {
                                                ui.label(egui::RichText::new("Installing…").size(12.0).color(theme::AGENT_COLOR));
                                            } else if let Some(err) = failed {
                                                ui.label(egui::RichText::new("Install failed").size(12.0).color(theme::CLIP_BRIGHT))
                                                    .on_hover_text(err);
                                            } else if theme::primary_button(ui, "Install")
                                                .on_hover_text(crate::catalog::repo_url(e.repo))
                                                .clicked()
                                            {
                                                install_request = Some(e.id.to_string());
                                            }
                                        });
                                    }
                                });
                                ui.add_space(gap);
                            }
                            return;
                        }

                        let show_builtin = matches!(self.plugin_category, PluginCategory::All | PluginCategory::BuiltIn)
                            && (filter.is_empty() || "metronome".contains(&filter) || "tempo tap count-in".contains(&filter));
                        let clap_matches: Vec<&ClapPluginInfo> = if matches!(self.plugin_category, PluginCategory::All | PluginCategory::Clap) {
                            self.plugins_clap
                                .iter()
                                .filter(|p| {
                                    filter.is_empty()
                                        || p.name.to_lowercase().contains(&filter)
                                        || p.vendor.to_lowercase().contains(&filter)
                                        || p.id.to_lowercase().contains(&filter)
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let lv2_matches: Vec<&Lv2PluginInfo> = if matches!(self.plugin_category, PluginCategory::All | PluginCategory::Lv2) {
                            self.plugins_lv2
                                .iter()
                                .filter(|p| {
                                    filter.is_empty()
                                        || p.name.to_lowercase().contains(&filter)
                                        || p.uri.to_lowercase().contains(&filter)
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };

                        if !show_builtin && clap_matches.is_empty() && lv2_matches.is_empty() {
                            ui.add_space(12.0);
                            let empty = if filter.is_empty() {
                                "No plugins found in this category.".to_string()
                            } else {
                                format!("No plugins match '{filter}'.")
                            };
                            ui.label(egui::RichText::new(empty).size(13.0).color(theme::INK_2));
                            return;
                        }

                        let mut need_divider = false;

                        let card_header = |ui: &mut egui::Ui, title: &str, badge: &str, badge_col: Color32, inner_w: f32| {
                            ui.horizontal(|ui| {
                                ui.set_width(inner_w);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    theme::tracked_label(ui, badge, 9.5, 0.12, badge_col);
                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(title)
                                                    .font(theme::medium_font(14.0))
                                                    .color(theme::INK),
                                            )
                                            .truncate(),
                                        );
                                    });
                                });
                            });
                        };

                        if show_builtin {
                            theme::section_label(ui, "Built-in");
                            ui.add_space(8.0);
                            Self::plugin_card(ui, card_w, |ui, iw| {
                                card_header(ui, "Metronome", "BUILT-IN", theme::LIVE_TEAL, iw);
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("tempo · tap · count-in").size(12.0).color(theme::INK_2));
                                ui.add_space(12.0);
                                if theme::primary_button(ui, "Add to rig").clicked() {
                                    add_metro = true;
                                }
                            });
                            need_divider = true;
                        }

                        if matches!(self.plugin_category, PluginCategory::All | PluginCategory::Clap)
                            && (!clap_matches.is_empty() || filter.is_empty())
                        {
                            if need_divider {
                                ui.add_space(14.0);
                                Self::stage_hairline(ui);
                                ui.add_space(14.0);
                            }
                            theme::section_label(ui, &format!("CLAP instruments ({})", clap_matches.len()));
                            ui.add_space(8.0);
                            if clap_matches.is_empty() {
                                ui.label(egui::RichText::new("No CLAP plugins found.").size(12.5).color(theme::INK_2));
                            } else {
                                for row in clap_matches.chunks(cols) {
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = gap;
                                        for p in row {
                                            Self::plugin_card(ui, card_w, |ui, iw| {
                                                card_header(ui, &p.name, "CLAP", theme::ACCENT_BRIGHT, iw);
                                                ui.add_space(4.0);
                                                let vendor = if !p.vendor.is_empty() && p.vendor != "?" {
                                                    p.vendor.as_str()
                                                } else {
                                                    "instrument plugin"
                                                };
                                                ui.add(
                                                    egui::Label::new(egui::RichText::new(vendor).size(12.0).color(theme::INK_2))
                                                        .truncate(),
                                                );
                                                ui.add_space(12.0);
                                                if theme::primary_button(ui, "New rig")
                                                    .on_hover_text(format!("{} · create Keys → {} → Master", p.id, p.name))
                                                    .clicked()
                                                {
                                                    add_clap = Some((p.id.clone(), p.name.clone()));
                                                }
                                            });
                                        }
                                    });
                                    ui.add_space(gap);
                                }
                            }
                            need_divider = true;
                        }

                        if matches!(self.plugin_category, PluginCategory::All | PluginCategory::Lv2)
                            && (!lv2_matches.is_empty() || filter.is_empty())
                        {
                            if need_divider {
                                ui.add_space(14.0);
                                Self::stage_hairline(ui);
                                ui.add_space(14.0);
                            }
                            theme::section_label(ui, &format!("LV2 plugins ({})", lv2_matches.len()));
                            ui.add_space(8.0);
                            if lv2_matches.is_empty() {
                                ui.label(egui::RichText::new("No LV2 plugins found.").size(12.5).color(theme::INK_2));
                            } else {
                                for row in lv2_matches.chunks(cols) {
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = gap;
                                        for p in row {
                                            Self::plugin_card(ui, card_w, |ui, iw| {
                                                let (badge, bcol) = if p.is_instrument {
                                                    ("INST", theme::LIVE_TEAL)
                                                } else {
                                                    ("FX", theme::INK_2)
                                                };
                                                card_header(ui, &p.name, badge, bcol, iw);
                                                ui.add_space(4.0);
                                                ui.add(
                                                    egui::Label::new(egui::RichText::new(&p.uri).size(11.0).color(theme::INK_2))
                                                        .truncate(),
                                                );
                                                ui.add_space(12.0);
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 6.0;
                                                    let inst_resp = if p.is_instrument {
                                                        theme::primary_button(ui, "Add instrument")
                                                    } else {
                                                        theme::chip_button(ui, "Add instrument", false)
                                                    };
                                                    if inst_resp.on_hover_text(&p.uri).clicked() {
                                                        add_lv2 = Some((p.uri.clone(), p.name.clone(), true));
                                                    }
                                                    let fx_resp = if p.is_instrument {
                                                        theme::chip_button(ui, "Add effect", false)
                                                    } else {
                                                        theme::primary_button(ui, "Add effect")
                                                    };
                                                    if fx_resp.on_hover_text(&p.uri).clicked() {
                                                        add_lv2 = Some((p.uri.clone(), p.name.clone(), false));
                                                    }
                                                });
                                            });
                                        }
                                    });
                                    ui.add_space(gap);
                                }
                            }
                        }
                    });

                if add_metro {
                    self.add_metronome_to_active_rig();
                }
                if let Some((id, name)) = add_clap {
                    self.add_clap_rig(&id, &name);
                }
                if let Some((uri, name, as_inst)) = add_lv2 {
                    if as_inst {
                        self.add_lv2_instrument(&uri, &name);
                    } else {
                        self.add_lv2_effect(&uri, &name);
                    }
                }
                if let Some(id) = install_request {
                    if let Err(e) = self.start_install(&id) {
                        self.plugin_msg = format!("Install: {e}");
                    }
                }
            });
        });

        if self.show_help { self.draw_help(&painter, rect); }
    }

    /// One bottom dock: exactly one occupant at a time (None / Keyboard /
    /// Assistant). The occupant owns the computer keyboard.
    fn dock_panel_ui(&mut self, root: &mut egui::Ui) {
        match self.dock {
            DockView::None => {
                self.dock_rect = Rect::NOTHING;
                self.assistant_rect = Rect::NOTHING;
                self.assistant_input_rect = Rect::NOTHING;
                self.dock_kb_rect = Rect::NOTHING;
            }
            DockView::Keyboard => self.keyboard_panel_ui(root),
            DockView::Assistant => self.assistant_panel_ui(root),
        }
    }

    /// Assistant dock occupant: a WIDE bottom panel — chat transcript +
    /// always-visible input on the left, live patch context & agent activity
    /// side column on the right when wide. `resizable(false)` with a
    /// deterministic `default_size` keeps the height stable frame to frame
    /// (the stored-size + per-frame `size_range` loop shrank it ~3px/frame).
    fn assistant_panel_ui(&mut self, root: &mut egui::Ui) {
        // The rack keeps a comfortable face height (2026-11-15, user: "racks
        // are squished when the agent loads"): the panel only gets the height
        // the rack can spare, within 150..=340 px — never the other way round.
        let max_h = (root.available_height() - self.rack_need_h()).clamp(150.0, 340.0);
        let mut starter_prompt: Option<String> = None;
        let mut submit_text: Option<String> = None;
        let mut clear_chat = false;
        let mut input_rect = Rect::NOTHING;
        let mut focus_chat = self.focus_chat_request;
        let confirm_choice: std::cell::Cell<Option<bool>> = std::cell::Cell::new(None);
        let resp = egui::Panel::bottom("dock")
            .default_size(max_h.min(260.0))
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(root, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 26.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Assistant").font(theme::medium_font(15.0)).color(theme::INK));
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("F12 hides").size(11.5).color(theme::INK_2));
                        if self.assistant.busy {
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new("● working").size(11.5).color(theme::AGENT_COLOR));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::chip_button(ui, "×", false).on_hover_text("hide the assistant (F12)").clicked() {
                                self.dock = DockView::None;
                            }
                            ui.add_space(8.0);
                            let cur = self.assistant.model();
                            let mut pick: Option<String> = None;
                            egui::ComboBox::from_id_salt("assistant_model")
                                .selected_text(egui::RichText::new(format!("model: {cur}")).size(12.0).color(theme::CREAM))
                                .show_ui(ui, |ui| {
                                    for m in self.assistant.models.clone() {
                                        if ui.selectable_label(m == cur, egui::RichText::new(&m).color(theme::CREAM)).clicked() { pick = Some(m); }
                                    }
                                }).response.on_hover_text("chat model — switch any time, the conversation stays");
                            if let Some(m) = pick { self.switch_model(&m); }
                            if !self.assistant.history.is_empty() && !self.assistant.busy {
                                ui.add_space(8.0);
                                if theme::chip_button(ui, "Clear", false).on_hover_text("clear visible transcript").clicked() {
                                    clear_chat = true;
                                }
                            }
                            if ui.available_width() > 360.0 {
                                if let Some(last) = self.agent_activity.log.front() {
                                    let active = AgentActivity::strength(Some(&(last.at + AGENT_TOUCH_SECS)), self.ui_time) > 0.01;
                                    if active {
                                        ui.add_space(10.0);
                                        ui.label(
                                            egui::RichText::new(format!("{} · {}", last.origin.tag(), last.summary))
                                                .size(11.5)
                                                .color(theme::AGENT_COLOR),
                                        );
                                    }
                                }
                            }
                        });
                    },
                );
                ui.add_space(6.0);
                Self::stage_hairline(ui);
                ui.add_space(6.0);

                let avail_w = ui.available_width();
                let avail_h = ui.available_height().max(40.0);
                let two_col = avail_w >= 860.0;
                let side_w = if two_col { (avail_w * 0.30).clamp(260.0, 360.0) } else { 0.0 };
                let main_w = if two_col { (avail_w - side_w - 25.0).max(340.0) } else { avail_w };
                let entities = self.chat_entities();
                let confirm_label = self.pending_confirm.as_ref().map(|p| p.label.clone());

                let draw_main = |ui: &mut egui::Ui,
                                 key_input: &mut String,
                                 chat_input: &mut String,
                                 assistant: &mut Assistant,
                                 assistant_input_h: &mut f32,
                                 starter_prompt: &mut Option<String>,
                                 submit_text: &mut Option<String>,
                                 input_rect: &mut Rect,
                                 focus_chat: &mut bool| {
                    if !assistant.key_set {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Connect DeepSeek (BYOK)").font(theme::medium_font(13.5)).color(theme::INK));
                            ui.add_space(4.0);
                            let row = ui.horizontal(|ui| {
                                let field_w = (ui.available_width() - 104.0).clamp(120.0, 380.0);
                                let key_resp = ui.add(
                                    egui::TextEdit::singleline(key_input)
                                        .id_salt("assistant_key_input")
                                        .password(true)
                                        .desired_width(field_w)
                                        .hint_text("sk-…"),
                                );
                                if *focus_chat { key_resp.request_focus(); *focus_chat = false; }
                                if theme::primary_button(ui, "Save key").clicked() && !key_input.trim().is_empty() {
                                    let k = key_input.trim().to_string();
                                    match crate::assistant::save_key(&k) {
                                        Ok(()) => {
                                            assistant.key_saved();
                                            key_input.clear();
                                        }
                                        Err(e) => assistant.history.push(ChatLine {
                                            role: "error".into(),
                                            text: format!("could not save key: {e}"),
                                        }),
                                    }
                                }
                            });
                            *input_rect = row.response.rect;
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(
                                    "Stored locally (~/.config/kabelsalat/config.json, 0600). Sent only to api.deepseek.com.",
                                )
                                .size(11.5)
                                .color(theme::INK_2),
                            );
                            for line in assistant.history.iter().filter(|l| l.role == "error") {
                                ui.label(egui::RichText::new(&line.text).size(12.0).color(theme::CLIP_BRIGHT));
                            }
                        });
                        return;
                    }

                    let col_h = ui.available_height().max(40.0);
                    let reserve = assistant_input_h.clamp(34.0, (col_h - 18.0).max(34.0));
                    let hist_h = (col_h - reserve).max(16.0);
                    egui::ScrollArea::vertical()
                        .id_salt("assistant_chat_scroll")
                        .min_scrolled_height(hist_h)
                        .max_height(hist_h)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if assistant.history.is_empty() && !assistant.busy {
                                ui.label(
                                    egui::RichText::new("Ask about routing, sounds or rigs — or run a quick starter:")
                                        .size(12.5)
                                        .color(theme::INK_2),
                                );
                                ui.add_space(6.0);
                                ui.horizontal_wrapped(|ui| {
                                    for prompt in [
                                        "Add a reverb to this rig",
                                        "Split keys at C4",
                                        "Load a warm pad",
                                        "Add a new rig",
                                    ] {
                                        if theme::chip_button(ui, prompt, false).clicked() {
                                            *starter_prompt = Some(prompt.to_string());
                                        }
                                    }
                                });
                            }
                            for line in &assistant.history {
                                let (label, color, rule) = match line.role.as_str() {
                                    "user" => ("YOU", theme::INK_2, theme::HAIRLINE_DARK),
                                    "assistant" => ("ASSISTANT", theme::AGENT_COLOR, theme::AGENT_COLOR),
                                    "system" => ("SYSTEM", theme::INK_2, theme::HAIRLINE),
                                    _ => ("ERROR", theme::CLIP_BRIGHT, theme::CLIP_RED),
                                };
                                ui.horizontal_top(|ui| {
                                    let (bar, _) = ui.allocate_exact_size(egui::vec2(2.0, 16.0), egui::Sense::hover());
                                    ui.painter().rect_filled(bar, 1.0, rule);
                                    ui.add_space(4.0);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new(label).font(theme::medium_font(10.5)).color(color));
                                        ui.add_space(4.0);
                                        for (k, part) in line.text.split("**").enumerate() {
                                            if part.is_empty() { continue; }
                                            for (seg, is_entity) in highlight_segments(part, &entities) {
                                                let mut t = egui::RichText::new(seg).size(13.0).color(theme::INK);
                                                if k % 2 == 1 { t = t.strong(); }
                                                if is_entity {
                                                    t = t.strong().color(theme::CREAM).background_color(theme::DARK_PANEL_HI);
                                                }
                                                ui.label(t);
                                            }
                                        }
                                    });
                                });
                                ui.add_space(5.0);
                            }
                            if assistant.busy {
                                ui.label(egui::RichText::new("thinking…").size(12.0).color(theme::AGENT_COLOR));
                            } else if let Some(label) = &confirm_label {
                                // #32: the click answers the assistant's question.
                                ui.horizontal(|ui| {
                                    ui.add_space(6.0);
                                    if theme::primary_button(ui, &format!("Confirm · {label}")).clicked() {
                                        confirm_choice.set(Some(true));
                                    }
                                    ui.add_space(6.0);
                                    if theme::chip_button(ui, "Cancel", false).clicked() {
                                        confirm_choice.set(Some(false));
                                    }
                                });
                                ui.add_space(5.0);
                            }
                        });
                    ui.add_space(4.0);
                    let row_top = ui.cursor().top();
                    let row = ui.horizontal(|ui| {
                        let edit = egui::Frame::new()
                            .fill(theme::PAPER_SINK)
                            .stroke(Stroke::new(1.0, theme::HAIRLINE))
                            .corner_radius(2.0)
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                let edit_resp = ui.add(
                                    egui::TextEdit::singleline(chat_input)
                                        .id_salt("assistant_chat_input")
                                        .frame(egui::Frame::NONE)
                                        .desired_width((ui.available_width() - 82.0).max(80.0))
                                        .hint_text("Ask or instruct…"),
                                );
                                if *focus_chat { edit_resp.request_focus(); *focus_chat = false; }
                                edit_resp
                            })
                            .inner;
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (theme::primary_button(ui, "Send").clicked() || enter)
                            && !chat_input.trim().is_empty()
                            && !assistant.busy
                        {
                            *submit_text = Some(chat_input.trim().to_string());
                            chat_input.clear();
                        }
                    });
                    *input_rect = row.response.rect;
                    *assistant_input_h = (4.0 + (ui.cursor().top() - row_top).max(26.0) + ui.spacing().item_spacing.y).max(34.0);
                };

                if two_col {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(main_w, avail_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                draw_main(
                                    ui,
                                    &mut self.key_input,
                                    &mut self.chat_input,
                                    &mut self.assistant,
                                    &mut self.assistant_input_h,
                                    &mut starter_prompt,
                                    &mut submit_text,
                                    &mut input_rect,
                                    &mut focus_chat,
                                );
                            },
                        );
                        ui.add_space(12.0);
                        let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, avail_h), egui::Sense::hover());
                        ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE));
                        ui.add_space(12.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(side_w, avail_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                theme::section_label(ui, "Patch & Engine");
                                ui.add_space(4.0);
                                let rig_lbl = format!("{} · {} ({} units)", self.active_rig + 1, self.rig_name(self.active_rig),
                                    self.units.iter().filter(|u| self.rig_count() <= 1 || u.rig as usize == self.active_rig).count());
                                ui.add(egui::Label::new(egui::RichText::new(format!("{rig_lbl} · octave C{}", self.octave))
                                    .size(12.0).color(theme::INK)).truncate());
                                ui.add(egui::Label::new(egui::RichText::new(format!("{} · {}", self.surge_status, self.hw_status))
                                    .size(11.5).color(theme::INK_2)).truncate());
                                ui.add_space(8.0);
                                Self::stage_hairline(ui);
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    theme::section_label(ui, "Agent Activity");
                                    // What the last request cost: model steps, model time, answering model.
                                    if let Some(cost) = self.assistant.turn.summary() {
                                        ui.add(egui::Label::new(egui::RichText::new(cost).size(10.5).color(theme::INK_2)).truncate());
                                    }
                                });
                                ui.add_space(4.0);
                                let act_h = ui.available_height().max(24.0);
                                egui::ScrollArea::vertical()
                                    .id_salt("assistant_activity_scroll")
                                    .min_scrolled_height(act_h)
                                    .max_height(act_h)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if self.agent_activity.log.is_empty() {
                                            ui.label(
                                                egui::RichText::new("No agent edits yet — assistant & MCP/CLI changes appear here and highlight in the rack.")
                                                    .size(11.5)
                                                    .color(theme::INK_2),
                                            );
                                            return;
                                        }
                                        for entry in &self.agent_activity.log {
                                            let active = AgentActivity::strength(Some(&(entry.at + AGENT_TOUCH_SECS)), self.ui_time) > 0.01;
                                            ui.horizontal(|ui| {
                                                let (tag_fill, tag_txt) = if active {
                                                    (theme::AGENT_COLOR, theme::AGENT_COLOR_TEXT)
                                                } else {
                                                    (theme::DARK_PANEL_HI, theme::INK_2)
                                                };
                                                let (tr, _) = ui.allocate_exact_size(egui::vec2(38.0, 16.0), egui::Sense::hover());
                                                ui.painter().rect_filled(tr, 2.0, tag_fill);
                                                ui.painter().text(tr.center(), egui::Align2::CENTER_CENTER, entry.origin.tag(),
                                                    theme::medium_font(9.5), tag_txt);
                                                ui.add(egui::Label::new(
                                                    egui::RichText::new(&entry.summary)
                                                        .size(12.0)
                                                        .color(if active { theme::INK } else { theme::INK_2 }),
                                                ).truncate());
                                            });
                                            ui.add_space(3.0);
                                        }
                                    });
                            },
                        );
                    });
                } else {
                    draw_main(
                        ui,
                        &mut self.key_input,
                        &mut self.chat_input,
                        &mut self.assistant,
                        &mut self.assistant_input_h,
                        &mut starter_prompt,
                        &mut submit_text,
                        &mut input_rect,
                        &mut focus_chat,
                    );
                }
            });
        self.dock_rect = resp.response.rect;
        self.assistant_rect = resp.response.rect;
        self.assistant_input_rect = input_rect;
        self.focus_chat_request = focus_chat;
        if clear_chat {
            self.assistant.history.clear();
            self.pending_confirm = None;
        }
        if let Some(choice) = confirm_choice.get() {
            self.resolve_confirm(choice);
        }
        if let Some(p) = starter_prompt.or(submit_text) {
            self.submit_chat(p);
        }
    }

    /// White key `wi` (0..8) rect inside the dock keyboard area, in points.
    /// Octaves the bottom keyboard shows at this width (white keys ≥ 44 px).
    fn dock_octaves(kb_rect: Rect) -> usize {
        (((kb_rect.width() - 12.0) / (7.0 * 44.0)).floor() as usize).clamp(1, 4)
    }

    /// White keys: 7 per octave plus the top C.
    fn dock_whites(kb_rect: Rect) -> usize {
        Self::dock_octaves(kb_rect) * 7 + 1
    }

    fn dock_key_rect(kb_rect: Rect, wi: usize) -> Rect {
        let kw = (kb_rect.width() - 12.0) / Self::dock_whites(kb_rect) as f32;
        let kh = (kb_rect.height() - 12.0).max(40.0);
        Rect::from_min_size(
            Pos2::new(kb_rect.left() + 6.0 + wi as f32 * kw, kb_rect.top() + 6.0),
            Vec2::new(kw - 1.0, kh),
        )
    }

    /// Black key `bi` (0..5) of octave `oct`.
    fn dock_black_key_rect(kb_rect: Rect, oct: usize, bi: usize) -> Rect {
        let base = Self::dock_key_rect(kb_rect, oct * 7 + BLACK_AFTER[bi]);
        let bw = (kb_rect.width() - 12.0) / Self::dock_whites(kb_rect) as f32 * 0.58;
        Rect::from_min_size(Pos2::new(base.right() - bw / 2.0 + 0.5, base.top()), Vec2::new(bw, base.height() * 0.62))
    }

    /// Semitone of white key `wi` above the keyboard's lowest C.
    fn dock_white_semi(wi: usize) -> usize {
        12 * (wi / 7) + WHITE_SEMI[wi % 7]
    }

    /// Keyboard dock height: a share of the WINDOW height (rule 64), never
    /// below the minimum playable size, and only what the rack can spare
    /// (rule 22). Whole pixels, so a resize does not jitter the layout.
    fn keyboard_dock_h(window_h: f32, avail_h: f32, rack_need_h: f32) -> f32 {
        const MIN_H: f32 = 140.0; // header + keys of at least 40 px
        const MAX_H: f32 = 300.0;
        let spare = (avail_h - rack_need_h).max(MIN_H);
        (window_h * 0.2).clamp(MIN_H, MAX_H).min(spare).round()
    }

    /// Which semitone (above the keyboard's lowest C) a point lands on, black first.
    fn dock_key_hit(kb_rect: Rect, pos: Pos2) -> Option<usize> {
        for oct in 0..Self::dock_octaves(kb_rect) {
            for bi in 0..5 {
                if Self::dock_black_key_rect(kb_rect, oct, bi).contains(pos) { return Some(12 * oct + BLACK_SEMI[bi]); }
            }
        }
        (0..Self::dock_whites(kb_rect))
            .find(|&wi| Self::dock_key_rect(kb_rect, wi).contains(pos))
            .map(Self::dock_white_semi)
    }

    /// Full-width playable software keyboard (GitHub #9): the Keys unit's octave
    /// moved to the bottom dock. Plays through the same `fire_keys` path, lights
    /// the same `key_leds`, and the Keys unit stays unchanged in the rack.
    fn keyboard_panel_ui(&mut self, root: &mut egui::Ui) {
        let h = Self::keyboard_dock_h(root.ctx().content_rect().height(), root.available_height(), self.rack_need_h());
        let note_leds = self.note_leds;
        let split = self.keys_unit.and_then(|k| self.shown_split(k));
        let octave = self.octave;
        let base = 12 * (octave as usize + 1); // the laptop keys' lowest note
        let n_oct = Self::dock_octaves(Rect::from_min_size(Pos2::ZERO, Vec2::new(root.available_width() - 28.0, 1.0)));
        let mut note: Option<usize> = None;
        let mut kb_rect = Rect::NOTHING;
        // exact_size, not default_size: egui keeps a panel's first size, so
        // the dock never followed a window resize (#23).
        let resp = egui::Panel::bottom("dock")
            .exact_size(h)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(root, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 22.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("Software Keyboard").font(theme::medium_font(13.5)).color(theme::INK));
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(format!("C{octave}–C{} · laptop keys play notes", octave as usize + n_oct))
                            .size(11.5).color(theme::INK_2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::chip_button(ui, "×", false).on_hover_text("hide the keyboard (V)").clicked() {
                                self.dock = DockView::None;
                            }
                        });
                    },
                );
                ui.add_space(4.0);
                // Claim the whole remaining area so the panel's measured height
                // (egui derives the outer rect from the content's min_rect) stays
                // at the requested size instead of collapsing to the header row.
                let (r, kb_resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ui.available_height().max(48.0)),
                    egui::Sense::click(),
                );
                kb_rect = r;
                let painter = ui.painter().clone();
                let lit = |semi: usize| note_leds[(base + semi).min(127)] > 0.02;
                for wi in 0..Self::dock_whites(kb_rect) {
                    let kr = Self::dock_key_rect(kb_rect, wi);
                    let fill = if lit(Self::dock_white_semi(wi)) { theme::ACCENT } else { WHITE };
                    painter.rect_filled(kr, 3.0, fill);
                    painter.rect_stroke(kr, 3.0, Stroke::new(1.0, BLACK), egui::StrokeKind::Inside);
                }
                for oct in 0..Self::dock_octaves(kb_rect) {
                    for bi in 0..5 {
                        let kr = Self::dock_black_key_rect(kb_rect, oct, bi);
                        let fill = if lit(12 * oct + BLACK_SEMI[bi]) { theme::ACCENT } else { BLACK };
                        painter.rect_filled(kr, 2.0, fill);
                        painter.rect_stroke(kr, 2.0, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
                    }
                }
                // Split point: a line on the left edge of the split key (white keys).
                if let Some(p) = split {
                    let span = 12 * Self::dock_octaves(kb_rect);
                    let semi = p as i64 - base as i64;
                    if semi > 0 && semi <= span as i64 {
                        let semi = semi as usize;
                        let wi = (0..Self::dock_whites(kb_rect)).find(|&w| Self::dock_white_semi(w) >= semi).unwrap_or(0);
                        let x = Self::dock_key_rect(kb_rect, wi).left() - 0.5;
                        let top = Self::dock_key_rect(kb_rect, 0).top() - 4.0;
                        let bot = Self::dock_key_rect(kb_rect, 0).bottom() + 4.0;
                        painter.line_segment([Pos2::new(x, top), Pos2::new(x, bot)], Stroke::new(2.0, theme::CREAM));
                    }
                }
                if kb_resp.clicked() {
                    if let Some(pos) = kb_resp.interact_pointer_pos() {
                        note = Self::dock_key_hit(kb_rect, pos);
                    }
                }
            });
        self.dock_rect = resp.response.rect;
        self.dock_kb_rect = kb_rect;
        self.assistant_rect = Rect::NOTHING;
        self.assistant_input_rect = Rect::NOTHING;
        if let Some(semi) = note {
            self.click_note((base + semi).min(127));
        }
    }

    /// A mouse click on an on-screen key: a note of `CLICK_NOTE_MS`, released
    /// by the note-off scheduler (a click has no key-up to send one).
    fn click_note(&mut self, note: usize) {
        self.current_source = SourceId::Keys;
        self.fire_keys(note, 0.95);
        self.note_offs.push((std::time::Instant::now() + std::time::Duration::from_millis(CLICK_NOTE_MS), note));
    }

    /// Unit index of the Keys source (fallback 0 for a degenerate graph).
    fn keys_idx(&self) -> usize {
        self.keys_unit.unwrap_or(0)
    }

    /// Unit index of the Master bus, if present.
    fn master_idx(&self) -> Option<usize> {
        self.master_unit
    }

    /// The instrument rigs (Clap units) in voice order: (rig index, node id).
    /// At most two — the engine owns two CLAP voices.
    /// Display name of a hosted CLAP plugin: the scanned plugin name, else a
    /// known id, else None (the caller keeps its generic subtitle).
    fn plugin_display_name(&self, plugin_id: &str) -> Option<String> {
        if let Some(p) = self.plugins_clap.iter().find(|p| p.id == plugin_id) {
            return Some(p.name.clone());
        }
        (plugin_id == SURGE_ID || plugin_id == "Surge XT").then(|| "Surge XT".to_string())
    }

    /// Clap unit subtitles name what is really loaded: "Surge XT · Wurli"
    /// (plugin · patch), not the generic "synth · clap" (user). Cheap per
    /// frame: a few string compares; the galley is rebuilt only on change.
    fn refresh_instrument_subs(&mut self) {
        if self.plugins_clap.is_empty() && !self.plugins_loaded && !cfg!(test)
            && self.units.iter().any(|u| matches!(u.node_kind, UnitKind::Clap))
        {
            self.start_plugin_scan(false); // background (rule 46); fills names when done
        }
        let ids: Vec<(u32, String)> = self.base_graph.nodes.iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Clap { plugin_id } => Some((n.id, name_str(plugin_id).to_string())),
                _ => None,
            })
            .collect();
        for (slot, node) in self.clap_rigs() {
            let Some(pid) = ids.iter().find(|(id, _)| *id == node).map(|(_, p)| p.clone()) else { continue };
            let Some(plugin) = self.plugin_display_name(&pid) else { continue };
            let sub = match self.patch_names.get(slot).cloned().flatten() {
                Some(patch) => format!("{plugin} · {patch}"),
                None => plugin,
            };
            if let Some(u) = self.units.iter_mut().find(|u| u.node_id == node) {
                if u.sub != sub {
                    u.sub = sub;
                    u.sub_g = None;
                }
            }
        }
    }

    fn clap_rigs(&self) -> Vec<(usize, u32)> {
        self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .enumerate()
            .map(|(rig, u)| (rig, u.node_id))
            .collect()
    }

    /// Voice index (0/1) for a Clap unit, for inline editing (task-26). None if
    /// the unit is not one of the hosted synth voices.
    fn rig_of_unit(&self, unit: usize) -> Option<usize> {
        let node_id = self.units.get(unit)?.node_id;
        self.clap_rigs().into_iter().find(|(_, nid)| *nid == node_id).map(|(rig, _)| rig)
    }

    /// Rig index (voice slot) for a node id; 0 if not a rig.
    fn rig_of_node(&self, node: u32) -> usize {
        self.clap_rigs().iter().find(|(_, n)| *n == node).map(|(r, _)| *r).unwrap_or(0)
    }

    fn fire_keys(&mut self, semitone: usize, amp: f32) { self.fire_keys_with(semitone, amp, true); }

    /// `send = false`: visuals only (the note already reached the engine).
    fn fire_keys_with(&mut self, semitone: usize, amp: f32, send: bool) {
        // VEL knob scales keyboard velocity (note-offs pass through at 0).
        let ki = self.keys_idx();
        let vel = self.units.get(ki).and_then(|u| u.knobs.get(1)).map(|k| k.value).unwrap_or(0.8);
        let amp = amp * (0.35 + vel * 0.65);
        // Light on note-on only: a note-off re-lighting the key restarted the
        // full decay tail AFTER release (user report: key color stays too long).
        if amp > 0.0 {
            self.key_leds[semitone % 12] = 1.0;
            self.note_leds[semitone.min(127)] = 1.0;
        }
        if let Some(u) = self.units.get_mut(ki) { u.led = 1.0; }
        // A split keyboard sends each note out ONE jack (LOW below the point,
        // HIGH at/above) — only that jack's cables pulse, like the engine.
        let port = split_port(self.units.get(ki).and_then(|u| u.split), semitone.min(127) as u8) as usize;
        for ci in 0..self.cables.len() {
            let c = &mut self.cables[ci];
            if c.src == ki && c.src_port == port && c.kind == PortKind::Midi && c.dying < 0.0 {
                c.spawn_pulse(0.0, amp);
            }
        }
        if !send { return; }
        if let Some(sink) = &mut self.note_sink {
            sink(NoteEvent { note: semitone.min(127) as u8, velocity: amp.clamp(0.0, 1.0), source: self.current_source, created: std::time::Instant::now() });
        }
    }

    fn ping_unit(&mut self, idx: usize) {
        if let Some(u) = self.units.get_mut(idx) { u.led = 1.0; }
        let is_keys = self.units.get(idx).map(|u| matches!(u.node_kind, UnitKind::Keys)).unwrap_or(false);
        if is_keys {
            let amps = [1.0, 0.8, 0.9, 0.7];
            let ki = self.keys_idx();
            for ci in 0..self.cables.len() {
                if self.cables[ci].src == ki && self.cables[ci].kind == PortKind::Midi && self.cables[ci].dying < 0.0 {
                    for (k, amp) in amps.iter().enumerate() {
                        self.cables[ci].spawn_pulse(-(k as f32) * 0.07, *amp);
                    }
                }
            }
        } else {
            for c in &mut self.cables {
                if c.src == idx && c.kind == PortKind::Audio && c.dying < 0.0 {
                    c.env = c.env.max(0.95);
                }
            }
        }
    }

    /// Publish the patch through the control plane: living cables become
    /// Connect commands (the engine rejects duplicates), knobs SetParam, and
    /// the shared graph mirror is rebuilt when the cable set changed. Runs
    /// once per frame after input, so a re-plug's Disconnect precedes it.
    /// Pinned Surge parameters: send changed knob values to the plugin.
    fn publish_pins(&mut self) {
        let slots = self.clap_rigs();
        for u in self.units.iter_mut() {
            let Some(slot) = slots.iter().find(|(_, n)| *n == u.node_id).map(|(s, _)| *s) else { continue };
            let Some(idx) = self.clap_slot_map.get(slot).copied().flatten() else { continue };
            let Some(ctl) = self.surge_controllers.get(idx) else { continue };
            for k in u.knobs.iter_mut() {
                if let Some(pin) = k.clap.as_mut() {
                    if (k.value - pin.sent).abs() > 1e-4 {
                        ctl.set_param(ClapId::new(pin.id), pin.min + k.value as f64 * (pin.max - pin.min));
                        pin.sent = k.value;
                    }
                }
            }
        }
    }

    /// Pin (or unpin) a Surge parameter of voice `slot` as a face knob.
    fn toggle_pin(&mut self, slot: usize, p: &crate::host::ParamDesc) -> String {
        let Some(node) = self.clap_rigs().into_iter().find(|(s, _)| *s == slot).map(|(_, n)| n) else { return "no synth".into() };
        let Some(u) = self.units.iter_mut().find(|u| u.node_id == node) else { return "no synth".into() };
        if let Some(i) = u.knobs.iter().position(|k| k.clap.map_or(false, |c| c.id == p.id)) {
            u.knobs.remove(i);
            return format!("unpinned {}", p.name);
        }
        if u.knobs.iter().filter(|k| k.clap.is_some()).count() >= MAX_PINS {
            return format!("at most {MAX_PINS} pinned knobs per synth");
        }
        let span = (p.max - p.min).max(1e-9);
        let norm = (((p.value - p.min) / span) as f32).clamp(0.0, 1.0);
        let label: String = p.name.to_uppercase().chars().take(9).collect();
        u.knobs.push(Knob {
            label, value: norm, default: (((p.default - p.min) / span) as f32).clamp(0.0, 1.0),
            param: None, hidden: false, sent: Some(norm), clap: Some(ClapPin { id: p.id, min: p.min, max: p.max, sent: norm }), label_g: None,
        });
        format!("pinned {} to the face", p.name)
    }

    fn publish_patch(&mut self) {
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap();
            for c in &self.cables {
                if c.dying >= 0.0 { continue; }
                let (Some(sn), Some(dn)) = (
                    self.units.get(c.src).map(|u| u.node_id),
                    self.units.get(c.dst).map(|u| u.node_id),
                ) else { continue };
                let kind = match c.kind {
                    PortKind::Midi => CPortKind::Midi,
                    PortKind::Audio => CPortKind::Audio,
                };
                let _ = b.send(Command::Connect {
                    id: c.id,
                    from: PortRef { node: sn, port: c.src_port as u8 },
                    to: PortRef { node: dn, port: c.dst_port as u8 },
                    kind,
                });
            }
            for u in &mut self.units {
                for k in &mut u.knobs {
                    if let Some(p) = k.param {
                        if k.sent.map_or(true, |s| (k.value - s).abs() > 1e-6) {
                            if b.send(Command::SetParam { node: u.node_id, param: p, value: k.value as f64 }) {
                                k.sent = Some(k.value);
                            }
                        }
                    }
                }
            }
        }

        // Mirror the live patch into the shared graph for bridge queries
        // (task-16): loaded-graph nodes + living cables as edges. Rebuild only
        // when the cable set changes — cheap per frame.
        let sig: Vec<(u32, u32, u8, u32, u8, CPortKind)> = self
            .cables
            .iter()
            .filter(|c| c.dying < 0.0)
            .map(|c| {
                let kind = match c.kind {
                    PortKind::Midi => CPortKind::Midi,
                    PortKind::Audio => CPortKind::Audio,
                };
                let sn = self.units.get(c.src).map(|u| u.node_id).unwrap_or(c.src as u32);
                let dn = self.units.get(c.dst).map(|u| u.node_id).unwrap_or(c.dst as u32);
                (c.id, sn, c.src_port as u8, dn, c.dst_port as u8, kind)
            })
            .collect();
        if sig != self.graph_sig {
            let mut g = self.base_graph.clone();
            g.edges.clear();
            for c in &self.cables {
                if c.dying >= 0.0 { continue; }
                let (Some(sn), Some(dn)) = (
                    self.units.get(c.src).map(|u| u.node_id),
                    self.units.get(c.dst).map(|u| u.node_id),
                ) else { continue };
                let kind = match c.kind {
                    PortKind::Midi => CPortKind::Midi,
                    PortKind::Audio => CPortKind::Audio,
                };
                g.connect_with_id(
                    c.id,
                    PortRef { node: sn, port: c.src_port as u8 },
                    PortRef { node: dn, port: c.dst_port as u8 },
                    kind,
                );
            }
            if let Ok(mut shared) = self.shared_graph.lock() {
                *shared = g;
            }
            self.graph_sig = sig;
        }
    }

    fn try_connect(&mut self, from: (usize, usize, Dir, PortKind), to: (usize, usize, Dir, PortKind)) -> bool {
        let (fu, fp, fd, fk) = from;
        let (tu, tp, td, tk) = to;
        if fd == td || fk != tk || fu == tu { return false; }
        let (src, sp, dst, dp) = if fd == Dir::Out { (fu, fp, tu, tp) } else { (tu, tp, fu, fp) };
        let pattern = if dst == 2 { 1 } else { 0 };
        if let Some(why) = self.cable_refusal(src, sp, dst, dp, None) {
            self.notify_patch(why);
        } else {
            self.add_cable(src, sp, dst, dp, fk, pattern);
        }
        true
    }

    fn start_pending(&mut self, unit: usize, jack: usize, cursor: Vec2) {
        let j = &self.units[unit].jacks[jack];
        let (dir, kind) = (j.dir, self.units[unit].kind(j.dir, j.port));
        let origin = self.jack_pos(unit, jack);
        self.pending = Some(Pending {
            unit, jack, dir, kind, cursor,
            c1: Vec2::new(16.0, 40.0), v1: Vec2::ZERO,
            c2: Vec2::new(-16.0, 40.0), v2: Vec2::ZERO,
            prev_b: if dir == Dir::Out { cursor } else { origin },
        });
    }

    fn compatible(&self, unit: usize, jack: usize) -> bool {
        match &self.pending {
            Some(p) => {
                let j = &self.units[unit].jacks[jack];
                p.dir != j.dir && p.kind == self.units[unit].kind(j.dir, j.port) && p.unit != unit
            }
            None => false,
        }
    }

    fn start_flip(&mut self) {
        if self.flipping { return; }
        self.flipping = true;
        self.flip_t = 0.0;
        self.flip_to = if self.side == Side::Front { Side::Back } else { Side::Front };
        self.pending = None;
        self.drag = Drag::None;
        self.press = None;
        self.knob_menu = None;
        self.knob_menu_rect = Rect::NOTHING;
        eprintln!(
            "rack flip: {} -> {} (embedded editor stays in its fixed region)",
            if self.side == Side::Front { "front" } else { "back" },
            if self.flip_to == Side::Front { "front" } else { "back" },
        );
    }

    fn toggle_play(&mut self) {
        self.playing = !self.playing;
        self.ui_transport_grace = self.ui_time + 0.6;
        for u in &self.units {
            if u.transport {
                self.ui_node_grace.insert(u.node_id, self.ui_time + 0.6);
            }
        }
        // Global transport = MASTER: Play/Stop fans out to every
        // transport-capable rack instrument; per-node chips override after.
        if self.playing {
            self.time = 0.0; // restart the visual beat phase on Play
            if self.graph_pub.is_none() && self.count_in_enabled {
                self.counting_in = true;
                self.count_in_beat = 1;
            }
            for c in &mut self.cables {
                c.next_event = self.time + 0.08;
                c.pat_idx = 0;
            }
        } else {
            self.counting_in = false;
            self.count_in_beat = 0;
        }
        self.master_transport(self.playing);
    }

    /// Toggle master recording (REC chip). The engine is the authority: it
    /// flips the recorder's recording flag and messages the writer thread.
    fn toggle_recording(&mut self) {
        let recording = self.recorder.as_ref().map(|r| r.is_recording()).unwrap_or(false);
        if let Some(bus) = &self.bus {
            let cmd = if recording { Command::RecordStop } else { Command::RecordStart };
            let _ = bus.lock().unwrap().send(cmd);
        }
    }

    /// Master transport (Space, masthead circle, transport window, assistant
    /// transport_play/stop): send TransportPlay/Stop. The engine sets every
    /// transport-capable node and publishes; faces follow via
    /// `sync_from_engine`. Headless (no GraphPub) applies the flags locally.
    fn master_transport(&mut self, playing: bool) {
        if let Some(bus) = &self.bus {
            let cmd = if playing { Command::TransportPlay } else { Command::TransportStop };
            let _ = bus.lock().unwrap().send(cmd);
        }
        if self.graph_pub.is_none() {
            let ids: Vec<u32> = self.units.iter().filter(|u| u.transport).map(|u| u.node_id).collect();
            for id in ids { self.apply_node_playing(id, playing); }
        }
    }

    /// Per-instrument transport (face chip, assistant set_node_playing):
    /// start/stop ONE transport-capable node. Returns false if no such unit.
    /// With an engine attached the face waits for the engine's publication
    /// (no local guess); headless it applies locally.
    fn set_node_playing(&mut self, node: u32, playing: bool) -> bool {
        if !self.units.iter().any(|u| u.node_id == node && u.transport) { return false; }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetNodePlaying { node, playing });
        }
        if self.graph_pub.is_none() { self.apply_node_playing(node, playing); }
        true
    }

    /// Write one node's playing flag into the unit face, the base graph
    /// (sessions) and the bridge mirror (MCP --attach get_graph).
    fn apply_node_playing(&mut self, node: u32, playing: bool) {
        for u in self.units.iter_mut().filter(|u| u.node_id == node && u.transport) {
            u.playing = playing;
        }
        self.base_graph.set_playing(node, playing);
        if let Ok(mut g) = self.shared_graph.lock() { g.set_playing(node, playing); }
    }

    /// Rack-local rect of a transport unit's Play/Stop face chip (front side
    /// only). Fixed width so the layout never jitters between PLAY/STOP;
    /// 24px tall keeps ≥4px vertical padding around the 12px label (rule 7).
    fn transport_chip_rect(&self, unit: usize) -> Option<Rect> {
        let u = self.units.get(unit)?;
        if !u.transport { return None; }
        let r = *self.unit_rects.get(unit)?;
        if r.width() < 140.0 { return None; }
        let top = (r.top() + 64.0).max(r.center().y - 16.0).min(r.bottom() - FACE_PAD_B - FACE_CTL_H);
        Some(Rect::from_min_size(Pos2::new(r.left() + FACE_X, top), Vec2::new(72.0, 24.0)))
    }

    /// Keys-face split controls (rack-local): the SPLIT chip and, right of
    /// it, the split-point readout (drawn/live only while split). Top-right
    /// of the unit header, left of the activity LED, so they never collide
    /// with the keyboard or the RATE/VEL knobs on short units. None when the
    /// unit is too narrow to keep them clear of its name.
    fn split_rects(&self, unit: usize) -> Option<(Rect, Rect)> {
        let u = self.units.get(unit)?;
        if !matches!(u.node_kind, UnitKind::Keys) { return None; }
        let r = *self.unit_rects.get(unit)?;
        if !(r.width() > 0.0) { return None; } // hidden (Rect::NOTHING) or unlaid
        let y = r.top() + 14.0;
        let readout = Rect::from_min_size(Pos2::new(r.right() - 36.0 - SPLIT_READOUT_W, y), Vec2::new(SPLIT_READOUT_W, FACE_CTL_H));
        // Chip + state field abut: ONE segmented control ("SPLIT | OFF"), not two buttons.
        let chip = Rect::from_min_size(Pos2::new(readout.left() - 1.0 - SPLIT_CHIP_W, y), Vec2::new(SPLIT_CHIP_W, FACE_CTL_H));
        let name_w = u.name_g.as_ref().map_or(40.0, |g| g.size().x);
        if chip.left() < r.left() + 34.0 + name_w + 8.0 { return None; }
        Some((chip, readout))
    }

    /// Split point shown on a Keys face: the live drag value while dragging
    /// its readout, else the unit's (engine-mirrored) split.
    fn shown_split(&self, unit: usize) -> Option<u8> {
        match self.drag {
            Drag::Split { unit: du, cur, .. } if du == unit => Some(cur),
            _ => self.units.get(unit).and_then(|u| u.split),
        }
    }

    /// Split or unsplit a Keys unit (face chip, point drag, assistant).
    /// Unsplitting first unplugs the HIGH cables — the jack goes away — with
    /// an on-stage notice (refusal/message pattern); their Disconnects
    /// precede SetNodeSplit on the FIFO bus. Enabling keeps every existing
    /// cable on port 0 (LOW). With an engine attached the jacks follow the
    /// engine's publication (no local guess); headless applies locally.
    /// Returns false if `unit` is not a Keys unit.
    fn set_keys_split(&mut self, unit: usize, point: Option<u8>) -> bool {
        let Some(u) = self.units.get(unit) else { return false };
        if !matches!(u.node_kind, UnitKind::Keys) { return false; }
        let point = point.map(|p| p.min(127));
        if u.split == point { return true; }
        let node = u.node_id;
        if point.is_none() {
            let high: Vec<u32> = self.cables.iter()
                .filter(|c| c.dying < 0.0 && c.src == unit && c.src_port >= 1)
                .map(|c| c.id).collect();
            for id in &high {
                if let Some(ci) = self.live_cable(*id) { self.unplug_cable(ci, false); }
            }
            if !high.is_empty() {
                let n = high.len();
                self.notify_patch(&format!("split off — {n} HIGH cable{} unplugged", if n == 1 { "" } else { "s" }));
            }
        }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetNodeSplit { node, point });
        }
        if self.graph_pub.is_none() { self.apply_keys_split(node, point); }
        true
    }

    /// Write one Keys node's split into its unit (jacks), the base graph
    /// (sessions) and the bridge mirror (MCP --attach get_graph).
    fn apply_keys_split(&mut self, node: u32, point: Option<u8>) {
        for (i, u) in self.units.iter_mut().enumerate() {
            if u.node_id != node || !matches!(u.node_kind, UnitKind::Keys) { continue; }
            set_unit_split(u, point);
            if self.pending.as_ref().map_or(false, |p| p.unit == i) { self.pending = None; }
            if matches!(self.hover, Some(Hit::Jack { unit, .. }) if unit == i) { self.hover = None; }
        }
        self.base_graph.set_split(node, point);
        if let Ok(mut g) = self.shared_graph.lock() { g.set_split(node, point); }
    }

    /// Unit names fed by a Keys unit's output `port` (living MIDI cables).
    fn zone_targets(&self, keys: usize, port: usize) -> Vec<String> {
        self.cables.iter()
            .filter(|c| c.dying < 0.0 && c.kind == PortKind::Midi && c.src == keys && c.src_port == port)
            .filter_map(|c| self.units.get(c.dst).map(|u| sentence_case(&u.name)))
            .collect()
    }

    /// Every split Keys unit: (unit, point).
    fn split_keys(&self) -> Vec<(usize, u8)> {
        self.units.iter().enumerate()
            .filter(|(_, u)| matches!(u.node_kind, UnitKind::Keys))
            .filter_map(|(i, u)| u.split.map(|p| (i, p)))
            .collect()
    }

    /// Matrix footer line: each Keys node's split (point + what LOW/HIGH
    /// feed), not a global flag.
    fn split_footer(&self) -> String {
        let splits = self.split_keys();
        if splits.is_empty() {
            return "Full keyboard on every MIDI cable · SPLIT on a Keys face gives it LOW / HIGH outs to wire".into();
        }
        let list = |v: Vec<String>| if v.is_empty() { "nothing".to_string() } else { v.join(", ") };
        splits.iter().map(|&(k, p)| {
            let who = if self.rig_count() > 1 { format!("{} keys", self.rig_name(self.units[k].rig as usize)) } else { "Keys".into() };
            format!("{who} split @ {} — LOW → {} · HIGH → {}", note_name(p), list(self.zone_targets(k, 0)), list(self.zone_targets(k, 1)))
        }).collect::<Vec<_>>().join("   ·   ")
    }

    /// Assistant get_matrix: per-Keys split state.
    fn split_report(&self) -> serde_json::Value {
        let ids = |k: usize, port: usize| -> Vec<u32> {
            self.cables.iter()
                .filter(|c| c.dying < 0.0 && c.kind == PortKind::Midi && c.src == k && c.src_port == port)
                .filter_map(|c| self.units.get(c.dst).map(|u| u.node_id)).collect()
        };
        serde_json::Value::Array(self.units.iter().enumerate()
            .filter(|(_, u)| matches!(u.node_kind, UnitKind::Keys))
            .map(|(k, u)| serde_json::json!({
                "keys_node": u.node_id,
                "point": u.split,
                "low_port0": ids(k, 0),
                "high_port1": ids(k, 1),
            }))
            .collect())
    }

    /// Metronome-face BPM control (rack-local): optional −/+ steppers around
    /// a drag readout. Preferred: one row after the beat LED; narrow units
    /// stack it under the Play/Stop chip; the narrowest keep the readout
    /// only. Every metronome face shows the SAME global tempo (engine
    /// SetBpm) — tempo is shared, Play/Stop is per instrument.
    fn bpm_rects(&self, unit: usize) -> Option<BpmRects> {
        let chip = self.transport_chip_rect(unit)?;
        let r = *self.unit_rects.get(unit)?;
        let right = r.right() - FACE_PAD_R;
        let full = 2.0 * (BPM_STEP_W + 4.0) + BPM_VALUE_W;
        let row = |x: f32, y: f32, steppers: bool| -> BpmRects {
            let sz = |w: f32| Vec2::new(w, FACE_CTL_H);
            if steppers {
                let minus = Rect::from_min_size(Pos2::new(x, y), sz(BPM_STEP_W));
                let value = Rect::from_min_size(Pos2::new(minus.right() + 4.0, y), sz(BPM_VALUE_W));
                let plus = Rect::from_min_size(Pos2::new(value.right() + 4.0, y), sz(BPM_STEP_W));
                BpmRects { minus: Some(minus), value, plus: Some(plus) }
            } else {
                BpmRects { minus: None, value: Rect::from_min_size(Pos2::new(x, y), sz(BPM_VALUE_W)), plus: None }
            }
        };
        let x_row = chip.right() + 32.0; // past the beat LED
        if x_row + full <= right { return Some(row(x_row, chip.top(), true)); }
        let y_under = chip.bottom() + 4.0;
        if y_under + FACE_CTL_H <= r.bottom() - FACE_PAD_B {
            if chip.left() + full <= right { return Some(row(chip.left(), y_under, true)); }
            if chip.left() + BPM_VALUE_W <= right { return Some(row(chip.left(), y_under, false)); }
        }
        if x_row + BPM_VALUE_W <= right { return Some(row(x_row, chip.top(), false)); }
        None
    }

    /// Metronome-face TAP + COUNT-IN controls (rack-local), placed below
    /// the top Play/BPM row (or on row 3 when BPM wraps on narrow faces).
    fn metro_action_rects(&self, unit: usize) -> Option<MetroActionRects> {
        let chip = self.transport_chip_rect(unit)?;
        let r = *self.unit_rects.get(unit)?;
        let right = r.right() - FACE_PAD_R;
        let base_bottom = match self.bpm_rects(unit) {
            Some(b) if b.value.top() > chip.top() + 1.0 => b.value.bottom(),
            _ => chip.bottom(),
        };
        let y = base_bottom + 4.0;
        if y + FACE_CTL_H > r.bottom() - FACE_PAD_B { return None; }
        let avail = right - chip.left();
        let (tap_w, gap, ci_w, compact) = if avail >= METRO_TAP_W + 6.0 + METRO_COUNT_IN_W {
            (METRO_TAP_W, 6.0, METRO_COUNT_IN_W, false)
        } else if avail >= 44.0 + 4.0 + 46.0 {
            (44.0, 4.0, 46.0, true)
        } else {
            return None;
        };
        let tap = Rect::from_min_size(Pos2::new(chip.left(), y), Vec2::new(tap_w, FACE_CTL_H));
        let count_in = Rect::from_min_size(Pos2::new(tap.right() + gap, y), Vec2::new(ci_w, FACE_CTL_H));
        Some(MetroActionRects { tap, count_in, compact })
    }

    /// Is rack-local `lp` on one of a unit's face drag readouts (the split
    /// point while split, or the BPM value)?
    fn on_face_field(&self, unit: usize, lp: Pos2) -> bool {
        let split = self.units.get(unit).map_or(false, |u| u.split.is_some())
            && self.split_rects(unit).map_or(false, |(_, r)| r.contains(lp));
        split || self.bpm_rects(unit).map_or(false, |b| b.value.contains(lp))
    }

    /// Set the metronome tempo, clamped to the engine's 20..=300 range, and
    /// notify the engine. Used by the metronome face and the assistant tool.
    fn set_bpm(&mut self, bpm: f32) {
        self.bpm = bpm.clamp(20.0, 300.0);
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetBpm { bpm: self.bpm });
        }
    }

    /// Record a tap-tempo event at monotonic timestamp `now_secs`, flashing
    /// the TAP chip and updating BPM once >=2 taps establish an interval.
    fn tap_tempo_at(&mut self, now_secs: f64) {
        self.tap_flash_t = 1.0;
        if let Some(bpm) = self.tap_tempo.tap(now_secs) {
            self.set_bpm(bpm);
        }
    }

    /// Update count-in configuration, persist it, and notify the engine.
    fn set_count_in(&mut self, enabled: bool, bars: u8) {
        let bars = bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS);
        self.count_in_enabled = enabled;
        self.count_in_bars = bars;
        let _ = crate::assistant::save_count_in(enabled, bars);
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetCountIn { enabled, bars });
        }
    }

    // -- simulation -----------------------------------------------------------

    fn step_spring(c: &mut Vec2, v: &mut Vec2, target: Vec2, h: f32) {
        *v += ((target - *c) * SPRING_K - *v * SPRING_D) * h;
        *c += *v * h;
        *c = clamp_len(*c, 460.0);
    }

    fn update(&mut self, dt: f32) {
        self.ui_time += dt;
        // Audio In tuner: drain each open capture's tuner ring and run YIN.
        if !self.audio_in_tuners.is_empty() {
            self.pump_audio_in_tuners();
        }
        // Periodic capture re-sync (hot-plug): cheap in the steady state; only
        // opens a stream when a device appears or a dead one returns.
        self.audio_in_sync_timer += dt;
        if self.audio_in_sync_timer >= 2.0 {
            self.audio_in_sync_timer = 0.0;
            self.sync_audio_in_captures();
        }
        self.agent_activity.prune(self.ui_time);
        let now_t = self.ui_time;
        self.ui_node_grace.retain(|_, until| *until > now_t);
        // `--demo-agent` keeps its touch fresh so the screenshot hook always
        // shows the highlight (not a 2.6s-faded rack).
        if self.demo_agent_hold {
            let now = self.ui_time;
            if let Some(k) = self.keys_unit {
                self.agent_activity.touch_node(self.units[k].node_id, now);
                self.agent_activity.touch_cell(SourceId::Keys, self.units[k].node_id, now);
            }
            self.agent_activity.touch_rig(0, now);
        }
        self.tap_flash_t = (self.tap_flash_t - dt * 5.0).max(0.0);
        if self.playing {
            self.time += dt as f64;
            if self.graph_pub.is_none() && self.counting_in {
                let beats = self.time * self.bpm.max(1.0) as f64 / 60.0;
                let total = self.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS) as f64
                    * crate::transport::BEATS_PER_BAR as f64;
                if beats >= total {
                    self.counting_in = false;
                    self.count_in_beat = 0;
                    self.time = 0.0;
                } else {
                    self.count_in_beat = (beats.floor() as u8).saturating_add(1).min(total as u8);
                }
            }
        }

        // Flip animation.
        if self.flipping {
            self.flip_t += dt / FLIP_SECS;
            if self.flip_t >= 0.5 && self.side != self.flip_to {
                self.side = self.flip_to;
                if self.side == Side::Back {
                    // Rack lands: cables get a little thump and settle.
                    for c in &mut self.cables {
                        let jx = (rng01(&mut c.rng) - 0.5) * 130.0;
                        c.v1 += Vec2::new(jx, 60.0 + rng01(&mut c.rng) * 60.0);
                        c.v2 += Vec2::new(-jx, 60.0 + rng01(&mut c.rng) * 60.0);
                    }
                }
            }
            if self.flip_t >= 1.0 {
                self.flip_t = 1.0;
                self.flipping = false;
            }
        }

        for u in &mut self.units { u.led *= (-dt / UNIT_LED_TAU).exp(); }
        for k in &mut self.key_leds { *k *= (-dt / KEY_LED_TAU).exp(); }
        for k in &mut self.note_leds { *k *= (-dt / KEY_LED_TAU).exp(); }
        // Held laptop keys stay lit (key repeats are swallowed, not re-fired);
        // the short tail starts on release.
        for &(_, semi) in &self.held_keys {
            self.key_leds[semi % 12] = 1.0;
            self.note_leds[semi.min(127)] = 1.0;
        }
        // Adopt hot-plug changes from the MIDI thread (one atomic load).
        let rev = self.hw_midi.rev.load(std::sync::atomic::Ordering::Acquire);
        if rev != self.hw_midi_rev {
            self.hw_midi_rev = rev;
            if let Ok(d) = self.hw_midi.devices.lock() {
                self.hw_devices = d.iter().map(|(n, _)| n.clone()).collect();
                let live: Vec<&str> = d.iter().filter(|(_, l)| *l).map(|(n, _)| n.as_str()).collect();
                self.hw_status = if live.is_empty() {
                    "no hardware MIDI — laptop keys live".into()
                } else {
                    format!("MIDI in: {}", live.join(", "))
                };
            }
        }
        // Per-rig activity for the overview strip: instant attack, slow release.
        let n_rigs = self.rig_count();
        if self.rig_levels.len() != n_rigs { self.rig_levels.resize(n_rigs, 0.0); }
        // With an engine, a rig is active when its audio actually sounds (the
        // published per-edge levels, rule 16): UI LEDs only follow the rig in
        // view, while the keyboard can play every rig that listens to it.
        // Read the levels directly: a hidden rig's cables are not simulated.
        let heard = self.edge_levels.clone();
        // Post-fader, like the engine's rig bus: a muted rig, or any rig
        // while another is soloed, reads silent (rule 16 — never show a
        // silenced path as live).
        let any_solo = self.rig_mix.iter().take(n_rigs).any(|m| m.solo);
        for r in 0..n_rigs {
            let m = self.rig_mix[r.min(7)];
            let bus_gain = if m.mute || (any_solo && !m.solo) { 0.0 } else { m.vol };
            let target = bus_gain * if let Some(l) = &heard {
                self.cables.iter()
                    .filter(|c| c.kind == PortKind::Audio && c.dying < 0.0)
                    .filter(|c| self.units.get(c.src).map_or(false, |u| u.rig == r as u8))
                    .map(|c| edge_display_level(l.load(c.id)).min(1.0))
                    .fold(0.0f32, f32::max)
            } else {
                self.units.iter().filter(|u| u.rig == r as u8).map(|u| u.led).fold(0.0f32, f32::max)
            };
            let cur = self.rig_levels[r];
            self.rig_levels[r] = if target > cur { target } else { cur + (target - cur) * (1.0 - (-dt / 0.25).exp()) };
        }
        // Matrix row activity: per-hardware-device LEDs, fast decay.
        if self.hw_leds.len() != self.hw_devices.len() { self.hw_leds.resize(self.hw_devices.len(), 0.0); }
        for l in &mut self.hw_leds { *l *= (-dt / 0.18).exp(); }
        // Motion: staged load fade-in (~0.45s) + ~120ms hover easing.
        self.load_fade = (self.load_fade + dt / 0.45).min(1.0);
        if self.unit_hover.len() != self.units.len() { self.unit_hover.resize(self.units.len(), 0.0); }
        let ease = 1.0 - (-dt / 0.12).exp();
        for (i, h) in self.unit_hover.iter_mut().enumerate() {
            let target = if matches!(self.hover, Some(Hit::Unit(u)) if u == i) { 1.0 } else { 0.0 };
            *h += (target - *h) * ease;
        }
        let pt = if matches!(self.hover, Some(Hit::Play)) { 1.0 } else { 0.0 };
        self.play_hover_t += (pt - self.play_hover_t) * ease;

        let mut n_arrivals = 0usize;
        let edge_levels = self.edge_levels.clone();
        for ci in 0..self.cables.len() {
            let ea = self.endpoint(&self.cables[ci], Dir::Out);
            let eb = self.endpoint(&self.cables[ci], Dir::In);
            let c = &mut self.cables[ci];
            c.age += dt;
            if c.dying >= 0.0 { c.dying += dt; }
            // Endpoints without a laid-out rect (a hidden rig's units, or a
            // rack rebuilt by a key handler AFTER this frame's layout) are
            // infinite: simulating them seeds NaN control points, and NaN
            // never recovers — every cable of a keyboard-loaded rig was
            // invisible on the back. Skip until laid out; heal any NaN.
            if !(ea.x.is_finite() && ea.y.is_finite() && eb.x.is_finite() && eb.y.is_finite()) { continue; }
            if !(c.c1.x.is_finite() && c.c1.y.is_finite() && c.c2.x.is_finite() && c.c2.y.is_finite()
                && c.v1.x.is_finite() && c.v1.y.is_finite() && c.v2.x.is_finite() && c.v2.y.is_finite()) {
                c.c1 = Vec2::ZERO; c.c2 = Vec2::ZERO; c.v1 = Vec2::ZERO; c.v2 = Vec2::ZERO;
            }
            if !(c.prev_a.x.is_finite() && c.prev_a.y.is_finite() && c.prev_b.x.is_finite() && c.prev_b.y.is_finite()) {
                c.prev_a = ea; c.prev_b = eb;
            }

            let mut a = c.over_a.unwrap_or(ea);
            let mut b = c.over_b.unwrap_or(eb);
            if c.dying >= 0.0 {
                let p = (c.dying / 0.55).clamp(0.0, 1.0);
                let fall = Vec2::new(c.drift * p, 430.0 * p * p);
                if c.detach_dst { b += fall; } else { a += fall; }
            }

            // Inertial feed: endpoint motion whips the cable. On the first
            // frame prev_* is still the constructor placeholder (unit rects
            // were Rect::NOTHING → NaN), so skip the feed and seed bulge side.
            let first = !c.prev_a.is_finite();
            let feed_a = if first { Vec2::ZERO } else { clamp_len((c.prev_a - a) / dt.max(1e-4), 2400.0) * 0.28 };
            let feed_b = if first { Vec2::ZERO } else { clamp_len((c.prev_b - b) / dt.max(1e-4), 2400.0) * 0.28 };
            c.v1 += feed_a;
            c.v2 += feed_b;
            if first {
                let mid_x = (a.x + b.x) * 0.5;
                c.bulge_side = if (mid_x - self.rack_w / 2.0).abs() < 26.0 {
                    if c.id % 2 == 0 { 1.0 } else { -1.0 }
                } else if mid_x < self.rack_w / 2.0 { -1.0 } else { 1.0 };
            }
            c.prev_a = a;
            c.prev_b = b;

            // Rest shape: gravity sag + lateral bulge for vertical runs.
            let d = b - a;
            let dist = d.length().max(1e-3);
            let dir = d / dist;
            let vert = dir.y.abs();
            let breathe = if c.kind == PortKind::Audio { 1.0 + 0.04 * c.level } else { 1.0 };
            let sag = (18.0 + dist * 0.20).min(170.0) * breathe;
            let bulge = dist * (0.06 + 0.30 * vert) * c.bulge_var * c.bulge_side;
            let stub = (dist * 0.12).clamp(10.0, 50.0);
            let t1 = Vec2::new(bulge + dir.x * stub, sag + dir.y * stub * 0.3);
            let t2 = Vec2::new(bulge - dir.x * stub, sag - dir.y * stub * 0.3);

            let steps = ((dt / (1.0 / 240.0)).ceil() as usize).clamp(1, 12);
            let h = dt / steps as f32;
            for _ in 0..steps {
                Self::step_spring(&mut c.c1, &mut c.v1, t1, h);
                Self::step_spring(&mut c.c2, &mut c.v2, t2, h);
            }

            // No simulated band: pulses are fired ONLY by real key presses
            // (fire_keys), so the rack shows exactly what is being played.

            // Pulses travel; arrivals excite the destination.
            let speed_mul = self.units.get(c.src).map(unit_time_mul).unwrap_or(1.0);
            for p in &mut c.pulses {
                if !p.active { continue; }
                p.t += p.speed * speed_mul * dt;
                if p.t >= 1.0 {
                    p.active = false;
                    if c.dying < 0.0 && n_arrivals < self.arrivals.len() {
                        self.arrivals[n_arrivals] = (c.dst, p.amp);
                        n_arrivals += 1;
                    }
                }
            }

            if c.kind == PortKind::Audio {
                // The engine's real per-edge level feeds the envelope; the
                // attack/decay smoothing below stays UI-side.
                if let (Some(l), true) = (&edge_levels, c.dying < 0.0) {
                    c.env = c.env.max(edge_display_level(l.load(c.id)));
                }
                c.env *= (-dt / 0.32).exp();
                c.level += (c.env - c.level) * (1.0 - (-dt / 0.05).exp());
                // Sounding audio cables flow: a traveling pulse every
                // AUDIO_PULSE_EVERY, amp ∝ level (MIDI pulses stay event-driven).
                if c.dying < 0.0 && c.level > AUDIO_PULSE_MIN {
                    c.audio_tick -= dt;
                    if c.audio_tick <= 0.0 {
                        let amp = c.level.min(1.0);
                        c.spawn_pulse(0.0, amp);
                        c.audio_tick = AUDIO_PULSE_EVERY;
                    }
                } else {
                    c.audio_tick = 0.0; // next sound spawns immediately
                }
            }
        }

        for &(unit, amp) in &self.arrivals[..n_arrivals] {
            let gain = self.units.get(unit).map(unit_out_gain).unwrap_or(1.0);
            if let Some(u) = self.units.get_mut(unit) { u.led = u.led.max(amp); }
            // No engine attached: arrivals fake the downstream audio envelope
            // so the chain still animates. With an engine, real per-edge
            // levels drive audio cables (a muted synth must not look live).
            if self.edge_levels.is_some() { continue; }
            for c in &mut self.cables {
                if c.kind == PortKind::Audio && c.src == unit && c.dying < 0.0 {
                    c.env = c.env.max(amp * 0.95 * gain);
                }
            }
        }

        // Master meter: the engine's real output peak, not a simulation.
        let target = self.out_level.as_ref()
            .map(|a| f32::from_bits(a.load(Ordering::Relaxed)))
            .unwrap_or(0.0);
        self.meter += (target - self.meter) * (1.0 - (-dt / 0.08).exp());
        // Peak hold: instant attack, slow release (~0.25/s).
        if self.meter >= self.meter_peak {
            self.meter_peak = self.meter;
        } else {
            self.meter_peak = (self.meter_peak - dt * 0.25).max(self.meter);
        }

        let selected = self.selected;
        self.cables.retain(|c| c.dying < 0.55);
        if let Some(sel) = selected {
            if !self.cables.iter().any(|c| c.id == sel) { self.selected = None; }
        }

        // Ghost wire.
        if self.pending.is_some() {
            let (pu, pj) = { let p = self.pending.as_ref().unwrap(); (p.unit, p.jack) };
            let origin = self.jack_pos(pu, pj);
            let side = p_side(pu, self);
            let p = self.pending.as_mut().unwrap();
            let (a, b) = if p.dir == Dir::Out { (origin, p.cursor) } else { (p.cursor, origin) };
            let d = b - a;
            let dist = d.length().max(1e-3);
            let dir = d / dist;
            let sag = (18.0 + dist * 0.20).min(170.0);
            let bulge = dist * (0.06 + 0.30 * dir.y.abs()) * side;
            let stub = (dist * 0.12).clamp(10.0, 50.0);
            let feed = clamp_len((p.prev_b - b) / dt.max(1e-4), 2400.0) * 0.28;
            p.v2 += feed;
            p.prev_b = b;
            let steps = ((dt / (1.0 / 240.0)).ceil() as usize).clamp(1, 12);
            let h = dt / steps as f32;
            for _ in 0..steps {
                Self::step_spring(&mut p.c1, &mut p.v1, Vec2::new(bulge + dir.x * stub, sag), h);
                Self::step_spring(&mut p.c2, &mut p.v2, Vec2::new(bulge - dir.x * stub, sag), h);
            }
            let _ = a;
        }
    }

    /// Rebuild local-space samples + arc-length LUT (LOD by on-screen size).
    fn tessellate(&mut self) {
        for c in &mut self.cables {
            let a = c.prev_a;
            let b = c.prev_b;
            let p = [a.to_pos2(), (a + c.c1).to_pos2(), (b + c.c2).to_pos2(), b.to_pos2()];
            c.curve = p;
            let rough = (p[3] - p[0]).length() + (p[1] - p[0]).length() + (p[2] - p[3]).length();
            let screen_len = rough * self.xf.sy.max(0.4);
            let n = ((screen_len / 11.0) as usize).clamp(10, MAX_SAMPLES - 1) + 1;
            c.n = n;
            let mut acc = 0.0;
            let mut prev = p[0];
            c.pts[0] = p[0];
            c.lut[0] = 0.0;
            for i in 1..n {
                let q = cubic(&p, i as f32 / (n - 1) as f32);
                acc += (q - prev).length();
                c.pts[i] = q;
                c.lut[i] = acc;
                prev = q;
            }
            c.total_len = acc.max(1e-3);
        }
    }

    // -- hit testing ----------------------------------------------------------

    /// Knob row center y: anchored to the unit bottom (face grid).
    fn knob_row_y(&self, unit: usize) -> f32 {
        self.unit_rects[unit].bottom() - FACE_PAD_B - KNOB_LABEL_H - KNOB_LABEL_GAP - KNOB_R
    }

    /// Right-to-left knob slot: pairs fill from the right (idx 0,1 = the
    /// rightmost pair, pinned knobs add pairs to the LEFT); a pair with one
    /// knob takes the slot next to its neighbours, never leaving a gap.
    fn knob_slot(n: usize, idx: usize) -> usize {
        let pair = idx / 2;
        let full = pair * 2 + 1 < n;
        pair * 2 + if full { 1 - idx % 2 } else { 0 }
    }

    fn knob_center(&self, unit: usize, idx: usize) -> Vec2 {
        let r = self.unit_rects[unit];
        let Some(u) = self.units.get(unit) else {
            return Vec2::new(r.left() + FACE_X, self.knob_row_y(unit));
        };
        let n = u.knobs.iter().filter(|k| !k.hidden).count();
        let ordinal = u.knobs[..idx].iter().filter(|k| !k.hidden).count();
        let slot = Self::knob_slot(n, ordinal);
        Vec2::new(r.right() - FACE_PAD_R - KNOB_SLOT_HALF - slot as f32 * KNOB_PITCH, self.knob_row_y(unit))
    }

    /// Display space left of the knob row (rack-local x start, width).
    fn display_area(&self, unit: usize) -> (f32, f32) {
        let r = self.unit_rects[unit];
        let start = r.left() + FACE_X;
        let n = self.units.get(unit).map_or(0, |u| u.knobs.iter().filter(|k| !k.hidden).count());
        let end = if n == 0 {
            r.right() - FACE_PAD_R
        } else {
            let slots = (0..n).map(|i| Self::knob_slot(n, i)).max().unwrap_or(0);
            r.right() - FACE_PAD_R - (slots + 1) as f32 * KNOB_PITCH - 8.0
        };
        (start, (end - start).max(0.0))
    }


    fn hit_test(&self, s: Pos2) -> Hit {
        if self.flipping || self.xf.sy < 0.9 { return Hit::Background; }
        // Panned-away rack parts are clipped: never hit what isn't drawn.
        if self.rack_clip.is_some_and(|c| !c.contains(s)) { return Hit::Background; }
        let l = self.xf.inv(s);
        let lp = l.to_pos2();

        match self.side {
            Side::Front => {
                for (ui, u) in self.units.iter().enumerate() {
                    if !self.visible(ui) { continue; }
                    for ki in 0..u.knobs.len() {
                        if u.knobs[ki].hidden { continue; }
                        if (l - self.knob_center(ui, ki)).length() < 23.0 {
                            return Hit::Knob { unit: ui, idx: ki };
                        }
                    }
                }
                for (ui, _) in self.units.iter().enumerate() {
                    if self.unit_rects[ui].contains(lp) { return Hit::Unit(ui); }
                }
            }
            Side::Back => {
                for (ui, u) in self.units.iter().enumerate() {
                    if !self.visible(ui) { continue; }
                    for ji in 0..u.jacks.len() {
                        if (l - self.jack_pos(ui, ji)).length() < 15.0 {
                            return Hit::Jack { unit: ui, jack: ji };
                        }
                    }
                }
                let mut best: Option<(u32, f32, f32)> = None;
                for c in &self.cables {
                    if c.dying >= 0.0 { continue; }
                    if !self.visible(c.src) || !self.visible(c.dst) { continue; }
                    let thresh = 8.0 + c.level * 3.0;
                    for i in 0..c.n.saturating_sub(1) {
                        let d = dist_sq_to_segment(lp, c.pts[i], c.pts[i + 1]);
                        if d < thresh * thresh && best.map_or(true, |(_, bd, _)| d < bd) {
                            // Grab parameter along the polyline.
                            let ab = c.pts[i + 1] - c.pts[i];
                            let t = if ab.length_sq() > 1e-9 { ((lp - c.pts[i]).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
                            let u = (i as f32 + t) / (c.n - 1).max(1) as f32;
                            best = Some((c.id, d, u));
                        }
                    }
                }
                if let Some((id, _, u)) = best { return Hit::Cable(id, u); }
                for (ui, _) in self.units.iter().enumerate() {
                    if self.unit_rects[ui].contains(lp) { return Hit::Unit(ui); }
                }
            }
        }
        Hit::Background
    }

    // -- input ----------------------------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context, rect: Rect) {
        // The "Delete rig?" modal owns all input (Esc/Del must reach it, not
        // the rack's cable shortcuts).
        if self.confirm_delete_rig.is_some()
            || self.save_rig_dialog.is_some()
            || self.load_rig_dialog.is_some()
        {
            return;
        }
        let mut toggle_play = false;
        let mut tap_tempo = false;
        let mut flip = false;
        let mut reset_knobs = false;
        let mut load_tpl: Option<Template> = None;
        let mut load_demo = false;
        let mut load_multi = false;
        let mut rig_switch: Option<usize> = None;
        let mut rig_cycle: i8 = 0;
        // Pan an overflowing rack with the wheel / trackpad — only over the
        // bare stage (never over a panel or window that scrolls itself).
        let ov = self.rack_overflow;
        if self.stage_view == StageView::Rack && (ov.x > 0.0 || ov.y > 0.0) {
            let over_stage = ctx.pointer_hover_pos().is_some_and(|p| self.stage_rect.contains(p)
                && ctx.layer_id_at(p).map_or(true, |l| l.order == egui::Order::Background));
            let d = ctx.input(|i| i.smooth_scroll_delta());
            if over_stage && d != Vec2::ZERO {
                // A plain wheel (y only) pans sideways when only x overflows.
                let p = self.rack_pan + if d.x == 0.0 && ov.y == 0.0 { Vec2::new(d.y, 0.0) } else { d };
                self.rack_pan = Vec2::new(p.x.clamp(-ov.x / 2.0, ov.x / 2.0), p.y.clamp(-ov.y / 2.0, ov.y / 2.0));
            }
        }
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Space) { toggle_play = true; }
            if i.consume_key(Modifiers::SHIFT, Key::T) { tap_tempo = true; }
            if i.consume_key(Modifiers::NONE, Key::Tab) { flip = true; }
            if i.consume_key(Modifiers::NONE, Key::R) { reset_knobs = true; }
            // I = info (MIDI + performance). Not D: D is a laptop note key.
            if i.consume_key(Modifiers::NONE, Key::I) { self.show_stats = !self.show_stats; }
            // Multi-rig big-view switching (visual only): 1–9 select, ←→ cycle.
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            for (d, k) in digits.iter().enumerate() {
                if i.consume_key(Modifiers::NONE, *k) { rig_switch = Some(d); }
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowLeft) { rig_cycle = -1; }
            if i.consume_key(Modifiers::NONE, Key::ArrowRight) { rig_cycle = 1; }
            // Multi-rig demo (two independent rigs), F4.
            if i.consume_key(Modifiers::NONE, Key::F4) { load_multi = true; }
            // Starter templates: F5/F6/F7, demo patch on F8 (task-17).
            if i.consume_key(Modifiers::NONE, Key::F5) { load_tpl = Some(Template::LeadReverb); }
            if i.consume_key(Modifiers::NONE, Key::F6) { load_tpl = Some(Template::DrumsBass); }
            if i.consume_key(Modifiers::NONE, Key::F7) { load_tpl = Some(Template::AmbientPad); }
            if i.consume_key(Modifiers::NONE, Key::F8) { load_demo = true; }
            // Plugin browser (task-18).
            if i.consume_key(Modifiers::NONE, Key::F9) {
                self.toggle_stage(StageView::Plugins);
            }
            // Hot-wire MIDI matrix.
            if i.consume_key(Modifiers::NONE, Key::F11) {
                self.toggle_stage(StageView::Matrix);
            }
            // Letter shortcuts to the views (user 2026-11-15) — letters the
            // laptop piano does not use: M mixer · N matrix · B browser.
            // (C chat and V keyboard are dock toggles, handled before the panels.)
            if i.consume_key(Modifiers::NONE, Key::M) {
                self.toggle_stage(StageView::Mixer);
            }
            if i.consume_key(Modifiers::NONE, Key::N) {
                self.toggle_stage(StageView::Matrix);
            }
            if i.consume_key(Modifiers::NONE, Key::B) {
                self.toggle_stage(StageView::Plugins);
            }
            if i.consume_key(Modifiers::NONE, Key::F10) {
                self.toggle_stage(StageView::Mixer);
            }
            if i.consume_key(Modifiers::NONE, Key::Questionmark) || i.consume_key(Modifiers::NONE, Key::F1) {
                self.show_help = !self.show_help;
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                if self.show_help { self.show_help = false; }
                else if self.knob_menu.is_some() { self.knob_menu = None; }
                else if self.pending.is_some() { self.pending = None; }
                else if self.cc_learn.is_some() { self.cc_learn = None; }
                else if self.stage_view == StageView::Matrix && self.rule_editor.is_some() { self.rule_editor = None; }
                else if self.confirm_delete_session.is_some() { self.confirm_delete_session = None; }
                else if self.stage_view != StageView::Rack { self.stage_view = StageView::Rack; }
                else { self.selected = None; }
            }
            if i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace) {
                if let Some(sel) = self.selected {
                    if let Some(ci) = self.cables.iter().position(|c| c.id == sel) {
                        self.unplug_cable(ci, true);
                        self.selected = None;
                    }
                }
            }
        });
        if toggle_play { self.toggle_play(); }
        if tap_tempo {
            self.ui_transport_grace = self.ui_time + 0.6;
            self.tap_tempo_at(self.tap_origin.elapsed().as_secs_f64());
        }
        if flip { self.on_tab(); }
        if let Some(t) = load_tpl { self.load_template(t); }
        if load_demo { self.load_demo_patch(); }
        if load_multi { self.load_multi_rig_demo(); }
        if (rig_switch.is_some() || rig_cycle != 0) && self.rig_count() <= 1 {
            // Not a silent no-op: say why nothing switched.
            self.notify_patch("one rig loaded — its stacks share a master · F4 loads two independent rigs, then 1–9 / ←→ switch");
        }
        if let Some(r) = rig_switch { self.set_active_rig(r); }
        if rig_cycle != 0 {
            let n = self.rig_count();
            if n > 1 {
                let next = (self.active_rig as i8 + rig_cycle).rem_euclid(n as i8) as usize;
                self.set_active_rig(next);
            }
        }
        if reset_knobs {
            for u in &mut self.units {
                for k in &mut u.knobs { k.value = k.default; }
            }
        }

        let (pointer, pressed, down, released, right_clicked, ctrl, shift) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.primary_pressed(), i.pointer.primary_down(),
             i.pointer.primary_released(), i.pointer.secondary_clicked(), i.modifiers.ctrl,
             i.modifiers.shift)
        });

        let Some(s) = pointer else {
            self.press = None;
            self.drag = Drag::None;
            return;
        };

        // Stage view mode: the rack is not on screen, so its hit-testing is inert
        // except the masthead's Play circle (Tab returns to the rack). The
        // stage's egui widgets handle their own input.
        if self.stage_view != StageView::Rack {
            let hit = if self.show_help { Hit::Background } else {
                match self.hit_test(s) { Hit::Play => Hit::Play, _ => Hit::Background }
            };
            self.hover = Some(hit);
            if self.show_help {
                if pressed { self.show_help = false; }
                return;
            }
            if pressed { self.press = Some((hit, s)); }
            if released {
                let moved = self.press.map_or(0.0, |(_, p)| (s - p).length());
                let press_hit = self.press.map(|(h, _)| h);
                self.press = None;
                // A rack drag that began before entering the matrix ends here.
                if let Drag::Plug { id, .. } = self.drag {
                    if let Some(c) = self.cables.iter_mut().find(|c| c.id == id) { c.over_a = None; c.over_b = None; }
                }
                self.drag = Drag::None;
                if moved < 6.0 {
                    match press_hit {
                        Some(Hit::Play) => self.toggle_play(),
                        _ => {}
                    }
                }
            }
            if matches!(hit, Hit::Play) { ctx.set_cursor_icon(CursorIcon::PointingHand); }
            return;
        }

        if right_clicked {
            if self.pending.is_some() {
                self.pending = None;
            } else if self.side == Side::Front {
                if let Hit::Knob { unit, idx } = self.hit_test(s) {
                    let node_id = self.units[unit].node_id;
                    let target = self.units[unit].knobs[idx].cc_target();
                    if let Some((param, tgt)) = target {
                        let same_learning = self.cc_learn.as_ref().is_some_and(|l| {
                            l.dest == node_id && l.param == param && l.target == tgt
                        });
                        let menu = KnobMenu { node: node_id, param, target: tgt, pos: s };
                        if same_learning {
                            // Learning keeps running until the menu decides:
                            // Cancel MIDI learn, or Reset (#20 — the only way
                            // to reset an unbound knob without the global R).
                            self.knob_menu = Some(menu);
                        } else if self.knob_cc_binding(node_id, param, tgt).is_some() {
                            self.cc_learn = None;
                            self.knob_menu = Some(menu);
                        } else {
                            self.knob_menu = None;
                            self.cc_learn = Some(CcLearn {
                                source: SourceId::Keys,
                                dest: node_id,
                                param,
                                target: tgt,
                                replace_existing: true,
                            });
                        }
                    } else {
                        self.knob_menu = None;
                        self.units[unit].knobs[idx].value = self.units[unit].knobs[idx].default;
                    }
                } else {
                    self.knob_menu = None;
                    self.cc_learn = None;
                }
            } else {
                self.knob_menu = None;
            }
        }

        if self.knob_menu.is_some() && self.knob_menu_rect.contains(s) {
            self.hover = Some(Hit::Background);
            return;
        }
        if pressed && self.knob_menu.is_some() {
            self.knob_menu = None;
            self.hover = Some(Hit::Background);
            return;
        }

        let hit = if self.show_help { Hit::Background } else { self.hit_test(s) };
        self.hover = Some(hit);
        let l = self.xf.inv(s);

        if self.show_help {
            if pressed { self.show_help = false; }
            return;
        }

        if pressed {
            self.press = Some((hit, s));
            // Ctrl+press on a stack unit → rearrange drag (task-21). Overrides
            // knob/jack sub-hits so the whole box is grabbable; KEYS/MASTER and
            // non-Ctrl presses fall through to the normal behavior untouched.
            if ctrl {
                if let Some(unit) = self.stack_unit_at(l) {
                    let grab = l - self.unit_rects[unit].min.to_vec2();
                    self.drag = Drag::Unit { unit, grab };
                }
            }
            if !matches!(self.drag, Drag::Unit { .. }) {
                match hit {
                    Hit::Knob { unit, idx } => {
                        self.drag = Drag::Knob { unit, idx, start_val: self.units[unit].knobs[idx].value, press_y: s.y };
                    }
                    Hit::Jack { unit, jack } if self.side == Side::Back => {
                        let j = &self.units[unit].jacks[jack];
                        // Shift on an occupied output = fan out a new cable
                        // (handled as a pending wire below), not grab the plug.
                        let fan_out = shift && j.dir == Dir::Out;
                        if !fan_out {
                            if let Some(ci) = self.cable_at_jack(unit, j.dir, j.port) {
                                self.drag = Drag::Plug { id: self.cables[ci].id, end: j.dir };
                            }
                        }
                    }
                    Hit::Cable(id, u) if self.side == Side::Back => {
                        self.drag = Drag::Cable { id, u };
                    }
                    // Face drag fields (custom-painted, so no egui focus can
                    // swallow rack keys — rule 12 by construction).
                    Hit::Unit(ui) if self.side == Side::Front => {
                        let lp = l.to_pos2();
                        let split = self.units[ui].split;
                        if let (Some(p), Some((_, readout))) = (split, self.split_rects(ui)) {
                            if readout.contains(lp) {
                                self.drag = Drag::Split { unit: ui, start: p, press: s, cur: p };
                            }
                        }
                        if self.bpm_rects(ui).map_or(false, |b| b.value.contains(lp)) {
                            self.drag = Drag::Bpm { start: self.bpm, press: s };
                        }
                    }
                    _ => {}
                }
            }
        }

        if down {
            match self.drag {
                Drag::Knob { unit, idx, start_val, press_y } => {
                    let v = (start_val + (press_y - s.y) * 0.006).clamp(0.0, 1.0);
                    self.units[unit].knobs[idx].value = v;
                }
                Drag::Plug { id, end } => {
                    match self.live_cable(id) {
                        Some(ci) => {
                            let c = &mut self.cables[ci];
                            if end == Dir::Out { c.over_a = Some(l); } else { c.over_b = Some(l); }
                        }
                        None => self.drag = Drag::None,
                    }
                }
                Drag::Cable { id, u } => {
                    if let Some(ci) = self.live_cable(id) {
                        let c = &mut self.cables[ci];
                        let pt = c.point_at_arc(u).to_vec2();
                        let delta = clamp_len(l - pt, 240.0);
                        // Soft grab: forces into both spring particles.
                        c.v1 += delta * 160.0 * (1.0 - u) * dt_of(ctx);
                        c.v2 += delta * 160.0 * u * dt_of(ctx);
                        c.c1 += delta * 3.0 * (1.0 - u) * dt_of(ctx);
                        c.c2 += delta * 3.0 * u * dt_of(ctx);
                        // Tug near an end → unplug.
                        let a = c.prev_a;
                        let b = c.prev_b;
                        if u < 0.16 && (l - a).length() > 48.0 {
                            self.unplug_cable(ci, false);
                            self.drag = Drag::None;
                        } else if u > 0.84 && (l - b).length() > 48.0 {
                            self.unplug_cable(ci, true);
                            self.drag = Drag::None;
                        }
                    } else {
                        self.drag = Drag::None;
                    }
                }
                Drag::Unit { unit, grab } => {
                    // Track the cursor in rack-local space; cable endpoints read
                    // unit_rects, so they stay attached while the box moves.
                    self.drag_cursor = Some(l);
                    let size = self.unit_rects[unit].size();
                    self.unit_rects[unit] = Rect::from_min_size((l - grab).to_pos2(), size);
                }
                Drag::Split { unit, start, press, cur } => {
                    // Up/right raises the point: one semitone per 6px.
                    let d = ((press.y - s.y) + (s.x - press.x)) / 6.0;
                    let p = (start as f32 + d).round().clamp(1.0, 127.0) as u8;
                    if p != cur {
                        self.drag = Drag::Split { unit, start, press, cur: p };
                        let nid = self.units[unit].node_id;
                        self.ui_node_grace.insert(nid, self.ui_time + 0.6);
                        self.set_keys_split(unit, Some(p));
                    }
                }
                Drag::Bpm { start, press } => {
                    // Up/right = faster: one BPM per 3px, whole BPMs only.
                    let d = ((press.y - s.y) + (s.x - press.x)) / 3.0;
                    let bpm = (start + d).round().clamp(20.0, 300.0);
                    if bpm != self.bpm {
                        self.ui_transport_grace = self.ui_time + 0.6;
                        self.set_bpm(bpm);
                    }
                }
                Drag::None => {
                    // Drag-from-jack: one-gesture patching.
                    if let Some((Hit::Jack { unit, jack }, press_pos)) = self.press {
                        if self.pending.is_none() && self.side == Side::Back && (s - press_pos).length() > 7.0 {
                            let (occupied, fan_out) = {
                                let j = &self.units[unit].jacks[jack];
                                (self.cable_at_jack(unit, j.dir, j.port).is_some(), shift && j.dir == Dir::Out)
                            };
                            if !occupied || fan_out { self.start_pending(unit, jack, l); }
                        }
                    }
                }
            }
            if let Some(p) = &mut self.pending { p.cursor = l; }
        }

        if released {
            let moved = self.press.map_or(0.0, |(_, p)| (s - p).length());
            let press_hit = self.press.map(|(h, _)| h);
            self.press = None;
            let was_plug = matches!(self.drag, Drag::Plug { .. });
            let plug_info = if let Drag::Plug { id, end } = self.drag { Some((id, end)) } else { None };
            let unit_drag = if let Drag::Unit { unit, .. } = self.drag { Some(unit) } else { None };
            self.drag = Drag::None;
            self.drag_cursor = None;
            self.drop_ghost = None;

            // Ctrl+drag release (task-21): reorder the stacks, then auto-layout
            // reflows. Only when actually moved, so a Ctrl+click is a no-op.
            if let Some(unit) = unit_drag {
                if moved >= 6.0 { self.drop_unit(unit, l); }
            }

            // Plug drag release: on a compatible jack → move the plug there;
            // far from the jack → unplug; near → snap back; click → unplug.
            if let Some((id, end)) = plug_info {
                self.release_plug(id, end, l, moved, hit);
            }

            // Drag-to-patch release.
            if let Some(p) = &self.pending {
                if let Hit::Jack { unit, jack } = hit {
                    let j = &self.units[unit].jacks[jack];
                    let from = (p.unit, self.units[p.unit].jacks[p.jack].port, p.dir, p.kind);
                    if self.try_connect(from, (unit, j.port, j.dir, self.units[unit].kind(j.dir, j.port))) {
                        self.pending = None;
                        return;
                    }
                }
            }

            if moved < 6.0 && !was_plug {
                match press_hit {
                    Some(Hit::Play) => self.toggle_play(),
                    Some(Hit::Jack { unit, jack }) if self.side == Side::Back => {
                        let j = &self.units[unit].jacks[jack];
                        let fan_out = shift && j.dir == Dir::Out;
                        if let Some(p) = &self.pending {
                            let from = (p.unit, self.units[p.unit].jacks[p.jack].port, p.dir, p.kind);
                            if unit == p.unit && jack == p.jack {
                                self.pending = None;
                            } else if self.try_connect(from, (unit, j.port, j.dir, self.units[unit].kind(j.dir, j.port))) {
                                self.pending = None;
                            }
                        } else if let Some(ci) = self.cable_at_jack(unit, j.dir, j.port).filter(|_| !fan_out) {
                            self.unplug_cable(ci, j.dir == Dir::In);
                        } else {
                            self.start_pending(unit, jack, l);
                        }
                    }
                    Some(Hit::Cable(id, _)) if self.side == Side::Back => {
                        if self.live_cable(id).is_some() {
                            self.selected = if self.selected == Some(id) { None } else { Some(id) };
                        }
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.split_rects(ui).map_or(false, |(chip, field)| chip.contains(l.to_pos2())
                            // Unsplit, "SPLIT · OFF" is ONE control: the OFF part toggles too.
                            || (self.shown_split(ui).is_none() && field.contains(l.to_pos2()))) => {
                        // Keys-face SPLIT chip: split at C4 / unsplit.
                        let next = if self.units[ui].split.is_some() { None } else { Some(DEFAULT_SPLIT) };
                        let nid = self.units[ui].node_id;
                        self.ui_node_grace.insert(nid, self.ui_time + 0.6);
                        self.set_keys_split(ui, next);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front && self.on_face_field(ui, l.to_pos2()) => {
                        // A click on a drag readout (split point / BPM) is not a ping.
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.bpm_rects(ui).map_or(false, |b| b.minus.map_or(false, |r| r.contains(l.to_pos2()))) => {
                        self.ui_transport_grace = self.ui_time + 0.6;
                        self.set_bpm(self.bpm.round() - 1.0);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.bpm_rects(ui).map_or(false, |b| b.plus.map_or(false, |r| r.contains(l.to_pos2()))) => {
                        self.ui_transport_grace = self.ui_time + 0.6;
                        self.set_bpm(self.bpm.round() + 1.0);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.metro_action_rects(ui).map_or(false, |m| m.tap.contains(l.to_pos2())) => {
                        self.ui_transport_grace = self.ui_time + 0.6;
                        self.tap_tempo_at(self.tap_origin.elapsed().as_secs_f64());
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.metro_action_rects(ui).map_or(false, |m| m.count_in.contains(l.to_pos2())) => {
                        self.ui_transport_grace = self.ui_time + 0.6;
                        self.set_count_in(!self.count_in_enabled, self.count_in_bars);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.transport_chip_rect(ui).map_or(false, |c| c.contains(l.to_pos2())) => {
                        // Per-instrument Play/Stop chip on the unit face.
                        let (node, playing) = (self.units[ui].node_id, self.units[ui].playing);
                        self.ui_node_grace.insert(node, self.ui_time + 0.6);
                        self.set_node_playing(node, !playing);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.audio_in_monitor_rect(ui).map_or(false, |c| c.contains(l.to_pos2())) => {
                        let on = self.units[ui].knobs.iter().find(|k| k.param == Some(P_AUDIO_IN_MONITOR))
                            .map_or(false, |k| k.value < 0.5);
                        self.set_audio_in_monitor(ui, on);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.audio_in_channel_rect(ui).map_or(false, |c| c.contains(l.to_pos2())) => {
                        self.cycle_audio_in_channel(ui);
                    }
                    Some(Hit::Unit(ui)) if self.side == Side::Front
                        && self.audio_in_device_rect(ui).map_or(false, |c| c.contains(l.to_pos2())) => {
                        self.cycle_audio_in_device(ui);
                    }
                    Some(Hit::Unit(ui)) => {
                        // Double-click a CLAP or LV2 plugin unit → toggle its inline editor.
                        // A single click still pings the unit.
                        let lv2_info = self.units.get(ui).and_then(|u| {
                            self.base_graph.nodes.iter().find(|n| n.id == u.node_id).and_then(|n| match &n.kind {
                                NodeKind::Lv2 { uri } => Some((u.node_id, uri.as_str().to_string(), u.name.clone())),
                                _ => None,
                            })
                        });
                        let is_clap = self.units.get(ui).map_or(false, |u| matches!(u.node_kind, UnitKind::Clap));
                        let has_editor = is_clap || lv2_info.is_some();
                        let now = std::time::Instant::now();
                        let double = has_editor && self.last_unit_click
                            .map_or(false, |(pu, pt)| pu == ui && now.duration_since(pt).as_millis() < 400);
                        if double {
                            self.last_unit_click = None;
                            if let Some(info) = lv2_info {
                                self.lv2_editor_request = Some(info);
                            } else {
                                match self.rig_of_unit(ui) {
                                    Some(rig) => self.editor_request = Some(rig),
                                    None => self.editor_status = "no hosted synth voice for this unit".into(),
                                }
                            }
                        } else {
                            if has_editor { self.last_unit_click = Some((ui, now)); }
                            self.ping_unit(ui);
                        }
                    }
                    Some(Hit::Background) | None => {
                        if self.pending.is_some() { self.pending = None; }
                        else { self.selected = None; }
                    }
                    _ => {}
                }
            }
        }

        // Cursor affordances.
        let on_field = match self.hover {
            Some(Hit::Unit(u)) if self.side == Side::Front => self.on_face_field(u, l.to_pos2()),
            _ => false,
        };
        let icon = if matches!(self.drag, Drag::Unit { .. }) {
            CursorIcon::Grabbing
        } else if ctrl && matches!(self.hover, Some(Hit::Unit(_))) {
            CursorIcon::Grab
        } else if on_field || matches!(self.drag, Drag::Split { .. } | Drag::Bpm { .. }) {
            CursorIcon::ResizeVertical
        } else {
            match self.hover {
                Some(Hit::Play) | Some(Hit::Jack { .. }) => CursorIcon::PointingHand,
                Some(Hit::Knob { .. }) => CursorIcon::ResizeVertical,
                Some(Hit::Cable(..)) => match self.drag {
                    Drag::Cable { .. } => CursorIcon::Grabbing,
                    _ => CursorIcon::Grab,
                },
                Some(Hit::Unit(_)) => CursorIcon::PointingHand,
                _ => CursorIcon::Default,
            }
        };
        ctx.set_cursor_icon(icon);
        let _ = rect;
    }

    // -- drawing ----------------------------------------------------------

    fn draw(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter();
        theme::paper_bg(&painter, rect);

        // The rack IS the reference's single inverted hero panel (§4.2):
        // flat near-black, radius 36, borderless, NO shadow (it sits on the
        // page; only floating sheets get shadows). Template loads fade in.
        let rack_local = Rect::from_min_size(Pos2::ZERO, Vec2::new(self.rack_w, self.rack_h));
        let rack = self.xf.rect(rack_local);
        let mut rp = painter.clone();
        if let Some(clip) = self.rack_clip { rp.set_clip_rect(clip.intersect(rect)); }
        rp.set_opacity(self.load_fade);
        rp.rect_filled(rack, 36.0, CHASSIS);

        if self.xf.sy > 0.04 {
            self.draw_rails(&rp);
            match self.side {
                Side::Front => self.draw_front(&rp),
                Side::Back => self.draw_back(&rp),
            }
        }

        self.draw_pan_bars(&painter);
        self.draw_hint(&painter, rect);
        if self.show_help { self.draw_help(&painter, rect); }
    }

    /// Hairline pan indicators when the rack overflows the stage: a thumb
    /// showing the visible part on the bottom / right edge of the view.
    fn draw_pan_bars(&self, painter: &egui::Painter) {
        let Some(view) = self.rack_clip.map(|c| c.shrink(8.0)) else { return };
        let ov = self.rack_overflow;
        if ov.x > 0.0 {
            let track = Rect::from_min_max(Pos2::new(view.left() + 40.0, view.bottom() + 6.0), Pos2::new(view.right() - 40.0, view.bottom() + 9.0));
            let frac = view.width() / self.rack_w;
            let t = 0.5 - self.rack_pan.x / ov.x.max(1.0); // pan +ov/2 = left end
            let w = track.width() * frac;
            let x = track.left() + (track.width() - w) * t.clamp(0.0, 1.0);
            painter.rect_filled(track, 1.5, theme::HAIRLINE);
            painter.rect_filled(Rect::from_min_size(Pos2::new(x, track.top()), Vec2::new(w, track.height())), 1.5, theme::CREAM_DIM);
        }
        if ov.y > 0.0 {
            let track = Rect::from_min_max(Pos2::new(view.right() + 4.0, view.top() + 20.0), Pos2::new(view.right() + 7.0, view.bottom() - 20.0));
            let frac = view.height() / self.rack_h;
            let t = 0.5 - self.rack_pan.y / ov.y.max(1.0);
            let h = track.height() * frac;
            let y = track.top() + (track.height() - h) * t.clamp(0.0, 1.0);
            painter.rect_filled(track, 1.5, theme::HAIRLINE);
            painter.rect_filled(Rect::from_min_size(Pos2::new(track.left(), y), Vec2::new(track.width(), h)), 1.5, theme::CREAM_DIM);
        }
    }

    /// The MIDI matrix as a MAIN-STAGE view (one-window rule, F11 / Matrix
    /// chip): masthead stays, the rack is replaced by a flat dark hero sheet
    /// (help-card motif, radius 28) holding the grid and — when a cell's rule
    /// is being edited — the inline rule editor on the right.
    fn matrix_stage(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter().clone();
        theme::paper_bg(&painter, rect);

        let (sheet, inner) = self.stage_sheet_rects(rect, 160.0);
        painter.rect_filled(sheet, 28.0, theme::DARK_PANEL);
        let help = self.show_help;
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.add_enabled_ui(!help, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("MIDI Matrix").font(theme::medium_font(18.0)).color(theme::INK));
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("any source → any rig · click: route · right-click: shape the rule · Tab / Esc: back to the rack")
                        .size(13.0).color(theme::INK_2));
                });
                ui.add_space(12.0);
                let editor_w = if self.rule_editor.is_some() { 320.0 } else { 0.0 };
                let h = ui.available_height();
                ui.horizontal_top(|ui| {
                    let gap = if editor_w > 0.0 { 25.0 } else { 0.0 };
                    let grid_w = (ui.available_width() - editor_w - gap).max(200.0);
                    ui.allocate_ui_with_layout(egui::vec2(grid_w, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                        egui::ScrollArea::both()
                            .id_salt("matrix_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // Center the table in the sheet while it fits
                                // (a top-left huddle left the sheet empty — review).
                                // Centered on the block's MEASURED width (table +
                                // legend, last frame) — an estimate of the table
                                // alone left the block visibly right of center.
                                let block_w = if self.matrix_block_w > 0.0 { self.matrix_block_w } else { 600.0 };
                                // Vertically: a third of the free height above (optical
                                // center), not pinned to the sheet's top edge.
                                let top_pad = ((h - self.matrix_block_h) / 3.0).max(12.0);
                                ui.horizontal_top(|ui| {
                                    ui.add_space(((grid_w - block_w) / 2.0).max(0.0));
                                    let r = ui.vertical(|ui| { ui.add_space(top_pad); self.matrix_grid_ui(ui); }).response.rect;
                                    self.matrix_block_w = r.width();
                                    self.matrix_block_h = r.height() - top_pad;
                                });
                            });
                    });
                    if editor_w > 0.0 {
                        ui.add_space(12.0);
                        let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, h), egui::Sense::hover());
                        ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE));
                        ui.add_space(12.0);
                        ui.allocate_ui_with_layout(egui::vec2(editor_w, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("rule_editor_scroll")
                                .max_height(h)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.rule_editor_ui(ui);
                                });
                        });
                    }
                });
            });
        });

        if self.show_help { self.draw_help(&painter, rect); }
    }

    /// Audio sources of a rig, with the knob that sets each one's level:
    /// synths, LV2 instruments and generator units (no audio input — system
    /// audio, player, metronome). Effects and Keys are not sources.
    fn mixer_sources(&self, rig: u8) -> Vec<(usize, Option<usize>)> {
        self.units.iter().enumerate()
            .filter(|(_, u)| u.rig == rig)
            .filter(|(_, u)| match u.node_kind {
                UnitKind::Clap => true,
                UnitKind::AudioIn => true,
                UnitKind::Keys | UnitKind::Master => false,
                UnitKind::Dsp(_) => u.lv2_inst
                    || (!u.jacks.iter().any(|j| j.dir == Dir::In) && u.jacks.iter().any(|j| j.dir == Dir::Out)),
            })
            .map(|(i, u)| (i, u.knobs.iter().position(|k| k.param.is_some()
                && matches!(k.label.as_str(), "LEVEL" | "GAIN" | "VOLUME" | "VOL"))))
            .collect()
    }

    /// Set a unit knob from the mixer and publish it (same path as the rack).
    fn set_unit_knob(&mut self, unit: usize, knob: usize, v: f32) {
        let Some(u) = self.units.get_mut(unit) else { return };
        let Some(k) = u.knobs.get_mut(knob) else { return };
        k.value = v.clamp(0.0, 1.0);
        if let (Some(p), Some(bus)) = (k.param, &self.bus) {
            if bus.lock().unwrap().send(Command::SetParam { node: u.node_id, param: p, value: k.value as f64 }) {
                k.sent = Some(k.value);
            }
        }
    }

    /// Mixer stage: every source → its rig bus → Master out, as channel
    /// strips. Source faders drive the units' own level knobs; the rig bus is
    /// the engine's per-rig mix (vol/pan/mute/solo). A main-stage view like the
    /// matrix (rule 8) — Tab / Esc return to the rack.
    fn mixer_stage(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        self.stage_rect = rect;
        let painter = ui.painter().clone();
        theme::paper_bg(&painter, rect);
        let (sheet, inner) = self.stage_sheet_rects(rect, 200.0);
        painter.rect_filled(sheet, 28.0, theme::DARK_PANEL);
        let help = self.show_help;
        let n = self.rig_count();
        self.mixer_fader_rects.clear();
        let mut switch: Option<usize> = None;
        // System audio (#31): other apps' streams + the output choice.
        let app_streams = self.app_streams_shown();
        let mut stream_changes: Vec<(u64, Option<f32>, Option<bool>)> = Vec::new();
        let mut out_pick: Option<Option<String>> = None;
        let (outputs, out_current) = self.audio_out.lock()
            .map(|s| (s.outputs.clone(), s.current.clone())).unwrap_or_default();
        let out_wanted = self.wanted_output.lock().ok().and_then(|w| w.clone());
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.add_enabled_ui(!help, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Mixer").font(theme::medium_font(18.0)).color(theme::INK));
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("every source → its rig bus → Master out · Tab / Esc: back to the rack")
                        .size(13.0).color(theme::INK_2));
                });
                ui.add_space(14.0);
                let h = ui.available_height();
                // Strip height: name + sub + fader + value + bus controls.
                let fader_h = (h - 210.0).clamp(90.0, 480.0);
                ui.horizontal_top(|ui| {
                    let scroll_w = ui.available_width().max(200.0);
                    ui.allocate_ui_with_layout(egui::vec2(scroll_w, h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        // Center the strips in the sheet while they fit (a left-
                        // huddled mixer left most of the sheet empty — review).
                        let gap = ui.spacing().item_spacing.x;
                        let strips: usize = (0..n).map(|r| self.mixer_sources(r as u8).len() + 1).sum();
                        let sys = if app_streams.is_empty() { 0.0 } else { app_streams.len() as f32 * (STRIP_W + gap) + 25.0 + gap };
                        let content_w = strips as f32 * (STRIP_W + gap) + n as f32 * (25.0 + gap) + sys + 170.0 + 2.0 * gap;
                        let pad = ((scroll_w - content_w) / 2.0).max(0.0);
                        egui::ScrollArea::horizontal().id_salt("mixer_scroll").auto_shrink([false, false]).show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                ui.add_space(pad);
                                for rig in 0..n {
                                    if rig > 0 {
                                        let (r, _) = ui.allocate_exact_size(egui::vec2(25.0, h - 8.0), egui::Sense::hover());
                                        ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE));
                                    }
                                    ui.vertical(|ui| {
                                        let name = self.rig_name(rig);
                                        // A group HEADING, not a chip: a filled chip read as a
                                        // filter/tab while every rig is shown (vision review).
                                        // The rig in the rack view gets an "IN VIEW" tag.
                                        let group_w = (self.mixer_sources(rig as u8).len() + 1) as f32 * (STRIP_W + ui.spacing().item_spacing.x) - ui.spacing().item_spacing.x;
                                        // Heading truly centered over ITS strips; the "shown in
                                        // rack" tag sits on a subline reserved for every rig, so
                                        // headings stay centered and strips stay aligned.
                                        ui.allocate_ui_with_layout(egui::vec2(group_w, 36.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                                            let head = ui.add(egui::Label::new(egui::RichText::new(format!("{} · {}", rig + 1, name))
                                                .font(theme::medium_font(14.0)).color(theme::INK)).sense(egui::Sense::click()))
                                                .on_hover_text("show this rig in the rack view");
                                            if head.hovered() { ui.ctx().set_cursor_icon(CursorIcon::PointingHand); }
                                            if head.clicked() { switch = Some(rig); }
                                            if rig == self.active_rig && n > 1 {
                                                theme::tracked_label(ui, "SHOWN IN RACK", 9.0, 0.14, theme::CREAM_DIM); // neutral: teal = live
                                            } else {
                                                ui.add_space(12.0);
                                            }
                                        });
                                        // Group rule: the heading spans its sources + bus.
                                        let (rule, _) = ui.allocate_exact_size(egui::vec2(group_w, 5.0), egui::Sense::hover());
                                        ui.painter().line_segment([rule.left_center(), rule.right_center()], Stroke::new(1.0, theme::HAIRLINE));
                                        ui.add_space(4.0);
                                        ui.horizontal_top(|ui| {
                                            for (unit, knob) in self.mixer_sources(rig as u8) {
                                                let (uname, sub, level) = {
                                                    let u = &self.units[unit];
                                                    (sentence_case(&u.name), u.sub.clone(), u.led)
                                                };
                                                let mut v = knob.map(|k| self.units[unit].knobs[k].value);
                                                mixer_strip(ui, &uname, &sub, &mut v, level, fader_h);
                                                if let (Some(k), Some(v)) = (knob, v) {
                                                    if (v - self.units[unit].knobs[k].value).abs() > f32::EPSILON {
                                                        self.set_unit_knob(unit, k, v);
                                                    }
                                                }
                                            }
                                            // The rig bus: engine per-rig mix.
                                            let level = self.rig_levels.get(rig).copied().unwrap_or(0.0);
                                            let mut m = self.rig_mix[rig.min(7)];
                                            let mut v = Some(m.vol);
                                            // One column: fader, then PAN and M/S directly under it.
                                            let mut changed = false;
                                            ui.allocate_ui_with_layout(egui::vec2(STRIP_W, fader_h + 150.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                                                let fr = mixer_strip(ui, "Rig bus", "→ MASTER OUT", &mut v, level, fader_h);
                                                self.mixer_fader_rects.push(fr);
                                                changed = v != Some(m.vol);
                                                m.vol = v.unwrap_or(m.vol);
                                                ui.add_space(4.0);
                                                ui.horizontal(|ui| {
                                                    ui.add_space(8.0);
                                                    theme::tracked_label(ui, "PAN", 9.0, 0.12, theme::INK_2);
                                                    let mut p = m.pan;
                                                    let r = drag_only(ui.add(egui::DragValue::new(&mut p).range(-1.0..=1.0).speed(0.02)
                                                        .custom_formatter(|v, _| pan_text(v as f32))
                                                        .custom_parser(parse_pan)));
                                                    if r.on_hover_text("rig pan (L −1 … +1 R) · drag").changed() { m.pan = p; changed = true; }
                                                });
                                                ui.add_space(4.0);
                                                ui.horizontal(|ui| {
                                                    ui.add_space(18.0);
                                                    ui.spacing_mut().item_spacing.x = 4.0;
                                                    let mb = egui::Button::new(egui::RichText::new("M").size(11.0)
                                                            .color(if m.mute { theme::DARK_PANEL } else { theme::CREAM_MID }))
                                                        .fill(if m.mute { theme::CHIP_ON } else { theme::DARK_PANEL_HI })
                                                        .corner_radius(2.0).min_size(egui::vec2(26.0, 22.0));
                                                    if ui.add(mb).on_hover_text("mute this rig").clicked() { m.mute = !m.mute; changed = true; }
                                                    let sb = egui::Button::new(egui::RichText::new("S").size(11.0)
                                                            .color(if m.solo { theme::DARK_PANEL } else { theme::CREAM_MID }))
                                                        .fill(if m.solo { theme::LIVE_TEAL } else { theme::DARK_PANEL_HI })
                                                        .corner_radius(2.0).min_size(egui::vec2(26.0, 22.0));
                                                    if ui.add(sb).on_hover_text("solo this rig").clicked() { m.solo = !m.solo; changed = true; }
                                                });
                                            });
                                            if changed {
                                                self.ui_mix_grace[rig.min(7)] = self.ui_time + 0.6;
                                                self.rig_mix[rig.min(7)] = m;
                                                self.send_rig_mix(rig);
                                            }
                                        });
                                    });
                                }
                                // System audio: every other app that is playing
                                // (YouTube in the browser, …) gets a strip —
                                // its PipeWire stream volume and mute.
                                if !app_streams.is_empty() {
                                    let (r, _) = ui.allocate_exact_size(egui::vec2(25.0, h - 8.0), egui::Sense::hover());
                                    ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE));
                                    ui.vertical(|ui| {
                                        let gap = ui.spacing().item_spacing.x;
                                        let group_w = app_streams.len() as f32 * (STRIP_W + gap) - gap;
                                        ui.allocate_ui_with_layout(egui::vec2(group_w, 36.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                                            ui.label(egui::RichText::new("System audio").font(theme::medium_font(14.0)).color(theme::INK));
                                            theme::tracked_label(ui, "OTHER APPS", 9.0, 0.14, theme::CREAM_DIM);
                                        });
                                        let (rule, _) = ui.allocate_exact_size(egui::vec2(group_w, 5.0), egui::Sense::hover());
                                        ui.painter().line_segment([rule.left_center(), rule.right_center()], Stroke::new(1.0, theme::HAIRLINE));
                                        ui.add_space(4.0);
                                        ui.horizontal_top(|ui| {
                                            for st in &app_streams {
                                                ui.allocate_ui_with_layout(egui::vec2(STRIP_W, fader_h + 150.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                                                    // "(2) Neo Soul … - YouTube" → "Neo Soul …": the tab counter is noise.
                                                    let title = st.title.trim_start_matches(|c: char| c == '(' || c.is_ascii_digit() || c == ')' || c == ' ');
                                                    let shown = st.volume.min(1.0);
                                                    let mut v = Some(shown);
                                                    // What the app plays, dB-scaled (rule 62).
                                                    let level = meter_db_pos(crate::audio_out::stream_level(&self.stream_levels, st.index));
                                                    let fr = mixer_strip(ui, &st.app, title, &mut v, level, fader_h);
                                                    ui.interact(fr, egui::Id::new(("app_stream_tip", st.index)), egui::Sense::hover())
                                                        .on_hover_text(format!("{} — {}", st.app, st.title));
                                                    if let Some(nv) = v {
                                                        if (nv - shown).abs() > 1e-4 { stream_changes.push((st.index, Some(nv), None)); }
                                                    }
                                                    ui.add_space(4.0);
                                                    let mb = egui::Button::new(egui::RichText::new("M").size(11.0)
                                                            .color(if st.muted { theme::DARK_PANEL } else { theme::CREAM_MID }))
                                                        .fill(if st.muted { theme::CHIP_ON } else { theme::DARK_PANEL_HI })
                                                        .corner_radius(2.0).min_size(egui::vec2(26.0, 22.0));
                                                    if ui.add(mb).on_hover_text("mute this app").clicked() {
                                                        stream_changes.push((st.index, None, Some(!st.muted)));
                                                    }
                                                });
                                            }
                                        });
                                    });
                                }
                                // Master out sits right after the last rig bus
                                // (the sum they feed) — not pinned far away.
                                let (r, _) = ui.allocate_exact_size(egui::vec2(25.0, h - 8.0), egui::Sense::hover());
                                ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, theme::HAIRLINE_DARK));
                                ui.vertical(|ui| {
                                    ui.set_width(170.0);
                                    theme::section_label(ui, "Master out");
                                    // Output device (#31): which sound card kabelsalat plays to.
                                    let label_of = |raw: &str| outputs.iter().find(|o| o.raw == raw)
                                        .map(|o| o.name.clone()).unwrap_or_else(|| raw.to_string());
                                    let selected = match &out_wanted {
                                        Some(w) => label_of(w),
                                        None => "System default".to_string(),
                                    };
                                    egui::ComboBox::from_id_salt("audio_output")
                                        .width(160.0)
                                        .selected_text(egui::RichText::new(selected).size(11.5).color(theme::INK))
                                        .show_ui(ui, |ui| {
                                            if ui.selectable_label(out_wanted.is_none(),
                                                egui::RichText::new("System default").color(theme::INK)).clicked() {
                                                out_pick = Some(None);
                                            }
                                            for o in &outputs {
                                                if ui.selectable_label(out_wanted.as_deref() == Some(o.raw.as_str()),
                                                    egui::RichText::new(&o.name).color(theme::INK)).clicked() {
                                                    out_pick = Some(Some(o.raw.clone()));
                                                }
                                            }
                                        })
                                        .response.on_hover_text("the sound card kabelsalat plays to");
                                    if let Some(c) = &out_current {
                                        ui.label(egui::RichText::new(format!("→ {}", label_of(c))).size(10.5).color(theme::INK_2));
                                    }
                                    ui.add_space(4.0);
                                    let rigs = if n == 1 { "1 rig · peak meter".to_string() } else { format!("sum of {n} rigs · peak meter") };
                                    ui.label(egui::RichText::new(rigs).size(11.0).color(theme::INK_2));
                                    ui.add_space(8.0);
                                    // Meter + dB scale: a labeled peak meter, not a missing fader.
                                    let (area, _) = ui.allocate_exact_size(egui::vec2(52.0, fader_h), egui::Sense::hover());
                                    let mr = Rect::from_min_size(area.min, egui::vec2(14.0, fader_h));
                                    paint_meter(ui.painter(), mr, meter_db_pos(self.meter), meter_db_pos(self.meter_peak));
                                    for db in [0.0f32, -6.0, -12.0, -24.0, -48.0] {
                                        // Labels only (a tick hairline beside "0" read as "-0"),
                                        // with a real minus sign.
                                        let y = mr.bottom() - mr.height() * meter_db_pos(10f32.powf(db / 20.0));
                                        let label = if db == 0.0 { "0 dB".to_string() } else { format!("\u{2212}{:.0}", -db) };
                                        ui.painter().text(Pos2::new(mr.right() + 6.0, y), egui::Align2::LEFT_CENTER,
                                            label, FontId::monospace(9.5), theme::CREAM_DIM);
                                    }
                                    ui.add_space(6.0);
                                    // Meter-only strip: say so with a live peak readout.
                                    let db = if self.meter_peak < 0.001 { "−∞ dB".to_string() } else { format!("{:.0} dB", 20.0 * self.meter_peak.log10()) };
                                    ui.label(egui::RichText::new(db).size(11.0).monospace().color(theme::CREAM_MID))
                                        .on_hover_text("output peak (meter only — set levels on the rig buses and sources)");
                                });
                            });
                        });
                    });
                });
            });
        });
        if let Some(r) = switch { self.set_active_rig(r); }
        for (index, vol, mute) in stream_changes { self.set_app_stream(index, vol, mute); }
        if let Some(o) = out_pick { self.set_audio_output(o); }
        if self.show_help { self.draw_help(&painter, rect); }
    }

    /// Tab: in any non-rack stage view it returns to the rack; otherwise it flips the rack.
    fn on_tab(&mut self) {
        if self.stage_view != StageView::Rack {
            self.confirm_delete_session = None;
            self.stage_view = StageView::Rack;
        } else {
            self.start_flip();
        }
    }

    fn draw_rails(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for side_x in [8.0, self.rack_w - 22.0] {
            let rail = Rect::from_min_size(Pos2::new(side_x, 8.0), Vec2::new(14.0, self.rack_h - 16.0));
            painter.rect_filled(xf.rect(rail), 4.0, RAIL);
        }
        // No screws — the editorial language has no hardware chrome (§4.2).
    }

    /// Text through the flip transform (fades while squashed).
    fn text(&self, painter: &egui::Painter, local: Vec2, g: &Option<Arc<Galley>>, color: Color32) {
        if self.text_alpha <= 0.01 { return; }
        let Some(g) = g else { return };
        let mut tp = painter.clone();
        tp.set_opacity(self.text_alpha);
        tp.galley(self.xf.p(local), g.clone(), color);
    }

    /// Tracked micro-caps through the flip transform: each cached per-char
    /// galley is mapped individually, so letter spacing squashes with the rack.
    fn text_tracked(&self, painter: &egui::Painter, local: Vec2, g: &Option<Arc<theme::TrackedLabel>>, color: Color32) {
        if self.text_alpha <= 0.01 { return; }
        let Some(g) = g else { return };
        let mut tp = painter.clone();
        tp.set_opacity(self.text_alpha);
        for (cg, dx) in &g.chars {
            tp.galley(self.xf.p(local + Vec2::new(*dx, 0.0)), cg.clone(), color);
        }
    }

    fn ell(&self, painter: &egui::Painter, center: Vec2, r: f32, fill: Color32, stroke: Stroke) {
        painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
            center: self.xf.p(center),
            radius: Vec2::new(r * self.xf.sx, r * self.xf.sy),
            fill, stroke, angle: 0.0,
        }));
    }

    /// Crisp "AGENT" pill badge inside a rack unit header (rack-local rect).
    fn draw_agent_badge(&self, painter: &egui::Painter, rect: Rect, alpha_t: f32) {
        if alpha_t <= 0.01 { return; }
        let sr = self.xf.rect(rect);
        painter.rect_filled(sr, 2.0, alpha(theme::AGENT_COLOR, alpha_t));
        self.face_text(painter, rect.center(), "AGENT", theme::medium_font(9.5), theme::alpha(theme::AGENT_COLOR_TEXT, alpha_t));
    }

    fn draw_front(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            let r = self.unit_rects[ui];
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            let agent_t = self.agent_activity.node_alpha(u.node_id, self.ui_time);
            // Single near-black family (§2.4): plates are the chassis color;
            // zones are drawn with 1px hairline rules that brighten on hover
            // (~120ms ease). Drag = 2px accent inset keyline; Agent = teal keyline.
            painter.rect_filled(sr, 2.0, CHASSIS);
            let ht = self.unit_hover.get(ui).copied().unwrap_or(0.0);
            if dragging {
                theme::inset_keyline(painter, sr, 2.0, theme::ACCENT, 2.0, 2.5);
            } else if agent_t > 0.01 {
                painter.rect_stroke(sr, 2.0, Stroke::new(1.8, alpha(theme::AGENT_COLOR, agent_t)), egui::StrokeKind::Inside);
            } else {
                // Faint default rule; hover eases to the accent (clear
                // interactive state without shadows — DeepSeek review fix).
                let rule = mix(BORDER, theme::ACCENT, ht);
                painter.rect_stroke(sr, 2.0, Stroke::new(1.0, alpha(rule, 0.3 + 0.7 * ht)), egui::StrokeKind::Inside);
            }

            // Label strip: 2px rule, accent while dragged (§4.3), teal when
            // touched by an agent. Activity is the LED's job alone — the
            // strip no longer flashes with it (one signal, not two).
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(2.0, r.height() - 24.0));
            let strip_col = if dragging {
                theme::ACCENT
            } else if agent_t > 0.01 {
                mix(BORDER, theme::AGENT_COLOR, agent_t)
            } else {
                BORDER
            };
            painter.rect_filled(xf.rect(strip), 1.0, strip_col);

            // Labels are fitted to the unit (ensure_galleys) AND clipped to
            // it: no label pixel may leave its box into a neighbor (C4).
            let label_painter = painter.with_clip_rect(sr.intersect(painter.clip_rect()));
            self.text(&label_painter, r.min.to_vec2() + Vec2::new(34.0, 17.0), &u.name_g, TEXT);
            self.text_tracked(&label_painter, r.min.to_vec2() + Vec2::new(34.0, 42.0), &u.sub_g, SUBTEXT);

            // Activity LED: flat teal disc (+ optional flat halo), accent
            // while dragged. No glow stack.
            let led = (r.right_top() + Vec2::new(-20.0, 20.0)).to_vec2();
            let led_col = if dragging { theme::ACCENT } else { GREEN };
            if u.led > 0.03 { self.ell(painter, led, 7.5, alpha(led_col, 0.15 * u.led), Stroke::NONE); }
            self.ell(painter, led, 3.2, alpha(led_col, 0.25 + 0.75 * u.led), Stroke::NONE);

            match &u.node_kind {
                UnitKind::Keys => {
                    self.draw_notes_display(painter, ui);
                    self.draw_split_face(painter, ui);
                }
                UnitKind::Clap => self.draw_surge_display(painter, ui),
                UnitKind::Dsp(DspKind::Delay) => self.draw_delay_display(painter, ui),
                UnitKind::Dsp(_) if u.transport => self.draw_transport_face(painter, ui),
                UnitKind::Dsp(_) => self.draw_dsp_display(painter, ui),
                UnitKind::Master => self.draw_meter(painter, ui),
                UnitKind::AudioIn => self.draw_audio_in_face(painter, ui),
            }

            for ki in 0..u.knobs.len() {
                if u.knobs[ki].hidden { continue; }
                self.draw_knob(painter, ui, ki);
            }

            if agent_t > 0.01 {
                let badge = if self.split_rects(ui).is_some() {
                    Rect::from_min_size(Pos2::new(r.right() - 60.0, r.top() + 40.0), Vec2::new(46.0, 16.0))
                } else {
                    Rect::from_min_size(Pos2::new(r.right() - 84.0, r.top() + 12.0), Vec2::new(46.0, 16.0))
                };
                self.draw_agent_badge(painter, badge, agent_t);
            }
        }
    }

    /// Is the pointer over rack-local `rect` of `unit` (front face hover)?
    fn face_hovered(&self, painter: &egui::Painter, unit: usize, rect: Rect) -> bool {
        matches!(self.hover, Some(Hit::Unit(h)) if h == unit)
            && painter.ctx().pointer_hover_pos().map_or(false, |p| rect.contains(self.xf.inv(p).to_pos2()))
    }

    /// A face chip in the theme::chip_button language with EXPLICIT colors
    /// per state (rule 1): on = cream fill + near-black label; off = raised
    /// dark chip + hairline + cream-mid label; hover brightens.
    fn paint_face_chip(&self, painter: &egui::Painter, rect: Rect, label: &str, on: bool, hovered: bool) {
        let (fill, stroke, txt) = match (on, hovered) {
            (true, false) => (theme::CREAM, theme::CREAM, theme::DARK_PANEL),
            (true, true) => (mix(theme::CREAM, theme::DARK_PANEL, 0.10), mix(theme::CREAM, theme::DARK_PANEL, 0.10), theme::DARK_PANEL),
            (false, false) => (theme::DARK_PANEL_HI, theme::HAIRLINE, theme::CREAM_MID),
            (false, true) => (mix(theme::DARK_PANEL_HI, theme::CREAM, 0.07), theme::MUTED, theme::CREAM),
        };
        let sc = self.xf.rect(rect);
        painter.rect_filled(sc, 2.0, fill);
        painter.rect_stroke(sc, 2.0, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
        self.face_text(painter, rect.center(), label, theme::medium_font(12.0), txt);
    }

    /// A recessed drag readout (split point / BPM): dark field + hairline
    /// that brightens on hover/drag; value text is explicit cream (rule 1).
    fn paint_face_field(&self, painter: &egui::Painter, rect: Rect, active: bool) {
        let sc = self.xf.rect(rect);
        painter.rect_filled(sc, 2.0, CHASSIS);
        let stroke = if active { theme::CREAM_DIM } else { theme::HAIRLINE };
        painter.rect_stroke(sc, 2.0, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    }

    /// Centered single-line face text at rack-local `center`, faded with the
    /// flip like every other rack label. Layout happens on the painter (not
    /// inside fonts_mut — egui deadlock rule).
    fn face_text(&self, painter: &egui::Painter, center: Pos2, text: &str, font: FontId, color: Color32) {
        if self.text_alpha <= 0.01 { return; }
        let g = painter.layout_no_wrap(text.into(), font, color);
        let pos = self.xf.p(center.to_vec2() - g.size() / 2.0);
        let mut tp = painter.clone();
        tp.set_opacity(self.text_alpha);
        tp.galley(pos, g, color);
    }

    /// Keys face: SPLIT chip (split at C4 / unsplit) + the split point as a
    /// drag readout (note name) while split.
    fn draw_split_face(&self, painter: &egui::Painter, unit: usize) {
        let Some((chip, readout)) = self.split_rects(unit) else { return };
        let point = self.shown_split(unit);
        let hov_chip = self.face_hovered(painter, unit, chip);
        // Unsplit: ONE chip "SPLIT · OFF" across both slots (click = split).
        // Split: the lit SPLIT chip + the draggable note field, in a shared frame.
        if point.is_none() {
            let whole = chip.union(readout);
            let hov = hov_chip || self.face_hovered(painter, unit, readout);
            self.paint_face_chip(painter, whole, "SPLIT · OFF", false, hov);
            return;
        }
        self.paint_face_chip(painter, chip, "SPLIT", true, hov_chip);
        let frame = chip.union(readout).expand(2.0);
        painter.rect_stroke(self.xf.rect(frame), 3.0, Stroke::new(1.0, theme::HAIRLINE_DARK), egui::StrokeKind::Inside);
        match point {
            Some(p) => {
                let dragging = matches!(self.drag, Drag::Split { unit: du, .. } if du == unit);
                let active = dragging || self.face_hovered(painter, unit, readout);
                self.paint_face_field(painter, readout, active);
                self.face_text(painter, readout.center(), &note_name(p), theme::medium_font(12.0), TEXT);
            }
            None => {
                self.paint_face_field(painter, readout, false);
                self.face_text(painter, readout.center(), "OFF", FontId::proportional(11.0), SUBTEXT);
            }
        }
    }

    /// Transport face (metronome; file player later): a Play/Stop chip in the
    /// theme::chip_button style — playing = cream fill + near-black label,
    /// stopped = raised dark chip + hairline — with explicit label colors
    /// (rule 1), plus a teal state LED that thumps on the beat. Reflects the
    /// unit's engine-mirrored `playing` flag.
    fn draw_transport_face(&self, painter: &egui::Painter, unit: usize) {
        let Some(chip) = self.transport_chip_rect(unit) else { return };
        let u = &self.units[unit];
        let xf = &self.xf;
        let hovered = matches!(self.hover, Some(Hit::Unit(h)) if h == unit)
            && painter.ctx().pointer_hover_pos().map_or(false, |p| chip.contains(xf.inv(p).to_pos2()));
        let (fill, stroke, txt) = match (u.playing, hovered) {
            (true, false) => (theme::CREAM, theme::CREAM, theme::DARK_PANEL),
            (true, true) => (mix(theme::CREAM, theme::DARK_PANEL, 0.10), mix(theme::CREAM, theme::DARK_PANEL, 0.10), theme::DARK_PANEL),
            (false, false) => (theme::DARK_PANEL_HI, theme::HAIRLINE, theme::CREAM_MID),
            (false, true) => (mix(theme::DARK_PANEL_HI, theme::CREAM, 0.07), theme::MUTED, theme::CREAM),
        };
        let sc = xf.rect(chip);
        painter.rect_filled(sc, 2.0, fill);
        painter.rect_stroke(sc, 2.0, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
        // Glyph (10px pad from the chip edge): ▶ when stopped, ■ when playing.
        let gc = Vec2::new(chip.left() + 10.0 + 4.0, chip.center().y);
        if u.playing {
            let sq = Rect::from_center_size(gc.to_pos2(), Vec2::splat(8.0));
            painter.rect_filled(xf.rect(sq), 1.0, txt);
        } else {
            let pts = [gc + Vec2::new(-3.5, -4.5), gc + Vec2::new(-3.5, 4.5), gc + Vec2::new(4.5, 0.0)];
            painter.add(Shape::convex_polygon(pts.iter().map(|p| xf.p(*p)).collect(), txt, Stroke::NONE));
        }
        if self.text_alpha > 0.01 {
            let g = painter.layout_no_wrap(
                if u.playing { "STOP" } else { "PLAY" }.into(), theme::medium_font(12.0), txt);
            let pos = xf.p(Vec2::new(chip.left() + 24.0, chip.center().y - g.size().y / 2.0));
            let mut tp = painter.clone();
            tp.set_opacity(self.text_alpha);
            tp.galley(pos, g, txt);
        }
        // State LED: tomato while counting in, teal while playing normally,
        // dim track when stopped.
        let led = Vec2::new(chip.right() + 16.0, chip.center().y);
        if u.playing {
            let beat = 60.0 / self.bpm.max(1.0) as f64;
            let phase = ((self.time / beat).fract() as f32).clamp(0.0, 1.0);
            let thump = if self.playing { (1.0 - phase).powi(3) } else { 0.0 };
            let led_col = if self.counting_in { theme::ACCENT } else { GREEN };
            self.ell(painter, led, 7.0 + 2.0 * thump, alpha(led_col, 0.15 + 0.15 * thump), Stroke::NONE);
            self.ell(painter, led, 4.0, led_col, Stroke::NONE);
        } else {
            self.ell(painter, led, 4.0, theme::METER_TRACK, Stroke::NONE);
        }
        self.draw_bpm_control(painter, unit);
        self.draw_metro_actions(painter, unit);
    }

    /// Metronome-face TAP + COUNT-IN chips below the Play/BPM row.
    fn draw_metro_actions(&self, painter: &egui::Painter, unit: usize) {
        let Some(m) = self.metro_action_rects(unit) else { return };
        let tap_hov = self.face_hovered(painter, unit, m.tap);
        self.paint_face_chip(painter, m.tap, "TAP", self.tap_flash_t > 0.15, tap_hov);

        let ci_hov = self.face_hovered(painter, unit, m.count_in);
        let ci_on = self.count_in_enabled || self.counting_in;
        let total_beats = self.count_in_bars.clamp(1, crate::transport::MAX_COUNT_IN_BARS)
            * crate::transport::BEATS_PER_BAR as u8;
        let ci_label = if self.counting_in {
            if m.compact {
                format!("IN {}", self.count_in_beat.max(1))
            } else {
                format!("IN {}/{}", self.count_in_beat.max(1), total_beats)
            }
        } else if m.compact {
            if self.count_in_bars > 1 { format!("{} BAR", self.count_in_bars) } else { "1 BAR".into() }
        } else if self.count_in_bars > 1 {
            format!("COUNT · {}", self.count_in_bars)
        } else {
            "COUNT-IN".into()
        };
        self.paint_face_chip(painter, m.count_in, &ci_label, ci_on, ci_hov);
    }

    /// Metronome-face tempo: −/+ steppers around a drag readout ("96 BPM").
    /// Shows the ONE global tempo: every metronome face reads and sets the
    /// same engine SetBpm value (per-face Play/Stop stays per instrument).
    fn draw_bpm_control(&self, painter: &egui::Painter, unit: usize) {
        let Some(b) = self.bpm_rects(unit) else { return };
        for (rect, plus) in [(b.minus, false), (b.plus, true)] {
            let Some(rect) = rect else { continue };
            let hov = self.face_hovered(painter, unit, rect);
            self.paint_face_chip(painter, rect, "", false, hov);
            // Painted glyphs (no font dependency for − / +), explicit color.
            let col = if hov { theme::CREAM } else { theme::CREAM_MID };
            let c = rect.center();
            let st = Stroke::new(1.5, col);
            painter.line_segment([self.xf.p(c.to_vec2() + Vec2::new(-4.5, 0.0)), self.xf.p(c.to_vec2() + Vec2::new(4.5, 0.0))], st);
            if plus {
                painter.line_segment([self.xf.p(c.to_vec2() + Vec2::new(0.0, -4.5)), self.xf.p(c.to_vec2() + Vec2::new(0.0, 4.5))], st);
            }
        }
        let dragging = matches!(self.drag, Drag::Bpm { .. }) && matches!(self.hover, Some(Hit::Unit(h)) if h == unit);
        let active = dragging || self.face_hovered(painter, unit, b.value);
        self.paint_face_field(painter, b.value, active);
        // Number (Medium 13, cream) + tracked-feel "BPM" caption (cream-mid),
        // centered together with a 4px gap — ≥ 8px side padding at "300".
        if self.text_alpha > 0.01 {
            let num = painter.layout_no_wrap(format!("{:.0}", self.bpm), theme::medium_font(13.0), TEXT);
            let cap = painter.layout_no_wrap("BPM".into(), FontId::proportional(9.0), SUBTEXT);
            let w = num.size().x + 4.0 + cap.size().x;
            let c = b.value.center();
            let x0 = c.x - w / 2.0;
            let mut tp = painter.clone();
            tp.set_opacity(self.text_alpha);
            tp.galley(self.xf.p(Vec2::new(x0, c.y - num.size().y / 2.0)), num.clone(), TEXT);
            tp.galley(self.xf.p(Vec2::new(x0 + num.size().x + 4.0, c.y - cap.size().y / 2.0 + 0.5)), cap, SUBTEXT);
        }
    }

    /// Generic DSP face: a small activity bar that breathes with the unit's LED.
    fn draw_dsp_display(&self, painter: &egui::Painter, unit: usize) {
        let u = &self.units[unit];
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let level = u.led.clamp(0.0, 1.0);
        let bar_w = width * 0.8;
        let y = self.knob_row_y(unit);
        // Generic DSP bar (§4.9): METER_TRACK track, flat accent fill.
        let track = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w, 8.0));
        painter.rect_filled(self.xf.rect(track), 4.0, theme::METER_TRACK);
        let fill = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w * (0.15 + 0.85 * level), 8.0));
        painter.rect_filled(self.xf.rect(fill), 4.0, alpha(theme::ACCENT, 0.5 + 0.5 * level));
    }

    /// Keys face: the 12 pitch classes as a one-octave dot strip (sharps
    /// raised, like a keyboard silhouette) lit by the played notes. Playing
    /// happens on the dock keyboard; the face only shows activity.
    fn draw_notes_display(&self, painter: &egui::Painter, unit: usize) {
        let (start, width) = self.display_area(unit);
        if width < 60.0 { return; }
        let y = self.knob_row_y(unit) + 4.0;
        let pitch = (width / 12.0).min(14.0);
        for pc in 0..12 {
            let sharp = matches!(pc, 1 | 3 | 6 | 8 | 10);
            let c = Vec2::new(start + pitch * (pc as f32 + 0.5), if sharp { y - 9.0 } else { y });
            let lit = self.key_leds[pc];
            self.ell(painter, c, 3.4, theme::METER_TRACK, Stroke::NONE);
            if lit > 0.02 {
                self.ell(painter, c, 3.4 + 1.6 * lit, alpha(theme::ACCENT, 0.35 + 0.65 * lit), Stroke::NONE);
            }
        }
        self.display_caption(painter, unit, start + pitch * 6.0, &self.caption_notes_g);
    }

    fn draw_surge_display(&self, painter: &egui::Painter, unit: usize) {
        let vis = 0.25 + 0.75 * self.units[unit].led.max(
            self.cables.iter().filter(|c| c.src == unit && c.kind == PortKind::Audio).map(|c| c.level).fold(0.0, f32::max));
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let pitch = (width / 11.0).min(9.0);
        for i in 0..11 {
            let h = (0.25 + 0.75 * (self.ui_time * 2.6 + i as f32 * 0.55).sin().abs()) * 34.0 * vis;
            let bar = Rect::from_center_size(
                Pos2::new(start + pitch * 0.5 + i as f32 * pitch, self.knob_row_y(unit)),
                Vec2::new(pitch * 0.45, h.max(2.0)),
            );
            // Alternate cream / dim cream, flat (§4.9).
            let col = if i % 2 == 0 { TEXT } else { SUBTEXT };
            painter.rect_filled(self.xf.rect(bar), 2.0, alpha(col, 0.5 + 0.5 * vis));
        }
        // Caption on the knob-label baseline: the bars are the synth's output.
        self.display_caption(painter, unit, start + pitch * 5.5, &self.caption_output_g);
    }

    /// A tracked micro-caps caption under a face display, centered at
    /// rack-local `cx`, on the same baseline as the unit's knob labels.
    fn display_caption(&self, painter: &egui::Painter, unit: usize, cx: f32, g: &Option<Arc<theme::TrackedLabel>>) {
        let w = g.as_ref().map_or(0.0, |g| g.size.x);
        let y = self.knob_row_y(unit) + KNOB_R + KNOB_LABEL_GAP;
        self.text_tracked(painter, Vec2::new(cx - w / 2.0, y), g, SUBTEXT);
    }

    fn draw_delay_display(&self, painter: &egui::Painter, unit: usize) {
        let led = self.units[unit].led;
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let pitch = (width / 4.5).min(26.0);
        for i in 0..4 {
            let c = Vec2::new(start + pitch * 0.5 + i as f32 * pitch, self.knob_row_y(unit));
            let a = 0.15 + 0.85 * led * (1.0 - i as f32 * 0.2);
            self.ell(painter, c, 6.0 - i as f32 * 1.1, alpha(theme::ACCENT, a), Stroke::NONE);
        }
        if led > 0.05 {
            let c = Vec2::new(start + pitch * 0.5, self.knob_row_y(unit));
            self.ell(painter, c, 10.0 + (1.0 - led) * 14.0, alpha(theme::ACCENT, 0.25 * led), Stroke::NONE);
        }
        self.display_caption(painter, unit, start + pitch * 2.0, &self.caption_echoes_g);
    }

    /// Master meter column: left x (on the face grid), bottom y (level with
    /// the knob rings' bottom), segment pitch (fits between the header and
    /// the knob row, room left for the OUTPUT caption).
    fn meter_geom(&self, unit: usize) -> (f32, f32, f32) {
        let r = self.unit_rects[unit];
        let x = r.left() + FACE_X;
        let bottom = self.knob_row_y(unit) + KNOB_R;
        let pitch = ((bottom - (r.top() + 84.0)) / METER_SEGS as f32).clamp(4.0, 17.0);
        (x, bottom, pitch)
    }

    /// Vertical LED meter on the tall MASTER unit (bottom = quiet).
    /// Stepped cream→tomato→red (§4.8) with a slow-release peak-hold tick.
    fn draw_meter(&self, painter: &egui::Painter, unit: usize) {
        let (x, bottom, pitch) = self.meter_geom(unit);
        let n = METER_SEGS;
        let lit = lit_segments(meter_db_pos(self.meter), n);
        for i in 0..n {
            let seg = Rect::from_min_size(
                Pos2::new(x, bottom - (i + 1) as f32 * pitch),
                Vec2::new(22.0, pitch * 0.72),
            );
            let col = theme::meter_color(i as f32 / (n - 1) as f32);
            let fill = if i < lit { col } else { theme::METER_TRACK };
            painter.rect_filled(self.xf.rect(seg), 1.0, fill);
        }
        // Peak-hold tick: 2px, accent — clip red once the peak hits ≥ 0.9.
        if meter_db_pos(self.meter_peak) > 0.02 {
            let i = ((meter_db_pos(self.meter_peak) * n as f32).ceil() as usize).clamp(1, n) - 1;
            let tick = Rect::from_min_size(
                Pos2::new(x, bottom - (i + 1) as f32 * pitch - 2.0),
                Vec2::new(22.0, 2.0),
            );
            let col = if self.meter_peak >= 0.9 { theme::CLIP_RED } else { theme::ACCENT };
            painter.rect_filled(self.xf.rect(tick), 0.8, col);
        }
        // Caption ON the meter column (above its top segment): attached to
        // the meter, not floating on the knob-label row (vision review).
        let top = bottom - n as f32 * pitch;
        // Left-aligned with the meter column and the title (face grid).
        self.text_tracked(painter, Vec2::new(x, top - 18.0), &self.caption_output_g, SUBTEXT);
    }

    // -- Audio In face (issue #28) -------------------------------------------

    /// The AudioIn node's input level (pre-gain peak), engine → UI, lock-free.
    fn audio_in_level(&self, node: u32) -> f32 {
        self.audio_in_levels.as_ref().map(|l| l.load(node)).unwrap_or(0.0)
    }

    /// Monitor chip rect (rack-local; None when the unit is too short/narrow).
    fn audio_in_monitor_rect(&self, unit: usize) -> Option<Rect> {
        if !matches!(self.units.get(unit)?.node_kind, UnitKind::AudioIn) { return None; }
        let r = *self.unit_rects.get(unit)?;
        if !(r.width() > 0.0) { return None; }
        let y = self.audio_in_chips_y(unit)?;
        let (start, width) = self.display_area(unit);
        if width < 148.0 { return None; } // three chips need ~148px
        Some(Rect::from_min_size(Pos2::new(start, y), Vec2::new(42.0, FACE_CTL_H)))
    }

    /// Device chip rect (right of MON).
    fn audio_in_device_rect(&self, unit: usize) -> Option<Rect> {
        let m = self.audio_in_monitor_rect(unit)?;
        Some(Rect::from_min_size(Pos2::new(m.right() + 6.0, m.top()), Vec2::new(64.0, FACE_CTL_H)))
    }

    /// Channel chip rect (right of DEV).
    fn audio_in_channel_rect(&self, unit: usize) -> Option<Rect> {
        let d = self.audio_in_device_rect(unit)?;
        Some(Rect::from_min_size(Pos2::new(d.right() + 6.0, d.top()), Vec2::new(36.0, FACE_CTL_H)))
    }

    /// The chip-row y when the face is tall enough that the chips clear the
    /// knob row (below the header, above the knobs); None when they'd collide
    /// (short units show device/channel in the subtitle instead).
    fn audio_in_chips_y(&self, unit: usize) -> Option<f32> {
        let r = *self.unit_rects.get(unit)?;
        let y = (r.top() + 60.0).max(r.center().y - 8.0).min(r.bottom() - FACE_PAD_B - FACE_CTL_H - 8.0);
        if y + FACE_CTL_H + 8.0 > self.knob_row_y(unit) {
            return None;
        }
        Some(y)
    }

    fn draw_audio_in_face(&self, painter: &egui::Painter, unit: usize) {
        let node = self.units[unit].node_id;
        let (start, width) = self.display_area(unit);

        // Input meter (pre-gain): a horizontal VU in the display area.
        if width >= 40.0 {
            let level = self.audio_in_level(node).clamp(0.0, 1.0);
            let y = self.knob_row_y(unit);
            let bar_w = width * 0.8;
            let track = Rect::from_min_size(Pos2::new(start, y - 12.0), Vec2::new(bar_w, 8.0));
            painter.rect_filled(self.xf.rect(track), 4.0, theme::METER_TRACK);
            let fill = Rect::from_min_size(Pos2::new(start, y - 12.0), Vec2::new(bar_w * (0.1 + 0.9 * level), 8.0));
            painter.rect_filled(self.xf.rect(fill), 4.0, alpha(theme::ACCENT, 0.5 + 0.5 * level));
            if level >= 0.92 {
                painter.rect_filled(self.xf.rect(Rect::from_min_size(Pos2::new(start, y - 12.0), Vec2::new(5.0, 8.0))), 1.0, theme::CLIP_RED);
            }
            // Tuner readout above the meter: note + cents (no signal → hint).
            let pitch = self.audio_in_tuners.get(&node).and_then(|(_, _, p)| *p);
            let text = match pitch {
                Some(p) => format!("{} {:+.0}¢", crate::audio_in::note_name(p.note), p.cents),
                None => "TUNE".into(),
            };
            self.face_text(painter, Pos2::new(start + width * 0.5, y - 22.0), &text, theme::medium_font(12.0), SUBTEXT);
        }

        // MON / DEV / CH chips (only when the face is tall AND wide enough;
        // otherwise the subtitle's "device · channel" carries the info).
        let mon = self.audio_in_monitor_rect(unit);
        let dev = self.audio_in_device_rect(unit);
        let ch = self.audio_in_channel_rect(unit);
        if let Some(m) = mon {
            let on = self.units[unit].knobs.iter().find(|k| k.param == Some(P_AUDIO_IN_MONITOR))
                .map_or(false, |k| k.value >= 0.5);
            let hov = self.face_hovered(painter, unit, m);
            self.paint_face_chip(painter, m, "MON", on, hov);
        }
        if let Some(d) = dev {
            let label = match self.units[unit].device.as_deref() {
                Some(raw) => crate::audio_in::pretty_device_name(raw),
                None => "DEFAULT".to_string(),
            };
            let hov = self.face_hovered(painter, unit, d);
            self.paint_face_chip(painter, d, &label, false, hov);
        }
        if let Some(c) = ch {
            let label = self.units[unit].channels.label();
            let hov = self.face_hovered(painter, unit, c);
            self.paint_face_chip(painter, c, label, false, hov);
        }
    }

    /// Drain each open AudioIn capture's tuner ring and refresh the pitch.
    fn pump_audio_in_tuners(&mut self) {
        // Collect fresh samples per node first (avoid a double borrow).
        let nodes: Vec<u32> = self.audio_in_tuners.keys().copied().collect();
        for node in nodes {
            let Some((rx, tuner, pitch)) = self.audio_in_tuners.get_mut(&node) else { continue };
            let mut chunk = [0.0f32; 512];
            let mut any = false;
            loop {
                let mut n = 0usize;
                while n < chunk.len() {
                    match rx.pop() {
                        Ok(v) => { chunk[n] = v; n += 1; }
                        Err(_) => break,
                    }
                }
                if n == 0 { break; }
                any = true;
                tuner.process(&chunk[..n]);
            }
            if any {
                *pitch = tuner.process(&[]);
            }
        }
    }

    /// Spawn the input-device discovery thread (rescan every 2 s, rule 48).
    fn start_input_device_scan(&self) {
        let shared = self.input_devices.clone();
        std::thread::Builder::new()
            .name("kabelsalat-input-scan".into())
            .spawn(move || loop {
                let devs = crate::audio_in::list_input_devices();
                if let Ok(mut slot) = shared.lock() {
                    *slot = devs;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            })
            .ok();
    }

    /// Ensure one open capture per AudioIn node (device match). Called on
    /// structural changes only (build_rack / apply_session), never per frame.
    fn sync_audio_in_captures(&mut self) {
        let sample_rate = self.sample_rate.max(1.0) as u32;
        // Nodes that need a capture (device name from the graph).
        let mut wanted: Vec<(u32, Option<String>)> = Vec::new();
        for n in &self.base_graph.nodes {
            if let NodeKind::AudioIn { device, .. } = &n.kind {
                wanted.push((n.id, device.clone()));
            }
        }
        // Drop streams/tuners for nodes no longer present.
        let present: std::collections::HashSet<u32> = wanted.iter().map(|(id, _)| *id).collect();
        self.audio_in_streams.retain(|id, _| present.contains(id));
        self.audio_in_tuners.retain(|id, _| present.contains(id));
        self.audio_in_opened.retain(|id, _| present.contains(id));
        self.audio_in_live.retain(|id, _| present.contains(id));
        let Some(tx) = self.audio_in_tx.as_mut() else { return };
        for (node, device) in wanted {
            if self.audio_in_opened.get(&node) == Some(&device) {
                // Already open for this device: re-open only if the stream died
                // (device unplugged → the capture reader hit EOF and cleared the flag).
                let dead = self.audio_in_live.get(&node).map_or(false, |l| !l.load(Ordering::Relaxed));
                if !dead {
                    continue;
                }
                self.audio_in_streams.remove(&node);
                self.audio_in_tuners.remove(&node);
                self.audio_in_opened.remove(&node);
                self.audio_in_live.remove(&node);
            }
            // Open on the UI thread (fast cpal open, like the output stream).
            let Some(cap) = crate::audio_in::open_capture(device.as_deref(), sample_rate, crate::bridge::BUFFER_FRAMES) else {
                continue; // no capture available — the meter/tuner stay silent
            };
            // Deliver the engine read end through the side channel.
            if tx.push((node, cap.engine_rx)).is_ok() {
                self.audio_in_streams.insert(node, cap.stream);
                self.audio_in_opened.insert(node, device.clone());
                self.audio_in_live.insert(node, cap.live.clone());
                self.audio_in_tuners.insert(node, (cap.tuner_rx, crate::audio_in::Tuner::new(cap.sample_rate), None));
            }
        }
    }

    /// Re-open (or close) the capture for one AudioIn node after a device
    /// change: drop the old stream/tuner and open the new device.
    fn resync_audio_in_capture(&mut self, node: u32) {
        self.audio_in_streams.remove(&node);
        self.audio_in_tuners.remove(&node);
        self.audio_in_opened.remove(&node);
        self.audio_in_live.remove(&node);
        self.sync_audio_in_captures();
    }

    fn set_audio_in_monitor(&mut self, unit: usize, on: bool) {
        let Some(u) = self.units.get(unit) else { return };
        let node = u.node_id;
        let v = if on { 1.0f32 } else { 0.0f32 };
        if let Some(k) = self.units[unit].knobs.iter_mut().find(|k| k.param == Some(P_AUDIO_IN_MONITOR)) {
            k.value = v;
        }
        if let Some(bus) = &self.bus {
            if bus.lock().unwrap().send(Command::SetParam { node, param: P_AUDIO_IN_MONITOR, value: v as f64 }) {
                if let Some(k) = self.units[unit].knobs.iter_mut().find(|k| k.param == Some(P_AUDIO_IN_MONITOR)) {
                    k.sent = Some(v);
                }
            }
        }
    }

    /// Cycle a channel selection for an AudioIn unit (L → R → ST → L ...).
    fn cycle_audio_in_channel(&mut self, unit: usize) {
        let next = match self.units.get(unit).map(|u| u.channels) {
            Some(AudioInChannel::MonoL) => AudioInChannel::MonoR,
            Some(AudioInChannel::MonoR) => AudioInChannel::Stereo,
            _ => AudioInChannel::MonoL,
        };
        self.set_audio_in_config(unit, self.units[unit].device.clone(), Some(next));
    }

    /// Rewrite an AudioIn node's device + channels: update the base graph,
    /// unit mirror, bridge mirror, and the engine (like `set_keys_split`).
    fn set_audio_in_config(&mut self, unit: usize, device: Option<String>, channels: Option<AudioInChannel>) {
        let Some(u) = self.units.get(unit) else { return };
        let node = u.node_id;
        let cur_dev = u.device.clone();
        let cur_ch = u.channels;
        let dev = device.or(cur_dev); // None = default input
        let ch = channels.unwrap_or(cur_ch);
        if let Some(n) = self.base_graph.nodes.iter_mut().find(|n| n.id == node) {
            if let NodeKind::AudioIn { device: d, channels: c } = &mut n.kind {
                *d = dev.clone();
                *c = ch;
            }
        }
        if let Ok(mut g) = self.shared_graph.lock() {
            if let Some(n) = g.nodes.iter_mut().find(|n| n.id == node) {
                if let NodeKind::AudioIn { device: d, channels: c } = &mut n.kind {
                    *d = dev.clone();
                    *c = ch;
                }
            }
        }
        if let Some(u) = self.units.get_mut(unit) {
            u.device = dev.clone();
            u.channels = ch;
            u.sub = audio_in_sub(u.device.as_deref(), ch);
            u.sub_g = None;
        }
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetAudioIn { node, device: dev, channels: Some(ch) });
        }
        self.resync_audio_in_capture(node);
    }

    /// Cycle a device selection for an AudioIn unit (through the discovered
    /// list; Q = DEFAULT). Called from the face chip click.
    fn cycle_audio_in_device(&mut self, unit: usize) {
        let devices: Vec<crate::audio_in::InputDevice> = self.input_devices.lock().map(|d| d.clone()).unwrap_or_default();
        let cur = self.units.get(unit).and_then(|u| u.device.clone());
        let next = match (&cur, devices.len()) {
            (None, _) => devices.first().map(|d| d.raw.clone()), // DEFAULT → first named
            (Some(_), 0) => None,
            (Some(c), _) => {
                let pos = devices.iter().position(|d| d.raw == *c);
                match pos {
                    Some(i) if i + 1 < devices.len() => Some(devices[i + 1].raw.clone()),
                    _ => None, // wrap to DEFAULT
                }
            }
        };
        self.set_audio_in_config(unit, next, None);
    }

    fn draw_knob(&self, painter: &egui::Painter, ui: usize, ki: usize) {
        let xf = &self.xf;
        let u = &self.units[ui];
        let k = &u.knobs[ki];
        let c = self.knob_center(ui, ki);
        let r = KNOB_R;
        let hovered = matches!(self.hover, Some(Hit::Knob { unit, idx }) if unit == ui && idx == ki);
        let dragging = matches!(self.drag, Drag::Knob { unit, idx, .. } if unit == ui && idx == ki);
        let target = k.cc_target();
        let is_learning = target.is_some_and(|(p, t)| {
            self.cc_learn.as_ref().is_some_and(|l| l.dest == u.node_id && l.param == p && l.target == t)
        });
        let bound = target.and_then(|(p, t)| self.knob_cc_binding(u.node_id, p, t));
        let pulse = 0.5 + 0.5 * (self.ui_time * 6.0).sin();
        let learn_col = mix(theme::MIDI_BLUE, theme::CABLE_CYAN, pulse);

        // Unfilled disc (§4.6): 1.5px hairline ring, cream on hover, MIDI pulse when learning.
        self.ell(painter, c, r, Color32::TRANSPARENT,
            Stroke::new(1.5, if is_learning { learn_col } else if hovered || dragging { TEXT } else { BORDER }));

        // Flat value arc: hairline track + solid accent arc + flat end dot
        // (theme helper; the closure carries the flip transform).
        theme::value_arc(painter, |l| xf.p(l), c, r, k.value, theme::ACCENT);

        // Pulsing non-tomato outline around the value arc while learning.
        if is_learning {
            self.ell(painter, c, r + 4.5, Color32::TRANSPARENT,
                Stroke::new(2.2, alpha(learn_col, 0.70 + 0.30 * pulse)));
        }

        // Active/dragging knob: the CTA inset-keyline motif as focus state.
        if dragging {
            let disc = Rect::from_center_size(c.to_pos2(), Vec2::splat(2.0 * r));
            theme::inset_keyline(painter, xf.rect(disc), r, theme::ACCENT, 2.0, 2.5);
        }

        // Pointer: center → 0.62r needle, 2px cream.
        let a0 = 135.0f32.to_radians();
        let sweep = 270.0f32.to_radians();
        let ang = a0 + sweep * k.value;
        let tip = xf.p(c + Vec2::new(ang.cos(), ang.sin()) * (r * 0.62));
        painter.line_segment([xf.p(c), tip], Stroke::new(2.0, TEXT));
        let lw = k.label_g.as_ref().map_or(0.0, |g| g.size.x);
        self.text_tracked(painter, c + Vec2::new(-lw / 2.0, r + KNOB_LABEL_GAP), &k.label_g, SUBTEXT);

        // Status plate (LEARN or CC nn) above the knob (rule 35 plate, rule 7 padding).
        let has_tag = (is_learning || bound.is_some()) && self.text_alpha > 0.01;
        if is_learning && self.text_alpha > 0.01 {
            let g = painter.layout_no_wrap("LEARN".into(), theme::medium_font(9.5), theme::CREAM);
            let box_sz = g.size() + Vec2::new(16.0, 8.0);
            let local = Rect::from_min_size(Pos2::new(c.x - box_sz.x / 2.0, c.y - r - 6.5 - box_sz.y), box_sz);
            let sr = xf.rect(local);
            painter.rect_filled(sr, 2.0, theme::DARK_PANEL_HI);
            painter.rect_stroke(sr, 2.0, Stroke::new(1.2, learn_col), egui::StrokeKind::Inside);
            let mut tp = painter.clone();
            tp.set_opacity(self.text_alpha);
            tp.galley(xf.p(local.min.to_vec2() + Vec2::new(8.0, 4.0)), g, theme::CREAM);
        } else if let Some((_, cc)) = bound {
            if self.text_alpha > 0.01 {
                let g = painter.layout_no_wrap(format!("CC {cc}"), theme::medium_font(9.5), theme::CREAM);
                let box_sz = g.size() + Vec2::new(16.0, 8.0);
                let local = Rect::from_min_size(Pos2::new(c.x - box_sz.x / 2.0, c.y - r - 6.5 - box_sz.y), box_sz);
                let sr = xf.rect(local);
                painter.rect_filled(sr, 2.0, theme::DARK_PANEL_HI);
                painter.rect_stroke(sr, 2.0, Stroke::new(1.0, theme::MIDI_BLUE), egui::StrokeKind::Inside);
                let mut tp = painter.clone();
                tp.set_opacity(self.text_alpha);
                tp.galley(xf.p(local.min.to_vec2() + Vec2::new(8.0, 4.0)), g, theme::CREAM);
            }
        }

        // Drag chip: a dark sticker with the value in Medium 13px cream.
        if dragging {
            let g = painter.layout(format!("{:.2}", k.value), theme::medium_font(13.0), theme::INK, f32::INFINITY);
            let y_off = if has_tag { -r - 52.0 } else { -r - 30.0 };
            let chip = Rect::from_min_size(
                xf.p(c + Vec2::new(-g.size().x / 2.0 - 8.0, y_off)),
                g.size() + Vec2::new(16.0, 10.0),
            );
            theme::drop_shadow(painter, chip, 6.0);
            painter.rect_filled(chip, 6.0, theme::PAPER_SINK);
            painter.rect_stroke(chip, 6.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
            painter.galley(chip.min + Vec2::new(8.0, 5.0), g, theme::INK);
        }
    }

    /// (unit, knob) indices of the knob a menu is about, resolved this frame.
    fn knob_menu_target(&self, menu: &KnobMenu) -> Option<(usize, usize)> {
        let unit = self.units.iter().position(|u| u.node_id == menu.node)?;
        let idx = self.units[unit].knobs.iter().position(|k| k.cc_target() == Some((menu.param, menu.target)))?;
        Some((unit, idx))
    }

    fn knob_menu_ui(&mut self, ctx: &egui::Context) {
        let Some(menu) = self.knob_menu else {
            self.knob_menu_rect = Rect::NOTHING;
            return;
        };
        let close = |app: &mut Self| {
            app.knob_menu = None;
            app.knob_menu_rect = Rect::NOTHING;
        };
        if self.stage_view != StageView::Rack || self.side != Side::Front {
            return close(self);
        }
        let Some((unit, idx)) = self.knob_menu_target(&menu) else { return close(self) };
        let (node_id, param, target) = (menu.node, menu.param, menu.target);
        let learning = self.cc_learn.as_ref().is_some_and(|l| l.dest == node_id && l.param == param && l.target == target);
        let binding = self.knob_cc_binding(node_id, param, target);
        if binding.is_none() && !learning {
            return close(self);
        }
        let u = &self.units[unit];
        let title = format!("{} · {}", sentence_case(&u.name), u.knobs[idx].label);
        let mut relearn = false;
        let mut cancel_learn = false;
        let mut clear = false;
        let mut reset_val = false;
        let area = egui::Area::new(egui::Id::new("knob_midi_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(menu.pos + Vec2::new(6.0, 6.0))
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(theme::DARK_PANEL)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::same(10))
                    .shadow(egui::Shadow {
                        offset: [0, 6],
                        blur: 16,
                        spread: 0,
                        color: Color32::from_black_alpha(130),
                    })
                    .show(ui, |ui| {
                        ui.set_min_width(170.0);
                        ui.label(egui::RichText::new(title).font(theme::medium_font(12.5)).color(theme::INK));
                        let status = match binding {
                            Some((src, cc)) if learning => format!("CC {cc} · {} · learning…", Self::source_str(src)),
                            Some((src, cc)) => format!("CC {cc} · {}", Self::source_str(src)),
                            None => "MIDI learn · move a controller".to_string(),
                        };
                        ui.label(egui::RichText::new(status).size(11.0).color(theme::MIDI_BLUE));
                        ui.add_space(6.0);
                        if learning {
                            if theme::chip_button(ui, "Cancel MIDI learn", false).clicked() {
                                cancel_learn = true;
                            }
                        } else if theme::chip_button(ui, "MIDI learn again", false).clicked() {
                            relearn = true;
                        }
                        if binding.is_some() {
                            ui.add_space(4.0);
                            if theme::chip_button(ui, "Clear MIDI map", false).clicked() {
                                clear = true;
                            }
                        }
                        ui.add_space(4.0);
                        if theme::chip_button(ui, "Reset to default", false).clicked() {
                            reset_val = true;
                        }
                    });
            });
        self.knob_menu_rect = area.response.rect;
        if !(relearn || cancel_learn || clear || reset_val) {
            return;
        }
        close(self);
        if relearn {
            self.cc_learn = Some(CcLearn {
                source: SourceId::Keys,
                dest: node_id,
                param,
                target,
                replace_existing: true,
            });
            return;
        }
        // Every other choice ends a learn the right-click started, so a later
        // controller move cannot bind the knob by surprise.
        if learning {
            self.cc_learn = None;
        }
        if clear {
            self.clear_knob_bindings(node_id, param, target);
        } else if reset_val {
            let k = &mut self.units[unit].knobs[idx];
            k.value = k.default;
        }
    }

    fn draw_back(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            let r = self.unit_rects[ui];
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            let agent_t = self.agent_activity.node_alpha(u.node_id, self.ui_time);
            // Same flat plate treatment as the front (§4.4).
            painter.rect_filled(sr, 2.0, CHASSIS);
            let ht = self.unit_hover.get(ui).copied().unwrap_or(0.0);
            if dragging {
                theme::inset_keyline(painter, sr, 2.0, theme::ACCENT, 2.0, 2.5);
            } else if agent_t > 0.01 {
                painter.rect_stroke(sr, 2.0, Stroke::new(1.8, alpha(theme::AGENT_COLOR, agent_t)), egui::StrokeKind::Inside);
            } else {
                // Faint default rule; hover eases to the accent (clear
                // interactive state without shadows — DeepSeek review fix).
                let rule = mix(BORDER, theme::ACCENT, ht);
                painter.rect_stroke(sr, 2.0, Stroke::new(1.0, alpha(rule, 0.3 + 0.7 * ht)), egui::StrokeKind::Inside);
            }
            let active = u.led > 0.05 || dragging;
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(2.0, r.height() - 24.0));
            let strip_col = if active {
                theme::ACCENT
            } else if agent_t > 0.01 {
                mix(BORDER, theme::AGENT_COLOR, agent_t)
            } else {
                BORDER
            };
            painter.rect_filled(xf.rect(strip), 1.0, strip_col);
        }

        // Selection path highlight set.
        let mut hl = [false; MAX_CABLES];
        let mut has_sel = false;
        if let Some(sel) = self.selected {
            if let Some(idx) = self.cables.iter().position(|c| c.id == sel && c.dying < 0.0) {
                has_sel = true;
                hl[idx] = true;
                let mut stack = [0usize; MAX_CABLES];
                let mut sp = 1;
                stack[0] = idx;
                while sp > 0 {
                    sp -= 1;
                    let n = self.cables[stack[sp]].src;
                    for j in 0..self.cables.len() {
                        if !hl[j] && self.cables[j].dst == n && self.cables[j].dying < 0.0 {
                            hl[j] = true; stack[sp] = j; sp += 1;
                        }
                    }
                }
                sp = 1;
                stack[0] = idx;
                while sp > 0 {
                    sp -= 1;
                    let n = self.cables[stack[sp]].dst;
                    for j in 0..self.cables.len() {
                        if !hl[j] && self.cables[j].src == n && self.cables[j].dying < 0.0 {
                            hl[j] = true; stack[sp] = j; sp += 1;
                        }
                    }
                }
            }
        }

        self.draw_cables(painter, &hl, has_sel);
        self.draw_jacks(painter);
        self.draw_plugs(painter);
        self.draw_pulses(painter, &hl, has_sel);
        self.draw_pending(painter);

        // Paint order fix (§4.4): unit titles + jack labels LAST, so cables
        // never cross them.
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            let r = self.unit_rects[ui];
            self.text_tracked(painter, r.min.to_vec2() + Vec2::new(32.0, 10.0), &u.back_g, SUBTEXT);
            let agent_t = self.agent_activity.node_alpha(u.node_id, self.ui_time);
            if agent_t > 0.01 {
                let badge = Rect::from_min_size(Pos2::new(r.right() - 56.0, r.top() + 8.0), Vec2::new(46.0, 16.0));
                self.draw_agent_badge(painter, badge, agent_t);
            }
            for (ji, j) in u.jacks.iter().enumerate() {
                let c = self.jack_pos(ui, ji);
                let (jw, jh) = j.label_g.as_ref().map_or((0.0, 0.0), |g| (g.size.x, g.size.y));
                // Plate-colored backing: a cable may pass BEHIND a jack label,
                // never through its glyphs ("L" read as "WARM" in review).
                if jw > 0.0 && self.text_alpha > 0.01 {
                    let plate = Rect::from_min_size((c + Vec2::new(-jw / 2.0 - 3.0, 16.0)).to_pos2(), Vec2::new(jw + 6.0, jh + 2.0));
                    painter.rect_filled(self.xf.rect(plate), 2.0, alpha(CHASSIS, self.text_alpha));
                }
                self.text_tracked(painter, c + Vec2::new(-jw / 2.0, 17.0), &j.label_g, SUBTEXT);
            }
        }
    }

    fn cable_alpha(c: &Cable, has_sel: bool, highlighted: bool) -> f32 {
        let mut a = 1.0;
        if c.age < 0.18 { a *= c.age / 0.18; }
        if c.dying >= 0.0 { a *= (1.0 - c.dying / 0.55).clamp(0.0, 1.0); }
        if has_sel && !highlighted { a *= 0.28; }
        a
    }

    fn cable_color(&self, c: &Cable) -> Color32 {
        let mut col = c.color;
        if c.kind == PortKind::Audio && c.color == theme::AUDIO_CREAM {
            // Subtle flat warm shift toward the accent (§4.10), ONLY for the
            // neutral cream jacket: TONE·0.20 + WARM·0.15. Colored jackets keep
            // their hue so tomato stays scarce (selection-only) on the back.
            let mut t = 0.0;
            if let Some(su) = self.units.get(c.src) {
                if matches!(su.node_kind, UnitKind::Clap) {
                    let tone = su.knobs.first().map(|k| k.value).unwrap_or(0.35);
                    t += tone * 0.20;
                }
            }
            let warm = self.master_idx().map(|m| knob_val(&self.units[m], P_WARM, 0.15)).unwrap_or(0.15);
            t += warm * 0.15;
            col = mix(col, ORANGE, t);
        }
        col
    }

    fn draw_cables(&self, painter: &egui::Painter, hl: &[bool; MAX_CABLES], has_sel: bool) {
        let xf = &self.xf;
        let volume = self.master_idx()
            .map(|m| 0.15 + knob_val(&self.units[m], P_VOL, 1.0) * 0.85)
            .unwrap_or(0.83);
        for (ci, c) in self.cables.iter().enumerate() {
            if !self.visible(c.src) || !self.visible(c.dst) { continue; }
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.01 { continue; }
            let hovered = matches!(self.hover, Some(Hit::Cable(id, _)) if id == c.id);
            let grabbed = matches!(self.drag, Drag::Cable { id, .. } if id == c.id);
            let level = if c.kind == PortKind::Audio {
                ((c.level + 0.05 + 0.035 * (self.ui_time * 1.4 + c.phase).sin()) * volume).clamp(0.0, 1.0)
            } else { 0.0 };
            let mut width = match c.kind {
                PortKind::Midi => 2.8,
                PortKind::Audio => 3.0 + 1.0 * level,
            };
            if hovered || grabbed { width += 0.8; }

            let curve = [xf.p(c.curve[0].to_vec2()), xf.p(c.curve[1].to_vec2()),
                         xf.p(c.curve[2].to_vec2()), xf.p(c.curve[3].to_vec2())];
            let mut col = self.cable_color(c);
            let edge_at = self.agent_activity.edge_alpha(c.id, self.ui_time);

            // One flat stroke per cable, round caps (§4.10). Selection = the
            // accent color swap + dimming the rest (cable_alpha) — no glow,
            // no centerline.
            if has_sel && hl[ci] {
                col = theme::ACCENT;
                width += 0.5;
            } else if edge_at > 0.01 {
                painter.add(bezier_shape(curve, width + 3.0, alpha(theme::AGENT_COLOR, a * edge_at * 0.45)));
                col = mix(col, theme::AGENT_COLOR, edge_at * 0.85);
            }
            painter.add(bezier_shape(curve, width, alpha(col, a)));
        }
    }

    fn draw_jacks(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            for (ji, j) in u.jacks.iter().enumerate() {
                let c = self.jack_pos(ui, ji);
                let kind = u.kind(j.dir, j.port);
                let hovered = matches!(self.hover, Some(Hit::Jack { unit, jack }) if unit == ui && jack == ji);
                let compat = self.compatible(ui, ji);
                // Flat socket (§4.12): DARK_PANEL fill + 1.2px hairline.
                let socket = Rect::from_center_size(c.to_pos2(), Vec2::splat(24.0));
                painter.rect_filled(xf.rect(socket), 2.0, CHASSIS);
                painter.rect_stroke(xf.rect(socket), 2.0, Stroke::new(1.2, BORDER), egui::StrokeKind::Inside);
                if compat {
                    // Compatible drop-target ring: the accent, ±1.5px pulse.
                    let pulse = 1.5 * (self.ui_time * 7.0).sin();
                    self.ell(painter, c, 15.0 + pulse, Color32::TRANSPARENT, Stroke::new(1.8, alpha(theme::ACCENT, 0.65)));
                }
                if hovered || compat {
                    self.ell(painter, c, 16.0, alpha(kind.color(), 0.16), Stroke::NONE);
                }
                self.ell(painter, c, 6.5, Color32::from_rgb(0x10, 0x10, 0x10),
                    Stroke::new(1.4, if hovered { mix(RING, WHITE, 0.35) } else { RING }));
                // Center terminal: audio round, MIDI square (§3 separability).
                let ta = alpha(kind.color(), if compat || hovered { 1.0 } else { 0.8 });
                match kind {
                    PortKind::Audio => self.ell(painter, c, 2.5, ta, Stroke::NONE),
                    PortKind::Midi => {
                        painter.rect_filled(
                            xf.rect(Rect::from_center_size(c.to_pos2(), Vec2::splat(4.5))), 0.5, ta);
                    }
                }
                // Jack labels are painted last, in draw_back (§4.4).
            }
        }
        // Plug-in flash rings on young cables.
        for c in &self.cables {
            if c.age < 0.4 && c.dying < 0.0 {
                let t = c.age / 0.4;
                let r = 9.0 + c.age * 55.0;
                let col = alpha(c.color, (1.0 - t) * 0.5);
                let a = self.endpoint(c, Dir::Out);
                let b = self.endpoint(c, Dir::In);
                self.ell(painter, a, r, Color32::TRANSPARENT, Stroke::new(1.6, col));
                self.ell(painter, b, r, Color32::TRANSPARENT, Stroke::new(1.6, col));
            }
        }
    }

    fn draw_plugs(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ci, c) in self.cables.iter().enumerate() {
            if c.dying >= 0.0 { continue; }
            if !self.visible(c.src) || !self.visible(c.dst) { continue; }
            let a = Self::cable_alpha(c, false, true);
            for end in [Dir::Out, Dir::In] {
                let over = if end == Dir::Out { c.over_a } else { c.over_b };
                let base = if end == Dir::Out { c.prev_a } else { c.prev_b };
                // Fan-in: stacked plugs on one jack spread slightly.
                let p = if let Some(q) = over { q } else {
                    let (u, port) = if end == Dir::Out { (c.src, c.src_port) } else { (c.dst, c.dst_port) };
                    let mut k = 0.0f32;
                    let mut n = 0.0f32;
                    for (cj, c2) in self.cables.iter().enumerate() {
                        if c2.dying >= 0.0 { continue; }
                        let (u2, p2) = if end == Dir::Out { (c2.src, c2.src_port) } else { (c2.dst, c2.dst_port) };
                        if u2 == u && p2 == port {
                            if cj < ci { k += 1.0; }
                            n += 1.0;
                        }
                    }
                    base + Vec2::new(0.0, (k - (n - 1.0) / 2.0) * 5.0)
                };
                // Flat plug (§4.12): DARK_PANEL_HI body + 1px hairline,
                // cable-family band. Terminal shape separates families (§3):
                // audio = round capsule, MIDI = square.
                let body = Rect::from_center_size(p.to_pos2(), Vec2::new(20.0, 16.0));
                let pr = match c.kind { PortKind::Audio => 8.0, PortKind::Midi => 1.0 };
                let edge_at = self.agent_activity.edge_alpha(c.id, self.ui_time);
                let stroke_col = if edge_at > 0.01 { mix(BORDER, theme::AGENT_COLOR, edge_at) } else { BORDER };
                painter.rect_filled(xf.rect(body), pr, alpha(PLUG, a));
                painter.rect_stroke(xf.rect(body), pr, Stroke::new(if edge_at > 0.01 { 1.5 } else { 1.0 }, alpha(stroke_col, a)), egui::StrokeKind::Inside);
                let band = Rect::from_min_size(body.min + Vec2::new(2.0, 3.0), Vec2::new(16.0, 3.5));
                painter.rect_filled(xf.rect(band), 1.5, alpha(self.cable_color(c), a));
            }
        }
    }

    fn draw_pulses(&self, painter: &egui::Painter, hl: &[bool; MAX_CABLES], has_sel: bool) {
        let xf = &self.xf;
        for (ci, c) in self.cables.iter().enumerate() {
            if !self.visible(c.src) || !self.visible(c.dst) { continue; }
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.02 { continue; }
            // Single flat dot (§4.11): cable color mixed 0.5 toward cream-
            // white. No glow ellipse, no trail bezier — motion = information.
            let col = mix(self.cable_color(c), WHITE, 0.5);
            for p in &c.pulses {
                if !p.active || p.t < 0.0 { continue; }
                let t = p.t.clamp(0.0, 1.0);
                let pos = xf.p(c.point_at_arc(t).to_vec2());
                let r = (3.5 + 2.0 * p.amp) * xf.sy.max(0.5);
                painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
                    center: pos, radius: Vec2::new(r * xf.sx, r * xf.sy),
                    fill: alpha(col, a), stroke: Stroke::NONE, angle: 0.0,
                }));
            }
        }
    }

    fn draw_pending(&self, painter: &egui::Painter) {
        let Some(p) = &self.pending else { return };
        let xf = &self.xf;
        let origin = self.jack_pos(p.unit, p.jack);
        let (a, b) = if p.dir == Dir::Out { (origin, p.cursor) } else { (p.cursor, origin) };
        let curve = [xf.p(a), xf.p(a + p.c1), xf.p(b + p.c2), xf.p(b)];
        let col = p.kind.color();
        // Single flat stroke (§4.12) — no under-stroke.
        painter.add(bezier_shape(curve, 2.8, alpha(col, 0.85)));
        let cursor = xf.p(p.cursor);
        painter.circle_filled(cursor, 6.5, alpha(col, 0.95));
        painter.circle_stroke(cursor, 9.5, Stroke::new(1.6, alpha(col, 0.5)));
    }

    fn draw_hint(&self, painter: &egui::Painter, rect: Rect) {
        // Plain caption line on paper (§4.15) — no pill container.
        let g = if self.pending.is_some() { &self.hint_wire_g }
            else if self.side == Side::Front { &self.hint_front_g }
            else { &self.hint_back_g };
        let Some(g) = g else { return };
        let pos = Pos2::new(16.0, rect.bottom() - 24.0);
        // A refused patch (duplicate, cable limit) replaces the hint briefly.
        if let Some((msg, until)) = &self.patch_notice {
            if *until > self.ui_time {
                let ng = painter.layout_no_wrap(msg.clone(), FontId::proportional(12.5), theme::ACCENT_DEEP);
                painter.galley(pos, ng, theme::ACCENT_DEEP);
                return;
            }
        }
        let hint_right = pos.x + g.size().x;
        painter.galley(pos, g.clone(), theme::INK_2);
        // Info line fallback (narrow window): right-aligned on the hint row,
        // clipped so it never runs into the hint text.
        if self.info_in_hint && !self.stats_line.is_empty() {
            let ig = painter.layout_no_wrap(self.stats_line.clone(), FontId::monospace(11.0), theme::INK_2);
            let clip = Rect::from_min_max(Pos2::new(hint_right + 24.0, pos.y - 4.0), Pos2::new(rect.right() - 16.0, pos.y + 20.0));
            if clip.width() > 60.0 {
                let x = (rect.right() - 16.0 - ig.size().x).max(clip.left());
                painter.with_clip_rect(clip).galley(Pos2::new(x, pos.y + 1.0), ig, theme::INK_2);
            }
        }
    }

    /// Telemetry line for the top bar (user: in the page, not an overlay;
    /// with the sample rate). Refreshed 4x/s so the numbers stay readable.
    /// "ESI MIDIMATE eX MIDI 1 · C4 v100": the device without ALSA's
    /// client prefix/suffix, then the event.
    fn midi_event_text(&self, src: SourceId, what: &str) -> String {
        let dev = match src {
            SourceId::Hardware(d) => self.hw_devices.get(d as usize)
                .map(|n| { let k = midi_port_key(n); k.split_once(':').map_or(k, |(_, port)| port).trim().to_string() })
                .unwrap_or_else(|| format!("HW {d}")),
            SourceId::Keys => "Keyboard".into(),
            other => format!("{other:?}"),
        };
        format!("{dev} · {what}")
    }

    fn stats_text(&mut self) -> String {
        self.stats_timer -= 1.0 / 60.0;
        if self.stats_timer <= 0.0 || self.stats_line.is_empty() {
            self.stats_timer = 0.25;
            let pulses: usize = self.cables.iter().map(|c| c.pulses.iter().filter(|p| p.active).count()).sum();
            let midi_ms = crate::bridge::MIDI_LATENCY_US.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0;
            let sr = crate::bridge::SAMPLE_RATE.load(std::sync::atomic::Ordering::Relaxed);
            let audio = if sr > 0 {
                let frames = crate::bridge::BUFFER_FRAMES;
                format!("{:.1} kHz · {frames} fr ({:.1} ms)", sr as f64 / 1000.0, frames as f64 / sr as f64 * 1000.0)
            } else { "no audio".into() };
            let midi = self.last_midi.as_deref().unwrap_or("no MIDI yet");
            let _ = pulses;
            self.stats_line = format!("{midi} | {audio} · midi {midi_ms:.1} ms · {:.0} fps", self.fps);
        }
        self.stats_line.clone()
    }

    fn draw_help(&self, painter: &egui::Painter, rect: Rect) {
        // Help modal (§4.16): ink scrim over paper, a second appearance of
        // the hero-panel motif — flat near-black card, radius 28, no border.
        painter.rect_filled(rect, 0.0, alpha(theme::SCRIM, 0.62));
        let (Some(title), Some(foot)) = (&self.help_title_g, &self.help_foot_g) else { return };
        const PAD: f32 = 32.0;
        const GUTTER: f32 = 44.0;
        let chip_h = self.help_cols.iter()
            .flat_map(|c| c.rows.iter().flat_map(|r| r.keys.iter().map(|g| g.size().y)))
            .fold(0.0, f32::max) + 2.0 * HELP_CHIP_PAD.y;
        let n_rows = self.help_cols.iter().map(|c| c.rows.len()).max().unwrap_or(0) as f32;
        let cols_w: f32 = self.help_cols.iter().map(|c| c.width).sum::<f32>()
            + GUTTER * self.help_cols.len().saturating_sub(1) as f32;
        // Vertical rhythm: title row, hairline, section header, rows, hairline, footer.
        let title_h = title.size.y.max(26.0);
        let head_h = self.help_cols.iter().map(|c| c.header.size.y).fold(0.0, f32::max);
        let fixed_h = PAD + title_h + 20.0 + head_h + 14.0 + 18.0 + foot.size.y + PAD - 6.0;
        // Generous leading (≥ 6px between chips); compress toward that floor
        // only if the window is too short for the natural pitch.
        let room = (rect.height() - 32.0 - fixed_h).max(0.0);
        let pitch = (chip_h + 10.0).min((room / n_rows.max(1.0)).max(chip_h + 6.0));
        let card_w = (cols_w.max(title.size.x + 34.0) + 2.0 * PAD).max(560.0);
        let card_h = fixed_h + pitch * n_rows;
        let card = Rect::from_center_size(rect.center(), Vec2::new(card_w, card_h));
        painter.rect_filled(card, 28.0, theme::DARK_PANEL);

        // Title row: crossed-wires mark + display title.
        let mut y = card.top() + PAD;
        let mark_c = Pos2::new(card.left() + PAD + 11.0, y + title_h * 0.5);
        theme::crossed_wires(painter, mark_c, 22.0, theme::DARK_PANEL, theme::MarkStyle::Brand);
        theme::paint_tracked(painter, Pos2::new(card.left() + PAD + 34.0, y + (title_h - title.size.y) * 0.5), title, TEXT);
        y += title_h + 10.0;
        let rule = Stroke::new(1.0, theme::HAIRLINE);
        painter.hline(card.left() + PAD..=card.right() - PAD, y, rule);
        y += 10.0;

        // Columns.
        let mut x = card.left() + PAD + ((card_w - 2.0 * PAD - cols_w) * 0.5).max(0.0);
        for c in &self.help_cols {
            theme::paint_tracked(painter, Pos2::new(x, y), &c.header, SUBTEXT);
            let mut ry = y + head_h + 14.0;
            for r in &c.rows {
                let mut kx = x;
                for g in &r.keys {
                    let chip = Rect::from_min_size(Pos2::new(kx, ry), Vec2::new(HelpRow::chip_w(g), chip_h));
                    painter.rect_filled(chip, 5.0, theme::PAPER_SINK);
                    painter.rect_stroke(chip, 5.0, rule, egui::StrokeKind::Inside);
                    painter.galley(chip.center() - g.size() * 0.5, g.clone(), theme::CREAM);
                    kx = chip.right() + HELP_CHIP_GAP;
                }
                let dy = ry + (chip_h - r.desc.size().y) * 0.5;
                painter.galley(Pos2::new(x + c.keys_w + HELP_KEY_GAP, dy), r.desc.clone(), theme::CREAM_MID);
                ry += pitch;
            }
            x += c.width + GUTTER;
        }

        // Footer: hairline + tracked micro-caps close hint.
        let fy = card.bottom() - PAD + 6.0 - foot.size.y;
        painter.hline(card.left() + PAD..=card.right() - PAD, fy - 12.0, rule);
        theme::paint_tracked(painter, Pos2::new(card.left() + PAD, fy), foot, SUBTEXT);
    }
}

fn dt_of(ctx: &egui::Context) -> f32 {
    ctx.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0)
}

fn p_side(unit: usize, app: &DemoApp) -> f32 {
    let x = app.jack_pos(unit, 0).x;
    if x < app.rack_w / 2.0 { -1.0 } else { 1.0 }
}

/// Is the user typing into a text field (chat, API key, search, a DragValue
/// in edit mode)? Only then do rack shortcuts yield. Not
/// `egui_wants_keyboard_input()`: that is true for ANY focused widget.
fn typing(ctx: &egui::Context) -> bool {
    ctx.text_edit_focused()
}

/// Strip-mixer values are drag controls: a click must not park them in
/// text-edit mode, where they swallow 1–9/←→ rig switching (user bug
/// 2026-11-15). Typed entry stays available in the rule editor.
/// One mixer channel strip: name, sub, vertical fader (+ live meter), value.
/// `value` None = a source without a level knob (meter only). Returns the
/// fader rect. The fader is a custom drag widget: it never takes keyboard
/// focus (rule 12), so rig shortcuts keep working after a drag.
/// Split `text` into (segment, is_entity) runs: case-insensitive matches of
/// `entities` (longest first) on word boundaries are highlighted.
fn highlight_segments(text: &str, entities: &[String]) -> Vec<(String, bool)> {
    let lower = text.to_lowercase();
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    let boundary = |p: usize| p == 0 || p >= bytes.len() || !(bytes[p].is_ascii_alphanumeric());
    let mut plain = String::new();
    while i < text.len() {
        if !text.is_char_boundary(i) { i += 1; continue; }
        let hit = entities.iter().find(|e| {
            let el = e.to_lowercase();
            lower[i..].starts_with(&el) && boundary(if i == 0 { 0 } else { i - 1 }).then_some(()).is_some()
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && boundary(i + el.len())
        });
        match hit {
            Some(e) => {
                if !plain.is_empty() { out.push((std::mem::take(&mut plain), false)); }
                let n = e.len();
                out.push((text[i..i + n].to_string(), true));
                i += n;
            }
            None => {
                let ch = text[i..].chars().next().unwrap();
                plain.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if !plain.is_empty() { out.push((plain, false)); }
    out
}

/// Pan readout: "C" at center, else the side and amount ("L 0.30").
fn pan_text(v: f32) -> String {
    if v.abs() < 0.005 { "C 0.00".into() } else if v < 0.0 { format!("L {:.2}", -v) } else { format!("R {:.2}", v) }
}

/// Inverse of [`pan_text`] (also accepts plain numbers).
fn parse_pan(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("c") || t.to_ascii_lowercase().starts_with("c ") { return Some(0.0); }
    if let Some(r) = t.strip_prefix(['L', 'l']) { return r.trim().parse::<f64>().ok().map(|v| -v.abs()); }
    if let Some(r) = t.strip_prefix(['R', 'r']) { return r.trim().parse::<f64>().ok().map(f64::abs); }
    t.parse().ok()
}

/// Mixer channel-strip width (sources and rig buses share it).
const STRIP_W: f32 = 116.0;

fn mixer_strip(ui: &mut egui::Ui, name: &str, sub: &str, value: &mut Option<f32>, level: f32, fader_h: f32) -> Rect {
    let w = STRIP_W;
    let mut fader_rect = Rect::NOTHING;
    ui.allocate_ui_with_layout(egui::vec2(w, fader_h + 90.0), egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.set_width(w);
        let name = if name.chars().count() > 11 { format!("{}…", name.chars().take(10).collect::<String>()) } else { name.to_string() };
        ui.label(egui::RichText::new(name).size(13.0).strong().color(theme::INK));
        let sub = sub.split('·').next().unwrap_or("").trim().to_uppercase();
        let sub = if sub.chars().count() > 13 { format!("{}…", sub.chars().take(12).collect::<String>()) } else { sub };
        theme::tracked_label(ui, &sub, 9.0, 0.12, theme::INK_2);
        ui.add_space(8.0);
        let (r, resp) = ui.allocate_exact_size(egui::vec2(48.0, fader_h), egui::Sense::click_and_drag());
        fader_rect = r;
        let fr = Rect::from_min_size(r.min, egui::vec2(30.0, fader_h));
        let mr = Rect::from_min_size(Pos2::new(r.right() - 8.0, r.top()), egui::vec2(6.0, fader_h));
        let p = ui.painter();
        // Track: hairline groove; travel keeps the cap fully inside the rect.
        let cap_h = 12.0;
        let (y0, y1) = (fr.bottom() - cap_h / 2.0, fr.top() + cap_h / 2.0);
        p.line_segment([Pos2::new(fr.center().x, y0), Pos2::new(fr.center().x, y1)], Stroke::new(2.0, theme::HAIRLINE));
        if let Some(v) = value.as_mut() {
            if resp.dragged() {
                let dy = resp.drag_delta().y;
                *v = (*v - dy / (y0 - y1)).clamp(0.0, 1.0);
            }
            if resp.double_clicked() { *v = 0.8; }
            let y = y0 + (y1 - y0) * *v;
            p.line_segment([Pos2::new(fr.center().x, y0), Pos2::new(fr.center().x, y)], Stroke::new(2.0, theme::CREAM_MID));
            let cap = Rect::from_center_size(Pos2::new(fr.center().x, y), egui::vec2(26.0, cap_h));
            p.rect_filled(cap, 2.0, if resp.dragged() || resp.hovered() { theme::CREAM } else { theme::CHIP_ON });
            p.line_segment([Pos2::new(cap.left() + 4.0, y), Pos2::new(cap.right() - 4.0, y)], Stroke::new(1.0, theme::DARK_PANEL));
        }
        paint_meter(p, mr, level, level);
        if value.is_some() {
            resp.on_hover_text("drag to set · double-click: 0.80");
        }
        ui.add_space(6.0);
        let txt = value.map_or("—".to_string(), |v| format!("{v:.2}"));
        ui.label(egui::RichText::new(txt).size(11.0).monospace().color(theme::CREAM_MID));
    });
    fader_rect
}

/// Lit segments of an n-segment meter. Below the -60 dBFS floor (0.001)
/// nothing lights: the smoothed meter decays toward zero without reaching
/// it, and `ceil` of a 1e-9 tail kept one segment lit forever in silence.
/// Master meter position for a linear peak: a dB scale from −60 dB (0.0)
/// to 0 dBFS (1.0), like a mixer. Linear amplitude made a healthy −12 dBFS
/// light a quarter of the meter ("the master is so low").
fn meter_db_pos(amp: f32) -> f32 {
    if !(amp > 0.0) { return 0.0; }
    ((20.0 * amp.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn lit_segments(level: f32, n: usize) -> usize {
    if level < 0.001 { return 0; }
    ((level.min(1.0) * n as f32).ceil() as usize).min(n)
}

/// Vertical stepped meter (Hurricane: flat segments, cream → tomato → clip).
fn paint_meter(p: &egui::Painter, r: Rect, level: f32, peak: f32) {
    let segs = ((r.height() / 6.0) as usize).max(4);
    let seg_h = r.height() / segs as f32;
    let lit = lit_segments(level, segs);
    let peak_i = (peak.clamp(0.0, 1.0) * segs as f32).round() as usize;
    for i in 0..segs {
        let y = r.bottom() - (i + 1) as f32 * seg_h;
        let sr = Rect::from_min_size(Pos2::new(r.left(), y + 1.0), egui::vec2(r.width(), seg_h - 2.0));
        let t = (i as f32 + 0.5) / segs as f32;
        let on = i < lit || (peak_i > 0 && i + 1 == peak_i);
        p.rect_filled(sr, 1.0, if on { theme::meter_color(t) } else { theme::METER_TRACK });
    }
}

fn drag_only(r: egui::Response) -> egui::Response {
    if r.has_focus() { r.surrender_focus(); }
    r
}

impl DemoApp {
    /// One app frame minus eframe plumbing (`ui()` delegates here) — tests
    /// drive the exact runtime path through a plain `egui::Context`.
    fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Subtitles first: a changed instrument name is laid out this frame.
        self.refresh_instrument_subs();
        self.ensure_galleys(&ctx);

        // Fractional-HiDPI fix: winit reports scale 1.0 on Hyprland's 1.6x
        // output, and eframe re-applies it AFTER the app creator runs — so the
        // correction must live here, on the frame path, not in the creator.
        // The target is the COMPOSITOR's monitor scale (ui_scale_target):
        // winit-under-XWayland derives 2.33 from the panel's physical DPI on
        // a 1.6x Hyprland output, which inflated every element by ~46% and
        // squished the rack (user report 2026-11-15).
        let target = ui_scale_target();
        if (ctx.pixels_per_point() - target).abs() > 0.01 {
            ctx.set_pixels_per_point(target);
            eprintln!("ui scale: corrected to {target:.2} on frame path");
        }
        static SCALE_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SCALE_LOGGED.swap(true, Ordering::Relaxed) {
            eprintln!("ui scale: first-frame ppp={:.2}", ctx.pixels_per_point());
        }

        if self.flip_requested {
            self.flip_requested = false;
            self.on_tab();
        }
        // Dock toggles (F12 / C / V) — handled before the panels so they work
        // even while the chat field has keyboard focus. The dock occupant owns
        // the computer keyboard, but its own close/swap shortcuts never yield.
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F12)) {
            self.toggle_dock(DockView::Assistant);
        }
        // Letter shortcuts yield to a focused text field (rule 12): 'c' / 'v'
        // typed into the chat must land in the chat, not swap the dock.
        if !typing(&ctx) {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::C)) {
                self.toggle_dock(DockView::Assistant);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::V)) {
                self.toggle_dock(DockView::Keyboard);
            }
            // Agent undo/redo (Ctrl+Shift+Z first: Ctrl+Z would match it too).
            let redo = ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z));
            let undo = !redo && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Z));
            if redo || undo {
                if let Err(e) = self.undo_agent(undo) { self.session_msg = e; }
            }
        }

        // Per-rig input selection strip (top, includes the transport row), then
        // assistant (right), then rack.
        self.rig_inputs_panel(ui);
        self.rig_overview_panel(ui);
        self.dock_panel_ui(ui);

        // Native rig editor (task-26): handle open/close requests from unit
        // double-clicks. No X11 — the editor is a plain egui window drawn below.
        if let Some(rig) = self.editor_request.take() {
            self.toggle_param_editor(rig);
        }
        if let Some((node_id, uri, name)) = self.lv2_editor_request.take() {
            self.toggle_lv2_editor(node_id, &uri, &name);
        }

        // Laptop keys (disabled while the assistant owns the computer keyboard,
        // or while any text field has focus). Losing keyboard focus drains held
        // keys so note-offs can't orphan (vibez lesson, docs/vibez-notes.md).
        let kb_focus = !typing(&ctx) && !self.assistant_open() && ctx.input(|i| i.focused);
        if !kb_focus && self.had_kb_focus && !self.held_keys.is_empty() {
            self.current_source = SourceId::Keys;
            let held: Vec<_> = self.held_keys.drain(..).collect();
            for (_, semi) in held { self.fire_keys(semi, 0.0); }
        }
        self.had_kb_focus = kb_focus;
        if !typing(&ctx) && !self.assistant_open() {
            let events = ctx.input(|i| i.events.clone());
            for ev in events {
                if let egui::Event::Key { key, pressed, repeat, modifiers, .. } = ev {
                    if !pressed || modifiers.is_none() {
                        self.handle_laptop_key(key, pressed, repeat);
                    }
                }
            }
        }
        self.poll_assistant();
        self.serve_ui_inbox();
        self.sync_from_engine();
        self.poll_plugin_scan();
        self.poll_install();
        if self.plugin_scan.is_some() || self.install.as_ref().is_some_and(|i| i.running) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Scheduled note-offs (assistant play_note tool).
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.note_offs.len() {
            if self.note_offs[i].0 <= now {
                let (_, semi) = self.note_offs.remove(i);
                self.current_source = SourceId::Keys;
                self.fire_keys(semi, 0.0);
            } else {
                i += 1;
            }
        }

        // Hardware MIDI: the midir thread already sounded it (hw_direct);
        // the frame animates keys + cables. Publish the VEL knob it scales by.
        self.hw_direct.set_keys_vel(self.units.get(self.keys_idx()).and_then(|u| u.knobs.get(1)).map(|k| k.value).unwrap_or(0.8));
        let hw: Vec<(NoteEvent, bool)> = self
            .hw_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        for (ev, sounded) in hw {
            if !sounded {
                // No engine on the direct path: arrival → UI drain is the latency.
                crate::bridge::record_midi_latency(ev.created.elapsed().as_micros() as u32);
            }
            if let SourceId::Hardware(d) = ev.source {
                if let Some(l) = self.hw_leds.get_mut(d as usize) { *l = 1.0; }
            }
            self.last_midi = Some(self.midi_event_text(ev.source, &if ev.velocity > 0.0 {
                format!("{} v{}", note_name(ev.note), (ev.velocity * 127.0).round() as u8)
            } else {
                format!("{} off", note_name(ev.note))
            }));
            let semi = ev.note as usize;
            self.current_source = ev.source;
            self.fire_keys_with(semi, ev.velocity, !sounded);
        }
        let hw_ccs: Vec<(SourceId, u8, u8, u8)> = self
            .hw_cc_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        for (src, ch, cc, val) in hw_ccs {
            if let SourceId::Hardware(d) = src {
                if let Some(l) = self.hw_leds.get_mut(d as usize) { *l = 1.0; }
            }
            self.last_midi = Some(self.midi_event_text(src, &format!("CC{cc} {val}")));
            let learned = self.complete_cc_learn(src, cc);
            self.apply_cc_to_knobs(src, ch, cc, val, learned);
        }

        let rect = ui.available_rect_before_wrap();
        let dt = ctx.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0);
        self.fps += (1.0 / dt - self.fps) * 0.05;

        self.layout(rect);
        if !typing(&ctx) {
            self.handle_input(&ctx, rect);
        }
        self.update(dt);

        self.publish_patch();
        self.publish_pins();

        self.tessellate();
        match self.stage_view {
            StageView::Rack => self.draw(ui),
            StageView::Matrix => self.matrix_stage(ui),
            StageView::Mixer => self.mixer_stage(ui),
            StageView::Sessions => self.sessions_stage(ui),
            StageView::Plugins => self.plugins_stage(ui),
        }
        self.knob_menu_ui(&ctx);
        self.delete_rig_modal(&ctx);
        self.rename_rig_modal(&ctx);
        self.save_rig_modal(&ctx);
        self.load_rig_modal(&ctx);
        self.param_editor_window(&ctx);

        // Full frame rate while focused (the cables are the show); ~30 fps in
        // the background — meters and cables still move, at a third of the
        // UI thread's CPU when the user works in another window.
        if ctx.input(|i| i.focused) || self.agent_activity.has_active(self.ui_time) {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

}

impl eframe::App for DemoApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Tab belongs to the rack flip, ALWAYS. Strip it from raw input before
        // egui processes anything, so its focus navigation can't steal it and
        // bounce focus between the assistant field and the widgets.
        let mut flip = false;
        raw_input.events.retain(|ev| {
            if matches!(ev, egui::Event::Key { key: Key::Tab, pressed: true, .. }) {
                flip = true;
                false
            } else {
                true
            }
        });
        if flip {
            self.flip_requested = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_ui(ui);
    }

    fn on_exit(&mut self) {
        // Native editor holds no plugin GUI; just drop its cached state.
        self.editor_open = false;
        self.editor_params.clear();
        // Drain + finalize any take in flight and join the writer thread, so a
        // clean exit never leaves a RIFF=36/data_size=0 shell behind.
        if let Some(r) = self.recorder.take() {
            r.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// End-to-end session self-test: capture → save → reset → load → restore.
/// Exercises the real capture_session/apply_session paths (no GUI/audio needed).
pub fn run_session_test() -> anyhow::Result<()> {
    let mut app = DemoApp::new_with_sink(None);
    app.units[1].knobs[1].value = 0.42; // rig A level
    app.units[3].knobs[0].value = 0.77; // master volume
    app.units[5].knobs[1].value = 0.31; // rig B delay mix

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = std::path::PathBuf::from(home).join(".cache").join("ksalat-session-selftest");
    let _ = std::fs::create_dir_all(&dir);

    let session = app.capture_session("selftest")?;
    let pkg = crate::session::Session::package_path(&dir, "selftest")?;
    session.save(&pkg)?;
    let edges = session.graph.edges.len();

    // Wipe the knobs, then restore from disk.
    app.units[1].knobs[1].value = 0.0;
    app.units[3].knobs[0].value = 0.0;
    app.units[5].knobs[1].value = 0.0;
    let loaded = crate::session::Session::load(&pkg)?;
    app.apply_session(&loaded)?;

    anyhow::ensure!((app.units[1].knobs[1].value - 0.42).abs() < 1e-6, "rig A level not restored");
    anyhow::ensure!((app.units[3].knobs[0].value - 0.77).abs() < 1e-6, "master volume not restored");
    anyhow::ensure!((app.units[5].knobs[1].value - 0.31).abs() < 1e-6, "rig B delay mix not restored");
    anyhow::ensure!(loaded.graph.edges.len() == edges, "edge count changed across round-trip");

    // --- Keys split (a node property): point + the HIGH cable round-trip ---
    let k = app.keys_unit.ok_or_else(|| anyhow::anyhow!("demo has no Keys"))?;
    app.set_keys_split(k, Some(60));
    let surge_b = app.units.iter().position(|u| u.node_id == 4).ok_or_else(|| anyhow::anyhow!("no Surge B"))?;
    let b = app.cables.iter().position(|c| c.src == k && c.dst == surge_b)
        .ok_or_else(|| anyhow::anyhow!("no Keys→Surge B cable"))?;
    let id = app.cables[b].id;
    app.cables.remove(b);
    app.add_cable_id(id, k, 1, surge_b, 0, PortKind::Midi); // Surge B on HIGH
    let split_pkg = crate::session::Session::package_path(&dir, "selftest-split")?;
    app.capture_session("selftest-split")?.save(&split_pkg)?;
    app.set_keys_split(k, None);
    app.apply_session(&crate::session::Session::load(&split_pkg)?)?;
    let k = app.keys_unit.ok_or_else(|| anyhow::anyhow!("split session lost Keys"))?;
    anyhow::ensure!(app.units[k].split == Some(60), "split point not restored");
    anyhow::ensure!(app.units[k].jacks.len() == 2, "split Keys must show LOW + HIGH jacks");
    anyhow::ensure!(app.cables.iter().any(|c| c.src == k && c.src_port == 1 && app.units[c.dst].node_id == 4),
        "HIGH cable (Keys port 1 → Surge B) not restored");
    println!("  keys split: point C4 + HIGH cable round-trip, two MIDI outs — OK");

    // --- Starter templates: each must rebuild a correct, playable rack ---
    // (units, cables, Keys/Master present, default params applied, layout grouped)
    let expect = [
        (Template::LeadReverb, 4usize, 4usize, 1usize, 2usize),
        (Template::DrumsBass, 6, 6, 2, 2),
        (Template::AmbientPad, 5, 5, 1, 3),
    ];
    for (t, n_units, n_cables, n_cols, n_rows) in expect {
        let s = t.session();
        app.apply_session(&s)?;
        anyhow::ensure!(app.units.len() == n_units, "{}: unit count {} != {}", t.title(), app.units.len(), n_units);
        anyhow::ensure!(app.cables.len() == n_cables, "{}: cable count {} != {}", t.title(), app.cables.len(), n_cables);
        anyhow::ensure!(app.keys_unit.is_some(), "{}: missing Keys", t.title());
        anyhow::ensure!(app.master_unit.is_some(), "{}: missing Master", t.title());
        anyhow::ensure!(app.stack_cols.len() == n_cols, "{}: stack columns {} != {}", t.title(), app.stack_cols.len(), n_cols);
        anyhow::ensure!(app.layout_rows == n_rows, "{}: rows {} != {}", t.title(), app.layout_rows, n_rows);
        for p in s.params() {
            let u = app.units.iter().find(|u| u.node_id == p.node)
                .ok_or_else(|| anyhow::anyhow!("{}: param node {} missing", t.title(), p.node))?;
            let k = u.knobs.iter().find(|k| k.param == Some(p.param))
                .ok_or_else(|| anyhow::anyhow!("{}: knob for param {} missing", t.title(), p.param))?;
            anyhow::ensure!((k.value as f64 - p.value).abs() < 1e-6, "{}: default param not applied", t.title());
        }
        println!("  template '{}': {} units, {} cables, {} stack col(s), {} row(s) — OK",
            t.title(), app.units.len(), app.cables.len(), app.stack_cols.len(), app.layout_rows);
    }
    // --- LV2 plugin-browser graphs (task-18 effect / task-20 instrument) ---
    // Effect chain: Keys→Surge→LV2→Master (+dry) = 4 units, 4 cables.
    app.add_lv2_effect("http://example.org/lv2/gain", "TEST GAIN");
    anyhow::ensure!(app.units.len() == 4, "lv2 effect unit count");
    anyhow::ensure!(app.cables.len() == 4, "lv2 effect cable count");
    // Instrument rig: Keys→LV2→Master = 3 units, 2 cables (1 MIDI + 1 audio).
    app.add_lv2_instrument("http://example.org/lv2/osc", "TEST OSC");
    anyhow::ensure!(app.units.len() == 3, "lv2 instrument unit count");
    anyhow::ensure!(app.cables.len() == 2, "lv2 instrument cable count");
    anyhow::ensure!(app.keys_unit.is_some() && app.master_unit.is_some(), "lv2 instrument keys/master");
    let n_midi = app.cables.iter().filter(|c| c.kind == PortKind::Midi).count();
    let n_audio = app.cables.iter().filter(|c| c.kind == PortKind::Audio).count();
    anyhow::ensure!(n_midi == 1 && n_audio == 1, "lv2 instrument cable kinds (midi={n_midi} audio={n_audio})");
    println!("  lv2 browser: effect 4u/4c, instrument 3u/2c (1 MIDI + 1 audio) — OK");

    // --- Ctrl+drag rearrange (task-21): reorder must keep a valid partition and
    // reflow with no overlaps; unit indices (cable endpoints) stay untouched. ---
    app.load_demo_patch();
    let test_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
    app.layout(test_rect);
    let u0 = app.stack_cols[0][0];
    let last_col = app.stack_cols.last().unwrap().clone();
    let anchor = app.unit_rects[last_col[0]];
    let drop = Vec2::new(anchor.center().x, anchor.bottom() + 40.0);
    let cables_before: Vec<(usize, usize)> = app.cables.iter().map(|c| (c.src, c.dst)).collect();
    app.drop_unit(u0, drop);
    let mut seen = vec![false; app.units.len()];
    for col in &app.stack_cols {
        for &u in col {
            anyhow::ensure!(!seen[u], "rearrange: unit {u} duplicated");
            seen[u] = true;
            anyhow::ensure!(app.keys_unit != Some(u) && app.master_unit != Some(u), "rearrange: anchor moved");
        }
    }
    for u in 0..app.units.len() {
        if app.keys_unit == Some(u) || app.master_unit == Some(u) { continue; }
        anyhow::ensure!(seen[u], "rearrange: unit {u} lost");
    }
    let cables_after: Vec<(usize, usize)> = app.cables.iter().map(|c| (c.src, c.dst)).collect();
    anyhow::ensure!(cables_before == cables_after, "rearrange: cable endpoints changed");
    app.layout(test_rect);
    for a in 0..app.units.len() {
        for b in (a + 1)..app.units.len() {
            anyhow::ensure!(!app.unit_rects[a].intersects(app.unit_rects[b]), "rearrange: overlap {a}/{b}");
        }
    }
    println!("  ctrl+drag rearrange: partition valid, cables intact, reflow no overlaps — OK");

    // Demo patch must still load after the templates.
    app.load_demo_patch();
    anyhow::ensure!(app.units.len() == 6 && app.cables.len() == 8, "demo patch not restored after templates");

    let _ = std::fs::remove_dir_all(&dir);
    println!("session-test: OK — capture→save→load→restore ({edges} edges, 3 knobs) + 3 templates + lv2 browser + demo restore");
    Ok(())
}

// ---------------------------------------------------------------------------
// agent-eval: the real agent loop against a headless app + offline engine
// ---------------------------------------------------------------------------

/// The patch a task starts from.
#[derive(Clone, Copy)]
enum EvalStart {
    /// What the app opens with: one rig, Keys → Surge A/B → Delay A/B → Master.
    Demo,
    /// Keys → Surge → Master.
    OneRig,
    /// Rig A: Keys → Surge → Delay → Master. Rig B: Keys → Surge → Master.
    TwoRigs,
}

/// One scripted request and what the app must look like afterwards.
struct EvalTask {
    name: &'static str,
    start: EvalStart,
    prompt: &'static str,
    /// Err says what is off. Gets the agent's final reply for questions.
    check: fn(&mut DemoApp, &str) -> Result<(), String>,
}

fn eval_ensure(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

fn eval_graph(app: &DemoApp) -> Graph {
    app.shared_graph.lock().map(|g| g.clone()).unwrap_or_default()
}

fn eval_node(g: &Graph, rig: u8, what: &str, is: impl Fn(&NodeKind) -> bool) -> Result<u32, String> {
    g.nodes.iter().find(|n| n.rig == rig && is(&n.kind)).map(|n| n.id)
        .ok_or_else(|| format!("no {what} in rig {}", rig + 1))
}

fn eval_named(g: &Graph, name: &str) -> Result<u32, String> {
    g.nodes.iter().find(|n| name_str(&n.name) == name).map(|n| n.id).ok_or_else(|| format!("no node named {name}"))
}

/// Does audio flow from `from` to `to` along the cables?
fn eval_audio_path(g: &Graph, from: u32, to: u32) -> bool {
    let mut seen = vec![from];
    let mut i = 0;
    while i < seen.len() {
        let at = seen[i];
        if at == to { return true; }
        for e in g.edges.iter().filter(|e| e.kind == CPortKind::Audio && e.from.node == at) {
            if !seen.contains(&e.to.node) { seen.push(e.to.node); }
        }
        i += 1;
    }
    false
}

fn eval_midi_cable(g: &Graph, from: u32, port: u8, to: u32) -> bool {
    g.edges.iter().any(|e| e.kind == CPortKind::Midi && e.from.node == from && e.from.port == port && e.to.node == to)
}

/// "category/name" (lower case) of the patch loaded into `rig`'s synth.
fn eval_patch(app: &mut DemoApp, rig: usize) -> Result<String, String> {
    let multi = app.rig_count() > 1;
    let slot = app.units.iter().filter(|u| matches!(u.node_kind, UnitKind::Clap))
        .position(|u| !multi || u.rig as usize == rig)
        .ok_or_else(|| format!("rig {} has no synth", rig + 1))?;
    let name = app.patch_names.get(slot).cloned().flatten()
        .ok_or_else(|| format!("rig {} still plays the init patch", rig + 1))?;
    let category = app.surge_patches().iter().find(|p| p.name == name).map(|p| p.category.clone()).unwrap_or_default();
    Ok(format!("{category}/{name}").to_lowercase())
}

/// The rig's one unit of `kind` (asking for "a delay" must not leave two).
fn eval_one_dsp(g: &Graph, rig: u8, kind: &DspKind) -> Result<u32, String> {
    let found: Vec<u32> = g.nodes.iter().filter(|n| n.rig == rig && n.kind == NodeKind::Dsp(kind.clone())).map(|n| n.id).collect();
    match found[..] {
        [one] => Ok(one),
        _ => Err(format!("rig {} has {} {kind:?} unit(s), expected 1", rig + 1, found.len())),
    }
}

/// One effect of `kind` that the rig's synth feeds and that reaches the master.
fn eval_effect_in_chain(g: &Graph, rig: u8, kind: DspKind) -> Result<(), String> {
    let fx = eval_one_dsp(g, rig, &kind)?;
    let synth = eval_node(g, rig, "synth", |k| matches!(k, NodeKind::Clap { .. }))?;
    let master = eval_node(g, rig, "master", |k| matches!(k, NodeKind::MasterOut))?;
    eval_ensure(eval_audio_path(g, synth, fx), || format!("the synth does not feed the {kind:?}"))?;
    eval_ensure(eval_audio_path(g, fx, master), || format!("the {kind:?} does not reach the master"))
}

const EVAL_TASKS: &[EvalTask] = &[
    EvalTask {
        name: "tempo", start: EvalStart::Demo, prompt: "Set the tempo to 100 BPM.",
        check: |app, _| eval_ensure((app.bpm - 100.0).abs() < 0.01, || format!("tempo is {}", app.bpm)),
    },
    EvalTask {
        name: "ask_tempo", start: EvalStart::Demo, prompt: "What tempo are we at?",
        check: |app, reply| {
            eval_ensure(reply.contains("96"), || "the reply does not name 96 BPM".into())?;
            eval_ensure(app.undo_past.is_empty() && (app.bpm - 96.0).abs() < 0.01, || "a question changed the patch".into())
        },
    },
    EvalTask {
        name: "split_c4", start: EvalStart::Demo, prompt: "Split the keyboard at C4.",
        check: |app, _| {
            let split = app.keys_unit.and_then(|k| app.units.get(k)).and_then(|u| u.split);
            eval_ensure(split == Some(60), || format!("split point is {split:?}"))
        },
    },
    EvalTask {
        name: "split_zones", start: EvalStart::Demo,
        prompt: "Split the keyboard at C4: Surge A plays the low half, Surge B the high half.",
        check: |app, _| {
            let g = eval_graph(app);
            let keys = eval_node(&g, 0, "Keys", |k| matches!(k, NodeKind::Keys))?;
            let (a, b) = (eval_named(&g, "SURGE A")?, eval_named(&g, "SURGE B")?);
            let split = g.node(keys).and_then(|n| n.split);
            eval_ensure(split == Some(60), || format!("split point is {split:?}"))?;
            eval_ensure(eval_midi_cable(&g, keys, 0, a), || "LOW is not wired to Surge A".into())?;
            eval_ensure(eval_midi_cable(&g, keys, 1, b), || "HIGH is not wired to Surge B".into())?;
            eval_ensure(!eval_midi_cable(&g, keys, 0, b), || "Surge B still plays the low half".into())?;
            eval_ensure(!eval_midi_cable(&g, keys, 1, a), || "Surge A also plays the high half".into())
        },
    },
    EvalTask {
        name: "warm_pad", start: EvalStart::OneRig, prompt: "Load a warm pad.",
        check: |app, _| {
            let patch = eval_patch(app, 0)?;
            eval_ensure(patch.contains("pad"), || format!("loaded {patch}, not a pad"))
        },
    },
    EvalTask {
        name: "add_reverb", start: EvalStart::TwoRigs, prompt: "Add a reverb to rig 1 and wire it in.",
        check: |app, _| {
            let g = eval_graph(app);
            eval_effect_in_chain(&g, 0, DspKind::Reverb)?;
            eval_ensure(!g.nodes.iter().any(|n| n.rig == 1 && n.kind == NodeKind::Dsp(DspKind::Reverb)), || "rig 2 got a reverb too".into())
        },
    },
    EvalTask {
        name: "delay_after_synth", start: EvalStart::OneRig, prompt: "Put a delay after the synth.",
        check: |app, _| eval_effect_in_chain(&eval_graph(app), 0, DspKind::Delay),
    },
    EvalTask {
        name: "remove_delay", start: EvalStart::TwoRigs, prompt: "Remove the delay from rig 1.",
        check: |app, _| {
            let g = eval_graph(app);
            eval_ensure(!g.nodes.iter().any(|n| n.rig == 0 && n.kind == NodeKind::Dsp(DspKind::Delay)), || "rig 1 still has its delay".into())?;
            let synth = eval_node(&g, 0, "synth", |k| matches!(k, NodeKind::Clap { .. }))?;
            let master = eval_node(&g, 0, "master", |k| matches!(k, NodeKind::MasterOut))?;
            eval_ensure(eval_audio_path(&g, synth, master), || "rig 1's synth no longer reaches its master".into())?;
            let rig_b = g.nodes.iter().filter(|n| n.rig == 1).count();
            eval_ensure(rig_b == 3, || format!("rig 2 changed ({rig_b} units)"))
        },
    },
    EvalTask {
        name: "metronome", start: EvalStart::OneRig, prompt: "Add a metronome and start it.",
        check: |app, _| {
            let g = eval_graph(app);
            let metro = eval_one_dsp(&g, 0, &DspKind::Metronome)?;
            let master = eval_node(&g, 0, "master", |k| matches!(k, NodeKind::MasterOut))?;
            eval_ensure(eval_audio_path(&g, metro, master), || "the metronome is not wired to the master".into())?;
            eval_ensure(g.node(metro).is_some_and(|n| n.playing), || "the metronome is not running".into())
        },
    },
    EvalTask {
        name: "add_rig", start: EvalStart::Demo, prompt: "Add a new rig.",
        check: |app, _| eval_ensure(app.rig_count() == 2, || format!("{} rig(s)", app.rig_count())),
    },
    EvalTask {
        name: "rig2_volume", start: EvalStart::TwoRigs, prompt: "Set rig 2 volume to 50 %.",
        check: |app, _| {
            let (a, b) = (app.rig_mix[0].vol, app.rig_mix[1].vol);
            eval_ensure((b - 0.5).abs() < 0.011 && a == 1.0, || format!("rig 1 vol {a:.2}, rig 2 vol {b:.2}"))
        },
    },
    EvalTask {
        name: "mute_rig1", start: EvalStart::TwoRigs, prompt: "Mute rig 1.",
        check: |app, _| {
            let (a, b) = (app.rig_mix[0].mute, app.rig_mix[1].mute);
            eval_ensure(a && !b, || format!("rig 1 mute {a}, rig 2 mute {b}"))
        },
    },
    EvalTask {
        name: "delete_rig2", start: EvalStart::TwoRigs, prompt: "Delete rig 2.",
        check: |app, _| {
            eval_ensure(app.rig_count() == 2, || "the rig was deleted without the user's Confirm".into())?;
            let asked = app.pending_confirm.as_ref()
                .is_some_and(|p| p.tool == "delete_rig" && p.args.get("rig").and_then(|r| r.as_u64()) == Some(1));
            eval_ensure(asked, || format!("no Confirm prompt for rig 2 (pending: {:?})", app.pending_confirm.as_ref().map(|p| &p.label)))
        },
    },
    EvalTask {
        name: "bass_rig", start: EvalStart::OneRig, prompt: "Make a new rig with a bass sound, quieter than rig 1.",
        check: |app, _| {
            eval_ensure(app.rig_count() == 2, || format!("{} rig(s)", app.rig_count()))?;
            let patch = eval_patch(app, 1)?;
            eval_ensure(patch.contains("bass"), || format!("the new rig plays {patch}, not a bass"))?;
            let (a, b) = (app.rig_mix[0].vol, app.rig_mix[1].vol);
            eval_ensure(b < a, || format!("the new rig is not quieter (rig 1 {a:.2}, rig 2 {b:.2})"))
        },
    },
];

/// A headless app on an offline engine: no window, no audio device, no
/// hardware scan. Frames run the app's real frame path; each frame renders
/// one engine block so commands and publications flow as in the live app.
struct EvalRig {
    // Declared first so it drops first: it owns the voices' audio halves.
    engine: GraphEngine,
    app: DemoApp,
    ctx: egui::Context,
    buf: Vec<f32>,
    started: std::time::Instant,
}

type EvalPlugins = (Vec<ClapPluginInfo>, Vec<Lv2PluginInfo>);

impl EvalRig {
    fn new(assistant: Assistant, plugins: &EvalPlugins) -> anyhow::Result<Self> {
        let sample_rate = 48000.0;
        let mut voices = Vec::with_capacity(VOICE_POOL);
        let mut mains = Vec::with_capacity(VOICE_POOL);
        for i in 0..VOICE_POOL {
            let (v, m) = ClapVoice::new(sample_rate)?;
            voices.push((match i { 0 => 1, 1 => 4, _ => u32::MAX }, v));
            mains.push(m);
        }
        let controllers = voices.iter().map(|(_, v)| v.controller()).collect();
        let voice_ids = voices.iter().map(|(_, v)| v.voice_id()).collect();
        let (bus, drain) = crate::control::command_bus();
        let mut engine = GraphEngine::new(Graph::demo_patch(), drain, sample_rate, voices);
        let mut app = DemoApp::new_app(None, false, assistant);
        engine.set_rig_snapshot(app.rig_snapshot.clone());
        let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
        engine.set_graph_pub(graph_pub.clone());
        let (voice_tx, voice_rx) = rtrb::RingBuffer::new(16);
        let (retire_tx, retire_rx) = rtrb::RingBuffer::new(16);
        engine.set_voice_channels(voice_rx, retire_tx);
        engine.spawn_lv2_loader();
        app.graph_pub = Some(graph_pub);
        app.attach_engine(mains, controllers, voice_ids, sample_rate, voice_tx, retire_rx, Arc::new(Mutex::new(bus)));
        (app.plugins_clap, app.plugins_lv2) = plugins.clone();
        app.plugins_loaded = true;
        Ok(Self { engine, app, ctx: egui::Context::default(), buf: vec![0.0; 256 * 2], started: std::time::Instant::now() })
    }

    fn frame(&mut self) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.time = Some(self.started.elapsed().as_secs_f64());
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        self.engine.render(&mut self.buf, 2);
    }

    /// Enough frames for the engine to apply what the app sent and for the
    /// app to adopt what the engine published.
    fn settle(&mut self) {
        for _ in 0..8 { self.frame(); }
    }

    fn load(&mut self, start: EvalStart) -> anyhow::Result<()> {
        self.settle();
        match start {
            EvalStart::Demo => {}
            EvalStart::OneRig => {
                let s = crate::session::Session::new("One rig", crate::control::standard_rig_graph(true));
                self.app.apply_session(&s)?;
            }
            EvalStart::TwoRigs => self.app.load_multi_rig_demo(),
        }
        self.settle();
        Ok(())
    }
}

/// What one run of one task cost and whether the app ended up right.
struct EvalRun {
    verdict: Result<(), String>,
    /// Wall time from the request to the agent's last word.
    secs: f64,
    /// Seconds each model step took; their sum is the part of `secs` spent
    /// waiting for the model.
    step_secs: Vec<f64>,
    /// Tool calls the worker refused (unknown tool, arguments off-schema).
    rejected: usize,
    tools: Vec<String>,
    routed: Vec<String>,
    reply: String,
}

/// Longest a turn may take before the run counts as failed.
const EVAL_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

fn run_eval_task(task: &EvalTask, model: &str, jev: bool, plugins: &EvalPlugins) -> anyhow::Result<EvalRun> {
    let mut rig = EvalRig::new(Assistant::with_model(model, jev), plugins)?;
    let endpoint = crate::llm::Route::for_model(model).endpoint.spec();
    anyhow::ensure!(rig.app.assistant.key_set, "{model} needs {}'s key ({}) in {}", endpoint.label, endpoint.key_field, crate::assistant::config_path().display());
    rig.load(task.start)?;
    rig.app.submit_chat(task.prompt.to_string());
    let sent = std::time::Instant::now();
    while rig.app.assistant.busy && sent.elapsed() < EVAL_TURN_TIMEOUT {
        rig.frame();
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    let timed_out = rig.app.assistant.busy;
    rig.settle();
    let turn = rig.app.assistant.turn.clone();
    let reply = rig.app.assistant.history.iter().rev().find(|l| l.role == "assistant").map(|l| l.text.clone()).unwrap_or_default();
    let verdict = if timed_out {
        Err(format!("no answer within {} s", EVAL_TURN_TIMEOUT.as_secs()))
    } else if let Some(e) = turn.errors.first() {
        Err(format!("agent error: {e}"))
    } else {
        (task.check)(&mut rig.app, &reply)
    };
    let mut routed: Vec<String> = turn.steps.iter().map(|s| s.model.clone()).filter(|m| !m.is_empty()).collect();
    routed.sort();
    routed.dedup();
    Ok(EvalRun {
        verdict,
        secs: turn.wall.unwrap_or_else(|| sent.elapsed()).as_secs_f64(),
        step_secs: turn.steps.iter().map(|s| s.ms as f64 / 1000.0).collect(),
        rejected: turn.steps.iter().map(|s| s.rejected).sum(),
        tools: turn.tools,
        routed,
        reply,
    })
}

/// Nearest-rank percentile of a non-empty sample (`q` in 0..=1).
fn eval_percentile(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn eval_median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 { 0.0 } else if n % 2 == 1 { xs[n / 2] } else { (xs[n / 2 - 1] + xs[n / 2]) / 2.0 }
}

/// `kabelsalat agent-eval [--model <id>] [--runs N] [--task <name>] [--no-jev]`:
/// run the real agent loop over the scripted tasks and report pass rate,
/// model steps, tool calls, wall time and the models that answered.
/// `--model` picks the acting chat model; `--no-jev` takes the decisions
/// model out of the loop.
pub fn run_agent_eval() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let model = opt("--model").unwrap_or_else(|| crate::llm::Route::configured(crate::llm::Role::Act).model);
    let runs: usize = opt("--runs").map(|r| r.parse()).transpose()?.unwrap_or(1).max(1);
    let only = opt("--task");
    let jev = !args.iter().any(|a| a == "--no-jev");
    let tasks: Vec<&EvalTask> = EVAL_TASKS.iter().filter(|t| only.as_deref().is_none_or(|o| o == t.name)).collect();
    anyhow::ensure!(!tasks.is_empty(), "no task named {:?} (tasks: {})", only.unwrap_or_default(),
        EVAL_TASKS.iter().map(|t| t.name).collect::<Vec<_>>().join(", "));
    // The frame path asks the compositor for its scale otherwise.
    if std::env::var_os("KABELSALAT_UI_SCALE").is_none() { std::env::set_var("KABELSALAT_UI_SCALE", "1.6"); }
    let plugins: EvalPlugins = (list_clap_plugins(), lv2_discover());

    let front = if jev { crate::llm::Route::configured(crate::llm::Role::Decide).model } else { "none".into() };
    println!("agent-eval: decisions {front} · acting model {model} · {} task(s) x {runs} run(s)", tasks.len());
    let mut results: Vec<(&EvalTask, Vec<EvalRun>)> = Vec::new();
    for task in tasks {
        let mut task_runs = Vec::with_capacity(runs);
        for _ in 0..runs {
            let run = run_eval_task(task, &model, jev, &plugins)?;
            let verdict = match &run.verdict { Ok(()) => "PASS".to_string(), Err(why) => format!("FAIL ({why})") };
            let steps: Vec<String> = run.step_secs.iter().map(|s| format!("{s:.1}")).collect();
            let rejected = if run.rejected > 0 { format!("  {} call(s) refused", run.rejected) } else { String::new() };
            println!("  {:<18} {:>6.2} s  {} step(s) [{} s]  {} tool(s) [{}]{rejected}  {}  {verdict}",
                task.name, run.secs, steps.len(), steps.join(" "), run.tools.len(), run.tools.join(", "), run.routed.join("+"));
            if run.verdict.is_err() {
                println!("    reply: {}", run.reply.replace('\n', " ").chars().take(240).collect::<String>());
            }
            task_runs.push(run);
        }
        results.push((task, task_runs));
    }

    println!("\n| task | pass | median s | p90 s | model s | steps | tools | answered by |");
    println!("|---|---|---|---|---|---|---|---|");
    let (mut passed, mut total, mut all_secs, mut all_model) = (0, 0, Vec::new(), 0.0);
    for (task, task_runs) in &results {
        let ok = task_runs.iter().filter(|r| r.verdict.is_ok()).count();
        let mut secs: Vec<f64> = task_runs.iter().map(|r| r.secs).collect();
        secs.sort_by(f64::total_cmp);
        let mut routed: Vec<String> = task_runs.iter().flat_map(|r| r.routed.clone()).collect();
        routed.sort();
        routed.dedup();
        println!("| {} | {ok}/{} | {:.2} | {:.2} | {:.2} | {:.0} | {:.0} | {} |",
            task.name, task_runs.len(), eval_median(secs.clone()), eval_percentile(&secs, 0.9),
            eval_median(task_runs.iter().map(|r| r.step_secs.iter().sum()).collect()),
            eval_median(task_runs.iter().map(|r| r.step_secs.len() as f64).collect()),
            eval_median(task_runs.iter().map(|r| r.tools.len() as f64).collect()),
            routed.join(", "));
        passed += ok;
        total += task_runs.len();
        all_secs.extend(secs);
        all_model += task_runs.iter().flat_map(|r| &r.step_secs).sum::<f64>();
    }
    all_secs.sort_by(f64::total_cmp);
    let wall: f64 = all_secs.iter().sum();
    println!("\nagent-eval: {model} passed {passed}/{total} ({:.0} %) · wall time per request: median {:.2} s, p90 {:.2} s · total {wall:.1} s, {:.0} % of it waiting for the model",
        100.0 * passed as f64 / total as f64, eval_median(all_secs.clone()), eval_percentile(&all_secs, 0.9), 100.0 * all_model / wall);
    Ok(())
}

pub fn run_demo() -> anyhow::Result<()> {
    run_inner(None, false)
}

/// Same demo, but every interactive key press is also delivered to `sink`.
/// This is the audio-integration seam (task-4): the UI already emits note
/// events here, so the engine can subscribe without touching the render path.
pub fn run_demo_with_note_sink(sink: Option<NoteSink>) -> anyhow::Result<()> {
    run_inner(sink, false)
}

/// The integrated app: rack UI + Surge XT audio + assistant, one window.
/// This is the default mode (`cargo run`).
pub fn run_app() -> anyhow::Result<()> {
    run_inner(None, true)
}

/// Prefer the native PipeWire host (bypasses the ALSA bridge and lets us
/// request a small quantum directly); fall back to the default host.
pub(crate) fn preferred_host() -> cpal::Host {
    for id in cpal::available_hosts() {
        if id.name().to_lowercase().contains("pipewire") {
            if let Ok(h) = cpal::host_from_id(id) {
                return h;
            }
        }
    }
    cpal::default_host()
}

/// Pin the current (audio render) thread to the fastest CPU cores on
/// heterogeneous systems (e.g. Apple M1 Firestorm P-cores vs Icestorm E-cores).
/// SCHED_RR otherwise happily parks the audio callback on a slower E-core.
fn pin_current_thread_to_p_cores() {
    #[cfg(target_os = "linux")]
    {
        let mut freqs: Vec<(usize, u64)> = Vec::new();
        for cpu in 0..libc::CPU_SETSIZE as usize {
            let p = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            let Ok(s) = std::fs::read_to_string(&p) else { continue };
            if let Ok(f) = s.trim().parse::<u64>() {
                freqs.push((cpu, f));
            }
        }
        let Some(max_f) = freqs.iter().map(|&(_, f)| f).max() else { return };
        let Some(min_f) = freqs.iter().map(|&(_, f)| f).min() else { return };
        if max_f <= min_f { return; } // homogeneous CPU
        let p_cores: Vec<usize> = freqs.iter().filter(|&&(_, f)| f == max_f).map(|&(c, _)| c).collect();
        if p_cores.is_empty() { return; }
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            for &c in &p_cores {
                libc::CPU_SET(c, &mut set);
            }
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 {
                eprintln!("audio: pinned render thread to P-cores {p_cores:?} ({} MHz)", max_f / 1000);
            }
        }
    }
}

/// Load Surge XT and open the audio stream. Called on the main thread inside
/// the eframe creator (CLAP main-thread rules). The voice itself moves into
/// the cpal callback; the controller stays available for the note sink.
struct BuiltAudio {
    mains: Vec<ClapVoiceMain>,
    controllers: Vec<VoiceController>,
    voice_ids: Vec<u64>,
    voice_tx: rtrb::Producer<(u32, ClapVoice, Vec<f32>)>,
    voice_retire_rx: rtrb::Consumer<ClapVoice>,
    stream: cpal::Stream,
    bus: CommandBus,
    level: Arc<std::sync::atomic::AtomicU32>,
    sample_rate: f64,
    player_ctrl: crate::player::PlayerControl,
    recorder: std::sync::Arc<crate::record::RecorderShared>,
    rig_snapshot: Arc<Mutex<crate::control::RigSnapshot>>,
    graph_pub: Arc<Mutex<crate::engine::GraphPub>>,
    edge_levels: Arc<EdgeLevels>,
    audio_in_tx: rtrb::Producer<(u32, rtrb::Consumer<f32>)>,
    audio_in_levels: Arc<AudioInLevels>,
}

fn build_audio() -> anyhow::Result<BuiltAudio> {
    let host = preferred_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let cfg = device.default_output_config()?;
    let sample_rate = cfg.sample_rate() as f64;
    crate::bridge::SAMPLE_RATE.store(sample_rate as u32, std::sync::atomic::Ordering::Relaxed);
    let ch = cfg.channels() as usize;
    // Instrument voice pool: the demo's two stacks (nodes 1 and 4) plus
    // spares for added rigs. Idle voices (unmapped) are not rendered.
    let mut voices = Vec::with_capacity(VOICE_POOL);
    let mut mains = Vec::with_capacity(VOICE_POOL);
    for i in 0..VOICE_POOL {
        let (v, m) = ClapVoice::new(sample_rate)?;
        let node = match i { 0 => 1, 1 => 4, _ => u32::MAX };
        voices.push((node, v));
        mains.push(m);
    }
    // Grab RT-safe param controllers BEFORE the voices move into the engine.
    let controllers: Vec<VoiceController> = voices.iter().map(|(_, v)| v.controller()).collect();
    let voice_ids: Vec<u64> = voices.iter().map(|(_, v)| v.voice_id()).collect();
    let mut config: cpal::StreamConfig = cfg.into();
    // 256 frames @48k = 5.3 ms. Holds under full CPU load (all cores busy:
    // 0 xruns, worst render 1.0 ms) because the render thread is promoted to
    // realtime below; at normal priority even 512 frames underran.
    config.buffer_size = cpal::BufferSize::Fixed(crate::bridge::BUFFER_FRAMES);
    eprintln!(
        "audio: host={} device={} rate={} ch={} buffer={} frames ({:.1} ms) voices={VOICE_POOL}",
        host.id().name(),
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        sample_rate,
        ch,
        crate::bridge::BUFFER_FRAMES,
        crate::bridge::BUFFER_FRAMES as f64 / sample_rate * 1000.0
    );
    let (bus, drain) = crate::control::command_bus();
    let mut engine = GraphEngine::new(
        Graph::demo_patch(),
        drain,
        sample_rate,
        voices,
    );
    let level = engine.level_handle();
    let edge_levels = engine.edge_levels_handle();
    // Multi-rig snapshot: engine publishes rig list/active on LoadRigs; the
    // bridge + UI read it (the audio render path never locks it).
    let rig_snapshot = Arc::new(Mutex::new(crate::control::RigSnapshot::default()));
    engine.set_rig_snapshot(rig_snapshot.clone());
    // Graph publication (MCP→UI live sync): the engine publishes the graph on
    // structural changes; the UI adopts it when it diverges from its mirror.
    let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::demo_patch(), rules: Vec::new(), ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    // System-audio capture: stream the default sink's monitor (e.g. YouTube)
    // into the engine, muted by default (gain 0). Started off the audio thread.
    if let Some(source) = crate::capture::SystemCapture::default_monitor() {
        match crate::capture::SystemCapture::start(&source, sample_rate as u32, ch) {
            Ok(cap) => {
                eprintln!("system audio: capturing {source} (muted; raise gain to mix in)");
                engine.set_system_capture(cap);
            }
            Err(e) => eprintln!("system audio: capture unavailable: {e}"),
        }
    }
    // File player (backing track): decode thread + ring; UI holds the control.
    let (player_sink, player_ctrl) = crate::player::start(sample_rate as u32, ch);
    engine.set_player_sink(player_sink);
    // Master recorder: writer thread + ring; engine holds the tap, the UI (and
    // the bridge via LIVE_RECORDER) hold the shared status.
    let (recorder_tap, recorder) = crate::record::start(crate::record::record_dir(), sample_rate as u32, 2);
    engine.set_recorder(recorder_tap);
    let _ = crate::record::LIVE_RECORDER.set(recorder.clone());
    let (voice_tx, voice_rx) = rtrb::RingBuffer::<(u32, ClapVoice, Vec<f32>)>::new(16);
    let (voice_retire_tx, voice_retire_rx) = rtrb::RingBuffer::<ClapVoice>::new(16);
    engine.set_voice_channels(voice_rx, voice_retire_tx);
    // Audio In (issue #28): the UI opens input captures off the audio thread
    // and hands each ring consumer to the engine through this side channel.
    let (audio_in_tx, audio_in_rx) = rtrb::RingBuffer::<(u32, rtrb::Consumer<f32>)>::new(64);
    engine.set_audio_in_channel(audio_in_rx);
    let audio_in_levels = engine.audio_in_levels_handle();
    engine.spawn_lv2_loader();
    let mut rt_tried = false;
    let mut _rt_handle: Option<audio_thread_priority::RtPriorityHandle> = None;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            // One-time realtime promotion via rtkit (first callback only; the
            // D-Bus round trip happens once, never in the steady state).
            // cpal does not promote pipewire-alsa (IOPLUG) streams, so the
            // render thread ran at normal priority and was preempted by the
            // UI for up to 26 ms (> the 10.7 ms buffer) → xruns.
            if !rt_tried {
                rt_tried = true;
                pin_current_thread_to_p_cores();
                if std::env::var_os("KABELSALAT_NO_RT").is_none() {
                    // Request 1s (clamped to rtkit's RTTimeUSecMax = 200ms) rather
                    // than one buffer (~5.3ms at 256/48kHz): Surge XT applies
                    // load_state inside process(), which exceeds 5.3ms and
                    // otherwise kills the SCHED_RR thread with SIGXCPU.
                    match audio_thread_priority::promote_current_thread_to_real_time(sample_rate as u32, sample_rate as u32) {
                        Ok(h) => { _rt_handle = Some(h); eprintln!("audio: render thread promoted to realtime (rtkit)"); }
                        Err(e) => eprintln!("audio: realtime promotion unavailable ({e}); running at normal priority"),
                    }
                }
            }
            let t0 = std::time::Instant::now();
            engine.render(data, ch);
            crate::bridge::note_render(t0.elapsed(), data.len() / ch.max(1), sample_rate);
        },
        |e| {
            crate::bridge::XRUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("audio stream error: {e}");
        },
        None,
    )?;
    stream.play()?;
    Ok(BuiltAudio {
        mains,
        controllers,
        voice_ids,
        voice_tx,
        voice_retire_rx,
        stream,
        bus,
        level,
        sample_rate,
        player_ctrl,
        recorder,
        rig_snapshot,
        graph_pub,
        edge_levels,
        audio_in_tx,
        audio_in_levels,
    })
}

/// UI scale (egui pixels_per_point): KABELSALAT_UI_SCALE overrides; else the
/// focused Hyprland monitor's scale (what every native app on this output
/// uses); else 1.6, the design target (1600x1000 points on a 2560x1600 panel).
/// Computed once — the hyprctl query never runs on the frame path again.
fn ui_scale_target() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        let valid = |s: f32| (1.0..=3.0).contains(&s);
        if let Some(s) = std::env::var("KABELSALAT_UI_SCALE").ok().and_then(|v| v.parse::<f32>().ok()).filter(|s| valid(*s)) {
            return s;
        }
        if cfg!(test) { return 1.6; }
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            let out = std::process::Command::new("hyprctl").args(["-j", "monitors"]).output();
            if let Ok(out) = out {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    let mons = v.as_array().cloned().unwrap_or_default();
                    let pick = mons.iter().find(|m| m["focused"].as_bool() == Some(true)).or(mons.first());
                    if let Some(s) = pick.and_then(|m| m["scale"].as_f64()).map(|s| s as f32).filter(|s| valid(*s)) {
                        return s;
                    }
                }
            }
        }
        1.6
    })
}

fn run_inner(sink: Option<NoteSink>, with_audio: bool) -> anyhow::Result<()> {
    // Run under XWayland, not native Wayland: (1) plugin editors are X11 and
    // must reparent into our window; (2) winit has no fractional-scale support,
    // so native Wayland on a 1.6x Hyprland output renders a logical-resolution
    // buffer that the compositor upscales (blurry). With Omarchy's
    // xwayland force_zero_scaling, X11 renders 1:1 at native pixels — sharp —
    // and egui's pixels_per_point handles element sizing.
    if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
        std::env::set_var("WINIT_UNIX_BACKEND", "x11");
    }
    // winit >= 0.29 REMOVED WINIT_UNIX_BACKEND: it picks Wayland whenever
    // WAYLAND_DISPLAY is set, else X11 via DISPLAY. To actually force X11
    // (required for editor embedding), hide the Wayland socket — but only
    // when an X display is available, so we never strand the app backend-less.
    let have_x = std::env::var_os("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false)
        || std::fs::read_dir("/tmp/.X11-unix").map(|mut d| d.next().is_some()).unwrap_or(false);
    if have_x && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
        eprintln!("ui: forcing X11 backend (WAYLAND_DISPLAY hidden; embedding needs XWayland)");
    }
    // Text input without an input-method server (#30): with XMODIFIERS
    // =@im=fcitx every typed character goes through fcitx5's XIM bridge, which
    // under XWayland stopped delivering text after a while — every text field
    // went dead while shortcuts (plain key events) kept working. The rack
    // needs no IME (umlauts come straight from the XKB layout), so use X's
    // local input method. KABELSALAT_IME=1 keeps the system IME.
    if std::env::var_os("KABELSALAT_IME").is_none() && std::env::var_os("XMODIFIERS").is_some() {
        std::env::remove_var("XMODIFIERS");
        eprintln!("ui: text input via the local X input method (XMODIFIERS cleared; KABELSALAT_IME=1 keeps fcitx/ibus)");
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_min_inner_size([760.0, 520.0])
            .with_maximized(true)
            .with_app_id("kabelsalat")
            .with_title("kabelsalat")
            // Crossed-wires brand mark (assets/logo.svg → icon-64.rgba, raw
            // RGBA8 so no image decoder is needed at runtime).
            .with_icon(egui::IconData {
                rgba: include_bytes!("../assets/icon-64.rgba").to_vec(),
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "kabelsalat",
        options,
        Box::new(move |cc| {
            // Trust eframe's detected scale (1.6 on the fractional-HiDPI eDP-1).
            // Only fall back to the native 1.6 if detection reports ~1.0 —
            // forcing a higher value makes elements oversized.
            let ppp = cc.egui_ctx.pixels_per_point();
            cc.egui_ctx.set_pixels_per_point(ui_scale_target());
            eprintln!(
                "ui scale: detected={ppp:.2} used={:.2}",
                cc.egui_ctx.pixels_per_point()
            );
            cc.egui_ctx.set_visuals(theme::visuals());
            theme::install_spacing(&cc.egui_ctx);
            theme::install_fonts(&cc.egui_ctx);

            let mut sink = sink;
            let mut audio = None;
            if with_audio {
                match build_audio() {
                    Ok(ba) => {
                        let BuiltAudio {
                            mains,
                            controllers,
                            voice_ids,
                            voice_tx,
                            voice_retire_rx,
                            stream,
                            bus,
                            level,
                            sample_rate,
                            player_ctrl,
                            recorder,
                            rig_snapshot,
                            graph_pub,
                            edge_levels,
                            audio_in_tx,
                            audio_in_levels,
                        } = ba;
                        let bus = Arc::new(Mutex::new(bus));
                        // Control bridge (task-16): MCP/CLI drive THIS app via
                        // the same bus + a live graph mirror. Additive — if the
                        // socket is unavailable the app just runs without it.
                        let shared_graph = Arc::new(Mutex::new(Graph::demo_patch()));
                        if let Err(e) = crate::bridge::spawn_with_state(
                            bus.clone(),
                            shared_graph.clone(),
                            level.clone(),
                            sample_rate,
                            rig_snapshot.clone(),
                            graph_pub.clone(),
                        ) {
                            eprintln!("bridge: control socket unavailable: {e:#}");
                        }
                        let b = bus.clone();
                        sink = Some(Box::new(move |ev: NoteEvent| {
                            // Notes flow through the control plane; the engine
                            // gates them on the Keys→Surge cable being plugged.
                            // ev.note is an absolute MIDI note (0..127).
                            let note = ev.note;
                            let cmd = if ev.velocity > 0.0 {
                                Command::NoteOn { note, velocity: ev.velocity, source: ev.source }
                            } else {
                                Command::NoteOff { note, source: ev.source }
                            };
                            let _ = b.lock().unwrap().send(cmd);
                        }));
                        audio = Some((
                            mains,
                            controllers,
                            voice_ids,
                            sample_rate,
                            voice_tx,
                            voice_retire_rx,
                            stream,
                            bus,
                            level,
                            shared_graph,
                            player_ctrl,
                            recorder,
                            rig_snapshot,
                            graph_pub,
                            edge_levels,
                            audio_in_tx,
                            audio_in_levels,
                        ));
                    }
                    Err(e) => eprintln!("Surge/audio unavailable, running silent: {e}"),
                }
            }
            let mut app = DemoApp::new_with_sink(sink);
            // Scan plugins in the background from the start, so the browser
            // opens instantly.
            app.start_plugin_scan(false);
            let app = match audio {
                Some((
                    mains,
                    controllers,
                    voice_ids,
                    sample_rate,
                    voice_tx,
                    voice_retire_rx,
                    stream,
                    bus,
                    level,
                    shared_graph,
                    player_ctrl,
                    recorder,
                    rig_snapshot,
                    graph_pub,
                    edge_levels,
                    audio_in_tx,
                    audio_in_levels,
                )) => {
                    app.shared_graph = shared_graph;
                    app.edge_levels = Some(edge_levels);
                    app.rig_snapshot = rig_snapshot;
                    app.graph_pub = Some(graph_pub);
                    app.player_ctrl = Some(player_ctrl);
                    app.recorder = Some(recorder);
                    app.audio_in_tx = Some(audio_in_tx);
                    app.audio_in_levels = Some(audio_in_levels);
                    app.with_audio(
                        mains,
                        controllers,
                        voice_ids,
                        sample_rate,
                        voice_tx,
                        voice_retire_rx,
                        stream,
                        bus,
                        level,
                    )
                }
                None => {
                    if std::env::args().any(|a| a == "--metro") {
                        app.add_metronome_to_active_rig();
                    }
                    if std::env::args().any(|a| a == "--demo-agent") {
                        app.assistant.key_set = true;
                        app.dock = DockView::Assistant;
                        app.demo_agent_hold = true;
                        app.exec_tool("set_split", &serde_json::json!({"point": 60}));
                        app.exec_tool("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.85}));
                        app.assistant.history.push(ChatLine {
                            role: "user".into(),
                            text: "Split the keyboard at C4 and level Rig A to 0.85.".into(),
                        });
                        app.assistant.history.push(ChatLine {
                            role: "assistant".into(),
                            text: "Split **Keys** at C4 and set **Rig A** volume to 0.85.".into(),
                        });
                    }
                    app
                }
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))
}

// ---------------------------------------------------------------------------
// Multi-rig UI tests: view switching is visual only — the merged
// graph, cables, and per-rig grouping must stay intact across switches.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod multi_rig_ui_tests {
    use super::*;

    fn two_rig_app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        app
    }

    #[test]
    fn multi_rig_demo_builds_two_tagged_rigs() {
        let app = two_rig_app();
        assert_eq!(app.rig_count(), 2);
        // Rig A: Keys, Surge, Delay, Master. Rig B: Keys, Surge, Master.
        assert_eq!(app.units.iter().filter(|u| u.rig == 0).count(), 4);
        assert_eq!(app.units.iter().filter(|u| u.rig == 1).count(), 3);
        assert_eq!(app.active_rig, 0);
        assert!(app.keys_unit.is_some() && app.master_unit.is_some());
    }

    #[test]
    fn each_rig_view_lays_out_only_its_own_stacks() {
        let mut app = two_rig_app();
        app.edit_rigs(None); // rig C: Keys → Surge → Master
        for rig in 0..3 {
            app.set_active_rig(rig);
            assert_eq!(app.stack_cols.len(), 1, "rig {rig}: one stack column, no gaps for hidden rigs");
            assert!(app.stack_cols.iter().flatten().all(|&u| app.units[u].rig as usize == rig));
        }
    }

    #[test]
    fn switching_rig_regroups_view_without_touching_cables() {
        let mut app = two_rig_app();
        let cables_before = app.cables.len();
        let keys0 = app.keys_unit.unwrap();
        let master0 = app.master_unit.unwrap();
        app.set_active_rig(1);
        assert_eq!(app.active_rig, 1);
        let keys1 = app.keys_unit.unwrap();
        assert_ne!(keys1, keys0); // rig B has its own Keys anchor
        assert!(app.visible(keys1));
        assert!(!app.visible(keys0));
        assert!(!app.visible(master0));
        // Hidden units keep no rect; all cables survive (every rig sounds on).
        assert_eq!(app.unit_rects[keys0], Rect::NOTHING);
        assert_eq!(app.cables.len(), cables_before);
        // Interaction state referencing the old view is cleared.
        assert!(app.selected.is_none() && app.pending.is_none());
    }

    #[test]
    fn rig_switch_clamps_to_range() {
        let mut app = two_rig_app();
        app.set_active_rig(99);
        assert_eq!(app.active_rig, 1);
        app.set_active_rig(0);
        assert_eq!(app.active_rig, 0);
    }

    #[test]
    fn single_graph_mode_shows_every_unit() {
        let app = DemoApp::new_with_sink(None); // default demo patch, all rig 0
        assert_eq!(app.rig_count(), 1);
        for ui in 0..app.units.len() {
            assert!(app.visible(ui));
        }
    }

    #[test]
    fn template_load_fades_in() {
        let mut app = DemoApp::new_with_sink(None);
        app.load_demo_patch(); // explicit load → begin_load_fade
        assert_eq!(app.load_fade, 0.0);
        app.update(0.1);
        assert!(app.load_fade > 0.0 && app.load_fade < 1.0);
        app.update(1.0);
        assert_eq!(app.load_fade, 1.0);
    }

    #[test]
    fn held_keys_drain_on_focus_loss() {
        let mut app = two_rig_app();
        // Simulate a held laptop key, then focus loss (had_kb_focus true →
        // the ui() drain path fires note-offs and empties the held set).
        app.handle_laptop_key(Key::A, true, false);
        assert_eq!(app.held_keys.len(), 1);
        app.current_source = SourceId::Keys;
        let held: Vec<_> = app.held_keys.drain(..).collect();
        for (_, semi) in held { app.fire_keys(semi, 0.0); }
        assert!(app.held_keys.is_empty());
    }

    #[test]
    fn focus_loss_drains_held_laptop_keys_as_keyboard_note_offs() {
        // Real ui() drain path: hold A in a focused frame, then lose focus.
        let events = Arc::new(Mutex::new(Vec::<NoteEvent>::new()));
        let log = events.clone();
        let mut app = DemoApp::new_with_sink(Some(Box::new(move |e| log.lock().unwrap().push(e))));
        app.load_multi_rig_demo();
        let ctx = egui::Context::default();
        let run = |app: &mut DemoApp, focused: bool, evs: Vec<egui::Event>| {
            let mut raw = egui::RawInput::default();
            raw.focused = focused;
            raw.events = evs;
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
        };
        let a = egui::Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        run(&mut app, true, vec![a]);
        assert_eq!(app.held_keys.len(), 1);
        // A hardware event in between must not re-tag the drain.
        app.current_source = SourceId::Hardware(3);
        run(&mut app, false, vec![]);
        assert!(app.held_keys.is_empty(), "focus loss drains held keys");
        let ev = events.lock().unwrap();
        let last = ev.last().expect("note-off emitted");
        assert_eq!(last.velocity, 0.0);
        assert_eq!(last.source, SourceId::Keys, "drain fires under the one Keyboard source");
        assert!(ev.iter().all(|e| e.source == SourceId::Keys));
    }
}

#[cfg(test)]
mod matrix_ui_tests {
    use super::*;

    #[test]
    fn clap_cell_state_table() {
        let all = (true, true);
        // Cable gate dominates everything.
        for row in [MatrixRow::Keys, MatrixRow::HwAll, MatrixRow::HwDev(0)] {
            assert!(!clap_cell(all, None, false, row));
        }
        // Defaults: keyboard/omni-hw on, specific device off.
        assert!(clap_cell(all, None, true, MatrixRow::Keys));
        assert!(clap_cell(all, None, true, MatrixRow::HwAll));
        assert!(!clap_cell(all, None, true, MatrixRow::HwDev(0)));
        // Device-bound rig: only that device.
        assert!(!clap_cell(all, Some(1), true, MatrixRow::HwAll));
        assert!(clap_cell(all, Some(1), true, MatrixRow::HwDev(1)));
        assert!(!clap_cell(all, Some(1), true, MatrixRow::HwDev(0)));
        // Hardware off.
        assert!(!clap_cell((true, false), None, true, MatrixRow::HwAll));
        assert!(!clap_cell((true, false), Some(0), true, MatrixRow::HwDev(0)));
        // Source flags independent.
        assert!(!clap_cell((false, true), None, true, MatrixRow::Keys));
        assert!(clap_cell((false, true), None, true, MatrixRow::HwAll));
    }

    fn demo_app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        app
    }

    /// Run one handle_input pass with the given key presses.
    fn press_keys(app: &mut DemoApp, keys: &[Key]) {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        for &key in keys {
            raw.events.push(egui::Event::Key {
                key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE,
            });
        }
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0));
        let mut out = ctx.run_ui(raw, |ui| {
            app.layout(rect);
            app.handle_input(ui.ctx(), rect);
        });
        out.textures_delta.clear();
    }

    #[test]
    fn tab_in_matrix_returns_to_rack_without_flipping() {
        let mut app = demo_app();
        app.stage_view = StageView::Matrix;
        let side = app.side;
        app.on_tab();
        assert_eq!(app.stage_view, StageView::Rack);
        assert!(!app.flipping && app.side == side, "Tab must not flip while leaving the matrix");
        app.on_tab();
        assert!(app.flipping, "Tab in rack mode flips");
    }

    #[test]
    fn escape_chain_closes_rule_editor_then_matrix() {
        let mut app = demo_app();
        let inst = app.matrix_sinks()[0];
        app.stage_view = StageView::Matrix;
        app.rule_editor = Some((inst, MatrixRow::Keys));
        press_keys(&mut app, &[Key::Escape]);
        assert!(app.rule_editor.is_none() && app.stage_view == StageView::Matrix, "first Esc closes the rule editor");
        press_keys(&mut app, &[Key::Escape]);
        assert_eq!(app.stage_view, StageView::Rack, "second Esc leaves the matrix");
        press_keys(&mut app, &[Key::F11]);
        assert_eq!(app.stage_view, StageView::Matrix, "F11 toggles the matrix stage");
    }

    #[test]
    fn matrix_sinks_are_the_instruments() {
        let app = demo_app();
        let sinks = app.matrix_sinks();
        assert_eq!(sinks.len(), 2); // one Surge per rig
        for s in sinks {
            assert!(matches!(app.units[s].node_kind, UnitKind::Clap));
        }
    }

    #[test]
    fn each_rig_has_its_own_keys_unit() {
        let app = demo_app();
        let k0 = app.keys_unit_for_rig(0).unwrap();
        let k1 = app.keys_unit_for_rig(1).unwrap();
        assert_ne!(k0, k1);
    }

    #[test]
    fn toggling_cell_on_hot_wires_missing_cable() {
        let mut app = demo_app();
        let inst = app.matrix_sinks()[0];
        // Unplug the rig's MIDI cable, then route Keys via the matrix.
        let ci = app.midi_cable_to(inst).expect("demo wires Keys→Surge");
        app.unplug_cable(ci, true);
        assert!(app.midi_cable_to(inst).is_none());
        app.matrix_toggle(inst, MatrixRow::Keys, true);
        assert!(app.midi_cable_to(inst).is_some(), "cell-on must hot-wire the cable");
        assert!(app.rig_sources[0].0);
    }

    #[test]
    fn hardware_rows_are_a_radio_group() {
        let mut app = demo_app();
        let inst = app.matrix_sinks()[0];
        // Bind device 0.
        app.matrix_toggle(inst, MatrixRow::HwDev(0), true);
        assert_eq!(app.rig_device[0], Some(0));
        assert!(app.rig_sources[0].1);
        // Switch to omni.
        app.matrix_toggle(inst, MatrixRow::HwAll, true);
        assert_eq!(app.rig_device[0], None);
        assert!(app.rig_sources[0].1);
        // Bind device 1, then click it off → hardware off.
        app.matrix_toggle(inst, MatrixRow::HwDev(1), true);
        assert_eq!(app.rig_device[0], Some(1));
        app.matrix_toggle(inst, MatrixRow::HwDev(1), false);
        assert!(!app.rig_sources[0].1);
        // The keyboard is untouched by the hw radio.
        assert!(app.rig_sources[0].0);
    }

    #[test]
    fn sync_adopts_remote_graph_and_skips_converged() {
        let mut app = DemoApp::new_with_sink(None);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 1, graph: app.base_graph.clone(), rules: Vec::new(), ..Default::default() }));
        app.graph_pub = Some(pub_.clone());
        // Engine graph == UI mirror → no rebuild (UI-originated edits converge).
        let units_before = app.units.len();
        app.sync_from_engine();
        assert_eq!(app.units.len(), units_before);
        assert_eq!(app.last_graph_rev, 1);
        assert_ne!(app.session_msg, "Patch updated from MCP/CLI");
        // Remote load: a different graph → adopt it.
        let mut remote = Graph::default();
        remote.reserve(4, 4);
        let k = remote.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let m = remote.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        remote.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
        {
            let mut p = pub_.lock().unwrap();
            p.rev = 2;
            p.graph = remote;
        }
        app.sync_from_engine();
        assert_eq!(app.units.len(), 2);
        assert_eq!(app.session_msg, "Patch updated from MCP/CLI");
        assert_eq!(app.last_graph_rev, 2);
    }

    #[test]
    fn sync_mirrors_remote_rule_edits() {
        let mut app = DemoApp::new_with_sink(None);
        let mut remote_rule = crate::matrix::Rule::new(SourceId::Keys, 1);
        remote_rule.transpose = -12;
        remote_rule.key_range = (0, 59);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub {
            rev: 1, graph: app.base_graph.clone(), rules: vec![remote_rule], ..Default::default()
        }));
        app.graph_pub = Some(pub_);
        app.sync_from_engine();
        let e = app.rule_edits.get(&(SourceId::Keys, 1)).expect("remote rule mirrored");
        assert_eq!(e.transpose, -12);
        assert_eq!(e.key_hi, 59);
    }

    #[test]
    fn set_matrix_rule_tool_shapes_route() {
        let mut app = demo_app();
        let out = app.exec_tool("set_matrix_rule", &serde_json::json!({"node": 1, "source": "keys", "key_hi": 59, "transpose": -12}));
        assert!(out.contains("-12 st"), "{out}");
        let e = app.rule_edits[&(SourceId::Keys, 1)].clone();
        assert_eq!(e.key_hi, 59);
        assert_eq!(e.transpose, -12);
        // get_matrix reports the edit.
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(v["rule_edits"].as_array().unwrap().len(), 1);
        // All-default values reset (remove) the edit.
        app.exec_tool("set_matrix_rule", &serde_json::json!({"node": 1, "source": "keys"}));
        assert!(!app.rule_edits.contains_key(&(SourceId::Keys, 1)));
    }

    #[test]
    fn get_matrix_tool_reports_state() {
        let mut app = demo_app();
        let out = app.exec_tool("get_matrix", &serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sinks"].as_array().unwrap().len(), 2);
        assert_eq!(v["sinks"][0]["keys"], serde_json::json!(true));
        // Hot-wire a cell through the assistant tool.
        let node = v["sinks"][1]["node"].as_u64().unwrap();
        let out = app.exec_tool("set_matrix_cell", &serde_json::json!({"node": node, "source": "keys", "on": false}));
        assert!(out.contains("= false"));
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(v["sinks"][1]["keys"], serde_json::json!(false));
        // One keyboard source: no separate laptop column/row in the report.
        assert!(v["sinks"][1].get("laptop").is_none());
        assert_eq!(v["sources"], serde_json::json!(["keys", "hw_all", "hw:<device index>"]));
        // "laptop" is accepted as the legacy spelling of the same row.
        app.exec_tool("set_matrix_cell", &serde_json::json!({"node": node, "source": "laptop", "on": true}));
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(v["sinks"][1]["keys"], serde_json::json!(true));
    }

    #[test]
    fn set_rig_source_tool_takes_pair_and_migrates_triple() {
        let mut app = demo_app();
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let node = app.units[app.matrix_sinks()[1]].node_id;
        let out = app.exec_tool("set_rig_source", &serde_json::json!({"node": node, "keyboard": false}));
        assert!(out.contains("keyboard=false hardware=true"), "{out}");
        assert_eq!(app.rig_sources[1], (false, true), "UI mirror follows the tool");
        assert!(matches!(drain.pop(), Some(Command::SetRigSource { keyboard: false, hardware: true, .. })));
        // Old triple: keyboard = keys || laptop.
        app.exec_tool("set_rig_source", &serde_json::json!({"node": node, "keys": false, "laptop": true, "hardware": false}));
        assert_eq!(app.rig_sources[1], (true, false));
        assert!(matches!(drain.pop(), Some(Command::SetRigSource { keyboard: true, hardware: false, .. })));
    }

    #[test]
    fn matrix_has_one_keyboard_row() {
        let app = demo_app();
        let rows = app.matrix_rows();
        let kb: Vec<_> = rows.iter().filter(|(r, _, _)| *r == MatrixRow::Keys).collect();
        assert_eq!(kb.len(), 1);
        assert_eq!(kb[0].1, "Keyboard");
        assert_eq!(rows.len(), 2 + app.hw_devices.len(), "Keyboard + HW all + one per device");
        assert!(rows.iter().all(|(_, l, _)| !l.to_lowercase().contains("laptop")));
    }

    /// Every text galley painted in a frame, with its screen rect.
    fn painted_texts(out: &egui::FullOutput) -> Vec<(String, Rect)> {
        fn walk(s: &egui::Shape, acc: &mut Vec<(String, Rect)>) {
            match s {
                egui::Shape::Text(t) => acc.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Vec(v) => for x in v { walk(x, acc) },
                _ => {}
            }
        }
        let mut acc = Vec::new();
        for cs in &out.shapes { walk(&cs.shape, &mut acc); }
        acc
    }

    fn panel_frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0)));
        raw.events = events;
        let mut out = ctx.run_ui(raw, |ui| {
            app.rig_inputs_panel(ui);
            egui::CentralPanel::default().show(ui, |ui| app.matrix_grid_ui(ui));
        });
        let texts = painted_texts(&out);
        out.textures_delta.clear();
        texts
    }

    #[test]
    fn top_bar_shows_the_brand_and_the_matrix_owns_rig_sources() {
        let mut app = demo_app();
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let ctx = egui::Context::default();
        panel_frame(&mut app, &ctx, vec![]); // fonts ready
        app.ensure_galleys(&ctx);            // as frame_ui does
        let texts = panel_frame(&mut app, &ctx, vec![]);
        let count = |t: &str| texts.iter().filter(|(s, _)| s == t).count();
        // User 2026-11-15: no RIG INPUTS chips in the top bar — the brand sits there.
        assert_eq!(count("K") + count("H"), 0, "no K/H chips: {texts:?}");
        assert!(texts.iter().any(|(s, _)| s == "k"), "wordmark painted (tracked per-char): {texts:?}");
        assert!(texts.iter().all(|(s, _)| !s.contains("Laptop") && !s.contains("laptop")));
        // The matrix keeps ONE Keyboard row (its rig-A cell is the old K chip);
        // the top bar adds a "Keyboard" toggle for the bottom software keyboard.
        assert_eq!(count("Keyboard"), 2);
        let cell = texts.iter().find(|(s, _)| s == "ALL").expect("rig A keyboard cell").1.center();
        let btn = |pressed| egui::Event::PointerButton { pos: cell, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        panel_frame(&mut app, &ctx, vec![egui::Event::PointerMoved(cell), btn(true)]);
        panel_frame(&mut app, &ctx, vec![btn(false)]);
        assert_eq!(app.rig_sources[0], (false, true), "keyboard off for rig A");
        let mut sent = Vec::new();
        while let Some(c) = drain.pop() { sent.push(c); }
        assert!(sent.iter().any(|c| matches!(c, Command::SetRigSource { keyboard: false, hardware: true, .. })), "{sent:?}");
    }

    #[test]
    fn laptop_typing_and_onscreen_clicks_light_the_same_keyboard_row() {
        let events = Arc::new(Mutex::new(Vec::<NoteEvent>::new()));
        let log = events.clone();
        let mut app = DemoApp::new_with_sink(Some(Box::new(move |e| log.lock().unwrap().push(e))));
        app.load_multi_rig_demo();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0));
        let ctx = egui::Context::default();
        let frame = |app: &mut DemoApp, evs: Vec<egui::Event>| {
            let mut raw = egui::RawInput::default();
            raw.events = evs;
            let mut out = ctx.run_ui(raw, |ui| {
                app.layout(rect);
                app.handle_input(ui.ctx(), rect);
            });
            out.textures_delta.clear();
            app.update(1.0 / 60.0);
        };
        frame(&mut app, vec![]);
        assert_eq!(app.keyboard_led(), 0.0);
        // Laptop typing.
        app.handle_laptop_key(Key::A, true, false);
        assert!(app.keyboard_led() > 0.5, "laptop typing lights the Keyboard row");
        app.handle_laptop_key(Key::A, false, false);
        for _ in 0..120 { app.update(1.0 / 30.0); }
        assert!(app.keyboard_led() < 0.05, "LED decayed");
        // On-screen click (the dock keyboard's click path).
        app.click_note(60 + WHITE_SEMI[0]);
        assert!(app.keyboard_led() > 0.5, "on-screen click lights the same row");
        frame(&mut app, vec![]);
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| e.velocity > 0.0 && e.note == 60 + WHITE_SEMI[0] as u8), "click fired a note");
        assert!(ev.len() >= 3);
        assert!(ev.iter().all(|e| e.source == SourceId::Keys), "both paths are the one Keyboard source");
    }
}
#[cfg(test)]
mod cable_replug_tests {
    use super::*;
    use crate::control::command_bus;

    const SCREEN: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 1280.0, y: 800.0 } };

    /// One real app frame: input through a live egui::Context, then the same
    /// update → publish order as `ui()`.
    fn frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>, modifiers: Modifiers) {
        let mut raw = egui::RawInput::default();
        raw.events = std::iter::once(egui::Event::ModifiersChanged(modifiers)).chain(events).collect();
        let mut out = ctx.run_ui(raw, |ui| {
            app.layout(SCREEN);
            app.handle_input(ui.ctx(), SCREEN);
        });
        out.textures_delta.clear();
        app.update(1.0 / 20.0);
        app.publish_patch();
    }

    fn button(pos: Pos2, pressed: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers }
    }

    /// Press at `from`, drag to `to` (holding `hold` frames midway), release.
    fn drag(app: &mut DemoApp, ctx: &egui::Context, from: Pos2, to: Pos2, hold: usize, m: Modifiers) {
        frame(app, ctx, vec![egui::Event::PointerMoved(from), button(from, true, m)], m);
        let mid = from + (to - from) * 0.5;
        for _ in 0..=hold { frame(app, ctx, vec![egui::Event::PointerMoved(mid)], m); }
        frame(app, ctx, vec![egui::Event::PointerMoved(to)], m);
        frame(app, ctx, vec![button(to, false, m)], m);
    }

    fn back_app() -> (DemoApp, egui::Context) {
        let mut app = DemoApp::new_with_sink(None);
        app.side = Side::Back;
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![], Modifiers::NONE); // settle layout
        (app, ctx)
    }

    fn jack_screen(app: &DemoApp, unit: usize, jack: usize) -> Pos2 {
        app.xf.p(app.jack_pos(unit, jack))
    }

    /// A living cable (index ≥ 1, so an earlier cable can die and shift
    /// indices) and a compatible free In jack to move its In plug onto.
    fn pick_replug(app: &DemoApp) -> (u32, usize, usize, Pos2, Pos2) {
        for c in app.cables.iter().skip(1) {
            if c.dying >= 0.0 { continue; }
            let origin_ji = app.units[c.dst].jack_index(Dir::In, c.dst_port).unwrap();
            let from = jack_screen(app, c.dst, origin_ji);
            for (ui, u) in app.units.iter().enumerate() {
                if ui == c.src || !app.visible(ui) { continue; }
                for (ji, j) in u.jacks.iter().enumerate() {
                    if j.dir != Dir::In || u.kind(Dir::In, j.port) != c.kind { continue; }
                    if (ui, j.port) == (c.dst, c.dst_port) { continue; }
                    if app.cable_at_jack(ui, Dir::In, j.port).is_some() { continue; }
                    let to = jack_screen(app, ui, ji);
                    if (to - from).length() < 80.0 { continue; }
                    if !matches!(app.hit_test(to), Hit::Jack { unit, jack } if unit == ui && jack == ji) { continue; }
                    return (c.id, ui, j.port, from, to);
                }
            }
        }
        panic!("demo patch has no re-pluggable cable");
    }

    #[test]
    fn dropping_a_plug_on_a_compatible_jack_moves_the_cable() {
        let (mut app, ctx) = back_app();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        // The engine side: a real GraphEngine fed what the UI sends.
        let (mut ebus, edrain) = command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::default(), rules: Vec::new(), ..Default::default() }));
        engine.set_graph_pub(pub_.clone());

        let (old, target, tport, from, to) = pick_replug(&app);
        let (src, src_port) = { let c = &app.cables[app.live_cable(old).unwrap()]; (c.src, c.src_port) };
        let alive_before = app.cables.iter().filter(|c| c.dying < 0.0).count();
        // Rule 10: an earlier cable dies and is retained away MID-gesture,
        // shifting every later index while the plug is held.
        let victim = app.cables[0].id;
        assert_ne!(victim, old);
        app.unplug_cable(0, true);
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c)); // drop the pre-gesture publish
        sent.clear();

        drag(&mut app, &ctx, from, to, 14, Modifiers::NONE);
        assert!(app.cables.iter().all(|c| c.id != victim), "victim fell away mid-gesture");

        // UI: old cable gone (no fall), one new living cable on the target.
        assert!(app.cables.iter().all(|c| c.id != old), "re-plugged cable must not linger");
        let moved: Vec<_> = app.cables.iter()
            .filter(|c| c.dying < 0.0 && c.dst == target && c.dst_port == tport)
            .collect();
        assert_eq!(moved.len(), 1, "exactly one living cable on the target jack");
        assert_eq!((moved[0].src, moved[0].src_port), (src, src_port), "the source end stays put");
        let new = moved[0].id;
        assert_eq!(app.cables.iter().filter(|c| c.dying < 0.0).count(), alive_before - 1);

        // Bus: Disconnect(old) strictly before Connect(new); old never re-sent after.
        drain.drain(|c| sent.push(c));
        let disc = sent.iter().position(|c| matches!(c, Command::Disconnect(id) if *id == old)).expect("Disconnect(old)");
        let conn = sent.iter().position(|c| matches!(c, Command::Connect { id, .. } if *id == new)).expect("Connect(new)");
        assert!(disc < conn, "Disconnect(old) must precede Connect(new)");
        assert!(!sent[disc..].iter().any(|c| matches!(c, Command::Connect { id, .. } if *id == old)));

        // Engine: graph has the new edge, not the old one.
        for c in sent { ebus.send(c); }
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        let g = pub_.lock().unwrap().graph.clone();
        assert!(!g.edges.iter().any(|e| e.id == old), "engine dropped the old route");
        let e = g.edges.iter().find(|e| e.id == new).expect("engine has the new route");
        assert_eq!(e.to.node, app.units[target].node_id);
        assert_eq!(e.to.port as usize, tport);
        assert_eq!(e.from.node, app.units[src].node_id);
        // UI mirror agrees.
        assert!(app.shared_graph.lock().unwrap().edges.iter().any(|e| e.id == new));
    }

    #[test]
    fn plain_click_on_a_plug_still_unplugs() {
        let (mut app, ctx) = back_app();
        let (old, _, _, from, _) = pick_replug(&app);
        frame(&mut app, &ctx, vec![egui::Event::PointerMoved(from), button(from, true, Modifiers::NONE)], Modifiers::NONE);
        frame(&mut app, &ctx, vec![button(from, false, Modifiers::NONE)], Modifiers::NONE);
        assert!(app.live_cable(old).is_none(), "click unplugs");
    }

    #[test]
    fn dropping_on_an_incompatible_spot_far_away_unplugs() {
        let (mut app, ctx) = back_app();
        let (old, _, _, from, _) = pick_replug(&app);
        // Empty paper far below the rack.
        let to = Pos2::new(from.x, SCREEN.max.y - 4.0);
        drag(&mut app, &ctx, from, to, 1, Modifiers::NONE);
        assert!(app.live_cable(old).is_none(), "tug off unplugs");
    }

    #[test]
    fn duplicate_replug_is_refused_with_a_notice_and_keeps_the_cable() {
        let (mut app, _ctx) = back_app();
        let (old, target, tport, _, _) = pick_replug(&app);
        let (src, sp, kind) = { let c = &app.cables[app.live_cable(old).unwrap()]; (c.src, c.src_port, c.kind) };
        // Occupy the target with the exact endpoints the move would create.
        assert!(app.add_cable(src, sp, target, tport, kind, 0).is_some());
        assert_eq!(app.replug_cable(old, Dir::In, target, Dir::In, tport), Replug::Refused);
        assert!(app.live_cable(old).is_some(), "refused move keeps the cable");
        assert!(app.patch_notice.is_some(), "refusal is surfaced, not silent");
        // Wrong direction is incompatible (falls back to unplug/snap-back).
        assert_eq!(app.replug_cable(old, Dir::In, src, Dir::Out, sp), Replug::Incompatible);
    }

    #[test]
    fn shift_drag_from_an_occupied_output_fans_out() {
        let (mut app, ctx) = back_app();
        let (old, target, tport, _, to) = pick_replug(&app);
        let (src, sp) = { let c = &app.cables[app.live_cable(old).unwrap()]; (c.src, c.src_port) };
        let out_ji = app.units[src].jack_index(Dir::Out, sp).unwrap();
        let from = jack_screen(&app, src, out_ji);
        drag(&mut app, &ctx, from, to, 1, Modifiers::SHIFT);
        assert!(app.live_cable(old).is_some(), "the original cable stays");
        assert!(app.cables.iter().any(|c| c.dying < 0.0 && c.src == src && c.src_port == sp && c.dst == target && c.dst_port == tport),
            "Shift+drag adds a second cable from the same output");
    }
}

#[cfg(test)]
mod rig_switch_tests {
    use super::*;

    /// One full app frame (`frame_ui`, the body of `eframe::App::ui`) through a
    /// live egui::Context, with the same raw-input hook eframe runs.
    fn frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
    }

    fn key(key: Key) -> Vec<egui::Event> {
        vec![
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ]
    }

    fn click(pos: Pos2) -> Vec<Vec<egui::Event>> {
        let b = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![vec![egui::Event::PointerMoved(pos)], vec![b(true)], vec![b(false)]]
    }

    fn two_rigs() -> (DemoApp, egui::Context) {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        let ctx = egui::Context::default();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.active_rig, 0);
        (app, ctx)
    }

    #[test]
    fn assistant_panel_does_not_squish_the_rack() {
        let (mut app, ctx) = two_rigs();
        // A 3-unit stack (Surge → Delay → Reverb) is the demanding case.
        let rows_before = app.layout_rows;
        let free = app.unit_h;
        app.assistant.key_set = true;
        app.dock = DockView::Assistant;
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        assert_eq!(app.layout_rows, rows_before);
        assert!(app.unit_h >= 140.0, "faces stay comfortable with the assistant open: {} (free: {free})", app.unit_h);
        assert_eq!(app.rack_overflow, Vec2::ZERO,
            "a demo-sized rack fits beside the assistant (no pan bar): stage {:?} rack_h {} unit_h {}", app.stage_rect, app.rack_h, app.unit_h);
        assert!(app.stage_rect.bottom() <= app.assistant_rect.top() + 0.5,
            "the stage ends where the assistant begins: stage {:?} panel {:?}", app.stage_rect, app.assistant_rect);
    }

    /// One rig with 6 instrument stacks of 4 units (Surge → 3 FX) — the
    /// kind of patch an agent builds; it must not squish.
    fn big_patch_app() -> (DemoApp, egui::Context) {
        let mut g = Graph::default();
        g.reserve(64, 64);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut eid = 1;
        for _ in 0..6 {
            let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_ID) }, name_bytes("SURGE"));
            g.connect_with_id(eid, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, CPortKind::Midi); eid += 1;
            let mut prev = synth;
            for k in [DspKind::Filter, DspKind::Delay, DspKind::Reverb] {
                let fx = g.add_node(NodeKind::Dsp(k), name_bytes("FX"));
                g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: fx, port: 0 }, CPortKind::Audio); eid += 1;
                prev = fx;
            }
            g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio); eid += 1;
        }
        let mut app = DemoApp::new_with_sink(None);
        app.apply_session(&crate::session::Session::new("big", g)).unwrap();
        let ctx = egui::Context::default();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        (app, ctx)
    }

    #[test]
    fn big_patch_keeps_readable_units_and_pans() {
        let (mut app, ctx) = big_patch_app();
        assert_eq!(app.stack_cols.len(), 6);
        assert_eq!(app.layout_rows, 4);
        assert!(app.unit_h >= 140.0, "unit height floor, not squished: {}", app.unit_h);
        let mid_w = app.unit_rects[app.stack_cols[0][0]].width();
        assert!(mid_w >= 200.0, "column width floor: {mid_w}");
        let ov = app.rack_overflow;
        assert!(ov.x > 0.0 && ov.y > 0.0, "the rack overflows the stage instead: {ov:?}");
        assert!(app.rack_clip.is_some());
        // Wheel over the bare stage pans (clamped to the overflow).
        let over = app.stage_rect.center();
        let before = app.xf.center;
        for _ in 0..12 {
            frame(&mut app, &ctx, vec![
                egui::Event::PointerMoved(over),
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(-60.0, -60.0),
                    phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: Modifiers::NONE },
            ]);
        }
        let moved = app.xf.center - before;
        assert!(moved.x < -1.0, "rack panned sideways: {moved:?}");
        assert!((moved.y + ov.y / 2.0).abs() < 0.5, "vertical pan goes exactly to its clamp: {moved:?} ov {ov:?}");
        assert!(app.rack_pan.x >= -ov.x / 2.0 - 0.5 && app.rack_pan.y >= -ov.y / 2.0 - 0.5, "pan stays clamped");
        // Nothing outside the clip is hittable.
        let outside = Pos2::new(app.stage_rect.center().x, app.stage_rect.bottom() - 2.0);
        assert!(matches!(app.hit_test(outside), Hit::Background));
    }

    #[test]
    fn small_patch_does_not_overflow() {
        let (app, _ctx) = two_rigs();
        assert_eq!(app.rack_overflow, Vec2::ZERO, "the demo fits: no pan, no clip");
        assert!(app.rack_clip.is_none());
    }

    #[test]
    fn plus_rig_with_a_live_engine_keeps_view_and_knobs() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (mut ebus, edrain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::default(), rules: Vec::new(), ..Default::default() }));
        engine.set_graph_pub(pub_.clone());
        engine.set_rig_snapshot(app.rig_snapshot.clone());
        app.graph_pub = Some(pub_);
        let mut pump = |drain: &mut crate::control::CommandDrain| {
            drain.drain(|c| { ebus.send(c); });
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
        };
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
        let (u, k) = app.units.iter().enumerate()
            .find_map(|(i, u)| u.knobs.iter().position(|k| k.param.is_some()).map(|k| (i, k))).unwrap();
        app.units[u].knobs[k].value = 0.123;
        app.edit_rigs(None);
        for _ in 0..4 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
        assert_ne!(app.session_msg, "Patch updated from MCP/CLI", "the engine echo must not rebuild the rack");
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.active_rig, 1, "view stays on the new rig");
        assert!((app.units[u].knobs[k].value - 0.123).abs() < 1e-6, "knobs survive the engine echo");
    }

    #[test]
    fn matrix_rules_and_cc_bindings_survive_a_session_round_trip() {
        // Live app + engine; returns the engine's published rules after `f`.
        fn live(f: impl FnOnce(&mut DemoApp)) -> (DemoApp, Vec<crate::matrix::Rule>) {
            let mut app = DemoApp::new_with_sink(None);
            let ctx = egui::Context::default();
            let (tx, mut drain) = crate::control::command_bus();
            app.bus = Some(Arc::new(Mutex::new(tx)));
            let (mut ebus, edrain) = crate::control::command_bus();
            let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
            let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
            engine.set_graph_pub(pub_.clone());
            app.graph_pub = Some(pub_.clone());
            let mut pump = |drain: &mut crate::control::CommandDrain| {
                drain.drain(|c| { ebus.send(c); });
                let mut buf = vec![0.0f32; 64 * 2];
                engine.render(&mut buf, 2);
            };
            for _ in 0..2 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
            f(&mut app);
            for _ in 0..3 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
            let rules = pub_.lock().unwrap().rules.clone();
            (app, rules)
        }
        let dev = SourceId::Hardware(20);
        let (mut app, _) = live(|app| {
            if let Some(bus) = &app.bus {
                let _ = bus.lock().unwrap().send(Command::MatrixRule {
                    source: SourceId::Keys, dest: 1, channel: None, transpose: 12,
                    key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: true,
                });
            }
            app.set_cc_binding(SourceId::Keys, 1, 74, Some(42), crate::matrix::CcTarget::PluginParam);
            app.set_cc_binding(dev, 1, 7, Some(43), crate::matrix::CcTarget::PluginParam); // learn on an unrouted device
            app.set_cc_binding(dev, 1, 14, Some(P_LEVEL), crate::matrix::CcTarget::Knob);
        });
        let saved = app.capture_session("cc").unwrap();
        let json = serde_json::to_string(&saved.matrix()).unwrap();
        let back: Option<serde_json::Value> = serde_json::from_str(&json).unwrap();
        let mut reloaded = crate::session::Session::new("cc", saved.graph.clone());
        reloaded.set_matrix(back);

        let (fresh, rules) = live(|app| app.apply_session(&reloaded).unwrap());
        let keys = rules.iter().find(|r| r.source == SourceId::Keys && r.dest == 1).expect("Keys rule restored");
        assert_eq!(keys.transpose, 12);
        assert_eq!(keys.param_for_cc(74), Some(42));
        let learned = rules.iter().find(|r| r.source == dev && r.dest == 1).expect("learned rule restored");
        assert!(learned.cc_only, "a learn-only rule must stay CC-only after reload");
        assert_eq!(learned.param_for_cc(7), Some(43));
        assert_eq!(learned.mapping_for_cc(14), Some(crate::matrix::CcMapping { cc: 14, param: P_LEVEL, target: crate::matrix::CcTarget::Knob }));
        assert!(fresh.rule_edits.contains_key(&(dev, 1)), "UI mirror shows the restored binding");
    }

    #[test]
    fn a_multi_rig_session_with_an_id_gap_keeps_routes_on_their_synth() {
        let mut demo_app = DemoApp::new_with_sink(None);
        demo_app.load_multi_rig_demo();
        let mut s = demo_app.capture_session("gap").unwrap();
        // Remove a rig-A unit: rig B's ids shift in the engine's LoadRigs layout.
        let gap = s.graph.nodes.iter()
            .find(|n| n.rig == 0 && !matches!(n.kind, NodeKind::Keys | NodeKind::MasterOut | NodeKind::Clap { .. }))
            .map(|n| n.id).expect("a removable rig-A unit");
        s.graph.remove_node(gap);
        let surge_b = s.graph.nodes.iter()
            .find(|n| n.rig == 1 && matches!(n.kind, NodeKind::Clap { .. })).unwrap().id;
        let rule = SavedRule { source: SourceId::Keys, dest: surge_b, edit: RuleEdit { transpose: 7, ..RuleEdit::default() } };
        s.set_matrix(Some(serde_json::to_value(vec![rule]).unwrap()));

        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (mut ebus, edrain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
        engine.set_graph_pub(pub_.clone());
        engine.set_rig_snapshot(app.rig_snapshot.clone());
        app.graph_pub = Some(pub_.clone());
        let mut pump = |drain: &mut crate::control::CommandDrain| {
            drain.drain(|c| { ebus.send(c); });
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
        };
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
        app.apply_session(&s).unwrap();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); pump(&mut drain); }
        let g = pub_.lock().unwrap();
        let r = g.rules.iter().find(|r| r.transpose == 7).expect("route restored");
        let dest = g.graph.node(r.dest).expect("route points at a live node");
        assert!(matches!(dest.kind, NodeKind::Clap { .. }) && dest.rig == 1, "route landed on {:?} (rig {})", dest.kind, dest.rig);
        assert!(app.units.iter().any(|u| u.node_id == r.dest && matches!(u.node_kind, UnitKind::Clap)), "UI and engine agree on the synth's id");
        assert_ne!(app.session_msg, "Patch updated from MCP/CLI", "no divergence rebuild after loading");
    }

    #[test]
    fn add_rig_with_a_plugin_id_builds_that_plugins_rig_next_to_the_others() {
        let mut app = DemoApp::new_with_sink(None);
        app.plugins_clap = vec![ClapPluginInfo { id: "org.example.synth".into(), name: "Example".into(), vendor: "V".into(), features: vec!["instrument".into()] }];
        app.plugins_loaded = true; // the background scan has finished (#15: tools never scan inline)
        let before = app.rig_count();
        let r = app.exec_tool("add_rig", &serde_json::json!({"plugin_id": "org.example.synth"}));
        assert_eq!(app.rig_count(), before + 1, "{r}");
        let rig = (app.rig_count() - 1) as u8;
        let synth = app.base_graph.nodes.iter()
            .find(|n| n.rig == rig && matches!(n.kind, NodeKind::Clap { .. }))
            .expect("the new rig has a CLAP synth");
        let NodeKind::Clap { plugin_id } = &synth.kind else { unreachable!() };
        assert_eq!(name_str(plugin_id), "org.example.synth");
        assert!(app.base_graph.nodes.iter().any(|n| n.rig == 0 && matches!(n.kind, NodeKind::Clap { .. })), "rig A keeps its synth");
        let bad = app.exec_tool("add_rig", &serde_json::json!({"plugin_id": "org.not.installed"}));
        assert!(bad.contains("no installed CLAP plugin"), "{bad}");
        assert_eq!(app.rig_count(), before + 1, "an unknown plugin adds nothing");
    }

    #[test]
    fn a_saved_multi_rig_session_loads_as_rigs() {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        let s = app.capture_session("two").unwrap();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        app.apply_session(&s).unwrap();
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c));
        assert!(sent.iter().any(|c| matches!(c, Command::LoadRigs { rigs, .. } if rigs.len() == 2)),
            "a rig-tagged session reloads as LoadRigs, not LoadGraph");
        assert!(!sent.iter().any(|c| matches!(c, Command::LoadGraph { .. })));
        assert_eq!(app.rig_count(), 2);
    }

    #[test]
    fn assistant_chat_content_stays_inside_its_panel() {
        let (mut app, ctx) = two_rigs();
        app.assistant.key_set = true; // chat layout (history + input row)
        app.dock = DockView::Assistant;
        // The activity header shows the last request's cost line too.
        app.assistant.turn.steps.push(crate::assistant::StepStat { ms: 1200, model: "anthropic/claude-opus-5.5".into(), attempts: 1, rejected: 0 });
        // A short window clamps the panel to its 150px minimum — the case
        // where a fixed input-row reserve overflowed the panel's top edge.
        for _ in 0..4 {
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 640.0)));
            eframe::App::raw_input_hook(&mut app, &ctx, &mut raw);
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
        }
        assert!(app.assistant_rect.height() < 200.0, "panel at its minimum: {:?}", app.assistant_rect);
        let panel = app.assistant_rect;
        let inside: Vec<Rect> = ctx.viewport(|v| v.prev_pass.widgets.get_layer(egui::LayerId::background())
            .filter(|w| w.rect.center().y > panel.top() - 60.0 && w.rect.width() < panel.width() - 4.0)
            .map(|w| w.rect).collect());
        assert!(!inside.is_empty());
        for r in inside {
            assert!(r.top() >= panel.top() - 0.5 && r.bottom() <= panel.bottom() + 0.5,
                "widget {r:?} sticks out of the assistant panel {panel:?}");
        }
    }

    #[test]
    fn silent_meters_light_nothing() {
        // Master meter in dB: −12 dBFS sits at 80 %, not at a linear 25 %.
        assert!((meter_db_pos(10f32.powf(-12.0 / 20.0)) - 0.8).abs() < 1e-4);
        assert!((meter_db_pos(1.0) - 1.0).abs() < 1e-6 && meter_db_pos(2.0) == 1.0);
        assert_eq!(meter_db_pos(0.0005), 0.0, "below −60 dB is dark");
        assert_eq!(meter_db_pos(f32::NAN), 0.0);
        assert!(lit_segments(meter_db_pos(0.25), 14) >= 11, "a healthy −12 dBFS lights most of the meter");
        assert_eq!(lit_segments(0.0, 14), 0);
        assert_eq!(lit_segments(1e-9, 14), 0, "a decaying tail is silence");
        assert_eq!(lit_segments(0.0009, 14), 0);
        assert_eq!(lit_segments(0.01, 14), 1, "real signal lights the first segment");
        assert_eq!(lit_segments(0.5, 14), 7);
        assert_eq!(lit_segments(3.0, 14), 14, "clipped input stays in range");
    }

    #[test]
    fn hardware_notes_reach_the_engine_without_a_ui_frame() {
        let mut app = DemoApp::new_with_sink(None);
        let (tx, mut drain) = crate::control::command_bus();
        let bus = Arc::new(Mutex::new(tx));
        if let Ok(mut b) = app.hw_direct.bus.lock() { *b = Some(bus.clone()); }
        app.hw_direct.set_keys_vel(0.0); // a real 0.0 knob, not "unset"
        let on = NoteEvent { note: 60, velocity: 1.0, source: SourceId::Hardware(2), created: std::time::Instant::now() };
        assert!(app.hw_direct.send(&on), "sounded on the MIDI thread");
        let off = NoteEvent { velocity: 0.0, ..on };
        assert!(app.hw_direct.send(&off));
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c));
        assert!(matches!(sent[0], Command::NoteOn { note: 60, source: SourceId::Hardware(2), velocity } if (velocity - 0.35).abs() < 1e-6),
            "VEL 0.0 scales to 0.35 like the UI path: {:?}", sent[0]);
        assert!(matches!(sent[1], Command::NoteOff { note: 60, .. }));
        // The UI frame then only animates it — no second NoteOn.
        app.bus = Some(bus);
        app.hw_queue.lock().unwrap().push_back((on, true));
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        let mut again = Vec::new();
        drain.drain(|c| again.push(c));
        assert!(!again.iter().any(|c| matches!(c, Command::NoteOn { .. })), "not sounded twice: {again:?}");
        assert!(app.key_leds[0] > 0.3, "but the key still lights (after one frame of decay): {}", app.key_leds[0]);
    }

    #[test]
    fn keyboard_loaded_rigs_keep_finite_visible_cables() {
        // F4 runs in handle_input AFTER this frame's layout: build_rack's
        // unlaid rects must not poison the cable springs with NaN.
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); }
        frame(&mut app, &ctx, key(Key::F4));
        assert_eq!(app.rig_count(), 2, "F4 loaded the two-rig demo");
        app.side = Side::Back;
        let finite = |c: &Cable| c.curve.iter().all(|p| p.x.is_finite() && p.y.is_finite());
        for rig in [0usize, 1, 0] {
            app.set_active_rig(rig);
            for _ in 0..6 { frame(&mut app, &ctx, vec![]); }
            let vis: Vec<&Cable> = app.cables.iter().filter(|c| app.visible(c.src) && app.visible(c.dst)).collect();
            assert!(!vis.is_empty(), "rig {rig} has cables");
            for c in vis { assert!(finite(c), "rig {rig} cable {} curve {:?}", c.id, c.curve); }
        }
    }

    #[test]
    fn pan_reads_as_a_side_and_round_trips() {
        assert_eq!(pan_text(0.0), "C 0.00");
        assert_eq!(pan_text(-0.3), "L 0.30");
        assert_eq!(pan_text(1.0), "R 1.00");
        for v in [-1.0f32, -0.3, 0.0, 0.42, 1.0] {
            assert!((parse_pan(&pan_text(v)).unwrap() as f32 - v).abs() < 0.006, "{v}");
        }
        assert_eq!(parse_pan("0.5"), Some(0.5));
    }

    #[test]
    fn matrix_cells_name_their_rule() {
        let base = RuleEdit::default();
        assert_eq!(base.cell_code(), "ALL");
        assert_eq!(RuleEdit { transpose: 12, ..base.clone() }.cell_code(), "+12");
        assert_eq!(RuleEdit { transpose: -5, ..base.clone() }.cell_code(), "-5");
        assert_eq!(RuleEdit { channel: Some(1), ..base.clone() }.cell_code(), "ch 2");
        assert_eq!(RuleEdit { key_lo: 0, key_hi: 59, ..base.clone() }.cell_code(), "keys");
        assert_eq!(RuleEdit { vel_lo: 40, ..base.clone() }.cell_code(), "vel");
        assert_eq!(RuleEdit { enabled: false, transpose: 12, ..base }.cell_code(), "off", "a muted rule says so first");
    }

    #[test]
    fn remote_mix_changes_reach_the_mixer() {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        let (mut ebus, edrain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
        engine.set_graph_pub(pub_.clone());
        app.graph_pub = Some(pub_.clone());
        // A remote client (MCP/CLI) sets rig B's mix — the UI never sent it.
        ebus.send(Command::SetRigMix { rig: 1, vol: Some(0.8), pan: Some(-0.3), mute: Some(true), solo: None });
        let mut buf = vec![0.0f32; 64 * 2];
        engine.render(&mut buf, 2);
        app.sync_from_engine();
        let m = app.rig_mix[1];
        assert!((m.vol - 0.8).abs() < 1e-6 && (m.pan + 0.3).abs() < 1e-6 && m.mute, "mixer mirrors the engine: {m:?}");
        assert!((app.rig_mix[0].vol - 1.0).abs() < 1e-6, "rig A untouched");
        // A busy publication (UI holding the lock) is retried, not lost.
        let guard = pub_.lock().unwrap();
        ebus.send(Command::SetRigMix { rig: 0, vol: Some(0.5), pan: None, mute: None, solo: None });
        engine.render(&mut buf, 2);
        drop(guard);
        engine.render(&mut buf, 2);
        app.sync_from_engine();
        assert!((app.rig_mix[0].vol - 0.5).abs() < 1e-6, "retried after the lock was free");
    }

    #[test]
    fn keys_face_is_not_a_keyboard() {
        // Playing moved to the dock keyboard: a click anywhere on the Keys
        // face is the unit (drag/select), never a note.
        let (app, _ctx) = two_rigs();
        let k = app.keys_unit.expect("keys");
        let r = app.unit_rects[k];
        for f in [0.3f32, 0.5, 0.7] {
            let p = Pos2::new(r.left() + r.width() * f, r.top() + r.height() * f);
            assert!(matches!(app.hit_test(app.xf.p(p.to_vec2())), Hit::Unit(u) if u == k));
        }
    }

    #[test]
    fn no_play_button_on_the_stage() {
        // User 2026-11-15: the Play button is gone (Space + unit chips remain).
        let (mut app, ctx) = two_rigs();
        let old_play = Pos2::new(40.0, app.toolbar_top + 44.0);
        assert!(!matches!(app.hit_test(old_play), Hit::Play));
        for ev in click(old_play) { frame(&mut app, &ctx, ev); }
        assert!(!app.playing, "clicking the old spot starts nothing");
        frame(&mut app, &ctx, key(Key::Space));
        assert!(app.playing, "Space is still master play");
        assert!(app.stage_rect.top() + 40.0 <= app.xf.rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(app.rack_w, app.rack_h))).top() + 0.5,
            "the rack sits below the stage margin, no masthead gap needed");
    }

    #[test]
    fn matrix_block_is_centered_in_its_sheet() {
        let (mut app, ctx) = two_rigs();
        app.stage_view = StageView::Matrix;
        for _ in 0..4 { frame(&mut app, &ctx, vec![]); }
        // The grid's widgets (cells) and the measured block: equal margins.
        let block_w = app.matrix_block_w;
        assert!(block_w > 200.0, "measured: {block_w}");
        let cells: Vec<Rect> = ctx.viewport(|v| v.prev_pass.widgets.get_layer(egui::LayerId::background())
            .filter(|w| w.sense.senses_click() && (w.rect.width() - 68.0).abs() < 1.0 && (w.rect.height() - 32.0).abs() < 1.0)
            .map(|w| w.rect).collect());
        assert!(!cells.is_empty(), "matrix cells found");
        let sheet_mid = app.stage_rect.center().x;
        let block_left = cells.iter().map(|r| r.left()).fold(f32::MAX, f32::min) - 120.0 - 20.0; // label col + spacing
        let block_mid = block_left + block_w / 2.0;
        assert!((block_mid - sheet_mid).abs() < 60.0, "block centered: mid {block_mid} vs sheet {sheet_mid}");
    }

    #[test]
    fn every_rig_view_has_the_same_rack_size() {
        // Rig A: Keys/Surge/Delay/Master (2 rows); Rig B: Keys/Surge/Master (1 row).
        let (mut app, ctx) = two_rigs();
        let (h_a, rows_a) = (app.rack_h, app.layout_rows);
        app.set_active_rig(1);
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); }
        assert_eq!(app.layout_rows, rows_a, "rows follow the tallest rig");
        assert!((app.rack_h - h_a).abs() < 0.5, "same rack height: A {h_a} vs B {}", app.rack_h);
        // Rig B's lone Surge fills the column like Keys does.
        let surge = app.stack_cols[0][0];
        let keys = app.keys_unit.unwrap();
        assert!((app.unit_rects[surge].height() - app.unit_rects[keys].height()).abs() < 0.5, "stretched to full height");
    }

    #[test]
    fn chat_highlights_entities_on_word_boundaries() {
        let ents = vec!["Surge B".to_string(), "Rig B".to_string(), "Reverb".to_string(), "Surge".to_string()];
        let segs = highlight_segments("Loaded a flute on Rig B: Surge B now feeds the reverb (not Surgery).", &ents);
        let hits: Vec<&str> = segs.iter().filter(|(_, e)| *e).map(|(t, _)| t.as_str()).collect();
        assert_eq!(hits, vec!["Rig B", "Surge B", "reverb"], "{segs:?}");
        let joined: String = segs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "Loaded a flute on Rig B: Surge B now feeds the reverb (not Surgery).", "text preserved");
    }

    #[test]
    fn model_switch_and_context_header() {
        let (mut app, _ctx) = two_rigs();
        let before = app.assistant.history.len();
        app.submit_chat("/model deepseek-reasoner".into());
        assert_eq!(app.assistant.model(), "deepseek-reasoner");
        assert!(!app.assistant.busy, "a /model command is local, nothing sent");
        assert_eq!(app.assistant.history.len(), before + 1);
        app.set_active_rig(1);
        let ctx = app.chat_context();
        assert!(ctx.contains("rig in view = 2 · Rig B (index 1)"), "{ctx}");
        assert!(ctx.contains("Surge (node"), "units of the rig in view are listed: {ctx}");
    }

    #[test]
    fn letter_shortcuts_open_the_views() {
        let (mut app, ctx) = two_rigs();
        frame(&mut app, &ctx, key(Key::M));
        assert_eq!(app.stage_view, StageView::Mixer);
        frame(&mut app, &ctx, key(Key::N));
        assert_eq!(app.stage_view, StageView::Matrix);
        frame(&mut app, &ctx, key(Key::B));
        assert_eq!(app.stage_view, StageView::Plugins);
        frame(&mut app, &ctx, key(Key::C));
        assert!(app.assistant_open());
    }

    #[test]
    fn pin_a_surge_parameter_as_a_face_knob() {
        let mut app = DemoApp::new_with_sink(None);
        let p = crate::host::ParamDesc { id: 42, name: "Filter 1 Cutoff".into(), module: "A/Filter".into(),
            min: -60.0, max: 70.0, value: 5.0, default: 3.0, automatable: true, hidden: false, readonly: false };
        let surge = app.units.iter().position(|u| matches!(u.node_kind, UnitKind::Clap)).unwrap();
        let before = app.units[surge].knobs.len();
        let msg = app.toggle_pin(0, &p);
        assert!(msg.starts_with("pinned"), "{msg}");
        let k = app.units[surge].knobs.last().unwrap();
        assert_eq!(k.label, "FILTER 1 ");
        assert!((k.value - 0.5).abs() < 1e-6, "65/130 of the range: {}", k.value);
        assert_eq!(app.units[surge].knobs.len(), before + 1);
        app.publish_pins(); // no controllers headless: must not panic
        assert!(app.toggle_pin(0, &p).starts_with("unpinned"));
        assert_eq!(app.units[surge].knobs.len(), before);
    }

    #[test]
    fn dragging_a_unit_previews_the_layout_and_release_commits_it() {
        let mut app = DemoApp::new_with_sink(None); // stacks: [Surge A, Delay A] [Surge B, Delay B]
        let ctx = egui::Context::default();
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); }
        let delay_a = app.stack_cols[0][1];
        let delay_b = app.stack_cols[1][1];
        let before_b = app.unit_rects[delay_b];
        // Drag Delay A into the 2nd column, below Surge B, Delay B.
        let target = Vec2::new(app.col_x[1], app.stack_tall_h - 10.0);
        app.drag = Drag::Unit { unit: delay_a, grab: Vec2::ZERO };
        app.drag_cursor = Some(target);
        app.layout(app.stage_rect);
        let ghost = app.drop_ghost.expect("a landing slot is previewed");
        assert!((ghost.center().x - app.col_x[1]).abs() < 1.0, "ghost in column 2");
        assert!(app.unit_rects[delay_b].height() < before_b.height() - 1.0, "column 2 already reflowed for 3 units");
        assert_eq!(app.stack_cols[0].len(), 2, "nothing committed before release");
        // Release where previewed: the committed order equals the preview.
        app.drop_unit(delay_a, target);
        app.drag = Drag::None; app.drag_cursor = None;
        assert_eq!(app.stack_cols[1].last(), Some(&delay_a), "landed as previewed: {:?}", app.stack_cols);
    }

    #[test]
    fn plus_rig_grows_a_single_rig_rack() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.rig_count(), 1);
        let before = app.units.len();
        app.edit_rigs(None);
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.units.iter().filter(|u| u.rig == 0).count(), before, "rig A untouched");
        assert_eq!(app.active_rig, 1, "the view jumps to the new rig");
        assert_eq!(app.rig_name(1), "Rig B");
        // Standard layout: Keys → Surge → Master, wired (MIDI + L/R audio).
        let rig1: Vec<&Unit> = app.units.iter().filter(|u| u.rig == 1).collect();
        assert_eq!(rig1.iter().filter(|u| matches!(u.node_kind, UnitKind::Clap)).count(), 1, "the new rig gets a synth");
        let live = |app: &DemoApp, r: u8| app.cables.iter().filter(|c| c.dying < 0.0 && app.units[c.src].rig == r).count();
        assert_eq!(live(&app, 1), 3, "Keys→Surge MIDI + Surge→Master L/R");
        let a_cables = live(&app, 0);
        assert!(a_cables > 0);
        // Pool of 4 voices: rig C takes the last one, rig D is a skeleton.
        app.edit_rigs(None);
        assert_eq!(app.units.iter().filter(|u| matches!(u.node_kind, UnitKind::Clap)).count(), 4);
        app.edit_rigs(None);
        assert_eq!(app.rig_count(), 4);
        assert!(app.units.iter().filter(|u| u.rig == 3).all(|u| !matches!(u.node_kind, UnitKind::Clap)), "no free voice → no voiceless synth");
        assert_eq!(live(&app, 0), a_cables, "earlier rigs keep every cable");
        frame(&mut app, &ctx, vec![]);
    }

    /// One frame; returns every painted text with its screen rect.
    fn frame_texts(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        fn walk(s: &egui::Shape, acc: &mut Vec<(String, Rect)>) {
            match s {
                egui::Shape::Text(t) => acc.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Vec(v) => for x in v { walk(x, acc) },
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for cs in &out.shapes { walk(&cs.shape, &mut texts); }
        texts
    }

    fn click_text(app: &mut DemoApp, ctx: &egui::Context, label: &str) {
        let texts = frame_texts(app, ctx, vec![]);
        let pos = texts.iter().find(|(t, _)| t == label)
            .unwrap_or_else(|| panic!("'{label}' not on screen: {:?}", texts.iter().map(|(t, _)| t).collect::<Vec<_>>()))
            .1.center();
        for ev in click(pos) { frame(app, ctx, ev); }
    }

    /// #32: a destructive assistant action waits for the chat's Confirm /
    /// Cancel buttons; the click runs or cancels it and tells the model.
    #[test]
    fn assistant_destructive_action_waits_for_confirm_or_cancel_buttons() {
        let (mut app, ctx) = two_rigs();
        app.assistant.key_set = true;
        app.dock = DockView::Assistant;
        // The model asks to delete rig 2 without confirm: refused, parked.
        let out = app.run_assistant_tool("delete_rig", &serde_json::json!({"rig": 1}));
        assert!(out.contains("Confirm / Cancel buttons"), "{out}");
        assert_eq!(app.rig_count(), 2, "nothing deleted yet");
        // Cancel: nothing happens; the model hears it.
        click_text(&mut app, &ctx, "Cancel");
        assert!(app.pending_confirm.is_none());
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.assistant.history.last().map(|l| l.text.as_str()), Some("Cancel · Delete rig 2 · Rig B"));
        // Ask again → Confirm deletes the rig.
        app.assistant.busy = false;
        app.run_assistant_tool("delete_rig", &serde_json::json!({"rig": 1}));
        click_text(&mut app, &ctx, "Confirm · Delete rig 2 · Rig B");
        assert_eq!(app.rig_count(), 1, "Confirm ran delete_rig with confirm=true");
        assert!(app.pending_confirm.is_none());
        // Buttons hide while the model is busy, and a typed answer replaces them.
        app.assistant.busy = false;
        app.run_assistant_tool("install_instrument", &serde_json::json!({"id": "setbfree"}));
        app.assistant.busy = true;
        assert!(!frame_texts(&mut app, &ctx, vec![]).iter().any(|(t, _)| t == "Confirm · Install setbfree"), "hidden while busy");
        app.assistant.busy = false;
        assert!(frame_texts(&mut app, &ctx, vec![]).iter().any(|(t, _)| t == "Confirm · Install setbfree"), "shown when idle");
        app.assistant.busy = false;
        app.submit_chat("no thanks".into());
        assert!(app.pending_confirm.is_none(), "typing answers instead of clicking");
        // Confirmed calls and other tools never park a prompt.
        app.run_assistant_tool("delete_rig", &serde_json::json!({"rig": 5, "confirm": true}));
        app.run_assistant_tool("list_rigs", &serde_json::json!({}));
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn deleting_a_rig_keeps_the_survivor_patch_knobs_and_mix() {
        let (mut app, ctx) = two_rigs();
        app.rig_mix[1].vol = 0.3;
        let (ui_idx, k_idx) = app.units.iter().enumerate()
            .filter(|(_, u)| u.rig == 1)
            .find_map(|(i, u)| u.knobs.iter().position(|k| k.param.is_some()).map(|k| (i, k)))
            .expect("rig B has a knob");
        app.units[ui_idx].knobs[k_idx].value = 0.123;
        let a_units = app.units.iter().filter(|u| u.rig == 0).count();
        let b_cables = app.cables.iter().filter(|c| c.dying < 0.0 && app.units[c.src].rig == 1).count();
        app.edit_rigs(Some(0));
        assert_eq!(app.rig_count(), 1);
        assert_eq!(app.units.len(), 3, "only rig B's Keys/Surge/Master remain");
        assert_eq!(app.rig_name(0), "Rig B");
        assert_eq!(app.active_rig, 0);
        assert!((app.rig_mix[0].vol - 0.3).abs() < 1e-6, "mixer follows its rig");
        assert_eq!(app.cables.iter().filter(|c| c.dying < 0.0).count(), b_cables, "rig B's cables survive");
        let knob = &app.units[ui_idx - a_units].knobs[k_idx];
        assert!((knob.value - 0.123).abs() < 1e-6, "knob value survives the id remap");
        frame(&mut app, &ctx, vec![]);
        // The last rig cannot be deleted.
        app.edit_rigs(Some(0));
        assert_eq!(app.rig_count(), 1);
    }

    #[test]
    fn rig_delete_waits_for_confirmation_and_esc_cancels() {
        let (mut app, ctx) = two_rigs();
        app.confirm_delete_rig = Some(1);
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); }
        assert_eq!(app.rig_count(), 2, "nothing is deleted before confirming");
        assert_eq!(app.confirm_delete_rig, Some(1), "the modal stays up");
        frame(&mut app, &ctx, key(Key::Escape));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.confirm_delete_rig, None, "Esc cancels");
        assert_eq!(app.rig_count(), 2);
    }

    fn descs_has_mixer(app: &DemoApp) -> bool {
        app.help_cols.iter().flat_map(|c| c.rows.iter())
            .any(|r| r.keys.iter().any(|g| g.text() == "F10") && r.desc.text().contains("mixer"))
    }

    /// #22: attached agents can start and stop backing tracks.
    #[test]
    fn play_file_and_stop_file_are_served_over_the_bridge() {
        let (mut app, _ctx) = two_rigs();
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus))); // the inbox is served only with an engine
        let ask = |app: &mut DemoApp, tool: &str, args: serde_json::Value| {
            let (tx, rx) = std::sync::mpsc::channel();
            crate::bridge::UI_INBOX.lock().unwrap().push((tool.into(), args, tx));
            app.serve_ui_inbox();
            rx.recv().unwrap()
        };
        let missing = ask(&mut app, "play_file", serde_json::json!({"path": "/nonexistent/take.wav"}));
        assert!(missing.contains("no file"), "{missing}");
        let stopped = ask(&mut app, "stop_file", serde_json::json!({}));
        assert_eq!(stopped, "file player stopped");
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetPlayer { gain } if *gain == 0.0)), "{cmds:?}");
    }

    #[test]
    fn ui_bridge_tools_cover_mcp() {
        // Rule 6 for the attached path: an MCP tool forwarded to the app that
        // the app does not serve answers "not served over the UI bridge"
        // (list_instruments/install_instrument shipped like that once).
        let src = include_str!("mcp.rs");
        let mut missing = Vec::new();
        for part in src.split("ui_bridge_tool(\"").skip(1) {
            let name = part.split('"').next().unwrap_or("");
            if !name.is_empty() && !UI_BRIDGE_TOOLS.contains(&name) { missing.push(name.to_string()); }
        }
        assert!(missing.is_empty(), "MCP forwards these to the app, which does not serve them: {missing:?}");
    }

    #[test]
    fn rigs_can_be_renamed_and_names_survive_save_and_load() {
        let (mut app, ctx) = two_rigs();
        let out = app.exec_tool("rename_rig", &serde_json::json!({"rig": 1, "name": "  Bass  "}));
        assert!(out.contains("'Bass'"), "{out}");
        assert_eq!(app.rig_name(1), "Bass");
        assert!(app.exec_tool("rename_rig", &serde_json::json!({"rig": 1, "name": "   "})).contains("cannot be empty"));
        assert!(app.exec_tool("rename_rig", &serde_json::json!({"rig": 7, "name": "X"})).contains("no rig 7"));
        assert_eq!(app.rig_name(1), "Bass", "rejected renames change nothing");

        // The dialog: Enter renames.
        app.rename_rig_dialog = Some((0, "Lead".into()));
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, key(Key::Enter));
        assert_eq!(app.rig_name(0), "Lead");
        assert!(app.rename_rig_dialog.is_none());

        // Names live in the session and come back on load (multi-rig).
        let s = app.capture_session("names").unwrap();
        assert_eq!(s.rig_names, vec!["Lead".to_string(), "Bass".to_string()]);
        let mut fresh = DemoApp::new_with_sink(None);
        fresh.apply_session(&s).unwrap();
        assert_eq!((fresh.rig_name(0), fresh.rig_name(1)), ("Lead".to_string(), "Bass".to_string()));

        // …and a single renamed rig keeps its name too.
        let mut one = DemoApp::new_with_sink(None);
        one.rename_rig(0, "Organ").unwrap();
        let s1 = one.capture_session("one").unwrap();
        let mut fresh1 = DemoApp::new_with_sink(None);
        fresh1.apply_session(&s1).unwrap();
        assert_eq!(fresh1.rig_name(0), "Organ");
        // A later load without names does not inherit stale ones.
        fresh1.apply_session(&crate::session::Session::new("plain", Graph::demo_patch())).unwrap();
        assert_eq!(fresh1.rig_name(0), "Rig A");
    }

    #[test]
    fn rig_preset_saves_from_slot_a_and_loads_into_other_slots_intact() {
        let (mut app, ctx) = two_rigs();
        app.exec_tool("rename_rig", &serde_json::json!({"rig": 0, "name": "Lead Stack"}));
        app.exec_tool("rename_rig", &serde_json::json!({"rig": 1, "name": "Pad"}));

        let slot0_surge = app.units.iter().find(|u| u.rig == 0 && matches!(u.node_kind, UnitKind::Clap)).unwrap().node_id;
        let slot0_master = app.units.iter().find(|u| u.rig == 0 && matches!(u.node_kind, UnitKind::Master)).unwrap().node_id;
        let slot0_cables: Vec<(u8, u8)> = app.cables.iter()
            .filter(|c| app.units[c.src].rig == 0 && app.units[c.dst].rig == 0)
            .map(|c| (c.src_port as u8, c.dst_port as u8))
            .collect();
        assert!(!slot0_cables.is_empty());

        for u in &mut app.units {
            if u.node_id == slot0_master {
                if let Some(k) = u.knobs.iter_mut().find(|k| k.param == Some(crate::engine::P_VOL)) {
                    k.value = 0.42;
                }
            }
        }
        app.rig_mix[0] = crate::engine::RigMix { vol: 0.55, pan: -0.4, mute: false, solo: true };
        app.rig_mix[1] = crate::engine::RigMix { vol: 0.85, pan: 0.3, mute: false, solo: false };
        app.rig_sources[0] = (false, true);
        app.patch_names[0] = Some("Wurli".into());
        app.rule_edits.insert(
            (crate::matrix::SourceId::Keys, slot0_surge),
            RuleEdit { transpose: -5, key_lo: 36, ..RuleEdit::default() },
        );

        let saved = app.exec_tool("save_rig", &serde_json::json!({"name": "Lead Preset", "rig": 0}));
        assert!(saved.contains("Lead Preset"), "{saved}");
        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("list_rig_presets", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["presets"], serde_json::json!(["Lead Preset"]));

        let loaded_new = app.exec_tool("load_rig", &serde_json::json!({"name": "Lead Preset"}));
        assert!(loaded_new.contains("rig 2"), "{loaded_new}");
        assert_eq!(app.rig_count(), 3);
        assert_eq!(app.active_rig, 2);
        assert_eq!(app.rig_name(2), "Lead Stack");
        assert_eq!(app.rig_mix[2], crate::engine::RigMix { vol: 0.55, pan: -0.4, mute: false, solo: true });
        assert_eq!(app.rig_mix[1], crate::engine::RigMix { vol: 0.85, pan: 0.3, mute: false, solo: false }, "other rigs keep mix");

        let slot2_surge = app.units.iter().find(|u| u.rig == 2 && matches!(u.node_kind, UnitKind::Clap)).unwrap().node_id;
        let slot2_master = app.units.iter().find(|u| u.rig == 2 && matches!(u.node_kind, UnitKind::Master)).unwrap().node_id;
        assert_ne!(slot2_surge, slot0_surge, "loaded into a different slot with remapped ids");
        let slot2_vol = app.units.iter().find(|u| u.node_id == slot2_master).unwrap()
            .knobs.iter().find(|k| k.param == Some(crate::engine::P_VOL)).unwrap().value;
        assert!((slot2_vol - 0.42).abs() < 1e-5, "non-default knob preserved: {slot2_vol}");
        let slot2_cables: Vec<(u8, u8)> = app.cables.iter()
            .filter(|c| app.units[c.src].rig == 2 && app.units[c.dst].rig == 2)
            .map(|c| (c.src_port as u8, c.dst_port as u8))
            .collect();
        assert_eq!(slot2_cables, slot0_cables);
        let slot2_rule = app.rule_edits.get(&(crate::matrix::SourceId::Keys, slot2_surge))
            .expect("matrix route remapped to slot 2's Clap node");
        assert_eq!((slot2_rule.transpose, slot2_rule.key_lo), (-5, 36));
        let slot2_voice = app.clap_rigs().into_iter().find(|(_, nid)| *nid == slot2_surge).unwrap().0;
        assert_eq!(app.rig_sources[slot2_voice], (false, true));
        assert_eq!(app.patch_names[slot2_voice].as_deref(), Some("Wurli"));

        let replaced = app.exec_tool("load_rig", &serde_json::json!({"name": "Lead Preset", "rig": 1}));
        assert!(replaced.contains("rig 1"), "{replaced}");
        assert_eq!(app.rig_count(), 3);
        assert_eq!(app.rig_name(1), "Lead Stack");
        assert_eq!(app.rig_mix[1], crate::engine::RigMix { vol: 0.55, pan: -0.4, mute: false, solo: true });
        let slot1_surge = app.units.iter().find(|u| u.rig == 1 && matches!(u.node_kind, UnitKind::Clap)).unwrap().node_id;
        let slot1_master = app.units.iter().find(|u| u.rig == 1 && matches!(u.node_kind, UnitKind::Master)).unwrap().node_id;
        let slot1_vol = app.units.iter().find(|u| u.node_id == slot1_master).unwrap()
            .knobs.iter().find(|k| k.param == Some(crate::engine::P_VOL)).unwrap().value;
        assert!((slot1_vol - 0.42).abs() < 1e-5);
        assert_eq!(app.rule_edits.get(&(crate::matrix::SourceId::Keys, slot1_surge)).unwrap().transpose, -5);

        let tools = crate::assistant::tools();
        let tool_names: Vec<&str> = tools.as_array().unwrap().iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|v| v.as_str()))
            .collect();
        for name in ["save_rig", "load_rig", "list_rig_presets"] {
            assert!(tool_names.contains(&name), "assistant::tools missing {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS missing {name}");
        }
        app.save_rig_dialog = Some((0, "Modal Rig".into()));
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, key(Key::Escape));
        assert!(app.save_rig_dialog.is_none(), "Esc closes save modal");
        app.load_rig_dialog = Some(None);
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, key(Key::Escape));
        assert!(app.load_rig_dialog.is_none(), "Esc closes load modal");
        let _ = std::fs::remove_dir_all(&app.rig_presets_dir);
    }

    #[test]
    fn info_line_never_runs_over_the_top_bar_buttons() {
        // A long MIDI event in narrow windows (down to the 760 px minimum):
        // nothing in the top bar — buttons or the info line — may run past
        // the window edge (REC/SYNC did at ~780 px).
        // Sizes in points (egui's screen_rect unit): the minimum window, the
        // user's ~780-point tile, and a full screen.
        // Worst case on purpose: an external agent is connected, so its chip
        // shows too (a global the bridge tests also set in parallel).
        crate::bridge::LAST_REMOTE_MS.store(crate::bridge::now_ms() + 600_000, std::sync::atomic::Ordering::Relaxed);
        for width in [760.0f32, 900.0, 1600.0] {
            let mut app = DemoApp::new_with_sink(None);
            let ctx = egui::Context::default();
            app.show_stats = true;
            app.last_midi = Some("ESI MIDIMATE eX MIDI 1 · C4 v100 · a very long device name that keeps going and going".into());
            let size = Vec2::new(width, 700.0);
            for _ in 0..3 {
                let mut raw = egui::RawInput::default();
                raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
                let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
                out.textures_delta.clear();
            }
            let bar: Vec<Rect> = ctx.viewport(|v| v.prev_pass.widgets.get_layer(egui::LayerId::background())
                .filter(|w| w.rect.top() < 40.0 && w.rect.height() < 40.0).map(|w| w.rect).collect());
            let right = bar.iter().map(|r| r.right()).fold(0.0f32, f32::max);
            assert!(app.dock_group_rect.is_positive());
            assert!(right <= width + 0.5, "{width} pt window: rightmost top-bar element at {right}");
            // And the bar still works there: Sessions (the 2nd View button) opens.
            let mut btns: Vec<Rect> = ctx.viewport(|v| v.prev_pass.widgets.get_layer(egui::LayerId::background())
                .filter(|w| w.sense.senses_click() && w.rect.top() < 40.0).map(|w| w.rect).collect());
            btns.sort_by(|a, b| a.left().total_cmp(&b.left()));
            let at = btns[1].center();
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
            raw.events = vec![egui::Event::PointerMoved(at),
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }];
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
            assert_eq!(app.stage_view, StageView::Sessions, "{width} pt: the View buttons stay clickable");
        }
        crate::bridge::LAST_REMOTE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn i_toggles_the_info_line_with_the_last_midi_event() {
        let (mut app, ctx) = two_rigs();
        assert!(!app.show_stats);
        frame(&mut app, &ctx, key(Key::I));
        assert!(app.show_stats, "I shows the info line");
        app.hw_devices = vec!["ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 1 28:0".into()];
        app.hw_queue.lock().unwrap().push_back((NoteEvent { note: 60, velocity: 100.0 / 127.0, source: SourceId::Hardware(0), created: std::time::Instant::now() }, true));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.last_midi.as_deref(), Some("ESI MIDIMATE eX MIDI 1 · C4 v100"));
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(0), 0, 74, 64));
        frame(&mut app, &ctx, vec![]);
        app.stats_timer = 0.0;
        assert!(app.stats_text().starts_with("ESI MIDIMATE eX MIDI 1 · CC74 64 |"), "{}", app.stats_line);
        frame(&mut app, &ctx, key(Key::I));
        assert!(!app.show_stats, "I hides it again");
    }

    #[test]
    fn help_overlay_lists_current_shortcuts_in_two_columns() {
        let (mut app, ctx) = two_rigs();
        frame(&mut app, &ctx, key(Key::F1));
        assert!(app.show_help, "F1 opens help");
        frame(&mut app, &ctx, vec![]); // paints the card without panicking
        assert_eq!(app.help_cols.len(), 2, "PLAY + PATCH & VIEWS");
        let keys: Vec<String> = app.help_cols.iter()
            .flat_map(|c| c.rows.iter().flat_map(|r| r.keys.iter().map(|g| g.text().to_owned())))
            .collect();
        for k in ["Tab", "Space", "Ctrl", "Del", "double-click", "F4", "F5–F8", "F9", "F10", "F11", "F12", "1–9", "R", "I", "SPLIT"] {
            assert!(keys.iter().any(|x| x == k), "help lists {k}");
        }
        assert!(descs_has_mixer(&app), "F10 is the mixer now (the Transport window is gone)");
        let descs: Vec<String> = app.help_cols.iter()
            .flat_map(|c| c.rows.iter().map(|r| r.desc.text().to_owned())).collect();
        assert!(descs.iter().any(|d| d.contains("BPM") && d.contains("metronome")), "help says where tempo lives");
        frame(&mut app, &ctx, key(Key::Escape));
        assert!(!app.show_help, "Esc closes help");
    }

    #[test]
    fn masthead_has_no_flip_pill_tab_still_flips() {
        let (mut app, ctx) = two_rigs();
        // Where the flip pill used to sit (x≈280..430, top+28..60).
        for ev in click(Pos2::new(355.0, app.toolbar_top + 44.0)) { frame(&mut app, &ctx, ev); }
        assert!(!app.flipping && app.side == Side::Front, "the old pill area is inert");
        frame(&mut app, &ctx, key(Key::Tab));
        assert!(app.flipping, "Tab flips the rack");
    }

    #[test]
    fn digit_keys_switch_rigs_on_the_real_frame_path() {
        let (mut app, ctx) = two_rigs();
        frame(&mut app, &ctx, key(Key::Num2));
        assert_eq!(app.active_rig, 1, "2 selects the second rig");
        frame(&mut app, &ctx, key(Key::Num1));
        assert_eq!(app.active_rig, 0, "1 selects the first rig");
    }

    #[test]
    fn arrow_keys_cycle_rigs_on_the_real_frame_path() {
        let (mut app, ctx) = two_rigs();
        frame(&mut app, &ctx, key(Key::ArrowRight));
        assert_eq!(app.active_rig, 1);
        frame(&mut app, &ctx, key(Key::ArrowRight));
        assert_eq!(app.active_rig, 0, "→ wraps");
        frame(&mut app, &ctx, key(Key::ArrowLeft));
        assert_eq!(app.active_rig, 1, "← wraps back");
    }

    #[test]
    fn keys_still_switch_after_tab_flip() {
        let (mut app, ctx) = two_rigs();
        frame(&mut app, &ctx, key(Key::Tab));
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        assert!(ctx.memory(|m| m.focused()).is_none(), "Tab must not park egui focus on a widget");
        frame(&mut app, &ctx, key(Key::Num2));
        assert_eq!(app.active_rig, 1);
    }

    /// Rig 0's pan DragValue in the mixer stage (first focusable drag
    /// widget right of rig 0's bus fader).
    fn mixer_pan_value(app: &DemoApp, ctx: &egui::Context) -> Rect {
        let f0 = app.mixer_fader_rects[0];
        ctx.viewport(|v| {
            v.prev_pass.widgets.get_layer(egui::LayerId::background())
                .filter(|w| w.sense.senses_drag() && w.sense.is_focusable() && w.rect.left() > f0.right())
                .min_by(|a, b| a.rect.left().total_cmp(&b.rect.left()))
                .map(|w| w.rect)
        }).expect("pan DragValue right of rig 0's bus fader")
    }

    fn open_mixer(app: &mut DemoApp, ctx: &egui::Context) {
        app.stage_view = StageView::Mixer;
        for _ in 0..2 { frame(app, ctx, vec![]); }
        assert_eq!(app.mixer_fader_rects.len(), 2, "one bus strip per rig");
    }

    #[test]
    fn rig_strip_has_no_mixer_controls() {
        let (app, ctx) = two_rigs();
        // No focusable drag widget (vol/pan) anywhere in the top strips.
        let below = app.rig_chip_rects[0].bottom() + 40.0;
        let n = ctx.viewport(|v| v.prev_pass.widgets.get_layer(egui::LayerId::background())
            .filter(|w| w.sense.senses_drag() && w.sense.is_focusable() && w.rect.bottom() < below).count());
        assert_eq!(n, 0, "vol/pan live in the Mixer view, not the rig strip");
    }

    #[test]
    fn keys_switch_rigs_after_clicking_a_mixer_value() {
        let (mut app, ctx) = two_rigs();
        open_mixer(&mut app, &ctx);
        let dv = mixer_pan_value(&app, &ctx);
        let pan = app.rig_mix[0].pan;
        for ev in click(dv.center()) { frame(&mut app, &ctx, ev); }
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, key(Key::Num2));
        assert_eq!(app.active_rig, 1, "2 must switch rigs even after a mixer DragValue was clicked");
        frame(&mut app, &ctx, key(Key::ArrowLeft));
        assert_eq!(app.active_rig, 0, "← must cycle rigs after a mixer click");
        assert_eq!(app.rig_mix[0].pan, pan, "rig keys must not be typed into the mixer value");
    }

    #[test]
    fn mixer_bus_fader_drags_and_esc_returns_to_rack() {
        let (mut app, ctx) = two_rigs();
        app.rig_mix[0].vol = 0.5;
        open_mixer(&mut app, &ctx);
        let c = app.mixer_fader_rects[0].center();
        let b = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut app, &ctx, vec![egui::Event::PointerMoved(c)]);
        frame(&mut app, &ctx, vec![b(c, true)]);
        for dy in [10.0, 20.0, 30.0] { frame(&mut app, &ctx, vec![egui::Event::PointerMoved(c - Vec2::new(0.0, dy))]); }
        frame(&mut app, &ctx, vec![b(c - Vec2::new(0.0, 30.0), false)]);
        assert!(app.rig_mix[0].vol > 0.5, "dragging up raises rig volume ({})", app.rig_mix[0].vol);
        assert_eq!(app.rig_mix[1].vol, 1.0, "the other rig is untouched");
        frame(&mut app, &ctx, key(Key::Escape));
        assert_eq!(app.stage_view, StageView::Rack, "Esc leaves the mixer");
    }

    #[test]
    fn mixer_lists_every_source_per_rig() {
        let (app, _ctx) = two_rigs();
        for rig in 0..2u8 {
            let src = app.mixer_sources(rig);
            assert_eq!(src.len(), 1, "rig {rig}: its Surge is the one source");
            let (u, k) = src[0];
            assert!(matches!(app.units[u].node_kind, UnitKind::Clap));
            assert_eq!(app.units[u].knobs[k.expect("Surge has a LEVEL knob")].label, "LEVEL");
        }
    }

    #[test]
    fn a_focused_text_field_still_gets_the_keys() {
        let (mut app, ctx) = two_rigs();
        app.assistant.key_set = true; // chat field (tests never load the real key)
        app.dock = DockView::Assistant;
        frame(&mut app, &ctx, vec![]);
        // The chat/API-key field: the widest focusable text widget low on
        // screen (height filter skips the panel's resize handle).
        let field = ctx.viewport(|v| {
            v.prev_pass.widgets.get_layer(egui::LayerId::background())
                .filter(|w| w.sense.is_focusable() && w.sense.senses_drag() && w.rect.top() > 500.0 && w.rect.height() > 10.0)
                .max_by(|a, b| a.rect.width().total_cmp(&b.rect.width()))
                .map(|w| w.rect)
        }).expect("assistant text field");
        for ev in click(field.center()) { frame(&mut app, &ctx, ev); }
        assert!(ctx.text_edit_focused(), "clicking the field focuses it");
        frame(&mut app, &ctx, vec![
            egui::Event::Key { key: Key::Num2, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Text("2".into()),
        ]);
        assert_eq!(app.active_rig, 0, "typing a 2 into a text field must not switch rigs");
    }

    #[test]
    fn single_rig_digit_says_why_nothing_switched() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.rig_count(), 1);
        frame(&mut app, &ctx, key(Key::Num2));
        assert_eq!(app.active_rig, 0);
        assert!(app.patch_notice.as_ref().map_or(false, |(m, _)| m.contains("one rig")), "single-rig press is explained");
    }

    #[test]
    fn chip_click_switches_and_keys_keep_working() {
        let (mut app, ctx) = two_rigs();
        let chip = app.rig_chip_rects[1].center();
        for ev in click(chip) { frame(&mut app, &ctx, ev); }
        assert_eq!(app.active_rig, 1, "clicking chip 2 switches");
        frame(&mut app, &ctx, key(Key::Num1));
        assert_eq!(app.active_rig, 0, "keys still work after a chip click");
        frame(&mut app, &ctx, key(Key::ArrowRight));
        assert_eq!(app.active_rig, 1, "arrows still work after a chip click");
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::control::command_bus;

    fn frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
    }

    fn click(app: &mut DemoApp, ctx: &egui::Context, pos: Pos2) {
        let b = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        for ev in [vec![egui::Event::PointerMoved(pos)], vec![b(true)], vec![b(false)]] {
            frame(app, ctx, ev);
        }
    }

    /// Demo patch + a METRONOME wired to the master; rack on the front.
    fn metro_app() -> (DemoApp, egui::Context, u32) {
        let mut app = DemoApp::new_with_sink(None);
        let mut g = Graph::demo_patch();
        let m = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
        g.connect_with_id(10, PortRef { node: m, port: 0 }, PortRef { node: 3, port: 0 }, CPortKind::Audio);
        app.build_rack(&g, &[]);
        let ctx = egui::Context::default();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        assert!(app.side == Side::Front);
        (app, ctx, m)
    }

    fn metro_unit(app: &DemoApp, m: u32) -> usize {
        app.units.iter().position(|u| u.node_id == m).unwrap()
    }

    /// Screen position of the metronome's Play/Stop chip.
    fn chip_pos(app: &DemoApp, m: u32) -> Pos2 {
        let chip = app.transport_chip_rect(metro_unit(app, m)).expect("metronome face has a chip");
        app.xf.p(chip.center().to_vec2())
    }

    #[test]
    fn only_transport_units_get_a_chip() {
        let (app, _ctx, m) = metro_app();
        for (i, u) in app.units.iter().enumerate() {
            assert_eq!(app.transport_chip_rect(i).is_some(), u.node_id == m, "unit {} chip", u.name);
        }
    }

    #[test]
    fn face_chip_sends_set_node_playing_and_mirrors_the_engine() {
        let (mut app, ctx, m) = metro_app();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (mut ebus, edrain) = command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::default(), rules: Vec::new(), ..Default::default() }));
        engine.set_graph_pub(pub_.clone());
        app.graph_pub = Some(pub_.clone());
        // Forward what the UI sent to the engine and render one block;
        // returns the SetNodePlaying commands seen.
        let mut pump = |drain: &mut crate::control::CommandDrain| {
            let mut sent = Vec::new();
            drain.drain(|c| sent.push(c));
            let toggles: Vec<(u32, bool)> = sent.iter().filter_map(|c| match c {
                Command::SetNodePlaying { node, playing } => Some((*node, *playing)),
                _ => None,
            }).collect();
            for c in sent { ebus.send(c); }
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
            toggles
        };
        frame(&mut app, &ctx, vec![]);
        pump(&mut drain); // settle the per-frame mirror
        frame(&mut app, &ctx, vec![]);
        let units_before = app.units.len();

        let p = chip_pos(&app, m);
        click(&mut app, &ctx, p);
        let ui = metro_unit(&app, m);
        assert!(!app.units[ui].playing, "no local guess while an engine is attached");
        assert_eq!(pump(&mut drain), vec![(m, true)], "chip click sends SetNodePlaying(true)");
        frame(&mut app, &ctx, vec![]); // sync_from_engine adopts the flag
        let ui = metro_unit(&app, m);
        assert!(app.units[ui].playing, "face mirrors the engine's playing flag");
        assert!(app.base_graph.node(m).unwrap().playing, "base graph (sessions) follows");
        assert!(app.shared_graph.lock().unwrap().node(m).map_or(true, |n| n.playing), "bridge mirror follows");
        assert_eq!(app.units.len(), units_before);
        assert_ne!(app.session_msg, "Patch updated from MCP/CLI", "a Play/Stop must not rebuild the rack");

        let p = chip_pos(&app, m);
        click(&mut app, &ctx, p);
        assert_eq!(pump(&mut drain), vec![(m, false)], "second click stops it");
        frame(&mut app, &ctx, vec![]);
        assert!(!app.units[metro_unit(&app, m)].playing);

        // Master transport (Space) starts every transport-capable node.
        frame(&mut app, &ctx, vec![
            egui::Event::Key { key: Key::Space, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Key { key: Key::Space, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ]);
        pump(&mut drain);
        frame(&mut app, &ctx, vec![]);
        assert!(app.playing);
        assert!(app.units[metro_unit(&app, m)].playing, "master play reaches the metronome face");
    }

    #[test]
    fn headless_chip_toggles_locally() {
        let (mut app, ctx, m) = metro_app();
        assert!(app.graph_pub.is_none());
        let p = chip_pos(&app, m);
        click(&mut app, &ctx, p);
        assert!(app.units[metro_unit(&app, m)].playing);
        assert!(app.base_graph.node(m).unwrap().playing);
        let p = chip_pos(&app, m);
        click(&mut app, &ctx, p);
        assert!(!app.units[metro_unit(&app, m)].playing);
    }

    #[test]
    fn set_node_playing_tool_parity() {
        // Rule 6: the tool exists in the assistant schema AND exec_tool.
        let tools = crate::assistant::tools();
        assert!(tools.as_array().unwrap().iter().any(|t| t["function"]["name"] == "set_node_playing"));
        let (mut app, _ctx, m) = metro_app();
        let r = app.exec_tool("set_node_playing", &serde_json::json!({"node": m, "playing": true}));
        assert!(!r.starts_with("unknown tool"), "{r}");
        assert!(app.units[metro_unit(&app, m)].playing, "{r}");
        let r = app.exec_tool("set_node_playing", &serde_json::json!({"node": 2, "playing": true}));
        assert!(r.contains("no transport"), "a delay has no transport: {r}");
        // get_graph exposes the flag.
        app.publish_patch();
        let g: serde_json::Value = serde_json::from_str(&app.exec_tool("get_graph", &serde_json::json!({}))).unwrap();
        let node = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == serde_json::json!(m)).cloned().unwrap();
        assert_eq!(node["playing"], serde_json::json!(true));
        // Master stop via the assistant clears it.
        app.exec_tool("transport_stop", &serde_json::json!({}));
        assert!(!app.units[metro_unit(&app, m)].playing);
    }

    #[test]
    fn playing_round_trips_through_sessions() {
        let (mut app, _ctx, m) = metro_app();
        assert!(app.set_node_playing(m, true));
        let s = app.capture_session("transport").unwrap();
        assert!(s.graph.node(m).unwrap().playing);
        let json = serde_json::to_string(&s.graph).unwrap();
        let back: Graph = serde_json::from_str(&json).unwrap();
        let mut fresh = DemoApp::new_with_sink(None);
        fresh.apply_session(&crate::session::Session::new("t", back)).unwrap();
        let u = fresh.units.iter().find(|u| u.node_id == m).unwrap();
        assert!(u.transport && u.playing, "restored metronome face shows playing");
    }
}

// ---------------------------------------------------------------------------
// Audio-chain visualization (docs/ARCHITECTURE.md §2): engine per-edge levels drive audio
// cables, key LEDs snap, labels fit their units, agent edits never flash.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod audio_viz_tests {
    use super::*;

    const SCREEN: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 1280.0, y: 800.0 } };

    fn engine_app() -> (DemoApp, Arc<EdgeLevels>) {
        let mut app = DemoApp::new_with_sink(None);
        app.layout(SCREEN);
        let levels = Arc::new(EdgeLevels::new());
        app.edge_levels = Some(levels.clone());
        (app, levels)
    }

    fn run(app: &mut DemoApp, secs: f32) {
        for _ in 0..(secs * 60.0).round() as usize { app.update(1.0 / 60.0); }
    }

    fn active_pulses(c: &Cable) -> usize { c.pulses.iter().filter(|p| p.active).count() }

    #[test]
    fn rig_strip_shows_every_rig_that_sounds_not_only_the_one_in_view() {
        let (mut app, levels) = engine_app();
        app.load_multi_rig_demo();
        app.layout(SCREEN);
        app.set_active_rig(0);
        // The keyboard plays both rigs; only rig B's audio is published here.
        let rig_b_audio: Vec<u32> = app.cables.iter()
            .filter(|c| c.kind == PortKind::Audio && app.units.get(c.src).map_or(false, |u| u.rig == 1))
            .map(|c| c.id).collect();
        assert!(!rig_b_audio.is_empty(), "rig B has audio cables");
        for _ in 0..30 {
            for &id in &rig_b_audio { levels.store(id, 0.3); }
            app.update(1.0 / 60.0);
        }
        assert!(app.rig_levels[1] > 0.3, "rig B sounds while rig A is in view: {:?}", app.rig_levels);
        assert!(app.rig_levels[0] < 0.05, "rig A is silent: {:?}", app.rig_levels);
    }

    #[test]
    fn published_edge_levels_make_the_whole_chain_flow() {
        let (mut app, levels) = engine_app();
        let audio: Vec<u32> = app.cables.iter().filter(|c| c.kind == PortKind::Audio).map(|c| c.id).collect();
        assert!(audio.len() >= 3, "demo has Surge→Delay→Master + Surge→Master");
        // The engine publishes a sounding level on every audio edge (block
        // rate; re-published each frame like the render path does).
        for _ in 0..30 {
            for &id in &audio { levels.store(id, 0.3); }
            app.update(1.0 / 60.0);
        }
        for c in app.cables.iter().filter(|c| c.kind == PortKind::Audio) {
            assert!(c.level > 0.5, "cable {} must breathe with its edge level (level {})", c.id, c.level);
            assert!(active_pulses(c) > 0, "cable {} must carry traveling pulses while sounding", c.id);
        }
        // MIDI pulses stay event-driven: no key pressed → none.
        for c in app.cables.iter().filter(|c| c.kind == PortKind::Midi) {
            assert_eq!(active_pulses(c), 0, "MIDI cable {} pulsed without an event", c.id);
        }
        // Silence: levels decay, the spawner stops, pulses drain off.
        for &id in &audio { levels.store(id, 0.0); }
        run(&mut app, 2.5);
        for c in app.cables.iter().filter(|c| c.kind == PortKind::Audio) {
            assert!(c.level < AUDIO_PULSE_MIN, "cable {} level did not decay ({})", c.id, c.level);
            assert_eq!(active_pulses(c), 0, "cable {} kept pulsing in silence", c.id);
        }
    }

    #[test]
    fn unpublished_edges_stay_dark_with_an_engine() {
        // With an engine attached, a MIDI arrival must not fake the synth's
        // audio envelope (a muted/unrouted synth must not look live).
        let (mut app, _levels) = engine_app();
        let surge = app.units.iter().position(|u| matches!(u.node_kind, UnitKind::Clap)).unwrap();
        app.arrivals[0] = (surge, 1.0);
        app.fire_keys(60, 0.9);
        run(&mut app, 1.0);
        for c in app.cables.iter().filter(|c| c.kind == PortKind::Audio) {
            assert!(c.level < 0.01, "cable {} lit without a published level ({})", c.id, c.level);
        }
        // Without an engine the arrival fallback still animates the chain.
        let mut silent = DemoApp::new_with_sink(None);
        silent.layout(SCREEN);
        silent.fire_keys(60, 0.9);
        let mut peak = 0.0f32;
        for _ in 0..90 {
            silent.update(1.0 / 60.0);
            peak = peak.max(silent.cables.iter().filter(|c| c.kind == PortKind::Audio).map(|c| c.level).fold(0.0, f32::max));
        }
        assert!(peak > 0.2, "no-engine fallback must still breathe ({peak})");
    }

    #[test]
    fn edge_display_level_maps_db() {
        assert_eq!(edge_display_level(0.0), 0.0);
        assert_eq!(edge_display_level(-1.0), 0.0);
        assert_eq!(edge_display_level(f32::NAN), 0.0);
        assert!((edge_display_level(1.0) - 1.0).abs() < 1e-6);
        assert!(edge_display_level(1e-4) == 0.0, "−80 dB is silence");
        let quiet = edge_display_level(0.05);
        assert!(quiet > AUDIO_PULSE_MIN, "audible −26 dB signal must flow ({quiet})");
    }

    /// Real frame through a live egui::Context (rule 11: clear textures).
    fn frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.focused = true;
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
    }

    fn key_ev(key: Key, pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat, modifiers: Modifiers::NONE }
    }

    #[test]
    fn key_led_holds_while_held_and_snaps_after_release() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![key_ev(Key::A, true, false)]);
        assert_eq!(app.held_keys.len(), 1);
        let semi = app.held_keys[0].1 % 12;
        // Held (OS key repeats are swallowed): stays fully lit.
        for _ in 0..20 { frame(&mut app, &ctx, vec![key_ev(Key::A, true, true)]); }
        app.update(0.5);
        assert!(app.key_leds[semi] > 0.99, "held key must stay lit ({})", app.key_leds[semi]);
        // Release: the note-off must not re-light the key; a short tail...
        frame(&mut app, &ctx, vec![key_ev(Key::A, false, false)]);
        assert!(app.held_keys.is_empty());
        for _ in 0..6 { app.update(1.0 / 60.0); }
        assert!(app.key_leds[semi] > 0.02, "a short visible tail remains ({})", app.key_leds[semi]);
        // ...then dark within ~0.35 s of release.
        for _ in 0..15 { app.update(1.0 / 60.0); }
        assert!(app.key_leds[semi] < 0.02, "key LED still lit 0.35 s after release ({})", app.key_leds[semi]);
    }

    #[test]
    fn fit_label_shrinks_then_truncates() {
        // Fake metric: width = chars × size × 0.6.
        let m = |t: &str, sz: f32| t.chars().count() as f32 * sz * 0.6;
        assert_eq!(fit_label("SURGE", 10.0, 8.0, 100.0, m), ("SURGE".to_string(), 10.0));
        assert_eq!(fit_label("SURGE", 10.0, 8.0, f32::INFINITY, m).1, 10.0);
        // 20 chars @10 = 120 px; 108 px fits at 9 px.
        let (t, sz) = fit_label("ABCDEFGHIJKLMNOPQRST", 10.0, 8.0, 108.0, m);
        assert_eq!(t, "ABCDEFGHIJKLMNOPQRST");
        assert!(sz <= 9.0 && sz >= 8.0 && m(&t, sz) <= 108.0, "{sz}");
        // Too wide even at the 8 px floor → ellipsis, still within bounds.
        let (t, sz) = fit_label("METRONOME · TRANSPORT CLICK", 10.0, 8.0, 60.0, m);
        assert_eq!(sz, 8.0);
        assert!(t.ends_with('…') && m(&t, sz) <= 60.0, "{t}");
        // Degenerate width never panics.
        let (t, _) = fit_label("X", 10.0, 8.0, -5.0, m);
        assert_eq!(t, "…");
    }

    #[test]
    fn compressed_unit_labels_fit_their_boxes() {
        // Many units in a narrow window: every name/subtitle galley must fit
        // inside its unit (draw_front additionally clips to the unit rect).
        let mut g = Graph::demo_patch();
        for (i, k) in [DspKind::Metronome, DspKind::Reverb, DspKind::Eq3, DspKind::Limiter].into_iter().enumerate() {
            g.add_node(NodeKind::Dsp(k), name_bytes(&format!("FX {i}")));
        }
        let mut app = DemoApp::new_with_sink(None);
        app.build_rack(&g, &[]);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut raw_frame = |app: &mut DemoApp| {
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0)));
            raw.focused = true;
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
        };
        // Frame 1 fixes the HiDPI scale + lays out; later frames refit the
        // galleys against the laid-out rects (one-frame lag by design).
        for _ in 0..8 { raw_frame(&mut app); }
        let mut shrunk = 0;
        for (i, u) in app.units.iter().enumerate() {
            if !app.visible(i) { continue; }
            let w = app.unit_rects[i].width();
            let name_w = u.name_g.as_ref().unwrap().size().x;
            let sub_w = u.sub_g.as_ref().unwrap().size.x;
            assert!(name_w <= (w - UNIT_NAME_INSET).max(0.0) + 1.0 || u.name_g.as_ref().unwrap().text() == "…",
                "unit {} name {name_w}px overflows {w}px", u.name);
            assert!(sub_w <= (w - UNIT_SUB_INSET).max(0.0) + 1.0 || u.sub_g.as_ref().unwrap().chars.len() == 1,
                "unit {} subtitle {sub_w}px overflows {w}px", u.name);
            if u.label_fit_w.is_finite() { shrunk += 1; }
        }
        assert!(shrunk > 0, "labels were never fitted to real unit widths");
    }

    #[test]
    fn sync_adoption_does_not_flash_but_loads_do() {
        let mut app = DemoApp::new_with_sink(None);
        app.update(1.0);
        assert_eq!(app.load_fade, 1.0);
        // Agent/MCP AddNode → engine publishes → UI adopts: no fade replay.
        let mut remote = app.base_graph.clone();
        remote.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 5, graph: remote, rules: Vec::new(), ..Default::default() }));
        let before = app.units.len();
        app.graph_pub = Some(pub_);
        app.sync_from_engine();
        assert_eq!(app.units.len(), before + 1, "remote AddNode adopted");
        assert_eq!(app.load_fade, 1.0, "sync adoption must not replay the load fade");
        // Explicit loads still fade in.
        app.load_template(Template::LeadReverb);
        assert_eq!(app.load_fade, 0.0);
        app.update(1.0);
        app.load_multi_rig_demo();
        assert_eq!(app.load_fade, 0.0);
    }
}

// ---------------------------------------------------------------------------
// Split as a Keys-node property (two MIDI outs) + BPM on the metronome face
// (brief D, user 2026-11-15).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod split_bpm_tests {
    use super::*;
    use crate::control::command_bus;

    /// One frame through the real runtime path; returns every painted text.
    fn frame(app: &mut DemoApp, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<String> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        fn walk(shape: &Shape, texts: &mut Vec<String>) {
            match shape {
                Shape::Text(t) => texts.push(t.galley.text().to_owned()),
                Shape::Vec(v) => for s in v { walk(s, texts) },
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for cs in &out.shapes { walk(&cs.shape, &mut texts); }
        texts
    }

    fn ptr(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    fn click(app: &mut DemoApp, ctx: &egui::Context, pos: Pos2) {
        for ev in [vec![egui::Event::PointerMoved(pos)], vec![ptr(pos, true)], vec![ptr(pos, false)]] {
            frame(app, ctx, ev);
        }
    }

    fn drag(app: &mut DemoApp, ctx: &egui::Context, from: Pos2, to: Pos2) {
        frame(app, ctx, vec![egui::Event::PointerMoved(from)]);
        frame(app, ctx, vec![ptr(from, true)]);
        for i in 1..=6 {
            let p = from + (to - from) * (i as f32 / 6.0);
            frame(app, ctx, vec![egui::Event::PointerMoved(p)]);
        }
        frame(app, ctx, vec![ptr(to, false)]);
    }

    fn screen(app: &DemoApp, r: Rect) -> Pos2 { app.xf.p(r.center().to_vec2()) }

    /// Demo patch (Keys 0 → Surge A 1 / Surge B 4) on the front.
    fn demo() -> (DemoApp, egui::Context) {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        assert!(app.side == Side::Front);
        (app, ctx)
    }

    fn keys(app: &DemoApp) -> usize { app.keys_unit.expect("keys unit") }

    fn out_labels(app: &DemoApp) -> Vec<&'static str> {
        app.units[keys(app)].jacks.iter().filter(|j| j.dir == Dir::Out).map(|j| j.label).collect()
    }

    #[test]
    fn instrument_catalog_parity_and_confirm_gate() {
        // Rule 6/23: both tools exist in the schema AND exec_tool; installing is
        // confirm-gated and surfaces through list_instruments.
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert!(names.contains(&"list_instruments"), "schema has list_instruments");
        assert!(names.contains(&"install_instrument"), "schema has install_instrument");

        let (mut app, _ctx) = demo();
        app.plugins_loaded = true;
        app.plugins_clap = vec![ClapPluginInfo {
            id: SURGE_ID.into(), name: "Surge XT".into(), vendor: "Surge Synth Team".into(), features: vec!["instrument".into()],
        }];
        app.catalog_roots = Some(vec![]); // machine-independent: only scan evidence

        assert!(app.catalog_installed(crate::catalog::find("surge-xt").unwrap()), "surge-xt detected from the CLAP scan");
        assert!(!app.catalog_installed(crate::catalog::find("setbfree").unwrap()), "empty roots + no scan URI = not installed");

        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("list_instruments", &serde_json::json!({}))).unwrap();
        let instruments = v["instruments"].as_array().unwrap();
        assert_eq!(instruments.len(), 5);
        let surge = instruments.iter().find(|e| e["id"] == "surge-xt").unwrap();
        assert_eq!(surge["installed"], serde_json::json!(true));
        assert_eq!(surge["format"], serde_json::json!("CLAP"));
        let yanes = instruments.iter().find(|e| e["id"] == "yanes").unwrap();
        assert_eq!(yanes["route"], serde_json::json!("source"));
        assert_eq!(yanes["url"], serde_json::json!("https://github.com/andrewfader/yanes"));

        // Refused without confirm, nothing starts.
        let r = app.exec_tool("install_instrument", &serde_json::json!({"id": "setbfree"}));
        assert!(r.starts_with("not installed"), "{r}");
        assert!(app.install.is_none(), "refused install starts nothing");
        // Accepted with confirm.
        let r = app.exec_tool("install_instrument", &serde_json::json!({"id": "setbfree", "confirm": true}));
        assert!(r.contains("installing 'setbfree'"), "{r}");
        assert!(app.install.as_ref().unwrap().running, "test-stub install runs");
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("list_instruments", &serde_json::json!({}))).unwrap();
        assert_eq!(v["instruments"].as_array().unwrap().iter().find(|e| e["id"] == "setbfree").unwrap()["installing"], serde_json::json!(true));
        // A second install is refused while one runs; an unknown id errors.
        assert!(app.exec_tool("install_instrument", &serde_json::json!({"id": "yanes", "confirm": true})).contains("already installing"));
        assert!(app.exec_tool("install_instrument", &serde_json::json!({"id": "nope", "confirm": true})).contains("no catalog instrument"));
    }

    #[test]
    fn instrument_catalog_renders_install_cards_in_stage() {
        let mut app = DemoApp::new_with_sink(None);
        app.plugins_loaded = true;
        app.plugins_clap = vec![ClapPluginInfo {
            id: SURGE_ID.into(), name: "Surge XT".into(), vendor: "Surge Synth Team".into(), features: vec!["instrument".into()],
        }];
        app.catalog_roots = Some(vec![]); // only Surge XT reads as installed
        app.stage_view = StageView::Plugins;
        app.plugin_category = PluginCategory::Install;
        let ctx = egui::Context::default();
        for _ in 0..2 { frame(&mut app, &ctx, vec![]); }
        let texts = frame(&mut app, &ctx, vec![]);
        for name in ["setBfree", "Surge XT", "Dexed", "Vitalium", "YANES"] {
            assert!(texts.iter().any(|t| t == name), "catalog shows {name}: {texts:?}");
        }
        assert!(texts.iter().any(|t| t == "Install"), "an Install button is painted: {texts:?}");
        assert!(texts.iter().any(|t| t == "Installed"), "installed entries show an Installed badge: {texts:?}");
    }

    #[test]
    fn keys_face_shows_one_or_two_midi_outs() {
        let (mut app, ctx) = demo();
        assert_eq!(out_labels(&app), vec!["MIDI OUT"], "unsplit: one MIDI OUT");
        let (chip, _) = app.split_rects(keys(&app)).expect("Keys face has a SPLIT chip");
        { let p = screen(&app, chip); click(&mut app, &ctx, p); }
        let k = keys(&app);
        assert_eq!(app.units[k].split, Some(60), "chip splits at C4");
        assert_eq!(out_labels(&app), vec!["LOW", "HIGH"], "split: LOW + HIGH MIDI outs");
        assert!(app.units[k].kind(Dir::Out, 1) == PortKind::Midi, "HIGH is a MIDI port");
        assert_eq!(app.base_graph.node(app.units[k].node_id).unwrap().split, Some(60), "sessions see it");
        // Existing cables stay on LOW.
        assert!(app.cables.iter().filter(|c| c.src == k).all(|c| c.src_port == 0 && c.dying < 0.0));
        // The point readout + keyboard marker paint without panicking.
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "C4"), "point readout shows the note name");
        assert!(texts.iter().any(|t| t == "SPLIT"));
        let (chip, _) = app.split_rects(keys(&app)).unwrap();
        { let p = screen(&app, chip); click(&mut app, &ctx, p); }
        assert_eq!(app.units[keys(&app)].split, None);
        assert_eq!(out_labels(&app), vec!["MIDI OUT"]);
    }

    #[test]
    fn unsplitting_unplugs_high_cables_with_a_notice() {
        let (mut app, ctx) = demo();
        let k = keys(&app);
        assert!(app.set_keys_split(k, Some(60)));
        // Move Surge B's cable to HIGH.
        let b = app.cables.iter().position(|c| c.src == k && app.units[c.dst].node_id == 4).unwrap();
        let id = app.cables[b].id;
        app.cables.remove(b);
        app.add_cable_id(id, k, 1, app.units.iter().position(|u| u.node_id == 4).unwrap(), 0, PortKind::Midi);
        frame(&mut app, &ctx, vec![]);
        assert!(app.split_footer().contains("LOW → Surge A") && app.split_footer().contains("HIGH → Surge B"),
            "matrix footer reflects the Keys split: {}", app.split_footer());
        assert!(app.set_keys_split(k, None));
        assert!(app.live_cable(id).is_none(), "HIGH cable unplugged");
        assert!(app.cables.iter().any(|c| c.src == k && c.src_port == 0 && c.dying < 0.0), "LOW cable kept");
        assert!(app.patch_notice.as_ref().map_or(false, |(m, _)| m.contains("HIGH")), "notice explains the unplug");
        frame(&mut app, &ctx, vec![]); // dying HIGH cable still animates without its jack
    }

    #[test]
    fn unsplit_off_half_is_part_of_the_one_split_control() {
        let (mut app, ctx) = demo();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        frame(&mut app, &ctx, vec![]);
        let (_, off_half) = app.split_rects(keys(&app)).unwrap();
        { let p = screen(&app, off_half); click(&mut app, &ctx, p); }
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c));
        assert!(sent.iter().any(|c| matches!(c, Command::SetNodeSplit { node: 0, point: Some(60) })),
            "clicking the OFF half of \"SPLIT · OFF\" splits: {sent:?}");
    }

    #[test]
    fn split_chip_sends_set_node_split_and_mirrors_the_engine() {
        let (mut app, ctx) = demo();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (mut ebus, edrain) = command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::default(), rules: Vec::new(), ..Default::default() }));
        engine.set_graph_pub(pub_.clone());
        app.graph_pub = Some(pub_.clone());
        let mut pump = |drain: &mut crate::control::CommandDrain| {
            let mut sent = Vec::new();
            drain.drain(|c| sent.push(c));
            let splits: Vec<(u32, Option<u8>)> = sent.iter().filter_map(|c| match c {
                Command::SetNodeSplit { node, point } => Some((*node, *point)),
                _ => None,
            }).collect();
            for c in sent { ebus.send(c); }
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
            splits
        };
        frame(&mut app, &ctx, vec![]);
        pump(&mut drain);
        frame(&mut app, &ctx, vec![]);
        let units_before = app.units.len();
        let (chip, _) = app.split_rects(keys(&app)).unwrap();
        { let p = screen(&app, chip); click(&mut app, &ctx, p); }
        assert_eq!(app.units[keys(&app)].split, None, "no local guess while an engine is attached");
        assert_eq!(pump(&mut drain), vec![(0, Some(60))], "chip sends SetNodeSplit(keys, 60)");
        frame(&mut app, &ctx, vec![]); // sync_from_engine adopts it
        assert_eq!(app.units[keys(&app)].split, Some(60));
        assert_eq!(out_labels(&app), vec!["LOW", "HIGH"]);
        assert_eq!(app.shared_graph.lock().unwrap().node(0).unwrap().split, Some(60), "bridge mirror follows");
        assert_eq!(app.units.len(), units_before);
        assert_ne!(app.session_msg, "Patch updated from MCP/CLI", "a split must not rebuild the rack");

        // Drag the point readout up two semitones (12px).
        let (_, readout) = app.split_rects(keys(&app)).unwrap();
        let from = screen(&app, readout);
        { let to = from - Vec2::new(0.0, 12.0 * app.xf.sy); drag(&mut app, &ctx, from, to); }
        let sent = pump(&mut drain);
        assert_eq!(sent.last(), Some(&(0, Some(62))), "drag sets the point: {sent:?}");
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.units[keys(&app)].split, Some(62));
        assert_eq!(engine.graph().node(0).unwrap().split, Some(62));
    }

    #[test]
    fn zone_pulses_only_the_jack_the_note_leaves() {
        let (mut app, _ctx) = demo();
        let k = keys(&app);
        app.set_keys_split(k, Some(60));
        let b = app.cables.iter().position(|c| c.src == k && app.units[c.dst].node_id == 4).unwrap();
        let id = app.cables[b].id;
        app.cables.remove(b);
        app.add_cable_id(id, k, 1, app.units.iter().position(|u| u.node_id == 4).unwrap(), 0, PortKind::Midi);
        let pulses = |app: &DemoApp, port: usize| app.cables.iter()
            .filter(|c| c.src == k && c.src_port == port)
            .map(|c| c.pulses.iter().filter(|p| p.active).count()).sum::<usize>();
        app.fire_keys(59, 0.9);
        assert!(pulses(&app, 0) > 0 && pulses(&app, 1) == 0, "59 → LOW only");
        app.fire_keys(60, 0.9);
        assert!(pulses(&app, 1) > 0, "60 → HIGH");
    }

    #[test]
    fn top_strip_has_no_split_or_transport_chip() {
        let (mut app, ctx) = demo();
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "Matrix"), "the nav strip painted: {texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Split @")), "global split chip is gone");
        assert!(!texts.iter().any(|t| t == "Transport"), "Transport nav chip is gone");
        // F10 no longer opens a Transport window — it toggles the Mixer stage.
        let f10 = |pressed| egui::Event::Key { key: Key::F10, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
        frame(&mut app, &ctx, vec![f10(true), f10(false)]);
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(!texts.iter().any(|t| t == "Transport"), "F10 opens no Transport window");
        assert_eq!(app.stage_view, StageView::Mixer, "F10 opens the mixer");
        assert!(texts.iter().any(|t| t == "Mixer"), "Mixer nav chip is painted");
    }

    #[test]
    fn agent_operates_on_all_rigs() {
        // Rule 6: every rig tool exists in the schema AND in exec_tool.
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for t in ["get_rigs", "add_rig", "delete_rig", "select_rig"] { assert!(names.contains(&t), "schema has {t}"); }
        let mut app = DemoApp::new_with_sink(None);
        let r = app.exec_tool("add_rig", &serde_json::json!({"name": "Bass"}));
        assert!(r.contains("Added Bass"), "{r}");
        assert_eq!(app.rig_count(), 2);
        let rigs: serde_json::Value = serde_json::from_str(&app.exec_tool("get_rigs", &serde_json::json!({}))).unwrap();
        assert_eq!(rigs["rigs"].as_array().unwrap().len(), 2);
        assert_eq!(rigs["rigs"][1]["name"], "Bass");
        assert_eq!(rigs["rigs"][1]["units"].as_array().unwrap().len(), 3);
        let r = app.exec_tool("select_rig", &serde_json::json!({"rig": 0}));
        assert!(r.contains("showing rig 0") && app.active_rig == 0, "{r}");
        assert!(app.exec_tool("add_node", &serde_json::json!({"kind": "dsp", "dsp": "reverb", "rig": 5})).contains("no rig 5"));
        assert!(app.exec_tool("add_node", &serde_json::json!({"kind": "dsp", "dsp": "reverb", "rig": 1})).contains("rig 1 'Bass'"));
        // Destructive: refused without confirm, done with it.
        assert!(app.exec_tool("delete_rig", &serde_json::json!({"rig": 0})).starts_with("not deleted"));
        assert_eq!(app.rig_count(), 2);
        let r = app.exec_tool("delete_rig", &serde_json::json!({"rig": 0, "confirm": true}));
        assert!(r.contains("Deleted"), "{r}");
        assert_eq!(app.rig_count(), 1);
        assert_eq!(app.rig_name(0), "Bass");
    }

    #[test]
    fn set_split_tool_parity() {
        // Rule 6: schema AND exec_tool.
        let tools = crate::assistant::tools();
        assert!(tools.as_array().unwrap().iter().any(|t| t["function"]["name"] == "set_split"));
        let (mut app, _ctx) = demo();
        let r = app.exec_tool("set_split", &serde_json::json!({"point": 64}));
        assert!(r.contains("split at note 64"), "{r}");
        assert_eq!(app.units[keys(&app)].split, Some(64));
        app.publish_patch();
        let g: serde_json::Value = serde_json::from_str(&app.exec_tool("get_graph", &serde_json::json!({}))).unwrap();
        let k = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == serde_json::json!(0)).cloned().unwrap();
        assert_eq!(k["split"], serde_json::json!(64), "get_graph exposes split");
        let m: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(m["split"][0]["point"], serde_json::json!(64));
        let r = app.exec_tool("set_split", &serde_json::json!({"node": 1, "point": 60}));
        assert!(r.contains("only keyboards split"), "{r}");
        app.exec_tool("set_split", &serde_json::json!({"node": 0}));
        assert_eq!(app.units[keys(&app)].split, None, "no point = unsplit");
    }

    #[test]
    fn split_round_trips_through_sessions() {
        let (mut app, _ctx) = demo();
        let k = keys(&app);
        app.set_keys_split(k, Some(55));
        let b = app.cables.iter().position(|c| c.src == k && app.units[c.dst].node_id == 4).unwrap();
        let id = app.cables[b].id;
        app.cables.remove(b);
        app.add_cable_id(id, k, 1, app.units.iter().position(|u| u.node_id == 4).unwrap(), 0, PortKind::Midi);
        let s = app.capture_session("split").unwrap();
        let json = serde_json::to_string(&s.graph).unwrap();
        let back: Graph = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node(0).unwrap().split, Some(55));
        let mut fresh = DemoApp::new_with_sink(None);
        fresh.apply_session(&crate::session::Session::new("t", back)).unwrap();
        let fk = fresh.keys_unit.unwrap();
        assert_eq!(fresh.units[fk].split, Some(55));
        assert_eq!(out_labels(&fresh), vec!["LOW", "HIGH"]);
        assert!(fresh.cables.iter().any(|c| c.src == fk && c.src_port == 1 && fresh.units[c.dst].node_id == 4),
            "HIGH cable restored on port 1");
    }

    /// Demo patch + two metronomes wired to the master.
    fn metro_app() -> (DemoApp, egui::Context, u32, u32) {
        let mut app = DemoApp::new_with_sink(None);
        let mut g = Graph::demo_patch();
        let m1 = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
        let m2 = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK 2"));
        g.connect_with_id(10, PortRef { node: m1, port: 0 }, PortRef { node: 3, port: 0 }, CPortKind::Audio);
        g.connect_with_id(11, PortRef { node: m2, port: 0 }, PortRef { node: 3, port: 0 }, CPortKind::Audio);
        app.build_rack(&g, &[]);
        let ctx = egui::Context::default();
        for _ in 0..3 { frame(&mut app, &ctx, vec![]); }
        (app, ctx, m1, m2)
    }

    fn unit_of(app: &DemoApp, node: u32) -> usize { app.units.iter().position(|u| u.node_id == node).unwrap() }

    #[test]
    fn metronome_face_has_bpm_steppers_and_drag() {
        let (mut app, ctx, m1, m2) = metro_app();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let u1 = unit_of(&app, m1);
        let b = app.bpm_rects(u1).expect("metronome face shows a BPM control");
        let chip = app.transport_chip_rect(u1).unwrap();
        assert!(!b.value.intersects(chip), "BPM never covers the Play/Stop chip");
        assert!(app.unit_rects[u1].contains_rect(b.value), "BPM stays on its face");
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "96") && texts.iter().any(|t| t == "BPM"), "face shows the tempo");
        let bpm_sent = |drain: &mut crate::control::CommandDrain| {
            let mut v = Vec::new();
            drain.drain(|c| if let Command::SetBpm { bpm } = c { v.push(bpm) });
            v
        };
        bpm_sent(&mut drain);
        // + / − steppers.
        let plus = b.plus.unwrap_or_else(|| panic!("steppers fit: unit {:?} chip {:?} bpm {:?}", app.unit_rects[u1], chip, b));
        { let p = screen(&app, plus); click(&mut app, &ctx, p); }
        assert_eq!(app.bpm, 97.0);
        assert_eq!(bpm_sent(&mut drain), vec![97.0]);
        let minus = app.bpm_rects(u1).unwrap().minus.unwrap();
        { let p = screen(&app, minus); click(&mut app, &ctx, p); }
        { let p = screen(&app, minus); click(&mut app, &ctx, p); }
        assert_eq!(app.bpm, 95.0);
        // Drag the value up 30px → +10 BPM (one per 3px).
        let from = screen(&app, app.bpm_rects(u1).unwrap().value);
        { let to = from - Vec2::new(0.0, 30.0 * app.xf.sy); drag(&mut app, &ctx, from, to); }
        assert_eq!(app.bpm, 105.0, "drag sets the tempo");
        assert_eq!(bpm_sent(&mut drain).last(), Some(&105.0), "engine gets SetBpm");
        assert!(!ctx.memory(|m| m.focused().is_some()), "no widget focus: rack keys keep working (rule 12)");
        // A drag/click on the BPM field never toggles Play/Stop.
        assert!(!app.units[u1].playing);
        // The second metronome shows/sets the SAME global tempo.
        let u2 = unit_of(&app, m2);
        let plus2 = app.bpm_rects(u2).and_then(|b| b.plus).expect("second face too");
        { let p = screen(&app, plus2); click(&mut app, &ctx, p); }
        assert_eq!(app.bpm, 106.0);
        // Clamped to the engine range.
        app.set_bpm(400.0);
        assert_eq!(app.bpm, 300.0);
    }

    #[test]
    fn narrow_metronome_still_shows_chip_and_bpm() {
        let (mut app, _ctx, m1, _m2) = metro_app();
        let u = unit_of(&app, m1);
        let r = app.unit_rects[u];
        for w in [150.0f32, 200.0, 240.0, 320.0] {
            app.unit_rects[u] = Rect::from_min_size(r.min, Vec2::new(w, r.height()));
            let chip = app.transport_chip_rect(u).expect("chip");
            let b = app.bpm_rects(u).unwrap_or_else(|| panic!("BPM at width {w}"));
            for rr in [Some(b.value), b.minus, b.plus].into_iter().flatten() {
                assert!(!rr.intersects(chip), "w={w}: overlaps chip");
                assert!(app.unit_rects[u].contains_rect(rr), "w={w}: leaves the face");
            }
        }
    }

    #[test]
    fn vitalium_and_dexed_editor_support() {
        let _one_at_a_time = crate::lv2::VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !lv2_discover().iter().any(|p| p.uri == "urn:distrho:vitalium") {
            return;
        }
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        app.add_lv2_instrument("urn:distrho:vitalium", "Vitalium");
        frame(&mut app, &ctx, vec![]);
        let u = app.units.iter().find(|u| u.lv2_inst).expect("Vitalium unit");
        assert!(u.lv2_inst, "Vitalium must be classified as lv2_inst");
        assert_eq!(u.sub, "lv2 synth", "Vitalium subtitle must be lv2 synth");
        let node_id = u.node_id;
        app.toggle_lv2_editor(node_id, "urn:distrho:vitalium", "VITALIUM");
        assert!(app.editor_open);
        assert_eq!(app.lv2_editor_node, Some(node_id));
        assert!(!app.lv2_editor_controls.is_empty(), "Vitalium controls should populate");
        assert!(!app.lv2_editor_presets.is_empty(), "Vitalium presets should populate");
        frame(&mut app, &ctx, vec![]);
        assert!(!app.dexed_patches().is_empty() || !crate::host::list_clap_plugins().iter().any(|p| p.id == DEXED_ID));
    }

    #[test]
    fn sync_mirrors_remote_cc_maps() {
        let mut app = DemoApp::new_with_sink(None);
        let remote_rule = crate::matrix::Rule::new(SourceId::Hardware(0), 1).map_cc(74, 42);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub {
            rev: 1,
            graph: app.base_graph.clone(),
            rules: vec![remote_rule],
            ..Default::default()
        }));
        app.graph_pub = Some(pub_);
        app.sync_from_engine();
        let e = app.rule_edits.get(&(SourceId::Hardware(0), 1)).expect("remote cc_map mirrored");
        assert_eq!(e.cc_map, vec![crate::matrix::CcMapping { cc: 74, param: 42, target: crate::matrix::CcTarget::PluginParam }]);
        assert_eq!(e.cell_code(), "1 CC");
    }

    #[test]
    fn cc_learn_binds_next_hardware_cc() {
        let (mut app, ctx) = demo();
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let out = app.exec_tool("learn_cc", &serde_json::json!({"node": 1, "source": "hw:0", "param": 123}));
        assert!(out.contains("\"armed\":true"), "{out}");
        assert!(app.cc_learn.is_some());
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(0), 0, 74, 100));
        frame(&mut app, &ctx, vec![]);
        assert!(app.cc_learn.is_none(), "MIDI-learn disarms after binding");
        let e = app.rule_edits.get(&(SourceId::Hardware(0), 1)).expect("learned binding stored");
        assert_eq!(e.cc_map, vec![crate::matrix::CcMapping { cc: 74, param: 123, target: crate::matrix::CcTarget::PluginParam }]);
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::MatrixCcMap { source: SourceId::Hardware(0), dest: 1, cc: 74, param: Some(123), target: crate::matrix::CcTarget::PluginParam })));
    }

    #[test]
    fn cc_tools_parity_and_roundtrip() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for t in ["set_cc_map", "clear_cc_map", "list_cc_maps", "list_params", "learn_cc"] {
            assert!(names.contains(&t), "assistant schema missing {t}");
        }
        let (mut app, _ctx) = demo();
        let (bus, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let r = app.exec_tool("set_cc_map", &serde_json::json!({"node": 1, "source": "keys", "cc": 74, "param": 55}));
        assert!(r.contains("\"cc\":74") && r.contains("\"param\":55"), "{r}");
        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("list_cc_maps", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["cc_maps"][0]["bindings"][0]["cc"], serde_json::json!(74));
        let mat: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(mat["rule_edits"][0]["cc_map"][0]["param"], serde_json::json!(55));
        let dsp_params: serde_json::Value = serde_json::from_str(&app.exec_tool("list_params", &serde_json::json!({"node": 2}))).unwrap();
        assert_eq!(dsp_params["params"][0]["name"], serde_json::json!("Time"));
        app.exec_tool("clear_cc_map", &serde_json::json!({"node": 1, "source": "keys"}));
        assert!(!app.rule_edits.contains_key(&(SourceId::Keys, 1)));
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::MatrixCcMap { dest: 1, cc: 74, param: None, .. })));
    }

    #[test]
    fn rule_editor_paints_cc_maps_and_learn() {
        let (mut app, ctx) = demo();
        let inst = app.matrix_sinks()[0];
        let node = app.units[inst].node_id;
        app.exec_tool("set_cc_map", &serde_json::json!({"node": node, "source": "keys", "cc": 74, "param": 7}));
        app.stage_view = StageView::Matrix;
        app.rule_editor = Some((inst, MatrixRow::Keys));
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t.contains("whitelist active")), "CC Maps hint painted: {texts:?}");
        assert!(texts.iter().any(|t| t == "Add") && texts.iter().any(|t| t == "Learn"), "Add/Learn buttons painted: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("74")), "CC 74 binding painted: {texts:?}");
        assert!(texts.iter().any(|t| t == "1 CC"), "Matrix cell reflects 1 CC: {texts:?}");
    }

    fn rclick(app: &mut DemoApp, ctx: &egui::Context, pos: Pos2) -> Vec<String> {
        let rp = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(app, ctx, vec![egui::Event::PointerMoved(pos)]);
        frame(app, ctx, vec![rp(true)]);
        frame(app, ctx, vec![rp(false)])
    }

    #[test]
    fn clap_units_name_the_loaded_instrument_and_patch() {
        let (mut app, ctx) = demo();
        let (slot, node) = app.clap_rigs()[0];
        app.patch_names[slot] = Some("Wurli".into());
        let texts = frame(&mut app, &ctx, vec![]);
        let u = app.units.iter().find(|u| u.node_id == node).unwrap();
        assert_eq!(u.sub, "Surge XT · Wurli");
        // Tracked captions paint one glyph per shape: join them.
        assert!(texts.concat().contains("SURGE XT · WURLI"), "subtitle repainted: {texts:?}");
    }

    #[test]
    fn agent_undo_reverts_one_whole_job_and_redo_restores_it() {
        let (mut app, ctx) = demo();
        assert_eq!(app.rig_count(), 1);
        // One assistant job: add a rig AND turn rig A down.
        app.exec_tool("add_rig", &serde_json::json!({"name": "Bass"}));
        app.exec_tool("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.3}));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.rig_count(), 2);
        assert!((app.rig_mix[0].vol - 0.3).abs() < 1e-6);
        assert_eq!(app.undo_past.len(), 1, "one job = one undo step");

        let out = app.exec_tool("undo", &serde_json::json!({}));
        frame(&mut app, &ctx, vec![]);
        assert!(out.contains("undid"), "{out}");
        assert_eq!(app.rig_count(), 1, "the added rig is gone");
        assert!((app.rig_mix[0].vol - 1.0).abs() < 1e-6, "and the mix change with it");

        app.exec_tool("redo", &serde_json::json!({}));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.rig_name(1), "Bass", "rig names survive undo/redo");
        assert!((app.rig_mix[0].vol - 0.3).abs() < 1e-6);
        assert!(app.exec_tool("redo", &serde_json::json!({})).contains("nothing to redo"));


        // A new chat request opens a new step; remote calls split on a 3 s gap.
        app.submit_chat("louder".into());
        app.exec_tool("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.9}));
        assert_eq!(app.undo_past.len(), 2);
        app.exec_tool_with_origin("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.5}), AgentOrigin::Remote);
        app.exec_tool_with_origin("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.4}), AgentOrigin::Remote);
        assert_eq!(app.undo_past.len(), 3, "a remote burst is one step");
        app.ui_time += REMOTE_JOB_GAP + 1.0;
        app.exec_tool_with_origin("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.2}), AgentOrigin::Remote);
        assert_eq!(app.undo_past.len(), 4, "after a pause, a new step");
        // Read-only tools never open a step.
        app.ui_time += REMOTE_JOB_GAP + 1.0;
        app.exec_tool_with_origin("get_rigs", &serde_json::json!({}), AgentOrigin::Remote);
        assert_eq!(app.undo_past.len(), 4);

        // Per-voice source switches survive an undo of a LATER job (#17):
        // job 1 turns the keyboard off, job 2 changes the mix, undo job 2.
        app.submit_chat("keyboard off".into());
        let node = app.clap_rigs()[0].1;
        app.exec_tool("set_rig_source", &serde_json::json!({"node": node, "keyboard": false, "hardware": true}));
        app.submit_chat("quieter".into());
        app.exec_tool("set_rig_mix", &serde_json::json!({"rig": 0, "vol": 0.2}));
        app.exec_tool("undo", &serde_json::json!({}));
        assert!(!app.rig_sources[0].0, "undoing the mix change keeps the keyboard off");
        assert!(app.rig_sources[0].1);
    }

    #[test]
    fn bridge_asks_for_an_undo_snapshot_before_a_remote_edit() {
        let (mut app, _ctx) = demo();
        let (bus, _drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        let (tx, rx) = std::sync::mpsc::channel();
        crate::bridge::UI_INBOX.lock().unwrap().push((crate::bridge::BEFORE_REMOTE_EDIT.into(), serde_json::json!({}), tx));
        app.serve_ui_inbox();
        assert_eq!(rx.recv().unwrap(), "ok");
        assert_eq!(app.undo_past.len(), 1, "the pre-edit rack is on the undo stack");
        assert!(Command::Connect { id: 1, from: PortRef { node: 0, port: 0 }, to: PortRef { node: 1, port: 0 }, kind: CPortKind::Midi }.is_rack_edit());
        assert!(!Command::NoteOn { note: 60, velocity: 1.0, source: SourceId::Keys }.is_rack_edit(), "playing is not an edit");
    }

    #[test]
    fn top_bar_separates_views_from_bottom_panels() {
        let (mut app, ctx) = demo();
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "Rack"), "Rack is an explicit view: {texts:?}");
        let (v, d) = (app.view_group_rect, app.dock_group_rect);
        assert!(v.is_positive() && d.is_positive() && v.right() < d.left(), "two separate groups: {v:?} {d:?}");
        // Views are exclusive: Mixer, Mixer again (stays), then Rack.
        let mixer = v.right_center() - Vec2::new(12.0, 0.0);
        click(&mut app, &ctx, mixer);
        assert_eq!(app.stage_view, StageView::Mixer);
        click(&mut app, &ctx, mixer);
        assert_eq!(app.stage_view, StageView::Mixer, "the active view stays selected");
        let rack = app.view_group_rect.left_center() + Vec2::new(12.0, 0.0);
        click(&mut app, &ctx, rack);
        assert_eq!(app.stage_view, StageView::Rack, "Rack button returns to the rack");
        // Bottom panel toggles independently of the view.
        let kb = app.dock_group_rect.left_center() + Vec2::new(12.0, 0.0);
        click(&mut app, &ctx, kb);
        assert!(app.keyboard_open() && app.stage_view == StageView::Rack);
    }

    #[test]
    fn rack_knob_midi_learn_bind_follow_and_cancel() {
        let (mut app, ctx) = demo();
        let (bus, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        frame(&mut app, &ctx, vec![]);
        drain.drain(|_| {}); // clear initial SetParam/Connect burst

        // Surge A is unit 1, LEVEL knob is index 1 (param = Some(P_LEVEL)).
        let surge_unit = 1usize;
        let level_idx = 1usize;
        let level_pos = app.xf.p(app.knob_center(surge_unit, level_idx));

        // 1. Right-click enters learning state and paints "LEARN".
        let texts = rclick(&mut app, &ctx, level_pos);
        assert!(app.cc_learn.is_some(), "right-click armed knob MIDI learn");
        assert!(texts.iter().any(|t| t == "LEARN"), "LEARN badge painted: {texts:?}");

        // 2. Right-click again opens the menu (learn still armed); its first
        // button cancels learning (#20).
        rclick(&mut app, &ctx, level_pos);
        assert!(app.cc_learn.is_some() && app.knob_menu.is_some(), "second right-click opens the menu");
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "Cancel MIDI learn"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Reset to default"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Clear MIDI map"), "nothing bound yet: {texts:?}");
        click(&mut app, &ctx, knob_menu_buttons(&ctx)[0].center());
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(app.cc_learn.is_none() && app.knob_menu.is_none(), "Cancel MIDI learn disarms");
        assert!(!texts.iter().any(|t| t == "LEARN"));

        // 3. Esc cancels learning on the rack.
        rclick(&mut app, &ctx, level_pos);
        assert!(app.cc_learn.is_some());
        frame(&mut app, &ctx, vec![
            egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Key { key: Key::Escape, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ]);
        assert!(app.cc_learn.is_none(), "Esc cancels knob learn on Rack");

        // 4. Arm and send CC 74 from Hardware(2): binds for Hardware(2) and paints "CC 74".
        rclick(&mut app, &ctx, level_pos);
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(2), 0, 74, 50));
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(app.cc_learn.is_none(), "disarmed after binding");
        assert!(texts.iter().any(|t| t == "CC 74"), "CC 74 tag painted: {texts:?}");
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(
            c,
            Command::MatrixCcMap {
                source: SourceId::Hardware(2),
                dest: 1,
                cc: 74,
                param: Some(P_LEVEL),
                target: crate::matrix::CcTarget::Knob,
            }
        )), "sent MatrixCcMap with CcTarget::Knob: {cmds:?}");
        // The binding CC itself counts: the engine saw it before the mapping
        // existed, so the knob adopts it and publish sends one catch-up SetParam.
        assert!((app.units[surge_unit].knobs[level_idx].value - 50.0 / 127.0).abs() < 1e-6,
            "knob adopts the CC that completed the learn");
        assert!(cmds.iter().any(|c| matches!(c, Command::SetParam { node: 1, param: P_LEVEL, .. })),
            "catch-up SetParam for the learn CC: {cmds:?}");

        // 5. Turning the bound hardware knob updates the on-screen knob without re-sending SetParam.
        let before_val = app.units[surge_unit].knobs[level_idx].value;
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(2), 0, 74, 127));
        frame(&mut app, &ctx, vec![]);
        assert!((app.units[surge_unit].knobs[level_idx].value - 1.0).abs() < 1e-6);
        assert_ne!(before_val, 1.0);
        let mut feedback_cmds = Vec::new();
        drain.drain(|c| feedback_cmds.push(c));
        assert!(!feedback_cmds.iter().any(|c| matches!(c, Command::SetParam { node: 1, param: P_LEVEL, .. })),
            "adopting CC must not re-send SetParam: {feedback_cmds:?}");

        // 6. Same CC from another hardware device does not move the Hardware(2)-bound knob.
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(0), 0, 74, 0));
        frame(&mut app, &ctx, vec![]);
        assert!((app.units[surge_unit].knobs[level_idx].value - 1.0).abs() < 1e-6);
    }

    /// Buttons of the knob context menu, top to bottom (previous pass).
    /// Buttons = click + focusable, no drag; the Area move widget is not focusable.
    fn knob_menu_buttons(ctx: &egui::Context) -> Vec<Rect> {
        let menu_layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("knob_midi_menu"));
        ctx.viewport(|v| {
            let mut btns: Vec<Rect> = v.prev_pass.widgets.get_layer(menu_layer)
                .filter(|w| w.sense.senses_click() && !w.sense.senses_drag() && w.sense.is_focusable())
                .map(|w| w.rect)
                .collect();
            btns.sort_by(|a, b| a.top().total_cmp(&b.top()));
            btns
        })
    }

    /// #20: an unbound knob can be reset from the menu — right-click arms
    /// learn, right-click again opens Reset / Cancel learn.
    #[test]
    fn rack_knob_unbound_reset_from_the_learn_menu() {
        let (mut app, ctx) = demo();
        frame(&mut app, &ctx, vec![]);
        let (surge_unit, level_idx) = (1usize, 1usize);
        let level_pos = app.xf.p(app.knob_center(surge_unit, level_idx));
        app.units[surge_unit].knobs[level_idx].value = 0.1;
        rclick(&mut app, &ctx, level_pos);
        rclick(&mut app, &ctx, level_pos);
        frame(&mut app, &ctx, vec![]);
        let btns = knob_menu_buttons(&ctx);
        assert_eq!(btns.len(), 2, "Cancel MIDI learn + Reset to default");
        click(&mut app, &ctx, btns[1].center());
        let k = &app.units[surge_unit].knobs[level_idx];
        assert!((k.value - k.default).abs() < 1e-6, "Reset to default");
        assert!(app.cc_learn.is_none(), "reset also ends the learn the right-click armed");
        assert!(app.knob_menu.is_none());
    }

    /// #24: the menu names its knob by node + param, so a unit reorder under
    /// an open menu still resolves the same knob.
    #[test]
    fn knob_menu_follows_its_knob_by_id_not_index() {
        let (mut app, ctx) = demo();
        frame(&mut app, &ctx, vec![]);
        let (surge_unit, level_idx) = (1usize, 1usize);
        let node = app.units[surge_unit].node_id;
        let menu = KnobMenu { node, param: P_LEVEL, target: crate::matrix::CcTarget::Knob, pos: Pos2::ZERO };
        assert_eq!(app.knob_menu_target(&menu), Some((surge_unit, level_idx)));
        app.units.swap(0, surge_unit);
        assert_eq!(app.knob_menu_target(&menu), Some((0, level_idx)), "follows the unit to its new index");
        app.units.retain(|u| u.node_id != node);
        assert_eq!(app.knob_menu_target(&menu), None, "a removed knob closes the menu");
    }

    #[test]
    fn rack_knob_bound_menu_relearn_and_clear() {
        let (mut app, ctx) = demo();
        let (bus, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        frame(&mut app, &ctx, vec![]);

        let surge_unit = 1usize;
        let level_idx = 1usize;
        let level_pos = app.xf.p(app.knob_center(surge_unit, level_idx));

        // Bind CC 74 on Hardware(2).
        rclick(&mut app, &ctx, level_pos);
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(2), 0, 74, 64));
        frame(&mut app, &ctx, vec![]);

        // Right-click bound knob opens the menu with "MIDI learn again" and "Clear MIDI map".
        // The egui Area needs one extra frame after the sizing pass to paint.
        rclick(&mut app, &ctx, level_pos);
        assert!(app.knob_menu.is_some(), "bound knob opens context menu");
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "MIDI learn again"), "menu shows relearn: {texts:?}");
        assert!(texts.iter().any(|t| t == "Clear MIDI map"), "menu shows clear: {texts:?}");

        // Find and click "MIDI learn again" widget rect from the foreground layer.
        let menu_buttons = || knob_menu_buttons(&ctx);
        let relearn_rect = menu_buttons()[0];
        click(&mut app, &ctx, relearn_rect.center());
        assert!(app.knob_menu.is_none());
        assert!(app.cc_learn.is_some(), "MIDI learn again armed");

        // Send CC 14 from Hardware(1): replaces old Hardware(2) CC 74 binding.
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(1), 0, 14, 90));
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "CC 14"), "replaced tag painted: {texts:?}");
        assert!(!app.rule_edits.contains_key(&(SourceId::Hardware(2), 1)), "old binding cleared");

        // Open menu again and click "Clear MIDI map" (2nd button vertically).
        rclick(&mut app, &ctx, level_pos);
        frame(&mut app, &ctx, vec![]);
        let btns = menu_buttons();
        assert!(btns.len() >= 2, "menu has >= 2 buttons");
        drain.drain(|_| {});
        click(&mut app, &ctx, btns[1].center());
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(!app.rule_edits.contains_key(&(SourceId::Hardware(1), 1)), "binding cleared");
        assert!(!texts.iter().any(|t| t == "CC 14"));
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::MatrixCcMap { source: SourceId::Hardware(1), dest: 1, cc: 14, param: None, .. })));
    }

    #[test]
    fn rack_knob_bindings_show_in_matrix_and_survive_rig_edits_and_agent_tools() {
        let (mut app, ctx) = demo();
        let (bus, _) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(bus)));
        frame(&mut app, &ctx, vec![]);

        // Agent parity: list_params exposes knobs, learn_cc / set_cc_map / clear_cc_map work with target="knob".
        let lp: serde_json::Value = serde_json::from_str(&app.exec_tool("list_params", &serde_json::json!({"node": 1}))).unwrap();
        assert!(lp["knobs"].as_array().unwrap().iter().any(|k| k["label"] == "LEVEL" && k["target"] == "knob"));
        let arm = app.exec_tool("learn_cc", &serde_json::json!({"node": 1, "knob": "LEVEL"}));
        assert!(arm.contains("\"target\":\"knob\"") && arm.contains("\"armed\":true"), "{arm}");
        app.hw_cc_queue.lock().unwrap().push_back((SourceId::Hardware(0), 0, 74, 80));
        frame(&mut app, &ctx, vec![]);

        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("list_cc_maps", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["cc_maps"][0]["bindings"][0]["target"], serde_json::json!("knob"));
        assert_eq!(listed["cc_maps"][0]["bindings"][0]["name"], serde_json::json!("LEVEL (knob)"));

        // Matrix rule editor shows knob binding cleanly.
        app.stage_view = StageView::Matrix;
        app.rule_editor = Some((1, MatrixRow::HwDev(0)));
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t.contains("74")), "CC 74 in rule editor: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("LEVEL (knob)")), "knob label in rule editor: {texts:?}");

        // Survives + Rig (edit_rigs) remapping.
        app.stage_view = StageView::Rack;
        app.edit_rigs(None);
        let e = app.rule_edits.get(&(SourceId::Hardware(0), 1)).expect("knob binding survived + Rig");
        assert_eq!(e.cc_map, vec![crate::matrix::CcMapping { cc: 74, param: P_LEVEL, target: crate::matrix::CcTarget::Knob }]);

        // Clear via knob name.
        app.exec_tool("clear_cc_map", &serde_json::json!({"node": 1, "source": "hw:0", "knob": "LEVEL"}));
        assert!(!app.rule_edits.contains_key(&(SourceId::Hardware(0), 1)));
    }

    #[test]
    fn metronome_face_tap_and_count_in_controls_work_and_fit() {
        let (mut app, ctx, m1, _m2) = metro_app();
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let u1 = unit_of(&app, m1);

        // Controls are painted, inside the face, and disjoint from Play/BPM across widths.
        let texts = frame(&mut app, &ctx, vec![]);
        assert!(texts.iter().any(|t| t == "TAP"), "TAP chip painted: {texts:?}");
        assert!(texts.iter().any(|t| t == "COUNT-IN"), "COUNT-IN chip painted: {texts:?}");

        let r_orig = app.unit_rects[u1];
        for w in [150.0f32, 200.0, 240.0, 320.0, 420.0] {
            app.unit_rects[u1] = Rect::from_min_size(r_orig.min, Vec2::new(w, r_orig.height()));
            let chip = app.transport_chip_rect(u1).expect("chip");
            let b = app.bpm_rects(u1).expect("bpm");
            let m = app.metro_action_rects(u1).unwrap_or_else(|| panic!("tap/count-in at width {w}"));
            assert!(!m.tap.intersects(chip) && !m.count_in.intersects(chip), "w={w}: overlaps Play");
            assert!(!m.tap.intersects(b.value) && !m.count_in.intersects(b.value), "w={w}: overlaps BPM");
            assert!(!m.tap.intersects(m.count_in), "w={w}: TAP overlaps COUNT-IN");
            assert!(app.unit_rects[u1].contains_rect(m.tap), "w={w}: TAP outside face");
            assert!(app.unit_rects[u1].contains_rect(m.count_in), "w={w}: COUNT-IN outside face");
        }
        app.unit_rects[u1] = r_orig;
        frame(&mut app, &ctx, vec![]);

        // Clicking COUNT-IN toggles count_in_enabled and sends SetCountIn.
        let ci_pos = screen(&app, app.metro_action_rects(u1).unwrap().count_in);
        click(&mut app, &ctx, ci_pos);
        assert!(app.count_in_enabled);
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetCountIn { enabled: true, bars: 1 })), "sent SetCountIn: {cmds:?}");

        // Shift+T taps tempo without firing laptop piano F#4 (Key::T).
        let (note_tx, note_log) = {
            let log = Arc::new(Mutex::new(Vec::<u8>::new()));
            let l = log.clone();
            let sink: NoteSink = Box::new(move |ev| {
                if ev.velocity > 0.0 { l.lock().unwrap().push(ev.note); }
            });
            (sink, log)
        };
        app.note_sink = Some(note_tx);
        let shift_t = vec![egui::Event::Key {
            key: Key::T,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }];
        frame(&mut app, &ctx, shift_t);
        assert!(note_log.lock().unwrap().is_empty(), "Shift+T must not trigger laptop piano note");
        assert!(app.tap_flash_t > 0.5, "Shift+T flashes TAP control");

        // Deterministic tap tempo averaging updates BPM and sends SetBpm.
        app.tap_tempo.reset();
        app.tap_tempo_at(10.0);
        app.tap_tempo_at(10.5);
        app.tap_tempo_at(11.0);
        assert_eq!(app.bpm, 120.0);
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetBpm { bpm } if (*bpm - 120.0).abs() < 1e-3)));

        // Assistant set_count_in + get_status tool parity.
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert!(names.contains(&"set_count_in"), "assistant tools missing set_count_in");
        let res = app.exec_tool("set_count_in", &serde_json::json!({"enabled": true, "bars": 2}));
        assert!(res.contains("\"enabled\":true") && res.contains("\"bars\":2"), "{res}");
        let st: serde_json::Value = serde_json::from_str(&app.exec_tool("get_status", &serde_json::json!({}))).unwrap();
        assert_eq!(st["count_in_enabled"], serde_json::json!(true));
        assert_eq!(st["count_in_bars"], serde_json::json!(2));
        assert_eq!(st["bpm"], serde_json::json!(120.0));
    }

    #[test]
    fn record_tools_parity_and_failure_surfacing() {
        // Rule 6 + finding #10: record tools exist in the assistant schema AND
        // exec_tool, and record_start does not claim success without a recorder.
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for t in ["record_start", "record_stop", "record_status", "set_record_sync"] {
            assert!(names.contains(&t), "assistant tools missing {t}");
        }

        // No audio engine (bus/recorder absent): record_start must fail loudly.
        let mut app = DemoApp::new_with_sink(None);
        assert!(app.bus.is_none() && app.recorder.is_none());
        let r = app.exec_tool("record_start", &serde_json::json!({}));
        assert!(r.contains("unavailable"), "record_start must not claim success without a recorder: {r}");
        assert!(app.exec_tool("record_stop", &serde_json::json!({})).contains("stopped"));
        assert!(app.exec_tool("record_status", &serde_json::json!({})).contains("\"available\":false"));

        // set_record_sync flips the mirror and, with a bus, sends the command.
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let r = app.exec_tool("set_record_sync", &serde_json::json!({"on": true}));
        assert!(r.contains("\"record_sync\":true"), "{r}");
        assert!(app.record_sync, "exec_tool mirrors record_sync");
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetRecordSync { on: true })), "sent SetRecordSync: {cmds:?}");
    }

    #[test]
    fn measure_mix_tool_parity_and_reset() {
        // Rule 6/23 (Bravo 1): `measure_mix` exists in the assistant schema and
        // in exec_tool, reads the mirrored meter, and reset sends ResetMeter.
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert!(names.contains(&"measure_mix"), "assistant tools missing measure_mix");

        let mut app = DemoApp::new_with_sink(None);
        assert!(app.bus.is_none());
        // Read-only by default: no undo step is opened.
        let r = app.exec_tool("measure_mix", &serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        for key in ["momentary_lufs", "short_term_lufs", "integrated_lufs", "true_peak_db", "low_end_corr", "low_end_mono", "band_db"] {
            assert!(v.get(key).is_some(), "measure_mix missing '{key}': {r}");
        }

        // With a bus, reset=true sends Command::ResetMeter.
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let _ = app.exec_tool("measure_mix", &serde_json::json!({"reset": true}));
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::ResetMeter)), "reset must send ResetMeter: {cmds:?}");
    }
}

#[cfg(test)]
mod stage_views_tests {
    use super::*;

    fn key(k: Key) -> Vec<egui::Event> {
        vec![
            egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
            egui::Event::Key { key: k, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ]
    }

    fn walk_texts(shape: &Shape, texts: &mut Vec<String>) {
        match shape {
            Shape::Text(t) => texts.push(t.galley.text().to_owned()),
            Shape::Vec(v) => for s in v { walk_texts(s, texts) },
            _ => {}
        }
    }

    fn run_frame(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, events: Vec<egui::Event>) -> (Vec<String>, Vec<egui::LayerId>) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        let mut bg = Vec::new();
        for cs in &out.shapes {
            walk_texts(&cs.shape, &mut bg);
        }
        let middle_layers = ctx.memory(|m| {
            m.areas()
                .visible_layer_ids()
                .into_iter()
                .filter(|l| l.order == egui::Order::Middle)
                .collect::<Vec<_>>()
        });
        (bg, middle_layers)
    }

    #[test]
    fn stage_views_are_mutually_exclusive_and_never_open_floating_windows() {
        let mut app = DemoApp::new_with_sink(None);
        app.plugins_loaded = true;
        app.plugins_clap = vec![ClapPluginInfo {
            id: SURGE_ID.into(),
            name: "Surge XT".into(),
            vendor: "Surge Synth Team".into(),
            features: vec!["instrument".into()],
        }];
        app.plugins_lv2 = vec![Lv2PluginInfo {
            uri: "http://example.org/lv2/verb".into(),
            name: "Lush Verb".into(),
            num_ports: 4,
            is_instrument: false,
        }];
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        run_frame(&mut app, &ctx, size, vec![]);
        // Frame 1 runs at ppp 1.0; the app corrects to the monitor scale
        // (1.6) for every later frame — measure the bar once it settled.
        run_frame(&mut app, &ctx, size, vec![]);

        let mut sessions_btns = ctx.viewport(|v| {
            v.prev_pass.widgets.get_layer(egui::LayerId::background())
                .filter(|w| w.sense.senses_click() && w.rect.top() < 50.0)
                .map(|w| w.rect)
                .collect::<Vec<_>>()
        });
        sessions_btns.sort_by(|a, b| a.left().total_cmp(&b.left()));
        assert!(!sessions_btns.is_empty(), "top bar buttons laid out");
        // [0] is Rack (the View group starts with it), [1] Sessions.
        let s_pos = sessions_btns[1].center();
        let ptr = |p, pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        run_frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(s_pos), ptr(s_pos, true), ptr(s_pos, false)]);
        assert_eq!(app.stage_view, StageView::Sessions, "Sessions chip opens Sessions stage");

        let (bg, middle) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(middle.is_empty(), "Sessions must not open a floating egui::Window: {middle:?}");
        assert!(bg.iter().any(|t| t == "Lead + Reverb"), "Sessions renders templates in main stage: {bg:?}");
        assert!(bg.iter().any(|t| t == "Save current patch"), "Sessions save action painted");

        run_frame(&mut app, &ctx, size, key(Key::F9));
        assert_eq!(app.stage_view, StageView::Plugins, "F9 opens Plugins stage and excludes Sessions");
        let (bg, middle) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(middle.is_empty(), "Plugins must not open a floating egui::Window: {middle:?}");
        assert!(bg.iter().any(|t| t == "Metronome"), "Plugins renders Built-in in stage");
        assert!(bg.iter().any(|t| t == "Surge XT"), "Plugins renders CLAP in stage");
        assert!(bg.iter().any(|t| t == "Lush Verb"), "Plugins renders LV2 in stage");

        run_frame(&mut app, &ctx, size, key(Key::Escape));
        assert_eq!(app.stage_view, StageView::Rack, "Esc returns from Plugins to Rack");
        run_frame(&mut app, &ctx, size, key(Key::B));
        assert_eq!(app.stage_view, StageView::Plugins, "B opens Plugins stage");
        run_frame(&mut app, &ctx, size, key(Key::B));
        assert_eq!(app.stage_view, StageView::Rack, "B toggles Plugins back to Rack");

        let side = app.side;
        app.toggle_stage(StageView::Sessions);
        run_frame(&mut app, &ctx, size, key(Key::Tab));
        assert_eq!(app.stage_view, StageView::Rack, "Tab returns from Sessions to Rack");
        assert!(app.side == side, "Tab from Sessions does not flip the rack");
        assert!(!app.flipping);

        run_frame(&mut app, &ctx, size, key(Key::F9));
        assert_eq!(app.stage_view, StageView::Plugins);
        run_frame(&mut app, &ctx, size, key(Key::Tab));
        assert_eq!(app.stage_view, StageView::Rack, "Tab returns from Plugins to Rack");
        assert!(app.side == side, "Tab from Plugins does not flip the rack");

        run_frame(&mut app, &ctx, size, key(Key::F10));
        assert_eq!(app.stage_view, StageView::Mixer);
        run_frame(&mut app, &ctx, size, key(Key::F11));
        assert_eq!(app.stage_view, StageView::Matrix);
        run_frame(&mut app, &ctx, size, key(Key::F9));
        assert_eq!(app.stage_view, StageView::Plugins);
        run_frame(&mut app, &ctx, size, key(Key::Escape));
        assert_eq!(app.stage_view, StageView::Rack);

        let min_size = Vec2::new(760.0, 520.0);
        app.toggle_stage(StageView::Sessions);
        run_frame(&mut app, &ctx, min_size, vec![]);
        app.toggle_stage(StageView::Plugins);
        run_frame(&mut app, &ctx, min_size, vec![]);
    }

    #[test]
    fn f9_loads_plugins_on_first_open() {
        let mut app = DemoApp::new_with_sink(None);
        assert!(!app.plugins_loaded);
        let ctx = egui::Context::default();
        run_frame(&mut app, &ctx, Vec2::new(1280.0, 800.0), key(Key::F9));
        assert_eq!(app.stage_view, StageView::Plugins);
        assert!(app.plugins_loaded, "opening Plugins scans plugins on first open");
    }

    #[test]
    fn plugins_stage_filters_by_text_and_category() {
        let mut app = DemoApp::new_with_sink(None);
        app.plugins_loaded = true;
        app.plugins_clap = vec![ClapPluginInfo {
            id: SURGE_ID.into(),
            name: "Surge XT".into(),
            vendor: "Surge Synth Team".into(),
            features: vec!["instrument".into()],
        }];
        app.plugins_lv2 = vec![Lv2PluginInfo {
            uri: "http://example.org/lv2/reverb".into(),
            name: "Room Reverb".into(),
            num_ports: 4,
            is_instrument: false,
        }];
        app.stage_view = StageView::Plugins;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);

        app.plugin_filter = "room".into();
        let (bg, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(bg.iter().any(|t| t == "Room Reverb"));
        assert!(!bg.iter().any(|t| t == "Surge XT"));
        assert!(!bg.iter().any(|t| t == "Metronome"));

        app.plugin_filter.clear();
        app.plugin_category = PluginCategory::Clap;
        let (bg, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(bg.iter().any(|t| t == "Surge XT"));
        assert!(!bg.iter().any(|t| t == "Room Reverb"));
        assert!(!bg.iter().any(|t| t == "Metronome"));
    }

    #[test]
    fn sessions_stage_save_load_delete_and_escape_confirm() {
        let mut app = DemoApp::new_with_sink(None);
        let dir = std::env::temp_dir().join(format!("ksalat-stage-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        app.stage_view = StageView::Sessions;
        app.session_name = "Stage Jam".into();
        app.save_current_session(&dir);
        assert!(app.session_msg.contains("Saved 'Stage Jam'"), "{}", app.session_msg);
        assert_eq!(DemoApp::list_saved_sessions(&dir), vec!["Stage Jam".to_string()]);

        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0)));
        let mut out = ctx.run_ui(raw, |ui| app.sessions_stage_in(ui, &dir));
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            walk_texts(&cs.shape, &mut texts);
        }
        assert!(texts.iter().any(|t| t == "Stage Jam"), "saved session rendered: {texts:?}");
        assert!(texts.iter().any(|t| t == "Delete"), "delete action rendered");

        app.confirm_delete_session = Some("Stage Jam".into());
        run_frame(&mut app, &ctx, Vec2::new(1280.0, 800.0), key(Key::Escape));
        assert!(app.confirm_delete_session.is_none());
        assert_eq!(app.stage_view, StageView::Sessions, "first Esc cancels delete confirm");

        DemoApp::delete_saved_session(&dir, "Stage Jam").unwrap();
        assert!(DemoApp::list_saved_sessions(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod ui_polish_agent_tests {
    use super::*;
    use crate::control::command_bus;

    fn walk_texts(shape: &egui::Shape, texts: &mut Vec<(String, Rect)>) {
        match shape {
            egui::Shape::Text(t) => texts.push((
                t.galley.text().to_owned(),
                t.galley.rect.translate(t.pos.to_vec2()),
            )),
            egui::Shape::Vec(v) => for s in v { walk_texts(s, texts) },
            _ => {}
        }
    }

    fn run_frame(
        app: &mut DemoApp,
        ctx: &egui::Context,
        size: Vec2,
        events: Vec<egui::Event>,
    ) -> Vec<(String, Rect)> {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        raw.events = events;
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            walk_texts(&cs.shape, &mut texts);
        }
        texts
    }

    fn click_at(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, pos: Pos2) {
        let b = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        for ev in [vec![egui::Event::PointerMoved(pos)], vec![b(true)], vec![b(false)]] {
            run_frame(app, ctx, size, ev);
        }
    }

    const WINDOW: Vec2 = Vec2 { x: 1600.0, y: 1000.0 };

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall { name: name.into(), args }
    }

    /// An app whose assistant worker is the test, on a real (voiceless)
    /// engine: each frame renders one engine block.
    struct Live {
        app: DemoApp,
        ctx: egui::Context,
        engine: GraphEngine,
        worker_tx: std::sync::mpsc::Sender<crate::assistant::FromWorker>,
        worker_rx: std::sync::mpsc::Receiver<crate::assistant::ToWorker>,
    }

    impl Live {
        fn new() -> Self {
            let mut app = DemoApp::new_with_sink(None);
            let (assistant, worker_tx, worker_rx) = Assistant::with_test_worker();
            app.assistant = assistant;
            let (bus, drain) = command_bus();
            let mut engine = GraphEngine::new(app.base_graph.clone(), drain, 48000.0, vec![]);
            let published = Arc::new(Mutex::new(crate::engine::GraphPub { graph: app.base_graph.clone(), ..Default::default() }));
            engine.set_graph_pub(published.clone());
            engine.set_rig_snapshot(app.rig_snapshot.clone());
            app.graph_pub = Some(published);
            app.bus = Some(Arc::new(Mutex::new(bus)));
            let mut live = Self { app, ctx: egui::Context::default(), engine, worker_tx, worker_rx };
            for _ in 0..3 { live.frame(); }
            live
        }

        fn frame(&mut self) {
            run_frame(&mut self.app, &self.ctx, WINDOW, vec![]);
            let mut buf = vec![0.0f32; 64 * 2];
            self.engine.render(&mut buf, 2);
        }

        /// Send one model step's calls and run frames until the app answers.
        fn run_batch(&mut self, calls: Vec<ToolCall>) -> Vec<String> {
            self.worker_tx.send(crate::assistant::FromWorker::ToolCalls(calls)).unwrap();
            for _ in 0..400 {
                self.frame();
                match self.worker_rx.try_recv() {
                    Ok(crate::assistant::ToWorker::ToolResults(results)) => return results,
                    Ok(_) => panic!("the app sent something other than tool results"),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
                }
            }
            panic!("the app never answered the batch");
        }
    }

    #[test]
    fn a_tool_batch_runs_in_one_frame_and_is_answered_once() {
        let mut app = DemoApp::new_with_sink(None);
        let (assistant, worker_tx, worker_rx) = Assistant::with_test_worker();
        app.assistant = assistant;
        worker_tx.send(crate::assistant::FromWorker::ToolCalls(vec![
            call("set_bpm", serde_json::json!({"bpm": 100})),
            call("set_rig_mix", serde_json::json!({"rig": 0, "vol": 0.5})),
            call("frobnicate", serde_json::json!({})),
        ])).unwrap();
        app.poll_assistant();
        assert_eq!(app.bpm, 100.0);
        assert_eq!(app.rig_mix[0].vol, 0.5);
        let Ok(crate::assistant::ToWorker::ToolResults(results)) = worker_rx.try_recv() else { panic!("no results after one frame") };
        assert_eq!(results.len(), 3, "one result per call, in call order");
        assert!(results[0].contains("100 BPM") && results[1].contains("vol 0.50") && results[2].contains("unknown tool"), "{results:?}");
        assert!(worker_rx.try_recv().is_err(), "one answer for the batch");
    }

    #[test]
    fn an_engine_edit_names_what_it_created_and_the_next_call_sees_it() {
        let mut live = Live::new();
        let results = live.run_batch(vec![
            call("add_node", serde_json::json!({"kind": "dsp", "dsp": "reverb"})),
            call("get_graph", serde_json::json!({})),
        ]);
        let id: u64 = results[0].split("new node id ").nth(1).and_then(|rest| rest.trim_end_matches(']').parse().ok())
            .unwrap_or_else(|| panic!("add_node does not name the new node: {}", results[0]));
        let graph: serde_json::Value = serde_json::from_str(&results[1]).unwrap();
        let node = graph["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id)
            .unwrap_or_else(|| panic!("get_graph in the same batch misses node {id}"));
        assert_eq!(node["kind"]["Dsp"], "Reverb");

        // Two cables in one step: each connect sees the cable before it.
        let master = live.app.units[live.app.master_unit.unwrap()].node_id;
        let results = live.run_batch(vec![
            call("connect", serde_json::json!({"from_node": 1, "to_node": id})),
            call("connect", serde_json::json!({"from_node": id, "to_node": master, "to_port": 1})),
        ]);
        assert!(results.iter().all(|r| r.contains("new cable id")), "{results:?}");
        let wired = |from: u32, to: u32| live.app.cables.iter().any(|c| live.app.units[c.src].node_id == from && live.app.units[c.dst].node_id == to);
        assert!(wired(1, id as u32) && wired(id as u32, master), "both cables exist");
    }

    #[test]
    fn the_disconnect_tool_removes_the_cable_for_good() {
        let mut live = Live::new();
        let id = live.app.cables[0].id;
        let results = live.run_batch(vec![call("disconnect", serde_json::json!({"id": id}))]);
        assert!(results[0].contains(&format!("removed cable {id}")), "{}", results[0]);
        for _ in 0..4 { live.frame(); }
        let engine_has = live.app.graph_pub.as_ref().unwrap().lock().unwrap().graph.edges.iter().any(|e| e.id == id);
        assert!(!engine_has, "the engine still routes cable {id}");
        assert!(live.app.live_cable(id).is_none(), "the rack still shows cable {id}");
    }

    #[test]
    fn an_engine_edit_that_changes_nothing_says_so() {
        let mut live = Live::new();
        let results = live.run_batch(vec![call("remove_node", serde_json::json!({"node": 900}))]);
        assert!(results[0].contains("NOT applied"), "{}", results[0]);
    }

    #[test]
    fn the_activity_panel_shows_what_the_last_request_cost() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        app.assistant.key_set = true;
        app.dock = DockView::Assistant;
        let step = |ms| crate::assistant::StepStat { ms, model: "anthropic/claude-opus-5.5".into(), attempts: 1, rejected: 0 };
        app.assistant.turn.steps = vec![step(700), step(500)];
        let mut texts = Vec::new();
        for _ in 0..3 { texts = run_frame(&mut app, &ctx, WINDOW, vec![]); }
        let (_, at) = texts.iter().find(|(t, _)| t == "2 steps · 1.2 s · claude-opus-5.5")
            .unwrap_or_else(|| panic!("no cost line painted: {:?}", texts.iter().map(|(t, _)| t).collect::<Vec<_>>()));
        assert!(app.assistant_rect.contains_rect(*at), "cost line {at:?} outside the panel {:?}", app.assistant_rect);
    }

    #[test]
    fn add_rig_button_aligns_with_rig_chips_and_adds_a_rig() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }

        assert!(app.add_rig_rect.is_positive(), "+ Rig rect laid out");
        assert_eq!(app.rig_chip_rects.len(), 1);
        let chip0 = app.rig_chip_rects[0];
        assert!(
            (app.add_rig_rect.height() - chip0.height()).abs() < 0.5,
            "+ Rig height {} != rig chip height {}",
            app.add_rig_rect.height(),
            chip0.height()
        );
        assert!(
            (app.add_rig_rect.center().y - chip0.center().y).abs() < 0.5,
            "+ Rig center.y {} != rig chip center.y {}",
            app.add_rig_rect.center().y,
            chip0.center().y
        );
        assert!(app.add_rig_rect.right() < chip0.left(), "+ Rig sits left of rig chips");

        // Clicking "+ Rig" adds a second rig, and alignment holds with 2 chips.
        let p = app.add_rig_rect.center();
        click_at(&mut app, &ctx, size, p);
        for _ in 0..2 { run_frame(&mut app, &ctx, size, vec![]); }
        assert_eq!(app.rig_count(), 2);
        assert_eq!(app.rig_chip_rects.len(), 2);
        for (i, cr) in app.rig_chip_rects.iter().enumerate() {
            assert!(
                (app.add_rig_rect.height() - cr.height()).abs() < 0.5,
                "rig {i} chip height {} vs + Rig {}", cr.height(), app.add_rig_rect.height()
            );
            assert!(
                (app.add_rig_rect.center().y - cr.center().y).abs() < 0.5,
                "rig {i} chip center.y {} vs + Rig {}", cr.center().y, app.add_rig_rect.center().y
            );
        }
    }

    #[test]
    fn assistant_panel_at_min_size_keeps_input_visible_and_never_squishes_rack() {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        app.assistant.key_set = true;
        app.dock = DockView::Assistant;
        let ctx = egui::Context::default();
        let min_size = Vec2::new(760.0, 520.0);
        let mut texts = Vec::new();
        for _ in 0..4 {
            texts = run_frame(&mut app, &ctx, min_size, vec![]);
        }
        assert!(app.unit_h >= 140.0, "rack unit height floor kept at min size: {}", app.unit_h);
        assert!(
            app.stage_rect.bottom() <= app.assistant_rect.top() + 0.5,
            "stage {:?} must not overlap assistant {:?}", app.stage_rect, app.assistant_rect
        );
        assert!(app.assistant_input_rect.is_positive(), "input row laid out");
        assert!(app.assistant_input_rect.height() >= 24.0, "input row not collapsed: {:?}", app.assistant_input_rect);
        assert!(
            app.assistant_rect.contains_rect(app.assistant_input_rect),
            "input row {:?} must fit completely inside assistant panel {:?}",
            app.assistant_input_rect,
            app.assistant_rect
        );
        let send_rect = texts.iter().find(|(t, _)| t == "Send").map(|(_, r)| *r).expect("Send button painted");
        assert!(
            app.assistant_rect.contains_rect(send_rect),
            "Send button {send_rect:?} visible inside assistant {:?}", app.assistant_rect
        );

        // Wide layout shows both chat and side Agent Activity column.
        app.exec_tool("set_split", &serde_json::json!({"point": 60}));
        let wide_texts = run_frame(&mut app, &ctx, Vec2::new(1600.0, 1000.0), vec![]);
        assert!(wide_texts.iter().any(|(t, _)| t == "PATCH & ENGINE" || t.starts_with('P')));
        assert!(wide_texts.iter().any(|(t, _)| t.contains("Split Keys")));
    }

    #[test]
    fn agent_touch_set_by_exec_tool_and_remote_graph_pub_fades_and_ignores_user_edits() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        let (tx, mut drain) = command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let (mut ebus, edrain) = command_bus();
        let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), edrain, 48000.0, vec![]);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub {
            rev: 0,
            graph: Graph::default(),
            rules: Vec::new(),
            ..Default::default()
        }));
        engine.set_graph_pub(pub_.clone());
        engine.set_rig_snapshot(app.rig_snapshot.clone());
        app.graph_pub = Some(pub_.clone());
        // Warm the frame path so the first-frame ppp correction settles before
        // any rack geometry is read (frame 2 transiently re-scales the rect).
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }

        let mut pump = |app: &mut DemoApp| {
            run_frame(app, &ctx, size, vec![]);
            drain.drain(|c| { ebus.send(c); });
            let mut buf = vec![0.0f32; 64 * 2];
            engine.render(&mut buf, 2);
            run_frame(app, &ctx, size, vec![])
        };
        pump(&mut app);
        assert!(!app.agent_activity.has_active(app.ui_time), "clean startup has no agent touch");

        // 1. Direct user edits must NOT set agent touch.
        let keys_idx = app.keys_unit.unwrap();
        let keys_id = app.units[keys_idx].node_id;
        let (split_chip, _) = app.split_rects(keys_idx).unwrap();
        let split_pos = app.xf.p(split_chip.center().to_vec2());
        click_at(&mut app, &ctx, size, split_pos);
        pump(&mut app);
        assert!(app.units[keys_idx].split.is_some(), "user split applied");
        assert!(!app.agent_activity.has_active(app.ui_time), "user split must not light agent touch");
        assert!(app.agent_activity.log.is_empty());

        // Direct user mixer edit must NOT set agent touch either.
        app.stage_view = StageView::Mixer;
        for _ in 0..2 { run_frame(&mut app, &ctx, size, vec![]); }
        let f0 = app.mixer_fader_rects[0].center();
        let btn = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        run_frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(f0), btn(f0, true)]);
        run_frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(f0 - Vec2::new(0.0, 20.0))]);
        run_frame(&mut app, &ctx, size, vec![btn(f0 - Vec2::new(0.0, 20.0), false)]);
        pump(&mut app);
        assert!(!app.agent_activity.has_active(app.ui_time), "user mixer drag must not light agent touch");
        app.stage_view = StageView::Rack;
        run_frame(&mut app, &ctx, size, vec![]);

        // 2. Built-in assistant exec_tool lights the named node and paints "AGENT".
        app.exec_tool("set_matrix_rule", &serde_json::json!({"node": 1, "source": "keys", "transpose": 12}));
        assert!(app.agent_activity.node_alpha(1, app.ui_time) > 0.9);
        assert!(app.agent_activity.cell_alpha(SourceId::Keys, 1, app.ui_time) > 0.9);
        assert_eq!(app.agent_activity.node_alpha(4, app.ui_time), 0.0, "unrelated node stays unlit");
        let texts = run_frame(&mut app, &ctx, size, vec![]);
        assert!(texts.iter().any(|(t, _)| t == "AGENT"), "AGENT tag painted in rack");

        // 3. Advancing past AGENT_TOUCH_SECS fades it completely.
        for _ in 0..100 { app.update(0.04); }
        assert_eq!(app.agent_activity.node_alpha(1, app.ui_time), 0.0, "agent touch faded out");
        assert!(!app.agent_activity.has_active(app.ui_time));
        let texts = run_frame(&mut app, &ctx, size, vec![]);
        assert!(!texts.iter().any(|(t, _)| t == "AGENT"), "AGENT tag disappears after fade");

        // 4. Remote GraphPub change (e.g. MCP/CLI add_node or SetRigMix) lights the affected node.
        {
            let mut g = pub_.lock().unwrap();
            let nid = g.graph.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
            g.graph.connect_with_id(99, PortRef { node: 1, port: 0 }, PortRef { node: nid, port: 0 }, CPortKind::Audio);
            g.rev = g.rev.wrapping_add(1);
            g.mix[0].vol = 0.65;
            g.mix_rev = g.mix_rev.wrapping_add(1);
        }
        let texts = run_frame(&mut app, &ctx, size, vec![]);
        let reverb_id = app.units.iter().find(|u| u.name == "REVERB").expect("remote node adopted").node_id;
        assert!(app.agent_activity.node_alpha(reverb_id, app.ui_time) > 0.9, "remote added node lit");
        assert!(app.agent_activity.edge_alpha(99, app.ui_time) > 0.9, "remote added edge lit");
        assert!(app.agent_activity.rig_alpha(0, app.ui_time) > 0.9, "remote mix rig lit");
        assert!(app.agent_activity.node_alpha(keys_id, app.ui_time) > 0.9, "remote rig mix lights rig nodes");
        assert!(texts.iter().any(|(t, _)| t == "AGENT"), "AGENT tag painted for remote GraphPub edit");
    }

    #[test]
    fn assistant_panel_height_is_stable_and_chat_accepts_text() {
        let mut app = DemoApp::new_with_sink(None);
        app.assistant.key_set = true;
        app.dock = DockView::Assistant;
        app.focus_chat_request = true;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }
        let h0 = app.dock_rect.height();
        assert!(h0 > 100.0, "dock has a real height: {h0}");
        for _ in 0..20 { run_frame(&mut app, &ctx, size, vec![]); }
        assert!(
            (app.dock_rect.height() - h0).abs() < 0.5,
            "panel height stable across 20 frames: {h0} -> {}",
            app.dock_rect.height()
        );

        // Click into the chat field, then an egui Text event lands in chat_input.
        let field = app.assistant_input_rect;
        assert!(field.is_positive(), "chat input row laid out");
        let pos = Pos2::new(field.left() + 24.0, field.center().y);
        click_at(&mut app, &ctx, size, pos);
        run_frame(&mut app, &ctx, size, vec![egui::Event::Text("hello".into())]);
        assert_eq!(app.chat_input, "hello", "typed text lands in chat_input");
    }

    #[test]
    fn dock_keyboard_spans_more_octaves_when_wide_and_follows_the_octave() {
        let narrow = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 120.0));
        let wide = Rect::from_min_size(Pos2::ZERO, Vec2::new(1500.0, 120.0));
        assert_eq!(DemoApp::dock_octaves(narrow), 1);
        assert_eq!(DemoApp::dock_octaves(wide), 4);
        // The top white key of a 4-octave keyboard is 48 semitones up.
        let top = DemoApp::dock_key_rect(wide, DemoApp::dock_whites(wide) - 1).center();
        assert_eq!(DemoApp::dock_key_hit(wide, top), Some(48));
        // A black key of the third octave.
        let b = DemoApp::dock_black_key_rect(wide, 2, 0).center();
        assert_eq!(DemoApp::dock_key_hit(wide, b), Some(25));
    }

    #[test]
    fn dock_keyboard_plays_notes_and_assistant_types_text() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<NoteEvent>::new()));
        let log = events.clone();
        let mut app = DemoApp::new_with_sink(Some(Box::new(move |e| log.lock().unwrap().push(e))));
        app.assistant.key_set = true;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        let key = |pressed: bool| egui::Event::Key {
            key: Key::A, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE,
        };

        // Keyboard occupant: laptop key A fires a note, and no chat text is kept.
        app.dock = DockView::Keyboard;
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }
        run_frame(&mut app, &ctx, size, vec![key(true), egui::Event::Text("a".into())]);
        run_frame(&mut app, &ctx, size, vec![key(false)]);
        assert!(
            events.lock().unwrap().iter().any(|e| e.velocity > 0.0),
            "keyboard dock plays a note from the laptop key"
        );
        assert_eq!(app.chat_input, "", "keyboard dock does not type into chat");

        // Assistant occupant: the same key press types into chat and fires no note.
        events.lock().unwrap().clear();
        app.dock = DockView::Assistant;
        app.focus_chat_request = true;
        for _ in 0..2 { run_frame(&mut app, &ctx, size, vec![]); }
        run_frame(&mut app, &ctx, size, vec![key(true), egui::Event::Text("a".into())]);
        run_frame(&mut app, &ctx, size, vec![key(false)]);
        assert_eq!(app.chat_input, "a", "assistant dock types into chat: {:?}", app.chat_input);
        assert!(events.lock().unwrap().is_empty(), "assistant dock fires no note");
    }

    #[test]
    fn typing_c_and_v_in_the_chat_stays_in_the_chat() {
        let mut app = DemoApp::new_with_sink(None);
        app.assistant.key_set = true;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        app.dock = DockView::Assistant;
        app.focus_chat_request = true;
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }
        for (k, ch) in [(Key::C, "c"), (Key::V, "v")] {
            let ev = |pressed| egui::Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
            run_frame(&mut app, &ctx, size, vec![ev(true), egui::Event::Text(ch.into())]);
            run_frame(&mut app, &ctx, size, vec![ev(false)]);
        }
        assert_eq!(app.chat_input, "cv", "letters reach the chat");
        assert_eq!(app.dock, DockView::Assistant, "typing does not swap the dock");
    }

    /// #23: the keyboard dock follows the window height both ways (it kept
    /// its first height) and never drops below a playable size.
    #[test]
    fn keyboard_dock_follows_the_window_height() {
        let mut app = DemoApp::new_with_sink(None);
        app.dock = DockView::Keyboard;
        let ctx = egui::Context::default();
        let dock_h = |app: &mut DemoApp, size: Vec2| {
            for _ in 0..3 { run_frame(app, &ctx, size, vec![]); }
            (app.dock_rect.height(), app.dock_kb_rect)
        };
        let (h_small, kb_small) = dock_h(&mut app, Vec2::new(1600.0, 800.0));
        let (h_big, kb_big) = dock_h(&mut app, Vec2::new(1600.0, 1400.0));
        let (h_back, _) = dock_h(&mut app, Vec2::new(1600.0, 800.0));
        assert!(h_big > h_small + 20.0, "grows with the window: {h_small} → {h_big}");
        assert!((h_back - h_small).abs() < 1.0, "and shrinks back: {h_back} vs {h_small}");
        assert!(kb_big.height() > kb_small.height());
        let (h_tiny, kb_tiny) = dock_h(&mut app, Vec2::new(900.0, 500.0));
        assert!(h_tiny >= 139.0, "minimum dock height: {h_tiny}");
        assert!(DemoApp::dock_key_rect(kb_tiny, 0).height() >= 40.0, "keys stay playable");
        assert!(DemoApp::dock_key_rect(kb_tiny, 0).width() >= 40.0, "{kb_tiny:?}");
    }

    #[test]
    fn a_clicked_on_screen_key_is_released() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<NoteEvent>::new()));
        let log = events.clone();
        let mut app = DemoApp::new_with_sink(Some(Box::new(move |e| log.lock().unwrap().push(e))));
        app.dock = DockView::Keyboard;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }
        let c = DemoApp::dock_key_rect(app.dock_kb_rect, 0).center();
        click_at(&mut app, &ctx, size, c);
        std::thread::sleep(std::time::Duration::from_millis(CLICK_NOTE_MS + 50));
        run_frame(&mut app, &ctx, size, vec![]);
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| e.velocity > 0.0), "the click plays a note");
        assert!(ev.iter().any(|e| e.velocity == 0.0), "and releases it: {:?}", ev.iter().map(|e| (e.note, e.velocity)).collect::<Vec<_>>());
    }

    #[test]
    fn dock_keyboard_clicks_play_the_same_notes_as_the_keys_unit() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<NoteEvent>::new()));
        let log = events.clone();
        let mut app = DemoApp::new_with_sink(Some(Box::new(move |e| log.lock().unwrap().push(e))));
        app.dock = DockView::Keyboard;
        let ctx = egui::Context::default();
        let size = Vec2::new(1600.0, 1000.0);
        for _ in 0..3 { run_frame(&mut app, &ctx, size, vec![]); }
        assert!(app.dock_kb_rect.is_positive(), "dock keyboard laid out");
        // Click a white key (C) and a black key (C#) — the same 60+semi path.
        let c = DemoApp::dock_key_rect(app.dock_kb_rect, 0).center();
        click_at(&mut app, &ctx, size, c);
        let cs = DemoApp::dock_black_key_rect(app.dock_kb_rect, 0, 0).center();
        click_at(&mut app, &ctx, size, cs);
        let notes: Vec<u8> = events.lock().unwrap().iter().map(|e| e.note).collect();
        assert!(notes.contains(&60), "white key C plays note 60: {notes:?}");
        assert!(notes.contains(&61), "black key C# plays note 61: {notes:?}");
    }
}

/// Face-grid audit: every element on every unit face (title, LED, knobs and
/// their labels, displays, meter, chips) stays inside its unit with padding
/// and never overlaps another element — across window sizes and rack shapes.
#[cfg(test)]
mod face_layout_tests {
    use super::*;

    fn frame_at(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
    }

    fn items(app: &DemoApp, ui: usize) -> Vec<(String, Rect)> {
        let u = &app.units[ui];
        let r = app.unit_rects[ui];
        let mut v = Vec::new();
        let sz = |g: &Option<Arc<theme::TrackedLabel>>| g.as_ref().map_or(Vec2::ZERO, |g| g.size);
        let title = Rect::from_min_size(r.min + Vec2::new(FACE_X, 17.0), u.name_g.as_ref().map_or(Vec2::ZERO, |g| g.size()));
        let sub = Rect::from_min_size(r.min + Vec2::new(FACE_X, 42.0), sz(&u.sub_g));
        v.push(("header".to_string(), title.union(sub)));
        v.push(("led".into(), Rect::from_center_size(r.right_top() + Vec2::new(-20.0, 20.0), Vec2::splat(15.0))));
        for (ki, k) in u.knobs.iter().enumerate() {
            if k.hidden { continue; }
            let c = app.knob_center(ui, ki).to_pos2();
            let lw = sz(&k.label_g).x;
            let label = Rect::from_min_size(Pos2::new(c.x - lw / 2.0, c.y + KNOB_R + KNOB_LABEL_GAP), Vec2::new(lw, KNOB_LABEL_H));
            let ring = Rect::from_center_size(c, Vec2::splat(2.0 * KNOB_R + 4.0));
            v.push((format!("knob {}", k.label), ring.union(label)));
        }
        let has_display = matches!(u.node_kind, UnitKind::Keys | UnitKind::Clap | UnitKind::AudioIn)
            || matches!(u.node_kind, UnitKind::Dsp(_)) && !u.transport;
        if has_display {
            let (start, width) = app.display_area(ui);
            if width >= 40.0 {
                let y = app.knob_row_y(ui);
                v.push(("display".into(), Rect::from_min_max(
                    Pos2::new(start, y - 20.0), Pos2::new(start + width, y + KNOB_R + KNOB_LABEL_GAP + KNOB_LABEL_H))));
            }
        }
        if matches!(u.node_kind, UnitKind::AudioIn) {
            let mut zone = app.audio_in_monitor_rect(ui).unwrap_or(Rect::NOTHING);
            if let Some(d) = app.audio_in_device_rect(ui) { zone = zone.union(d); }
            if let Some(c) = app.audio_in_channel_rect(ui) { zone = zone.union(c); }
            if zone.is_finite() { v.push(("audio_in chips".into(), zone)); }
        }
        if matches!(u.node_kind, UnitKind::Master) {
            let (x, bottom, pitch) = app.meter_geom(ui);
            let top = bottom - METER_SEGS as f32 * pitch - 18.0;
            let w = sz(&app.caption_output_g).x.max(22.0);
            v.push(("meter".into(), Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + w, bottom))));
        }
        if let Some((chip, readout)) = app.split_rects(ui) { v.push(("split".into(), chip.union(readout))); }
        if let Some(c) = app.transport_chip_rect(ui) { v.push(("play".into(), c.expand2(Vec2::new(0.0, 0.0)).union(Rect::from_center_size(Pos2::new(c.right() + 16.0, c.center().y), Vec2::splat(14.0))))); }
        if let Some(b) = app.bpm_rects(ui) {
            let mut br = b.value;
            if let Some(m) = b.minus { br = br.union(m); }
            if let Some(p) = b.plus { br = br.union(p); }
            v.push(("bpm".into(), br));
        }
        if let Some(m) = app.metro_action_rects(ui) { v.push(("metro".into(), m.tap.union(m.count_in))); }
        v
    }

    /// Violations for the rig in view: out of the padded unit, or overlapping.
    fn audit(app: &DemoApp) -> Vec<String> {
        let mut bad = Vec::new();
        for ui in 0..app.units.len() {
            if !app.visible(ui) { continue; }
            let r = app.unit_rects[ui];
            if !(r.width() > 0.0) { continue; }
            let inner = Rect::from_min_max(r.min + Vec2::new(20.0, 8.0), r.max - Vec2::new(10.0, 10.0));
            let it = items(app, ui);
            let name = &app.units[ui].name;
            for (n, rr) in &it {
                if !inner.contains_rect(*rr) {
                    bad.push(format!("{name}: {n} {rr:?} leaves padded unit {inner:?}"));
                }
            }
            for i in 0..it.len() {
                for j in i + 1..it.len() {
                    let (a, b) = (&it[i], &it[j]);
                    if a.1.expand(2.0).intersects(b.1) {
                        bad.push(format!("{name}: {} {:?} overlaps {} {:?}", a.0, a.1, b.0, b.1));
                    }
                }
            }
        }
        bad
    }

    fn check(mut app: DemoApp, what: &str) {
        let ctx = egui::Context::default();
        let mut all = Vec::new();
        for size in [Vec2::new(1280.0, 800.0), Vec2::new(1600.0, 1000.0), Vec2::new(2560.0, 1440.0)] {
            for _ in 0..3 { frame_at(&mut app, &ctx, size); }
            for v in audit(&app) { all.push(format!("[{what} {}x{}] {v}", size.x, size.y)); }
        }
        assert!(all.is_empty(), "face layout violations:\n{}", all.join("\n"));
    }

    #[test]
    fn demo_faces_are_padded_and_clear() {
        check(DemoApp::new_with_sink(None), "demo");
    }

    #[test]
    fn multi_rig_faces_are_padded_and_clear() {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        check(app, "two rigs");
    }

    #[test]
    fn template_faces_are_padded_and_clear() {
        for t in crate::session::Template::ALL {
            let mut app = DemoApp::new_with_sink(None);
            app.load_template(t);
            check(app, t.title());
        }
    }

    #[test]
    fn audio_in_face_is_padded_and_clear() {
        // A minimal rack: Keys + AudioIn (In 1, wired to Master) + Master. The
        // Keys/Master stay wide side anchors so the AudioIn unit is the lone
        // middle column (full chip row visible).
        let mut app = DemoApp::new_with_sink(None);
        let mut g = crate::control::Graph::default();
        let _keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let ain = g.add_node(
            NodeKind::AudioIn { device: None, channels: AudioInChannel::MonoL },
            name_bytes("AUDIO IN"),
        );
        g.connect_with_id(1, PortRef { node: ain, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
        app.build_rack(&g, &[]);
        assert!(app.units.iter().any(|u| matches!(u.node_kind, UnitKind::AudioIn)), "AudioIn unit built");
        check(app, "audio in");
    }

    #[test]
    fn knob_rows_line_up_across_columns() {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        for _ in 0..3 { frame_at(&mut app, &ctx, Vec2::new(1600.0, 1000.0)); }
        let bottom_row: Vec<f32> = (0..app.units.len())
            .filter(|&u| app.visible(u) && !app.units[u].knobs.is_empty())
            .filter(|&u| (app.unit_rects[u].bottom() - app.stack_tall_h - 0.0).abs() < 1e9)
            .map(|u| (app.unit_rects[u].bottom(), app.knob_center(u, 0).y))
            .filter(|(b, _)| (b - app.unit_rects.iter().map(|r| r.bottom()).filter(|b| b.is_finite()).fold(0.0, f32::max)).abs() < 0.5)
            .map(|(_, y)| y)
            .collect();
        assert!(bottom_row.len() >= 3, "keys, a bottom stack and master share the bottom: {bottom_row:?}");
        assert!(bottom_row.iter().all(|y| (y - bottom_row[0]).abs() < 0.5), "one knob line: {bottom_row:?}");
    }
}

#[cfg(test)]
mod midi_hotplug_tests {
    use super::*;

    #[test]
    fn a_replug_under_the_same_name_is_detected_as_unsubscribed() {
        // Real /proc/asound/seq/clients text after a USB re-plug: the M2 is
        // still connected to us (129), the MIDIMATE's ports lost theirs.
        let seq = "Client  24 : \"M2\" [Kernel Legacy]\n  Port   0 : \"M2 MIDI 1\" (RWeX) [In/Out]\n    Connecting To: 129:0\n\
Client  28 : \"ESI MIDIMATE eX\" [Kernel Legacy]\n  Port   0 : \"ESI MIDIMATE eX MIDI 1\" (RWeX) [In/Out]\n  Port   1 : \"ESI MIDIMATE eX MIDI 2\" (RWeX) [In/Out]\n\
Client 129 : \"kabelsalat-hw\" [User Legacy]\n  Port   0 : \"kabelsalat-hw   \" (-We-) [Out]\n    Connected From: 24:0\n\
Client 130 : \"kabelsalat-hw\" [User Legacy]\n  Port   0 : \"kabelsalat-hw   \" (-We-) [Out]\n";
        assert_eq!(alsa_port_subscribed(seq, "M2:M2 MIDI 1 24:0"), Some(true));
        assert_eq!(alsa_port_subscribed(seq, "ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 1 28:0"), Some(false), "stale → reconnect");
        assert_eq!(alsa_port_subscribed(seq, "ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 2 28:1"), Some(false));
        assert_eq!(alsa_port_subscribed(seq, "Gone:Gone 40:0"), None, "unknown port: no verdict");
        assert_eq!(alsa_port_subscribed(seq, "No numbers"), None);
        // Subscribed to someone else only (e.g. PipeWire) is not ours.
        let other = "Client  28 : \"X\" [Kernel]\n  Port   0 : \"X\" (RWeX)\n    Connecting To: 142:0\n";
        assert_eq!(alsa_port_subscribed(other, "X:X 28:0"), Some(false));
    }

    #[test]
    fn replugged_device_keeps_its_index_even_with_a_new_client_number() {
        assert_eq!(midi_port_key("M-Wave FM-1:M-Wave FM-1 MIDI 1 24:0"), "M-Wave FM-1:M-Wave FM-1 MIDI 1");
        assert_eq!(midi_port_key("Some Synth"), "Some Synth");
        assert_eq!(midi_port_key("Synth 2 Port A"), "Synth 2 Port A", "words are not client numbers");

        let devices = vec![
            ("Launchkey:Launchkey MIDI 20:0".to_string(), true),
            ("M-Wave FM-1:M-Wave FM-1 MIDI 1 24:0".to_string(), false), // unplugged
        ];
        // Re-plugged with another ALSA client number → same index 1.
        assert_eq!(match_midi_device(&devices, "M-Wave FM-1:M-Wave FM-1 MIDI 1 28:0"), Ok(Some(1)));
        // Still connected under its exact name → skip.
        assert_eq!(match_midi_device(&devices, "Launchkey:Launchkey MIDI 20:0"), Err(()));
        // A second, identical Launchkey while the first is live → a new device.
        assert_eq!(match_midi_device(&devices, "Launchkey:Launchkey MIDI 32:0"), Ok(None));
        // Something never seen → new.
        assert_eq!(match_midi_device(&devices, "Other:Other 40:0"), Ok(None));
    }
}
