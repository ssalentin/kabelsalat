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
const WHITE: Color32 = theme::ON_ACCENT; // #FFFBFA
const BLACK: Color32 = theme::INK; // #1C1C1C

/// The reference CLAP instrument (PLAN §1). The engine's two voices are Surge,
/// so it is the only CLAP plugin that can currently be added from the browser.
const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

const MAX_SAMPLES: usize = 49; // arc-length LUT cap (48 segments per cable)
const MAX_PULSES: usize = 12;
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
    label: &'static str,
    value: f32,
    default: f32,
    /// Engine param id this knob publishes (None = visual-only knob).
    param: Option<u32>,
    label_g: Option<Arc<theme::TrackedLabel>>,
}

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
    name_g: Option<Arc<Galley>>,
    sub_g: Option<Arc<theme::TrackedLabel>>,
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
    Knob { label, value: default, default, param, label_g: None }
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

/// One row of the hot-wire MIDI matrix (PLAN §13): a MIDI source that can
/// feed any instrument rig. `HwAll` = any hardware device (omni).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MatrixRow { Keys, Laptop, HwAll, HwDev(u32) }

/// Effective cell state for a CLAP rig: the MIDI cable gate AND the source
/// filter/device binding (mirrors engine::source_accepted + keys_gate).
fn clap_cell(src: (bool, bool, bool), dev: Option<u32>, gate: bool, row: MatrixRow) -> bool {
    if !gate { return false; }
    match row {
        MatrixRow::Keys => src.0,
        MatrixRow::Laptop => src.1,
        MatrixRow::HwAll => src.2 && dev.is_none(),
        MatrixRow::HwDev(d) => src.2 && dev == Some(d),
    }
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

/// Plain-text toggle (§4.14): no chip, no box, no dot. On = ink text with a
/// 2px accent underline (the reference's active-row logic); off = secondary.
fn text_toggle(ui: &mut egui::Ui, name: &str, on: bool) -> bool {
    let txt = egui::RichText::new(name).size(13.0)
        .color(if on { theme::INK } else { theme::INK_2 });
    let resp = ui.add(egui::Label::new(txt).sense(egui::Sense::click()));
    if on {
        let r = resp.rect;
        ui.painter().line_segment(
            [Pos2::new(r.left(), r.bottom() + 1.0), Pos2::new(r.right(), r.bottom() + 1.0)],
            Stroke::new(2.0, theme::ACCENT),
        );
    }
    if resp.hovered() { ui.ctx().set_cursor_icon(CursorIcon::PointingHand); }
    resp.clicked()
}

/// Nav item (§4.14): plain 13px ink text; active/open = deep-accent text +
/// 2px accent underline (the reference's active-row logic). No rings/chips.
fn nav_button(ui: &mut egui::Ui, name: &str, active: bool) -> bool {
    let color = if active { theme::ACCENT_DEEP } else { theme::INK };
    let resp = ui.add(egui::Label::new(egui::RichText::new(name).size(13.0).color(color))
        .sense(egui::Sense::click()));
    if active {
        let r = resp.rect;
        ui.painter().line_segment(
            [Pos2::new(r.left(), r.bottom() + 1.0), Pos2::new(r.right(), r.bottom() + 1.0)],
            Stroke::new(2.0, theme::ACCENT),
        );
    }
    if resp.hovered() { ui.ctx().set_cursor_icon(CursorIcon::PointingHand); }
    resp.clicked()
}

/// 1px hairline vertical rule between nav items (§4.14).
fn nav_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, 14.0), egui::Sense::hover());
    ui.painter().line_segment(
        [Pos2::new(rect.center().x, rect.top()), Pos2::new(rect.center().x, rect.bottom())],
        Stroke::new(1.0, theme::HAIRLINE),
    );
}

/// Build a rack unit from a control-plane node. The face (jacks + knobs) is
/// chosen by node kind; known kinds keep their bespoke faces, other DSP kinds
/// get a generic labeled box with 1–2 knobs bound to engine params 0/1.
/// Unit identity is carried by typography and position — no per-unit hues.
fn make_unit(node: &Node) -> Unit {
    let name = name_str(&node.name).to_string();
    let (node_kind, sub, ins, outs, knobs, jacks): (
        UnitKind, String, Vec<PortKind>, Vec<PortKind>, Vec<Knob>, Vec<(Dir, usize, f32, &'static str)>,
    ) = match &node.kind {
        NodeKind::Keys => (
            UnitKind::Keys, "midi source · click the keys".into(),
            vec![], vec![PortKind::Midi],
            vec![knob("RATE", 0.5, None), knob("VEL", 0.8, None)],
            vec![(Dir::Out, 0, 0.62, "MIDI OUT")],
        ),
        NodeKind::Clap { .. } => (
            UnitKind::Clap, "synth · clap".into(),
            vec![PortKind::Midi], vec![PortKind::Audio],
            vec![knob("TONE", 0.35, None), knob("LEVEL", 0.85, Some(P_LEVEL))],
            vec![(Dir::In, 0, 0.30, "MIDI IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::MasterOut => (
            UnitKind::Master, "stereo + mono bus".into(),
            vec![PortKind::Audio, PortKind::Audio, PortKind::Audio], vec![],
            vec![knob("VOLUME", 0.8, Some(P_VOL)), knob("WARM", 0.15, Some(P_WARM))],
            vec![(Dir::In, 0, 0.30, "L"), (Dir::In, 2, 0.50, "MONO"), (Dir::In, 1, 0.70, "R")],
        ),
        NodeKind::Dsp(DspKind::Delay) => (
            UnitKind::Dsp(DspKind::Delay), "insert fx · midi tap".into(),
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
                UnitKind::Dsp(kind.clone()), sub,
                vec![PortKind::Audio], vec![PortKind::Audio],
                knobs,
                vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
            )
        }
        NodeKind::Lv2 { .. } => (
            UnitKind::Dsp(DspKind::Gain), "lv2 effect".into(),
            vec![PortKind::Audio], vec![PortKind::Audio],
            vec![knob("GAIN", 0.9, Some(0))],
            vec![(Dir::In, 0, 0.30, "IN"), (Dir::Out, 0, 0.70, "OUT")],
        ),
        NodeKind::SystemAudio => (
            UnitKind::Dsp(DspKind::Gain), "system audio".into(),
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
    /// Peak-hold for the master meter (slow release).
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
    state_play_g: Option<Arc<theme::TrackedLabel>>,
    state_pause_g: Option<Arc<theme::TrackedLabel>>,
    flip_label_g: Option<Arc<Galley>>,
    /// Flip-pill width, derived from the measured label (§4.13); the rect is
    /// right-anchored at x=376 and shared by draw_toolbar and hit_test.
    flip_w: f32,
    hint_front_g: Option<Arc<Galley>>,
    hint_back_g: Option<Arc<Galley>>,
    hint_wire_g: Option<Arc<Galley>>,
    help_title_g: Option<Arc<theme::TrackedLabel>>,
    help_body_g: Option<Arc<Galley>>,
    help_foot_g: Option<Arc<theme::TrackedLabel>>,

    arrivals: [(usize, f32); 32],

    /// Audio-integration seam: interactive key presses are delivered here.
    note_sink: Option<NoteSink>,

    /// Set by raw_input_hook when Tab is pressed (stripped from egui's input
    /// so focus navigation never sees it); consumed in ui().
    flip_requested: bool,

    /// Source tag applied to the next fire_keys (set by each input path).
    current_source: SourceId,
    /// Multi-rig UI (PLAN §13.1): the rig shown in the big view. Visual only —
    /// every rig keeps sounding; switching never touches the audio path.
    active_rig: usize,
    /// Smoothed per-rig activity for the overview strip (max unit LED).
    rig_levels: Vec<f32>,
    /// Hot-wire MIDI matrix window (PLAN §13).
    show_matrix: bool,
    /// Per-hardware-device activity LEDs for the matrix row headers.
    hw_leds: Vec<f32>,
    /// Motion (design polish): staged fade-in after a template/session load.
    load_fade: f32,
    /// Per-unit hover easing (0..1, ~120ms) for the hairline rule brighten.
    unit_hover: Vec<f32>,
    /// Toolbar hover easing (~120ms).
    play_hover_t: f32,
    flip_hover_t: f32,
    /// Previous frame's keyboard-focus state: losing focus drains held keys
    /// so note-offs can't orphan (vibez lesson, docs/vibez-notes.md).
    had_kb_focus: bool,

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
    /// Engine graph publication (MCP→UI live sync). None in silent/demo mode.
    graph_pub: Option<Arc<Mutex<crate::engine::GraphPub>>>,
    /// Last engine graph revision the UI has seen/adopted.
    last_graph_rev: u64,
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
            flip_w: 150.0,
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
            active_rig: 0,
            rig_levels: Vec::new(),
            show_matrix: false,
            hw_leds: Vec::new(),
            load_fade: 1.0,
            unit_hover: Vec::new(),
            play_hover_t: 0.0,
            flip_hover_t: 0.0,
            had_kb_focus: true,
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
            graph_pub: None,
            last_graph_rev: 0,
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
        // Classify LV2 instruments (hot-wire matrix sink columns). Rare
        // structural op: the discovery scan runs only when LV2 nodes exist.
        if graph.nodes.iter().any(|n| matches!(n.kind, NodeKind::Lv2 { .. })) {
            let infos = lv2_discover();
            for u in &mut self.units {
                if let Some(NodeKind::Lv2 { uri }) = graph.nodes.iter().find(|n| n.id == u.node_id).map(|n| &n.kind) {
                    u.lv2_inst = infos.iter().any(|p| p.uri == uri.as_str() && p.is_instrument);
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
        self.hover = None;
        self.graph_sig.clear(); // force shared-graph re-mirror
        // Motion: staged fade-in for the new rack.
        self.load_fade = 0.0;
        self.unit_hover = vec![0.0; self.units.len()];
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
        let mut claimed = vec![false; self.units.len()];
        for (i, u) in self.units.iter().enumerate() {
            if !in_rig(u) { claimed[i] = true; } // never group hidden units
        }
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
        // Hidden units (other rigs) keep no rect: layout/draw/hit skip them.
        for (i, u) in self.units.iter().enumerate() {
            if !in_rig(u) {
                if let Some(r) = self.unit_rects.get_mut(i) { *r = Rect::NOTHING; }
            }
        }
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
        let gap = 12.0;
        let pad = 20.0;
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
        // Tracked micro-caps (§4.5): per-char galleys, cached like the rest.
        let tracked = |text: &str, size: f32, tracking_em: f32| -> Arc<theme::TrackedLabel> {
            let font = FontId::proportional(size);
            Arc::new(ctx.fonts_mut(|f| theme::layout_tracked(f, text, font, tracking_em)))
        };
        // Unit galleys: (re)fill any that are missing — templates rebuild units.
        for u in &mut self.units {
            if u.name_g.is_none() {
                // Name: Medium 15px, sentence case (§4.3). Subtitle + back
                // label: tracked micro-caps 10px (+0.14em).
                u.name_g = Some(layout(&sentence_case(&u.name), theme::medium_font(16.0), TEXT, f32::INFINITY));
                u.sub_g = Some(tracked(&u.sub.to_uppercase(), 10.0, 0.14));
                u.back_g = Some(tracked(&u.name.to_uppercase(), 10.0, 0.14));
            }
            for k in &mut u.knobs {
                if k.label_g.is_none() {
                    k.label_g = Some(tracked(k.label, 10.0, 0.14));
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
        self.state_play_g = Some(tracked("PLAYING", 10.0, 0.14));
        self.state_pause_g = Some(tracked("STOPPED", 10.0, 0.14));
        // Flip pill (§4.13): Bold 19px = WCAG large text, so ON_ACCENT on the
        // vivid tomato passes (3.63 ≥ 3.0). Shorten the label if it outgrows
        // the pill; the pill width follows the measured galley.
        let mut flip_g = layout("flip rack · Tab", theme::bold_font(19.0), WHITE, f32::INFINITY);
        if flip_g.size().x > 128.0 {
            flip_g = layout("flip · Tab", theme::bold_font(19.0), WHITE, f32::INFINITY);
        }
        self.flip_w = (flip_g.size().x + 22.0).max(150.0).min(170.0);
        self.flip_label_g = Some(flip_g);
        // Hints (§4.15): plain caption lines on paper, sentence case.
        self.hint_front_g = Some(layout(
            "perform: drag knobs · click the keys · press Tab to flip the rack and patch",
            FontId::proportional(12.5), theme::INK_2, f32::INFINITY));
        self.hint_back_g = Some(layout(
            "patch: drag jack to jack · grab a cable to shake it · tug a plug to unplug · Tab flips back",
            FontId::proportional(12.5), theme::INK_2, f32::INFINITY));
        self.hint_wire_g = Some(layout(
            "now click a highlighted jack to connect · esc or right-click cancels",
            FontId::proportional(12.5), theme::ACCENT_DEEP, f32::INFINITY));
        // Help card (§4.16): Medium 22px title, 14px body with tight leading
        // so all 15 rows fit the 560×352 card, tracked micro-caps footer.
        self.help_title_g = Some(Arc::new(ctx.fonts_mut(|f| {
            theme::layout_tracked(f, "kabelsalat — the cables are the show", theme::medium_font(22.0), -0.03)
        })));
        // Body: 14px cream with tight leading (15 rows fit the 352px card);
        // key-name columns highlighted in ACCENT_BRIGHT — the reference's
        // dark-panel "orange highlighted phrases" (§4.16).
        let body = "starter templates: Sessions window, or F5 Lead+Reverb / F6 Drums+Bass\n\
             F7 Ambient Pad / F8 demo / F4 multi-rig — each loads a playable rack\n\
             F9 Plugins · F11 MIDI matrix — browse plugins / hot-wire sources→rigs\n\
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
             1–9/←→ rigs · R reset · D stats · ? help · esc cancel";
        let body_fmt = egui::epaint::text::TextFormat {
            font_id: FontId::proportional(14.0),
            color: TEXT,
            line_height: Some(16.5),
            ..Default::default()
        };
        let mut body_job = egui::epaint::text::LayoutJob::simple(
            body.to_owned(), FontId::proportional(14.0), TEXT, 504.0);
        body_job.sections.clear();
        let mut start = 0usize;
        for line in body.split_inclusive('\n') {
            let end = start + line.len();
            let key_end = line.trim_end_matches('\n').find("  ").map(|p| start + p);
            match key_end {
                Some(ke) => {
                    body_job.sections.push(egui::epaint::text::LayoutSection {
                        leading_space: 0.0,
                        byte_range: egui::text::ByteIndex(start)..egui::text::ByteIndex(ke),
                        format: egui::epaint::text::TextFormat { color: theme::ACCENT_BRIGHT, ..body_fmt.clone() },
                    });
                    body_job.sections.push(egui::epaint::text::LayoutSection {
                        leading_space: 0.0,
                        byte_range: egui::text::ByteIndex(ke)..egui::text::ByteIndex(end),
                        format: body_fmt.clone(),
                    });
                }
                None => body_job.sections.push(egui::epaint::text::LayoutSection {
                    leading_space: 0.0,
                    byte_range: egui::text::ByteIndex(start)..egui::text::ByteIndex(end),
                    format: body_fmt.clone(),
                }),
            }
            start = end;
        }
        self.help_body_g = Some(ctx.fonts_mut(|f| f.layout_job(body_job)));
        self.help_foot_g = Some(tracked("ESC OR ? TO CLOSE", 10.0, 0.14));
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
        let gap = 12.0;
        let pad = 20.0;
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
        // Audio is one cream family now (§3); MIDI stays blue.
        let color = match kind {
            PortKind::Midi => YELLOW,
            PortKind::Audio => theme::AUDIO_CREAM,
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

    /// MCP→UI live sync (PLAN §13): adopt the engine's graph when a remote
    /// structural change (MCP/CLI load_graph/load_rigs/add_node/connect/…)
    /// diverges from the UI's own mirror. UI-originated edits converge (the
    /// per-frame Connect mirror is idempotent engine-side), so they never
    /// trigger a rebuild — animations and interaction state survive.
    fn sync_from_engine(&mut self) {
        let Some(pub_) = &self.graph_pub else { return };
        let Ok(g) = pub_.lock() else { return };
        if g.rev == self.last_graph_rev { return; }
        self.last_graph_rev = g.rev;
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
        if same { return; }
        let adopted = g.graph.clone();
        drop(g);
        self.build_rack(&adopted, &[]);
        // Reset per-rig source filters to the engine's rig bindings.
        self.rig_sources = [(true, true, true); 2];
        self.rig_device = [None, None];
        if let Ok(s) = self.rig_snapshot.lock() {
            for (i, m) in s.rigs.iter().enumerate().take(2) {
                if let Some(d) = m.midi_device {
                    self.rig_sources[i] = (false, false, true);
                    self.rig_device[i] = Some(d);
                }
            }
        }
        self.split_on = false;
        self.editor_open = false;
        self.editor_params.clear();
        self.session_msg = "Patch updated from MCP/CLI".into();
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

    /// Load two independent rigs (PLAN §13.1, AUM operating model): each a
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
        self.editor_open = false;
        self.editor_params.clear();
        self.rig_sources = [(true, true, true); 2];
        self.rig_device = [None, None];
        self.split_on = false;
        if let Some(bus) = &self.bus {
            let _ = bus.lock().unwrap().send(Command::SetSplit { point: None });
        }
        self.session_msg = "Loaded multi-rig demo — 1–9 / ←→ switch rigs".into();
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
            "get_matrix" => {
                let sinks: Vec<serde_json::Value> = self.matrix_sinks().iter().map(|&s| {
                    let u = &self.units[s];
                    let gate = self.midi_cable_to(s).is_some();
                    let is_clap = matches!(u.node_kind, UnitKind::Clap);
                    let rig = self.rig_of_unit(s).unwrap_or(0);
                    let src = self.rig_sources.get(rig).copied().unwrap_or((true, true, true));
                    let dev = self.rig_device.get(rig).copied().flatten();
                    let cell = |row: MatrixRow| if is_clap { clap_cell(src, dev, gate, row) } else { gate };
                    serde_json::json!({
                        "node": u.node_id, "name": u.name, "rig": u.rig,
                        "kind": if is_clap { "clap" } else { "lv2" },
                        "midi_cable": gate,
                        "keys": cell(MatrixRow::Keys),
                        "laptop": cell(MatrixRow::Laptop),
                        "hw_all": cell(MatrixRow::HwAll),
                        "hw_devices": (0..self.hw_devices.len())
                            .map(|d| cell(MatrixRow::HwDev(d as u32))).collect::<Vec<_>>(),
                    })
                }).collect();
                serde_json::json!({
                    "sources": ["keys", "laptop", "hw_all", "hw:<device index>"],
                    "hw_devices": self.hw_devices,
                    "split": self.split_on,
                    "sinks": sinks,
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
                    "keys" => MatrixRow::Keys,
                    "laptop" => MatrixRow::Laptop,
                    "hw_all" => MatrixRow::HwAll,
                    s if s.starts_with("hw:") => match s[3..].parse::<u32>() {
                        Ok(d) => MatrixRow::HwDev(d),
                        Err(_) => return format!("bad source '{source}'"),
                    },
                    _ => return format!("unknown source '{source}' (keys|laptop|hw_all|hw:<d>)"),
                };
                self.matrix_toggle(inst, row, on);
                format!("matrix cell {source} → node {node} = {on}")
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
                    "multi_rig" | "rigs" => { self.load_multi_rig_demo(); "Loaded the multi-rig demo (2 rigs) — switch with set_active_rig or 1–9".into() }
                    _ => format!("unknown template '{name}' (use lead_reverb, drums_bass, ambient_pad, demo, multi_rig)"),
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
            .map(|u| sentence_case(&u.name))
            .unwrap_or_else(|| if rig == 0 { "Surge A".to_string() } else { "Surge B".to_string() });
        let mut open = self.editor_open;
        let mut refresh = false;
        let mut changes: Vec<(u32, f64)> = Vec::new();
        egui::Window::new(egui::RichText::new(format!("{name} — parameters"))
                .font(theme::medium_font(16.0)).color(theme::INK))
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
                                        let text = mains.get_mut(rig)
                                            .map(|m| m.param_text(p.id, value))
                                            .unwrap_or_default();
                                        ui.label(egui::RichText::new(text)
                                            .font(theme::medium_font(13.0)).color(theme::INK));
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

    // -- hot-wire MIDI matrix (PLAN §13) ------------------------------------

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
                MatrixRow::Laptop => s.1 = on,
                MatrixRow::HwAll => { s.2 = on; if on { *d = None; } }
                MatrixRow::HwDev(dev) => {
                    if on { s.2 = true; *d = Some(dev); }
                    else if *d == Some(dev) { s.2 = false; }
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

    /// The hot-wire MIDI matrix window: sources × instrument rigs, live.
    /// Hurricane language: paper sheet, hairline grid, teal = routed.
    fn matrix_window(&mut self, ctx: &egui::Context) {
        if !self.show_matrix { return; }
        let mut open = self.show_matrix;
        let sinks = self.matrix_sinks();
        let mut toggles: Vec<(usize, MatrixRow, bool)> = Vec::new();
        egui::Window::new(egui::RichText::new("MIDI matrix").font(theme::medium_font(16.0)).color(theme::INK))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);
                ui.label(egui::RichText::new("any source → any rig · cells hot-wire live (MIDI cable + source filter)")
                    .size(12.0).color(theme::INK_2));
                ui.add_space(6.0);
                if sinks.is_empty() {
                    ui.label(egui::RichText::new("no instruments in the patch — add a rig first").size(12.0).color(theme::INK_2));
                    return;
                }
                let mut rows: Vec<(MatrixRow, String, f32)> = vec![
                    (MatrixRow::Keys, "On-screen keys".into(), self.key_leds.iter().cloned().fold(0.0f32, f32::max)),
                    (MatrixRow::Laptop, "Laptop keys".into(), self.key_leds.iter().cloned().fold(0.0f32, f32::max)),
                    (MatrixRow::HwAll, "Hardware: all".into(), self.hw_leds.iter().cloned().fold(0.0f32, f32::max)),
                ];
                for (d, name) in self.hw_devices.iter().enumerate() {
                    rows.push((MatrixRow::HwDev(d as u32), format!("Hardware: {name}"),
                        self.hw_leds.get(d).copied().unwrap_or(0.0)));
                }
                egui::Grid::new("midi_matrix_grid").num_columns(sinks.len() + 1).spacing([10.0, 6.0]).show(ui, |ui| {
                    // Header: sink names + rig tags + cable state.
                    theme::section_label(ui, "Source");
                    for &s in &sinks {
                        let u = &self.units[s];
                        let gate = self.midi_cable_to(s).is_some();
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(sentence_case(&u.name)).font(theme::medium_font(13.0)).color(theme::INK));
                            ui.label(egui::RichText::new(format!("rig {} · midi {}", (b'A' + u.rig.min(25)) as char, if gate { "in ✓" } else { "unplugged" }))
                                .size(10.0).color(if gate { theme::LIVE_TEAL_DEEP } else { theme::INK_2 }));
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
                            ui.label(egui::RichText::new(label).size(12.0).color(theme::INK));
                        });
                        for &s in &sinks {
                            let u = &self.units[s];
                            let is_clap = matches!(u.node_kind, UnitKind::Clap);
                            let gate = self.midi_cable_to(s).is_some();
                            let on = if is_clap {
                                let rig = self.rig_of_unit(s).unwrap_or(0);
                                let src = self.rig_sources.get(rig).copied().unwrap_or((true, true, true));
                                let dev = self.rig_device.get(rig).copied().flatten();
                                clap_cell(src, dev, gate, *row)
                            } else {
                                gate // LV2: the cable is the route (all sources)
                            };
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(30.0, 22.0), egui::Sense::click());
                            let p = ui.painter();
                            let cell = rect.shrink(3.0);
                            if on {
                                p.rect_filled(cell, 2.0, theme::LIVE_TEAL);
                            } else {
                                p.rect_stroke(cell, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
                            }
                            if resp.hovered() {
                                p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                            }
                            if resp.clicked() { toggles.push((s, *row, !on)); }
                        }
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                let split_txt = if self.split_on {
                    "Split @ C4 active — low zone → first rig, high zone → second (toggle in the top strip)"
                } else {
                    "Full keyboard to every routed rig · split toggle in the top strip"
                };
                ui.label(egui::RichText::new(split_txt).size(11.0).color(theme::INK_2));
                if !self.hw_devices.is_empty() {
                    ui.label(egui::RichText::new("Hardware rows are a radio group per rig: one device, all devices, or off.")
                        .size(11.0).color(theme::INK_2));
                }
            });
        self.show_matrix = open;
        for (s, row, on) in toggles { self.matrix_toggle(s, row, on); }
    }

    /// Multi-rig overview strip (PLAN §13.1): always visible above the rack
    /// when >1 rig is loaded. Per rig: number-key hint, name, bound MIDI device,
    /// live level; the active rig gets the reference's tomato underline. Click
    /// or 1–9/←→ switches the BIG VIEW ONLY — every rig keeps sounding.
    fn rig_overview_panel(&mut self, root: &mut egui::Ui) {
        let n = self.rig_count();
        if n <= 1 { return; }
        let mut switch: Option<usize> = None;
        egui::Panel::top("rig_overview")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    theme::section_label(ui, "Rigs");
                    ui.add_space(6.0);
                    for rig in 0..n.min(9) {
                        let active = rig == self.active_rig;
                        let name = self.rig_name(rig);
                        let dev = self.rig_device_label(rig);
                        let level = self.rig_levels.get(rig).copied().unwrap_or(0.0);
                        let name_col = if active { theme::INK } else { theme::INK_2 };
                        let inner = egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(format!("{}", rig + 1))
                                        .monospace().size(11.0)
                                        .color(if active { theme::ACCENT_DEEP } else { theme::INK_2 }));
                                    ui.add_space(4.0);
                                    ui.label(egui::RichText::new(name).font(theme::medium_font(14.0)).color(name_col));
                                });
                                ui.horizontal(|ui| {
                                    theme::tracked_label(ui, &dev, 9.0, 0.14, theme::INK_2);
                                    ui.add_space(6.0);
                                    // Live level: flat teal bar on a hairline track.
                                    let (r, _) = ui.allocate_exact_size(egui::vec2(36.0, 3.0), egui::Sense::hover());
                                    let p = ui.painter();
                                    p.rect_filled(r, 1.0, theme::HAIRLINE);
                                    let w = 36.0 * level.clamp(0.0, 1.0);
                                    if w > 0.5 {
                                        p.rect_filled(Rect::from_min_size(r.min, egui::vec2(w, 3.0)), 1.0, theme::LIVE_TEAL);
                                    }
                                });
                            });
                        let resp = inner.response.interact(egui::Sense::click());
                        if active {
                            let r = inner.response.rect;
                            ui.painter().line_segment(
                                [Pos2::new(r.left() + 8.0, r.bottom() + 1.0), Pos2::new(r.right() - 8.0, r.bottom() + 1.0)],
                                Stroke::new(2.0, theme::ACCENT),
                            );
                        }
                        if resp.hovered() { ui.ctx().set_cursor_icon(CursorIcon::PointingHand); }
                        if resp.clicked() { switch = Some(rig); }
                        ui.add_space(4.0);
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("1–9 / ←→ switch view · all rigs keep sounding")
                        .size(11.0).color(theme::INK_2));
                });
            });
        if let Some(r) = switch { self.set_active_rig(r); }
    }

    /// AUM-style per-rig input selection: which sources feed each rig.
    /// Hurricane top strip (§4.14): paper, plain-text toggles, hairline nav.
    fn rig_inputs_panel(&mut self, root: &mut egui::Ui) {
        let rigs = self.clap_rigs();
        egui::Panel::top("rig_inputs")
            .default_size(40.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    theme::section_label(ui, "Rig inputs");
                    ui.add_space(10.0);
                    for (rig, node) in rigs {
                        // Rigs stay distinguishable by label, not hue (§3).
                        let label = if rig == 0 { "A" } else { "B" };
                        ui.label(egui::RichText::new(format!("Rig {label}")).size(13.0).strong().color(theme::INK));
                        ui.add_space(4.0);
                        let (k, l, h) = self.rig_sources[rig];
                        for (name, on, idx) in [("Keys", k, 0usize), ("Laptop", l, 1), ("HW", h, 2)] {
                            if text_toggle(ui, name, on) {
                                let s = &mut self.rig_sources[rig];
                                match idx {
                                    0 => s.0 = !s.0,
                                    1 => s.1 = !s.1,
                                    _ => s.2 = !s.2,
                                }
                                self.send_rig_source(node);
                            }
                            ui.add_space(6.0);
                        }
                        // Per-rig hardware device selector (AUM: a keyboard per rig).
                        if !self.hw_devices.is_empty() {
                            let cur = self.rig_device[rig];
                            let label = match cur {
                                None => "HW: All".to_string(),
                                Some(d) => format!("HW: {}", self.hw_devices.get(d as usize).map(|s| s.as_str()).unwrap_or("?")),
                            };
                            let resp = ui.add(egui::Label::new(
                                egui::RichText::new(label).size(13.0).color(theme::INK_2))
                                .sense(egui::Sense::click()))
                                .on_hover_text("cycle which hardware device feeds this rig");
                            if resp.hovered() { ui.ctx().set_cursor_icon(CursorIcon::PointingHand); }
                            if resp.clicked() {
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
                            ui.add_space(6.0);
                        }
                        // Editor is opened inline by double-clicking the synth
                        // unit in the rack (task-26) — no per-rig button here.
                        ui.add_space(4.0);
                    }
                    if nav_button(ui, "Split @ C4", self.split_on) {
                        self.split_on = !self.split_on;
                        let point = if self.split_on { Some(60) } else { None };
                        if let Some(bus) = &self.bus {
                            let _ = bus.lock().unwrap().send(Command::SetSplit { point });
                        }
                    }
                    ui.add_space(10.0);
                    if !self.editor_open && !self.editor_status.is_empty() {
                        ui.label(egui::RichText::new(&self.editor_status).size(12.0).color(theme::ACCENT_DEEP));
                        ui.add_space(8.0);
                    }
                    nav_separator(ui);
                    ui.add_space(6.0);
                    if nav_button(ui, "Sessions", self.show_sessions) {
                        self.show_sessions = !self.show_sessions;
                    }
                    ui.add_space(10.0);
                    nav_separator(ui);
                    ui.add_space(10.0);
                    if nav_button(ui, "Plugins", self.show_plugins) {
                        self.show_plugins = !self.show_plugins;
                        if self.show_plugins && !self.plugins_loaded {
                            self.refresh_plugins();
                        }
                    }
                    ui.add_space(10.0);
                    nav_separator(ui);
                    ui.add_space(10.0);
                    if nav_button(ui, "Transport", self.show_transport || self.playing) {
                        self.show_transport = !self.show_transport;
                    }
                    ui.add_space(10.0);
                    nav_separator(ui);
                    ui.add_space(10.0);
                    if nav_button(ui, "Matrix", self.show_matrix) {
                        self.show_matrix = !self.show_matrix;
                    }
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
                    // Play/Stop toggle (§4.13 rules, small version): idle =
                    // paper + ink ring; running = teal fill + ink glyph.
                    let btn = if self.playing {
                        egui::Button::new(egui::RichText::new("Stop").color(theme::INK).strong())
                            .fill(GREEN)
                            .stroke(Stroke::NONE)
                    } else {
                        egui::Button::new(egui::RichText::new("Play").color(theme::INK).strong())
                            .fill(theme::PAPER)
                            .stroke(Stroke::new(1.5, theme::INK))
                    };
                    if ui.add(btn.corner_radius(2.0).min_size(egui::vec2(56.0, 20.0)))
                        .on_hover_text("start / stop the metronome (Space)").clicked()
                    {
                        self.toggle_play();
                    }
                    ui.add_space(8.0);
                    theme::tracked_label(ui, "BPM", 10.0, 0.14, theme::INK_2);
                    ui.add_space(4.0);
                    if ui.small_button("−").on_hover_text("slower").clicked() {
                        self.set_bpm(self.bpm - 1.0);
                    }
                    // BPM as a big number (§4.18): Medium 24px ink.
                    let mut bpm = self.bpm;
                    let dv = egui::DragValue::new(&mut bpm)
                        .range(20.0..=300.0)
                        .speed(1.0)
                        .fixed_decimals(0);
                    let changed = ui.scope(|ui| {
                        let ts = ui.style().drag_value_text_style.clone();
                        ui.style_mut().text_styles.insert(ts, theme::medium_font(36.0));
                        ui.add(dv).on_hover_text("metronome tempo (20–300)").changed()
                    }).inner;
                    if changed {
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
                    theme::tracked_label(ui, if self.playing { "PLAYING" } else { "STOPPED" },
                        10.0, 0.14, theme::INK_2);
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
                theme::section_label(ui, "Starter templates");
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
                if ui.button("Multi-rig demo (2 rigs)")
                    .on_hover_text("two independent rigs, both sounding — switch the view with 1–9 / ←→ (F4)")
                    .clicked()
                {
                    self.load_multi_rig_demo();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.text_edit_singleline(&mut self.session_name);
                });
                if theme::primary_button(ui, "Save current patch").clicked() {
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
                theme::section_label(ui, "Saved sessions");
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
                    ui.label(egui::RichText::new("none yet — save one above").size(12.0).color(theme::INK_2));
                }
                // Service-list rows (§4.18): 1px hairline separators.
                for (i, name) in names.iter().enumerate() {
                    if i > 0 { ui.separator(); }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(name).size(13.0));
                        if theme::primary_button(ui, "Load").clicked() {
                            let path = crate::session::Session::package_path(&dir, name);
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
                    let col = if self.session_msg.contains("failed") { theme::CLIP_TEXT } else { theme::LIVE_TEAL_DEEP };
                    ui.label(egui::RichText::new(&self.session_msg).size(12.0).color(col));
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
                ui.label(egui::RichText::new("adding a plugin rebuilds the current patch").size(12.0).color(theme::INK_2));
                let filter = self.plugin_filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    theme::section_label(ui, &format!("LV2 plugins ({})", self.plugins_lv2.len()));
                    let mut added: Option<(String, String, bool)> = None;
                    let mut first_row = true;
                    for p in &self.plugins_lv2 {
                        if !filter.is_empty() && !p.name.to_lowercase().contains(&filter) { continue; }
                        if !first_row { ui.separator(); }
                        first_row = false;
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&p.name).size(13.0));
                            if p.is_instrument {
                                ui.label(egui::RichText::new("inst").size(11.0).color(theme::LIVE_TEAL_DEEP));
                            }
                            // Offer BOTH roles; the likely one is the primary
                            // action (the is_instrument hint is permissive).
                            let inst_resp = if p.is_instrument {
                                theme::primary_button(ui, "Add instrument")
                            } else {
                                ui.button("Add instrument")
                            };
                            if inst_resp.on_hover_text(&p.uri).clicked() {
                                added = Some((p.uri.clone(), p.name.clone(), true));
                            }
                            let fx_resp = if p.is_instrument {
                                ui.button("Add effect")
                            } else {
                                theme::primary_button(ui, "Add effect")
                            };
                            if fx_resp.on_hover_text(&p.uri).clicked() {
                                added = Some((p.uri.clone(), p.name.clone(), false));
                            }
                        });
                    }
                    if let Some((uri, name, as_inst)) = added {
                        if as_inst { self.add_lv2_instrument(&uri, &name); } else { self.add_lv2_effect(&uri, &name); }
                    }
                    ui.separator();
                    theme::section_label(ui, &format!("CLAP instruments ({})", self.plugins_clap.len()));
                    let mut add_surge = false;
                    let mut first_row = true;
                    for p in &self.plugins_clap {
                        if !filter.is_empty() && !p.name.to_lowercase().contains(&filter) { continue; }
                        if !first_row { ui.separator(); }
                        first_row = false;
                        let is_surge = p.id == SURGE_ID;
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&p.name).size(13.0));
                            if is_surge {
                                if theme::primary_button(ui, "New rig").clicked() { add_surge = true; }
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
                    let col = if self.plugin_msg.contains("failed") { theme::CLIP_TEXT } else { theme::LIVE_TEAL_DEEP };
                    ui.label(egui::RichText::new(&self.plugin_msg).size(12.0).color(col));
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
                    .fill(theme::PAPER)
                    .stroke(Stroke::new(1.0, theme::HAIRLINE))
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(root, |ui| {
                ui.heading(egui::RichText::new("Assistant").font(theme::medium_font(16.0)).color(theme::INK));
                ui.label(egui::RichText::new("DeepSeek · bring your own key").size(12.0).color(theme::INK_2));
                ui.add_space(6.0);
                if !self.assistant.key_set {
                    ui.label("Paste your DeepSeek API key:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.key_input)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    if theme::primary_button(ui, "Save key").clicked() && !self.key_input.trim().is_empty() {
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
                        .size(12.0)
                        .color(theme::INK_2),
                    );
                } else {
                    let avail = ui.available_height();
                    egui::ScrollArea::vertical()
                        .max_height((avail - 90.0).max(100.0))
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.assistant.history {
                                let (label, color) = match line.role.as_str() {
                                    "user" => ("You", theme::INK),
                                    "assistant" => ("Assistant", theme::LIVE_TEAL_DEEP),
                                    _ => ("Error", theme::CLIP_TEXT),
                                };
                                ui.label(egui::RichText::new(label).size(12.0).strong().color(color));
                                ui.label(&line.text);
                                ui.add_space(6.0);
                            }
                            if self.assistant.busy {
                                ui.label(egui::RichText::new("thinking…").size(12.0).color(theme::INK_2));
                            }
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        // The input is a recessed paper-sink field with a
                        // hairline frame — not a browser-default box.
                        let edit = egui::Frame::new()
                            .fill(theme::PAPER_SINK)
                            .stroke(Stroke::new(1.0, theme::HAIRLINE))
                            .corner_radius(2.0)
                            .inner_margin(egui::Margin::symmetric(6, 4))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.chat_input)
                                        .desired_width((ui.available_width() - 76.0).max(80.0))
                                        .hint_text("Ask or instruct…"),
                                )
                            }).inner;
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (theme::primary_button(ui, "Send").clicked() || enter)
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
                            .size(12.0)
                            .color(theme::INK_2),
                    );
                    ui.label(egui::RichText::new(&self.hw_status).size(12.0).color(theme::INK_2));
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
        // Per-rig activity for the overview strip: instant attack, slow release.
        let n_rigs = self.rig_count();
        if self.rig_levels.len() != n_rigs { self.rig_levels.resize(n_rigs, 0.0); }
        for r in 0..n_rigs {
            let target = self.units.iter().filter(|u| u.rig == r as u8).map(|u| u.led).fold(0.0f32, f32::max);
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
        let ft = if matches!(self.hover, Some(Hit::Flip)) { 1.0 } else { 0.0 };
        self.flip_hover_t += (ft - self.flip_hover_t) * ease;

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
        if (s - Pos2::new(40.0, self.toolbar_top + 44.0)).length() < 19.0 { return Hit::Play; }
        if self.flip_rect().contains(s) {
            return Hit::Flip;
        }
        if self.flipping || self.xf.sy < 0.9 { return Hit::Background; }
        let l = self.xf.inv(s);
        let lp = l.to_pos2();

        match self.side {
            Side::Front => {
                for (ui, u) in self.units.iter().enumerate() {
                    if !self.visible(ui) { continue; }
                    for ki in 0..u.knobs.len() {
                        if (l - self.knob_center(ui, ki)).length() < 23.0 {
                            return Hit::Knob { unit: ui, idx: ki };
                        }
                    }
                }
                // Keys keyboard (only when the active rig has a Keys unit).
                if self.keys_unit.map_or(false, |k| self.visible(k)) {
                    for wi in 0..8 {
                        if self.key_rect(wi).contains(lp) { return Hit::Key(WHITE_SEMI[wi]); }
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
                let mut best: Option<(usize, f32, f32)> = None;
                for (ci, c) in self.cables.iter().enumerate() {
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
        let mut load_multi = false;
        let mut rig_switch: Option<usize> = None;
        let mut rig_cycle: i8 = 0;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Space) { toggle_play = true; }
            if i.consume_key(Modifiers::NONE, Key::Tab) { flip = true; }
            if i.consume_key(Modifiers::NONE, Key::R) { reset_knobs = true; }
            if i.consume_key(Modifiers::NONE, Key::D) { self.show_stats = !self.show_stats; }
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
                self.show_plugins = !self.show_plugins;
                if self.show_plugins && !self.plugins_loaded { self.refresh_plugins(); }
            }
            // Transport / metronome popover (task-19).
            if i.consume_key(Modifiers::NONE, Key::F10) {
                self.show_transport = !self.show_transport;
            }
            // Hot-wire MIDI matrix (PLAN §13).
            if i.consume_key(Modifiers::NONE, Key::F11) {
                self.show_matrix = !self.show_matrix;
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
        if load_multi { self.load_multi_rig_demo(); }
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
        theme::paper_bg(&painter, rect);

        // The rack IS the reference's single inverted hero panel (§4.2):
        // flat near-black, radius 36, borderless, NO shadow (it sits on the
        // page; only floating sheets get shadows). Template loads fade in.
        let rack_local = Rect::from_min_size(Pos2::ZERO, Vec2::new(self.rack_w, self.rack_h));
        let rack = self.xf.rect(rack_local);
        let mut rp = painter.clone();
        rp.set_opacity(self.load_fade);
        rp.rect_filled(rack, 36.0, CHASSIS);

        if self.xf.sy > 0.04 {
            self.draw_rails(&rp);
            match self.side {
                Side::Front => self.draw_front(&rp),
                Side::Back => self.draw_back(&rp),
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

    fn draw_front(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            let r = self.unit_rects[ui];
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            // Single near-black family (§2.4): plates are the chassis color;
            // zones are drawn with 1px hairline rules that brighten on hover
            // (~120ms ease). Drag = 2px accent inset keyline.
            painter.rect_filled(sr, 2.0, CHASSIS);
            let ht = self.unit_hover.get(ui).copied().unwrap_or(0.0);
            if dragging {
                theme::inset_keyline(painter, sr, 2.0, theme::ACCENT, 2.0, 2.5);
            } else {
                // Faint default rule; hover eases to the accent (clear
                // interactive state without shadows — DeepSeek review fix).
                let rule = mix(BORDER, theme::ACCENT, ht);
                painter.rect_stroke(sr, 2.0, Stroke::new(1.0, alpha(rule, 0.3 + 0.7 * ht)), egui::StrokeKind::Inside);
            }

            // Label strip: 2px rule, accent when active/dragged (§4.3 — the
            // reference's active service row gets the orange rule).
            let active = u.led > 0.05 || dragging;
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(2.0, r.height() - 24.0));
            painter.rect_filled(xf.rect(strip), 1.0, if active { theme::ACCENT } else { BORDER });

            self.text(painter, r.min.to_vec2() + Vec2::new(34.0, 17.0), &u.name_g, TEXT);
            self.text_tracked(painter, r.min.to_vec2() + Vec2::new(34.0, 42.0), &u.sub_g, SUBTEXT);

            // Activity LED: flat teal disc (+ optional flat halo), accent
            // while dragged. No glow stack.
            let led = (r.right_top() + Vec2::new(-16.0, 16.0)).to_vec2();
            let led_col = if dragging { theme::ACCENT } else { GREEN };
            if u.led > 0.03 { self.ell(painter, led, 7.5, alpha(led_col, 0.15 * u.led), Stroke::NONE); }
            self.ell(painter, led, 3.2, alpha(led_col, 0.25 + 0.75 * u.led), Stroke::NONE);

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
        // Generic DSP bar (§4.9): METER_TRACK track, flat accent fill.
        let track = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w, 8.0));
        painter.rect_filled(self.xf.rect(track), 4.0, theme::METER_TRACK);
        let fill = Rect::from_min_size(Pos2::new(start, y - 4.0), Vec2::new(bar_w * (0.15 + 0.85 * level), 8.0));
        painter.rect_filled(self.xf.rect(fill), 4.0, alpha(theme::ACCENT, 0.5 + 0.5 * level));
    }

    fn draw_keyboard(&self, painter: &egui::Painter) {
        if self.key_w() < 8.0 { return; }
        let xf = &self.xf;
        // Flat keys (§4.7): cream whites with a 1px ink hairline gap, ink
        // blacks with a dark hairline; lit keys go solid accent. No gloss.
        for wi in 0..8 {
            let kr = self.key_rect(wi);
            let semi = WHITE_SEMI[wi];
            let lit = self.key_leds[semi];
            let hovered = matches!(self.hover, Some(Hit::Key(s)) if s == semi);
            let fill = if lit > 0.02 { theme::ACCENT }
                else if hovered { Color32::from_rgb(0xF0, 0xED, 0xE6) }
                else { WHITE };
            painter.rect_filled(xf.rect(kr), 2.0, fill);
            painter.rect_stroke(xf.rect(kr), 2.0, Stroke::new(1.0, BLACK), egui::StrokeKind::Inside);
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
            let fill = if lit > 0.02 { theme::ACCENT } else { BLACK };
            painter.rect_filled(xf.rect(kr), 1.5, fill);
            painter.rect_stroke(xf.rect(kr), 1.5, Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
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
            // Alternate cream / dim cream, flat (§4.9).
            let col = if i % 2 == 0 { TEXT } else { SUBTEXT };
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
            self.ell(painter, c, 6.0 - i as f32 * 1.1, alpha(theme::ACCENT, a), Stroke::NONE);
        }
        if led > 0.05 {
            let c = Vec2::new(start + pitch * 0.5, r.center().y + 16.0);
            self.ell(painter, c, 10.0 + (1.0 - led) * 14.0, alpha(theme::ACCENT, 0.25 * led), Stroke::NONE);
        }
    }

    /// Vertical LED meter on the tall MASTER unit (bottom = quiet).
    /// Stepped cream→tomato→red (§4.8) with a slow-release peak-hold tick.
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
            let fill = if i < lit { col } else { theme::METER_TRACK };
            painter.rect_filled(self.xf.rect(seg), 1.0, fill);
        }
        // Peak-hold tick: 2px, accent — clip red once the peak hits ≥ 0.9.
        if self.meter_peak > 0.02 {
            let i = ((self.meter_peak * n as f32).ceil() as usize).clamp(1, n) - 1;
            let tick = Rect::from_min_size(
                Pos2::new(x, bottom - (i + 1) as f32 * pitch - 2.0),
                Vec2::new(22.0, 2.0),
            );
            let col = if self.meter_peak >= 0.9 { theme::CLIP_RED } else { theme::ACCENT };
            painter.rect_filled(self.xf.rect(tick), 0.8, col);
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

        // Unfilled disc (§4.6): 1.5px hairline ring, cream on hover. No fill
        // bead, no gradient, no hover halo.
        self.ell(painter, c, r, Color32::TRANSPARENT,
            Stroke::new(1.5, if hovered || dragging { TEXT } else { BORDER }));

        // Flat value arc: hairline track + solid accent arc + flat end dot
        // (theme helper; the closure carries the flip transform).
        theme::value_arc(painter, |l| xf.p(l), c, r, k.value, theme::ACCENT);

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
        self.text_tracked(painter, c + Vec2::new(-lw / 2.0, r + 10.0), &k.label_g, SUBTEXT);

        // Drag chip: a paper sticker with the value in Medium 13px ink.
        if dragging {
            let g = painter.layout(format!("{:.2}", k.value), theme::medium_font(13.0), theme::INK, f32::INFINITY);
            let chip = Rect::from_min_size(
                xf.p(c + Vec2::new(-g.size().x / 2.0 - 6.0, -r - 28.0)),
                g.size() + Vec2::new(12.0, 6.0),
            );
            theme::drop_shadow(painter, chip, 6.0);
            painter.rect_filled(chip, 6.0, theme::PAPER);
            painter.galley(chip.min + Vec2::new(6.0, 3.0), g, theme::INK);
        }
    }

    fn draw_back(&self, painter: &egui::Painter) {
        let xf = &self.xf;
        for (ui, u) in self.units.iter().enumerate() {
            if !self.visible(ui) { continue; }
            let r = self.unit_rects[ui];
            let dragging = matches!(self.drag, Drag::Unit { unit: i, .. } if i == ui);
            let sr = xf.rect(r);
            // Same flat plate treatment as the front (§4.4).
            painter.rect_filled(sr, 2.0, CHASSIS);
            let ht = self.unit_hover.get(ui).copied().unwrap_or(0.0);
            if dragging {
                theme::inset_keyline(painter, sr, 2.0, theme::ACCENT, 2.0, 2.5);
            } else {
                // Faint default rule; hover eases to the accent (clear
                // interactive state without shadows — DeepSeek review fix).
                let rule = mix(BORDER, theme::ACCENT, ht);
                painter.rect_stroke(sr, 2.0, Stroke::new(1.0, alpha(rule, 0.3 + 0.7 * ht)), egui::StrokeKind::Inside);
            }
            let active = u.led > 0.05 || dragging;
            let strip = Rect::from_min_size(r.min + Vec2::new(12.0, 12.0), Vec2::new(2.0, r.height() - 24.0));
            painter.rect_filled(xf.rect(strip), 1.0, if active { theme::ACCENT } else { BORDER });
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
            for (ji, j) in u.jacks.iter().enumerate() {
                let c = self.jack_pos(ui, ji);
                let jw = j.label_g.as_ref().map_or(0.0, |g| g.size.x);
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
        if c.kind == PortKind::Audio {
            // Subtle flat warm shift toward the accent (§4.10): TONE·0.20 +
            // WARM·0.15 — keeps the knob feedback without a rainbow.
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
            .map(|m| 0.15 + knob_val(&self.units[m], P_VOL, 0.8) * 0.85)
            .unwrap_or(0.83);
        for (ci, c) in self.cables.iter().enumerate() {
            if !self.visible(c.src) || !self.visible(c.dst) { continue; }
            let a = Self::cable_alpha(c, has_sel, hl[ci]);
            if a <= 0.01 { continue; }
            let hovered = matches!(self.hover, Some(Hit::Cable(i, _)) if i == ci);
            let grabbed = matches!(self.drag, Drag::Cable { ci: i, .. } if i == ci);
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

            // One flat stroke per cable, round caps (§4.10). Selection = the
            // accent color swap + dimming the rest (cable_alpha) — no glow,
            // no centerline.
            if has_sel && hl[ci] {
                col = theme::ACCENT;
                width += 0.5;
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
                painter.rect_filled(xf.rect(body), pr, alpha(PLUG, a));
                painter.rect_stroke(xf.rect(body), pr, Stroke::new(1.0, alpha(BORDER, a)), egui::StrokeKind::Inside);
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

    /// The flip pill rect (§4.13): right-anchored at the masthead's right edge,
    /// width follows the measured label. Shared by draw_toolbar and hit_test.
    fn flip_rect(&self) -> Rect {
        Rect::from_min_size(Pos2::new(430.0 - self.flip_w, self.toolbar_top + 28.0), Vec2::new(self.flip_w, 32.0))
    }

    fn draw_toolbar(&self, painter: &egui::Painter, top: f32) {
        // Open masthead (§2.4: structure with rules, not boxes): the wordmark
        // sits directly on the paper — the reference's display moment. Only
        // the play circle and the single flip pill are objects here.
        let c = Pos2::new(40.0, top + 44.0);
        let ht = self.play_hover_t;
        let ink = BLACK;
        if self.playing {
            painter.circle_filled(c, 15.0, mix(GREEN, BLACK, 0.08 * ht));
        } else {
            painter.circle_filled(c, 15.0, mix(theme::PAPER, theme::PAPER_SINK, ht));
            painter.circle_stroke(c, 15.0, Stroke::new(1.5, ink));
        }
        if self.playing {
            // Stop square (transport is Play/Stop, not play/pause).
            painter.rect_filled(Rect::from_min_size(Pos2::new(34.5, top + 38.5), Vec2::new(11.0, 11.0)), 1.6, ink);
        } else {
            painter.add(Shape::convex_polygon(
                vec![Pos2::new(36.5, top + 36.5), Pos2::new(36.5, top + 51.5), Pos2::new(48.5, top + 44.0)],
                ink, Stroke::NONE));
        }

        // Beat dot: teal thump, no glow ring.
        let beat = 60.0 / self.bpm.max(1.0) as f64;
        let beat_phase = ((self.time / beat).fract() as f32).clamp(0.0, 1.0);
        let thump = if self.playing { (1.0 - beat_phase).powi(3) } else { 0.0 };
        painter.circle_filled(Pos2::new(66.0, top + 44.0), 3.0 + 3.5 * thump, alpha(GREEN, 0.35 + 0.65 * thump));

        // Asterisk + display wordmark — the editorial display moment.
        theme::asterisk(painter, Pos2::new(80.0, top + 33.0), 9.0, theme::ACCENT, 2.0);
        if let Some(g) = &self.wordmark_g {
            theme::paint_tracked(painter, Pos2::new(94.0, top + 18.0), g, theme::INK);
        }
        // State line: tracked micro-caps; a 6px teal dot precedes "playing".
        let state_g = if self.playing { &self.state_play_g } else { &self.state_pause_g };
        let state_x = if self.playing {
            painter.circle_filled(Pos2::new(97.0, top + 57.0), 3.0, GREEN);
            104.0
        } else { 94.0 };
        if let Some(g) = state_g {
            theme::paint_tracked(painter, Pos2::new(state_x, top + 52.0), g, theme::INK_2);
        }

        // Flip button = the app's single pill (§4.13): vivid accent fill,
        // inset cream keyline, Bold 19px cream label (WCAG large text).
        let fb = self.flip_rect();
        let fh = self.flip_hover_t;
        painter.rect_filled(fb, fb.height() / 2.0, mix(theme::ACCENT, BLACK, 0.08 * fh));
        theme::inset_keyline(painter, fb, fb.height() / 2.0, alpha(theme::ON_ACCENT, 0.9), 1.5, 2.5);
        if let Some(g) = &self.flip_label_g {
            let pos = fb.center() - g.size() * 0.5;
            painter.galley(pos, g.clone(), WHITE);
        }
    }

    fn draw_hint(&self, painter: &egui::Painter, rect: Rect) {
        // Plain caption line on paper (§4.15) — no pill container.
        let g = if self.pending.is_some() { &self.hint_wire_g }
            else if self.side == Side::Front { &self.hint_front_g }
            else { &self.hint_back_g };
        let Some(g) = g else { return };
        painter.galley(Pos2::new(16.0, rect.bottom() - 24.0), g.clone(), theme::INK_2);
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
            // Telemetry is the only mono left in the app (§4.15).
            self.stats_g = Some(painter.layout(text, FontId::monospace(11.0), theme::INK_2, f32::INFINITY));
        }
        if let Some(g) = &self.stats_g {
            let w = g.size().x + 24.0;
            let bar = Rect::from_min_size(Pos2::new(rect.right() - w - 16.0, 16.0), Vec2::new(w, 26.0));
            painter.rect_filled(bar, 2.0, theme::PAPER_SINK);
            painter.rect_stroke(bar, 2.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
            painter.galley(bar.min + Vec2::new(12.0, 6.0), g.clone(), theme::INK_2);
        }
    }

    fn draw_help(&self, painter: &egui::Painter, rect: Rect) {
        // Help modal (§4.16): ink scrim over paper, a second appearance of
        // the hero-panel motif — flat near-black card, radius 28, no border.
        painter.rect_filled(rect, 0.0, alpha(theme::SCRIM, 0.62));
        let card = Rect::from_center_size(rect.center(), Vec2::new(560.0, 352.0));
        // Flat dark card (hero-panel motif), no shadow — the scrim separates it.
        painter.rect_filled(card, 28.0, theme::DARK_PANEL);
        theme::asterisk(painter, card.min + Vec2::new(34.0, 36.0), 7.0, theme::ACCENT, 2.0);
        if let Some(g) = &self.help_title_g {
            theme::paint_tracked(painter, card.min + Vec2::new(48.0, 22.0), g, TEXT);
        }
        if let Some(g) = &self.help_body_g { painter.galley(card.min + Vec2::new(28.0, 58.0), g.clone(), TEXT); }
        if let Some(g) = &self.help_foot_g {
            theme::paint_tracked(painter, card.min + Vec2::new(28.0, 326.0), g, SUBTEXT);
        }
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
        self.rig_overview_panel(ui);
        self.assistant_panel_ui(ui);

        // Native rig editor (task-26): handle open/close requests from unit
        // double-clicks. No X11 — the editor is a plain egui window drawn below.
        if let Some(rig) = self.editor_request.take() {
            self.toggle_param_editor(rig);
        }

        // Laptop keys (disabled while the assistant text field has focus).
        // Losing keyboard focus drains held keys so note-offs can't orphan
        // (vibez lesson, docs/vibez-notes.md). held_keys are laptop keys, so
        // the drain fires under the LaptopKeys source regardless of the last
        // hardware event.
        let kb_focus = !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.focused);
        if !kb_focus && self.had_kb_focus && !self.held_keys.is_empty() {
            self.current_source = SourceId::LaptopKeys;
            let held: Vec<_> = self.held_keys.drain(..).collect();
            for (_, semi) in held { self.fire_keys(semi, 0.0); }
        }
        self.had_kb_focus = kb_focus;
        if !ctx.egui_wants_keyboard_input() {
            let events = ctx.input(|i| i.events.clone());
            for ev in events {
                if let egui::Event::Key { key, pressed, repeat, .. } = ev {
                    self.handle_laptop_key(key, pressed, repeat);
                }
            }
        }
        self.poll_assistant();
        self.sync_from_engine();

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
            if let SourceId::Hardware(d) = ev.source {
                if let Some(l) = self.hw_leds.get_mut(d as usize) { *l = 1.0; }
            }
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
        self.matrix_window(&ctx);
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
fn build_audio() -> anyhow::Result<(Vec<ClapVoiceMain>, Vec<VoiceController>, cpal::Stream, CommandBus, Arc<std::sync::atomic::AtomicU32>, f64, crate::player::PlayerControl, Arc<Mutex<crate::control::RigSnapshot>>, Arc<Mutex<crate::engine::GraphPub>>)> {
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
    // Graph publication (MCP→UI live sync): the engine publishes the graph on
    // structural changes; the UI adopts it when it diverges from its mirror.
    let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 0, graph: Graph::demo_patch() }));
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
    Ok((vec![main_a, main_b], controllers, stream, bus, level, sample_rate, player_ctrl, rig_snapshot, graph_pub))
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
                    Ok((mains, controllers, stream, bus, level, sample_rate, player_ctrl, rig_snapshot, graph_pub)) => {
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
                        audio = Some((mains, controllers, stream, bus, level, shared_graph, player_ctrl, rig_snapshot, graph_pub));
                    }
                    Err(e) => eprintln!("Surge/audio unavailable, running silent: {e}"),
                }
            }
            let mut app = DemoApp::new_with_sink(sink);
            let app = match audio {
                Some((mains, controllers, stream, bus, level, shared_graph, player_ctrl, rig_snapshot, graph_pub)) => {
                    app.shared_graph = shared_graph;
                    app.rig_snapshot = rig_snapshot;
                    app.graph_pub = Some(graph_pub);
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

// ---------------------------------------------------------------------------
// Multi-rig UI tests (PLAN §13.1): view switching is visual only — the merged
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
        app.load_demo_patch(); // build_rack resets the fade
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
        app.current_source = SourceId::LaptopKeys;
        let held: Vec<_> = app.held_keys.drain(..).collect();
        for (_, semi) in held { app.fire_keys(semi, 0.0); }
        assert!(app.held_keys.is_empty());
    }
}

#[cfg(test)]
mod matrix_ui_tests {
    use super::*;

    #[test]
    fn clap_cell_state_table() {
        let all = (true, true, true);
        // Cable gate dominates everything.
        for row in [MatrixRow::Keys, MatrixRow::Laptop, MatrixRow::HwAll, MatrixRow::HwDev(0)] {
            assert!(!clap_cell(all, None, false, row));
        }
        // Defaults: keys/laptop/omni-hw on, specific device off.
        assert!(clap_cell(all, None, true, MatrixRow::Keys));
        assert!(clap_cell(all, None, true, MatrixRow::Laptop));
        assert!(clap_cell(all, None, true, MatrixRow::HwAll));
        assert!(!clap_cell(all, None, true, MatrixRow::HwDev(0)));
        // Device-bound rig: only that device.
        assert!(!clap_cell(all, Some(1), true, MatrixRow::HwAll));
        assert!(clap_cell(all, Some(1), true, MatrixRow::HwDev(1)));
        assert!(!clap_cell(all, Some(1), true, MatrixRow::HwDev(0)));
        // Hardware off.
        assert!(!clap_cell((true, true, false), None, true, MatrixRow::HwAll));
        assert!(!clap_cell((true, true, false), Some(0), true, MatrixRow::HwDev(0)));
        // Source flags independent.
        assert!(!clap_cell((false, true, true), None, true, MatrixRow::Keys));
        assert!(clap_cell((false, true, true), None, true, MatrixRow::Laptop));
    }

    fn demo_app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        app
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
        assert!(app.rig_sources[0].2);
        // Switch to omni.
        app.matrix_toggle(inst, MatrixRow::HwAll, true);
        assert_eq!(app.rig_device[0], None);
        assert!(app.rig_sources[0].2);
        // Bind device 1, then click it off → hardware off.
        app.matrix_toggle(inst, MatrixRow::HwDev(1), true);
        assert_eq!(app.rig_device[0], Some(1));
        app.matrix_toggle(inst, MatrixRow::HwDev(1), false);
        assert!(!app.rig_sources[0].2);
        // Keys/laptop untouched by the hw radio.
        assert!(app.rig_sources[0].0 && app.rig_sources[0].1);
    }

    #[test]
    fn sync_adopts_remote_graph_and_skips_converged() {
        let mut app = DemoApp::new_with_sink(None);
        let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub { rev: 1, graph: app.base_graph.clone() }));
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
    fn get_matrix_tool_reports_state() {
        let mut app = demo_app();
        let out = app.exec_tool("get_matrix", &serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sinks"].as_array().unwrap().len(), 2);
        assert_eq!(v["sinks"][0]["keys"], serde_json::json!(true));
        // Hot-wire a cell through the assistant tool.
        let node = v["sinks"][1]["node"].as_u64().unwrap();
        let out = app.exec_tool("set_matrix_cell", &serde_json::json!({"node": node, "source": "laptop", "on": false}));
        assert!(out.contains("= false"));
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_matrix", &serde_json::json!({}))).unwrap();
        assert_eq!(v["sinks"][1]["laptop"], serde_json::json!(false));
    }
}