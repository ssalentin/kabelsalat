//! #309: rack viewport zoom. Every test runs real frames through the app's
//! own input path (`frame_ui`), never `layout` + `handle_input` alone.

use super::*;

const SIZE: Vec2 = Vec2::new(1280.0, 800.0);

struct Rig { app: DemoApp, ctx: egui::Context, t: f64, mods: Modifiers }

impl Rig {
    fn new() -> Self {
        let mut app = DemoApp::new_with_sink(None);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        app.load_multi_rig_demo();
        let mut r = Rig { app, ctx, t: 1.0, mods: Modifiers::NONE };
        for _ in 0..4 { r.frame(vec![]); }
        r
    }
    fn frame(&mut self, events: Vec<egui::Event>) {
        self.t += 0.05;
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, SIZE));
        raw.time = Some(self.t);
        raw.events = events;
        raw.events.insert(0, egui::Event::ModifiersChanged(self.mods));
        eframe::App::raw_input_hook(&mut self.app, &self.ctx, &mut raw);
        let mut out = self.ctx.run_ui(raw, |ui| self.app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
    }
    fn settle(&mut self) {
        for _ in 0..80 { self.frame(vec![]); if self.app.rack_zoom_goal.is_none() { break; } }
        for _ in 0..3 { self.frame(vec![]); }
    }
    fn key(&mut self, key: Key, m: Modifiers) {
        self.frame(vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: m }]);
        self.frame(vec![egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: m }]);
        self.settle();
    }
    fn zoom_to(&mut self, z: f32) {
        let at = self.app.xf.pivot;
        self.app.zoom_rack_at(z, at);
        self.settle();
    }
    fn press(&mut self, p: Pos2, down: bool, m: Modifiers) {
        self.frame(vec![egui::Event::PointerMoved(p), egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: down, modifiers: m }]);
    }
    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.frame(vec![egui::Event::PointerMoved(from)]);
        self.press(from, true, Modifiers::NONE);
        for i in 1..=6 { self.frame(vec![egui::Event::PointerMoved(from + (to - from) * (i as f32 / 6.0))]); }
        self.press(to, false, Modifiers::NONE);
        self.frame(vec![]);
    }
    /// A knob that is on screen, with its screen position.
    fn visible_knob(&self) -> (usize, usize, Pos2) {
        let view = self.app.rack_view;
        for (ui, u) in self.app.units.iter().enumerate() {
            if !self.app.visible(ui) { continue; }
            for ki in 0..u.knobs.len() {
                if u.knobs[ki].hidden { continue; }
                let p = self.app.xf.screen(self.app.knob_center(ui, ki));
                if view.shrink(40.0).contains(p) && matches!(self.app.hit_test(p), Hit::Knob { unit, idx } if unit == ui && idx == ki) {
                    return (ui, ki, p);
                }
            }
        }
        panic!("no knob on screen at zoom {}", self.app.rack_zoom);
    }
}

#[test]
fn keys_step_reset_and_fit_the_zoom() {
    let mut r = Rig::new();
    assert_eq!(r.app.rack_zoom, 1.0);
    r.key(Key::Equals, Modifiers::COMMAND);
    assert_eq!(r.app.rack_zoom, 1.25, "Ctrl+= steps up the ladder");
    r.key(Key::Minus, Modifiers::COMMAND);
    r.key(Key::Minus, Modifiers::COMMAND);
    assert_eq!(r.app.rack_zoom, 0.8);
    r.key(Key::Num0, Modifiers::COMMAND);
    assert_eq!(r.app.rack_zoom, 1.0, "Ctrl+0 back to 100 %");
    assert_eq!(r.app.rack_pan, Vec2::ZERO);
    // F4 still loads the multi-rig demo, untouched by the zoom keys.
    r.key(Key::F4, Modifiers::NONE);
    assert_eq!(r.app.rack_zoom, 1.0);
}

#[test]
fn fit_all_zooms_an_oversized_rack_out_until_it_fits() {
    let mut r = Rig::new();
    // A short stage: the rack overflows at 100 %.
    r.app.rack_view = Rect::from_min_size(r.app.rack_view.min, Vec2::new(r.app.rack_w * 0.5, r.app.rack_h * 0.5));
    let fit = r.app.rack_fit_zoom();
    assert!(fit < 0.6 && fit >= RACK_ZOOM_MIN, "fit {fit}");
    r.app.zoom_rack_reset();
    for _ in 0..80 { r.app.step_rack_zoom(0.05); }
    assert!((r.app.rack_zoom - fit).abs() < 1e-3, "{} vs {fit}", r.app.rack_zoom);
    assert!(r.app.rack_overflow_at(r.app.rack_zoom).x < 1.0, "the whole rack fits");
}

#[test]
fn ctrl_scroll_zooms_about_the_pointer() {
    let mut r = Rig::new();
    let at = r.app.rack_view.center() + Vec2::new(200.0, 60.0);
    let before = r.app.xf.inv(at);
    r.frame(vec![egui::Event::PointerMoved(at)]);
    r.frame(vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, 80.0), modifiers: Modifiers::CTRL, phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Wheel }]);
    r.frame(vec![]);
    assert!(r.app.rack_zoom > 1.0, "zoom {}", r.app.rack_zoom);
    let after = r.app.xf.inv(at);
    assert!((after - before).length() < 1.5, "the rack point under the pointer stayed put: {before:?} → {after:?}");
}

#[test]
fn knob_hit_test_and_drag_are_exact_at_non_unit_zoom_on_the_front() {
    for z in [0.5, 0.67, 1.5, 2.0] {
        let mut r = Rig::new();
        r.zoom_to(z);
        let (ui, ki, p) = r.visible_knob();
        let before = r.app.units[ui].knobs[ki].value;
        r.drag(p, p - Vec2::new(0.0, 40.0));
        let after = r.app.units[ui].knobs[ki].value;
        assert!(after > before + 0.01, "zoom {z}: dragging the knob up raised it ({before} → {after})");
        // Just outside the knob (a zoom-scaled 23 pt radius) is not the knob.
        let outside = p + Vec2::new(23.0 * z + 6.0, 0.0);
        assert!(!matches!(r.app.hit_test(outside), Hit::Knob { unit, idx } if unit == ui && idx == ki), "zoom {z}");
    }
}

#[test]
fn jack_and_cable_hits_are_exact_at_non_unit_zoom_on_the_back() {
    for z in [0.5, 1.5, 2.0] {
        let mut r = Rig::new();
        r.app.side = Side::Back;
        r.app.flip_to = Side::Back;
        r.app.flipping = false;
        r.zoom_to(z);
        let view = r.app.rack_view.shrink(30.0);
        let mut seen = 0;
        for ui in 0..r.app.units.len() {
            for ji in 0..r.app.units[ui].jacks.len() {
                let p = r.app.xf.screen(r.app.jack_pos(ui, ji));
                if view.contains(p) && r.app.visible(ui) {
                    assert!(matches!(r.app.hit_test(p), Hit::Jack { unit, jack } if unit == ui && jack == ji), "zoom {z}: jack {ui}:{ji}");
                    seen += 1;
                }
            }
        }
        assert!(seen > 2, "zoom {z}: jacks were on screen");
        // A cable midpoint (rack-local polyline point) is hit through the zoom.
        let c = r.app.cables.iter().find(|c| c.dying < 0.0 && c.n > 4).expect("a living cable");
        let mid = c.pts[c.n / 2];
        let sp = r.app.xf.screen(mid.to_vec2());
        if view.contains(sp) && !matches!(r.app.hit_test(sp), Hit::Jack { .. }) {
            assert!(matches!(r.app.hit_test(sp), Hit::Cable(..) | Hit::Unit(_)), "zoom {z}");
        }
        // The jack's hover tooltip anchors at the jack's screen position.
        let (ui, ji) = (c.src, r.app.units[c.src].jack_index(Dir::Out, c.src_port).unwrap());
        let jp = r.app.xf.screen(r.app.jack_pos(ui, ji));
        assert!((r.app.xf.inv(jp) - r.app.jack_pos(ui, ji)).length() < 0.01, "screen/inv round-trip at zoom {z}");
    }
}

#[test]
fn a_cable_dragged_between_jacks_connects_at_zoom() {
    let mut r = Rig::new();
    r.app.side = Side::Back;
    r.app.flip_to = Side::Back;
    r.app.flipping = false;
    r.zoom_to(1.5);
    let view = r.app.rack_view.shrink(30.0);
    // Unplug a living cable whose jacks are both on screen, then plug it back by hand.
    let mut pair = None;
    for (ci, c) in r.app.cables.iter().enumerate() {
        if c.dying >= 0.0 { continue; }
        let (ja, jb) = (r.app.units[c.src].jack_index(Dir::Out, c.src_port).unwrap(), r.app.units[c.dst].jack_index(Dir::In, c.dst_port).unwrap());
        let (pa, pb) = (r.app.xf.screen(r.app.jack_pos(c.src, ja)), r.app.xf.screen(r.app.jack_pos(c.dst, jb)));
        if view.contains(pa) && view.contains(pb) && (pa - pb).length() > 60.0 {
            pair = Some((ci, c.src, c.src_port, c.dst, c.dst_port, pa, pb));
            break;
        }
    }
    let (ci, a, ap, b, bp, pa, pb) = pair.expect("a cable with both jacks on screen");
    r.app.unplug_cable(ci, true);
    for _ in 0..40 { r.frame(vec![]); }
    assert!(!r.app.cables.iter().any(|c| c.dying < 0.0 && c.src == a && c.src_port == ap && c.dst == b && c.dst_port == bp), "unplugged");
    r.drag(pa, pb);
    assert!(r.app.cables.iter().any(|c| c.dying < 0.0 && c.src == a && c.src_port == ap && c.dst == b && c.dst_port == bp),
        "the cable landed on the jacks it was dragged between");
}

#[test]
fn ctrl_drag_moves_a_unit_at_zoom() {
    let mut r = Rig::new();
    r.zoom_to(0.67);
    let (ui, p) = (0..r.app.units.len())
        .filter(|&u| Some(u) != r.app.keys_unit && Some(u) != r.app.master_unit && r.app.visible(u))
        .map(|u| (u, r.app.xf.screen(r.app.unit_rects[u].center().to_vec2())))
        .find(|&(u, p)| r.app.rack_view.contains(p) && matches!(r.app.hit_test(p), Hit::Unit(h) if h == u))
        .expect("a stack unit on screen");
    r.mods = Modifiers::CTRL;
    r.frame(vec![egui::Event::ModifiersChanged(Modifiers::CTRL), egui::Event::PointerMoved(p)]);
    r.press(p, true, Modifiers::CTRL);
    assert!(matches!(r.app.drag, Drag::Unit { unit, .. } if unit == ui), "Ctrl+drag grabs the unit at zoom");
    let (grab, rect) = (r.app.drag_cursor, r.app.unit_rects[ui]);
    r.frame(vec![egui::Event::PointerMoved(p + Vec2::new(30.0, 10.0))]);
    let moved = r.app.drag_cursor.expect("drag cursor");
    let want = r.app.xf.inv(p + Vec2::new(30.0, 10.0)).to_pos2();
    assert!((moved.to_pos2() - want).length() < 1.0, "cursor is in rack-local space ({moved:?} vs {want:?}, was {grab:?}, rect {rect:?})");
    r.mods = Modifiers::NONE;
    r.press(p, false, Modifiers::NONE);
}

#[test]
fn focus_zooms_onto_the_selected_unit_and_back() {
    let mut r = Rig::new();
    let ui = r.app.units.iter().position(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay))).expect("delay unit");
    r.app.selected_unit = Some(r.app.units[ui].node_id);
    r.key(Key::F, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(r.app.rack_zoom > 1.2, "zoomed in on one unit: {}", r.app.rack_zoom);
    let sr = r.app.xf.screen_rect(r.app.unit_rects[ui]);
    assert!(r.app.rack_view.expand(1.0).contains_rect(sr), "the unit fits the view: {sr:?} in {:?}", r.app.rack_view);
    assert!((sr.center() - r.app.rack_view.center()).length() < 2.0 || r.app.rack_overflow_at(r.app.rack_zoom).max_elem() > 0.0);
    r.key(Key::F, Modifiers::COMMAND | Modifiers::SHIFT);
    assert_eq!(r.app.rack_zoom, 1.0, "a second press zooms back out");
}

#[test]
fn zoom_font_sizes_are_a_small_fixed_set() {
    let mut sizes = std::collections::BTreeSet::new();
    for base in [9.0f32, 10.5, 12.5, 14.0, 18.0] {
        for k in 0..=1000 {
            let z = RACK_ZOOM_MIN + (RACK_ZOOM_MAX - RACK_ZOOM_MIN) * k as f32 / 1000.0;
            let s = zoom_font_size(base * z);
            assert!(s <= base * z + 0.01 || s <= 2.0, "never above the box: {s} > {}", base * z);
            sizes.insert((s * 2.0) as u32);
        }
    }
    assert!(sizes.len() < 60, "a bounded ladder: {} sizes", sizes.len());
}

#[test]
fn continuous_zoom_never_grows_the_font_atlas_after_a_warm_up() {
    let mut r = Rig::new();
    let atlas = |r: &Rig| r.ctx.fonts(|f| (f.font_atlas_fill_ratio(), f.font_image_size()));
    let sweep = |r: &mut Rig, offset: f32| {
        for k in 0..=120 {
            let z = RACK_ZOOM_MIN + (RACK_ZOOM_MAX - RACK_ZOOM_MIN) * ((k as f32 + offset) / 121.0);
            let at = r.app.xf.pivot;
            r.app.zoom_rack_at(z, at);
            r.frame(vec![]);
        }
    };
    sweep(&mut r, 0.0);
    sweep(&mut r, 0.0);
    let warm = atlas(&r);
    sweep(&mut r, 0.37);
    sweep(&mut r, 0.71);
    r.app.side = Side::Back;
    r.app.flip_to = Side::Back;
    r.app.flipping = false;
    sweep(&mut r, 0.0);
    sweep(&mut r, 0.53);
    let back = atlas(&r);
    eprintln!("font atlas warm {warm:?}, after {back:?}");
    // The back face has its own glyphs: warm that side too, then compare.
    sweep(&mut r, 0.11);
    let after = atlas(&r);
    assert_eq!(after, back, "a continuous zoom added glyphs to the font atlas");
    assert!(after.0 < 0.5, "far from egui's 80 % clear: {}", after.0);
}

#[test]
fn zoomed_text_stays_inside_its_scaled_unit_box() {
    let mut r = Rig::new();
    r.zoom_to(0.5);
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, SIZE));
    raw.time = Some(r.t + 0.05);
    eframe::App::raw_input_hook(&mut r.app, &r.ctx, &mut raw);
    let mut out = r.ctx.run_ui(raw, |ui| r.app.frame_ui(ui));
    out.textures_delta.clear();
    let rack = r.app.xf.screen_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(r.app.rack_w, r.app.rack_h)));
    fn walk(s: &Shape, rack: Rect, bad: &mut Vec<String>) {
        match s {
            Shape::Text(t) if t.galley.text().trim().len() > 2 => {
                let b = t.visual_bounding_rect();
                let c = b.center();
                if rack.contains(c) && !rack.expand(2.0).contains_rect(b) { bad.push(format!("{} {b:?}", t.galley.text())); }
            }
            Shape::Vec(v) => for x in v { walk(x, rack, bad) },
            _ => {}
        }
    }
    let mut bad = Vec::new();
    for cs in &out.shapes { walk(&cs.shape, rack, &mut bad); }
    assert!(bad.is_empty(), "text outside the rack at 50 %: {bad:?}");
}

/// `KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat zoom_shots -- --ignored`
#[test]
#[ignore = "writes PNGs"]
fn zoom_shots() {
    let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, zoom, back, focus) in [
        ("zoom-100-front", 1.0, false, false), ("zoom-50-front", 0.5, false, false),
        ("zoom-200-front", 2.0, false, false), ("zoom-50-back", 0.5, true, false),
        ("zoom-150-back", 1.5, true, false), ("zoom-focus-delay", 1.0, false, true),
    ] {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        if back { app.side = Side::Back; app.flip_to = Side::Back; }
        let delay = app.units.iter().position(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay)));
        if focus { app.selected_unit = delay.map(|d| app.units[d].node_id); }
        super::offline_shots::shot_with(&mut app, Vec2::new(1280.0, 800.0), &dir.join(format!("{name}.png")), |a, i| {
            if i == 2 {
                if focus {
                    if let Some(d) = delay { a.zoom_rack_focus(d); a.rack_zoom = a.rack_zoom_goal.unwrap().0; a.rack_pan = a.rack_zoom_goal.unwrap().1; a.rack_zoom_goal = None; }
                } else if zoom != 1.0 {
                    let at = a.xf.pivot;
                    a.zoom_rack_at(zoom, at);
                }
            }
            Vec::new()
        });
    }
}
