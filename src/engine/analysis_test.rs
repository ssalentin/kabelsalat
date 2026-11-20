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
    // The first click of a started metronome is the bar's accent (#87).
    let tone = super::CLICK_ACCENT_HZ;
    println!("  metronome click: pitch {pitch:.2} Hz (the bar accent's tone is {tone} Hz), centroid {:.0} Hz", s.centroid_hz());
    anyhow::ensure!((pitch - tone).abs() < 1.0, "the metronome's first click measures {pitch:.2} Hz, not {tone} Hz");
    clips.push(Clip { file: "click-accent", listen: "a clean 1.5 kHz tick, no thump or buzz", samples: stereo(&c) });

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
    hosted_latency()
}

/// A hosted plugin's reported latency (#76): the latent test plugin, an LV2
/// unit that delays by its Delay control and reports that as its latency,
/// loaded the way the app loads plugins. Its latency is changed while it
/// runs, as a look-ahead knob would.
fn hosted_latency() -> anyhow::Result<()> {
    use super::latent;
    latent::install()?;
    println!("  hosted LV2 plugin reporting its latency ({}), in place of the units:", latent::URI);
    let (mut p, units) = probe::split_paths_of(&[latent::kind()]);
    let input = p.engine.graph().nodes[0].id;
    for frames in [latent::DEFAULT_FRAMES, 1000, 37, 0, 4096] {
        p.send(Command::SetParam { node: units[0], param: latent::P_DELAY, value: latent::delay_value(frames) });
        let x = probe::lag_r_behind_l(&mut p);
        let strip = p.engine.pdc.late().iter().find(|l| l.0 == input).map_or(0, |l| l.1);
        println!("  {:>25}: R is {} frames behind L, r {:.3}; the input's strip shows {strip} frames", format!("reports {frames} frames"), x.lag_frames, x.r);
        anyhow::ensure!(x.lag_frames == 0 && x.r > 0.95, "a plugin reporting {frames} frames: the parallel paths are {} frames apart (r {:.3})", x.lag_frames, x.r);
        anyhow::ensure!(strip as usize == frames, "a plugin reporting {frames} frames: the strip shows {strip}");
    }
    // And in an aux bus's chain: dry rig + bus return stay in phase as the bus plugin's latency moves.
    let mut g = Graph::default();
    let ain = g.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::Stereo }, name_bytes("IN"));
    let bus = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("BUS"));
    let fx = g.add_node(latent::kind(), name_bytes("BUS LATENT"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    probe::wire(&mut g, 1, ain, master, 0);
    probe::wire(&mut g, 2, ain, master, 1);
    probe::wire(&mut g, 3, bus, fx, 0);
    probe::wire(&mut g, 4, fx, master, 0);
    probe::wire(&mut g, 5, fx, master, 1);
    let mut p = probe::Probe::hosting(g);
    p.send(Command::SetSend { from: crate::control::SendSource::Rig(0), bus, level: Some(1.0), pre: Some(false) });
    p.feed("noise", ain, analysis::white(11).map(|x| x * 0.1));
    for frames in [latent::DEFAULT_FRAMES, 1000] {
        p.send(Command::SetParam { node: fx, param: latent::P_DELAY, value: latent::delay_value(frames) });
        let settle = probe::SETTLE_BLOCKS.max(frames / crate::bridge::BUFFER_FRAMES as usize + 4);
        p.settle(settle);
        let win = 32 * crate::bridge::BUFFER_FRAMES as usize;
        let blocks = 32 + frames / crate::bridge::BUFFER_FRAMES as usize + 1;
        let run = p.run(blocks, &[probe::Tap::MasterL]);
        let x = analysis::xcorr_peak(&run.feeds[0].1[..win], &run.taps[0].1[frames..frames + win], 600);
        println!("  {:>25}: rig+bus return off by {} frames at lag {frames}, r {:.3}", format!("bus plugin {frames} frames"), x.lag_frames, x.r);
        anyhow::ensure!(x.lag_frames == 0 && x.r > 0.95, "bus plugin at {frames} frames: off by {} (r {:.3})", x.lag_frames, x.r);
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
    let synth = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
    probe::wire(&mut g, 2, synth, master, 0);
    probe::wire(&mut g, 3, synth, master, 1);
    let mut engine = super::GraphEngine::new(g, drain, SR as f64, vec![(synth, voice)]);
    bus.send(Command::NoteOn { note: 60, velocity: 0.8, source: SourceId::Keys, channel: 0 });
    let mut buf = vec![0.0f32; 256 * 2];
    let mut out = Vec::new();
    for _ in 0..(SR as usize * 6 / 5 / 256) {
        engine.render(&mut buf, 2);
        out.extend_from_slice(&buf);
    }
    bus.send(Command::NoteOff { note: 60, source: SourceId::Keys, channel: 0 });
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

/// Real CLAP audio effect (`Dragonfly Hall Reverb` or `Surge XT Effects`)
/// wired `Audio In → CLAP FX → Master`: proves audio passes through a hosted
/// CLAP effect and is changed by it (#181).
fn clap_effect(clips: &mut Vec<Clip>) -> anyhow::Result<()> {
    use crate::control::{AudioInChannel, ClapId, PortKind, PortRef};
    let candidates = [
        ("michaelwillis.dragonfly.hall", "DragonflyHallReverb.clap"),
        ("org.surge-synth-team.surge-xt-fx", "Surge XT Effects.clap"),
    ];
    let mut loaded = None;
    for (id, bundle_name) in candidates {
        if let Ok((v, m)) = crate::host::ClapVoice::new_plugin(SR as f64, id) {
            loaded = Some((id, v, m));
            break;
        }
        for root in [
            "/var/lib/openbot/.cache/ksalat-clapfx/root/usr/lib/clap",
            "/var/lib/openbot/.cache/ksalat-issue29/root/usr/lib/clap",
            "/usr/lib/clap",
        ] {
            let path = Path::new(root).join(bundle_name);
            if path.exists() {
                if let Ok((v, m)) = crate::host::ClapVoice::new_from_bundle(SR as f64, &path, id) {
                    loaded = Some((id, v, m));
                    break;
                }
            }
        }
        if loaded.is_some() {
            break;
        }
    }
    let Some((plugin_id, voice, main)) = loaded else {
        println!("clap effect: SKIP — neither Dragonfly Hall Reverb nor Surge XT Effects is installed");
        return Ok(());
    };
    anyhow::ensure!(voice.is_effect(), "{plugin_id} should classify as a CLAP effect");

    let mut g = Graph::default();
    let src = g.add_node(
        NodeKind::AudioIn { device: None, channels: AudioInChannel::Stereo },
        name_bytes("IN"),
    );
    let fx = g.add_node(
        NodeKind::Clap { plugin_id: ClapId::new(plugin_id) },
        name_bytes("CLAP FX"),
    );
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    g.connect_with_id(1, PortRef { node: src, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Audio);
    probe::wire(&mut g, 2, fx, master, 0);
    probe::wire(&mut g, 3, fx, master, 1);

    let mut p = probe::Probe::with_voices(g, vec![(fx, voice)]);
    // 8 blocks (~42 ms) of a 440 Hz tone burst into the CLAP effect, then 40 blocks of silence.
    let burst_frames = 8 * crate::bridge::BUFFER_FRAMES as usize;
    p.feed(
        "burst",
        src,
        (0..burst_frames)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * (i as f32 / SR)).sin())
            .chain(std::iter::repeat(0.0)),
    );
    let burst = p.run(8, &[probe::Tap::Node(fx), probe::Tap::MasterL]);
    let tail = p.run(40, &[probe::Tap::Node(fx), probe::Tap::MasterL]);

    let rms = |xs: &[f32]| -> f32 {
        if xs.is_empty() {
            return 0.0;
        }
        (xs.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / xs.len() as f64).sqrt() as f32
    };
    let dry = &burst.feeds[0].1;
    // Tap::Node is L + R; scale by 0.5 to compare against the per-channel dry signal.
    let wet: Vec<f32> = burst.taps[0].1.iter().map(|s| 0.5 * *s).collect();
    let tail_l: Vec<f32> = tail.taps[0].1.iter().map(|s| 0.5 * *s).collect();
    let burst_peak = analysis::peak(&wet);
    let tail_rms = rms(&tail_l);
    let diff_rms = rms(
        &dry.iter().zip(wet.iter()).map(|(a, b)| a - b).collect::<Vec<f32>>(),
    );
    println!(
        "clap effect ({plugin_id}): burst peak {burst_peak:.3}, diff vs dry rms {diff_rms:.4}, reverb tail rms {tail_rms:.4}"
    );
    anyhow::ensure!(burst_peak > 0.01, "CLAP effect output was silent (peak {burst_peak})");
    anyhow::ensure!(
        diff_rms > 0.01 || tail_rms > 1e-4,
        "CLAP effect did not alter the audio (diff_rms {diff_rms}, tail_rms {tail_rms})"
    );
    let mut samples = burst.master;
    samples.extend_from_slice(&tail.master);
    clips.push(Clip {
        file: "clap-effect-burst",
        listen: "a short 440 Hz tone burst through a hosted CLAP effect with an audible reverb tail",
        samples,
    });
    if let Some(slot) = std::mem::take(&mut p.engine.voices).into_iter().next() {
        main.retire(slot.into_voice());
    }
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
    clap_effect(&mut clips)?;
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
