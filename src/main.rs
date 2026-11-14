//! kabelsalat — live audio/MIDI wiring, alive.
//!
//! P0 spike entry point. Each subsystem has a standalone test mode so the team
//! can develop and verify them in parallel; integration happens in `main` once
//! the spikes pass their go/no-go checks.

mod audio;
mod host;
mod midi;
mod ui;

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "audio-test".into());
    match mode.as_str() {
        "audio-test" => audio::run_test(),
        "midi-test" => midi::run_test(),
        "host-test" => host::run_test(),
        "ui-demo" => ui::run_demo(),
        other => {
            eprintln!("unknown mode: {other}\nmodes: audio-test, midi-test, host-test, ui-demo");
            std::process::exit(1);
        }
    }
}
