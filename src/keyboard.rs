//! Instrument-adaptive virtual keyboard surfaces (issue #163):
//! pure classification, chord detection, scale folding, tonewheel presets,
//! and interaction math shared by `ui.rs`, `session.rs`, `assistant.rs` and `mcp.rs`.

use serde::{Deserialize, Serialize};

use crate::aeolus;
use crate::arp::ScaleChoice;
use crate::control::NodeKind;
use crate::organ;

/// What kind of instrument a rig plays (#198): the one vocabulary the
/// keyboard, the Plugins browser, the catalog, sessions and the agent share.
/// Every `match` on it is exhaustive, so a ninth type names every place it
/// needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardType {
    /// Acoustic piano and its relatives (harpsichord, clavinet, celesta):
    /// dynamics, damper & soft pedals, chord readout.
    Piano,
    /// Electric piano (Rhodes, Wurlitzer, tine and FM pianos).
    ElectricPiano,
    /// Tonewheel jazz/rock organ (setBfree B3): 9 drawbars, Leslie, swell, waterfall gliss, dual manual.
    JazzOrgan,
    /// Classical church / pipe organ (Aeolus): divisions, stop jamb, couplers, pistons, tremulant, swell, pedalboard.
    PipeOrgan,
    /// Synthesizer (Surge XT, Vitalium, Dexed, Odin 2, YANES, fallback): Pitch/Mod, macros, X/Y pad, scale/fold, arp.
    Synth,
    /// Bass instrument (bass guitar models, bass-line synths).
    Bass,
    /// Wind and brass, often played from a breath controller.
    Wind,
    /// Drum kits and drum machines on the General MIDI drum notes.
    Drums,
}

impl KeyboardType {
    pub const ALL: [KeyboardType; 8] = [
        KeyboardType::Piano,
        KeyboardType::ElectricPiano,
        KeyboardType::JazzOrgan,
        KeyboardType::PipeOrgan,
        KeyboardType::Synth,
        KeyboardType::Bass,
        KeyboardType::Wind,
        KeyboardType::Drums,
    ];

    /// Display label on full-width headers and chips.
    pub const fn label(self) -> &'static str {
        match self {
            KeyboardType::Piano => "Piano",
            KeyboardType::ElectricPiano => "E-Piano",
            KeyboardType::JazzOrgan => "Jazz Organ",
            KeyboardType::PipeOrgan => "Pipe Organ",
            KeyboardType::Synth => "Synth",
            KeyboardType::Bass => "Bass",
            KeyboardType::Wind => "Wind",
            KeyboardType::Drums => "Drums",
        }
    }

    /// Compact label for narrow windows.
    pub const fn short_label(self) -> &'static str {
        match self {
            KeyboardType::Piano => "Piano",
            KeyboardType::ElectricPiano => "EP",
            KeyboardType::JazzOrgan => "Jazz",
            KeyboardType::PipeOrgan => "Pipe",
            KeyboardType::Synth => "Synth",
            KeyboardType::Bass => "Bass",
            KeyboardType::Wind => "Wind",
            KeyboardType::Drums => "Drums",
        }
    }

    /// Upper-case badge on cards and rig rows.
    pub const fn badge(self) -> &'static str {
        match self {
            KeyboardType::Piano => "PIANO",
            KeyboardType::ElectricPiano => "E-PIANO",
            KeyboardType::JazzOrgan => "JAZZ ORGAN",
            KeyboardType::PipeOrgan => "PIPE ORGAN",
            KeyboardType::Synth => "SYNTH",
            KeyboardType::Bass => "BASS",
            KeyboardType::Wind => "WIND",
            KeyboardType::Drums => "DRUMS",
        }
    }

    /// Machine-readable name for MCP/assistant JSON and sessions.
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyboardType::Piano => "piano",
            KeyboardType::ElectricPiano => "electric_piano",
            KeyboardType::JazzOrgan => "jazz_organ",
            KeyboardType::PipeOrgan => "pipe_organ",
            KeyboardType::Synth => "synth",
            KeyboardType::Bass => "bass",
            KeyboardType::Wind => "wind",
            KeyboardType::Drums => "drums",
        }
    }

    /// Parse a human or tool string into a type.
    pub fn parse(s: &str) -> Option<KeyboardType> {
        match s.trim().to_ascii_lowercase().replace(['-', ' ', '.'], "_").as_str() {
            "piano" | "grand" | "acoustic_piano" | "upright" | "keys" | "harpsichord" | "clavinet" => Some(KeyboardType::Piano),
            "electric_piano" | "e_piano" | "epiano" | "ep" | "rhodes" | "wurli" | "wurlitzer" | "suitcase" => {
                Some(KeyboardType::ElectricPiano)
            }
            "jazz_organ" | "jazz" | "organ" | "tonewheel" | "hammond" | "b3" | "setbfree" => Some(KeyboardType::JazzOrgan),
            "pipe_organ" | "pipe" | "church" | "church_organ" | "cathedral" | "aeolus" => Some(KeyboardType::PipeOrgan),
            "synth" | "synthesizer" | "pad" | "lead" | "default" => Some(KeyboardType::Synth),
            "bass" | "bass_guitar" | "bassline" | "sub_bass" => Some(KeyboardType::Bass),
            "wind" | "winds" | "woodwind" | "brass" | "sax" | "saxophone" | "flute" | "trumpet" | "horn" | "ewi"
            | "breath" => Some(KeyboardType::Wind),
            "drums" | "drum" | "kit" | "drum_kit" | "drumkit" | "percussion" | "pads" | "drum_machine" => {
                Some(KeyboardType::Drums)
            }
            _ => None,
        }
    }
}

/// A keyboard surface the app can draw: deck, cheek block, keybed/pads look
/// and full-window console.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Piano,
    ElectricPiano,
    JazzOrgan,
    PipeOrgan,
    Synth,
    Bass,
    Wind,
    Drums,
}

impl KeyboardType {
    /// The surface this type is drawn with: each of the eight instrument types
    /// has its own dock surface and full-window console (#198).
    pub const fn surface(self) -> Surface {
        match self {
            KeyboardType::Piano => Surface::Piano,
            KeyboardType::ElectricPiano => Surface::ElectricPiano,
            KeyboardType::JazzOrgan => Surface::JazzOrgan,
            KeyboardType::PipeOrgan => Surface::PipeOrgan,
            KeyboardType::Synth => Surface::Synth,
            KeyboardType::Bass => Surface::Bass,
            KeyboardType::Wind => Surface::Wind,
            KeyboardType::Drums => Surface::Drums,
        }
    }
}

/// Which MIDI message the Wind surface's breath controller sends (#198).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreathTarget {
    /// MIDI CC 2 (Breath Controller) — standard for Starbreath, EWI, and wind synths.
    #[default]
    Cc2,
    /// MIDI CC 11 (Expression) — supported by orchestral/brass libraries and Surge.
    Cc11,
    /// Channel Pressure (Aftertouch) — monophonic wind expression.
    Aftertouch,
}

impl BreathTarget {
    pub const ALL: [BreathTarget; 3] = [BreathTarget::Cc2, BreathTarget::Cc11, BreathTarget::Aftertouch];

    pub const fn label(self) -> &'static str {
        match self {
            BreathTarget::Cc2 => "CC 2 BREATH",
            BreathTarget::Cc11 => "CC 11 EXPR",
            BreathTarget::Aftertouch => "AFTERTOUCH",
        }
    }

    pub const fn short_label(self) -> &'static str {
        match self {
            BreathTarget::Cc2 => "CC2",
            BreathTarget::Cc11 => "CC11",
            BreathTarget::Aftertouch => "AT",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            BreathTarget::Cc2 => "cc2",
            BreathTarget::Cc11 => "cc11",
            BreathTarget::Aftertouch => "aftertouch",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "cc2" | "cc_2" | "breath" => Some(Self::Cc2),
            "cc11" | "cc_11" | "expr" | "expression" => Some(Self::Cc11),
            "aftertouch" | "at" | "pressure" | "channel_pressure" => Some(Self::Aftertouch),
            _ => None,
        }
    }
}

/// Map vertical hit/drag position on a Wind key (`0.0` = top, `1.0` = bottom)
/// to breath pressure in `0.15..=1.0`.
pub fn wind_y_breath(y_frac: f32) -> f32 {
    0.15 + 0.85 * y_frac.clamp(0.0, 1.0)
}

/// Standard 4-string electric/upright bass tuning, high string to low string
/// (matching fretboard orientation: G on top, E on bottom).
pub const BASS_STRINGS: [(&str, u8); 4] = [
    ("G", 43), // G2
    ("D", 38), // D2
    ("A", 33), // A1
    ("E", 28), // E1
];

/// Fret inlay positions on the bass neck (12 is a double dot).
pub const BASS_INLAYS: [u8; 6] = [3, 5, 7, 9, 12, 15];

/// MIDI note for string `string_idx` (0 = G, 3 = E) at `fret` (0 = open).
pub const fn bass_fret_note(string_idx: usize, fret: u8) -> u8 {
    BASS_STRINGS[string_idx].1 + fret
}

/// Find the lowest-fret natural position `(string_idx, fret)` on the 4-string
/// bass for `note`, within `0..=max_fret`. Prefers the lowest fret (open or
/// lower position on the neck) so played notes land where a bassist plays them.
pub fn bass_note_position(note: u8, max_fret: u8) -> Option<(usize, u8)> {
    let mut best: Option<(usize, u8)> = None;
    for (si, &(_, open)) in BASS_STRINGS.iter().enumerate() {
        if note >= open && note - open <= max_fret {
            let fret = note - open;
            if best.is_none_or(|(_, bf)| fret < bf) {
                best = Some((si, fret));
            }
        }
    }
    best
}

/// Visual color family for a General MIDI drum pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrumGroup {
    Kick,
    Snare,
    Hat,
    Tom,
    Cymbal,
    Perc,
}

/// One General MIDI drum pad (16-pad bank, rows ordered top-to-bottom in 4×4
/// or 2×8 layouts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrumPad {
    pub note: u8,
    pub label: &'static str,
    pub short: &'static str,
    pub group: DrumGroup,
}

/// 16 General MIDI drum pads: top bank = toms/ride/perc, bottom bank = core kit
/// (kick, snare, rim, clap, hi-hats, crash).
pub const DRUM_PADS: [DrumPad; 16] = [
    // Upper bank (toms, ride, perc)
    DrumPad { note: 41, label: "Floor Tom",   short: "FL TOM", group: DrumGroup::Tom },
    DrumPad { note: 45, label: "Low Tom",     short: "LO TOM", group: DrumGroup::Tom },
    DrumPad { note: 47, label: "Mid Tom",     short: "MD TOM", group: DrumGroup::Tom },
    DrumPad { note: 50, label: "High Tom",    short: "HI TOM", group: DrumGroup::Tom },
    DrumPad { note: 51, label: "Ride",        short: "RIDE",   group: DrumGroup::Cymbal },
    DrumPad { note: 53, label: "Ride Bell",   short: "BELL",   group: DrumGroup::Cymbal },
    DrumPad { note: 56, label: "Cowbell",     short: "COWB",   group: DrumGroup::Perc },
    DrumPad { note: 54, label: "Tambourine",  short: "TAMB",   group: DrumGroup::Perc },
    // Lower bank (core kit)
    DrumPad { note: 36, label: "Kick",        short: "KICK",   group: DrumGroup::Kick },
    DrumPad { note: 38, label: "Snare",       short: "SNARE",  group: DrumGroup::Snare },
    DrumPad { note: 37, label: "Rimshot",     short: "RIM",    group: DrumGroup::Snare },
    DrumPad { note: 39, label: "Clap",        short: "CLAP",   group: DrumGroup::Snare },
    DrumPad { note: 42, label: "Closed Hat",  short: "CHH",    group: DrumGroup::Hat },
    DrumPad { note: 46, label: "Open Hat",    short: "OHH",    group: DrumGroup::Hat },
    DrumPad { note: 44, label: "Pedal Hat",   short: "PHH",    group: DrumGroup::Hat },
    DrumPad { note: 49, label: "Crash",       short: "CRASH",  group: DrumGroup::Cymbal },
];

/// Map a hit's normalized position `(nx, ny)` in `0.0..=1.0` on a drum pad or
/// drum head to velocity (`0.25..=1.0`): dead center = `1.0`, rim/corner = `0.25`.
pub fn drum_pad_velocity(nx: f32, ny: f32) -> f32 {
    let dx = (nx.clamp(0.0, 1.0) - 0.5) * 2.0;
    let dy = (ny.clamp(0.0, 1.0) - 0.5) * 2.0;
    let dist = (dx * dx + dy * dy).sqrt().min(1.0);
    (1.0 - 0.75 * dist).clamp(0.25, 1.0)
}

/// What a tool or an old session says about a rig's type: follow its
/// instrument (`auto`), or one type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum KeyboardMode {
    #[default]
    Auto,
    Pinned(KeyboardType),
}

impl KeyboardMode {
    /// What `parse` accepts, for error messages and tool descriptions.
    pub const CHOICES: &'static str = "auto, piano, electric_piano, jazz_organ, pipe_organ, synth, bass, wind or drums";

    pub fn parse(s: &str) -> Option<Self> {
        let q = s.trim().to_ascii_lowercase();
        if q == "auto" {
            return Some(KeyboardMode::Auto);
        }
        KeyboardType::parse(&q).map(KeyboardMode::Pinned)
    }

    pub const fn pinned(self) -> Option<KeyboardType> {
        match self {
            KeyboardMode::Auto => None,
            KeyboardMode::Pinned(t) => Some(t),
        }
    }
}

impl<'de> Deserialize<'de> for KeyboardMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        KeyboardMode::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown keyboard mode '{s}'")))
    }
}

/// How horizontal pointer drag behaves on the Synth keybed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthDragMode {
    /// Dragging across keys smoothly slides note-to-note (glissando).
    #[default]
    Gliss,
    /// Dragging horizontally on a held key bends pitch smoothly (spring-return on release).
    Slide,
}

impl SynthDragMode {
    pub const fn label(self) -> &'static str {
        match self {
            SynthDragMode::Gliss => "GLISS",
            SynthDragMode::Slide => "SLIDE",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            SynthDragMode::Gliss => "gliss",
            SynthDragMode::Slide => "slide",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gliss" | "glissando" => Some(Self::Gliss),
            "slide" | "pitch" | "bend" => Some(Self::Slide),
            _ => None,
        }
    }
}

/// One instant tonewheel registration preset for the Jazz Organ deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrganPreset {
    pub label: &'static str,
    pub short: &'static str,
    pub drawbars: [u8; organ::DRAWBARS],
}

pub const JAZZ_ORGAN_PRESETS: [OrganPreset; 4] = [
    OrganPreset { label: "888 JAZZ",   short: "JAZZ",   drawbars: [8, 8, 8, 0, 0, 0, 0, 0, 0] },
    OrganPreset { label: "8888 ROCK",  short: "ROCK",   drawbars: [8, 8, 8, 8, 8, 8, 0, 0, 0] },
    OrganPreset { label: "808 GOSPEL", short: "GOSPEL", drawbars: [8, 0, 8, 0, 0, 8, 0, 0, 8] },
    OrganPreset { label: "008 FLUTE",  short: "FLUTE",  drawbars: [0, 0, 8, 0, 0, 0, 0, 0, 0] },
];

/// Curated thumb-piston registrations for the Pipe Organ (Aeolus) console dock.
/// Indices into `aeolus::REGISTRATIONS`.
pub const PIPE_ORGAN_PISTONS: [(&str, &str); 6] = [
    ("1 Diapason", "Diapason 8"),
    ("2 Chorus",   "Principal Chorus"),
    ("3 Plenum",   "Plenum"),
    ("4 Flutes",   "Flutes 8 4"),
    ("5 Vox Hum.", "Vox Humana"),
    ("6 Full",     "Full Organ"),
];

pub fn pipe_piston_registration(index: usize) -> Option<&'static aeolus::Registration> {
    let (_, name) = PIPE_ORGAN_PISTONS.get(index)?;
    aeolus::REGISTRATIONS.iter().find(|r| r.name == *name)
}

/// Which plugin a node or a scanned plugin is, by the id its format gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginRef<'a> {
    Clap(&'a str),
    Vst3(&'a str),
    Lv2(&'a str),
}

impl<'a> PluginRef<'a> {
    pub fn of_node(kind: &'a NodeKind) -> Option<Self> {
        match kind {
            NodeKind::Clap { plugin_id } => Some(PluginRef::Clap(plugin_id.as_str())),
            NodeKind::Vst3 { plugin_id } => Some(PluginRef::Vst3(plugin_id.as_str())),
            NodeKind::Lv2 { uri } => Some(PluginRef::Lv2(uri.as_str())),
            _ => None,
        }
    }
}

/// The instrument type the catalog gives this plugin: `None` when no card
/// names it or the card is not an instrument (every URI of a card counts).
pub fn catalog_type(plugin: PluginRef<'_>) -> Option<KeyboardType> {
    crate::catalog::CATALOG.iter().find(|e| match plugin {
        PluginRef::Clap(id) => e.clap_id == Some(id),
        PluginRef::Vst3(_) => false,
        PluginRef::Lv2(uri) => e.lv2_uris.contains(&uri),
    })?.instrument
}

/// The type of a plugin before any sound is loaded into it: what the Plugins
/// browser shows and where a rig's detection starts. A dedicated catalog
/// instrument decides, then the plugin's display name, then its CLAP
/// features, else `Synth`.
pub fn classify_plugin(plugin: Option<PluginRef<'_>>, plugin_name: Option<&str>, clap_features: &[String]) -> KeyboardType {
    if let Some(t) = plugin.and_then(catalog_type).filter(|t| *t != KeyboardType::Synth) {
        return t;
    }
    if let Some(t) = plugin_name.and_then(classify_text) {
        return t;
    }
    for f in clap_features {
        match f.to_ascii_lowercase().as_str() {
            "drum" | "drum-machine" => return KeyboardType::Drums,
            // Not CLAP standard features, but plugins declare them.
            "piano" => return KeyboardType::Piano,
            "organ" => return KeyboardType::JazzOrgan,
            _ => {}
        }
    }
    KeyboardType::Synth
}

/// The type of a rig's instrument node. Order: (a) a dedicated catalog
/// instrument (setBfree, Aeolus, OneTrick KEYS, …) whatever it plays;
/// (b) the loaded patch's name, so Surge XT or Dexed playing a Rhodes or a
/// flute adapts; (c) the plugin's display name; (d) its CLAP features;
/// (e) `Synth`.
pub fn classify_instrument(
    kind: &NodeKind,
    patch_name: Option<&str>,
    plugin_name: Option<&str>,
    clap_features: &[String],
) -> KeyboardType {
    let plugin = PluginRef::of_node(kind);
    if let Some(t) = plugin.and_then(catalog_type).filter(|t| *t != KeyboardType::Synth) {
        return t;
    }
    if let Some(t) = patch_name.and_then(classify_text) {
        return t;
    }
    classify_plugin(plugin, plugin_name, clap_features)
}

/// Word cues per type, checked in this order: pipe before tonewheel organ
/// ("Pipe Organ"), electric before acoustic piano ("Electric Piano"), wind
/// and drums before bass ("Bass Clarinet", "Bass Drum").
const TEXT_CUES: &[(KeyboardType, &[&str])] = &[
    (KeyboardType::PipeOrgan, &[
        "pipe organ", "church organ", "church", "aeolus", "cathedral", "diapason",
        "principal chorus", "plenum", "vox humana", "positive organ",
    ]),
    (KeyboardType::JazzOrgan, &[
        "organ", "organs", "hammond", "tonewheel", "drawbar", "drawbars", "leslie", "farfisa", "setbfree", "b3",
    ]),
    (KeyboardType::ElectricPiano, &[
        "rhodes", "wurli", "wurlitzer", "ep", "epiano", "e.piano", "e-piano", "e piano", "electric piano",
        "suitcase", "tine", "tines",
    ]),
    (KeyboardType::Piano, &[
        "piano", "pianos", "grand", "upright", "clavinet", "clav", "harpsichord", "harpsi", "celesta",
        "pianoteq", "salamander", "honky tonk", "honky-tonk",
    ]),
    (KeyboardType::Wind, &[
        "flute", "flutes", "clarinet", "oboe", "bassoon", "contrabassoon", "sax", "saxophone", "trumpet",
        "trombone", "tuba", "horn", "horns", "brass", "recorder", "piccolo", "ocarina", "panflute",
        "shakuhachi", "harmonica", "bagpipe", "bagpipes", "reed", "reeds", "woodwind", "woodwinds", "winds",
        "ewi", "breath",
    ]),
    (KeyboardType::Drums, &[
        "drum", "drums", "drumkit", "drumkits", "drum kit", "kick", "kicks", "snare", "snares", "hihat",
        "hi-hat", "cymbal", "cymbals", "percussion",
    ]),
    (KeyboardType::Bass, &["bass", "basses", "bassline"]),
];

/// Patch folders that say what the sound is, whatever its name says
/// ("Basses/Classic Organ Bass" is a bass).
const TYPE_FOLDERS: &[(&str, KeyboardType)] = &[
    ("basses", KeyboardType::Bass),
    ("bass", KeyboardType::Bass),
    ("winds", KeyboardType::Wind),
    ("brass", KeyboardType::Wind),
    ("drums", KeyboardType::Drums),
    ("percussion", KeyboardType::Drums),
];

/// Folders that only say which family a sound is in; its name refines it
/// ("Keys/DX EP" is an electric piano, "Keys/Organ 1" an organ).
const FAMILY_FOLDERS: &[(&str, KeyboardType)] = &[
    ("keys", KeyboardType::Piano),
    ("keyboards", KeyboardType::Piano),
    ("organs", KeyboardType::JazzOrgan),
];

/// Folders of synth sounds: their names are no cue ("Splits/Trevor Horn…",
/// "Pads/Hassel Lost His New Horn", "Leads/Sax Destroyer").
const SYNTH_FOLDERS: &[&str] = &[
    "pads", "leads", "plucks", "polysynths", "synths", "chords", "sequences", "splits", "fx", "arps",
    "atmospheres", "drones", "soundscapes", "ambiance", "rhythms", "templates", "tutorials", "vocoder",
    "voices", "vox", "bells", "mallets", "strings", "guitars",
];

/// The type a patch label ("Winds/Flute 1", "Emu/Keys/Light EP") or a
/// plugin name says, if any.
fn classify_text(text: &str) -> Option<KeyboardType> {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let mut folders: Vec<&str> = lower.split('/').map(str::trim).collect();
    let name = folders.pop().unwrap_or_default();
    if let Some(&(_, t)) = TYPE_FOLDERS.iter().find(|(f, _)| folders.contains(f)) {
        return Some(t);
    }
    if SYNTH_FOLDERS.iter().any(|f| folders.contains(f)) {
        return None;
    }
    let words = TEXT_CUES.iter().find(|(_, cues)| cues.iter().any(|c| contains_cue(name, c)));
    if let Some(&(t, _)) = words {
        return Some(t);
    }
    FAMILY_FOLDERS.iter().find(|(f, _)| folders.contains(f)).map(|&(_, t)| t)
}

/// A cue matches whole words: no letter or digit before it ("sub3" is no
/// "b3"), no letter after it ("Hornet" is no "horn", "Bassoon" no "bass"),
/// while a number may follow ("EP1", "Organ2").
fn contains_cue(haystack: &str, cue: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(cue) {
        let start = from + pos;
        let end = start + cue.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Map vertical hit position (`0.0` top edge .. `1.0` bottom lip) on a Piano key
/// to an expressive dynamic velocity in `0.25..=1.0`.
pub fn piano_y_velocity(y_frac: f32) -> f32 {
    (0.25 + y_frac.clamp(0.0, 1.0) * 0.75).clamp(0.25, 1.0)
}

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "Bb", "B"];

/// Detect a concise note or chord name from currently sounding MIDI pitches.
pub fn detect_chord(notes: &[u8]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let mut sorted: Vec<u8> = notes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() == 1 {
        let m = sorted[0];
        let oct = (m as i32 / 12) - 1;
        return Some(format!("{}{}", NOTE_NAMES[(m % 12) as usize], oct));
    }

    let mut pcs: Vec<u8> = sorted.iter().map(|n| n % 12).collect();
    pcs.sort_unstable();
    pcs.dedup();

    if pcs.len() == 1 {
        let m = sorted[0];
        return Some(format!("{} oct", NOTE_NAMES[(m % 12) as usize]));
    }
    if pcs.len() == 2 {
        let iv = (pcs[1] + 12 - pcs[0]) % 12;
        if iv == 7 {
            return Some(format!("{}5", NOTE_NAMES[pcs[0] as usize]));
        }
        return Some(format!("{}+{}", NOTE_NAMES[pcs[0] as usize], NOTE_NAMES[pcs[1] as usize]));
    }

    const SHAPES: &[(&[u8], &str)] = &[
        (&[0, 4, 7, 11], "maj7"),
        (&[0, 3, 7, 10], "m7"),
        (&[0, 4, 7, 10], "7"),
        (&[0, 4, 7, 9],  "6"),
        (&[0, 3, 7, 9],  "m6"),
        (&[0, 4, 7, 2],  "add9"),
        (&[0, 4, 7],     ""),
        (&[0, 3, 7],     "m"),
        (&[0, 5, 7],     "sus4"),
        (&[0, 2, 7],     "sus2"),
        (&[0, 3, 6],     "dim"),
        (&[0, 4, 8],     "aug"),
    ];

    // Prefer bass note as root when it forms a clean shape.
    let bass_pc = sorted[0] % 12;
    let mut roots = Vec::with_capacity(pcs.len());
    roots.push(bass_pc);
    for &p in &pcs {
        if p != bass_pc {
            roots.push(p);
        }
    }

    for &root in &roots {
        let rel: Vec<u8> = pcs.iter().map(|&p| (p + 12 - root) % 12).collect();
        for &(pat, suffix) in SHAPES {
            if pat.len() == rel.len() && pat.iter().all(|iv| rel.contains(iv)) {
                return Some(format!("{}{}", NOTE_NAMES[root as usize], suffix));
            }
        }
    }
    // Relaxed subset match for extended voicings.
    for &root in &roots {
        let rel: Vec<u8> = pcs.iter().map(|&p| (p + 12 - root) % 12).collect();
        for &(pat, suffix) in SHAPES {
            if pat.iter().all(|iv| rel.contains(iv)) {
                return Some(format!("{}{}", NOTE_NAMES[root as usize], suffix));
            }
        }
    }

    Some(
        sorted
            .iter()
            .take(3)
            .map(|&m| NOTE_NAMES[(m % 12) as usize])
            .collect::<Vec<_>>()
            .join("·"),
    )
}

/// Returns `(in_scale, is_root)` for a MIDI note under `choice`.
pub fn note_in_scale(choice: ScaleChoice, midi: u8) -> (bool, bool) {
    let pc = (midi % 12) as i8;
    let rel = (pc - (choice.key % 12) as i8).rem_euclid(12);
    let in_s = choice.scale.intervals().contains(&rel);
    (in_s, rel == 0)
}

/// Degree label (e.g. `"1"`, `"b3"`, `"5"`) for an interval in semitones above the root.
pub const fn degree_label(semi: i8) -> &'static str {
    match semi.rem_euclid(12) {
        0 => "1",
        1 => "b2",
        2 => "2",
        3 => "b3",
        4 => "3",
        5 => "4",
        6 => "b5",
        7 => "5",
        8 => "b6",
        9 => "6",
        10 => "b7",
        11 => "7",
        _ => "",
    }
}

/// One key in GarageBand-style Scale Fold mode (only in-scale notes shown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldedKey {
    pub midi: u8,
    pub degree: &'static str,
    pub note_label: String,
    pub is_root: bool,
}

/// Build `count` consecutive in-scale keys starting at or just above `base_midi`.
pub fn folded_scale_keys(choice: ScaleChoice, base_midi: u8, count: usize) -> Vec<FoldedKey> {
    let intervals = choice.scale.intervals();
    let key = choice.key % 12;
    let base_oct = (base_midi / 12) as i32;
    let mut start_root = base_oct * 12 + key as i32;
    if start_root < base_midi as i32 {
        start_root += 12;
    }
    // Step back one octave if the root is far above base_midi so the range stays centered.
    if start_root - base_midi as i32 >= 7 && start_root >= 12 {
        start_root -= 12;
    }

    let mut out = Vec::with_capacity(count);
    let mut oct = 0i32;
    'outer: while out.len() < count {
        for &iv in intervals {
            let m = start_root + oct * 12 + iv as i32;
            if m > 127 {
                break 'outer;
            }
            if m >= 0 {
                let midi = m as u8;
                let oct_num = (m / 12) - 1;
                out.push(FoldedKey {
                    midi,
                    degree: degree_label(iv),
                    note_label: format!("{}{}", NOTE_NAMES[(midi % 12) as usize], oct_num),
                    is_root: iv == 0,
                });
                if out.len() == count {
                    break 'outer;
                }
            }
        }
        oct += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::Scale;
    use crate::control::Lv2Uri;

    fn surge() -> NodeKind {
        NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") }
    }

    #[test]
    fn dedicated_catalog_instruments_win_over_any_patch() {
        let b3 = NodeKind::Lv2 { uri: Lv2Uri::new(organ::B_SYNTH) };
        assert_eq!(classify_instrument(&b3, Some("Pads/Warm"), None, &[]), KeyboardType::JazzOrgan);
        let aeolus = NodeKind::Clap { plugin_id: crate::control::ClapId::new(aeolus::AEOLUS_ID) };
        assert_eq!(classify_instrument(&aeolus, None, None, &[]), KeyboardType::PipeOrgan);
        let clap = |id: &str| NodeKind::Clap { plugin_id: crate::control::ClapId::new(id) };
        assert_eq!(classify_instrument(&clap("com.punklabs.onetrick.cryptid"), Some("Keys/EP 1"), None, &[]), KeyboardType::Drums);
        assert_eq!(classify_instrument(&clap("com.punklabs.onetrick.chonk"), None, None, &[]), KeyboardType::Bass);
        assert_eq!(classify_instrument(&clap("org.starbreath.starbreath"), None, None, &[]), KeyboardType::Wind);
        assert_eq!(classify_instrument(&clap("com.punklabs.onetrick.keys"), None, None, &[]), KeyboardType::Piano);
        // Every URI of a card counts, not only the first.
        let kit = NodeKind::Lv2 { uri: Lv2Uri::new("http://gareus.org/oss/lv2/avldrums#BuskmansHoliday") };
        assert_eq!(classify_instrument(&kit, None, None, &[]), KeyboardType::Drums);
        // A general synth's card says Synth: its patch decides.
        assert_eq!(classify_instrument(&surge(), Some("Winds/Flute 1"), Some("Surge XT"), &[]), KeyboardType::Wind);
    }

    /// Real labels as the Surge XT library lists them (category/name, rule
    /// 63, from /usr/share/surge-xt) and as `patch_names` keeps them (the
    /// bare name), and real installed plugin and Dexed cartridge names.
    #[test]
    fn real_patch_and_plugin_names_classify() {
        use KeyboardType::*;
        let table: &[(&str, KeyboardType)] = &[
            ("Keys/DX EP", ElectricPiano),
            ("Keys/EP 1", ElectricPiano),
            ("Keys/Soft Suitcase", ElectricPiano),
            ("Emu/Keys/Light EP", ElectricPiano),
            ("Bluelight/Keys/Treated Rhodes", ElectricPiano),
            ("Cybersoda/Keys/Wurli", ElectricPiano),
            ("Treated Rhodes", ElectricPiano),
            ("DX EP", ElectricPiano),
            ("Keys/Digi Harpsi", Piano),
            ("Keys/Circus 1", Piano),
            ("Keys/Organ 1", JazzOrgan),
            ("House Organ", JazzOrgan),
            ("Keys/Church", PipeOrgan),
            ("John Valentine/Keys/Pipe Organ Flutes & Reeds", PipeOrgan),
            ("Winds/Flute 1", Wind),
            ("Winds/Low", Wind),
            ("Brass/Synth Brass 1", Wind),
            ("Bluelight/Brass/Soprano Sax", Wind),
            ("John Valentine/Winds/Bassoon", Wind),
            ("Bassoon", Wind),
            ("Contrabassoon", Wind),
            ("Clarinet", Wind),
            ("Emu/Keys/Trombone Harmonica", Wind),
            ("Percussion/Kick 909ish", Drums),
            ("Percussion/Synth Tom 1", Drums),
            ("Argitoth/Drums/Snare", Drums),
            ("Kick 909ish", Drums),
            ("Drum One", Drums),
            ("Basses/Bass 1", Bass),
            ("Basses/Attacky", Bass),
            ("Kinsey Dulcet/Basses/Classic Organ Bass + Bonus Organ", Bass),
            ("Rare Earth/Basses/Bass Tuba", Bass),
            ("Bass 1", Bass),
            // Synth folders: the name is no cue.
            ("Bluelight/Splits/Trevor Horn Is My Mother 110 BPM", Synth),
            ("Emu/Leads/Sax Destroyer", Synth),
            ("Bluelight/Pads/Hassel Lost His New Horn", Synth),
            ("John Valentine/Guitars/Nylon Acoustic Guitar", Synth),
            ("Leads/Octave Saw", Synth),
            // Word boundaries: no "b3" in "Sub3", no "horn" in a Hornet,
            // no "ep" in "Deep".
            ("Sub3 Deep", Synth),
            ("Nick Moritz/Basses/Deep Stack", Bass),
            ("GxHornet", Synth),
            ("Deep Jungler", Synth),
            // Installed plugin names.
            ("Surge XT", Synth),
            ("Dexed", Synth),
            ("Odin 2", Synth),
            ("Six Sines", Synth),
            ("YANES", Synth),
            ("setBfree DSP Tonewheel Organ", JazzOrgan),
            ("Aeolus", PipeOrgan),
            // Dexed_01.syx voice names.
            ("ARP 2600  ", Synth),
            ("Slow3D Pad", Synth),
            ("RUMBLE   1", Synth),
        ];
        let wrong: Vec<String> = table.iter()
            .filter(|(label, want)| classify_instrument(&surge(), Some(label), Some("Surge XT"), &[]) != *want)
            .map(|(label, want)| format!("{label:?}: want {want:?}, got {:?}", classify_instrument(&surge(), Some(label), Some("Surge XT"), &[])))
            .collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        // CLAP features when the names say nothing.
        assert_eq!(classify_plugin(None, Some("Mystery"), &["instrument".into(), "drum-machine".into()]), Drums);
        assert_eq!(classify_plugin(None, Some("Mystery"), &["instrument".into(), "piano".into()]), Piano);
        assert_eq!(classify_plugin(None, Some("Mystery"), &["instrument".into(), "synthesizer".into()]), Synth);
    }

    #[test]
    fn every_catalog_instrument_card_has_a_type() {
        for e in crate::catalog::CATALOG {
            let is_inst = e.plugin_type == crate::plugins::PluginType::Instrument;
            assert_eq!(e.instrument.is_some(), is_inst, "{}: instrument {:?} vs plugin type {:?}", e.id, e.instrument, e.plugin_type);
            // The lookup by plugin identity finds the card's type.
            if let Some(t) = e.instrument {
                for p in e.clap_id.map(PluginRef::Clap).into_iter().chain(e.lv2_uris.iter().map(|u| PluginRef::Lv2(u))) {
                    assert_eq!(catalog_type(p), Some(t), "{} {p:?}", e.id);
                }
            }
        }
    }

    #[test]
    fn parses_all_keyboard_types_and_round_trips() {
        for t in KeyboardType::ALL {
            assert_eq!(KeyboardType::parse(t.as_str()), Some(t));
            assert_eq!(KeyboardType::parse(t.label()), Some(t), "{}", t.label());
            assert_eq!(KeyboardMode::parse(t.as_str()), Some(KeyboardMode::Pinned(t)));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()), "serde uses the machine name");
            assert_eq!(serde_json::from_str::<KeyboardType>(&json).unwrap(), t);
        }
        // The names sessions saved before #198 still parse.
        for (old, t) in [("piano", KeyboardType::Piano), ("jazz_organ", KeyboardType::JazzOrgan), ("pipe_organ", KeyboardType::PipeOrgan), ("synth", KeyboardType::Synth)] {
            assert_eq!(serde_json::from_str::<KeyboardType>(&format!("\"{old}\"")).unwrap(), t);
            assert_eq!(serde_json::from_str::<KeyboardMode>(&format!("\"{old}\"")).unwrap(), KeyboardMode::Pinned(t));
        }
        assert_eq!(KeyboardMode::parse("auto"), Some(KeyboardMode::Auto));
        assert_eq!(KeyboardType::parse("church"), Some(KeyboardType::PipeOrgan));
        assert_eq!(KeyboardType::parse("hammond"), Some(KeyboardType::JazzOrgan));
        for (alias, t) in [
            ("rhodes", KeyboardType::ElectricPiano), ("wurli", KeyboardType::ElectricPiano), ("ep", KeyboardType::ElectricPiano),
            ("epiano", KeyboardType::ElectricPiano), ("E-Piano", KeyboardType::ElectricPiano),
            ("sax", KeyboardType::Wind), ("flute", KeyboardType::Wind), ("brass", KeyboardType::Wind), ("ewi", KeyboardType::Wind),
            ("breath", KeyboardType::Wind), ("kit", KeyboardType::Drums), ("percussion", KeyboardType::Drums),
            ("pads", KeyboardType::Drums), ("bass guitar", KeyboardType::Bass),
        ] {
            assert_eq!(KeyboardType::parse(alias), Some(t), "{alias}");
        }
        assert_eq!(KeyboardType::parse("theremin"), None);
    }

    #[test]
    fn detects_single_notes_and_chords() {
        assert_eq!(detect_chord(&[]), None);
        assert_eq!(detect_chord(&[60]), Some("C4".into()));
        assert_eq!(detect_chord(&[60, 64, 67]), Some("C".into()));
        assert_eq!(detect_chord(&[62, 65, 69]), Some("Dm".into()));
        assert_eq!(detect_chord(&[60, 64, 67, 71]), Some("Cmaj7".into()));
        assert_eq!(detect_chord(&[62, 65, 69, 72]), Some("Dm7".into()));
        assert_eq!(detect_chord(&[67, 71, 74, 77]), Some("G7".into()));
        assert_eq!(detect_chord(&[60, 67]), Some("C5".into()));
    }

    #[test]
    fn scale_fold_emits_only_in_scale_notes_with_roots_marked() {
        let d_min = ScaleChoice { key: 2, scale: Scale::Minor };
        let keys = folded_scale_keys(d_min, 48, 15);
        assert_eq!(keys.len(), 15);
        assert!(keys.iter().all(|k| note_in_scale(d_min, k.midi).0), "every folded key is in D minor");
        assert!(keys[0].is_root && keys[0].degree == "1" && keys[0].note_label == "D3");
        assert_eq!(keys[2].degree, "b3");
        assert_eq!(keys[2].note_label, "F3");
        assert!(keys[7].is_root && keys[7].note_label == "D4");
    }

    #[test]
    fn piano_y_velocity_scales_soft_top_to_loud_bottom() {
        assert!((piano_y_velocity(0.0) - 0.25).abs() < 1e-6);
        assert!((piano_y_velocity(1.0) - 1.0).abs() < 1e-6);
        assert!(piano_y_velocity(0.75) > piano_y_velocity(0.25));
    }

    #[test]
    fn pipe_organ_pistons_resolve_to_real_aeolus_registrations() {
        for i in 0..PIPE_ORGAN_PISTONS.len() {
            assert!(pipe_piston_registration(i).is_some(), "piston {i} resolves");
        }
    }

    #[test]
    fn every_keyboard_type_has_its_own_surface() {
        let mut seen = std::collections::HashSet::new();
        for t in KeyboardType::ALL {
            assert!(seen.insert(t.surface()), "{t:?} shares a surface");
        }
        assert_eq!(seen.len(), KeyboardType::ALL.len());
    }

    #[test]
    fn breath_target_round_trips_and_y_breath_scales() {
        for b in BreathTarget::ALL {
            assert_eq!(BreathTarget::parse(b.as_str()), Some(b));
        }
        assert_eq!(BreathTarget::parse("breath"), Some(BreathTarget::Cc2));
        assert_eq!(BreathTarget::parse("expression"), Some(BreathTarget::Cc11));
        assert_eq!(BreathTarget::parse("pressure"), Some(BreathTarget::Aftertouch));
        assert!((wind_y_breath(0.0) - 0.15).abs() < 1e-6);
        assert!((wind_y_breath(1.0) - 1.0).abs() < 1e-6);
        assert!(wind_y_breath(0.8) > wind_y_breath(0.2));
    }

    #[test]
    fn bass_fretboard_maps_notes_to_lowest_position() {
        assert_eq!(bass_fret_note(3, 0), 28); // open E1
        assert_eq!(bass_fret_note(0, 12), 55); // G3 (12th fret on G string)
        assert_eq!(bass_note_position(28, 15), Some((3, 0))); // E1 = open E
        assert_eq!(bass_note_position(33, 15), Some((2, 0))); // A1 = open A, not 5th fret E
        assert_eq!(bass_note_position(43, 15), Some((0, 0))); // G2 = open G
        assert_eq!(bass_note_position(27, 15), None); // below E1
        assert_eq!(bass_note_position(59, 15), None); // above 15th fret G (58)
    }

    #[test]
    fn drum_pads_have_unique_gm_notes_and_center_is_loudest() {
        let mut notes = std::collections::HashSet::new();
        for pad in DRUM_PADS {
            assert!((35..=81).contains(&pad.note), "GM drum range: {}", pad.note);
            assert!(notes.insert(pad.note), "duplicate GM note {}", pad.note);
        }
        assert!((drum_pad_velocity(0.5, 0.5) - 1.0).abs() < 1e-6);
        assert!(drum_pad_velocity(0.5, 0.5) > drum_pad_velocity(0.1, 0.5));
        assert!(drum_pad_velocity(0.0, 0.0) >= 0.25);
    }
}
