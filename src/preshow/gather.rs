//! Finding the [`Facts`] of this machine. Every function here blocks on I/O
//! (ALSA, pactl, the plugin scan, files, an offline render), so it runs in
//! the CLI, on the headless agent's own thread or on the app's check
//! thread, never on the UI thread (rules 48, 57, 127).

use std::path::Path;
use std::time::Instant;

use super::{Disk, Facts, FileFact, Installed, Power, Report, Session, Status};
use crate::host::ClapPluginInfo;
use crate::lv2::Lv2PluginInfo;

/// Hardware MIDI inputs plugged in now, by port name without ALSA's
/// client:port numbers (rule 58).
pub fn midi_ports() -> Option<Vec<String>> {
    let probe = midir::MidiInput::new("kabelsalat-preshow").ok()?;
    let ports = probe.ports();
    Some(
        ports
            .iter()
            .filter_map(|p| probe.port_name(p).ok())
            .filter(|n| crate::ui::is_hw_midi_port(n))
            .map(|n| crate::ui::midi_port_key(&n).to_string())
            .collect(),
    )
}

pub fn outputs() -> Option<Vec<crate::audio_out::OutputDevice>> {
    let sinks = crate::audio_out::pactl(&["-f", "json", "list", "sinks"])?;
    let default = crate::audio_out::pactl(&["get-default-sink"]).unwrap_or_default();
    Some(crate::audio_out::parse_sinks(&sinks, default.trim()))
}

pub fn inputs() -> Option<Vec<crate::audio_in::InputDevice>> {
    crate::audio_out::pactl(&["info"])?;
    Some(crate::audio_in::list_input_devices())
}

impl Installed {
    pub fn of(clap: &[ClapPluginInfo], lv2: &[Lv2PluginInfo]) -> Self {
        Self {
            clap: clap.iter().map(|p| p.id.clone()).collect(),
            lv2: lv2.iter().map(|p| (p.uri.clone(), p.is_instrument)).collect(),
        }
    }
}

pub fn installed() -> Installed {
    Installed::of(&crate::host::list_clap_plugins(), &crate::lv2::discover())
}

/// Every backing track the setlist names, read up to its chunk headers.
pub fn files(s: &Session) -> Vec<(String, FileFact)> {
    let mut out: Vec<(String, FileFact)> = Vec::new();
    for path in s.setlist.songs.iter().filter_map(|song| song.backing.as_deref()) {
        if out.iter().any(|(p, _)| p == path) {
            continue;
        }
        let fact = if !Path::new(path).is_file() {
            FileFact::Missing
        } else {
            match crate::player::wav_seconds(Path::new(path)) {
                Ok(seconds) => FileFact::Audio { seconds },
                Err(e) => FileFact::Unreadable(e.to_string()),
            }
        };
        out.push((path.to_string(), fact));
    }
    out
}

/// Free space where takes are written (or where that folder will be made).
pub fn disk(dir: &Path) -> Option<Disk> {
    use std::os::unix::ffi::OsStrExt;
    let mut at = dir;
    while !at.exists() {
        at = at.parent()?;
    }
    let path = std::ffi::CString::new(at.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid C string and `st` a plain struct statvfs fills.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let shown = dir.to_string_lossy();
    let shown = match shown.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => shown.into_owned(),
    };
    Some(Disk { dir: shown, free_bytes: st.f_bavail as u64 * st.f_frsize as u64 })
}

pub fn power() -> Option<Power> {
    power_in(Path::new("/sys/class/power_supply"))
}

/// The system's power supplies as the kernel lists them. A battery of a
/// device (a mouse, `scope` Device) is not the machine's.
fn power_in(root: &Path) -> Option<Power> {
    let read = |p: &Path, f: &str| std::fs::read_to_string(p.join(f)).ok().map(|s| s.trim().to_string());
    let (mut mains_seen, mut mains_online, mut battery, mut has_battery, mut discharging) = (false, false, None, false, false);
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let p = e.path();
        match read(&p, "type").as_deref() {
            Some("Mains" | "USB") => {
                mains_seen = true;
                mains_online |= read(&p, "online").as_deref() == Some("1");
            }
            Some("Battery") if read(&p, "scope").as_deref() != Some("Device") => {
                has_battery = true;
                battery = battery.or_else(|| read(&p, "capacity").and_then(|c| c.parse::<u8>().ok()));
                discharging |= read(&p, "status").as_deref() == Some("Discharging");
            }
            _ => {}
        }
    }
    Some(if !has_battery {
        Power::NoBattery
    } else if discharging || (mains_seen && !mains_online) {
        Power::Battery { percent: battery }
    } else {
        Power::Mains { battery }
    })
}

/// Fill in what the app does not keep: the backing tracks, the disk, the
/// power, the probe of every rig, and the render time when no running
/// engine measured it.
pub fn complete(facts: &mut Facts, s: &Session, buffer_frames: u32) {
    facts.files = files(s);
    facts.disk = disk(&crate::record::record_dir());
    facts.power = power();
    // Without the scan the probe would take every plugin for missing.
    if let Some(installed) = &facts.plugins {
        let (probe, render) = super::probe::run(s, installed, buffer_frames);
        facts.probe = Some(probe);
        facts.render.get_or_insert(render);
    }
}

/// Every fact of this machine for a saved session (the CLI and a headless
/// agent; the app has most of them at hand already).
pub fn machine_facts(s: &Session) -> Facts {
    let (lists, installed) = std::thread::scope(|scope| {
        let lists = scope.spawn(|| (midi_ports(), outputs(), inputs()));
        let installed = installed();
        (lists.join().unwrap_or((None, None, None)), installed)
    });
    let mut facts = Facts {
        midi_ports: lists.0,
        outputs: lists.1,
        chosen_output: crate::assistant::load_config_str("audio_output"),
        inputs: lists.2,
        plugins: Some(installed),
        ..Default::default()
    };
    let frames = crate::assistant::load_buffer_frames_from(&crate::assistant::config_path());
    complete(&mut facts, s, frames);
    facts
}

pub fn check_package(path: &Path) -> anyhow::Result<Report> {
    let t = Instant::now();
    let session = Session::load(path)?;
    let facts = machine_facts(&session);
    Ok(Report::of(&session, &facts, t.elapsed()))
}

/// `kabelsalat preshow-check <package.ksalat> [--json]`: exit status 1 when
/// a check found a problem.
pub fn run_cli(args: &[String]) -> anyhow::Result<()> {
    let json = args.iter().any(|a| a == "--json");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("usage: kabelsalat preshow-check <session.ksalat> [--json]"))?;
    let report = check_package(Path::new(path))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report.json())?);
    } else {
        print!("{}", report.text());
    }
    if report.count(Status::Problem) > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, fields: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (f, v) in fields {
            std::fs::write(d.join(f), format!("{v}\n")).unwrap();
        }
    }

    /// Layouts as a laptop's /sys/class/power_supply shows them.
    #[test]
    fn the_power_state_comes_from_the_machines_own_supplies() {
        let root = std::env::temp_dir().join(format!("ksalat-power-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(power_in(&root), Some(Power::NoBattery), "a desktop or a server");
        supply(&root, "hid-mouse-battery", &[("type", "Battery"), ("scope", "Device"), ("capacity", "5"), ("status", "Discharging")]);
        assert_eq!(power_in(&root), Some(Power::NoBattery), "a mouse's battery is not the machine's");
        supply(&root, "AC", &[("type", "Mains"), ("online", "1")]);
        supply(&root, "BAT0", &[("type", "Battery"), ("capacity", "83"), ("status", "Charging")]);
        assert_eq!(power_in(&root), Some(Power::Mains { battery: Some(83) }));
        supply(&root, "AC", &[("type", "Mains"), ("online", "0")]);
        supply(&root, "BAT0", &[("type", "Battery"), ("capacity", "41"), ("status", "Discharging")]);
        assert_eq!(power_in(&root), Some(Power::Battery { percent: Some(41) }));
        assert_eq!(power_in(&root.join("gone")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backing_tracks_are_found_missing_or_unreadable() {
        let dir = std::env::temp_dir().join(format!("ksalat-backing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("notes.wav");
        std::fs::write(&text, b"not audio").unwrap();
        let mut s = Session::new("Gig", crate::control::standard_rig_graph(true));
        for path in [text.to_string_lossy().into_owned(), "/nowhere/intro.wav".into(), text.to_string_lossy().into_owned()] {
            let mut song = crate::setlist::Song::new("Song", vec![]);
            song.backing = Some(path);
            s.setlist.songs.push(song);
        }
        let f = files(&s);
        assert_eq!(f.len(), 2, "a file two songs share is read once");
        assert_eq!(f[0].1, FileFact::Unreadable("not a RIFF/WAVE file".into()));
        assert_eq!(f[1], ("/nowhere/intro.wav".into(), FileFact::Missing));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_space_is_read_where_takes_will_go() {
        let d = disk(&std::env::temp_dir().join("not-made-yet").join("kabelsalat")).expect("statvfs of the nearest folder");
        assert!(d.free_bytes > 0 && d.dir.ends_with("not-made-yet/kabelsalat"), "{d:?}");
    }
}
