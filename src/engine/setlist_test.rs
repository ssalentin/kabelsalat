//! `kabelsalat setlist-test` (#204): a song switch with real Surge XT voices.
//! Two rigs share one Keys node; `SetRigZones` takes rig A out of the song
//! and puts rig B in, while a chord is held on A. Every level is read from
//! one render (rule 150): each rig's synth → master cable carries the
//! synth's post-fader block peak, and the master output is kept whole.

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, ClapId, Command, CommandBus, Graph, NodeKind, PortKind, PortRef, RigZone};
use crate::host::ClapVoice;
use crate::matrix::SourceId;

const SR: f64 = 48000.0;
const BLOCK: usize = 256;
/// Cables whose levels the test reads: each synth into its rig's master,
/// and rig A's metronome into it.
const EDGE_A: u32 = 2;
const EDGE_B: u32 = 4;
const EDGE_CLICK: u32 = 6;
const CHORD: [u8; 3] = [60, 64, 67];

const OFF: RigZone = RigZone { live: false, ..RigZone::OPEN };

fn zones(a: RigZone, b: RigZone) -> Command {
    let mut zones = [RigZone::OPEN; crate::control::MAX_RIGS];
    zones[0] = a;
    zones[1] = b;
    Command::SetRigZones { zones }
}

fn on(note: u8) -> Command {
    Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys, channel: 0 }
}

fn off(note: u8) -> Command {
    Command::NoteOff { note, source: SourceId::Keys, channel: 0 }
}

/// One block's levels.
#[derive(Clone, Copy, Default)]
struct Block {
    a: f32,
    b: f32,
    click: f32,
    /// Master output peak and largest sample-to-sample step (both channels).
    master: f32,
    step: f32,
}

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    buf: Vec<f32>,
    /// The master's last frame, so a step across a block boundary counts.
    last: [f32; 2],
}

impl Rack {
    fn send(&mut self, cmd: Command) {
        assert!(self.bus.send(cmd), "command bus full");
    }

    fn render(&mut self, blocks: usize) -> Vec<Block> {
        let levels = self.engine.edge_levels_handle();
        (0..blocks)
            .map(|_| {
                self.engine.render(&mut self.buf, 2);
                let mut b = Block { a: levels.load(EDGE_A), b: levels.load(EDGE_B), click: levels.load(EDGE_CLICK), ..Block::default() };
                for f in self.buf.chunks_exact(2) {
                    for ch in 0..2 {
                        b.master = b.master.max(f[ch].abs());
                        b.step = b.step.max((f[ch] - self.last[ch]).abs());
                        self.last[ch] = f[ch];
                    }
                }
                b
            })
            .collect()
    }
}

fn max_of(blocks: &[Block], f: impl Fn(&Block) -> f32) -> f32 {
    blocks.iter().map(f).fold(0.0, f32::max)
}

fn min_of(blocks: &[Block], f: impl Fn(&Block) -> f32) -> f32 {
    blocks.iter().map(f).fold(f32::INFINITY, f32::min)
}

fn blocks_in(secs: f64) -> usize {
    (secs * SR / BLOCK as f64).ceil() as usize
}

pub fn run() -> anyhow::Result<()> {
    let (va, vb) = match (ClapVoice::new(SR), ClapVoice::new(SR)) {
        (Ok((va, _)), Ok((vb, _))) => (va, vb),
        (Err(e), _) | (_, Err(e)) => {
            println!("setlist-test: SKIP — Surge XT is not installed ({e})");
            return Ok(());
        }
    };
    let mut g = Graph::default();
    g.reserve(16, 16);
    let surge = || NodeKind::Clap { plugin_id: ClapId::new("Surge XT") };
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let sa = g.add_node(surge(), name_bytes("SURGE A"));
    let ma = g.add_node(NodeKind::MasterOut, name_bytes("MASTER A"));
    let sb = g.add_node(surge(), name_bytes("SURGE B"));
    let mb = g.add_node(NodeKind::MasterOut, name_bytes("MASTER B"));
    let click = g.add_node(NodeKind::Dsp(crate::control::DspKind::Metronome), name_bytes("CLICK"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: sa, port: 0 }, PortKind::Midi);
    g.connect_with_id(EDGE_A, PortRef { node: sa, port: 0 }, PortRef { node: ma, port: 0 }, PortKind::Audio);
    g.connect_with_id(3, PortRef { node: keys, port: 0 }, PortRef { node: sb, port: 0 }, PortKind::Midi);
    g.connect_with_id(EDGE_B, PortRef { node: sb, port: 0 }, PortRef { node: mb, port: 0 }, PortKind::Audio);
    g.connect_with_id(EDGE_CLICK, PortRef { node: click, port: 0 }, PortRef { node: ma, port: 0 }, PortKind::Audio);
    for n in &mut g.nodes {
        if n.id == sb || n.id == mb {
            n.rig = 1;
        }
    }
    let (bus, drain) = command_bus();
    let engine = GraphEngine::new(g, drain, SR, vec![(sa, va), (sb, vb)]);
    let mut rack = Rack { engine, bus, buf: vec![0.0; BLOCK * 2], last: [0.0; 2] };
    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures.push(what);
        }
    };

    // Song 1: only rig A plays. Hold a chord on it.
    rack.send(zones(RigZone::OPEN, OFF));
    rack.render(4);
    for n in CHORD {
        rack.send(on(n));
    }
    let song1 = rack.render(blocks_in(1.0));
    let before = &song1[song1.len() - blocks_in(0.2)..];
    let a_before = min_of(before, |b| b.a);
    let b_before = max_of(&song1, |b| b.b);
    let step_before = max_of(before, |b| b.step);
    println!("song 1 (rig A live, chord held): rig A ≥ {a_before:.4}, rig B ≤ {b_before:.6}, largest master step {step_before:.4}");
    check(a_before > 1e-3, format!("the chord sounds on rig A (block peak ≥ {a_before:.4})"));
    check(b_before < 1e-5, format!("rig B, not in song 1, stays silent ({b_before:.6})"));

    // Song 2: rig A out, rig B in. The chord stays held.
    rack.send(zones(OFF, RigZone::OPEN));
    let switch = rack.render(blocks_in(0.02));
    let song2 = rack.render(blocks_in(0.5));
    let step_switch = max_of(&switch, |b| b.step);
    let quiet_switch = min_of(&switch, |b| b.master);
    let a_held = min_of(&song2, |b| b.a);
    println!(
        "switch block ±20 ms: largest master step {step_switch:.4} (before: {step_before:.4}), quietest block {quiet_switch:.4}; rig A while held ≥ {a_held:.4}"
    );
    check(step_switch <= step_before, format!("no sample jump at the switch larger than before it ({step_switch:.4} ≤ {step_before:.4})"));
    check(quiet_switch > 1e-3, format!("no block of silence at the switch (quietest {quiet_switch:.4})"));
    check(a_held > 0.5 * a_before, format!("rig A keeps sounding while the chord is held ({a_held:.4} > half of {a_before:.4})"));

    // A note in song 2: rig B rises from silence.
    rack.send(on(72));
    let note = rack.render(blocks_in(0.3));
    let b_rise = max_of(&note, |b| b.b);
    println!("note 72 in song 2: rig B rises {b_before:.6} → {b_rise:.4}");
    check(b_rise > 1e-3, format!("rig B plays the new note (peak {b_rise:.4})"));

    // Release the chord: rig A decays to silence; rig B keeps its note.
    for n in CHORD {
        rack.send(off(n));
    }
    rack.render(blocks_in(2.0)); // release tail, and the master's (rule 133)
    let tail = rack.render(blocks_in(0.2));
    let a_tail = max_of(&tail, |b| b.a);
    let b_tail = min_of(&tail, |b| b.b);
    println!("chord released: rig A {a_tail:.6} after 2 s; rig B still holds 72 ≥ {b_tail:.4}");
    check(a_tail < 1e-4, format!("rig A decays to silence ({a_tail:.6})"));
    check(b_tail > 1e-3, format!("rig B's own note is untouched ({b_tail:.4})"));
    // Rig A, silent and out of the song, takes no new note. A silence check,
    // never a level comparison of free-running voices (rules 150, 182).
    rack.send(on(76));
    let late = rack.render(blocks_in(0.3));
    let a_late = max_of(&late, |b| b.a);
    println!("note 76 while rig A is out and silent: rig A {a_late:.6}");
    check(a_late < 1e-4, format!("rig A takes no new note ({a_late:.6})"));
    rack.send(off(76));
    rack.send(off(72));
    rack.render(blocks_in(2.0));

    // A generator leaves with its rig, an instrument note held there does not.
    rack.send(zones(RigZone::OPEN, OFF));
    rack.send(Command::SetNodePlaying { node: click, playing: true });
    rack.send(on(48));
    let live = rack.render(blocks_in(1.5));
    rack.send(zones(OFF, RigZone::OPEN));
    let out = rack.render(blocks_in(1.5));
    let (click_live, click_out) = (max_of(&live, |b| b.click), max_of(&out, |b| b.click));
    let a_out = min_of(&out, |b| b.a);
    println!("metronome in rig A: {click_live:.4} live, {click_out:.6} out of the song; rig A's held note ≥ {a_out:.4} out of the song");
    check(click_live > 1e-2, format!("the metronome clicks while rig A is live ({click_live:.4})"));
    check(click_out == 0.0, format!("the metronome is silent while rig A is out ({click_out:.6})"));
    check(a_out > 1e-3, format!("the note held on rig A keeps sounding ({a_out:.4})"));
    rack.send(off(48));
    rack.render(4);

    anyhow::ensure!(failures.is_empty(), "setlist-test: {} check(s) failed: {}", failures.len(), failures.join("; "));
    println!("setlist-test: OK — a song switch keeps held notes, starts new ones only on the live rig, and has no gap");
    Ok(())
}
