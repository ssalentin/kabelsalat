//! egui UI — the signature animated wiring (PLAN.md §2), Reason-style rack.
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
use midir::os::unix::VirtualInput;
use midir::{Ignore, MidiInput};

use crate::assistant::{Assistant, ChatLine, FromWorker};
use crate::engine::{
    AudioShared, DelayLine, R_DELAY_MASTER, R_KEYS_SURGE, R_SURGE_DELAY, R_SURGE_MASTER,
};
use crate::host::{ClapVoice, ClapVoiceMain, VoiceController};

// ---------------------------------------------------------------------------
// Palette (assets/logo.svg + Omarchy/Tokyo-night accents)
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(0x15, 0x16, 0x1e);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD6, 0x0A); // MIDI
const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7); // audio A
const GREEN: Color32 = Color32::from_rgb(0x9e, 0xce, 0x6a); // audio B
const PURPLE: Color32 = Color32::from_rgb(0xbb, 0x9a, 0xf7);
const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9e, 0x64);
const CHASSIS: Color32 = Color32::from_rgb(0x19, 0x1b, 0x26);
const RAIL: Color32 = Color32::from_rgb(0x26, 0x28, 0x38);
const PLATE_FRONT: Color32 = Color32::from_rgb(0x23, 0x25, 0x38);
const PLATE_BACK: Color32 = Color32::from_rgb(0x1b, 0x1d, 0x29);
const BORDER: Color32 = Color32::from_rgb(0x34, 0x37, 0x4d);
const RING: Color32 = Color32::from_rgb(0x4a, 0x4e, 0x69);
const PLUG: Color32 = Color32::from_rgb(0x41, 0x44, 0x59);
const TEXT: Color32 = Color32::from_rgb(0xec, 0xee, 0xf6);
const SUBTEXT: Color32 = Color32::from_rgb(0x8f, 0x95, 0xad);
const WHITE: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
const BLACK: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);

const N_UNITS: usize = 4;
const MAX_SAMPLES: usize = 49; // arc-length LUT cap (48 segments per cable)
const MAX_PULSES: usize = 12;
const MAX_CABLES: usize = 64;
const FLIP_SECS: f32 = 0.35;

// Damped spring: omega ≈ 11.4 rad/s (~1.8 Hz), zeta ≈ 0.3 → a few visible
// swings before settling. Semi-implicit Euler at ≤ 1/240 s substeps.
const SPRING_K: f32 = 130.0;
const SPRING_D: f32 = 6.8;

const BPM: f64 = 96.0;
const BEAT: f64 = 60.0 / BPM;

// Simulated band: swung eighths with accents and a breath at the end (2 bars).
const MELODY: [(f32, f32); 16] = [
    (0.55, 1.0), (0.45, 0.55), (0.55, 0.8), (0.45, 0.6),
    (0.55, 0.95), (0.45, 0.5), (0.55, 0.75), (0.45, 0.65),
    (0.55, 1.0), (0.45, 0.55), (0.55, 0.85), (0.45, 0.6),
    (0.55, 0.9), (0.45, 0.7), (0.55, 0.5), (0.45, 0.0),
];
const TAP: [(f32, f32); 4] = [(2.0, 0.75), (2.0, 0.6), (2.0, 0.75), (2.0, 0.9)];
// Which key lights up per melody step (semitones, 0..12).
const NOTE_SEQ: [usize; 16] = [0, 4, 7, 4, 2, 5, 9, 5, 0, 4, 7, 9, 11, 7, 4, 2];
const WHITE_SEMI: [usize; 8] = [0, 2, 4, 5, 7, 9, 11, 12];
const BLACK_SEMI: [usize; 5] = [1, 3, 6, 8, 10];
const BLACK_AFTER: [usize; 5] = [0, 1, 3, 4, 5]; // white-key index each black key follows

// ---------------------------------------------------------------------------
// Small helpers (no allocation)
// ---------------------------------------------------------------------------

#[inline]
fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

#[inline]
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, _] = a.to_array();
    let [br, bg, bb, _] = b.to_array();
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(ar, br), l(ag, bg), l(ab, bb))
}

#[inline]
fn clamp_len(v: Vec2, max: f32) -> Vec2 {
    let len = v.length();
    if len > max && len > 0.0 { v * (max / len) } else { v }
}

#[inline]
fn lerp_pos(a: Pos2, b: Pos2, t: f32) -> Pos2 {
    Pos2::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
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

fn split_cubic(p: &[Pos2; 4], t: f32) -> ([Pos2; 4], [Pos2; 4]) {
    let a = lerp_pos(p[0], p[1], t);
    let b = lerp_pos(p[1], p[2], t);
    let c = lerp_pos(p[2], p[3], t);
    let d = lerp_pos(a, b, t);
    let e = lerp_pos(b, c, t);
    let m = lerp_pos(d, e, t);
    ([p[0], a, d, m], [m, e, c, p[3]])
}

/// Sub-curve between parameters t0..t1 (pulse trails).
fn sub_cubic(p: &[Pos2; 4], t0: f32, t1: f32) -> [Pos2; 4] {
    let (left, _) = split_cubic(p, t1);
    let (_, right) = split_cubic(&left, (t0 / t1.max(1e-6)).clamp(0.0, 1.0));
    right
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
        match self { PortKind::Midi => YELLOW, PortKind::Audio => BLUE }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side { Front, Back }

struct Knob {
    label: &'static str,
    value: f32,
    default: f32,
    label_g: Option<Arc<Galley>>,
}

/// A jack on the back panel: direction, port index, x fraction of the unit.
struct Jack {
    dir: Dir,
    port: usize,
    xf: f32,
    label: &'static str,
    label_g: Option<Arc<Galley>>,
}

struct Unit {
    name: &'static str,
    sub: &'static str,
    accent: Color32,
    ins: [PortKind; 2],
    outs: [PortKind; 2],
    knobs: [Knob; 2],
    jacks: Vec<Jack>,
    led: f32,
    name_g: Option<Arc<Galley>>,
    sub_g: Option<Arc<Galley>>,
    back_g: Option<Arc<Galley>>,
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
        if dir == Dir::In { self.ins[port] } else { self.outs[port] }
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
            level: 0.0, env: 0.0, phase: (seed % 628) as f32 / 100.0,
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

#[derive(Clone, Copy)]
enum Hit {
    Play,
    Flip,
    Knob { unit: usize, idx: usize },
    Key(usize), // semitone
    Jack { unit: usize, jack: usize },
    Cable(usize, f32), // cable index + grab parameter
    Unit(usize),
    Background,
}

enum Drag {
    None,
    Knob { unit: usize, idx: usize, start_val: f32, press_y: f32 },
    Plug { ci: usize, end: Dir },
    Cable { ci: usize, u: f32 },
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
// App
// ---------------------------------------------------------------------------

struct DemoApp {
    units: Vec<Unit>,
    cables: Vec<Cable>,
    next_id: u32,
    audio_color_idx: usize,

    side: Side,
    flipping: bool,
    flip_t: f32,
    flip_to: Side,

    time: f64,    // musical clock (pausable)
    ui_time: f32, // wall clock for idle breathing / affordances
    playing: bool,

    drag: Drag,
    press: Option<(Hit, Pos2)>,
    pending: Option<Pending>,
    selected: Option<u32>,
    hover: Option<Hit>,

    key_leds: [f32; 13],
    meter: f32,

    show_help: bool,
    show_stats: bool,
    fps: f32,
    stats_timer: f32,
    stats_g: Option<Arc<Galley>>,

    // Per-frame layout / transform.
    rack_w: f32,
    rack_h: f32,
    unit_h: f32,
    unit_rects: [Rect; N_UNITS], // rack-local
    xf: Xform,
    text_alpha: f32,

    // Cached text.
    wordmark_g: Option<Arc<Galley>>,
    state_play_g: Option<Arc<Galley>>,
    state_pause_g: Option<Arc<Galley>>,
    flip_label_g: Option<Arc<Galley>>,
    hint_front_g: Option<Arc<Galley>>,
    hint_back_g: Option<Arc<Galley>>,
    hint_wire_g: Option<Arc<Galley>>,
    help_title_g: Option<Arc<Galley>>,
    help_body_g: Option<Arc<Galley>>,
    help_foot_g: Option<Arc<Galley>>,

    arrivals: [(usize, f32); 32],

    /// Audio-integration seam: interactive key presses are delivered here.
    note_sink: Option<NoteSink>,

    // --- Integrated app (task-4 merge): audio, assistant, input routing ---
    assistant: Assistant,
    key_input: String,
    chat_input: String,
    octave: u8,
    held_keys: Vec<(Key, usize)>,
    note_offs: Vec<(std::time::Instant, usize)>,
    hw_queue: Arc<Mutex<VecDeque<NoteEvent>>>,
    _hw_conns: Vec<midir::MidiInputConnection<()>>,
    hw_status: String,
    surge_status: String,
    // !Send: CLAP main-thread object; DemoApp never leaves the UI thread.
    _surge_main: Option<ClapVoiceMain>,
    _audio_stream: Option<cpal::Stream>,
    audio_shared: Option<Arc<AudioShared>>,
}

impl DemoApp {
    fn new_with_sink(note_sink: Option<NoteSink>) -> Self {
        let knob = |label: &'static str, default: f32| Knob {
            label, value: default, default, label_g: None,
        };

        let mk_unit = |name: &'static str, sub: &'static str, accent: Color32,
                       ins: &[PortKind], outs: &[PortKind],
                       knobs: [Knob; 2],
                       jacks: &[(Dir, usize, f32, &'static str)]| -> Unit {
            let mut ins_arr = [PortKind::Audio; 2];
            let mut outs_arr = [PortKind::Audio; 2];
            ins_arr[..ins.len()].copy_from_slice(ins);
            outs_arr[..outs.len()].copy_from_slice(outs);
            Unit {
                name, sub, accent,
                ins: ins_arr,
                outs: outs_arr,
                knobs,
                jacks: jacks.iter().map(|&(dir, port, xf, label)| Jack {
                    dir, port, xf, label, label_g: None,
                }).collect(),
                led: 0.0,
                name_g: None, sub_g: None, back_g: None,
            }
        };

        use PortKind::{Audio, Midi};
        let units = vec![
            mk_unit("KEYS", "midi source · click the keys", YELLOW, &[], &[Midi],
                [knob("RATE", 0.5), knob("VEL", 0.8)],
                &[(Dir::Out, 0, 0.62, "MIDI OUT")]),
            mk_unit("SURGE XT", "synth · clap", BLUE, &[Midi], &[Audio],
                [knob("TONE", 0.35), knob("LEVEL", 0.85)],
                &[(Dir::In, 0, 0.30, "MIDI IN"), (Dir::Out, 0, 0.70, "OUT")]),
            mk_unit("DELAY", "insert fx · midi tap", PURPLE, &[Audio, Midi], &[Audio],
                [knob("TIME", 0.4), knob("MIX", 0.5)],
                &[(Dir::In, 0, 0.22, "IN"), (Dir::In, 1, 0.44, "MIDI"), (Dir::Out, 0, 0.76, "OUT")]),
            mk_unit("MASTER OUT", "stereo bus", ORANGE, &[Audio, Audio], &[],
                [knob("VOLUME", 0.8), knob("WARM", 0.15)],
                &[(Dir::In, 0, 0.36, "L"), (Dir::In, 1, 0.64, "R")]),
        ];

        let mut app = Self {
            units,
            cables: Vec::with_capacity(MAX_CABLES),
            next_id: 1,
            audio_color_idx: 0,
            side: Side::Front,
            flipping: false,
            flip_t: 1.0,
            flip_to: Side::Front,
            time: 0.0,
            ui_time: 0.0,
            playing: true,
            drag: Drag::None,
            press: None,
            pending: None,
            selected: None,
            hover: None,
            key_leds: [0.0; 13],
            meter: 0.0,
            show_help: false,
            show_stats: false,
            fps: 60.0,
            stats_timer: 0.0,
            stats_g: None,
            rack_w: 640.0,
            rack_h: 560.0,
            unit_h: 120.0,
            unit_rects: [Rect::NOTHING; N_UNITS],
            xf: Xform { center: Pos2::ZERO, lc: Vec2::ZERO, sx: 1.0, sy: 1.0 },
            text_alpha: 1.0,
            wordmark_g: None,
            state_play_g: None,
            state_pause_g: None,
            flip_label_g: None,
            hint_front_g: None,
            hint_back_g: None,
            hint_wire_g: None,
            help_title_g: None,
            help_body_g: None,
            help_foot_g: None,
            arrivals: [(0, 0.0); 32],
            note_sink,
            assistant: Assistant::new(),
            key_input: String::new(),
            chat_input: String::new(),
            octave: 4,
            held_keys: Vec::new(),
            note_offs: Vec::new(),
            hw_queue: Arc::new(Mutex::new(VecDeque::new())),
            _hw_conns: Vec::new(),
            hw_status: String::new(),
            surge_status: "no synth loaded".into(),
            _surge_main: None,
            _audio_stream: None,
            audio_shared: None,
        };

        // Hardware MIDI auto-wire at startup: events queue here and are drained
        // on the UI thread through fire_keys, so sound and cable animation stay
        // in sync.
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
                    let q = app.hw_queue.clone();
                    if let Ok(conn) = mi.connect(
                        &port,
                        "kabelsalat-hw",
                        move |_stamp, msg: &[u8], _| {
                            if msg.len() >= 3 {
                                let st = msg[0] & 0xF0;
                                if st == 0x90 || st == 0x80 {
                                    let vel = if st == 0x90 { msg[2] as f32 / 127.0 } else { 0.0 };
                                    if let Ok(mut q) = q.lock() {
                                        q.push_back(NoteEvent { note: msg[1], velocity: vel });
                                        while q.len() > 256 {
                                            q.pop_front();
                                        }
                                    }
                                }
                            }
                        },
                        (),
                    ) {
                        app._hw_conns.push(conn);
                        hw_names.push(name);
                    }
                }
            }
        }
        app.hw_status = if hw_names.is_empty() {
            "no hardware MIDI — laptop keys live".into()
        } else {
            format!("MIDI in: {}", hw_names.join(", "))
        };

        // Demo patch (back panel).
        app.add_cable(0, 0, 1, 0, Midi, 0);  // Keys → Surge (melody)
        app.add_cable(0, 0, 2, 1, Midi, 1);  // Keys → Delay (tap, long run)
        app.add_cable(1, 0, 2, 0, Audio, 0); // Surge → Delay
        app.add_cable(2, 0, 3, 0, Audio, 0); // Delay → Master L
        app.add_cable(1, 0, 3, 1, Audio, 0); // Surge → Master R (dry)
        if let Some(c) = app.cables.first_mut() { c.next_event = 0.35; }
        if let Some(c) = app.cables.get_mut(1) { c.next_event = 1.1; }
        app
    }

    /// Galleys are laid out on the first frame: egui loads fonts only once
    /// `Context::run` has started, so the app creator cannot do this.
    fn ensure_galleys(&mut self, ctx: &egui::Context) {
        if self.wordmark_g.is_some() { return; }
        let layout = |text: &str, font: FontId, color: Color32, wrap: f32| -> Arc<Galley> {
            ctx.fonts_mut(|f| f.layout(text.to_owned(), font, color, wrap))
        };
        let label_font = FontId::proportional(9.5);
        for u in &mut self.units {
            u.name_g = Some(layout(u.name, FontId::proportional(16.0), TEXT, f32::INFINITY));
            u.sub_g = Some(layout(u.sub, FontId::proportional(10.5), SUBTEXT, f32::INFINITY));
            u.back_g = Some(layout(u.name, FontId::proportional(11.0), SUBTEXT, f32::INFINITY));
            for k in &mut u.knobs {
                k.label_g = Some(layout(k.label, label_font.clone(), SUBTEXT, f32::INFINITY));
            }
            for j in &mut u.jacks {
                j.label_g = Some(layout(j.label, label_font.clone(), SUBTEXT, f32::INFINITY));
            }
        }
        self.wordmark_g = Some(layout("kabelsalat", FontId::proportional(14.5), TEXT, f32::INFINITY));
        self.state_play_g = Some(layout("96 bpm · playing", FontId::proportional(10.5), GREEN, f32::INFINITY));
        self.state_pause_g = Some(layout("96 bpm · paused", FontId::proportional(10.5), SUBTEXT, f32::INFINITY));
        self.flip_label_g = Some(layout("flip rack · Tab", FontId::proportional(11.0), TEXT, f32::INFINITY));
        self.hint_front_g = Some(layout(
            "perform: drag knobs · click the keys · press Tab to flip the rack and patch",
            FontId::proportional(12.0), SUBTEXT, f32::INFINITY));
        self.hint_back_g = Some(layout(
            "patch: drag jack to jack · grab a cable to shake it · tug a plug to unplug · Tab flips back",
            FontId::proportional(12.0), SUBTEXT, f32::INFINITY));
        self.hint_wire_g = Some(layout(
            "now click a glowing jack to connect · esc or right-click cancels",
            FontId::proportional(12.0), YELLOW, f32::INFINITY));
        self.help_title_g = Some(layout("kabelsalat — the cables are the show", FontId::proportional(17.0), TEXT, f32::INFINITY));
        self.help_body_g = Some(layout(
            "Tab                flip the rack · front = perform, back = patch\n\
             drag knobs         shape the sound · right-click resets\n\
             click the keys     play Keys — pulses run down the cables\n\
             drag jack to jack  patch a cable (back)\n\
             grab a cable       shake it — watch it swing and settle (back)\n\
             tug a plug         pull it out to unplug (back)\n\
             click a cable      highlight its whole signal path\n\
             space              play / pause the band\n\
             R reset knobs · D stats · ? help · esc cancel",
            FontId::monospace(12.5), Color32::from_rgb(0xc6, 0xca, 0xde), 560.0));
        self.help_foot_g = Some(layout("esc or ? to close", FontId::proportional(11.5), SUBTEXT, f32::INFINITY));
    }

    // -- layout -----------------------------------------------------------

    fn layout(&mut self, rect: Rect) {
        self.rack_w = (rect.width() - 250.0).clamp(320.0, 700.0);
        self.unit_h = ((rect.height() - 235.0) / N_UNITS as f32).clamp(84.0, 142.0);
        let gap = 10.0;
        let pad = 16.0;
        self.rack_h = pad * 2.0 + N_UNITS as f32 * self.unit_h + (N_UNITS as f32 - 1.0) * gap;
        let origin = Pos2::new(
            rect.center().x - self.rack_w / 2.0,
            rect.center().y - self.rack_h / 2.0 + 16.0,
        );
        for i in 0..N_UNITS {
            self.unit_rects[i] = Rect::from_min_size(
                Pos2::new(pad + 14.0, pad + i as f32 * (self.unit_h + gap)),
                Vec2::new(self.rack_w - 2.0 * (pad + 14.0), self.unit_h),
            );
        }
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

    fn add_cable(&mut self, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind, pattern: u8) {
        if self.cables.len() >= MAX_CABLES || src == dst { return; }
        for c in &mut self.cables {
            if c.dst == dst && c.dst_port == dst_port && c.dying < 0.0 {
                c.dying = 0.0;
                c.detach_dst = true;
                c.drift = if rng01(&mut c.rng) > 0.5 { 34.0 } else { -34.0 };
            }
        }
        if self.cables.iter().any(|c| c.dying < 0.0 && c.src == src && c.src_port == src_port && c.dst == dst && c.dst_port == dst_port) {
            return;
        }
        let color = match kind {
            PortKind::Midi => YELLOW,
            PortKind::Audio => {
                let c = if self.audio_color_idx % 2 == 0 { BLUE } else { GREEN };
                self.audio_color_idx += 1;
                c
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut c = Cable::new(id, src, src_port, dst, dst_port, kind, color, pattern, id * 7919 + 13);
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
            c.dying = 0.0;
            c.detach_dst = detach_dst;
            c.over_a = None;
            c.over_b = None;
            c.drift = if rng01(&mut c.rng) > 0.5 { 34.0 } else { -34.0 };
            c.v1 += Vec2::new(0.0, -120.0);
            c.v2 += Vec2::new(0.0, -120.0);
        }
    }

    fn cable_at_jack(&self, unit: usize, dir: Dir, port: usize) -> Option<usize> {
        self.cables.iter().position(|c| {
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
        main: ClapVoiceMain,
        stream: cpal::Stream,
        shared: Arc<AudioShared>,
    ) -> Self {
        self._surge_main = Some(main);
        self._audio_stream = Some(stream);
        self.audio_shared = Some(shared);
        self.surge_status = "Surge XT loaded — play!".into();
        self
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
        let semi = (off as i16 + (self.octave as i16 - 4) * 12).max(0) as usize;
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

    fn poll_assistant(&mut self) {
        for ev in self.assistant.poll() {
            match ev {
                FromWorker::Text(t) => self
                    .assistant
                    .history
                    .push(ChatLine { role: "assistant".into(), text: t }),
                FromWorker::ToolCall { id, name, args } => {
                    let result = self.exec_tool(&name, &args);
                    self.assistant.send_tool_result(id, result);
                }
                FromWorker::Done => self.assistant.busy = false,
                FromWorker::Error(e) => {
                    self.assistant.busy = false;
                    self.assistant
                        .history
                        .push(ChatLine { role: "error".into(), text: e });
                }
            }
        }
    }

    fn exec_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        match name {
            "get_status" => serde_json::json!({
                "synth": self.surge_status,
                "midi_in": self.hw_status,
                "octave": self.octave,
                "rack_side": match self.side { Side::Front => "front", Side::Back => "back" },
                "playing": self.playing,
                "cables": self.cables.len(),
                "fps": self.fps,
            })
            .to_string(),
            "play_note" => {
                let note = args.get("note").and_then(|v| v.as_u64()).unwrap_or(60).min(127);
                let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(500).min(10000);
                let semi = (note as i16 - 60).max(0) as usize;
                self.fire_keys(semi, 0.8);
                self.note_offs.push((
                    std::time::Instant::now() + std::time::Duration::from_millis(ms),
                    semi,
                ));
                format!("played note {note} for {ms} ms")
            }
            "set_octave" => {
                let oct = args.get("octave").and_then(|v| v.as_u64()).unwrap_or(4).clamp(1, 7) as u8;
                self.octave = oct;
                format!("octave set to C{oct}")
            }
            "flip_rack" => {
                self.start_flip();
                "rack flipped".into()
            }
            _ => format!("unknown tool: {name}"),
        }
    }

    fn assistant_panel_ui(&mut self, root: &mut egui::Ui) {
        egui::Panel::right("assistant")
            .default_size(340.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(0x1e, 0x20, 0x2e))
                    .inner_margin(egui::Margin::same(10)),
            )
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
                        match crate::assistant::save_key(&k) {
                            Ok(()) => {
                                self.assistant.set_key(k);
                                self.key_input.clear();
                            }
                            Err(e) => self.assistant.history.push(ChatLine {
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
                                    _ => ("Error", Color32::from_rgb(0xf7, 0x76, 0x8e)),
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
                            edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (ui.button("Send").clicked() || enter)
                            && !self.chat_input.trim().is_empty()
                            && !self.assistant.busy
                        {
                            let t = self.chat_input.trim().to_string();
                            self.chat_input.clear();
                            self.assistant.send_user(t);
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("{} · octave C{}", self.surge_status, self.octave))
                            .small()
                            .italics(),
                    );
                    ui.label(egui::RichText::new(&self.hw_status).small().italics());
                }
            });
    }

    fn fire_keys(&mut self, semitone: usize, amp: f32) {
        self.key_leds[semitone.min(12)] = 1.0;
        self.units[0].led = 1.0;
        for ci in 0..self.cables.len() {
            let c = &mut self.cables[ci];
            if c.src == 0 && c.kind == PortKind::Midi && c.dying < 0.0 {
                c.spawn_pulse(0.0, amp);
            }
        }
        if let Some(sink) = &mut self.note_sink {
            sink(NoteEvent { note: semitone.min(127) as u8, velocity: amp.clamp(0.0, 1.0) });
        }
    }

    fn ping_unit(&mut self, idx: usize) {
        self.units[idx].led = 1.0;
        if idx == 0 {
            let amps = [1.0, 0.8, 0.9, 0.7];
            for ci in 0..self.cables.len() {
                if self.cables[ci].src == 0 && self.cables[ci].kind == PortKind::Midi && self.cables[ci].dying < 0.0 {
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

    fn try_connect(&mut self, from: (usize, usize, Dir, PortKind), to: (usize, usize, Dir, PortKind)) -> bool {
        let (fu, fp, fd, fk) = from;
        let (tu, tp, td, tk) = to;
        if fd == td || fk != tk || fu == tu { return false; }
        let (src, sp, dst, dp) = if fd == Dir::Out { (fu, fp, tu, tp) } else { (tu, tp, fu, fp) };
        let pattern = if dst == 2 { 1 } else { 0 };
        self.add_cable(src, sp, dst, dp, fk, pattern);
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
    }

    fn toggle_play(&mut self) {
        self.playing = !self.playing;
        if self.playing {
            for c in &mut self.cables {
                c.next_event = self.time + 0.08;
                c.pat_idx = 0;
            }
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
        if self.playing { self.time += dt as f64; }

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

        for u in &mut self.units { u.led *= (-dt / 0.20).exp(); }
        for k in &mut self.key_leds { *k *= (-dt / 0.22).exp(); }

        // Knob → simulation mapping.
        let rate = 0.5 + self.units[0].knobs[0].value * 1.5;
        let vel_mul = 0.35 + self.units[0].knobs[1].value * 0.65;
        let surge_level = 0.25 + self.units[1].knobs[1].value * 0.75;
        let delay_time = 0.55 + self.units[2].knobs[0].value * 1.3;
        let delay_mix = 0.15 + self.units[2].knobs[1].value * 0.85;
        let volume = 0.15 + self.units[3].knobs[0].value * 0.85;

        let mut n_arrivals = 0usize;
        for ci in 0..self.cables.len() {
            let ea = self.endpoint(&self.cables[ci], Dir::Out);
            let eb = self.endpoint(&self.cables[ci], Dir::In);
            let c = &mut self.cables[ci];
            c.age += dt;
            if c.dying >= 0.0 { c.dying += dt; }

            let mut a = c.over_a.unwrap_or(ea);
            let mut b = c.over_b.unwrap_or(eb);
            if c.dying >= 0.0 {
                let p = (c.dying / 0.55).clamp(0.0, 1.0);
                let fall = Vec2::new(c.drift * p, 430.0 * p * p);
                if c.detach_dst { b += fall; } else { a += fall; }
            }

            // Inertial feed: endpoint motion whips the cable.
            let feed_a = clamp_len((c.prev_a - a) / dt.max(1e-4), 2400.0) * 0.28;
            let feed_b = clamp_len((c.prev_b - b) / dt.max(1e-4), 2400.0) * 0.28;
            c.v1 += feed_a;
            c.v2 += feed_b;
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

            // Simulated band on MIDI runs.
            if c.kind == PortKind::Midi && c.dying < 0.0 && self.playing {
                let pattern: &[(f32, f32)] = if c.pattern == 1 { &TAP } else { &MELODY };
                let mut guard = 0;
                while self.time >= c.next_event && guard < 8 {
                    let (beats, vel) = pattern[c.pat_idx % pattern.len()];
                    if vel > 0.0 {
                        c.spawn_pulse(0.0, vel * vel_mul);
                        self.units[c.src].led = self.units[c.src].led.max(vel * 0.7);
                        if c.pattern == 0 {
                            let note = NOTE_SEQ[c.pat_idx % NOTE_SEQ.len()];
                            self.key_leds[note] = 1.0;
                        }
                    }
                    c.pat_idx += 1;
                    c.next_event += beats as f64 * BEAT / rate as f64;
                    guard += 1;
                }
            }

            // Pulses travel; arrivals excite the destination.
            if self.playing {
                let speed_mul = if c.src == 2 { delay_time } else { 1.0 };
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
            }

            if c.kind == PortKind::Audio {
                c.env *= (-dt / 0.32).exp();
                c.level += (c.env - c.level) * (1.0 - (-dt / 0.05).exp());
            }
        }

        for &(unit, amp) in &self.arrivals[..n_arrivals] {
            self.units[unit].led = self.units[unit].led.max(amp);
            for c in &mut self.cables {
                if c.kind == PortKind::Audio && c.src == unit && c.dying < 0.0 {
                    let mut a = amp * 0.95;
                    if unit == 1 { a *= surge_level; }
                    if unit == 2 { a *= delay_mix; }
                    c.env = c.env.max(a);
                }
            }
        }

        // Master meter.
        let target = self.cables.iter()
            .filter(|c| c.dst == 3 && c.dying < 0.0)
            .map(|c| c.level)
            .fold(0.0f32, f32::max) * volume;
        self.meter += (target - self.meter) * (1.0 - (-dt / 0.08).exp());

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

    fn knob_center(&self, unit: usize, idx: usize) -> Vec2 {
        let r = self.unit_rects[unit];
        Vec2::new(r.right() - 118.0 + idx as f32 * 68.0, r.center().y + 4.0)
    }

    /// Horizontal space between the label block and the knob column —
    /// device displays adapt to it so narrow windows never overlap.
    fn display_area(&self, unit: usize) -> (f32, f32) {
        let r = self.unit_rects[unit];
        let start = r.left() + 190.0;
        let width = (r.right() - 140.0 - start).max(0.0);
        (start, width)
    }

    fn key_w(&self) -> f32 {
        let r = self.unit_rects[0];
        let avail = (r.right() - 140.0) - (r.left() + 185.0);
        if avail < 96.0 { return 0.0; } // too narrow: hide the keyboard
        (avail / 8.0).clamp(12.0, 28.0)
    }

    fn key_rect(&self, white_idx: usize) -> Rect {
        let r = self.unit_rects[0];
        let x0 = r.left() + 185.0;
        let kw = self.key_w();
        Rect::from_min_size(
            Pos2::new(x0 + white_idx as f32 * kw, r.center().y - 24.0),
            Vec2::new(kw - 1.0, 52.0),
        )
    }

    fn hit_test(&self, s: Pos2) -> Hit {
        if (s - Pos2::new(40.0, 40.0)).length() < 19.0 { return Hit::Play; }
        if Rect::from_min_size(Pos2::new(246.0, 24.0), Vec2::new(126.0, 32.0)).contains(s) {
            return Hit::Flip;
        }
        if self.flipping || self.xf.sy < 0.9 { return Hit::Background; }
        let l = self.xf.inv(s);
        let lp = l.to_pos2();

        match self.side {
            Side::Front => {
                for (ui, u) in self.units.iter().enumerate() {
                    for ki in 0..2 {
                        if (l - self.knob_center(ui, ki)).length() < 23.0 {
                            return Hit::Knob { unit: ui, idx: ki };
                        }
                    }
                    let _ = u;
                }
                // Keys keyboard (unit 0).
                for wi in 0..8 {
                    if self.key_rect(wi).contains(lp) { return Hit::Key(WHITE_SEMI[wi]); }
                }
                for (ui, _) in self.units.iter().enumerate() {
                    if self.unit_rects[ui].contains(lp) { return Hit::Unit(ui); }
                }
            }
            Side::Back => {
                for (ui, u) in self.units.iter().enumerate() {
                    for ji in 0..u.jacks.len() {
                        if (l - self.jack_pos(ui, ji)).length() < 15.0 {
                            return Hit::Jack { unit: ui, jack: ji };
                        }
                    }
                }
                let mut best: Option<(usize, f32, f32)> = None;
                for (ci, c) in self.cables.iter().enumerate() {
                    if c.dying >= 0.0 { continue; }
                    let thresh = 8.0 + c.level * 3.0;
                    for i in 0..c.n.saturating_sub(1) {
                        let d = dist_sq_to_segment(lp, c.pts[i], c.pts[i + 1]);
                        if d < thresh * thresh && best.map_or(true, |(_, bd, _)| d < bd) {
                            // Grab parameter along the polyline.
                            let ab = c.pts[i + 1] - c.pts[i];
                            let t = if ab.length_sq() > 1e-9 { ((lp - c.pts[i]).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
                            let u = (i as f32 + t) / (c.n - 1).max(1) as f32;
                            best = Some((ci, d, u));
                        }
                    }
                }
                if let Some((ci, _, u)) = best { return Hit::Cable(ci, u); }
                for (ui, _) in self.units.iter().enumerate() {
                    if self.unit_rects[ui].contains(lp) { return Hit::Unit(ui); }
                }
            }
        }
        Hit::Background
    }

    // -- input ----------------------------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context, rect: Rect) {
        let mut toggle_play = false;
        let mut flip = false;
        let mut reset_knobs = false;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Space) { toggle_play = true; }
            if i.consume_key(Modifiers::NONE, Key::Tab) { flip = true; }
            if i.consume_key(Modifiers::NONE, Key::R) { reset_knobs = true; }
            if i.consume_key(Modifiers::NONE, Key::D) { self.show_stats = !self.show_stats; }
            if i.consume_key(Modifiers::NONE, Key::Questionmark) || i.consume_key(Modifiers::NONE, Key::F1) {
                self.show_help = !self.show_help;
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                if self.show_help { self.show_help = false; }
                else if self.pending.is_some() { self.pending = None; }
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
        if flip { self.start_flip(); }
        if reset_knobs {
            for u in &mut self.units {
                for k in &mut u.knobs { k.value = k.default; }
            }
        }

        let (pointer, pressed, down, released, right_clicked) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.primary_pressed(), i.pointer.primary_down(),
             i.pointer.primary_released(), i.pointer.secondary_clicked())
        });

        let Some(s) = pointer else {
            self.press = None;
            self.drag = Drag::None;
            return;
        };

        if right_clicked {
            if self.pending.is_some() {
                self.pending = None;
            } else if let Hit::Knob { unit, idx } = self.hit_test(s) {
                self.units[unit].knobs[idx].value = self.units[unit].knobs[idx].default;
            }
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
            match hit {
                Hit::Knob { unit, idx } => {
                    self.drag = Drag::Knob { unit, idx, start_val: self.units[unit].knobs[idx].value, press_y: s.y };
                }
                Hit::Key(semi) => {
                    self.fire_keys(semi, 0.95);
                }
                Hit::Jack { unit, jack } if self.side == Side::Back => {
                    let j = &self.units[unit].jacks[jack];
                    if let Some(ci) = self.cable_at_jack(unit, j.dir, j.port) {
                        self.drag = Drag::Plug { ci, end: j.dir };
                    }
                }
                Hit::Cable(ci, u) if self.side == Side::Back => {
                    self.drag = Drag::Cable { ci, u };
                }
                _ => {}
            }
        }

        if down {
            match self.drag {
                Drag::Knob { unit, idx, start_val, press_y } => {
                    let v = (start_val + (press_y - s.y) * 0.006).clamp(0.0, 1.0);
                    self.units[unit].knobs[idx].value = v;
                }
                Drag::Plug { ci, end } => {
                    if ci < self.cables.len() {
                        let c = &mut self.cables[ci];
                        if end == Dir::Out { c.over_a = Some(l); } else { c.over_b = Some(l); }
                    }
                }
                Drag::Cable { ci, u } => {
                    if ci < self.cables.len() && self.cables[ci].dying < 0.0 {
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
                            let ci2 = ci;
                            self.unplug_cable(ci2, false);
                            self.drag = Drag::None;
                        } else if u > 0.84 && (l - b).length() > 48.0 {
                            let ci2 = ci;
                            self.unplug_cable(ci2, true);
                            self.drag = Drag::None;
                        }
                    } else {
                        self.drag = Drag::None;
                    }
                }
                Drag::None => {
                    // Drag-from-jack: one-gesture patching.
                    if let Some((Hit::Jack { unit, jack }, press_pos)) = self.press {
                        if self.pending.is_none() && self.side == Side::Back && (s - press_pos).length() > 7.0 {
                            let occupied = {
                                let j = &self.units[unit].jacks[jack];
                                self.cable_at_jack(unit, j.dir, j.port).is_some()
                            };
                            if !occupied { self.start_pending(unit, jack, l); }
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
            let plug_info = if let Drag::Plug { ci, end } = self.drag { Some((ci, end)) } else { None };
            self.drag = Drag::None;

            // Plug drag release: far from the jack → unplug; near → snap back.
            if let Some((ci, end)) = plug_info {
                if ci < self.cables.len() {
                    let jack = self.endpoint(&self.cables[ci], end);
                    if moved < 6.0 || (l - jack).length() > 42.0 {
                        self.unplug_cable(ci, end == Dir::In);
                    } else {
                        let c = &mut self.cables[ci];
                        if end == Dir::Out { c.over_a = None; } else { c.over_b = None; }
                    }
                }
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
                    Some(Hit::Flip) => self.start_flip(),
                    Some(Hit::Jack { unit, jack }) if self.side == Side::Back => {
                        let j = &self.units[unit].jacks[jack];
                        if let Some(p) = &self.pending {
                            let from = (p.unit, self.units[p.unit].jacks[p.jack].port, p.dir, p.kind);
                            if unit == p.unit && jack == p.jack {
                                self.pending = None;
                            } else if self.try_connect(from, (unit, j.port, j.dir, self.units[unit].kind(j.dir, j.port))) {
                                self.pending = None;
                            }
                        } else if let Some(ci) = self.cable_at_jack(unit, j.dir, j.port) {
                            self.unplug_cable(ci, j.dir == Dir::In);
                        } else {
                            self.start_pending(unit, jack, l);
                        }
                    }
                    Some(Hit::Cable(ci, _)) if self.side == Side::Back => {
                        if ci < self.cables.len() {
                            let id = self.cables[ci].id;
                            self.selected = if self.selected == Some(id) { None } else { Some(id) };
                        }
                    }
                    Some(Hit::Unit(ui)) => self.ping_unit(ui),
                    Some(Hit::Background) | None => {
                        if self.pending.is_some() { self.pending = None; }
                        else { self.selected = None; }
                    }
                    _ => {}
                }
            }
        }

        // Cursor affordances.
        let icon = match self.hover {
            Some(Hit::Play) | Some(Hit::Flip) | Some(Hit::Jack { .. }) | Some(Hit::Key(_)) => CursorIcon::PointingHand,
            Some(Hit::Knob { .. }) => CursorIcon::ResizeVertical,
            Some(Hit::Cable(..)) => match self.drag {
                Drag::Cable { .. } => CursorIcon::Grabbing,
                _ => CursorIcon::Grab,
            },
            Some(Hit::Unit(_)) => CursorIcon::PointingHand,
            _ => CursorIcon::Default,
        };
        ctx.set_cursor_icon(icon);
        let _ = rect;
    }

    // -- drawing ----------------------------------------------------------

    fn draw(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, BG);
        self.draw_grid(&painter, rect);

        // Rack shadow + chassis (through the flip transform).
        let rack_local = Rect::from_min_size(Pos2::ZERO, Vec2::new(self.rack_w, self.rack_h));
        let rack = self.xf.rect(rack_local);
        let shadow = rack.translate(Vec2::new(0.0, 10.0 * self.xf.sy));
        painter.rect_filled(shadow, 20.0, alpha(BLACK, 0.35));
        painter.rect_filled(rack, 18.0, CHASSIS);
        painter.rect_stroke(rack, 18.0, Stroke::new(1.2, BORDER), egui::StrokeKind::Inside);

        if self.xf.sy > 0.04 {
            self.draw_rails(&painter);
            match self.side {
                Side::Front => self.draw_front(&painter),
                Side::Back => self.draw_back(&painter),
            }
        }

        self.draw_toolbar(&painter);
        self.draw_hint(&painter, rect);
        if self.show_stats { self.draw_stats(&painter, rect); }
        if self.show_help { self.draw_help(&painter, rect); }
    }

    fn draw_grid(&self, painter: &egui::Painter, rect: Rect) {
        let spacing = 44.0;
        let dot = alpha(WHITE, 0.045);
        let mut count = 0;
        let mut y = rect.top();
        while y <= rect.bottom() && count < 2200 {
            let mut x = rect.left();
            while x <= rect.right() && count < 2200 {
                painter.rect_filled(Rect::from_center_size(Pos2::new(x, y), Vec2::splat(1.6)), 0.8, dot);
                x += spacing;
                count += 1;
            }
            y += spacing;
        }
    }

    fn draw_rails(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for side_x in [8.0, self.rack_w - 22.0] {
            let rail = Rect::from_min_size(Pos2::new(side_x, 8.0), Vec2::new(14.0, self.rack_h - 16.0));
            painter.rect_filled(xf.rect(rail), 6.0, RAIL);
        }
        // Screws at unit boundaries.
        for i in 0..=N_UNITS {
            let y = 16.0 + i as f32 * (self.unit_h + 10.0) - 5.0;
            for x in [15.0, self.rack_w - 15.0] {
                let c = xf.p(Vec2::new(x, y));
                painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
                    center: c, radius: Vec2::new(2.6 * xf.sx, 2.6 * xf.sy),
                    fill: RING, stroke: Stroke::NONE, angle: 0.0,
                }));
            }
        }
    }

    /// Text through the flip transform (fades while squashed).
    fn text(&self, painter: &egui::Painter, local: Vec2, g: &Option<Arc<Galley>>, color: Color32) {
        if self.text_alpha <= 0.01 { return; }
        let Some(g) = g else { return };
        let mut tp = painter.clone();
        tp.set_opacity(self.text_alpha);
        tp.galley(self.xf.p(local), g.clone(), color);
    }

    fn ell(&self, painter: &egui::Painter, center: Vec2, r: f32, fill: Color32, stroke: Stroke) {
        painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
            center: self.xf.p(center),
            radius: Vec2::new(r * self.xf.sx, r * self.xf.sy),
            fill, stroke, angle: 0.0,
        }));
    }

    fn draw_front(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            let r = self.unit_rects[ui];
            let hovered = matches!(self.hover, Some(Hit::Unit(i)) if i == ui);
            painter.rect_filled(xf.rect(r), 10.0, if hovered { mix(PLATE_FRONT, WHITE, 0.03) } else { PLATE_FRONT });
            painter.rect_stroke(xf.rect(r), 10.0, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);

            // Colored label strip.
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(7.0, r.height() - 24.0));
            painter.rect_filled(xf.rect(strip), 3.5, u.accent);

            self.text(painter, r.min.to_vec2() + Vec2::new(32.0, 16.0), &u.name_g, TEXT);
            self.text(painter, r.min.to_vec2() + Vec2::new(32.0, 40.0), &u.sub_g, SUBTEXT);

            // Activity LED.
            let led = (r.right_top() + Vec2::new(-16.0, 16.0)).to_vec2();
            if u.led > 0.03 { self.ell(painter, led, 7.5, alpha(u.accent, 0.30 * u.led), Stroke::NONE); }
            self.ell(painter, led, 3.2, alpha(u.accent, 0.25 + 0.75 * u.led), Stroke::NONE);

            match ui {
                0 => self.draw_keyboard(painter),
                1 => self.draw_surge_display(painter, r),
                2 => self.draw_delay_display(painter, r),
                3 => self.draw_meter(painter, r),
                _ => {}
            }

            for ki in 0..2 {
                self.draw_knob(painter, ui, ki);
            }
        }
    }

    fn draw_keyboard(&self, painter: &egui::Painter) {
        if self.key_w() < 8.0 { return; }
        let xf = &self.xf;
        for wi in 0..8 {
            let kr = self.key_rect(wi);
            let semi = WHITE_SEMI[wi];
            let lit = self.key_leds[semi];
            let hovered = matches!(self.hover, Some(Hit::Key(s)) if s == semi);
            let fill = mix(Color32::from_rgb(0xe8, 0xea, 0xf2), YELLOW, lit * 0.85);
            painter.rect_filled(xf.rect(kr), 3.0, if hovered { mix(fill, WHITE, 0.3) } else { fill });
            painter.rect_stroke(xf.rect(kr), 3.0, Stroke::new(1.0, alpha(BLACK, 0.4)), egui::StrokeKind::Inside);
        }
        for bi in 0..5 {
            let after = BLACK_AFTER[bi];
            let base = self.key_rect(after);
            let bw = self.key_w() * 0.62;
            let kr = Rect::from_min_size(
                Pos2::new(base.right() - bw / 2.0 + 0.5, base.top()),
                Vec2::new(bw, 32.0),
            );
            let lit = self.key_leds[BLACK_SEMI[bi]];
            painter.rect_filled(xf.rect(kr), 2.5, mix(Color32::from_rgb(0x1a, 0x1b, 0x24), YELLOW, lit * 0.7));
        }
    }

    fn draw_surge_display(&self, painter: &egui::Painter, r: Rect) {
        let vis = 0.25 + 0.75 * self.units[1].led.max(
            self.cables.iter().filter(|c| c.src == 1 && c.kind == PortKind::Audio).map(|c| c.level).fold(0.0, f32::max));
        let (start, width) = self.display_area(1);
        if width < 40.0 { return; }
        let pitch = (width / 11.0).min(9.0);
        for i in 0..11 {
            let h = (0.25 + 0.75 * (self.ui_time * 2.6 + i as f32 * 0.55).sin().abs()) * 40.0 * vis;
            let bar = Rect::from_center_size(
                Pos2::new(start + pitch * 0.5 + i as f32 * pitch, r.center().y + 4.0),
                Vec2::new(pitch * 0.45, h.max(2.0)),
            );
            painter.rect_filled(self.xf.rect(bar), 2.0, alpha(BLUE, 0.5 + 0.5 * vis));
        }
    }

    fn draw_delay_display(&self, painter: &egui::Painter, r: Rect) {
        let led = self.units[2].led;
        let (start, width) = self.display_area(2);
        if width < 40.0 { return; }
        let pitch = (width / 4.5).min(26.0);
        for i in 0..4 {
            let c = Vec2::new(start + pitch * 0.5 + i as f32 * pitch, r.center().y + 4.0);
            let a = 0.15 + 0.85 * led * (1.0 - i as f32 * 0.2);
            self.ell(painter, c, 6.0 - i as f32 * 1.1, alpha(PURPLE, a), Stroke::NONE);
        }
        if led > 0.05 {
            let c = Vec2::new(start + pitch * 0.5, r.center().y + 4.0);
            self.ell(painter, c, 10.0 + (1.0 - led) * 14.0, alpha(PURPLE, 0.25 * led), Stroke::NONE);
        }
    }

    fn draw_meter(&self, painter: &egui::Painter, r: Rect) {
        let n = 14;
        let lit = (self.meter * n as f32).ceil() as usize;
        let (start, width) = self.display_area(3);
        if width < 40.0 { return; }
        let pitch = (width / n as f32).min(16.0);
        for i in 0..n {
            let seg = Rect::from_min_size(
                Pos2::new(start + i as f32 * pitch, r.center().y - 11.0),
                Vec2::new(pitch * 0.75, 26.0),
            );
            let col = if i < 8 { GREEN } else if i < 11 { YELLOW } else { ORANGE };
            let fill = if i < lit { col } else { Color32::from_rgb(0x26, 0x28, 0x38) };
            painter.rect_filled(self.xf.rect(seg), 2.5, fill);
        }
    }

    fn draw_knob(&self, painter: &egui::Painter, ui: usize, ki: usize) {
        let xf = &self.xf;
        let u = &self.units[ui];
        let k = &u.knobs[ki];
        let c = self.knob_center(ui, ki);
        let r = 17.0;
        let hovered = matches!(self.hover, Some(Hit::Knob { unit, idx }) if unit == ui && idx == ki);
        let dragging = matches!(self.drag, Drag::Knob { unit, idx, .. } if unit == ui && idx == ki);

        if hovered || dragging {
            self.ell(painter, c, r + 5.0, alpha(u.accent, 0.15), Stroke::NONE);
        }
        self.ell(painter, c, r, Color32::from_rgb(0x2a, 0x2d, 0x40),
            Stroke::new(1.4, if hovered || dragging { mix(RING, u.accent, 0.6) } else { RING }));

        // Value arc: 270° sweep from bottom-left to bottom-right.
        let a0 = 135.0f32.to_radians();
        let sweep = 270.0f32.to_radians();
        let arc_r = r + 4.5;
        let segs = 16;
        let mut prev_track: Option<Pos2> = None;
        let mut prev_val: Option<Pos2> = None;
        for i in 0..=segs {
            let f = i as f32 / segs as f32;
            let ang = a0 + sweep * f;
            let p = xf.p(c + Vec2::new(ang.cos(), ang.sin()) * arc_r);
            if let Some(pp) = prev_track {
                painter.line_segment([pp, p], Stroke::new(2.0, alpha(RING, 0.55)));
            }
            prev_track = Some(p);
            if f <= k.value {
                if let Some(pp) = prev_val {
                    painter.line_segment([pp, p], Stroke::new(2.4, u.accent));
                }
                prev_val = Some(p);
            }
        }
        // Pointer.
        let ang = a0 + sweep * k.value;
        let tip = xf.p(c + Vec2::new(ang.cos(), ang.sin()) * (r * 0.62));
        painter.line_segment([xf.p(c), tip], Stroke::new(2.4, TEXT));
        let lw = k.label_g.as_ref().map_or(0.0, |g| g.size().x);
        self.text(painter, c + Vec2::new(-lw / 2.0, r + 10.0), &k.label_g, SUBTEXT);
    }

    fn draw_back(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            let r = self.unit_rects[ui];
            painter.rect_filled(xf.rect(r), 10.0, PLATE_BACK);
            painter.rect_stroke(xf.rect(r), 10.0, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(7.0, r.height() - 24.0));
            painter.rect_filled(xf.rect(strip), 3.5, alpha(u.accent, 0.55));
            self.text(painter, r.min.to_vec2() + Vec2::new(32.0, 10.0), &u.back_g, SUBTEXT);
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
        if c.kind == PortKind::Audio {
            if c.src == 1 { col = mix(col, ORANGE, self.units[1].knobs[0].value * 0.45); }
            col = mix(col, ORANGE, self.units[3].knobs[1].value * 0.30);
        }
        col
    }

    fn draw_cables(&self, painter: &egui::Painter, hl: &[bool; MAX_CABLES], has_sel: bool) {
        let xf = &self.xf;
        for (ci, c) in self.cables.iter().enumerate() {
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.01 { continue; }
            let hovered = matches!(self.hover, Some(Hit::Cable(i, _)) if i == ci);
            let grabbed = matches!(self.drag, Drag::Cable { ci: i, .. } if i == ci);
            let volume = 0.15 + self.units[3].knobs[0].value * 0.85;
            let level = if c.kind == PortKind::Audio {
                ((c.level + 0.05 + 0.035 * (self.ui_time * 1.4 + c.phase).sin()) * volume).clamp(0.0, 1.0)
            } else { 0.0 };
            let mut width = match c.kind {
                PortKind::Midi => 2.8,
                PortKind::Audio => 3.2 + 2.6 * level,
            };
            if hovered || grabbed { width += 0.8; }

            let curve = [xf.p(c.curve[0].to_vec2()), xf.p(c.curve[1].to_vec2()),
                         xf.p(c.curve[2].to_vec2()), xf.p(c.curve[3].to_vec2())];
            let col = self.cable_color(c);

            if c.kind == PortKind::Audio {
                let gw = width + 5.0 + 10.0 * level;
                painter.add(bezier_shape(curve, gw, alpha(col, a * (0.05 + 0.22 * level))));
            }
            if has_sel && hl[ci] {
                painter.add(bezier_shape(curve, width + 7.0, alpha(WHITE, 0.10 * a)));
            }
            let mut main_col = col;
            if c.kind == PortKind::Audio { main_col = mix(main_col, WHITE, 0.22 * level); }
            if hovered || grabbed { main_col = mix(main_col, WHITE, 0.18); }
            painter.add(bezier_shape(curve, width, alpha(main_col, a)));
            if self.selected == Some(c.id) {
                painter.add(bezier_shape(curve, 1.1, alpha(WHITE, 0.55 * a)));
            }
        }
    }

    fn draw_jacks(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            for (ji, j) in u.jacks.iter().enumerate() {
                let c = self.jack_pos(ui, ji);
                let kind = u.kind(j.dir, j.port);
                let hovered = matches!(self.hover, Some(Hit::Jack { unit, jack }) if unit == ui && jack == ji);
                let compat = self.compatible(ui, ji);
                let socket = Rect::from_center_size(c.to_pos2(), Vec2::splat(24.0));
                painter.rect_filled(xf.rect(socket), 5.0, Color32::from_rgb(0x10, 0x11, 0x1a));
                painter.rect_stroke(xf.rect(socket), 5.0, Stroke::new(1.2, RING), egui::StrokeKind::Inside);
                if compat {
                    let pulse = 1.5 * (self.ui_time * 7.0).sin();
                    self.ell(painter, c, 15.0 + pulse, alpha(kind.color(), 0.0), Stroke::new(1.8, alpha(kind.color(), 0.65)));
                }
                if hovered || compat {
                    self.ell(painter, c, 16.0, alpha(kind.color(), 0.16), Stroke::NONE);
                }
                self.ell(painter, c, 6.5, Color32::from_rgb(0x0a, 0x0b, 0x10),
                    Stroke::new(1.4, if hovered { mix(RING, WHITE, 0.35) } else { RING }));
                self.ell(painter, c, 2.5, alpha(kind.color(), if compat || hovered { 1.0 } else { 0.8 }), Stroke::NONE);
                let jw = j.label_g.as_ref().map_or(0.0, |g| g.size().x);
                self.text(painter, c + Vec2::new(-jw / 2.0, 17.0), &j.label_g, SUBTEXT);
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
        for c in &self.cables {
            if c.dying >= 0.0 { continue; }
            let a = Self::cable_alpha(c, false, true);
            for end in [Dir::Out, Dir::In] {
                let p = if end == Dir::Out { c.over_a.unwrap_or(c.prev_a) } else { c.over_b.unwrap_or(c.prev_b) };
                let body = Rect::from_center_size(p.to_pos2(), Vec2::new(20.0, 16.0));
                painter.rect_filled(xf.rect(body), 3.5, alpha(PLUG, a));
                painter.rect_stroke(xf.rect(body), 3.5, Stroke::new(1.0, alpha(mix(PLUG, WHITE, 0.25), a)), egui::StrokeKind::Inside);
                let band = Rect::from_min_size(body.min + Vec2::new(2.0, 3.0), Vec2::new(16.0, 3.5));
                painter.rect_filled(xf.rect(band), 1.5, alpha(self.cable_color(c), a));
            }
        }
    }

    fn draw_pulses(&self, painter: &egui::Painter, hl: &[bool; MAX_CABLES], has_sel: bool) {
        let xf = &self.xf;
        for (ci, c) in self.cables.iter().enumerate() {
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.02 { continue; }
            let base_w = match c.kind { PortKind::Midi => 2.8, PortKind::Audio => 3.2 };
            let col = self.cable_color(c);
            for p in &c.pulses {
                if !p.active || p.t < 0.0 { continue; }
                let t = p.t.clamp(0.0, 1.0);
                let trail = 0.05 + 0.06 * p.amp;
                let u1 = c.arc_to_param(t);
                let u0 = c.arc_to_param((t - trail).max(0.0));
                if u1 > u0 + 1e-4 {
                    let seg = sub_cubic(&c.curve, u0, u1);
                    let seg = [xf.p(seg[0].to_vec2()), xf.p(seg[1].to_vec2()),
                               xf.p(seg[2].to_vec2()), xf.p(seg[3].to_vec2())];
                    painter.add(bezier_shape(seg, base_w + 1.2, alpha(mix(col, WHITE, 0.55), 0.9 * a)));
                }
                let pos = xf.p(c.point_at_arc(t).to_vec2());
                let glow_r = (6.0 + 5.0 * p.amp) * xf.sy.max(0.5);
                let core_r = (2.6 + 2.4 * p.amp) * xf.sy.max(0.5);
                painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
                    center: pos, radius: Vec2::new(glow_r * xf.sx, glow_r * xf.sy),
                    fill: alpha(col, 0.20 * a), stroke: Stroke::NONE, angle: 0.0,
                }));
                painter.add(Shape::Ellipse(egui::epaint::EllipseShape {
                    center: pos, radius: Vec2::new(core_r * xf.sx, core_r * xf.sy),
                    fill: alpha(mix(col, WHITE, 0.8), a), stroke: Stroke::NONE, angle: 0.0,
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
        painter.add(bezier_shape(curve, 6.5, alpha(col, 0.14)));
        painter.add(bezier_shape(curve, 2.8, alpha(col, 0.85)));
        let cursor = xf.p(p.cursor);
        painter.circle_filled(cursor, 6.5, alpha(col, 0.95));
        painter.circle_stroke(cursor, 9.5, Stroke::new(1.6, alpha(col, 0.5)));
    }

    fn draw_toolbar(&self, painter: &egui::Painter) {
        let bar = Rect::from_min_size(Pos2::new(16.0, 16.0), Vec2::new(376.0, 48.0));
        painter.rect_filled(bar, 24.0, alpha(Color32::from_rgb(0x1d, 0x1f, 0x2e), 0.94));
        painter.rect_stroke(bar, 24.0, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);

        let c = Pos2::new(40.0, 40.0);
        let hovered = matches!(self.hover, Some(Hit::Play));
        painter.circle_filled(c, 15.0, if hovered { mix(YELLOW, WHITE, 0.2) } else { YELLOW });
        let ink = BG;
        if self.playing {
            painter.rect_filled(Rect::from_min_size(Pos2::new(35.0, 33.5), Vec2::new(3.6, 13.0)), 1.2, ink);
            painter.rect_filled(Rect::from_min_size(Pos2::new(41.4, 33.5), Vec2::new(3.6, 13.0)), 1.2, ink);
        } else {
            painter.add(Shape::convex_polygon(
                vec![Pos2::new(36.5, 32.5), Pos2::new(36.5, 47.5), Pos2::new(48.5, 40.0)],
                ink, Stroke::NONE));
        }

        let beat_phase = ((self.time / BEAT).fract() as f32).clamp(0.0, 1.0);
        let thump = if self.playing { (1.0 - beat_phase).powi(3) } else { 0.0 };
        painter.circle_filled(Pos2::new(66.0, 40.0), 3.0 + 3.5 * thump, alpha(GREEN, 0.35 + 0.65 * thump));

        if let Some(g) = &self.wordmark_g { painter.galley(Pos2::new(80.0, 24.0), g.clone(), TEXT); }
        let state_g = if self.playing { &self.state_play_g } else { &self.state_pause_g };
        if let Some(g) = state_g { painter.galley(Pos2::new(80.0, 42.0), g.clone(), SUBTEXT); }

        // Flip button.
        let fb = Rect::from_min_size(Pos2::new(246.0, 24.0), Vec2::new(126.0, 32.0));
        let fhover = matches!(self.hover, Some(Hit::Flip));
        painter.rect_filled(fb, 16.0, if fhover { Color32::from_rgb(0x2c, 0x2f, 0x44) } else { Color32::from_rgb(0x26, 0x28, 0x3a) });
        painter.rect_stroke(fb, 16.0, Stroke::new(1.0, if fhover { mix(BORDER, YELLOW, 0.5) } else { BORDER }), egui::StrokeKind::Inside);
        // ⇄ arrows drawn as shapes (font-independent).
        let ax = 260.0;
        painter.line_segment([Pos2::new(ax, 34.0), Pos2::new(ax + 12.0, 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 8.0, 30.5), Pos2::new(ax + 12.0, 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 8.0, 37.5), Pos2::new(ax + 12.0, 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax, 46.0), Pos2::new(ax + 12.0, 46.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 4.0, 42.5), Pos2::new(ax, 46.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 4.0, 49.5), Pos2::new(ax, 46.0)], Stroke::new(1.8, TEXT));
        if let Some(g) = &self.flip_label_g { painter.galley(Pos2::new(280.0, 33.0), g.clone(), TEXT); }
    }

    fn draw_hint(&self, painter: &egui::Painter, rect: Rect) {
        let g = if self.pending.is_some() { &self.hint_wire_g }
            else if self.side == Side::Front { &self.hint_front_g }
            else { &self.hint_back_g };
        let Some(g) = g else { return };
        let w = g.size().x + 28.0;
        let bar = Rect::from_min_size(Pos2::new(16.0, rect.bottom() - 42.0), Vec2::new(w, 28.0));
        painter.rect_filled(bar, 14.0, alpha(Color32::from_rgb(0x1d, 0x1f, 0x2e), 0.9));
        painter.galley(bar.min + Vec2::new(14.0, 7.0), g.clone(), SUBTEXT);
    }

    fn draw_stats(&mut self, painter: &egui::Painter, rect: Rect) {
        self.stats_timer -= 1.0 / 60.0;
        if self.stats_timer <= 0.0 || self.stats_g.is_none() {
            self.stats_timer = 0.25;
            let pulses: usize = self.cables.iter().map(|c| c.pulses.iter().filter(|p| p.active).count()).sum();
            let text = format!(
                "{:.0} fps · {} cables · {} pulses · {}",
                self.fps, self.cables.len(), pulses,
                if self.side == Side::Front { "front" } else { "back" });
            self.stats_g = Some(painter.layout(text, FontId::monospace(11.0), SUBTEXT, f32::INFINITY));
        }
        if let Some(g) = &self.stats_g {
            let w = g.size().x + 24.0;
            let bar = Rect::from_min_size(Pos2::new(rect.right() - w - 16.0, 16.0), Vec2::new(w, 26.0));
            painter.rect_filled(bar, 13.0, alpha(Color32::from_rgb(0x1d, 0x1f, 0x2e), 0.9));
            painter.galley(bar.min + Vec2::new(12.0, 6.0), g.clone(), SUBTEXT);
        }
    }

    fn draw_help(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 0.0, alpha(BLACK, 0.45));
        let card = Rect::from_center_size(rect.center(), Vec2::new(560.0, 352.0));
        painter.rect_filled(card, 18.0, Color32::from_rgb(0x1c, 0x1e, 0x2c));
        painter.rect_stroke(card, 18.0, Stroke::new(1.2, Color32::from_rgb(0x3a, 0x3e, 0x56)), egui::StrokeKind::Inside);
        if let Some(g) = &self.help_title_g { painter.galley(card.min + Vec2::new(28.0, 24.0), g.clone(), TEXT); }
        if let Some(g) = &self.help_body_g { painter.galley(card.min + Vec2::new(28.0, 62.0), g.clone(), TEXT); }
        if let Some(g) = &self.help_foot_g { painter.galley(card.min + Vec2::new(28.0, 318.0), g.clone(), SUBTEXT); }
    }
}

fn dt_of(ctx: &egui::Context) -> f32 {
    ctx.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0)
}

fn p_side(unit: usize, app: &DemoApp) -> f32 {
    let x = app.jack_pos(unit, 0).x;
    if x < app.rack_w / 2.0 { -1.0 } else { 1.0 }
}

impl eframe::App for DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.ensure_galleys(&ctx);

        // Fractional-HiDPI fix: winit reports scale 1.0 on Hyprland's 1.6x
        // output, and eframe re-applies it AFTER the app creator runs — so the
        // correction must live here, on the frame path, not in the creator.
        if ctx.pixels_per_point() < 1.05 {
            ctx.set_pixels_per_point(1.6);
            eprintln!("ui scale: corrected to 1.60 on frame path");
        }
        static SCALE_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SCALE_LOGGED.swap(true, Ordering::Relaxed) {
            eprintln!("ui scale: first-frame ppp={:.2}", ctx.pixels_per_point());
        }

        // Tab ALWAYS flips the rack — consume it before egui's focus navigation
        // or the typing gate can steal it (the assistant panel has focusable
        // widgets; without this, Tab silently moves focus instead of flipping).
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            self.start_flip();
        }

        // Assistant panel takes the right strip; the rack gets the rest.
        self.assistant_panel_ui(ui);

        // Laptop keys (disabled while the assistant text field has focus).
        if !ctx.egui_wants_keyboard_input() {
            let events = ctx.input(|i| i.events.clone());
            for ev in events {
                if let egui::Event::Key { key, pressed, repeat, .. } = ev {
                    self.handle_laptop_key(key, pressed, repeat);
                }
            }
        }
        self.poll_assistant();

        // Scheduled note-offs (assistant play_note tool).
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.note_offs.len() {
            if self.note_offs[i].0 <= now {
                let (_, semi) = self.note_offs.remove(i);
                self.fire_keys(semi, 0.0);
            } else {
                i += 1;
            }
        }

        // Hardware MIDI → fire_keys (sound + cable animation in sync).
        let hw: Vec<NoteEvent> = self
            .hw_queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        for ev in hw {
            let semi = (ev.note as i16 - 60).max(0) as usize;
            self.fire_keys(semi, ev.velocity);
        }

        let rect = ui.available_rect_before_wrap();
        let dt = ctx.input(|i| i.stable_dt).clamp(1.0 / 240.0, 1.0 / 20.0);
        self.fps += (1.0 / dt - self.fps) * 0.05;

        self.layout(rect);
        if !ctx.egui_wants_keyboard_input() {
            self.handle_input(&ctx, rect);
        }
        self.update(dt);

        // Publish the patch: cables and knobs drive the audio engine.
        if let Some(sh) = &self.audio_shared {
            let mut r = 0u32;
            for c in &self.cables {
                if c.dying >= 0.0 {
                    continue;
                }
                match (c.src, c.dst, c.kind) {
                    (0, 1, PortKind::Midi) => r |= R_KEYS_SURGE,
                    (1, 2, PortKind::Audio) => r |= R_SURGE_DELAY,
                    (2, 3, PortKind::Audio) => r |= R_DELAY_MASTER,
                    (1, 3, PortKind::Audio) => r |= R_SURGE_MASTER,
                    _ => {}
                }
            }
            sh.routes.store(r, Ordering::Relaxed);
            AudioShared::set_f(&sh.surge_level, self.units[1].knobs[1].value);
            AudioShared::set_f(&sh.delay_time, self.units[2].knobs[0].value);
            AudioShared::set_f(&sh.delay_mix, self.units[2].knobs[1].value);
            AudioShared::set_f(&sh.master_vol, self.units[3].knobs[0].value);
            AudioShared::set_f(&sh.master_warm, self.units[3].knobs[1].value);
        }

        self.tessellate();
        self.draw(ui);

        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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
fn preferred_host() -> cpal::Host {
    for id in cpal::available_hosts() {
        if id.name().to_lowercase().contains("pipewire") {
            if let Ok(h) = cpal::host_from_id(id) {
                return h;
            }
        }
    }
    cpal::default_host()
}

/// Load Surge XT and open the audio stream. Called on the main thread inside
/// the eframe creator (CLAP main-thread rules). The voice itself moves into
/// the cpal callback; the controller stays available for the note sink.
fn build_audio() -> anyhow::Result<(ClapVoiceMain, cpal::Stream, VoiceController, Arc<AudioShared>)> {
    let host = preferred_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let cfg = device.default_output_config()?;
    let sample_rate = cfg.sample_rate() as f64;
    let ch = cfg.channels() as usize;
    let (voice, main) = ClapVoice::new(sample_rate)?;
    let ctrl = voice.controller();
    let mut config: cpal::StreamConfig = cfg.into();
    // Low latency from the start: 256 frames @48k = 5.3 ms. On the native
    // PipeWire host this sets the stream's node latency, which drives the
    // graph quantum — no ALSA bridge buffering in between.
    config.buffer_size = cpal::BufferSize::Fixed(256);
    eprintln!(
        "audio: host={} device={} rate={} ch={} buffer=256 frames ({:.1} ms)",
        host.id().name(),
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        sample_rate,
        ch,
        256.0 / sample_rate * 1000.0
    );
    let mut voice = voice;
    let mut scratch = vec![0.0f32; 8192 * 2];
    let shared = AudioShared::new();
    let sh = shared.clone();
    let mut delay = DelayLine::new(sample_rate, 2.0);
    let mut delay_in = vec![0.0f32; 8192 * 2];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            let frames = data.len() / ch;
            if frames * 2 > scratch.len() {
                for s in data.iter_mut() {
                    *s = 0.0;
                }
                return;
            }
            voice.render(&mut scratch[..frames * 2]);
            let routes = sh.routes();
            let level = sh.f(&sh.surge_level);
            let vol = sh.f(&sh.master_vol);
            let warm = sh.f(&sh.master_warm);
            let time = sh.f(&sh.delay_time);
            let mix = sh.f(&sh.delay_mix);
            for s in scratch[..frames * 2].iter_mut() {
                *s *= level;
            }
            // Delay path: feed Surge when wired, silence otherwise (tail rings out).
            if routes & R_SURGE_DELAY != 0 {
                delay_in[..frames * 2].copy_from_slice(&scratch[..frames * 2]);
            } else {
                for s in delay_in[..frames * 2].iter_mut() {
                    *s = 0.0;
                }
            }
            delay.process(&mut delay_in[..frames * 2], time, mix);
            let dry = routes & R_SURGE_MASTER != 0;
            let wet = routes & R_DELAY_MASTER != 0;
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                if dry {
                    l += scratch[i * 2];
                    r += scratch[i * 2 + 1];
                }
                if wet {
                    l += delay_in[i * 2];
                    r += delay_in[i * 2 + 1];
                }
                l = (l * (1.0 + warm * 3.0)).tanh() * vol;
                r = (r * (1.0 + warm * 3.0)).tanh() * vol;
                frame[0] = l;
                if ch > 1 {
                    frame[1] = r;
                }
                for s in frame.iter_mut().skip(2) {
                    *s = 0.0;
                }
            }
        },
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((main, stream, ctrl, shared))
}

fn run_inner(sink: Option<NoteSink>, with_audio: bool) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_min_inner_size([760.0, 520.0])
            .with_maximized(true)
            .with_app_id("kabelsalat")
            .with_title("kabelsalat"),
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
            if ppp < 1.05 {
                cc.egui_ctx.set_pixels_per_point(1.6);
            }
            eprintln!(
                "ui scale: detected={ppp:.2} used={:.2}",
                cc.egui_ctx.pixels_per_point()
            );
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = BG;
            visuals.window_fill = BG;
            visuals.override_text_color = Some(TEXT);
            cc.egui_ctx.set_visuals(visuals);

            let mut sink = sink;
            let mut audio = None;
            if with_audio {
                match build_audio() {
                    Ok((main, stream, ctrl, shared)) => {
                        let sh = shared.clone();
                        sink = Some(Box::new(move |ev: NoteEvent| {
                            // The Keys→Surge MIDI cable is real: unplugged = silent.
                            if sh.routes() & R_KEYS_SURGE == 0 {
                                return;
                            }
                            let note = 60 + ev.note as u16;
                            if ev.velocity > 0.0 {
                                ctrl.note_on(note, ev.velocity as f64);
                            } else {
                                ctrl.note_off(note);
                            }
                        }));
                        audio = Some((main, stream, shared));
                    }
                    Err(e) => eprintln!("Surge/audio unavailable, running silent: {e}"),
                }
            }
            let app = DemoApp::new_with_sink(sink);
            let app = match audio {
                Some((main, stream, shared)) => app.with_audio(main, stream, shared),
                None => app,
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))
}