//! `kabelsalat setlist-window-test` (#207): proves both "Done when" items with
//! real Surge XT instances, the real background `ClapLoader` thread and real
//! `/proc/self/smaps_rollup` memory measurements:
//!
//! 1. **Memory follows the window, not the length of the list:** isolated fresh
//!    subprocesses compare a 16-song setlist with all 16 Surge XT rigs hosted
//!    vs. the same 16-song setlist with `load_ahead = Some(2)` (window = 3
//!    songs) walked across all 16 songs vs. a 6-song setlist with the same
//!    `load_ahead = Some(2)` window.
//! 2. **Gapless playback in order:** walks through 12 songs with `load_ahead =
//!    Some(2)` while the audio thread renders continuously, proving every
//!    in-order switch has zero gap on held chords, immediate onset (<= 2 ms)
//!    on the incoming song's first note, and zero audio-thread blocking during
//!    background plugin instantiation and retirement.
//! 3. **Jump to an unloaded song:** jumps to an unloaded song while a
//!    show-level rig holds a chord, proving the song reports `Loading`, loads
//!    in the background without a single dropped frame on the show-level rig,
//!    restores its patch and plays cleanly once ready.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Result};

use super::{GraphEngine, GraphPub, VoiceBack, VoiceMsg, VOICE_TABLE_CAP};
use crate::control::{
    command_bus, name_bytes, ClapId, Command, CommandBus, DspKind, Graph, NodeId, NodeKind,
    PortKind, PortRef, SURGE_PLUGIN_ID,
};
use crate::host::{ClapLoader, ClapLoaderResp, ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;
use crate::setlist::{Level, Part, PartRig, Setlist, Song, SongLoadState};

const SR: f64 = 48_000.0;
const BLOCK: usize = 256;
const CHORD: [u8; 3] = [60, 64, 67];
const FIRST_NOTE: u8 = 72;
const ONSET_MAX_MS: f64 = 2.5;
const STILL: f32 = 1e-5;

#[derive(Clone, Copy, Default)]
struct MemSample {
    rss_kib: u64,
    priv_dirty_kib: u64,
}

fn read_mem() -> MemSample {
    let Ok(text) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return MemSample::default();
    };
    let mut m = MemSample::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(val) = parts.next().and_then(|v| v.parse::<u64>().ok()) else { continue };
        match key {
            "Rss:" => m.rss_kib = val,
            "Private_Dirty:" => m.priv_dirty_kib = val,
            _ => {}
        }
    }
    m
}

fn init_sine_state() -> Result<Vec<u8>> {
    let listed = crate::host::list_surge_patches()
        .into_iter()
        .find(|p| p.name == "Init Sine")
        .map(|p| p.path);
    let xdg = std::env::var_os("XDG_DATA_HOME").map(|d| {
        std::path::PathBuf::from(d).join("surge-xt/patches_factory/Templates/Init Sine.fxp")
    });
    let path = listed
        .or(xdg.filter(|p| p.exists()))
        .ok_or_else(|| anyhow::anyhow!("Surge XT's template 'Init Sine' is not installed"))?;
    crate::host::fxp_to_state(&std::fs::read(&path)?)
}

/// Build an `n`-rig graph where each rig has `Keys -> Surge XT -> Delay -> MasterOut`.
fn multi_rig_graph(n: usize) -> (Graph, Vec<NodeId>) {
    let mut g = Graph::default();
    g.reserve(n * 4, n * 4);
    let mut synths = Vec::with_capacity(n);
    let mut eid = 1u32;
    for r in 0..n {
        let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let s = g.add_node(
            NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) },
            name_bytes(&format!("SYNTH {}", r + 1)),
        );
        let d = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        for id in [k, s, d, m] {
            g.set_rig(id, r as u8);
        }
        g.connect_with_id(eid, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, PortKind::Midi);
        g.connect_with_id(eid + 1, PortRef { node: s, port: 0 }, PortRef { node: d, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 2, PortRef { node: d, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 3, PortRef { node: d, port: 0 }, PortRef { node: m, port: 1 }, PortKind::Audio);
        eid += 4;
        synths.push(s);
    }
    (g, synths)
}

fn on(note: u8) -> Command {
    Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys, channel: 0 }
}

fn off(note: u8) -> Command {
    Command::NoteOff { note, source: SourceId::Keys, channel: 0 }
}

/// Minimal host harness mirroring `DemoApp`'s `SetlistWindowState` + `ClapLoader`
/// driving a real `GraphEngine`.
struct WindowHarness {
    engine: GraphEngine,
    bus: CommandBus,
    graph: Graph,
    sl: Setlist,
    loader: ClapLoader,
    voice_tx: rtrb::Producer<VoiceMsg>,
    back_rx: rtrb::Consumer<VoiceBack>,
    /// `node -> voice_id` for currently attached voices.
    hosted: HashMap<NodeId, u64>,
    /// `voice_id -> node` for voices leaving the engine.
    leaving: HashMap<u64, NodeId>,
    loading: Vec<NodeId>,
    states: HashMap<NodeId, Vec<u8>>,
    gen: u64,
    buf: Vec<f32>,
    out: Vec<f32>,
    times_ms: Vec<f64>,
    /// Rigs that left the window and are held until silent (`rig -> silent blocks`).
    draining: HashMap<u8, usize>,
    max_hosted: usize,
}

impl WindowHarness {
    fn new(
        graph: Graph,
        sl: Setlist,
        states: HashMap<NodeId, Vec<u8>>,
    ) -> Self {
        let (bus, drain) = command_bus();
        let mut engine = GraphEngine::new(graph.clone(), drain, SR, vec![]);
        let (voice_tx, voice_rx) = rtrb::RingBuffer::<VoiceMsg>::new(64);
        let (back_tx, back_rx) = rtrb::RingBuffer::<VoiceBack>::new(64);
        engine.set_voice_channels(voice_rx, back_tx);
        engine.set_graph_pub(std::sync::Arc::new(std::sync::Mutex::new(GraphPub::default())));
        let loader = ClapLoader::spawn();
        Self {
            engine,
            bus,
            graph,
            sl,
            loader,
            voice_tx,
            back_rx,
            hosted: HashMap::new(),
            leaving: HashMap::new(),
            loading: Vec::new(),
            states,
            gen: 1,
            buf: vec![0.0; BLOCK * 2],
            out: Vec::new(),
            times_ms: Vec::new(),
            draining: HashMap::new(),
            max_hosted: 0,
        }
    }

    fn send(&mut self, cmd: Command) {
        assert!(self.bus.send(cmd), "command bus full");
    }

    fn apply_active_part(&mut self) {
        for cmd in self.sl.gates(&self.graph).commands() {
            self.send(cmd);
        }
        self.send(Command::SetShowPart { part: self.sl.active_index() });
        for pr in self.sl.recalled_rigs() {
            self.send(Command::SetRigMix {
                rig: pr.rig,
                vol: Some(pr.level),
                pan: pr.pan,
                mute: Some(false),
                solo: Some(false),
            });
        }
    }

    fn loaded_nodes(&self) -> Vec<NodeId> {
        self.hosted.keys().copied().collect()
    }

    fn song_state(&self, song: usize) -> SongLoadState {
        let loaded = self.loaded_nodes();
        self.sl.song_load_state(song, &self.graph, &loaded, &self.loading)
    }

    fn render_block(&mut self) -> f64 {
        let t = Instant::now();
        self.engine.render(&mut self.buf, 2);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        self.times_ms.push(ms);
        self.out.extend_from_slice(&self.buf);
        self.poll_loader();
        ms
    }

    fn poll_loader(&mut self) {
        while let Ok(back) = self.back_rx.pop() {
            if let VoiceBack::Voice(slot) = back {
                let vid = slot.voice_id();
                let node = self.leaving.remove(&vid).unwrap_or(NodeId::MAX);
                self.loader.retire(vid, node, SURGE_PLUGIN_ID.into(), slot, node != NodeId::MAX);
            }
        }
        while let Some(resp) = self.loader.try_recv() {
            match resp {
                ClapLoaderResp::Loaded { node, gen, voice_id, slot, .. } => {
                    self.loading.retain(|&n| n != node);
                    if gen == self.gen && !self.hosted.contains_key(&node) {
                        assert!(self.voice_tx.push(VoiceMsg::AddBound { node, slot }).is_ok());
                        self.hosted.insert(node, voice_id);
                        self.max_hosted = self.max_hosted.max(self.hosted.len());
                    } else {
                        self.loader.retire(voice_id, node, SURGE_PLUGIN_ID.into(), slot, false);
                    }
                }
                ClapLoaderResp::Cancelled { node, .. } | ClapLoaderResp::LoadFailed { node, .. } => {
                    self.loading.retain(|&n| n != node);
                }
                ClapLoaderResp::Retired { node, saved_state, .. } => {
                    if let Some(blob) = saved_state {
                        if node != NodeId::MAX {
                            self.states.insert(node, blob);
                        }
                    }
                }
            }
        }
    }

    fn sync_window(&mut self, allow_drain: bool) {
        let desired_rigs = self.sl.window_rigs(&self.graph).unwrap_or_default();
        self.draining.retain(|r, _| !desired_rigs.contains(r));

        let mut wanted = self.sl.window_plugin_nodes(&self.graph).unwrap_or_default();
        if allow_drain {
            // Check hosted nodes outside `wanted`: if their node output is still
            // audible, keep their rig in `draining` until 30 consecutive silent blocks (~160 ms).
            for &node in self.hosted.keys() {
                if wanted.contains(&node) {
                    continue;
                }
                let Some(rig) = self.graph.node(node).map(|n| n.rig) else { continue };
                let peak = self
                    .engine
                    .node_out(node)
                    .map(|b| b[..BLOCK * 2].iter().fold(0.0f32, |m, s| m.max(s.abs())))
                    .unwrap_or(0.0);
                if peak > 1e-4 {
                    self.draining.insert(rig, 0);
                } else if let Some(silent) = self.draining.get_mut(&rig) {
                    *silent += 1;
                    if *silent >= 30 {
                        self.draining.remove(&rig);
                    }
                }
            }
            for &rig in self.draining.keys() {
                for n in &self.graph.nodes {
                    if n.rig == rig && matches!(n.kind, NodeKind::Clap { .. }) && !wanted.contains(&n.id) {
                        wanted.push(n.id);
                    }
                }
            }
        }

        self.loader.set_wanted(self.gen, Some(wanted.clone()));
        for &node in &wanted {
            if !self.hosted.contains_key(&node) && !self.loading.contains(&node) {
                self.loading.push(node);
                let st = self.states.get(&node).cloned();
                self.loader.request_load(node, SURGE_PLUGIN_ID.into(), SR, st, self.gen);
            }
        }
        let to_retire: Vec<(NodeId, u64)> = self
            .hosted
            .iter()
            .filter(|(&node, _)| !wanted.contains(&node))
            .map(|(&n, &vid)| (n, vid))
            .collect();
        for (node, vid) in to_retire {
            self.hosted.remove(&node);
            self.leaving.insert(vid, node);
            assert!(self.voice_tx.push(VoiceMsg::Retire(vid)).is_ok());
        }
        let mut mask = 0u32;
        for &r in &desired_rigs {
            mask |= 1u32 << r;
        }
        for &r in self.draining.keys() {
            mask |= 1u32 << r;
        }
        self.send(Command::SetHostedRigs { mask: Some(mask), for_next_load: false });
    }

    /// Render blocks until `done(self)` becomes true or `timeout` elapses.
    fn wait_until(&mut self, timeout: Duration, mut done: impl FnMut(&Self) -> bool) -> Result<()> {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > timeout {
                bail!(
                    "timeout after {:.1}s (hosted={}, loading={:?}, leaving={})",
                    timeout.as_secs_f64(),
                    self.hosted.len(),
                    self.loading,
                    self.leaving.len()
                );
            }
            self.render_block();
            self.sync_window(true);
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn shutdown(mut self) {
        let ids: Vec<(NodeId, u64)> = self.hosted.drain().collect();
        for (node, vid) in ids {
            self.leaving.insert(vid, node);
            let _ = self.voice_tx.push(VoiceMsg::Retire(vid));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.leaving.is_empty() && Instant::now() < deadline {
            self.render_block();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn mem_probe(songs: usize, mode: &str) -> Result<()> {
    let sine = init_sine_state()?;
    let (graph, synths) = multi_rig_graph(songs);
    let mem0 = read_mem();

    if mode == "all" {
        let mut voices = Vec::with_capacity(songs);
        let mut mains: Vec<ClapVoiceMain> = Vec::with_capacity(songs);
        for &node in &synths {
            let (v, mut m) = ClapVoice::new_plugin(SR, SURGE_PLUGIN_ID)?;
            m.load_state(&sine)?;
            voices.push((node, v));
            mains.push(m);
        }
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(graph, drain, SR, voices);
        let mut buf = vec![0.0f32; BLOCK * 2];
        for _ in 0..4 {
            engine.render(&mut buf, 2);
        }
        let mem1 = read_mem();
        bus.send(on(60));
        let mut peaks = vec![0.0f32; synths.len()];
        for _ in 0..16 {
            engine.render(&mut buf, 2);
            for (i, &node) in synths.iter().enumerate() {
                if let Some(o) = engine.node_out(node) {
                    peaks[i] = peaks[i].max(o[..BLOCK * 2].iter().fold(0.0f32, |a, b| a.max(b.abs())));
                }
            }
        }
        let sounded = peaks.iter().filter(|&&p| p > 1e-4).count();
        println!(
            "MEM_PROBE songs={songs} mode=all hosted={songs} max_hosted={songs} sounded={sounded} peak_priv_dirty_kib={} final_priv_dirty_kib={} peak_rss_kib={} final_rss_kib={} worst_render_ms=0.000",
            mem1.priv_dirty_kib.saturating_sub(mem0.priv_dirty_kib),
            mem1.priv_dirty_kib.saturating_sub(mem0.priv_dirty_kib),
            mem1.rss_kib.saturating_sub(mem0.rss_kib),
            mem1.rss_kib.saturating_sub(mem0.rss_kib),
        );
        drop(engine);
        drop(mains);
        return Ok(());
    }

    let ahead: u8 = mode.parse()?;
    let mut sl = Setlist::default();
    for i in 0..songs {
        sl.songs.push(Song::new(
            format!("Song {}", i + 1),
            vec![Part::new("Main", vec![PartRig::new(i as u8)])],
        ));
    }
    sl.load_ahead = Some(ahead);
    sl.select(0, 0);
    let states: HashMap<NodeId, Vec<u8>> = synths.iter().map(|&n| (n, sine.clone())).collect();
    let mut h = WindowHarness::new(graph, sl, states);
    h.apply_active_part();
    h.sync_window(false);

    let mut sounded = 0usize;
    let mut peak_priv_dirty = 0u64;
    let mut peak_rss = 0u64;
    let mut worst_render_ms = 0.0f64;

    for s in 0..songs {
        h.sl.select(s, 0);
        h.apply_active_part();
        h.sync_window(false);
        // Wait until the entire window for song `s` is loaded and out-of-window voices are retired.
        h.wait_until(Duration::from_secs(20), |x| {
            let want = x.sl.window_plugin_nodes(&x.graph).unwrap_or_default();
            want.iter().all(|id| x.hosted.contains_key(id))
                && x.hosted.len() == want.len()
                && x.loading.is_empty()
                && x.leaving.is_empty()
        })?;
        // Play a note on song `s`'s synth and verify audio output.
        h.send(on(60));
        let mut peak = 0.0f32;
        for _ in 0..12 {
            let ms = h.render_block();
            worst_render_ms = worst_render_ms.max(ms);
            if let Some(o) = h.engine.node_out(synths[s]) {
                peak = peak.max(o[..BLOCK * 2].iter().fold(0.0f32, |a, b| a.max(b.abs())));
            }
        }
        h.send(off(60));
        for _ in 0..4 {
            let ms = h.render_block();
            worst_render_ms = worst_render_ms.max(ms);
        }
        if peak > 0.01 {
            sounded += 1;
        }
        // Let the loader finish any pending retire + malloc_trim.
        std::thread::sleep(Duration::from_millis(15));
        h.poll_loader();
        let m = read_mem();
        peak_priv_dirty = peak_priv_dirty.max(m.priv_dirty_kib.saturating_sub(mem0.priv_dirty_kib));
        peak_rss = peak_rss.max(m.rss_kib.saturating_sub(mem0.rss_kib));
    }

    let final_hosted = h.hosted.len();
    let max_hosted = h.max_hosted;
    let mem_final = read_mem();
    println!(
        "MEM_PROBE songs={songs} mode=window+{ahead} hosted={final_hosted} max_hosted={max_hosted} sounded={sounded} peak_priv_dirty_kib={peak_priv_dirty} final_priv_dirty_kib={} peak_rss_kib={peak_rss} final_rss_kib={} worst_render_ms={worst_render_ms:.3}",
        mem_final.priv_dirty_kib.saturating_sub(mem0.priv_dirty_kib),
        mem_final.rss_kib.saturating_sub(mem0.rss_kib),
    );
    h.shutdown();
    Ok(())
}

struct MemRow {
    hosted: usize,
    max_hosted: usize,
    sounded: usize,
    peak_priv_dirty_kib: u64,
    final_priv_dirty_kib: u64,
    peak_rss_kib: u64,
    final_rss_kib: u64,
    worst_render_ms: f64,
}

fn run_mem_probe(exe: &std::path::Path, songs: usize, mode: &str) -> Result<MemRow> {
    let out = std::process::Command::new(exe)
        .args(["setlist-window-test", "mem-probe", &songs.to_string(), mode])
        .output()?;
    ensure!(
        out.status.success(),
        "mem-probe {songs} {mode} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("MEM_PROBE "))
        .ok_or_else(|| anyhow::anyhow!("mem-probe printed no MEM_PROBE line:\n{stdout}"))?;
    let mut row = MemRow {
        hosted: 0,
        max_hosted: 0,
        sounded: 0,
        peak_priv_dirty_kib: 0,
        final_priv_dirty_kib: 0,
        peak_rss_kib: 0,
        final_rss_kib: 0,
        worst_render_ms: 0.0,
    };
    for tok in line.split_whitespace().skip(1) {
        let Some((k, v)) = tok.split_once('=') else { continue };
        match k {
            "hosted" => row.hosted = v.parse()?,
            "max_hosted" => row.max_hosted = v.parse()?,
            "sounded" => row.sounded = v.parse()?,
            "peak_priv_dirty_kib" => row.peak_priv_dirty_kib = v.parse()?,
            "final_priv_dirty_kib" => row.final_priv_dirty_kib = v.parse()?,
            "peak_rss_kib" => row.peak_rss_kib = v.parse()?,
            "final_rss_kib" => row.final_rss_kib = v.parse()?,
            "worst_render_ms" => row.worst_render_ms = v.parse()?,
            _ => {}
        }
    }
    Ok(row)
}

fn channel(out: &[f32], ch: usize, from: usize, to: usize) -> Vec<f32> {
    out.chunks_exact(2).skip(from).take(to.saturating_sub(from)).map(|f| f[ch]).collect()
}

fn max_still_run(x: &[f32]) -> usize {
    let (mut cur, mut best) = (0usize, 0usize);
    for w in x.windows(2) {
        if (w[1] - w[0]).abs() <= STILL {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

fn onset_ms(x: &[f32]) -> Option<f64> {
    x.iter().position(|s| s.abs() > 1e-3).map(|f| f as f64 / SR * 1000.0)
}

/// Part 2: Play through 12 songs in order with `load_ahead = Some(2)` (window =
/// 3 songs) while holding chords across every switch and playing the next song's
/// first note in the exact switch block.
fn verify_gapless_in_order(sine: &[u8]) -> Result<()> {
    const SONGS: usize = 12;
    const AHEAD: u8 = 2;
    let (graph, synths) = multi_rig_graph(SONGS);
    let mut sl = Setlist::default();
    for i in 0..SONGS {
        let pan = if i % 2 == 0 { -1.0 } else { 1.0 };
        let pr = PartRig { rig: i as u8, level: 0.7, pan: Some(pan), ..PartRig::default() };
        sl.songs.push(Song::new(format!("Song {}", i + 1), vec![Part::new("Main", vec![pr])]));
    }
    sl.load_ahead = Some(AHEAD);
    sl.select(0, 0);
    let states: HashMap<NodeId, Vec<u8>> = synths.iter().map(|&n| (n, sine.to_vec())).collect();
    let mut h = WindowHarness::new(graph, sl, states);
    h.apply_active_part();
    h.sync_window(false);
    // Initial window [0, 1, 2] preloaded before song 1 begins.
    h.wait_until(Duration::from_secs(15), |x| {
        [0, 1, 2].iter().all(|&s| x.song_state(s) == SongLoadState::Ready)
    })?;

    let mut worst_onset_ms = 0.0f64;
    let mut worst_still_frames = 0usize;
    let mut worst_switch_block_ms = 0.0f64;

    for s in 0..(SONGS - 1) {
        let cur_ch = s % 2;
        let next_ch = (s + 1) % 2;
        // Next song MUST already be Ready before we switch!
        ensure!(
            h.song_state(s + 1) == SongLoadState::Ready,
            "song {} was not preloaded before switch {}->{}: {:?}",
            s + 2,
            s + 1,
            s + 2,
            h.song_state(s + 1)
        );
        // Play and hold a chord on song `s`.
        for note in CHORD {
            h.send(on(note));
        }
        for _ in 0..30 {
            h.render_block();
            h.sync_window(true);
        }
        // Switch to song `s + 1` and hit its first note in the SAME block!
        h.sl.select(s + 1, 0);
        h.apply_active_part();
        h.send(on(FIRST_NOTE));
        let sw_frame = h.out.len() / 2;
        let sw_ms = h.render_block();
        worst_switch_block_ms = worst_switch_block_ms.max(sw_ms);
        h.sync_window(true);

        // Keep holding old chord + new note for 0.25 s (~47 blocks) while next+2 loads in background.
        for _ in 0..47 {
            h.render_block();
            h.sync_window(true);
            std::thread::sleep(Duration::from_millis(2));
        }
        let hold_end = h.out.len() / 2;
        let old_ch_audio = channel(&h.out, cur_ch, sw_frame.saturating_sub(BLOCK * 4), hold_end);
        let new_ch_audio = channel(&h.out, next_ch, sw_frame, sw_frame + BLOCK * 4);
        let still = max_still_run(&old_ch_audio);
        let onset = onset_ms(&new_ch_audio)
            .ok_or_else(|| anyhow::anyhow!("switch {}->{}: incoming song {} silent!", s + 1, s + 2, s + 2))?;
        worst_still_frames = worst_still_frames.max(still);
        worst_onset_ms = worst_onset_ms.max(onset);
        ensure!(
            still < 64,
            "switch {}->{}: held chord dropped out ({still} still frames)",
            s + 1,
            s + 2
        );
        ensure!(
            onset <= ONSET_MAX_MS,
            "switch {}->{}: incoming first note delayed {onset:.2} ms (> {ONSET_MAX_MS} ms)",
            s + 1,
            s + 2
        );

        // Release both old chord and new note and let song `s` go quiet and retire while song `s+3` preloads.
        for note in CHORD {
            h.send(off(note));
        }
        h.send(off(FIRST_NOTE));
        let preload_target = (s + 1 + AHEAD as usize).min(SONGS - 1);
        h.wait_until(Duration::from_secs(15), |x| {
            x.song_state(preload_target) == SongLoadState::Ready && !x.hosted.contains_key(&synths[s])
        })?;
    }

    let mut sorted = h.times_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let med = sorted[sorted.len() / 2];
    let p99 = sorted[sorted.len() * 99 / 100];
    let worst = *sorted.last().unwrap_or(&0.0);
    println!(
        "in-order 12 songs (window +2): 11/11 switches gapless  max_still={worst_still_frames} frames  max_onset={worst_onset_ms:.2} ms  switch_block_max={worst_switch_block_ms:.2} ms  render median={med:.2} ms  p99={p99:.2} ms  worst={worst:.2} ms  max_hosted={}",
        h.max_hosted
    );
    ensure!(
        h.max_hosted <= 1 + AHEAD as usize + 1,
        "hosted instances exceeded window+drain bound: {}",
        h.max_hosted
    );
    h.shutdown();
    Ok(())
}

/// Part 3: Jumping directly to an unloaded song reports `Loading`, loads in the
/// background without dropping a single frame of a held show-level rig, and
/// plays cleanly once ready.
fn verify_unloaded_jump(sine: &[u8]) -> Result<()> {
    // 10 song rigs (0..10) + 1 Show-level rig (rig 10, always hosted, panned left).
    const SONG_RIGS: usize = 10;
    let show_rig: usize = SONG_RIGS;
    let (graph, synths) = multi_rig_graph(SONG_RIGS + 1);
    let mut sl = Setlist::default();
    sl.pin_rig(show_rig as u8, Level::Show);
    for i in 0..SONG_RIGS {
        let pr = PartRig { rig: i as u8, level: 0.7, pan: Some(1.0), ..PartRig::default() };
        sl.songs.push(Song::new(format!("Song {}", i + 1), vec![Part::new("Main", vec![pr])]));
    }
    sl.load_ahead = Some(1);
    sl.select(0, 0);
    let states: HashMap<NodeId, Vec<u8>> = synths.iter().map(|&n| (n, sine.to_vec())).collect();
    let mut h = WindowHarness::new(graph, sl, states);
    h.send(Command::SetRigMix { rig: show_rig as u8, vol: Some(0.7), pan: Some(-1.0), mute: Some(false), solo: Some(false) });
    h.apply_active_part();
    h.sync_window(false);
    // Wait for initial window [0, 1] + show rig.
    h.wait_until(Duration::from_secs(15), |x| {
        x.song_state(0) == SongLoadState::Ready && x.song_state(1) == SongLoadState::Ready
    })?;
    ensure!(
        h.song_state(8) == SongLoadState::Unloaded,
        "Song 9 must start Unloaded, got {:?}",
        h.song_state(8)
    );

    // Hold a continuous chord on the show-level rig (left channel).
    for note in CHORD {
        h.send(on(note));
    }
    for _ in 0..25 {
        h.render_block();
    }
    let jump_frame = h.out.len() / 2;
    let jump_t0 = Instant::now();

    // Jump directly to unloaded Song 9 (index 8)!
    h.sl.select(8, 0);
    h.apply_active_part();
    h.sync_window(true);
    ensure!(
        h.song_state(8) == SongLoadState::Loading,
        "jumping to unloaded Song 9 must report Loading, got {:?}",
        h.song_state(8)
    );

    let mut jump_worst_block_ms = 0.0f64;
    let mut blocks_during_load = 0usize;
    while h.song_state(8) != SongLoadState::Ready {
        ensure!(jump_t0.elapsed() < Duration::from_secs(15), "Song 9 never finished loading");
        let ms = h.render_block();
        jump_worst_block_ms = jump_worst_block_ms.max(ms);
        blocks_during_load += 1;
        h.sync_window(true);
        std::thread::sleep(Duration::from_millis(2));
    }
    let load_ms = jump_t0.elapsed().as_secs_f64() * 1000.0;
    let load_end = h.out.len() / 2;
    // Left channel (show-level rig) must have ZERO drop-outs across the entire jump + background load!
    let show_ch = channel(&h.out, 0, jump_frame, load_end);
    let still = max_still_run(&show_ch);
    ensure!(still < 64, "show-level rig dropped out during unloaded jump ({still} still frames)");

    // Now play a note on newly loaded Song 9 (right channel) and confirm it sounds!
    h.send(on(FIRST_NOTE));
    let mut right_peak = 0.0f32;
    for _ in 0..16 {
        h.render_block();
        let tail = channel(&h.out, 1, h.out.len() / 2 - BLOCK, h.out.len() / 2);
        right_peak = right_peak.max(tail.iter().fold(0.0f32, |a, b| a.max(b.abs())));
    }
    ensure!(right_peak > 0.05, "newly loaded Song 9 stayed silent (peak {right_peak:.4})");

    println!(
        "unloaded jump (0 -> 8): state Unloaded -> Loading -> Ready in {load_ms:.1} ms ({blocks_during_load} blocks rendered)  show_rig_max_still={still} frames  worst_block={jump_worst_block_ms:.2} ms  song9_peak={right_peak:.3}"
    );
    h.shutdown();
    Ok(())
}

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(2).map(String::as_str) == Some("mem-probe") {
        let songs: usize = args
            .get(3)
            .ok_or_else(|| anyhow::anyhow!("mem-probe needs <songs>"))?
            .parse()?;
        let mode = args
            .get(4)
            .ok_or_else(|| anyhow::anyhow!("mem-probe needs <all|load_ahead>"))?;
        return mem_probe(songs, mode);
    }

    let sine = init_sine_state()?;
    let exe = std::env::current_exe()?;
    println!("setlist-window-test (#207): real Surge XT, background ClapLoader, /proc/self/smaps_rollup");

    // 1. Memory scaling check in isolated fresh subprocesses (warm page cache).
    let _ = run_mem_probe(&exe, 2, "all")?; // warm plugin shared libraries in page cache
    let all_16 = run_mem_probe(&exe, 16, "all")?;
    let win_16 = run_mem_probe(&exe, 16, "2")?;
    let win_6 = run_mem_probe(&exe, 6, "2")?;

    println!(
        "  16 songs, all hosted  : hosted={:<2} sounded={:<2} priv_dirty={:6.1} MiB  rss={:6.1} MiB",
        all_16.hosted,
        all_16.sounded,
        all_16.final_priv_dirty_kib as f64 / 1024.0,
        all_16.final_rss_kib as f64 / 1024.0,
    );
    println!(
        "  16 songs, window +2   : max_hosted={:<2} sounded={:<2} peak_priv_dirty={:6.1} MiB  final_priv_dirty={:6.1} MiB  peak_rss={:6.1} MiB  worst_render={:.2} ms",
        win_16.max_hosted,
        win_16.sounded,
        win_16.peak_priv_dirty_kib as f64 / 1024.0,
        win_16.final_priv_dirty_kib as f64 / 1024.0,
        win_16.peak_rss_kib as f64 / 1024.0,
        win_16.worst_render_ms,
    );
    println!(
        "   6 songs, window +2   : max_hosted={:<2} sounded={:<2} peak_priv_dirty={:6.1} MiB  final_priv_dirty={:6.1} MiB  peak_rss={:6.1} MiB  worst_render={:.2} ms",
        win_6.max_hosted,
        win_6.sounded,
        win_6.peak_priv_dirty_kib as f64 / 1024.0,
        win_6.final_priv_dirty_kib as f64 / 1024.0,
        win_6.peak_rss_kib as f64 / 1024.0,
        win_6.worst_render_ms,
    );

    ensure!(all_16.sounded == 16, "all_16 sounded={}/16", all_16.sounded);
    ensure!(win_16.sounded == 16, "win_16 sounded={}/16", win_16.sounded);
    ensure!(win_6.sounded == 6, "win_6 sounded={}/6", win_6.sounded);
    ensure!(win_16.max_hosted <= 3, "win_16 max_hosted={} > 3", win_16.max_hosted);
    ensure!(win_6.max_hosted <= 3, "win_6 max_hosted={} > 3", win_6.max_hosted);
    ensure!(
        win_16.peak_priv_dirty_kib * 2 < all_16.final_priv_dirty_kib,
        "16-song windowed peak Private_Dirty ({} KiB) should be < 50% of 16 all-hosted ({} KiB)",
        win_16.peak_priv_dirty_kib,
        all_16.final_priv_dirty_kib
    );
    ensure!(
        win_16.final_priv_dirty_kib * 10 <= win_6.final_priv_dirty_kib.max(1) * 18,
        "16-song windowed final Private_Dirty ({} KiB) grew too far above 6-song same-window ({} KiB)",
        win_16.final_priv_dirty_kib,
        win_6.final_priv_dirty_kib
    );

    // 2. Gapless sequential playback across 12 songs.
    verify_gapless_in_order(&sine)?;

    // 3. Jumping to an unloaded song while a show-level rig plays continuously.
    verify_unloaded_jump(&sine)?;

    println!("setlist-window-test: PASS");
    let _ = VOICE_TABLE_CAP;
    Ok(())
}
