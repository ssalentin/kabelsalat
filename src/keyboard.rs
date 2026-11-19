//! Instrument-adaptive virtual keyboard surfaces (issue #163):
//! pure classification, chord detection, scale folding, tonewheel presets,
//! and interaction math shared by `ui.rs`, `session.rs`, `assistant.rs` and `mcp.rs`.

use serde::{Deserialize, Serialize};

use crate::aeolus;
use crate::arp::ScaleChoice;
use crate::control::NodeKind;
use crate::organ;

/// Which instrument-native virtual keyboard surface the dock renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardType {
    /// Weighted piano / electric piano: dynamics, damper & soft pedals, chord readout.
    Piano,
    /// Tonewheel jazz/rock organ (setBfree B3): 9 drawbars, Leslie, swell, waterfall gliss, dual manual.
    JazzOrgan,
    /// Classical church / pipe organ (Aeolus): divisions, stop jamb, couplers, pistons, tremulant, swell, pedalboard.
    PipeOrgan,
    /// Synthesizer (Surge XT, Vitalium, Dexed, Odin 2, YANES, fallback): Pitch/Mod, macros, X/Y pad, scale/fold, arp.
    Synth,
}

impl KeyboardType {
    pub const ALL: [KeyboardType; 4] = [
        KeyboardType::Piano,
        KeyboardType::JazzOrgan,
        KeyboardType::PipeOrgan,
        KeyboardType::Synth,
    ];

    /// Display label on full-width headers and chips.
    pub const fn label(self) -> &'static str {
        match self {
            KeyboardType::Piano => "Piano",
            KeyboardType::JazzOrgan => "Jazz Organ",
            KeyboardType::PipeOrgan => "Pipe Organ",
            KeyboardType::Synth => "Synth",
        }
    }

    /// Compact label for narrow windows.
    pub const fn short_label(self) -> &'static str {
        match self {
            KeyboardType::Piano => "Piano",
            KeyboardType::JazzOrgan => "Jazz",
            KeyboardType::PipeOrgan => "Pipe",
            KeyboardType::Synth => "Synth",
        }
    }

    /// Machine-readable name for MCP/assistant JSON and sessions.
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyboardType::Piano => "piano",
            KeyboardType::JazzOrgan => "jazz_organ",
            KeyboardType::PipeOrgan => "pipe_organ",
            KeyboardType::Synth => "synth",
        }
    }

    /// Parse a human or tool string into a surface type.
    pub fn parse(s: &str) -> Option<KeyboardType> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "piano" | "grand" | "ep" | "epiano" | "electric_piano" | "keys" => Some(KeyboardType::Piano),
            "jazz_organ" | "jazz" | "organ" | "tonewheel" | "hammond" | "b3" | "setbfree" => Some(KeyboardType::JazzOrgan),
            "pipe_organ" | "pipe" | "church" | "church_organ" | "cathedral" | "aeolus" => Some(KeyboardType::PipeOrgan),
            "synth" | "synthesizer" | "pad" | "lead" | "default" => Some(KeyboardType::Synth),
            _ => None,
        }
    }
}

/// Dock keyboard surface selection: follow the active rig's instrument, or pin one surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum KeyboardMode {
    #[default]
    Auto,
    Pinned(KeyboardType),
}

impl KeyboardMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyboardMode::Auto => "auto",
            KeyboardMode::Pinned(t) => t.as_str(),
        }
    }

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

impl Serialize for KeyboardMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
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

/// Classify an instrument node into its natural virtual keyboard representation.
///
/// Priority:
/// 1. Known dedicated plugins (`setBfree` -> `JazzOrgan`, `Aeolus` -> `PipeOrgan`).
/// 2. Loaded patch/category or plugin display name keywords (so Surge XT / Dexed /
///    Floe loading a Rhodes, Wurli, Grand Piano, or Organ patch adapts automatically).
/// 3. CLAP plugin feature hints (`piano`, `organ`).
/// 4. Safe fallback: `Synth`.
pub fn classify_instrument(
    kind: &NodeKind,
    patch_name: Option<&str>,
    plugin_name: Option<&str>,
    clap_features: &[String],
) -> KeyboardType {
    if organ::is_organ(kind) {
        return KeyboardType::JazzOrgan;
    }
    if let NodeKind::Clap { plugin_id } = kind {
        if crate::control::name_str(plugin_id) == aeolus::AEOLUS_ID {
            return KeyboardType::PipeOrgan;
        }
    }

    // Check loaded patch name first (a synth playing a piano/organ patch adopts that surface),
    // then plugin display name.
    for text in [patch_name, plugin_name].into_iter().flatten() {
        if let Some(t) = classify_text(text) {
            return t;
        }
    }

    for f in clap_features {
        let lower = f.to_ascii_lowercase();
        if lower == "piano" {
            return KeyboardType::Piano;
        }
        if lower == "organ" {
            return KeyboardType::JazzOrgan;
        }
    }

    KeyboardType::Synth
}

fn classify_text(text: &str) -> Option<KeyboardType> {
    let lower = text.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    // Pipe / church organ cues first (more specific than generic "organ").
    const PIPE_CUES: &[&str] = &[
        "pipe organ", "church organ", "aeolus", "cathedral", "diapason",
        "principal chorus", "plenum", "vox humana", "positive organ",
    ];
    if PIPE_CUES.iter().any(|c| lower.contains(c)) {
        return Some(KeyboardType::PipeOrgan);
    }

    // Tonewheel / jazz organ cues.
    const JAZZ_ORGAN_CUES: &[&str] = &[
        "organ", "hammond", "tonewheel", "drawbar", "leslie", "farfisa", "setbfree", "b3",
    ];
    if JAZZ_ORGAN_CUES.iter().any(|c| contains_cue(&lower, c)) {
        return Some(KeyboardType::JazzOrgan);
    }

    // Piano / electric piano / acoustic keyboard cues.
    const PIANO_CUES: &[&str] = &[
        "piano", "grand", "upright", "rhodes", "wurli", "wurlitzer",
        "epiano", "e.piano", "e-piano", "clavinet", "clav", "harpsichord",
        "celesta", "pianoteq", "salamander", "suitcase", "keyboards/", "keys/",
    ];
    if PIANO_CUES.iter().any(|c| contains_cue(&lower, c)) {
        return Some(KeyboardType::Piano);
    }

    None
}

/// Match short cues (`b3`, `clav`) on word boundaries so "sub3" or "octave" don't false-match.
fn contains_cue(haystack: &str, cue: &str) -> bool {
    if cue.len() >= 5 || cue.contains('/') || cue.contains(' ') || cue.contains('.') || cue.contains('-') {
        return haystack.contains(cue);
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(cue) {
        let start = from + pos;
        let end = start + cue.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
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
    use crate::control::{name_bytes, Lv2Uri};

    #[test]
    fn classifies_dedicated_plugins_and_patch_categories() {
        let b3 = NodeKind::Lv2 { uri: Lv2Uri::new(organ::B_SYNTH) };
        assert_eq!(classify_instrument(&b3, None, None, &[]), KeyboardType::JazzOrgan);

        let aeolus = NodeKind::Clap { plugin_id: name_bytes(aeolus::AEOLUS_ID) };
        assert_eq!(classify_instrument(&aeolus, None, None, &[]), KeyboardType::PipeOrgan);

        let surge = NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") };
        assert_eq!(classify_instrument(&surge, None, Some("Surge XT"), &[]), KeyboardType::Synth);
        assert_eq!(classify_instrument(&surge, Some("Keys/Wurli Warm"), Some("Surge XT"), &[]), KeyboardType::Piano);
        assert_eq!(classify_instrument(&surge, Some("Keyboards/Rhodes Mk1"), Some("Surge XT"), &[]), KeyboardType::Piano);
        assert_eq!(classify_instrument(&surge, Some("Organ/Jazz B3"), Some("Surge XT"), &[]), KeyboardType::JazzOrgan);
        assert_eq!(classify_instrument(&surge, Some("Church Organ Plenum"), Some("Surge XT"), &[]), KeyboardType::PipeOrgan);

        // Word-boundary safety: "Sub3 Bass" must stay Synth, not JazzOrgan ("b3").
        assert_eq!(classify_instrument(&surge, Some("Bass/Sub3 Deep"), Some("Surge XT"), &[]), KeyboardType::Synth);
        // "Octave Lead" must stay Synth, not Piano ("clav").
        assert_eq!(classify_instrument(&surge, Some("Leads/Octave Saw"), Some("Surge XT"), &[]), KeyboardType::Synth);

        // CLAP feature fallback.
        assert_eq!(classify_instrument(&surge, None, Some("Mystery"), &["instrument".into(), "piano".into()]), KeyboardType::Piano);
    }

    #[test]
    fn parses_all_keyboard_types_and_round_trips() {
        for t in KeyboardType::ALL {
            assert_eq!(KeyboardType::parse(t.as_str()), Some(t));
            assert_eq!(KeyboardType::parse(t.label()), Some(t));
        }
        assert_eq!(KeyboardType::parse("church"), Some(KeyboardType::PipeOrgan));
        assert_eq!(KeyboardType::parse("hammond"), Some(KeyboardType::JazzOrgan));
        assert_eq!(KeyboardType::parse("rhodes"), None);
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
}
