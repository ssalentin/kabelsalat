//! Arpeggiator, chord mode and scale snap (issue #90) — GarageBand
//! smart-keyboard parity.
//!
//! Three features, one file:
//!
//! - **Arp** — a MIDI-generating rack unit that sits between a keyboard
//!   source and a synth like an effect: held notes become an arpeggio
//!   (up/down/up-down/chord/random), the step clock is the song clock's beat
//!   position (rule 43), with octave span, gate and swing. Chord mode turns
//!   every held key into a whole chord (triad/7th/power, maj/min/sus
//!   shapes) sustained while the key is down, like the iOS chord strips.
//! - **Scale snap** — a property of the Keys source (like `split`):
//!   out-of-scale keys snap to the nearest in-scale note on their way out.
//!
//! RT discipline: [`ArpState`] is fixed-size (`Copy`-like arrays, no heap)
//! and lives on the audio thread; `note_in`/`note_out` run inside
//! `GraphEngine::apply` (block boundaries), `tick` inside `render`.
//! Every note the arp starts is tracked in `down`, so any release path can
//! silence what it started (rule 41 — the held-notes check drives this file).

use serde::{Deserialize, Serialize};

use crate::control::NodeId;

/// Engine param ids for the arp's knobs (`Command::SetParam`; rule 67's one
/// knob table maps labels onto these).
pub const P_ARP_MODE: u32 = 70;
pub const P_ARP_RATE: u32 = 71;
pub const P_ARP_OCTAVE: u32 = 72;
pub const P_ARP_GATE: u32 = 73;
pub const P_ARP_SWING: u32 = 74;
pub const P_ARP_CHORD: u32 = 75;

// ---------------------------------------------------------------- scales --

/// A musical scale, as semitone offsets from its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scale {
    Major,
    /// Natural minor (Aeolian).
    Minor,
    Dorian,
    Mixolydian,
    HarmonicMinor,
    MajorPentatonic,
    MinorPentatonic,
    Blues,
}

impl Scale {
    /// Every scale the Keys face and the agent offer, in menu order.
    pub const ALL: [Scale; 8] = [
        Scale::Major,
        Scale::Minor,
        Scale::Dorian,
        Scale::Mixolydian,
        Scale::HarmonicMinor,
        Scale::MajorPentatonic,
        Scale::MinorPentatonic,
        Scale::Blues,
    ];

    /// Semitones above the key that are in scale.
    pub fn intervals(self) -> &'static [i8] {
        match self {
            Scale::Major => &[0, 2, 4, 5, 7, 9, 11],
            Scale::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Scale::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Scale::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Scale::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Scale::MajorPentatonic => &[0, 2, 4, 7, 9],
            Scale::MinorPentatonic => &[0, 3, 5, 7, 10],
            Scale::Blues => &[0, 3, 5, 6, 7, 10],
        }
    }

    /// Name for the Keys readout and the agent ("MAJOR", "MIN PENTA" …).
    pub fn label(self) -> &'static str {
        match self {
            Scale::Major => "MAJOR",
            Scale::Minor => "MINOR",
            Scale::Dorian => "DORIAN",
            Scale::Mixolydian => "MIXOLYDIAN",
            Scale::HarmonicMinor => "HARM MINOR",
            Scale::MajorPentatonic => "MAJ PENTA",
            Scale::MinorPentatonic => "MIN PENTA",
            Scale::Blues => "BLUES",
        }
    }

    /// Resolve a scale by (case-insensitive) name or label, with the
    /// spellings a player types ("minor", "pentatonic" → major pentatonic).
    pub fn from_name(name: &str) -> Option<Scale> {
        let q = name.trim().to_ascii_lowercase();
        Scale::ALL.into_iter().find(|s| {
            let l = s.label().to_ascii_lowercase();
            l == q || format!("{s:?}").to_ascii_lowercase() == q
        }).or(match q.as_str() {
            "penta" | "pentatonic" => Some(Scale::MajorPentatonic),
            "aeolian" => Some(Scale::Minor),
            _ => None,
        })
    }
}

/// Key + scale of a Keys source: `key` is the pitch class the scale starts
/// on (0 = C). `None` on the node = no snapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleChoice {
    pub scale: Scale,
    pub key: u8,
}

impl ScaleChoice {
    /// "C MAJOR" — the Keys readout, the agent's answer, the session.
    pub fn label(self) -> String {
        format!("{} {}", KEY_NAMES[self.key.min(11) as usize], self.scale.label())
    }
}

/// Pitch-class names, sharps only (the app's keyboard shows them this way).
pub const KEY_NAMES: [&str; 12] =
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// Resolve a key by letter ("c", "F#", "bes"→no) or number (0..=11).
pub fn key_from_name(name: &str) -> Option<u8> {
    let q = name.trim();
    if let Ok(n) = q.parse::<u8>() {
        return (n <= 11).then_some(n);
    }
    let lower = q.to_ascii_lowercase();
    KEY_NAMES.iter().position(|k| k.to_ascii_lowercase() == lower).map(|i| i as u8)
}

/// Is `note` in the scale of `choice`?
pub fn in_scale(note: u8, choice: ScaleChoice) -> bool {
    let pc = (note as i16 - choice.key as i16).rem_euclid(12) as i8;
    choice.scale.intervals().contains(&pc)
}

/// Snap `note` to the nearest in-scale note (rule 90: "out-of-scale keys
/// snap to the nearest in-scale note"). In-scale notes pass unchanged;
/// ties go DOWN (the flat side), like GarageBand's snapped keyboard.
/// Pure, deterministic: a note-off snaps to the same note its note-on
/// snapped to, so releases always meet their pitch (rule 41).
pub fn snap_note(note: u8, choice: ScaleChoice) -> u8 {
    if in_scale(note, choice) {
        return note;
    }
    for d in 1..=6i16 {
        let down = note as i16 - d;
        if down >= 0 && in_scale(down as u8, choice) {
            return down as u8;
        }
        let up = note as i16 + d;
        if up <= 127 && in_scale(up as u8, choice) {
            return up as u8;
        }
    }
    note
}

// ---------------------------------------------------------------- chords --

/// What shape of chord one held key becomes in chord mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChordShape {
    Major,
    Minor,
    Sus2,
    Sus4,
    Dominant7,
    Major7,
    Minor7,
    /// Root + fifth (the power chord).
    Power,
}

impl ChordShape {
    /// Menu order of the CHORD knob.
    pub const ALL: [ChordShape; 8] = [
        ChordShape::Major,
        ChordShape::Minor,
        ChordShape::Sus2,
        ChordShape::Sus4,
        ChordShape::Dominant7,
        ChordShape::Major7,
        ChordShape::Minor7,
        ChordShape::Power,
    ];

    pub fn intervals(self) -> &'static [u8] {
        match self {
            ChordShape::Major => &[4, 7],
            ChordShape::Minor => &[3, 7],
            ChordShape::Sus2 => &[2, 7],
            ChordShape::Sus4 => &[5, 7],
            ChordShape::Dominant7 => &[4, 7, 10],
            ChordShape::Major7 => &[4, 7, 11],
            ChordShape::Minor7 => &[3, 7, 10],
            ChordShape::Power => &[7],
        }
    }

    /// The CHORD knob's readout ("MAJ", "MIN7", "5" …).
    pub fn label(self) -> &'static str {
        match self {
            ChordShape::Major => "MAJ",
            ChordShape::Minor => "MIN",
            ChordShape::Sus2 => "SUS2",
            ChordShape::Sus4 => "SUS4",
            ChordShape::Dominant7 => "7",
            ChordShape::Major7 => "MAJ7",
            ChordShape::Minor7 => "MIN7",
            ChordShape::Power => "5",
        }
    }

    /// Resolve by (case-insensitive) name or label ("power", "dominant 7").
    pub fn from_name(name: &str) -> Option<ChordShape> {
        let q = name.trim().to_ascii_lowercase();
        ChordShape::ALL.into_iter().find(|c| {
            let l = c.label().to_ascii_lowercase();
            l == q || format!("{c:?}").to_ascii_lowercase() == q
        }).or(match q.as_str() {
            "dominant7" | "dominant 7" | "dom" => Some(ChordShape::Dominant7),
            "power chord" | "powerchord" => Some(ChordShape::Power),
            _ => None,
        })
    }

    fn from_index(i: usize) -> ChordShape {
        ChordShape::ALL[i.min(ChordShape::ALL.len() - 1)]
    }

    fn index(self) -> usize {
        ChordShape::ALL.into_iter().position(|c| c == self).unwrap_or(0)
    }
}

/// Most chord notes any shape makes (root + 3 intervals) — stack size.
pub const MAX_CHORD: usize = 4;

/// The notes of `shape` rooted at `root`: the root first, then each
/// interval clamped into 0..=127 (out-of-range intervals are skipped, so
/// the very top of the keyboard still makes the notes that fit).
pub fn chord_notes(root: u8, shape: ChordShape) -> ([u8; MAX_CHORD], usize) {
    let mut out = [0u8; MAX_CHORD];
    let mut n = 0;
    if root <= 127 {
        out[n] = root;
        n += 1;
    }
    for &i in shape.intervals() {
        let note = root as i16 + i as i16;
        if (0..=127).contains(&note) {
            out[n] = note as u8;
            n += 1;
        }
    }
    (out, n)
}

// ------------------------------------------------------------- arp clock --

/// Arp pattern order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArpMode {
    Up,
    Down,
    /// Up then down without repeating either end.
    UpDown,
    /// Every held key is a whole chord (CHORD shape), sustained while held.
    Chord,
    Random,
}

impl ArpMode {
    /// Menu order of the MODE knob.
    pub const ALL: [ArpMode; 5] =
        [ArpMode::Up, ArpMode::Down, ArpMode::UpDown, ArpMode::Chord, ArpMode::Random];

    /// The MODE knob's readout.
    pub fn label(self) -> &'static str {
        match self {
            ArpMode::Up => "UP",
            ArpMode::Down => "DOWN",
            ArpMode::UpDown => "U-D",
            ArpMode::Chord => "CHORD",
            ArpMode::Random => "RAND",
        }
    }

    /// Resolve by (case-insensitive) name or label ("updown", "chords").
    pub fn from_name(name: &str) -> Option<ArpMode> {
        let q = name.trim().to_ascii_lowercase();
        ArpMode::ALL.into_iter().find(|m| {
            let l = m.label().to_ascii_lowercase();
            l == q || format!("{m:?}").to_ascii_lowercase() == q
        }).or(match q.as_str() {
            "updown" | "up-down" | "up down" => Some(ArpMode::UpDown),
            "chords" => Some(ArpMode::Chord),
            "rnd" => Some(ArpMode::Random),
            _ => None,
        })
    }

    fn from_index(i: usize) -> ArpMode {
        ArpMode::ALL[i.min(ArpMode::ALL.len() - 1)]
    }

    fn index(self) -> usize {
        ArpMode::ALL.into_iter().position(|m| m == self).unwrap_or(0)
    }
}

/// Step rate in note values, synced to the song clock (rule 43): one step
/// every `beats_per_step()` quarter notes. T = triplet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArpRate {
    Quarter,
    Eighth,
    EighthT,
    Sixteenth,
    SixteenthT,
    ThirtySecond,
}

impl ArpRate {
    /// Menu order of the RATE knob.
    pub const ALL: [ArpRate; 6] = [
        ArpRate::Quarter,
        ArpRate::Eighth,
        ArpRate::EighthT,
        ArpRate::Sixteenth,
        ArpRate::SixteenthT,
        ArpRate::ThirtySecond,
    ];

    /// Quarter-note beats one step lasts.
    pub fn beats_per_step(self) -> f64 {
        match self {
            ArpRate::Quarter => 1.0,
            ArpRate::Eighth => 0.5,
            ArpRate::EighthT => 1.0 / 3.0,
            ArpRate::Sixteenth => 0.25,
            ArpRate::SixteenthT => 1.0 / 6.0,
            ArpRate::ThirtySecond => 0.125,
        }
    }

    /// The RATE knob's readout.
    pub fn label(self) -> &'static str {
        match self {
            ArpRate::Quarter => "1/4",
            ArpRate::Eighth => "1/8",
            ArpRate::EighthT => "1/8T",
            ArpRate::Sixteenth => "1/16",
            ArpRate::SixteenthT => "1/16T",
            ArpRate::ThirtySecond => "1/32",
        }
    }

    /// Resolve by (case-insensitive) name or label ("16", "sixteenth").
    pub fn from_name(name: &str) -> Option<ArpRate> {
        let q = name.trim().to_ascii_lowercase();
        ArpRate::ALL.into_iter().find(|r| {
            let l = r.label().to_ascii_lowercase();
            l == q || format!("{r:?}").to_ascii_lowercase() == q
        }).or(match q.as_str() {
            "quarter" | "1/4 note" => Some(ArpRate::Quarter),
            "eighth" | "8th" => Some(ArpRate::Eighth),
            "triplet" | "8t" | "eighth triplet" => Some(ArpRate::EighthT),
            "sixteenth" | "16th" => Some(ArpRate::Sixteenth),
            "16t" | "sixteenth triplet" => Some(ArpRate::SixteenthT),
            "thirtysecond" | "32nd" | "1/32 note" => Some(ArpRate::ThirtySecond),
            _ => None,
        })
    }

    fn from_index(i: usize) -> ArpRate {
        ArpRate::ALL[i.min(ArpRate::ALL.len() - 1)]
    }

    fn index(self) -> usize {
        ArpRate::ALL.into_iter().position(|r| r == self).unwrap_or(3)
    }
}

// --------------------------------------------------------------- the arp --

/// Held keys an [`ArpState`] tracks (more than this many distinct notes and
/// the pattern keeps the lowest — a player has ten fingers).
const MAX_NOTES: usize = 32;
/// Which of `n` evenly spaced positions a 0..1 knob value means: the face's
/// own mapping (`Knob::step_index`), so the needle, the engine and the agent
/// tools agree on what every value is.
pub fn step_index(value: f64, n: usize) -> usize {
    ((value.clamp(0.0, 1.0) * (n.max(2) - 1) as f64).round() as usize).min(n.max(2) - 1)
}

/// The knob value of position `pos` out of `n` (inverse of `step_index`).
pub fn step_value(pos: usize, n: usize) -> f32 {
    pos.min(n.max(2) - 1) as f32 / (n.max(2) - 1) as f32
}

/// What a `set_arp` call (agent or MCP) asks for: (knob label, engine param,
/// 0..1 knob value) per setting named. One parser for the app and headless
/// MCP, so both tools mean the same thing (rule 6).
pub fn knob_values_from_args(args: &serde_json::Value) -> Result<Vec<(&'static str, u32, f32)>, String> {
    let num = |k: &str| args.get(k).and_then(|v| v.as_f64());
    let name = |k: &str| args.get(k).and_then(|v| v.as_str());
    let mut out = Vec::new();
    if let Some(m) = name("mode") {
        let m = ArpMode::from_name(m)
            .ok_or_else(|| format!("set_arp: '{m}' is not a mode (up, down, up-down, chord, random)"))?;
        out.push(("MODE", P_ARP_MODE, step_value(m.index(), ArpMode::ALL.len())));
    }
    if let Some(r) = name("rate") {
        let r = ArpRate::from_name(r)
            .ok_or_else(|| format!("set_arp: '{r}' is not a rate (1/4, 1/8, 1/8T, 1/16, 1/16T, 1/32)"))?;
        out.push(("RATE", P_ARP_RATE, step_value(r.index(), ArpRate::ALL.len())));
    }
    if let Some(o) = num("octaves").or_else(|| num("oct")) {
        let pos = (o.round().clamp(1.0, MAX_OCTAVES as f64) as usize) - 1;
        out.push(("OCT", P_ARP_OCTAVE, step_value(pos, MAX_OCTAVES as usize)));
    }
    if let Some(g) = num("gate") {
        out.push(("GATE", P_ARP_GATE, g.clamp(0.05, 1.0) as f32));
    }
    if let Some(w) = num("swing") {
        out.push(("SWING", P_ARP_SWING, w.clamp(0.0, 1.0) as f32));
    }
    if let Some(c) = name("chord") {
        let c = ChordShape::from_name(c)
            .ok_or_else(|| format!("set_arp: '{c}' is not a chord shape (maj, min, sus2, sus4, 7, maj7, min7, power)"))?;
        out.push(("CHORD", P_ARP_CHORD, step_value(c.index(), ChordShape::ALL.len())));
    }
    if out.is_empty() {
        return Err("set_arp: give at least one of mode, rate, octaves, gate, swing, chord".into());
    }
    Ok(out)
}

/// Longest pattern (MAX_NOTES × 4 octaves).
const MAX_SEQ: usize = MAX_NOTES * 4;

/// How many octaves the arp spans (1..=4).
pub const MAX_OCTAVES: u8 = 4;

/// One arp unit's settings — the face knobs, in the order the knob table
/// lists them. All fields are what the knobs publish (0..1 normalized for
/// the stepped ones; the engine quantizes defensively here).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArpSettings {
    pub mode: ArpMode,
    pub rate: ArpRate,
    /// 1..=4.
    pub octaves: u8,
    /// 0.05..=1.0 of one step that a note sounds.
    pub gate: f32,
    /// 0..=1; every odd step starts `swing/2` steps late.
    pub swing: f32,
    pub chord: ChordShape,
}

impl Default for ArpSettings {
    fn default() -> Self {
        ArpSettings {
            mode: ArpMode::Up,
            rate: ArpRate::Sixteenth,
            octaves: 1,
            gate: 0.75,
            swing: 0.0,
            chord: ChordShape::Major,
        }
    }
}

/// The per-node state the engine owns: which keys are held, which generated
/// notes are down, and where the step clock is. Fixed-size, no heap.
///
/// The clock is in SONG-CLOCK BEATS (rule 43): while the master transport
/// plays, the engine feeds `tick` the song position, so the pattern sits on
/// the music's grid; stopped, the engine feeds a free-running beat phase at
/// the same BPM (a live arp keeps arping between songs).
pub struct ArpState {
    pub node: NodeId,
    pub settings: ArpSettings,
    /// Held-note reference counts (one per source holding that key).
    held: [u8; 128],
    held_n: usize,
    /// Generated-note reference counts (chord notes of two keys can share a
    /// pitch; a burst is the notes one step started).
    down: [u8; 128],
    /// Index on the beat grid of the next step to fire (its onset, and the
    /// swing parity) and where in the pattern it plays (`pos`: a press starts
    /// the pattern over from its first note, wherever in the bar it lands).
    step: usize,
    pos: usize,
    next_on: f64,
    /// When the current burst's notes go off; `INFINITY` while none is down.
    next_off: f64,
    /// Velocity of the most recent held key (the burst's velocity).
    vel: f32,
    /// The free-running beat phase this arp ticks on while the master
    /// transport is stopped (the engine mirrors the song position into it
    /// while playing, so a stop continues seamlessly).
    pub phase: f64,
    rng: u32,
}

impl ArpState {
    pub fn new(node: NodeId) -> Self {
        Self {
            node,
            settings: ArpSettings::default(),
            held: [0; 128],
            held_n: 0,
            down: [0; 128],
            step: 0,
            pos: 0,
            next_on: f64::INFINITY,
            next_off: f64::INFINITY,
            vel: 0.8,
            phase: 0.0,
            rng: node.wrapping_mul(2654435761) | 1,
        }
    }

    /// Velocity the burst plays at (the last held key's).
    pub fn velocity(&self) -> f32 {
        self.vel
    }

    /// Beat time of pattern step `k` (swing shifts every odd step late).
    fn onset(&self, k: usize) -> f64 {
        let step_beats = self.settings.rate.beats_per_step();
        let mut t = k as f64 * step_beats;
        if k % 2 == 1 {
            t += self.settings.swing.clamp(0.0, 1.0) as f64 * step_beats * 0.5;
        }
        t
    }

    /// The first pattern step that starts at or after beat `t`, and its
    /// onset. The first call after a hold begins aligns the pattern to the
    /// music's grid instead of firing between steps.
    fn first_onset_at_or_after(&self, t: f64) -> (usize, f64) {
        let step_beats = self.settings.rate.beats_per_step();
        let mut k = (t / step_beats).floor().max(0.0) as usize;
        while self.onset(k) < t {
            k += 1;
        }
        (k, self.onset(k))
    }

    /// Emit note-offs for every generated note that is down. Safe to call
    /// any time (block boundary, routing edit, settings change): every note
    /// the arp started is tracked, so nothing can be orphaned (rule 41).
    fn all_off(&mut self, emit: &mut dyn FnMut(u8, bool)) {
        for (note, c) in self.down.iter_mut().enumerate() {
            if *c == 0 {
                continue;
            }
            *c = 0;
            emit(note as u8, false);
        }
        self.next_off = f64::INFINITY;
    }

    /// A key went down into the arp (the engine already routed + snapped
    /// it). Restarts the pattern when the first key lands; in chord mode
    /// the key's chord sounds at once and is sustained while held.
    pub fn note_in(&mut self, note: u8, vel: f32, now: f64, emit: &mut dyn FnMut(u8, bool)) {
        if note > 127 {
            return;
        }
        self.vel = vel.clamp(0.0, 1.0);
        let first = self.held[note as usize] == 0;
        self.held[note as usize] = self.held[note as usize].saturating_add(1);
        if first {
            self.held_n += 1;
            // First key: the pattern starts over from its first note, on the
            // next grid point.
            self.pos = 0;
            let (k, at) = self.first_onset_at_or_after(now);
            self.step = k;
            self.next_on = at;
        }
        if self.settings.mode == ArpMode::Chord {
            let (notes, n) = chord_notes(note, self.settings.chord);
            for &cn in &notes[..n] {
                if self.down[cn as usize] == 0 {
                    emit(cn, true);
                }
                self.down[cn as usize] = self.down[cn as usize].saturating_add(1);
            }
        }
    }

    /// A key came up. The burst the pattern is playing dies with it (any
    /// release silences what the arp had going — aggressive on purpose: a
    /// stuck generated note is the one hazard this unit adds), and in
    /// chord mode the key's chord notes come off. Whatever is still held
    /// keeps the pattern running.
    pub fn note_out(&mut self, note: u8, emit: &mut dyn FnMut(u8, bool)) {
        let Some(note) = (note <= 127).then_some(note as usize) else { return };
        if self.held[note] == 0 {
            return; // a release whose note-on never arrived (routing changed)
        }
        self.held[note] -= 1;
        if self.held[note] == 0 {
            self.held_n -= 1;
        }
        if self.settings.mode == ArpMode::Chord {
            let (notes, n) = chord_notes(note as u8, self.settings.chord);
            for &cn in &notes[..n] {
                let c = &mut self.down[cn as usize];
                if *c > 0 {
                    *c -= 1;
                    if *c == 0 {
                        emit(cn, false);
                    }
                }
            }
        } else {
            // Release the burst: the pattern re-fires on the next step
            // with whatever keys remain.
            self.all_off(emit);
        }
    }

    /// Clear held keys AND silence everything the arp generated — what the
    /// engine's `release_all` calls on this node for every routing edit.
    /// The caller releases the instruments the arp feeds as well.
    pub fn release_all(&mut self, emit: &mut dyn FnMut(u8, bool)) {
        self.held = [0; 128];
        self.held_n = 0;
        self.all_off(emit);
        self.step = 0;
        self.pos = 0;
        self.next_on = f64::INFINITY;
    }

    /// Apply one knob (engine param id, 0..1 normalized like every rack
    /// knob). A stepped knob quantizes here, so a mid-drag value never
    /// means a half-way mode. Settings that shape the burst or the clock
    /// release the sounding burst first (rule 41: its note-off time was
    /// computed under the old settings).
    pub fn set_param(&mut self, param: u32, value: f64, emit: &mut dyn FnMut(u8, bool)) -> bool {
        let v = value.clamp(0.0, 1.0) as f32;
        let s = &mut self.settings;
        match param {
            P_ARP_MODE => {
                let m = ArpMode::from_index(step_index(value, ArpMode::ALL.len()));
                if m == s.mode {
                    return true;
                }
                s.mode = m;
            }
            P_ARP_RATE => {
                let r = ArpRate::from_index(step_index(value, ArpRate::ALL.len()));
                if r == s.rate {
                    return true;
                }
                s.rate = r;
            }
            P_ARP_OCTAVE => {
                let o = step_index(value, MAX_OCTAVES as usize) as u8 + 1;
                if o == s.octaves {
                    return true;
                }
                s.octaves = o;
            }
            P_ARP_GATE => {
                let g = v.clamp(0.05, 1.0);
                if (g - s.gate).abs() < 1e-6 {
                    return true;
                }
                s.gate = g;
            }
            P_ARP_SWING => {
                let w = v.clamp(0.0, 1.0);
                if (w - s.swing).abs() < 1e-6 {
                    return true;
                }
                s.swing = w;
            }
            P_ARP_CHORD => {
                let c = ChordShape::from_index(step_index(value, ChordShape::ALL.len()));
                if c == s.chord {
                    return true;
                }
                s.chord = c;
            }
            _ => return false,
        }
        // What sounds now was started under the old settings: stop it, and
        // let the next tick reschedule on the new clock. Held keys stay.
        self.all_off(emit);
        if self.settings.mode == ArpMode::Chord && self.held_n > 0 {
            // The new shape re-sounds every held key at once, like the face
            // implies: the chord is a property of the keys being held.
            for note in 0..128u16 {
                if self.held[note as usize] == 0 {
                    continue;
                }
                let (notes, n) = chord_notes(note as u8, self.settings.chord);
                for &cn in &notes[..n] {
                    if self.down[cn as usize] == 0 {
                        emit(cn, true);
                    }
                    self.down[cn as usize] = self.down[cn as usize].saturating_add(1);
                }
            }
        }
        if self.settings.mode != ArpMode::Chord {
            self.next_on = f64::INFINITY; // reschedule from the next tick
        }
        true
    }

    /// The pattern over the held notes: every held note once per octave,
    /// in the mode's order.
    fn sequence(&mut self) -> ([u8; MAX_SEQ], usize) {
        let mut notes = [0u8; MAX_NOTES];
        let mut n = 0;
        for (note, c) in self.held.iter().enumerate() {
            if *c > 0 {
                notes[n] = note as u8;
                n += 1;
                if n == MAX_NOTES {
                    break; // more than a player has fingers: keep the lowest
                }
            }
        }
        let octaves = self.settings.octaves.clamp(1, MAX_OCTAVES) as usize;
        let mut seq = [0u8; MAX_SEQ];
        let mut len = 0;
        let push = |seq: &mut [u8; MAX_SEQ], len: &mut usize, note: u8| {
            if *len < seq.len() {
                seq[*len] = note;
                *len += 1;
            }
        };
        let up = |seq: &mut [u8; MAX_SEQ], len: &mut usize| {
            for oct in 0..octaves {
                for &note in &notes[..n] {
                    let shifted = note as i16 + 12 * oct as i16;
                    if (0..=127).contains(&shifted) {
                        push(seq, len, shifted as u8);
                    }
                }
            }
        };
        match self.settings.mode {
            ArpMode::Up | ArpMode::Chord => up(&mut seq, &mut len),
            ArpMode::Down => {
                up(&mut seq, &mut len);
                seq[..len].reverse();
            }
            ArpMode::UpDown => {
                let mut full = [0u8; MAX_SEQ];
                let mut fl = 0;
                up(&mut full, &mut fl);
                if fl > 1 {
                    // Up without the top, then down without the bottom:
                    // C E G E C E G E — no end repeats.
                    let (head, mid) = (fl - 1, fl - 1);
                    for &x in &full[..head] {
                        push(&mut seq, &mut len, x);
                    }
                    for i in (1..mid + 1).rev() {
                        push(&mut seq, &mut len, full[i]);
                    }
                } else {
                    len = 0;
                    up(&mut seq, &mut len);
                }
            }
            ArpMode::Random => {
                // One note of the up-sequence, picked by the xorshift clock.
                let mut full = [0u8; MAX_SEQ];
                let mut fl = 0;
                up(&mut full, &mut fl);
                if fl == 0 {
                    return (seq, 0);
                }
                // xorshift32: deterministic, allocation-free.
                let mut x = self.rng;
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                self.rng = x;
                push(&mut seq, &mut len, full[(x as usize) % fl]);
            }
        }
        (seq, len)
    }

    /// Advance the step clock through the beat window `[t0, t1)` (song-clock
    /// beats — the engine passes the transport position while it plays,
    /// a free-running phase when it does not). Due gate-offs fire before
    /// onsets, so a gate of 1.0 still retriggers cleanly. Chord mode never
    /// steps (its notes are sustained by the keys). Events land on block
    /// boundaries, the timing of every other note in this app.
    pub fn tick(&mut self, t0: f64, t1: f64, emit: &mut dyn FnMut(u8, bool)) {
        if self.settings.mode == ArpMode::Chord {
            return; // chord notes live and die with their keys
        }
        if self.held_n == 0 && self.next_off == f64::INFINITY {
            return; // nothing to do; the next first key reschedules
        }
        // The clock jumped (transport play/stop, a load): reschedule instead
        // of machine-gunning every missed step into one block.
        let step_beats = self.settings.rate.beats_per_step();
        if self.next_on != f64::INFINITY
            && (self.next_on < t0 - step_beats || self.next_on > t1 + 8.0 * step_beats)
        {
            self.all_off(emit);
            let (k, at) = self.first_onset_at_or_after(t0);
            self.step = k;
            self.next_on = at;
        }
        if self.held_n > 0 && self.next_on == f64::INFINITY {
            // Settings changed under held keys: resume from here.
            let (k, at) = self.first_onset_at_or_after(t0);
            self.step = k;
            self.next_on = at;
        }
        loop {
            // A burst whose gate time passed goes off before anything new.
            if self.next_off < t1 {
                self.all_off(emit);
                continue;
            }
            if self.held_n == 0 || self.next_on >= t1 {
                break;
            }
            // Cut a burst that ran past its gate (gate = 1.0: retrigger).
            if self.next_off != f64::INFINITY {
                self.all_off(emit);
            }
            let at = self.next_on;
            let (seq, len) = self.sequence();
            if len == 0 {
                break;
            }
            let note = seq[self.pos % len];
            self.pos += 1;
            if self.down[note as usize] == 0 {
                emit(note, true);
            }
            self.down[note as usize] = self.down[note as usize].saturating_add(1);
            self.next_off = at + self.settings.gate.clamp(0.05, 1.0) as f64 * step_beats;
            self.step += 1;
            self.next_on = self.onset(self.step);
        }
    }

    /// Notes the arp holds (engine tests + the held-notes check).
    pub fn held_notes(&self) -> impl Iterator<Item = u8> + '_ {
        (0..128u8).filter(move |&n| self.held[n as usize] > 0)
    }

    /// Generated notes that are down right now (self-test: chord mode emits
    /// 3 voices per held key).
    pub fn sounding(&self) -> Vec<u8> {
        (0..128u8).filter(|&n| self.down[n as usize] > 0).collect()
    }

    /// True while any key is held or any generated note is sounding. Allocation-free.
    pub fn is_active(&self) -> bool {
        self.held_n > 0 || self.down.iter().any(|&c| c > 0)
    }

    /// Everything that decides this arp's future behaviour, as a comparable
    /// string (the held-notes check's state key). The step counter only
    /// matters modulo the pattern length (≤ 4 here); the clock itself is
    /// relative, so it is left out.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{:?}|held {:?}|down {:?}|step {}|on {}|off {}",
            self.settings,
            self.held_notes().collect::<Vec<_>>(),
            self.sounding(),
            self.pos % 12,
            self.next_on.is_finite(),
            self.next_off.is_finite(),
        )
    }

    /// One "time passed" step for the held-notes check's Tick alphabet:
    /// advance the free-running phase through a window that certainly holds
    /// the next due onset, and process everything due on the way.
    #[cfg(test)]
    pub(crate) fn test_step(&mut self, emit: &mut dyn FnMut(u8, bool)) {
        let span = 2.0 * self.settings.rate.beats_per_step() + 1e-9;
        let t1 = self.phase + span;
        self.tick(self.phase, t1, emit);
        self.phase = t1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One arp with a logging emit closure: (note, on) events.
    struct Log {
        events: Vec<(u8, bool)>,
    }

    impl Log {
        fn new() -> Self {
            Log { events: Vec::new() }
        }
        fn emit(&mut self) -> impl FnMut(u8, bool) + '_ {
            |note, on| self.events.push((note, on))
        }
        /// Note-ons in order.
        fn ons(&self) -> Vec<u8> {
            self.events.iter().filter(|e| e.1).map(|e| e.0).collect()
        }
        /// Down-count after replaying all events.
        fn down(&self) -> Vec<u8> {
            let mut down = vec![0u8; 128];
            for &(n, on) in &self.events {
                down[n as usize] = if on { down[n as usize] + 1 } else { down[n as usize].saturating_sub(1) };
            }
            down.into_iter().enumerate().filter(|(_, c)| *c > 0).map(|(n, _)| n as u8).collect()
        }
    }

    /// `now` for note_in: the tick domain in these tests.
    const NOW: f64 = 0.0;

    fn arp() -> ArpState {
        ArpState::new(9)
    }

    // -- scales ----------------------------------------------------------

    #[test]
    fn in_scale_notes_pass_and_out_of_scale_snap_nearest() {
        let cmaj = ScaleChoice { scale: Scale::Major, key: 0 };
        for n in 0..=127u8 {
            if in_scale(n, cmaj) {
                assert_eq!(snap_note(n, cmaj), n, "in-scale {n} moved");
            } else {
                let s = snap_note(n, cmaj);
                assert!(in_scale(s, cmaj), "{n} snapped to out-of-scale {s}");
                let d = (s as i16 - n as i16).abs();
                assert!(d <= 1, "{n} snapped {d} semitones to {s}");
            }
        }
        // C major has no C# / D# / F# / G# / A#.
        assert_eq!(snap_note(61, cmaj), 60, "C# snaps down to C");
        assert_eq!(snap_note(63, cmaj), 62, "D# snaps down to D");
        assert_eq!(snap_note(66, cmaj), 65, "F# snaps down to F");
    }

    #[test]
    fn snap_ties_go_down_and_the_key_offsets_the_scale() {
        // Between two in-scale notes both 1 away, the flat side wins.
        let pent = ScaleChoice { scale: Scale::MajorPentatonic, key: 0 };
        // Major pentatonic on C: C D E G A. F (65) sits between E and G.
        assert_eq!(snap_note(65, pent), 64, "tie goes down to E");
        // The key rotates the scale: A minor pentatonic starts on A (57).
        let amin = ScaleChoice { scale: Scale::MinorPentatonic, key: 9 };
        assert!(in_scale(57, amin), "A is in A minor pentatonic");
        assert!(!in_scale(58, amin), "A# is not");
        assert_eq!(snap_note(58, amin), 57, "A# snaps down to A");
        // The scale repeats below its key: C (0) is D minor's b7, so C#
        // snaps down to it and MIDI 0 stays.
        let dmin = ScaleChoice { scale: Scale::Minor, key: 2 };
        assert_eq!(snap_note(1, dmin), 0, "C# below the key snaps down to C");
        assert_eq!(snap_note(0, dmin), 0, "MIDI 0 is in D minor");
        // The top of the keyboard clamps: nothing snaps past 127.
        let g = ScaleChoice { scale: Scale::Major, key: 7 };
        assert_eq!(snap_note(127, g), 127, "G7 in G major stays");
        let eb = ScaleChoice { scale: Scale::Major, key: 3 };
        // 127 = G9, not in E♭ major (which has G… wait, G IS the 3rd of E♭).
        assert!(in_scale(127, eb), "G is in E♭ major");
    }

    #[test]
    fn scale_and_key_resolve_by_name() {
        assert_eq!(Scale::from_name("MAJOR"), Some(Scale::Major));
        assert_eq!(Scale::from_name("minor"), Some(Scale::Minor));
        assert_eq!(Scale::from_name("Dorian"), Some(Scale::Dorian));
        assert_eq!(Scale::from_name("pentatonic"), Some(Scale::MajorPentatonic));
        assert_eq!(Scale::from_name("aeolian"), Some(Scale::Minor));
        assert_eq!(Scale::from_name("nope"), None);
        assert_eq!(key_from_name("C"), Some(0));
        assert_eq!(key_from_name("f#"), Some(6));
        assert_eq!(key_from_name("11"), Some(11));
        assert_eq!(key_from_name("12"), None);
        assert_eq!(key_from_name("h"), None);
        let c = ScaleChoice { scale: Scale::Blues, key: 8 };
        assert_eq!(c.label(), "G# BLUES");
    }

    // -- chords ----------------------------------------------------------

    #[test]
    fn chord_shapes_are_the_named_intervals() {
        let (n, len) = chord_notes(60, ChordShape::Major);
        assert_eq!(&n[..len], &[60, 64, 67], "C major triad");
        let (n, len) = chord_notes(60, ChordShape::Minor);
        assert_eq!(&n[..len], &[60, 63, 67], "C minor triad");
        let (n, len) = chord_notes(60, ChordShape::Dominant7);
        assert_eq!(&n[..len], &[60, 64, 67, 70], "C7");
        let (n, len) = chord_notes(60, ChordShape::Major7);
        assert_eq!(&n[..len], &[60, 64, 67, 71], "Cmaj7");
        let (n, len) = chord_notes(60, ChordShape::Minor7);
        assert_eq!(&n[..len], &[60, 63, 67, 70], "Cm7");
        let (n, len) = chord_notes(60, ChordShape::Power);
        assert_eq!(&n[..len], &[60, 67], "C5");
        let (n, len) = chord_notes(60, ChordShape::Sus4);
        assert_eq!(&n[..len], &[60, 65, 67], "Csus4");
        let (n, len) = chord_notes(60, ChordShape::Sus2);
        assert_eq!(&n[..len], &[60, 62, 67], "Csus2");
        // The very top of the keyboard skips intervals that do not fit.
        let (n, len) = chord_notes(125, ChordShape::Major);
        assert_eq!(&n[..len], &[125], "no interval fits above G9");
        let (n, len) = chord_notes(121, ChordShape::Power);
        assert_eq!(&n[..len], &[121], "root + a fifth past 127: root only");
        let (n, len) = chord_notes(120, ChordShape::Power);
        assert_eq!(&n[..len], &[120, 127], "the fifth fits exactly at the top");
    }

    #[test]
    fn chord_shape_resolves_by_name() {
        assert_eq!(ChordShape::from_name("MIN7"), Some(ChordShape::Minor7));
        assert_eq!(ChordShape::from_name("power chord"), Some(ChordShape::Power));
        assert_eq!(ChordShape::from_name("dominant 7"), Some(ChordShape::Dominant7));
        assert_eq!(ChordShape::from_name("sus4"), Some(ChordShape::Sus4));
        assert_eq!(ChordShape::from_name("nope"), None);
    }

    // -- modes / rates ---------------------------------------------------

    #[test]
    fn modes_and_rates_resolve_by_name() {
        assert_eq!(ArpMode::from_name("UP"), Some(ArpMode::Up));
        assert_eq!(ArpMode::from_name("up-down"), Some(ArpMode::UpDown));
        assert_eq!(ArpMode::from_name("chords"), Some(ArpMode::Chord));
        assert_eq!(ArpMode::from_name("rnd"), Some(ArpMode::Random));
        assert_eq!(ArpMode::from_name("nope"), None);
        assert_eq!(ArpRate::from_name("1/16"), Some(ArpRate::Sixteenth));
        assert_eq!(ArpRate::from_name("16th"), Some(ArpRate::Sixteenth));
        assert_eq!(ArpRate::from_name("triplet"), Some(ArpRate::EighthT));
        assert_eq!(ArpRate::from_name("quarter"), Some(ArpRate::Quarter));
        assert_eq!(ArpRate::from_name("nope"), None);
        assert_eq!(ArpRate::Quarter.beats_per_step(), 1.0);
        assert_eq!(ArpRate::SixteenthT.beats_per_step(), 1.0 / 6.0);
    }

    // -- the pattern -----------------------------------------------------

    #[test]
    fn up_pattern_walks_held_notes_then_octaves() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.note_in(64, 0.9, NOW, &mut log.emit());
        a.note_in(67, 0.9, NOW, &mut log.emit());
        a.settings.octaves = 2;
        // One step at a time: [t0, t1) windows that each catch one onset.
        let step = 0.25;
        for i in 0..6 {
            a.tick(i as f64 * step, (i + 1) as f64 * step, &mut log.emit());
        }
        assert_eq!(log.ons(), vec![60, 64, 67, 72, 76, 79], "up: C E G then the octave C' E' G'");
    }

    #[test]
    fn down_pattern_reverses_and_updown_has_no_end_repeats() {
        let mut a = arp();
        let mut log = Log::new();
        for n in [60u8, 64, 67] {
            a.note_in(n, 0.9, NOW, &mut log.emit());
        }
        let mut seq = ([0u8; MAX_SEQ], 0);
        seq = a.sequence();
        assert_eq!(&seq.0[..seq.1], &[60, 64, 67], "up");
        a.settings.mode = ArpMode::Down;
        seq = a.sequence();
        assert_eq!(&seq.0[..seq.1], &[67, 64, 60], "down");
        a.settings.mode = ArpMode::UpDown;
        seq = a.sequence();
        assert_eq!(&seq.0[..seq.1], &[60, 64, 67, 64], "up-down: C E G E");
        // One held note: up-down is just that note.
        let mut one = arp();
        one.note_in(60, 0.9, NOW, &mut log.emit());
        one.settings.mode = ArpMode::UpDown;
        let s = one.sequence();
        assert_eq!(&s.0[..s.1], &[60]);
    }

    #[test]
    fn random_picks_pattern_notes_deterministically() {
        let mut a = arp();
        let mut log = Log::new();
        for n in [60u8, 64, 67] {
            a.note_in(n, 0.9, NOW, &mut log.emit());
        }
        a.settings.mode = ArpMode::Random;
        let s1 = a.sequence();
        let s2 = a.sequence();
        assert_eq!(s1.1, 1, "one note per step");
        assert_ne!(s1.0[0], s2.0[0], "the xorshift clock moves between steps");
        let all: Vec<u8> = (0..30).map(|_| a.sequence().0[0]).collect();
        assert!(
            all.iter().all(|n| [60, 64, 67].contains(n)),
            "random stays within the held pattern: {all:?}"
        );
    }

    #[test]
    fn gate_cuts_the_note_and_gate_one_retriggers() {
        // Gate 0.5 at 1/16: on at 0.0, off at 0.125, on at 0.25 …
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.settings.gate = 0.5;
        for i in 0..4 {
            a.tick(i as f64 * 0.25, (i + 1) as f64 * 0.25, &mut log.emit());
        }
        assert_eq!(
            log.events,
            vec![(60, true), (60, false), (60, true), (60, false), (60, true), (60, false), (60, true), (60, false)],
            "each step: on then off at half the step"
        );
        assert!(a.sounding().is_empty(), "nothing left down");

        // Gate 1.0: the off lands with the next on, still off-first.
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.settings.gate = 1.0;
        for i in 0..3 {
            a.tick(i as f64 * 0.25, (i + 1) as f64 * 0.25, &mut log.emit());
        }
        assert_eq!(log.events.first(), Some(&(60, true)));
        assert_eq!(log.events.iter().filter(|e| e.1).count(), 3);
        assert_eq!(log.events.iter().filter(|e| !e.1).count(), 2, "retrigger cuts the tail");
    }

    #[test]
    fn swing_delays_every_second_step() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.settings.swing = 0.66; // ~triplet feel: odd steps late by 1/3 step
        // Straight even steps land on the grid; a swung odd step starts at
        // 0.25 + 0.66 * 0.5 * 0.25 = 0.3325.
        a.tick(0.0, 0.26, &mut log.emit());
        assert_eq!(log.ons(), vec![60], "step 0 fires on the grid");
        a.tick(0.26, 0.32, &mut log.emit());
        assert_eq!(log.ons(), vec![60], "the swung step has not started yet");
        a.tick(0.32, 0.36, &mut log.emit());
        assert_eq!(log.ons(), vec![60, 60], "the swung step fires at 0.3325");
        a.tick(0.36, 0.51, &mut log.emit());
        assert_eq!(log.ons().len(), 3, "step 2 back on the grid at 0.5");
        // Gate tail: the swung step's off is also late (its own gate clock).
        assert!(log.events.contains(&(60, false)), "the gate cut the first step by now");
    }

    #[test]
    fn chord_mode_sounds_and_releases_the_shape_per_key() {
        let mut a = arp();
        a.settings.mode = ArpMode::Chord;
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        assert_eq!(log.ons(), vec![60, 64, 67], "one key, three voices (triad)");
        // Chord mode does not step.
        a.tick(0.0, 8.0, &mut log.emit());
        assert_eq!(log.ons(), vec![60, 64, 67], "no retrigger while held");
        a.note_in(65, 0.9, NOW, &mut log.emit());
        assert_eq!(log.ons(), vec![60, 64, 67, 65, 69, 72], "second key, second chord");
        // Releasing one key leaves the other's chord sounding.
        a.note_out(60, &mut log.emit());
        assert_eq!(log.down(), vec![65, 69, 72], "the first chord came off with its key");
        a.note_out(65, &mut log.emit());
        assert!(log.down().is_empty(), "everything released");
        // A 7th emits four voices.
        a.settings.chord = ChordShape::Dominant7;
        a.note_in(60, 0.9, NOW, &mut log.emit());
        assert_eq!(a.sounding(), vec![60, 64, 67, 70], "7th = 4 voices");
    }

    #[test]
    fn chord_mode_shapes_change_releases_and_resounds_held_keys() {
        let mut a = arp();
        a.settings.mode = ArpMode::Chord;
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        assert_eq!(log.down(), vec![60, 64, 67]);
        // Switching the shape under a held key: off the old, on the new.
        let mut emit = log.emit();
        assert!(a.set_param(P_ARP_CHORD, 0.25, &mut emit)); // round(0.25*7) = 2 → Sus2
        drop(emit);
        assert_eq!(a.sounding(), vec![60, 62, 67], "sus2 re-sounded under the held key");
        assert_eq!(log.events, vec![
            (60, true), (64, true), (67, true),
            (60, false), (64, false), (67, false),
            (60, true), (62, true), (67, true),
        ]);
    }

    #[test]
    fn release_all_clears_held_and_generated_notes() {
        let mut a = arp();
        a.settings.mode = ArpMode::Chord;
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.note_in(64, 0.9, NOW, &mut log.emit());
        let mut emit = log.emit();
        a.release_all(&mut emit);
        drop(emit);
        assert!(a.held_notes().next().is_none(), "held cleared");
        assert!(log.down().is_empty(), "chords silenced");
        // And it stays silent: no note is left to pattern over.
        let before = log.events.len();
        a.tick(0.0, 4.0, &mut log.emit());
        assert_eq!(log.events.len(), before, "nothing fires after release_all");
    }

    #[test]
    fn a_key_release_stops_the_burst_and_the_pattern_keeps_running() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.tick(0.0, 0.2, &mut log.emit());
        assert_eq!(log.ons(), vec![60]);
        // A second key joins, then the first leaves.
        a.note_in(64, 0.9, 0.2, &mut log.emit());
        a.note_out(60, &mut log.emit());
        assert!(log.down().is_empty(), "the burst went off with the release");
        // The tick continues over the remaining key.
        let n_before = log.ons().len();
        a.tick(0.2, 10.0, &mut log.emit());
        assert!(log.ons().len() > n_before, "the arp keeps firing over the held 64");
        assert!(log.ons()[n_before..].iter().all(|n| *n == 64), "only the held note sounds");
    }

    #[test]
    fn a_press_mid_bar_starts_the_pattern_on_its_first_note() {
        let mut a = arp();
        let mut log = Log::new();
        for n in [60, 64, 67] {
            a.note_in(n, 0.9, 3.4, &mut log.emit()); // grid step 14 of a bar: 14 % 3 = 2
        }
        a.tick(3.4, 5.0, &mut log.emit());
        assert_eq!(&log.ons()[..4], &[60, 64, 67, 60], "C E G C, not G C E G");
    }

    #[test]
    fn note_in_restarts_the_pattern_from_the_grid() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, 3.9, &mut log.emit()); // mid-step: aligns to 4.0
        a.tick(3.9, 3.95, &mut log.emit());
        assert!(log.ons().is_empty(), "before the grid point: silent");
        a.tick(3.95, 4.01, &mut log.emit());
        assert_eq!(log.ons(), vec![60], "fired at the next 1/16 grid point");
    }

    #[test]
    fn clock_jumps_retime_instead_of_machine_gunning() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, 0.0, &mut log.emit());
        // The clock runs on to beat ~12 (48 steps at 1/16), then the
        // transport rewinds to beat 0.
        a.tick(11.9, 12.1, &mut log.emit());
        let fired = log.ons().len();
        assert!(fired >= 1);
        let mut next = Log::new();
        // The jump back: ONE step on the new downbeat, not 48 in a block.
        a.tick(0.0, 0.02, &mut next.emit());
        assert_eq!(next.ons(), vec![60], "rescheduled onto the downbeat");
        a.tick(0.02, 0.26, &mut next.emit());
        assert_eq!(next.ons(), vec![60, 60], "and the next step lands on the grid at 0.25");
        assert!(next.ons().len() < 5, "no machine-gun of missed steps");
    }

    #[test]
    fn set_param_quantizes_stepped_knobs_and_only_shape_changes_release() {
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        a.tick(0.0, 0.26, &mut log.emit());
        assert_eq!(a.sounding(), vec![60]);
        // A no-op value (same mode) must not release anything.
        let before = log.events.len();
        let mut emit = log.emit();
        assert!(a.set_param(P_ARP_MODE, 0.0, &mut emit), "mode knob applies");
        drop(emit);
        assert_eq!(log.events.len(), before, "same mode: nothing released");
        // A real mode change releases the sounding burst and (chord mode)
        // re-sounds the held keys.
        let mut emit = log.emit();
        assert!(a.set_param(P_ARP_MODE, 0.75, &mut emit), "mode → CHORD (index 3)");
        drop(emit);
        assert_eq!(a.sounding(), vec![60, 64, 67], "chord re-sounded: {:?}", a.sounding());
        assert!(log.events.contains(&(60, false)), "the burst was released first");
        // Octave knob quantizes to whole octaves.
        let mut emit = log.emit();
        assert!(a.set_param(P_ARP_OCTAVE, 0.4, &mut emit));
        drop(emit);
        assert_eq!(a.settings.octaves, 2, "round(0.4 * 3) = 1 → 2 octaves");
        assert!(a.set_param(P_ARP_OCTAVE, 0.0, &mut log.emit()));
        assert_eq!(a.settings.octaves, 1);
        // Unknown param: not ours.
        let mut emit = log.emit();
        assert!(!a.set_param(P_LEVEL_NOT_OURS, 0.5, &mut emit));
        drop(emit);
        // Rate change under held keys reschedules without losing the hold.
        let mut a = arp();
        let mut log = Log::new();
        a.note_in(60, 0.9, NOW, &mut log.emit());
        let mut emit = log.emit();
        assert!(a.set_param(P_ARP_RATE, 0.0, &mut emit), "rate → 1/4");
        drop(emit);
        assert!(log.down().is_empty(), "nothing sounding after the change");
        a.tick(0.0, 0.9, &mut log.emit());
        assert_eq!(log.ons(), vec![60], "fired on the new slower clock");
        a.tick(0.9, 1.1, &mut log.emit());
        assert_eq!(log.ons().len(), 2, "one step per beat at 1/4");
    }

    const P_LEVEL_NOT_OURS: u32 = 0;

    #[test]
    fn no_held_notes_means_no_output() {
        let mut a = arp();
        let mut log = Log::new();
        a.tick(0.0, 100.0, &mut log.emit());
        assert!(log.events.is_empty());
        // A release for a note that never arrived is ignored.
        a.note_out(60, &mut log.emit());
        assert!(log.events.is_empty());
        // Notes out of range are ignored.
        a.note_in(128, 0.9, NOW, &mut log.emit());
        a.note_out(128, &mut log.emit());
        assert!(log.events.is_empty());
    }
}
