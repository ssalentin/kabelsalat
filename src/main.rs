#![recursion_limit = "512"]
//! kabelsalat — live audio/MIDI wiring, alive.
//!
//! P0 spike entry point. Each subsystem has a standalone test mode so the team
//! can develop and verify them in parallel; integration happens in `main` once
//! the spikes pass their go/no-go checks.

/// Release version and the commit it was built from, e.g. "0.1.0 (abc1234)".
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("KABELSALAT_COMMIT"), ")");

mod aeolus;
mod assistant;
mod arp;
mod audio;
mod audio_in;
mod audio_out;
mod bridge;
mod capture;
mod catalog;
mod control;
mod dsp;
mod engine;
mod engine_info;
mod guitar;
mod host;
mod juce;
mod listen;
mod llm;
mod lv2;
mod main_controls;
mod matrix;
mod mcp;
mod organ;
mod instruments;
mod meter;
mod midi;
mod player;
mod plugin_window;
mod preview;
mod record;
mod rope;
mod session;
mod sounds;
mod theme;
mod transport;
mod ui;
mod vital;

const MODES: &str = "app, preview, audio-test, audio-in-test, monitor-latency-test, guitar-rig-test, midi-test, host-test, ui-demo, mcp [--attach], send, agent-eval [--model <id>] [--runs N] [--task <name>] [--no-jev], jev-test, listen-test [--model <id>] [--runs N] [--audio-only] [--reasoning <effort|default>] [--rate <Hz|native>] [--save <dir>], session-test, capture-test, player-test, lv2-test, lv2-host-test, lv2-voice-test, lv2-engine-test, metro-test, transport-test, record-test, cc-map-test, meter-test, clap-scan-test, edge-level-test, lv2-instrument-test, lv2-instrument-engine-test, main-controls-test, sound-library-test, many-voices-test, arp-test, preset-test, add-node-test, mono-master-test, multi-rig-test, multi-rig-load-test, rig-mix-test, lv2-async-test, organ-test, editor-test [plugin-id...], catalog-install-test [id...]";

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "app".into());
    match mode.as_str() {
        "app" => ui::run_app(),
        "preview" => preview::run(),
        "audio-test" => audio::run_test(),
        "midi-test" => midi::run_test(),
        "host-test" => host::run_test(),
        "editor-test" => host::run_editor_test(&std::env::args().skip(2).collect::<Vec<_>>()),
        "preset-test" => host::run_preset_test(),
        "ui-demo" => ui::run_demo(),
        "mcp" => {
            // --attach (or KABELSALAT_ATTACH): proxy the same tools to the
            // RUNNING GUI app via the control bridge instead of a local engine.
            if std::env::args().any(|a| a == "--attach")
                || std::env::var_os("KABELSALAT_ATTACH").is_some()
            {
                mcp::run_attach()
            } else {
                mcp::run()
            }
        }
        // One-shot control-bridge client: kabelsalat send '<json request>'.
        "send" => {
            let payload = std::env::args()
                .nth(2)
                .ok_or_else(|| anyhow::anyhow!("usage: kabelsalat send '<json>' (e.g. '{{\"op\":\"query\",\"name\":\"get_graph\"}}')"))?;
            bridge::cli_send(&payload)
        }
        "scan-clap" => {
            let path = std::env::args_os()
                .nth(2)
                .ok_or_else(|| anyhow::anyhow!("usage: kabelsalat scan-clap <bundle-path>"))?;
            host::run_scan_clap(std::path::Path::new(&path))
        }
        "clap-scan-test" => host::run_clap_scan_test(),
        "catalog-install-test" => catalog::install_test(&std::env::args().skip(2).collect::<Vec<_>>()),
        "session-test" => ui::run_session_test(),
        "session-load-bench" => ui::run_session_load_bench(&std::env::args().skip(2).collect::<Vec<_>>()),
        "agent-eval" => ui::run_agent_eval(),
        "jev-test" => assistant::run_jev_test(),
        "listen-test" => listen::run_test(),
        "capture-test" => capture::run_test(),
        "player-test" => player::run_test(),
        "lv2-test" => lv2::run_test(),
        "lv2-host-test" => lv2::host_test(),
        "lv2-voice-test" => lv2::voice_test(),
        "lv2-engine-test" => engine::lv2_engine_test(),
        "metro-test" => engine::metro_test(),
        "transport-test" => transport::run_test(),
        "edge-level-test" => engine::edge_level_test(),
        "lv2-instrument-test" => lv2::instrument_test(),
        "organ-test" => engine::organ_test(),
        "lv2-instrument-engine-test" => engine::lv2_instrument_engine_test(),
        "lv2-async-test" => engine::lv2_async_test(),
        "sound-library-test" => engine::sound_library_test(&std::env::args().skip(2).collect::<Vec<_>>()),
        "vital-test" => vital::run_test(&std::env::args_os().skip(2).map(Into::into).collect::<Vec<_>>()),
        "vital-import" => {
            let files: Vec<std::path::PathBuf> = std::env::args_os().skip(2).map(Into::into).collect();
            anyhow::ensure!(!files.is_empty(), "usage: kabelsalat vital-import <file.vitalbank|file.vital>...");
            let data = vital::data_dir();
            for f in &files {
                println!("{}: {}", f.display(), vital::import(f, &data).map_err(|e| anyhow::anyhow!(e))?);
            }
            println!("into {}", data.display());
            Ok(())
        }
        "add-node-test" => engine::add_node_test(),
        "many-voices-test" => engine::many_voices_test(),
        "arp-test" => engine::arp_test(),
        "mono-master-test" => engine::mono_master_test(),
        "multi-rig-test" => engine::multi_rig_test(),
        "rig-mix-test" => engine::rig_mix_test(),
        "multi-rig-load-test" => engine::multi_rig_load_test(),
        "cc-map-test" => engine::cc_map_test(),
        "main-controls-test" => engine::main_controls_test(),
        "meter-test" => engine::meter_test(),
        "record-test" => record::run_test(),
        "audio-in-test" => audio_in::run_test(),
        "monitor-latency-test" => audio_in::run_monitor_latency_test(),
        "guitar-rig-test" => engine::guitar_rig_test(),
        "--version" | "-V" => {
            println!("kabelsalat {VERSION}");
            Ok(())
        }
        "--help" | "-h" | "help" => {
            println!("usage: kabelsalat [mode]\nmodes: {MODES}");
            Ok(())
        }
        other => {
            eprintln!("unknown mode: {other}\nmodes: {MODES}");
            std::process::exit(1);
        }
    }
}
