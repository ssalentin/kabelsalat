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
//! Each cable is a Verlet rope (`rope.rs`, ported from the salen7in site):
//! point masses under gravity with distance constraints, plugged ends pinned
//! to their jacks, a free end left to fall. Grabbing a cable drags its nearest
//! point; dragging a plug moves an endpoint — the cable whips and settles.
//! MIDI cables carry pulses fired by a simulated 96 BPM band; audio cables
//! breathe with envelopes triggered when pulses arrive.
//!
//! Performance: fixed-size rope and sample arrays, fixed-size arc-length
//! LUTs, cached galleys, fixed pulse arrays — no per-frame allocation
//! storms. Pure snapshot-style simulation; never touches audio.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui::{
    self, Color32, CursorIcon, FontId, Galley, Key, Modifiers, Pos2, Rect, Shape, Stroke, Vec2,
};
use midir::MidiInput;

use crate::assistant::{Assistant, ChatLine, FromWorker};
use crate::control::{
    name_bytes, name_str, Command, CommandBus, DspKind, Graph, Lv2Uri, Node, NodeKind,
    PortKind as CPortKind, PortRef,
};
use crate::engine::{GraphEngine, P_LEVEL, P_MIX, P_TIME, P_VOL, P_WARM};
use crate::host::{
    list_clap_plugins, ClapPluginInfo, ClapVoice, ClapVoiceMain, ParamDesc, VoiceController,
};
use clack_host::prelude::ClapId;
use crate::lv2::{discover as lv2_discover, Lv2PluginInfo};
use crate::matrix::SourceId;
use crate::rope::{self, Rope};
use crate::session::Template;
use crate::theme;

// ---------------------------------------------------------------------------
// Palette — Signal design language (src/theme.rs, docs/design-language.md).
// The legacy names stay as aliases so the drawing code reads naturally;
// all values come from the single theme source.
// ---------------------------------------------------------------------------

const BG: Color32 = theme::BG_DEEP;
const YELLOW: Color32 = theme::MIDI_AMBER; // MIDI
const BLUE: Color32 = theme::AUDIO_CYAN; // audio A
const GREEN: Color32 = theme::LIVE_GREEN; // active / playing / sources
const PURPLE: Color32 = theme::ACCENT_VIOLET; // selection
const ORANGE: Color32 = theme::WARN_AMBER;
const CHASSIS: Color32 = theme::BG_RAISED;
const RAIL: Color32 = theme::SURFACE;
const PLATE_FRONT: Color32 = theme::SURFACE;
const PLATE_BACK: Color32 = Color32::from_rgb(0x0d, 0x12, 0x21); // SURFACE → BG_DEEP 45%
const BORDER: Color32 = theme::HAIRLINE;
const RING: Color32 = Color32::from_rgb(0x42, 0x4f, 0x6b); // HAIRLINE → TEXT_DIM 35%
const PLUG: Color32 = theme::SURFACE_HI;
const TEXT: Color32 = theme::TEXT;
const SUBTEXT: Color32 = theme::TEXT_DIM;
const WHITE: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
const BLACK: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);

/// The reference CLAP instrument (PLAN §1). The engine's two voices are Surge,
/// so it is the only CLAP plugin that can currently be added from the browser.
const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

const MAX_SAMPLES: usize = rope::MAX_SAMPLES;
const MAX_PULSES: usize = 12;
const MAX_CABLES: usize = 64;
const FLIP_SECS: f32 = 0.35;

const WHITE_SEMI: [usize; 8] = [0, 2, 4, 5, 7, 9, 11, 12];
const BLACK_SEMI: [usize; 5] = [1, 3, 6, 8, 10];
const BLACK_AFTER: [usize; 5] = [0, 1, 3, 4, 5]; // white-key index each black key follows

// ---------------------------------------------------------------------------
// Small helpers (no allocation)
// ---------------------------------------------------------------------------

// alpha/mix live in the theme module (single source for the Signal palette).
use crate::theme::{alpha, mix};

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
    /// Engine param id this knob publishes (None = visual-only knob).
    param: Option<u32>,
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

/// What a rack unit IS — drives its face, knobs, and layout grouping.
#[derive(Clone)]
enum UnitKind {
    Keys,
    Clap,
    Dsp(crate::control::DspKind),
    Master,
}

struct Unit {
    /// Engine node id (control-plane Graph). Unit index ≠ node id in general.
    node_id: u32,
    node_kind: UnitKind,
    name: String,
    sub: String,
    accent: Color32,
    ins: [PortKind; 3],
    outs: [PortKind; 3],
    knobs: Vec<Knob>,
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
        if dir == Dir::In {
            self.ins.get(port).copied().unwrap_or(PortKind::Audio)
        } else {
            self.outs.get(port).copied().unwrap_or(PortKind::Audio)
        }
    }
}

/// Build a knob. `param` = engine param id it publishes (None = visual-only).
fn knob(label: &'static str, default: f32, param: Option<u32>) -> Knob {
    Knob { label, value: default, default, param, label_g: None }
}

/// Value of the knob bound to `param`, or `default` if the unit has no such knob.
fn knob_val(u: &Unit, param: u32, default: f32) -> f32 {
    u.knobs.iter().find(|k| k.param == Some(param)).map(|k| k.value).unwrap_or(default)
}

/// Visual breathing factor for audio leaving a unit (the engine does the real
/// audio; this only drives the cable glow). Clap → LEVEL, Delay → MIX, other
/// DSP → its first knob, everything else → 1.
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

/// Accent color for a built-in DSP kind (generic faces).
fn dsp_accent(kind: &DspKind) -> Color32 {
    match kind {
        DspKind::Delay => PURPLE,
        DspKind::Reverb => theme::AUDIO_BLUE,
        DspKind::Gain => GREEN,
        DspKind::Filter => PURPLE,
        DspKind::Eq3 => BLUE,
        DspKind::Limiter => ORANGE,
        DspKind::Saturation => theme::ACCENT_MAGENTA,
        DspKind::Pan => YELLOW,
        DspKind::Metronome => YELLOW,
    }
}

/// Build a rack unit from a control-plane node. The face (jacks + knobs) is
/// chosen by node kind; known kinds keep their bespoke faces, other DSP kinds
/// get a generic labeled box with 1–2 knobs bound to engine params 0/1.
fn make_unit(node: &Node) -> Unit {
    let name = name_str(&node.name).to_string();
    let (node_kind, sub, accent, ins, outs, knobs, jacks): (
        UnitKind, String, Color32, Vec<PortKind>, Vec<PortKind>, Vec<Knob>, Vec<(Dir, usize, f32, &'static str)>,
    ) = match &node.kind {
        NodeKind::Keys => (
            UnitKind::Keys, "midi source · click the keys".into(), YELLOW,
            vec![], vec![PortKind::Midi],
            vec![knob("RATE", 0.5, None), knob("VEL", 0.8, None)],
            vec![(Dir::Out, 0, 0.62, "MIDI OUT")],
        ),
        NodeKind::Clap { .. } => (
            UnitKind::Clap, "synth · clap".into(), BLUE,
            vec![PortKind::Midi], vec![PortKind::Audio],
            vec![knob("TONE", 0.35, None), knob("LEVEL", 0.85, Some(P_LEVEL))],
            vec![(Dir::In, 0, 0.30, "MIDI IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::MasterOut => (
            UnitKind::Master, "stereo + mono bus".into(), theme::ACCENT_MAGENTA,
            vec![PortKind::Audio, PortKind::Audio, PortKind::Audio], vec![],
            vec![knob("VOLUME", 0.8, Some(P_VOL)), knob("WARM", 0.15, Some(P_WARM))],
            vec![(Dir::In, 0, 0.30, "L"), (Dir::In, 2, 0.50, "MONO"), (Dir::In, 1, 0.70, "R")],
        ),
        NodeKind::Dsp(DspKind::Delay) => (
            UnitKind::Dsp(DspKind::Delay), "insert fx · midi tap".into(), PURPLE,
            vec![PortKind::Audio, PortKind::Midi], vec![PortKind::Audio],
            vec![knob("TIME", 0.4, Some(P_TIME)), knob("MIX", 0.5, Some(P_MIX))],
            vec![(Dir::In, 0, 0.22, "IN"), (Dir::In, 1, 0.44, "MIDI"), (Dir::Out, 0, 0.76, "OUT")],
        ),
        NodeKind::Dsp(kind) => {
            // Generic DSP face: 1–2 knobs on engine params 0/1.
            let (knobs, sub): (Vec<Knob>, String) = match kind {
                DspKind::Reverb => (vec![knob("MIX", 0.35, Some(0)), knob("SIZE", 0.5, Some(1))], "reverb".into()),
                DspKind::Gain => (vec![knob("GAIN", 0.9, Some(0))], "gain".into()),
                DspKind::Filter => (vec![knob("CUTOFF", 0.45, Some(0)), knob("RESO", 0.2, Some(1))], "filter".into()),
                DspKind::Eq3 => (vec![knob("LOW", 0.5, Some(0)), knob("HIGH", 0.5, Some(1))], "3-band eq".into()),
                DspKind::Limiter => (vec![knob("CEIL", 0.8, Some(0)), knob("DRIVE", 0.2, Some(1))], "limiter".into()),
                DspKind::Saturation => (vec![knob("DRIVE", 0.4, Some(0)), knob("MIX", 0.5, Some(1))], "saturation".into()),
                DspKind::Pan => (vec![knob("PAN", 0.5, Some(0))], "panner".into()),
                DspKind::Metronome => (vec![], "metronome · transport click".into()),
                DspKind::Delay => unreachable!(),
            };
            (
                UnitKind::Dsp(kind.clone()), sub, dsp_accent(kind),
                vec![PortKind::Audio], vec![PortKind::Audio],
                knobs,
                vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
            )
        }
        NodeKind::Lv2 { .. } => (
            UnitKind::Dsp(DspKind::Gain), "lv2 effect".into(), theme::AUDIO_BLUE,
            vec![PortKind::Audio], vec![PortKind::Audio],
            vec![knob("GAIN", 0.9, Some(0))],
            vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::SystemAudio => (
            UnitKind::Dsp(DspKind::Gain), "system audio".into(), GREEN,
            vec![], vec![PortKind::Audio],
            vec![knob("GAIN", 0.5, Some(0))],
            vec![(Dir::Out, 0, 0.62, "OUT")],
        ),
    };

    let mut ins_arr = [PortKind::Audio; 3];
    let mut outs_arr = [PortKind::Audio; 3];
    for (i, k) in ins.iter().take(3).enumerate() { ins_arr[i] = *k; }
    for (i, k) in outs.iter().take(3).enumerate() { outs_arr[i] = *k; }
    Unit {
        node_id: node.id,
        node_kind,
        name,
        sub,
        accent,
        ins: ins_arr,
        outs: outs_arr,
        knobs,
        jacks: jacks.iter().map(|&(dir, port, xf, label)| Jack { dir, port, xf, label, label_g: None }).collect(),
        led: 0.0,
        name_g: None, sub_g: None, back_g: None,
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

    /// Laid out on the first update, once the unit rects (and so the jack
    /// positions) exist.
    rope: Option<Rope>,
    // Endpoint overrides while a plug is being dragged.
    over_a: Option<Vec2>,
    over_b: Option<Vec2>,

    // Local-space samples + cumulative arc length, rebuilt each frame.
    pts: [Pos2; MAX_SAMPLES],
    lut: [f32; MAX_SAMPLES],
    n: usize,
    total_len: f32,

    pulses: [Pulse; MAX_PULSES],
    level: f32,
    env: f32,
    phase: f32,

    age: f32,
    dying: f32, // < 0 alive; else unplug-fall progress (seconds)
    detach_dst: bool,

    next_event: f64,
    pat_idx: usize,
    rng: u32,
}

impl Cable {
    fn new(id: u32, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind, color: Color32, pattern: u8, seed: u32) -> Self {
        let dead = Pulse { active: false, t: 0.0, speed: 0.0, amp: 0.0 };
        Self {
            id, src, src_port, dst, dst_port, kind, color, pattern,
            rope: None,
            over_a: None, over_b: None,
            pts: [Pos2::ZERO; MAX_SAMPLES], lut: [0.0; MAX_SAMPLES],
            n: 0, total_len: 0.0,
            pulses: [dead; MAX_PULSES],
            level: 0.0, env: 0.0, phase: (seed % 628) as f32 / 100.0,
            age: 0.0, dying: -1.0, detach_dst: true,
            next_event: 0.0, pat_idx: 0, rng: seed | 1,
        }
    }

    fn spawn_pulse(&mut self, t: f32, amp: f32) {
        let speed = 1.55 * (0.9 + 0.25 * rng01(&mut self.rng));
        let slot = self.pulses.iter().position(|p| !p.active).unwrap_or(0);
        self.pulses[slot] = Pulse { active: true, t, speed, amp };
    }

    /// Sample segment and fraction within it at arc-length fraction `frac`.
    fn arc_seg(&self, frac: f32) -> (usize, f32) {
        let s = frac.clamp(0.0, 1.0) * self.total_len;
        let n = self.n;
        if n < 2 { return (0, 0.0); }
        let i = self.lut[1..n - 1].partition_point(|&l| l < s);
        let span = self.lut[i + 1] - self.lut[i];
        let f = if span > 1e-6 { (s - self.lut[i]) / span } else { 0.0 };
        (i, f.clamp(0.0, 1.0))
    }

    fn point_at_arc(&self, frac: f32) -> Pos2 {
        if self.n < 2 { return self.pts[0]; }
        let (i, f) = self.arc_seg(frac);
        self.pts[i] + (self.pts[i + 1] - self.pts[i]) * f
    }

    /// The stretch of cable between arc-length fractions `f0..f1`.
    fn arc_slice(&self, f0: f32, f1: f32) -> Vec<Pos2> {
        let (i0, _) = self.arc_seg(f0);
        let (i1, _) = self.arc_seg(f1);
        let mut out = Vec::with_capacity(i1 - i0 + 2);
        out.push(self.point_at_arc(f0));
        out.extend_from_slice(&self.pts[i0 + 1..=i1]);
        out.push(self.point_at_arc(f1));
        out
    }
}

struct Pending {
    unit: usize,
    jack: usize,
    dir: Dir,
    kind: PortKind,
    cursor: Vec2, // rack-local
    rope: Rope,
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
    /// Ctrl+drag a rack unit to rearrange it (task-21). `grab` is the pointer's
    /// rack-local offset inside the unit at press, so the box tracks the cursor.
    Unit { unit: usize, grab: Vec2 },
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
    playing: bool, // transport state: engine metronome running
    bpm: f32,      // metronome tempo (20..=300), mirrors Command::SetBpm

    drag: Drag,
    press: Option<(Hit, Pos2)>,
    pending: Option<Pending>,
    selected: Option<u32>,
    hover: Option<Hit>,

    key_leds: [f32; 13],
    meter: f32,
    /// Peak-hold for the master meter (slow release, Signal design language).
    meter_peak: f32,

    show_help: bool,
    show_stats: bool,
    show_sessions: bool,
    session_name: String,
    session_msg: String,
    // Plugin browser (task-18): add hosted LV2/CLAP plugins to a rig.
    show_plugins: bool,
    plugins_loaded: bool,
    plugins_clap: Vec<ClapPluginInfo>,
    plugins_lv2: Vec<Lv2PluginInfo>,
    plugin_filter: String,
    plugin_msg: String,
    show_transport: bool,
    fps: f32,
    stats_timer: f32,
    stats_g: Option<Arc<Galley>>,

    // Per-frame layout / transform.
    rack_w: f32,
    rack_h: f32,
    /// Unsimulated time carried to the next frame (fixed rope step).
    rope_acc: f32,
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

    /// Set by raw_input_hook when Tab is pressed (stripped from egui's input
    /// so focus navigation never sees it); consumed in ui().
    flip_requested: bool,

    /// Source tag applied to the next fire_keys (set by each input path).
    current_source: SourceId,

    /// Per-rig input selection (keys, laptop, hardware) for rigs A=node1, B=node4.
    rig_sources: [(bool, bool, bool); 2],

    /// Keyboard split at C4 across the two rigs (A=low, B=high).
    split_on: bool,

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
    /// Connected hardware MIDI device names, indexed by device id.
    hw_devices: Vec<String>,
    /// Per-rig hardware device selection (None = any), rigs A=node1, B=node4.
    rig_device: [Option<u32>; 2],
    surge_status: String,
    // !Send: CLAP main-thread objects; DemoApp never leaves the UI thread.
    _surge_main: Vec<ClapVoiceMain>,
    /// RT-safe parameter setters, one per hosted voice (rig A/B). Grabbed
    /// before the voices move into the engine; used by the native editor.
    surge_controllers: Vec<VoiceController>,
    _audio_stream: Option<cpal::Stream>,
    bus: Option<Arc<Mutex<CommandBus>>>,
    out_level: Option<Arc<std::sync::atomic::AtomicU32>>,

    /// Live patch mirrored for the control bridge (task-16): MCP/CLI queries
    /// read this snapshot. Rebuilt from the living cables whenever they change.
    shared_graph: Arc<Mutex<Graph>>,
    /// Multi-rig list/active published by the engine (for the rig overview UI).
    rig_snapshot: Arc<Mutex<crate::control::RigSnapshot>>,
    /// File-player control (decode thread); engine owns the ring consumer.
    player_ctrl: Option<crate::player::PlayerControl>,
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
    editor_status: String,
    /// Last single click on a unit (unit, time) for double-click detection.
    last_unit_click: Option<(usize, std::time::Instant)>,
}

impl DemoApp {
    fn new_with_sink(note_sink: Option<NoteSink>) -> Self {
        let mut app = Self {
            units: Vec::new(),
            cables: Vec::with_capacity(MAX_CABLES),
            next_id: 1,
            audio_color_idx: 0,
            side: Side::Front,
            flipping: false,
            flip_t: 1.0,
            flip_to: Side::Front,
            time: 0.0,
            ui_time: 0.0,
            playing: false,
            bpm: 96.0,
            drag: Drag::None,
            press: None,
            pending: None,
            selected: None,
            hover: None,
            key_leds: [0.0; 13],
            meter: 0.0,
            meter_peak: 0.0,
            show_help: false,
            show_stats: false,
            show_sessions: false,
            session_name: String::new(),
            session_msg: String::new(),
            show_plugins: false,
            plugins_loaded: false,
            plugins_clap: Vec::new(),
            plugins_lv2: Vec::new(),
            plugin_filter: String::new(),
            plugin_msg: String::new(),
            show_transport: false,
            fps: 60.0,
            stats_timer: 0.0,
            stats_g: None,
            rack_w: 640.0,
            rack_h: 560.0,
            rope_acc: 0.0,
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
            flip_requested: false,
            current_source: SourceId::Keys,
            rig_sources: [(true, true, true); 2],
            split_on: false,
            assistant: Assistant::new(),
            key_input: String::new(),
            chat_input: String::new(),
            octave: 4,
            held_keys: Vec::new(),
            note_offs: Vec::new(),
            hw_queue: Arc::new(Mutex::new(VecDeque::new())),
            _hw_conns: Vec::new(),
            hw_status: String::new(),
            hw_devices: Vec::new(),
            rig_device: [None, None],
            surge_status: "no synth loaded".into(),
            _surge_main: Vec::new(),
            surge_controllers: Vec::new(),
            _audio_stream: None,
            bus: None,
            out_level: None,
            shared_graph: Arc::new(Mutex::new(Graph::demo_patch())),
            rig_snapshot: Arc::new(Mutex::new(crate::control::RigSnapshot::default())),
            player_ctrl: None,
            graph_sig: Vec::new(),
            editor_open: false,
            editor_rig: 0,
            editor_params: Vec::new(),
            editor_filter: String::new(),
            editor_request: None,
            editor_status: String::new(),
            last_unit_click: None,
        };

        // Hardware MIDI auto-wire at startup: events queue here and are drained
        // on the UI thread through fire_keys, so sound and cable animation stay
        // in sync.
        let mut hw_names = Vec::new();
        let mut hw_devices: Vec<String> = Vec::new();
        let mut dev_idx = 0u32;
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
                    let dev = dev_idx;
                    if let Ok(conn) = mi.connect(
                        &port,
                        "kabelsalat-hw",
                        move |_stamp, msg: &[u8], _| {
                            if msg.len() >= 3 {
                                let st = msg[0] & 0xF0;
                                if st == 0x90 || st == 0x80 {
                                    let vel = if st == 0x90 { msg[2] as f32 / 127.0 } else { 0.0 };
                                    if let Ok(mut q) = q.lock() {
                                        q.push_back(NoteEvent { note: msg[1], velocity: vel, source: SourceId::Hardware(dev), created: std::time::Instant::now() });
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
                        hw_names.push(name.clone());
                        hw_devices.push(name);
                        dev_idx += 1;
                    }
                }
            }
        }
        app.hw_devices = hw_devices;
        app.hw_status = if hw_names.is_empty() {
            "no hardware MIDI — laptop keys live".into()
        } else {
            format!("MIDI in: {}", hw_names.join(", "))
        };

        // Default rack = the engine's demo patch (2 stacks). build_rack mirrors
        // its nodes/edges into units/cables; ids match so per-frame Connect
        // publishes are engine-side duplicates.
        app.build_rack(&Graph::demo_patch(), &[]);
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
        self.compute_groups();
        // Reset interaction state that referenced the old rack.
        self.selected = None;
        self.pending = None;
        self.drag = Drag::None;
        self.press = None;
        self.hover = None;
        self.graph_sig.clear(); // force shared-graph re-mirror
    }

    /// Derive rack structure: Keys (left), Master (right), and one middle
    /// column per instrument (Clap) followed by its downstream DSP chain.
    fn compute_groups(&mut self) {
        self.keys_unit = self.units.iter().position(|u| matches!(u.node_kind, UnitKind::Keys));
        self.master_unit = self.units.iter().position(|u| matches!(u.node_kind, UnitKind::Master));
        let mut claimed = vec![false; self.units.len()];
        if let Some(k) = self.keys_unit { claimed[k] = true; }
        if let Some(m) = self.master_unit { claimed[m] = true; }
        let mut cols: Vec<Vec<usize>> = Vec::new();
        for i in 0..self.units.len() {
            if !matches!(self.units[i].node_kind, UnitKind::Clap) { continue; }
            let mut col = vec![i];
            claimed[i] = true;
            let mut cur = i;
            // Follow the audio chain instrument → DSP → DSP …
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
        // Any DSP not claimed by an instrument gets its own column.
        for i in 0..self.units.len() {
            if !claimed[i] && matches!(self.units[i].node_kind, UnitKind::Dsp(_)) {
                claimed[i] = true;
                cols.push(vec![i]);
            }
        }
        self.layout_rows = cols.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        self.stack_cols = cols;
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

    /// Ctrl+drag release (task-21): move `unit` to the slot nearest the drop
    /// point by reordering `stack_cols`; layout() then reflows with no overlaps.
    /// Unit indices — and therefore cable endpoints — are untouched.
    fn drop_unit(&mut self, unit: usize, drop: Vec2) {
        if self.keys_unit == Some(unit) || self.master_unit == Some(unit) { return; }
        let src = self.stack_cols.iter().enumerate()
            .find_map(|(j, col)| col.iter().position(|&u| u == unit).map(|r| (j, r)));
        let Some((sj, sr)) = src else { return };

        // Target column = nearest column x-center, judged from the OTHER units
        // (the dragged unit's rect is currently at the cursor, so skip it).
        let mut tj = sj;
        let mut best_dx = f32::INFINITY;
        for (j, col) in self.stack_cols.iter().enumerate() {
            if let Some(r) = col.iter().find(|&&u| u != unit).copied() {
                let dx = (drop.x - self.unit_rects[r].center().x).abs();
                if dx < best_dx { best_dx = dx; tj = j; }
            }
        }
        // Target row from the drop height.
        let gap = 10.0;
        let pad = 16.0;
        let len = self.stack_cols[tj].len() as i32;
        let tr = (((drop.y - pad) / (self.unit_h + gap)).floor() as i32).clamp(0, len);

        // Remove from source, then insert at target (adjust for same-column shift).
        self.stack_cols[sj].remove(sr);
        let mut tr = tr as usize;
        if sj == tj && sr < tr { tr -= 1; }
        let tr = tr.min(self.stack_cols[tj].len());
        self.stack_cols[tj].insert(tr, unit);
        self.stack_cols.retain(|c| !c.is_empty());
        self.layout_rows = self.stack_cols.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
    }

    /// Galleys are laid out on the first frame: egui loads fonts only once
    /// `Context::run` has started, so the app creator cannot do this.
    fn ensure_galleys(&mut self, ctx: &egui::Context) {
        let layout = |text: &str, font: FontId, color: Color32, wrap: f32| -> Arc<Galley> {
            ctx.fonts_mut(|f| f.layout(text.to_owned(), font, color, wrap))
        };
        let label_font = FontId::proportional(10.0);
        // Unit galleys: (re)fill any that are missing — templates rebuild units.
        for u in &mut self.units {
            if u.name_g.is_none() {
                u.name_g = Some(layout(&u.name, theme::bold_font(16.0), TEXT, f32::INFINITY));
                u.sub_g = Some(layout(&u.sub, FontId::proportional(10.5), SUBTEXT, f32::INFINITY));
                u.back_g = Some(layout(&u.name, FontId::proportional(11.0), SUBTEXT, f32::INFINITY));
            }
            for k in &mut u.knobs {
                if k.label_g.is_none() {
                    k.label_g = Some(layout(k.label, label_font.clone(), SUBTEXT, f32::INFINITY));
                }
            }
            for j in &mut u.jacks {
                if j.label_g.is_none() {
                    j.label_g = Some(layout(j.label, label_font.clone(), SUBTEXT, f32::INFINITY));
                }
            }
        }
        if self.wordmark_g.is_some() { return; }
        self.wordmark_g = Some(layout("kabelsalat", theme::bold_font(14.5), TEXT, f32::INFINITY));
        self.state_play_g = Some(layout("metronome · playing", FontId::proportional(10.5), GREEN, f32::INFINITY));
        self.state_pause_g = Some(layout("metronome · stopped", FontId::proportional(10.5), SUBTEXT, f32::INFINITY));
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
        self.help_title_g = Some(layout("kabelsalat — the cables are the show", theme::bold_font(17.0), TEXT, f32::INFINITY));
        self.help_body_g = Some(layout(
            "starter templates: Sessions window, or F5 Lead+Reverb / F6 Drums+Bass\n\
             F7 Ambient Pad / F8 demo patch — each loads a playable rack\n\
             F9 / Plugins       browse installed LV2/CLAP and add to a rig\n\
             Space / F10        metronome Play·Stop · Transport window sets BPM\n\
             Tab                flip the rack · front = perform, back = patch\n\
             double-click synth open its embedded editor inline · × closes\n\
             drag knobs         shape the sound · right-click resets\n\
             click the keys     play Keys — pulses run down the cables\n\
             laptop keys        A W S E D F T G Y H U J K · Z/X octave\n\
             drag jack to jack  patch a cable (back) · master jacks fan in\n\
             Ctrl+drag a unit   rearrange the rack — it reflows on release\n\
             grab a cable       shake it — watch it swing and settle (back)\n\
             tug a plug         pull it out to unplug (back)\n\
             click a cable      highlight its whole signal path\n\
             R reset knobs · D stats · ? help · esc cancel",
            FontId::monospace(12.5), TEXT, 560.0));
        self.help_foot_g = Some(layout("esc or ? to close", FontId::proportional(11.5), SUBTEXT, f32::INFINITY));
    }

    // -- layout -----------------------------------------------------------

    fn layout(&mut self, rect: Rect) {
        self.toolbar_top = rect.top();
        // Adaptive rack: KEYS (left, full height) | one column per instrument
        // stack (instrument + its FX chain) | MASTER (right, full height).
        let rows = self.layout_rows.max(1);
        let n_mid = self.stack_cols.len();
        self.rack_w = (rect.width() - 60.0).clamp(560.0, 1240.0);
        self.unit_h = ((rect.height() - 210.0) / rows as f32).clamp(84.0, 190.0);
        let gap = 10.0;
        let pad = 16.0;
        let rail = 14.0;
        self.rack_h = pad * 2.0 + rows as f32 * self.unit_h + (rows as f32 - 1.0) * gap;
        let inner_w = self.rack_w - 2.0 * (pad + rail);
        let n_cols = n_mid + 2; // keys + stacks + master
        let usable = inner_w - (n_cols as f32 - 1.0) * gap;
        let side_frac = if n_mid == 0 { 0.5 } else if n_mid <= 2 { 0.26 } else { 0.20 };
        let mid_frac = if n_mid == 0 { 0.0 } else { (1.0 - 2.0 * side_frac) / n_mid as f32 };
        let mut widths: Vec<f32> = Vec::with_capacity(n_cols);
        widths.push(usable * side_frac); // keys
        for _ in 0..n_mid { widths.push(usable * mid_frac); }
        widths.push(usable * side_frac); // master
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
        for j in 0..n_mid {
            let cx = xs[j + 1];
            let cw = widths[j + 1];
            for r in 0..self.stack_cols[j].len() {
                let u = self.stack_cols[j][r];
                let y = pad + r as f32 * (self.unit_h + gap);
                self.unit_rects[u] = mk(cx, y, cw, self.unit_h);
            }
        }
        let origin = Pos2::new(
            rect.center().x - self.rack_w / 2.0,
            rect.center().y - self.rack_h / 2.0 + 16.0,
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

    fn add_cable(&mut self, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind, pattern: u8) {
        let id = self.next_id;
        self.next_id += 1;
        self.add_cable_id(id, src, src_port, dst, dst_port, kind);
        let _ = pattern; // patterns retired with the demo band
    }

    /// Add a cable with an explicit id (demo patch mirrors engine edge ids).
    /// Inputs fan in: several cables may share one input jack (master bus).
    fn add_cable_id(&mut self, id: u32, src: usize, src_port: usize, dst: usize, dst_port: usize, kind: PortKind) {
        if self.cables.len() >= MAX_CABLES || src == dst { return; }
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
        let mut c = Cable::new(id, src, src_port, dst, dst_port, kind, color, 0, id * 7919 + 13);
        c.next_event = self.time + 0.2 + rng01(&mut c.rng) as f64 * 0.4;
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
            let drift = if rng01(&mut c.rng) > 0.5 { 120.0 } else { -120.0 };
            if let Some(r) = &mut c.rope {
                r.kick_all(Vec2::new(drift, -160.0), |t| if detach_dst { t } else { 1.0 - t });
            }
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
        stream: cpal::Stream,
        bus: Arc<Mutex<CommandBus>>,
        level: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        self._surge_main = mains;
        self.surge_controllers = controllers;
        self._audio_stream = Some(stream);
        self.bus = Some(bus);
        self.out_level = Some(level);
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
        self.current_source = SourceId::LaptopKeys;
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

    fn sessions_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = std::path::PathBuf::from(home).join("kabelsalat-sessions");
        let _ = std::fs::create_dir_all(&dir);
        dir
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
        let surge_id = "org.surge-synth-team.surge-xt";
        let mut rig = 0;
        for u in &self.units {
            if matches!(u.node_kind, UnitKind::Clap) {
                if let Some(m) = self._surge_main.get_mut(rig) {
                    if let Ok(blob) = m.save_state() {
                        s.set_plugin_state(u.node_id, surge_id, blob);
                    }
                }
                rig += 1;
            }
        }
        Ok(s)
    }

    /// Load a Session: rebuild the rack from its graph + params, restore plugin
    /// state, and rewire the engine. Works for any topology (templates, saved
    /// sessions, the demo patch).
    fn apply_session(&mut self, s: &crate::session::Session) -> anyhow::Result<()> {
        self.build_rack(&s.graph, s.params());
        // Restore plugin state into the remapped voices (rig order).
        let clap_nodes: Vec<u32> = self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .map(|u| u.node_id).collect();
        for (rig, node) in clap_nodes.iter().enumerate() {
            if let Some(st) = s.plugin_state(*node) {
                if let Some(m) = self._surge_main.get_mut(rig) {
                    let _ = m.load_state(&st.data);
                }
            }
        }
        // Rewire the engine, then publish every knob value.
        self.load_graph_into_engine(&s.graph);
        // The editor referenced the old voice mapping; close it.
        self.editor_open = false;
        self.editor_params.clear();
        self.editor_status = "editor closed (rack reloaded)".into();
        // Reset per-rig filters / split for the new graph.
        self.rig_sources = [(true, true, true); 2];
        self.rig_device = [None, None];
        self.split_on = false;
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap();
            let _ = b.send(Command::SetSplit { point: None });
            for u in &self.units {
                for k in &u.knobs {
                    if let Some(p) = k.param {
                        let _ = b.send(Command::SetParam { node: u.node_id, param: p, value: k.value as f64 });
                    }
                }
            }
        }
        Ok(())
    }

    /// Send the engine a full graph replacement (voices remap, DSP rebuild).
    fn load_graph_into_engine(&mut self, graph: &Graph) {
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::LoadGraph { graph: graph.clone() });
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
                "buffer_frames": crate::bridge::BUFFER_FRAMES,
                "buffer_latency_ms": crate::bridge::BUFFER_FRAMES as f64 / 48000.0 * 1000.0,
                "xruns": crate::bridge::XRUNS.load(std::sync::atomic::Ordering::Relaxed),
                "midi_latency_ms": crate::bridge::MIDI_LATENCY_US.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
            })
            .to_string(),
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
            "set_rig_source" => {
                let node = args.get("node").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let keys = args.get("keys").and_then(|v| v.as_bool()).unwrap_or(true);
                let laptop = args.get("laptop").and_then(|v| v.as_bool()).unwrap_or(true);
                let hardware = args.get("hardware").and_then(|v| v.as_bool()).unwrap_or(true);
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetRigSource { node, keys, laptop, hardware });
                }
                format!("rig {node} sources: keys={keys} laptop={laptop} hardware={hardware}")
            }
            "set_split" => {
                let point = args.get("point").and_then(|v| v.as_u64()).map(|p| p.min(127) as u8);
                self.split_on = point.is_some();
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::SetSplit { point });
                }
                match point {
                    Some(p) => format!("keyboard split at note {p}: rig A below, rig B at/above"),
                    None => "split disabled — both rigs play the full keyboard".into(),
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
                let dir = Self::sessions_dir();
                let mut names: Vec<String> = std::fs::read_dir(&dir)
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
            "play_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let gain = args.get("gain").and_then(|v| v.as_f64()).unwrap_or(0.8).clamp(0.0, 1.0) as f32;
                if path.is_empty() {
                    return "play_file needs a path to a 16-bit WAV file".into();
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
                match device {
                    Some(d) => format!("rig {node} now listens only to hardware device {d}"),
                    None => format!("rig {node} listens to any hardware device"),
                }
            }
            "list_plugins" => {
                let clap = crate::host::list_clap_plugins();
                let lv2 = crate::lv2::discover();
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
            "transport_play" => {
                self.playing = true;
                self.time = 0.0;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::TransportPlay);
                }
                "metronome started".into()
            }
            "transport_stop" => {
                self.playing = false;
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap().send(Command::TransportStop);
                }
                "metronome stopped".into()
            }
            "set_bpm" => {
                let bpm = args.get("bpm").and_then(|v| v.as_f64()).unwrap_or(96.0).clamp(20.0, 300.0) as f32;
                self.set_bpm(bpm);
                format!("tempo set to {bpm:.0} BPM")
            }
            "load_template" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("demo").to_lowercase();
                match name.as_str() {
                    "lead_reverb" | "lead" => { self.load_template(Template::LeadReverb); "Loaded template 'Lead + Reverb'".into() }
                    "drums_bass" | "drums" => { self.load_template(Template::DrumsBass); "Loaded template 'Drums + Bass'".into() }
                    "ambient_pad" | "ambient" | "pad" => { self.load_template(Template::AmbientPad); "Loaded template 'Ambient Pad'".into() }
                    "demo" => { self.load_demo_patch(); "Loaded the demo patch".into() }
                    _ => format!("unknown template '{name}' (use lead_reverb, drums_bass, ambient_pad, demo)"),
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
                    "system_audio" => Some(NodeKind::SystemAudio),
                    "master_out" => Some(NodeKind::MasterOut),
                    _ => None,
                };
                match kind {
                    Some(k) => {
                        if let Some(bus) = &self.bus { let _ = bus.lock().unwrap().send(Command::AddNode { kind: k, name }); }
                        format!("added {kind_str} node (wire it with connect)")
                    }
                    None => format!("invalid node kind/dsp/uri for '{kind_str}'"),
                }
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
                }
                format!("connected {from_node} -> {to_node} ({})", if midi { "midi" } else { "audio" })
            }
            "disconnect" => {
                let id = args.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Some(bus) = &self.bus { let _ = bus.lock().unwrap().send(Command::Disconnect(id)); }
                format!("disconnected cable {id}")
            }
            _ => format!("unknown tool: {name}"),
        }
    }

    fn send_rig_source(&self, node: u32) {
        let rig = self.rig_of_node(node);
        let (keys, laptop, hardware) = self.rig_sources[rig];
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetRigSource { node, keys, laptop, hardware });
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
        if self.editor_open && self.editor_rig == rig {
            self.editor_open = false;
            return;
        }
        self.editor_rig = rig;
        self.editor_filter.clear();
        self.editor_params = self._surge_main.get_mut(rig)
            .map(|m| m.param_list())
            .unwrap_or_default();
        self.editor_open = !self.editor_params.is_empty();
        self.editor_status = if self.editor_open {
            String::new()
        } else {
            "no synth loaded — editor unavailable".into()
        };
    }

    /// Re-read the open rig's parameter list (on open / Refresh button).
    fn refresh_editor_params(&mut self) {
        let rig = self.editor_rig;
        if let Some(main) = self._surge_main.get_mut(rig) {
            self.editor_params = main.param_list();
        }
    }

    /// The native parameter editor window: search box + module-grouped
    /// sliders. Changes are collected and applied after rendering so the
    /// cached list and the RT-safe controller stay in sync.
    fn param_editor_window(&mut self, ctx: &egui::Context) {
        if !self.editor_open {
            return;
        }
        let rig = self.editor_rig;
        let name = self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .nth(rig)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| if rig == 0 { "SURGE A".to_string() } else { "SURGE B".to_string() });
        let mut open = self.editor_open;
        let mut refresh = false;
        let mut changes: Vec<(u32, f64)> = Vec::new();
        egui::Window::new(format!("{name} — parameters"))
            .open(&mut open)
            .collapsible(false)
            .fixed_size([480.0, 560.0])
            .show(ctx, |ui| {
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
                // Split borrows: cached params (read) vs voice mains (param_text).
                let params = &self.editor_params;
                let mains = &mut self._surge_main;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                                        let resp = ui.add_sized(
                                            [(w - 96.0).max(120.0), 18.0],
                                            egui::Slider::new(&mut value, p.min..=p.max).text(&p.name),
                                        );
                                        if resp.changed() {
                                            changes.push((p.id, value));
                                        }
                                        let text = mains.get_mut(rig)
                                            .map(|m| m.param_text(p.id, value))
                                            .unwrap_or_default();
                                        ui.label(egui::RichText::new(text).small().color(SUBTEXT));
                                    });
                                }
                            });
                    }
                    if shown == 0 {
                        ui.label(egui::RichText::new("no matching parameters").italics().color(SUBTEXT));
                    }
                });
            });
        self.editor_open = open;
        // Apply slider changes: update the cache + schedule RT-safe param events.
        for (id, value) in changes {
            if let Some(p) = self.editor_params.iter_mut().find(|p| p.id == id) {
                p.value = value;
            }
            if let Some(ctrl) = self.surge_controllers.get(rig) {
                ctrl.set_param(ClapId::new(id), value);
            }
        }
        if refresh {
            self.refresh_editor_params();
        }
    }

    /// AUM-style per-rig input selection: which sources feed each rig.
    fn rig_inputs_panel(&mut self, root: &mut egui::Ui) {
        let rigs = self.clap_rigs();
        egui::Panel::top("rig_inputs")
            .default_size(36.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_RAISED)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::same(6)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::section_label("Rig inputs"));
                    ui.add_space(10.0);
                    for (rig, node) in rigs {
                        let accent = if rig == 0 { BLUE } else { theme::AUDIO_BLUE };
                        let label = if rig == 0 { "A" } else { "B" };
                        ui.label(egui::RichText::new(format!("RIG {label}")).small().strong().color(accent));
                        let (k, l, h) = self.rig_sources[rig];
                        for (name, on, idx) in [("Keys", k, 0usize), ("Laptop", l, 1), ("HW", h, 2)] {
                            let fill = if on { alpha(accent, 0.30) } else { theme::SURFACE_HI };
                            let txt = if on { TEXT } else { SUBTEXT };
                            let mut btn = egui::Button::new(egui::RichText::new(name).small().color(txt))
                                .fill(fill)
                                .corner_radius(8.0);
                            if on { btn = btn.stroke(Stroke::new(1.0, alpha(accent, 0.6))); }
                            if ui.add(btn).clicked() {
                                let s = &mut self.rig_sources[rig];
                                match idx {
                                    0 => s.0 = !s.0,
                                    1 => s.1 = !s.1,
                                    _ => s.2 = !s.2,
                                }
                                self.send_rig_source(node);
                            }
                        }
                        // Per-rig hardware device selector (AUM: a keyboard per rig).
                        if !self.hw_devices.is_empty() {
                            let cur = self.rig_device[rig];
                            let label = match cur {
                                None => "HW: All".to_string(),
                                Some(d) => format!("HW: {}", self.hw_devices.get(d as usize).map(|s| s.as_str()).unwrap_or("?")),
                            };
                            let dbtn = egui::Button::new(egui::RichText::new(label).small().color(SUBTEXT))
                                .fill(theme::SURFACE_HI)
                                .corner_radius(8.0);
                            if ui.add(dbtn).on_hover_text("cycle which hardware device feeds this rig").clicked() {
                                let n = self.hw_devices.len() as u32;
                                let next = match cur {
                                    None => Some(0),
                                    Some(d) => if d + 1 >= n { None } else { Some(d + 1) },
                                };
                                self.rig_device[rig] = next;
                                if let Some(bus) = &self.bus {
                                    let _ = bus.lock().unwrap().send(Command::SetRigDevice { node, device: next });
                                }
                            }
                        }
                        // Editor is opened inline by double-clicking the synth
                        // unit in the rack (task-26) — no per-rig button here.
                    }
                    ui.add_space(4.0);
                    let split_txt = if self.split_on { "Split @ C4: ON" } else { "Split @ C4: off" };
                    let split_fill = if self.split_on { alpha(YELLOW, 0.30) } else { theme::SURFACE_HI };
                    let split_col = if self.split_on { TEXT } else { SUBTEXT };
                    let mut btn = egui::Button::new(egui::RichText::new(split_txt).small().color(split_col))
                        .fill(split_fill)
                        .corner_radius(8.0);
                    if self.split_on { btn = btn.stroke(Stroke::new(1.0, alpha(YELLOW, 0.6))); }
                    if ui.add(btn).clicked() {
                        self.split_on = !self.split_on;
                        let point = if self.split_on { Some(60) } else { None };
                        if let Some(bus) = &self.bus {
                            let _ = bus.lock().unwrap().send(Command::SetSplit { point });
                        }
                    }
                    ui.add_space(10.0);
                    if !self.editor_open && !self.editor_status.is_empty() {
                        ui.label(egui::RichText::new(&self.editor_status).small().italics().color(ORANGE));
                        ui.add_space(8.0);
                    }
                    let sess_btn = egui::Button::new(egui::RichText::new("Sessions").small().color(TEXT))
                        .fill(theme::SURFACE_HI)
                        .corner_radius(8.0);
                    if ui.add(sess_btn).clicked() {
                        self.show_sessions = !self.show_sessions;
                    }
                    ui.add_space(6.0);
                    let plug_btn = egui::Button::new(egui::RichText::new("Plugins").small().color(TEXT))
                        .fill(theme::SURFACE_HI)
                        .corner_radius(8.0);
                    if ui.add(plug_btn).on_hover_text("browse installed LV2 / CLAP plugins and add them to a rig").clicked() {
                        self.show_plugins = !self.show_plugins;
                        if self.show_plugins && !self.plugins_loaded {
                            self.refresh_plugins();
                        }
                    }
                    ui.add_space(6.0);
                    let tr_fill = if self.playing { alpha(GREEN, 0.30) } else { theme::SURFACE_HI };
                    let mut tr_btn = egui::Button::new(egui::RichText::new("Transport").small().color(TEXT))
                        .fill(tr_fill)
                        .corner_radius(8.0);
                    if self.playing { tr_btn = tr_btn.stroke(Stroke::new(1.0, alpha(GREEN, 0.6))); }
                    if ui.add(tr_btn).on_hover_text("metronome: Play/Stop + BPM (F10)").clicked() {
                        self.show_transport = !self.show_transport;
                    }
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new("which sources feed each rig").small().italics().color(SUBTEXT));
                });
            });
    }

    /// Compact transport / metronome popover (task-19). A small window (like
    /// Sessions/Plugins) so it never collides with the custom rack drawing:
    /// Play/Stop toggle + BPM stepper. The toolbar Play button and Space drive
    /// the same transport.
    fn transport_window(&mut self, ctx: &egui::Context) {
        if !self.show_transport {
            return;
        }
        let mut open = self.show_transport;
        egui::Window::new("Transport")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Play/Stop toggle — reflects transport state, drives the engine.
                    let (label, accent) = if self.playing { ("Stop", theme::CLIP_RED) } else { ("Play", GREEN) };
                    let btn = egui::Button::new(egui::RichText::new(label).small().strong().color(accent))
                        .fill(alpha(accent, 0.18))
                        .stroke(Stroke::new(1.0, alpha(accent, 0.55)))
                        .corner_radius(8.0)
                        .min_size(egui::vec2(56.0, 20.0));
                    if ui.add(btn).on_hover_text("start / stop the metronome (Space)").clicked() {
                        self.toggle_play();
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("BPM").small().color(SUBTEXT));
                    if ui.small_button("−").on_hover_text("slower").clicked() {
                        self.set_bpm(self.bpm - 1.0);
                    }
                    let mut bpm = self.bpm;
                    let dv = egui::DragValue::new(&mut bpm)
                        .range(20.0..=300.0)
                        .speed(1.0)
                        .fixed_decimals(0);
                    if ui.add(dv).on_hover_text("metronome tempo (20–300)").changed() {
                        self.set_bpm(bpm);
                    }
                    if ui.small_button("+").on_hover_text("faster").clicked() {
                        self.set_bpm(self.bpm + 1.0);
                    }
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    // Live beat indicator: thumps at the current tempo while playing.
                    let beat = 60.0 / self.bpm.max(1.0) as f64;
                    let phase = ((self.time / beat).fract() as f32).clamp(0.0, 1.0);
                    let thump = if self.playing { (1.0 - phase).powi(3) } else { 0.0 };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        rect.center(),
                        3.0 + 3.0 * thump,
                        alpha(GREEN, 0.30 + 0.70 * thump),
                    );
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(if self.playing { "metronome on" } else { "metronome off" })
                        .small().italics().color(SUBTEXT));
                });
            });
        self.show_transport = open;
    }

    /// Visual save/load for sessions (no assistant needed).
    fn sessions_window(&mut self, ctx: &egui::Context) {
        if !self.show_sessions {
            return;
        }
        let mut open = self.show_sessions;
        egui::Window::new("Sessions")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(theme::section_label("Starter templates"));
                ui.horizontal(|ui| {
                    for t in Template::ALL {
                        if ui.button(t.title()).on_hover_text("load a playable pre-wired rack").clicked() {
                            self.load_template(t);
                        }
                    }
                });
                if ui.button("Demo patch (2 stacks)").clicked() {
                    self.load_demo_patch();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.session_name);
                });
                if ui.button("Save current patch").clicked() {
                    let name = if self.session_name.trim().is_empty() {
                        "Untitled".to_string()
                    } else {
                        self.session_name.clone()
                    };
                    match self.capture_session(&name) {
                        Ok(s) => {
                            let path = crate::session::Session::package_path(&Self::sessions_dir(), &name);
                            match path.and_then(|p| s.save(&p).map(|_| p)) {
                                Ok(_) => self.session_msg = format!("Saved '{name}'"),
                                Err(e) => self.session_msg = format!("Save failed: {e}"),
                            }
                        }
                        Err(e) => self.session_msg = format!("Capture failed: {e}"),
                    }
                }
                ui.separator();
                ui.label(theme::section_label("Saved sessions"));
                let dir = Self::sessions_dir();
                let mut names: Vec<String> = std::fs::read_dir(&dir)
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
                if names.is_empty() {
                    ui.label(egui::RichText::new("none yet — save one above").small().italics());
                }
                for name in names {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&name).small());
                        if ui.small_button("Load").clicked() {
                            let path = crate::session::Session::package_path(&dir, &name);
                            match path.and_then(|p| crate::session::Session::load(&p)) {
                                Ok(s) => match self.apply_session(&s) {
                                    Ok(_) => self.session_msg = format!("Loaded '{name}'"),
                                    Err(e) => self.session_msg = format!("Apply failed: {e}"),
                                },
                                Err(e) => self.session_msg = format!("Load failed: {e}"),
                            }
                        }
                    });
                }
                if !self.session_msg.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new(&self.session_msg).small().color(GREEN));
                }
            });
        self.show_sessions = open;
    }

    // -- plugin browser (task-18) -------------------------------------------

    /// Scan installed CLAP + LV2 plugins. Filesystem / lilv work — done only on
    /// open / refresh, never on the per-frame path.
    fn refresh_plugins(&mut self) {
        self.plugins_clap = list_clap_plugins();
        self.plugins_lv2 = lv2_discover();
        self.plugins_loaded = true;
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

    /// Start a fresh single-Surge rig (the only CLAP instrument the engine loads).
    fn add_surge_rig(&mut self) {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_ID) }, name_bytes("SURGE"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
        g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, CPortKind::Midi);
        g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
        g.connect_with_id(3, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 1 }, CPortKind::Audio);
        let s = crate::session::Session::new("Surge rig", g);
        match self.apply_session(&s) {
            Ok(_) => self.plugin_msg = "New Surge rig — Keys→Surge→Master".into(),
            Err(e) => self.plugin_msg = format!("Add failed: {e}"),
        }
    }

    /// Browse installed plugins and add them to a rig (no assistant needed).
    fn plugins_window(&mut self, ctx: &egui::Context) {
        if !self.show_plugins { return; }
        let mut open = self.show_plugins;
        egui::Window::new("Plugins")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.text_edit_singleline(&mut self.plugin_filter);
                    if ui.button("Refresh").clicked() { self.refresh_plugins(); }
                });
                ui.label(egui::RichText::new("adding a plugin rebuilds the current patch").small().italics().color(SUBTEXT));
                let filter = self.plugin_filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    ui.label(theme::section_label(&format!("LV2 plugins ({})", self.plugins_lv2.len())));
                    let mut added: Option<(String, String, bool)> = None;
                    for p in &self.plugins_lv2 {
                        if !filter.is_empty() && !p.name.to_lowercase().contains(&filter) { continue; }
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&p.name).small());
                            if p.is_instrument {
                                ui.label(egui::RichText::new("inst").small().italics().color(GREEN));
                            }
                            // Offer BOTH roles; emphasize the likely one (the
                            // is_instrument hint is permissive, not authoritative).
                            let base = theme::SURFACE_HI;
                            let inst_fill = if p.is_instrument { alpha(BLUE, 0.35) } else { base };
                            let fx_fill = if p.is_instrument { base } else { alpha(BLUE, 0.35) };
                            let ib = egui::Button::new(egui::RichText::new("Add instrument").small().color(TEXT))
                                .fill(inst_fill).corner_radius(8.0);
                            if ui.add(ib).on_hover_text(&p.uri).clicked() {
                                added = Some((p.uri.clone(), p.name.clone(), true));
                            }
                            let fb = egui::Button::new(egui::RichText::new("Add effect").small().color(TEXT))
                                .fill(fx_fill).corner_radius(8.0);
                            if ui.add(fb).on_hover_text(&p.uri).clicked() {
                                added = Some((p.uri.clone(), p.name.clone(), false));
                            }
                        });
                    }
                    if let Some((uri, name, as_inst)) = added {
                        if as_inst { self.add_lv2_instrument(&uri, &name); } else { self.add_lv2_effect(&uri, &name); }
                    }
                    ui.separator();
                    ui.label(theme::section_label(&format!("CLAP instruments ({})", self.plugins_clap.len())));
                    let mut add_surge = false;
                    for p in &self.plugins_clap {
                        if !filter.is_empty() && !p.name.to_lowercase().contains(&filter) { continue; }
                        let is_surge = p.id == SURGE_ID;
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&p.name).small());
                            if is_surge {
                                if ui.small_button("New rig").clicked() { add_surge = true; }
                            } else {
                                ui.add_enabled(false, egui::Button::new(egui::RichText::new("coming soon").small()))
                                    .on_hover_text("only Surge loads for now — other CLAP instruments are not wired yet");
                            }
                        });
                    }
                    if add_surge { self.add_surge_rig(); }
                });
                if !self.plugin_msg.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new(&self.plugin_msg).small().color(GREEN));
                }
            });
        self.show_plugins = open;
    }

    fn assistant_panel_ui(&mut self, root: &mut egui::Ui) {
        egui::Panel::right("assistant")
            .default_size(340.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_RAISED)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
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
                                    _ => ("Error", theme::CLIP_RED),
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
    fn clap_rigs(&self) -> Vec<(usize, u32)> {
        self.units.iter()
            .filter(|u| matches!(u.node_kind, UnitKind::Clap))
            .take(2)
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

    fn fire_keys(&mut self, semitone: usize, amp: f32) {
        // VEL knob scales keyboard velocity (note-offs pass through at 0).
        let ki = self.keys_idx();
        let vel = self.units.get(ki).and_then(|u| u.knobs.get(1)).map(|k| k.value).unwrap_or(0.8);
        let amp = amp * (0.35 + vel * 0.65);
        self.key_leds[semitone % 12] = 1.0;
        if let Some(u) = self.units.get_mut(ki) { u.led = 1.0; }
        for ci in 0..self.cables.len() {
            let c = &mut self.cables[ci];
            if c.src == ki && c.kind == PortKind::Midi && c.dying < 0.0 {
                c.spawn_pulse(0.0, amp);
            }
        }
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
        let side = p_side(unit, self);
        self.pending = Some(Pending {
            unit, jack, dir, kind, cursor,
            rope: if dir == Dir::Out { Rope::new(origin, cursor, side) } else { Rope::new(cursor, origin, side) },
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
        eprintln!(
            "rack flip: {} -> {} (embedded editor stays in its fixed region)",
            if self.side == Side::Front { "front" } else { "back" },
            if self.flip_to == Side::Front { "front" } else { "back" },
        );
    }

    fn toggle_play(&mut self) {
        self.playing = !self.playing;
        // Drive the engine metronome (task-19): the transport toggle is the
        // single source of truth for Play/Stop.
        if let Some(bus) = &self.bus {
            let cmd = if self.playing { Command::TransportPlay } else { Command::TransportStop };
            let _ = bus.lock().unwrap().send(cmd);
        }
        if self.playing {
            self.time = 0.0; // restart the visual beat phase on Play
            for c in &mut self.cables {
                c.next_event = self.time + 0.08;
                c.pat_idx = 0;
            }
        }
    }

    /// Set the metronome tempo, clamped to the engine's 20..=300 range, and
    /// notify the engine. Used by the transport stepper and the assistant tool.
    fn set_bpm(&mut self, bpm: f32) {
        self.bpm = bpm.clamp(20.0, 300.0);
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetBpm { bpm: self.bpm });
        }
    }

    // -- simulation -----------------------------------------------------------

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
                        let jy = 60.0 + rng01(&mut c.rng) * 60.0;
                        if let Some(r) = &mut c.rope {
                            r.kick_all(Vec2::new(jx, jy), |t| (t * std::f32::consts::PI).sin());
                        }
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

        self.rope_acc = (self.rope_acc + dt).min(8.0 * rope::STEP);
        let rope_steps = (self.rope_acc / rope::STEP) as usize;
        self.rope_acc -= rope_steps as f32 * rope::STEP;
        let bounds = rope::Bounds { min_x: 4.0, max_x: self.rack_w - 4.0, floor: self.rack_h - 6.0 };

        let mut n_arrivals = 0usize;
        for ci in 0..self.cables.len() {
            let ea = self.endpoint(&self.cables[ci], Dir::Out);
            let eb = self.endpoint(&self.cables[ci], Dir::In);
            let c = &mut self.cables[ci];
            c.age += dt;
            if c.dying >= 0.0 { c.dying += dt; }

            // Pinned ends: the jack, or the cursor while a plug is dragged.
            // An unplugged cable lets its detached end fall.
            let a = (c.dying < 0.0 || c.detach_dst).then(|| c.over_a.unwrap_or(ea));
            let b = (c.dying < 0.0 || !c.detach_dst).then(|| c.over_b.unwrap_or(eb));
            // Unit rects are Rect::NOTHING until the first layout, so the jacks
            // can still be NaN here.
            if c.rope.is_none() && ea.is_finite() && eb.is_finite() {
                let mid_x = (ea.x + eb.x) * 0.5;
                let side = if (mid_x - self.rack_w / 2.0).abs() < 26.0 {
                    if c.id % 2 == 0 { 1.0 } else { -1.0 }
                } else if mid_x < self.rack_w / 2.0 { -1.0 } else { 1.0 };
                c.rope = Some(Rope::new(ea, eb, side));
            }
            let breathe = if c.kind == PortKind::Audio { 1.0 + 0.04 * c.level } else { 1.0 };
            if let Some(rope) = &mut c.rope {
                for _ in 0..rope_steps { rope.step(a, b, breathe, bounds); }
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
                c.env *= (-dt / 0.32).exp();
                c.level += (c.env - c.level) * (1.0 - (-dt / 0.05).exp());
            }
        }

        for &(unit, amp) in &self.arrivals[..n_arrivals] {
            let gain = self.units.get(unit).map(unit_out_gain).unwrap_or(1.0);
            if let Some(u) = self.units.get_mut(unit) { u.led = u.led.max(amp); }
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
            let p = self.pending.as_mut().unwrap();
            let (a, b) = if p.dir == Dir::Out { (origin, p.cursor) } else { (p.cursor, origin) };
            for _ in 0..rope_steps { p.rope.step(Some(a), Some(b), 1.0, bounds); }
        }
    }

    /// Rebuild local-space samples + arc-length LUT from each rope.
    fn tessellate(&mut self) {
        for c in &mut self.cables {
            let Some(r) = &c.rope else { continue };
            let n = r.sample(&mut c.pts);
            c.n = n;
            let mut acc = 0.0;
            c.lut[0] = 0.0;
            for i in 1..n {
                acc += (c.pts[i] - c.pts[i - 1]).length();
                c.lut[i] = acc;
            }
            c.total_len = acc.max(1e-3);
        }
    }

    // -- hit testing ----------------------------------------------------------

    fn knob_center(&self, unit: usize, idx: usize) -> Vec2 {
        let r = self.unit_rects[unit];
        let tall = self.units.get(unit)
            .map(|u| matches!(u.node_kind, UnitKind::Keys | UnitKind::Master))
            .unwrap_or(false);
        if tall {
            // Tall shared units: knobs along the bottom.
            let fx = if idx == 0 { 0.32 } else { 0.68 };
            Vec2::new(r.left() + r.width() * fx, r.bottom() - 40.0)
        } else {
            // Stack units: knob column on the right.
            Vec2::new(r.right() - 86.0 + idx as f32 * 56.0, r.center().y + 14.0)
        }
    }

    /// Display space left of the knob column on stack units.
    fn display_area(&self, unit: usize) -> (f32, f32) {
        let r = self.unit_rects[unit];
        let start = r.left() + 20.0;
        let width = (r.right() - 112.0 - start).max(0.0);
        (start, width)
    }

    fn key_w(&self) -> f32 {
        let Some(k) = self.keys_unit else { return 0.0 };
        let r = self.unit_rects[k];
        let avail = r.width() - 40.0;
        if avail < 96.0 { return 0.0; } // too narrow: hide the keyboard
        (avail / 8.0).clamp(12.0, 30.0)
    }

    fn key_rect(&self, white_idx: usize) -> Rect {
        let r = self.unit_rects[self.keys_idx()];
        let kw = self.key_w();
        let x0 = r.center().x - kw * 4.0;
        Rect::from_min_size(
            Pos2::new(x0 + white_idx as f32 * kw, r.center().y - 30.0),
            Vec2::new(kw - 1.0, 60.0),
        )
    }

    fn hit_test(&self, s: Pos2) -> Hit {
        if (s - Pos2::new(40.0, self.toolbar_top + 40.0)).length() < 19.0 { return Hit::Play; }
        if Rect::from_min_size(Pos2::new(246.0, self.toolbar_top + 24.0), Vec2::new(126.0, 32.0)).contains(s) {
            return Hit::Flip;
        }
        if self.flipping || self.xf.sy < 0.9 { return Hit::Background; }
        let l = self.xf.inv(s);
        let lp = l.to_pos2();

        match self.side {
            Side::Front => {
                for (ui, u) in self.units.iter().enumerate() {
                    for ki in 0..u.knobs.len() {
                        if (l - self.knob_center(ui, ki)).length() < 23.0 {
                            return Hit::Knob { unit: ui, idx: ki };
                        }
                    }
                }
                // Keys keyboard.
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
        let mut load_tpl: Option<Template> = None;
        let mut load_demo = false;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Space) { toggle_play = true; }
            if i.consume_key(Modifiers::NONE, Key::Tab) { flip = true; }
            if i.consume_key(Modifiers::NONE, Key::R) { reset_knobs = true; }
            if i.consume_key(Modifiers::NONE, Key::D) { self.show_stats = !self.show_stats; }
            // Starter templates: F5/F6/F7, demo patch on F8 (task-17).
            if i.consume_key(Modifiers::NONE, Key::F5) { load_tpl = Some(Template::LeadReverb); }
            if i.consume_key(Modifiers::NONE, Key::F6) { load_tpl = Some(Template::DrumsBass); }
            if i.consume_key(Modifiers::NONE, Key::F7) { load_tpl = Some(Template::AmbientPad); }
            if i.consume_key(Modifiers::NONE, Key::F8) { load_demo = true; }
            // Plugin browser (task-18).
            if i.consume_key(Modifiers::NONE, Key::F9) {
                self.show_plugins = !self.show_plugins;
                if self.show_plugins && !self.plugins_loaded { self.refresh_plugins(); }
            }
            // Transport / metronome popover (task-19).
            if i.consume_key(Modifiers::NONE, Key::F10) {
                self.show_transport = !self.show_transport;
            }
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
        if let Some(t) = load_tpl { self.load_template(t); }
        if load_demo { self.load_demo_patch(); }
        if reset_knobs {
            for u in &mut self.units {
                for k in &mut u.knobs { k.value = k.default; }
            }
        }

        let (pointer, pressed, down, released, right_clicked, ctrl) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.primary_pressed(), i.pointer.primary_down(),
             i.pointer.primary_released(), i.pointer.secondary_clicked(), i.modifiers.ctrl)
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
                    Hit::Key(semi) => {
                        self.current_source = SourceId::Keys;
                        self.fire_keys(60 + semi, 0.95);
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
                    if let Some(r) = self.cables.get_mut(ci).filter(|c| c.dying < 0.0).and_then(|c| c.rope.as_mut()) {
                        r.pull(u, l, 0.5);
                        // Tug near an end → unplug.
                        let (a, b) = (r.start(), r.end());
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
                Drag::Unit { unit, grab } => {
                    // Track the cursor in rack-local space; cable endpoints read
                    // unit_rects, so they stay attached while the box moves.
                    let size = self.unit_rects[unit].size();
                    self.unit_rects[unit] = Rect::from_min_size((l - grab).to_pos2(), size);
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
            let unit_drag = if let Drag::Unit { unit, .. } = self.drag { Some(unit) } else { None };
            self.drag = Drag::None;

            // Ctrl+drag release (task-21): reorder the stacks, then auto-layout
            // reflows. Only when actually moved, so a Ctrl+click is a no-op.
            if let Some(unit) = unit_drag {
                if moved >= 6.0 { self.drop_unit(unit, l); }
            }

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
                    Some(Hit::Unit(ui)) => {
                        // Double-click a synth unit → toggle its inline editor
                        // (task-26). A single click still pings the unit.
                        let is_synth = self.units.get(ui).map_or(false, |u| matches!(u.node_kind, UnitKind::Clap));
                        let now = std::time::Instant::now();
                        let double = is_synth && self.last_unit_click
                            .map_or(false, |(pu, pt)| pu == ui && now.duration_since(pt).as_millis() < 400);
                        if double {
                            self.last_unit_click = None;
                            match self.rig_of_unit(ui) {
                                Some(rig) => self.editor_request = Some(rig),
                                None => self.editor_status = "no hosted synth voice for this unit".into(),
                            }
                        } else {
                            if is_synth { self.last_unit_click = Some((ui, now)); }
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
        let icon = if matches!(self.drag, Drag::Unit { .. }) {
            CursorIcon::Grabbing
        } else if ctrl && matches!(self.hover, Some(Hit::Unit(_))) {
            CursorIcon::Grab
        } else {
            match self.hover {
                Some(Hit::Play) | Some(Hit::Flip) | Some(Hit::Jack { .. }) | Some(Hit::Key(_)) => CursorIcon::PointingHand,
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
        let painter = ui.painter();
        theme::aurora_bg(&painter, rect);

        // Rack shadow + glass chassis (through the flip transform). The
        // chassis sits a layer darker than the units so they pop off it.
        let rack_local = Rect::from_min_size(Pos2::ZERO, Vec2::new(self.rack_w, self.rack_h));
        let rack = self.xf.rect(rack_local);
        theme::drop_shadow(&painter, rack, 18.0);
        theme::gradient_rect(&painter, rack, 18.0, theme::SURFACE, CHASSIS);
        painter.rect_stroke(rack, 18.0, Stroke::new(1.2, alpha(BORDER, 0.9)), egui::StrokeKind::Inside);
        theme::top_highlight(&painter, rack, 18.0);

        if self.xf.sy > 0.04 {
            self.draw_rails(&painter);
            match self.side {
                Side::Front => self.draw_front(&painter),
                Side::Back => self.draw_back(&painter),
            }
        }

        self.draw_toolbar(&painter, self.toolbar_top);
        self.draw_hint(&painter, rect);
        if self.show_stats { self.draw_stats(&painter, rect); }
        if self.show_help { self.draw_help(&painter, rect); }
    }

    fn draw_rails(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for side_x in [8.0, self.rack_w - 22.0] {
            let rail = Rect::from_min_size(Pos2::new(side_x, 8.0), Vec2::new(14.0, self.rack_h - 16.0));
            painter.rect_filled(xf.rect(rail), 6.0, RAIL);
        }
        // Screws at the row boundaries.
        for i in 0..=self.layout_rows {
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
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            // Glass plate: gradient brightens on hover; violet edge while
            // dragging (selection color); accent-tinted border otherwise.
            let top = if dragging { mix(theme::SURFACE_HI, PURPLE, 0.14) }
                else if hovered { mix(theme::SURFACE_HI, WHITE, 0.05) }
                else { theme::SURFACE_HI };
            theme::gradient_rect(painter, sr, 10.0, top, PLATE_FRONT);
            let (bw, bc) = if dragging { (1.6, alpha(PURPLE, 0.85)) } else { (1.0, alpha(u.accent, 0.35)) };
            painter.rect_stroke(sr, 10.0, Stroke::new(bw, bc), egui::StrokeKind::Inside);
            theme::top_highlight(painter, sr, 10.0);
            // Active unit: accent edge glow breathing with the signal LED.
            if u.led > 0.05 { theme::glow_rect(painter, sr, 10.0, u.accent, u.led * 0.8); }
            if dragging { theme::glow_rect(painter, sr, 10.0, PURPLE, 0.9); }

            // Colored label strip.
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(7.0, r.height() - 24.0));
            painter.rect_filled(xf.rect(strip), 3.5, u.accent);

            self.text(painter, r.min.to_vec2() + Vec2::new(32.0, 16.0), &u.name_g, TEXT);
            self.text(painter, r.min.to_vec2() + Vec2::new(32.0, 40.0), &u.sub_g, SUBTEXT);

            // Activity LED.
            let led = (r.right_top() + Vec2::new(-16.0, 16.0)).to_vec2();
            if u.led > 0.03 { self.ell(painter, led, 7.5, alpha(u.accent, 0.30 * u.led), Stroke::NONE); }
            self.ell(painter, led, 3.2, alpha(u.accent, 0.25 + 0.75 * u.led), Stroke::NONE);

            match &u.node_kind {
                UnitKind::Keys => self.draw_keyboard(painter),
                UnitKind::Clap => self.draw_surge_display(painter, ui),
                UnitKind::Dsp(DspKind::Delay) => self.draw_delay_display(painter, ui),
                UnitKind::Dsp(_) => self.draw_dsp_display(painter, ui),
                UnitKind::Master => self.draw_meter(painter, r),
            }

            for ki in 0..u.knobs.len() {
                self.draw_knob(painter, ui, ki);
            }
        }
    }

    /// Generic DSP face: a small activity bar that breathes with the unit's LED.
    fn draw_dsp_display(&self, painter: &egui::Painter, unit: usize) {
        let r = self.unit_rects[unit];
        let u = &self.units[unit];
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let level = u.led.clamp(0.0, 1.0);
        let bar_w = width * 0.8;
        let y = r.center().y + 16.0;
        let track = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w, 8.0));
        painter.rect_filled(self.xf.rect(track), 4.0, BG);
        let fill = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w * (0.15 + 0.85 * level), 8.0));
        painter.rect_filled(self.xf.rect(fill), 4.0, alpha(u.accent, 0.5 + 0.5 * level));
    }

    fn draw_keyboard(&self, painter: &egui::Painter) {
        if self.key_w() < 8.0 { return; }
        let xf = &self.xf;
        for wi in 0..8 {
            let kr = self.key_rect(wi);
            let semi = WHITE_SEMI[wi];
            let lit = self.key_leds[semi];
            let hovered = matches!(self.hover, Some(Hit::Key(s)) if s == semi);
            let fill = mix(Color32::from_rgb(0xe6, 0xed, 0xf7), YELLOW, lit * 0.85);
            painter.rect_filled(xf.rect(kr), 3.0, if hovered { mix(fill, WHITE, 0.3) } else { fill });
            painter.rect_stroke(xf.rect(kr), 3.0, Stroke::new(1.0, alpha(BLACK, 0.4)), egui::StrokeKind::Inside);
        }
        for bi in 0..5 {
            let after = BLACK_AFTER[bi];
            let base = self.key_rect(after);
            let bw = self.key_w() * 0.62;
            let kr = Rect::from_min_size(
                Pos2::new(base.right() - bw / 2.0 + 0.5, base.top()),
                Vec2::new(bw, 38.0),
            );
            let lit = self.key_leds[BLACK_SEMI[bi]];
            painter.rect_filled(xf.rect(kr), 2.5, mix(theme::BG_RAISED, YELLOW, lit * 0.7));
        }
    }

    fn draw_surge_display(&self, painter: &egui::Painter, unit: usize) {
        let r = self.unit_rects[unit];
        let vis = 0.25 + 0.75 * self.units[unit].led.max(
            self.cables.iter().filter(|c| c.src == unit && c.kind == PortKind::Audio).map(|c| c.level).fold(0.0, f32::max));
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let pitch = (width / 11.0).min(9.0);
        for i in 0..11 {
            let h = (0.25 + 0.75 * (self.ui_time * 2.6 + i as f32 * 0.55).sin().abs()) * 34.0 * vis;
            let bar = Rect::from_center_size(
                Pos2::new(start + pitch * 0.5 + i as f32 * pitch, r.center().y + 16.0),
                Vec2::new(pitch * 0.45, h.max(2.0)),
            );
            let col = if i % 2 == 0 { BLUE } else { theme::AUDIO_BLUE };
            painter.rect_filled(self.xf.rect(bar), 2.0, alpha(col, 0.5 + 0.5 * vis));
        }
    }

    fn draw_delay_display(&self, painter: &egui::Painter, unit: usize) {
        let r = self.unit_rects[unit];
        let led = self.units[unit].led;
        let (start, width) = self.display_area(unit);
        if width < 40.0 { return; }
        let pitch = (width / 4.5).min(26.0);
        for i in 0..4 {
            let c = Vec2::new(start + pitch * 0.5 + i as f32 * pitch, r.center().y + 16.0);
            let a = 0.15 + 0.85 * led * (1.0 - i as f32 * 0.2);
            self.ell(painter, c, 6.0 - i as f32 * 1.1, alpha(PURPLE, a), Stroke::NONE);
        }
        if led > 0.05 {
            let c = Vec2::new(start + pitch * 0.5, r.center().y + 16.0);
            self.ell(painter, c, 10.0 + (1.0 - led) * 14.0, alpha(PURPLE, 0.25 * led), Stroke::NONE);
        }
    }

    /// Vertical LED meter on the tall MASTER unit (bottom = quiet).
    /// Gradient green→amber→red with a slow-release peak-hold tick.
    fn draw_meter(&self, painter: &egui::Painter, r: Rect) {
        let n = 14;
        let lit = (self.meter * n as f32).ceil() as usize;
        let pitch = ((r.height() - 130.0) / n as f32).clamp(8.0, 17.0);
        let x = r.left() + 34.0;
        let bottom = r.bottom() - 62.0;
        for i in 0..n {
            let seg = Rect::from_min_size(
                Pos2::new(x, bottom - (i + 1) as f32 * pitch),
                Vec2::new(22.0, pitch * 0.72),
            );
            let col = theme::meter_color(i as f32 / (n - 1) as f32);
            let fill = if i < lit { col } else { BG };
            painter.rect_filled(self.xf.rect(seg), 2.5, fill);
        }
        // Peak-hold tick.
        if self.meter_peak > 0.02 {
            let i = ((self.meter_peak * n as f32).ceil() as usize).clamp(1, n) - 1;
            let tick = Rect::from_min_size(
                Pos2::new(x, bottom - (i + 1) as f32 * pitch - 2.0),
                Vec2::new(22.0, 1.6),
            );
            painter.rect_filled(self.xf.rect(tick), 0.8, theme::meter_color(self.meter_peak));
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
        self.ell(painter, c, r, mix(theme::SURFACE_HI, theme::SURFACE, 0.5),
            Stroke::new(1.4, if hovered || dragging { mix(RING, u.accent, 0.6) } else { RING }));

        // Value arc: HAIRLINE track + glowing accent arc + indicator dot
        // (theme helper; the closure carries the flip transform).
        theme::value_arc(painter, |l| xf.p(l), c, r, k.value, u.accent);

        // Pointer.
        let a0 = 135.0f32.to_radians();
        let sweep = 270.0f32.to_radians();
        let ang = a0 + sweep * k.value;
        let tip = xf.p(c + Vec2::new(ang.cos(), ang.sin()) * (r * 0.62));
        painter.line_segment([xf.p(c), tip], Stroke::new(2.4, TEXT));
        let lw = k.label_g.as_ref().map_or(0.0, |g| g.size().x);
        self.text(painter, c + Vec2::new(-lw / 2.0, r + 10.0), &k.label_g, SUBTEXT);

        // Live monospace value chip while dragging (Signal design language).
        if dragging {
            let g = painter.layout(format!("{:.2}", k.value), FontId::monospace(11.0), theme::AUDIO_CYAN, f32::INFINITY);
            let chip = Rect::from_min_size(
                xf.p(c + Vec2::new(-g.size().x / 2.0 - 6.0, -r - 28.0)),
                g.size() + Vec2::new(12.0, 6.0),
            );
            painter.rect_filled(chip, 6.0, alpha(BG, 0.92));
            painter.rect_stroke(chip, 6.0, Stroke::new(1.0, alpha(u.accent, 0.5)), egui::StrokeKind::Inside);
            painter.galley(chip.min + Vec2::new(6.0, 3.0), g, theme::AUDIO_CYAN);
        }
    }

    fn draw_back(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            let r = self.unit_rects[ui];
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            let top = if dragging { mix(theme::SURFACE, PURPLE, 0.14) } else { theme::SURFACE };
            theme::gradient_rect(painter, sr, 10.0, top, PLATE_BACK);
            let (bw, bc) = if dragging { (1.6, alpha(PURPLE, 0.85)) } else { (1.0, alpha(u.accent, 0.25)) };
            painter.rect_stroke(sr, 10.0, Stroke::new(bw, bc), egui::StrokeKind::Inside);
            theme::top_highlight(painter, sr, 10.0);
            if dragging { theme::glow_rect(painter, sr, 10.0, PURPLE, 0.9); }
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
            // Clap sources warm their cable with the TONE knob.
            if let Some(su) = self.units.get(c.src) {
                if matches!(su.node_kind, UnitKind::Clap) {
                    let tone = su.knobs.first().map(|k| k.value).unwrap_or(0.35);
                    col = mix(col, ORANGE, tone * 0.45);
                }
            }
            let warm = self.master_idx().map(|m| knob_val(&self.units[m], P_WARM, 0.15)).unwrap_or(0.15);
            col = mix(col, ORANGE, warm * 0.30);
        }
        col
    }

    fn draw_cables(&self, painter: &egui::Painter, hl: &[bool; MAX_CABLES], has_sel: bool) {
        let xf = &self.xf;
        let volume = self.master_idx()
            .map(|m| 0.15 + knob_val(&self.units[m], P_VOL, 0.8) * 0.85)
            .unwrap_or(0.83);
        for (ci, c) in self.cables.iter().enumerate() {
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.01 { continue; }
            let hovered = matches!(self.hover, Some(Hit::Cable(i, _)) if i == ci);
            let grabbed = matches!(self.drag, Drag::Cable { ci: i, .. } if i == ci);
            let level = if c.kind == PortKind::Audio {
                ((c.level + 0.05 + 0.035 * (self.ui_time * 1.4 + c.phase).sin()) * volume).clamp(0.0, 1.0)
            } else { 0.0 };
            let mut width = match c.kind {
                PortKind::Midi => 2.8,
                PortKind::Audio => 3.2 + 2.6 * level,
            };
            if hovered || grabbed { width += 0.8; }

            if c.n < 2 { continue; }
            let pts: Vec<Pos2> = c.pts[..c.n].iter().map(|p| xf.p(p.to_vec2())).collect();
            let curve = |w: f32, col: Color32| Shape::line(pts.clone(), Stroke::new(w, col));
            let col = self.cable_color(c);

            // Glow: 3 layered strokes, wide low-alpha → narrow full-color.
            // Audio breathes with the live level; MIDI keeps a faint spark halo.
            if c.kind == PortKind::Audio {
                let gw = width + 6.0 + 11.0 * level;
                painter.add(curve(gw, alpha(col, a * (0.05 + 0.20 * level))));
                painter.add(curve(width + 2.5, alpha(col, a * (0.10 + 0.25 * level))));
            } else {
                painter.add(curve(width + 4.0, alpha(col, a * 0.08)));
            }
            if has_sel && hl[ci] {
                painter.add(curve(width + 7.0, alpha(PURPLE, 0.16 * a)));
            }
            let mut main_col = col;
            if c.kind == PortKind::Audio { main_col = mix(main_col, WHITE, 0.22 * level); }
            if hovered || grabbed { main_col = mix(main_col, WHITE, 0.18); }
            painter.add(curve(width, alpha(main_col, a)));
            if self.selected == Some(c.id) {
                painter.add(curve(1.1, alpha(mix(PURPLE, WHITE, 0.5), 0.6 * a)));
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
                painter.rect_filled(xf.rect(socket), 5.0, BG);
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
        for (ci, c) in self.cables.iter().enumerate() {
            if c.dying >= 0.0 { continue; }
            let a = Self::cable_alpha(c, false, true);
            for end in [Dir::Out, Dir::In] {
                let over = if end == Dir::Out { c.over_a } else { c.over_b };
                let Some(r) = &c.rope else { continue };
                let base = if end == Dir::Out { r.start() } else { r.end() };
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
                let t0 = (t - trail).max(0.0);
                if c.n >= 2 && t > t0 + 1e-4 {
                    let seg = c.arc_slice(t0, t).into_iter().map(|p| xf.p(p.to_vec2())).collect();
                    painter.add(Shape::line(seg, Stroke::new(base_w + 1.2, alpha(mix(col, WHITE, 0.55), 0.9 * a))));
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
        let mut samples = [Pos2::ZERO; MAX_SAMPLES];
        let n = p.rope.sample(&mut samples);
        let pts: Vec<Pos2> = samples[..n].iter().map(|q| xf.p(q.to_vec2())).collect();
        let col = p.kind.color();
        painter.add(Shape::line(pts.clone(), Stroke::new(6.5, alpha(col, 0.14))));
        painter.add(Shape::line(pts, Stroke::new(2.8, alpha(col, 0.85))));
        let cursor = xf.p(p.cursor);
        painter.circle_filled(cursor, 6.5, alpha(col, 0.95));
        painter.circle_stroke(cursor, 9.5, Stroke::new(1.6, alpha(col, 0.5)));
    }

    fn draw_toolbar(&self, painter: &egui::Painter, top: f32) {
        let bar = Rect::from_min_size(Pos2::new(16.0, top + 16.0), Vec2::new(376.0, 48.0));
        theme::drop_shadow(painter, bar, 24.0);
        theme::gradient_rect(painter, bar, 24.0, theme::SURFACE_HI, theme::SURFACE);
        painter.rect_stroke(bar, 24.0, Stroke::new(1.0, alpha(BLUE, 0.25)), egui::StrokeKind::Inside);
        theme::top_highlight(painter, bar, 24.0);

        // Transport play: glowing green when running, glass + green ring idle.
        let c = Pos2::new(40.0, top + 40.0);
        let hovered = matches!(self.hover, Some(Hit::Play));
        let ink;
        if self.playing {
            painter.circle_filled(c, 19.0, alpha(GREEN, 0.18));
            painter.circle_filled(c, 15.0, if hovered { mix(GREEN, WHITE, 0.15) } else { GREEN });
            ink = BG;
        } else {
            painter.circle_filled(c, 15.0, if hovered { mix(theme::SURFACE_HI, WHITE, 0.08) } else { theme::SURFACE_HI });
            painter.circle_stroke(c, 15.0, Stroke::new(1.2, alpha(GREEN, if hovered { 0.8 } else { 0.45 })));
            ink = GREEN;
        }
        if self.playing {
            // Stop square (transport is Play/Stop, not play/pause).
            painter.rect_filled(Rect::from_min_size(Pos2::new(34.5, top + 34.5), Vec2::new(11.0, 11.0)), 1.6, ink);
        } else {
            painter.add(Shape::convex_polygon(
                vec![Pos2::new(36.5, top + 32.5), Pos2::new(36.5, top + 47.5), Pos2::new(48.5, top + 40.0)],
                ink, Stroke::NONE));
        }

        let beat = 60.0 / self.bpm.max(1.0) as f64;
        let beat_phase = ((self.time / beat).fract() as f32).clamp(0.0, 1.0);
        let thump = if self.playing { (1.0 - beat_phase).powi(3) } else { 0.0 };
        painter.circle_filled(Pos2::new(66.0, top + 40.0), 3.0 + 3.5 * thump, alpha(GREEN, 0.35 + 0.65 * thump));

        if let Some(g) = &self.wordmark_g { painter.galley(Pos2::new(80.0, top + 24.0), g.clone(), TEXT); }
        let state_g = if self.playing { &self.state_play_g } else { &self.state_pause_g };
        if let Some(g) = state_g { painter.galley(Pos2::new(80.0, top + 42.0), g.clone(), SUBTEXT); }

        // Flip button.
        let fb = Rect::from_min_size(Pos2::new(246.0, top + 24.0), Vec2::new(126.0, 32.0));
        let fhover = matches!(self.hover, Some(Hit::Flip));
        painter.rect_filled(fb, 16.0, if fhover { mix(theme::SURFACE_HI, BLUE, 0.12) } else { theme::BG_RAISED });
        painter.rect_stroke(fb, 16.0, Stroke::new(1.0, if fhover { alpha(YELLOW, 0.5) } else { BORDER }), egui::StrokeKind::Inside);
        // ⇄ arrows drawn as shapes (font-independent).
        let ax = 260.0;
        painter.line_segment([Pos2::new(ax, top + 34.0), Pos2::new(ax + 12.0, top + 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 8.0, top + 30.5), Pos2::new(ax + 12.0, top + 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 8.0, top + 37.5), Pos2::new(ax + 12.0, top + 34.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax, top + 46.0), Pos2::new(ax + 12.0, top + 46.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 4.0, top + 42.5), Pos2::new(ax, top + 46.0)], Stroke::new(1.8, TEXT));
        painter.line_segment([Pos2::new(ax + 4.0, top + 49.5), Pos2::new(ax, top + 46.0)], Stroke::new(1.8, TEXT));
        if let Some(g) = &self.flip_label_g { painter.galley(Pos2::new(280.0, top + 33.0), g.clone(), TEXT); }
    }

    fn draw_hint(&self, painter: &egui::Painter, rect: Rect) {
        let g = if self.pending.is_some() { &self.hint_wire_g }
            else if self.side == Side::Front { &self.hint_front_g }
            else { &self.hint_back_g };
        let Some(g) = g else { return };
        let w = g.size().x + 28.0;
        let bar = Rect::from_min_size(Pos2::new(16.0, rect.bottom() - 42.0), Vec2::new(w, 28.0));
        painter.rect_filled(bar, 14.0, alpha(theme::BG_RAISED, 0.92));
        painter.rect_stroke(bar, 14.0, Stroke::new(1.0, alpha(BORDER, 0.9)), egui::StrokeKind::Inside);
        painter.galley(bar.min + Vec2::new(14.0, 7.0), g.clone(), SUBTEXT);
    }

    fn draw_stats(&mut self, painter: &egui::Painter, rect: Rect) {
        self.stats_timer -= 1.0 / 60.0;
        if self.stats_timer <= 0.0 || self.stats_g.is_none() {
            self.stats_timer = 0.25;
            let pulses: usize = self.cables.iter().map(|c| c.pulses.iter().filter(|p| p.active).count()).sum();
            let midi_ms = crate::bridge::MIDI_LATENCY_US.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0;
            let text = format!(
                "{:.0} fps · {} cables · {} pulses · midi {:.1}ms · {}",
                self.fps, self.cables.len(), pulses, midi_ms,
                if self.side == Side::Front { "front" } else { "back" });
            self.stats_g = Some(painter.layout(text, FontId::monospace(11.0), SUBTEXT, f32::INFINITY));
        }
        if let Some(g) = &self.stats_g {
            let w = g.size().x + 24.0;
            let bar = Rect::from_min_size(Pos2::new(rect.right() - w - 16.0, 16.0), Vec2::new(w, 26.0));
            painter.rect_filled(bar, 13.0, alpha(theme::BG_RAISED, 0.92));
            painter.rect_stroke(bar, 13.0, Stroke::new(1.0, alpha(BORDER, 0.9)), egui::StrokeKind::Inside);
            painter.galley(bar.min + Vec2::new(12.0, 6.0), g.clone(), SUBTEXT);
        }
    }

    fn draw_help(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 0.0, alpha(theme::SCRIM, 0.62));
        let card = Rect::from_center_size(rect.center(), Vec2::new(560.0, 352.0));
        theme::glass_panel(painter, card, 18.0, PURPLE);
        if let Some(g) = &self.help_title_g { painter.galley(card.min + Vec2::new(28.0, 24.0), g.clone(), TEXT); }
        if let Some(g) = &self.help_body_g { painter.galley(card.min + Vec2::new(28.0, 62.0), g.clone(), TEXT); }
        if let Some(g) = &self.help_foot_g { painter.galley(card.min + Vec2::new(28.0, 318.0), g.clone(), SUBTEXT); }
    }
}

fn p_side(unit: usize, app: &DemoApp) -> f32 {
    let x = app.jack_pos(unit, 0).x;
    if x < app.rack_w / 2.0 { -1.0 } else { 1.0 }
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

        if self.flip_requested {
            self.flip_requested = false;
            self.start_flip();
        }

        // Per-rig input selection strip (top, includes the transport row), then
        // assistant (right), then rack.
        self.rig_inputs_panel(ui);
        self.assistant_panel_ui(ui);

        // Native rig editor (task-26): handle open/close requests from unit
        // double-clicks. No X11 — the editor is a plain egui window drawn below.
        if let Some(rig) = self.editor_request.take() {
            self.toggle_param_editor(rig);
        }

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
                self.current_source = SourceId::Keys;
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
            // Hardware input latency: arrival (midir callback) → UI drain (task-23).
            crate::bridge::record_midi_latency(ev.created.elapsed().as_micros() as u32);
            let semi = ev.note as usize;
            self.current_source = ev.source;
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

        // Publish the patch through the control plane: cables become Connect
        // commands (duplicates rejected by the model), knobs become SetParam.
        // Node ids come from the units (unit index ≠ node id in general).
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
            for u in &self.units {
                for k in &u.knobs {
                    if let Some(p) = k.param {
                        let _ = b.send(Command::SetParam { node: u.node_id, param: p, value: k.value as f64 });
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

        self.tessellate();
        self.draw(ui);
        self.sessions_window(&ctx);
        self.plugins_window(&ctx);
        self.transport_window(&ctx);
        self.param_editor_window(&ctx);

        ctx.request_repaint();
    }

    fn on_exit(&mut self) {
        // Native editor holds no plugin GUI; just drop its cached state.
        self.editor_open = false;
        self.editor_params.clear();
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
fn build_audio() -> anyhow::Result<(Vec<ClapVoiceMain>, Vec<VoiceController>, cpal::Stream, CommandBus, Arc<std::sync::atomic::AtomicU32>, f64, crate::player::PlayerControl, Arc<Mutex<crate::control::RigSnapshot>>)> {
    let host = preferred_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let cfg = device.default_output_config()?;
    let sample_rate = cfg.sample_rate() as f64;
    let ch = cfg.channels() as usize;
    // Two instrument voices — one per channel stack (nodes 1 and 4).
    let (voice_a, main_a) = ClapVoice::new(sample_rate)?;
    let (voice_b, main_b) = ClapVoice::new(sample_rate)?;
    // Grab RT-safe param controllers BEFORE the voices move into the engine.
    let controllers = vec![voice_a.controller(), voice_b.controller()];
    let mut config: cpal::StreamConfig = cfg.into();
    // Low latency with headroom: 512 frames @48k = 10.7 ms. Survives CPU load
    // spikes (e.g. parallel builds) that would underrun a 256-frame buffer.
    config.buffer_size = cpal::BufferSize::Fixed(crate::bridge::BUFFER_FRAMES);
    eprintln!(
        "audio: host={} device={} rate={} ch={} buffer=512 frames ({:.1} ms) voices=2",
        host.id().name(),
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        sample_rate,
        ch,
        512.0 / sample_rate * 1000.0
    );
    let (bus, drain) = crate::control::command_bus();
    let mut engine = GraphEngine::new(
        Graph::demo_patch(),
        drain,
        sample_rate,
        vec![(1, voice_a), (4, voice_b)],
    );
    let level = engine.level_handle();
    // Multi-rig snapshot: engine publishes rig list/active on LoadRigs; the
    // bridge + UI read it (the audio render path never locks it).
    let rig_snapshot = Arc::new(Mutex::new(crate::control::RigSnapshot::default()));
    engine.set_rig_snapshot(rig_snapshot.clone());
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
    // File player (backing tracks): decode thread + ring; UI holds the control.
    let (player_sink, player_ctrl) = crate::player::start(sample_rate as u32, ch);
    engine.set_player_sink(player_sink);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            engine.render(data, ch);
        },
        |e| {
            crate::bridge::XRUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!("audio stream error: {e}");
        },
        None,
    )?;
    stream.play()?;
    Ok((vec![main_a, main_b], controllers, stream, bus, level, sample_rate, player_ctrl, rig_snapshot))
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
            cc.egui_ctx.set_visuals(theme::visuals());
            theme::install_fonts(&cc.egui_ctx);

            let mut sink = sink;
            let mut audio = None;
            if with_audio {
                match build_audio() {
                    Ok((mains, controllers, stream, bus, level, sample_rate, player_ctrl, rig_snapshot)) => {
                        let bus = Arc::new(Mutex::new(bus));
                        // Control bridge (task-16): MCP/CLI drive THIS app via
                        // the same bus + a live graph mirror. Additive — if the
                        // socket is unavailable the app just runs without it.
                        let shared_graph = Arc::new(Mutex::new(Graph::demo_patch()));
                        if let Err(e) = crate::bridge::spawn_with_rigs(
                            bus.clone(),
                            shared_graph.clone(),
                            level.clone(),
                            sample_rate,
                            rig_snapshot.clone(),
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
                        audio = Some((mains, controllers, stream, bus, level, shared_graph, player_ctrl, rig_snapshot));
                    }
                    Err(e) => eprintln!("Surge/audio unavailable, running silent: {e}"),
                }
            }
            let mut app = DemoApp::new_with_sink(sink);
            let app = match audio {
                Some((mains, controllers, stream, bus, level, shared_graph, player_ctrl, rig_snapshot)) => {
                    app.shared_graph = shared_graph;
                    app.rig_snapshot = rig_snapshot;
                    app.player_ctrl = Some(player_ctrl);
                    app.with_audio(mains, controllers, stream, bus, level)
                }
                None => app,
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))
}