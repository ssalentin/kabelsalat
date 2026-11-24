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
use crate::keyboard::KeyboardType;

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
    /// A prebuilt bundle from a pinned release download (a GitHub release
    /// asset, or the vendor's own file), unpacked into the build cache and
    /// copied into `home`.
    Release {
        /// The only CPU it runs on (`std::env::consts::ARCH` naming).
        arch: &'static str,
        /// The release, as the card names it.
        tag: &'static str,
        /// The `.zip` or `.tar.gz` to download.
        url: &'static str,
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

/// Where a project publishes its source code.
#[derive(Clone, Copy)]
pub enum Source {
    /// GitHub "owner/repo": the card shows its stars, and release and data
    /// routes download from it.
    GitHub(&'static str),
    /// A project page off GitHub (Punk Labs publishes source zips on its site).
    Web(&'static str),
    /// Freeware without published source (Starbreath): the card links only
    /// the vendor's website.
    Closed,
}

impl Source {
    pub fn url(self) -> Option<String> {
        match self {
            Source::GitHub(repo) => Some(repo_url(repo)),
            Source::Web(url) => Some(url.to_string()),
            Source::Closed => None,
        }
    }

    pub fn github(self) -> Option<&'static str> {
        match self {
            Source::GitHub(repo) => Some(repo),
            Source::Web(_) | Source::Closed => None,
        }
    }
}

/// One static catalog entry.
#[derive(Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub format: Format,
    /// Musical role (Instrument vs Effect suite, e.g. Guitarix).
    pub plugin_type: crate::plugins::PluginType,
    /// What kind of instrument it is (#198): `None` for a card that is no
    /// instrument, `Some(Synth)` for a general synth whose loaded patch
    /// decides, a specific type for a dedicated instrument.
    pub instrument: Option<crate::keyboard::KeyboardType>,
    /// One line for the card title row.
    pub description: &'static str,
    /// A few sentences for the card body: what it sounds like and what it needs.
    pub about: &'static str,
    /// The licence as its SPDX id, or a short name when it has none.
    pub license: &'static str,
    /// The project's own website, when it has one besides its source page.
    pub homepage: Option<&'static str>,
    pub source: Source,
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
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::JazzOrgan),
        description: "Hammond B3 tonewheel organ + rotary",
        about: "A tonewheel organ modelled on the Hammond B3, with drawbars, percussion, vibrato and a rotary speaker. kabelsalat gives it its own organ face.",
        license: "GPL-2.0",
        homepage: Some("https://setbfree.org"),
        source: Source::GitHub("pantherb/setBfree"),
        routes: &[InstallRoute::DistroPackage { pkg: "setbfree-lv2" }],
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &["http://gareus.org/oss/lv2/b_synth"],
        // Arch installs the bundle without the .lv2 suffix.
        bundles: &["b_synth", "b_synth.lv2", "setBfree.lv2"],
        data: None,
    },
    CatalogEntry {
        id: "surge-xt",
        name: "Surge XT",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Open-source hybrid wavetable synth",
        about: "A hybrid synth with wavetable, FM, analogue-style and physical-modelling oscillators. Ships with about 3000 patches that kabelsalat can search and load.",
        license: "GPL-3.0",
        homepage: Some("https://surge-synthesizer.github.io"),
        source: Source::GitHub("surge-synthesizer/surge"),
        routes: &[InstallRoute::DistroPackage { pkg: "surge-xt-clap" }],
        home: Home::Clap,
        clap_id: Some("org.surge-synth-team.surge-xt"),
        lv2_uris: &[],
        bundles: &["Surge XT.clap", "org.surge-synth-team.surge-xt.clap"],
        data: None,
    },
    CatalogEntry {
        id: "dexed",
        name: "Dexed",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Yamaha DX7 FM synthesizer emulation",
        about: "An FM synth modelled on the Yamaha DX7, loading the classic SysEx cartridges: electric pianos, basses, bells and brass.",
        license: "GPL-3.0",
        homepage: None,
        source: Source::GitHub("asb2m10/dexed"),
        routes: &[InstallRoute::DistroPackage { pkg: "dexed-clap" }],
        home: Home::Clap,
        clap_id: Some("com.digital-suburban.dexed"),
        lv2_uris: &[],
        bundles: &["Dexed.clap", "com.digital-suburban.dexed.clap"],
        data: None,
    },
    CatalogEntry {
        id: "vitalium",
        name: "Vitalium",
        format: Format::Lv2,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Spectral warping wavetable synth (Vital port)",
        about: "The DISTRHO port of the Vital wavetable synth, with spectral warping and a modulation matrix. Plays Vital patches: drop a .vitalbank or .vital file on the window to add it.",
        license: "GPL-3.0",
        homepage: Some("https://distrho.github.io/DISTRHO-Ports/"),
        source: Source::GitHub("DISTRHO/DISTRHO-Ports"),
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
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "NES / chiptune multi-chip synthesizer",
        about: "A chiptune synth with NES-style pulse, triangle and noise channels plus extra chips, and 146 built-in presets.",
        license: "Source-available",
        homepage: None,
        source: Source::GitHub("andrewfader/yanes"),
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
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Semi-modular 24-voice polyphonic synth",
        about: "Three oscillators with analogue, wavetable, FM and chiptune modes, two filters with Moog, Korg and SEM models, and a modulation matrix.",
        license: "GPL-3.0",
        homepage: Some("https://thewavewarden.com/odin2"),
        source: Source::GitHub("TheWaveWarden/odin2"),
        // Arch packages Odin for both CPUs, but not its factory patches
        // (the data folder below brings them).
        routes: &[InstallRoute::DistroPackage { pkg: "odin2-synthesizer-clap" }],
        home: Home::Clap,
        clap_id: Some("com.thewavewarden.odin2"),
        lv2_uris: &[],
        bundles: &["Odin2.clap"],
        // Odin's factory patches are compiled into the plugin; kabelsalat's
        // sound library reads them from Odin's soundbank folder (rule 94).
        data: Some(RepoData { path: "assets/Soundbanks/Factory Presets", dest: ".local/share/odin2/Soundbanks" }),
    },
    CatalogEntry {
        id: "ob-xf",
        name: "OB-Xf",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Oberheim OB-X / OB-Xa / OB-8 polysynth",
        about: "A virtual-analogue polysynth modelled on Oberheim's OB-X, OB-Xa and OB-8, for brass, pads, stabs and fat basses. The Surge Synth Team's continuation of OB-Xd.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::GitHub("surge-synthesizer/OB-Xf"),
        routes: &[InstallRoute::DistroPackage { pkg: "ob-xf-clap" }],
        home: Home::Clap,
        clap_id: Some("org.surge-synth-team.OB-Xf"),
        lv2_uris: &[],
        bundles: &["OB-Xf.clap"],
        data: None,
    },
    CatalogEntry {
        id: "kr106",
        name: "Ultramaster KR-106",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Roland Juno-106 style polysynth",
        about: "A polysynth inspired by the Roland Juno: one oscillator with saw, pulse and sub, a resonant low-pass filter and the Juno's chorus, all on one panel. Quick to play live.",
        license: "GPL-3.0-only",
        homepage: Some("https://kayrock.org/kr106/"),
        source: Source::GitHub("kayrockscreenprinting/ultramaster_kr106"),
        routes: &[InstallRoute::DistroPackage { pkg: "ultramaster-kr106-clap" }],
        home: Home::Clap,
        clap_id: Some("org.kayrock.kr106"),
        lv2_uris: &[],
        bundles: &["Ultramaster KR-106.clap"],
        data: None,
    },
    CatalogEntry {
        id: "six-sines",
        name: "Six Sines",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Six-operator FM / phase-modulation synth",
        about: "Six sine operators that modulate each other at audio rate, each with its own envelope and LFO, for bells, electric pianos, plucks and basses. A modern take on FM by Surge XT's lead developer, with MPE.",
        license: "GPL-3.0 (source MIT)",
        homepage: None,
        source: Source::GitHub("baconpaul/six-sines"),
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "v1.2.1",
            url: "https://github.com/baconpaul/six-sines/releases/download/v1.2.1/six-sines-Linux-2026-07-20-18ecb36.zip",
            sha256: "9b5353eebfec3e462c80ed8c0f53c965e6262bb38537518159cce71de7eace41",
            artifact: "six-sines_assets/Six Sines.clap",
        }, InstallRoute::Source {
            repo: "baconpaul/six-sines",
            submodules: true,
            // README.md "How to build this software".
            build: &[
                &["cmake", "-Bignore/build", "-DCMAKE_BUILD_TYPE=Release"],
                &["cmake", "--build", "ignore/build", "--config", "Release", "--target", "six-sines_all"],
            ],
            artifact: "ignore/build/six-sines_assets/Six Sines.clap",
        }],
        home: Home::Clap,
        clap_id: Some("org.baconpaul.six-sines"),
        lv2_uris: &[],
        bundles: &["Six Sines.clap"],
        data: None,
    },
    CatalogEntry {
        id: "jc303",
        name: "JC303",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Bass),
        description: "Roland TB-303 acid bass line",
        about: "A model of the Roland TB-303 bass line: saw or square into the 303's squelchy resonant filter, with accent and slide. Put kabelsalat's arpeggiator in front of it for acid lines.",
        license: "GPL-3.0-only",
        homepage: Some("https://midilab.co/jc303/"),
        source: Source::GitHub("midilab/jc303"),
        routes: &[InstallRoute::DistroPackage { pkg: "jc303-clap" }],
        home: Home::Clap,
        clap_id: Some("co.midilab.JC303"),
        lv2_uris: &[],
        bundles: &["JC303.clap"],
        data: None,
    },
    CatalogEntry {
        id: "cardinal",
        name: "Cardinal",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Modular synth: VCV Rack in a rack unit",
        about: "A free and open build of VCV Rack with over a thousand modules: oscillators, filters, sequencers and effects you patch with virtual cables in its own window. kabelsalat hosts Cardinal Synth, the instrument version.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::GitHub("DISTRHO/Cardinal"),
        routes: &[InstallRoute::DistroPackage { pkg: "cardinal-clap" }],
        home: Home::Clap,
        clap_id: Some("studio.kx.distrho.cardinal#synth"),
        lv2_uris: &[],
        // One bundle dir holds the three Cardinal plugins.
        bundles: &["Cardinal.clap/CardinalSynth.clap"],
        data: None,
    },
    CatalogEntry {
        id: "unsampled-piano",
        name: "Unsampled Piano",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Piano),
        description: "Physically modelled acoustic grand piano",
        about: "A concert grand piano modelled from physics in Rust: felt hammers, stiff multi-string choirs coupled at the bridge, a modal soundboard, lid and room reflections, sympathetic resonance, and half-pedal, sostenuto and una corda pedals. No sample download needed.",
        license: "GPL-3.0-or-later",
        homepage: Some("https://unsampled.mgth.fr"),
        source: Source::GitHub("mgth/unsampled-piano"),
        routes: &[
            InstallRoute::Release {
                arch: "x86_64",
                tag: "v0.2.1",
                url: "https://github.com/mgth/unsampled-piano/releases/download/v0.2.1/unsampled-piano-linux-x86_64.zip",
                sha256: "951e2d33e16dfdb2f4c58085b3bc142299fda1cf61ec4b50a44daddbf7ae14fa",
                artifact: "Unsampled Piano.clap",
            },
            InstallRoute::Source {
                repo: "mgth/unsampled-piano",
                submodules: false,
                build: &[&["env", "-u", "CARGO_TARGET_DIR", "cargo", "xtask", "bundle", "piano-plugin", "--release"]],
                artifact: "target/bundled/Unsampled Piano.clap",
            },
        ],
        home: Home::Clap,
        clap_id: Some("fr.mgth.unsampled-piano"),
        lv2_uris: &[],
        bundles: &["Unsampled Piano.clap"],
        // Shipped TOML presets read by the plugin editor from
        // `$XDG_DATA_HOME/unsampled-piano/presets` (same path as upstream's
        // `installer/linux/install.sh` and `tools/install-app.sh`).
        data: Some(RepoData { path: "presets", dest: ".local/share/unsampled-piano" }),
    },
    CatalogEntry {
        id: "ot-keys",
        name: "OneTrick KEYS",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Piano),
        description: "Physically modelled lo-fi piano",
        about: "A piano that is modelled, not sampled: a small download with a warm, slightly worn tone. Fills the acoustic piano slot without gigabytes of samples.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::Web("https://punklabs.com/ot-keys"),
        routes: &[InstallRoute::DistroPackage { pkg: "ot-keys-clap" }],
        home: Home::Clap,
        clap_id: Some("com.punklabs.onetrick.keys"),
        lv2_uris: &[],
        bundles: &["OneTrick KEYS.clap"],
        data: None,
    },
    CatalogEntry {
        id: "ot-chonk",
        name: "OneTrick CHONK",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Bass),
        description: "Physically modelled electric bass",
        about: "A monophonic electric bass guitar modelled from the string up. Good on the left hand of a split keyboard.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::Web("https://punklabs.com/ot-chonk"),
        routes: &[InstallRoute::DistroPackage { pkg: "ot-chonk-clap" }],
        home: Home::Clap,
        clap_id: Some("com.punklabs.onetrick.chonk"),
        lv2_uris: &[],
        bundles: &["OneTrick CHONK.clap"],
        data: None,
    },
    CatalogEntry {
        id: "ot-cryptid",
        name: "OneTrick CRYPTID",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Drums),
        description: "FM drum machine with a DX7 engine",
        about: "Drums synthesised with a Yamaha DX7 style FM engine: kicks, snares, hats and toms, no samples. Plays on the General MIDI drum notes.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::Web("https://punklabs.com/ot-cryptid"),
        routes: &[InstallRoute::DistroPackage { pkg: "ot-cryptid-clap" }],
        home: Home::Clap,
        clap_id: Some("com.punklabs.onetrick.cryptid"),
        lv2_uris: &[],
        bundles: &["OneTrick CRYPTID.clap"],
        data: None,
    },
    CatalogEntry {
        id: "avldrums",
        name: "AVL Drumkits",
        format: Format::Lv2,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Drums),
        description: "Sampled acoustic drum kits",
        about: "Five multi-velocity acoustic kits from the AV Linux drum library (Black Pearl, Red Zeppelin, Blonde Bop, Blonde Bop HR, Buskman's Holiday), samples included. Plays on the General MIDI drum notes.",
        license: "GPL-2.0-or-later",
        homepage: Some("https://x42-plugins.com/x42/x42-avldrums"),
        source: Source::GitHub("x42/avldrums.lv2"),
        routes: &[InstallRoute::Source {
            repo: "x42/avldrums.lv2",
            // robtk (the GUI toolkit) is a submodule.
            submodules: true,
            // Makefile: `make install DESTDIR=... PREFIX=/usr` puts the bundle
            // with its five .sf2 kits in $(DESTDIR)/usr/lib/lv2/avldrums.lv2.
            build: &[&["make"], &["make", "install", "DESTDIR=out", "PREFIX=/usr"]],
            artifact: "out/usr/lib/lv2/avldrums.lv2",
        }],
        home: Home::Lv2,
        clap_id: None,
        // The stereo kits; the "Multi" variants have one output per drum.
        lv2_uris: &[
            "http://gareus.org/oss/lv2/avldrums#BlackPearl",
            "http://gareus.org/oss/lv2/avldrums#RedZeppelin",
            "http://gareus.org/oss/lv2/avldrums#BlondeBop",
            "http://gareus.org/oss/lv2/avldrums#BlondeBopHR",
            "http://gareus.org/oss/lv2/avldrums#BuskmansHoliday",
        ],
        bundles: &["avldrums.lv2"],
        data: None,
    },
    CatalogEntry {
        id: "starbreath",
        name: "Starbreath",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Wind),
        description: "Physically modelled winds for breath controllers",
        about: "Flutes, reeds, saxophone and brass modelled from physics and played from the breath of a wind controller (EWI, Aerophone, Sylphyo; it finds breath on CC2, CC11 or aftertouch), with harmoniser and spaces. Freeware, not open source: free to download until 31 January 2027, and the version you have keeps working. Linux x86-64 only.",
        license: "Proprietary freeware",
        homepage: Some("https://vsti.im-a-bird.com"),
        source: Source::Closed,
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "v0.9.0",
            url: "https://vsti.im-a-bird.com/downloads/v0.9.0/Starbreath-v0.9.0-linux-x64.tar.gz",
            sha256: "74332886c1f2f8e4f58034ad4dda8de60ccca9f600f40b322463c9ebaabb585c",
            artifact: "Starbreath.clap",
        }],
        home: Home::Clap,
        // Not published; the card finds the bundle by its file name.
        clap_id: Some("org.starbreath.starbreath"),
        lv2_uris: &[],
        bundles: &["Starbreath.clap"],
        data: None,
    },
    CatalogEntry {
        id: "floe",
        name: "Floe",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::Synth),
        description: "Sample library player",
        about: "Plays sample libraries in the Floe format, with layered instruments, effects and presets. Its sounds come from libraries installed separately from floe.audio.",
        license: "GPL-3.0-or-later",
        homepage: Some("https://floe.audio"),
        source: Source::GitHub("floe-audio/Floe"),
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "v2.0.5",
            url: "https://github.com/floe-audio/Floe/releases/download/v2.0.5/Floe-CLAP-v2.0.5-Linux.tar.gz",
            sha256: "14e6e7bf684253facb534ae034bc7324772552997017fb03de608a8c6f25d476",
            artifact: "Floe.clap",
        }],
        home: Home::Clap,
        clap_id: Some("com.floe-audio.floe"),
        lv2_uris: &[],
        bundles: &["Floe.clap"],
        data: None,
    },
    CatalogEntry {
        id: "aeolus",
        name: "Aeolus",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Instrument,
        instrument: Some(KeyboardType::PipeOrgan),
        description: "Pipe organ with additive synthesis",
        about: "A church pipe organ built on Fons Adriaensen's Aeolus: additive pipe models with chiff on the attack, several divisions of stops, tremulant and a convolution reverb.",
        license: "GPL-3.0-or-later",
        homepage: None,
        source: Source::GitHub("Archie3d/aeolus_plugin"),
        routes: &[InstallRoute::Release {
            arch: "x86_64",
            tag: "v0.3.0",
            url: "https://github.com/Archie3d/aeolus_plugin/releases/download/v0.3.0/Aeolus-clap-linux-0.3.0.zip",
            sha256: "498170149337c7d2ce19783156522d3bf489f811c2a61b7323b64071882cc370",
            artifact: "Aeolus.clap",
        }, InstallRoute::Source {
            repo: "Archie3d/aeolus_plugin",
            submodules: true,
            // CMakeLists.txt: clap_juce_extensions_plugin(TARGET Aeolus), so
            // the CLAP target is Aeolus_CLAP. JUCE and clap-juce-extensions
            // are submodules.
            build: &[
                &["cmake", "-B", "build", "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release"],
                &["cmake", "--build", "build", "--target", "Aeolus_CLAP"],
            ],
            artifact: "build/Aeolus_artefacts/Release/CLAP/Aeolus.clap",
        }],
        home: Home::Clap,
        clap_id: Some("com.ArthurBenilov.Aeolus"),
        lv2_uris: &[],
        bundles: &["Aeolus.clap"],
        data: None,
    },
    CatalogEntry {
        id: "guitarix",
        name: "Guitarix",
        format: Format::Lv2,
        plugin_type: crate::plugins::PluginType::Effect,
        instrument: None,
        description: "Guitar amp, cabinets and stompbox effects",
        about: "A mono guitar amplifier with a tube-modelled preamp, tone stack and power amp, matched cabinet impulses and a pedalboard of stomps (overdrive, fuzz, delay, reverb, wah, tremolo). kabelsalat's Guitar rig template wires an Audio In through the screamer, the amp and a cabinet — the tuner is on the Audio In unit's face. An effect suite: it has no sound library (its LV2 bundles ship no presets; the amp settings are dialled in on the face).",
        license: "GPL-2.0-or-later",
        homepage: Some("https://guitarix.org"),
        source: Source::GitHub("brummer10/guitarix"),
        routes: &[InstallRoute::DistroPackage { pkg: "guitarix" }],
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: crate::guitar::ALL,
        bundles: &["gx_amp.lv2", "gx_cabinet.lv2", "gx_scream.lv2"],
        data: None,
    },
    CatalogEntry {
        id: "dragonfly-reverb",
        name: "Dragonfly Reverb",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Effect,
        instrument: None,
        description: "Concert hall, room, plate and early-reflections reverbs",
        about: "Four stereo algorithmic reverbs built on Freeverb3 (Hall, Room, Plate and Early Reflections), packaged as native CLAP audio effects. Insert one on a synth or guitar strip, or put Hall on an aux bus and share it across your rigs with sends.",
        license: "GPL-3.0-or-later",
        homepage: Some("https://michaelwillis.github.io/dragonfly-reverb/"),
        source: Source::GitHub("michaelwillis/dragonfly-reverb"),
        routes: &[InstallRoute::DistroPackage { pkg: "dragonfly-reverb-clap" }],
        home: Home::Clap,
        clap_id: Some("michaelwillis.dragonfly.hall"),
        lv2_uris: &[],
        bundles: &[
            "DragonflyHallReverb.clap",
            "DragonflyRoomReverb.clap",
            "DragonflyPlateReverb.clap",
            "DragonflyEarlyReflections.clap",
        ],
        data: None,
    },
    CatalogEntry {
        id: "airwindows",
        name: "Airwindows Consolidated",
        format: Format::Clap,
        plugin_type: crate::plugins::PluginType::Effect,
        instrument: None,
        description: "Hundreds of Chris Johnson's studio processors in one CLAP effect",
        about: "Chris Johnson's entire Airwindows collection — console and tape saturation, reverbs (Galactic, Verbity, kPlate), delays, EQs, dynamics and spatial tools — wrapped into a single searchable CLAP audio effect by Paul Walker (Surge Synth Team).",
        license: "GPL-3.0-or-later (DSP MIT)",
        homepage: Some("https://www.airwindows.com"),
        source: Source::GitHub("baconpaul/airwin2rack"),
        routes: &[
            InstallRoute::Release {
                arch: "x86_64",
                tag: "DAWPlugin",
                url: "https://github.com/baconpaul/airwin2rack/releases/download/DAWPlugin/AirwindowsConsolidated-2026-11-15-59a227e-Linux.zip",
                sha256: "32cb5d1f6cc39128fee901d9e2add7d22a3c9fb2fa67963281aa3c13f0d2d158",
                artifact: "awcons-products/Airwindows Consolidated.clap",
            },
            InstallRoute::Source {
                repo: "baconpaul/airwin2rack",
                submodules: true,
                build: &[
                    &["cmake", "-B", "ignore/daw-plugin", "-DBUILD_JUCE_PLUGIN=TRUE", "-DCMAKE_BUILD_TYPE=Release"],
                    &["cmake", "--build", "ignore/daw-plugin", "--config", "Release", "--target", "airwin-consolidated_CLAP"],
                ],
                artifact: "ignore/daw-plugin/src-juce/airwin-consolidated_artefacts/Release/CLAP/Airwindows Consolidated.clap",
            },
        ],
        home: Home::Clap,
        clap_id: Some("com.airwindows.consolidated"),
        lv2_uris: &[],
        bundles: &["Airwindows Consolidated.clap"],
        data: None,
    },
    CatalogEntry {
        id: "x42-midifilter",
        name: "x42 MIDI filters",
        format: Format::Lv2,
        plugin_type: crate::plugins::PluginType::MidiFx,
        instrument: None,
        description: "36 MIDI tools for the MIDI strips",
        about: "Robin Gareus' MIDI filter set: chord, strum, transpose, key split, scale lock, velocity curves, CC remapping, mono legato and more, each a MIDI unit you put in front of an instrument. For a wind controller: Velocity Scale fixes the velocity, Mono Legato plays one note at a time, Map CC turns breath CC2 into the CC a patch reads. Comes with the rest of the x42 LV2 set (EQ, limiter, meters, tuner).",
        license: "GPL-2.0-or-later",
        homepage: Some("https://x42-plugins.com/x42/x42-midifilter"),
        source: Source::GitHub("x42/midifilter.lv2"),
        routes: &[InstallRoute::DistroPackage { pkg: "x42-plugins-lv2" }],
        home: Home::Lv2,
        clap_id: None,
        lv2_uris: &[
            "http://gareus.org/oss/lv2/midifilter#midichord",
            "http://gareus.org/oss/lv2/midifilter#miditranspose",
            "http://gareus.org/oss/lv2/midifilter#monolegato",
            "http://gareus.org/oss/lv2/midifilter#velocityscale",
            "http://gareus.org/oss/lv2/midifilter#mapcc",
            "http://gareus.org/oss/lv2/midifilter#midistrum",
        ],
        bundles: &["midifilter.lv2"],
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
        "type": e.plugin_type.slug(),
        "description": e.description,
        "about": e.about,
        "license": e.license,
        "homepage": e.homepage,
        "source_url": e.source.url(),
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
/// no-rescan fallback). A bundle built for another CPU does not count: it
/// can never load here (#135).
pub fn bundle_installed(entry: &CatalogEntry, roots: &[&Path]) -> bool {
    entry.bundles.iter().any(|b| {
        roots.iter().any(|r| {
            let path = r.join(b);
            path.exists() && crate::binarch::foreign(&path).is_none()
        })
    })
}

/// Combined detection: scan evidence OR a bundle path on disk.
pub fn installed(entry: &CatalogEntry, scan: &ScanSet, roots: &[&Path]) -> bool {
    scan_installed(entry, scan) || bundle_installed(entry, roots)
}

/// The catalog entries among `ids` that `installed` says are missing, in
/// `ids` order (ids the catalog does not know are skipped; the template
/// tests check every id resolves).
pub fn missing(ids: &[&str], installed: impl Fn(&CatalogEntry) -> bool) -> Vec<&'static CatalogEntry> {
    ids.iter().filter_map(|id| find(id)).filter(|e| !installed(e)).collect()
}

/// One sentence telling the user what to install: "Needs Aeolus and
/// Guitarix — install them from Plugins (F9)."
pub fn install_hint(missing: &[&CatalogEntry]) -> String {
    let names: Vec<&str> = missing.iter().map(|e| e.name).collect();
    let (list, them) = match names.as_slice() {
        [] => return String::new(),
        [one] => (one.to_string(), "it"),
        [rest @ .., last] => (format!("{} and {last}", rest.join(", ")), "them"),
    };
    format!("Needs {list} — install {them} from Plugins (F9).")
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
        InstallRoute::DistroPackage { pkg } => {
            let mut cmds = vec![InstallCommand {
                argv: vec![
                    "sudo".into(), "-n".into(), "pacman".into(), "-S".into(),
                    "--needed".into(), "--noconfirm".into(), pkg.into(),
                ],
                cwd: None,
                changes_system: true,
            }];
            cmds.extend(fetch_data(entry, cache, home));
            cmds
        }
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
        InstallRoute::Release { url, sha256, artifact, .. } => {
            let dir = cache.join(entry.id);
            let asset = url.rsplit('/').next().unwrap_or(url);
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
                    "curl".into(), "-fL".into(), "--retry".into(), "2".into(), "-o".into(), file.clone(), url.into(),
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
            repo_url(entry.source.github().expect("a data folder comes from GitHub (catalog test)")), path_arg(&dir),
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
    log(format!("installing {}", entry.name));
    let route = local_route(entry).ok_or_else(|| {
        format!("{} has no Linux build for this machine ({})", entry.name, std::env::consts::ARCH)
    })?;
    run_commands(&plan(entry, route, cache, home), log).map_err(|mut msg| {
        if let InstallRoute::DistroPackage { pkg } = route {
            msg.push_str(&format!("\nrun: sudo pacman -S {pkg}"));
        }
        msg
    })?;
    log(format!("installed {}", entry.name));
    Ok(())
}

fn run_commands(cmds: &[InstallCommand], log: &mut dyn FnMut(String)) -> Result<(), String> {
    for cmd in cmds {
        let label = shell_join(&cmd.argv);
        log(format!("$ {label}"));
        let status = run_one(cmd, log)?;
        if !status.success() {
            return Err(format!("command failed (exit {status}): {label}\ninstall left incomplete."));
        }
    }
    Ok(())
}

/// `kabelsalat catalog-install-test [id...]`: puts each named card (default:
/// every card whose route here is not a source build) into a scratch dir under
/// `~/.cache` (rule 79) the way its route would, without changing the system,
/// then loads it from there and plays it (rule 116: the bundle just fetched,
/// never another copy on the search paths).
///
/// A source build or release installs into a scratch home. A distro package
/// is downloaded with `pacman -Sp` and unpacked, never installed: no root,
/// and its dependencies are whatever this machine has. A CLAP card must report
/// its catalog id and sound on a probe chord; every URI of an LV2 card must
/// load: an instrument sounds, an effect passes a test tone, a MIDI unit
/// passes a note.
pub fn install_test(ids: &[String]) -> anyhow::Result<()> {
    let entries: Vec<&CatalogEntry> = if ids.is_empty() {
        CATALOG.iter().filter(|e| local_route(e).is_some_and(|r| !matches!(r, InstallRoute::Source { .. }))).collect()
    } else {
        ids.iter().map(|id| find(id).ok_or_else(|| anyhow::anyhow!("no catalog entry '{id}'"))).collect::<anyhow::Result<_>>()?
    };
    let root = home_dir().join(".cache").join("ksalat-catalog-install-test");
    let _ = std::fs::remove_dir_all(&root);
    let (mut tested, mut failed) = (0, Vec::new());
    for entry in entries {
        let Some(route) = local_route(entry) else {
            // A build you were given by hand (a beta, a build for your CPU
            // that is not published) is played where you put it.
            let Some(bundle) = hand_installed(entry) else {
                println!("  {}: no route for {}, skipped", entry.name, std::env::consts::ARCH);
                continue;
            };
            tested += 1;
            println!("  {}: no route for {}, playing the copy installed by hand at {}", entry.name, std::env::consts::ARCH, bundle.display());
            let played = match entry.format {
                Format::Clap => play_clap_card(entry, &bundle),
                Format::Lv2 => play_lv2_card(entry, bundle.parent().expect("a bundle lies in a folder")),
            };
            if let Err(e) = played {
                println!("  {}: FAILED: {e:#}", entry.name);
                failed.push(entry.name);
            }
            continue;
        };
        tested += 1;
        println!("  {}: {}", entry.name, route.summary());
        match fetch_and_play(entry, route, &root.join(entry.id)) {
            Ok(()) => {}
            Err(e) => {
                println!("  {}: FAILED: {e:#}", entry.name);
                failed.push(entry.name);
            }
        }
    }
    anyhow::ensure!(tested > 0, "catalog-install-test: nothing installable here");
    anyhow::ensure!(failed.is_empty(), "catalog-install-test failed: {}", failed.join(", "));
    println!("catalog-install-test: OK ({tested} loaded and heard)");
    Ok(())
}

fn fetch_and_play(entry: &CatalogEntry, route: &InstallRoute, dir: &Path) -> anyhow::Result<()> {
    let log = &mut |l: String| println!("  | {l}");
    let (cache, home) = (dir.join("build"), dir.join("home"));
    let plugins = match *route {
        InstallRoute::DistroPackage { pkg } => {
            let unpacked = dir.join("pkg");
            if let Err(e) = fetch_package(pkg, &unpacked, log) {
                if let Some(bundle) = hand_installed(entry) {
                    log(format!("{e} — playing the copy installed by hand at {}", bundle.display()));
                    return match entry.format {
                        Format::Clap => play_clap_card(entry, &bundle),
                        Format::Lv2 => play_lv2_card(entry, bundle.parent().expect("a bundle lies in a folder")),
                    };
                }
                return Err(e);
            }
            // What the install adds besides the package (Odin's patches).
            let user_steps: Vec<InstallCommand> = plan(entry, route, &cache, &home).into_iter().filter(|c| !c.changes_system).collect();
            run_commands(&user_steps, log).map_err(anyhow::Error::msg)?;
            unpacked.join("usr/lib").join(match entry.home { Home::Clap => "clap", Home::Lv2 => "lv2" })
        }
        _ => {
            run_install(entry, &cache, &home, log).map_err(anyhow::Error::msg)?;
            home.join(entry.home.dir_name())
        }
    };
    if let Some(data) = entry.data {
        let folder = Path::new(data.path).file_name().expect("a data path names a folder");
        let files = files_under(&home.join(data.dest).join(folder));
        let patches: Vec<&PathBuf> = files.iter().filter(|f| f.extension().is_some_and(|x| x == "odin")).collect();
        let unreadable: Vec<String> = patches
            .iter()
            .filter_map(|f| crate::juce::value_tree_to_state(&std::fs::read(f).ok()?).err().map(|e| format!("{}: {e}", f.display())))
            .collect();
        if patches.is_empty() {
            println!("  {}: {} data files under ~/{}/{}", entry.name, files.len(), data.dest, folder.display());
        } else {
            println!("  {}: {} data files under ~/{}, {} .odin patches convert to plugin state", entry.name, files.len(), data.dest, patches.len() - unreadable.len());
        }
        anyhow::ensure!(!files.is_empty() && unreadable.is_empty(), "data: {unreadable:?}");
    }
    match entry.format {
        Format::Clap => play_clap_card(entry, &plugins.join(entry.bundles[0])),
        Format::Lv2 => play_lv2_card(entry, &plugins),
    }
}

/// Download `pkg` (no dependencies) the way pacman would install it and
/// unpack it into `dir`. `pacman -Sp` only prints URLs: it needs no root.
fn fetch_package(pkg: &str, dir: &Path, log: &mut dyn FnMut(String)) -> anyhow::Result<()> {
    let out = std::process::Command::new("pacman").args(["-Sp", "--print-format", "%n %l", pkg]).output()?;
    anyhow::ensure!(out.status.success(), "pacman -Sp {pkg}: {}", String::from_utf8_lossy(&out.stderr).trim());
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut url = None;
    for (name, u) in listing.lines().filter_map(|l| l.split_once(' ')) {
        if name == pkg {
            url = Some(u.to_string());
        } else {
            log(format!("{pkg} needs {name}, not installed here (the test runs without it)"));
        }
    }
    let url = url.ok_or_else(|| anyhow::anyhow!("pacman -Sp lists no package {pkg}"))?;
    let file = dir.join(url.rsplit('/').next().unwrap_or(pkg));
    std::fs::create_dir_all(dir)?;
    run_commands(
        &[
            user_cmd(vec!["curl".into(), "-fsSL".into(), "--retry".into(), "2".into(), "-o".into(), path_arg(&file), url]),
            user_cmd(vec!["tar".into(), "-xf".into(), path_arg(&file), "-C".into(), path_arg(dir)]),
        ],
        log,
    )
    .map_err(anyhow::Error::msg)
}

/// The card's bundle in the user's plugin folder, when one is there.
fn hand_installed(entry: &CatalogEntry) -> Option<PathBuf> {
    let dir = home_dir().join(entry.home.dir_name());
    entry.bundles.iter().map(|b| dir.join(b)).find(|p| p.exists())
}

/// Cards built for a wind controller: their test also blows into them.
const BREATH_CARDS: &[&str] = &["starbreath"];

/// Notes every card should answer: General MIDI kick and snare for the drum
/// machines, middle C for everything else.
const PROBE_CHORD: [u8; 3] = [36, 38, 60];

fn play_clap_card(entry: &CatalogEntry, bundle: &Path) -> anyhow::Result<()> {
    let infos = crate::host::scan_clap_bundle(bundle)?;
    let id = match entry.clap_id {
        Some(id) => {
            anyhow::ensure!(infos.iter().any(|p| p.id == id), "bundle reports {:?}, catalog says {id}", infos.iter().map(|p| &p.id).collect::<Vec<_>>());
            id.to_string()
        }
        None => {
            let p = infos.iter().find(|p| p.features.iter().any(|f| f == "instrument")).ok_or_else(|| anyhow::anyhow!("bundle has no instrument"))?;
            println!("  {}: the bundle's instrument is {}", entry.name, p.id);
            p.id.clone()
        }
    };
    let rate = 48_000.0;
    let (mut voice, main) = crate::host::ClapVoice::new_from_bundle(rate, bundle, &id)?;
    if entry.plugin_type == crate::plugins::PluginType::Effect || voice.is_effect() {
        let peak = clap_test_tone(&mut voice, rate, 512);
        main.retire(voice);
        println!("  {}: {id} loads from {}, effect, test tone peak {peak:.3}", entry.name, bundle.display());
        anyhow::ensure!(peak >= 1e-3, "silent");
        return Ok(());
    }
    let mut buf = vec![0.0f32; 2 * 4800];
    let mut peak = 0.0f32;
    for n in PROBE_CHORD {
        voice.note_on(n.into(), 0.8);
    }
    for _ in 0..10 {
        voice.render(&mut buf, crate::transport::Transport::default());
        peak = buf.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    for n in PROBE_CHORD {
        voice.note_off(n.into());
    }
    let breath = BREATH_CARDS.contains(&entry.id).then(|| breath_response(&mut voice));
    main.retire(voice);
    println!("  {}: {id} loads from {}, probe chord peak {peak:.3}", entry.name, bundle.display());
    anyhow::ensure!(peak >= 1e-3, "silent");
    if let Some([soft, hard, pressure]) = breath {
        println!("  {}: one held note, RMS at breath (CC2) 20: {soft:.4}, at 127: {hard:.4}; channel pressure 127: {pressure:.4}", entry.name);
        anyhow::ensure!(hard > 2.0 * soft, "breath (CC2) does not shape the level");
    }
    Ok(())
}

/// Feed a CLAP effect a 220 Hz tone for half a second; return the output peak.
fn clap_test_tone(voice: &mut crate::host::ClapVoice, rate: f64, block: usize) -> f32 {
    let mut inp = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * 2];
    let mut peak = 0.0f32;
    for b in 0..(rate as usize / block) {
        for i in 0..block {
            let s = (2.0 * std::f32::consts::PI * 220.0 * ((b * block + i) as f32 / rate as f32)).sin() * 0.5;
            inp[i * 2] = s;
            inp[i * 2 + 1] = s;
        }
        voice.process(&inp, &mut out, crate::transport::Transport::default());
        if b > 20 {
            peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
        }
    }
    peak
}

/// RMS of one held note at a soft and a full breath (CC2, how an EWI or
/// Aerophone sends it), then at full channel pressure after CC2 went to 0.
fn breath_response(voice: &mut crate::host::ClapVoice) -> [f32; 3] {
    let mut buf = vec![0.0f32; 2 * 4800];
    let mut hold = |voice: &mut crate::host::ClapVoice, msg: [u8; 3]| {
        let mut sum = 0.0f64;
        for i in 0..10 {
            voice.send_midi_rt(msg);
            voice.render(&mut buf, crate::transport::Transport::default());
            // The second half: what the instrument settled on.
            if i >= 5 {
                sum += buf.iter().map(|s| f64::from(s * s)).sum::<f64>();
            }
        }
        (sum / (5.0 * buf.len() as f64)).sqrt() as f32
    };
    hold(voice, [0xB0, 2, 0]); // let the chord ring out
    voice.note_on(60, 0.8);
    let soft = hold(voice, [0xB0, 2, 20]);
    let hard = hold(voice, [0xB0, 2, 127]);
    hold(voice, [0xB0, 2, 0]);
    let pressure = hold(voice, [0xD0, 127, 0]);
    voice.note_off(60);
    [soft, hard, pressure]
}

/// Load every URI of the card from `plugins` alone (`LV2_PATH`, so a copy in
/// `/usr/lib/lv2` cannot stand in) and play it as what it is.
fn play_lv2_card(entry: &CatalogEntry, plugins: &Path) -> anyhow::Result<()> {
    // Single-threaded here: the test runs one card at a time, before any
    // LV2 thread exists.
    std::env::set_var("LV2_PATH", plugins);
    let (rate, block) = (48_000.0, 512usize);
    let mut silent = Vec::new();
    for uri in entry.lv2_uris {
        let mut p = crate::lv2::Lv2Plugin::load(uri, rate, block)?;
        let (what, level) = if p.is_midi_fx() {
            ("MIDI unit, notes out", midi_through(&mut p, block))
        } else if p.is_instrument() {
            ("instrument, probe chord peak", lv2_probe_chord(&mut p, rate, block))
        } else {
            ("effect, test tone peak", lv2_test_tone(&mut p, rate, block))
        };
        crate::lv2::retire(p); // rule 56: retire, never free
        println!("  {}: {uri}: {what} {level:.3}", entry.name);
        if level < 1e-3 {
            silent.push(*uri);
        }
    }
    anyhow::ensure!(silent.is_empty(), "silent: {silent:?}");
    Ok(())
}

fn lv2_probe_chord(p: &mut crate::lv2::Lv2Plugin, rate: f64, block: usize) -> f32 {
    let inp = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * 2];
    // A sample player loads its kit on its worker thread (AVL Drumkits:
    // 20 to 45 MB of SoundFont), which takes wall-clock time: render in real
    // time for a few seconds before the notes.
    let warm_up = std::time::Instant::now();
    while warm_up.elapsed() < std::time::Duration::from_secs(3) {
        p.process(&inp, &mut out, block);
        std::thread::sleep(std::time::Duration::from_secs_f64(block as f64 / rate));
    }
    for n in PROBE_CHORD {
        p.note_on(n, 100);
    }
    let mut peak = 0.0f32;
    for _ in 0..(rate as usize / block) {
        p.process(&inp, &mut out, block);
        peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    peak
}

/// Feed an effect a 220 Hz tone for half a second; the output peak.
fn lv2_test_tone(p: &mut crate::lv2::Lv2Plugin, rate: f64, block: usize) -> f32 {
    let mut inp = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * 2];
    let mut peak = 0.0f32;
    for b in 0..(rate as usize / block) {
        for i in 0..block {
            let s = (2.0 * std::f32::consts::PI * 220.0 * ((b * block + i) as f32 / rate as f32)).sin() * 0.5;
            inp[i * 2] = s;
            inp[i * 2 + 1] = s;
        }
        p.process(&inp, &mut out, block);
        if b > 20 { // let any attack settle in
            peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
        }
    }
    peak
}

/// Notes a MIDI unit put out for a held middle C over a second (1.0 = at
/// least one; chord or delay units answer after some blocks).
fn midi_through(p: &mut crate::lv2::Lv2Plugin, block: usize) -> f32 {
    let inp = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * 2];
    let mut events = Vec::with_capacity(64);
    p.note_on(60, 100);
    for _ in 0..(48_000 / block) {
        p.process(&inp, &mut out, block);
        p.read_midi_out(&mut events);
        if events.iter().any(|e| matches!(e, crate::lv2::FxOutEvent::NoteOn { .. })) {
            return 1.0;
        }
        events.clear();
    }
    0.0
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .flat_map(|p| if p.is_dir() { files_under(&p) } else { vec![p] })
        .collect()
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
            match e.source {
                Source::GitHub(repo) => assert!(repo.split('/').count() == 2, "{} repo is owner/repo", e.id),
                Source::Web(url) => assert!(url.starts_with("https://"), "{} source page is https", e.id),
                Source::Closed => assert!(e.homepage.is_some(), "{} links at least its website", e.id),
            }
            assert!(e.data.is_none() || e.source.github().is_some(), "{} fetches its data folder from GitHub", e.id);
            assert!(!e.routes.is_empty(), "{} has an install route", e.id);
            for route in e.routes {
                match *route {
                    InstallRoute::DistroPackage { pkg } => assert!(!pkg.is_empty(), "{} pkg", e.id),
                    InstallRoute::Source { repo, build, artifact, .. } => {
                        assert!(!repo.is_empty());
                        assert!(!build.is_empty(), "{} has build steps", e.id);
                        assert!(!artifact.is_empty(), "{} has an artifact", e.id);
                    }
                    InstallRoute::Release { tag, url, sha256, artifact, .. } => {
                        assert!(!tag.is_empty() && !artifact.is_empty(), "{} release", e.id);
                        assert!(url.starts_with("https://"), "{} downloads over https", e.id);
                        assert!(url.ends_with(".zip") || url.ends_with(".tar.gz"), "{} asset unpacks", e.id);
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
        assert_eq!(CATALOG.len(), 23);
        assert_eq!(CATALOG[0].id, "setbfree", "setBfree is first per the spec");
    }

    #[test]
    fn guitarix_is_a_distro_lv2_card_detected_by_uri() {
        let gx = find("guitarix").unwrap();
        // The Arch package is native on both CPUs this project ships for
        // (rule 114: routes best first, a distro package runs everywhere
        // pacman does).
        assert_eq!(route_for(gx, "aarch64").unwrap().kind(), "distro");
        assert_eq!(route_for(gx, "x86_64").unwrap().kind(), "distro");
        assert!(gx.lv2_uris.contains(&crate::guitar::AMP), "the card detects the amp");
        assert!(!gx.lv2_uris.iter().any(|u| u.contains(".monitor")), "a card never points at a monitor");
        let scan = ScanSet { clap_ids: vec![], lv2_uris: vec![crate::guitar::AMP.into()] };
        assert!(scan_installed(gx, &scan), "the background scan marks the card installed");
        let roots: Vec<&Path> = vec![Path::new("/usr/lib/lv2")];
        let present = bundle_installed(gx, &roots);
        if present {
            // On a machine with guitarix actually installed (verified with
            // `file`: aarch64 ELF), the install test checks the URI.
            println!("guitarix bundle present: {}", std::fs::read_dir("/usr/lib/lv2/gx_amp.lv2").is_ok());
        }
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
        // A bundle built for another CPU is on disk and still not installed.
        let (machine, _) = crate::binarch::other_machine();
        std::fs::write(clap.join("YANES.clap"), crate::binarch::fake_elf(machine)).unwrap();
        assert!(!bundle_installed(yanes, &roots), "a foreign YANES.clap cannot load here");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn distro_plan_is_sudo_n_pacman() {
        let setbfree = find("setbfree").unwrap();
        let cmds = plan(setbfree, &setbfree.routes[0], Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].argv, vec!["sudo", "-n", "pacman", "-S", "--needed", "--noconfirm", "setbfree-lv2"]);
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
        let aeolus = find("aeolus").unwrap();
        let release = route_for(aeolus, "x86_64").unwrap();
        assert_eq!(release.kind(), "release", "x86_64 takes the prebuilt release");
        let cmds = plan(aeolus, release, Path::new("/c"), Path::new("/home/u"));
        let argv: Vec<Vec<&str>> = cmds.iter().map(|c| c.argv.iter().map(String::as_str).collect()).collect();
        assert_eq!(argv[0], ["rm", "-rf", "/c/aeolus"]);
        assert_eq!(argv[1], ["mkdir", "-p", "/c/aeolus/unpacked"]);
        assert_eq!(
            argv[2],
            ["curl", "-fL", "--retry", "2", "-o", "/c/aeolus/Aeolus-clap-linux-0.3.0.zip",
             "https://github.com/Archie3d/aeolus_plugin/releases/download/v0.3.0/Aeolus-clap-linux-0.3.0.zip"]
        );
        assert_eq!(argv[3][..3], ["sh", "-c", r#"echo "$0  $1" | sha256sum -c -"#]);
        assert_eq!(argv[3][3..], ["498170149337c7d2ce19783156522d3bf489f811c2a61b7323b64071882cc370", "/c/aeolus/Aeolus-clap-linux-0.3.0.zip"]);
        assert_eq!(argv[4], ["unzip", "-o", "-q", "/c/aeolus/Aeolus-clap-linux-0.3.0.zip", "-d", "/c/aeolus/unpacked"]);
        assert_eq!(argv[5], ["mkdir", "-p", "/home/u/.clap"]);
        assert_eq!(argv[6], ["cp", "-r", "/c/aeolus/unpacked/Aeolus.clap", "/home/u/.clap/"]);
        assert_eq!(argv.len(), 7);
        assert!(cmds.iter().all(|c| !c.changes_system && c.cwd.is_none()));

        let floe = find("floe").unwrap();
        let cmds = plan(floe, route_for(floe, "x86_64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds[4].argv, ["tar", "-xzf", "/c/floe/Floe-CLAP-v2.0.5-Linux.tar.gz", "-C", "/c/floe/unpacked"]);
        assert_eq!(cmds[6].argv, ["cp", "-r", "/c/floe/unpacked/Floe.clap", "/home/u/.clap/"]);
        assert_eq!(cmds.len(), 7, "Floe fetches no extra data");
    }

    #[test]
    fn a_vendor_release_downloads_from_the_vendor_and_installs_only_on_its_cpu() {
        let sb = find("starbreath").unwrap();
        assert!(route_for(sb, "aarch64").is_none(), "Starbreath ships x86-64 Linux builds only");
        let cmds = plan(sb, route_for(sb, "x86_64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(
            cmds[2].argv[5..],
            ["/c/starbreath/Starbreath-v0.9.0-linux-x64.tar.gz", "https://vsti.im-a-bird.com/downloads/v0.9.0/Starbreath-v0.9.0-linux-x64.tar.gz"]
        );
        assert_eq!(cmds[3].argv[3], "74332886c1f2f8e4f58034ad4dda8de60ccca9f600f40b322463c9ebaabb585c");
        assert_eq!(cmds[6].argv, ["cp", "-r", "/c/starbreath/unpacked/Starbreath.clap", "/home/u/.clap/"]);
        let json = entry_json(sb, false, false);
        assert_eq!(json["source_url"], serde_json::Value::Null, "closed freeware has no source link");
        assert_eq!(json["installable_here"], std::env::consts::ARCH == "x86_64");
    }

    #[test]
    fn a_source_off_github_is_linked_but_has_no_stars() {
        let keys = find("ot-keys").unwrap();
        assert_eq!(keys.source.url().as_deref(), Some("https://punklabs.com/ot-keys"));
        assert_eq!(keys.source.github(), None, "the star fetch skips it");
        assert_eq!(find("ob-xf").unwrap().source.github(), Some("surge-synthesizer/OB-Xf"));
    }

    #[test]
    fn a_distro_card_also_fetches_its_data_folder() {
        // Arch's Odin 2 package has no factory patches.
        let odin = find("odin2").unwrap();
        let cmds = plan(odin, route_for(odin, "aarch64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        let argv: Vec<Vec<&str>> = cmds.iter().map(|c| c.argv.iter().map(String::as_str).collect()).collect();
        assert_eq!(argv[0], ["sudo", "-n", "pacman", "-S", "--needed", "--noconfirm", "odin2-synthesizer-clap"]);
        assert_eq!(argv[1], ["rm", "-rf", "/c/odin2-data"]);
        assert_eq!(
            argv[2],
            ["git", "clone", "--depth", "1", "--filter=blob:none", "--sparse", "https://github.com/TheWaveWarden/odin2", "/c/odin2-data"]
        );
        assert_eq!(argv[3], ["git", "-C", "/c/odin2-data", "sparse-checkout", "set", "assets/Soundbanks/Factory Presets"]);
        assert_eq!(argv[4], ["mkdir", "-p", "/home/u/.local/share/odin2/Soundbanks"]);
        assert_eq!(argv[5], ["cp", "-r", "/c/odin2-data/assets/Soundbanks/Factory Presets", "/home/u/.local/share/odin2/Soundbanks/"]);
        assert_eq!(argv.len(), 6);
        assert_eq!(cmds.iter().filter(|c| c.changes_system).count(), 1, "only pacman needs root");
    }

    #[test]
    fn each_machine_takes_the_first_route_that_runs_on_it() {
        let floe = find("floe").unwrap();
        assert!(route_for(floe, "aarch64").is_none(), "Floe ships x86_64 Linux builds only");
        assert_eq!(route_for(find("dexed").unwrap(), "aarch64").unwrap().kind(), "distro");

        let aeolus = find("aeolus").unwrap();
        let cmds = plan(aeolus, route_for(aeolus, "aarch64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert!(cmds[1].argv.iter().any(|a| a == "--recurse-submodules"), "JUCE comes as a submodule");
        assert_eq!(cmds[3].argv, ["cmake", "--build", "build", "--target", "Aeolus_CLAP"]);
        assert_eq!(cmds[5].argv, ["cp", "-r", "/c/aeolus/build/Aeolus_artefacts/Release/CLAP/Aeolus.clap", "/home/u/.clap/"]);

        let six = find("six-sines").unwrap();
        assert_eq!(route_for(six, "x86_64").unwrap().kind(), "release");
        assert_eq!(route_for(six, "aarch64").unwrap().kind(), "source", "no aarch64 release: built here");

        let avl = find("avldrums").unwrap();
        let cmds = plan(avl, route_for(avl, "aarch64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(cmds[2].argv, ["make"]);
        assert_eq!(cmds[3].argv, ["make", "install", "DESTDIR=out", "PREFIX=/usr"]);
        assert_eq!(cmds[5].argv, ["cp", "-r", "/c/avldrums/out/usr/lib/lv2/avldrums.lv2", "/home/u/.lv2/"]);
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

    #[test]
    fn dragonfly_reverb_and_airwindows_are_clap_effect_cards() {
        let df = find("dragonfly-reverb").unwrap();
        assert_eq!(df.format, Format::Clap);
        assert_eq!(df.plugin_type, crate::plugins::PluginType::Effect);
        assert_eq!(df.clap_id, Some("michaelwillis.dragonfly.hall"));
        assert_eq!(route_for(df, "x86_64").unwrap().kind(), "distro");
        assert_eq!(route_for(df, "aarch64").unwrap().kind(), "distro");

        let aw = find("airwindows").unwrap();
        assert_eq!(aw.format, Format::Clap);
        assert_eq!(aw.plugin_type, crate::plugins::PluginType::Effect);
        assert_eq!(aw.clap_id, Some("com.airwindows.consolidated"));
        assert_eq!(route_for(aw, "x86_64").unwrap().kind(), "release");
        assert_eq!(route_for(aw, "aarch64").unwrap().kind(), "source");
        let cmds = plan(aw, route_for(aw, "x86_64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(
            cmds[6].argv,
            ["cp", "-r", "/c/airwindows/unpacked/awcons-products/Airwindows Consolidated.clap", "/home/u/.clap/"]
        );
    }

    #[test]
    fn unsampled_piano_is_a_clap_instrument_with_release_and_source_routes() {
        let piano = find("unsampled-piano").unwrap();
        assert_eq!(piano.format, Format::Clap);
        assert_eq!(piano.plugin_type, crate::plugins::PluginType::Instrument);
        assert_eq!(piano.clap_id, Some("fr.mgth.unsampled-piano"));
        assert_eq!(route_for(piano, "x86_64").unwrap().kind(), "release");
        assert_eq!(route_for(piano, "aarch64").unwrap().kind(), "source");
        let x86 = plan(piano, route_for(piano, "x86_64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(x86[6].argv, ["cp", "-r", "/c/unsampled-piano/unpacked/Unsampled Piano.clap", "/home/u/.clap/"]);
        assert_eq!(x86[9].argv, ["git", "-C", "/c/unsampled-piano-data", "sparse-checkout", "set", "presets"]);
        assert_eq!(x86[11].argv, ["cp", "-r", "/c/unsampled-piano-data/presets", "/home/u/.local/share/unsampled-piano/"]);
        let arm = plan(piano, route_for(piano, "aarch64").unwrap(), Path::new("/c"), Path::new("/home/u"));
        assert_eq!(arm[2].argv, ["env", "-u", "CARGO_TARGET_DIR", "cargo", "xtask", "bundle", "piano-plugin", "--release"]);
        assert_eq!(arm[4].argv, ["cp", "-r", "/c/unsampled-piano/target/bundled/Unsampled Piano.clap", "/home/u/.clap/"]);
        assert_eq!(arm[7].argv, ["git", "-C", "/c/unsampled-piano-data", "sparse-checkout", "set", "presets"]);
        assert_eq!(arm[9].argv, ["cp", "-r", "/c/unsampled-piano-data/presets", "/home/u/.local/share/unsampled-piano/"]);
    }

    #[test]
    fn missing_lists_what_is_not_installed_and_the_hint_names_it() {
        let missing = missing(&["setbfree", "surge-xt", "guitarix"], |e| e.id == "surge-xt");
        assert_eq!(missing.iter().map(|e| e.id).collect::<Vec<_>>(), ["setbfree", "guitarix"]);
        assert_eq!(install_hint(&missing), "Needs setBfree and Guitarix — install them from Plugins (F9).");
        assert_eq!(install_hint(&missing[..1]), "Needs setBfree — install it from Plugins (F9).");
        assert_eq!(install_hint(&[]), "");
        assert!(super::missing(&["aeolus"], |_| true).is_empty());
    }

    #[test]
    fn every_catalog_entry_is_tracked_in_docs_plugins_md() {
        let doc = include_str!("../docs/PLUGINS.md");
        let missing: Vec<&str> = CATALOG
            .iter()
            .map(|e| e.id)
            .filter(|id| !doc.contains(&format!("`{id}`")))
            .collect();
        assert!(
            missing.is_empty(),
            "docs/PLUGINS.md is missing catalog entries: {missing:?}"
        );
    }
}
