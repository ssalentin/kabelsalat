//! `kabelsalat gig-test [--keep]` (#400): a gig take with raw inputs, MIDI
//! and two part switches, read back, replayed through the rack, exported.
//!
//! Two rigs, each an Audio In (a vocal at 440 Hz, a keys line at 220 Hz)
//! into its own master; rig A's goes through a Filter whose cutoff a
//! hardware CC moves. Three setlist parts: A alone, B alone, both. The live
//! engine records master, both stems, both raw inputs, the MIDI (CC sweep,
//! notes, a program change, a bend) and the part switches. Then:
//!
//! 1. every file is read back: master and inputs sample for sample against
//!    what was rendered and fed (the master also through the file player's
//!    reader, rule 54), every event at its block's frame with its payload,
//!    every marker at the block of its switch, no dropped frame;
//! 2. a fresh engine with the same rack replays the take (Virtual
//!    Soundcheck, no input device attached): its output must equal the
//!    take's master sample for sample, since nothing in this rack is
//!    free-running (rule 150 holds for plugins, not for these units);
//! 3. a live fader move during the replay is heard in the next blocks;
//!    a seek to the last marker lands in its part; a loop wraps;
//! 4. the export's float mixdown is the master, and its stems are the
//!    take's stems.
//!
//! Needs no plugin, no device and no network; files go under
//! `~/.cache/ksalat-gig-test-<pid>` and are removed unless `--keep`.

use std::path::PathBuf;
use std::sync::Arc;

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, AudioInChannel, Command, CommandBus, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef, RigZone, MAX_RIGS};
use crate::matrix::{CcTarget, SourceId};
use crate::show::{ShowPart, ShowParts};
use crate::take::{Take, TakeEv};

const SR: f64 = 48000.0;
const BLOCK: usize = 256;
/// Blocks of the take: 2 s.
const BLOCKS: usize = 375;
const SWITCH_1: usize = 120;
const SWITCH_2: usize = 250;
const HW: SourceId = SourceId::Hardware(0);

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    vocal: NodeId,
    keys: NodeId,
}

fn zones(a: bool, b: bool) -> [RigZone; MAX_RIGS] {
    let mut z = [RigZone::OPEN; MAX_RIGS];
    z[0].live = a;
    z[1].live = b;
    z
}

fn parts() -> Vec<[RigZone; MAX_RIGS]> {
    vec![zones(true, false), zones(false, true), zones(true, true)]
}

fn rack() -> Rack {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let input = |g: &mut Graph, name: &str, rig: u8| {
        let n = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes(name));
        g.set_rig(n, rig);
        n
    };
    let vocal = input(&mut g, "VOCAL MIC", 0);
    let filter = g.add_node(NodeKind::Dsp(DspKind::Filter), name_bytes("FILTER"));
    g.set_rig(filter, 0);
    let ma = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.set_rig(ma, 0);
    let keys = input(&mut g, "KEYS DI", 1);
    let mb = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.set_rig(mb, 1);
    let audio = |g: &mut Graph, id: u32, from: NodeId, to: NodeId| {
        g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port: 0 }, PortKind::Audio);
    };
    audio(&mut g, 1, vocal, filter);
    audio(&mut g, 2, filter, ma);
    audio(&mut g, 3, keys, mb);
    let (mut bus, drain) = command_bus();
    let engine = GraphEngine::new(g, drain, SR, vec![]);
    let table = parts().into_iter().enumerate().map(|(song, zones)| ShowPart { song: 0, skip: false, zones: { let _ = song; zones } }).collect();
    assert!(bus.send(Command::SetShowParts { parts: ShowParts::new(table) }));
    assert!(bus.send(Command::SetRigZones { zones: parts()[0] }));
    assert!(bus.send(Command::SetShowPart { part: Some(0) }));
    assert!(bus.send(Command::MatrixCcMap { source: HW, dest: filter, cc: 7, param: Some(0), target: CcTarget::PluginParam, scale: Default::default() }));
    Rack { engine, bus, vocal, keys }
}

fn tone(frame: usize, hz: f32, amp: f32) -> f32 {
    // A little second partial so the filter has something to take away.
    let t = frame as f32 / SR as f32;
    amp * ((std::f32::consts::TAU * hz * t).sin() + 0.4 * (std::f32::consts::TAU * hz * 7.0 * t).sin())
}

/// What is played in block `b`, as the hardware would send it.
fn midi_of(b: usize) -> Vec<Command> {
    let mut out = Vec::new();
    if b % 10 == 3 {
        out.push(Command::Cc { source: HW, channel: 0, cc: 7, value: ((b * 7) % 128) as u8 });
    }
    if b % 40 == 5 {
        out.push(Command::NoteOn { note: 60 + (b % 12) as u8, velocity: 0.75, source: HW, channel: 1 });
    }
    if b % 40 == 25 {
        out.push(Command::NoteOff { note: 60 + ((b - 20) % 12) as u8, source: HW, channel: 1 });
    }
    if b == 77 {
        out.push(Command::Program { source: HW, channel: 0, program: 5 });
        out.push(Command::PitchBend { source: HW, channel: 0, value: -1234 });
    }
    out
}

fn scratch() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache").join(format!("ksalat-gig-test-{}", std::process::id()))
}

fn wait(what: &str, mut done: impl FnMut() -> bool) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    while !done() {
        if t0.elapsed() > std::time::Duration::from_secs(10) {
            anyhow::bail!("gig-test: timed out waiting for {what}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

fn same(what: &str, got: &[f32], want: &[f32]) -> anyhow::Result<()> {
    if got.len() != want.len() {
        anyhow::bail!("gig-test: {what}: {} samples, want {}", got.len(), want.len());
    }
    if let Some(i) = got.iter().zip(want).position(|(a, b)| a.to_bits() != b.to_bits()) {
        anyhow::bail!("gig-test: {what}: sample {i} is {} where {} was ({} of {} differ)", got[i], want[i],
            got.iter().zip(want).filter(|(a, b)| a.to_bits() != b.to_bits()).count(), got.len());
    }
    Ok(())
}

fn read_all(path: &std::path::Path) -> anyhow::Result<Vec<f32>> {
    let mut w = crate::take::WavStream::open(path).map_err(anyhow::Error::msg)?;
    let mut out = Vec::new();
    let n = w.frames as usize;
    w.read_stereo(0, n, &mut out).map_err(anyhow::Error::msg)?;
    Ok(out)
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

pub fn run(keep: bool) -> anyhow::Result<()> {
    let dir = scratch();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let r = run_in(&dir);
    if !keep {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("gig-test: files kept in {}", dir.display());
    }
    r
}

fn run_in(dir: &std::path::Path) -> anyhow::Result<()> {
    // ---- 1. the gig ----
    let mut live = rack();
    let (tap, rec) = crate::record::start(dir.to_path_buf(), SR as u32, 2);
    live.engine.set_recorder(tap);
    rec.set_rig_names(vec!["Vocal".into(), "Keys".into()]);
    rec.set_input_names(vec![(live.vocal, "Vocal mic".into()), (live.keys, "Keys DI".into())]);
    rec.set_part_names(vec![("Opener".into(), "Intro".into()), ("Opener".into(), "Verse".into()), ("Opener".into(), "Outro".into())]);
    rec.set_midi_ports(vec!["Stage keyboard".into()]);
    rec.ensure_stem(0);
    rec.ensure_stem(1);
    assert!(live.bus.send(Command::SetRecordArm { rig: Some(0), armed: true }));
    assert!(live.bus.send(Command::SetRecordArm { rig: Some(1), armed: true }));
    let (mut side_tx, side_rx) = rtrb::RingBuffer::new(8);
    live.engine.set_audio_in_channel(side_rx);
    let mut feeds = Vec::new();
    for (node, hz, amp) in [(live.vocal, 440.0f32, 0.25f32), (live.keys, 220.0, 0.2)] {
        let (tx, rx) = rtrb::RingBuffer::<f32>::new(BLOCK * 8);
        let _ = side_tx.push((node, rx));
        feeds.push((tx, hz, amp, Vec::<f32>::new()));
    }
    let mut out = vec![0.0f32; BLOCK * 2];
    live.engine.render(&mut out, 2); // the rack set up, silence through it
    let mut master = Vec::new();
    let mut sent: Vec<(u64, TakeEv)> = Vec::new();
    let mut switches = Vec::new();
    for b in 0..BLOCKS {
        if b == 0 {
            assert!(live.bus.send(Command::RecordStart));
        }
        for (i, z) in [(SWITCH_1, 1usize), (SWITCH_2, 2)] {
            if b == i {
                assert!(live.bus.send(Command::SetRigZones { zones: parts()[z] }));
                assert!(live.bus.send(Command::SetShowPart { part: Some(z) }));
                switches.push(((b * BLOCK) as u64, z));
            }
        }
        for c in midi_of(b) {
            sent.push(((b * BLOCK) as u64, TakeEv::of(&c).expect("a played event")));
            assert!(live.bus.send(c));
        }
        for (tx, hz, amp, fed) in &mut feeds {
            for i in 0..BLOCK {
                let s = tone(b * BLOCK + i, *hz, *amp);
                let _ = tx.push(s);
                let _ = tx.push(-s);
                fed.push(s);
                fed.push(-s);
            }
        }
        live.engine.render(&mut out, 2);
        master.extend_from_slice(&out);
    }
    assert!(live.bus.send(Command::RecordStop));
    live.engine.render(&mut out, 2);
    wait("the take to finalize", || rec.snapshot().finalized && rec.snapshot().takes == 1)?;
    let status = rec.snapshot();
    let folder = PathBuf::from(status.folder.clone().ok_or_else(|| anyhow::anyhow!("no take folder"))?);
    if status.dropped_frames != 0 {
        anyhow::bail!("gig-test: {} frames dropped while recording", status.dropped_frames);
    }

    // ---- 2. read it back ----
    let take = Take::open(&folder).map_err(anyhow::Error::msg)?;
    let m = &take.manifest;
    if m.frames != (BLOCKS * BLOCK) as u64 || m.inputs.len() != 2 || m.stems.len() != 2 || m.master.is_none() {
        anyhow::bail!("gig-test: manifest {m:?}");
    }
    same("master.wav", &read_all(&folder.join(m.master.as_ref().unwrap()))?, &master)?;
    let played = crate::player::load_file(&folder.join(m.master.as_ref().unwrap()))?;
    same("master.wav through the file player", &played.samples, &master)?;
    for (i, (_, _, _, fed)) in feeds.iter().enumerate() {
        same(&format!("input {} ({})", i + 1, m.inputs[i].file), &read_all(&folder.join(&m.inputs[i].file))?, fed)?;
    }
    let midi: Vec<(u64, TakeEv)> = take.events.iter().filter(|t| !matches!(t.ev, TakeEv::Part { .. })).map(|t| (t.frame, t.ev)).collect();
    if midi != sent {
        anyhow::bail!("gig-test: {} events read back, {} sent; first difference: {:?}", midi.len(), sent.len(),
            midi.iter().zip(&sent).find(|(a, b)| a != b));
    }
    let markers: Vec<(u64, Option<usize>, String)> = m.markers.iter().map(|k| (k.frame, k.part, k.label())).collect();
    let want = vec![
        (0, Some(0), "Opener · Intro".to_string()),
        (switches[0].0, Some(1), "Opener · Verse".to_string()),
        (switches[1].0, Some(2), "Opener · Outro".to_string()),
    ];
    if markers != want {
        anyhow::bail!("gig-test: markers {markers:?}, want {want:?}");
    }
    let stem_b = read_all(&folder.join(&m.stems[1].file))?;
    let (b_off, b_on) = (rms(&stem_b[..SWITCH_1 * BLOCK * 2]), rms(&stem_b[(SWITCH_1 + 20) * BLOCK * 2..SWITCH_2 * BLOCK * 2]));
    if b_off > 1e-6 || b_on < 0.01 {
        anyhow::bail!("gig-test: rig B's stem reads {b_off} before its part and {b_on} in it");
    }
    let cc_moved = {
        // The CC sweep moves the filter: rig A's output is not one level.
        let stem_a = read_all(&folder.join(&m.stems[0].file))?;
        let w = |k: usize| rms(&stem_a[k * BLOCK * 2..(k + 10) * BLOCK * 2]);
        (w(10) - w(60)).abs() > 1e-4
    };
    if !cc_moved {
        anyhow::bail!("gig-test: the recorded CC sweep did not move the filter on rig A");
    }
    println!(
        "gig-test: recorded {:.2} s: master, 2 stems, 2 raw inputs, {} MIDI events, {} markers; every file read back sample for sample",
        take.seconds(), midi.len(), markers.len()
    );

    // ---- 3. Virtual Soundcheck ----
    let mut sc = rack();
    let (feed, shared) = crate::soundcheck::channel(SR as u32);
    sc.engine.set_soundcheck(feed);
    sc.engine.render(&mut out, 2); // the same set-up block as live
    shared.start(take.clone(), crate::soundcheck::Play::default()).map_err(anyhow::Error::msg)?;
    if !shared.wait_primed(std::time::Duration::from_secs(5)) {
        anyhow::bail!("gig-test: the soundcheck reader never read ahead");
    }
    let mut replay = Vec::new();
    for _ in 0..BLOCKS {
        shared.wait_ahead(2, std::time::Duration::from_secs(5));
        sc.engine.render(&mut out, 2);
        replay.extend_from_slice(&out);
    }
    same("the replay through the rack against the take's master", &replay, &master)?;
    if shared.status().underruns != 0 {
        anyhow::bail!("gig-test: the replay underran {} times", shared.status().underruns);
    }
    println!("gig-test: Virtual Soundcheck replays the take through the rack: output = master.wav, sample for sample ({} samples)", replay.len());

    // A live edit during the replay is heard: rig A muted from the marker on.
    let section = |i: usize| (take.manifest.markers[i].frame, take.marker_end(i));
    shared.start(take.clone(), crate::soundcheck::Play { from: section(0).0, loop_range: None }).map_err(anyhow::Error::msg)?;
    shared.wait_primed(std::time::Duration::from_secs(5));
    let mut before = Vec::new();
    for _ in 0..40 {
        shared.wait_ahead(2, std::time::Duration::from_secs(5));
        sc.engine.render(&mut out, 2);
        before.extend_from_slice(&out[..]);
    }
    assert!(sc.bus.send(Command::SetRigMix { rig: 0, vol: Some(0.0), pan: None, mute: None, solo: None }));
    let mut after = Vec::new();
    for _ in 0..40 {
        shared.wait_ahead(2, std::time::Duration::from_secs(5));
        sc.engine.render(&mut out, 2);
        after.extend_from_slice(&out[..]);
    }
    let (lb, la) = (rms(&before[20 * BLOCK * 2..]), rms(&after[20 * BLOCK * 2..]));
    if lb < 0.01 || la > lb * 0.05 {
        anyhow::bail!("gig-test: rig A's fader at 0 during the replay: {lb} before, {la} after");
    }
    assert!(sc.bus.send(Command::SetRigMix { rig: 0, vol: Some(1.0), pan: None, mute: None, solo: None }));
    // Seek to the last marker: the setlist is in its part.
    shared.start(take.clone(), crate::soundcheck::Play { from: section(2).0, loop_range: None }).map_err(anyhow::Error::msg)?;
    shared.wait_primed(std::time::Duration::from_secs(5));
    sc.engine.render(&mut out, 2);
    if sc.engine.show.part() != Some(2) || sc.engine.rig_zones[0].live != parts()[2][0].live || !sc.engine.rig_zones[1].live {
        anyhow::bail!("gig-test: after a seek to the outro the part is {:?}", sc.engine.show.part());
    }
    // Loop the verse: the position wraps back to its start.
    let (a, b) = section(1);
    shared.start(take.clone(), crate::soundcheck::Play { from: a, loop_range: Some((a, b)) }).map_err(anyhow::Error::msg)?;
    shared.wait_primed(std::time::Duration::from_secs(5));
    let mut wrapped = false;
    let mut last = a;
    for _ in 0..((b - a) as usize / BLOCK + 30) {
        shared.wait_ahead(2, std::time::Duration::from_secs(5));
        sc.engine.render(&mut out, 2);
        let p = shared.status().seconds;
        let p = (p * SR).round() as u64;
        wrapped |= p < last;
        last = p;
        if p < a || p > b {
            anyhow::bail!("gig-test: the loop {a}..{b} played frame {p}");
        }
    }
    if !wrapped || sc.engine.show.part() != Some(1) {
        anyhow::bail!("gig-test: the verse loop did not wrap (wrapped {wrapped}, part {:?})", sc.engine.show.part());
    }
    shared.stop();
    sc.engine.render(&mut out, 2);
    if sc.engine.soundcheck.as_ref().is_some_and(|f| f.is_playing()) {
        anyhow::bail!("gig-test: the feed still plays after a stop");
    }
    println!("gig-test: a fader move is heard during the replay ({lb:.3} → {la:.4} rms), a seek lands in its part, a loop wraps, a stop hands the inputs back");

    // ---- 4. export ----
    let spec = crate::take::ExportSpec { mixdown: true, stems: None, inputs: true, format: crate::take::ExportFormat::Float32, ..Default::default() };
    let x = crate::take::export(&take, &spec).map_err(anyhow::Error::msg)?;
    same("the exported mixdown", &read_all(std::path::Path::new(&x.files[0]))?, &master)?;
    for (i, s) in take.manifest.stems.iter().enumerate() {
        same(&format!("the exported stem {}", s.name), &read_all(std::path::Path::new(&x.files[1 + i]))?, &read_all(&folder.join(&s.file))?)?;
    }
    let pcm = crate::take::export(&take, &crate::take::ExportSpec { mixdown: true, stems: Some(vec![]), range: Some(section(1)), ..Default::default() }).map_err(anyhow::Error::msg)?;
    let verse = read_all(std::path::Path::new(&pcm.files[0]))?;
    let (va, vb) = section(1);
    let want = &master[va as usize * 2..vb as usize * 2];
    if verse.len() != want.len() || verse.iter().zip(want).any(|(g, w)| (g - w.clamp(-1.0, 1.0)).abs() > 1.0 / 8_388_608.0) {
        anyhow::bail!("gig-test: the 24-bit export of the verse is not the master's verse");
    }
    println!("gig-test: export: float mixdown = master and stems = take stems sample for sample; the verse as 24-bit within one LSB");
    let _ = Arc::strong_count(&shared);
    rec.shutdown();
    println!("gig-test: OK");
    Ok(())
}
