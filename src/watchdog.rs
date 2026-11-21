//! The watchdog (#279). `kabelsalat` (app mode) runs the app as its own
//! child and starts it again when it crashes: killed by a signal other than
//! SIGINT, SIGTERM or SIGHUP, or ended by a panic (exit code 101). The new
//! child gets `KABELSALAT_RECOVER` and opens the session and setlist part
//! the crashed one was on, from the marker the app keeps up to date
//! (`Marker`), and `KABELSALAT_CRASH_REPORT` with the report it left.
//!
//! A child is only recovered when it wrote the marker itself, after its
//! own start: a crash before the first frame (or while a recovered child
//! loads the session again) starts the next child fresh, so a session that
//! crashes on load cannot loop. Five crashes within a minute end the watch.
//!
//! `KABELSALAT_WATCHDOG=0` runs the app without one. SIGINT, SIGTERM and
//! SIGHUP to the watchdog go on to the app.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::setlist::SetlistPos;

mod selftest;
pub use selftest::run as run_test;

/// Set for the child: it is supervised (and writes the marker).
pub const SUPERVISED: &str = "KABELSALAT_SUPERVISED";
/// Set for a child after a crash: open what the marker names.
pub const RECOVER: &str = "KABELSALAT_RECOVER";
/// Set for a child after a crash: the report the crashed one wrote.
pub const CRASH_REPORT: &str = "KABELSALAT_CRASH_REPORT";
/// `0` runs the app without a watchdog.
pub const WATCHDOG: &str = "KABELSALAT_WATCHDOG";

const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

/// What the app was on, so a restart can go back to it: the saved session
/// whose file holds the rack (the open session, or the one autosave wrote)
/// and the setlist position.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub session: Option<String>,
    pub pos: Option<SetlistPos>,
}

impl Marker {
    /// `$XDG_STATE_HOME/kabelsalat/recover.json`, else under
    /// `~/.local/state`. Tests use a folder of their own (rule 37).
    pub fn path() -> PathBuf {
        crate::crash::crash_dir().parent().map(|p| p.join("recover.json")).unwrap_or_else(|| "recover.json".into())
    }

    pub fn read(path: &Path) -> Option<Marker> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Replace the marker in one rename, so a crash mid-write leaves the old one.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// How a child ended, as the watchdog sees it.
#[derive(Debug, PartialEq)]
enum Ended {
    /// Closed, interrupted or failed to start: the watchdog ends with this code.
    Quit(i32),
    /// Crashed: start it again.
    Crashed(String),
}

fn ended(status: ExitStatus) -> Ended {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = status.signal() {
        return if [libc::SIGINT, libc::SIGTERM, libc::SIGHUP].contains(&sig) {
            Ended::Quit(128 + sig)
        } else {
            Ended::Crashed(format!("signal {sig}"))
        };
    }
    match status.code() {
        Some(101) => Ended::Crashed("panic".into()),
        Some(code) => Ended::Quit(code),
        None => Ended::Quit(1),
    }
}

/// The app, under a watchdog unless this process is the supervised child
/// or `KABELSALAT_WATCHDOG=0`. `headless` runs the windowless app
/// (`crash-test`).
pub fn run_app(headless: bool) -> anyhow::Result<()> {
    if std::env::var_os(SUPERVISED).is_some() || std::env::var_os(WATCHDOG).is_some_and(|v| v == "0") {
        crate::crash::install();
        return if headless { crate::ui::run_headless() } else { crate::ui::run_app() };
    }
    let code = supervise()?;
    std::process::exit(code);
}

static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

fn supervise() -> anyhow::Result<i32> {
    let exe = std::env::current_exe()?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = forward as extern "C" fn(libc::c_int) as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
    let marker = Marker::path();
    let mut crashes: Vec<Instant> = Vec::new();
    let mut recover = false;
    let mut report: Option<PathBuf> = None;
    loop {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args).env(SUPERVISED, "1").env_remove(RECOVER).env_remove(CRASH_REPORT);
        if recover {
            cmd.env(RECOVER, "1");
        }
        if let Some(r) = &report {
            cmd.env(CRASH_REPORT, r);
        }
        let started = SystemTime::now();
        let mut child = cmd.spawn()?;
        let pid = child.id();
        CHILD.store(pid as i32, Ordering::Relaxed);
        let status = child.wait()?;
        CHILD.store(0, Ordering::Relaxed);
        let why = match ended(status) {
            Ended::Quit(code) => return Ok(code),
            Ended::Crashed(why) => why,
        };
        let now = Instant::now();
        crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
        crashes.push(now);
        report = crate::crash::report_of(&crate::crash::crash_dir(), pid);
        let shown = report.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into());
        if crashes.len() >= MAX_CRASHES {
            eprintln!("watchdog: the app crashed {} times within a minute ({why}); not starting it again. Last report: {shown}", crashes.len());
            return Ok(1);
        }
        recover = wrote_since(&marker, started);
        eprintln!(
            "watchdog: the app crashed ({why}), report: {shown}; starting it again{}",
            if recover { " where it was" } else { " fresh (it crashed before it was up)" }
        );
    }
}

/// Whether the marker was written at or after `t`.
fn wrote_since(marker: &Path, t: SystemTime) -> bool {
    std::fs::metadata(marker).and_then(|m| m.modified()).is_ok_and(|m| m >= t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn crashes_restart_and_deliberate_ends_do_not() {
        let signal = |s: i32| ExitStatus::from_raw(s);
        let code = |c: i32| ExitStatus::from_raw(c << 8);
        assert_eq!(ended(signal(libc::SIGSEGV)), Ended::Crashed("signal 11".into()));
        assert_eq!(ended(signal(libc::SIGABRT)), Ended::Crashed("signal 6".into()));
        assert_eq!(ended(signal(libc::SIGKILL)), Ended::Crashed("signal 9".into()));
        assert_eq!(ended(code(101)), Ended::Crashed("panic".into()));
        assert_eq!(ended(code(0)), Ended::Quit(0));
        assert_eq!(ended(code(1)), Ended::Quit(1), "a start that fails (no display) is not retried");
        assert_eq!(ended(signal(libc::SIGTERM)), Ended::Quit(143));
        assert_eq!(ended(signal(libc::SIGINT)), Ended::Quit(130));
    }

    #[test]
    fn the_marker_round_trips_and_says_when_it_was_written() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-test-marker-{}", std::process::id()));
        let path = dir.join("recover.json");
        let before = SystemTime::now() - Duration::from_secs(1);
        assert!(!wrote_since(&path, before));
        let m = Marker { session: Some("Gig".into()), pos: Some(SetlistPos { song: 1, part: 2 }) };
        m.write(&path).unwrap();
        assert_eq!(Marker::read(&path), Some(m));
        assert!(wrote_since(&path, before));
        assert!(!wrote_since(&path, SystemTime::now() + Duration::from_secs(5)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
