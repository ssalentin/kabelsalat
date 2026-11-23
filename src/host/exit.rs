//! How a process that loaded plugin code leaves (#422).
//!
//! A plugin binary stays loaded for the life of the process (C++ plugin
//! libraries keep static state that races on unload), so a plain return from
//! `main` runs `exit()`, and with it the static and thread-local destructors
//! of every plugin library. Those are not ours to trust: Six Sines' VST3
//! destroys a still joinable `std::thread` there, which is `std::terminate`
//! ("terminate called without an active exception", `SIGABRT`, exit 134)
//! after `vst3-test` and `editor-test` had already printed PASS.
//!
//! A process that loaded plugin code in-process therefore reports its result,
//! flushes its output and leaves with `_exit`, which runs no destructors. The
//! sandbox workers leave the same way (rules 56, 221).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static PLUGIN_CODE: AtomicBool = AtomicBool::new(false);

/// A plugin binary (CLAP, VST3, LV2 or an LV2 UI) is now loaded into this
/// process. Call it at every place that opens one.
pub fn plugin_code_loaded() {
    PLUGIN_CODE.store(true, Ordering::Release);
}

/// The exit code to leave with at once, or `None` to return from `main` the
/// ordinary way (no plugin code in the process: nothing foreign runs at exit).
fn exit_code<E>(plugin_code: bool, result: &Result<(), E>) -> Option<i32> {
    plugin_code.then_some(if result.is_ok() { 0 } else { 1 })
}

/// The last call of `main`: hand `result` back when no plugin code was
/// loaded; otherwise print an error as `main` would, flush, and `_exit`.
pub fn leave(result: anyhow::Result<()>) -> anyhow::Result<()> {
    let Some(code) = exit_code(PLUGIN_CODE.load(Ordering::Acquire), &result) else {
        return result;
    };
    if let Err(e) = &result {
        eprintln!("Error: {e:?}");
    }
    exit_now(code)
}

/// Leave at once with `code`, without the destructors of loaded plugin
/// libraries. For a path that would otherwise call `std::process::exit`
/// after plugin code was loaded.
pub fn exit_now(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { libc::_exit(code) }
}

#[cfg(test)]
mod tests {
    use super::exit_code;

    #[test]
    fn a_process_without_plugin_code_returns_from_main_the_ordinary_way() {
        assert_eq!(exit_code(false, &Ok::<(), ()>(())), None);
        assert_eq!(exit_code(false, &Err::<(), ()>(())), None);
    }

    #[test]
    fn a_process_with_plugin_code_leaves_at_once_with_the_results_code() {
        assert_eq!(exit_code(true, &Ok::<(), ()>(())), Some(0));
        assert_eq!(exit_code(true, &Err::<(), ()>(())), Some(1));
    }
}
