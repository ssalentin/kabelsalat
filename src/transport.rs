//! Host transport: the engine's song clock, handed to every hosted CLAP
//! voice each block so tempo-synced plugin features follow kabelsalat.

/// Time signature reported to plugins (kabelsalat has no meter setting yet).
pub const BEATS_PER_BAR: u16 = 4;

pub const MIN_BPM: f32 = 20.0;
pub const MAX_BPM: f32 = 300.0;
pub const TAP_RESET_SECS: f64 = 2.0;
pub const MAX_TAP_SAMPLES: usize = 8;
pub const MAX_COUNT_IN_BARS: u8 = 4;

/// Count-in configuration: off by default, 1..=4 bars when enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountInConfig {
    pub enabled: bool,
    pub bars: u8,
}

impl Default for CountInConfig {
    fn default() -> Self {
        Self { enabled: false, bars: 1 }
    }
}

impl CountInConfig {
    pub fn normalized(enabled: bool, bars: u8) -> Self {
        Self { enabled, bars: bars.clamp(1, MAX_COUNT_IN_BARS) }
    }

    pub fn total_beats(self) -> f64 {
        if self.enabled {
            self.bars.clamp(1, MAX_COUNT_IN_BARS) as f64 * BEATS_PER_BAR as f64
        } else {
            0.0
        }
    }
}

/// Master transport lifecycle: stopped, counting in before beat 0, or playing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MasterClock {
    Stopped,
    CountingIn { remaining_beats: f64, total_beats: f64 },
    Playing,
}

impl MasterClock {
    pub fn is_counting_in(self) -> bool {
        matches!(self, Self::CountingIn { .. })
    }

    pub fn is_active(self) -> bool {
        !matches!(self, Self::Stopped)
    }

    /// 1-based beat index within the count-in (1..=total_beats), or 0 when not counting in.
    pub fn count_in_beat(self) -> u8 {
        match self {
            Self::CountingIn { remaining_beats, total_beats } => {
                let elapsed = (total_beats - remaining_beats).max(0.0);
                (elapsed.floor() as u8).saturating_add(1).min(total_beats.round() as u8)
            }
            _ => 0,
        }
    }
}

/// Pure tap-tempo estimator: averages recent tap intervals, resets after
/// `TAP_RESET_SECS` idle, rejects outlier intervals around the median, and
/// clamps the resulting tempo to `20..=300` BPM.
#[derive(Clone, Debug, Default)]
pub struct TapTempo {
    taps: Vec<f64>,
}

impl TapTempo {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reset(&mut self) {
        self.taps.clear();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tap_count(&self) -> usize {
        self.taps.len()
    }

    /// Record a tap at `now_secs` (monotonic seconds) and return the estimated BPM
    /// once at least two taps have been recorded in the active window.
    pub fn tap(&mut self, now_secs: f64) -> Option<f32> {
        if !now_secs.is_finite() {
            self.taps.clear();
            return None;
        }
        if let Some(&prev) = self.taps.last() {
            let dt = now_secs - prev;
            if dt < 0.0 || dt > TAP_RESET_SECS {
                self.taps.clear();
            } else if dt < 1e-6 {
                return None;
            }
        }
        self.taps.push(now_secs);
        if self.taps.len() > MAX_TAP_SAMPLES {
            let drop_n = self.taps.len() - MAX_TAP_SAMPLES;
            self.taps.drain(..drop_n);
        }
        if self.taps.len() < 2 {
            return None;
        }
        let mut intervals: Vec<f64> = self.taps.windows(2).map(|w| w[1] - w[0]).collect();
        let avg = if intervals.len() >= 3 {
            let mut sorted = intervals.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let median = sorted[sorted.len() / 2];
            let lo = median * 0.65;
            let hi = median * 1.5;
            let kept: Vec<f64> = intervals.drain(..).filter(|dt| *dt >= lo && *dt <= hi).collect();
            kept.iter().sum::<f64>() / kept.len().max(1) as f64
        } else {
            intervals.iter().sum::<f64>() / intervals.len() as f64
        };
        if avg <= 0.0 {
            return None;
        }
        Some((60.0 / avg).round().clamp(MIN_BPM as f64, MAX_BPM as f64) as f32)
    }
}

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

/// The next bar boundary strictly after `beats` (AUM's sync quantum, one
/// bar): the file player's play-enable waits for it while the transport
/// rolls. Beat 0 itself is not "next" — a start there begins immediately.
pub fn next_bar_beats(beats: f64) -> f64 {
    let bar = BEATS_PER_BAR as f64;
    (beats.max(0.0) / bar).floor() * bar + bar
}

/// Note-onset strength of a stereo-interleaved signal: the rising part of
/// its RMS envelope, one value per `ONSET_HOP` frames. The envelope at a hop
/// is the RMS of the `ONSET_WINDOW_HOPS` hops that end there, so a rise shows
/// at its own hop and the few after it, never before.
fn onset_strength(samples: &[f32]) -> Vec<f64> {
    let power: Vec<f64> = samples
        .chunks(ONSET_HOP * 2)
        .map(|c| c.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / c.len() as f64)
        .collect();
    let env: Vec<f64> = (0..power.len())
        .map(|i| {
            let window = &power[i.saturating_sub(ONSET_WINDOW_HOPS - 1)..=i];
            (window.iter().sum::<f64>() / window.len() as f64).sqrt()
        })
        .collect();
    env.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect()
}
const ONSET_HOP: usize = 64;
/// 21.3 ms: longer than a cycle of the held chord's lowest tone. An envelope
/// over one hop (1.3 ms) followed the waveform itself, so most "onsets" were
/// waveform cycles weighted by loudness, the gates' grid reached a coherence
/// of only 0.27, and a harmonic of the 16-step loudness pattern (2.48 Hz at
/// 120 BPM) came within 0.5 % of it: `transport-test` failed 4 of 74 runs
/// (#18). Over this window the grid reaches 0.7 and nothing below it 0.47 of
/// that (160 recorded real runs).
const ONSET_WINDOW_HOPS: usize = 16;

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

/// Rate (Hz) of the grid the onsets sit on, between 1 and 20 Hz: the lowest
/// rate whose coherence is a local peak within 10 % of the best. Not the
/// best itself, because onsets on a 4 Hz grid sit on 8 Hz too and either may
/// score higher.
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
/// `step_hz` steps per second: the rotation of the patch's sounding mask that
/// best covers the onset energy per step (silent steps have none). Returns
/// (step, share of onset energy on sounding steps for that rotation).
fn sequencer_step_at_start(onsets: &[f64], sample_rate: f64, step_hz: f64) -> (usize, f64) {
    let mut per_step = [0.0f64; 16];
    for (i, o) in onsets.iter().enumerate() {
        let step = ((i * ONSET_HOP) as f64 * step_hz / sample_rate) as usize;
        per_step[step % 16] += o;
    }
    let total: f64 = per_step.iter().sum::<f64>().max(f64::MIN_POSITIVE);
    (0..16)
        .map(|start| {
            let on_sounding: f64 = (0..16)
                .filter(|slot| GATE_CHORD_SOUNDING >> ((slot + start) % 16) & 1 == 1)
                .map(|slot| per_step[slot])
                .sum();
            (start, on_sounding / total)
        })
        .fold((0, 0.0), |best, r| if r.1 > best.1 { r } else { best })
}
/// Steps where "Gate Chord" produces onset energy: every step except 9, 10, 15.
/// `trigmask` (27135) also clears bit 12 (no envelope retrigger), but step 12
/// opens the filter (`s12 = 0.833333`) over step 11's envelope + 1/4-note
/// delay (`fx5`), so step 12 is loud; marking it silent let free-running
/// unison phase variance tip false rotations ahead of the true step.
const GATE_CHORD_SOUNDING: u32 = 0xffff & !((1 << 9) | (1 << 10) | (1 << 15));

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
        NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") },
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
    for (run, bpm) in [120.0f32, 60.0, 90.0, 120.0].into_iter().enumerate() {
        bus.send(Command::SetBpm { bpm });
        bus.send(Command::TransportPlay);
        let onsets = hold_note(&mut engine, &mut bus, 14.0);
        let hz = onset_grid_hz(&onsets, SR);
        bus.send(Command::TransportStop);
        let steps = hz * 60.0 / bpm as f64;
        // A failing check keeps its onsets; `KABELSALAT_ONSET_DUMP=<dir>`
        // saves every run for `grid_estimators_on_dumped_real_onsets`.
        let dump_dir = std::env::var_os("KABELSALAT_ONSET_DUMP")
            .map(std::path::PathBuf::from)
            .or_else(|| ((steps - 4.0).abs() >= 0.04).then(std::env::temp_dir));
        if let Some(dir) = dump_dir {
            let bytes: Vec<u8> = onsets.iter().flat_map(|o| o.to_le_bytes()).collect();
            let path = dir.join(format!("{}-{bpm}-{run}.f64", std::process::id()));
            if std::fs::write(&path, bytes).is_ok() {
                println!("transport-test: onsets saved to {}", path.display());
            }
        }
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
            "transport-test: note at engine beat {:.2} ({}) → Surge's sequencer on step {step:>2}, expected {want:>2} ({:.0}% of onset energy on its sounding steps)",
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

    // Count-in (1 bar at 120 BPM = 2.0 s = 200 blocks of 480 frames):
    // during all 200 blocks of the count-in, the voice clock stays stopped at
    // beat 0; at block 200 the song clock starts from beat 0 and Surge's
    // sequencer starts on step 0 (step 2 after 0.5 song beats).
    bus.send(Command::SetBpm { bpm: 120.0 });
    bus.send(Command::SetCountIn { enabled: true, bars: 1 });
    bus.send(Command::TransportPlay);
    let mut buf = [0.0f32; BLOCK * 2];
    for blk in 0..200 {
        engine.render(&mut buf, 2);
        let t = engine.transport();
        if blk < 199 {
            anyhow::ensure!(
                !t.playing && t.beats == 0.0,
                "count-in leaked song playback at block {blk}: {t:?}"
            );
        }
    }
    let after_count_in = engine.transport();
    anyhow::ensure!(
        after_count_in.playing && after_count_in.beats == 0.0,
        "song clock did not start at beat 0 after count-in: {after_count_in:?}"
    );
    let (step0, share0) = sequencer_step_at_start(&hold_note(&mut engine, &mut bus, 4.0), SR, STEP_HZ);
    println!(
        "transport-test: after 4-beat count-in at engine beat {:.2} (playing) → Surge's sequencer on step {step0:>2} ({:.0}% on sounding steps)",
        after_count_in.beats,
        share0 * 100.0
    );
    anyhow::ensure!(step0 == 0, "expected Surge on step 0 after count-in, got {step0}");
    bus.send(Command::TransportStop);
    bus.send(Command::TransportPlay);
    render(&mut engine, 2.25, None); // 2.0 s count-in + 0.25 s (0.5 beat) song
    let after_half = engine.transport();
    anyhow::ensure!(
        after_half.playing && (after_half.beats - 0.5).abs() < 1e-6,
        "expected beat 0.5 after count-in + 0.25s, got {after_half:?}"
    );
    let (step2, share2) = sequencer_step_at_start(&hold_note(&mut engine, &mut bus, 4.0), SR, STEP_HZ);
    println!(
        "transport-test: count-in + 0.50 song beats (playing) → Surge's sequencer on step {step2:>2} ({:.0}% on sounding steps)",
        share2 * 100.0
    );
    anyhow::ensure!(step2 == 2, "expected Surge on step 2 after count-in + 0.5 beat, got {step2}");
    bus.send(Command::TransportStop);
    bus.send(Command::SetCountIn { enabled: false, bars: 1 });

    println!("transport-test: OK — Surge follows the engine's tempo (4 steps per beat at 120/60/90 BPM), song position, play/stop, and count-in");
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Real onsets dumped by `KABELSALAT_ONSET_DUMP=<dir> transport-test`:
    /// the grid estimator against the step rate the file's BPM implies.
    #[test]
    #[ignore]
    fn grid_estimators_on_dumped_real_onsets() {
        let dir = std::env::var("KABELSALAT_ONSET_DUMP").expect("set KABELSALAT_ONSET_DUMP");
        let (mut n, mut wrong) = (0, 0);
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let bpm: f64 = p.file_name().unwrap().to_str().unwrap().split('-').nth(1).unwrap().parse().unwrap();
            let bytes = std::fs::read(&p).unwrap();
            let on: Vec<f64> = bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
            let want = bpm * 4.0 / 60.0;
            let got = onset_grid_hz(&on, SR);
            n += 1;
            if (got - want).abs() > 0.04 { wrong += 1; println!("wrong {}: {got} Hz, want {want}", p.display()); }
        }
        println!("dumps={n} wrong={wrong}");
        assert_eq!(wrong, 0);
    }

    use super::*;

    const SR: f64 = 48000.0;

    /// A low chord gated on an 8 Hz grid with Gate Chord's step pattern:
    /// the onsets are the gates, not the cycles of the chord's waveform (#18).
    #[test]
    fn onsets_of_a_gated_low_chord_sit_on_the_gate_grid() {
        const STEP_HZ: f64 = 8.0;
        let mut samples = Vec::new();
        for i in 0..(14.0 * SR) as usize {
            let t = i as f64 / SR;
            let step = (t * STEP_HZ) as usize;
            let in_step = t - step as f64 / STEP_HZ;
            let gate = if GATE_CHORD_SOUNDING >> (step % 16) & 1 == 1 {
                (in_step / 0.002).min(1.0) * (-in_step * 12.0).exp() * (0.5 + 0.5 * ((step * 7) % 16) as f64 / 16.0)
            } else {
                0.0
            };
            let chord: f64 = [130.81, 131.4, 164.81, 165.5, 196.0, 196.9]
                .iter()
                .map(|hz| (std::f64::consts::TAU * hz * t).sin())
                .sum();
            let x = (gate * chord / 6.0) as f32;
            samples.extend([x, x]);
        }
        let onsets = onset_strength(&samples);
        let on_grid = grid_coherence(&onsets, SR, STEP_HZ);
        let (rival, rival_hz) = (0..1370)
            .map(|i| 1.0 + i as f64 * 0.005)
            .map(|hz| (grid_coherence(&onsets, SR, hz), hz))
            .fold((0.0, 0.0), |a, b| if b.0 > a.0 { b } else { a });
        assert!(
            rival < 0.6 * on_grid,
            "the 8 Hz gate grid scores {on_grid:.3}, {rival_hz:.3} Hz below it {rival:.3}"
        );
        let hz = onset_grid_hz(&onsets, SR);
        assert!((hz - STEP_HZ).abs() < 0.04, "grid read as {hz:.3} Hz");
    }

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

    #[test]
    fn next_bar_is_strictly_ahead() {
        assert_eq!(next_bar_beats(0.0), 4.0);
        assert_eq!(next_bar_beats(3.999), 4.0);
        assert_eq!(next_bar_beats(4.0), 8.0);
        assert_eq!(next_bar_beats(12.5), 16.0);
        assert_eq!(next_bar_beats(-0.5), 4.0, "never played: the first downbeat");
    }

    #[test]
    fn tap_tempo_steady_and_jitter() {
        let mut tap = TapTempo::new();
        assert_eq!(tap.tap(0.0), None);
        assert_eq!(tap.tap(0.5), Some(120.0));
        assert_eq!(tap.tap(1.0), Some(120.0));
        assert_eq!(tap.tap(1.5), Some(120.0));

        // Slight human jitter around 100 BPM (0.600 s/beat) averages within ±1.
        let mut jitter = TapTempo::new();
        let times = [0.0, 0.612, 1.190, 1.805, 2.398];
        let mut last = None;
        for t in times {
            last = jitter.tap(t);
        }
        let bpm = last.expect("estimated BPM");
        assert!((bpm - 100.0).abs() <= 1.0, "expected ~100 BPM, got {bpm}");
    }

    #[test]
    fn tap_tempo_resets_after_idle() {
        let mut tap = TapTempo::new();
        assert_eq!(tap.tap(0.0), None);
        assert_eq!(tap.tap(0.5), Some(120.0));
        // Idle > 2.0 s resets history; first tap after pause returns None.
        assert_eq!(tap.tap(2.6), None);
        assert_eq!(tap.tap_count(), 1);
        // Subsequent tap uses the new tempo (90 BPM = 0.6667 s).
        assert_eq!(tap.tap(2.6 + 60.0 / 90.0), Some(90.0));
    }

    #[test]
    fn tap_tempo_ignores_outliers() {
        let mut tap = TapTempo::new();
        // 120 BPM steady (0.5 s) with one skipped beat (1.0 s gap) -> median 0.5 rejects 1.0.
        for t in [0.0, 0.5, 1.0, 2.0, 2.5, 3.0] {
            let _ = tap.tap(t);
        }
        assert_eq!(tap.tap(3.5), Some(120.0));

        // Fast accidental double-tap (0.08 s) inside a 100 BPM (0.6 s) sequence.
        let mut bounce = TapTempo::new();
        for t in [0.0, 0.6, 1.2, 1.28, 1.8, 2.4] {
            let _ = bounce.tap(t);
        }
        let bpm = bounce.tap(3.0).unwrap();
        assert!((bpm - 100.0).abs() <= 3.0, "outlier should not yank tempo, got {bpm}");
    }

    #[test]
    fn tap_tempo_clamps_range() {
        let mut tap = TapTempo::new();
        assert_eq!(tap.tap(0.0), None);
        // 0.05 s interval = 1200 BPM -> clamped to 300.
        assert_eq!(tap.tap(0.05), Some(300.0));

        // Max non-reset interval is 2.0 s -> 30 BPM; verify clamping helper covers 20..=300.
        let mut slow = TapTempo::new();
        assert_eq!(slow.tap(0.0), None);
        assert_eq!(slow.tap(2.0), Some(30.0));
        assert_eq!((60.0f64 / 4.0).round().clamp(MIN_BPM as f64, MAX_BPM as f64), 20.0);
    }

    #[test]
    fn count_in_config_and_master_clock_beats() {
        let def = CountInConfig::default();
        assert!(!def.enabled);
        assert_eq!(def.bars, 1);
        assert_eq!(def.total_beats(), 0.0);

        let one = CountInConfig::normalized(true, 1);
        assert_eq!(one.total_beats(), 4.0);
        assert_eq!(CountInConfig::normalized(true, 9).bars, MAX_COUNT_IN_BARS);

        let c = MasterClock::CountingIn { remaining_beats: 4.0, total_beats: 4.0 };
        assert_eq!(c.count_in_beat(), 1);
        let c2 = MasterClock::CountingIn { remaining_beats: 2.1, total_beats: 4.0 };
        assert_eq!(c2.count_in_beat(), 2);
        let c4 = MasterClock::CountingIn { remaining_beats: 0.2, total_beats: 4.0 };
        assert_eq!(c4.count_in_beat(), 4);
        assert_eq!(MasterClock::Playing.count_in_beat(), 0);
    }

    #[test]
    fn gate_chord_sounding_mask_separates_every_rotation() {
        // Observed step-energy profile of Gate Chord (step 12 is loud; 9, 10, 15 are silent).
        let profile = [
            0.05, 0.04, 0.10, 0.04, 0.11, 0.04, 0.05, 0.11,
            0.04, 0.007, 0.013, 0.13, 0.08, 0.04, 0.08, 0.018,
        ];
        let step_hz = 8.0;
        let hops = (16.0 * SR / (step_hz * ONSET_HOP as f64)).ceil() as usize;
        for start in 0..16 {
            let mut onsets = vec![0.0; hops];
            for slot in 0..16 {
                let idx = ((slot as f64 + 0.5) * SR / (step_hz * ONSET_HOP as f64)) as usize;
                onsets[idx] = profile[(slot + start) % 16];
            }
            let (got, share) = sequencer_step_at_start(&onsets, SR, step_hz);
            assert_eq!(got, start, "rotation {start} misidentified as {got}");
            assert!(share > 0.94, "expected >94% on sounding steps, got {share}");
        }
    }
}
