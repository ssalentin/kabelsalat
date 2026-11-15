//! kabelsalat — live audio/MIDI wiring, alive.
//!
//! P0 spike entry point. Each subsystem has a standalone test mode so the team
//! can develop and verify them in parallel; integration happens in `main` once
//! the spikes pass their go/no-go checks.

mod assistant;
mod audio;
mod bridge;
mod capture;
mod control;
mod dsp;
mod engine;
mod host;
mod lv2;
mod matrix;
mod mcp;
mod midi;
mod player;
mod preview;
mod session;
mod ui;

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "app".into());
    match mode.as_str() {
        "app" => ui::run_app(),
        "preview" => preview::run(),
        "audio-test" => audio::run_test(),
        "midi-test" => midi::run_test(),
        "host-test" => host::run_test(),
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
        "session-test" => ui::run_session_test(),
        "capture-test" => capture::run_test(),
        "player-test" => player::run_test(),
        "lv2-test" => lv2::run_test(),
        "lv2-host-test" => lv2::host_test(),
        other => {
            eprintln!("unknown mode: {other}\nmodes: app, preview, audio-test, midi-test, host-test, ui-demo, mcp [--attach], send, session-test, capture-test, player-test, lv2-test, lv2-host-test");
            std::process::exit(1);
        }
    }
}
