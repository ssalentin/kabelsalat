//! Hurricane design language (docs/design-reference-analysis.md) — editorial,
//! flat, hairline-structured. **All-dark chrome** (user pivot 2026-11-15):
//! the whole app lives on warm near-black surfaces; the rack is a slightly
//! lighter hero panel. One tomato accent for CTA/selection only.
//!
//! Central palette + painter helpers. Everything here is paint-only: no audio
//! state, no locks, no I/O on the frame path (font files are read once at
//! startup). Flat fills, 1px hairlines, one tomato accent — no gradients, no
//! glass, no glows. The only shadow is a soft ink one under floating sheets.
//!
//! Semantics: audio = cream (round terminals) · MIDI = blue (square) ·
//! selection/CTA = tomato · active/playing = teal · clip/danger = red.
//! Cable jackets: six-color CABLE_PALETTE (shape separates audio/MIDI).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui::{
    self, Color32, FontFamily, FontId, Pos2, Rect, Stroke, Vec2,
};

// ---------------------------------------------------------------------------
// Palette — Hurricane structure, all-dark surfaces (user pivot 2026-11-15).
// The legacy chrome names (PAPER/INK/…) are kept so call sites stay stable;
// their VALUES are now the dark side. Rack tokens (DARK_PANEL/CREAM/…) are
// unchanged — the rack reads one surface step above the window base.
// ---------------------------------------------------------------------------

// Chrome surfaces & text (dark side).
pub const PAPER: Color32 = Color32::from_rgb(0x15, 0x14, 0x12); // window base / panels (warm near-black)
pub const PAPER_SINK: Color32 = Color32::from_rgb(0x10, 0x0F, 0x0E); // recessed fields, chips
pub const INK: Color32 = Color32::from_rgb(0xEF, 0xEE, 0xE9); // primary text (= CREAM)
pub const INK_2: Color32 = Color32::from_rgb(0xB4, 0xB4, 0xB0); // secondary text (= CREAM_MID)
pub const HAIRLINE: Color32 = Color32::from_rgb(0x3A, 0x38, 0x35); // 1px rules on dark chrome
pub const MUTED: Color32 = Color32::from_rgb(0x6E, 0x6A, 0x66); // decorative / hover rules

// The inverted hero panel (dark side).
pub const DARK_PANEL: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C); // rack chassis, help card
pub const DARK_PANEL_HI: Color32 = Color32::from_rgb(0x23, 0x21, 0x20); // plates, rails, plugs
pub const SCRIM: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C); // help backdrop (α 0.62)
pub const CREAM: Color32 = Color32::from_rgb(0xEF, 0xEE, 0xE9); // primary text on dark
pub const CREAM_MID: Color32 = Color32::from_rgb(0xB4, 0xB4, 0xB0); // rack micro-labels (crisp on near-black)
/// Engaged chip fill: a slightly warmed cream so ON chips read as part of the
/// dark language, not OS-white (DARK_PANEL text on it ≈ 13:1).
pub const CHIP_ON: Color32 = Color32::from_rgb(0xE6, 0xE4, 0xDC);
pub const CREAM_DIM: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x8C); // secondary text on dark
pub const HAIRLINE_DARK: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x8C); // 1px rules on dark
pub const METER_TRACK: Color32 = Color32::from_rgb(0x35, 0x35, 0x35); // unlit meter segments

// The one hot accent + its text-safe variants (§5 contrast table).
pub const ACCENT: Color32 = Color32::from_rgb(0xEF, 0x48, 0x25); // graphics / fills / large text
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0xC0, 0x39, 0x1B); // small filled buttons, warn text on paper
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0xF1, 0x55, 0x2E); // small orange text on dark
pub const ON_ACCENT: Color32 = Color32::from_rgb(0xFF, 0xFB, 0xFA); // text/glyphs on accent fills

// Semantic expansion, sampled from the reference imagery (§3).
pub const AUDIO_CREAM: Color32 = Color32::from_rgb(0xEF, 0xEE, 0xE9); // audio cables/jacks
pub const MIDI_BLUE: Color32 = Color32::from_rgb(0x5B, 0x8F, 0xD0); // MIDI on dark
pub const MIDI_BLUE_DEEP: Color32 = Color32::from_rgb(0x1C, 0x5A, 0x8B); // MIDI on paper / links
pub const LIVE_TEAL: Color32 = Color32::from_rgb(0x2E, 0x9E, 0x9B); // active / playing on dark
pub const LIVE_TEAL_DEEP: Color32 = Color32::from_rgb(0x1F, 0x7A, 0x70); // active text on paper
pub const CLIP_RED: Color32 = Color32::from_rgb(0xD9, 0x25, 0x1C); // clip graphics
pub const CLIP_TEXT: Color32 = Color32::from_rgb(0xB9, 0x1C, 0x14); // danger text on paper
#[allow(dead_code)] // reserved token (§3.1): danger text on dark, no call site yet
pub const CLIP_BRIGHT: Color32 = Color32::from_rgb(0xF0, 0x55, 0x4A); // danger text on dark

// Cable-salad palette (user request 2026-11-15): harmonious jacket colors for
// the back panel, split by family so the audio≠MIDI hue semantics survive:
// audio = warm/neutral jackets, MIDI = cool jackets. Terminal shape (round vs
// square) remains the primary separator; selection is the one tomato swap.
// All ≥ 5.9:1 on the near-black hero. CABLE_CYAN is deliberately cooler than
// LIVE_TEAL so a jacket never reads as "active/playing".
pub const CABLE_AMBER: Color32 = Color32::from_rgb(0xE2, 0xA6, 0x4E);
pub const CABLE_ROSE: Color32 = Color32::from_rgb(0xD9, 0x8B, 0xA0);
/// Warm sand (replaced a sage green: audio jackets stay warm/neutral, rule 3).
pub const CABLE_SAND: Color32 = Color32::from_rgb(0xCF, 0xB8, 0x96);
pub const CABLE_CYAN: Color32 = Color32::from_rgb(0x4F, 0xB0, 0xC6);
pub const CABLE_VIOLET: Color32 = Color32::from_rgb(0x9C, 0x8F, 0xD0);
pub const CABLE_AUDIO_PALETTE: [Color32; 4] = [AUDIO_CREAM, CABLE_AMBER, CABLE_ROSE, CABLE_SAND];
pub const CABLE_MIDI_PALETTE: [Color32; 3] = [MIDI_BLUE, CABLE_CYAN, CABLE_VIOLET];

// ---------------------------------------------------------------------------
// Color helpers (no allocation)
// ---------------------------------------------------------------------------

#[inline]
pub fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

#[inline]
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, _] = a.to_array();
    let [br, bg, bb, _] = b.to_array();
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(ar, br), l(ag, bg), l(ab, bb))
}

// ---------------------------------------------------------------------------
// Depth & atmosphere — flat fills, hairlines, one soft shadow (§2.4 / §4.1)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Brand mark — crossed patch cables (logo redesign 2026-11-15)
// ---------------------------------------------------------------------------
//
// One geometry, two renderers: `crossed_wires` (egui painter) and
// assets/logo.svg. Every proportion below is in units of `h` = half the CABLE
// box (cable endpoints sit at center ± (h, h)); the plugs extend outward
// beyond it along the 45° diagonals. SVG mapping: 512×512 tile, glyph box
// 392 (60 px padding each side) → h = 196 / MARK_EXT_FULL ≈ 120.8, so e.g.
// the over-cable is `M 135.2 135.2 C 231.8 328.5, 280.2 183.5, 376.8 376.8`.
// `mark_geometry_matches_logo_svg` (tests below) pins the two together.

/// Inner Bézier control offsets (x, y)·h — the S-curved sag. Both cables
/// pass exactly through the center at t = 0.5.
const MARK_CTRL: (f32, f32) = (0.2, 0.6);
const MARK_CABLE_W: f32 = 0.20;
/// Strain-relief boot: length along the plug axis, width.
const MARK_BOOT_LEN: f32 = 0.20;
const MARK_BOOT_W: f32 = 0.28;
/// Plug housing (dark rounded rect): length along the axis, width.
const MARK_HOUS_LEN: f32 = 0.34;
const MARK_HOUS_W: f32 = 0.34;
/// Tip ring + jack socket, centered MARK_TIP_D out along the diagonal.
const MARK_TIP_D: f32 = 0.64;
const MARK_TIP_R: f32 = 0.085;
const MARK_TIP_STROKE: f32 = 0.045;
const MARK_SOCK_HALF: f32 = 0.17;
const MARK_SOCK_RAD: f32 = 0.05;
/// Weave gap on each side of the over-cable, over t ∈ MARK_GAP_T.
const MARK_GAP: f32 = 0.07;
const MARK_GAP_T: std::ops::Range<f32> = 0.28..0.72;
/// Flat sheen line: width and offset toward the top edge, × cable width.
const MARK_SHEEN_W: f32 = 0.25;
const MARK_SHEEN_OFF: f32 = 0.19;
const MARK_PULSE_R: f32 = 0.12;
const MARK_RING_R: f32 = 0.24;
/// Below this glyph size the mark drops its fine detail (see crossed_wires).
const MARK_DETAIL_MIN: f32 = 32.0;
const MARK_SOCKET: Color32 = PAPER_SINK;

const DIAG: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// Glyph half-extent in h: cable box + plug run out to the socket's edge.
const MARK_EXT_FULL: f32 = 1.0 + MARK_TIP_D * DIAG + MARK_SOCK_HALF;
/// Small mode is the bare woven X: the cable ends (plus half a stroke)
/// bound the glyph.
const MARK_EXT_SMALL: f32 = 1.0 + MARK_CABLE_W * 0.5;

/// Pure geometry of the mark (shared by the painter and the SVG parity test).
struct MarkGeometry {
    h: f32,
    /// Tomato, top-left → bottom-right.
    over: [Pos2; 4],
    /// MIDI blue, bottom-left → top-right (left → right, like `over`, so
    /// the top-edge normal of both is (dy, −dx)).
    under: [Pos2; 4],
}

fn mark_geometry(center: Pos2, size: f32, detailed: bool) -> MarkGeometry {
    let ext = if detailed { MARK_EXT_FULL } else { MARK_EXT_SMALL };
    let h = size * 0.5 / ext;
    let (cx, cy) = MARK_CTRL;
    let at = |x: f32, y: f32| center + Vec2::new(x * h, y * h);
    MarkGeometry {
        h,
        over: [at(-1.0, -1.0), at(-cx, cy), at(cx, -cy), at(1.0, 1.0)],
        under: [at(-1.0, 1.0), at(-cx, -cy), at(cx, cy), at(1.0, -1.0)],
    }
}

/// Tiller–Hanson offset of a cubic: shift each control-polygon leg by `d`
/// along its (dy, −dx) normal and re-intersect. Good for the mark's gentle S.
fn offset_cubic(p: [Pos2; 4], d: f32) -> [Pos2; 4] {
    let leg = |a: Pos2, b: Pos2| {
        let t = (b - a).normalized();
        let n = Vec2::new(t.y, -t.x) * d;
        (a + n, b + n)
    };
    let l = [leg(p[0], p[1]), leg(p[1], p[2]), leg(p[2], p[3])];
    let meet = |(a, b): (Pos2, Pos2), (c, e): (Pos2, Pos2)| {
        let (r, s) = (b - a, e - c);
        let den = r.x * s.y - r.y * s.x;
        if den.abs() < 1e-6 { return b; }
        a + r * (((c - a).x * s.y - (c - a).y * s.x) / den)
    };
    [l[0].0, meet(l[0], l[1]), meet(l[1], l[2]), l[2].1]
}

/// Color treatment of [`crossed_wires`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkStyle {
    /// Full brand colors: tomato over MIDI blue, cream pulse (logo, icon,
    /// help card).
    Brand,
    /// Understated chrome treatment (masthead): cream over dim cream, the
    /// only color a tiny tomato pulse.
    Mono,
}

/// The brand mark (logo redesign 2026-11-15): two patch cables in a real
/// weave — over-cable on top of the under-cable with a `bg`-colored gap cut
/// where they cross — and a signal pulse at the crossing.
///
/// `size` is the whole glyph's square bounding box; `bg` is the surface under
/// the mark so the weave gap matches it.
///
/// Scale discipline: at `size >= MARK_DETAIL_MIN` (32 px) the craft is drawn
/// — each end a patch plug (jacket-colored two-step boot, dark housing with
/// hairline, cream tip ring in a jack socket), a flat sheen line per jacket,
/// one hairline pulse ring. Below that the mark is the bare woven X + pulse
/// (hardware would be sub-pixel mush): the cable box fills the freed space,
/// and stroke/gap widths get pixel floors so the weave still reads at 16 px.
pub fn crossed_wires(painter: &egui::Painter, center: Pos2, size: f32, bg: Color32, style: MarkStyle) {
    use egui::epaint::CubicBezierShape;
    let detailed = size >= MARK_DETAIL_MIN;
    let g = mark_geometry(center, size, detailed);
    let h = g.h;
    let hair = 1.0;
    let (over_c, under_c, pulse_c) = match style {
        MarkStyle::Brand => (ACCENT, MIDI_BLUE, ON_ACCENT),
        MarkStyle::Mono => (INK, CREAM_DIM, ACCENT),
    };
    let w = (MARK_CABLE_W * h).max(1.5);
    let gap = (MARK_GAP * h).max(if detailed { 0.0 } else { 1.0 });
    let bez = |p: [Pos2; 4], width: f32, col: Color32| {
        egui::Shape::CubicBezier(CubicBezierShape::from_points_stroke(
            p, false, Color32::TRANSPARENT, Stroke::new(width, col),
        ))
    };
    let sheen = |p: [Pos2; 4], jacket: Color32| {
        bez(offset_cubic(p, MARK_SHEEN_OFF * w), MARK_SHEEN_W * w, mix(jacket, CREAM, 0.35))
    };

    // Under-cable, then the weave gap (a bg stroke hugging the over-cable
    // through the crossing), then the over-cable on top.
    painter.add(bez(g.under, w, under_c));
    if detailed { painter.add(sheen(g.under, under_c)); }
    let over = CubicBezierShape::from_points_stroke(g.over, false, Color32::TRANSPARENT, Stroke::new(w + 2.0 * gap, bg));
    painter.add(egui::Shape::CubicBezier(over.split_range(MARK_GAP_T)));
    painter.add(bez(g.over, w, over_c));
    if detailed { painter.add(sheen(g.over, over_c)); }

    if detailed {
        for (end, jacket) in [(g.under[0], under_c), (g.under[3], under_c), (g.over[0], over_c), (g.over[3], over_c)] {
            mark_plug(painter, center, end, h, jacket, hair);
        }
        // One flat hairline ring around the pulse — no glow.
        painter.circle_stroke(center, MARK_RING_R * h, Stroke::new(hair, alpha(pulse_c, 0.35)));
    }
    painter.circle_filled(center, (MARK_PULSE_R * h).max(1.5), pulse_c);
}

/// One patch plug at cable end `end`, pointing outward along the diagonal:
/// jack socket, rounded housing, two-step boot, cream tip ring.
fn mark_plug(painter: &egui::Painter, center: Pos2, end: Pos2, h: f32, jacket: Color32, hair: f32) {
    let u = Vec2::new((end.x - center.x).signum(), (end.y - center.y).signum()) * DIAG;
    let n = Vec2::new(-u.y, u.x);
    let at = |d: f32| end + u * (d * h);
    let tip = at(MARK_TIP_D);
    let sock = Rect::from_center_size(tip, Vec2::splat(2.0 * MARK_SOCK_HALF * h));
    painter.rect_filled(sock, MARK_SOCK_RAD * h, MARK_SOCKET);
    painter.rect_stroke(sock, MARK_SOCK_RAD * h, Stroke::new(hair, HAIRLINE_DARK), egui::StrokeKind::Middle);
    // Housing: rotated rounded rect, corner radius 22% of its width.
    let hw = MARK_HOUS_W * h;
    let mid = at(MARK_BOOT_LEN + MARK_HOUS_LEN * 0.5);
    let (ax, ay) = (u * (MARK_HOUS_LEN * h * 0.5), n * (hw * 0.5));
    let rad = hw * 0.22;
    let mut pts = Vec::with_capacity(16);
    for (sx, sy) in [(1.0f32, 1.0f32), (-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)] {
        let corner = mid + ax * sx + ay * sy;
        let c = corner - u * (rad * sx) - n * (rad * sy); // arc center
        let a0 = (corner - c).angle() - std::f32::consts::FRAC_PI_4;
        for k in 0..4 {
            pts.push(c + Vec2::angled(a0 + std::f32::consts::FRAC_PI_2 * k as f32 / 3.0) * rad);
        }
    }
    painter.add(egui::Shape::convex_polygon(pts, DARK_PANEL_HI, Stroke::new(hair, HAIRLINE_DARK)));
    // Boot: straight along the plug axis, two-step strain relief; the round
    // inner cap hides the cable's entry angle.
    let bw = MARK_BOOT_W * h;
    painter.line_segment([at(0.0), at(MARK_BOOT_LEN * 0.5)], Stroke::new(bw * 0.82, jacket));
    painter.circle_filled(at(0.0), bw * 0.41, jacket);
    painter.line_segment([at(MARK_BOOT_LEN * 0.45), at(MARK_BOOT_LEN + 0.02)], Stroke::new(bw, jacket));
    painter.circle_filled(tip, MARK_TIP_R * h, MARK_SOCKET);
    painter.circle_stroke(tip, MARK_TIP_R * h, Stroke::new(MARK_TIP_STROKE * h, CREAM));
}

/// Window background: flat warm near-black. No watermark — decoration must
/// never overlap panels or controls (AGENTS.md rule 5).
pub fn paper_bg(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, PAPER);
}

/// The ONLY shadow: soft, large-blur, ink-based, under whole floating sheets.
/// Two stacked rounded rects approximate the blur. Paint BEFORE the sheet.
pub fn drop_shadow(painter: &egui::Painter, rect: Rect, radius: f32) {
    painter.rect_filled(rect.expand(4.0).translate(Vec2::new(0.0, 8.0)), radius + 4.0, alpha(INK, 0.08));
    painter.rect_filled(rect.expand(2.0).translate(Vec2::new(0.0, 5.0)), radius + 2.0, alpha(INK, 0.10));
}

/// The reference CTA's inner cream line — the universal focus/selection
/// treatment on dark: a stroke inset a few px inside the rect edge.
pub fn inset_keyline(painter: &egui::Painter, rect: Rect, radius: f32, color: Color32, width: f32, inset: f32) {
    let r = rect.expand(-inset);
    if r.width() <= 0.0 || r.height() <= 0.0 { return; }
    painter.rect_stroke(r, (radius - inset).max(0.0), Stroke::new(width, color), egui::StrokeKind::Inside);
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Knob value arc, flat (§4.6): hairline track α 0.35 at 2px, solid accent
/// value arc at 2.4px (no glow pass), flat end dot r 2.2. 270° sweep from
/// bottom-left; geometry unchanged. `map` carries the rack flip transform.
pub fn value_arc(
    painter: &egui::Painter,
    map: impl Fn(Vec2) -> Pos2,
    center: Vec2,
    r: f32,
    value: f32,
    accent: Color32,
) {
    let a0 = 135.0f32.to_radians();
    let sweep = 270.0f32.to_radians();
    let arc_r = r + 4.5;
    let segs = 20;
    let v = value.clamp(0.0, 1.0);
    let mut prev_track: Option<Pos2> = None;
    let mut prev_val: Option<Pos2> = None;
    for i in 0..=segs {
        let f = i as f32 / segs as f32;
        let ang = a0 + sweep * f;
        let p = map(center + Vec2::new(ang.cos(), ang.sin()) * arc_r);
        if let Some(pp) = prev_track {
            painter.line_segment([pp, p], Stroke::new(2.0, alpha(HAIRLINE_DARK, 0.35)));
        }
        prev_track = Some(p);
        if f <= v {
            if let Some(pp) = prev_val {
                painter.line_segment([pp, p], Stroke::new(2.4, accent));
            }
            prev_val = Some(p);
        }
    }
    // Flat indicator dot at the value angle.
    let ang = a0 + sweep * v;
    let dot = map(center + Vec2::new(ang.cos(), ang.sin()) * arc_r);
    painter.circle_filled(dot, 2.2, accent);
}

/// Meter color, stepped — not a gradient (§4.8): cream → tomato → clip red.
pub fn meter_color(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.6 { CREAM } else if t < 0.9 { ACCENT } else { CLIP_RED }
}

// ---------------------------------------------------------------------------
// Typography: tracked micro-caps (§4.5). egui has no letter-spacing API, so
// tracked labels are laid out per character: advance = char_width + em·size.
// Per-char galleys use delayed color (cached once, painted in any color).
// ---------------------------------------------------------------------------

/// A cached tracked label: per-character galleys + their x offsets.
/// Built once (in `ensure_galleys` for rack labels); painting is RT-safe.
pub struct TrackedLabel {
    /// (galley, x offset of the character origin).
    pub chars: Vec<(Arc<egui::Galley>, f32)>,
    pub size: Vec2,
}

/// Lay out `text` with letter-spacing `tracking_em` (em fractions; negative
/// for display type, clamped so advance ≥ 0.9·char_width). Color is applied
/// at paint time via [`paint_tracked`].
pub fn layout_tracked(fonts: &mut egui::epaint::FontsView<'_>, text: &str, font: FontId, tracking_em: f32) -> TrackedLabel {
    let tracking = tracking_em * font.size;
    let mut x = 0.0f32;
    let mut h = 0.0f32;
    let mut chars = Vec::with_capacity(text.chars().count());
    for ch in text.chars() {
        let g = fonts.layout_delayed_color(ch.to_string(), font.clone(), f32::INFINITY);
        h = h.max(g.size().y);
        chars.push((g, x));
        let w = fonts.glyph_width(&font, ch);
        let adv = if tracking_em < 0.0 { (w + tracking).max(w * 0.9) } else { w + tracking };
        x += adv;
    }
    TrackedLabel { chars, size: Vec2::new(x, h) }
}

/// Paint a cached tracked label at `pos` (top-left of the first character).
pub fn paint_tracked(painter: &egui::Painter, pos: Pos2, label: &TrackedLabel, color: Color32) {
    for (g, dx) in &label.chars {
        painter.galley(pos + Vec2::new(*dx, 0.0), g.clone(), color);
    }
}

/// Tracked label as an egui widget (paper-side chrome). Per-character galleys
/// hit egui's galley cache, so this stays allocation-light per frame.
/// IMPORTANT: layout happens under the fonts lock, painting AFTER it is
/// released — egui 0.37's Context is one RwLock, and Painter::add takes the
/// same write lock (painting inside fonts_mut deadlocks).
pub fn tracked_label(ui: &mut egui::Ui, text: &str, size: f32, tracking_em: f32, color: Color32) -> egui::Response {
    let font = FontId::proportional(size);
    let tracking = tracking_em * size;
    let advance = |f: &mut egui::epaint::FontsView<'_>, ch: char| {
        let w = f.glyph_width(&font, ch);
        if tracking_em < 0.0 { (w + tracking).max(w * 0.9) } else { w + tracking }
    };
    let (width, height, galleys) = ui.fonts_mut(|f| {
        let mut x = 0.0f32;
        let mut h = 0.0f32;
        let mut gs = Vec::with_capacity(text.chars().count());
        for ch in text.chars() {
            let g = f.layout_delayed_color(ch.to_string(), font.clone(), f32::INFINITY);
            h = h.max(g.size().y);
            gs.push((g, x));
            x += advance(f, ch);
        }
        (x, h, gs)
    });
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        for (g, dx) in galleys {
            painter.galley(Pos2::new(rect.left() + dx, rect.top()), g, color);
        }
    }
    resp
}

/// Section label for egui chrome: uppercase, 10px, tracked +0.14em, INK_2.
pub fn section_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    tracked_label(ui, &text.to_uppercase(), 10.0, 0.14, INK_2)
}

/// Primary action button (Send, Load, Add, Capture): ACCENT_DEEP fill,
/// ON_ACCENT text (5.32:1), near-sharp sheet radius, no border; hover darkens.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let base = egui::style::WidgetVisuals {
            bg_fill: ACCENT_DEEP,
            weak_bg_fill: ACCENT_DEEP,
            bg_stroke: Stroke::NONE,
            corner_radius: 2.0.into(),
            fg_stroke: Stroke::new(1.0, ON_ACCENT),
            expansion: 0.0,
        };
        let hot = egui::style::WidgetVisuals {
            bg_fill: mix(ACCENT_DEEP, INK, 0.08),
            weak_bg_fill: mix(ACCENT_DEEP, INK, 0.08),
            ..base
        };
        let v = ui.visuals_mut();
        v.widgets.inactive = base;
        v.widgets.hovered = hot;
        v.widgets.active = hot;
        v.widgets.open = base;
        ui.spacing_mut().button_padding = egui::vec2(12.0, 5.0);
        ui.add(egui::Button::new(egui::RichText::new(text).color(ON_ACCENT)).corner_radius(2.0).min_size(egui::vec2(0.0, 26.0)))
    }).inner
}

/// Real button chip (user request 2026-11-15: buttons, not underlined text).
/// On/active = cream fill + near-black text; off = raised dark chip with a
/// hairline. Tomato stays reserved for the flip CTA, selection and primary
/// actions — a row of engaged chips reads calm, not alarming. Text color is
/// driven by the widget visuals so hover feedback applies.
pub fn chip_button(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    ui.scope(|ui| {
        let (fill, txt) = if on { (CHIP_ON, DARK_PANEL) } else { (DARK_PANEL_HI, CREAM_MID) };
        let base = egui::style::WidgetVisuals {
            bg_fill: fill,
            weak_bg_fill: fill,
            bg_stroke: Stroke::new(1.0, if on { CHIP_ON } else { HAIRLINE }),
            corner_radius: 2.0.into(),
            fg_stroke: Stroke::new(1.0, txt),
            expansion: 0.0,
        };
        let hot_fill = if on { mix(CHIP_ON, DARK_PANEL, 0.10) } else { mix(DARK_PANEL_HI, CREAM, 0.07) };
        let hot = egui::style::WidgetVisuals {
            bg_fill: hot_fill,
            weak_bg_fill: hot_fill,
            bg_stroke: Stroke::new(1.0, if on { hot_fill } else { MUTED }),
            fg_stroke: Stroke::new(1.0, if on { DARK_PANEL } else { CREAM }),
            ..base
        };
        let v = ui.visuals_mut();
        v.widgets.inactive = base;
        v.widgets.hovered = hot;
        v.widgets.active = hot;
        v.widgets.open = base;
        // Breathing room: boxed text never touches the chip edge (rule 7).
        ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
        // Explicit text color: visuals.override_text_color would otherwise
        // force cream on the cream ON-fill (invisible label).
        ui.add(
            egui::Button::new(egui::RichText::new(text).size(12.5).color(txt))
                .corner_radius(2.0)
                .min_size(egui::vec2(0.0, 26.0)),
        )
    }).inner
}

// ---------------------------------------------------------------------------
// egui shell (§4.17): light paper chrome around the dark rack hero.
// ---------------------------------------------------------------------------

/// Hurricane visuals for the egui chrome — all-dark (user pivot 2026-11-15).
/// Shape scarcity (§2.3): page sheets & controls are essentially sharp (2–4px),
/// the hero rack is the one generous radius, the flip CTA the one pill.
/// Shadows only under floating sheets (windows) — never panels/controls.
pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PAPER;
    v.window_fill = PAPER;
    v.window_corner_radius = 4.0.into();
    v.window_stroke = Stroke::new(1.0, HAIRLINE);
    v.window_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: alpha(Color32::BLACK, 0.45) };
    v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 12, spread: 0, color: alpha(Color32::BLACK, 0.40) };
    v.override_text_color = Some(INK);
    v.hyperlink_color = MIDI_BLUE;
    v.selection.bg_fill = alpha(ACCENT, 0.30);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.extreme_bg_color = PAPER_SINK;
    v.code_bg_color = PAPER_SINK;
    v.warn_fg_color = ACCENT_BRIGHT;
    v.error_fg_color = CLIP_BRIGHT;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let r = 2.0;
    v.widgets.noninteractive.bg_fill = PAPER;
    v.widgets.noninteractive.weak_bg_fill = PAPER;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK_2);
    v.widgets.noninteractive.corner_radius = r.into();
    v.widgets.inactive.bg_fill = PAPER;
    v.widgets.inactive.weak_bg_fill = PAPER_SINK;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.inactive.corner_radius = r.into();
    v.widgets.hovered.bg_fill = PAPER_SINK;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2A, 0x28, 0x26);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.hovered.corner_radius = r.into();
    v.widgets.active.bg_fill = alpha(ACCENT, 0.20);
    v.widgets.active.weak_bg_fill = alpha(ACCENT, 0.12);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.active.corner_radius = r.into();
    v.widgets.open.bg_fill = PAPER_SINK;
    v.widgets.open.weak_bg_fill = PAPER_SINK;
    v.widgets.open.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.open.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.open.corner_radius = r.into();
    v
}

// ---------------------------------------------------------------------------
// Typography (§4.19): Noto Sans Regular/Medium/Bold + JetBrains Mono for
// telemetry only, with the bundled egui fonts as fallback. Files are read
// once, at startup only. Medium carries display/headings/unit names; Bold is
// reserved for the flip-pill large label.
// ---------------------------------------------------------------------------

static BOLD_INSTALLED: AtomicBool = AtomicBool::new(false);
static MEDIUM_INSTALLED: AtomicBool = AtomicBool::new(false);

fn load_font(fonts: &mut egui::FontDefinitions, name: &str, path: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => {
            fonts.font_data.insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            true
        }
        Err(_) => false,
    }
}

/// Install the Hurricane typefaces. Missing files degrade gracefully to the
/// egui defaults (the app must never fail to start over a font).
/// Global breathing room for plain egui buttons / drag values (rule 7):
/// boxed text gets ≥8px horizontal and ≥4px vertical padding everywhere,
/// not just on the themed chips.
pub fn install_spacing(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let reg = load_font(&mut fonts, "noto", "/usr/share/fonts/noto/NotoSans-Regular.ttf");
    let med = load_font(&mut fonts, "noto-medium", "/usr/share/fonts/noto/NotoSans-Medium.ttf");
    let bold = load_font(&mut fonts, "noto-bold", "/usr/share/fonts/noto/NotoSans-Bold.ttf");
    let mono = load_font(&mut fonts, "jbmono", "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf");
    if reg {
        fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "noto".into());
    }
    if mono {
        fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "jbmono".into());
    }
    if bold {
        let mut fam = vec!["noto-bold".to_owned()];
        if reg { fam.push("noto".into()); }
        fonts.families.insert(FontFamily::Name("bold".into()), fam);
        BOLD_INSTALLED.store(true, Ordering::Relaxed);
    }
    if med {
        let mut fam = vec!["noto-medium".to_owned()];
        if reg { fam.push("noto".into()); }
        fonts.families.insert(FontFamily::Name("medium".into()), fam);
        MEDIUM_INSTALLED.store(true, Ordering::Relaxed);
    }
    ctx.set_fonts(fonts);
}

/// Bold proportional face (falls back to the regular proportional face when
/// the bold file was unavailable). Unused since the flip pill was removed;
/// kept installed for large-label emphasis.
#[allow(dead_code)]
pub fn bold_font(size: f32) -> FontId {
    if BOLD_INSTALLED.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name("bold".into()))
    } else {
        FontId::proportional(size)
    }
}

/// Medium proportional face for display/headings/unit names (weight 500 ≈
/// the reference's 400–550). Falls back to regular when unavailable.
pub fn medium_font(size: f32) -> FontId {
    if MEDIUM_INSTALLED.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name("medium".into()))
    } else {
        FontId::proportional(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers of the `d` (or any) attribute of the element with `id`.
    fn svg_numbers(svg: &str, id: &str, attr: &str) -> Vec<f32> {
        let el = &svg[svg.find(&format!("id=\"{id}\"")).expect(id)..];
        let el = &el[..el.find('>').unwrap()];
        let v = &el[el.find(&format!(" {attr}=\"")).expect(attr) + attr.len() + 3..];
        let v = &v[..v.find('"').unwrap()];
        v.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect()
    }

    fn flat(p: [Pos2; 4]) -> Vec<f32> {
        p.iter().flat_map(|q| [q.x, q.y]).collect()
    }

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 0.06)
    }

    /// assets/logo.svg is the full-detail mark in a 392 px glyph box centered
    /// on the 512 tile — same control points and proportions as the painter.
    #[test]
    fn mark_geometry_matches_logo_svg() {
        let svg = include_str!("../assets/logo.svg");
        let g = mark_geometry(Pos2::new(256.0, 256.0), 392.0, true);
        assert!(close(&svg_numbers(svg, "cable-over", "d"), &flat(g.over)));
        assert!(close(&svg_numbers(svg, "cable-under", "d"), &flat(g.under)));
        let gap = egui::epaint::CubicBezierShape::from_points_stroke(
            g.over, false, Color32::TRANSPARENT, Stroke::NONE,
        )
        .split_range(MARK_GAP_T);
        assert!(close(&svg_numbers(svg, "weave-gap", "d"), &flat(gap.points)));
        let w = MARK_CABLE_W * g.h;
        assert!(close(&svg_numbers(svg, "cable-over", "stroke-width"), &[w]));
        assert!(close(&svg_numbers(svg, "weave-gap", "stroke-width"), &[w + 2.0 * MARK_GAP * g.h]));
        assert!(close(&svg_numbers(svg, "pulse", "r"), &[MARK_PULSE_R * g.h]));
    }

    /// Both detail levels keep the whole glyph (plugs included) inside `size`.
    #[test]
    fn mark_stays_inside_its_box() {
        for (size, detailed) in [(16.0, false), (392.0, true)] {
            let g = mark_geometry(Pos2::ZERO, size, detailed);
            let ext = if detailed { MARK_EXT_FULL } else { MARK_EXT_SMALL };
            assert!((g.h * ext - size * 0.5).abs() < 1e-3);
            assert_eq!(g.over[0], Pos2::new(-g.h, -g.h));
        }
    }
}
