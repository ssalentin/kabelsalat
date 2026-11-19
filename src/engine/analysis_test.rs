//! `kabelsalat analysis-test [--save <dir>]` (issue #13): checks the engine
//! by what it sounds like, through a real `GraphEngine` with built-in
//! sources only: a note's pitch, brightness and aliasing, the master's
//! crackle regression, routing proven with noise, and delay compensation.
//! Real Surge XT adds a played note when it is installed. What stays a
//! matter of ear gets a "listen for" line, and `--save` writes those clips.

use super::probe::{self, Crackle, Probe, Tap, SR};
use crate::analysis::{self, Spectrum};
use crate::control::{name_bytes, Command, DspKind, Graph, NodeKind};
use std::path::Path;

/// A clip worth hearing: its file name says what to listen for.
struct Clip {
    file: &'static str,
    listen: &'static str,
    /// Stereo interleaved.
    samples: Vec<f32>,
}

fn stereo(mono: &[f32]) -> Vec<f32> {
    mono.iter().flat_map(|s| [*s, *s]).collect()
}

/// A Metronome → Master rack started: the first click, master left.
fn click() -> Vec<f32> {
    let mut g = Graph::default();
    let metro = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("CLICK"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    probe::wire(&mut g, 1, metro, master, 0);
    probe::wire(&mut g, 2, metro, master, 1);
    let mut p = Probe::new(g);
    p.send(Command::SetNodePlaying { node: metro, playing: true });
    let mut l = p.run(10, &[Tap::MasterL]).taps.remove(0).1;
    l.truncate(super::CLICK_FRAMES);
    l
}

/// One second of `signal` through Audio In → `unit` (if any) → Master,
/// master left, after a settle.
fn through(unit: Option<(DspKind, f32)>, signal: impl Iterator<Item = f32> + 'static) -> Vec<f32> {
    let mut g = Graph::default();
    let ain = g.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::Stereo }, name_bytes("IN"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let mut last = ain;
    let mut knob = None;
    if let Some((kind, value)) = unit {
        last = g.add_node(NodeKind::Dsp(kind), name_bytes("UNIT"));
        probe::wire(&mut g, 1, ain, last, 0);
        knob = Some((last, value));
    }
    probe::wire(&mut g, 2, last, master, 0);
    probe::wire(&mut g, 3, last, master, 1);
    let mut p = Probe::new(g);
    if let Some((node, value)) = knob {
        p.send(Command::SetParam { node, param: 0, value: value as f64 });
    }
    p.feed("signal", ain, signal);
    p.settle(probe::SETTLE_BLOCKS);
    p.run(SR as usize / crate::bridge::BUFFER_FRAMES as usize, &[Tap::MasterL]).taps.remove(0).1
}

fn sine(hz: f64, amp: f32) -> impl Iterator<Item = f32> {
    (0u64..).map(move |i| (std::f64::consts::TAU * hz * i as f64 / SR as f64).sin() as f32 * amp)
}

fn spectra(clips: &mut Vec<Clip>) -> anyhow::Result<()> {
    println!("spectra — built-in units through the engine:");
    let c = click();
    let s = Spectrum::of(&c, SR);
    let pitch = s.fundamental_hz().unwrap_or(0.0);
    println!("  metronome click: pitch {pitch:.2} Hz (its tone is 1000 Hz), centroid {:.0} Hz", s.centroid_hz());
    anyhow::ensure!((pitch - 1000.0).abs() < 1.0, "the metronome click measures {pitch:.2} Hz, not 1000 Hz");
    clips.push(Clip { file: "click-1khz", listen: "a clean 1 kHz tick, no thump or buzz", samples: stereo(&c) });

    for (hz, amp) in [(440.0f64, 0.3f32), (3000.0, 0.9)] {
        let out = through(None, sine(hz, amp));
        let s = Spectrum::of(&out, SR);
        let pitch = s.fundamental_hz().unwrap_or(0.0);
        let off = s.inharmonic_share(pitch);
        println!("  sine {hz} Hz at {amp} through the master: pitch {pitch:.2} Hz, inharmonic power {off:.2e}, peak {:.3}", analysis::peak(&out));
        anyhow::ensure!((pitch as f64 - hz).abs() < 1.0, "a {hz} Hz sine leaves the master at {pitch:.2} Hz");
        anyhow::ensure!(off < 1e-3, "the master adds inharmonic power (aliasing) to a {hz} Hz sine: {off:.2e}");
    }

    let dry = through(Some((DspKind::Gain, 0.5)), probe::noise(3));
    let dull = through(Some((DspKind::Filter, 0.5)), probe::noise(3));
    let (bright, dark) = (Spectrum::of(&dry, SR).centroid_hz(), Spectrum::of(&dull, SR).centroid_hz());
    println!("  white noise: centroid {bright:.0} Hz dry, {dark:.0} Hz through a Filter at CUTOFF 50 % (632 Hz)");
    anyhow::ensure!(dark < bright / 3.0, "the Filter does not darken noise: centroid {dark:.0} Hz vs {bright:.0} Hz dry");
    clips.push(Clip { file: "noise-filtered", listen: "a dull hiss, its top cut off", samples: stereo(&dull) });
    Ok(())
}

fn crackle(clips: &mut Vec<Clip>) -> anyhow::Result<()> {
    println!("master crackle (rules 49, 62, 120) — 16-note chord at the default volume and WARM:");
    let loud = probe::chord_through_master(0.25);
    let quiet = probe::chord_through_master(0.01);
    let clipped = probe::hard_clipped(&loud);
    let (l, q, c) = (Crackle::of(&loud), Crackle::of(&quiet), Crackle::of(&clipped));
    for (name, x, m) in [("loud", &loud, l), ("quiet", &quiet, q), ("hard-clipped copy", &clipped, c)] {
        println!(
            "  {name:>17}: peak {:.3}, samples at the knee ({:.3}) {:.2} %, longest flat top {} samples, power above 8 kHz {:.1} dB",
            analysis::peak(x), probe::MASTER_KNEE, m.at_knee * 100.0, m.flat_top, 10.0 * m.above_8k.max(1e-30).log10()
        );
    }
    for (name, m) in [("loud", l), ("quiet", q)] {
        anyhow::ensure!(m.at_knee == 0.0 && m.flat_top < probe::FLAT_TOP_MAX, "the {name} chord flattens at the master: {m:?}");
        anyhow::ensure!(m.above_8k < probe::HF_MAX, "the {name} chord has high end above −60 dB (crackle): {m:?}");
    }
    anyhow::ensure!(
        c.at_knee > 0.05 && c.flat_top > probe::FLAT_TOP_MAX && c.above_8k > probe::HF_MAX,
        "the measures do not flag a hard-clipped chord: {c:?}"
    );
    clips.push(Clip { file: "chord-loud", listen: "the loud chord sounds like the quiet one turned up: no crackle or fizz on its attacks", samples: stereo(&loud) });
    clips.push(Clip { file: "chord-quiet", listen: "the same chord played quietly, the reference for chord-loud", samples: stereo(&quiet) });
    clips.push(Clip { file: "chord-hard-clipped", listen: "what crackle sounds like: the loud chord through a hard clipper", samples: stereo(&clipped) });
    Ok(())
}

fn routing() -> anyhow::Result<()> {
    println!("routing — white noise into In A (rig A), pink into In B (rig B), correlation at each tap:");
    let mut rigs = probe::TwoRigs::new();
    for (step, cmds, a, b, sent) in rigs.steps() {
        for c in cmds {
            rigs.probe.send(c);
        }
        let rows = rigs.rows(a, b, sent);
        match rigs.probe.check(&rows) {
            Ok(p) => println!("  {step}:\n{}", indent(&p.to_string())),
            Err(e) => anyhow::bail!("routing, {step}: {e}"),
        }
    }
    Ok(())
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

fn compensation(clips: &mut Vec<Clip>) -> anyhow::Result<()> {
    use crate::dsp::Processor;
    let latency = crate::dsp::Limiter::new(SR as f64).latency_frames();
    println!("delay compensation — Audio In → Pan L → Master beside Audio In → <units> → Pan R → Master (Limiter lookahead {latency} frames):");
    let cases: [(&str, &[DspKind], bool); 3] = [
        ("Limiter", &[DspKind::Limiter], false),
        ("Limiter → Gain → Limiter", &[DspKind::Limiter, DspKind::Gain, DspKind::Limiter], false),
        ("Limiter, bypassed", &[DspKind::Limiter], true),
    ];
    for (name, chain, bypass) in cases {
        let (mut p, units) = probe::split_paths(chain);
        if bypass {
            p.send(Command::SetStripBypass { node: units[0], bypass: true });
        }
        let x = probe::lag_r_behind_l(&mut p);
        println!("  {name:>25}: R is {} frames behind L, r {:.3}", x.lag_frames, x.r);
        anyhow::ensure!(x.lag_frames == 0 && x.r > 0.95, "{name}: the parallel paths are {} frames apart (r {:.3})", x.lag_frames, x.r);
        if !bypass && chain.len() == 1 {
            let run = p.run(SR as usize / crate::bridge::BUFFER_FRAMES as usize, &[]);
            clips.push(Clip { file: "noise-dry-left-limited-right", listen: "the noise sits in the centre; paths 5 ms apart would pull it to the left", samples: run.master });
        }
    }
    Ok(())
}

/// Real Surge XT: middle C on its default patch.
fn surge(clips: &mut Vec<Clip>) -> anyhow::Result<()> {
    use crate::control::{PortKind, PortRef};
    use crate::matrix::SourceId;
    let Ok((voice, _main)) = crate::host::ClapVoice::new(SR as f64) else {
        println!("surge note: SKIP — Surge XT is not installed");
        return Ok(());
    };
    let (mut bus, drain) = crate::control::command_bus();
    let mut g = Graph::default();
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    probe::wire(&mut g, 2, synth, master, 0);
    probe::wire(&mut g, 3, synth, master, 1);
    let mut engine = super::GraphEngine::new(g, drain, SR as f64, vec![(synth, voice)]);
    bus.send(Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys });
    let mut buf = vec![0.0f32; 256 * 2];
    let mut out = Vec::new();
    for _ in 0..(SR as usize * 6 / 5 / 256) {
        engine.render(&mut buf, 2);
        out.extend_from_slice(&buf);
    }
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys });
    let l: Vec<f32> = out.iter().step_by(2).copied().skip(SR as usize / 5).collect();
    let s = Spectrum::of(&l, SR);
    let want = probe::midi_hz(60) as f32;
    let pitch = s.fundamental_hz().unwrap_or(0.0);
    let (centroid, off) = (s.centroid_hz(), s.inharmonic_share(pitch));
    println!("surge note: middle C, pitch {pitch:.2} Hz ({want:.2}), centroid {centroid:.0} Hz, inharmonic power {off:.2e}, peak {:.3}", analysis::peak(&l));
    anyhow::ensure!((pitch - want).abs() < 1.0, "Surge's middle C measures {pitch:.2} Hz, not {want:.2} Hz");
    anyhow::ensure!(centroid > 2.0 * want, "Surge's default saw is dull: centroid {centroid:.0} Hz");
    anyhow::ensure!(off < 0.05, "Surge's middle C has {off:.2e} of its power off its harmonics (aliasing)");
    clips.push(Clip { file: "surge-middle-c", listen: "a steady saw at middle C, no whistling tones moving against it (aliasing)", samples: out });
    Ok(())
}

/// Write each clip as WAV, named with what to listen for, and read it back
/// with the app's own reader (rule 54).
fn save(dir: &Path, clips: &[Clip]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (i, c) in clips.iter().enumerate() {
        let slug: String = c.listen.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' }).collect();
        let slug: String = slug.split('-').filter(|w| !w.is_empty()).take(10).collect::<Vec<_>>().join("-");
        let path = dir.join(format!("{}-{}--listen-for-{slug}.wav", i + 1, c.file));
        let clip = crate::listen::Clip { sample_rate: SR as u32, channels: 2, samples: c.samples.clone() };
        std::fs::write(&path, clip.wav())?;
        let back = crate::player::load_file(&path)?;
        // 16-bit PCM: the writer scales by 32767, the reader by 32768.
        let worst = back.samples.iter().zip(&c.samples).fold(0.0f32, |m, (a, b)| m.max((a - b.clamp(-1.0, 1.0)).abs()));
        anyhow::ensure!(
            back.sample_rate == SR as u32 && back.samples.len() == c.samples.len() && worst <= 2.0 / 32768.0,
            "{} does not read back as written (worst sample off by {worst})",
            path.display()
        );
        println!("  wrote {}", path.display());
    }
    Ok(())
}

pub fn run(save_dir: Option<&Path>) -> anyhow::Result<()> {
    let mut clips = Vec::new();
    spectra(&mut clips)?;
    crackle(&mut clips)?;
    routing()?;
    compensation(&mut clips)?;
    surge(&mut clips)?;
    println!("by ear:");
    for c in &clips {
        println!("  listen for: {} ({})", c.listen, c.file);
    }
    if let Some(dir) = save_dir {
        save(dir, &clips)?;
    }
    println!("analysis-test: OK");
    Ok(())
}
