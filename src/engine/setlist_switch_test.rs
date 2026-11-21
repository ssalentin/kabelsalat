//! `kabelsalat setlist-switch-test` (#274): song and part switches under held
//! notes with real Surge XT voices, measured.
//!
//! Rig A (Surge on Init Sine → Delay → Reverb) plays the song "Strings", rig B
//! (Surge on its init saw) the song "Piano". Rig A is panned hard left and
//! rig B hard right, so the master's left channel is the old sound and its
//! right channel the new one (rule 182); every value is read from one render
//! (rule 150). A switch goes either way the app makes one: what
//! `DemoApp::apply_setlist_part` sends (the setlist's gates and part, the
//! song's tempo, the recalled rig faders and knobs), or a foot switch the
//! engine steps on at once, with the recall following one UI frame later
//! (`follow_show`).
//!
//! Measured: a chord held across a part switch that moves its rig's fader
//! and its Surge LEVEL, and across a song switch (no run of silence, no click, the level it should
//! have), the delay and reverb tail of a note released just before a switch,
//! and the first note of the new song played together with the switch,
//! against the same note without one, at three block sizes, with the render
//! time of every switch block. Nothing loads on a switch: every rig of the
//! setlist is hosted before the first one. The control is a copy of a take
//! with rig A's channel zeroed for one block at every switch, a drop-out:
//! there the held-note and tail checks must fail, or they prove nothing.

use std::time::Instant;

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, ClapId, Command, CommandBus, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef};
use crate::host::{ClapVoice, ClapVoiceMain};
use crate::matrix::SourceId;
use crate::setlist::{Part, PartRig, Setlist, Song};

const SR: f64 = 48000.0;
const CHORD: [u8; 3] = [60, 64, 67];
const FIRST_NOTE: u8 = 72;
const BUDGET_MS: f64 = 50.0;
/// Surge's saw is silent until its first edge, 0 to 1 ms after the note-on
/// wherever the note-on lands in a block: a note that starts later waited
/// for something.
const ONSET_MAX_MS: f64 = 2.0;
/// Rig A hard left, rig B hard right.
const PAN: [f32; 2] = [-1.0, 1.0];
/// Rig A's fader and its Surge LEVEL in the verse and the chorus.
const VERSE: f32 = 0.4;
const CHORUS: f32 = 0.6;
const VERSE_LEVEL: f64 = 0.85;
const CHORUS_LEVEL: f64 = 0.7;
/// The foot switch that steps to the next part (a hardware CC).
const NEXT_CC: u8 = 80;
/// How late the app recalls tempo, faders and knobs after the engine
/// stepped on a foot switch: one UI frame at 60 Hz.
const UI_FRAME_S: f64 = 1.0 / 60.0;
/// A sample that moves less than this from the one before is still: the
/// sound stopped (a master's DC blocker decays from a cut far slower).
const STILL: f32 = 1e-5;

fn ms(m: f64) -> usize {
    (m * SR / 1000.0) as usize
}

fn on(note: u8) -> Command {
    Command::NoteOn { note, velocity: 0.9, source: SourceId::Keys, channel: 0 }
}

fn off(note: u8) -> Command {
    Command::NoteOff { note, source: SourceId::Keys, channel: 0 }
}

/// Keys → Strings (rig 0) → Delay → Reverb → Master, Keys → Piano (rig 1) →
/// Master.
fn rack_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let surge = || NodeKind::Clap { plugin_id: ClapId::new("Surge XT") };
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let a = g.add_node(surge(), name_bytes("STRINGS"));
    let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
    let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
    let ma = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let b = g.add_node(surge(), name_bytes("PIANO"));
    let mb = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let cables = [(keys, a, PortKind::Midi), (a, delay, PortKind::Audio), (delay, verb, PortKind::Audio), (verb, ma, PortKind::Audio), (keys, b, PortKind::Midi), (b, mb, PortKind::Audio)];
    for (id, (from, to, kind)) in cables.into_iter().enumerate() {
        g.connect_with_id(id as u32 + 1, PortRef { node: from, port: 0 }, PortRef { node: to, port: 0 }, kind);
    }
    g.set_rig(b, 1);
    g.set_rig(mb, 1);
    (g, a, b)
}

fn setlist(strings_rig: NodeId, piano: NodeId) -> Setlist {
    let rig = |rig, level| PartRig { rig, level, ..PartRig::default() };
    let mut strings = Song::new("Strings", vec![Part::new("Verse", vec![rig(0, VERSE)]), Part::new("Chorus", vec![rig(0, CHORUS)])]);
    strings.bpm = Some(80.0);
    strings.parts[0].set_param(strings_rig, super::P_LEVEL, VERSE_LEVEL);
    strings.parts[1].set_param(strings_rig, super::P_LEVEL, CHORUS_LEVEL);
    let mut piano_song = Song::new("Piano", vec![Part::new("Main", vec![rig(1, 0.5)])]);
    piano_song.bpm = Some(120.0);
    piano_song.parts[0].set_param(piano, super::P_LEVEL, 0.85);
    let mut sl = Setlist { songs: vec![strings, piano_song], ..Setlist::default() };
    sl.learn(crate::show::ShowAction::NextPart, None, crate::show::ShowMsg::Cc(NEXT_CC));
    sl.normalize();
    sl
}

/// How a switch reaches the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Via {
    /// The app's `apply_setlist_part`.
    App,
    /// The foot switch: the engine steps, the app recalls the rest a UI
    /// frame later.
    FootSwitch,
}

/// What `apply_setlist_part` sends for the active part, or with `zones`
/// false what `follow_show` sends after an engine step. The rigs keep the
/// pans this test gave them.
fn recall(sl: &Setlist, graph: &Graph, zones: bool) -> Vec<Command> {
    let mut out: Vec<Command> = sl.gates(graph).commands().into_iter().filter(|c| zones || !matches!(c, Command::SetRigZones { .. })).collect();
    if zones {
        out.push(Command::SetShowPart { part: sl.active_index() });
    }
    if let Some(bpm) = sl.current().and_then(|(s, _, _)| s.bpm) {
        out.push(Command::SetBpm { bpm });
    }
    out.extend(sl.recalled_rigs().iter().map(|r| Command::SetRigMix { rig: r.rig, vol: Some(r.level), pan: Some(PAN[r.rig as usize]), mute: Some(false), solo: Some(false) }));
    out.extend(sl.recalled_params(graph).iter().map(|p| Command::SetParam { node: p.node, param: p.param, value: p.value }));
    out
}

/// Surge's "Init Sine" template: a held sine chord shows a click as a spike
/// of its second difference.
fn init_sine() -> anyhow::Result<Vec<u8>> {
    let listed = crate::host::list_surge_patches().into_iter().find(|p| p.name == "Init Sine").map(|p| p.path);
    let xdg = std::env::var_os("XDG_DATA_HOME").map(|d| std::path::PathBuf::from(d).join("surge-xt/patches_factory/Templates/Init Sine.fxp"));
    let path = listed.or(xdg.filter(|p| p.exists())).ok_or_else(|| anyhow::anyhow!("Surge XT's template 'Init Sine' is not installed"))?;
    crate::host::fxp_to_state(&std::fs::read(&path)?)
}

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    graph: Graph,
    sl: Setlist,
    block: usize,
    buf: Vec<f32>,
    /// Every rendered frame, interleaved L/R.
    out: Vec<f32>,
    /// Seconds each render call took.
    times: Vec<f64>,
    _mains: [ClapVoiceMain; 2],
}

impl Rack {
    fn new(block: usize, sine: &[u8]) -> anyhow::Result<Self> {
        let (va, mut ma) = ClapVoice::new(SR)?;
        let (vb, mb) = ClapVoice::new(SR)?;
        let (graph, strings, piano) = rack_graph();
        let (bus, drain) = command_bus();
        let engine = GraphEngine::new(graph.clone(), drain, SR, vec![(strings, va), (piano, vb)]);
        ma.load_state(sine)?;
        let sl = setlist(strings, piano);
        let show = [Command::SetShowControls { controls: sl.show_controls() }, Command::SetShowParts { parts: crate::show::ShowParts::new(sl.show_parts()) }];
        let mut rack = Rack { engine, bus, graph, sl, block, buf: vec![0.0; block * 2], out: Vec::new(), times: Vec::new(), _mains: [ma, mb] };
        rack.send(show);
        rack.send((0..2).map(|rig| Command::SetRigMix { rig, vol: Some(0.5), pan: Some(PAN[rig as usize]), mute: None, solo: None }));
        // Surge applies a loaded state on the audio thread.
        rack.render(0.25);
        Ok(rack)
    }

    fn send(&mut self, cmds: impl IntoIterator<Item = Command>) {
        for c in cmds {
            assert!(self.bus.send(c), "command bus full");
        }
    }

    /// The frame the next block starts at.
    fn now(&self) -> usize {
        self.out.len() / 2
    }

    fn render_blocks(&mut self, n: usize) {
        for _ in 0..n {
            let t = Instant::now();
            self.engine.render(&mut self.buf, 2);
            self.times.push(t.elapsed().as_secs_f64());
            self.out.extend_from_slice(&self.buf);
        }
    }

    fn render(&mut self, secs: f64) {
        self.render_blocks((secs * SR / self.block as f64).ceil() as usize);
    }

    /// Switch to the setlist's active part in the next block, with `with`
    /// in the same block. Returns the frames the switch block and the
    /// recall's block start at.
    fn switch(&mut self, via: Via, with: &[Command]) -> (usize, usize) {
        let press = |value| Command::Cc { source: SourceId::Hardware(0), channel: 0, cc: NEXT_CC, value };
        match via {
            Via::App => self.send(recall(&self.sl, &self.graph, true)),
            Via::FootSwitch => self.send([press(127)]),
        }
        self.send(with.iter().cloned());
        let at = self.now();
        self.render_blocks(1);
        if via == Via::App {
            return (at, at);
        }
        assert_eq!(self.engine.show.part(), self.sl.active_index(), "the foot switch stepped to the part the setlist steps to");
        self.send([press(0)]);
        self.render(UI_FRAME_S);
        self.send(recall(&self.sl, &self.graph, false));
        let recalled = self.now();
        self.render_blocks(1);
        (at, recalled)
    }
}

/// Where each switch of a take happened (frames: the switch block, the
/// recall's block), and what it rendered.
#[derive(Clone)]
struct Take {
    block: usize,
    via: Via,
    out: Vec<f32>,
    times: Vec<f64>,
    part_switch: (usize, usize),
    song_switch: (usize, usize),
    release: usize,
    tail_switch: (usize, usize),
    baseline: usize,
}

fn take(block: usize, via: Via, sine: &[u8]) -> anyhow::Result<Take> {
    let mut r = Rack::new(block, sine)?;
    r.sl.select(0, 0);
    r.switch(Via::App, &[]);
    r.send(CHORD.map(on));
    r.render(1.5);
    r.sl.next_part();
    let part_switch = r.switch(via, &[]);
    r.render(1.0);
    r.sl.next_part();
    let song_switch = r.switch(via, &[on(FIRST_NOTE)]);
    r.render(1.0);
    r.send(CHORD.map(off));
    let release = r.now();
    r.render(3.0);
    r.send([off(FIRST_NOTE)]);
    r.render(3.0);
    // A note released just before the switch: its tail rings across it.
    r.sl.select(0, 1);
    r.switch(Via::App, &[]);
    r.render(0.3);
    r.send([on(64)]);
    r.render(0.2);
    r.send([off(64)]);
    r.render(0.05);
    r.sl.next_part();
    let tail_switch = r.switch(via, &[]);
    r.render(3.0);
    // The same first note with rig B already live: no switch.
    r.send([on(FIRST_NOTE)]);
    let baseline = r.now();
    r.render(0.5);
    r.send([off(FIRST_NOTE)]);
    r.render(0.5);
    Ok(Take { block, via, out: r.out, times: r.times, part_switch, song_switch, release, tail_switch, baseline })
}

/// `t` with rig A's channel zeroed for one block at every switch.
fn dropped_out(t: &Take) -> Take {
    let mut c = t.clone();
    for (s0, _) in [t.part_switch, t.song_switch, t.tail_switch] {
        for f in c.out[s0 * 2..(s0 + t.block) * 2].chunks_exact_mut(2) {
            f[0] = 0.0;
        }
    }
    c
}

fn channel(out: &[f32], c: usize, from: usize, to: usize) -> Vec<f32> {
    out.chunks_exact(2).skip(from).take(to.saturating_sub(from)).map(|f| f[c]).collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| (v * v) as f64).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
}

fn window_rms(x: &[f32], w: usize) -> Vec<f32> {
    x.chunks_exact(w).map(rms).collect()
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

fn longest_still_run(x: &[f32]) -> usize {
    let (mut best, mut run) = (0, 0);
    for w in x.windows(2) {
        run = if (w[1] - w[0]).abs() < STILL { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Largest second difference: a sine moves smoothly, a step does not.
fn max_d2(x: &[f32]) -> f32 {
    x.windows(3).map(|w| (w[0] - 2.0 * w[1] + w[2]).abs()).fold(0.0, f32::max)
}

/// The held chord (left channel) across a switch whose block starts at
/// frame `s0` and whose recall's block starts at `recalled`.
struct Held {
    still_before: usize,
    still_after: usize,
    /// Level 0.7 to 1 s after the switch against the 300 ms before it: LEVEL
    /// acts before the rig's delay and reverb, whose echoes of the old level
    /// take that long to fade.
    level_db: f32,
    /// Largest second difference from the switch until the recalled gains
    /// have landed, and in the 300 ms before.
    click: f32,
    natural: f32,
}

impl Held {
    fn across(t: &Take, (s0, recalled): (usize, usize)) -> Self {
        let l = |a, b| channel(&t.out, 0, a, b);
        let before = l(s0 - ms(300.0), s0);
        let after = l(s0, s0 + ms(300.0));
        let level = |x: &[f32]| median(window_rms(x, ms(10.0)));
        let landed = recalled + t.block.max(ms(super::gain_ramp::RAMP_S * 1000.0) + 1);
        Held {
            still_before: longest_still_run(&before),
            still_after: longest_still_run(&after),
            level_db: db(level(&l(s0 + ms(700.0), s0 + ms(1000.0))) / level(&before)),
            click: max_d2(&l(s0 - ms(1.0), landed + ms(1.0))),
            natural: max_d2(&before[..before.len() - ms(1.0)]),
        }
    }

    fn checks(&self, expect_db: f32) -> [(bool, String); 3] {
        let grow = 10f32.powf(expect_db.max(0.0) / 20.0);
        [
            (
                self.still_after <= self.still_before + 1,
                format!("no silence: longest run of samples that move less than {STILL:e} is {} after the switch, {} before", self.still_after, self.still_before),
            ),
            (
                self.click <= 4.0 * self.natural * grow,
                format!("no click: largest second difference across the switch {:.5}, {:.5} in the 300 ms before", self.click, self.natural),
            ),
            ((self.level_db - expect_db).abs() <= 1.0, format!("level {:+.2} dB across the switch (expected {expect_db:+.2} dB)", self.level_db)),
        ]
    }
}

/// The tail of a note released 50 ms before a switch at `s0` (left channel).
struct Tail {
    /// Energy in the 300 ms after the switch against the 50 ms before it.
    energy_db: f32,
    /// Quietest 5 ms window in the 100 ms after the switch against the 50 ms before it.
    dip_db: f32,
}

impl Tail {
    fn across(t: &Take, (s0, _): (usize, usize)) -> Self {
        let before = rms(&channel(&t.out, 0, s0 - ms(50.0), s0));
        let after = channel(&t.out, 0, s0, s0 + ms(300.0));
        let dip = window_rms(&after[..ms(100.0)], ms(5.0)).into_iter().fold(f32::INFINITY, f32::min);
        Tail { energy_db: db(rms(&after) / before), dip_db: db(dip / before) }
    }

    fn checks(&self) -> [(bool, String); 2] {
        [
            (self.energy_db >= -20.0, format!("the tail rings on: {:+.1} dB in the 300 ms after the switch against the 50 ms before", self.energy_db)),
            (self.dip_db >= -30.0, format!("the tail has no gap: quietest 5 ms after the switch {:+.1} dB", self.dip_db)),
        ]
    }
}

/// Frames from `s0` until the right channel (rig B) reaches 1 % of the
/// note's peak, and how quiet it was in the second before.
fn onset(t: &Take, s0: usize) -> (Option<usize>, f32) {
    let r = channel(&t.out, 1, s0, s0 + ms(300.0));
    let top = peak(&r);
    let at = (top > 1e-3).then(|| r.iter().position(|v| v.abs() >= 0.01 * top)).flatten();
    (at, peak(&channel(&t.out, 1, s0 - ms(1000.0), s0)))
}

type Check<'a> = dyn FnMut(bool, String) + 'a;

/// The held-note, tail and render-time checks of one take.
fn check_take(t: &Take, check: &mut Check) {
    let deadline_ms = t.block as f64 / SR * 1000.0;
    println!("{:?} switches, {}-frame blocks ({deadline_ms:.2} ms); rig A = left channel, rig B = right", t.via, t.block);
    println!("part switch Verse → Chorus, chord held, rig A's fader {VERSE} → {CHORUS} and LEVEL {VERSE_LEVEL} → {CHORUS_LEVEL}:");
    for (ok, what) in Held::across(t, t.part_switch).checks(chorus_db()) {
        check(ok, what);
    }
    println!("song switch Strings → Piano, chord held on rig A, first note {FIRST_NOTE} with the switch:");
    for (ok, what) in Held::across(t, t.song_switch).checks(0.0) {
        check(ok, what);
    }
    let after_release = peak(&channel(&t.out, 0, t.release + ms(2800.0), t.release + ms(3000.0)));
    check(after_release < 1e-3, format!("the chord released in the new song falls silent on rig A: peak {after_release:.6} 2.8 s after the release"));
    println!("tail of a note released 50 ms before a switch:");
    for (ok, what) in Tail::across(t, t.tail_switch).checks() {
        check(ok, what);
    }
    // Leave out the warm-up, where Surge applies the loaded patch.
    let times_ms: Vec<f64> = t.times.iter().map(|s| s * 1000.0).collect();
    let mut sorted = times_ms[ms(250.0) / t.block + 1..].to_vec();
    sorted.sort_by(f64::total_cmp);
    let shares = switch_shares(t);
    println!(
        "render time of the switch blocks: {:.0} / {:.0} / {:.0} % of the block's {deadline_ms:.2} ms; all blocks median {:.3} ms, max {:.3} ms",
        shares[0] * 100.0, shares[1] * 100.0, shares[2] * 100.0, sorted[sorted.len() / 2], sorted[sorted.len() - 1]
    );
}

/// The level the chorus should have against the verse: fader and LEVEL.
fn chorus_db() -> f32 {
    db(CHORUS / VERSE * (CHORUS_LEVEL / VERSE_LEVEL) as f32)
}

/// Render time of each switch block as a share of the time the block plays.
fn switch_shares(t: &Take) -> [f64; 3] {
    let period = t.block as f64 / SR;
    [t.part_switch, t.song_switch, t.tail_switch].map(|s| t.times[s.0 / t.block] / period)
}

pub fn run() -> anyhow::Result<()> {
    if let Err(e) = ClapVoice::new(SR) {
        println!("setlist-switch-test: SKIP — Surge XT is not installed ({e})");
        return Ok(());
    }
    let sine = init_sine()?;
    let block = crate::bridge::BUFFER_FRAMES as usize;
    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures.push(what);
        }
    };

    let takes = [take(block, Via::App, &sine)?, take(block, Via::FootSwitch, &sine)?, take(64, Via::App, &sine)?, take(1024, Via::App, &sine)?];
    for t in &takes {
        check_take(t, &mut check);
    }
    // A median, so one preemption of this busy machine is not a slow switch.
    let mut shares: Vec<f64> = takes.iter().flat_map(switch_shares).collect();
    shares.sort_by(f64::total_cmp);
    let median_share = shares[shares.len() / 2];
    check(median_share < 0.5, format!("a switch block renders in {:.0} % of its time (median of {}), no xrun to fear", median_share * 100.0, shares.len()));

    println!("first note of the new song (rig B reaches 1 % of its peak), from the start of the block that applied the switch:");
    println!("  switch       block   with the switch       no switch             worst case (+ one block until the switch is picked up)");
    for t in &takes {
        let (with, quiet_with) = onset(t, t.song_switch.0);
        let (without, quiet_without) = onset(t, t.baseline);
        let f2ms = |f: usize| f as f64 / SR * 1000.0;
        let show = |f: Option<usize>| f.map_or("none".to_string(), |f| format!("{f} frames ({:.2} ms)", f2ms(f)));
        let worst = with.map(|f| f2ms(f + t.block));
        let what = format!("{:?}, {}-frame blocks", t.via, t.block);
        println!("  {:<12} {:5}   {:<21} {:<21} {}", format!("{:?}", t.via), t.block, show(with), show(without), worst.map_or("-".into(), |w| format!("{w:.2} ms")));
        check(quiet_with < 1e-6 && quiet_without < 1e-6, format!("{what}: rig B was silent before each first note ({quiet_with:.1e}, {quiet_without:.1e})"));
        let prompt = |f: Option<usize>| f.is_some_and(|f| f2ms(f) <= ONSET_MAX_MS);
        check(prompt(with) && prompt(without), format!("{what}: with the switch the note starts as it does without one, within {ONSET_MAX_MS} ms of its block"));
        check(worst.is_some_and(|w| w <= BUDGET_MS), format!("{what}: within the {BUDGET_MS} ms budget, {} worst case", worst.map_or("none".into(), |w| format!("{w:.2} ms"))));
    }

    println!("control: the app take with rig A's channel zeroed for one block at every switch:");
    let c = dropped_out(&takes[0]);
    let caught = |checks: &[(bool, String)]| {
        for (ok, what) in checks {
            println!("    {} {what}", if *ok { "passes" } else { "fails " });
        }
        checks.iter().filter(|(ok, _)| !ok).count()
    };
    let part = caught(&Held::across(&c, c.part_switch).checks(chorus_db()));
    let song = caught(&Held::across(&c, c.song_switch).checks(0.0));
    let tail = caught(&Tail::across(&c, c.tail_switch).checks());
    check(part >= 2 && song >= 2 && tail >= 1, format!("the checks see the drop-out: {part} of 3 fail at the part switch, {song} of 3 at the song switch, {tail} of 2 on the tail"));

    anyhow::ensure!(failures.is_empty(), "setlist-switch-test: {} check(s) failed: {}", failures.len(), failures.join("; "));
    println!("setlist-switch-test: OK — held notes and tails ring through every switch, the first new note is within {BUDGET_MS} ms");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::AudioInChannel;

    const BLOCK: usize = 256;

    /// A 440 Hz sine on an Audio In through rig 0's master, quiet enough
    /// that the master chain stays linear.
    struct Sine {
        engine: GraphEngine,
        bus: CommandBus,
        feed: rtrb::Producer<f32>,
        frame: usize,
        buf: Vec<f32>,
    }

    impl Sine {
        fn new() -> Self {
            let mut g = Graph::default();
            let input = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("PAD"));
            let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: input, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
            let (bus, drain) = command_bus();
            let mut engine = GraphEngine::new(g, drain, SR, vec![]);
            let (mut side_tx, side_rx) = rtrb::RingBuffer::new(1);
            engine.set_audio_in_channel(side_rx);
            let (feed, rx) = rtrb::RingBuffer::<f32>::new(BLOCK * 8);
            let _ = side_tx.push((input, rx));
            Sine { engine, bus, feed, frame: 0, buf: vec![0.0; BLOCK * 2] }
        }

        /// The left channel of `blocks` blocks.
        fn render(&mut self, blocks: usize) -> Vec<f32> {
            let mut left = Vec::new();
            for _ in 0..blocks {
                for i in 0..BLOCK {
                    let s = 0.1 * (std::f32::consts::TAU * 440.0 * (self.frame + i) as f32 / SR as f32).sin();
                    let _ = self.feed.push(s);
                    let _ = self.feed.push(s);
                }
                self.frame += BLOCK;
                self.engine.render(&mut self.buf, 2);
                left.extend(self.buf.chunks_exact(2).map(|f| f[0]));
            }
            left
        }

        fn fader(&mut self, vol: f32) {
            assert!(self.bus.send(Command::SetRigMix { rig: 0, vol: Some(vol), pan: None, mute: None, solo: None }));
        }
    }

    #[test]
    fn a_rig_fader_a_part_recalls_under_a_held_note_moves_without_a_click() {
        let mut s = Sine::new();
        s.fader(0.4);
        let before = s.render(40);
        let natural = max_d2(&before[before.len() - 4 * BLOCK..]);
        s.fader(0.8);
        let across = s.render(2);
        let after = s.render(20);
        let level = |x: &[f32]| rms(&x[x.len() - 8 * BLOCK..]);
        // At twice the level the sine's own second difference doubles.
        assert!(max_d2(&[&before[before.len() - 2..], &across[..]].concat()) < 4.0 * natural, "the fader ramps: no second-difference spike");
        assert!((level(&after) / level(&before) - 2.0).abs() < 0.02, "and lands on the new level: {} → {}", level(&before), level(&after));
        s.fader(0.0);
        let down = s.render(2);
        assert!(max_d2(&[&after[after.len() - 2..], &down[..]].concat()) < 4.0 * natural, "a fader pulled to 0 fades out too");
        assert!(rms(&s.render(30)[29 * BLOCK..]) < 1e-4, "and the rig is silent then");
    }

    #[test]
    fn a_muted_rig_fades_out_and_back_in() {
        let mut s = Sine::new();
        let held = s.render(40);
        let natural = max_d2(&held[held.len() - 4 * BLOCK..]);
        let mute = |m| Command::SetRigMix { rig: 0, vol: None, pan: None, mute: Some(m), solo: None };
        assert!(s.bus.send(mute(true)));
        let muted = s.render(20);
        assert!(max_d2(&[&held[held.len() - 2..], &muted[..2 * BLOCK]].concat()) < 2.0 * natural, "a mute fades the rig out");
        assert!(s.bus.send(mute(false)));
        let back = s.render(2);
        assert!(max_d2(&[&muted[muted.len() - 2..], &back[..]].concat()) < 2.0 * natural, "no step where the rig comes back");
        let (was, is) = (rms(&held[held.len() - BLOCK..]), rms(&back[BLOCK..]));
        assert!((is / was - 1.0).abs() < 0.05, "and it is back at its level after one block: {was} → {is}");
    }
}
