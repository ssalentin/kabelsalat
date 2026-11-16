//! Host transport: the engine's song clock, handed to every hosted CLAP
//! voice each block so tempo-synced plugin features follow kabelsalat.

/// Time signature reported to plugins (kabelsalat has no meter setting yet).
pub const BEATS_PER_BAR: u16 = 4;

/// Song clock as of a block's first frame. Plain `Copy` state: the engine
/// owns one, the audio thread advances it, nothing allocates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    pub playing: bool,
    pub bpm: f64,
    /// Song position in quarter notes.
    pub beats: f64,
    pub seconds: f64,
}

impl Default for Transport {
    /// Stopped at 120 BPM — what a plugin assumes when a host sends no
    /// transport at all; used by the callers that have no engine clock.
    fn default() -> Self {
        Transport::stopped(120.0)
    }
}

impl Transport {
    pub fn stopped(bpm: f64) -> Self {
        Transport { playing: false, bpm, beats: 0.0, seconds: 0.0 }
    }

    /// Beats one frame lasts. The metronome advances its phase with the same
    /// factor, so a master-started click stays on the transport's beats.
    pub fn beats_per_frame(bpm: f64, sample_rate: f64) -> f64 {
        bpm / (60.0 * sample_rate)
    }

    /// Play rewinds to beat 0 (like the metronome, which starts on the
    /// downbeat); stop keeps the position so plugins see where it halted.
    pub fn set_playing(&mut self, playing: bool) {
        if playing && !self.playing {
            self.beats = 0.0;
            self.seconds = 0.0;
        }
        self.playing = playing;
    }

    /// Move the clock past `frames` rendered frames. Frozen while stopped.
    pub fn advance(&mut self, frames: usize, sample_rate: f64) {
        if self.playing {
            self.beats += frames as f64 * Self::beats_per_frame(self.bpm, sample_rate);
            self.seconds += frames as f64 / sample_rate;
        }
    }

    /// (bar number, beat the bar starts on), both from 0.
    pub fn bar(&self) -> (i32, f64) {
        let bar = (self.beats / BEATS_PER_BAR as f64).floor();
        (bar as i32, bar * BEATS_PER_BAR as f64)
    }
}

/// Note-onset strength of a stereo-interleaved signal: the rising part of
/// its RMS envelope, one value per `ONSET_HOP` frames.
fn onset_strength(samples: &[f32]) -> Vec<f64> {
    let env: Vec<f64> = samples
        .chunks(ONSET_HOP * 2)
        .map(|c| (c.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / c.len() as f64).sqrt())
        .collect();
    env.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect()
}
const ONSET_HOP: usize = 64;

/// How well the onsets sit on a `hz` grid: |Σ o·e^(iωt)| / Σ o, 1 when
/// every onset is on it. Only onset *times* count, so Surge's per-note
/// loudness differences barely move it.
fn grid_coherence(onsets: &[f64], sample_rate: f64, hz: f64) -> f64 {
    let (mut re, mut im, mut total) = (0.0, 0.0, f64::MIN_POSITIVE);
    for (i, o) in onsets.iter().enumerate() {
        let ph = std::f64::consts::TAU * hz * (i * ONSET_HOP) as f64 / sample_rate;
        re += o * ph.cos();
        im += o * ph.sin();
        total += o;
    }
    (re * re + im * im).sqrt() / total
}

/// Rate (Hz) of the grid the onsets sit on: the lowest rate between 1 and
/// 20 Hz whose coherence is about as high as any (multiples match too).
fn onset_grid_hz(onsets: &[f64], sample_rate: f64) -> f64 {
    const STEP_HZ: f64 = 0.005;
    let scores: Vec<f64> = (0..)
        .map(|i| 1.0 + i as f64 * STEP_HZ)
        .take_while(|hz| *hz < 20.0)
        .map(|hz| grid_coherence(onsets, sample_rate, hz))
        .collect();
    let best = scores.iter().cloned().fold(0.0, f64::max);
    let first = (1..scores.len() - 1)
        .find(|&i| scores[i] >= 0.9 * best && scores[i] >= scores[i - 1] && scores[i] >= scores[i + 1])
        .unwrap_or(0);
    1.0 + first as f64 * STEP_HZ
}

/// Which of the 16 sequencer steps was playing when the onsets start, at
/// `step_hz` steps per second: the rotation of the patch's trigger mask that
/// best covers the onset energy per step (silent steps have none). Returns
/// (step, share of onset energy on triggering steps for that rotation).
fn sequencer_step_at_start(onsets: &[f64], sample_rate: f64, step_hz: f64) -> (usize, f64) {
    let mut per_step = [0.0f64; 16];
    for (i, o) in onsets.iter().enumerate() {
        let step = ((i * ONSET_HOP) as f64 * step_hz / sample_rate) as usize;
        per_step[step % 16] += o;
    }
    let total: f64 = per_step.iter().sum::<f64>().max(f64::MIN_POSITIVE);
    (0..16)
        .map(|start| {
            let on_triggers: f64 = (0..16)
                .filter(|slot| GATE_CHORD_TRIGGERS >> ((slot + start) % 16) & 1 == 1)
                .map(|slot| per_step[slot])
                .sum();
            (start, on_triggers / total)
        })
        .fold((0, 0.0), |best, r| if r.1 > best.1 { r } else { best })
}
/// "Gate Chord"'s `trigmask`: bit k set = step k retriggers the envelopes.
/// Steps 9, 10, 12 and 15 are silent, so every rotation is distinct.
const GATE_CHORD_TRIGGERS: u32 = 27135;

/// `transport-test`: real Surge XT, hosted by the real engine, must follow the
/// engine's tempo, song position and play state. The factory patch
/// "Sequences/Gate Chord" gates a held note with a tempo-synced, free-running
/// 16-step sequencer (16th notes, some steps silent): the gate rate heard in
/// the rendered audio is the tempo Surge received, and where the pattern
/// starts relative to the note-on is the song position it received.
pub fn run_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, name_bytes, Command, CommandBus, Graph, NodeKind, PortKind, PortRef};
    use crate::engine::GraphEngine;
    use crate::host::{fxp_to_state, list_surge_patches, ClapVoice};
    use crate::matrix::SourceId;

    const SR: f64 = 48000.0;
    const BLOCK: usize = 480; // 100 blocks per second: whole beats at 120 BPM
    let patches = list_surge_patches();
    let patch = patches
        .iter()
        .find(|p| p.category == "Sequences" && p.name == "Gate Chord")
        .ok_or_else(|| anyhow::anyhow!("Surge factory patch Sequences/Gate Chord not installed"))?;
    let (voice, mut main) = ClapVoice::new(SR)?;
    main.load_state(&fxp_to_state(&std::fs::read(&patch.path)?)?)?;

    let mut g = Graph::default();
    g.reserve(8, 8);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(
        NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") },
        name_bytes("SYNTH"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(g, drain, SR, vec![(synth, voice)]);

    fn render(engine: &mut GraphEngine, seconds: f64, mut heard: Option<&mut Vec<f32>>) {
        let mut buf = [0.0f32; BLOCK * 2];
        for _ in 0..(seconds * SR / BLOCK as f64).round() as usize {
            engine.render(&mut buf, 2);
            if let Some(h) = heard.as_deref_mut() {
                h.extend_from_slice(&buf);
            }
        }
    }
    /// Hold note 60 and return the onsets of `seconds` of it, skipping the
    /// first 2 s (attack, effect tails building up); then release.
    fn hold_note(engine: &mut GraphEngine, bus: &mut CommandBus, seconds: f64) -> Vec<f64> {
        bus.send(Command::NoteOn { note: 60, velocity: 0.9, source: SourceId::Keys });
        render(engine, 2.0, None);
        let mut heard = Vec::new();
        render(engine, seconds, Some(&mut heard));
        bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
        render(engine, 1.3, None);
        onset_strength(&heard)
    }
    render(&mut engine, 0.2, None); // Surge applies the patch on the audio thread

    // Tempo: 16th-note steps are 4 per beat at any tempo. The repeated 120
    // shows the run-to-run spread.
    for bpm in [120.0f32, 60.0, 90.0, 120.0] {
        bus.send(Command::SetBpm { bpm });
        bus.send(Command::TransportPlay);
        let hz = onset_grid_hz(&hold_note(&mut engine, &mut bus, 14.0), SR);
        bus.send(Command::TransportStop);
        let steps = hz * 60.0 / bpm as f64;
        println!("transport-test: {bpm:>5.1} BPM → Surge's gates sit on a {hz:.3} Hz grid = {steps:.3} steps per beat");
        anyhow::ensure!(
            (steps - 4.0).abs() < 0.04,
            "Surge ignores the engine tempo: at {bpm} BPM its sequencer runs {steps:.3} steps per beat, not 4"
        );
    }

    // Song position and play state, at 120 BPM. While the engine plays, a
    // note pressed k beats into the song must find the free-running sequencer
    // on step 4k (mod 16): Surge reads the position we send, and Play rewinds
    // it to 0. Once the engine stops, Surge ignores the (frozen) host
    // position and counts on by itself, so after a 6-step pause the note
    // finds step 4k+6 — step 4k there would mean Surge never saw the stop.
    const STEP_HZ: f64 = 8.0;
    const PAUSE_STEPS: usize = 6;
    for (lead_beats, stop_first) in [
        (0.0, false), (1.0, false), (2.0, false), (3.0, false), (4.0, false), (0.5, false), (0.0, false),
        (1.0, true), (2.0, true),
    ] {
        bus.send(Command::TransportPlay);
        render(&mut engine, lead_beats * 0.5, None);
        if stop_first {
            bus.send(Command::TransportStop);
            render(&mut engine, PAUSE_STEPS as f64 / STEP_HZ, None);
        }
        engine.render(&mut [], 2); // an empty block applies the queued commands
        let at = engine.transport();
        let (step, share) = sequencer_step_at_start(&hold_note(&mut engine, &mut bus, 4.0), SR, STEP_HZ);
        bus.send(Command::TransportStop);
        let host_step = (at.beats * 4.0).round() as usize % 16;
        let want = if stop_first { (host_step + PAUSE_STEPS) % 16 } else { host_step };
        println!(
            "transport-test: note at engine beat {:.2} ({}) → Surge's sequencer on step {step:>2}, expected {want:>2} ({:.0}% of onset energy on its trigger steps)",
            at.beats,
            if at.playing { "playing" } else { "stopped" },
            share * 100.0
        );
        anyhow::ensure!(at.playing != stop_first, "engine transport play state is wrong: {at:?}");
        anyhow::ensure!(
            step == want,
            "Surge ignores the engine's song position or play state: sequencer on step {step}, not {want}"
        );
    }
    println!("transport-test: OK — Surge follows the engine's tempo (4 steps per beat at 120/60/90 BPM), song position and play/stop");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48000.0;

    #[test]
    fn beats_advance_with_tempo_while_playing() {
        let mut t = Transport::stopped(120.0);
        t.set_playing(true);
        for _ in 0..100 {
            t.advance(480, SR);
        }
        // 48000 frames = 1 s = 2 beats at 120 BPM.
        assert!((t.beats - 2.0).abs() < 1e-9, "beats {}", t.beats);
        assert!((t.seconds - 1.0).abs() < 1e-9, "seconds {}", t.seconds);
    }

    #[test]
    fn stopped_transport_is_frozen() {
        let mut t = Transport::stopped(120.0);
        t.advance(48000, SR);
        assert_eq!((t.beats, t.seconds), (0.0, 0.0), "never played, never moves");
        t.set_playing(true);
        t.advance(24000, SR);
        t.set_playing(false);
        let halted = t;
        t.advance(48000, SR);
        assert_eq!(t, halted, "stop keeps the position");
        assert!((t.beats - 1.0).abs() < 1e-9);
    }

    #[test]
    fn play_rewinds_only_from_stopped() {
        let mut t = Transport::stopped(120.0);
        t.set_playing(true);
        t.advance(24000, SR);
        t.set_playing(true);
        assert!((t.beats - 1.0).abs() < 1e-9, "a second play while playing is a no-op");
        t.set_playing(false);
        t.set_playing(true);
        assert_eq!((t.beats, t.seconds), (0.0, 0.0), "play after stop starts at beat 0");
    }

    #[test]
    fn tempo_change_mid_run_keeps_position_and_changes_slope() {
        let mut t = Transport::stopped(120.0);
        t.set_playing(true);
        t.advance(24000, SR); // 0.5 s at 120 = 1 beat
        t.bpm = 60.0;
        t.advance(48000, SR); // 1 s at 60 = 1 beat
        assert!((t.beats - 2.0).abs() < 1e-9, "beats {}", t.beats);
        assert!((t.seconds - 1.5).abs() < 1e-9, "seconds are tempo-independent");
    }

    #[test]
    fn bar_follows_four_four() {
        let mut t = Transport::stopped(120.0);
        assert_eq!(t.bar(), (0, 0.0));
        t.beats = 3.99;
        assert_eq!(t.bar(), (0, 0.0));
        t.beats = 9.5;
        assert_eq!(t.bar(), (2, 8.0));
    }
}
