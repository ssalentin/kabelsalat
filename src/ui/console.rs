//! The expanded keyboard (#163): each instrument as its own full-window
//! console, laid out for the screen it fills rather than the dock grown
//! taller. The rack and the top bars step aside; Esc or DOCK bring them
//! back. Keys, header and wheel MIDI are the dock's own (`kb_keybed`,
//! `kb_header`, `kb_wheel_drive`), so both play exactly alike.
//!
//! - Piano: the open case behind the fallboard (bronze plate, a string per
//!   key that swings while it sounds, dampers that lift with the pedal,
//!   hammers), touch and tone on the fallboard, which mirrors the keys in
//!   its lacquer, and the pedal lyre between the legs.
//! - Jazz Organ: the tone generator (every wheel a sounding key drives
//!   through the drawbars lights up), preset keys and percussion tablets,
//!   nine drawbars, the rotary speaker, DRIVE/REVERB, one or two manuals
//!   between walnut cheeks, the swell shoe.
//! - Pipe Organ: a pipe facade (a pipe speaks for each sounding note),
//!   every division's drawknobs on two stop jambs at once, couplers and
//!   tremulants, the swell box's shutters, thumb pistons, the swell shoe.
//! - Synth: a display with the patch and a scope of what sounds, the
//!   function row, up to eight macro knobs, a large X/Y pad, and
//!   full-height pitch and mod wheels.
//!
//! One light for every part: it falls from the top, so upper edges catch
//! it, recesses are shaded under their upper rim, and shadows drop down.

use super::*;
use crate::keyboard::{KeyboardType, SynthDragMode};
use egui::epaint::{Mesh, RectShape};
use egui::Align;
use std::f32::consts::{PI, TAU};

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

const SHADE: Color32 = Color32::BLACK;
const LACQUER: Color32 = rgb(0x0B, 0x0A, 0x09);
const LACQUER_HI: Color32 = rgb(0x24, 0x22, 0x1F);
const WALNUT: Color32 = rgb(0x3E, 0x27, 0x17);
const WALNUT_DEEP: Color32 = rgb(0x21, 0x15, 0x0C);
const WALNUT_HI: Color32 = rgb(0x5E, 0x3C, 0x22);
const JAMB: Color32 = rgb(0x17, 0x11, 0x0B);
const SPRUCE: Color32 = rgb(0x4C, 0x36, 0x1D);
const SPRUCE_DEEP: Color32 = rgb(0x21, 0x17, 0x0D);
const BRASS: Color32 = rgb(0xC8, 0xA0, 0x5A);
const BRASS_DEEP: Color32 = rgb(0x7A, 0x5F, 0x33);
const BRASS_HI: Color32 = rgb(0xF0, 0xD8, 0x9C);
const BRONZE: Color32 = rgb(0x8C, 0x6C, 0x36);
const BRONZE_DEEP: Color32 = rgb(0x45, 0x34, 0x1A);
const GOLD_LEAF: Color32 = rgb(0xD9, 0xC2, 0x8E);
const PIPE_TIN: Color32 = rgb(0xA4, 0xA0, 0x97);
const TIN_HI: Color32 = rgb(0xE6, 0xE3, 0xDA);
const TIN_DEEP: Color32 = rgb(0x3E, 0x3C, 0x38);
const IVORY: Color32 = rgb(0xEC, 0xE4, 0xCF);
const IVORY_HI: Color32 = rgb(0xF8, 0xF3, 0xE4);
const IVORY_INK: Color32 = rgb(0x24, 0x1A, 0x10);
const STRING_STEEL: Color32 = rgb(0xA8, 0xA6, 0x9E);
const STRING_COPPER: Color32 = rgb(0x9A, 0x63, 0x38);
const HAMMER_FELT: Color32 = rgb(0xDE, 0xD8, 0xC8);
const METAL: Color32 = rgb(0x1B, 0x1A, 0x18);
const METAL_HI: Color32 = rgb(0x2C, 0x2A, 0x27);
const ALU: Color32 = rgb(0xC4, 0xC1, 0xB8);
const ALU_DEEP: Color32 = rgb(0x4E, 0x4B, 0x46);
const RUBBER: Color32 = rgb(0x16, 0x15, 0x14);
const LCD: Color32 = rgb(0x08, 0x13, 0x12);
const LCD_HI: Color32 = rgb(0x0E, 0x24, 0x22);

const HEADER_H: f32 = 46.0;

/// What a console laid out: the (upper) keybed, the control area, and the
/// MIDI strip's rects (pitch, mod, sustain, VEL) where the console has them.
struct Laid {
    bed: Rect,
    deck: Rect,
    strip: [Rect; 4],
}

impl DemoApp {
    /// The expanded keyboard: the header row over the instrument's console,
    /// filling the window (the caller hides every other panel).
    pub(super) fn keyboard_console_ui(&mut self, root: &mut egui::Ui) {
        let h = (root.available_height() - 2.0).max(164.0).round();
        let surface = self.kb_surface();
        let target = self.kb_target_unit();
        let mut laid = Laid { bed: Rect::NOTHING, deck: Rect::NOTHING, strip: [Rect::NOTHING; 4] };
        let resp = egui::Panel::bottom("dock")
            .exact_size(h)
            .resizable(false)
            .frame(egui::Frame::new().fill(theme::PAPER))
            .show(root, |ui| {
                let full = ui.available_rect_before_wrap();
                let pad = (full.width() * 0.018).clamp(10.0, 28.0).round();
                let header = Rect::from_min_size(full.min + egui::vec2(pad, 0.0), egui::vec2(full.width() - 2.0 * pad, HEADER_H));
                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(header).layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| self.kb_header(ui, surface, target, true),
                );
                ui.painter().line_segment(
                    [Pos2::new(full.left(), header.bottom()), Pos2::new(full.right(), header.bottom())],
                    Stroke::new(1.0, theme::HAIRLINE),
                );
                let gap = (pad * 0.6).round();
                let body = Rect::from_min_max(
                    Pos2::new(full.left() + pad, header.bottom() + gap),
                    Pos2::new(full.right() - pad, full.bottom() - gap),
                );
                // The instrument stands on the stage: its shadow under it.
                drop_shadow(ui.painter(), body.shrink(4.0), 12.0, 5.0, 16.0, 0.6);
                match surface {
                    KeyboardType::Piano => self.console_piano(ui, body, target, &mut laid),
                    KeyboardType::JazzOrgan => self.console_jazz(ui, body, &mut laid),
                    KeyboardType::PipeOrgan => self.console_pipe(ui, body, target, &mut laid),
                    KeyboardType::Synth => self.console_synth(ui, body, target, &mut laid),
                }
                // Strings, wheels, pipes and the scope move while notes sound.
                if self.note_leds.iter().any(|l| *l > 0.02) {
                    ui.ctx().request_repaint();
                }
                ui.allocate_rect(full, egui::Sense::hover());
            });
        self.dock_rect = resp.response.rect;
        self.dock_kb_rect = laid.bed;
        self.kb_deck_rect = laid.deck;
        self.kb_strip = laid.strip;
        self.assistant_rect = Rect::NOTHING;
        self.assistant_input_rect = Rect::NOTHING;
    }

    /// White-key width a keybed of width `w` gets (the dock's geometry).
    fn console_key_w(w: f32) -> f32 {
        let bed = Rect::from_min_size(Pos2::ZERO, egui::vec2(w, 100.0));
        (w - 12.0) / Self::dock_whites(bed) as f32
    }

    /// Center x of semitone `semi` (above the bed's lowest C) on `bed`.
    fn console_semi_x(bed: Rect, semi: usize) -> f32 {
        let (oct, n) = (semi / 12, semi % 12);
        match BLACK_SEMI.iter().position(|&b| b == n) {
            Some(bi) => Self::dock_black_key_rect(bed, oct, bi).center().x,
            None => {
                let wi = oct * 7 + WHITE_SEMI.iter().position(|&w| w == n).unwrap_or(0);
                Self::dock_key_rect(bed, wi).center().x
            }
        }
    }

    /// How loud semitone `semi` of the keybed sounds right now (0..1).
    fn console_led(&self, semi: usize) -> f32 {
        let base = 12 * (self.octave as usize + 1);
        self.note_leds[(base + semi).min(127)]
    }

    /// The target's main controls (the plugin's own knobs, rule 89).
    fn console_mains(&self, target: Option<usize>, n: usize) -> Vec<(String, f32)> {
        target.map(|t| {
            self.units[t].knobs.iter()
                .filter(|k| matches!(k.source, KnobSource::Main(_)))
                .take(n)
                .map(|k| (k.label.clone(), k.display_value))
                .collect()
        }).unwrap_or_default()
    }

    fn console_set_main(&mut self, target: Option<usize>, label: &str, v: f32) {
        let Some(t) = target else { return };
        if let Some(k) = self.units[t].knobs.iter_mut().find(|k| k.label == label && matches!(k.source, KnobSource::Main(_))) {
            k.set_immediate(v);
        }
    }

    /// "Rig A · Surge XT · Grand" for the console's name plates.
    fn console_subtitle(&self, target: Option<usize>) -> String {
        match target {
            Some(t) => {
                let u = &self.units[t];
                let rig = self.rig_name(u.rig as usize);
                if u.sub.is_empty() { rig } else { format!("{rig} · {}", u.sub) }
            }
            None => "no instrument in this rig".into(),
        }
    }

    /// The patch a name plate shows: the loaded sound, else the unit.
    fn console_patch(&self, target: Option<usize>, fallback: &str) -> String {
        target.map(|t| {
            let u = &self.units[t];
            u.sub.rsplit(" · ").next().filter(|s| !s.is_empty() && u.sub.contains(" · "))
                .map(str::to_string)
                .unwrap_or_else(|| sentence_case(&u.name))
        }).unwrap_or_else(|| fallback.to_string())
    }

    /// ARP state of the active rig: None without an Arp unit, else whether
    /// it runs a pattern (off = CHORD mode).
    pub(super) fn kb_arp_on(&self) -> Option<bool> {
        let a = self.units.iter().position(|u| u.rig == self.active_rig as u8 && matches!(u.node_kind, UnitKind::Arp))?;
        let mode_idx = self.units[a].knobs.iter().find(|k| k.param == Some(crate::arp::P_ARP_MODE))
            .map(|k| crate::arp::step_index(k.value as f64, crate::arp::ArpMode::ALL.len()))
            .unwrap_or(0);
        Some(crate::arp::ArpMode::ALL[mode_idx] != crate::arp::ArpMode::Chord)
    }

    pub(super) fn kb_arp_toggle(&mut self) {
        let Some(on) = self.kb_arp_on() else {
            self.plugin_msg = "No Arp unit in this rig — add one from the Plugins view or ask the agent.".into();
            return;
        };
        let a = self.units.iter().position(|u| u.rig == self.active_rig as u8 && matches!(u.node_kind, UnitKind::Arp));
        let next = if on { crate::arp::ArpMode::Chord } else { crate::arp::ArpMode::Up };
        let v = crate::arp::step_value(crate::arp::ArpMode::ALL.iter().position(|m| *m == next).unwrap_or(0), crate::arp::ArpMode::ALL.len());
        if let Some(k) = a.and_then(|a| self.units[a].knobs.iter_mut().find(|k| k.param == Some(crate::arp::P_ARP_MODE))) {
            k.set_immediate(v);
        }
    }

    fn console_swell(&mut self, ui: &mut egui::Ui, rect: Rect) {
        if let Some(v) = swell_shoe(ui, cid("con_swell"), rect, self.kb_swell) {
            if (v - self.kb_swell).abs() > 1e-4 {
                self.kb_swell = v;
                self.kb_cc(11, (v * 127.0).round() as u8);
                self.kb_strip_pulse(0.3 + v * 0.4);
            }
        }
    }

    fn console_vel(&mut self, ui: &mut egui::Ui, rect: Rect) -> Rect {
        let v = self.kb_vel();
        if let Some(nv) = hbar(ui, cid("con_vel"), rect, "VEL", v) {
            self.kb_set_vel(nv);
        }
        rect
    }

    // ------------------------------------------------------------------
    // Piano

    fn console_piano(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let arm = (b.width() * 0.022).clamp(12.0, 34.0).round();
        let inner_w = b.width() - 2.0 * arm;
        let kw = Self::console_key_w(inner_w);
        let slip_h = 9.0;
        let mut keys_h = (kw * 4.4 + 12.0).min(b.height() * 0.44).max(90.0).round();
        let pedal_h = (b.height() * 0.15).clamp(54.0, 132.0).round();
        let mut nb_h = (b.height() * 0.15).clamp(64.0, 132.0).round();
        let spare = b.height() - keys_h - pedal_h - nb_h - slip_h;
        let action_h = if spare >= 72.0 { spare } else {
            if spare > 0.0 { nb_h += spare; } else { keys_h = (keys_h + spare).max(70.0); }
            0.0
        };
        let mut y = b.top();
        let action = Rect::from_min_max(Pos2::new(b.left() + 7.0, y + 7.0), Pos2::new(b.right() - 7.0, y + action_h - 3.0));
        y += action_h;
        let nb = Rect::from_min_size(Pos2::new(b.left() + arm, y), egui::vec2(inner_w, nb_h));
        y += nb_h;
        let bed_area = Rect::from_min_size(Pos2::new(b.left() + arm, y), egui::vec2(inner_w, keys_h));
        y += keys_h;
        let slip = Rect::from_min_size(Pos2::new(b.left(), y), egui::vec2(b.width(), slip_h));
        y += slip_h;
        let under = Rect::from_min_max(Pos2::new(b.left(), y), b.max);
        let case = Rect::from_min_max(b.min, slip.max);

        // The room under the keybed, then the case in black lacquer.
        p.rect_filled(b, 10.0, rgb(0x0A, 0x09, 0x08));
        vgrad(&p, Rect::from_min_max(Pos2::new(b.left() + 8.0, under.bottom() - under.height() * 0.5), Pos2::new(b.right() - 8.0, under.bottom() - 1.0)), alpha(WALNUT_HI, 0.0), alpha(WALNUT_HI, 0.3));
        p.add(RectShape::filled(case, egui::CornerRadius { nw: 10, ne: 10, sw: 3, se: 3 }, LACQUER));
        vgrad(&p, Rect::from_min_size(case.min + egui::vec2(10.0, 1.0), egui::vec2(case.width() - 20.0, 4.0)), alpha(theme::CREAM, 0.16), alpha(theme::CREAM, 0.0));
        p.rect_stroke(b, 10.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);

        // The keys in their well.
        p.rect_filled(bed_area, 0.0, rgb(0x05, 0x04, 0x04));
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::Piano, 0.0);
        let bed = laid.bed;
        let keys = Rect::from_min_max(Self::dock_key_rect(bed, 0).left_top(), Self::dock_key_rect(bed, Self::dock_whites(bed) - 1).right_bottom());
        // The fallboard overhangs the back of the keys.
        vgrad(&p, Rect::from_min_size(keys.min, egui::vec2(keys.width(), (keys.height() * 0.09).clamp(8.0, 20.0))), alpha(SHADE, 0.42), alpha(SHADE, 0.0));

        // The arms either side of the keys, and the key slip under them.
        for r in [
            Rect::from_min_max(Pos2::new(b.left() + 1.0, nb.top()), Pos2::new(nb.left(), slip.top())),
            Rect::from_min_max(Pos2::new(nb.right(), nb.top()), Pos2::new(b.right() - 1.0, slip.top())),
        ] {
            hgrad(&p, Rect::from_min_max(r.min, Pos2::new(r.center().x, r.bottom())), LACQUER, LACQUER_HI);
            hgrad(&p, Rect::from_min_max(Pos2::new(r.center().x, r.top()), r.max), LACQUER_HI, LACQUER);
            p.line_segment([r.left_top() + egui::vec2(2.0, 0.5), r.right_top() + egui::vec2(-2.0, 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.14)));
        }
        vgrad(&p, slip.shrink2(egui::vec2(1.0, 0.0)), rgb(0x2E, 0x2C, 0x29), LACQUER);
        p.line_segment([slip.left_top() + egui::vec2(3.0, 0.5), slip.right_top() + egui::vec2(-3.0, 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.22)));

        // Behind the fallboard: the open case, a string per key.
        if action.height() >= 60.0 {
            self.console_piano_action(&p, action, bed);
        }

        // The fallboard: lacquer that mirrors the keys under it.
        vgrad(&p, nb, rgb(0x1D, 0x1B, 0x19), LACQUER);
        p.line_segment([nb.left_top() + egui::vec2(0.0, 0.5), nb.right_top() + egui::vec2(0.0, 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.13)));
        let refl_h = ((nb_h - 50.0) * 0.6).clamp(8.0, 42.0);
        let refl_top = nb.bottom() - refl_h;
        for wi in 0..Self::dock_whites(bed) {
            let kr = Self::dock_key_rect(bed, wi);
            let lit = self.console_led(Self::dock_white_semi(wi)) > 0.02;
            let col = if lit { alpha(theme::ACCENT, 0.5) } else { alpha(IVORY, 0.2) };
            vgrad(&p, Rect::from_min_max(Pos2::new(kr.left(), refl_top), Pos2::new(kr.right(), nb.bottom())), alpha(col, 0.0), col);
        }
        for oct in 0..Self::dock_octaves(bed) {
            for bi in 0..5 {
                let kr = Self::dock_black_key_rect(bed, oct, bi);
                let lit = self.console_led(12 * oct + BLACK_SEMI[bi]) > 0.02;
                let col = if lit { alpha(theme::ACCENT, 0.5) } else { alpha(LACQUER, 0.9) };
                vgrad(&p, Rect::from_min_max(Pos2::new(kr.left(), refl_top + refl_h * 0.2), Pos2::new(kr.right(), nb.bottom())), alpha(col, 0.0), col);
            }
        }
        // The felt strip where the keys meet the fallboard.
        p.rect_filled(Rect::from_min_max(Pos2::new(nb.left(), nb.bottom() - 1.0), Pos2::new(nb.right(), bed.top() + 1.0)), 0.0, mix(Self::KB_FELT, SHADE, 0.35));

        // Touch on the left, the name in gold leaf on the instrument's
        // axis, tone (the patch's own controls) on the right.
        let zone_w = (nb.width() * 0.24).clamp(150.0, 300.0);
        let content = Rect::from_min_max(nb.min + egui::vec2(16.0, 6.0), Pos2::new(nb.right() - 16.0, refl_top - 2.0));
        let mains = self.console_mains(target, 4);
        let row_h = ((content.height() - 6.0) / 2.0).clamp(16.0, 28.0);
        // Two rows on the left; one each side when the right is free.
        let rows_top = if mains.is_empty() { content.center().y - row_h * 0.5 } else { (content.center().y - row_h - 3.0).max(nb.top() + 5.0) };
        let yt = Rect::from_min_size(Pos2::new(content.left(), rows_top), egui::vec2(96.0f32.min(zone_w * 0.62), row_h));
        if tab(ui, cid("con_ytouch"), yt, "Y-TOUCH", self.kb_y_touch, theme::LIVE_TEAL)
            .on_hover_text("where you hit the key sets velocity: top = pp, bottom = ff")
            .clicked()
        {
            self.kb_y_touch = !self.kb_y_touch;
        }
        // The last hit's loudness, pp to ff, beside Y-TOUCH.
        let meter = Rect::from_min_max(Pos2::new(yt.right() + 12.0, yt.center().y - 4.0), Pos2::new(content.left() + zone_w, yt.center().y + 4.0));
        if meter.width() > 30.0 {
            let n = ((meter.width() / 7.0) as usize).clamp(6, 24);
            let v = self.kb_last_vel.clamp(0.0, 1.0);
            let sw = meter.width() / n as f32;
            for k in 0..n {
                let on = (k as f32 + 0.5) / n as f32 <= v;
                let seg = Rect::from_min_size(Pos2::new(meter.left() + k as f32 * sw, meter.top()), egui::vec2(sw - 2.0, meter.height()));
                let hot = mix(BRASS, BRASS_HI, k as f32 / n as f32);
                p.rect_filled(seg, 1.0, if on { hot } else { rgb(0x23, 0x21, 0x1E) });
            }
        }
        let vel_h = row_h.min(22.0);
        let vel = if mains.is_empty() {
            Rect::from_min_size(Pos2::new(content.right() - zone_w, content.center().y - vel_h * 0.5), egui::vec2(zone_w, vel_h))
        } else {
            Rect::from_min_size(Pos2::new(content.left(), rows_top + row_h + 6.0), egui::vec2(zone_w, vel_h))
        };
        laid.strip[3] = self.console_vel(ui, vel);

        if !mains.is_empty() {
            let cell_w = (zone_w / mains.len() as f32).min(nb_h * 0.9).min(96.0);
            let rw = cell_w * mains.len() as f32;
            for (i, (label, v)) in mains.iter().enumerate() {
                let cell = Rect::from_min_max(
                    Pos2::new(content.right() - rw + i as f32 * cell_w, nb.top() + 4.0),
                    Pos2::new(content.right() - rw + (i + 1) as f32 * cell_w, nb.bottom() - 3.0),
                );
                if let Some(nv) = knob(ui, cid(("con_main", i)), cell, label, *v, KnobLook::Brass, 64.0) {
                    self.console_set_main(target, label, nv);
                }
            }
        }
        let name_w = nb.width() - 2.0 * (zone_w + 16.0 + 22.0);
        let name = self.console_patch(target, "Piano");
        let (name, font) = fit_text(ui, &name, name_w, (nb_h * 0.3).clamp(15.0, 34.0), 11.0, true);
        let cx = nb.center().x;
        let sub_room = content.height() >= font.size * 1.25 + 20.0;
        let name_y = content.center().y - if sub_room { 8.0 } else { 0.0 };
        let name_r = p.text(Pos2::new(cx, name_y), egui::Align2::CENTER_CENTER, name, font, GOLD_LEAF);
        if sub_room {
            let rule = (name_r.width() * 0.5).clamp(18.0, 60.0);
            let ry = name_r.bottom() + 3.5;
            hgrad(&p, Rect::from_min_max(Pos2::new(cx - rule, ry), Pos2::new(cx, ry + 1.0)), alpha(BRASS, 0.0), BRASS);
            hgrad(&p, Rect::from_min_max(Pos2::new(cx, ry), Pos2::new(cx + rule, ry + 1.0)), BRASS, alpha(BRASS, 0.0));
            legend_fit(ui, Pos2::new(cx, ry + 5.0), Align::Center, &self.console_subtitle(target).to_uppercase(), name_w, 9.0, theme::CREAM_DIM);
        }

        // Under the keybed: its shadow, the legs and the pedal lyre with
        // SOFT (una corda, CC 67) and SUSTAIN (CC 64).
        vgrad(&p, Rect::from_min_size(under.min + egui::vec2(6.0, 0.0), egui::vec2(under.width() - 12.0, (under.height() * 0.5).min(40.0))), alpha(SHADE, 0.75), alpha(SHADE, 0.0));
        let floor = under.bottom() - 4.0;
        let leg_w = (arm * 1.5).clamp(18.0, 46.0);
        for lx in [b.left() + arm * 0.45 + leg_w * 0.5, b.right() - arm * 0.45 - leg_w * 0.5] {
            let (top_y, cast) = (under.top(), floor - 7.0);
            let pts = [
                Pos2::new(lx - leg_w * 0.5, top_y),
                Pos2::new(lx + leg_w * 0.5, top_y),
                Pos2::new(lx + leg_w * 0.3, cast),
                Pos2::new(lx - leg_w * 0.3, cast),
            ];
            fan(&p, &pts, |q| {
                let t = ((q.x - lx) / (leg_w * 0.5)).abs().min(1.0);
                mix(mix(rgb(0x42, 0x3F, 0x3A), LACQUER, t), SHADE, 0.4 * (1.0 - ((q.y - top_y) / (cast - top_y).max(1.0)).min(1.0)))
            });
            p.line_segment([Pos2::new(lx - leg_w * 0.2, top_y + 6.0), Pos2::new(lx - leg_w * 0.12, cast - 3.0)], Stroke::new(1.0, alpha(theme::CREAM, 0.22)));
            let ferrule = Rect::from_min_max(Pos2::new(lx - leg_w * 0.3, cast - 1.0), Pos2::new(lx + leg_w * 0.3, floor));
            hgrad(&p, Rect::from_min_max(ferrule.min, Pos2::new(lx - leg_w * 0.08, ferrule.bottom())), BRASS_DEEP, BRASS_HI);
            hgrad(&p, Rect::from_min_max(Pos2::new(lx - leg_w * 0.08, ferrule.top()), ferrule.max), BRASS_HI, BRASS_DEEP);
        }
        let pw = (kw * 1.3).clamp(44.0, 90.0);
        let ph = (under.height() * 0.34).clamp(16.0, 42.0);
        let label_h = 13.0;
        let py = floor - label_h - ph - 2.0;
        let cx = under.center().x;
        let box_h = (under.height() * 0.2).clamp(9.0, 26.0);
        let lyre_box = Rect::from_min_max(Pos2::new(cx - pw * 1.55, py - box_h * 0.7), Pos2::new(cx + pw * 1.55, py + ph * 0.3));
        if lyre_box.top() - under.top() >= 6.0 {
            for s in [-1.0f32, 1.0] {
                let (top_x, bot_x) = (cx + s * pw * 0.55, cx + s * pw * 1.2);
                let pts = [
                    Pos2::new(top_x - 4.5, under.top()),
                    Pos2::new(top_x + 4.5, under.top()),
                    Pos2::new(bot_x + 5.5, lyre_box.top() + 2.0),
                    Pos2::new(bot_x - 5.5, lyre_box.top() + 2.0),
                ];
                p.add(egui::Shape::convex_polygon(pts.to_vec(), rgb(0x24, 0x22, 0x1F), Stroke::new(1.0, alpha(theme::CREAM, 0.18))));
                // The pedal rod beside it.
                p.line_segment([Pos2::new(cx + s * pw * 0.2, under.top()), Pos2::new(cx + s * pw * 0.72, lyre_box.top() + 1.0)], Stroke::new(1.5, BRASS_DEEP));
            }
        }
        drop_shadow(&p, lyre_box, 4.0, 3.0, 8.0, 0.7);
        plate(&p, lyre_box, 4.0, rgb(0x3A, 0x37, 0x33), rgb(0x12, 0x11, 0x10));
        p.line_segment([lyre_box.left_top() + egui::vec2(4.0, 1.0), lyre_box.right_top() + egui::vec2(-4.0, 1.0)], Stroke::new(1.0, alpha(theme::CREAM, 0.3)));
        let soft_r = Rect::from_min_size(Pos2::new(cx - pw * 1.3, py), egui::vec2(pw, ph));
        let sus_r = Rect::from_min_size(Pos2::new(cx + pw * 0.3, py), egui::vec2(pw, ph));
        if pedal(ui, cid("con_soft"), soft_r, "SOFT", self.kb_soft).on_hover_text("una corda (CC 67)").clicked() {
            self.kb_soft = !self.kb_soft;
            self.kb_cc(67, if self.kb_soft { 127 } else { 0 });
            self.kb_strip_pulse(0.6);
        }
        let sus = pedal(ui, cid("con_sustain"), sus_r, "SUSTAIN", self.kb_sustain);
        laid.strip[2] = sus.rect;
        if sus.on_hover_text("sustain pedal (CC 64): released keys ring on, lit teal").clicked() {
            self.kb_toggle_sustain();
        }
        laid.deck = nb;
    }

    /// The piano's action seen through the open case: the bronze plate with
    /// its tuning pins, a string per key (copper-wound in the bass, its
    /// length halving up the scale, swinging while it sounds), the dampers
    /// (lifted by a held key or the sustain pedal), and the hammers.
    fn console_piano_action(&self, p: &egui::Painter, a: Rect, bed: Rect) {
        // The soundboard: spruce, its grain running with the strings.
        plate(p, a, 6.0, SPRUCE, SPRUCE_DEEP);
        let lines = (a.width() / 5.0) as u32;
        for k in 0..lines {
            let x = a.left() + 4.0 + (a.width() - 8.0) * (k as f32 + noise(k)) / lines as f32;
            let dark = noise(k + 900) > 0.25;
            p.line_segment(
                [Pos2::new(x, a.top() + 3.0), Pos2::new(x, a.bottom() - 3.0)],
                Stroke::new(1.0, if dark { alpha(SHADE, 0.05 + 0.12 * noise(k + 300)) } else { alpha(BRASS_HI, 0.03 + 0.04 * noise(k + 600)) }),
            );
        }
        let octaves = Self::dock_octaves(bed);
        let span = 12 * octaves;
        let kw = (bed.width() - 12.0) / Self::dock_whites(bed) as f32;
        let rail_h = (a.height() * 0.1).clamp(7.0, 14.0);
        let rail = Rect::from_min_max(Pos2::new(a.left() + 3.0, a.bottom() - rail_h - 3.0), Pos2::new(a.right() - 3.0, a.bottom() - 3.0));
        let head_h = (a.height() * 0.1).clamp(8.0, 13.0);
        let rest_y = rail.top() - head_h * 0.5 - 3.0;
        let lift_h = (a.height() * 0.07).clamp(4.0, 9.0);
        let strike = rest_y - head_h * 0.5 - lift_h;
        let len_max = (strike - a.top() - 16.0).max(12.0);
        // A string's length halves with the octave; the curve is squeezed so
        // the treble keeps a length worth looking at.
        let (x0, x1) = (Self::console_semi_x(bed, 0), Self::console_semi_x(bed, span));
        let t_of = |x: f32| ((x - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
        let short = 2f32.powf(-0.62 * octaves as f32);
        let top_of = |t: f32| strike - len_max * (0.34 + 0.66 * (2f32.powf(-0.62 * octaves as f32 * t) - short) / (1.0 - short));
        let damper_y = strike - len_max * 0.34 * 0.5;

        // The bronze plate above the pins, its edge following the strings.
        let edge_of = |x: f32| top_of(t_of(x)) + 8.0;
        let steps = ((a.width() / 10.0) as usize).max(8);
        let mut m = Mesh::default();
        let mut edge = Vec::with_capacity(steps + 1);
        for k in 0..=steps {
            let x = a.left() + 2.0 + (a.width() - 4.0) * k as f32 / steps as f32;
            let y = edge_of(x);
            m.colored_vertex(Pos2::new(x, a.top() + 2.0), mix(BRONZE, BRASS, 0.25));
            m.colored_vertex(Pos2::new(x, y), mix(BRONZE_DEEP, BRONZE, ((y - a.top()) / a.height()).clamp(0.0, 1.0) * 0.5));
            edge.push(Pos2::new(x, y));
            if k > 0 {
                let i = 2 * k as u32;
                m.add_triangle(i - 2, i - 1, i);
                m.add_triangle(i - 1, i + 1, i);
            }
        }
        p.add(egui::Shape::line(edge.iter().map(|q| *q + egui::vec2(0.0, 2.5)).collect(), Stroke::new(3.0, alpha(SHADE, 0.45))));
        p.add(egui::Shape::mesh(m));
        p.add(egui::Shape::line(edge, Stroke::new(1.5, alpha(BRASS_HI, 0.55))));
        // The round ports cast into the plate where it is deepest.
        for t in [0.5f32, 0.68, 0.86] {
            let x = x0 + (x1 - x0) * t;
            let depth = edge_of(x) - a.top();
            let r = (depth * 0.27).min(kw * 0.62).min(26.0);
            if r >= 6.0 {
                let c = Pos2::new(x, a.top() + depth * 0.46);
                p.circle_filled(c, r + 2.0, mix(BRONZE, BRASS_HI, 0.25));
                p.circle_filled(c, r, rgb(0x15, 0x0F, 0x09));
                arc(p, c, r - 1.0, PI * 1.1, PI * 1.9, Stroke::new(2.0, alpha(SHADE, 0.6)));
                arc(p, c, r + 1.5, PI * 0.1, PI * 0.9, Stroke::new(1.0, alpha(BRASS_HI, 0.5)));
            }
        }
        // Struts between the string sections, at every C.
        for oct in 1..octaves {
            let x = Self::dock_key_rect(bed, oct * 7).left() - 0.5;
            if kw >= 16.0 {
                let top = edge_of(x) - 2.0;
                p.rect_filled(Rect::from_min_max(Pos2::new(x - 1.0, top), Pos2::new(x + 5.0, rail.top())), 0.0, alpha(SHADE, 0.35));
                hgrad(p, Rect::from_min_max(Pos2::new(x - 3.0, top), Pos2::new(x + 3.0, rail.top())), mix(BRONZE, BRASS_HI, 0.3), BRONZE_DEEP);
            }
        }

        let shift = if self.kb_soft { 2.5 } else { 0.0 };
        let wobble = self.ui_time * 46.0;
        for semi in 0..=span {
            let x = Self::console_semi_x(bed, semi);
            let top = top_of(t_of(x));
            let led = self.console_led(semi).min(1.0);
            let wound = semi < 12;
            let ringing = led > 0.02;
            if ringing {
                // A sounding string blurs into the lens it swings through.
                let amp = (kw * 0.2).clamp(1.6, 4.5) * led * (0.8 + 0.2 * (wobble + semi as f32).sin());
                let n = 10;
                let mut lens: Vec<Pos2> = (0..=n).map(|k| {
                    let u = k as f32 / n as f32;
                    Pos2::new(x - amp * (PI * u).sin(), top + (rail.top() - top) * u)
                }).collect();
                lens.extend((1..n).rev().map(|k| {
                    let u = k as f32 / n as f32;
                    Pos2::new(x + amp * (PI * u).sin(), top + (rail.top() - top) * u)
                }));
                p.add(egui::Shape::convex_polygon(lens, alpha(GOLD_LEAF, 0.2 + 0.2 * led), Stroke::NONE));
                p.line_segment([Pos2::new(x, top), Pos2::new(x, rail.top())], Stroke::new(1.5, alpha(IVORY_HI, 0.55 + 0.45 * led)));
            } else if wound {
                p.line_segment([Pos2::new(x, top), Pos2::new(x, rail.top())], Stroke::new(2.2, mix(STRING_COPPER, SHADE, 0.35)));
                p.line_segment([Pos2::new(x - 0.5, top), Pos2::new(x - 0.5, rail.top())], Stroke::new(0.8, alpha(mix(STRING_COPPER, BRASS_HI, 0.45), 0.9)));
            } else if kw >= 30.0 {
                for dx in [-1.4, 1.4] {
                    p.line_segment([Pos2::new(x + dx, top), Pos2::new(x + dx, rail.top())], Stroke::new(1.0, alpha(STRING_STEEL, 0.62)));
                }
            } else {
                p.line_segment([Pos2::new(x, top), Pos2::new(x, rail.top())], Stroke::new(1.0, alpha(STRING_STEEL, 0.7)));
            }
            // The tuning pin on the plate.
            let pin = Pos2::new(x, top);
            p.circle_filled(pin + egui::vec2(0.0, 1.2), 2.6, alpha(SHADE, 0.5));
            p.circle_filled(pin, 2.4, rgb(0xB4, 0xB0, 0xA6));
            p.circle_filled(pin - egui::vec2(0.6, 0.7), 0.9, IVORY_HI);
            // The damper: on its string, or lifted off it.
            let up = ringing || self.kb_sustain || self.kb_sustain_holds(semi);
            let dw = (kw * 0.36).clamp(4.0, 15.0);
            let d = Rect::from_center_size(Pos2::new(x, damper_y - if up { 6.0 } else { 0.0 }), egui::vec2(dw, 9.0));
            p.rect_filled(d.translate(egui::vec2(0.0, if up { 5.0 } else { 2.0 })), 2.0, alpha(SHADE, if up { 0.3 } else { 0.45 }));
            vgrad(p, d, if up { rgb(0x6A, 0x56, 0x40) } else { rgb(0x4A, 0x3A, 0x2B) }, rgb(0x22, 0x1A, 0x12));
            p.rect_filled(Rect::from_min_size(d.min, egui::vec2(d.width(), 1.0)), 0.0, alpha(BRASS_HI, if up { 0.5 } else { 0.22 }));
        }
        // The hammer rail and its felt.
        drop_shadow(p, rail, 2.0, -2.0, 6.0, 0.5);
        vgrad(p, rail, WALNUT_HI, WALNUT_DEEP);
        p.rect_filled(Rect::from_min_size(rail.min, egui::vec2(rail.width(), 2.5)), 0.0, Self::KB_FELT);
        for semi in 0..=span {
            let x = Self::console_semi_x(bed, semi) + shift;
            let led = self.console_led(semi).min(1.0);
            let hw = (kw * 0.4).clamp(4.5, 15.0);
            let head = Rect::from_center_size(Pos2::new(x, rest_y - lift_h * led), egui::vec2(hw, head_h));
            p.line_segment([Pos2::new(x, head.bottom() - 1.0), Pos2::new(x, rail.top() + 2.0)], Stroke::new(1.5, WALNUT_HI));
            if led > 0.02 {
                glow(p, head.center_top(), hw * 1.3, alpha(theme::ACCENT, 0.45 * led));
            }
            p.rect_filled(head.translate(egui::vec2(0.0, 1.5)), hw * 0.5, alpha(SHADE, 0.5));
            let felt = if led > 0.02 { mix(HAMMER_FELT, theme::ACCENT, 0.8) } else { HAMMER_FELT };
            p.rect_filled(head, hw * 0.5, felt);
            p.rect_filled(Rect::from_center_size(head.center() + egui::vec2(0.0, head_h * 0.2), egui::vec2(hw * 0.4, head_h * 0.5)), 1.0, mix(felt, WALNUT, 0.7));
        }
        // The case rim shades what lies under it.
        inner_shadow(p, a.shrink(1.0), (a.height() * 0.16).clamp(8.0, 22.0), 0.6);
        p.rect_stroke(a, 6.0, Stroke::new(2.0, rgb(0x06, 0x05, 0x05)), egui::StrokeKind::Inside);
        p.rect_stroke(a.expand(1.0), 7.0, Stroke::new(1.0, alpha(theme::CREAM, 0.1)), egui::StrokeKind::Outside);
    }

    /// A key released under the sustain pedal still rings (its damper is up).
    fn kb_sustain_holds(&self, semi: usize) -> bool {
        let base = 12 * (self.octave as usize + 1);
        self.kb_sustained[(base + semi).min(127)]
    }

    // ------------------------------------------------------------------
    // Jazz Organ

    fn console_jazz(&mut self, ui: &mut egui::Ui, b: Rect, laid: &mut Laid) {
        let p = ui.painter().clone();
        let cheek = (b.width() * 0.03).clamp(16.0, 44.0).round();
        let kw = Self::console_key_w(b.width() - 2.0 * cheek);
        let foot_h = (b.height() * 0.085).clamp(38.0, 62.0).round();
        let dual = self.kb_dual_manual;
        let n = if dual { 2.0 } else { 1.0 };
        let man_gap = 14.0;
        let gap = 8.0;
        let ctrl_min = if b.height() >= 520.0 { 190.0 } else { 116.0 };
        let mh = (kw * 3.4 + 12.0)
            .min((b.height() - ctrl_min - foot_h - 2.0 * gap - (n - 1.0) * man_gap) / n)
            .min(if dual { f32::INFINITY } else { b.height() * 0.36 })
            .max(60.0)
            .round();
        let manuals_h = n * mh + (n - 1.0) * man_gap;
        let avail = b.height() - manuals_h - foot_h - 2.0 * gap;
        // The tone generator takes the top of a tall cabinet.
        let pitch = (b.width() - 2.0 * cheek - 28.0) / GENERATOR_WHEELS as f32;
        let gen_h = if avail >= 330.0 { (pitch * 3.6 + 50.0).clamp(84.0, 136.0).min(avail - 246.0) } else { 0.0 };
        let ctrl_h = (avail - gen_h - if gen_h > 0.0 { gap } else { 0.0 }).max(80.0);

        // The cabinet.
        wood(&p, b, 10.0, WALNUT, WALNUT_DEEP, true, 11);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, mix(WALNUT, SHADE, 0.5)), egui::StrokeKind::Inside);
        p.line_segment([b.left_top() + egui::vec2(10.0, 1.5), b.right_top() + egui::vec2(-10.0, 1.5)], Stroke::new(1.0, alpha(BRASS_HI, 0.14)));
        let mut y = b.top() + gap;
        if gen_h > 0.0 {
            self.console_jazz_generator(ui, &p, Rect::from_min_size(Pos2::new(b.left() + cheek, y), egui::vec2(b.width() - 2.0 * cheek, gen_h)));
            y += gen_h + gap;
        }
        let ctrl = Rect::from_min_size(Pos2::new(b.left() + cheek, y), egui::vec2(b.width() - 2.0 * cheek, ctrl_h));
        drop_shadow(&p, ctrl, 6.0, 2.0, 7.0, 0.55);
        plate(&p, ctrl, 6.0, rgb(0x1C, 0x1A, 0x18), rgb(0x0E, 0x0D, 0x0C));
        p.rect_stroke(ctrl, 6.0, Stroke::new(1.0, rgb(0x05, 0x04, 0x04)), egui::StrokeKind::Inside);
        p.line_segment([ctrl.left_top() + egui::vec2(6.0, 1.5), ctrl.right_top() + egui::vec2(-6.0, 1.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.1)));
        if ctrl_h >= 170.0 {
            for c in [ctrl.left_top() + egui::vec2(9.0, 9.0), ctrl.right_top() + egui::vec2(-9.0, 9.0), ctrl.left_bottom() + egui::vec2(9.0, -9.0), ctrl.right_bottom() + egui::vec2(-9.0, -9.0)] {
                screw(&p, c, 3.0);
            }
        }
        let man = Rect::from_min_size(Pos2::new(b.left(), ctrl.bottom() + 6.0), egui::vec2(b.width(), manuals_h));
        // The key well, then walnut cheeks either side of the manuals.
        p.rect_filled(Rect::from_min_max(Pos2::new(man.left() + cheek, man.top()), Pos2::new(man.right() - cheek, man.bottom())), 0.0, rgb(0x0A, 0x07, 0x05));
        for r in [
            Rect::from_min_size(man.min + egui::vec2(1.0, 0.0), egui::vec2(cheek - 1.0, manuals_h)),
            Rect::from_min_size(Pos2::new(man.right() - cheek, man.top()), egui::vec2(cheek - 1.0, manuals_h)),
        ] {
            drop_shadow(&p, r, 3.0, 2.0, 6.0, 0.5);
            wood(&p, r, 3.0, WALNUT_HI, WALNUT, false, 23 + r.left() as u32);
            p.line_segment([r.left_top() + egui::vec2(2.0, 1.0), r.right_top() + egui::vec2(-2.0, 1.0)], Stroke::new(1.0, alpha(BRASS_HI, 0.3)));
        }
        let bed_area = Rect::from_min_max(Pos2::new(man.left() + cheek, man.top()), Pos2::new(man.right() - cheek, man.bottom()));
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::JazzOrgan, man_gap);
        // Each manual lies in the shade of what overhangs it.
        let rows = if dual && bed_area.height() >= 130.0 { 2 } else { 1 };
        for k in 0..rows {
            let top = if k == 0 { laid.bed.top() } else { bed_area.bottom() - laid.bed.height() };
            if k == 1 {
                let seam = Rect::from_min_max(Pos2::new(bed_area.left(), laid.bed.bottom() + 1.0), Pos2::new(bed_area.right(), top - 1.0));
                wood(&p, seam, 1.0, WALNUT_HI, WALNUT, true, 57);
                p.line_segment([seam.left_top() + egui::vec2(0.0, 0.5), seam.right_top() + egui::vec2(0.0, 0.5)], Stroke::new(1.0, alpha(BRASS_HI, 0.25)));
            }
            vgrad(&p, Rect::from_min_size(Pos2::new(bed_area.left() + 6.0, top + 6.0), egui::vec2(bed_area.width() - 12.0, (laid.bed.height() * 0.1).clamp(7.0, 16.0))), alpha(SHADE, 0.4), alpha(SHADE, 0.0));
        }
        let foot = Rect::from_min_max(Pos2::new(b.left(), man.bottom()), b.max);
        let swell_w = (b.width() * 0.36).clamp(220.0, 480.0);
        let swell_h = (foot.height() - 14.0).clamp(22.0, 40.0);
        self.console_swell(ui, Rect::from_center_size(foot.center(), egui::vec2(swell_w, swell_h)));
        laid.deck = ctrl;

        let c = ctrl.shrink2(egui::vec2(16.0, 10.0));
        let side_w = (c.width() * 0.25).clamp(178.0, 330.0);
        let left = Rect::from_min_size(c.min, egui::vec2(side_w, c.height()));
        let right = Rect::from_min_size(Pos2::new(c.right() - side_w, c.top()), egui::vec2(side_w, c.height()));
        let mid = Rect::from_min_max(Pos2::new(left.right() + 22.0, c.top()), Pos2::new(right.left() - 22.0, c.bottom()));
        for x in [left.right() + 11.0, right.left() - 11.0] {
            engraved_rule(&p, Pos2::new(x, c.top() + 2.0), Pos2::new(x, c.bottom() - 2.0));
        }

        // Left: preset keys (reverse-coloured, like the preset keys left of
        // a tonewheel manual), then percussion.
        let leg_h = 15.0;
        let tab_h = (c.height() * 0.17).clamp(22.0, 34.0);
        let pk_w = (left.width() - 3.0 * 5.0) / 4.0;
        let pk_h = (c.height() - tab_h - 2.0 * leg_h - 12.0).min(pk_w * 3.3).max(24.0);
        legend(ui, left.left_top(), Align::Min, "PRESETS", 9.0, theme::CREAM_DIM);
        let keys_r = Rect::from_min_size(Pos2::new(left.left(), left.top() + leg_h), egui::vec2(left.width(), pk_h));
        for (i, preset) in crate::keyboard::JAZZ_ORGAN_PRESETS.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(keys_r.left() + i as f32 * (pk_w + 5.0), keys_r.top()), egui::vec2(pk_w, keys_r.height()));
            if preset_key(ui, cid(("con_preset", i)), r, preset.short, &preset.drawbars).on_hover_text(preset.label).clicked() {
                self.kb_jazz_preset(preset);
            }
        }
        let perc = crate::organ::step(self.kb_organ_value(crate::organ::P_PERC), 3);
        // A tall panel has room for a third group: the manuals move here
        // from under the knobs, and percussion sits between the two.
        let three = c.height() - leg_h - pk_h - 2.0 * (leg_h + tab_h) >= 44.0;
        let py = if three {
            let (from, to) = (keys_r.bottom(), left.bottom() - leg_h - tab_h);
            ((from + to) * 0.5 - (leg_h + tab_h) * 0.5 + leg_h).round()
        } else {
            left.bottom() - tab_h
        };
        legend(ui, Pos2::new(left.left(), py - leg_h), Align::Min, "PERCUSSION", 9.0, theme::CREAM_DIM);
        let tw = (left.width() - 6.0) / 2.0;
        if tablet(ui, cid("con_perc"), Rect::from_min_size(Pos2::new(left.left(), py), egui::vec2(tw, tab_h)), "PERC", "", perc > 0, BRASS)
            .on_hover_text("percussion on/off (CC 80)")
            .clicked()
        {
            self.kb_set_organ_param(crate::organ::P_PERC, if perc > 0 { 0.0 } else { 0.5 });
        }
        if tablet(ui, cid("con_perc_soft"), Rect::from_min_size(Pos2::new(left.left() + tw + 6.0, py), egui::vec2(tw, tab_h)), "SOFT", "", perc == 2, BRASS)
            .on_hover_text("soft percussion")
            .clicked()
        {
            self.kb_set_organ_param(crate::organ::P_PERC, if perc == 2 { 0.5 } else { 1.0 });
        }

        // Middle: the nine drawbars, centred.
        let n_bars = crate::organ::DRAWBARS as f32;
        let bar_w = ((mid.width() - 8.0 * 10.0 - 14.0) / n_bars).clamp(18.0, 56.0).floor();
        let bar_gap = ((mid.width() - n_bars * bar_w - 14.0) / 8.0).clamp(4.0, bar_w * 0.55);
        let total = n_bars * bar_w + 8.0 * bar_gap + 14.0;
        let mut x = mid.center().x - total * 0.5;
        for i in 0..crate::organ::DRAWBARS {
            if i == 2 { x += 14.0; } // the two sub-octave bars stand apart
            let r = Rect::from_min_size(Pos2::new(x, mid.top()), egui::vec2(bar_w, mid.height()));
            let pos = crate::organ::drawbar_position(self.kb_organ_value(crate::organ::drawbar_param(i)));
            if let Some(np) = drawbar(ui, cid(("con_drawbar", i)), r, i, crate::organ::FOOTAGES[i], pos) {
                if np != pos {
                    self.kb_set_organ_param(crate::organ::drawbar_param(i), crate::organ::drawbar_value(np));
                }
            }
            x += bar_w + bar_gap;
        }

        // Right: the rotary speaker (rotor + STOP/SLOW/FAST), DRIVE,
        // REVERB, and the second manual.
        let rotary = crate::organ::step(self.kb_organ_value(crate::organ::P_LESLIE), 3);
        let col_a = Rect::from_min_size(right.min, egui::vec2(right.width() * 0.54, right.height()));
        let col_b = Rect::from_min_max(Pos2::new(col_a.right() + 10.0, right.top()), right.max);
        legend(ui, col_a.left_top(), Align::Min, "ROTARY", 9.0, theme::CREAM_DIM);
        let sw_h = tab_h;
        let sw_y = col_a.bottom() - sw_h;
        // The speed tablets take the whole section once the manuals moved.
        let sw_w = (if three { right.width() } else { col_a.width() } - 6.0) / 3.0;
        for (i, name) in ["STOP", "SLOW", "FAST"].iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(col_a.left() + i as f32 * (sw_w + 3.0), sw_y), egui::vec2(sw_w, sw_h));
            if tablet(ui, cid(("con_leslie", i)), r, name, "", rotary == i, theme::LIVE_TEAL).clicked() {
                self.kb_set_organ_param(crate::organ::P_LESLIE, i as f32 / 2.0);
                self.kb_strip_pulse(0.5);
            }
        }
        let rotor_area = Rect::from_min_max(Pos2::new(col_a.left(), col_a.top() + leg_h + 2.0), Pos2::new(col_a.right(), sw_y - 8.0));
        let d = rotor_area.width().min(rotor_area.height()).min(190.0);
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        // Spin-up and spin-down take a moment, as on the real rotor.
        let target_speed = [0.0, 0.8, 5.5][rotary] * TAU;
        self.kb_rotor.1 += (target_speed - self.kb_rotor.1) * (dt * 1.6).min(1.0);
        self.kb_rotor.0 = (self.kb_rotor.0 + self.kb_rotor.1 * dt) % TAU;
        if d >= 34.0 {
            rotor(&p, rotor_area.center(), d * 0.5, self.kb_rotor.0, self.kb_rotor.1, rotary > 0);
            if rotary > 0 { ui.ctx().request_repaint(); }
        }
        // DRIVE over REVERB when the panel is tall, side by side when not.
        let knobs_h = col_b.height() - sw_h - 8.0;
        let stacked = knobs_h >= 170.0;
        let knob_h = if stacked { knobs_h / 2.0 } else { knobs_h };
        let knob_w = if stacked { col_b.width() } else { col_b.width() / 2.0 };
        for (k, (param, label)) in [(crate::organ::P_DRIVE, "DRIVE"), (crate::organ::P_REVERB, "REVERB")].into_iter().enumerate() {
            let at = if stacked { egui::vec2(0.0, k as f32 * knob_h) } else { egui::vec2(k as f32 * knob_w, 0.0) };
            let cell = Rect::from_min_size(col_b.min + at, egui::vec2(knob_w, knob_h));
            let v = self.kb_organ_value(param);
            if let Some(nv) = knob(ui, cid(("con_organ_knob", k)), cell, label, v, KnobLook::Bakelite, 104.0) {
                self.kb_set_organ_param(param, nv);
            }
        }
        let dual_r = if three {
            legend(ui, Pos2::new(left.left(), left.bottom() - tab_h - leg_h), Align::Min, "MANUALS", 9.0, theme::CREAM_DIM);
            Rect::from_min_size(Pos2::new(left.left(), left.bottom() - tab_h), egui::vec2(left.width(), tab_h))
        } else {
            Rect::from_min_size(Pos2::new(col_b.left(), col_b.bottom() - sw_h), egui::vec2(col_b.width(), sw_h))
        };
        if tablet(ui, cid("con_dual"), dual_r, "2 MANUALS", "", dual, theme::LIVE_TEAL)
            .on_hover_text("upper manual on MIDI channel 1, lower on channel 2 (setBfree)")
            .clicked()
        {
            self.kb_dual_manual = !self.kb_dual_manual;
        }
    }

    /// The tone generator: ninety-one wheels on two shafts, low notes to
    /// the left. A sounding key lights the wheel of every drawbar that is
    /// pulled (its pitch, the octave under it, the fifths and octaves over
    /// it), as bright as the bar is far out: the registration, seen.
    fn console_jazz_generator(&self, ui: &egui::Ui, p: &egui::Painter, g: Rect) {
        well(p, g, 5.0, rgb(0x0C, 0x0A, 0x08));
        let mut level = [0.0f32; GENERATOR_WHEELS];
        let mut sounding = false;
        for (bar, harmonic) in DRAWBAR_SEMITONES.iter().enumerate() {
            let pull = crate::organ::drawbar_position(self.kb_organ_value(crate::organ::drawbar_param(bar))) as f32 / 8.0;
            for (note, led) in self.note_leds.iter().enumerate().filter(|(_, l)| **l > 0.02) {
                sounding = true;
                // Past either end the generator folds back by octaves.
                let mut w = note as i32 + harmonic - 24;
                while w < 0 { w += 12; }
                while w >= GENERATOR_WHEELS as i32 { w -= 12; }
                level[w as usize] = (level[w as usize] + led * pull).min(1.0);
            }
        }
        let leg = legend(ui, g.left_top() + egui::vec2(12.0, 7.0), Align::Min, "TONE GENERATOR", 8.5, theme::CREAM_DIM);
        let area = Rect::from_min_max(Pos2::new(g.left() + 14.0, leg.bottom() + 5.0), Pos2::new(g.right() - 14.0, g.bottom() - 9.0));
        let pitch = area.width() / GENERATOR_WHEELS as f32;
        let d = (pitch * 1.76).min((area.height() - 10.0) * 0.5).max(4.0);
        let rows = [area.center().y - d * 0.5, area.center().y + d * 0.5];
        for y in rows {
            p.line_segment([Pos2::new(area.left(), y), Pos2::new(area.right(), y)], Stroke::new(2.0, rgb(0x3A, 0x38, 0x34)));
            p.line_segment([Pos2::new(area.left(), y - 0.5), Pos2::new(area.right(), y - 0.5)], Stroke::new(0.6, alpha(ALU, 0.5)));
        }
        for w in 0..GENERATOR_WHEELS {
            let row = w % 2;
            let c = Pos2::new(area.left() + pitch * (w as f32 + 0.5), rows[row]);
            let lv = level[w];
            let r = d * 0.5 - 0.5;
            // The pickup facing the wheel's rim.
            let pick = Pos2::new(c.x, if row == 0 { c.y - r - 4.0 } else { c.y + r + 4.0 });
            if lv > 0.02 {
                glow(p, pick, 5.0, alpha(theme::CABLE_AMBER, 0.6 * lv));
                glow(p, c, r * 1.7, alpha(theme::CABLE_AMBER, 0.3 * lv));
            }
            p.rect_filled(Rect::from_center_size(pick, egui::vec2(2.5, 4.0)), 0.5, if lv > 0.02 { mix(BRASS, BRASS_HI, lv) } else { rgb(0x2A, 0x27, 0x23) });
            p.circle_filled(c + egui::vec2(0.0, 1.5), r, alpha(SHADE, 0.5));
            disc(p, c, r, mix(rgb(0x74, 0x70, 0x68), BRASS_HI, lv * 0.85), mix(rgb(0x2C, 0x2A, 0x27), theme::CABLE_AMBER, lv * 0.7));
            // Teeth: more of them up the scale; they turn while notes sound.
            let teeth = 4 + 2 * (w / 12);
            let turn = if sounding { self.ui_time * (0.8 + w as f32 * 0.06) } else { 0.0 } + noise(w as u32) * TAU;
            if r >= 4.0 {
                for k in 0..teeth {
                    let a = turn + TAU * k as f32 / teeth as f32;
                    let dir = egui::vec2(a.cos(), a.sin());
                    p.line_segment([c + dir * (r * 0.62), c + dir * (r * 0.98)], Stroke::new(1.0, alpha(SHADE, 0.55)));
                }
            }
            p.circle_filled(c, (r * 0.24).max(1.0), rgb(0x1A, 0x18, 0x16));
        }
    }

    // ------------------------------------------------------------------
    // Pipe Organ

    fn console_pipe(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let aeolus = target.filter(|&u| self.is_aeolus_unit(u));
        let side = (b.width() * 0.014).clamp(9.0, 22.0).round();
        let kw = Self::console_key_w(b.width() - 2.0 * side);
        let bottom_h = (b.height() * 0.078).clamp(42.0, 66.0).round();
        let keys_h = (kw * 3.4 + 12.0).min(b.height() * 0.28).max(70.0).round();
        let jamb_need = (b.height() * 0.40).max(200.0);
        let rest = b.height() - keys_h - bottom_h - jamb_need - 26.0;
        let facade_h = if rest >= 80.0 { rest.min(270.0) } else { 0.0 };
        let jamb_h = b.height() - keys_h - bottom_h - facade_h - 26.0;

        wood(&p, b, 10.0, mix(WALNUT, WALNUT_DEEP, 0.35), WALNUT_DEEP, true, 71);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, mix(WALNUT, SHADE, 0.5)), egui::StrokeKind::Inside);
        p.line_segment([b.left_top() + egui::vec2(10.0, 1.5), b.right_top() + egui::vec2(-10.0, 1.5)], Stroke::new(1.0, alpha(BRASS_HI, 0.14)));
        let mut y = b.top() + 8.0;
        let facade = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, facade_h));
        if facade_h > 0.0 { y += facade_h + 6.0; }
        let jambs = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, jamb_h));
        y += jamb_h + 6.0;
        let bed_area = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, keys_h));
        y += keys_h + 4.0;
        let bottom = Rect::from_min_max(Pos2::new(b.left() + side, y), Pos2::new(b.right() - side, b.bottom() - 4.0));

        p.rect_filled(bed_area, 2.0, rgb(0x0A, 0x07, 0x05));
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::PipeOrgan, 0.0);
        vgrad(&p, Rect::from_min_size(laid.bed.min + egui::vec2(6.0, 6.0), egui::vec2(laid.bed.width() - 12.0, (laid.bed.height() * 0.1).clamp(7.0, 16.0))), alpha(SHADE, 0.4), alpha(SHADE, 0.0));
        if facade_h > 0.0 {
            self.console_pipe_facade(&p, facade, laid.bed);
        }
        laid.deck = jambs;

        // Stop jambs: III and II on the left, I and Pedal on the right,
        // every division's drawknobs at once (no tabs to page through).
        let center_w = (jambs.width() * 0.2).clamp(140.0, 300.0);
        let jw = (jambs.width() - center_w - 24.0) / 2.0;
        let left = Rect::from_min_size(jambs.min, egui::vec2(jw, jambs.height()));
        let right = Rect::from_min_size(Pos2::new(jambs.right() - jw, jambs.top()), egui::vec2(jw, jambs.height()));
        let center = Rect::from_min_max(Pos2::new(left.right() + 12.0, jambs.top()), Pos2::new(right.left() - 12.0, jambs.bottom()));
        let drawn = self.kb_pipe_drawn();
        for (jamb, divs) in [(left, [0usize, 1]), (right, [2, 3])] {
            drop_shadow(&p, jamb, 6.0, 2.0, 7.0, 0.5);
            plate(&p, jamb, 6.0, rgb(0x1E, 0x16, 0x0E), rgb(0x12, 0x0D, 0x08));
            p.rect_stroke(jamb, 6.0, Stroke::new(1.0, rgb(0x08, 0x05, 0x03)), egui::StrokeKind::Inside);
            p.rect_stroke(jamb.shrink(1.0), 5.0, Stroke::new(1.0, alpha(WALNUT_HI, 0.55)), egui::StrokeKind::Inside);
            let inner = jamb.shrink2(egui::vec2(10.0, 8.0));
            let gh = (inner.height() - 8.0) / 2.0;
            for (k, &d) in divs.iter().enumerate() {
                let g = Rect::from_min_size(Pos2::new(inner.left(), inner.top() + k as f32 * (gh + 8.0)), egui::vec2(inner.width(), gh));
                if k == 1 {
                    engraved_rule(&p, Pos2::new(inner.left(), g.top() - 4.0), Pos2::new(inner.right(), g.top() - 4.0));
                }
                self.console_division(ui, &p, g, d, drawn[d]);
            }
        }

        // Center: the name plate, the coupler tablets, and the swell box.
        let plate_h = (center.height() * 0.22).clamp(34.0, 62.0);
        let name_plate = Rect::from_min_size(center.min, egui::vec2(center.width(), plate_h));
        drop_shadow(&p, name_plate, 4.0, 2.0, 6.0, 0.55);
        plate(&p, name_plate, 4.0, mix(BRASS, BRASS_HI, 0.15), mix(BRASS, BRASS_DEEP, 0.75));
        p.rect_stroke(name_plate, 4.0, Stroke::new(1.0, mix(BRASS_DEEP, SHADE, 0.6)), egui::StrokeKind::Inside);
        p.line_segment([name_plate.left_top() + egui::vec2(4.0, 1.5), name_plate.right_top() + egui::vec2(-4.0, 1.5)], Stroke::new(1.0, alpha(BRASS_HI, 0.7)));
        if name_plate.width() >= 120.0 {
            for c in [name_plate.left_center() + egui::vec2(8.0, 0.0), name_plate.right_center() - egui::vec2(8.0, 0.0)] {
                screw(&p, c, 2.5);
            }
        }
        let name = aeolus.map(|t| sentence_case(&self.units[t].name)).unwrap_or_else(|| "Pipe Organ".into());
        let two = plate_h >= 46.0;
        let (name, font) = fit_text(ui, &name, name_plate.width() - 36.0, (plate_h * 0.36).clamp(12.0, 21.0), 9.0, true);
        p.text(name_plate.center() - egui::vec2(0.0, if two { 7.0 } else { 0.0 }), egui::Align2::CENTER_CENTER, name, font, IVORY_INK);
        if two {
            let sub = if aeolus.is_some() { self.console_subtitle(target) } else { "preview: load Aeolus to play it".into() };
            legend_fit(ui, Pos2::new(name_plate.center().x, name_plate.bottom() - 17.0), Align::Center, &sub.to_uppercase(), name_plate.width() - 36.0, 8.0, IVORY_INK);
        }
        let couplers = self.kb_pipe_couplers();
        let links: Vec<(usize, usize, String, String)> = (0..4).flat_map(|d| {
            crate::aeolus::DIVISIONS[d].links.iter().enumerate().map(move |(l, target_name)| {
                let short = target_name.trim_start_matches("Manual ");
                let short = if short == "Pedal" { "P" } else { short };
                let tab_name = crate::aeolus::division_tab(d);
                (d, l, format!("{short}/{tab_name}"), format!("{target_name} sounds from {}", crate::aeolus::DIVISIONS[d].name))
            })
        }).collect();
        legend(ui, Pos2::new(center.left(), name_plate.bottom() + 9.0), Align::Min, "COUPLERS", 9.0, BRASS);
        let grid = Rect::from_min_max(Pos2::new(center.left(), name_plate.bottom() + 25.0), center.max);
        let rows = links.len().div_ceil(2);
        let th = ((grid.height() - (rows as f32 - 1.0) * 5.0) / rows as f32).clamp(18.0, 30.0);
        let tw = (grid.width() - 6.0) / 2.0;
        let mut used = grid.top();
        for (i, (d, l, label, tip)) in links.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(grid.left() + (i % 2) as f32 * (tw + 6.0), grid.top() + (i / 2) as f32 * (th + 5.0)), egui::vec2(tw, th));
            if r.bottom() > grid.bottom() + 0.5 { break; }
            used = r.bottom();
            let on = (couplers[*d] >> l) & 1 == 1;
            if tablet(ui, cid(("con_coupler", *d, *l)), r, label, "", on, theme::LIVE_TEAL).on_hover_text(tip.as_str()).clicked() {
                self.kb_pipe_coupler_toggle(*d, *l);
            }
        }
        // What the swell shoe moves: the shutters of the swell box.
        let shutters = Rect::from_min_max(Pos2::new(center.left(), used + 27.0), center.max);
        if shutters.height() >= 46.0 {
            legend(ui, Pos2::new(center.left(), used + 11.0), Align::Min, "SWELL BOX", 9.0, BRASS);
            swell_box(&p, shutters, self.kb_swell);
        }

        // Under the manual: thumb pistons (instant registrations) and the
        // swell shoe.
        let swell_w = (bottom.width() * 0.28).clamp(180.0, 380.0);
        let swell_h = (bottom.height() - 8.0).clamp(22.0, 40.0);
        let swell = Rect::from_min_size(Pos2::new(bottom.right() - swell_w, bottom.center().y - swell_h * 0.5), egui::vec2(swell_w, swell_h));
        self.console_swell(ui, swell);
        let pist = Rect::from_min_max(bottom.min, Pos2::new(swell.left() - 16.0, bottom.bottom()));
        let n = crate::keyboard::PIPE_ORGAN_PISTONS.len() as f32;
        let pd = (bottom.height() - 17.0).clamp(18.0, 36.0);
        let cell_w = (pist.width() / n).min(116.0);
        let x0 = pist.center().x - cell_w * n * 0.5;
        for (i, (short, name)) in crate::keyboard::PIPE_ORGAN_PISTONS.iter().enumerate() {
            let cell = Rect::from_min_size(Pos2::new(x0 + i as f32 * cell_w, pist.top()), egui::vec2(cell_w, pist.height()));
            let label = short.split_once(' ').map(|(_, n)| n).unwrap_or(short);
            if piston(ui, cid(("con_piston", i)), cell, pd, &(i + 1).to_string(), label).on_hover_text(format!("piston: {name}")).clicked() {
                self.kb_pipe_piston(i);
                self.kb_strip_pulse(0.7);
            }
        }
    }

    /// One division on a stop jamb: its name, its tremulant tablet, and its
    /// stops: drawknobs (drawn = pulled out, ivory face lit) where each has
    /// room for its name, stop tablets that carry the name where not.
    fn console_division(&mut self, ui: &mut egui::Ui, p: &egui::Painter, g: Rect, d: usize, drawn: u32) {
        let div = &crate::aeolus::DIVISIONS[d];
        let head_h = 22.0;
        let trem_w = 60.0f32.min(g.width() * 0.4);
        let title = legend_fit(ui, Pos2::new(g.left() + 1.0, g.top() + 5.0), Align::Min, &div.name.to_uppercase(), g.width() - trem_w - 16.0, 10.0, GOLD_LEAF);
        let rule_y = g.top() + head_h * 0.5;
        if g.right() - trem_w - 10.0 > title.right() + 10.0 {
            hgrad(p, Rect::from_min_max(Pos2::new(title.right() + 10.0, rule_y), Pos2::new(g.right() - trem_w - 10.0, rule_y + 1.0)), alpha(BRASS, 0.4), alpha(BRASS, 0.05));
        }
        let trem_r = Rect::from_min_size(Pos2::new(g.right() - trem_w, g.top() + 1.0), egui::vec2(trem_w, head_h - 4.0));
        if tablet(ui, cid(("con_trem", d)), trem_r, "TREM", "", self.kb_pipe_trem(d), theme::LIVE_TEAL)
            .on_hover_text(format!("tremulant of {}", div.name))
            .clicked()
        {
            self.kb_pipe_trem_toggle(d);
        }
        let area = Rect::from_min_max(Pos2::new(g.left(), g.top() + head_h + 4.0), g.max);
        let n = div.stops.len();
        let (cols, rows, d_knob, font) = knob_grid(n, area);
        if d_knob < 26.0 {
            let rows = ((area.height() / 16.5).floor() as usize).clamp(1, n.max(1));
            let cols = n.div_ceil(rows);
            let cw = area.width() / cols as f32;
            let ch = (area.height() / rows as f32).min(26.0);
            for (i, stop) in div.stops.iter().enumerate() {
                let cell = Rect::from_min_size(Pos2::new(area.left() + (i % cols) as f32 * cw, area.top() + (i / cols) as f32 * ch), egui::vec2(cw - 3.0, ch - 3.0));
                let (name, foot) = stop_label(stop);
                let on = (drawn >> i) & 1 == 1;
                if tablet(ui, cid(("con_stop", d, i)), cell, &name, &foot, on, theme::LIVE_TEAL).on_hover_text(format!("{name} {foot}")).clicked() {
                    self.kb_pipe_toggle_stop(d, i);
                }
            }
            return;
        }
        let cw = area.width() / cols as f32;
        let ch = (area.height() / rows as f32).min(d_knob + 2.0 * font * 1.25 + 16.0);
        let top = area.top() + (area.height() - ch * rows as f32) * 0.5;
        for (i, stop) in div.stops.iter().enumerate() {
            let cell = Rect::from_min_size(Pos2::new(area.left() + (i % cols) as f32 * cw, top + (i / cols) as f32 * ch), egui::vec2(cw, ch));
            let (name, foot) = stop_label(stop);
            let on = (drawn >> i) & 1 == 1;
            if drawknob(ui, cid(("con_stop", d, i)), cell, d_knob, &name, &foot, on, font)
                .on_hover_text(format!("{} {}", name, foot))
                .clicked()
            {
                self.kb_pipe_toggle_stop(d, i);
            }
        }
    }

    /// The case pipes: one per key, bass to the left, their lengths halving
    /// up the scale, each speaking (mouth lit) while its note sounds. A
    /// carved shade closes the case above them.
    fn console_pipe_facade(&self, p: &egui::Painter, f: Rect, bed: Rect) {
        well(p, f, 6.0, rgb(0x0D, 0x09, 0x06));
        let octaves = Self::dock_octaves(bed);
        let span = 12 * octaves;
        let inner = f.shrink2(egui::vec2(10.0, 0.0));
        let step = inner.width() / (span as f32 + 1.0);
        let impost_h = (f.height() * 0.07).clamp(6.0, 12.0);
        let base_y = f.bottom() - impost_h - 2.0;
        let full = base_y - f.top() - 12.0;
        let short = 2f32.powf(-0.6 * octaves as f32);
        let len_of = |t: f32| full * (0.36 + 0.64 * (2f32.powf(-0.6 * octaves as f32 * t) - short) / (1.0 - short));
        let x_of = |semi: usize| inner.left() + step * (semi as f32 + 0.5);

        // The pipe shade: carved wood from the case top down to the pipes.
        let mut m = Mesh::default();
        let mut edge = Vec::with_capacity(span + 3);
        let steps = span + 2;
        for k in 0..=steps {
            let x = f.left() + 3.0 + (f.width() - 6.0) * k as f32 / steps as f32;
            let t = ((x - x_of(0)) / (x_of(span) - x_of(0)).max(1.0)).clamp(0.0, 1.0);
            let y = (base_y - len_of(t) - 7.0).max(f.top() + 3.0);
            m.colored_vertex(Pos2::new(x, f.top() + 3.0), mix(WALNUT, WALNUT_DEEP, 0.2));
            m.colored_vertex(Pos2::new(x, y), mix(WALNUT_DEEP, SHADE, 0.25));
            edge.push(Pos2::new(x, y));
            if k > 0 {
                let i = 2 * k as u32;
                m.add_triangle(i - 2, i - 1, i);
                m.add_triangle(i - 1, i + 1, i);
            }
        }
        p.add(egui::Shape::mesh(m));
        // Its fretwork: a lattice of small pierced lozenges.
        let cell = (f.height() * 0.085).clamp(9.0, 15.0);
        let mut row = 0;
        let mut y = f.top() + 3.0 + cell * 0.8;
        while y < base_y {
            let mut x = f.left() + 3.0 + cell * if row % 2 == 0 { 0.8 } else { 1.3 };
            while x < f.right() - cell * 0.6 {
                let t = ((x - x_of(0)) / (x_of(span) - x_of(0)).max(1.0)).clamp(0.0, 1.0);
                if y + cell * 0.75 < base_y - len_of(t) - 7.0 {
                    let h = cell * 0.26;
                    p.add(egui::Shape::convex_polygon(
                        vec![Pos2::new(x, y - h), Pos2::new(x + h, y), Pos2::new(x, y + h), Pos2::new(x - h, y)],
                        alpha(SHADE, 0.5),
                        Stroke::new(0.6, alpha(GOLD_LEAF, 0.22)),
                    ));
                }
                x += cell;
            }
            y += cell * 0.5;
            row += 1;
        }
        p.add(egui::Shape::line(edge.iter().map(|q| *q + egui::vec2(0.0, 2.0)).collect(), Stroke::new(3.0, alpha(SHADE, 0.5))));
        p.add(egui::Shape::line(edge, Stroke::new(1.5, alpha(GOLD_LEAF, 0.75))));

        for semi in 0..=span {
            let x = x_of(semi);
            let t = semi as f32 / span.max(1) as f32;
            let pw = (step * (0.84 - 0.2 * t)).clamp(3.0, 30.0);
            let len = len_of(t);
            let foot_h = (len * 0.2).clamp(8.0, 40.0);
            let body = Rect::from_min_max(Pos2::new(x - pw * 0.5, base_y - len), Pos2::new(x + pw * 0.5, base_y - foot_h));
            let led = self.console_led(semi).min(1.0);
            let speaks = led > 0.02;
            let (edge_c, mid_c, hi_c) = if speaks {
                (mix(PIPE_TIN, GOLD_LEAF, 0.5), mix(TIN_HI, GOLD_LEAF, 0.35), IVORY_HI)
            } else {
                (TIN_DEEP, PIPE_TIN, TIN_HI)
            };
            if speaks {
                soft(p, body.expand(pw * 0.3), pw * 0.5, pw * 0.9, alpha(GOLD_LEAF, 0.22 * led));
            }
            // A cylinder under one light: dark edge, a bright stripe left
            // of the middle, the tin's own grey, dark edge again.
            let stops = [(0.0, edge_c), (0.3, hi_c), (0.62, mid_c), (1.0, edge_c)];
            lathe(p, &[(body.top(), body.left(), body.right()), (body.bottom(), body.left(), body.right())], &stops, |t| 0.18 * t);
            // The foot: a cone narrowing to the toe.
            lathe(p, &[(body.bottom(), body.left(), body.right()), (base_y, x - pw * 0.15, x + pw * 0.15)], &stops, |t| 0.22 + 0.18 * t);
            p.line_segment([body.left_top(), body.right_top()], Stroke::new(1.0, alpha(hi_c, 0.8)));
            // The mouth: the upper lip rises to a point, the lower lip
            // falls to one, the slit between them glows while it speaks.
            if pw >= 6.0 {
                let my = body.bottom();
                let lw = pw * 0.36;
                let lip = mix(edge_c, mid_c, 0.35);
                p.add(egui::Shape::convex_polygon(
                    vec![Pos2::new(x - lw, my - 1.0), Pos2::new(x + lw, my - 1.0), Pos2::new(x + lw, my - pw * 0.35), Pos2::new(x, my - pw * 0.95), Pos2::new(x - lw, my - pw * 0.35)],
                    lip,
                    Stroke::new(0.8, alpha(SHADE, 0.45)),
                ));
                p.add(egui::Shape::convex_polygon(
                    vec![Pos2::new(x - lw, my + 1.0), Pos2::new(x + lw, my + 1.0), Pos2::new(x, my + pw * 0.6)],
                    mix(lip, SHADE, 0.3),
                    Stroke::NONE,
                ));
            }
            let slit = Rect::from_center_size(Pos2::new(x, body.bottom()), egui::vec2((pw * 0.66).max(2.0), (pw * 0.16).clamp(1.5, 3.5)));
            if speaks {
                glow(p, slit.center(), pw * 0.9, alpha(theme::ACCENT, 0.4 * led));
            }
            p.rect_filled(slit, 0.5, if speaks { mix(theme::ACCENT, BRASS_HI, 0.25) } else { rgb(0x12, 0x0F, 0x0C) });
        }
        // The impost the pipes stand on.
        let impost = Rect::from_min_max(Pos2::new(f.left() + 2.0, base_y), Pos2::new(f.right() - 2.0, f.bottom() - 2.0));
        vgrad(p, impost, WALNUT_HI, WALNUT);
        p.line_segment([impost.left_top() + egui::vec2(0.0, 0.5), impost.right_top() + egui::vec2(0.0, 0.5)], Stroke::new(1.0, alpha(GOLD_LEAF, 0.6)));
        p.rect_stroke(f, 6.0, Stroke::new(2.0, rgb(0x07, 0x05, 0x03)), egui::StrokeKind::Inside);
    }

    // ------------------------------------------------------------------
    // Synth

    fn console_synth(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let cheek = (b.width() * 0.014).clamp(8.0, 20.0).round();
        let well_w = (b.width() * 0.09).clamp(84.0, 150.0).round();
        let inset = 12.0;
        let panel = Rect::from_min_max(Pos2::new(b.left() + cheek, b.top()), Pos2::new(b.right() - cheek, b.bottom()));
        let kw = Self::console_key_w(panel.width() - well_w - 2.0 * inset);
        let keys_h = (kw * 4.6 + 12.0).min(b.height() * 0.44).max(80.0).round();
        // Walnut end cheeks, the metal panel between them.
        wood(&p, b, 10.0, WALNUT_HI, WALNUT, false, 91);
        vgrad(&p, panel, METAL_HI, METAL);
        vgrad(&p, Rect::from_min_size(panel.min, egui::vec2(panel.width(), 2.0)), alpha(theme::CREAM, 0.16), alpha(theme::CREAM, 0.0));
        for x in [panel.left(), panel.right() - 1.0] {
            p.line_segment([Pos2::new(x + 0.5, b.top() + 2.0), Pos2::new(x + 0.5, b.bottom() - 2.0)], Stroke::new(1.0, alpha(SHADE, 0.6)));
        }
        p.rect_stroke(b, 10.0, Stroke::new(1.0, mix(WALNUT, SHADE, 0.55)), egui::StrokeKind::Inside);
        let ctrl = Rect::from_min_max(panel.min + egui::vec2(inset, inset), Pos2::new(panel.right() - inset, b.bottom() - keys_h - inset - 10.0));
        laid.deck = ctrl;
        let keys_row = Rect::from_min_max(Pos2::new(panel.left() + inset, b.bottom() - keys_h - inset), Pos2::new(panel.right() - inset, b.bottom() - inset));
        engraved_rule(&p, Pos2::new(panel.left() + 6.0, keys_row.top() - 6.0), Pos2::new(panel.right() - 6.0, keys_row.top() - 6.0));
        let wheels = Rect::from_min_size(keys_row.min, egui::vec2(well_w, keys_row.height()));
        let bed_area = Rect::from_min_max(Pos2::new(wheels.right() + 6.0, keys_row.top()), keys_row.max);
        p.rect_filled(bed_area, 3.0, rgb(0x08, 0x08, 0x07));
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::Synth, 0.0);
        vgrad(&p, Rect::from_min_size(laid.bed.min + egui::vec2(6.0, 6.0), egui::vec2(laid.bed.width() - 12.0, (laid.bed.height() * 0.08).clamp(6.0, 16.0))), alpha(SHADE, 0.4), alpha(SHADE, 0.0));

        // The wheel well: PITCH (springs to center) and MOD (holds).
        well(&p, wheels, 6.0, rgb(0x0C, 0x0B, 0x0A));
        let wh = wheels.height() - 34.0;
        let ww = ((wheels.width() - 34.0) / 2.0).max(16.0);
        for (k, (label, spring)) in [("PITCH", true), ("MOD", false)].into_iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(wheels.left() + 12.0 + k as f32 * (ww + 10.0), wheels.top() + 12.0), egui::vec2(ww, wh));
            let resp = ui.interact(r, cid(("con_wheel", k)), egui::Sense::click_and_drag());
            let value = self.kb_wheel_drive(&resp, r, spring);
            wheel(&p, r, if spring { (value + 1.0) * 0.5 } else { value }, spring, resp.hovered() || resp.dragged());
            legend(ui, Pos2::new(r.center().x, wheels.bottom() - 16.0), Align::Center, label, 8.0, theme::CREAM_DIM);
            laid.strip[k] = r;
        }

        // Control panel: display + function row + macro knobs on the left,
        // the X/Y pad on the right.
        let mains = self.console_mains(target, 8);
        let pad_side = (ctrl.height() - 22.0).min(ctrl.width() * 0.34).max(0.0);
        let show_pad = pad_side >= 90.0;
        let pad_w = if show_pad { (pad_side * 1.3).min(ctrl.width() * 0.4) } else { 0.0 };
        let left = Rect::from_min_max(ctrl.min, Pos2::new(ctrl.right() - pad_w - if show_pad { 22.0 } else { 0.0 }, ctrl.bottom()));
        let lcd_h = (ctrl.height() * 0.32).clamp(44.0, 210.0);
        let bezel = Rect::from_min_size(left.min, egui::vec2(left.width(), lcd_h));
        drop_shadow(&p, bezel, 6.0, 1.0, 4.0, 0.5);
        plate(&p, bezel, 6.0, rgb(0x10, 0x10, 0x0F), rgb(0x06, 0x06, 0x06));
        p.line_segment([bezel.left_bottom() + egui::vec2(6.0, -0.5), bezel.right_bottom() + egui::vec2(-6.0, -0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.1)));
        let lcd = bezel.shrink(4.0);
        vgrad(&p, lcd, LCD, LCD_HI);
        let mut sy = lcd.top() + 1.5;
        while sy < lcd.bottom() {
            p.line_segment([Pos2::new(lcd.left(), sy), Pos2::new(lcd.right(), sy)], Stroke::new(1.0, alpha(SHADE, 0.16)));
            sy += 3.0;
        }
        inner_shadow(&p, lcd, 10.0, 0.55);
        p.rect_stroke(lcd, 2.0, Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.28)), egui::StrokeKind::Inside);
        let scope_w = if lcd.width() >= 380.0 { (lcd.width() * 0.38).min(lcd.height() * 3.2) } else { 0.0 };
        let text_w = lcd.width() - 28.0 - scope_w - if scope_w > 0.0 { 12.0 } else { 0.0 };
        let name = self.console_patch(target, "Synth");
        let (name, font) = fit_text(ui, &name, text_w, (lcd_h * 0.34).clamp(14.0, 38.0), 10.0, true);
        let two = lcd.height() >= font.size * 1.25 + 24.0;
        let name_y = if two { lcd.center().y - font.size * 0.62 - 5.0 } else { lcd.center().y - font.size * 0.62 };
        let name_r = p.text(Pos2::new(lcd.left() + 14.0, name_y), egui::Align2::LEFT_TOP, name, font, rgb(0x52, 0xC8, 0xC2));
        if two {
            legend_fit(ui, Pos2::new(lcd.left() + 14.0, name_r.bottom() + 5.0), Align::Min, &self.console_subtitle(target).to_uppercase(), text_w, 9.0, alpha(theme::LIVE_TEAL, 0.85));
        }
        if scope_w > 0.0 {
            let r = Rect::from_min_max(Pos2::new(lcd.right() - scope_w - 8.0, lcd.top() + 7.0), Pos2::new(lcd.right() - 8.0, lcd.bottom() - 7.0));
            scope(&p, r, &self.note_leds, self.ui_time);
        }

        // Function row: SCALE (+ key), FOLD, ARP, DRAG, SUSTAIN, VEL.
        let fr_h = (ctrl.height() * 0.085).clamp(26.0, 34.0);
        let fr = Rect::from_min_size(Pos2::new(left.left(), bezel.bottom() + 10.0), egui::vec2(left.width(), fr_h));
        let scale = self.keys_unit.and_then(|k| self.keys_scale(k));
        let scale_txt = scale.map(|c| c.label()).unwrap_or_else(|| "SCALE".into());
        let arp = self.kb_arp_on();
        let tabs: [(&str, String, bool); 5] = [
            ("scale", scale_txt, scale.is_some()),
            ("fold", "FOLD".into(), self.kb_scale_fold),
            ("arp", "ARP".into(), arp == Some(true)),
            ("drag", self.kb_synth_drag.label().to_string(), self.kb_synth_drag == SynthDragMode::Slide),
            ("sustain", "SUSTAIN".into(), self.kb_sustain),
        ];
        let tab_w = ((fr.width() - 4.0 * 6.0) / 6.0).clamp(54.0, 124.0);
        let mut x = fr.left();
        for (id, label, on) in tabs.iter() {
            let r = Rect::from_min_size(Pos2::new(x, fr.top()), egui::vec2(tab_w, fr_h));
            x += tab_w + 6.0;
            let resp = tab(ui, cid(("con_fn", *id)), r, label, *on, theme::LIVE_TEAL);
            let resp = match *id {
                "scale" => resp.on_hover_text("scale snap + key of the Keys unit"),
                "fold" => resp.on_hover_text("only the scale's keys (set a scale first)"),
                "arp" => resp.on_hover_text(if arp.is_some() { "arpeggiator: on runs the pattern, off plays chords" } else { "no Arp unit in this rig" }),
                "drag" => resp.on_hover_text("dragging across the keys: GLISS slides note to note, SLIDE bends the held note"),
                _ => resp.on_hover_text("hold pedal (CC 64)"),
            };
            if *id == "sustain" { laid.strip[2] = r; }
            if resp.clicked() {
                match *id {
                    "scale" => self.scale_menu = Some(ScaleMenu { unit: self.keys_idx(), pos: Pos2::new(r.left(), r.bottom()) }),
                    "fold" => self.kb_scale_fold = !self.kb_scale_fold,
                    "arp" => self.kb_arp_toggle(),
                    "drag" => self.kb_synth_drag = match self.kb_synth_drag {
                        SynthDragMode::Gliss => SynthDragMode::Slide,
                        SynthDragMode::Slide => SynthDragMode::Gliss,
                    },
                    _ => self.kb_toggle_sustain(),
                }
            }
        }
        let vel_w = (fr.right() - x - 4.0).min(240.0);
        laid.strip[3] = if vel_w >= 96.0 {
            self.console_vel(ui, Rect::from_min_size(Pos2::new(x + 4.0, fr.center().y - 10.0), egui::vec2(vel_w, 20.0)))
        } else {
            Rect::NOTHING
        };

        // Macro knobs: one row (two when there are more than four and the
        // panel is tall enough).
        let kn = Rect::from_min_max(Pos2::new(left.left(), fr.bottom() + 10.0), left.max);
        if mains.is_empty() {
            let (t, f) = fit_text(ui, "this patch names no macros: its main controls appear here", kn.width(), 11.0, 8.5, false);
            p.text(kn.center(), egui::Align2::CENTER_CENTER, t, f, theme::CREAM_DIM);
        } else if kn.height() >= 40.0 {
            let framed = kn.height() >= 110.0;
            if framed {
                well(&p, kn, 6.0, rgb(0x15, 0x14, 0x13));
                legend(ui, kn.left_top() + egui::vec2(12.0, 9.0), Align::Min, "MACROS", 8.5, theme::CREAM_DIM);
            }
            let area = if framed { Rect::from_min_max(kn.min + egui::vec2(8.0, 24.0), kn.max - egui::vec2(8.0, 6.0)) } else { kn };
            let n = mains.len();
            // One row, or two where that gives larger knobs (many macros, or
            // a narrow and tall panel).
            let knob_d = |rows: usize| (area.width() / n.div_ceil(rows) as f32 * 0.74).min(area.height() / rows as f32 - 23.0).min(152.0);
            let rows = if n > 2 && knob_d(2) > knob_d(1) + 6.0 { 2 } else { 1 };
            let cols = n.div_ceil(rows);
            let cw = (area.width() / cols as f32).min(230.0);
            let ch = (area.height() / rows as f32).min(cw * 0.9 + 30.0).min(210.0);
            let top = area.center().y - ch * rows as f32 * 0.5;
            let x0 = area.center().x - cw * cols as f32 * 0.5;
            for (i, (label, v)) in mains.iter().enumerate() {
                let cell = Rect::from_min_size(Pos2::new(x0 + (i % cols) as f32 * cw, top + (i / cols) as f32 * ch), egui::vec2(cw, ch));
                if let Some(nv) = knob(ui, cid(("con_macro", i)), cell, label, *v, KnobLook::Synth, 136.0) {
                    self.console_set_main(target, label, nv);
                }
            }
        }

        // The X/Y pad over the first two macros.
        if show_pad {
            let pad = Rect::from_min_size(Pos2::new(ctrl.right() - pad_w, ctrl.top()), egui::vec2(pad_w, ctrl.height() - 20.0));
            let resp = ui.interact(pad, cid("con_xy"), egui::Sense::click_and_drag());
            let live = mains.len() >= 2;
            let (mut nx, mut ny) = if live { (mains[0].1, mains[1].1) } else { (0.5, 0.5) };
            if live && (resp.dragged() || resp.clicked()) {
                if let Some(q) = resp.interact_pointer_pos() {
                    nx = ((q.x - pad.left()) / pad.width()).clamp(0.0, 1.0);
                    ny = 1.0 - ((q.y - pad.top()) / pad.height()).clamp(0.0, 1.0);
                    self.console_set_main(target, &mains[0].0.clone(), nx);
                    self.console_set_main(target, &mains[1].0.clone(), ny);
                }
            }
            drop_shadow(&p, pad, 8.0, 1.0, 4.0, 0.5);
            plate(&p, pad, 8.0, rgb(0x0A, 0x0C, 0x0C), rgb(0x10, 0x16, 0x16));
            // A dot grid, brighter on the quarter lines.
            let cols = ((pad.width() / 26.0) as usize).clamp(4, 24) / 4 * 4;
            let rows = ((pad.height() / 26.0) as usize).clamp(4, 24) / 4 * 4;
            for i in 1..cols {
                for j in 1..rows {
                    let q = Pos2::new(pad.left() + pad.width() * i as f32 / cols as f32, pad.top() + pad.height() * j as f32 / rows as f32);
                    let major = i % (cols / 4) == 0 && j % (rows / 4) == 0;
                    p.circle_filled(q, if major { 1.4 } else { 0.9 }, alpha(theme::CREAM, if major { 0.26 } else { 0.1 }));
                }
            }
            inner_shadow(&p, pad.shrink(1.0), 14.0, 0.5);
            p.rect_stroke(pad, 8.0, Stroke::new(1.0, if resp.hovered() { theme::CREAM_DIM } else { rgb(0x05, 0x05, 0x05) }), egui::StrokeKind::Inside);
            p.line_segment([pad.left_bottom() + egui::vec2(8.0, 0.5), pad.right_bottom() + egui::vec2(-8.0, 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.1)));
            let axis = if live { format!("X {} · Y {}", mains[0].0, mains[1].0) } else { "X/Y needs two macros".into() };
            legend_fit(ui, Pos2::new(pad.center().x, pad.bottom() + 6.0), Align::Center, &axis.to_uppercase(), pad.width(), 8.5, theme::CREAM_DIM);
            if live {
                let c = Pos2::new(pad.left() + nx * pad.width(), pad.top() + (1.0 - ny) * pad.height());
                let hair = alpha(theme::LIVE_TEAL, 0.4);
                hgrad(&p, Rect::from_min_max(Pos2::new(pad.left() + 2.0, c.y - 0.5), Pos2::new(c.x, c.y + 0.5)), alpha(hair, 0.0), hair);
                hgrad(&p, Rect::from_min_max(Pos2::new(c.x, c.y - 0.5), Pos2::new(pad.right() - 2.0, c.y + 0.5)), hair, alpha(hair, 0.0));
                vgrad(&p, Rect::from_min_max(Pos2::new(c.x - 0.5, pad.top() + 2.0), Pos2::new(c.x + 0.5, c.y)), alpha(hair, 0.0), hair);
                vgrad(&p, Rect::from_min_max(Pos2::new(c.x - 0.5, c.y), Pos2::new(c.x + 0.5, pad.bottom() - 2.0)), hair, alpha(hair, 0.0));
                glow(&p, c, 30.0, alpha(theme::LIVE_TEAL, 0.3));
                p.circle_filled(c, 7.0, theme::LIVE_TEAL);
                p.circle_filled(c, 3.0, rgb(0xC8, 0xF2, 0xEE));
                p.circle_stroke(c, 11.0, Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.6)));
            }
        }
    }
}

/// Wheels of the tone generator, C1 upward.
const GENERATOR_WHEELS: usize = 91;
/// Semitones from the played key to the wheel each drawbar draws on
/// (16', 5⅓', 8', 4', 2⅔', 2', 1⅗', 1⅓', 1').
const DRAWBAR_SEMITONES: [i32; crate::organ::DRAWBARS] = [-12, 7, 0, 12, 19, 24, 28, 31, 36];

// ----------------------------------------------------------------------
// Materials: gradients, shadows and glows the parts are built from.

pub(super) fn vgrad(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    if !r.is_positive() { return; }
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.right_bottom(), bottom);
    m.colored_vertex(r.left_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(m));
}

fn hgrad(p: &egui::Painter, r: Rect, left: Color32, right: Color32) {
    if !r.is_positive() { return; }
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), left);
    m.colored_vertex(r.right_top(), right);
    m.colored_vertex(r.right_bottom(), right);
    m.colored_vertex(r.left_bottom(), left);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(m));
}

/// A convex outline filled with a colour per point (a fan around its
/// middle). The edge is not anti-aliased: stroke it or keep it straight.
fn fan(p: &egui::Painter, pts: &[Pos2], col: impl Fn(Pos2) -> Color32) {
    if pts.len() < 3 { return; }
    let n = pts.len() as u32;
    let mid = pts.iter().fold(egui::Vec2::ZERO, |a, q| a + q.to_vec2()) / n as f32;
    let mut m = Mesh::default();
    m.colored_vertex(mid.to_pos2(), col(mid.to_pos2()));
    for q in pts { m.colored_vertex(*q, col(*q)); }
    for i in 0..n { m.add_triangle(0, 1 + i, 1 + (i + 1) % n); }
    p.add(egui::Shape::mesh(m));
}

/// A turned or rolled metal form: `rows` of (y, left x, right x) from the
/// top down, lit across by `stops` (fraction of the width, colour) and
/// darkened by `shade` along its length (0 at the top row, 1 at the last).
fn lathe(p: &egui::Painter, rows: &[(f32, f32, f32)], stops: &[(f32, Color32)], shade: impl Fn(f32) -> f32) {
    let n = stops.len() as u32;
    let mut m = Mesh::default();
    for (k, (y, l, r)) in rows.iter().enumerate() {
        let dark = shade(k as f32 / (rows.len() - 1).max(1) as f32);
        for (f, c) in stops {
            m.colored_vertex(Pos2::new(l + (r - l) * f, *y), mix(*c, SHADE, dark));
        }
        if k > 0 {
            let (a, b) = ((k as u32 - 1) * n, k as u32 * n);
            for i in 0..n - 1 {
                m.add_triangle(a + i, a + i + 1, b + i);
                m.add_triangle(a + i + 1, b + i + 1, b + i);
            }
        }
    }
    p.add(egui::Shape::mesh(m));
}

fn round_pts(r: Rect, rad: f32) -> Vec<Pos2> {
    let rad = rad.min(r.width() * 0.5).min(r.height() * 0.5);
    if rad < 0.75 {
        return vec![r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()];
    }
    let n = 5;
    let mut pts = Vec::with_capacity(4 * (n + 1));
    for (c, a0) in [
        (Pos2::new(r.left() + rad, r.top() + rad), PI),
        (Pos2::new(r.right() - rad, r.top() + rad), 1.5 * PI),
        (Pos2::new(r.right() - rad, r.bottom() - rad), 0.0),
        (Pos2::new(r.left() + rad, r.bottom() - rad), 0.5 * PI),
    ] {
        for k in 0..=n {
            let a = a0 + 0.5 * PI * k as f32 / n as f32;
            pts.push(c + egui::vec2(a.cos(), a.sin()) * rad);
        }
    }
    pts
}

fn circle_pts(c: Pos2, r: f32) -> Vec<Pos2> {
    let n = ((r * 1.4) as usize).clamp(14, 56);
    (0..n).map(|k| {
        let a = TAU * k as f32 / n as f32;
        c + egui::vec2(a.cos(), a.sin()) * r
    }).collect()
}

/// A rounded part lit from above: `top` to `bottom`, its silhouette
/// smoothed by a hairline of the edge colours.
fn plate(p: &egui::Painter, r: Rect, rad: f32, top: Color32, bottom: Color32) {
    if !r.is_positive() { return; }
    let h = r.height();
    fan(p, &round_pts(r, rad), |q| mix(top, bottom, (q.y - r.top()) / h));
    if rad >= 0.75 {
        p.rect_stroke(r, rad, Stroke::new(1.0, mix(top, bottom, 0.5)), egui::StrokeKind::Inside);
    }
}

/// A disc lit from above.
fn disc(p: &egui::Painter, c: Pos2, r: f32, top: Color32, bottom: Color32) {
    if r < 0.5 { return; }
    fan(p, &circle_pts(c, r), |q| mix(top, bottom, (q.y - c.y + r) / (2.0 * r)));
    p.circle_stroke(c, r - 0.5, Stroke::new(1.0, mix(top, bottom, 0.6)));
}

/// A turned metal disc: two bright lobes, as a lathe leaves them.
fn spun(p: &egui::Painter, c: Pos2, r: f32, dark: Color32, light: Color32) {
    if r < 0.5 { return; }
    let pts = circle_pts(c, r);
    let mut m = Mesh::default();
    m.colored_vertex(c, mix(dark, light, 0.62));
    for q in &pts {
        let a = (q.y - c.y).atan2(q.x - c.x);
        m.colored_vertex(*q, mix(dark, light, (0.5 + 0.5 * (2.0 * (a + 0.9)).cos()).powf(1.5)));
    }
    let n = pts.len() as u32;
    for i in 0..n { m.add_triangle(0, 1 + i, 1 + (i + 1) % n); }
    p.add(egui::Shape::mesh(m));
    p.circle_stroke(c, r - 0.5, Stroke::new(1.0, mix(dark, SHADE, 0.3)));
}

/// A blurred rounded rect: a shadow or a glow.
fn soft(p: &egui::Painter, r: Rect, rad: f32, blur: f32, col: Color32) {
    if !r.is_positive() { return; }
    p.add(RectShape::filled(r, rad.clamp(0.0, 255.0), col).with_blur_width(blur));
}

fn drop_shadow(p: &egui::Painter, r: Rect, rad: f32, dy: f32, blur: f32, a: f32) {
    soft(p, r.translate(egui::vec2(0.0, dy)), rad, blur, alpha(SHADE, a));
}

fn glow(p: &egui::Painter, c: Pos2, r: f32, col: Color32) {
    soft(p, Rect::from_center_size(c, egui::Vec2::splat(r * 2.0)), r, r, col);
}

/// The shade a rim casts into what lies under it: along the top edge.
fn inner_shadow(p: &egui::Painter, r: Rect, depth: f32, a: f32) {
    vgrad(p, Rect::from_min_size(r.min, egui::vec2(r.width(), depth.min(r.height()))), alpha(SHADE, a), alpha(SHADE, 0.0));
}

/// A recess in a panel: shaded under its upper rim, its lower rim catching
/// the light.
fn well(p: &egui::Painter, r: Rect, rad: f32, fill: Color32) {
    p.rect_filled(r, rad, fill);
    inner_shadow(p, r.shrink2(egui::vec2(rad * 0.4, 1.0)), (r.height() * 0.2).clamp(4.0, 14.0), 0.55);
    p.rect_stroke(r, rad, Stroke::new(1.0, alpha(SHADE, 0.75)), egui::StrokeKind::Inside);
    p.line_segment([r.left_bottom() + egui::vec2(rad, 0.5), r.right_bottom() + egui::vec2(-rad, 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.09)));
}

/// A line engraved into a panel: its shaded wall and its lit one.
fn engraved_rule(p: &egui::Painter, a: Pos2, b: Pos2) {
    let n = if (a.x - b.x).abs() < 0.5 { egui::vec2(1.0, 0.0) } else { egui::vec2(0.0, 1.0) };
    p.line_segment([a, b], Stroke::new(1.0, alpha(SHADE, 0.6)));
    p.line_segment([a + n, b + n], Stroke::new(1.0, alpha(theme::CREAM, 0.07)));
}

fn arc(p: &egui::Painter, c: Pos2, r: f32, from: f32, to: f32, stroke: Stroke) {
    let steps = (((to - from).abs() * r / 4.0) as usize).clamp(6, 48);
    let pts: Vec<Pos2> = (0..=steps).map(|k| {
        let a = from + (to - from) * k as f32 / steps as f32;
        c + egui::vec2(a.cos(), a.sin()) * r
    }).collect();
    p.add(egui::Shape::line(pts, stroke));
}

/// Repeatable noise in 0..1 for grain and scatter: the same every frame.
fn noise(i: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// A wooden part: `hi` to `deep` from the top, its grain running along it.
fn wood(p: &egui::Painter, r: Rect, rad: f32, hi: Color32, deep: Color32, horizontal: bool, seed: u32) {
    if !r.is_positive() { return; }
    plate(p, r, rad, hi, deep);
    let inner = r.shrink(rad * 0.35 + 1.0);
    let (len, across) = if horizontal { (inner.width(), inner.height()) } else { (inner.height(), inner.width()) };
    let n = (across / 3.4) as u32;
    for k in 0..n {
        let u = (k as f32 + noise(seed + k)) / n as f32;
        let s0 = len * 0.3 * noise(seed.wrapping_mul(7) + k);
        let s1 = len * (1.0 - 0.3 * noise(seed.wrapping_mul(13) + k));
        let col = if noise(seed + 5000 + k) > 0.78 {
            alpha(BRASS_HI, 0.03 + 0.05 * noise(seed + 9000 + k))
        } else {
            alpha(SHADE, 0.05 + 0.15 * noise(seed + 7000 + k))
        };
        let (a, b) = if horizontal {
            let y = inner.top() + across * u;
            (Pos2::new(inner.left() + s0, y), Pos2::new(inner.left() + s1, y))
        } else {
            let x = inner.left() + across * u;
            (Pos2::new(x, inner.top() + s0), Pos2::new(x, inner.top() + s1))
        };
        p.line_segment([a, b], Stroke::new(1.0, col));
    }
}

/// A screw head holding a plate down.
fn screw(p: &egui::Painter, c: Pos2, r: f32) {
    p.circle_filled(c + egui::vec2(0.0, 1.0), r + 0.5, alpha(SHADE, 0.6));
    disc(p, c, r, rgb(0x9A, 0x96, 0x8C), rgb(0x3A, 0x38, 0x34));
    let a = noise((c.x * 3.0 + c.y * 7.0) as u32) * PI;
    let dir = egui::vec2(a.cos(), a.sin()) * (r - 0.8);
    p.line_segment([c - dir, c + dir], Stroke::new(1.0, rgb(0x16, 0x15, 0x13)));
}

// ----------------------------------------------------------------------
// Lettering.

fn text_w(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x)
}

/// `text` at the largest size from `size` down to `min` (0.5 px steps,
/// rule 17) that fits `max_w`; at `min` it is cut to fit.
fn fit_text(ui: &egui::Ui, text: &str, max_w: f32, size: f32, min: f32, medium: bool) -> (String, FontId) {
    let font = |s: f32| if medium { theme::medium_font(s) } else { FontId::proportional(s) };
    let mut s = (size * 2.0).floor() / 2.0;
    while s > min {
        if text_w(ui, text, font(s)) <= max_w { return (text.to_owned(), font(s)); }
        s -= 0.5;
    }
    let f = font(min);
    if text_w(ui, text, f.clone()) <= max_w { return (text.to_owned(), f); }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let t: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
        if text_w(ui, &t, f.clone()) <= max_w { return (t, f); }
    }
    (String::new(), f)
}

const TRACKING: f32 = 0.14;

fn tracked(ui: &egui::Ui, text: &str, size: f32) -> (theme::TrackedLabel, f32) {
    let label = ui.fonts_mut(|f| theme::layout_tracked(f, text, FontId::proportional(size), TRACKING));
    let w = (label.size.x - TRACKING * size).max(0.0); // no gap after the last letter
    (label, w)
}

/// A panel legend: spaced capitals as engraved on hardware. `at` is the
/// top of the text and its left, middle or right per `align`; returns
/// where the letters went.
fn legend(ui: &egui::Ui, at: Pos2, align: Align, text: &str, size: f32, col: Color32) -> Rect {
    legend_fit(ui, at, align, text, f32::INFINITY, size, col)
}

/// [`legend`] kept within `max_w`: smaller first (0.5 px steps, rule 17),
/// then cut.
fn legend_fit(ui: &egui::Ui, at: Pos2, align: Align, text: &str, max_w: f32, size: f32, col: Color32) -> Rect {
    let mut s = (size * 2.0).floor() / 2.0;
    let mut text = text.to_owned();
    let (label, w) = loop {
        let (label, w) = tracked(ui, &text, s);
        if w <= max_w || text.is_empty() { break (label, w); }
        if s > 7.0 {
            s -= 0.5;
        } else {
            let mut chars: Vec<char> = text.trim_end_matches('…').chars().collect();
            chars.pop();
            text = chars.iter().collect::<String>().trim_end().to_string();
            if !text.is_empty() { text.push('…'); }
        }
    };
    let x = match align {
        Align::Min => at.x,
        Align::Center => at.x - w * 0.5,
        Align::Max => at.x - w,
    };
    let pos = Pos2::new(x.round(), at.y.round());
    theme::paint_tracked(ui.painter(), pos, &label, col);
    Rect::from_min_size(pos, egui::vec2(w, label.size.y))
}

// ----------------------------------------------------------------------
// Painted console parts. Each takes an absolute rect and an id, so the
// layouts above place them like parts on a panel.

/// A lit push button: its lamp and its cap glow in `lit` while on.
fn tab(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, on: bool, lit: Color32) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter().clone();
    let body = if down { r.translate(egui::vec2(0.0, 1.0)) } else { r };
    if !down { drop_shadow(&p, r, 4.0, 1.5, 4.0, 0.5); }
    let (top, bottom) = if on {
        (mix(rgb(0x36, 0x34, 0x31), lit, 0.34), mix(rgb(0x1A, 0x19, 0x17), lit, 0.2))
    } else if resp.hovered() {
        (rgb(0x3A, 0x38, 0x34), rgb(0x1E, 0x1D, 0x1B))
    } else {
        (rgb(0x2C, 0x2A, 0x27), rgb(0x16, 0x15, 0x13))
    };
    plate(&p, body, 4.0, top, bottom);
    p.rect_stroke(body, 4.0, Stroke::new(1.0, if on { alpha(lit, 0.85) } else { rgb(0x08, 0x07, 0x07) }), egui::StrokeKind::Inside);
    p.line_segment([body.left_top() + egui::vec2(4.0, 1.5), body.right_top() + egui::vec2(-4.0, 1.5)], Stroke::new(1.0, alpha(theme::CREAM, if on { 0.22 } else { 0.1 })));
    let led_room = if r.width() >= 52.0 { 12.0 } else { 0.0 };
    if led_room > 0.0 {
        let c = Pos2::new(body.left() + 10.0, body.center().y);
        p.circle_filled(c, 3.0, rgb(0x08, 0x07, 0x07));
        if on {
            glow(&p, c, 7.0, alpha(lit, 0.55));
            p.circle_filled(c, 2.4, mix(lit, IVORY_HI, 0.35));
        } else {
            p.circle_filled(c, 2.0, rgb(0x2E, 0x2C, 0x29));
        }
    }
    let (t, f) = fit_text(ui, label, r.width() - 10.0 - led_room, (r.height() * 0.42).clamp(8.5, 12.0), 6.5, true);
    p.text(Pos2::new(body.center().x + led_room * 0.5, body.center().y), egui::Align2::CENTER_CENTER, t, f, if on { theme::CREAM } else { theme::CREAM_META });
    resp
}

/// An ivory rocker tablet, as organs switch stops and couplers: aged and
/// in shade while off, bright with its lamp bar in `lamp` while on. `tag`
/// (a stop's pitch) stands at its right edge.
fn tablet(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, tag: &str, on: bool, lamp: Color32) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter().clone();
    let body = if down { r.translate(egui::vec2(0.0, 1.0)) } else { r };
    if !down { drop_shadow(&p, r, 3.0, 1.5, 4.0, 0.55); }
    let lift = if resp.hovered() { 0.08 } else { 0.0 };
    let (top, bottom) = if on {
        (IVORY_HI, mix(IVORY, BRASS_DEEP, 0.12))
    } else {
        (mix(mix(IVORY, JAMB, 0.26), IVORY_HI, lift), mix(mix(IVORY, JAMB, 0.46), IVORY_HI, lift))
    };
    plate(&p, body, 3.0, top, bottom);
    p.rect_stroke(body, 3.0, Stroke::new(1.0, rgb(0x0C, 0x08, 0x05)), egui::StrokeKind::Inside);
    p.line_segment([body.left_top() + egui::vec2(3.0, 1.5), body.right_top() + egui::vec2(-3.0, 1.5)], Stroke::new(1.0, alpha(IVORY_HI, if on { 0.9 } else { 0.25 })));
    let bar = if body.height() >= 20.0 { 3.0 } else { 2.0 };
    // The lamp let into its lower edge.
    let lamp_r = Rect::from_min_max(Pos2::new(body.left() + 4.0, body.bottom() - bar - 2.0), Pos2::new(body.right() - 4.0, body.bottom() - 2.0));
    if on {
        soft(&p, lamp_r.expand(2.0), 2.0, 6.0, alpha(lamp, 0.6));
        p.rect_filled(lamp_r, 1.0, lamp);
    } else {
        p.rect_filled(lamp_r, 1.0, alpha(IVORY_INK, 0.42));
    }
    let text_r = Rect::from_min_max(body.min + egui::vec2(5.0, 1.0), body.max - egui::vec2(5.0, bar + 2.0));
    let ink = IVORY_INK;
    let size = (r.height() * 0.48).clamp(7.5, 11.5);
    let tag_w = if tag.is_empty() || text_r.width() < 40.0 { 0.0 } else { text_w(ui, tag, theme::medium_font(size)) + 5.0 };
    if tag_w > 0.0 {
        p.text(text_r.right_center(), egui::Align2::RIGHT_CENTER, tag, theme::medium_font(size), ink);
    }
    let (t, f) = fit_text(ui, label, text_r.width() - tag_w, size, 6.5, tag.is_empty());
    if tag_w > 0.0 {
        p.text(text_r.left_center(), egui::Align2::LEFT_CENTER, t, f, ink);
    } else {
        p.text(text_r.center(), egui::Align2::CENTER_CENTER, t, f, ink);
    }
    resp
}

/// A reverse-coloured preset key: black with its name in ivory.
fn preset_key(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, bars: &[u8; crate::organ::DRAWBARS]) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter().clone();
    let body = if down { r.translate(egui::vec2(0.0, 2.0)) } else { r };
    if !down { drop_shadow(&p, r, 3.0, 3.0, 6.0, 0.6); }
    plate(&p, body, 3.0, if resp.hovered() { rgb(0x4C, 0x49, 0x44) } else { rgb(0x3E, 0x3B, 0x37) }, rgb(0x10, 0x0F, 0x0E));
    // The front lip a key ends in.
    let lip_h = (body.height() * 0.1).clamp(3.0, 8.0);
    p.rect_filled(Rect::from_min_max(Pos2::new(body.left() + 1.5, body.bottom() - lip_h), body.max - egui::vec2(1.5, 1.0)), 2.0, rgb(0x07, 0x06, 0x06));
    p.rect_stroke(body, 3.0, Stroke::new(1.0, rgb(0x05, 0x04, 0x04)), egui::StrokeKind::Inside);
    p.line_segment([body.left_top() + egui::vec2(3.0, 1.5), body.right_top() + egui::vec2(-3.0, 1.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.22)));
    let (t, f) = fit_text(ui, label, r.width() - 6.0, 10.5, 7.0, true);
    let ty = if body.height() >= 40.0 { body.bottom() - lip_h - 10.0 } else { body.center().y };
    p.text(Pos2::new(body.center().x, ty), egui::Align2::CENTER_CENTER, t, f, IVORY);
    // The registration it sets, as the drawbars will stand.
    let glyph = Rect::from_min_max(body.min + egui::vec2(body.width() * 0.16, 9.0), Pos2::new(body.right() - body.width() * 0.16, ty - 13.0));
    if glyph.height() >= 22.0 {
        let step = glyph.width() / bars.len() as f32;
        for (i, pos) in bars.iter().enumerate() {
            let x = glyph.left() + step * (i as f32 + 0.5);
            let w = (step * 0.5).clamp(1.5, 4.0);
            p.rect_filled(Rect::from_min_max(Pos2::new(x - w * 0.5, glyph.top()), Pos2::new(x + w * 0.5, glyph.bottom())), 1.0, alpha(SHADE, 0.45));
            if *pos > 0 {
                let col = match i { 0 | 1 => mix(theme::DRAWBAR_BROWN, BRASS, 0.35), 4 | 6 | 7 => rgb(0x8E, 0x8A, 0x82), _ => IVORY };
                p.rect_filled(Rect::from_min_max(Pos2::new(x - w * 0.5, glyph.top()), Pos2::new(x + w * 0.5, glyph.top() + glyph.height() * *pos as f32 / 8.0)), 1.0, col);
            }
        }
    }
    resp
}

/// How a knob is made: a spun-aluminium cap in an LED ring (synth), black
/// bakelite with an ivory pointer over an engraved scale (organ), or a
/// spun-brass cap over the same scale (piano).
#[derive(Clone, Copy, PartialEq)]
enum KnobLook {
    Synth,
    Bakelite,
    Brass,
}

/// A knob in `cell` (label under it), at most `max_d` across: drag up/down.
fn knob(ui: &mut egui::Ui, id: egui::Id, cell: Rect, label: &str, value: f32, look: KnobLook, max_d: f32) -> Option<f32> {
    let label_h = 15.0;
    let d0 = (cell.width() * 0.74).min(cell.height() - label_h - 8.0).clamp(10.0, max_d + 16.0);
    let ring = (d0 * 0.1).clamp(3.5, 8.0);
    let d = (d0 - 2.0 * ring).max(8.0);
    let r = d * 0.5;
    // The knob and its label, centred in the cell together.
    let group_h = d + 2.0 * ring + label_h;
    let c = Pos2::new(cell.center().x, cell.center().y - group_h * 0.5 + ring + r);
    let hit = Rect::from_center_size(c, egui::Vec2::splat(d + 2.0 * ring));
    let resp = ui.interact(hit, id, egui::Sense::click_and_drag()).on_hover_text(format!("{label}: drag up/down"));
    let out = resp.dragged().then(|| (value - resp.drag_delta().y / (d * 2.5).max(80.0)).clamp(0.0, 1.0));
    let v = out.unwrap_or(value).clamp(0.0, 1.0);
    let hot = resp.hovered() || resp.dragged();
    let p = ui.painter().clone();
    // 270° of travel from the lower left.
    let a0 = PI * 0.75;
    let a = a0 + PI * 1.5 * v;
    let dir = egui::vec2(a.cos(), a.sin());
    match look {
        KnobLook::Synth => {
            let rr = r + ring * 0.62;
            arc(&p, c, rr, a0, a0 + PI * 1.5, Stroke::new(ring * 0.42, rgb(0x0B, 0x0B, 0x0A)));
            if v > 0.005 {
                arc(&p, c, rr, a0, a, Stroke::new(ring * 0.9, alpha(theme::LIVE_TEAL, 0.22)));
                arc(&p, c, rr, a0, a, Stroke::new(ring * 0.42, theme::LIVE_TEAL));
                p.circle_filled(c + dir * rr, ring * 0.32, rgb(0xC8, 0xF2, 0xEE));
            }
        }
        KnobLook::Bakelite | KnobLook::Brass => {
            for k in 0..=10 {
                let ta = a0 + PI * 1.5 * k as f32 / 10.0;
                let td = egui::vec2(ta.cos(), ta.sin());
                let major = k % 5 == 0;
                let passed = k as f32 / 10.0 <= v + 0.001;
                p.line_segment(
                    [c + td * (r + ring * 0.3), c + td * (r + ring * if major { 0.95 } else { 0.7 })],
                    Stroke::new(if major { 1.4 } else { 1.0 }, if passed { GOLD_LEAF } else { theme::MUTED }),
                );
            }
        }
    }
    soft(&p, Rect::from_center_size(c + egui::vec2(0.0, r * 0.3), egui::Vec2::splat(d * 0.96)), r, r * 0.55, alpha(SHADE, 0.7));
    disc(&p, c, r, rgb(0x3C, 0x3A, 0x36), rgb(0x0E, 0x0D, 0x0C));
    match look {
        KnobLook::Synth => {
            spun(&p, c, r * 0.74, ALU_DEEP, ALU);
            p.line_segment([c + dir * (r * 0.2), c + dir * (r * 0.7)], Stroke::new((d * 0.045).clamp(1.5, 3.0), rgb(0x12, 0x11, 0x10)));
            p.circle_filled(c + dir * (r * 0.87), (d * 0.03).clamp(1.0, 2.0), theme::CREAM);
        }
        KnobLook::Bakelite => {
            disc(&p, c, r * 0.7, rgb(0x46, 0x43, 0x3F), rgb(0x1A, 0x19, 0x17));
            p.line_segment([c + dir * (r * 0.3), c + dir * (r * 0.95)], Stroke::new((d * 0.05).clamp(1.5, 3.0), IVORY));
        }
        KnobLook::Brass => {
            spun(&p, c, r * 0.72, BRASS_DEEP, BRASS_HI);
            p.line_segment([c + dir * (r * 0.18), c + dir * (r * 0.68)], Stroke::new((d * 0.05).clamp(1.5, 3.0), IVORY_INK));
            p.circle_filled(c + dir * (r * 0.86), (d * 0.035).clamp(1.0, 2.0), IVORY);
        }
    }
    if hot {
        p.circle_stroke(c, r, Stroke::new(1.0, theme::CREAM_DIM));
    }
    let (t, f) = fit_text(ui, label, cell.width() - 4.0, 10.5, 7.5, false);
    p.text(Pos2::new(cell.center().x, c.y + r + ring + 2.0), egui::Align2::CENTER_TOP, t, f, theme::CREAM_META);
    out
}

/// One tonewheel drawbar: the footage over its channel, the shaft pulled
/// down to `pos` (0..8) with its stops engraved, the coloured handle at
/// the end (brown: sub-octave, ivory: octaves, black: the rest).
fn drawbar(ui: &mut egui::Ui, id: egui::Id, r: Rect, bar: usize, footage: &str, pos: u8) -> Option<u8> {
    let slot = Rect::from_min_max(r.min + egui::vec2(0.0, 19.0), r.max);
    let tip_h = (r.width() * 0.95).clamp(18.0, 52.0);
    let unit = (slot.height() - tip_h - 6.0) / 8.0;
    let resp = ui.interact(r, id, egui::Sense::click_and_drag()).on_hover_text(format!("drawbar {footage}: drag down to pull it out (0 to 8)"));
    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(q) = resp.interact_pointer_pos() {
            out = Some((((q.y - slot.top() - 3.0 - tip_h * 0.5) / unit).round()).clamp(0.0, 8.0) as u8);
        }
    }
    let pos = out.unwrap_or(pos);
    let hot = resp.hovered() || resp.dragged();
    let p = ui.painter().clone();
    let (t, f) = fit_text(ui, footage, r.width() + 6.0, 11.0, 8.0, true);
    p.text(Pos2::new(r.center().x, r.top() + 7.0), egui::Align2::CENTER_CENTER, t, f, theme::CREAM_META);
    // The channel the bar slides in, a mark for each stop of its travel.
    let ch = Rect::from_min_max(Pos2::new(r.center().x - r.width() * 0.34, slot.top()), Pos2::new(r.center().x + r.width() * 0.34, slot.bottom()));
    well(&p, ch, 3.0, rgb(0x07, 0x06, 0x05));
    for k in 0..=8u8 {
        let y = slot.top() + 3.0 + unit * k as f32 + tip_h * 0.5;
        p.line_segment([Pos2::new(ch.left() + 3.0, y), Pos2::new(ch.right() - 3.0, y)], Stroke::new(1.0, alpha(theme::CREAM, 0.07)));
    }
    let tip_top = slot.top() + 3.0 + unit * pos as f32;
    if pos > 0 {
        let shaft_w = r.width() * 0.5;
        let shaft = Rect::from_min_max(Pos2::new(r.center().x - shaft_w * 0.5, slot.top() + 1.0), Pos2::new(r.center().x + shaft_w * 0.5, tip_top + 3.0));
        let hi = shaft.left() + shaft_w * 0.36;
        hgrad(&p, Rect::from_min_max(shaft.min, Pos2::new(hi, shaft.bottom())), rgb(0x4A, 0x47, 0x42), rgb(0xD2, 0xCD, 0xC2));
        hgrad(&p, Rect::from_min_max(Pos2::new(hi, shaft.top()), shaft.max), rgb(0xD2, 0xCD, 0xC2), rgb(0x56, 0x52, 0x4C));
        if unit >= 11.0 {
            for k in 1..=pos {
                let y = slot.top() + 3.0 + unit * (k as f32 - 0.5);
                p.text(Pos2::new(shaft.center().x, y), egui::Align2::CENTER_CENTER, k.to_string(), theme::medium_font((unit * 0.58).clamp(7.5, 11.0)), rgb(0x22, 0x20, 0x1E));
            }
        }
        inner_shadow(&p, shaft, 5.0, 0.5);
    }
    let (tip, ink) = match bar {
        0 | 1 => (theme::DRAWBAR_BROWN, theme::CREAM),
        4 | 6 | 7 => (rgb(0x24, 0x22, 0x20), theme::CREAM),
        _ => (IVORY, theme::DARK_PANEL),
    };
    let tip_r = Rect::from_min_size(Pos2::new(r.left() + 1.0, tip_top), egui::vec2(r.width() - 2.0, tip_h));
    drop_shadow(&p, tip_r, 5.0, 3.0, 7.0, 0.7);
    plate(&p, tip_r, 5.0, mix(tip, IVORY_HI, 0.16), mix(tip, SHADE, 0.3));
    // The grip the fingers pull on.
    let grip = Rect::from_min_max(Pos2::new(tip_r.left() + 2.0, tip_r.bottom() - tip_h * 0.24), tip_r.max - egui::vec2(2.0, 2.0));
    p.rect_filled(grip, 3.0, mix(tip, SHADE, 0.42));
    p.line_segment([grip.left_top() + egui::vec2(2.0, 0.5), grip.right_top() + egui::vec2(-2.0, 0.5)], Stroke::new(1.0, alpha(SHADE, 0.35)));
    p.rect_stroke(tip_r, 5.0, Stroke::new(1.0, if hot { theme::CREAM_DIM } else { mix(tip, SHADE, 0.7) }), egui::StrokeKind::Inside);
    p.line_segment([tip_r.left_top() + egui::vec2(5.0, 1.5), tip_r.right_top() + egui::vec2(-5.0, 1.5)], Stroke::new(1.0, alpha(IVORY_HI, 0.4)));
    p.text(Pos2::new(tip_r.center().x, tip_r.top() + tip_h * 0.38), egui::Align2::CENTER_CENTER, pos.to_string(), theme::medium_font((tip_h * 0.42).clamp(9.0, 17.0)), ink);
    out
}

/// The rotary speaker seen from above: the horn turning in its baffle,
/// smeared along its path the faster it goes.
fn rotor(p: &egui::Painter, c: Pos2, r: f32, angle: f32, speed: f32, on: bool) {
    soft(p, Rect::from_center_size(c + egui::vec2(0.0, r * 0.12), egui::Vec2::splat(r * 2.0)), r, r * 0.3, alpha(SHADE, 0.6));
    disc(p, c, r, WALNUT_HI, WALNUT_DEEP);
    disc(p, c, r * 0.88, rgb(0x08, 0x07, 0x06), rgb(0x1A, 0x16, 0x12));
    for k in [0.66, 0.44] {
        p.circle_stroke(c, r * k, Stroke::new(1.0, alpha(theme::CREAM, 0.05)));
    }
    let live = if on { theme::LIVE_TEAL } else { theme::CREAM_DIM };
    let horn = |a: f32, col: Color32, fade: f32| {
        let dir = egui::vec2(a.cos(), a.sin());
        let n = egui::vec2(-dir.y, dir.x);
        let tip = c + dir * (r * 0.78);
        let pts = [c + n * (r * 0.07), tip + n * (r * 0.25), tip - n * (r * 0.25), c - n * (r * 0.07)];
        fan(p, &pts, |q| alpha(mix(mix(col, SHADE, 0.55), col, ((q - c).length() / (r * 0.8)).min(1.0)), fade));
        if fade >= 1.0 {
            p.line_segment([tip + n * (r * 0.25), tip - n * (r * 0.25)], Stroke::new(2.0, mix(col, IVORY_HI, 0.45)));
        }
    };
    let smear = (speed.abs() / 30.0).clamp(0.0, 1.0);
    if smear > 0.03 {
        for k in (1..=6).rev() {
            let back = angle - k as f32 * 0.2 * smear;
            horn(back, live, 0.2 * (7 - k) as f32 / 6.0);
            horn(back + PI, rgb(0x5A, 0x57, 0x52), 0.14 * (7 - k) as f32 / 6.0);
        }
    }
    horn(angle + PI, rgb(0x5A, 0x57, 0x52), 1.0);
    horn(angle, live, 1.0);
    spun(p, c, r * 0.17, ALU_DEEP, ALU);
    p.circle_stroke(c, r - 0.5, Stroke::new(1.0, mix(WALNUT_DEEP, SHADE, 0.5)));
}

/// The swell box: shutters that open as the swell shoe is pressed, the
/// pipes' light behind them.
fn swell_box(p: &egui::Painter, r: Rect, v: f32) {
    well(p, r, 4.0, rgb(0x0A, 0x07, 0x05));
    let inner = r.shrink2(egui::vec2(5.0, 4.0));
    vgrad(p, inner, alpha(theme::CABLE_AMBER, 0.05 + 0.2 * v), alpha(theme::CABLE_AMBER, 0.02 + 0.08 * v));
    let n = ((inner.height() / 11.0) as usize).clamp(3, 10);
    let slot = inner.height() / n as f32;
    for k in 0..n {
        // Closed, a shutter fills its slot; open, it stands edge on.
        let h = (slot * (0.94 - 0.74 * v)).max(2.0);
        let s = Rect::from_min_size(Pos2::new(inner.left(), inner.top() + slot * k as f32 + (slot - h) * 0.5), egui::vec2(inner.width(), h));
        vgrad(p, s, WALNUT_HI, mix(WALNUT, WALNUT_DEEP, 0.5));
        p.line_segment([s.left_top() + egui::vec2(0.0, 0.5), s.right_top() + egui::vec2(0.0, 0.5)], Stroke::new(1.0, alpha(BRASS_HI, 0.3)));
    }
}

/// A swell (expression) shoe: a ribbed rubber tread in its track, the
/// track lit up to where it stands; drag left/right.
fn swell_shoe(ui: &mut egui::Ui, id: egui::Id, r: Rect, value: f32) -> Option<f32> {
    let resp = ui.interact(r, id, egui::Sense::click_and_drag()).on_hover_text("swell pedal (CC 11): drag to open or close");
    let label_w = 54.0;
    let track = Rect::from_min_max(Pos2::new(r.left() + label_w, r.top()), r.max);
    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(q) = resp.interact_pointer_pos() {
            out = Some(((q.x - track.left()) / track.width()).clamp(0.0, 1.0));
        }
    }
    let v = out.unwrap_or(value).clamp(0.0, 1.0);
    let hot = resp.hovered() || resp.dragged();
    let p = ui.painter().clone();
    legend(ui, Pos2::new(r.left(), r.center().y - 5.5), Align::Min, "SWELL", 9.0, theme::CREAM_DIM);
    well(&p, track, 6.0, rgb(0x0B, 0x09, 0x07));
    let tread_w = (track.width() * 0.17).clamp(30.0, 66.0);
    let lane = track.shrink2(egui::vec2(3.0, 3.0));
    let tx = lane.left() + (lane.width() - tread_w) * v;
    if tx - lane.left() > 2.0 {
        let lit = Rect::from_min_max(Pos2::new(lane.left() + 1.0, lane.center().y - 2.5), Pos2::new(tx, lane.center().y + 2.5));
        hgrad(&p, lit, BRASS_DEEP, BRASS_HI);
    }
    for k in 1..10 {
        let x = lane.left() + lane.width() * k as f32 / 10.0;
        p.line_segment([Pos2::new(x, lane.bottom() - 4.0), Pos2::new(x, lane.bottom() - 1.0)], Stroke::new(1.0, alpha(theme::CREAM, if k == 5 { 0.3 } else { 0.14 })));
    }
    let tread = Rect::from_min_size(Pos2::new(tx, lane.top()), egui::vec2(tread_w, lane.height()));
    drop_shadow(&p, tread, 4.0, 2.0, 6.0, 0.7);
    plate(&p, tread, 4.0, rgb(0x3A, 0x37, 0x33), RUBBER);
    let ribs = ((tread_w / 6.0) as usize).max(4);
    for k in 1..ribs {
        let x = tread.left() + tread_w * k as f32 / ribs as f32;
        p.line_segment([Pos2::new(x, tread.top() + 4.0), Pos2::new(x, tread.bottom() - 4.0)], Stroke::new(1.0, alpha(SHADE, 0.6)));
        p.line_segment([Pos2::new(x + 1.0, tread.top() + 4.0), Pos2::new(x + 1.0, tread.bottom() - 4.0)], Stroke::new(1.0, alpha(theme::CREAM, 0.08)));
    }
    p.rect_stroke(tread, 4.0, Stroke::new(1.0, if hot { theme::CREAM_DIM } else { rgb(0x05, 0x05, 0x05) }), egui::StrokeKind::Inside);
    p.line_segment([tread.left_top() + egui::vec2(4.0, 1.5), tread.right_top() + egui::vec2(-4.0, 1.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.25)));
    out
}

/// A horizontal value bar (VEL): label left, value right, drag to set.
fn hbar(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, value: f32) -> Option<f32> {
    let label_w = 34.0;
    let value_w = 34.0;
    let track = Rect::from_min_max(Pos2::new(r.left() + label_w, r.top()), Pos2::new(r.right() - value_w, r.bottom()));
    let resp = ui.interact(track, id, egui::Sense::click_and_drag()).on_hover_text("note velocity of the on-screen and laptop keys");
    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(q) = resp.interact_pointer_pos() {
            out = Some(((q.x - track.left()) / track.width()).clamp(0.0, 1.0));
        }
    }
    let v = out.unwrap_or(value).clamp(0.0, 1.0);
    let p = ui.painter().clone();
    legend(ui, Pos2::new(r.left(), r.center().y - 5.5), Align::Min, label, 9.0, theme::CREAM_DIM);
    let groove = Rect::from_center_size(track.center(), egui::vec2(track.width(), track.height().min(14.0)));
    well(&p, groove, 3.0, rgb(0x0C, 0x0B, 0x0A));
    let lane = groove.shrink(3.0);
    let x = lane.left() + lane.width() * v;
    if x - lane.left() > 1.0 {
        hgrad(&p, Rect::from_min_max(lane.min, Pos2::new(x, lane.bottom())), mix(theme::CREAM, SHADE, 0.45), theme::CREAM);
    }
    let cap = Rect::from_center_size(Pos2::new(x.clamp(lane.left() + 2.0, lane.right() - 2.0), groove.center().y), egui::vec2(5.0, groove.height() + 4.0));
    drop_shadow(&p, cap, 2.0, 1.5, 3.0, 0.6);
    plate(&p, cap, 2.0, IVORY_HI, mix(IVORY, SHADE, 0.3));
    if resp.hovered() || resp.dragged() {
        p.rect_stroke(cap, 2.0, Stroke::new(1.0, theme::CREAM_DIM), egui::StrokeKind::Outside);
    }
    p.text(Pos2::new(r.right(), r.center().y), egui::Align2::RIGHT_CENTER, format!("{v:.2}"), FontId::proportional(10.0), theme::CREAM_META);
    out
}

/// A brass piano pedal seen from the bench (pressed while `on`), its name
/// under it.
fn pedal(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, on: bool) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let p = ui.painter().clone();
    let body = if on { r.translate(egui::vec2(0.0, 3.0)) } else { r };
    let (w, h, cx) = (body.width(), body.height(), body.center().x);
    // Narrow where it leaves the lyre, wide and rounded at the toe.
    let toe = (h * 0.45).min(w * 0.2);
    let mut rows: Vec<(f32, f32, f32)> = (0..=4).map(|k| {
        let u = k as f32 / 4.0;
        let half = w * (0.3 + 0.2 * u);
        (body.top() + (h - toe) * u, cx - half, cx + half)
    }).collect();
    rows.extend((1..=5).map(|k| {
        let a = 0.5 * PI * k as f32 / 5.0;
        let half = w * 0.5 - toe + toe * a.cos();
        (body.bottom() - toe + toe * a.sin(), cx - half, cx + half)
    }));
    soft(&p, Rect::from_min_max(Pos2::new(r.left() + 2.0, r.bottom() - 2.0), Pos2::new(r.right() - 2.0, r.bottom() + if on { 4.0 } else { 8.0 })), 4.0, 8.0, alpha(SHADE, 0.75));
    let lift = if on { 0.28 } else if resp.hovered() { 0.12 } else { 0.0 };
    let stops = [(0.0, BRASS_DEEP), (0.3, BRASS_HI), (0.62, BRASS), (1.0, BRASS_DEEP)].map(|(f, c)| (f, mix(c, IVORY_HI, lift)));
    lathe(&p, &rows, &stops, |t| 0.42 * (1.0 - t));
    let outline: Vec<Pos2> = rows.iter().map(|(y, l, _)| Pos2::new(*l, *y)).chain(rows.iter().rev().map(|(y, _, r)| Pos2::new(*r, *y))).collect();
    p.add(egui::Shape::closed_line(outline, Stroke::new(1.0, mix(BRASS_DEEP, SHADE, 0.45))));
    p.line_segment([Pos2::new(cx - w * 0.2, body.top() + h * 0.28), Pos2::new(cx - w * 0.3, body.bottom() - h * 0.3)], Stroke::new(1.5, alpha(IVORY_HI, 0.45)));
    legend(ui, Pos2::new(r.center().x, r.bottom() + 6.0), Align::Center, label, 8.5, if on { theme::LIVE_TEAL } else { theme::CREAM_DIM });
    resp
}

/// A thumb piston (ivory button in a brass collar, its number on it) with
/// its name under it.
fn piston(ui: &mut egui::Ui, id: egui::Id, cell: Rect, d: f32, number: &str, name: &str) -> egui::Response {
    let named = cell.height() - d >= 13.0;
    let c = Pos2::new(cell.center().x, if named { cell.top() + d * 0.5 + 2.0 } else { cell.center().y });
    let hit = Rect::from_center_size(c, egui::vec2(d + 6.0, d + 6.0));
    let resp = ui.interact(hit, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter().clone();
    let r = d * 0.5;
    soft(&p, Rect::from_center_size(c + egui::vec2(0.0, 2.5), egui::Vec2::splat(d)), r, 5.0, alpha(SHADE, 0.7));
    spun(&p, c, r, BRASS_DEEP, BRASS_HI);
    let cap = c + egui::vec2(0.0, if down { 1.0 } else { 0.0 });
    p.circle_filled(c, r * 0.8, rgb(0x1A, 0x12, 0x0A));
    if down {
        disc(&p, cap, r * 0.74, mix(IVORY, theme::ACCENT, 0.55), theme::ACCENT);
    } else {
        disc(&p, cap, r * 0.76, IVORY_HI, mix(IVORY, BRASS_DEEP, 0.3));
    }
    if resp.hovered() {
        p.circle_stroke(c, r, Stroke::new(1.0, theme::CREAM));
    }
    p.text(cap, egui::Align2::CENTER_CENTER, number, theme::medium_font((d * 0.42).clamp(9.0, 15.0)), IVORY_INK);
    if named {
        let (t, f) = fit_text(ui, name, cell.width() - 6.0, 10.0, 7.5, false);
        p.text(Pos2::new(c.x, c.y + r + 3.0), egui::Align2::CENTER_TOP, t, f, theme::CREAM_META);
    }
    resp
}

/// One pitch/mod wheel standing in its slot: a ribbed cylinder that turns
/// with the value (`t` 0..1), the marker at its position.
fn wheel(p: &egui::Painter, r: Rect, t: f32, center_detent: bool, hot: bool) {
    p.rect_filled(r.expand(3.0), 5.0, rgb(0x04, 0x04, 0x04));
    let crown = rgb(0x48, 0x45, 0x41);
    let rim = rgb(0x0C, 0x0B, 0x0A);
    let mid = r.center().y - r.height() * 0.08;
    vgrad(p, Rect::from_min_max(r.min, Pos2::new(r.right(), mid)), rim, crown);
    vgrad(p, Rect::from_min_max(Pos2::new(r.left(), mid), r.max), crown, rim);
    // Ribs roll with the wheel, closing up towards the rims as on a drum.
    let phase = t * 2.4;
    for k in -12..=12 {
        let a = (k as f32 + phase.fract()) * 0.125;
        if a.abs() >= 0.5 * PI { continue; }
        let y = r.center().y + a.sin() * r.height() * 0.5;
        let face = a.cos();
        p.line_segment([Pos2::new(r.left() + 2.0, y), Pos2::new(r.right() - 2.0, y)], Stroke::new(1.0 + face, alpha(SHADE, 0.3 + 0.3 * face)));
        p.line_segment([Pos2::new(r.left() + 2.0, y + 1.0 + face * 0.5), Pos2::new(r.right() - 2.0, y + 1.0 + face * 0.5)], Stroke::new(1.0, alpha(theme::CREAM, 0.07 * face)));
    }
    hgrad(p, Rect::from_min_size(r.min, egui::vec2(3.0, r.height())), alpha(SHADE, 0.5), alpha(SHADE, 0.0));
    hgrad(p, Rect::from_min_max(Pos2::new(r.right() - 3.0, r.top()), r.max), alpha(SHADE, 0.0), alpha(SHADE, 0.5));
    if center_detent {
        p.line_segment([Pos2::new(r.left() - 5.0, r.center().y), Pos2::new(r.left() - 1.0, r.center().y)], Stroke::new(2.0, theme::MUTED));
    }
    let my = r.bottom() - t.clamp(0.0, 1.0) * r.height();
    let mark = Rect::from_min_max(Pos2::new(r.left() + 1.0, my - 2.0), Pos2::new(r.right() - 1.0, my + 2.0)).intersect(r);
    if hot { soft(p, mark.expand(2.0), 2.0, 6.0, alpha(theme::ACCENT, 0.5)); }
    p.rect_filled(mark, 1.0, if hot { theme::ACCENT } else { theme::CREAM });
}

/// What sounds, as a scope would draw it: the held notes' sines summed,
/// each at its pitch and level.
fn scope(p: &egui::Painter, r: Rect, leds: &[f32; 128], time: f32) {
    let line = rgb(0x52, 0xC8, 0xC2);
    p.rect_filled(r, 2.0, alpha(SHADE, 0.3));
    for k in 1..4 {
        let x = r.left() + r.width() * k as f32 / 4.0;
        p.line_segment([Pos2::new(x, r.top() + 2.0), Pos2::new(x, r.bottom() - 2.0)], Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.14)));
    }
    p.line_segment([Pos2::new(r.left() + 2.0, r.center().y), Pos2::new(r.right() - 2.0, r.center().y)], Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.14)));
    p.rect_stroke(r, 2.0, Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.22)), egui::StrokeKind::Inside);
    let notes: Vec<(f32, f32)> = leds.iter().enumerate()
        .filter(|(_, l)| **l > 0.02)
        .map(|(n, l)| (2f32.powf((n as f32 - 60.0) / 12.0), l.min(1.0)))
        .collect();
    let total: f32 = notes.iter().map(|(_, l)| l).sum::<f32>().max(1.0);
    let lowest = notes.iter().map(|(f, _)| *f).fold(f32::INFINITY, f32::min);
    let steps = (r.width() / 2.0) as usize;
    let pts: Vec<Pos2> = (0..=steps).map(|k| {
        let u = k as f32 / steps.max(1) as f32;
        let y: f32 = notes.iter().map(|(f, l)| l * (TAU * (2.0 * u * f / lowest + time * 0.6 * f)).sin()).sum::<f32>() / total;
        Pos2::new(r.left() + 3.0 + (r.width() - 6.0) * u, r.center().y - y * (r.height() * 0.5 - 5.0))
    }).collect();
    if !notes.is_empty() {
        p.add(egui::Shape::line(pts.clone(), Stroke::new(4.5, alpha(line, 0.18))));
    }
    p.add(egui::Shape::line(pts, Stroke::new(1.5, if notes.is_empty() { alpha(line, 0.5) } else { line })));
}

/// The grid that gives `n` drawknobs in `area` the largest knobs:
/// (columns, rows, knob diameter, label font size).
fn knob_grid(n: usize, area: Rect) -> (usize, usize, f32, f32) {
    // Names under the knobs stay readable: cells at least 58 wide when
    // the area allows it.
    let max_cols = ((area.width() / 58.0).floor() as usize).clamp(1, n.max(1));
    let mut best = (max_cols, n.max(1).div_ceil(max_cols), 0.0f32, 8.0f32);
    for cols in 1..=max_cols {
        let rows = n.div_ceil(cols);
        let cw = area.width() / cols as f32;
        let ch = area.height() / rows as f32;
        let font = (cw * 0.12).clamp(8.0, 10.5);
        let label_h = 2.0 * font * 1.25 + 4.0;
        let d = (cw * 0.62).min(ch - label_h - 10.0).min(60.0);
        if d > best.2 + 0.5 {
            best = (cols, rows, d, font);
        }
    }
    best.2 = best.2.max(10.0);
    best
}

/// "Open Diapason\n8" → ("Open Diapason", "8"); "Harmonic\nFlute 8" →
/// ("Harmonic Flute", "8"): the pitch goes on the knob, the name under it.
fn stop_label(stop: &str) -> (String, String) {
    let (first, second) = stop.split_once('\n').unwrap_or((stop, ""));
    let pitchy = |w: &str| w.chars().all(|c| c.is_ascii_digit() || c == '/') || w.chars().all(|c| "IVX".contains(c));
    let words: Vec<&str> = second.split_whitespace().collect();
    let cut = words.iter().position(|w| pitchy(w)).unwrap_or(words.len());
    let mut name = first.to_string();
    for w in &words[..cut] { name.push(' '); name.push_str(w); }
    (name, words[cut..].join(" "))
}

/// A drawknob in `cell`: a turned rosewood knob with an ivory face that
/// carries its pitch; pushed in and in shade while off, pulled out and lit
/// while drawn. The stop's name under it in up to two lines.
fn drawknob(ui: &mut egui::Ui, id: egui::Id, cell: Rect, d: f32, name: &str, foot: &str, on: bool, font: f32) -> egui::Response {
    let c = Pos2::new(cell.center().x, cell.top() + d * 0.5 + 6.0);
    let hit = Rect::from_min_max(Pos2::new(cell.left() + 2.0, cell.top()), Pos2::new(cell.right() - 2.0, cell.bottom()));
    let resp = ui.interact(hit, id, egui::Sense::click());
    let p = ui.painter().clone();
    let r = d * 0.5;
    // The bushing it slides in.
    p.circle_filled(c, r + 1.5, rgb(0x07, 0x05, 0x03));
    let rr = if on { r } else { r * 0.9 };
    if on {
        soft(&p, Rect::from_center_size(c + egui::vec2(0.0, r * 0.34), egui::Vec2::splat(d * 1.02)), r, r * 0.6, alpha(SHADE, 0.8));
        soft(&p, Rect::from_center_size(c, egui::Vec2::splat(d + 8.0)), r + 4.0, 6.0, alpha(theme::LIVE_TEAL, 0.55));
        disc(&p, c, rr, rgb(0x6A, 0x40, 0x24), rgb(0x2C, 0x19, 0x0D));
        disc(&p, c, rr * 0.8, IVORY_HI, mix(IVORY, BRASS_DEEP, 0.28));
    } else {
        disc(&p, c, rr, rgb(0x40, 0x28, 0x17), rgb(0x1C, 0x11, 0x09));
        disc(&p, c, rr * 0.8, mix(IVORY, JAMB, 0.28), mix(IVORY, JAMB, 0.5));
    }
    if resp.hovered() {
        p.circle_stroke(c, rr, Stroke::new(1.0, theme::CREAM_DIM));
    }
    if !foot.is_empty() && d >= 16.0 {
        let (t, f) = fit_text(ui, foot, rr * 1.3, (d * 0.3).clamp(7.0, 15.0), 6.5, true);
        p.text(c, egui::Align2::CENTER_CENTER, t, f, IVORY_INK);
    }
    // The name: one line if it fits, else split at the word that balances
    // the two lines best.
    let max_w = cell.width() - 4.0;
    let fid = FontId::proportional(font);
    let lines: Vec<String> = if text_w(ui, name, fid.clone()) <= max_w {
        vec![name.to_string()]
    } else {
        let words: Vec<&str> = name.split_whitespace().collect();
        let split = (1..words.len()).min_by(|&a, &b| {
            let wa = text_w(ui, &words[..a].join(" "), fid.clone()).max(text_w(ui, &words[a..].join(" "), fid.clone()));
            let wb = text_w(ui, &words[..b].join(" "), fid.clone()).max(text_w(ui, &words[b..].join(" "), fid.clone()));
            wa.total_cmp(&wb)
        });
        match split {
            Some(s) => vec![words[..s].join(" "), words[s..].join(" ")],
            None => vec![name.to_string()],
        }
    };
    let ink = if on { theme::CREAM } else { theme::CREAM_MID };
    let mut y = c.y + r + 5.0;
    for line in lines {
        let (t, f) = fit_text(ui, &line, max_w, font, 6.5, false);
        let h = f.size * 1.2;
        if y + h > cell.bottom() + 1.0 { break; }
        p.text(Pos2::new(c.x, y), egui::Align2::CENTER_TOP, t, f, ink);
        y += h;
    }
    resp
}

/// A console part's id: global, keyed by what the part is (rule 135).
fn cid(salt: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
    egui::Id::NULL.with(("kb_console", salt))
}
