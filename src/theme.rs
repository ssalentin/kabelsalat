//! Hurricane design language (docs/design-reference-analysis.md) — light,
//! warm, editorial. One hero inversion: the rack is the reference's single
//! near-black panel; all egui chrome lives on warm paper.
//!
//! Central palette + painter helpers. Everything here is paint-only: no audio
//! state, no locks, no I/O on the frame path (font files are read once at
//! startup). Flat fills, 1px hairlines, one tomato accent — no gradients, no
//! glass, no glows. The only shadow is a soft ink one under floating sheets.
//!
//! Semantics: audio = cream (round terminals) · MIDI = blue (square) ·
//! selection/CTA = tomato · active/playing = teal · clip/danger = red.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui::{
    self, Color32, FontFamily, FontId, Pos2, Rect, Stroke, Vec2,
};

// ---------------------------------------------------------------------------
// Palette — measured from the Hurricane reference (§2.1 / §3.1 of the brief)
// ---------------------------------------------------------------------------

// Paper & ink (light side).
pub const PAPER: Color32 = Color32::from_rgb(0xF4, 0xF3, 0xEE); // window base / panels
pub const PAPER_SINK: Color32 = Color32::from_rgb(0xEC, 0xEA, 0xE3); // recessed fields, chips
pub const INK: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C); // primary text on paper
pub const INK_2: Color32 = Color32::from_rgb(0x64, 0x64, 0x64); // secondary text on paper
pub const HAIRLINE: Color32 = Color32::from_rgb(0xBE, 0xBE, 0xBE); // 1px rules on paper
pub const MUTED: Color32 = Color32::from_rgb(0xA7, 0xA2, 0x9F); // decorative ONLY (2.27:1)
pub const WATERMARK: Color32 = Color32::from_rgb(0xE7, 0xE5, 0xDF); // ambient signature

// The inverted hero panel (dark side).
pub const DARK_PANEL: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C); // rack chassis, help card
pub const DARK_PANEL_HI: Color32 = Color32::from_rgb(0x23, 0x21, 0x20); // plates, rails, plugs
pub const SCRIM: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1C); // help backdrop (α 0.62)
pub const CREAM: Color32 = Color32::from_rgb(0xEF, 0xEE, 0xE9); // primary text on dark
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

/// The brand asterisk: 3 lines through a center at 0°/60°/120° (six spokes).
pub fn asterisk(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, width: f32) {
    for i in 0..3 {
        let a = (i as f32) * std::f32::consts::PI / 3.0;
        let d = Vec2::new(a.cos(), a.sin()) * radius;
        painter.line_segment([center - d, center + d], Stroke::new(width, color));
    }
}

/// Window background: flat warm paper + one oversized watermark asterisk
/// bleeding off the bottom-right (purely decorative, behind everything).
pub fn paper_bg(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, PAPER);
    let center = Pos2::new(rect.right() - 60.0, rect.bottom() - 30.0);
    asterisk(painter, center, 140.0, WATERMARK, 2.0);
}

/// The ONLY shadow: soft, large-blur, ink-based, under whole floating sheets.
/// Two stacked rounded rects approximate the blur. Paint BEFORE the sheet.
pub fn drop_shadow(painter: &egui::Painter, rect: Rect, radius: f32) {
    painter.rect_filled(rect.expand(4.0).translate(Vec2::new(0.0, 8.0)), radius + 4.0, alpha(INK, 0.08));
    painter.rect_filled(rect.expand(2.0).translate(Vec2::new(0.0, 5.0)), radius + 2.0, alpha(INK, 0.10));
}

/// Flat sheet: soft shadow + flat fill + optional 1px hairline. No gradients,
/// no glass, no top highlight.
pub fn flat_panel(painter: &egui::Painter, rect: Rect, radius: f32, fill: Color32, border: Option<Color32>) {
    drop_shadow(painter, rect, radius);
    painter.rect_filled(rect, radius, fill);
    if let Some(c) = border {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, c), egui::StrokeKind::Inside);
    }
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
/// ON_ACCENT text (5.32:1), radius 8, no border; hover darkens toward ink.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let base = egui::style::WidgetVisuals {
            bg_fill: ACCENT_DEEP,
            weak_bg_fill: ACCENT_DEEP,
            bg_stroke: Stroke::NONE,
            corner_radius: 8.0.into(),
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
        ui.add(egui::Button::new(egui::RichText::new(text).color(ON_ACCENT)).corner_radius(8.0))
    }).inner
}

// ---------------------------------------------------------------------------
// egui shell (§4.17): light paper chrome around the dark rack hero.
// ---------------------------------------------------------------------------

/// Hurricane visuals for the egui chrome around the custom rack painting.
pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    v.panel_fill = PAPER;
    v.window_fill = PAPER;
    v.window_corner_radius = 14.0.into();
    v.window_stroke = Stroke::new(1.0, HAIRLINE);
    v.window_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: alpha(INK, 0.10) };
    v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 12, spread: 0, color: alpha(INK, 0.10) };
    v.override_text_color = Some(INK);
    v.hyperlink_color = MIDI_BLUE_DEEP;
    v.selection.bg_fill = alpha(ACCENT, 0.25);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.extreme_bg_color = PAPER_SINK;
    v.code_bg_color = PAPER_SINK;
    v.warn_fg_color = ACCENT_DEEP;
    v.error_fg_color = CLIP_TEXT;

    let r = 8.0;
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
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xE4, 0xE2, 0xDA);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.hovered.corner_radius = r.into();
    v.widgets.active.bg_fill = alpha(ACCENT, 0.15);
    v.widgets.active.weak_bg_fill = alpha(ACCENT, 0.10);
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

/// Bold proportional face — reserved for the flip-pill large label (falls
/// back to the regular proportional face when the bold file was unavailable).
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
