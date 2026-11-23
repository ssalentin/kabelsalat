//! The stage output matrix (#399) through the real engine: the click on
//! outputs 3–4 while the master plays on 1–2, with nothing of one on the
//! other's channels, and the fall back to the master mix on 2 channels.

use super::*;
use crate::control::{command_bus, name_bytes, AudioInChannel, Command, CommandBus, PortRef};

const N: usize = 256;

struct Stage {
    engine: GraphEngine,
    bus: CommandBus,
    feed: rtrb::Producer<f32>,
    phase: usize,
    metro: NodeId,
    tone: bool,
}

impl Stage {
    /// A tone from an audio input and a metronome, both cabled to the master.
    fn new() -> Self {
        let (bus, drain) = command_bus();
        let mut g = Graph::default();
        g.reserve(16, 16);
        let a = g.add_node(NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo }, name_bytes("IN"));
        let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        g.connect_with_id(1, PortRef { node: a, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g.connect_with_id(2, PortRef { node: metro, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        let mut engine = GraphEngine::new(g, drain, 48000.0, vec![]);
        let (mut side_tx, side_rx) = rtrb::RingBuffer::new(4);
        engine.set_audio_in_channel(side_rx);
        let (feed, rx) = rtrb::RingBuffer::<f32>::new(N * 8);
        let _ = side_tx.push((a, rx));
        Stage { engine, bus, feed, phase: 0, metro, tone: false }
    }

    /// Render `blocks` blocks of `ch` channels; per-channel peaks over all.
    fn render(&mut self, blocks: usize, ch: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; N * ch];
        let mut peaks = vec![0.0f32; ch];
        for _ in 0..blocks {
            for i in 0..N {
                let s = if self.tone { 0.4 * (std::f32::consts::TAU * 440.0 * (self.phase + i) as f32 / 48000.0).sin() } else { 0.0 };
                let _ = self.feed.push(s);
                let _ = self.feed.push(s);
            }
            self.phase += N;
            self.engine.render(&mut out, ch);
            for f in out.chunks_exact(ch) {
                for (c, x) in f.iter().enumerate() {
                    peaks[c] = peaks[c].max(x.abs());
                }
            }
        }
        peaks
    }

    fn click(&mut self, on: bool) {
        self.bus.send(Command::SetBpm { bpm: 240.0 });
        self.bus.send(Command::SetNodePlaying { node: self.metro, playing: on });
    }

    fn route(&mut self, source: OutSource, to: Option<OutChannels>) {
        self.bus.send(Command::SetOutRoute { source, to });
    }
}

/// Enough blocks for several clicks at 240 BPM (one every 0.25 s).
const SECOND: usize = 48000 / N;

#[test]
fn the_click_plays_on_outputs_3_4_and_the_master_on_1_2_with_no_bleed() {
    let mut s = Stage::new();
    s.route(OutSource::Click, Some(OutChannels::pair(2)));
    // The click alone: outputs 3–4 only, 1–2 hold exact silence.
    s.click(true);
    s.render(SECOND / 4, 4);
    let p = s.render(SECOND, 4);
    assert!(p[2] > 0.05 && p[3] > 0.05, "the click is on outputs 3–4: {p:?}");
    assert_eq!((p[0], p[1]), (0.0, 0.0), "nothing of the click on outputs 1–2: {p:?}");
    // The tone alone: outputs 1–2 only, 3–4 hold exact silence.
    s.click(false);
    s.tone = true;
    s.render(SECOND / 2, 4);
    let p = s.render(SECOND, 4);
    assert!(p[0] > 0.1 && p[1] > 0.1, "the master plays on outputs 1–2: {p:?}");
    assert_eq!((p[2], p[3]), (0.0, 0.0), "nothing of the master on outputs 3–4: {p:?}");
    // Both: each on its own pair, and the master meter never hears the click.
    s.click(true);
    let p = s.render(SECOND, 4);
    assert!(p.iter().all(|&x| x > 0.05), "{p:?}");
}

#[test]
fn on_two_channels_the_assigned_click_plays_in_the_master_mix() {
    let mut s = Stage::new();
    let pubd = std::sync::Arc::new(std::sync::Mutex::new(GraphPub::default()));
    s.engine.set_graph_pub(pubd.clone());
    s.route(OutSource::Click, Some(OutChannels::pair(2)));
    s.click(true);
    s.render(SECOND / 4, 2);
    let p = s.render(SECOND, 2);
    assert!(p[0] > 0.05 && p[1] > 0.05, "a 2-channel output plays the click in the master mix: {p:?}");
    {
        let g = pubd.lock().unwrap();
        assert_eq!(g.out_channels, 2, "the engine publishes the stream's channels (rule 33)");
        let held: Vec<OutRoute> = g.out_routes.iter().flatten().copied().collect();
        assert_eq!(held, vec![OutRoute { source: OutSource::Click, to: OutChannels::pair(2) }], "and keeps the assignment");
    }
    // The stage interface comes back: the click leaves the master again,
    // once the master's DC blocker has rung out what it held (rule 133).
    s.render(2 * SECOND, 4);
    let p = s.render(SECOND, 4);
    assert!(p[2] > 0.05 && p[0] == 0.0, "{p:?}");
    assert_eq!(pubd.lock().unwrap().out_channels, 4);
}

#[test]
fn the_master_moves_to_other_outputs_and_a_mono_click_takes_its_level() {
    let mut s = Stage::new();
    s.tone = true;
    s.route(OutSource::Master, Some(OutChannels::pair(4)));
    s.render(SECOND / 2, 6);
    let p = s.render(SECOND / 2, 6);
    assert!(p[4] > 0.1 && p[5] > 0.1, "the master on outputs 5–6: {p:?}");
    assert!(p[..4].iter().all(|&x| x == 0.0), "and on no other output: {p:?}");
    s.tone = false;
    s.route(OutSource::Master, None);
    s.route(OutSource::Click, Some(OutChannels { mono: true, level: 1.0, ..OutChannels::pair(3) }));
    s.click(true);
    s.render(SECOND / 2, 6);
    let full = s.render(SECOND, 6)[3];
    s.route(OutSource::Click, Some(OutChannels { mono: true, level: 0.5, ..OutChannels::pair(3) }));
    s.render(SECOND / 4, 6);
    let p = s.render(SECOND, 6);
    assert!(full > 0.05 && (p[3] / full - 0.5).abs() < 0.05, "a mono click at half level: {} of {full}", p[3]);
    assert!(p[2] == 0.0 && p[4] == 0.0, "a mono output is one channel: {p:?}");
}

#[test]
fn a_load_clears_the_matrix_and_a_full_matrix_refuses_one_more() {
    let mut s = Stage::new();
    assert!(s.engine.set_out_route(OutSource::Click, Some(OutChannels::pair(2))));
    assert!(!s.engine.set_out_route(OutSource::Click, Some(OutChannels::pair(MAX_OUT_CH as u8 - 1))), "a pair past the last output");
    assert!(!s.engine.set_out_route(OutSource::Bus(SendSource::Node(999)), Some(OutChannels::pair(2))), "no such bus");
    for r in 0..(MAX_OUT_ROUTES - 1) as u8 {
        assert!(s.engine.set_out_route(OutSource::Bus(SendSource::Rig(r)), Some(OutChannels::pair(4))));
    }
    assert!(!s.engine.set_out_route(OutSource::Player, Some(OutChannels::pair(2))), "the matrix is full");
    assert_eq!(s.engine.out_routes.len(), MAX_OUT_ROUTES);
    assert_eq!(s.engine.out_routes.capacity(), MAX_OUT_ROUTES, "no growth on the audio thread");
    let g = s.engine.graph().clone();
    s.bus.send(Command::LoadGraph { graph: g });
    s.render(1, 4);
    assert!(s.engine.out_routes.is_empty(), "a load starts without assignments");
}
