//! First run (#287): sound within a minute, without a terminal.
//!
//! The pure part says, in one plain sentence, what is wrong with a first
//! start (no output, a large buffer, no realtime rights) and always offers
//! the Audio Doctor. `ui/firstrun.rs` gathers the facts off the UI thread and
//! shows the sentence; `run_test` is the scripted first start.

use std::time::{Duration, Instant};

/// A buffer longer than this (ms) makes keys answer late.
pub const LARGE_BUFFER_MS: f32 = 25.0;

/// Where the Audio Doctor is, in the words of every sentence.
const DOCTOR: &str = "Open the Audio Doctor (Settings → Audio)";

/// What the app knows about its sound after the first seconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    /// Why no output stream is open; `None` while one is.
    pub no_output: Option<String>,
    /// Length of the output buffer, once a stream is open.
    pub buffer_ms: Option<f32>,
    /// The audio thread's realtime warning (`audio_doctor::startup_rt_warning`).
    pub realtime_warning: Option<String>,
}

/// The one sentence a first start says, worst problem first; `None` when
/// everything is fine.
pub fn sentence(f: &Facts) -> Option<String> {
    if let Some(why) = &f.no_output {
        return Some(format!("No sound output was found ({}), so kabelsalat is silent. {DOCTOR} to see why.", why.trim_end_matches('.')));
    }
    if let Some(ms) = f.buffer_ms.filter(|ms| *ms > LARGE_BUFFER_MS) {
        return Some(format!("The audio buffer is large ({ms:.0} ms), so keys will answer late. {DOCTOR} to fix it."));
    }
    if f.realtime_warning.is_some() {
        return Some(format!("The audio thread has no realtime priority, so sound may crackle. {DOCTOR} to fix it."));
    }
    None
}

/// The sentence for a stand-in start: Surge XT is missing and the factory
/// piano plays.
pub fn factory_sentence() -> &'static str {
    "No synth plugin is installed, so the factory piano plays. Plugins → Install adds more."
}

/// `kabelsalat first-run-test`: a scripted first start in an isolated home,
/// no plugin anywhere. With PipeWire the real output would be tested too;
/// without it (or in CI) the offline engine stands in. Output must come
/// within 60 seconds.
pub fn run_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, Command, Graph, NodeKind};
    use crate::engine::{GraphEngine, GraphPub, VoiceBack, VoiceMsg, VoiceSlot};
    use crate::matrix::SourceId;
    use std::sync::{Arc, Mutex};
    println!("=== first-run-test (isolated home, no plugin, no config) ===");
    let t0 = Instant::now();
    let home = crate::catalog::home_dir().join(".cache").join(format!("ksalat-firstrun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home)?;
    // Isolated: nothing of the user's config, plugins or sessions is read.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("CLAP_PATH", home.join("none"));
    std::env::set_var("LV2_PATH", home.join("none"));
    anyhow::ensure!(crate::assistant::load_config_str("first_run_done").is_none(), "an isolated home is a first run");
    let pw = std::process::Command::new("pactl").arg("info").output().map(|o| o.status.success()).unwrap_or(false);
    println!("  PipeWire here: {} ({})", pw, if pw { "the offline engine is checked, the real stream is the bench test" } else { "offline engine stands in" });

    // What the app does at start.
    let sr = 48_000.0;
    anyhow::ensure!(!crate::host::surge_available(), "Surge XT is found in an isolated home");
    crate::factory::set_fallback(true);
    let graph = Graph::demo_patch();
    let mut voices = Vec::new();
    for n in graph.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Clap { .. })) {
        let id = n.kind.voice_plugin_key().unwrap_or_default();
        let kind = crate::factory::kind_of(&id).ok_or_else(|| anyhow::anyhow!("start-up rack names {id}, not a factory sound"))?;
        anyhow::ensure!(crate::factory::wait_ready(kind, Duration::from_secs(30)), "{} not decoded", kind.name());
        voices.push(crate::host::ClapVoice::new_plugin(sr, &id)?);
    }
    let (mut bus, drain) = command_bus();
    let mut engine = GraphEngine::new(graph.clone(), drain, sr, vec![]);
    engine.set_graph_pub(Arc::new(Mutex::new(GraphPub { graph, ..Default::default() })));
    let (mut tx, rx) = rtrb::RingBuffer::<VoiceMsg>::new(16);
    let (back_tx, _back) = rtrb::RingBuffer::<VoiceBack>::new(16);
    engine.set_voice_channels(rx, back_tx);
    for (v, _main) in voices {
        tx.push(VoiceMsg::Add(VoiceSlot::new(v))).map_err(|_| anyhow::anyhow!("voice ring full"))?;
    }
    let mut buf = vec![0.0f32; 256 * 2];
    engine.render(&mut buf, 2);
    // The first test note (the app plays it at its first start) and a key.
    bus.send(Command::NoteOn { note: 72, velocity: 0.45, source: SourceId::Keys, channel: 0 });
    let mut peak = 0.0f32;
    for _ in 0..120 {
        engine.render(&mut buf, 2);
        peak = peak.max(buf.iter().fold(0.0f32, |m, x| m.max(x.abs())));
    }
    let took = t0.elapsed();
    println!("  first note after {:.1} s: peak {peak:.3}", took.as_secs_f32());
    anyhow::ensure!(peak > 0.03, "the first start is silent (peak {peak})");
    anyhow::ensure!(took < Duration::from_secs(60), "the first note came after {took:?}");
    let msg = sentence(&Facts::default());
    anyhow::ensure!(msg.is_none(), "a healthy start says nothing: {msg:?}");
    let _ = std::fs::remove_dir_all(&home);
    println!("first-run-test: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_start_says_nothing_and_each_problem_offers_the_audio_doctor() {
        assert_eq!(sentence(&Facts::default()), None);
        let cases = [
            Facts { no_output: Some("no device".into()), ..Default::default() },
            Facts { buffer_ms: Some(85.0), ..Default::default() },
            Facts { realtime_warning: Some("x".into()), ..Default::default() },
        ];
        for f in cases {
            let s = sentence(&f).expect("a problem is told");
            assert!(s.contains("Audio Doctor"), "{s}");
            assert!(s.ends_with('.') && !s.contains("  "), "{s}");
        }
    }

    #[test]
    fn no_output_comes_before_the_rest_and_a_small_buffer_is_fine() {
        let f = Facts { no_output: Some("nothing".into()), buffer_ms: Some(90.0), realtime_warning: Some("x".into()) };
        assert!(sentence(&f).unwrap().starts_with("No sound output"));
        assert_eq!(sentence(&Facts { buffer_ms: Some(5.3), ..Default::default() }), None);
    }
}
