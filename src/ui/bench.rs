//! Headless benchmarks for UI frames, session loading, and startup/memory:
//!
//! - `kabelsalat ui-bench [frames] [scene...] [--json]`
//! - `kabelsalat session-load-bench (--builtin | <package.ksalat>...) [--rounds N] [--json]`
//! - `kabelsalat startup-bench [--with-plugins] [--json]`
//!
//! Each scene runs the real UI frame path (`frame_ui`) and egui tessellation
//! at 1600x1000 and 1.6x without opening a window or audio device.

use super::*;
use serde::Serialize;

const ALL_SCENES: &[&str] = &[
    "rack-front",
    "rack-back",
    "big-front",
    "big-back",
    "mixer",
    "big-mixer",
    "matrix",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiBenchOpts {
    frames: usize,
    scenes: Vec<String>,
    json: bool,
}

impl Default for UiBenchOpts {
    fn default() -> Self {
        Self { frames: 600, scenes: Vec::new(), json: false }
    }
}

fn parse_ui_bench_args(args: &[String]) -> anyhow::Result<UiBenchOpts> {
    let mut opts = UiBenchOpts::default();
    let mut saw_frames = false;
    for arg in args {
        if arg == "--json" {
            opts.json = true;
        } else if arg.starts_with('-') {
            anyhow::bail!("unknown ui-bench flag: {arg}");
        } else if !saw_frames && arg.chars().all(|c| c.is_ascii_digit()) {
            let frames: usize = arg.parse()?;
            anyhow::ensure!(frames >= 1, "ui-bench frames must be >= 1");
            opts.frames = frames;
            saw_frames = true;
        } else {
            anyhow::ensure!(
                ALL_SCENES.contains(&arg.as_str()),
                "unknown ui-bench scene {arg:?} (expected one of: {})",
                ALL_SCENES.join(", ")
            );
            if !opts.scenes.iter().any(|s| s == arg) {
                opts.scenes.push(arg.clone());
            }
            saw_frames = true;
        }
    }
    Ok(opts)
}

#[derive(Debug, Clone, Serialize)]
struct UiSceneRow {
    name: &'static str,
    units: usize,
    cables: usize,
    frame_median_us: f64,
    frame_min_us: f64,
    frame_p99_us: f64,
    ui_median_us: f64,
    tess_median_us: f64,
    vertices: usize,
}

#[derive(Debug, Clone, Serialize)]
struct UiBenchReport {
    bench: &'static str,
    frames: usize,
    viewport: [u32; 2],
    scale: f32,
    scenes: Vec<UiSceneRow>,
}

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

fn stat_f64(v: &[f64]) -> (f64, f64, f64) {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    (v[v.len() / 2], v[v.len() * 99 / 100], v[0])
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_ui_bench_args(args)?;
    let scenes = [
        Scene { name: "rack-front", graph: None, view: StageView::Rack, side: Side::Front },
        Scene { name: "rack-back", graph: None, view: StageView::Rack, side: Side::Back },
        Scene { name: "big-front", graph: Some(big_rig(4)), view: StageView::Rack, side: Side::Front },
        Scene { name: "big-back", graph: Some(big_rig(4)), view: StageView::Rack, side: Side::Back },
        Scene { name: "mixer", graph: None, view: StageView::Mixer, side: Side::Front },
        Scene { name: "big-mixer", graph: Some(big_rig(4)), view: StageView::Mixer, side: Side::Front },
        Scene { name: "matrix", graph: Some(big_rig(4)), view: StageView::Matrix, side: Side::Front },
    ];
    if !opts.json {
        println!("ui-bench: {} frames per scene at 1600x1000, 1.6x; times in us per frame", opts.frames);
    }
    let mut rows = Vec::new();
    for s in scenes.iter().filter(|s| opts.scenes.is_empty() || opts.scenes.iter().any(|o| o == s.name)) {
        let t = run_scene(s, opts.frames);
        let total: Vec<f64> = t.ui.iter().zip(&t.tess).map(|(a, b)| a + b).collect();
        let ((um, _, _), (tm, _, _), (m, p99, lo)) = (stat_f64(&t.ui), stat_f64(&t.tess), stat_f64(&total));
        if !opts.json {
            println!(
                "{:<11} units={:<2} cables={:<2} frame median={m:8.1}  min={lo:8.1}  p99={p99:8.1}  ui={um:8.1}  tess={tm:8.1}  vertices={}",
                s.name, t.units, t.cables, t.vertices
            );
        }
        rows.push(UiSceneRow {
            name: s.name,
            units: t.units,
            cables: t.cables,
            frame_median_us: m,
            frame_min_us: lo,
            frame_p99_us: p99,
            ui_median_us: um,
            tess_median_us: tm,
            vertices: t.vertices,
        });
    }
    if opts.json {
        let report = UiBenchReport {
            bench: "ui-bench",
            frames: opts.frames,
            viewport: [1600, 1000],
            scale: 1.6,
            scenes: rows,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// session-load-bench
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionLoadOpts {
    builtin: bool,
    json: bool,
    rounds: Option<usize>,
    paths: Vec<String>,
}

fn parse_rounds(raw: &str) -> anyhow::Result<usize> {
    let n: usize = raw.parse().map_err(|_| anyhow::anyhow!("--rounds expects a positive integer, got {raw:?}"))?;
    anyhow::ensure!(n >= 1, "--rounds must be >= 1");
    Ok(n)
}

fn parse_session_load_args(args: &[String]) -> anyhow::Result<SessionLoadOpts> {
    let mut opts = SessionLoadOpts { builtin: false, json: false, rounds: None, paths: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--builtin" {
            opts.builtin = true;
        } else if arg == "--json" {
            opts.json = true;
        } else if arg == "--rounds" {
            let val = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--rounds requires a value"))?;
            opts.rounds = Some(parse_rounds(val)?);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--rounds=") {
            opts.rounds = Some(parse_rounds(val)?);
        } else if arg.starts_with('-') {
            anyhow::bail!("unknown session-load-bench flag: {arg}");
        } else {
            opts.paths.push(arg.clone());
        }
        i += 1;
    }
    anyhow::ensure!(
        opts.builtin ^ !opts.paths.is_empty(),
        "usage: kabelsalat session-load-bench (--builtin | <package.ksalat>...) [--rounds N] [--json]"
    );
    Ok(opts)
}

struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic, plugin-free 4-rig gig session (35 nodes, aux bus, mixer
/// metadata, and a 10-song × 2-part setlist) so CI can time disk load +
/// `apply_session` without external plugins.
fn reference_session() -> anyhow::Result<crate::session::Session> {
    use crate::control::{AuxSend, SendSource};
    use crate::session::{Session, StripSaved};
    use crate::setlist::{Part, PartRig, Setlist, Song};

    let mut g = Graph::default();
    g.reserve(48, 64);
    let mut eid = 0u32;
    let mut wire = |g: &mut Graph, from: u32, to: u32, port: u8| {
        eid += 1;
        g.connect_with_id(eid, PortRef { node: from, port: 0 }, PortRef { node: to, port }, CPortKind::Audio);
    };

    let mut masters = Vec::new();
    let mut filt_nodes = Vec::new();
    let mut delay_nodes = Vec::new();
    let mut verb_nodes = Vec::new();
    let mut src_nodes = Vec::new();

    for rig in 0..4u8 {
        let keys = g.add_node(NodeKind::Keys, name_bytes(&format!("KEYS {}", rig + 1)));
        let src = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes(&format!("OSC {}", rig + 1)));
        let filt = g.add_node(NodeKind::Dsp(DspKind::Filter), name_bytes("FILTER"));
        let drv = g.add_node(NodeKind::Dsp(DspKind::Saturation), name_bytes("DRIVE"));
        let dly = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
        let eq = g.add_node(NodeKind::Dsp(DspKind::Eq3), name_bytes("EQ"));
        let rev = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));

        wire(&mut g, src, filt, 0);
        wire(&mut g, filt, drv, 0);
        wire(&mut g, drv, dly, 0);
        wire(&mut g, dly, master, 0);

        wire(&mut g, src, eq, 0);
        wire(&mut g, eq, rev, 0);
        wire(&mut g, rev, master, 0);

        wire(&mut g, src, master, 1);

        for id in [keys, src, filt, drv, dly, eq, rev, master] {
            g.set_rig(id, rig);
        }
        masters.push(master);
        src_nodes.push(src);
        filt_nodes.push(filt);
        delay_nodes.push(dly);
        verb_nodes.push(rev);
    }

    let bus = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("BUS"));
    let hall = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("HALL"));
    let glue = g.add_node(NodeKind::Dsp(DspKind::Limiter), name_bytes("GLUE"));
    wire(&mut g, bus, hall, 0);
    wire(&mut g, hall, glue, 0);
    wire(&mut g, glue, masters[0], 0);

    let mut s = Session::new("CI Reference Gig", g);
    s.rig_names = vec!["Keys".into(), "Pad".into(), "Lead".into(), "Bass".into()];

    for (i, &node) in filt_nodes.iter().enumerate() {
        s.set_param(node, 0, 0.35 + 0.1 * i as f64);
        s.set_param(node, 1, 0.25 + 0.05 * i as f64);
    }
    for (i, &node) in delay_nodes.iter().enumerate() {
        s.set_param(node, crate::engine::P_TIME, 0.3 + 0.08 * i as f64);
        s.set_param(node, crate::engine::P_MIX, 0.25 + 0.05 * i as f64);
    }
    for (i, &node) in verb_nodes.iter().enumerate() {
        s.set_param(node, 0, 0.2 + 0.06 * i as f64);
        s.set_param(node, 1, 0.55);
    }
    for (i, &node) in masters.iter().enumerate() {
        s.set_param(node, crate::engine::P_VOL, 0.78 - 0.03 * i as f64);
        s.set_param(node, crate::engine::P_WARM, 0.15);
    }
    for (i, &node) in src_nodes.iter().enumerate() {
        let pan = [-0.45, 0.45, -0.2, 0.2][i];
        s.set_strip(StripSaved { node, pan, color: Some(0xE06C50), ..StripSaved::default() });
    }
    s.set_sends(
        (0..4u8)
            .map(|rig| AuxSend { from: SendSource::Rig(rig), bus, level: 0.28, pre: false })
            .collect(),
    );

    let mut sl = Setlist::default();
    for song_idx in 0..10usize {
        let r0 = (song_idx % 4) as u8;
        let r1 = ((song_idx + 1) % 4) as u8;
        let verse = Part::new(
            "Verse",
            vec![
                PartRig { rig: r0, key_lo: 0, key_hi: 71, transpose: 0, level: 0.85, pan: Some(-0.2), ..PartRig::default() },
                PartRig { rig: r1, key_lo: 60, key_hi: 127, transpose: 0, level: 0.72, pan: Some(0.25), ..PartRig::default() },
            ],
        );
        let chorus = Part::new(
            "Chorus",
            vec![
                PartRig { rig: r0, key_lo: 0, key_hi: 127, transpose: 0, level: 0.92, pan: Some(-0.1), ..PartRig::default() },
                PartRig { rig: r1, key_lo: 48, key_hi: 127, transpose: 12, level: 0.80, pan: Some(0.15), ..PartRig::default() },
            ],
        );
        sl.songs.push(Song::new(format!("Song {}", song_idx + 1), vec![verse, chorus]));
    }
    sl.normalize();
    s.setlist = sl;
    s.validate()?;
    Ok(s)
}

#[derive(Debug, Clone, Serialize)]
struct SessionLoadRow {
    target: String,
    rounds: usize,
    nodes: usize,
    cables: usize,
    rigs: usize,
    songs: usize,
    disk_load_median_ms: f64,
    apply_median_ms: f64,
    total_median_ms: f64,
    slowest_frame_median_ms: f64,
    apply_rounds_ms: Vec<f64>,
    disk_load_rounds_ms: Vec<f64>,
    total_rounds_ms: Vec<f64>,
    slowest_frame_rounds_ms: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionLoadReport {
    bench: &'static str,
    mode: &'static str,
    rounds: usize,
    targets: Vec<SessionLoadRow>,
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn run_builtin_session_load(rounds: usize, print_human: bool) -> anyhow::Result<SessionLoadReport> {
    const SR: f64 = 48000.0;
    const BLOCK: usize = crate::bridge::BUFFER_FRAMES as usize;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let scratch = std::env::temp_dir().join(format!("kabelsalat-session-load-bench-{pid}-{nanos}"));
    std::fs::create_dir_all(&scratch)?;
    let _guard = TempDirGuard(scratch.clone());

    let pkg = scratch.join("CI Reference Gig.ksalat");
    let ref_sess = reference_session()?;
    let nodes = ref_sess.graph.nodes.len();
    let cables = ref_sess.graph.edges.len();
    let rigs = ref_sess.rig_names.len();
    let songs = ref_sess.setlist.songs.len();
    ref_sess.save(&pkg)?;

    let (bus, drain) = crate::control::command_bus();
    let mut engine = GraphEngine::new(Graph::default(), drain, SR, Vec::new());
    let mut app = DemoApp::new_app(None, false, Assistant::new());
    app.sessions_dir_override = Some(scratch.clone());
    app.build_rack(&Graph::default(), &[]);
    engine.set_rig_snapshot(app.rig_snapshot.clone());
    let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
    engine.set_graph_pub(graph_pub.clone());
    let (voice_tx, voice_rx) = rtrb::RingBuffer::new(VOICE_RING);
    let (retire_tx, retire_rx) = rtrb::RingBuffer::new(VOICE_RING);
    engine.set_voice_channels(voice_rx, retire_tx);
    app.graph_pub = Some(graph_pub);
    app.edge_levels = Some(engine.edge_levels_handle());
    app.node_times = Some(engine.node_times_handle());
    app.out_level = Some(engine.level_handle());
    app.attach_engine(Vec::new(), Vec::new(), Vec::new(), SR, voice_tx, retire_rx, Arc::new(Mutex::new(bus)));
    app.plugins_loaded = true;

    let ctx = egui::Context::default();
    ctx.set_visuals(theme::visuals());
    theme::install_spacing(&ctx);
    theme::install_fonts(&ctx);
    let mut buf = vec![0.0f32; BLOCK * 2];
    let mut frame_i = 0usize;
    let mut run_frame = |app: &mut DemoApp, engine: &mut GraphEngine| {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
        raw.time = Some(frame_i as f64 / 60.0);
        frame_i += 1;
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        out.textures_delta.clear(); // rule 11
        std::hint::black_box(prims);
        engine.render(&mut buf, 2);
    };

    // Warmup round so caches/fonts/layout buffers settle before timing.
    {
        let s = crate::session::Session::load(&pkg)?;
        app.apply_session(&s)?;
        for _ in 0..8 {
            run_frame(&mut app, &mut engine);
        }
    }

    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let mut disk_rounds = Vec::with_capacity(rounds);
    let mut apply_rounds = Vec::with_capacity(rounds);
    let mut total_rounds = Vec::with_capacity(rounds);
    let mut worst_rounds = Vec::with_capacity(rounds);

    for round in 0..rounds {
        let t_disk = std::time::Instant::now();
        let s = crate::session::Session::load(&pkg)?;
        let disk_ms = ms(t_disk.elapsed());

        let t_apply = std::time::Instant::now();
        app.apply_session(&s)?;
        let apply_ms = ms(t_apply.elapsed());
        let total_ms = disk_ms + apply_ms;

        let mut worst = 0.0f64;
        for _ in 0..16 {
            let f = std::time::Instant::now();
            run_frame(&mut app, &mut engine);
            worst = worst.max(ms(f.elapsed()));
        }
        if print_human {
            println!(
                "session-load-bench round {round} builtin: disk_load {disk_ms:.2} ms, apply_session {apply_ms:.2} ms, total {total_ms:.2} ms, slowest next frame {worst:.2} ms"
            );
        }
        disk_rounds.push(disk_ms);
        apply_rounds.push(apply_ms);
        total_rounds.push(total_ms);
        worst_rounds.push(worst);
    }

    let row = SessionLoadRow {
        target: "builtin-4rig-gig".into(),
        rounds,
        nodes,
        cables,
        rigs,
        songs,
        disk_load_median_ms: median_f64(disk_rounds.clone()),
        apply_median_ms: median_f64(apply_rounds.clone()),
        total_median_ms: median_f64(total_rounds.clone()),
        slowest_frame_median_ms: median_f64(worst_rounds.clone()),
        apply_rounds_ms: apply_rounds,
        disk_load_rounds_ms: disk_rounds,
        total_rounds_ms: total_rounds,
        slowest_frame_rounds_ms: worst_rounds,
    };
    if print_human {
        println!(
            "session-load-bench median: disk_load {:.2} ms, apply_session {:.2} ms, total {:.2} ms, slowest next frame {:.2} ms",
            row.disk_load_median_ms, row.apply_median_ms, row.total_median_ms, row.slowest_frame_median_ms
        );
    }
    Ok(SessionLoadReport { bench: "session-load-bench", mode: "builtin", rounds, targets: vec![row] })
}

pub fn run_session_load(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_session_load_args(args)?;
    let print_human = !opts.json;
    if opts.builtin {
        let rounds = opts.rounds.unwrap_or(5);
        let report = run_builtin_session_load(rounds, print_human)?;
        if opts.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        return Ok(());
    }

    let rounds = opts.rounds.unwrap_or(2);
    let mut rig = EvalRig::new(Assistant::new(), &(Vec::new(), crate::lv2::discover()))?;
    rig.settle();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let mut per_path: Vec<(String, usize, usize, usize, usize, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> = Vec::new();
    for p in &opts.paths {
        per_path.push((p.clone(), 0, 0, 0, 0, Vec::new(), Vec::new(), Vec::new(), Vec::new()));
    }
    for round in 0..rounds {
        for (idx, p) in opts.paths.iter().enumerate() {
            let t_disk = std::time::Instant::now();
            let s = crate::session::Session::load(std::path::Path::new(p))?;
            let disk_ms = ms(t_disk.elapsed());
            let t_apply = std::time::Instant::now();
            rig.app.apply_session(&s)?;
            let apply_ms = ms(t_apply.elapsed());
            let total_ms = disk_ms + apply_ms;
            let mut worst = 0.0f64;
            for _ in 0..100 {
                let f = std::time::Instant::now();
                rig.frame();
                worst = worst.max(ms(f.elapsed()));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if print_human {
                println!("session-load-bench round {round} {p}: apply_session {apply_ms:.1} ms, slowest next frame {worst:.1} ms");
            }
            let entry = &mut per_path[idx];
            entry.1 = s.graph.nodes.len();
            entry.2 = s.graph.edges.len();
            entry.3 = s.rig_names.len().max(1);
            entry.4 = s.setlist.songs.len();
            entry.5.push(disk_ms);
            entry.6.push(apply_ms);
            entry.7.push(total_ms);
            entry.8.push(worst);
        }
    }
    if opts.json {
        let targets = per_path
            .into_iter()
            .map(|(target, nodes, cables, rigs, songs, disk, apply, total, worst)| SessionLoadRow {
                target,
                rounds,
                nodes,
                cables,
                rigs,
                songs,
                disk_load_median_ms: median_f64(disk.clone()),
                apply_median_ms: median_f64(apply.clone()),
                total_median_ms: median_f64(total.clone()),
                slowest_frame_median_ms: median_f64(worst.clone()),
                apply_rounds_ms: apply,
                disk_load_rounds_ms: disk,
                total_rounds_ms: total,
                slowest_frame_rounds_ms: worst,
            })
            .collect();
        let report = SessionLoadReport { bench: "session-load-bench", mode: "paths", rounds, targets };
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// startup-bench
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StartupBenchOpts {
    json: bool,
    with_plugins: bool,
}

fn parse_startup_args(args: &[String]) -> anyhow::Result<StartupBenchOpts> {
    let mut opts = StartupBenchOpts::default();
    for arg in args {
        if arg == "--json" {
            opts.json = true;
        } else if arg == "--with-plugins" {
            opts.with_plugins = true;
        } else {
            anyhow::bail!("unknown startup-bench arg: {arg}");
        }
    }
    Ok(opts)
}

#[derive(Debug, Clone, Serialize)]
struct StartupBenchReport {
    bench: &'static str,
    with_plugins: bool,
    units: usize,
    cables: usize,
    voices: usize,
    vertices: usize,
    fonts_theme_ms: f64,
    init_ms: f64,
    first_frame_ms: f64,
    to_first_frame_ms: f64,
    rss_kib: u64,
    rss_mib: f64,
    priv_dirty_kib: u64,
    priv_dirty_mib: f64,
    clean_kib: u64,
    clean_mib: f64,
    hwm_kib: u64,
    hwm_mib: f64,
}

fn measure_startup(with_plugins: bool) -> anyhow::Result<StartupBenchReport> {
    const SR: f64 = 48000.0;
    const BLOCK: usize = crate::bridge::BUFFER_FRAMES as usize;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;

    let t_total = std::time::Instant::now();

    let t_fonts = std::time::Instant::now();
    let ctx = egui::Context::default();
    ctx.set_visuals(theme::visuals());
    theme::install_spacing(&ctx);
    theme::install_fonts(&ctx);
    let fonts_theme_ms = ms(t_fonts.elapsed());

    let t_init = std::time::Instant::now();
    let (voices, mains) = if with_plugins {
        startup_voices(SR)?
    } else {
        (Vec::new(), Vec::new())
    };
    let controllers: Vec<VoiceController> = voices.iter().map(|(_, v)| v.controller()).collect();
    let voice_ids: Vec<u64> = voices.iter().map(|(_, v)| v.voice_id()).collect();
    let (bus, drain) = crate::control::command_bus();
    let mut engine = GraphEngine::new(Graph::demo_patch(), drain, SR, voices);
    let mut app = DemoApp::new_app(None, false, Assistant::new());
    engine.set_rig_snapshot(app.rig_snapshot.clone());
    let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub { graph: Graph::demo_patch(), ..Default::default() }));
    engine.set_graph_pub(graph_pub.clone());
    app.graph_pub = Some(graph_pub);
    app.edge_levels = Some(engine.edge_levels_handle());
    app.node_times = Some(engine.node_times_handle());
    app.out_level = Some(engine.level_handle());
    app.plugins_loaded = true;
    let bus = Arc::new(Mutex::new(bus));
    if with_plugins {
        let (voice_tx, voice_rx) = rtrb::RingBuffer::new(VOICE_RING);
        let (retire_tx, retire_rx) = rtrb::RingBuffer::new(VOICE_RING);
        engine.set_voice_channels(voice_rx, retire_tx);
        app.attach_engine(mains, controllers, voice_ids, SR, voice_tx, retire_rx, bus);
    } else {
        app.sample_rate = SR;
        app.bus = Some(bus);
    }
    let init_ms = ms(t_init.elapsed());

    let t_frame = std::time::Instant::now();
    let mut buf = vec![0.0f32; BLOCK * 2];
    for _ in 0..if with_plugins { 2 } else { 1 } {
        engine.render(&mut buf, 2);
    }
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 1000.0)));
    raw.time = Some(0.0);
    let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
    let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
    out.textures_delta.clear(); // rule 11
    let vertices: usize = prims
        .iter()
        .map(|p| match &p.primitive {
            egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
            _ => 0,
        })
        .sum();
    std::hint::black_box(&prims);
    std::hint::black_box(&buf);
    let first_frame_ms = ms(t_frame.elapsed());
    let to_first_frame_ms = ms(t_total.elapsed());

    let mem = crate::engine::bench::read_process_mem();
    let report = StartupBenchReport {
        bench: "startup-bench",
        with_plugins,
        units: app.units.len(),
        cables: app.cables.len(),
        voices: app._surge_main.len(),
        vertices,
        fonts_theme_ms,
        init_ms,
        first_frame_ms,
        to_first_frame_ms,
        rss_kib: mem.rss_kib,
        rss_mib: mem.rss_kib as f64 / 1024.0,
        priv_dirty_kib: mem.priv_dirty_kib,
        priv_dirty_mib: mem.priv_dirty_kib as f64 / 1024.0,
        clean_kib: mem.clean_kib,
        clean_mib: mem.clean_kib as f64 / 1024.0,
        hwm_kib: mem.hwm_kib,
        hwm_mib: mem.hwm_kib as f64 / 1024.0,
    };
    drop(prims);
    drop(app);
    drop(engine);
    Ok(report)
}

pub fn run_startup(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_startup_args(args)?;
    let report = measure_startup(opts.with_plugins)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "startup-bench (with_plugins={}): to_first_frame={:.2} ms (fonts={:.2} ms, init={:.2} ms, first_frame={:.2} ms)  units={} cables={} voices={} vertices={}  rss={:.1} MiB ({} KiB) priv_dirty={:.1} MiB hwm={:.1} MiB",
            report.with_plugins,
            report.to_first_frame_ms,
            report.fonts_theme_ms,
            report.init_ms,
            report.first_frame_ms,
            report.units,
            report.cables,
            report.voices,
            report.vertices,
            report.rss_mib,
            report.rss_kib,
            report.priv_dirty_mib,
            report.hwm_mib,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_bench_parse_args_handles_frames_scenes_and_json() {
        let args: Vec<String> = ["120", "rack-front", "big-back", "--json"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let opts = parse_ui_bench_args(&args).unwrap();
        assert_eq!(opts.frames, 120);
        assert_eq!(opts.scenes, vec!["rack-front", "big-back"]);
        assert!(opts.json);

        assert!(parse_ui_bench_args(&["0".into()]).is_err());
        assert!(parse_ui_bench_args(&["no-such-scene".into()]).is_err());
    }

    #[test]
    fn session_load_reference_session_is_valid_and_plugin_free() {
        let s = reference_session().unwrap();
        s.validate().unwrap();
        assert_eq!(s.rig_names.len(), 4);
        assert_eq!(s.setlist.songs.len(), 10);
        assert!(
            !s.graph.nodes.iter().any(|n| matches!(n.kind, NodeKind::Clap { .. } | NodeKind::Lv2 { .. })),
            "CI reference session must be plugin-free"
        );
        assert!(parse_session_load_args(&["--builtin".into(), "--rounds".into(), "3".into(), "--json".into()]).is_ok());
        assert!(parse_session_load_args(&["--builtin".into(), "foo.ksalat".into()]).is_err());
    }

    #[test]
    fn startup_bench_measures_headless_default_rack_without_plugins() {
        let r = measure_startup(false).unwrap();
        assert!(!r.with_plugins);
        assert_eq!(r.units, 6);
        assert_eq!(r.voices, 0);
        assert!(r.vertices > 0);
        assert!(r.to_first_frame_ms > 0.0);
        assert!(r.rss_kib > 0);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["bench"], "startup-bench");
        assert!(v["to_first_frame_ms"].is_f64());
        assert!(v["rss_mib"].is_f64());
    }
}
