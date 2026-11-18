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
    /// A prebuilt bundle from a pinned GitHub release of the entry's repo,
    /// unpacked into the build cache and copied into `home`.
    Release {
        /// The only CPU it runs on (`std::env::consts::ARCH` naming).
        arch: &'static str,
        tag: &'static str,
        /// Release asset file name (`.zip` or `.tar.gz`).
        asset: &'static str,
        /// The asset's SHA-256: a re-uploaded asset under the same name
        /// fails the install instead of being installed.
        sha256: &'static str,
        /// Bundle path inside the unpacked asset.
        artifact: &'static str,
    },
}

/// A folder of the entry's repo the plugin reads at run time (sounds the
/// binary does not carry), fetched with a sparse clone and copied home.
#[derive(Clone, Copy)]
pub struct RepoData {
    /// Folder in the repo; it lands in `dest` under its own name.
    pub path: &'static str,
    /// Directory under `$HOME`.
    pub dest: &'static str,
}

/// One static catalog entry.
#[derive(Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub format: Format,
    /// One line for the card title row.
    pub description: &'static str,
    /// A few sentences for the card body: what it sounds like and what it needs.
    pub about: &'static str,
    /// The licence as its SPDX id, or a short name when it has none.
    pub license: &'static str,
    /// The project's own website, when it has one besides GitHub.
    pub homepage: Option<&'static str>,
    /// GitHub "owner/repo".
    pub repo: &'static str,
    /// Install routes, best first; an install takes the first one that
    /// runs on this machine ([`route_for`]).
    pub routes: &'static [InstallRoute],
    pub home: Home,
    /// CLAP plugin id ("installed" when the scan sees it).
    pub clap_id: Option<&'static str>,
    /// LV2 URIs ("installed" when the scan sees any).
    pub lv2_uris: &'static [&'static str],
    /// Bundle dir names for the filesystem probe (`.clap`/`.lv2` suffix kept).
    pub bundles: &'static [&'static str],
    pub data: Option<RepoData>,
}

/// The catalog. Order is the display order (setBfree first, per the spec).
pub static CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "setbfree",
        name: "setBfree",
        format: Format::Lv2,
        description: "Hammond B3 tonewheel organ + rotary",
        about: "A tonewheel organ modelled on the Hammond B3, with drawbars, percussion, vibrato and a rotary speaker. kabelsalat gives it its own organ face.",
        license: "GPL-2.0",
        homepage: Some("https://setbfree.org"),
        repo: "pantherb/setBfree",
        routes: &[InstallRoute::DistroPackage { pkg: "setbfree" }],
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &["http://gareus.org/oss/lv2/b_synth"],
        bundles: &["b_synth.lv2", "setBfree.lv2"],
        data: None,
    },
    CatalogEntry {
        id: "surge-xt",
        name: "Surge XT",
        format: Format::Clap,
        description: "Open-source hybrid wavetable synth",
        about: "A hybrid synth with wavetable, FM, analogue-style and physical-modelling oscillators. Ships with about 3000 patches that kabelsalat can search and load.",
        license: "GPL-3.0",
        homepage: Some("https://surge-synthesizer.github.io"),
        repo: "surge-synthesizer/surge",
        routes: &[InstallRoute::DistroPackage { pkg: "surge-xt-clap" }],
        home: Home::Clap,
        clap_id: Some("org.surge-synth-team.surge-xt"),
        lv2_uris: &[],
        bundles: &["org.surge-synth-team.surge-xt.clap"],
        data: None,
    },
    CatalogEntry {
        id: "dexed",
        name: "Dexed",
        format: Format::Clap,
        description: "Yamaha DX7 FM synthesizer emulation",
        about: "An FM synth modelled on the Yamaha DX7, loading the classic SysEx cartridges: electric pianos, basses, bells and brass.",
        license: "GPL-3.0",
        homepage: None,
        repo: "asb2m10/dexed",
        routes: &[InstallRoute::DistroPackage { pkg: "dexed" }],
        home: Home::Clap,
        clap_id: Some("com.digital-suburban.dexed"),
        lv2_uris: &[],
        bundles: &["com.digital-suburban.dexed.clap"],
        data: None,
    },
    CatalogEntry {
        id: "vitalium",
        name: "Vitalium",
        format: Format::Lv2,
        description: "Spectral warping wavetable synth (Vital port)",
        about: "The DISTRHO port of the Vital wavetable synth, with spectral warping and a modulation matrix. Plays Vital patches: drop a .vitalbank or .vital file on the window to add it.",
        license: "GPL-3.0",
        homepage: Some("https://distrho.github.io/DISTRHO-Ports/"),
        repo: "DISTRHO/DISTRHO-Ports",
        routes: &[InstallRoute::Source {
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
        }],
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &["urn:distrho:vitalium"],
        bundles: &["vitalium.lv2", "Vitalium-unfa.lv2"],
        data: None,
    },
    CatalogEntry {
        id: "yanes",
        name: "YANES",
        format: Format::Clap,
        description: "NES / chiptune multi-chip synthesizer",
        about: "A chiptune synth with NES-style pulse, triangle and noise channels plus extra chips, and 146 built-in presets.",
        license: "Source-available",
        homepage: None,
        repo: "andrewfader/yanes",
        routes: &[InstallRoute::Source {
            repo: "andrewfader/yanes",
            submodules: false,
            // README.md "Build"/"Local build and install" (lines 481–563):
            // cmake configure + build; copy build/YANES.clap into ~/.clap.
            build: &[
                &["cmake", "-S", ".", "-B", "build", "-DCMAKE_BUILD_TYPE=Release"],
                &["cmake", "--build", "build", "-j"],
            ],
            artifact: "build/YANES.clap",
        }],
        home: Home::Clap,
        clap_id: Some("org.yanes.native"),
        lv2_uris: &[],
        bundles: &["YANES.clap"],
        data: None,
    },
    CatalogEntry {
        id: "odin2",
        name: "Odin 2",
        format: Format::Clap,
        description: "Semi-modular 24-voice polyphonic synth",
        about: "Three oscillators with analogue, wavetable, FM and chiptune modes, two filters with Moog, Korg and SEM models, and a modulation matrix.",
        license: "GPL-3.0",
        homepage: Some("https://thewavewarden.com/odin2"),
        repo: "TheWaveWarden/odin2",
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "NightlyDevel",
            asset: "Odin2.4.1Linux.zip",
            sha256: "462c4fffda99ff91a6c362d8cf41616b50807778ae7a96471753046a799aed63",
            artifact: "Odin2.clap",
        }, InstallRoute::Source {
            repo: "TheWaveWarden/odin2",
            submodules: true,
            // README.md "Build Instructions": cmake configure + build. Only the
            // CLAP target (clap-juce-extensions), not VST3/LV2/Standalone.
            build: &[
                &["cmake", "-B", "build", "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release"],
                &["cmake", "--build", "build", "--target", "Odin2_CLAP"],
            ],
            artifact: "build/Odin2_artefacts/Release/CLAP/Odin2.clap",
        }],
        home: Home::Clap,
        clap_id: Some("com.thewavewarden.odin2"),
        lv2_uris: &[],
        bundles: &["Odin2.clap"],
        // Odin's factory patches are compiled into the plugin; kabelsalat's
        // sound library reads them from Odin's soundbank folder (rule 94).
        data: Some(RepoData { path: "assets/Soundbanks/Factory Presets", dest: ".local/share/odin2/Soundbanks" }),
    },
    CatalogEntry {
        id: "floe",
        name: "Floe",
        format: Format::Clap,
        description: "Sample library player",
        about: "Plays sample libraries in the Floe format, with layered instruments, effects and presets. Its sounds come from libraries installed separately from floe.audio.",
        license: "GPL-3.0-or-later",
        homepage: Some("https://floe.audio"),
        repo: "floe-audio/Floe",
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "v2.0.5",
            asset: "Floe-CLAP-v2.0.5-Linux.tar.gz",
            sha256: "14e6e7bf684253facb534ae034bc7324772552997017fb03de608a8c6f25d476",
            artifact: "Floe.clap",
        }],
        home: Home::Clap,
        clap_id: Some("com.floe-audio.floe"),
        lv2_uris: &[],
        bundles: &["Floe.clap"],
        data: None,
    },
];

impl InstallRoute {
    /// Can this route install on a machine of CPU `arch`?
    pub fn runs_on(&self, arch: &str) -> bool {
        match self {
            InstallRoute::Release { arch: a, .. } => *a == arch,
            InstallRoute::DistroPackage { .. } | InstallRoute::Source { .. } => true,
        }
    }

    /// Short name for the agent tools ("distro" / "source" / "release").
    pub const fn kind(&self) -> &'static str {
        match self {
            InstallRoute::DistroPackage { .. } => "distro",
            InstallRoute::Source { .. } => "source",
            InstallRoute::Release { .. } => "release",
        }
    }

    /// One line for the card: how the install gets the plugin.
    pub fn summary(&self) -> String {
        match self {
            InstallRoute::DistroPackage { pkg } => format!("Arch package {pkg}"),
            InstallRoute::Source { .. } => "Built from source".to_string(),
            InstallRoute::Release { tag, .. } => format!("Prebuilt release {tag}"),
        }
    }
}

/// The first route of `entry` that installs on CPU `arch`.
pub fn route_for(entry: &CatalogEntry, arch: &str) -> Option<&'static InstallRoute> {
    entry.routes.iter().find(|r| r.runs_on(arch))
}

/// [`route_for`] this machine.
pub fn local_route(entry: &CatalogEntry) -> Option<&'static InstallRoute> {
    route_for(entry, std::env::consts::ARCH)
}

/// Case-insensitive catalog lookup by id.
pub fn find(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id.eq_ignore_ascii_case(id))
}

/// `https://github.com/<owner/repo>`.
pub fn repo_url(repo: &str) -> String {
    format!("https://github.com/{repo}")
}

/// GitHub's star count for `repo` ("owner/repo"), read live from the REST
/// API. None when offline, rate-limited or on any other failure: the card
/// then shows no stars at all.
pub fn fetch_stars(client: &reqwest::blocking::Client, repo: &str) -> Option<u32> {
    let body = client
        .get(format!("https://api.github.com/repos/{repo}"))
        .header(reqwest::header::USER_AGENT, "kabelsalat")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .ok()?;
    parse_stars(&body)
}

pub fn parse_stars(body: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("stargazers_count")?.as_u64()?.try_into().ok()
}

/// GitHub's own short form: 235, 1.2k, 12k.
pub fn stars_label(n: u32) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..10_000 => format!("{:.1}k", f64::from(n) / 1000.0).replace(".0k", "k"),
        _ => format!("{}k", n / 1000),
    }
}

/// One entry as the agent tools (`list_instruments`, app and MCP) report it.
pub fn entry_json(e: &CatalogEntry, installed: bool, installing: bool) -> serde_json::Value {
    let route = local_route(e);
    serde_json::json!({
        "id": e.id,
        "name": e.name,
        "format": e.format.label(),
        "description": e.description,
        "about": e.about,
        "license": e.license,
        "homepage": e.homepage,
        "source_url": repo_url(e.repo),
        "route": route.map(InstallRoute::kind),
        "installable_here": route.is_some(),
        "installed": installed,
        "installing": installing,
    })
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
pub fn plan(entry: &CatalogEntry, route: &InstallRoute, cache: &Path, home: &Path) -> Vec<InstallCommand> {
    match *route {
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
            let mut cmds = vec![fresh_dir(&clone_dir)];

            let mut git = vec!["git".to_string(), "clone".to_string(), "--depth".to_string(), "1".to_string()];
            if submodules {
                git.push("--recurse-submodules".to_string());
            }
            git.push(repo_url(repo));
            git.push(clone_dir.to_string_lossy().into_owned());
            cmds.push(user_cmd(git));

            for step in build {
                cmds.push(InstallCommand {
                    argv: step.iter().map(|s| (*s).to_string()).collect(),
                    cwd: Some(clone_dir.clone()),
                    changes_system: false,
                });
            }

            cmds.extend(copy_into_home(entry, &clone_dir.join(artifact), home));
            cmds.extend(fetch_data(entry, cache, home));
            cmds
        }
        InstallRoute::Release { tag, asset, sha256, artifact, .. } => {
            let dir = cache.join(entry.id);
            let file = path_arg(&dir.join(asset));
            let unpacked = dir.join("unpacked");
            let unpack: Vec<String> = if asset.ends_with(".zip") {
                vec!["unzip".into(), "-o".into(), "-q".into(), file.clone(), "-d".into(), path_arg(&unpacked)]
            } else {
                vec!["tar".into(), "-xzf".into(), file.clone(), "-C".into(), path_arg(&unpacked)]
            };
            let mut cmds = vec![
                fresh_dir(&dir),
                user_cmd(vec!["mkdir".into(), "-p".into(), path_arg(&unpacked)]),
                user_cmd(vec![
                    "curl".into(), "-fL".into(), "--retry".into(), "2".into(), "-o".into(), file.clone(),
                    format!("{}/releases/download/{tag}/{asset}", repo_url(entry.repo)),
                ]),
                // The hash and path go in as $0/$1, never spliced into the script.
                user_cmd(vec![
                    "sh".into(), "-c".into(), r#"echo "$0  $1" | sha256sum -c -"#.into(), sha256.into(), file,
                ]),
                user_cmd(unpack),
            ];
            cmds.extend(copy_into_home(entry, &unpacked.join(artifact), home));
            cmds.extend(fetch_data(entry, cache, home));
            cmds
        }
    }
}

/// `rm -rf` a cache dir about to be cloned into: a retry after a failed
/// install finds the old clone there, and `git clone` refuses a used dir.
fn fresh_dir(dir: &Path) -> InstallCommand {
    user_cmd(vec!["rm".into(), "-rf".into(), path_arg(dir)])
}

/// Sparse-clone the entry's [`RepoData`] folder and copy it home.
fn fetch_data(entry: &CatalogEntry, cache: &Path, home: &Path) -> Vec<InstallCommand> {
    let Some(data) = entry.data else { return Vec::new() };
    let dir = cache.join(format!("{}-data", entry.id));
    let dest = home.join(data.dest);
    vec![
        fresh_dir(&dir),
        user_cmd(vec![
            "git".into(), "clone".into(), "--depth".into(), "1".into(), "--filter=blob:none".into(), "--sparse".into(),
            repo_url(entry.repo), path_arg(&dir),
        ]),
        user_cmd(vec!["git".into(), "-C".into(), path_arg(&dir), "sparse-checkout".into(), "set".into(), data.path.into()]),
        user_cmd(vec!["mkdir".into(), "-p".into(), path_arg(&dest)]),
        user_cmd(vec!["cp".into(), "-r".into(), path_arg(&dir.join(data.path)), path_arg(&dest) + "/"]),
    ]
}

fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn user_cmd(argv: Vec<String>) -> InstallCommand {
    InstallCommand { argv, cwd: None, changes_system: false }
}

/// `mkdir -p ~/.clap` (or `~/.lv2`) and copy the built bundle into it.
fn copy_into_home(entry: &CatalogEntry, bundle: &Path, home: &Path) -> [InstallCommand; 2] {
    let install_dir = home.join(entry.home.dir_name());
    [
        user_cmd(vec!["mkdir".into(), "-p".into(), path_arg(&install_dir)]),
        user_cmd(vec!["cp".into(), "-r".into(), path_arg(bundle), path_arg(&install_dir) + "/"]),
    ]
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
    let route = local_route(entry).ok_or_else(|| {
        format!("{} has no Linux build for this machine ({})", entry.name, std::env::consts::ARCH)
    })?;
    for cmd in plan(entry, route, cache, home) {
        let label = shell_join(&cmd.argv);
        log(format!("$ {label}"));
        match run_one(&cmd, log) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                let mut msg = format!(
                    "command failed (exit {status}): {label}\ninstall left incomplete.",
                );
                if let InstallRoute::DistroPackage { pkg } = route {
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

/// `kabelsalat catalog-install-test [id...]`: installs each named entry
/// (default: Odin 2 and Floe) through the route this machine takes, into a
/// scratch home under `~/.cache` (rule 79), then loads the bundle in the CLAP
/// host, checks its plugin id and plays a note. Downloads from GitHub; a
/// source route builds, which takes minutes.
pub fn install_test(ids: &[String]) -> anyhow::Result<()> {
    let ids: Vec<&str> = if ids.is_empty() { vec!["odin2", "floe"] } else { ids.iter().map(String::as_str).collect() };
    let root = home_dir().join(".cache").join("ksalat-catalog-install-test");
    let _ = std::fs::remove_dir_all(&root);
    let (cache, home) = (root.join("build"), root.join("home"));
    let (mut tested, mut failed) = (0, Vec::new());
    for id in ids {
        let entry = find(id).ok_or_else(|| anyhow::anyhow!("no catalog entry '{id}'"))?;
        let Some(route) = local_route(entry) else {
            println!("  {}: no route for {}, skipped", entry.name, std::env::consts::ARCH);
            continue;
        };
        let Some(clap_id) = entry.clap_id else {
            println!("  {}: not a CLAP plugin, skipped", entry.name);
            continue;
        };
        if let InstallRoute::DistroPackage { .. } = route {
            println!("  {}: distro package, skipped (changes the system)", entry.name);
            continue;
        }
        tested += 1;
        println!("  {}: {}", entry.name, route.summary());
        if let Err(e) = run_install(entry, &cache, &home, &mut |l| println!("  | {l}")) {
            println!("  {}: install FAILED: {e}", entry.name);
            failed.push(entry.name);
            continue;
        }
        let bundle = home.join(".clap").join(entry.bundles[0]);
        let ids: Vec<String> = crate::host::scan_clap_bundle(&bundle)?.into_iter().map(|p| p.id).collect();
        if !ids.iter().any(|i| i == clap_id) {
            println!("  {}: bundle reports {ids:?}, catalog says {clap_id}", entry.name);
            failed.push(entry.name);
            continue;
        }
        if let Some(data) = entry.data {
            let folder = Path::new(data.path).file_name().expect("a data path names a folder");
            let files = files_under(&home.join(data.dest).join(folder));
            let patches: Vec<&PathBuf> = files.iter().filter(|f| f.extension().is_some_and(|x| x == "odin")).collect();
            let unreadable: Vec<String> = patches
                .iter()
                .filter_map(|f| crate::juce::value_tree_to_state(&std::fs::read(f).ok()?).err().map(|e| format!("{}: {e}", f.display())))
                .collect();
            println!("  {}: {} data files under ~/{}, {} .odin patches convert to plugin state", entry.name, files.len(), data.dest, patches.len() - unreadable.len());
            if files.is_empty() || !unreadable.is_empty() {
                println!("  {}: data FAILED: {unreadable:?}", entry.name);
                failed.push(entry.name);
            }
        }
        let peak = play_one_note(&bundle, clap_id)?;
        println!("  {}: installed, scan sees {clap_id}, middle C peak {peak:.3}", entry.name);
        if peak < 1e-3 {
            failed.push(entry.name);
        }
    }
    anyhow::ensure!(tested > 0, "catalog-install-test: nothing installable here");
    anyhow::ensure!(failed.is_empty(), "catalog-install-test failed: {}", failed.join(", "));
    println!("catalog-install-test: OK ({tested} installed, found by the scan and heard)");
    Ok(())
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .flat_map(|p| if p.is_dir() { files_under(&p) } else { vec![p] })
        .collect()
}

/// Peak of one second of middle C on a fresh voice of `plugin_id`, loaded
/// from `bundle` (never an installed copy elsewhere on the search paths).
fn play_one_note(bundle: &Path, plugin_id: &str) -> anyhow::Result<f32> {
    let rate = 48_000.0;
    let (mut voice, main) = crate::host::ClapVoice::new_from_bundle(rate, bundle, plugin_id)?;
    let mut buf = vec![0.0f32; 2 * 4800];
    let mut peak = 0.0f32;
    voice.note_on(60, 0.8);
    for _ in 0..10 {
        voice.render(&mut buf, crate::transport::Transport::default());
        peak = buf.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    voice.note_off(60);
    main.retire(voice);
    Ok(peak)
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
            assert!(!e.about.is_empty(), "{} has an about text", e.id);
            assert!(!e.license.is_empty(), "{} names its licence", e.id);
            assert!(e.homepage.is_none_or(|h| h.starts_with("https://")), "{} homepage is https", e.id);
            assert!(e.repo.contains('/'), "{} repo is owner/repo", e.id);
            assert!(!e.routes.is_empty(), "{} has an install route", e.id);
            for route in e.routes {
                match *route {
                    InstallRoute::DistroPackage { pkg } => assert!(!pkg.is_empty(), "{} pkg", e.id),
                    InstallRoute::Source { repo, build, artifact, .. } => {
                        assert!(!repo.is_empty());
                        assert!(!build.is_empty(), "{} has build steps", e.id);
                        assert!(!artifact.is_empty(), "{} has an artifact", e.id);
                    }
                    InstallRoute::Release { tag, asset, sha256, artifact, .. } => {
                        assert!(!tag.is_empty() && !artifact.is_empty(), "{} release", e.id);
                        assert!(asset.ends_with(".zip") || asset.ends_with(".tar.gz"), "{} asset unpacks", e.id);
                        assert!(sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()), "{} sha256", e.id);
                    }
                }
            }
            // Every entry must be detectable one way or another.
            assert!(
                e.clap_id.is_some() || !e.lv2_uris.is_empty() || !e.bundles.is_empty(),
                "{} has a detection signal",
                e.id
            );
        }
        assert_eq!(CATALOG.len(), 7);
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
        let cmds = plan(setbfree, &setbfree.routes[0], Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].argv, vec!["sudo", "-n", "pacman", "-S", "--needed", "--noconfirm", "setbfree"]);
        assert!(cmds[0].changes_system);
        assert!(cmds[0].cwd.is_none());
    }

    #[test]
    fn source_plan_clones_builds_and_copies() {
        let yanes = find("yanes").unwrap();
        let cmds = plan(yanes, &yanes.routes[0], Path::new("/cache/build"), Path::new("/home/u"));
        assert_eq!(cmds.len(), 6);
        assert_eq!(cmds[0].argv, ["rm", "-rf", "/cache/build/yanes"], "a retry starts from a fresh clone");
        assert_eq!(cmds[1].argv[..4], ["git", "clone", "--depth", "1"][..]);
        assert_eq!(cmds[1].argv[4], "https://github.com/andrewfader/yanes");
        assert_eq!(cmds[1].argv[5], "/cache/build/yanes");
        assert_eq!(cmds[2].argv, ["cmake", "-S", ".", "-B", "build", "-DCMAKE_BUILD_TYPE=Release"]);
        assert_eq!(cmds[2].cwd.as_deref(), Some(Path::new("/cache/build/yanes")));
        assert_eq!(cmds[3].argv, ["cmake", "--build", "build", "-j"]);
        assert_eq!(cmds[4].argv, ["mkdir", "-p", "/home/u/.clap"]);
        assert_eq!(cmds[5].argv, ["cp", "-r", "/cache/build/yanes/build/YANES.clap", "/home/u/.clap/"]);
        assert!(cmds.iter().all(|c| !c.changes_system), "source build never needs root");

        let vitalium = find("vitalium").unwrap();
        let cmds = plan(vitalium, &vitalium.routes[0], Path::new("/cache/build"), Path::new("/home/u"));
        // rm, clone --recurse-submodules, meson, ninja, mkdir ~/.lv2, cp the bundle.
        assert!(cmds[1].argv.iter().any(|a| a == "--recurse-submodules"));
        assert!(cmds[2].argv.iter().any(|a| a == "meson"));
        assert!(cmds[3].argv.iter().any(|a| a == "ninja"));
        assert_eq!(cmds[4].argv, ["mkdir", "-p", "/home/u/.lv2"]);
        assert_eq!(cmds[5].argv[1], "-r");
        assert!(cmds[5].argv[2].ends_with("build/ports-juce6.0/vitalium.lv2"));
        assert_eq!(cmds[5].argv[3], "/home/u/.lv2/");
    }

    #[test]
    fn release_plan_downloads_verifies_unpacks_and_copies() {
        let odin = find("odin2").unwrap();
        let release = route_for(odin, "x86_64").unwrap();
        assert_eq!(release.kind(), "release", "x86_64 takes the prebuilt release");
        let cmds = plan(odin, release, Path::new("/c"), Path::new("/home/u"));
        let argv: Vec<Vec<&str>> = cmds.iter().map(|c| c.argv.iter().map(String::as_str).collect()).collect();
        assert_eq!(argv[0], ["rm", "-rf", "/c/odin2"]);
        assert_eq!(argv[1], ["mkdir", "-p", "/c/odin2/unpacked"]);
        assert_eq!(
            argv[2],
            ["curl", "-fL", "--retry", "2", "-o", "/c/odin2/Odin2.4.1Linux.zip",
             "https://github.com/TheWaveWarden/odin2/releases/download/NightlyDevel/Odin2.4.1Linux.zip"]
        );
        assert_eq!(argv[3][..3], ["sh", "-c", r#"echo "$0  $1" | sha256sum -c -"#]);
        assert_eq!(argv[3][3..], ["462c4fffda99ff91a6c362d8cf41616b50807778ae7a96471753046a799aed63", "/c/odin2/Odin2.4.1Linux.zip"]);
        assert_eq!(argv[4], ["unzip", "-o", "-q", "/c/odin2/Odin2.4.1Linux.zip", "-d", "/c/odin2/unpacked"]);
        assert_eq!(argv[5], ["mkdir", "-p", "/home/u/.clap"]);
        assert_eq!(argv[6], ["cp", "-r", "/c/odin2/unpacked/Odin2.clap", "/home/u/.clap/"]);
        // The factory patches the release zip does not carry.
        assert_eq!(argv[7], ["rm", "-rf", "/c/odin2-data"]);
        assert_eq!(
            argv[8],
            ["git", "clone", "--depth", "1", "--filter=blob:none", "--sparse", "https://github.com/TheWaveWarden/odin2", "/c/odin2-data"]
        );
        assert_eq!(argv[9], ["git", "-C", "/c/odin2-data", "sparse-checkout", "set", "assets/Soundbanks/Factory Presets"]);
        assert_eq!(argv[10], ["mkdir", "-p", "/home/u/.local/share/odin2/Soundbanks"]);
        assert_eq!(
            argv[11],
            ["cp", "-r", "/c/odin2-data/assets/Soundbanks/Factory Presets", "/home/u/.local/share/odin2/Soundbanks/"]
        );
        assert_eq!(argv.len(), 12);
        assert!(cmds.iter().all(|c| !c.changes_system && c.cwd.is_none()));

        let floe = find("floe").unwrap();
        let cmds = plan(floe, route_for(floe, "x86_64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds[4].argv, ["tar", "-xzf", "/c/floe/Floe-CLAP-v2.0.5-Linux.tar.gz", "-C", "/c/floe/unpacked"]);
        assert_eq!(cmds[6].argv, ["cp", "-r", "/c/floe/unpacked/Floe.clap", "/home/u/.clap/"]);
        assert_eq!(cmds.len(), 7, "Floe fetches no extra data");
    }

    #[test]
    fn each_machine_takes_the_first_route_that_runs_on_it() {
        let odin = find("odin2").unwrap();
        let arm = route_for(odin, "aarch64").unwrap();
        assert_eq!(arm.kind(), "source", "aarch64 builds Odin 2 from source");
        let cmds = plan(odin, arm, Path::new("/c"), Path::new("/home/u"));
        assert!(cmds[1].argv.iter().any(|a| a == "--recurse-submodules"), "JUCE comes as a submodule");
        assert_eq!(cmds[5].argv[2], "/c/odin2/build/Odin2_artefacts/Release/CLAP/Odin2.clap");
        assert!(cmds.last().unwrap().argv[2].ends_with("assets/Soundbanks/Factory Presets"), "then the factory patches");

        let floe = find("floe").unwrap();
        assert!(route_for(floe, "aarch64").is_none(), "Floe ships x86_64 Linux builds only");
        assert_eq!(route_for(find("dexed").unwrap(), "aarch64").unwrap().kind(), "distro");
    }

    #[test]
    fn repo_url_is_github() {
        assert_eq!(repo_url("pantherb/setBfree"), "https://github.com/pantherb/setBfree");
    }

    #[test]
    fn stars_come_from_the_real_github_answer() {
        // Trimmed from `curl https://api.github.com/repos/pantherb/setBfree` (rule 63).
        let ok = r#"{
  "id": 5001593,
  "name": "setBfree",
  "full_name": "pantherb/setBfree",
  "private": false,
  "stargazers_count": 235,
  "watchers_count": 235,
  "forks_count": 44
}"#;
        assert_eq!(parse_stars(ok), Some(235));
        let limited = r#"{"message":"API rate limit exceeded for 0.0.0.0. (But here's the good news: Authenticated requests get a higher rate limit. Check out the documentation for more details.)","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"}"#;
        assert_eq!(parse_stars(limited), None);
        assert_eq!(parse_stars("<html>proxy login</html>"), None);
    }

    #[test]
    fn star_counts_read_like_github() {
        assert_eq!(stars_label(0), "0");
        assert_eq!(stars_label(235), "235");
        assert_eq!(stars_label(999), "999");
        assert_eq!(stars_label(1000), "1k");
        assert_eq!(stars_label(1249), "1.2k");
        assert_eq!(stars_label(3870), "3.9k");
        assert_eq!(stars_label(9999), "10k");
        assert_eq!(stars_label(12_345), "12k");
    }
}