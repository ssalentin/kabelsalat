//! Demo phrases (#200): a short note sequence per kind of sound, played live
//! through one rig's instruments on the engine's clock and looped until
//! stopped, so a rack can be heard without a keyboard. Never recorded audio.
//!
//! The phrases are data (one table, one phrase per kind); `DemoPlayer` steps
//! one of them through a block's beat window. Neither knows the engine: the
//! engine decides where an event goes (`GraphEngine::demo_event`).

use crate::keyboard::KeyboardType;
use serde::{Deserialize, Serialize};

/// One note of a phrase, in beats from the phrase's start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    pub beat: f32,
    pub len: f32,
    pub note: u8,
    pub vel: u8,
}

const fn n(beat: f32, len: f32, note: u8, vel: u8) -> Note {
    Note { beat, len, note, vel }
}

/// What kind of sound a phrase is written for.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhraseKind {
    Keys,
    Bass,
    Lead,
    Pad,
    Organ,
    Drums,
    Wind,
}

impl PhraseKind {
    pub const ALL: [PhraseKind; 7] = [
        PhraseKind::Keys,
        PhraseKind::Bass,
        PhraseKind::Lead,
        PhraseKind::Pad,
        PhraseKind::Organ,
        PhraseKind::Drums,
        PhraseKind::Wind,
    ];

    /// The name tools and the wire format use.
    pub const fn name(self) -> &'static str {
        match self {
            PhraseKind::Keys => "keys",
            PhraseKind::Bass => "bass",
            PhraseKind::Lead => "lead",
            PhraseKind::Pad => "pad",
            PhraseKind::Organ => "organ",
            PhraseKind::Drums => "drums",
            PhraseKind::Wind => "wind",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        Self::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(s))
    }
}

pub struct Phrase {
    pub kind: PhraseKind,
    pub title: &'static str,
    /// Loop length: 16 or 32 beats (4 or 8 bars of 4/4).
    pub beats: f32,
    /// Breath follows the notes (CC 2 and channel pressure): a wind phrase.
    pub breath: bool,
    /// Sorted by `beat`.
    pub notes: &'static [Note],
}

impl Phrase {
    pub fn seconds(&self, bpm: f32) -> f32 {
        self.beats * 60.0 / bpm.max(1.0)
    }
}

pub fn phrase(kind: PhraseKind) -> &'static Phrase {
    &PHRASES[kind as usize]
}

/// C Am F G: left-hand root and fifth, right-hand chords on 1 and 3, and a
/// melody on top.
const KEYS: &[Note] = &[
    n(0.0, 1.9, 48, 84), n(0.0, 1.5, 60, 62), n(0.0, 1.5, 64, 62), n(0.0, 1.5, 67, 62), n(0.0, 1.0, 76, 96),
    n(1.0, 0.5, 79, 88), n(1.5, 0.5, 76, 84),
    n(2.0, 1.9, 55, 76), n(2.0, 1.5, 60, 58), n(2.0, 1.5, 64, 58), n(2.0, 1.5, 67, 58), n(2.0, 1.0, 74, 90),
    n(3.0, 1.0, 72, 86),
    n(4.0, 1.9, 45, 84), n(4.0, 1.5, 60, 62), n(4.0, 1.5, 64, 62), n(4.0, 1.5, 69, 62), n(4.0, 1.0, 72, 92),
    n(5.0, 1.0, 76, 90),
    n(6.0, 1.9, 52, 76), n(6.0, 1.5, 60, 58), n(6.0, 1.5, 64, 58), n(6.0, 1.5, 69, 58), n(6.0, 1.5, 81, 98),
    n(7.5, 0.5, 79, 84),
    n(8.0, 1.9, 41, 84), n(8.0, 1.5, 60, 62), n(8.0, 1.5, 65, 62), n(8.0, 1.5, 69, 62), n(8.0, 1.0, 77, 94),
    n(9.0, 0.5, 81, 90), n(9.5, 0.5, 79, 84),
    n(10.0, 1.9, 48, 76), n(10.0, 1.5, 60, 58), n(10.0, 1.5, 65, 58), n(10.0, 1.5, 69, 58), n(10.0, 1.0, 77, 88),
    n(11.0, 1.0, 76, 86),
    n(12.0, 1.9, 43, 84), n(12.0, 1.5, 59, 62), n(12.0, 1.5, 62, 62), n(12.0, 1.5, 67, 62), n(12.0, 1.5, 74, 94),
    n(13.5, 0.5, 76, 84),
    n(14.0, 1.9, 50, 76), n(14.0, 1.5, 59, 58), n(14.0, 1.5, 62, 58), n(14.0, 1.5, 67, 58), n(14.0, 1.0, 74, 88),
    n(15.0, 0.9, 71, 82),
];

/// E minor eighth-note groove, E1 to E3: Em Em C D.
const BASS: &[Note] = &[
    n(0.0, 0.45, 40, 112), n(0.5, 0.45, 40, 92), n(1.0, 0.45, 47, 100), n(1.5, 0.45, 50, 96),
    n(2.0, 0.95, 52, 108), n(3.0, 0.45, 50, 96), n(3.5, 0.45, 47, 92),
    n(4.0, 0.45, 40, 112), n(4.5, 0.45, 40, 92), n(5.0, 0.45, 43, 100), n(5.5, 0.45, 45, 96),
    n(6.0, 0.95, 47, 108), n(7.0, 0.45, 45, 96), n(7.5, 0.45, 43, 92),
    n(8.0, 0.45, 36, 112), n(8.5, 0.45, 36, 92), n(9.0, 0.45, 43, 100), n(9.5, 0.45, 48, 96),
    n(10.0, 0.95, 47, 108), n(11.0, 0.45, 43, 96), n(11.5, 0.45, 36, 92),
    n(12.0, 0.45, 38, 112), n(12.5, 0.45, 38, 92), n(13.0, 0.45, 45, 100), n(13.5, 0.45, 50, 96),
    n(14.0, 0.45, 45, 104), n(14.5, 0.45, 42, 96), n(15.0, 0.45, 38, 100), n(15.5, 0.45, 35, 92),
];

/// A minor lead line, A4 to A5, with long notes to hear the sound sustain.
const LEAD: &[Note] = &[
    n(0.0, 1.5, 69, 100), n(1.5, 0.5, 72, 92), n(2.0, 1.0, 76, 104), n(3.0, 0.5, 74, 92), n(3.5, 0.5, 72, 90),
    n(4.0, 1.5, 74, 100), n(5.5, 0.5, 76, 92), n(6.0, 2.0, 79, 108),
    n(8.0, 1.0, 81, 110), n(9.0, 0.5, 79, 94), n(9.5, 0.5, 76, 92), n(10.0, 1.0, 77, 100),
    n(11.0, 0.5, 76, 92), n(11.5, 0.5, 74, 90),
    n(12.0, 3.0, 76, 104), n(15.0, 0.5, 72, 88), n(15.5, 0.5, 74, 90),
];

/// Am F C G, two bars each, held.
const PAD: &[Note] = &[
    n(0.0, 7.9, 57, 80), n(0.0, 7.9, 60, 76), n(0.0, 7.9, 64, 76), n(0.0, 7.9, 69, 72),
    n(8.0, 7.9, 53, 80), n(8.0, 7.9, 60, 76), n(8.0, 7.9, 65, 76), n(8.0, 7.9, 69, 72),
    n(16.0, 7.9, 55, 80), n(16.0, 7.9, 60, 76), n(16.0, 7.9, 64, 76), n(16.0, 7.9, 72, 72),
    n(24.0, 7.9, 55, 80), n(24.0, 7.9, 59, 76), n(24.0, 7.9, 62, 76), n(24.0, 7.9, 67, 72),
];

/// C F G C: a pedal note and a held chord per bar under a moving line.
const ORGAN: &[Note] = &[
    n(0.0, 3.95, 36, 96), n(0.0, 3.95, 55, 88), n(0.0, 3.95, 60, 88), n(0.0, 3.95, 64, 88),
    n(0.0, 1.0, 67, 100), n(1.0, 0.5, 72, 96), n(1.5, 0.5, 71, 92), n(2.0, 1.0, 72, 100), n(3.0, 1.0, 76, 100),
    n(4.0, 3.95, 41, 96), n(4.0, 3.95, 57, 88), n(4.0, 3.95, 60, 88), n(4.0, 3.95, 65, 88),
    n(4.0, 1.5, 77, 104), n(5.5, 0.5, 76, 92), n(6.0, 0.5, 74, 92), n(6.5, 0.5, 72, 92), n(7.0, 1.0, 69, 96),
    n(8.0, 3.95, 43, 96), n(8.0, 3.95, 55, 88), n(8.0, 3.95, 59, 88), n(8.0, 3.95, 62, 88),
    n(8.0, 1.0, 71, 100), n(9.0, 1.0, 74, 100), n(10.0, 1.5, 79, 108), n(11.5, 0.5, 77, 92),
    n(12.0, 3.95, 36, 96), n(12.0, 3.95, 55, 88), n(12.0, 3.95, 60, 88), n(12.0, 3.95, 64, 88),
    n(12.0, 2.0, 76, 104), n(14.0, 0.5, 74, 92), n(14.5, 1.5, 72, 100),
];

/// General MIDI drums.
const KICK: u8 = 36;
const SNARE: u8 = 38;
const HAT: u8 = 42;
const OPEN_HAT: u8 = 46;
const CRASH: u8 = 49;
const TOM_HI: u8 = 50;
const TOM_MID: u8 = 47;
const TOM_LO: u8 = 45;

/// A rock beat on closed hats, an open hat at the end of bar 2 and a tom
/// fill into the crash that starts the loop.
const DRUMS: &[Note] = &[
    n(0.0, 0.25, KICK, 116), n(0.0, 0.25, HAT, 90), n(0.0, 1.0, CRASH, 104), n(0.5, 0.25, HAT, 70),
    n(1.0, 0.25, SNARE, 108), n(1.0, 0.25, HAT, 84), n(1.5, 0.25, HAT, 70),
    n(2.0, 0.25, HAT, 84), n(2.5, 0.25, KICK, 100), n(2.5, 0.25, HAT, 70),
    n(3.0, 0.25, SNARE, 108), n(3.0, 0.25, HAT, 84), n(3.5, 0.25, HAT, 70),
    n(4.0, 0.25, KICK, 116), n(4.0, 0.25, HAT, 90), n(4.5, 0.25, HAT, 70),
    n(5.0, 0.25, SNARE, 108), n(5.0, 0.25, HAT, 84), n(5.5, 0.25, HAT, 70),
    n(6.0, 0.25, KICK, 96), n(6.0, 0.25, HAT, 84), n(6.5, 0.25, KICK, 100), n(6.5, 0.25, HAT, 70),
    n(7.0, 0.25, SNARE, 108), n(7.0, 0.25, HAT, 84), n(7.5, 0.4, OPEN_HAT, 86),
    n(8.0, 0.25, KICK, 116), n(8.0, 0.25, HAT, 90), n(8.5, 0.25, HAT, 70),
    n(9.0, 0.25, SNARE, 108), n(9.0, 0.25, HAT, 84), n(9.5, 0.25, HAT, 70),
    n(10.0, 0.25, HAT, 84), n(10.5, 0.25, KICK, 100), n(10.5, 0.25, HAT, 70),
    n(11.0, 0.25, SNARE, 108), n(11.0, 0.25, HAT, 84), n(11.5, 0.25, HAT, 70),
    n(12.0, 0.25, KICK, 116), n(12.0, 0.25, HAT, 90), n(12.5, 0.25, HAT, 70),
    n(13.0, 0.25, SNARE, 108), n(13.0, 0.25, HAT, 84), n(13.5, 0.25, HAT, 70),
    n(14.0, 0.25, TOM_HI, 100), n(14.5, 0.25, TOM_HI, 96), n(15.0, 0.25, TOM_MID, 100),
    n(15.25, 0.25, TOM_MID, 96), n(15.5, 0.25, TOM_LO, 104), n(15.75, 0.2, TOM_LO, 100),
];

/// Two legato phrases, A4 to F5, each ending in a rest to breathe.
const WIND: &[Note] = &[
    n(0.0, 1.0, 69, 92), n(1.0, 1.0, 72, 100), n(2.0, 1.5, 74, 110), n(3.5, 0.5, 72, 96),
    n(4.0, 1.0, 69, 96), n(5.0, 1.0, 67, 92), n(6.0, 1.5, 69, 100),
    n(8.0, 1.0, 74, 100), n(9.0, 1.0, 76, 106), n(10.0, 1.0, 77, 116), n(11.0, 0.5, 76, 100),
    n(11.5, 0.5, 74, 96), n(12.0, 1.0, 72, 100), n(13.0, 2.0, 74, 108),
];

static PHRASES: [Phrase; 7] = [
    Phrase { kind: PhraseKind::Keys, title: "C Am F G with a melody", beats: 16.0, breath: false, notes: KEYS },
    Phrase { kind: PhraseKind::Bass, title: "E minor groove", beats: 16.0, breath: false, notes: BASS },
    Phrase { kind: PhraseKind::Lead, title: "A minor lead line", beats: 16.0, breath: false, notes: LEAD },
    Phrase { kind: PhraseKind::Pad, title: "Am F C G pad", beats: 32.0, breath: false, notes: PAD },
    Phrase { kind: PhraseKind::Organ, title: "C F G C chords under a moving line", beats: 16.0, breath: false, notes: ORGAN },
    Phrase { kind: PhraseKind::Drums, title: "Rock beat with a tom fill", beats: 16.0, breath: false, notes: DRUMS },
    Phrase { kind: PhraseKind::Wind, title: "Two legato phrases with breaths", beats: 16.0, breath: true, notes: WIND },
];

/// Words that tell a synth's sounds apart. What kind of instrument a rig
/// is (bass, wind, drums, organ, piano) is the rig's type (#198,
/// `keyboard::classify_instrument`); these only split a Synth rig.
/// A cue matches a word it starts.
const SYNTH_CUES: &[(PhraseKind, &[&str])] = &[
    (PhraseKind::Pad, &["pad"]),
    (PhraseKind::Lead, &["lead"]),
];

/// The phrase a rig gets: its type decides, and a Synth rig plays a pad or
/// lead phrase when its loaded patch (then its plugin's name) says so, else
/// `Keys`.
pub fn kind_for(ty: KeyboardType, patch_label: Option<&str>, plugin_name: Option<&str>) -> PhraseKind {
    match ty {
        KeyboardType::Piano | KeyboardType::ElectricPiano => PhraseKind::Keys,
        KeyboardType::JazzOrgan | KeyboardType::PipeOrgan => PhraseKind::Organ,
        KeyboardType::Bass => PhraseKind::Bass,
        KeyboardType::Wind => PhraseKind::Wind,
        KeyboardType::Drums => PhraseKind::Drums,
        KeyboardType::Synth => [patch_label, plugin_name].into_iter().flatten().find_map(kind_in).unwrap_or(PhraseKind::Keys),
    }
}

fn kind_in(text: &str) -> Option<PhraseKind> {
    let words = words(text);
    let has = |cue: &str| words.iter().any(|w| w.starts_with(cue));
    SYNTH_CUES.iter().find(|(_, cues)| cues.iter().any(|c| has(c))).map(|(k, _)| *k)
}

/// Lower-case words split at anything but letters and digits, plus the
/// parts of camel-case words ("SubBass" is also "sub" and "bass").
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        out.push(word.to_lowercase());
        let mut part = String::new();
        let mut prev_lower = false;
        for c in word.chars() {
            if c.is_uppercase() && prev_lower && !part.is_empty() {
                out.push(std::mem::take(&mut part).to_lowercase());
            }
            prev_lower = c.is_lowercase();
            part.push(c);
        }
        if part.len() < word.len() {
            out.push(part.to_lowercase());
        }
    }
    out
}

/// Notes the player can owe a note-off for at once; every phrase stays
/// below it (`every_phrase_fits_the_players_pending_offs`).
pub const MAX_PENDING: usize = 16;

/// How long breath takes to rise to a note's velocity.
const BREATH_RISE_S: f64 = 0.040;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemoEvent {
    On { note: u8, vel: u8 },
    Off { note: u8 },
    /// Breath (CC 2 and channel pressure), 0..=127.
    Breath(u8),
}

/// One phrase looping through one rig. Fixed size, allocation-free: it runs
/// on the audio thread.
#[derive(Clone, Copy)]
pub struct DemoPlayer {
    pub rig: u8,
    phrase: &'static Phrase,
    /// Beats since the start.
    pos: f64,
    /// Where the current pass of the loop started, in beats since the start.
    pass: f64,
    /// The next note of the current pass to start.
    next: usize,
    /// Sounding notes and the beat (since the start) each one ends.
    pending: [(u8, f64); MAX_PENDING],
    n_pending: usize,
    /// Velocity of the last note started: where breath goes while it sounds.
    last_vel: u8,
    breath_now: f64,
    breath_sent: u8,
}

impl DemoPlayer {
    pub fn new(rig: u8, kind: PhraseKind) -> Self {
        Self {
            rig,
            phrase: phrase(kind),
            pos: 0.0,
            pass: 0.0,
            next: 0,
            pending: [(0, 0.0); MAX_PENDING],
            n_pending: 0,
            last_vel: 0,
            breath_now: 0.0,
            breath_sent: 0,
        }
    }

    pub fn kind(&self) -> PhraseKind {
        self.phrase.kind
    }

    /// Sounding `(note, off_beat)` pairs currently held by this player.
    pub fn sounding(&self) -> &[(u8, f64)] {
        &self.pending[..self.n_pending]
    }

    /// Play the window of `beats` (one block; `secs` is its length in
    /// seconds) that starts where the last one ended: the note-offs due in
    /// it, then its note-ons, then breath. The loop wraps seamlessly, and a
    /// note-off past the loop end still comes. A window plays at most one
    /// pass of the phrase, however long it is (rule 151).
    pub fn tick(&mut self, beats: f64, secs: f64, emit: &mut dyn FnMut(DemoEvent)) {
        let end = self.pos + beats.max(0.0);
        let mut i = 0;
        while i < self.n_pending {
            if self.pending[i].1 < end {
                emit(DemoEvent::Off { note: self.pending[i].0 });
                self.n_pending -= 1;
                self.pending[i] = self.pending[self.n_pending];
            } else {
                i += 1;
            }
        }
        let notes = self.phrase.notes;
        let len = self.phrase.beats as f64;
        let mut budget = notes.len();
        while budget > 0 {
            if self.next == notes.len() {
                if self.pass + len >= end {
                    break;
                }
                self.pass += len;
                self.next = 0;
            }
            let note = notes[self.next];
            let at = self.pass + note.beat as f64;
            if at >= end {
                break;
            }
            if self.n_pending < MAX_PENDING {
                emit(DemoEvent::On { note: note.note, vel: note.vel });
                self.pending[self.n_pending] = (note.note, at + note.len as f64);
                self.n_pending += 1;
                self.last_vel = note.vel;
            }
            self.next += 1;
            budget -= 1;
        }
        self.pos = end;
        if self.phrase.breath {
            self.breathe(secs, emit);
        }
    }

    /// Breath rises to the sounding note's velocity over `BREATH_RISE_S`,
    /// holds through legato notes, and falls at once when no note sounds.
    fn breathe(&mut self, secs: f64, emit: &mut dyn FnMut(DemoEvent)) {
        let to = if self.n_pending == 0 { 0.0 } else { self.last_vel as f64 };
        self.breath_now = if to <= self.breath_now { to } else { (self.breath_now + to * secs / BREATH_RISE_S).min(to) };
        let v = self.breath_now.round() as u8;
        if v != self.breath_sent {
            self.breath_sent = v;
            emit(DemoEvent::Breath(v));
        }
    }

    /// Note-off for every sounding note, and breath back to 0.
    pub fn stop(&mut self, emit: &mut dyn FnMut(DemoEvent)) {
        for &(note, _) in &self.pending[..self.n_pending] {
            emit(DemoEvent::Off { note });
        }
        self.n_pending = 0;
        if self.breath_sent != 0 {
            self.breath_sent = 0;
            self.breath_now = 0.0;
            emit(DemoEvent::Breath(0));
        }
    }

    /// Beats from now to the next note-on or note-off.
    #[cfg(test)]
    pub fn beats_to_next_event(&self) -> f64 {
        let notes = self.phrase.notes;
        let next_on = match notes.get(self.next) {
            Some(n) => self.pass + n.beat as f64,
            None => self.pass + self.phrase.beats as f64 + notes.first().map_or(0.0, |n| n.beat as f64),
        };
        let next_off = self.pending[..self.n_pending].iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        (next_on.min(next_off) - self.pos).max(0.0)
    }

    /// Everything that decides what the player emits next, relative to the
    /// loop, so a search over its states stays finite.
    #[cfg(test)]
    pub fn fingerprint(&self) -> String {
        let q = |b: f64| (b * 1000.0).round() as i64;
        let mut pending: Vec<(u8, i64)> = self.pending[..self.n_pending].iter().map(|p| (p.0, q(p.1 - self.pos))).collect();
        pending.sort_unstable();
        format!("{:?}/{}/{}/{pending:?}/{}", self.phrase.kind, self.next, q(self.pos - self.pass), self.breath_sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every (beat, on, note) a phrase plays over `passes` loops, ticked in
    /// blocks of `block` beats.
    fn play(kind: PhraseKind, passes: usize, block: f64) -> (Vec<(f64, bool, u8)>, usize) {
        let mut p = DemoPlayer::new(0, kind);
        let mut out = Vec::new();
        let mut most = 0;
        let total = phrase(kind).beats as f64 * passes as f64;
        let mut t = 0.0;
        while t < total {
            p.tick(block, 0.005, &mut |e| match e {
                DemoEvent::On { note, .. } => out.push((t, true, note)),
                DemoEvent::Off { note } => out.push((t, false, note)),
                DemoEvent::Breath(_) => {}
            });
            most = most.max(p.n_pending);
            t += block;
        }
        p.stop(&mut |e| if let DemoEvent::Off { note } = e { out.push((t, false, note)) });
        (out, most)
    }

    #[test]
    fn every_phrase_is_well_formed() {
        for kind in PhraseKind::ALL {
            let p = phrase(kind);
            assert_eq!(p.kind, kind, "the table is indexed by kind");
            assert!(p.beats == 16.0 || p.beats == 32.0, "{kind:?}: {} beats is not 4 or 8 bars", p.beats);
            assert!(!p.notes.is_empty());
            for w in p.notes.windows(2) {
                assert!(w[0].beat <= w[1].beat, "{kind:?}: notes out of order at beat {}", w[1].beat);
            }
            for note in p.notes {
                assert!(note.beat >= 0.0 && note.beat < p.beats, "{kind:?}: {note:?} outside the loop");
                assert!(note.len > 0.0, "{kind:?}: {note:?} has no length");
                assert!(note.note <= 127 && (1..=127).contains(&note.vel), "{kind:?}: {note:?}");
            }
        }
    }

    /// A pitch that starts again while it still sounds would end both notes
    /// with its first note-off, also across the loop seam.
    #[test]
    fn no_pitch_overlaps_itself_across_the_seam() {
        for kind in PhraseKind::ALL {
            let p = phrase(kind);
            let two: Vec<(f32, f32, u8)> = (0..2)
                .flat_map(|k| p.notes.iter().map(move |n| (n.beat + k as f32 * p.beats, n.beat + k as f32 * p.beats + n.len, n.note)))
                .collect();
            for (i, a) in two.iter().enumerate() {
                for b in &two[i + 1..] {
                    assert!(a.2 != b.2 || b.0 >= a.1 || a.0 >= b.1, "{kind:?}: pitch {} at {} overlaps itself at {}", a.2, a.0, b.0);
                }
            }
        }
    }

    #[test]
    fn every_phrase_sits_where_its_kind_lives() {
        let range = |kind| {
            let notes = phrase(kind).notes;
            (notes.iter().map(|n| n.note).min().unwrap(), notes.iter().map(|n| n.note).max().unwrap())
        };
        let (lo, hi) = range(PhraseKind::Bass);
        assert!(lo >= 28 && hi <= 52, "bass {lo}..{hi} is not E1..E3");
        let (lo, hi) = range(PhraseKind::Lead);
        assert!(lo >= 60 && hi <= 84, "lead {lo}..{hi} is not C4..C6");
        assert!(phrase(PhraseKind::Drums).notes.iter().all(|n| (35..=81).contains(&n.note)), "drums are General MIDI percussion");
        assert!(phrase(PhraseKind::Pad).notes.iter().all(|n| n.len >= 4.0), "a pad holds its chords");
        let wind = phrase(PhraseKind::Wind);
        assert!(wind.breath && PhraseKind::ALL.iter().filter(|k| phrase(**k).breath).count() == 1, "only the wind phrase breathes");
        for w in wind.notes.windows(2) {
            assert!(w[0].beat + w[0].len <= w[1].beat + 1e-6, "the wind line is monophonic");
        }
        let rests = wind.notes.windows(2).filter(|w| w[0].beat + w[0].len < w[1].beat - 0.1).count();
        assert!(rests >= 1, "the wind line rests to breathe");
    }

    #[test]
    fn every_phrase_fits_the_players_pending_offs() {
        for kind in PhraseKind::ALL {
            let (events, most) = play(kind, 2, 1.0 / 64.0);
            assert!(most < MAX_PENDING, "{kind:?} holds {most} notes");
            let ons = events.iter().filter(|e| e.1).count();
            let offs = events.iter().filter(|e| !e.1).count();
            assert_eq!(ons, 2 * phrase(kind).notes.len(), "{kind:?}: every note of two passes started");
            assert_eq!(ons, offs, "{kind:?}: every note ends");
        }
    }

    /// Onsets of pass k land at k * beats + beat, within one block, and the
    /// note-offs of the last notes of a pass still come after the seam.
    #[test]
    fn the_loop_is_seamless_and_onsets_land_within_a_block() {
        let block = 256.0 * 120.0 / (60.0 * 48000.0); // 256 frames at 120 BPM
        for kind in PhraseKind::ALL {
            let p = phrase(kind);
            let (events, _) = play(kind, 3, block);
            let ons: Vec<f64> = events.iter().filter(|e| e.1).map(|e| e.0).collect();
            // The last block may start the fourth pass's downbeat early.
            let want: Vec<f64> = (0..4).flat_map(|k| p.notes.iter().map(move |n| k as f64 * p.beats as f64 + n.beat as f64)).collect();
            assert!(ons.len() >= 3 * p.notes.len() && ons.len() < want.len(), "{kind:?}: {} onsets", ons.len());
            for (got, want) in ons.iter().zip(&want) {
                assert!(got <= want && want - got < block + 1e-9, "{kind:?}: onset due at {want} played at {got}");
            }
        }
    }

    #[test]
    fn a_long_window_plays_at_most_one_pass() {
        let mut p = DemoPlayer::new(0, PhraseKind::Drums);
        let mut ons = 0;
        p.tick(1000.0, 10.0, &mut |e| if matches!(e, DemoEvent::On { .. }) { ons += 1 });
        assert!(ons <= phrase(PhraseKind::Drums).notes.len(), "{ons} note-ons from one window");
    }

    #[test]
    fn stop_releases_every_sounding_note_and_the_breath() {
        let mut p = DemoPlayer::new(0, PhraseKind::Wind);
        let mut held = Vec::new();
        let mut breath = 0;
        for _ in 0..40 {
            p.tick(0.01, 0.005, &mut |e| match e {
                DemoEvent::On { note, .. } => held.push(note),
                DemoEvent::Off { note } => held.retain(|n| *n != note),
                DemoEvent::Breath(v) => breath = v,
            });
        }
        assert!(!held.is_empty() && breath > 0, "the wind line plays and breathes");
        p.stop(&mut |e| match e {
            DemoEvent::Off { note } => held.retain(|n| *n != note),
            DemoEvent::Breath(v) => breath = v,
            DemoEvent::On { .. } => panic!("stop starts nothing"),
        });
        assert!(held.is_empty() && breath == 0, "after stop: held {held:?}, breath {breath}");
    }

    /// Breath reaches the note's velocity about 40 ms after it starts,
    /// stays up from one legato note to the next, and drops to 0 in a rest.
    #[test]
    fn breath_rises_holds_through_legato_and_falls_in_a_rest() {
        let bpm = 120.0;
        let secs = 0.005;
        let beats = secs * bpm / 60.0;
        let mut p = DemoPlayer::new(0, PhraseKind::Wind);
        let mut t = 0.0;
        let mut trace: Vec<(f64, u8)> = Vec::new();
        let mut breath = 0u8;
        while t < 16.0 {
            p.tick(beats, secs, &mut |e| if let DemoEvent::Breath(v) = e { breath = v });
            t += beats;
            trace.push((t, breath));
        }
        let at = |beat: f64| trace.iter().find(|(t, _)| *t >= beat).unwrap().1;
        let first = WIND[0].vel;
        assert!(at(0.04 * bpm / 60.0 * 0.5) < first, "breath is still rising 20 ms in");
        assert_eq!(at(0.05 * bpm / 60.0), first, "breath reached the first note's velocity 50 ms in");
        assert!((1..16).all(|k| at(k as f64 * 0.1 + 0.2) > 0), "breath stays up through the legato notes of the first phrase");
        assert_eq!(at(7.7), 0, "breath is 0 in the rest before beat 8");
        assert!(at(8.2) > 0, "breath comes back with the second phrase");
    }

    /// Real sound and plugin names go through the rig's type (the same
    /// classification the keyboard and the rail use), then the phrase.
    #[test]
    fn real_sound_names_pick_their_phrase() {
        use crate::control::{ClapId, Lv2Uri, NodeKind};
        let clap = |id: &str| NodeKind::Clap { plugin_id: ClapId::new(id) };
        let surge = clap("org.surge-synth-team.surge-xt");
        let cases: &[(NodeKind, Option<&str>, Option<&str>, PhraseKind)] = &[
            (surge.clone(), Some("Basses/Sub Bass 1"), Some("Surge XT"), PhraseKind::Bass),
            (surge.clone(), Some("Pads/Warm Pad"), Some("Surge XT"), PhraseKind::Pad),
            (surge.clone(), Some("Leads/Saw Lead"), Some("Surge XT"), PhraseKind::Lead),
            (surge.clone(), Some("Keys/EP Suitcase"), Some("Surge XT"), PhraseKind::Keys),
            (surge.clone(), Some("Plucks/Pluck 1"), Some("Surge XT"), PhraseKind::Keys),
            (surge.clone(), Some("Winds/Flute"), Some("Surge XT"), PhraseKind::Wind),
            (surge.clone(), Some("Winds/Bassoon"), Some("Surge XT"), PhraseKind::Wind),
            (surge.clone(), Some("Contrabassoon"), Some("Surge XT"), PhraseKind::Wind),
            (surge.clone(), Some("Keys/Organ 1"), Some("Surge XT"), PhraseKind::Organ),
            (surge.clone(), Some("Percussion/Metal Hit"), Some("Surge XT"), PhraseKind::Drums),
            (surge.clone(), Some("Epic Strings"), Some("Surge XT"), PhraseKind::Keys),
            (NodeKind::Lv2 { uri: Lv2Uri::new(crate::organ::B_SYNTH) }, Some("Jimmy Smith"), Some("setBfree DSP Tonewheel Organ"), PhraseKind::Organ),
            (clap(crate::aeolus::AEOLUS_ID), None, Some("Aeolus"), PhraseKind::Organ),
            (NodeKind::Lv2 { uri: Lv2Uri::new("http://gareus.org/oss/lv2/avldrums#BlackPearl") }, None, Some("AVL Drumkits"), PhraseKind::Drums),
            (clap("org.starbreath.starbreath"), None, Some("Starbreath"), PhraseKind::Wind),
            (clap("com.punklabs.onetrick.chonk"), None, Some("OneTrick CHONK"), PhraseKind::Bass),
            (clap("com.digital-suburban.dexed"), None, Some("Dexed"), PhraseKind::Keys),
            (surge.clone(), None, None, PhraseKind::Keys),
        ];
        for (kind, patch, plugin, want) in cases {
            let ty = crate::keyboard::classify_instrument(kind, *patch, *plugin, &[]);
            assert_eq!(kind_for(ty, *patch, *plugin), *want, "{kind:?} {patch:?} {plugin:?} ({ty:?})");
        }
    }

    /// A rig marked by hand plays its mark's phrase, whatever the patch says.
    #[test]
    fn a_marked_type_decides_the_phrase() {
        assert_eq!(kind_for(KeyboardType::Drums, Some("Pads/Warm Pad"), Some("Surge XT")), PhraseKind::Drums);
        assert_eq!(kind_for(KeyboardType::Wind, None, Some("Surge XT")), PhraseKind::Wind);
        assert_eq!(kind_for(KeyboardType::ElectricPiano, Some("Leads/Saw Lead"), None), PhraseKind::Keys);
        assert_eq!(kind_for(KeyboardType::Synth, Some("Leads/Saw Lead"), None), PhraseKind::Lead);
    }

    #[test]
    fn kinds_round_trip_through_their_names_and_numbers() {
        for kind in PhraseKind::ALL {
            assert_eq!(PhraseKind::parse(kind.name()), Some(kind));
            assert_eq!(PhraseKind::ALL[kind as usize], kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(kind.name()));
        }
        assert_eq!(PhraseKind::parse(" Bass "), Some(PhraseKind::Bass));
        assert_eq!(PhraseKind::parse("tuba"), None);
    }
}
