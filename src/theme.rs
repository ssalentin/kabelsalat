//! Signal design language (docs/design-language.md) — "audio is light".
//!
//! Central palette + painter helpers for the luminous glass control surface.
//! Everything here is paint-only: no audio state, no locks, no I/O on the
//! frame path (font files are read once at startup). The rack, toolbar,
//! keyboard, popovers and egui panels all draw through these tokens so the
//! look stays consistent.
//!
//! Semantics: audio = cyan/blue · MIDI = amber · selection = violet ·
//! active/playing = green · clip/danger = red · special/record = magenta.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui::{
    self, Color32, FontFamily, FontId, Pos2, Rect, Shape, Stroke, Vec2,
};

// ---------------------------------------------------------------------------
// Palette — commit to it (docs/design-language.md)
// ---------------------------------------------------------------------------

// Background / neutrals: cool, layered — never flat gray.
pub const BG_DEEP: Color32 = Color32::from_rgb(0x07, 0x0A, 0x12); // window base
pub const BG_RAISED: Color32 = Color32::from_rgb(0x0C, 0x11, 0x20); // raised surface
pub const SURFACE: Color32 = Color32::from_rgb(0x12, 0x1A, 0x2E); // glass panel base
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1B, 0x27, 0x40); // gradient lighter end
pub const SCRIM: Color32 = Color32::from_rgb(0x05, 0x07, 0x0D); // overlays / help backdrop
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xED, 0xF7); // primary
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x7C, 0x8A, 0xA5); // secondary / labels
pub const HAIRLINE: Color32 = Color32::from_rgb(0x23, 0x30, 0x4C); // default border

// Signal accents: full chroma, separable by luminance too.
pub const AUDIO_CYAN: Color32 = Color32::from_rgb(0x22, 0xD3, 0xEE); // audio A / primary
pub const AUDIO_BLUE: Color32 = Color32::from_rgb(0x38, 0xBD, 0xF8); // audio B
pub const MIDI_AMBER: Color32 = Color32::from_rgb(0xFB, 0xBF, 0x24); // MIDI (warm)
pub const ACCENT_VIOLET: Color32 = Color32::from_rgb(0xA7, 0x8B, 0xFA); // selection
pub const ACCENT_MAGENTA: Color32 = Color32::from_rgb(0xF4, 0x72, 0xB6); // special / record
pub const LIVE_GREEN: Color32 = Color32::from_rgb(0x34, 0xD3, 0x99); // active / playing
pub const CLIP_RED: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71); // overload / danger
pub const WARN_AMBER: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B); // warning

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
// Depth & atmosphere
// ---------------------------------------------------------------------------

/// Vertical-gradient rounded rect as a triangle-fan mesh (one draw call).
/// Perimeter vertices are colored by their y; the fan center takes the mid
/// color. ~21 vertices — cheap enough for every panel, every frame.
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 { return; }
    let r = radius.clamp(0.0, rect.width().min(rect.height()) * 0.5);
    let color_at = |y: f32| {
        let t = ((y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        mix(top, bottom, t)
    };
    let mut mesh = egui::Mesh::default();
    let center = rect.center();
    mesh.vertices.push(egui::epaint::Vertex::untextured(center, color_at(center.y)));
    // Clockwise perimeter: TL → TR → BR → BL corner arcs.
    let corners = [
        (Pos2::new(rect.left() + r, rect.top() + r), std::f32::consts::PI),
        (Pos2::new(rect.right() - r, rect.top() + r), 1.5 * std::f32::consts::PI),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), 0.5 * std::f32::consts::PI),
    ];
    let segs = 4;
    let mut first = 1usize;
    for (cc, a0) in corners {
        for i in 0..=segs {
            let a = a0 + (std::f32::consts::FRAC_PI_2) * (i as f32 / segs as f32);
            let p = cc + Vec2::new(a.cos(), a.sin()) * r;
            let idx = mesh.vertices.len();
            mesh.vertices.push(egui::epaint::Vertex::untextured(p, color_at(p.y)));
            if idx > first {
                mesh.indices.extend_from_slice(&[0, idx as u32 - 1, idx as u32]);
            }
            if i == 0 && first == 1 { first = idx; }
        }
    }
    // Close the fan back to the first perimeter vertex.
    let last = mesh.vertices.len() as u32 - 1;
    mesh.indices.extend_from_slice(&[0, last, first as u32]);
    painter.add(Shape::mesh(mesh));
}

/// Soft drop shadow: two stacked rounded rects approximating a blur
/// (black ~35% total, offset y+6). Paint BEFORE the panel itself.
pub fn drop_shadow(painter: &egui::Painter, rect: Rect, radius: f32) {
    painter.rect_filled(rect.expand(3.0).translate(Vec2::new(0.0, 7.0)), radius + 3.0, alpha(Color32::BLACK, 0.16));
    painter.rect_filled(rect.expand(1.0).translate(Vec2::new(0.0, 5.0)), radius + 1.0, alpha(Color32::BLACK, 0.20));
}

/// The glass panel: vertical SURFACE_HI→SURFACE gradient, 1px accent-tinted
/// border (~35%), 1px top inner highlight (white ~6%). Shadow included.
pub fn glass_panel(painter: &egui::Painter, rect: Rect, radius: f32, accent: Color32) {
    drop_shadow(painter, rect, radius);
    gradient_rect(painter, rect, radius, SURFACE_HI, SURFACE);
    painter.rect_stroke(rect, radius, Stroke::new(1.0, alpha(accent, 0.35)), egui::StrokeKind::Inside);
    top_highlight(painter, rect, radius);
}

/// 1px inner highlight along the top edge (the "glass rim light").
pub fn top_highlight(painter: &egui::Painter, rect: Rect, radius: f32) {
    let y = rect.top() + 1.0;
    painter.line_segment(
        [Pos2::new(rect.left() + radius, y), Pos2::new(rect.right() - radius, y)],
        Stroke::new(1.0, alpha(Color32::WHITE, 0.06)),
    );
}

/// Accent edge glow for active/selected units: 3 stacked strokes with
/// decreasing width and alpha, drawn outside the rect.
pub fn glow_rect(painter: &egui::Painter, rect: Rect, radius: f32, color: Color32, strength: f32) {
    let s = strength.clamp(0.0, 1.0);
    if s <= 0.01 { return; }
    painter.rect_stroke(rect, radius, Stroke::new(6.0, alpha(color, 0.10 * s)), egui::StrokeKind::Outside);
    painter.rect_stroke(rect, radius, Stroke::new(3.5, alpha(color, 0.20 * s)), egui::StrokeKind::Outside);
    painter.rect_stroke(rect, radius, Stroke::new(1.5, alpha(color, 0.45 * s)), egui::StrokeKind::Outside);
}

/// Window atmosphere: BG_DEEP base + two very-low-alpha radial aurora glows
/// (cyan top-left, violet bottom-right) + the faint dot grid. Stacked
/// translucent circles approximate the radial falloff without textures.
pub fn aurora_bg(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, BG_DEEP);
    let r = rect.width().max(rect.height()) * 0.55;
    let glows = [
        (Pos2::new(rect.left() + rect.width() * 0.16, rect.top() + rect.height() * 0.10), AUDIO_CYAN, 0.013),
        (Pos2::new(rect.left() + rect.width() * 0.88, rect.top() + rect.height() * 0.92), ACCENT_VIOLET, 0.012),
    ];
    for (center, color, a) in glows {
        for i in 0..12 {
            let rad = r * (1.0 - i as f32 / 12.0);
            painter.circle_filled(center, rad, alpha(color, a));
        }
    }
    // Faint dot grid (HAIRLINE family, ~4% perceived).
    let spacing = 44.0;
    let dot = alpha(mix(HAIRLINE, TEXT_DIM, 0.35), 0.30);
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

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Knob value arc: HAIRLINE track, accent value arc with a soft glow pass,
/// indicator dot at the value angle. 270° sweep from bottom-left. `map`
/// converts unit-local offsets to screen space (the rack flip transform).
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
    let mut prev_glow: Option<Pos2> = None;
    let mut prev_val: Option<Pos2> = None;
    for i in 0..=segs {
        let f = i as f32 / segs as f32;
        let ang = a0 + sweep * f;
        let p = map(center + Vec2::new(ang.cos(), ang.sin()) * arc_r);
        if let Some(pp) = prev_track {
            painter.line_segment([pp, p], Stroke::new(2.0, alpha(HAIRLINE, 0.9)));
        }
        prev_track = Some(p);
        if f <= v {
            if let Some(pp) = prev_glow {
                painter.line_segment([pp, p], Stroke::new(5.5, alpha(accent, 0.18)));
            }
            prev_glow = Some(p);
            if let Some(pp) = prev_val {
                painter.line_segment([pp, p], Stroke::new(2.4, accent));
            }
            prev_val = Some(p);
        }
    }
    // Indicator dot at the value angle.
    let ang = a0 + sweep * v;
    let dot = map(center + Vec2::new(ang.cos(), ang.sin()) * arc_r);
    painter.circle_filled(dot, 2.2, mix(accent, Color32::WHITE, 0.45));
}

/// Meter gradient: green → amber → red across t ∈ [0,1].
pub fn meter_color(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.6 { mix(LIVE_GREEN, MIDI_AMBER, t / 0.6) } else { mix(MIDI_AMBER, CLIP_RED, (t - 0.6) / 0.4) }
}

/// Small uppercase letter-spaced section label (paint helper for galleys).
pub fn section_label(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).small().strong().color(TEXT_DIM)
}

// ---------------------------------------------------------------------------
// egui shell (panels, windows, widgets)
// ---------------------------------------------------------------------------

/// Signal visuals for the egui chrome around the custom rack painting.
pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG_DEEP;
    v.window_fill = SURFACE;
    v.window_corner_radius = 14.0.into();
    v.window_stroke = Stroke::new(1.0, HAIRLINE);
    v.window_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: alpha(Color32::BLACK, 0.45) };
    v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 12, spread: 0, color: alpha(Color32::BLACK, 0.40) };
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = AUDIO_CYAN;
    v.selection.bg_fill = alpha(ACCENT_VIOLET, 0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT_VIOLET);
    v.extreme_bg_color = BG_DEEP;
    v.code_bg_color = BG_RAISED;
    v.warn_fg_color = WARN_AMBER;
    v.error_fg_color = CLIP_RED;

    let r = 8.0;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.noninteractive.corner_radius = r.into();
    v.widgets.inactive.bg_fill = SURFACE_HI;
    v.widgets.inactive.weak_bg_fill = SURFACE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = r.into();
    v.widgets.hovered.bg_fill = mix(SURFACE_HI, AUDIO_CYAN, 0.14);
    v.widgets.hovered.weak_bg_fill = mix(SURFACE, AUDIO_CYAN, 0.10);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, alpha(AUDIO_CYAN, 0.45));
    v.widgets.hovered.corner_radius = r.into();
    v.widgets.active.bg_fill = mix(SURFACE_HI, ACCENT_VIOLET, 0.25);
    v.widgets.active.weak_bg_fill = mix(SURFACE, ACCENT_VIOLET, 0.18);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.bg_stroke = Stroke::new(1.0, alpha(ACCENT_VIOLET, 0.60));
    v.widgets.active.corner_radius = r.into();
    v.widgets.open.bg_fill = SURFACE_HI;
    v.widgets.open.weak_bg_fill = SURFACE_HI;
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.open.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.open.corner_radius = r.into();
    v
}

// ---------------------------------------------------------------------------
// Typography: Noto Sans (regular/medium/bold) + JetBrains Mono, with the
// bundled egui fonts as fallback. Files are read once, at startup only.
// ---------------------------------------------------------------------------

static BOLD_INSTALLED: AtomicBool = AtomicBool::new(false);

fn load_font(fonts: &mut egui::FontDefinitions, name: &str, path: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => {
            fonts.font_data.insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            true
        }
        Err(_) => false,
    }
}

/// Install the Signal typefaces. Missing files degrade gracefully to the
/// egui defaults (the app must never fail to start over a font).
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
    }
    ctx.set_fonts(fonts);
}

/// Bold proportional face for unit names / headings (falls back to the
/// regular proportional face when the bold font file was unavailable).
pub fn bold_font(size: f32) -> FontId {
    if BOLD_INSTALLED.load(Ordering::Relaxed) {
        FontId::new(size, FontFamily::Name("bold".into()))
    } else {
        FontId::proportional(size)
    }
}
