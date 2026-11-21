#![recursion_limit = "512"]
//! kabelsalat — live audio/MIDI wiring, alive.
//!
//! P0 spike entry point. Each subsystem has a standalone test mode so the team
//! can develop and verify them in parallel; integration happens in `main` once
//! the spikes pass their go/no-go checks.

/// Release version and the commit it was built from, e.g. "0.1.0 (abc1234)".
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("KABELSALAT_COMMIT"), ")");

mod aeolus;
mod analysis;
mod alsaseq;
mod assistant;
mod arp;
mod audio;
mod audio_backend;
mod audio_in;
mod audio_out;
mod binarch;
mod bridge;
mod capture;
mod catalog;
mod clocksend;
mod control;
mod crash;
mod demo;
mod dsp;
mod engine;
mod engine_info;
mod guitar;
mod host;
mod juce;
mod layer;
mod keyboard;
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
mod preshow;
mod plugins;
mod plugin_window;
mod preview;
mod record;
mod retro;
mod rope;
mod session;
mod setlist;
mod show;
mod sounds;
mod theme;
mod transport;
mod ui;
mod vital;
mod watchdog;

const MODES: &str = "app [--headless], crash-test, preview, analysis-test [--save <dir>], audio-test, audio-backend-test, audio-in-test, audio-out-test, monitor-latency-test, guitar-rig-test, midi-test, host-test, ui-demo, mcp [--attach], send, agent-eval [--model <id>] [--runs N] [--task <name>] [--no-jev], jev-test, listen-test [--model <id>] [--runs N] [--audio-only] [--reasoning <effort|default>] [--rate <Hz|native>] [--save <dir>], session-test, capture-test, player-test, lv2-test, lv2-host-test, lv2-voice-test, lv2-engine-test, metro-test, transport-test, clock-test, record-test, cc-map-test, meter-test, target-test, clap-scan-test, edge-level-test, lv2-instrument-test, lv2-instrument-engine-test, midifx-test, main-controls-test, sound-library-test, many-voices-test, arp-test, demo-test, preset-test, state-race-test [seconds] [patch.fxp...], add-node-test, mono-master-test, multi-rig-test, multi-rig-load-test, rig-mix-test, setlist-test, setlist-switch-test, levels-test [--save <dir>], preshow-check <session.ksalat>, send-test, lv2-async-test, organ-test, editor-test [plugin-id...], catalog-install-test [id...]";

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "app".into());
    match mode.as_str() {
        "app" => watchdog::run_app(std::env::args().any(|a| a == "--headless")),
        "crash-test" => watchdog::run_test(),
        "preview" => preview::run(),
        "audio-test" => audio::run_test(),
        "audio-backend-test" => audio_backend::run_test(),
        "midi-test" => midi::run_test(),
        "host-test" => host::run_test(),
        "editor-test" => host::run_editor_test(&std::env::args().skip(2).collect::<Vec<_>>()),
        "preset-test" => host::run_preset_test(),
        "state-race-test" => {
            let seconds = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(20);
            let patches: Vec<_> = std::env::args().skip(3).map(std::path::PathBuf::from).collect();
            host::run_state_race_test(seconds, &patches)
        }
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
        "engine-bench" => engine::bench(),
        "ui-bench" => ui::run_ui_bench(&std::env::args().skip(2).collect::<Vec<_>>()),
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
        "clock-test" => clocksend::run_clock_test(),
        "edge-level-test" => engine::edge_level_test(),
        "lv2-instrument-test" => lv2::instrument_test(),
        "organ-test" => engine::organ_test(),
        "lv2-instrument-engine-test" => engine::lv2_instrument_engine_test(),
        "midifx-test" => engine::midifx_test(),
        "lv2-async-test" => engine::lv2_async_test(),
        "wind-test" => engine::wind_test(std::env::args().nth(2).as_deref()),
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
        "demo-test" => engine::demo_test(),
        "mono-master-test" => engine::mono_master_test(),
        "multi-rig-test" => engine::multi_rig_test(),
        "rig-mix-test" => engine::rig_mix_test(),
        "setlist-test" => engine::setlist_test::run(),
        "preshow-check" => preshow::gather::run_cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        "setlist-switch-test" => engine::setlist_switch_test::run(),
        "levels-test" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let save = args.iter().position(|a| a == "--save").and_then(|i| args.get(i + 1));
            engine::levels_test::run(save.map(std::path::Path::new))
        }
        "send-test" => engine::send_test(),
        "multi-rig-load-test" => engine::multi_rig_load_test(),
        "cc-map-test" => engine::cc_map_test(),
        "main-controls-test" => engine::main_controls_test(),
        "meter-test" => engine::meter_test(),
        "target-test" => engine::target_test(),
        "record-test" => record::run_test(),
        "audio-in-test" => audio_in::run_test(),
        "audio-out-test" => audio_out::run_test(),
        "monitor-latency-test" => audio_in::run_monitor_latency_test(),
        "guitar-rig-test" => engine::guitar_rig_test(),
        "analysis-test" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let save = args.iter().position(|a| a == "--save").and_then(|i| args.get(i + 1));
            engine::analysis_test::run(save.map(std::path::Path::new))
        }
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
