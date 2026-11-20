//! The expanded keyboard (#163): each instrument as its own full-window
//! console, laid out for the screen it fills rather than the dock grown
//! taller. The rack and the top bars step aside; Esc or DOCK bring them
//! back. Keys, header and wheel MIDI are the dock's own (`kb_keybed`,
//! `kb_header`, `kb_wheel_drive`), so both play exactly alike.
//!
//! - Piano: the strings and hammers behind the nameboard (a hammer rises
//!   on every sounding note, the dampers lift with the pedal), touch and
//!   tone controls on the nameboard, a brass pedal lyre under the keys.
//! - Jazz Organ: preset keys and percussion tabs, nine full-length
//!   drawbars, the Leslie with its spinning rotor, DRIVE/REVERB, one or
//!   two manuals between walnut cheeks, the swell shoe.
//! - Pipe Organ: a pipe facade aligned to the keys (a pipe speaks for each
//!   sounding note), every division's drawknobs on two stop jambs at once,
//!   couplers and tremulants, thumb pistons, the swell shoe.
//! - Synth: a patch display, the function row, up to eight macro knobs,
//!   a large X/Y pad, and full-height pitch and mod wheels.

use super::*;
use crate::keyboard::{KeyboardType, SynthDragMode};
use std::f32::consts::PI;

const LACQUER: Color32 = Color32::from_rgb(0x0B, 0x0A, 0x09);
const LACQUER_HI: Color32 = Color32::from_rgb(0x26, 0x24, 0x21);
const WALNUT: Color32 = Color32::from_rgb(0x3E, 0x27, 0x17);
const WALNUT_DEEP: Color32 = Color32::from_rgb(0x21, 0x15, 0x0C);
const WALNUT_HI: Color32 = Color32::from_rgb(0x5E, 0x3C, 0x22);
const BRASS: Color32 = Color32::from_rgb(0xC8, 0xA0, 0x5A);
const BRASS_DEEP: Color32 = Color32::from_rgb(0x7A, 0x5F, 0x33);
const GOLD_LEAF: Color32 = Color32::from_rgb(0xD9, 0xC2, 0x8E);
const PIPE_TIN: Color32 = Color32::from_rgb(0xA4, 0xA0, 0x97);
const IVORY_KNOB: Color32 = Color32::from_rgb(0xEC, 0xE4, 0xCF);
const STRING_STEEL: Color32 = Color32::from_rgb(0x8E, 0x8C, 0x86);
const STRING_COPPER: Color32 = Color32::from_rgb(0x9A, 0x63, 0x38);
const HAMMER_FELT: Color32 = Color32::from_rgb(0xD8, 0xD2, 0xC2);
const METAL: Color32 = Color32::from_rgb(0x1E, 0x1D, 0x1B);
const METAL_HI: Color32 = Color32::from_rgb(0x2E, 0x2C, 0x29);
const LCD: Color32 = Color32::from_rgb(0x0B, 0x17, 0x16);

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
                match surface {
                    KeyboardType::Piano => self.console_piano(ui, body, target, &mut laid),
                    KeyboardType::JazzOrgan => self.console_jazz(ui, body, &mut laid),
                    KeyboardType::PipeOrgan => self.console_pipe(ui, body, target, &mut laid),
                    KeyboardType::Synth => self.console_synth(ui, body, target, &mut laid),
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
        if let Some(nv) = hbar(ui, cid("con_vel"), rect, "VEL", v, theme::CREAM) {
            self.kb_set_vel(nv);
        }
        rect
    }

    // ------------------------------------------------------------------
    // Piano

    fn console_piano(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let side = (b.width() * 0.012).clamp(8.0, 18.0).round();
        let kw = Self::console_key_w(b.width() - 2.0 * side);
        let mut keys_h = (kw * 4.3 + 12.0).min(b.height() * 0.46).max(90.0).round();
        let pedal_h = (b.height() * 0.15).clamp(48.0, 120.0).round();
        let mut nb_h = (b.height() * 0.15).clamp(64.0, 130.0).round();
        let spare = b.height() - keys_h - pedal_h - nb_h - 6.0;
        let action_h = if spare >= 70.0 { spare } else {
            if spare > 0.0 { nb_h += spare; } else { keys_h = (keys_h + spare).max(70.0); }
            0.0
        };
        // The case: black lacquer, a sheen along the top.
        p.rect_filled(b, 10.0, LACQUER);
        vgrad(&p, Rect::from_min_size(b.min + egui::vec2(10.0, 0.0), egui::vec2(b.width() - 20.0, 3.0)), LACQUER_HI, LACQUER);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
        let mut y = b.top();
        let action = Rect::from_min_size(Pos2::new(b.left() + side, y + side * 0.6), egui::vec2(b.width() - 2.0 * side, (action_h - side * 0.6).max(0.0)));
        y += action_h;
        let nb = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, nb_h));
        y += nb_h;
        let rail = Rect::from_min_size(Pos2::new(b.left() + side + 6.0, y), egui::vec2(b.width() - 2.0 * side - 12.0, 6.0));
        y += 6.0;
        let bed_area = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, keys_h));
        y += keys_h;
        let pedals = Rect::from_min_max(Pos2::new(b.left() + side, y), Pos2::new(b.right() - side, b.bottom()));

        // The key slip's felt, then the keys.
        p.rect_filled(rail, 1.0, Self::KB_FELT);
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::Piano, 0.0);
        let bed = laid.bed;

        // Behind the nameboard: strings and hammers, one per key.
        if action.height() >= 60.0 {
            self.console_piano_action(&p, action, bed);
        }

        // The nameboard: touch on the left, the name in gold leaf, tone
        // (the patch's own controls) on the right.
        p.line_segment([Pos2::new(nb.left(), nb.top()), Pos2::new(nb.right(), nb.top())], Stroke::new(1.0, LACQUER_HI));
        let lw = (nb.width() * 0.22).clamp(150.0, 280.0);
        let left = Rect::from_min_size(nb.min + egui::vec2(6.0, 8.0), egui::vec2(lw, nb.height() - 16.0));
        let row_h = ((left.height() - 4.0) / 2.0).min(30.0);
        let yt = Rect::from_min_size(left.min, egui::vec2(96.0f32.min(lw), row_h));
        if tab(ui, cid("con_ytouch"), yt, "Y-TOUCH", self.kb_y_touch, theme::LIVE_TEAL)
            .on_hover_text("where you hit the key sets velocity: top = pp, bottom = ff")
            .clicked()
        {
            self.kb_y_touch = !self.kb_y_touch;
        }
        // The last hit's loudness, PP..FF, beside Y-TOUCH.
        let meter = Rect::from_min_max(Pos2::new(yt.right() + 10.0, yt.center().y - 3.0), Pos2::new(left.right(), yt.center().y + 3.0));
        if meter.width() > 30.0 {
            p.rect_filled(meter, 2.0, theme::PAPER_SINK);
            let v = self.kb_last_vel.clamp(0.0, 1.0);
            p.rect_filled(Rect::from_min_size(meter.min, egui::vec2(meter.width() * v, meter.height())), 2.0, theme::ACCENT);
        }
        let vel = Rect::from_min_size(Pos2::new(left.left(), left.top() + row_h + 4.0), egui::vec2(lw, row_h.min(24.0)));
        laid.strip[3] = self.console_vel(ui, vel);

        let mains = self.console_mains(target, 4);
        let cell_w = (nb.height() * 0.8).clamp(48.0, 84.0);
        let rw = cell_w * mains.len() as f32;
        for (i, (label, v)) in mains.iter().enumerate() {
            let cell = Rect::from_min_size(Pos2::new(nb.right() - rw + i as f32 * cell_w, nb.top() + 6.0), egui::vec2(cell_w, nb.height() - 10.0));
            if let Some(nv) = knob(ui, cid(("con_main", i)), cell, label, *v, BRASS) {
                self.console_set_main(target, label, nv);
            }
        }
        let name_w = nb.width() - lw - rw - 48.0;
        let name = self.console_patch(target, "Piano");
        let (name, font) = fit_text(ui, &name, name_w, (nb.height() * 0.3).clamp(15.0, 32.0), 11.0, true);
        let cx = nb.left() + 6.0 + lw + (nb.width() - lw - rw - 6.0) * 0.5;
        let name_r = p.text(Pos2::new(cx, nb.center().y - 4.0), egui::Align2::CENTER_CENTER, name, font, GOLD_LEAF);
        let sub = self.console_subtitle(target);
        let (sub, sfont) = fit_text(ui, &sub, name_w, 11.0, 8.5, false);
        if nb.bottom() - name_r.bottom() >= 16.0 {
            p.line_segment([Pos2::new(cx - 24.0, name_r.bottom() + 4.0), Pos2::new(cx + 24.0, name_r.bottom() + 4.0)], Stroke::new(1.0, BRASS_DEEP));
            p.text(Pos2::new(cx, name_r.bottom() + 8.0), egui::Align2::CENTER_TOP, sub, sfont, theme::CREAM_DIM);
        }

        // The pedal lyre: SOFT (una corda, CC 67) and SUSTAIN (CC 64).
        let pw = (kw * 1.5).clamp(54.0, 104.0);
        let ph = (pedals.height() * 0.32).clamp(16.0, 30.0);
        let label_h = 14.0;
        let py = pedals.bottom() - label_h - ph - 4.0;
        let cx = pedals.center().x;
        let lyre_top = pedals.top() + 4.0;
        for dx in [-pw * 0.35, pw * 0.35] {
            p.line_segment([Pos2::new(cx + dx, lyre_top), Pos2::new(cx + dx * 1.6, py)], Stroke::new(3.0, BRASS_DEEP));
        }
        let box_r = Rect::from_min_max(Pos2::new(cx - pw * 1.45, py - 6.0), Pos2::new(cx + pw * 1.45, py + ph * 0.5));
        p.rect_filled(box_r, 3.0, Color32::from_rgb(0x15, 0x13, 0x11));
        let soft_r = Rect::from_min_size(Pos2::new(cx - pw * 1.25, py), egui::vec2(pw, ph));
        let sus_r = Rect::from_min_size(Pos2::new(cx + pw * 0.25, py), egui::vec2(pw, ph));
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

    /// The piano's action seen through the open case: a string per key
    /// (copper-wound in the bass, longer to the left), the damper rail
    /// (lifted while the sustain pedal is down), and the hammers, which
    /// rise against their string while the key sounds.
    fn console_piano_action(&self, p: &egui::Painter, a: Rect, bed: Rect) {
        // The cast-iron frame: gold paint fading into the dark soundboard.
        p.rect_filled(a, 6.0, Color32::from_rgb(0x12, 0x0F, 0x0C));
        vgrad(p, Rect::from_min_max(a.min + egui::vec2(3.0, 3.0), Pos2::new(a.right() - 3.0, a.top() + a.height() * 0.5)), alpha(BRASS_DEEP, 0.55), alpha(BRASS_DEEP, 0.0));
        p.rect_stroke(a, 6.0, Stroke::new(2.0, BRASS_DEEP), egui::StrokeKind::Inside);
        let span = 12 * Self::dock_octaves(bed);
        let hammer_y = a.bottom() - 16.0;
        let damper_y = a.top() + a.height() * 0.62 - if self.kb_sustain { 8.0 } else { 0.0 };
        let shift = if self.kb_soft { 3.0 } else { 0.0 };
        let kw = (bed.width() - 12.0) / Self::dock_whites(bed) as f32;
        for semi in 0..=span {
            let x = Self::console_semi_x(bed, semi);
            let t = semi as f32 / span.max(1) as f32;
            let top = a.top() + 10.0 + a.height() * 0.42 * t;
            let led = self.console_led(semi);
            let col = if semi < 12 { STRING_COPPER } else { STRING_STEEL };
            p.line_segment([Pos2::new(x, top), Pos2::new(x, hammer_y - 6.0)], Stroke::new(if semi < 12 { 1.6 } else { 1.0 }, alpha(col, 0.75)));
            p.circle_filled(Pos2::new(x, top), 2.2, Color32::from_rgb(0xB8, 0xB4, 0xAC)); // tuning pin
            if led > 0.02 {
                p.line_segment([Pos2::new(x, top), Pos2::new(x, hammer_y - 6.0)], Stroke::new(2.0, alpha(theme::CREAM, 0.25 + 0.5 * led)));
            }
            // Dampers sit on the strings that are not playing.
            if led <= 0.02 && !self.kb_sustain_holds(semi) && damper_y > top + 6.0 {
                p.rect_filled(Rect::from_center_size(Pos2::new(x, damper_y), egui::vec2((kw * 0.32).max(3.0), 7.0)), 1.0, Color32::from_rgb(0x3A, 0x33, 0x2C));
            }
            let lift = 7.0 * led.min(1.0);
            let hw = (kw * 0.34).clamp(4.0, 14.0);
            let head = Rect::from_center_size(Pos2::new(x + shift, hammer_y - lift), egui::vec2(hw, 9.0));
            p.line_segment([Pos2::new(x + shift, head.bottom()), Pos2::new(x + shift, a.bottom() - 3.0)], Stroke::new(1.0, WALNUT_HI));
            p.rect_filled(head, 3.0, if led > 0.02 { mix(HAMMER_FELT, theme::ACCENT, 0.75) } else { HAMMER_FELT });
        }
        p.line_segment([Pos2::new(a.left() + 8.0, damper_y - 6.0), Pos2::new(a.right() - 8.0, damper_y - 6.0)], Stroke::new(2.0, alpha(BRASS_DEEP, 0.8)));
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
        let cheek = (b.width() * 0.025).clamp(14.0, 34.0).round();
        let kw = Self::console_key_w(b.width() - 2.0 * cheek);
        let swell_h = (b.height() * 0.08).clamp(36.0, 60.0).round();
        let dual = self.kb_dual_manual;
        let n = if dual { 2.0 } else { 1.0 };
        let man_gap = 14.0;
        let ctrl_min = if b.height() >= 520.0 { 190.0 } else { 116.0 };
        let mh = (kw * 3.0 + 12.0)
            .min((b.height() - ctrl_min - swell_h - 20.0 - (n - 1.0) * man_gap) / n)
            .max(60.0)
            .round();
        let manuals_h = n * mh + (n - 1.0) * man_gap;
        let ctrl_h = (b.height() - manuals_h - swell_h - 20.0).max(80.0);
        // The cabinet.
        p.rect_filled(b, 10.0, WALNUT_DEEP);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, WALNUT), egui::StrokeKind::Inside);
        let ctrl = Rect::from_min_size(b.min + egui::vec2(cheek, 8.0), egui::vec2(b.width() - 2.0 * cheek, ctrl_h));
        p.rect_filled(ctrl, 6.0, Color32::from_rgb(0x13, 0x11, 0x0F));
        p.rect_stroke(ctrl, 6.0, Stroke::new(1.0, WALNUT_HI), egui::StrokeKind::Inside);
        let man = Rect::from_min_size(Pos2::new(b.left(), ctrl.bottom() + 6.0), egui::vec2(b.width(), manuals_h));
        // Walnut key cheeks either side of the manuals.
        for r in [
            Rect::from_min_size(man.min, egui::vec2(cheek, manuals_h)),
            Rect::from_min_size(Pos2::new(man.right() - cheek, man.top()), egui::vec2(cheek, manuals_h)),
        ] {
            hgrad(&p, r.shrink2(egui::vec2(2.0, 0.0)), WALNUT_HI, WALNUT);
        }
        if dual {
            let seam = Rect::from_min_size(Pos2::new(man.left() + cheek, man.top() + mh + 2.0), egui::vec2(man.width() - 2.0 * cheek, man_gap - 4.0));
            vgrad(&p, seam, WALNUT_HI, WALNUT);
        }
        let bed_area = Rect::from_min_max(Pos2::new(man.left() + cheek, man.top()), Pos2::new(man.right() - cheek, man.bottom()));
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::JazzOrgan, man_gap);
        let swell = Rect::from_min_size(
            Pos2::new(b.center().x - (b.width() * 0.18).clamp(110.0, 240.0), man.bottom() + 8.0),
            egui::vec2((b.width() * 0.36).clamp(220.0, 480.0), swell_h - 4.0),
        );
        self.console_swell(ui, swell);
        laid.deck = ctrl;

        let c = ctrl.shrink(12.0);
        let side_w = (c.width() * 0.26).clamp(186.0, 340.0);
        let left = Rect::from_min_size(c.min, egui::vec2(side_w, c.height()));
        let right = Rect::from_min_size(Pos2::new(c.right() - side_w, c.top()), egui::vec2(side_w, c.height()));
        let mid = Rect::from_min_max(Pos2::new(left.right() + 16.0, c.top()), Pos2::new(right.left() - 16.0, c.bottom()));

        // Left: preset keys (reverse-coloured, like the cancel/preset keys
        // left of a tonewheel manual), then percussion.
        let tab_h = (c.height() * 0.16).clamp(22.0, 32.0);
        let pk_h = (c.height() - tab_h - 16.0 - 22.0 - 10.0).min(150.0).max(24.0);
        let group_h = 16.0 + pk_h + 10.0 + 16.0 + tab_h;
        let left = Rect::from_min_size(Pos2::new(left.left(), left.center().y - group_h * 0.5), egui::vec2(left.width(), group_h));
        caption(&p, left.left_top(), "PRESETS");
        let keys_r = Rect::from_min_size(Pos2::new(left.left(), left.top() + 16.0), egui::vec2(left.width(), pk_h));
        let pk_w = (keys_r.width() - 3.0 * 5.0) / 4.0;
        for (i, preset) in crate::keyboard::JAZZ_ORGAN_PRESETS.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(keys_r.left() + i as f32 * (pk_w + 5.0), keys_r.top()), egui::vec2(pk_w, keys_r.height()));
            if preset_key(ui, cid(("con_preset", i)), r, preset.short).on_hover_text(preset.label).clicked() {
                self.kb_jazz_preset(preset);
            }
        }
        let perc = crate::organ::step(self.kb_organ_value(crate::organ::P_PERC), 3);
        let py = left.bottom() - tab_h;
        caption(&p, Pos2::new(left.left(), py - 16.0), "PERCUSSION");
        let tw = (left.width() - 6.0) / 2.0;
        if tab(ui, cid("con_perc"), Rect::from_min_size(Pos2::new(left.left(), py), egui::vec2(tw, tab_h)), "PERC", perc > 0, BRASS)
            .on_hover_text("percussion on/off (CC 80)")
            .clicked()
        {
            self.kb_set_organ_param(crate::organ::P_PERC, if perc > 0 { 0.0 } else { 0.5 });
        }
        if tab(ui, cid("con_perc_soft"), Rect::from_min_size(Pos2::new(left.left() + tw + 6.0, py), egui::vec2(tw, tab_h)), "SOFT", perc == 2, BRASS)
            .on_hover_text("soft percussion")
            .clicked()
        {
            self.kb_set_organ_param(crate::organ::P_PERC, if perc == 2 { 0.5 } else { 1.0 });
        }

        // Middle: the nine drawbars, full length, centred.
        let n_bars = crate::organ::DRAWBARS as f32;
        let bar_w = ((mid.width() - 8.0 * 10.0 - 14.0) / n_bars).clamp(18.0, 46.0).floor();
        let gap = ((mid.width() - n_bars * bar_w - 14.0) / 8.0).clamp(4.0, bar_w * 0.6);
        let total = n_bars * bar_w + 8.0 * gap + 14.0;
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
            x += bar_w + gap;
        }

        // Right: the Leslie (rotor + STOP/SLOW/FAST), DRIVE, REVERB, and
        // the second manual.
        let leslie = crate::organ::step(self.kb_organ_value(crate::organ::P_LESLIE), 3);
        let col_a = Rect::from_min_size(right.min, egui::vec2(right.width() * 0.52, right.height()));
        let col_b = Rect::from_min_max(Pos2::new(col_a.right() + 10.0, right.top()), right.max);
        caption(&p, col_a.left_top(), "LESLIE");
        let sw_h = tab_h;
        let sw_y = col_a.bottom() - sw_h;
        let sw_w = (col_a.width() - 8.0) / 3.0;
        for (i, name) in ["STOP", "SLOW", "FAST"].iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(col_a.left() + i as f32 * (sw_w + 4.0), sw_y), egui::vec2(sw_w, sw_h));
            if tab(ui, cid(("con_leslie", i)), r, name, leslie == i, theme::LIVE_TEAL).clicked() {
                self.kb_set_organ_param(crate::organ::P_LESLIE, i as f32 / 2.0);
                self.kb_strip_pulse(0.5);
            }
        }
        let rotor_area = Rect::from_min_max(Pos2::new(col_a.left(), col_a.top() + 16.0), Pos2::new(col_a.right(), sw_y - 8.0));
        let d = rotor_area.width().min(rotor_area.height()).min(120.0);
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        // Spin-up and spin-down take a moment, as on the real rotor.
        let target_speed = [0.0, 0.8, 5.5][leslie] * 2.0 * PI;
        self.kb_rotor.1 += (target_speed - self.kb_rotor.1) * (dt * 1.6).min(1.0);
        self.kb_rotor.0 = (self.kb_rotor.0 + self.kb_rotor.1 * dt) % (2.0 * PI);
        if d >= 34.0 {
            rotor(&p, rotor_area.center(), d * 0.5, self.kb_rotor.0, leslie > 0);
            if leslie > 0 { ui.ctx().request_repaint(); }
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
            if let Some(nv) = knob(ui, cid(("con_organ_knob", k)), cell, label, v, BRASS) {
                self.kb_set_organ_param(param, nv);
            }
        }
        let dual_r = Rect::from_min_size(Pos2::new(col_b.left(), col_b.bottom() - sw_h), egui::vec2(col_b.width(), sw_h));
        if tab(ui, cid("con_dual"), dual_r, "2 MANUALS", dual, theme::LIVE_TEAL)
            .on_hover_text("upper manual on MIDI channel 1, lower on channel 2 (setBfree)")
            .clicked()
        {
            self.kb_dual_manual = !self.kb_dual_manual;
        }
    }

    // ------------------------------------------------------------------
    // Pipe Organ

    fn console_pipe(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let aeolus = target.filter(|&u| self.is_aeolus_unit(u));
        let side = (b.width() * 0.012).clamp(8.0, 18.0).round();
        let kw = Self::console_key_w(b.width() - 2.0 * side);
        let bottom_h = (b.height() * 0.075).clamp(40.0, 64.0).round();
        let keys_h = (kw * 3.4 + 12.0).min(b.height() * 0.28).max(70.0).round();
        let jamb_need = (b.height() * 0.40).max(200.0);
        let rest = b.height() - keys_h - bottom_h - jamb_need - 24.0;
        let facade_h = if rest >= 80.0 { rest.min(260.0) } else { 0.0 };
        let jamb_h = b.height() - keys_h - bottom_h - facade_h - 24.0;

        p.rect_filled(b, 10.0, WALNUT_DEEP);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, WALNUT), egui::StrokeKind::Inside);
        let mut y = b.top() + 8.0;
        let facade = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, facade_h));
        if facade_h > 0.0 { y += facade_h + 4.0; }
        let jambs = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, jamb_h));
        y += jamb_h + 6.0;
        let bed_area = Rect::from_min_size(Pos2::new(b.left() + side, y), egui::vec2(b.width() - 2.0 * side, keys_h));
        y += keys_h + 4.0;
        let bottom = Rect::from_min_max(Pos2::new(b.left() + side, y), Pos2::new(b.right() - side, b.bottom() - 4.0));

        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::PipeOrgan, 0.0);
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
            p.rect_filled(jamb, 6.0, Color32::from_rgb(0x17, 0x11, 0x0B));
            p.rect_stroke(jamb, 6.0, Stroke::new(1.0, WALNUT_HI), egui::StrokeKind::Inside);
            let inner = jamb.shrink(8.0);
            let gh = (inner.height() - 8.0) / 2.0;
            for (k, &d) in divs.iter().enumerate() {
                let g = Rect::from_min_size(Pos2::new(inner.left(), inner.top() + k as f32 * (gh + 8.0)), egui::vec2(inner.width(), gh));
                self.console_division(ui, &p, g, d, drawn[d]);
            }
        }

        // Center: the name plate and the coupler tabs.
        let plate_h = (center.height() * 0.22).clamp(34.0, 64.0);
        let plate = Rect::from_min_size(center.min, egui::vec2(center.width(), plate_h));
        p.rect_filled(plate, 4.0, BRASS_DEEP);
        p.rect_stroke(plate, 4.0, Stroke::new(1.0, BRASS), egui::StrokeKind::Inside);
        let name = aeolus.map(|t| sentence_case(&self.units[t].name)).unwrap_or_else(|| "Pipe Organ".into());
        let (name, font) = fit_text(ui, &name, plate.width() - 16.0, (plate_h * 0.34).clamp(12.0, 20.0), 9.0, true);
        p.text(plate.center() - egui::vec2(0.0, if plate_h >= 48.0 { 7.0 } else { 0.0 }), egui::Align2::CENTER_CENTER, name, font, GOLD_LEAF);
        if plate_h >= 48.0 {
            let sub = if aeolus.is_some() { self.console_subtitle(target) } else { "preview — load Aeolus to play it".into() };
            let (sub, sfont) = fit_text(ui, &sub, plate.width() - 16.0, 10.0, 8.0, false);
            p.text(Pos2::new(plate.center().x, plate.bottom() - 11.0), egui::Align2::CENTER_CENTER, sub, sfont, theme::CREAM);
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
        caption(&p, Pos2::new(center.left(), plate.bottom() + 8.0), "COUPLERS");
        let grid = Rect::from_min_max(Pos2::new(center.left(), plate.bottom() + 24.0), center.max);
        let rows = links.len().div_ceil(2);
        let th = ((grid.height() - (rows as f32 - 1.0) * 5.0) / rows as f32).clamp(18.0, 34.0);
        let tw = (grid.width() - 6.0) / 2.0;
        for (i, (d, l, label, tip)) in links.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(grid.left() + (i % 2) as f32 * (tw + 6.0), grid.top() + (i / 2) as f32 * (th + 5.0)), egui::vec2(tw, th));
            if r.bottom() > grid.bottom() + 0.5 { break; }
            let on = (couplers[*d] >> l) & 1 == 1;
            if tab(ui, cid(("con_coupler", *d, *l)), r, label, on, theme::LIVE_TEAL).on_hover_text(tip.as_str()).clicked() {
                self.kb_pipe_coupler_toggle(*d, *l);
            }
        }

        // Under the manual: thumb pistons (instant registrations) and the
        // swell shoe.
        let swell_w = (bottom.width() * 0.28).clamp(180.0, 380.0);
        let swell = Rect::from_min_size(Pos2::new(bottom.right() - swell_w, bottom.top()), egui::vec2(swell_w, bottom.height()));
        self.console_swell(ui, swell);
        let pist = Rect::from_min_max(bottom.min, Pos2::new(swell.left() - 16.0, bottom.bottom()));
        let n = crate::keyboard::PIPE_ORGAN_PISTONS.len() as f32;
        let pd = (bottom.height() - 16.0).clamp(18.0, 34.0);
        let cell_w = (pist.width() / n).min(110.0);
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

    /// One division on a stop jamb: its name, its tremulant tab, and its
    /// drawknobs (drawn = pulled out, ivory face lit).
    fn console_division(&mut self, ui: &mut egui::Ui, p: &egui::Painter, g: Rect, d: usize, drawn: u32) {
        let div = &crate::aeolus::DIVISIONS[d];
        let head_h = 22.0;
        let title = div.name.to_uppercase();
        let trem_w = 64.0f32.min(g.width() * 0.4);
        let (title, tf) = fit_text(ui, &title, g.width() - trem_w - 10.0, 10.5, 8.0, true);
        p.text(Pos2::new(g.left() + 2.0, g.top() + head_h * 0.5), egui::Align2::LEFT_CENTER, title, tf, BRASS);
        let trem_r = Rect::from_min_size(Pos2::new(g.right() - trem_w, g.top() + 1.0), egui::vec2(trem_w, head_h - 4.0));
        if tab(ui, cid(("con_trem", d)), trem_r, "TREM", self.kb_pipe_trem(d), theme::LIVE_TEAL)
            .on_hover_text(format!("tremulant of {}", div.name))
            .clicked()
        {
            self.kb_pipe_trem_toggle(d);
        }
        let area = Rect::from_min_max(Pos2::new(g.left(), g.top() + head_h + 2.0), g.max);
        let n = div.stops.len();
        let (cols, rows, d_knob, font) = knob_grid(n, area);
        if d_knob < 22.0 {
            // Too small for drawknobs with names: lit stop tabs instead.
            let rows = ((area.height() / 18.0).floor() as usize).clamp(1, n.max(1));
            let cols = n.div_ceil(rows);
            let cw = area.width() / cols as f32;
            let ch = (area.height() / rows as f32).min(26.0);
            for (i, stop) in div.stops.iter().enumerate() {
                let cell = Rect::from_min_size(Pos2::new(area.left() + (i % cols) as f32 * cw, area.top() + (i / cols) as f32 * ch), egui::vec2(cw - 3.0, ch - 2.0));
                let (name, foot) = stop_label(stop);
                let on = (drawn >> i) & 1 == 1;
                if tab(ui, cid(("con_stop", d, i)), cell, &name, on, theme::LIVE_TEAL).on_hover_text(format!("{name} {foot}")).clicked() {
                    self.kb_pipe_toggle_stop(d, i);
                }
            }
            return;
        }
        let cw = area.width() / cols as f32;
        let ch = area.height() / rows as f32;
        for (i, stop) in div.stops.iter().enumerate() {
            let cell = Rect::from_min_size(Pos2::new(area.left() + (i % cols) as f32 * cw, area.top() + (i / cols) as f32 * ch), egui::vec2(cw, ch));
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

    /// The case pipes: one per key, standing above it (bass to the left,
    /// longest), each speaking (mouth lit) while its note sounds.
    fn console_pipe_facade(&self, p: &egui::Painter, f: Rect, bed: Rect) {
        p.rect_filled(f, 6.0, Color32::from_rgb(0x14, 0x0E, 0x09));
        let span = 12 * Self::dock_octaves(bed);
        let step = (bed.width() - 12.0) / (span as f32 + 1.0);
        let pw = (step * 0.72).clamp(3.0, 22.0);
        let foot_h = (f.height() * 0.16).clamp(10.0, 34.0);
        let base_y = f.bottom() - 6.0;
        p.rect_filled(Rect::from_min_max(Pos2::new(f.left() + 4.0, base_y), Pos2::new(f.right() - 4.0, f.bottom() - 2.0)), 1.0, WALNUT_HI);
        for semi in 0..=span {
            let x = Self::console_semi_x(bed, semi);
            let t = semi as f32 / span.max(1) as f32;
            let len = (f.height() - 14.0) * (1.0 - 0.62 * t);
            let top = base_y - len;
            let body = Rect::from_min_max(Pos2::new(x - pw * 0.5, top), Pos2::new(x + pw * 0.5, base_y - foot_h));
            let led = self.console_led(semi);
            let (edge, mid) = if led > 0.02 {
                (mix(PIPE_TIN, theme::CREAM, 0.2), mix(theme::CREAM, GOLD_LEAF, 0.3))
            } else {
                (mix(PIPE_TIN, BLACK, 0.55), PIPE_TIN)
            };
            // A cylinder: dark edges, light down the middle.
            hgrad(p, Rect::from_min_max(body.min, Pos2::new(body.center().x, body.bottom())), edge, mid);
            hgrad(p, Rect::from_min_max(Pos2::new(body.center().x, body.top()), body.max), mid, edge);
            // The foot: a cone narrowing to the toe.
            p.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(body.left(), body.bottom()),
                    Pos2::new(body.right(), body.bottom()),
                    Pos2::new(x + pw * 0.14, base_y),
                    Pos2::new(x - pw * 0.14, base_y),
                ],
                mix(edge, BLACK, 0.2),
                Stroke::NONE,
            ));
            // The mouth: dark, glowing while it speaks.
            let mouth = Rect::from_min_size(Pos2::new(x - pw * 0.3, body.bottom() - pw * 0.55 - 2.0), egui::vec2(pw * 0.6, (pw * 0.4).max(2.0)));
            p.rect_filled(mouth, 1.0, if led > 0.02 { mix(theme::ACCENT, GOLD_LEAF, 0.3) } else { Color32::from_rgb(0x1A, 0x17, 0x14) });
            if led > 0.02 {
                p.circle_filled(mouth.center(), pw * 0.9, alpha(theme::ACCENT, 0.18 * led));
            }
        }
    }

    // ------------------------------------------------------------------
    // Synth

    fn console_synth(&mut self, ui: &mut egui::Ui, b: Rect, target: Option<usize>, laid: &mut Laid) {
        let p = ui.painter().clone();
        let well_w = (b.width() * 0.09).clamp(84.0, 150.0).round();
        let inset = 12.0;
        let kw = Self::console_key_w(b.width() - well_w - 2.0 * inset);
        let keys_h = (kw * 4.6 + 12.0).min(b.height() * 0.44).max(80.0).round();
        p.rect_filled(b, 10.0, METAL);
        vgrad(&p, Rect::from_min_size(b.min + egui::vec2(10.0, 1.0), egui::vec2(b.width() - 20.0, (b.height() * 0.3).min(160.0))), METAL_HI, METAL);
        p.rect_stroke(b, 10.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
        let ctrl = Rect::from_min_max(b.min + egui::vec2(inset, inset), Pos2::new(b.right() - inset, b.bottom() - keys_h - inset - 8.0));
        laid.deck = ctrl;
        let keys_row = Rect::from_min_max(Pos2::new(b.left() + inset, b.bottom() - keys_h - inset), Pos2::new(b.right() - inset, b.bottom() - inset));
        let well = Rect::from_min_size(keys_row.min, egui::vec2(well_w, keys_row.height()));
        let bed_area = Rect::from_min_max(Pos2::new(well.right() + 6.0, keys_row.top()), keys_row.max);
        laid.bed = self.kb_keybed(ui, bed_area, KeyboardType::Synth, 0.0);

        // The wheel well: PITCH (springs to center) and MOD (holds).
        p.rect_filled(well, 6.0, theme::PAPER_SINK);
        let wh = well.height() - 30.0;
        let ww = ((well.width() - 30.0) / 2.0).max(16.0);
        for (k, (label, spring)) in [("PITCH", true), ("MOD", false)].into_iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(well.left() + 10.0 + k as f32 * (ww + 10.0), well.top() + 10.0), egui::vec2(ww, wh));
            let resp = ui.interact(r, cid(("con_wheel", k)), egui::Sense::click_and_drag());
            let value = self.kb_wheel_drive(&resp, r, spring);
            wheel(&p, r, if spring { (value + 1.0) * 0.5 } else { value }, spring, resp.hovered() || resp.dragged());
            p.text(Pos2::new(r.center().x, well.bottom() - 9.0), egui::Align2::CENTER_CENTER, label, FontId::proportional(9.5), theme::INK_2);
            laid.strip[k] = r;
        }

        // Control panel: patch display + function row + macro knobs on the
        // left, the X/Y pad on the right.
        let mains = self.console_mains(target, 8);
        let pad_side = (ctrl.height() - 22.0).min(ctrl.width() * 0.34).max(0.0);
        let show_pad = pad_side >= 90.0;
        let pad_w = if show_pad { (pad_side * 1.3).min(ctrl.width() * 0.4) } else { 0.0 };
        let left = Rect::from_min_max(ctrl.min, Pos2::new(ctrl.right() - pad_w - if show_pad { 20.0 } else { 0.0 }, ctrl.bottom()));
        let lcd_h = (ctrl.height() * 0.24).clamp(44.0, 92.0);
        let lcd = Rect::from_min_size(left.min, egui::vec2(left.width(), lcd_h));
        p.rect_filled(lcd, 6.0, LCD);
        p.rect_stroke(lcd, 6.0, Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.35)), egui::StrokeKind::Inside);
        let name = self.console_patch(target, "Synth");
        let (name, font) = fit_text(ui, &name, lcd.width() - 28.0, (lcd_h * 0.36).clamp(14.0, 30.0), 10.0, true);
        let name_r = p.text(Pos2::new(lcd.left() + 14.0, lcd.top() + 8.0), egui::Align2::LEFT_TOP, name, font, theme::LIVE_TEAL);
        if lcd.bottom() - name_r.bottom() >= 18.0 {
            let sub = self.console_subtitle(target);
            let (sub, sf) = fit_text(ui, &sub, lcd.width() - 28.0, 11.0, 8.5, false);
            let y = (lcd.bottom() - 8.0 - sf.size * 1.3).max(name_r.bottom() + 3.0);
            p.text(Pos2::new(lcd.left() + 14.0, y), egui::Align2::LEFT_TOP, sub, sf, alpha(theme::LIVE_TEAL, 0.7));
        }

        // Function row: SCALE (+ key), FOLD, ARP, DRAG, SUSTAIN, VEL.
        let fr_h = 28.0;
        let fr = Rect::from_min_size(Pos2::new(left.left(), lcd.bottom() + 10.0), egui::vec2(left.width(), fr_h));
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
        let tab_w = ((fr.width() - 4.0 * 6.0) / 6.0).clamp(54.0, 120.0);
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
        let vel_w = (fr.right() - x).min(220.0);
        laid.strip[3] = if vel_w >= 80.0 {
            self.console_vel(ui, Rect::from_min_size(Pos2::new(x, fr.top() + 4.0), egui::vec2(vel_w, fr_h - 8.0)))
        } else {
            Rect::NOTHING
        };

        // Macro knobs: one row (two when there are more than four and the
        // panel is tall enough).
        let kn = Rect::from_min_max(Pos2::new(left.left(), fr.bottom() + 12.0), left.max);
        if mains.is_empty() {
            let (t, f) = fit_text(ui, "this patch names no macros: its main controls appear here", kn.width(), 11.0, 8.5, false);
            p.text(kn.center(), egui::Align2::CENTER_CENTER, t, f, theme::CREAM_DIM);
        } else if kn.height() >= 40.0 {
            let n = mains.len();
            let rows = if n > 4 && kn.height() >= 200.0 { 2 } else { 1 };
            let cols = n.div_ceil(rows);
            let cw = kn.width() / cols as f32;
            let ch = (kn.height() / rows as f32).min(cw * 0.9 + 30.0);
            let top = kn.center().y - ch * rows as f32 * 0.5;
            for (i, (label, v)) in mains.iter().enumerate() {
                let cell = Rect::from_min_size(Pos2::new(kn.left() + (i % cols) as f32 * cw, top + (i / cols) as f32 * ch), egui::vec2(cw, ch));
                if let Some(nv) = knob(ui, cid(("con_macro", i)), cell, label, *v, theme::LIVE_TEAL) {
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
            p.rect_filled(pad, 8.0, theme::PAPER_SINK);
            for k in 1..4 {
                let f = k as f32 / 4.0;
                let gx = pad.left() + pad.width() * f;
                let gy = pad.top() + pad.height() * f;
                p.line_segment([Pos2::new(gx, pad.top() + 4.0), Pos2::new(gx, pad.bottom() - 4.0)], Stroke::new(1.0, alpha(theme::CREAM, 0.05)));
                p.line_segment([Pos2::new(pad.left() + 4.0, gy), Pos2::new(pad.right() - 4.0, gy)], Stroke::new(1.0, alpha(theme::CREAM, 0.05)));
            }
            p.rect_stroke(pad, 8.0, Stroke::new(1.0, if resp.hovered() { theme::CREAM_DIM } else { theme::HAIRLINE }), egui::StrokeKind::Inside);
            let axis = if live { format!("X {} · Y {}", mains[0].0, mains[1].0) } else { "X/Y needs two macros".into() };
            let (axis, af) = fit_text(ui, &axis, pad.width(), 10.5, 8.0, false);
            p.text(Pos2::new(pad.center().x, pad.bottom() + 11.0), egui::Align2::CENTER_CENTER, axis, af, theme::INK_2);
            if live {
                let c = Pos2::new(pad.left() + nx * pad.width(), pad.top() + (1.0 - ny) * pad.height());
                p.line_segment([Pos2::new(c.x, pad.top() + 2.0), Pos2::new(c.x, pad.bottom() - 2.0)], Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.35)));
                p.line_segment([Pos2::new(pad.left() + 2.0, c.y), Pos2::new(pad.right() - 2.0, c.y)], Stroke::new(1.0, alpha(theme::LIVE_TEAL, 0.35)));
                p.circle_filled(c, 14.0, alpha(theme::LIVE_TEAL, 0.15));
                p.circle_filled(c, 6.0, theme::LIVE_TEAL);
            }
        }
    }
}

// ----------------------------------------------------------------------
// Painted console parts. Each takes an absolute rect and an id, so the
// layouts above place them like parts on a panel.

fn vgrad(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    if !r.is_positive() { return; }
    let mut m = egui::epaint::Mesh::default();
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
    let mut m = egui::epaint::Mesh::default();
    m.colored_vertex(r.left_top(), left);
    m.colored_vertex(r.right_top(), right);
    m.colored_vertex(r.right_bottom(), right);
    m.colored_vertex(r.left_bottom(), left);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(m));
}

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

/// A small upper-case caption over a group of parts.
fn caption(p: &egui::Painter, at: Pos2, text: &str) {
    p.text(at, egui::Align2::LEFT_TOP, text, FontId::proportional(9.5), theme::CREAM_DIM);
}

/// A tablet switch: lit in `lit` with an LED while on.
fn tab(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, on: bool, lit: Color32) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let p = ui.painter();
    let fill = if on { mix(theme::DARK_PANEL_HI, lit, 0.28) } else if resp.hovered() { theme::DARK_PANEL_HI } else { theme::PAPER_SINK };
    p.rect_filled(r, 4.0, fill);
    p.rect_stroke(r, 4.0, Stroke::new(1.0, if on { alpha(lit, 0.8) } else { theme::HAIRLINE }), egui::StrokeKind::Inside);
    let led_room = if r.width() >= 52.0 { 12.0 } else { 0.0 };
    if led_room > 0.0 {
        let c = Pos2::new(r.left() + 9.0, r.center().y);
        if on {
            p.circle_filled(c, 5.0, alpha(lit, 0.25));
            p.circle_filled(c, 2.5, lit);
        } else {
            p.circle_stroke(c, 2.5, Stroke::new(1.0, theme::MUTED));
        }
    }
    let (t, f) = fit_text(ui, label, r.width() - 8.0 - led_room, (r.height() * 0.42).clamp(8.5, 12.0), 6.5, true);
    ui.painter().text(Pos2::new(r.center().x + led_room * 0.5, r.center().y), egui::Align2::CENTER_CENTER, t, f, if on { theme::CREAM } else { theme::INK_2 });
    resp
}

/// A reverse-coloured preset key: black with its name in ivory.
fn preset_key(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter();
    let body = if down { r.translate(egui::vec2(0.0, 2.0)) } else { r };
    vgrad(p, body, Color32::from_rgb(0x2A, 0x28, 0x26), Color32::from_rgb(0x0E, 0x0D, 0x0C));
    p.rect_stroke(body, 3.0, Stroke::new(1.0, if resp.hovered() { theme::CREAM_DIM } else { BORDER }), egui::StrokeKind::Inside);
    let (t, f) = fit_text(ui, label, r.width() - 6.0, 10.5, 7.0, true);
    ui.painter().text(Pos2::new(body.center().x, body.bottom() - 10.0), egui::Align2::CENTER_CENTER, t, f, IVORY_KNOB);
    resp
}

/// A big arc knob in `cell` (label under it): drag up/down.
fn knob(ui: &mut egui::Ui, id: egui::Id, cell: Rect, label: &str, value: f32, accent: Color32) -> Option<f32> {
    let label_h = 16.0;
    let d = (cell.width() * 0.7).min(cell.height() - label_h - 12.0).clamp(8.0, 130.0);
    let c = Pos2::new(cell.center().x, cell.top() + 4.0 + d * 0.5 + 2.0);
    let hit = Rect::from_center_size(c, egui::vec2(d + 8.0, d + 8.0));
    let resp = ui.interact(hit, id, egui::Sense::click_and_drag()).on_hover_text(format!("{label}: drag up/down"));
    let out = resp.dragged().then(|| (value - resp.drag_delta().y / (d * 2.5).max(80.0)).clamp(0.0, 1.0));
    let v = out.unwrap_or(value).clamp(0.0, 1.0);
    let p = ui.painter();
    let r = d * 0.5;
    // The value ring: 270° from the lower left.
    let a0 = PI * 0.75;
    let ring = |from: f32, to: f32, col: Color32, w: f32| {
        let steps = 40;
        let pts: Vec<Pos2> = (0..=steps).map(|k| {
            let a = from + (to - from) * k as f32 / steps as f32;
            c + egui::vec2(a.cos(), a.sin()) * (r + 4.0)
        }).collect();
        p.add(egui::Shape::line(pts, Stroke::new(w, col)));
    };
    ring(a0, a0 + PI * 1.5, theme::HAIRLINE, 3.0);
    if v > 0.005 { ring(a0, a0 + PI * 1.5 * v, accent, 3.0); }
    p.circle_filled(c + egui::vec2(0.0, 2.0), r - 1.0, alpha(BLACK, 0.6));
    p.circle_filled(c, r - 2.0, METAL_HI);
    p.circle_filled(c - egui::vec2(0.0, r * 0.18), r * 0.7, alpha(theme::CREAM, 0.04));
    p.circle_stroke(c, r - 2.0, Stroke::new(1.0, if resp.hovered() || resp.dragged() { theme::CREAM_DIM } else { theme::HAIRLINE }));
    let a = a0 + PI * 1.5 * v;
    let dir = egui::vec2(a.cos(), a.sin());
    p.line_segment([c + dir * (r * 0.25), c + dir * (r - 5.0)], Stroke::new(2.5, theme::CREAM));
    let (t, f) = fit_text(ui, label, cell.width() - 4.0, 10.5, 7.5, false);
    ui.painter().text(Pos2::new(cell.center().x, c.y + r + 6.0), egui::Align2::CENTER_TOP, t, f, theme::INK_2);
    out
}

/// One tonewheel drawbar at full length: the footage over its slot, the
/// shaft pulled down to `pos` (0..8) with its stops marked, the coloured
/// tip at the end (brown: sub-octave, ivory: octaves, black: the rest).
fn drawbar(ui: &mut egui::Ui, id: egui::Id, r: Rect, bar: usize, footage: &str, pos: u8) -> Option<u8> {
    let slot = Rect::from_min_max(r.min + egui::vec2(0.0, 18.0), r.max);
    let tip_h = (r.width() * 1.1).clamp(18.0, 44.0);
    let unit = (slot.height() - tip_h - 4.0) / 8.0;
    let resp = ui.interact(r, id, egui::Sense::click_and_drag()).on_hover_text(format!("drawbar {footage}: drag down to pull it out (0 to 8)"));
    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(q) = resp.interact_pointer_pos() {
            out = Some((((q.y - slot.top() - tip_h * 0.5) / unit).round()).clamp(0.0, 8.0) as u8);
        }
    }
    let pos = out.unwrap_or(pos);
    let p = ui.painter();
    let (t, f) = fit_text(ui, footage, r.width() + 6.0, 11.0, 8.0, true);
    p.text(Pos2::new(r.center().x, r.top() + 7.0), egui::Align2::CENTER_CENTER, t, f, theme::INK_2);
    p.rect_filled(slot, 3.0, Color32::from_rgb(0x09, 0x08, 0x07));
    let (tip, ink) = match bar {
        0 | 1 => (theme::DRAWBAR_BROWN, theme::CREAM),
        4 | 6 | 7 => (Color32::from_rgb(0x1E, 0x1C, 0x1A), theme::CREAM),
        _ => (IVORY_KNOB, theme::DARK_PANEL),
    };
    let shaft_w = r.width() * 0.5;
    let shaft = Rect::from_min_size(Pos2::new(r.center().x - shaft_w * 0.5, slot.top() + 2.0), egui::vec2(shaft_w, unit * pos as f32));
    if pos > 0 {
        hgrad(p, Rect::from_min_max(shaft.min, Pos2::new(shaft.center().x, shaft.bottom())), Color32::from_rgb(0x5A, 0x56, 0x50), Color32::from_rgb(0xB8, 0xB2, 0xA8));
        hgrad(p, Rect::from_min_max(Pos2::new(shaft.center().x, shaft.top()), shaft.max), Color32::from_rgb(0xB8, 0xB2, 0xA8), Color32::from_rgb(0x5A, 0x56, 0x50));
        if unit >= 11.0 {
            for k in 1..=pos {
                let y = slot.top() + 2.0 + unit * (k as f32 - 0.5);
                p.text(Pos2::new(shaft.center().x, y), egui::Align2::CENTER_CENTER, k.to_string(), FontId::proportional((unit * 0.6).clamp(7.5, 11.0)), Color32::from_rgb(0x22, 0x20, 0x1E));
            }
        }
    }
    let tip_r = Rect::from_min_size(Pos2::new(r.left() + 1.0, slot.top() + 2.0 + unit * pos as f32), egui::vec2(r.width() - 2.0, tip_h));
    p.rect_filled(tip_r.translate(egui::vec2(0.0, 2.0)), 5.0, alpha(BLACK, 0.5));
    p.rect_filled(tip_r, 5.0, tip);
    p.rect_stroke(tip_r, 5.0, Stroke::new(1.0, if resp.hovered() || resp.dragged() { theme::CREAM_DIM } else { alpha(BLACK, 0.6) }), egui::StrokeKind::Inside);
    p.text(tip_r.center(), egui::Align2::CENTER_CENTER, pos.to_string(), theme::medium_font((tip_h * 0.42).clamp(9.0, 16.0)), ink);
    out
}

/// The Leslie seen from above: the cabinet and the rotating horn.
fn rotor(p: &egui::Painter, c: Pos2, r: f32, angle: f32, spinning: bool) {
    p.circle_filled(c, r, Color32::from_rgb(0x1A, 0x14, 0x0E));
    p.circle_stroke(c, r, Stroke::new(1.5, WALNUT_HI));
    p.circle_stroke(c, r * 0.72, Stroke::new(1.0, alpha(theme::CREAM, 0.06)));
    for k in [0.0, PI] {
        let a = angle + k;
        let dir = egui::vec2(a.cos(), a.sin());
        let n = egui::vec2(-dir.y, dir.x);
        let tip = c + dir * (r * 0.82);
        p.add(egui::Shape::convex_polygon(
            vec![c + n * (r * 0.06), tip + n * (r * 0.2), tip - n * (r * 0.2), c - n * (r * 0.06)],
            if k == 0.0 { if spinning { theme::LIVE_TEAL } else { theme::CREAM_DIM } } else { mix(theme::CREAM_DIM, BLACK, 0.4) },
            Stroke::NONE,
        ));
    }
    p.circle_filled(c, r * 0.14, METAL_HI);
}

/// A swell (expression) shoe: a ribbed pedal plate; drag left/right.
fn swell_shoe(ui: &mut egui::Ui, id: egui::Id, r: Rect, value: f32) -> Option<f32> {
    let resp = ui.interact(r, id, egui::Sense::click_and_drag()).on_hover_text("swell pedal (CC 11): drag to open or close");
    let label_w = 54.0;
    let plate = Rect::from_min_max(Pos2::new(r.left() + label_w, r.top() + 4.0), r.max - egui::vec2(0.0, 4.0));
    let mut out = None;
    if resp.dragged() || resp.clicked() {
        if let Some(q) = resp.interact_pointer_pos() {
            out = Some(((q.x - plate.left()) / plate.width()).clamp(0.0, 1.0));
        }
    }
    let v = out.unwrap_or(value).clamp(0.0, 1.0);
    let p = ui.painter();
    p.text(Pos2::new(r.left(), r.center().y), egui::Align2::LEFT_CENTER, "SWELL", FontId::proportional(10.0), theme::CREAM_DIM);
    p.rect_filled(plate, 6.0, Color32::from_rgb(0x0F, 0x0D, 0x0B));
    let fill = Rect::from_min_size(plate.min, egui::vec2(plate.width() * v, plate.height()));
    if fill.width() > 1.0 {
        hgrad(p, fill.shrink(2.0), BRASS_DEEP, BRASS);
    }
    let ribs = ((plate.width() / 9.0) as usize).max(4);
    for k in 1..ribs {
        let x = plate.left() + plate.width() * k as f32 / ribs as f32;
        p.line_segment([Pos2::new(x, plate.top() + 5.0), Pos2::new(x, plate.bottom() - 5.0)], Stroke::new(1.0, alpha(BLACK, 0.35)));
    }
    p.rect_stroke(plate, 6.0, Stroke::new(1.0, if resp.hovered() || resp.dragged() { theme::CREAM_DIM } else { theme::HAIRLINE }), egui::StrokeKind::Inside);
    out
}

/// A horizontal value bar (VEL): label left, value right, drag to set.
fn hbar(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, value: f32, col: Color32) -> Option<f32> {
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
    let v = out.unwrap_or(value);
    let p = ui.painter();
    p.text(Pos2::new(r.left(), r.center().y), egui::Align2::LEFT_CENTER, label, FontId::proportional(10.0), theme::CREAM_DIM);
    p.rect_filled(track, 3.0, theme::PAPER_SINK);
    p.rect_stroke(track, 3.0, Stroke::new(1.0, theme::HAIRLINE), egui::StrokeKind::Inside);
    p.rect_filled(Rect::from_min_max(track.min + egui::vec2(2.0, 3.0), Pos2::new(track.left() + 2.0 + (track.width() - 4.0) * v, track.bottom() - 3.0)), 2.0, col);
    p.text(Pos2::new(r.right(), r.center().y), egui::Align2::RIGHT_CENTER, format!("{v:.2}"), FontId::proportional(10.0), theme::INK_2);
    out
}

/// A brass piano pedal (pressed while `on`), its name under it.
fn pedal(ui: &mut egui::Ui, id: egui::Id, r: Rect, label: &str, on: bool) -> egui::Response {
    let resp = ui.interact(r, id, egui::Sense::click());
    let p = ui.painter();
    let body = if on { r.translate(egui::vec2(0.0, 3.0)) } else { r };
    let w = body.width();
    let pts = vec![
        Pos2::new(body.left() + w * 0.18, body.top()),
        Pos2::new(body.right() - w * 0.18, body.top()),
        Pos2::new(body.right(), body.bottom()),
        Pos2::new(body.left(), body.bottom()),
    ];
    if !on {
        p.add(egui::Shape::convex_polygon(pts.iter().map(|q| *q + egui::vec2(0.0, 3.0)).collect(), alpha(BLACK, 0.6), Stroke::NONE));
    }
    let fill = if on { mix(BRASS, theme::CREAM, 0.25) } else if resp.hovered() { mix(BRASS, theme::CREAM, 0.1) } else { BRASS };
    p.add(egui::Shape::convex_polygon(pts, fill, Stroke::new(1.0, BRASS_DEEP)));
    p.text(Pos2::new(r.center().x, r.bottom() + 6.0), egui::Align2::CENTER_TOP, label, FontId::proportional(10.0), if on { theme::LIVE_TEAL } else { theme::CREAM_DIM });
    resp
}

/// A thumb piston (round button, number on it) with its name under it.
fn piston(ui: &mut egui::Ui, id: egui::Id, cell: Rect, d: f32, number: &str, name: &str) -> egui::Response {
    let c = Pos2::new(cell.center().x, cell.top() + d * 0.5 + 2.0);
    let hit = Rect::from_center_size(c, egui::vec2(d + 6.0, d + 6.0));
    let resp = ui.interact(hit, id, egui::Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter();
    p.circle_filled(c + egui::vec2(0.0, 2.0), d * 0.5, alpha(BLACK, 0.6));
    p.circle_filled(c + egui::vec2(0.0, if down { 1.5 } else { 0.0 }), d * 0.5, if down { theme::ACCENT } else { IVORY_KNOB });
    p.circle_stroke(c, d * 0.5, Stroke::new(1.0, if resp.hovered() { theme::CREAM } else { BRASS_DEEP }));
    p.text(c, egui::Align2::CENTER_CENTER, number, theme::medium_font((d * 0.45).clamp(9.0, 15.0)), theme::DARK_PANEL);
    if cell.bottom() - (c.y + d * 0.5) >= 12.0 {
        let (t, f) = fit_text(ui, name, cell.width() - 6.0, 10.0, 7.5, false);
        ui.painter().text(Pos2::new(c.x, c.y + d * 0.5 + 3.0), egui::Align2::CENTER_TOP, t, f, theme::CREAM_DIM);
    }
    resp
}

/// One pitch/mod wheel standing in its slot: a ribbed cylinder that turns
/// with the value (`t` 0..1), the marker at its position.
fn wheel(p: &egui::Painter, r: Rect, t: f32, center_detent: bool, hot: bool) {
    p.rect_filled(r.expand(3.0), 5.0, BLACK);
    let mid = Rect::from_min_max(Pos2::new(r.left(), r.center().y - r.height() * 0.15), Pos2::new(r.right(), r.center().y + r.height() * 0.15));
    vgrad(p, Rect::from_min_max(r.min, Pos2::new(r.right(), mid.top())), Color32::from_rgb(0x0E, 0x0D, 0x0C), Color32::from_rgb(0x3A, 0x38, 0x35));
    p.rect_filled(mid, 0.0, Color32::from_rgb(0x3A, 0x38, 0x35));
    vgrad(p, Rect::from_min_max(Pos2::new(r.left(), mid.bottom()), r.max), Color32::from_rgb(0x3A, 0x38, 0x35), Color32::from_rgb(0x0E, 0x0D, 0x0C));
    // Ribs roll with the wheel.
    let spacing = 9.0;
    let phase = (t * r.height() * 0.8) % spacing;
    let mut y = r.top() + phase;
    while y < r.bottom() {
        let depth = 1.0 - ((y - r.center().y).abs() / (r.height() * 0.5)).min(1.0);
        p.line_segment([Pos2::new(r.left() + 2.0, y), Pos2::new(r.right() - 2.0, y)], Stroke::new(1.0, alpha(BLACK, 0.25 + 0.35 * depth)));
        y += spacing;
    }
    if center_detent {
        p.line_segment([Pos2::new(r.left() - 3.0, r.center().y), Pos2::new(r.left(), r.center().y)], Stroke::new(2.0, theme::MUTED));
    }
    let my = r.bottom() - t.clamp(0.0, 1.0) * r.height();
    p.rect_filled(Rect::from_min_max(Pos2::new(r.left() + 1.0, my - 2.0), Pos2::new(r.right() - 1.0, my + 2.0)), 1.0, if hot { theme::ACCENT } else { theme::CREAM });
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
        let font = (cw * 0.12).clamp(7.5, 10.5);
        let label_h = 2.0 * font * 1.25 + 4.0;
        let d = (cw * 0.62).min(ch - label_h - 6.0).min(64.0);
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

/// A drawknob in `cell`: round ivory face with its pitch, pulled out (lit,
/// shadowed) while drawn; the stop's name under it in up to two lines.
fn drawknob(ui: &mut egui::Ui, id: egui::Id, cell: Rect, d: f32, name: &str, foot: &str, on: bool, font: f32) -> egui::Response {
    let c = Pos2::new(cell.center().x, cell.top() + d * 0.5 + 4.0);
    let hit = Rect::from_min_max(Pos2::new(cell.left() + 2.0, cell.top()), Pos2::new(cell.right() - 2.0, cell.bottom()));
    let resp = ui.interact(hit, id, egui::Sense::click());
    let p = ui.painter();
    let r = d * 0.5;
    if on {
        p.circle_filled(c + egui::vec2(0.0, 4.0), r, alpha(BLACK, 0.7));
        p.circle_filled(c, r + 3.0, alpha(theme::LIVE_TEAL, 0.18));
    }
    let face = if on { IVORY_KNOB } else { mix(IVORY_KNOB, Color32::from_rgb(0x17, 0x11, 0x0B), 0.62) };
    p.circle_filled(c, r, face);
    p.circle_stroke(c, r, Stroke::new(if on { 2.0 } else { 1.0 }, if on { theme::LIVE_TEAL } else if resp.hovered() { theme::CREAM_DIM } else { WALNUT_HI }));
    if !foot.is_empty() && d >= 16.0 {
        let (t, f) = fit_text(ui, foot, d * 0.8, (d * 0.3).clamp(7.0, 15.0), 6.5, true);
        ui.painter().text(c, egui::Align2::CENTER_CENTER, t, f, if on { theme::DARK_PANEL } else { alpha(IVORY_KNOB, 0.55) });
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
    let ink = if on { theme::CREAM } else { theme::CREAM_DIM };
    let mut y = c.y + r + 4.0;
    for line in lines {
        let (t, f) = fit_text(ui, &line, max_w, font, 6.5, false);
        let h = f.size * 1.2;
        if y + h > cell.bottom() + 1.0 { break; }
        ui.painter().text(Pos2::new(c.x, y), egui::Align2::CENTER_TOP, t, f, ink);
        y += h;
    }
    resp
}

/// A console part's id: global, keyed by what the part is (rule 135).
fn cid(salt: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
    egui::Id::NULL.with(("kb_console", salt))
}
