//! `kabelsalat crash-test` (#279): the real binary under its watchdog,
//! crashed three ways, comes back on the same song and part with sound.
//!
//! The app runs headless (`app --headless`) in a scratch home under
//! `~/.cache`, so nothing of the user's sessions, config or crash folder is
//! read or written. It opens a two-song session whose rigs click on the
//! transport (no plugin needed), goes to a part that was never autosaved,
//! and is then killed with SIGSEGV, made to panic and made to segfault. After
//! each crash the test checks that the watchdog started it again on that
//! part, times how long until the master is not silent, and reads the crash
//! report: the cause, the frames, the commands, and no path or session name.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};

use crate::control::{merge_rigs, name_bytes, DspKind, Graph, NodeKind, PortKind, PortRef, Rig};
use crate::session::Session;
use crate::setlist::{Part, PartRig, SetlistPos, Song};

const SESSION: &str = "Crash test";
/// Longest wait for one answer of the app.
const ANSWER: Duration = Duration::from_secs(30);
/// What the test states: from the crash to sound, at most this long.
const BACK_WITH_SOUND: Duration = Duration::from_secs(10);

/// Names the reports must not hold (the session and its songs and parts).
const PRIVATE: [&str; 6] = [SESSION, "Opener", "Encore", "Intro", "Groove", "Outro"];

fn songs() -> Vec<Song> {
    let song = |name: &str, bpm: f32, rig: u8, parts: [&str; 2]| Song {
        bpm: Some(bpm),
        ..Song::new(name, parts.iter().map(|p| Part::new(*p, vec![PartRig::new(rig)])).collect())
    };
    vec![song("Opener", 100.0, 0, ["Intro", "Groove"]), song("Encore", 140.0, 1, ["Groove", "Outro"])]
}

/// Keys and a metronome into the master: a rig that sounds on the transport.
fn click_rig(name: &str) -> Rig {
    let mut g = Graph::default();
    g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let click = g.add_node(NodeKind::Dsp(DspKind::Metronome), name_bytes("METRONOME"));
    for port in 0..2u8 {
        g.connect_with_id(1 + port as u32, PortRef { node: click, port: 0 }, PortRef { node: master, port }, PortKind::Audio);
    }
    Rig::new(name, g, None)
}

fn write_session(sessions: &Path) -> anyhow::Result<()> {
    let rigs = [click_rig("Rig A"), click_rig("Rig B")];
    let mut s = Session::new(SESSION, merge_rigs(&rigs));
    s.rig_names = rigs.iter().map(|r| r.name.clone()).collect();
    s.setlist.songs = songs();
    s.setlist.active = Some(SetlistPos { song: 0, part: 0 });
    s.save(&Session::package_path(sessions, SESSION)?)
}

/// The watchdog process and the app it runs, talked to over stdin/stdout.
struct App {
    watchdog: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl App {
    fn start(home: &Path, log: &Path) -> anyhow::Result<Self> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(["app", "--headless"]).env("HOME", home);
        for var in ["XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", super::SUPERVISED, super::RECOVER, super::CRASH_REPORT, super::WATCHDOG] {
            cmd.env_remove(var);
        }
        let mut watchdog = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(std::fs::File::create(log)?).spawn()?;
        let stdin = watchdog.stdin.take().context("stdin")?;
        let stdout = watchdog.stdout.take().context("stdout")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { watchdog, stdin, lines })
    }

    fn send(&mut self, line: &str) -> anyhow::Result<()> {
        writeln!(self.stdin, "{line}")?;
        Ok(self.stdin.flush()?)
    }

    /// The next answer that starts with `what`.
    fn expect(&mut self, what: &str) -> anyhow::Result<Answer> {
        self.expect_within(what, ANSWER)?.ok_or_else(|| anyhow::anyhow!("no '{what}' from the app within {} s", ANSWER.as_secs()))
    }

    fn expect_within(&mut self, what: &str, wait: Duration) -> anyhow::Result<Option<Answer>> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = self.lines.recv_timeout(left) else { return Ok(None) };
            let Some(answer) = line.strip_prefix("headless: ") else { continue };
            if let Some(e) = answer.strip_prefix("error ") {
                bail!("the app answered: {e}");
            }
            if let Some(rest) = answer.strip_prefix(what) {
                return Ok(Some(Answer(rest.trim().to_string())));
            }
        }
    }

    fn ask(&mut self, line: &str, what: &str) -> anyhow::Result<Answer> {
        self.send(line)?;
        self.expect(what)
    }
}

/// `key=value …` with values that may hold spaces (session names).
struct Answer(String);

impl Answer {
    fn get(&self, key: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for tok in self.0.split(' ') {
            match tok.split_once('=') {
                Some((k, v)) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase()) => {
                    if out.is_some() {
                        break;
                    }
                    if k == key {
                        out = Some(v.to_string());
                    }
                }
                _ => {
                    if let Some(o) = &mut out {
                        o.push(' ');
                        o.push_str(tok);
                    }
                }
            }
        }
        out
    }

    fn pos(&self) -> Option<SetlistPos> {
        Some(SetlistPos { song: self.get("song")?.parse().ok()?, part: self.get("part")?.parse().ok()? })
    }
}

enum Crash {
    Kill(libc::c_int),
    Say(&'static str),
}

struct Round {
    how: &'static str,
    goto: SetlistPos,
    crash: Crash,
    cause: &'static str,
}

const ROUNDS: [Round; 3] = [
    Round { how: "kill -SEGV", goto: SetlistPos { song: 1, part: 1 }, crash: Crash::Kill(libc::SIGSEGV), cause: "cause: SIGSEGV (signal 11), sent by process" },
    Round { how: "panic on the UI thread", goto: SetlistPos { song: 0, part: 1 }, crash: Crash::Say("panic"), cause: "cause: panic: crash-test: a panic on the UI thread" },
    Round { how: "segfault on the UI thread", goto: SetlistPos { song: 1, part: 0 }, crash: Crash::Say("segv"), cause: "cause: SIGSEGV (signal 11), fault address 0x10" },
];

/// Every check on one report; `forbidden` are strings of this machine
/// (homes, user name) that must not appear.
fn check_report(text: &str, round: &Round, forbidden: &[String]) -> anyhow::Result<()> {
    for want in ["kabelsalat crash report", &format!("version: {}", crate::VERSION), round.cause, "thread: ", "plugins loaded:", "last engine commands", "LoadRigs", "frames (module+offset):", "kabelsalat+0x"] {
        ensure!(text.contains(want), "the report lacks {want:?}:\n{text}");
    }
    if matches!(round.crash, Crash::Say(_)) {
        ensure!(text.contains("thread: kabelsalat (main)"), "the report names another thread:\n{text}");
    }
    if matches!(round.crash, Crash::Say("panic")) {
        ensure!(text.contains("at: src/ui/headless.rs:"), "the panic report lacks its source line:\n{text}");
    }
    for tok in text.split_whitespace() {
        let tok = tok.trim_start_matches(|c: char| "'\"([<=,:".contains(c));
        ensure!(!tok.starts_with('/') && !tok.starts_with("~/"), "the report holds a path ({tok}):\n{text}");
    }
    for bad in PRIVATE.iter().map(|s| s.to_string()).chain(forbidden.iter().cloned()) {
        ensure!(!text.contains(&bad), "the report holds {bad:?}:\n{text}");
    }
    Ok(())
}

fn drive(app: &mut App, home: &Path, forbidden: &[String]) -> anyhow::Result<()> {
    let songs = songs();
    let ready = app.expect("ready")?;
    ensure!(ready.get("recovered").as_deref() == Some("0"), "a first start recovered something: {}", ready.0);
    let mut pid: i32 = ready.get("pid").context("pid")?.parse()?;
    app.expect("state")?;
    // Only the session's rigs make a sound: without it the transport is silent.
    app.send("play")?;
    ensure!(app.expect_within("sound", Duration::from_secs(1))?.is_none(), "the app sounded before the session was open");
    app.ask("stop", "state")?;
    let opened = app.ask(&format!("open {SESSION}"), "state")?;
    ensure!(opened.get("session").as_deref() == Some(SESSION), "open: {}", opened.0);
    let crashes = home.join(".local/state/kabelsalat/crashes");
    let mut times = Vec::new();
    for round in &ROUNDS {
        let at = app.ask(&format!("select {} {}", round.goto.song, round.goto.part), "state")?;
        ensure!(at.pos() == Some(round.goto), "select: {}", at.0);
        let t0 = Instant::now();
        match round.crash {
            Crash::Kill(sig) => unsafe {
                libc::kill(pid, sig);
            },
            Crash::Say(what) => app.send(what)?,
        }
        let ready = app.expect("ready")?;
        let new_pid: i32 = ready.get("pid").context("pid")?.parse()?;
        ensure!(new_pid != pid, "the same process answered after the crash");
        ensure!(ready.get("recovered").as_deref() == Some("1"), "{}: not recovered: {}", round.how, ready.0);
        let state = app.expect("state")?;
        let song = &songs[round.goto.song];
        let want_bpm = format!("{:.0}", song.bpm.unwrap_or_default());
        ensure!(state.get("session").as_deref() == Some(SESSION) && state.pos() == Some(round.goto) && state.get("bpm") == Some(want_bpm),
            "{}: back on {} instead of {SESSION} song {} part {}", round.how, state.0, round.goto.song, round.goto.part);
        let sound = app.ask("play", "sound")?;
        let back = t0.elapsed();
        times.push(back);
        let report = PathBuf::from(ready.get("report").context("report")?);
        ensure!(report.starts_with(&crashes), "{}: report {} is not in the crash folder", round.how, report.display());
        ensure!(crate::crash::report_of(&crashes, pid as u32).as_ref() == Some(&report), "{}: the report is not the crashed process's", round.how);
        let text = std::fs::read_to_string(&report)?;
        check_report(&text, round, forbidden).with_context(|| round.how)?;
        ensure!(back <= BACK_WITH_SOUND, "{}: sound only after {:.2} s", round.how, back.as_secs_f64());
        println!("  {:<26} back on {} · {} with sound ({}) after {:.2} s; report {} ({} lines)",
            round.how, song.name, song.parts[round.goto.part].name, sound.0, back.as_secs_f64(),
            report.file_name().unwrap_or_default().to_string_lossy(), text.lines().count());
        pid = new_pid;
    }
    app.send("quit")?;
    let deadline = Instant::now() + ANSWER;
    let status = loop {
        if let Some(s) = app.watchdog.try_wait()? {
            break s;
        }
        ensure!(Instant::now() < deadline, "the watchdog did not end after quit");
        std::thread::sleep(Duration::from_millis(50));
    };
    ensure!(status.success(), "the watchdog ended with {status} after a clean quit");
    let worst = times.iter().max().copied().unwrap_or_default();
    println!("crash-test: PASS · back with sound after at most {:.2} s (stated: {} s) · clean quit ends the watchdog", worst.as_secs_f64(), BACK_WITH_SOUND.as_secs());
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".cache"));
    let scratch = cache.join(format!("ksalat-crash-test-{}", std::process::id()));
    let home = scratch.join("home");
    std::fs::create_dir_all(home.join("kabelsalat-sessions"))?;
    write_session(&home.join("kabelsalat-sessions"))?;
    let log = scratch.join("app.log");
    let mut forbidden = vec![home.display().to_string()];
    forbidden.extend(std::env::var("HOME").ok().filter(|h| h.len() > 1));
    forbidden.extend(std::env::var("USER").ok().filter(|u| u.len() >= 3));
    println!("crash-test: {} under its watchdog, headless, home {}", crate::VERSION, home.display());
    let mut app = App::start(&home, &log)?;
    let result = drive(&mut app, &home, &forbidden);
    let _ = app.watchdog.kill();
    let _ = app.watchdog.wait();
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    for line in log_text.lines().filter(|l| l.starts_with("watchdog:")) {
        println!("  {line}");
    }
    match result {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&scratch);
            Ok(())
        }
        Err(e) => {
            let tail: Vec<&str> = log_text.lines().rev().take(30).collect();
            eprintln!("app log (last lines, kept in {}):", scratch.display());
            for l in tail.iter().rev() {
                eprintln!("  {l}");
            }
            Err(e)
        }
    }
}
