//! Sound libraries: what an instrument can be asked to sound like, one
//! library per instrument a rig can hold.
//!
//! The instrument (`Instrument`) is the key and `Instrument::source` the
//! table that says where its sounds come from; every entry (`Sound`) carries
//! how it is loaded (`Load`). Scans walk directories, parse preset files or
//! instantiate a plugin, so `Libraries` runs them on their own thread and
//! keeps the results (rules 46, 57).

use crate::control::{name_str, Command, CommandBus, NodeId, NodeKind};
use crate::host::{ClapVoiceMain, ParamDesc, VoiceController};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const SURGE_ID: &str = "org.surge-synth-team.surge-xt";
pub const DEXED_ID: &str = "com.digital-suburban.dexed";
pub const ODIN_ID: &str = "com.thewavewarden.odin2";

/// The instrument a rig node hosts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Instrument {
    /// A CLAP plugin, by plugin id.
    Clap(String),
    /// An LV2 plugin, by URI.
    Lv2(String),
}

impl Instrument {
    /// The plugin a node hosts; None for every other node kind.
    pub fn of(kind: &NodeKind) -> Option<Instrument> {
        match kind {
            NodeKind::Clap { plugin_id } => {
                let id = name_str(plugin_id);
                // Graphs written before plugin ids name Surge by its label.
                Some(Instrument::Clap(if id.is_empty() || id == "Surge XT" { SURGE_ID.to_string() } else { id.to_string() }))
            }
            NodeKind::Lv2 { uri } => Some(Instrument::Lv2(uri.as_str().to_string())),
            _ => None,
        }
    }
}

/// How one sound gets into its instrument.
#[derive(Clone, Debug, PartialEq)]
pub enum Load {
    /// A Surge `.fxp` patch: its chunk is the plugin's CLAP state.
    SurgePatch(PathBuf),
    /// One voice of a DX7 cartridge, written into Dexed's CLAP state.
    DexedVoice { syx: PathBuf, program: usize },
    /// A binary JUCE `ValueTree` patch (Odin 2's `.odin`), loaded as the
    /// plugin's CLAP state (`juce::value_tree_to_state`).
    ValueTreePatch(PathBuf),
    /// A Vital patch (`.vital`), restored as Vitalium's state on the
    /// engine's LV2 loader thread.
    VitalPatch(PathBuf),
    /// An installed `pset:Preset`, applied by the engine's LV2 loader thread.
    Lv2Preset(String),
    /// A MIDI programme change, sent to the instrument by the engine.
    MidiProgram(u8),
    /// One value of the plugin's own preset parameter.
    ParamValue { param: u32, value: f64 },
    /// An Aeolus registration, written as the plugin's CLAP state.
    AeolusRegistration(&'static crate::aeolus::Registration),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    /// Folder, bank or group; empty when the library is flat.
    pub category: String,
    pub name: String,
    /// Wins a tie in `find` (Surge's curated factory patches).
    pub preferred: bool,
    pub load: Load,
}

impl Sound {
    /// What a search lists and `find` matches exactly.
    pub fn label(&self) -> String {
        if self.category.is_empty() { self.name.clone() } else { format!("{}/{}", self.category, self.name) }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Library {
    /// What the entries are, in the words a tool result uses ("Surge patches").
    pub kind: String,
    pub sounds: Vec<Sound>,
    /// What the instrument is shaped with when it has no sounds to browse.
    pub params: Vec<String>,
}

/// Where an instrument's sounds come from: the one table of libraries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `.fxp` files under Surge's patch folders.
    SurgePatches,
    /// DX7 `.syx` cartridges in Dexed's folder.
    DexedCartridges,
    /// `.odin` patches in Odin 2's soundbank folder. The factory ones are
    /// compiled into the plugin, so the catalog install copies them there.
    OdinSoundbanks,
    /// Registrations of Aeolus's built-in organ, kabelsalat's own table
    /// (`aeolus::REGISTRATIONS`): the plugin has no presets.
    AeolusRegistrations,
    /// `pset:Preset` files in the LV2 bundles, plus the programmes the
    /// plugin names in its MIDNAM document.
    Lv2Bundles,
    /// `.vital` patches in Vitalium's data folder, then its LV2 presets.
    VitalPatches,
    /// A stepped parameter of the plugin's own ("Preset"), read from the
    /// live plugin on the main thread: `preset_param_library`.
    PresetParameter,
}

impl Instrument {
    /// The CLAP plugin id or the LV2 URI.
    pub fn id(&self) -> &str {
        let (Instrument::Clap(id) | Instrument::Lv2(id)) = self;
        id
    }

    pub fn source(&self) -> Source {
        match self {
            Instrument::Clap(id) if id == SURGE_ID => Source::SurgePatches,
            Instrument::Clap(id) if id == DEXED_ID => Source::DexedCartridges,
            Instrument::Clap(id) if id == ODIN_ID => Source::OdinSoundbanks,
            Instrument::Clap(id) if id == crate::aeolus::AEOLUS_ID => Source::AeolusRegistrations,
            Instrument::Clap(_) => Source::PresetParameter,
            Instrument::Lv2(uri) if uri == crate::vital::URI => Source::VitalPatches,
            Instrument::Lv2(_) => Source::Lv2Bundles,
        }
    }
}

/// Read an instrument's library from disk. Blocks (directory walks, preset
/// parsing, an LV2 instantiation): `Libraries` runs it off the UI thread.
pub fn scan(instrument: &Instrument) -> Library {
    match instrument.source() {
        Source::SurgePatches => surge_library(),
        Source::DexedCartridges => dexed_library(),
        Source::OdinSoundbanks => odin_library(&odin_soundbanks_dir()),
        Source::AeolusRegistrations => aeolus_library(),
        Source::Lv2Bundles => lv2_library(instrument.id()),
        Source::VitalPatches => vital_library(&crate::vital::data_dir()),
        // Nothing on disk: the plugin is asked.
        Source::PresetParameter => Library::default(),
    }
}

fn surge_library() -> Library {
    let sounds = crate::host::list_surge_patches().into_iter().map(|p| Sound {
        preferred: p.path.to_string_lossy().contains("patches_factory"),
        category: p.category,
        name: p.name,
        load: Load::SurgePatch(p.path),
    }).collect();
    Library { kind: "Surge patches".into(), sounds, params: Vec::new() }
}

fn dexed_library() -> Library {
    let sounds = crate::host::discover_dexed_patches().into_iter().map(|p| Sound {
        category: p.bank,
        name: p.name,
        preferred: false,
        load: Load::DexedVoice { syx: p.syx_path, program: p.program },
    }).collect();
    Library { kind: "DX7 cartridge voices".into(), sounds, params: Vec::new() }
}

/// Where Odin 2 on Linux keeps its soundbanks (`ODIN_STORAGE_PATH`).
pub fn odin_soundbanks_dir() -> PathBuf {
    crate::catalog::home_dir().join(".local/share/odin2/Soundbanks")
}

/// One sound per `.odin` file: `<soundbank>/<category>/<name>.odin`, the
/// category being the folder inside the soundbank.
fn odin_library(soundbanks: &std::path::Path) -> Library {
    fn walk(dir: &std::path::Path, base: &std::path::Path, factory: bool, out: &mut Vec<Sound>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                walk(&path, base, factory, out);
            } else if path.extension().is_some_and(|x| x == "odin") {
                let category = path.parent().and_then(|d| d.strip_prefix(base).ok()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
                let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Sound { category, name, preferred: factory, load: Load::ValueTreePatch(path) });
            }
        }
    }
    let mut sounds = Vec::new();
    for bank in std::fs::read_dir(soundbanks).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let factory = bank.file_name().is_some_and(|n| n == "Factory Presets");
        walk(&bank, &bank, factory, &mut sounds);
    }
    sounds.sort_by_key(|s| (s.category.to_lowercase(), s.name.to_lowercase()));
    Library { kind: "Odin 2 patches".into(), sounds, params: Vec::new() }
}

fn aeolus_library() -> Library {
    let sounds = crate::aeolus::REGISTRATIONS.iter().map(|r| Sound {
        category: r.category.into(),
        name: r.name.into(),
        preferred: false,
        load: Load::AeolusRegistration(r),
    }).collect();
    Library { kind: "Aeolus organ registrations".into(), sounds, params: Vec::new() }
}

/// An LV2 instrument's installed presets plus the MIDI programmes it names.
fn lv2_library(uri: &str) -> Library {
    let presets = crate::lv2::discover_presets(uri);
    let programs = crate::lv2::midi_programs(uri);
    let kind = match (presets.is_empty(), programs.is_empty()) {
        (false, false) => "LV2 presets and MIDI programmes",
        (true, false) => "MIDI programmes",
        _ => "LV2 presets",
    };
    let mut sounds: Vec<Sound> = presets.into_iter()
        .map(|p| Sound { category: String::new(), name: p.name, preferred: false, load: Load::Lv2Preset(p.uri) })
        .collect();
    sounds.extend(programs.into_iter().map(|(program, name)| Sound { category: String::new(), name, preferred: false, load: Load::MidiProgram(program) }));
    let params = if sounds.is_empty() {
        crate::lv2::control_descs_for_uri(uri).into_iter().map(|c| c.name).collect()
    } else {
        Vec::new()
    };
    Library { kind: kind.into(), sounds, params }
}

fn vital_library(data: &std::path::Path) -> Library {
    let mut sounds = crate::vital::patches(data);
    sounds.extend(lv2_library(crate::vital::URI).sounds);
    Library { kind: "Vital patches".into(), sounds, params: Vec::new() }
}

/// Parameter names that select a whole sound.
const PRESET_PARAM_NAMES: &[&str] = &["preset", "presets", "program", "programs", "programme", "patch"];
/// Most values a preset parameter is read for.
const MAX_PRESET_VALUES: usize = 1024;

/// The library of a plugin that keeps its sounds on a stepped parameter of
/// its own (YANES' "Preset"): one sound per value, named by the plugin's
/// value text. `text` is the plugin's value-to-text, a main-thread call.
pub fn preset_param_library(params: &[ParamDesc], mut text: impl FnMut(u32, f64) -> String) -> Library {
    let preset = params.iter().find(|p| {
        p.stepped && !p.readonly && p.max - p.min >= 1.0 && PRESET_PARAM_NAMES.contains(&p.name.trim().to_lowercase().as_str())
    });
    let Some(preset) = preset else {
        let names = params.iter().filter(|p| !p.hidden && !p.readonly).map(|p| p.name.clone()).collect();
        return Library { kind: "presets".into(), sounds: Vec::new(), params: names };
    };
    let count = ((preset.max - preset.min).round() as usize + 1).min(MAX_PRESET_VALUES);
    let sounds = (0..count).filter_map(|i| {
        let value = preset.min + i as f64;
        let name = text(preset.id, value).trim().to_string();
        (!name.is_empty()).then(|| Sound { category: String::new(), name, preferred: false, load: Load::ParamValue { param: preset.id, value } })
    }).collect();
    Library { kind: format!("presets (its '{}' parameter)", preset.name.trim()), sounds, params: Vec::new() }
}

/// One instrument unit of a rack, as the sound tools address it.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub node: NodeId,
    pub rig: usize,
    pub name: String,
    pub instrument: Instrument,
    /// The app's CLAP voice slot that plays it (None for LV2 and headless).
    pub slot: Option<usize>,
}

/// Sounds one list_sounds call returns.
pub const LIST_LIMIT: usize = 25;

/// The instrument a sound tool means: the unit `node` names, else the
/// instrument of `rig`. Of several alike (two Surges) the first is meant;
/// a rig whose instruments have different libraries needs `node`.
pub fn pick_unit(units: &[Unit], rig: usize, rig_name: &str, node: Option<NodeId>) -> Result<Unit, String> {
    let list = |units: &[&Unit]| units.iter().map(|u| format!("{} (node {})", u.name, u.node)).collect::<Vec<_>>().join(", ");
    if let Some(node) = node {
        return units.iter().find(|u| u.node == node).cloned().ok_or_else(|| {
            let all: Vec<&Unit> = units.iter().collect();
            format!("node {node} is not an instrument (instruments: {})", if all.is_empty() { "none".into() } else { list(&all) })
        });
    }
    let here: Vec<&Unit> = units.iter().filter(|u| u.rig == rig).collect();
    match here[..] {
        [] => Err(format!("rig {rig} '{rig_name}' has no instrument")),
        [first, ..] if here.iter().all(|u| u.instrument == first.instrument) => Ok(first.clone()),
        _ => Err(format!("rig {rig} '{rig_name}' has {} different instruments: {}. Say which one with node=<id>", here.len(), list(&here))),
    }
}

/// What an instrument without sounds is told to be shaped with instead.
fn nothing_to_browse(unit: &Unit, library: &Library) -> String {
    let shown: Vec<&str> = library.params.iter().take(12).map(String::as_str).collect();
    let params = match shown.len() {
        0 => String::new(),
        n => format!(" ({}{})", shown.join(", "), if library.params.len() > n { ", …" } else { "" }),
    };
    format!("{} has no sounds to browse (it has no {}). It is shaped by its parameters{params}: see list_params(node={})", unit.name, library.kind, unit.node)
}

fn no_match(unit: &Unit, library: &Library, query: &str) -> String {
    let some: Vec<String> = library.sounds.iter().take(8).map(Sound::label).collect();
    format!("no {} of {} match '{query}' ({} in its library, e.g. {}): search with other words", library.kind, unit.name, library.sounds.len(), some.join(", "))
}

/// list_sounds' answer: the sounds a query finds, which instrument and
/// library they are from, and the words that were left out of the search.
/// A miss starts with "no ".
pub fn list_answer(unit: &Unit, library: &Library, query: &str) -> String {
    if library.sounds.is_empty() { return nothing_to_browse(unit, library); }
    let hits = search(&library.sounds, query, LIST_LIMIT);
    if hits.labels.is_empty() { return no_match(unit, library, query); }
    let mut answer = serde_json::json!({
        "instrument": unit.name, "node": unit.node, "rig": unit.rig,
        "library": library.kind, "total": library.sounds.len(), "sounds": hits.labels,
    });
    if !hits.ignored.is_empty() { answer["words_no_sound_has"] = serde_json::json!(hits.ignored); }
    answer.to_string()
}

/// The sound load_sound's query means, or why there is none.
pub fn find_or_explain<'a>(unit: &Unit, library: &'a Library, query: &str) -> Result<&'a Sound, String> {
    if library.sounds.is_empty() { return Err(nothing_to_browse(unit, library)); }
    find(&library.sounds, query).ok_or_else(|| no_match(unit, library, query))
}

/// load_sound's answer.
pub fn loaded_answer(unit: &Unit, sound: &Sound, rig_name: &str) -> String {
    format!("loaded '{}' into {} (node {}), rig {} '{rig_name}'", sound.label(), unit.name, unit.node, unit.rig)
}

/// The live handles of the node a sound is loaded into.
pub struct Target<'a> {
    pub node: NodeId,
    /// A CLAP instrument's main-thread half and its parameter ring.
    pub clap: Option<(&'a mut ClapVoiceMain, &'a VoiceController)>,
    pub bus: Option<&'a mut CommandBus>,
}

/// Load one sound, each kind along its own path: plugin state on the CLAP
/// main thread, a parameter over the voice's ring, presets and programme
/// changes as engine commands (the engine hands preset parsing to its LV2
/// loader thread).
pub fn load(sound: &Sound, to: Target) -> Result<(), String> {
    let Target { node, clap, bus } = to;
    let plugin = || clap.ok_or_else(|| "no live plugin (headless)".to_string());
    let send = |cmd: Command| match bus.map(|bus| bus.send(cmd)) {
        Some(true) => Ok(()),
        Some(false) => Err("command bus full, try again".to_string()),
        None => Err("no audio engine (headless)".to_string()),
    };
    match &sound.load {
        Load::SurgePatch(path) => {
            let data = std::fs::read(path).map_err(|e| format!("reading patch: {e}"))?;
            let state = crate::host::fxp_to_state(&data).map_err(|e| e.to_string())?;
            plugin()?.0.load_state(&state).map_err(|e| e.to_string())
        }
        Load::DexedVoice { syx, program } => crate::host::load_dexed_voice(plugin()?.0, syx, *program).map_err(|e| e.to_string()),
        Load::ValueTreePatch(path) => {
            let data = std::fs::read(path).map_err(|e| format!("reading patch: {e}"))?;
            let state = crate::juce::value_tree_to_state(&data).map_err(|e| format!("{}: {e}", path.display()))?;
            plugin()?.0.load_state(&state).map_err(|e| e.to_string())
        }
        Load::AeolusRegistration(reg) => plugin()?.0.load_state(&crate::aeolus::state(reg)).map_err(|e| e.to_string()),
        Load::ParamValue { param, value } => {
            plugin()?.1.set_param(clack_host::prelude::ClapId::new(*param), *value);
            Ok(())
        }
        Load::VitalPatch(path) => {
            let state = std::fs::read(path).map_err(|e| format!("reading patch: {e}"))?;
            send(Command::LoadLv2State { node, state })
        }
        Load::Lv2Preset(uri) => send(Command::LoadLv2Preset { node, preset_uri: uri.clone() }),
        Load::MidiProgram(program) => send(Command::ProgramChange { node, program: *program }),
    }
}

fn word_alternatives(w: &str) -> &'static [&'static str] {
    match w {
        "wurlitzer" | "wurli" => &["wurlitzer", "wurli"],
        "rhodes" => &["rhodes", "suitcase", "ep"],
        "ep" => &["ep", "electric piano", "suitcase", "wurli", "rhodes"],
        "electric" => &["electric", "ep", "suitcase", "wurli", "rhodes"],
        "piano" => &["piano", "ep", "suitcase", "wurli", "keys"],
        "harpsichord" | "harpsi" => &["harpsichord", "harpsi"],
        "saxophone" | "sax" => &["saxophone", "sax"],
        "synthesizer" | "synth" => &["synthesizer", "synth"],
        "vibraphone" | "vibes" => &["vibraphone", "vibes"],
        "glockenspiel" | "glock" => &["glockenspiel", "glock"],
        "clavinet" | "clav" => &["clavinet", "clav"],
        "percussion" | "perc" => &["percussion", "perc"],
        "reverb" | "verb" => &["reverb", "verb"],
        "atmospheric" | "atmosphere" | "atmos" => &["atmospheric", "atmosphere", "atmos", "ambient", "pad"],
        "spooky" | "creepy" | "eerie" => &["spooky", "creepy", "eerie", "dark", "haunt"],
        _ => &[],
    }
}

fn word_matches_alias(haystack: &str, w: &str) -> bool {
    haystack.contains(w) || word_alternatives(w).iter().any(|alt| haystack.contains(alt))
}

/// What a search found.
#[derive(Debug, Default, PartialEq)]
pub struct Hits {
    /// Labels of the sounds found, best first.
    pub labels: Vec<String>,
    /// Query words that were left out because no sound of the library
    /// holds them.
    pub ignored: Vec<String>,
}

/// The query's words that say something in this library: those at least one
/// sound holds. "organ" says nothing among setBfree's programmes, where
/// every sound is an organ and none is called one. None when every word
/// tells (nothing to leave out) or none does (nothing left to search).
fn telling_words<'a>(labels: &[String], words: &[&'a str]) -> Option<(Vec<&'a str>, Vec<String>)> {
    let (telling, silent): (Vec<&str>, Vec<&str>) = words.iter().partition(|w| labels.iter().any(|l| word_matches_alias(l, w)));
    (!telling.is_empty() && !silent.is_empty()).then(|| (telling, silent.iter().map(|w| w.to_string()).collect()))
}

/// The sounds a query finds: every word literally, then every word through
/// an instrument alias (`wurlitzer` → `wurli`); when that finds nothing,
/// the same without the words no sound of the library holds.
pub fn search(sounds: &[Sound], query: &str, limit: usize) -> Hits {
    let q = query.trim().to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let labels: Vec<String> = sounds.iter().map(Sound::label).collect();
    let low: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
    let pass = |words: &[&str]| {
        let mut out: Vec<String> = Vec::new();
        for alias in [false, true] {
            for (low, label) in low.iter().zip(&labels) {
                let all = if alias { words.iter().all(|w| word_matches_alias(low, w)) } else { words.iter().all(|w| low.contains(w)) };
                if all && out.len() < limit && !out.contains(label) { out.push(label.clone()); }
            }
        }
        out
    };
    let found = pass(&words);
    if !found.is_empty() { return Hits { labels: found, ignored: Vec::new() }; }
    match telling_words(&low, &words) {
        Some((telling, ignored)) => Hits { labels: pass(&telling), ignored },
        None => Hits::default(),
    }
}

/// The one sound a free-text query means ("flute", "warm pad",
/// "Winds/Flute 2"). A label, as `search` lists it, means exactly that
/// sound. Otherwise: name is the query > name starts with it > name holds
/// every word > label holds every word > the same through aliases > any
/// word, with preferred sounds winning at every tier; when that finds
/// nothing, the same without the words no sound of the library holds.
pub fn find<'a>(sounds: &'a [Sound], query: &str) -> Option<&'a Sound> {
    let q = query.trim().to_lowercase();
    if q.is_empty() { return None; }
    let name = |s: &Sound| s.name.to_lowercase();
    let full = |s: &Sound| s.label().to_lowercase();
    if let Some(listed) = sounds.iter().find(|s| full(s) == q) { return Some(listed); }
    let by_words = |q: &str, words: &[&str]| {
        let tiers: [&dyn Fn(&Sound) -> bool; 6] = [
            &|s| name(s) == q,
            &|s| name(s).starts_with(q),
            &|s| words.iter().all(|w| name(s).contains(w)),
            &|s| words.iter().all(|w| full(s).contains(w)),
            &|s| words.iter().all(|w| word_matches_alias(&name(s), w)),
            &|s| words.iter().all(|w| word_matches_alias(&full(s), w)),
        ];
        [true, false].into_iter().find_map(|preferred_only| {
            tiers.iter().find_map(|t| sounds.iter().find(|s| (!preferred_only || s.preferred) && t(s)))
        })
    };
    let words: Vec<&str> = q.split_whitespace().collect();
    if let Some(s) = by_words(&q, &words) { return Some(s); }
    let low: Vec<String> = sounds.iter().map(|s| full(s)).collect();
    if let Some((telling, _)) = telling_words(&low, &words) {
        if let Some(s) = by_words(&telling.join(" "), &telling) { return Some(s); }
    }
    // Last resort: the first sound that holds any of the words.
    [true, false].into_iter().find_map(|preferred_only| {
        sounds.iter().find(|s| (!preferred_only || s.preferred) && words.iter().any(|w| w.len() > 2 && word_matches_alias(&full(s), w)))
    })
}

/// What asking for an instrument's library gives.
#[derive(Clone)]
pub enum Lookup {
    Ready(Arc<Library>),
    /// The scan is on its way: ask again.
    Scanning,
    /// `Source::PresetParameter`: build it with `preset_param_library` from
    /// the live plugin and `insert` it.
    AskPlugin,
}

enum Slot {
    Scanning,
    Ready(Arc<Library>),
}

/// The libraries found so far, by instrument. Cheap to clone; clones share
/// the cache.
#[derive(Clone)]
pub struct Libraries {
    found: Arc<Mutex<HashMap<Instrument, Slot>>>,
    scan: fn(&Instrument) -> Library,
    /// Scan on a thread of its own (the app) or in the caller (tests, CLI).
    threaded: bool,
}

impl Libraries {
    /// The libraries installed on this machine, scanned in the background.
    pub fn installed() -> Self {
        Self::background(scan)
    }

    /// Libraries from `scan`, each read on a thread of its own.
    pub fn background(scan: fn(&Instrument) -> Library) -> Self {
        Self { found: Default::default(), scan, threaded: true }
    }

    /// Libraries from `scan`, read in the caller: for tests on fakes and
    /// for callers that may block (the CLI, headless MCP).
    pub fn blocking(scan: fn(&Instrument) -> Library) -> Self {
        Self { found: Default::default(), scan, threaded: false }
    }

    pub fn get(&self, instrument: &Instrument) -> Lookup {
        let mut found = self.found.lock().unwrap_or_else(|e| e.into_inner());
        match found.get(instrument) {
            Some(Slot::Ready(library)) => return Lookup::Ready(library.clone()),
            Some(Slot::Scanning) => return Lookup::Scanning,
            None => {}
        }
        if instrument.source() == Source::PresetParameter {
            return Lookup::AskPlugin;
        }
        if !self.threaded {
            let library = Arc::new((self.scan)(instrument));
            found.insert(instrument.clone(), Slot::Ready(library.clone()));
            return Lookup::Ready(library);
        }
        found.insert(instrument.clone(), Slot::Scanning);
        drop(found);
        let (cache, scan, key) = (self.found.clone(), self.scan, instrument.clone());
        let spawned = std::thread::Builder::new().name("kabelsalat-sounds".into()).spawn(move || {
            let library = Arc::new(scan(&key));
            cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, Slot::Ready(library));
        });
        if spawned.is_err() {
            self.found.lock().unwrap_or_else(|e| e.into_inner()).remove(instrument);
        }
        Lookup::Scanning
    }

    /// Read an instrument's library again on its next use (files were added).
    pub fn forget(&self, instrument: &Instrument) {
        self.found.lock().unwrap_or_else(|e| e.into_inner()).remove(instrument);
    }

    /// Keep a library that was read from the live plugin.
    pub fn insert(&self, instrument: Instrument, library: Library) -> Arc<Library> {
        let library = Arc::new(library);
        self.found.lock().unwrap_or_else(|e| e.into_inner()).insert(instrument, Slot::Ready(library.clone()));
        library
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odin_soundbanks_list_one_sound_per_patch_file() {
        let root = std::env::temp_dir().join(format!("ksalat-odin-banks-{}", std::process::id()));
        let factory = root.join("Factory Presets/Bass");
        let user = root.join("User Patches");
        std::fs::create_dir_all(&factory).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(factory.join("Deep Sub.odin"), b"").unwrap();
        std::fs::write(user.join("Mine.odin"), b"").unwrap();
        std::fs::write(user.join("notes.txt"), b"").unwrap();
        let library = odin_library(&root);
        let labels: Vec<String> = library.sounds.iter().map(Sound::label).collect();
        assert_eq!(labels, ["Mine", "Bass/Deep Sub"]);
        assert!(library.sounds[1].preferred, "factory patches win a tie");
        assert!(!library.sounds[0].preferred);
        assert_eq!(library.sounds[1].load, Load::ValueTreePatch(factory.join("Deep Sub.odin")));
        assert_eq!(Instrument::Clap(ODIN_ID.into()).source(), Source::OdinSoundbanks);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn sound(category: &str, name: &str, preferred: bool) -> Sound {
        Sound { category: category.into(), name: name.into(), preferred, load: Load::MidiProgram(0) }
    }

    #[test]
    fn search_and_find_fall_back_to_instrument_aliases() {
        let patches = vec![
            sound("Cybersoda/Keys", "Wurli", false),
            sound("3rd/Winds", "Jazz Flute", false),
            sound("Keys", "EP 1", true),
            sound("Winds", "Flute 1", true),
        ];
        assert_eq!(find(&patches, "wurlitzer").map(|p| p.name.as_str()), Some("Wurli"));
        assert_eq!(search(&patches, "wurlitzer", 10).labels, ["Cybersoda/Keys/Wurli"]);
        assert_eq!(find(&patches, "flute").map(|p| p.name.as_str()), Some("Flute 1"));
    }

    #[test]
    fn preferred_sounds_win_a_tie() {
        let patches = vec![sound("Third/Pads", "Warm Pad", false), sound("Pads", "Warm Pad", true)];
        assert_eq!(find(&patches, "warm pad").map(|p| p.category.as_str()), Some("Pads"));
        assert_eq!(find(&patches, "Third/Pads/Warm Pad").map(|p| p.category.as_str()), Some("Third/Pads"), "an exact label names its sound");
    }

    #[test]
    fn a_listed_label_loads_exactly_that_sound() {
        // The preferred "Pads/Warm Pad" holds the word "pad" too.
        let patches = vec![sound("Pads", "Warm Pad", true), sound("Third/Pads", "Warm Pad", false), sound("Third/Pads", "Pad of Glass", false)];
        for label in search(&patches, "pad", 10).labels {
            assert_eq!(find(&patches, &label).map(Sound::label), Some(label));
        }
    }

    #[test]
    fn words_no_sound_holds_are_left_out_of_the_search() {
        // Every setBfree programme is an organ and none says so.
        let organ = vec![sound("", "Gospel", false), sound("", "Jazz 1 all", false), sound("", "Jimmy Smith", false)];
        assert_eq!(search(&organ, "jazz drawbar organ", 10), Hits { labels: vec!["Jazz 1 all".into()], ignored: vec!["drawbar".into(), "organ".into()] });
        assert_eq!(find(&organ, "jazz drawbar organ").map(|s| s.name.as_str()), Some("Jazz 1 all"));
        assert_eq!(search(&organ, "pad", 10), Hits::default(), "no word left to search with");
        assert!(find(&organ, "pad").is_none());
        let chips = vec![sound("", "NES triangle bass", false), sound("", "SID 6581 bass", false), sound("", "SID 8580 lead", false)];
        assert_eq!(search(&chips, "sid bass", 10).labels, ["SID 6581 bass"]);
        assert_eq!(search(&chips, "yanes bass", 10).labels, ["NES triangle bass", "SID 6581 bass"]);
        // Words the library does hold are never dropped: no "SID lead bass".
        assert_eq!(search(&chips, "sid lead bass", 10), Hits::default());
    }

    fn unit(node: NodeId, rig: usize, name: &str, instrument: Instrument) -> Unit {
        Unit { node, rig, name: name.into(), instrument, slot: None }
    }

    #[test]
    fn a_sound_tool_means_the_rigs_instrument_or_the_node_it_names() {
        let surge = || Instrument::Clap(SURGE_ID.into());
        let organ = || Instrument::Lv2("urn:organ".into());
        let units = [unit(1, 0, "Surge A", surge()), unit(4, 0, "Surge B", surge()), unit(8, 1, "Organ", organ()), unit(12, 2, "Surge", surge()), unit(13, 2, "Organ", organ())];
        assert_eq!(pick_unit(&units, 1, "Rig B", None).map(|u| u.node), Ok(8));
        assert_eq!(pick_unit(&units, 0, "Rig A", None).map(|u| u.node), Ok(1), "two Surges share one library: the first is meant");
        assert_eq!(pick_unit(&units, 0, "Rig A", Some(4)).map(|u| u.node), Ok(4));
        let which = pick_unit(&units, 2, "Rig C", None).unwrap_err();
        assert!(which.contains("2 different instruments: Surge (node 12), Organ (node 13)") && which.contains("node=<id>"), "{which}");
        assert_eq!(pick_unit(&units, 2, "Rig C", Some(13)).map(|u| u.name), Ok("Organ".into()), "a node settles it");
        assert_eq!(pick_unit(&units, 3, "Rig D", None), Err("rig 3 'Rig D' has no instrument".into()));
        let not_one = pick_unit(&units, 0, "Rig A", Some(99)).unwrap_err();
        assert!(not_one.starts_with("node 99 is not an instrument (instruments: Surge A (node 1), ") , "{not_one}");
    }

    #[test]
    fn an_answer_says_which_instrument_and_library_it_searched() {
        let organ = unit(8, 1, "Organ", Instrument::Lv2("urn:organ".into()));
        let library = Library { kind: "MIDI programmes".into(), sounds: vec![sound("", "Gospel", false), sound("", "Jazz 1 all", false)], params: vec![] };
        let hits: serde_json::Value = serde_json::from_str(&list_answer(&organ, &library, "jazz organ")).unwrap();
        assert_eq!(hits, serde_json::json!({"instrument": "Organ", "node": 8, "rig": 1, "library": "MIDI programmes", "total": 2, "sounds": ["Jazz 1 all"], "words_no_sound_has": ["organ"]}));
        let miss = list_answer(&organ, &library, "pad");
        assert_eq!(miss, "no MIDI programmes of Organ match 'pad' (2 in its library, e.g. Gospel, Jazz 1 all): search with other words");
        assert_eq!(find_or_explain(&organ, &library, "pad").unwrap_err(), miss);
        assert_eq!(find_or_explain(&organ, &library, "gospel").map(|s| s.name.as_str()), Ok("Gospel"));
        assert_eq!(loaded_answer(&organ, &library.sounds[1], "Keys rig"), "loaded 'Jazz 1 all' into Organ (node 8), rig 1 'Keys rig'");
    }

    #[test]
    fn an_instrument_without_sounds_says_what_it_is_shaped_with() {
        let osc = unit(3, 0, "Oscillator", Instrument::Lv2("urn:osc".into()));
        let library = Library { kind: "LV2 presets".into(), sounds: vec![], params: vec!["Frequency".into(), "Waveform".into()] };
        let said = "Oscillator has no sounds to browse (it has no LV2 presets). It is shaped by its parameters (Frequency, Waveform): see list_params(node=3)";
        assert_eq!(list_answer(&osc, &library, "bass"), said);
        assert_eq!(find_or_explain(&osc, &library, "bass").unwrap_err(), said);
        assert!(!said.starts_with("no ") && !said.starts_with('{'), "neither a miss nor hits: the shortcut plan hands it to the chat model");
    }

    #[test]
    fn each_kind_of_sound_is_loaded_along_its_own_path() {
        let (mut bus, mut drain) = crate::control::command_bus();
        let entry = |load| Sound { category: String::new(), name: "x".into(), preferred: false, load };
        let mut to_bus = |how| load(&entry(how), Target { node: 8, clap: None, bus: Some(&mut bus) });
        assert_eq!(to_bus(Load::MidiProgram(103)), Ok(()));
        assert_eq!(to_bus(Load::Lv2Preset("urn:organ#full".into())), Ok(()));
        assert!(matches!(drain.pop(), Some(Command::ProgramChange { node: 8, program: 103 })));
        assert!(matches!(drain.pop(), Some(Command::LoadLv2Preset { node: 8, preset_uri }) if preset_uri == "urn:organ#full"));
        // Plugin state and parameters need the live plugin, never the bus.
        assert_eq!(to_bus(Load::ParamValue { param: 78, value: 5.0 }), Err("no live plugin (headless)".into()));
        assert_eq!(to_bus(Load::DexedVoice { syx: "/nowhere.syx".into(), program: 0 }), Err("no live plugin (headless)".into()));
        assert!(drain.pop().is_none());
        assert_eq!(load(&entry(Load::MidiProgram(1)), Target { node: 8, clap: None, bus: None }), Err("no audio engine (headless)".into()));
    }

    fn param(id: u32, name: &str, max: f64, stepped: bool) -> ParamDesc {
        ParamDesc { id, name: name.into(), module: String::new(), min: 0.0, max, value: 0.0, default: 0.0, automatable: true, hidden: false, readonly: false, stepped }
    }

    #[test]
    fn a_stepped_preset_parameter_is_a_library_of_its_values() {
        let params = [param(1, "Cutoff", 1.0, false), param(7, "Preset", 2.0, true)];
        let names = ["Manual", "Clean NES lead", "SMS bass"];
        let library = preset_param_library(&params, |id, v| { assert_eq!(id, 7); names[v as usize].to_string() });
        assert_eq!(library.sounds.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), names);
        assert_eq!(library.sounds[2].load, Load::ParamValue { param: 7, value: 2.0 });
        assert!(library.kind.contains("'Preset' parameter"), "{}", library.kind);
    }

    #[test]
    fn a_plugin_without_a_preset_parameter_names_its_parameters() {
        // A continuous "Preset" knob or a stepped "Waveform" is not a library.
        let params = [param(1, "Cutoff", 1.0, false), param(2, "Waveform", 4.0, true), param(3, "Preset", 9.0, false)];
        let library = preset_param_library(&params, |_, _| unreachable!("no preset parameter to read"));
        assert!(library.sounds.is_empty());
        assert_eq!(library.params, ["Cutoff", "Waveform", "Preset"]);
    }

    fn fake_scan(instrument: &Instrument) -> Library {
        Library { kind: "LV2 presets".into(), sounds: vec![sound("", instrument.id(), false)], params: vec![] }
    }

    #[test]
    fn libraries_scan_once_per_instrument_and_keep_what_a_plugin_told_them() {
        let libraries = Libraries::blocking(fake_scan);
        let organ = Instrument::Lv2("urn:organ".into());
        let Lookup::Ready(first) = libraries.get(&organ) else { panic!("scanned in the caller") };
        let Lookup::Ready(again) = libraries.get(&organ) else { panic!("kept") };
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(first.sounds[0].name, "urn:organ");

        let chip = Instrument::Clap("org.yanes.native".into());
        assert!(matches!(libraries.get(&chip), Lookup::AskPlugin), "a preset parameter is never scanned from disk");
        libraries.insert(chip.clone(), Library { kind: "presets".into(), sounds: vec![sound("", "SMS bass", false)], params: vec![] });
        let Lookup::Ready(told) = libraries.get(&chip) else { panic!("inserted") };
        assert_eq!(told.sounds[0].name, "SMS bass");
    }

    #[test]
    fn a_node_names_its_instrument() {
        use crate::control::{name_bytes, Lv2Uri};
        assert_eq!(Instrument::of(&NodeKind::Clap { plugin_id: name_bytes("org.yanes.native") }), Some(Instrument::Clap("org.yanes.native".into())));
        assert_eq!(Instrument::of(&NodeKind::Clap { plugin_id: name_bytes("Surge XT") }), Some(Instrument::Clap(SURGE_ID.into())));
        assert_eq!(Instrument::of(&NodeKind::Lv2 { uri: Lv2Uri::new("urn:distrho:vitalium") }), Some(Instrument::Lv2("urn:distrho:vitalium".into())));
        assert_eq!(Instrument::of(&NodeKind::Keys), None);
    }
}
