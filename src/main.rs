//! kabelsalat — live audio/MIDI wiring, alive.
//!
//! P0 spike entry point. Each subsystem has a standalone test mode so the team
//! can develop and verify them in parallel; integration happens in `main` once
//! the spikes pass their go/no-go checks.

mod assistant;
mod audio;
mod control;
mod dsp;
mod engine;
mod host;
mod matrix;
mod mcp;
mod midi;
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
        "mcp" => mcp::run(),
        "session-test" => ui::run_session_test(),
        other => {
            eprintln!("unknown mode: {other}\nmodes: app, preview, audio-test, midi-test, host-test, ui-demo, mcp, session-test");
            std::process::exit(1);
        }
    }
}
