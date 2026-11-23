//! `kabelsalat levels-test [--save <dir>]` (#273): a show-level vocal rig and
//! reverb bus play through ten song switches without a gap.
//!
//! Rig A is a vocal mic (an Audio In fed a 440 Hz tone) with a send into a
//! Hall bus (bus → reverb → master). Rigs B and C are song-level sources at
//! 220 and 330 Hz that the songs take turns on. Every switch sends what the
//! app sends: `Setlist::gates(..).commands()`. The master is written to a WAV
//! file, read back with the app's own reader, and searched for a gap in the
//! vocal's band. The same session with the vocal and the bus at song level
//! (named only in every other song) is rendered too: there the search must
//! find the gaps, or it proves nothing. Needs no plugin and no hardware.

use std::path::{Path, PathBuf};

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, AudioInChannel, Command, CommandBus, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef, SendSource};
use crate::setlist::{Level, Part, PartRig, Setlist, SetlistPos, Song};

const SR: f64 = 48000.0;
const BLOCK: usize = 256;
const SWITCHES: usize = 10;
const SONG_SECS: f64 = 0.5;
const EDGE_VOCAL: u32 = 1;
const EDGE_HALL: u32 = 3;
const EDGE_B: u32 = 4;
const EDGE_C: u32 = 5;
/// Analysis window of the file search: 10 ms.
const WINDOW: usize = 480;

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    feeds: Vec<(rtrb::Producer<f32>, f32, f32)>,
    frame: usize,
    out: Vec<f32>,
    hall: NodeId,
}

impl Rack {
    fn new() -> Self {
        let mut g = Graph::default();
        g.reserve(16, 16);
        let input = |g: &mut Graph, name: &str, rig: u8| {
            let n = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes(name));
            g.set_rig(n, rig);
            n
        };
        let master = |g: &mut Graph, rig: u8| {
            let n = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.set_rig(n, rig);
            n
        };
        let audio = |g: &mut Graph, id: u32, from: NodeId, to: NodeId| {
            g.connect_with_id(id, PortRef { node: from, port: 0 }, PortRef { node: to, port: 0 }, PortKind::Audio);
        };
        let vocal = input(&mut g, "VOCAL MIC", 0);
        let ma = master(&mut g, 0);
        let hall = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("HALL"));
        let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
        let b = input(&mut g, "PIANO", 1);
        let mb = master(&mut g, 1);
        let c = input(&mut g, "ORGAN", 2);
        let mc = master(&mut g, 2);
        audio(&mut g, EDGE_VOCAL, vocal, ma);
        audio(&mut g, 2, hall, verb);
        audio(&mut g, EDGE_HALL, verb, ma);
        audio(&mut g, EDGE_B, b, mb);
        audio(&mut g, EDGE_C, c, mc);
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(g, drain, SR, vec![]);
        let (mut side_tx, side_rx) = rtrb::RingBuffer::new(8);
        engine.set_audio_in_channel(side_rx);
        let mut feeds = Vec::new();
        for (node, hz, amp) in [(vocal, 440.0, 0.3), (b, 220.0, 0.2), (c, 330.0, 0.2)] {
            let (tx, rx) = rtrb::RingBuffer::<f32>::new(BLOCK * 8);
            let _ = side_tx.push((node, rx));
            feeds.push((tx, hz, amp));
        }
        assert!(bus.send(Command::SetSend { from: SendSource::Node(vocal), bus: hall, level: Some(0.5), pre: Some(false) }));
        Rack { engine, bus, feeds, frame: 0, out: vec![0.0; BLOCK * 2], hall }
    }

    fn send_all(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            assert!(self.bus.send(c), "command bus full");
        }
    }

    /// Render `blocks` blocks: the master's samples, and per block the
    /// vocal's, the hall return's and rigs B and C's cable levels.
    fn render(&mut self, blocks: usize, master: &mut Vec<f32>) -> Vec<[f32; 4]> {
        let levels = self.engine.edge_levels_handle();
        (0..blocks)
            .map(|_| {
                for i in 0..BLOCK {
                    let t = (self.frame + i) as f32 / SR as f32;
                    for (tx, hz, amp) in &mut self.feeds {
                        let s = (std::f32::consts::TAU * *hz * t).sin() * *amp;
                        let _ = tx.push(s);
                        let _ = tx.push(s);
                    }
                }
                self.frame += BLOCK;
                self.engine.render(&mut self.out, 2);
                master.extend_from_slice(&self.out);
                [levels.load(EDGE_VOCAL), levels.load(EDGE_HALL), levels.load(EDGE_B), levels.load(EDGE_C)]
            })
            .collect()
    }
}

/// Eleven songs that take turns on rig B and rig C: ten switches.
fn alternating_songs() -> Setlist {
    let mut sl = Setlist::default();
    for i in 0..=SWITCHES {
        let rig = 1 + (i % 2) as u8;
        sl.songs.push(Song::new(format!("Song {}", i + 1), vec![Part::new("Main", vec![PartRig::new(rig)])]));
    }
    sl
}

/// The same songs with the vocal rig and the hall bus at song level, named
/// only in every other song.
fn song_level_vocal(hall: NodeId) -> Setlist {
    let mut sl = alternating_songs();
    sl.pin_rig(0, Level::Song);
    sl.pin_bus(hall, Level::Song);
    for (i, s) in sl.songs.iter_mut().enumerate() {
        if i % 2 == 0 {
            s.parts[0].set_rig(PartRig::new(0));
            s.parts[0].buses.push(hall);
        }
    }
    sl
}

/// Goertzel magnitude of `hz` in a Hann-windowed stretch of the left channel.
fn band(samples: &[f32], start: usize, hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / SR;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for i in 0..WINDOW {
        let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (WINDOW - 1) as f64).cos();
        let x = samples.get((start + i) * 2).copied().unwrap_or(0.0) as f64 * hann;
        let s0 = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).max(0.0).sqrt()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// What one session's render shows.
struct Take {
    /// The vocal band's magnitude in each 10 ms window of the file.
    bands: Vec<f64>,
    /// Frames per song.
    song_frames: usize,
    /// (lowest block, median block) of the vocal's and the Hall return's cable.
    vocal: (f32, f32),
    hall: (f32, f32),
    /// Per song: did rig B and rig C sound?
    played: Vec<(bool, bool)>,
}

fn render_take(setlist: impl Fn(NodeId) -> Setlist, file: &Path) -> anyhow::Result<Take> {
    let mut rack = Rack::new();
    let mut sl = setlist(rack.hall);
    sl.normalize();
    let graph = rack.engine.graph().clone();
    let per_song = (SONG_SECS * SR / BLOCK as f64).ceil() as usize;
    let mut master = Vec::new();
    let mut blocks = Vec::new();
    for song in 0..sl.songs.len() {
        sl.active = Some(SetlistPos { song, part: 0 });
        rack.send_all(sl.gates(&graph).commands());
        blocks.extend(rack.render(per_song, &mut master));
    }
    let clip = crate::listen::Clip { sample_rate: SR as u32, channels: 2, samples: master.clone() };
    std::fs::write(file, clip.wav())?;
    let back = crate::player::load_file(file)?;
    anyhow::ensure!(back.samples.len() == master.len(), "{} does not read back as written", file.display());

    let frames = back.samples.len() / 2;
    let bands: Vec<f64> = (0..frames / WINDOW).map(|w| band(&back.samples, w * WINDOW, 440.0)).collect();

    // The reverb needs longer than the direct sound to reach its level.
    let low_and_median = |i: usize, warm_secs: f64| {
        let v: Vec<f64> = blocks[(warm_secs * SR) as usize / BLOCK..].iter().map(|b| b[i] as f64).collect();
        let m = median(v.clone());
        (v.into_iter().fold(f64::INFINITY, f64::min) as f32, m as f32)
    };
    let settle = per_song / 4;
    let played = (0..sl.songs.len())
        .map(|s| {
            let song = &blocks[s * per_song + settle..(s + 1) * per_song];
            (song.iter().any(|b| b[2] > 1e-3), song.iter().any(|b| b[3] > 1e-3))
        })
        .collect();
    Ok(Take {
        bands,
        song_frames: per_song * BLOCK,
        vocal: low_and_median(0, 0.3),
        hall: low_and_median(1, 1.5),
        played,
    })
}

/// Windows after the warm-up where the vocal's band falls below half of
/// `steady`, and the songs (1-based, by each window's centre) they are in.
fn gaps(take: &Take, steady: f64) -> (usize, usize, Vec<usize>) {
    let warm = (0.3 * SR) as usize / WINDOW;
    let gaps: Vec<usize> = (warm..take.bands.len()).filter(|&w| take.bands[w] < 0.5 * steady).collect();
    let mut songs: Vec<usize> = gaps.iter().map(|&w| (w * WINDOW + WINDOW / 2) / take.song_frames + 1).collect();
    songs.dedup();
    (gaps.len(), take.bands.len() - warm, songs)
}

pub fn run(save_dir: Option<&Path>) -> anyhow::Result<()> {
    let dir: PathBuf = match save_dir {
        Some(d) => d.to_path_buf(),
        None => dirs_cache().join("levels-test"),
    };
    std::fs::create_dir_all(&dir)?;
    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures.push(what);
        }
    };

    let show_file = dir.join("levels-test-show.wav");
    let show = render_take(|_| alternating_songs(), &show_file)?;
    println!("show level: vocal rig and Hall bus named by no song ({SWITCHES} switches, {SONG_SECS} s per song) → {}", show_file.display());
    let steady = median(show.bands[(0.3 * SR) as usize / WINDOW..].to_vec());
    let (gap_windows, windows, _) = gaps(&show, steady);
    check(gap_windows == 0, format!("no gap in the vocal's band: {gap_windows} of {windows} 10 ms windows below half its level"));
    let (lo, med) = show.vocal;
    check(med > 0.1 && lo > 0.9 * med, format!("the vocal cable never drops: lowest block peak {lo:.4}, median {med:.4}"));
    let (lo, med) = show.hall;
    check(med > 1e-3 && lo > 0.5 * med, format!("the Hall return never drops: lowest block peak {lo:.4}, median {med:.4}"));
    let turns = show.played.iter().enumerate().all(|(i, &(b, c))| if i % 2 == 0 { b && !c } else { c && !b });
    check(turns, format!("the songs switched: rig B plays in odd songs, rig C in even ones ({:?})", show.played));

    let song_file = dir.join("levels-test-song.wav");
    let song = render_take(song_level_vocal, &song_file)?;
    println!("song level: the same vocal rig and bus, named only in every other song → {}", song_file.display());
    let expected: Vec<usize> = (2..=SWITCHES + 1).step_by(2).collect();
    let (gap_windows, _, gap_songs) = gaps(&song, steady);
    check(
        gap_songs == expected,
        format!("the search finds the gaps where the vocal is out of the song: {gap_windows} windows in songs {gap_songs:?} (expected {expected:?})"),
    );

    // Send scenes (#415): the Stage Sends template's parts under a held tone.
    let scene_file = dir.join("levels-test-send-scenes.wav");
    let take = send_scene_take()?;
    std::fs::write(&scene_file, crate::listen::Clip { sample_rate: SR as u32, channels: 2, samples: take.master.clone() }.wav())?;
    println!("send scenes: the Stage Sends template, Verse → Chorus → Solo → Verse under a held 440 Hz tone → {}", scene_file.display());
    for line in take.report() {
        check(line.0, line.1);
    }

    if failures.is_empty() {
        println!("levels-test: PASS");
        Ok(())
    } else {
        anyhow::bail!("levels-test: {} check(s) failed: {}", failures.len(), failures.join("; "))
    }
}

/// How long each part of the send-scene take plays.
const SCENE_PART_SECS: f64 = 1.0;

/// What the send-scene take shows (#415): per frame, the left channel of
/// what the Hall and Echo buses take in, of the Echo's delay and of the
/// master, and the frame of each part switch.
pub(crate) struct SceneTake {
    pub hall_in: Vec<f32>,
    pub echo_in: Vec<f32>,
    pub echo_out: Vec<f32>,
    /// The master, both channels interleaved, for the file.
    pub master: Vec<f32>,
    pub master_l: Vec<f32>,
    pub switches: Vec<(String, usize)>,
}

/// The largest second difference of `x`: a step in a held sine stands out
/// of it by its whole height, where the largest sample step hides it
/// (rule 202).
fn curve(x: &[f32]) -> f32 {
    x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

impl SceneTake {
    const MS: usize = SR as usize / 1000;

    /// One switch of one bus: (level before, level after, the curve at the
    /// switch, the curve of the settled tone on its louder side).
    fn switch(&self, bus: &[f32], at: usize) -> (f32, f32, f32, f32) {
        let ms = Self::MS;
        let before = &bus[at - 40 * ms..at - 10 * ms];
        let after = &bus[at + 40 * ms..at + 70 * ms];
        (peak(before), peak(after), curve(&bus[at - ms..at + 12 * ms]), curve(before).max(curve(after)))
    }

    /// Every check of the take: (passed, what it showed).
    pub fn report(&self) -> Vec<(bool, String)> {
        let ms = Self::MS;
        let mut out = Vec::new();
        let want = [("Hall", &self.hall_in, [0.5 / 0.15, 0.3 / 0.5, 0.15 / 0.3]), ("Echo", &self.echo_in, [1.0, f32::INFINITY, 0.0])];
        for (name, bus, ratios) in want {
            for (i, (part, at)) in self.switches.iter().enumerate() {
                let (before, after, at_switch, steady) = self.switch(bus, *at);
                let moved = match ratios[i] {
                    r if r == 1.0 => before == 0.0 && after == 0.0,
                    r if r.is_infinite() => before == 0.0 && after > 0.01,
                    r if r == 0.0 => before > 0.01 && after == 0.0,
                    r => before > 0.0 && (after / before - r).abs() < 0.05 * r,
                };
                out.push((moved, format!("→ {part}: the {name} send moved from {before:.4} to {after:.4}")));
                // The ramp's two corners add at most the tone's own curve;
                // a step would add the whole level change.
                let step = (after - before).abs();
                out.push((
                    at_switch <= 3.0 * steady.max(1e-6) || step == 0.0,
                    format!("→ {part}: no click in what the {name} bus takes: second difference {at_switch:.6} at the switch, {steady:.6} in the held tone (a step of {step:.3} would show as up to {step:.3})"),
                ));
            }
        }
        // The Solo's echo rings on after the Verse closed its send.
        let (_, at) = self.switches[2];
        let closed = peak(&self.echo_in[at + 10 * ms..at + 250 * ms]);
        let tail = peak(&self.echo_out[at + 100 * ms..at + 250 * ms]);
        out.push((closed == 0.0 && tail > 0.01, format!("→ Verse: the Echo takes nothing new 10 ms after the switch ({closed:.4}) and its tail rings on ({tail:.4} from 100 to 250 ms after)")));
        for (part, at) in &self.switches {
            let at_switch = curve(&self.master_l[at - ms..at + 12 * ms]);
            let steady = curve(&self.master_l[at - 60 * ms..at - 10 * ms]).max(curve(&self.master_l[at + 40 * ms..at + 90 * ms]));
            out.push((at_switch <= 2.0 * steady, format!("→ {part}: no click at the master: second difference {at_switch:.6} at the switch, {steady:.6} around it")));
        }
        out
    }
}

/// Play the Stage Sends template (`Template::StageSends`: an input rig at
/// show level, Hall and Echo buses, a song of three parts with a send scene
/// each) through Verse → Chorus → Solo → Verse under a held 440 Hz tone. A
/// switch sends what the app sends: the part's gates and its send scene.
pub(crate) fn send_scene_take() -> anyhow::Result<SceneTake> {
    let session = crate::session::Template::StageSends.session();
    let graph = session.graph.clone();
    let node = |name: &str| -> anyhow::Result<NodeId> {
        graph.nodes.iter().find(|n| crate::control::name_str(&n.name) == name).map(|n| n.id).ok_or_else(|| anyhow::anyhow!("the template has no {name}"))
    };
    let (mic, hall, echo, delay) = (node("AUDIO IN")?, node(crate::session::STAGE_HALL)?, node(crate::session::STAGE_ECHO)?, node("ECHO DELAY")?);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(graph.clone(), drain, SR, vec![]);
    let (mut side_tx, side_rx) = rtrb::RingBuffer::new(8);
    engine.set_audio_in_channel(side_rx);
    let (mut feed, rx) = rtrb::RingBuffer::<f32>::new(BLOCK * 8);
    let _ = side_tx.push((mic, rx));
    let send_all = |bus: &mut CommandBus, cmds: Vec<Command>| -> anyhow::Result<()> {
        for c in cmds {
            anyhow::ensure!(bus.send(c), "command bus full");
        }
        Ok(())
    };
    send_all(&mut bus, session.params().iter().map(|p| Command::SetParam { node: p.node, param: p.param, value: p.value }).collect())?;
    send_all(&mut bus, session.sends().iter().map(|x| Command::SetSend { from: x.from, bus: x.bus, level: Some(x.level), pre: Some(x.pre) }).collect())?;
    let mut sl = session.setlist.clone();
    anyhow::ensure!(sl.songs.len() == 1 && sl.songs[0].parts.len() == 3, "the template's song has three parts");
    anyhow::ensure!(sl.rig_level(0) == Level::Show, "the input rig lives at show level");

    let mut take = SceneTake { hall_in: Vec::new(), echo_in: Vec::new(), echo_out: Vec::new(), master: Vec::new(), master_l: Vec::new(), switches: Vec::new() };
    let per_part = (SCENE_PART_SECS * SR / BLOCK as f64).ceil() as usize;
    let mut out = vec![0.0f32; BLOCK * 2];
    let mut frame = 0usize;
    for (step, part) in [0usize, 1, 2, 0].into_iter().enumerate() {
        sl.active = Some(SetlistPos { song: 0, part });
        if step > 0 {
            take.switches.push((sl.songs[0].parts[part].name.clone(), frame));
        }
        send_all(&mut bus, sl.gates(&graph).commands())?;
        send_all(&mut bus, sl.recalled_scene(&graph).commands())?;
        for _ in 0..per_part {
            for i in 0..BLOCK {
                let s = (std::f32::consts::TAU * 440.0 * (frame + i) as f32 / SR as f32).sin() * 0.3;
                let _ = feed.push(s);
                let _ = feed.push(s);
            }
            frame += BLOCK;
            engine.render(&mut out, 2);
            take.master.extend_from_slice(&out);
            take.master_l.extend(out.chunks_exact(2).map(|f| f[0]));
            for (n, dest) in [(hall, &mut take.hall_in), (echo, &mut take.echo_in), (delay, &mut take.echo_out)] {
                let d = engine.dsp.iter().find(|d| d.node == n).ok_or_else(|| anyhow::anyhow!("no slot for node {n}"))?;
                dest.extend(d.buf[..BLOCK * 2].chunks_exact(2).map(|f| f[0]));
            }
        }
    }
    Ok(take)
}

/// `$XDG_CACHE_HOME/kabelsalat`, else `~/.cache/kabelsalat` (never /tmp, rule 79).
fn dirs_cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kabelsalat")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::{CcTarget, MixControl, SourceId};
    use crate::setlist::Binding;

    fn select(rack: &mut Rack, sl: &mut Setlist, song: usize) {
        sl.active = Some(SetlistPos { song, part: 0 });
        let graph = rack.engine.graph().clone();
        rack.send_all(sl.gates(&graph).commands());
    }

    /// Over `blocks` blocks: the largest block peak of what the Hall bus
    /// takes in (its own slot, before its inserts), and of its reverb's return.
    fn hall_peaks(rack: &mut Rack, blocks: usize) -> (f32, f32) {
        let hall = rack.hall;
        let mut sink = Vec::new();
        let mut input = 0.0f32;
        let mut ret = 0.0f32;
        for _ in 0..blocks {
            ret = ret.max(rack.render(1, &mut sink)[0][1]);
            let d = rack.engine.dsp.iter().find(|d| d.node == hall).expect("the bus slot");
            input = input.max(d.buf[..BLOCK * 2].iter().fold(0.0, |m, x| m.max(x.abs())));
        }
        (input, ret)
    }

    /// #415: the Stage Sends template through its three parts under a held
    /// tone. Every send lands on its scene's level, nothing the buses or
    /// the master get steps (second difference, rule 202), and the Solo's
    /// echo rings on after the Verse closed its send.
    #[test]
    fn a_parts_send_scene_moves_the_sends_under_a_held_tone_without_a_click_and_the_tail_rings_on() {
        let take = send_scene_take().unwrap();
        assert_eq!(take.switches.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["Chorus", "Solo", "Verse"]);
        for (ok, what) in take.report() {
            assert!(ok, "{what}");
        }
        // The detector sees a step: the same Hall signal with the Chorus's
        // level put on in one sample.
        let at = take.switches[0].1;
        let mut stepped = take.hall_in[at - 480..at - 240].to_vec();
        // At the tone's crest in the middle of the slice, not at a zero crossing.
        let crest = (60..180).max_by(|&a, &b| stepped[a].abs().total_cmp(&stepped[b].abs())).unwrap();
        for x in &mut stepped[crest..] {
            *x *= 0.5 / 0.15;
        }
        assert!(curve(&stepped) > 20.0 * curve(&take.hall_in[at - 480..at - 240]), "a step stands out of the held tone");
    }

    #[test]
    fn a_song_level_bus_takes_signal_only_in_the_parts_that_name_it_and_its_tail_rings_out() {
        let mut rack = Rack::new();
        let hall = rack.hall;
        let mut sl = song_level_vocal(hall);
        sl.pin_rig(0, Level::Show);
        select(&mut rack, &mut sl, 0);
        assert!(hall_peaks(&mut rack, 40).0 > 0.05, "song 1 names the Hall: it takes the vocal");
        select(&mut rack, &mut sl, 1);
        hall_peaks(&mut rack, 1);
        let (input, tail) = hall_peaks(&mut rack, 4);
        assert_eq!(input, 0.0, "song 2 does not name it: after one block's fade it takes no new signal");
        assert!(tail > 1e-4, "while its reverb still rings ({tail})");
        select(&mut rack, &mut sl, 2);
        hall_peaks(&mut rack, 1);
        assert!(hall_peaks(&mut rack, 4).0 > 0.05, "song 3 names it again");
        select(&mut rack, &mut sl, 1);
        sl.active = None;
        rack.send_all(sl.gates(&rack.engine.graph().clone()).commands());
        hall_peaks(&mut rack, 1);
        assert!(hall_peaks(&mut rack, 4).0 > 0.05, "with no song active every bus is open");
    }

    /// A volume pedal pinned at show level reaches the piano rig's gain in
    /// every song, also those without the piano; the organ's pedal, at the
    /// level of its rig, moves the organ only while the organ plays.
    #[test]
    fn a_show_level_binding_works_in_every_song_and_a_song_level_one_only_with_its_rig() {
        let mut rack = Rack::new();
        let graph = rack.engine.graph().clone();
        let piano = graph.nodes.iter().find(|n| n.rig == 1 && matches!(n.kind, NodeKind::AudioIn { .. })).unwrap().id;
        let organ = graph.nodes.iter().find(|n| n.rig == 2 && matches!(n.kind, NodeKind::AudioIn { .. })).unwrap().id;
        let master_b = graph.nodes.iter().find(|n| n.rig == 1 && matches!(n.kind, NodeKind::MasterOut)).unwrap().id;
        let pedal = SourceId::Hardware(0);
        let bind = |dest, cc, param, target| Command::MatrixCcMap { source: pedal, dest, cc, param: Some(param), target, scale: Default::default() };
        rack.send_all(vec![
            bind(piano, 11, super::super::P_AUDIO_IN_GAIN, CcTarget::Knob),
            bind(organ, 4, super::super::P_AUDIO_IN_GAIN, CcTarget::Knob),
            bind(master_b, 7, MixControl::Vol.param(), CcTarget::Mix),
        ]);
        let mut sl = alternating_songs();
        sl.pin_binding(Binding { source: pedal, dest: piano, cc: 11 }, Level::Show);
        sl.normalize();
        assert_eq!(sl.binding_level(Binding { source: pedal, dest: organ, cc: 4 }, CcTarget::Knob, &graph), Level::Song, "an unpinned binding follows its rig");
        assert_eq!(sl.binding_level(Binding { source: pedal, dest: master_b, cc: 7 }, CcTarget::Mix, &graph), Level::Show, "a mixer binding is at show level");
        let mut sink = Vec::new();
        for song in 0..sl.songs.len() {
            select(&mut rack, &mut sl, song);
            rack.render(1, &mut sink);
            rack.engine.cc_log.clear();
            for cc in [11, 4, 7] {
                rack.bus.send(Command::Cc { source: pedal, channel: 0, cc, value: 100 });
            }
            rack.render(1, &mut sink);
            let moved = |node| rack.engine.cc_log.iter().any(|e| e.0 == node);
            let organ_plays = song % 2 == 1;
            assert!(moved(piano), "song {}: the show-level pedal reaches the piano without being bound again", song + 1);
            assert!(moved(master_b), "song {}: the mixer binding works", song + 1);
            assert_eq!(moved(organ), organ_plays, "song {}: the organ's pedal works only while the organ plays", song + 1);
        }
    }

    #[test]
    fn a_closed_binding_still_lets_a_sustain_pedal_come_up() {
        let mut rack = Rack::new();
        let graph = rack.engine.graph().clone();
        let organ = graph.nodes.iter().find(|n| n.rig == 2 && matches!(n.kind, NodeKind::AudioIn { .. })).unwrap().id;
        let pedal = SourceId::Hardware(0);
        rack.send_all(vec![
            Command::MatrixCcMap { source: pedal, dest: organ, cc: 64, param: Some(super::super::P_AUDIO_IN_GAIN), target: CcTarget::Knob, scale: Default::default() },
            Command::SetBindingLive { source: pedal, dest: organ, cc: 64, live: false },
        ]);
        let mut sink = Vec::new();
        rack.render(1, &mut sink);
        rack.engine.cc_log.clear();
        rack.bus.send(Command::Cc { source: pedal, channel: 0, cc: 64, value: 127 });
        rack.render(1, &mut sink);
        assert!(rack.engine.cc_log.is_empty(), "a closed binding moves nothing");
        rack.bus.send(Command::Cc { source: pedal, channel: 0, cc: 64, value: 0 });
        rack.render(1, &mut sink);
        assert_eq!(rack.engine.cc_log.len(), 1, "but the pedal coming up passes, so nothing stays held");
    }

    #[test]
    fn a_load_opens_every_bus_and_clears_every_binding_gate() {
        let mut rack = Rack::new();
        let hall = rack.hall;
        rack.send_all(vec![
            Command::SetBusLive { bus: hall, live: false },
            Command::SetBindingLive { source: SourceId::Keys, dest: 0, cc: 7, live: false },
        ]);
        hall_peaks(&mut rack, 2);
        assert_eq!(hall_peaks(&mut rack, 2).0, 0.0, "a closed bus takes nothing");
        let graph = rack.engine.graph().clone();
        rack.send_all(vec![Command::LoadGraph { graph }]);
        let mut sink = Vec::new();
        rack.render(1, &mut sink);
        assert_eq!(rack.engine.show_gates.binding(SourceId::Keys, 0, 7), None, "the binding is back at its default");
        let mut buf = vec![1.0f32; 4];
        rack.engine.show_gates.gate_bus_input(hall, &mut buf);
        assert_eq!(buf, vec![1.0; 4], "the bus takes its input again at once, with no fade");
    }
}
