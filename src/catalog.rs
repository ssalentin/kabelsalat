//! Instrument browser catalog (#10): a hand-selected set of instruments, each
//! linked to its GitHub project, installable from the app.
//!
//! Everything here is pure data + pure functions. The install *worker* lives
//! in `ui.rs` (it spawns a thread that runs [`plan`]'s commands off the UI
//! thread, rule 46); the `assistant`/`mcp` tools call the same [`find`],
//! [`installed`] and [`plan`] helpers so all three frontends agree (rule 6).
//!
//! Detection is two-fold (spec): a plugin id / LV2 URI present in the scan
//! results, or the bundle path existing on disk. The filesystem probe never
//! runs lilv or clack — it only stats a handful of known bundle dirs.

use std::path::{Path, PathBuf};

/// Plugin format (drives the badge, the install home and the scan column).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Clap,
    Lv2,
}

impl Format {
    pub const fn label(self) -> &'static str {
        match self {
            Format::Clap => "CLAP",
            Format::Lv2 => "LV2",
        }
    }
}

/// Where a source-built bundle lands (distro packages go to `/usr` instead).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Home {
    Clap,
    Lv2,
}

impl Home {
    pub const fn dir_name(self) -> &'static str {
        match self {
            Home::Clap => ".clap",
            Home::Lv2 => ".lv2",
        }
    }
}

/// How an instrument is obtained.
#[derive(Clone, Copy)]
pub enum InstallRoute {
    /// Prebuilt, signed distro package.
    DistroPackage { pkg: &'static str },
    /// Shallow-cloned and built from GitHub source into the user's plugin dir.
    Source {
        repo: &'static str,
        /// `git clone --recurse-submodules` (DISTRHO-Ports needs vendored JUCE).
        submodules: bool,
        /// Build commands, run in order in the clone dir.
        build: &'static [&'static [&'static str]],
        /// Built artifact, relative to the clone dir, copied into `home`.
        artifact: &'static str,
    },
}

/// One static catalog entry.
#[derive(Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub format: Format,
    pub description: &'static str,
    /// GitHub "owner/repo".
    pub repo: &'static str,
    pub route: InstallRoute,
    pub home: Home,
    /// CLAP plugin id ("installed" when the scan sees it).
    pub clap_id: Option<&'static str>,
    /// LV2 URIs ("installed" when the scan sees any).
    pub lv2_uris: &'static [&'static str],
    /// Bundle dir names for the filesystem probe (`.clap`/`.lv2` suffix kept).
    pub bundles: &'static [&'static str],
}

/// The catalog. Order is the display order (setBfree first, per the spec).
pub static CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "setbfree",
        name: "setBfree",
        format: Format::Lv2,
        description: "Hammond B3 tonewheel organ + rotary",
        repo: "pantherb/setBfree",
        route: InstallRoute::DistroPackage { pkg: "setbfree" },
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &["http://gareus.org/oss/lv2/b_synth"],
        bundles: &["b_synth.lv2", "setBfree.lv2"],
    },
    CatalogEntry {
        id: "surge-xt",
        name: "Surge XT",
        format: Format::Clap,
        description: "Open-source hybrid wavetable synth",
        repo: "surge-synthesizer/surge",
        route: InstallRoute::DistroPackage { pkg: "surge-xt-clap" },
        home: Home::Clap,
        clap_id: Some("org.surge-synth-team.surge-xt"),
        lv2_uris: &[],
        bundles: &["org.surge-synth-team.surge-xt.clap"],
    },
    CatalogEntry {
        id: "dexed",
        name: "Dexed",
        format: Format::Clap,
        description: "Yamaha DX7 FM synthesizer emulation",
        repo: "asb2m10/dexed",
        route: InstallRoute::DistroPackage { pkg: "dexed" },
        home: Home::Clap,
        clap_id: Some("com.digital-suburban.dexed"),
        lv2_uris: &[],
        bundles: &["com.digital-suburban.dexed.clap"],
    },
    CatalogEntry {
        id: "vitalium",
        name: "Vitalium",
        format: Format::Lv2,
        description: "Spectral warping wavetable synth (Vital port)",
        repo: "DISTRHO/DISTRHO-Ports",
        route: InstallRoute::Source {
            repo: "DISTRHO/DISTRHO-Ports",
            submodules: true,
            build: &[
                // README.md "BUILD and INSTALL": meson + ninja. Vitalium only,
                // LV2 only (the juce6.0 port). The source tree ships vendored;
                // the juce6.0 submodule is fetched by --recurse-submodules.
                &[
                    "meson", "setup", "build", "--buildtype", "release", "-Dplugins=vitalium",
                    "-Dbuild-lv2=true", "-Dbuild-vst2=false", "-Dbuild-vst3=false",
                ],
                &["ninja", "-C", "build"],
            ],
            // ports-juce6.0/meson.build: custom_target(plugin_name + "-lv2-bundle",
            // output: plugin_name + ".lv2"), plugin_name = "vitalium"
            // (ports-juce6.0/vitalium/meson.build:70). Meson emits it into
            // <builddir>/<subdir>/<output>.
            artifact: "build/ports-juce6.0/vitalium.lv2",
        },
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &["urn:distrho:vitalium"],
        bundles: &["vitalium.lv2", "Vitalium-unfa.lv2"],
    },
    CatalogEntry {
        id: "yanes",
        name: "YANES",
        format: Format::Clap,
        description: "NES / chiptune multi-chip synthesizer",
        repo: "andrewfader/yanes",
        route: InstallRoute::Source {
            repo: "andrewfader/yanes",
            submodules: false,
            // README.md "Build"/"Local build and install" (lines 481–563):
            // cmake configure + build; copy build/YANES.clap into ~/.clap.
            build: &[
                &["cmake", "-S", ".", "-B", "build", "-DCMAKE_BUILD_TYPE=Release"],
                &["cmake", "--build", "build", "-j"],
            ],
            artifact: "build/YANES.clap",
        },
        home: Home::Clap,
        clap_id: Some("org.yanes.native"),
        lv2_uris: &[],
        bundles: &["YANES.clap"],
    },
];

/// Case-insensitive catalog lookup by id.
pub fn find(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id.eq_ignore_ascii_case(id))
}

/// `https://github.com/<owner/repo>`.
pub fn repo_url(repo: &str) -> String {
    format!("https://github.com/{repo}")
}

/// The user's home directory (empty path when `$HOME` is unset, e.g. tests).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Build cache root for source installs: `~/.cache/kabelsalat/build`.
pub fn cache_root() -> PathBuf {
    home_dir().join(".cache").join("kabelsalat").join("build")
}

/// Plugin bundle search roots: the user dirs plus the system LV2/CLAP dirs a
/// distro package installs into. Pass an explicit set under test.
pub fn plugin_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".clap"),
        home.join(".lv2"),
        PathBuf::from("/usr/lib/clap"),
        PathBuf::from("/usr/lib/lv2"),
        PathBuf::from("/usr/local/lib/lv2"),
    ]
}

/// Evidence distilled from a plugin scan (or empty for the filesystem-only
/// probe the agent tools use headlessly). `Vec<String>` keeps `catalog`
/// free of `host`/`lv2` types so it stays cheap to unit-test (rules 48/37).
#[derive(Default)]
pub struct ScanSet {
    pub clap_ids: Vec<String>,
    pub lv2_uris: Vec<String>,
}

/// "Installed" when the scan saw the plugin id or one of the LV2 URIs.
pub fn scan_installed(entry: &CatalogEntry, scan: &ScanSet) -> bool {
    entry.clap_id.is_some_and(|id| scan.clap_ids.iter().any(|c| c == id))
        || entry.lv2_uris.iter().any(|u| scan.lv2_uris.iter().any(|v| v == u))
}

/// "Installed" when any declared bundle dir exists under any root (the
/// no-rescan fallback).
pub fn bundle_installed(entry: &CatalogEntry, roots: &[&Path]) -> bool {
    entry.bundles.iter().any(|b| roots.iter().any(|r| r.join(b).exists()))
}

/// Combined detection: scan evidence OR a bundle path on disk.
pub fn installed(entry: &CatalogEntry, scan: &ScanSet, roots: &[&Path]) -> bool {
    scan_installed(entry, scan) || bundle_installed(entry, roots)
}

/// One shell command the install worker runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Mutates the system (a `sudo pacman` install), distinct from building
    /// and copying a bundle into the user's plugin dir.
    pub changes_system: bool,
}

/// The exact commands to install an entry. Pure: given the build cache and the
/// home dir it always produces the same argv list — this is what the tests
/// assert and what [`run_install`] executes.
pub fn plan(entry: &CatalogEntry, cache: &Path, home: &Path) -> Vec<InstallCommand> {
    match entry.route {
        InstallRoute::DistroPackage { pkg } => vec![InstallCommand {
            argv: vec![
                "sudo".into(), "-n".into(), "pacman".into(), "-S".into(),
                "--needed".into(), "--noconfirm".into(), pkg.into(),
            ],
            cwd: None,
            changes_system: true,
        }],
        InstallRoute::Source { repo, submodules, build, artifact } => {
            let clone_dir = cache.join(entry.id);
            let mut cmds = Vec::with_capacity(build.len() + 3);

            let mut git = vec!["git".to_string(), "clone".to_string(), "--depth".to_string(), "1".to_string()];
            if submodules {
                git.push("--recurse-submodules".to_string());
            }
            git.push(repo_url(repo));
            git.push(clone_dir.to_string_lossy().into_owned());
            cmds.push(InstallCommand { argv: git, cwd: None, changes_system: false });

            for step in build {
                cmds.push(InstallCommand {
                    argv: step.iter().map(|s| (*s).to_string()).collect(),
                    cwd: Some(clone_dir.clone()),
                    changes_system: false,
                });
            }

            let install_dir = home.join(entry.home.dir_name());
            cmds.push(InstallCommand {
                argv: vec![
                    "mkdir".into(), "-p".into(), install_dir.to_string_lossy().into_owned(),
                ],
                cwd: None,
                changes_system: false,
            });
            cmds.push(InstallCommand {
                argv: vec![
                    "cp".into(), "-r".into(),
                    clone_dir.join(artifact).to_string_lossy().into_owned(),
                    install_dir.to_string_lossy().into_owned() + "/",
                ],
                cwd: None,
                changes_system: false,
            });
            cmds
        }
    }
}

/// Run a single prepared command, streaming its stdout + stderr to `log`.
fn run_one(cmd: &InstallCommand, log: &mut dyn FnMut(String)) -> Result<std::process::ExitStatus, String> {
    use std::io::BufRead;
    let mut c = std::process::Command::new(&cmd.argv[0]);
    c.args(&cmd.argv[1..]);
    if let Some(dir) = &cmd.cwd {
        c.current_dir(dir);
    }
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("failed to run '{}': {e}", cmd.argv[0]))?;

    // stderr has no chunked `lines` on `ChildStderr`; drain it on a thread and
    // fold the (usually short) lines back after stdout is consumed.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let r = std::io::BufReader::new(stderr);
            for line in r.lines().map_while(|l| l.ok()) {
                let _ = tx.send(line);
            }
        })
    });

    if let Some(stdout) = child.stdout.take() {
        let r = std::io::BufReader::new(stdout);
        for line in r.lines().map_while(|l| l.ok()) {
            log(line);
        }
    }
    while let Ok(line) = rx.try_recv() {
        log(line);
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if let Some(t) = stderr_reader {
        let _ = t.join();
    }
    while let Ok(line) = rx.try_recv() {
        log(line);
    }
    Ok(status)
}

fn shell_join(argv: &[String]) -> String {
    argv.join(" ")
}

/// Execute an entry's install commands, streaming a readable log. On the
/// distro route a `sudo -n` failure (password not cached) is reported with the
/// exact command the user can run themselves — passwords are never prompted or
/// handled (rule: install changes the system, confirm first).
pub fn run_install(entry: &CatalogEntry, cache: &Path, home: &Path, log: &mut dyn FnMut(String)) -> Result<(), String> {
    log(format!("installing {} ({})", entry.name, entry.repo));
    for cmd in plan(entry, cache, home) {
        let label = shell_join(&cmd.argv);
        log(format!("$ {label}"));
        match run_one(&cmd, log) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let mut msg = format!(
                    "command failed (exit {status}): {label}\ninstall left incomplete.",
                );
                if let InstallRoute::DistroPackage { pkg } = entry.route {
                    msg.push_str(&format!("\nrun: sudo pacman -S {pkg}"));
                }
                return Err(msg);
            }
            Err(e) => return Err(e),
        }
    }
    log(format!("installed {}", entry.name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_every_entry_is_complete() {
        let mut seen = std::collections::HashSet::new();
        for e in CATALOG {
            assert!(seen.insert(e.id), "duplicate catalog id '{}'", e.id);
            assert!(!e.name.is_empty());
            assert!(!e.description.is_empty());
            assert!(e.repo.contains('/'), "{} repo is owner/repo", e.id);
            match e.route {
                InstallRoute::DistroPackage { pkg } => assert!(!pkg.is_empty(), "{} pkg", e.id),
                InstallRoute::Source { repo, build, artifact, .. } => {
                    assert!(!repo.is_empty());
                    assert!(!build.is_empty(), "{} has build steps", e.id);
                    assert!(!artifact.is_empty(), "{} has an artifact", e.id);
                }
            }
            // Every entry must be detectable one way or another.
            assert!(
                e.clap_id.is_some() || !e.lv2_uris.is_empty() || !e.bundles.is_empty(),
                "{} has a detection signal",
                e.id
            );
        }
        assert_eq!(CATALOG.len(), 5);
        assert_eq!(CATALOG[0].id, "setbfree", "setBfree is first per the spec");
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(find("setBfree").unwrap().id, "setbfree");
        assert_eq!(find("SURGE-XT").unwrap().id, "surge-xt");
        assert!(find("nope").is_none());
    }

    #[test]
    fn scan_detection_matches_ids_and_uris() {
        let scan = ScanSet {
            clap_ids: vec!["org.surge-synth-team.surge-xt".into()],
            lv2_uris: vec!["urn:distrho:vitalium".into()],
        };
        let surge = find("surge-xt").unwrap();
        let vitalium = find("vitalium").unwrap();
        let setbfree = find("setbfree").unwrap();
        assert!(scan_installed(surge, &scan));
        assert!(scan_installed(vitalium, &scan));
        assert!(!scan_installed(setbfree, &scan), "URI not in the scan is not installed");
    }

    #[test]
    fn bundle_detection_matches_temp_dirs() {
        let tmp = std::env::temp_dir().join(format!("ksalat-catalog-{}", std::process::id()));
        let clap = tmp.join(".clap");
        let lv2 = tmp.join(".lv2");
        std::fs::create_dir_all(&clap).unwrap();
        std::fs::create_dir_all(&lv2).unwrap();
        // Fake a source-built YANES bundle.
        std::fs::write(clap.join("YANES.clap"), b"").unwrap();
        let roots: Vec<&Path> = vec![&clap, &lv2];
        let yanes = find("yanes").unwrap();
        let vitalium = find("vitalium").unwrap();
        assert!(bundle_installed(yanes, &roots), "YANES.clap present => installed");
        assert!(!bundle_installed(vitalium, &roots), "no vitalium bundle => not installed");
        // A directory named after the LV2 bundle also counts.
        std::fs::create_dir_all(lv2.join("vitalium.lv2")).unwrap();
        assert!(bundle_installed(vitalium, &roots));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn distro_plan_is_sudo_n_pacman() {
        let setbfree = find("setbfree").unwrap();
        let cmds = plan(setbfree, Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].argv, vec!["sudo", "-n", "pacman", "-S", "--needed", "--noconfirm", "setbfree"]);
        assert!(cmds[0].changes_system);
        assert!(cmds[0].cwd.is_none());
    }

    #[test]
    fn source_plan_clones_builds_and_copies() {
        let yanes = find("yanes").unwrap();
        let cmds = plan(yanes, Path::new("/cache/build"), Path::new("/home/u"));
        assert_eq!(cmds.len(), 5);
        assert_eq!(cmds[0].argv[..4], ["git", "clone", "--depth", "1"][..]);
        assert_eq!(cmds[0].argv[4], "https://github.com/andrewfader/yanes");
        assert_eq!(cmds[0].argv[5], "/cache/build/yanes");
        assert_eq!(cmds[1].argv, ["cmake", "-S", ".", "-B", "build", "-DCMAKE_BUILD_TYPE=Release"]);
        assert_eq!(cmds[1].cwd.as_deref(), Some(Path::new("/cache/build/yanes")));
        assert_eq!(cmds[2].argv, ["cmake", "--build", "build", "-j"]);
        assert_eq!(cmds[3].argv, ["mkdir", "-p", "/home/u/.clap"]);
        assert_eq!(cmds[4].argv, ["cp", "-r", "/cache/build/yanes/build/YANES.clap", "/home/u/.clap/"]);
        assert!(cmds.iter().all(|c| !c.changes_system), "source build never needs root");

        let vitalium = find("vitalium").unwrap();
        let cmds = plan(vitalium, Path::new("/cache/build"), Path::new("/home/u"));
        // clone --recurse-submodules, meson, ninja, mkdir ~/.lv2, cp the bundle.
        assert!(cmds[0].argv.iter().any(|a| a == "--recurse-submodules"));
        assert!(cmds[1].argv.iter().any(|a| a == "meson"));
        assert!(cmds[2].argv.iter().any(|a| a == "ninja"));
        assert_eq!(cmds[3].argv, ["mkdir", "-p", "/home/u/.lv2"]);
        assert_eq!(cmds[4].argv[1], "-r");
        assert!(cmds[4].argv[2].ends_with("build/ports-juce6.0/vitalium.lv2"));
        assert_eq!(cmds[4].argv[3], "/home/u/.lv2/");
    }

    #[test]
    fn repo_url_is_github() {
        assert_eq!(repo_url("pantherb/setBfree"), "https://github.com/pantherb/setBfree");
    }
}