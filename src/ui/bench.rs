//! `kabelsalat ui-bench [frames]`: what a UI frame costs, without a window.
//! Each scene runs the app's real frame path (`frame_ui`) on a headless egui
//! context at 1600x1000 and 1.6x, then tessellates the frame as eframe does
//! before it hands the meshes to the GPU. No engine, no plugins, no devices:
//! the cables are kept alive through the same lock-free level table the
//! engine writes, so every cable breathes and flows in every frame.
//!
//! Per scene: the median and the 99th percentile of the frame time, split
//! into layout+paint (`ui`) and tessellation (`tess`), and the frame's
//! vertex count, which does not depend on how busy the machine is.

use super::*;

/// One instrument column per source, each through three effects into the
/// master, plus a dry cable: a rack as full as the stage shows at once.
fn big_rig(sources: usize) -> Graph {
    let mut g = Graph::default();
    g.reserve(64, 256);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
    let mut id = 0;
    let mut wire = |g: &mut Graph, from: u32, to: u32, port: u8, kind: CPortKind| {
        id += 1;
        g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port }, kind);
    };
    for s in 0..sources {
        let synth = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE_ID) }, name_bytes(&format!("SURGE {}", s + 1)));
        wire(&mut g, keys, synth, 0, CPortKind::Midi);
        let mut prev = synth;
        for (kind, name) in [(DspKind::Filter, "FILTER"), (DspKind::Delay, "DELAY"), (DspKind::Reverb, "REVERB")] {
            let fx = g.add_node(NodeKind::Dsp(kind), name_bytes(name));
            wire(&mut g, prev, fx, 0, CPortKind::Audio);
            prev = fx;
        }
        wire(&mut g, prev, master, 0, CPortKind::Audio);
        wire(&mut g, synth, master, 1, CPortKind::Audio);
    }
    g
}

struct Scene {
    name: &'static str,
    graph: Option<Graph>,
    view: StageView,
    side: Side,
}

struct Timing {
    ui: Vec<f64>,
    tess: Vec<f64>,
    vertices: usize,
    cables: usize,
    units: usize,
}

fn run_scene(scene: &Scene, frames: usize) -> Timing {
    const WARMUP: usize = 60;
    let mut app = DemoApp::new_app(None, false, Assistant::new());
    if let Some(g) = &scene.graph {
        app.build_rack(g, &[]);
    }
    app.stage_view = scene.view;
    app.side = scene.side;
    app.flip_to = scene.side;
    let levels = Arc::new(EdgeLevels::new());
    app.edge_levels = Some(levels.clone());
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    let mut t = Timing { ui: Vec::with_capacity(frames), tess: Vec::with_capacity(frames), vertices: 0, cables: 0, units: 0 };
    for i in 0..WARMUP + frames {
        // Every audio cable carries a level that moves, as while playing.
        for c in &app.cables {
            levels.store(c.id, 0.35 + 0.3 * ((i as f32 * 0.21) + c.id as f32).sin());
        }
        // A key every 20 frames: key LEDs and MIDI pulses.
        if i % 20 == 0 {
            app.fire_keys((i / 20) % 12, 0.8);
        } else if i % 20 == 10 {
            app.fire_keys((i / 20) % 12, 0.0);
        }
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.time = Some(i as f64 / 60.0);
        let t0 = std::time::Instant::now();
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        let t1 = std::time::Instant::now();
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        let t2 = std::time::Instant::now();
        out.textures_delta.clear(); // rule 11
        if i >= WARMUP {
            t.ui.push((t1 - t0).as_secs_f64() * 1e6);
            t.tess.push((t2 - t1).as_secs_f64() * 1e6);
            t.vertices = prims
                .iter()
                .map(|p| match &p.primitive {
                    egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                    _ => 0,
                })
                .sum();
        }
        std::hint::black_box(prims);
    }
    t.cables = app.cables.len();
    t.units = app.units.len();
    t
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let frames: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(600);
    let only = args.get(1);
    let scenes = [
        Scene { name: "rack-front", graph: None, view: StageView::Rack, side: Side::Front },
        Scene { name: "rack-back", graph: None, view: StageView::Rack, side: Side::Back },
        Scene { name: "big-front", graph: Some(big_rig(4)), view: StageView::Rack, side: Side::Front },
        Scene { name: "big-back", graph: Some(big_rig(4)), view: StageView::Rack, side: Side::Back },
        Scene { name: "mixer", graph: None, view: StageView::Mixer, side: Side::Front },
        Scene { name: "big-mixer", graph: Some(big_rig(4)), view: StageView::Mixer, side: Side::Front },
        Scene { name: "matrix", graph: Some(big_rig(4)), view: StageView::Matrix, side: Side::Front },
    ];
    println!("ui-bench: {frames} frames per scene at 1600x1000, 1.6x; times in us per frame");
    for s in scenes.iter().filter(|s| only.is_none_or(|o| o == s.name)) {
        let t = run_scene(s, frames);
        let stat = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(|a, b| a.total_cmp(b));
            (v[v.len() / 2], v[v.len() * 99 / 100], v[0])
        };
        let total: Vec<f64> = t.ui.iter().zip(&t.tess).map(|(a, b)| a + b).collect();
        let ((um, _, _), (tm, _, _), (m, p99, lo)) = (stat(&t.ui), stat(&t.tess), stat(&total));
        println!(
            "{:<11} units={:<2} cables={:<2} frame median={m:8.1}  min={lo:8.1}  p99={p99:8.1}  ui={um:8.1}  tess={tm:8.1}  vertices={}",
            s.name, t.units, t.cables, t.vertices
        );
    }
    Ok(())
}
