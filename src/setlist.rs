//! Setlist: songs and parts over the loaded rigs (#204).
//!
//! A setlist lives inside a [`crate::session::Session`]. Every rig of the gig
//! is loaded once; a song only says which rigs are played, with what key range,
//! transpose, level, and stored knob values. Switching parts sends
//! [`crate::control::Command::SetRigZones`] (plus tempo, time signature, rig
//! levels, and knob values) without changing routing — held notes keep
//! sounding until released and reverb/delay tails ring out.
//!
//! Songs can be grouped into sets, and every rig, send bus and controller
//! binding has a level (show, set or song) that decides what a switch
//! touches: see [`levels`] (#273).

mod levels;
pub use levels::{part_buses, source_str, Binding, Level, Levels, Set, SetId};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::control::{Graph, NodeId, RigZone};
use crate::session::ParamValue;
use crate::transport::TimeSig;

/// Maximum number of rigs the engine's zone table serves right now (#205 raises this).
pub const MAX_RIGS: usize = 8;

/// Which rig is played in a [`Part`], and how.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartRig {
    /// 0-based rig index in the session.
    pub rig: u8,
    /// Lowest played MIDI note (inclusive, 0..=127, before transpose).
    pub key_lo: u8,
    /// Highest played MIDI note (inclusive, 0..=127, before transpose).
    pub key_hi: u8,
    /// Semitone transpose applied after matrix/strip transforms (-48..=48).
    pub transpose: i8,
    /// Rig mixer level (0.0..=1.0) applied when this part is recalled.
    pub level: f32,
}

impl Default for PartRig {
    fn default() -> Self {
        Self {
            rig: 0,
            key_lo: 0,
            key_hi: 127,
            transpose: 0,
            level: 1.0,
        }
    }
}

impl PartRig {
    pub fn new(rig: u8) -> Self {
        Self { rig, ..Self::default() }
    }

    /// Convert to the engine's [`RigZone`] (always `live: true`, range ordered).
    pub fn to_zone(self) -> RigZone {
        let lo = self.key_lo.min(127);
        let hi = self.key_hi.min(127);
        RigZone {
            live: true,
            key_lo: lo.min(hi),
            key_hi: lo.max(hi),
            transpose: self.transpose.clamp(-48, 48),
        }
    }
}

/// One musical section of a [`Song`] (e.g. Verse, Chorus, Solo).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    /// Rigs that take new notes in this part. Rigs absent from this list are
    /// gated (`live: false`) while their held notes and effect tails ring out.
    #[serde(default)]
    pub rigs: Vec<PartRig>,
    /// Stored knob values for the rigs in this part, recalled on switch.
    /// Keyed by node id like `Session::params` — remapped with the session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamValue>,
    /// Aux buses (node ids) that take signal in this part when they are at
    /// song level (#273).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buses: Vec<NodeId>,
}

impl Part {
    pub fn new(name: impl Into<String>, rigs: Vec<PartRig>) -> Self {
        Self {
            name: name.into(),
            rigs,
            params: Vec::new(),
            buses: Vec::new(),
        }
    }

    /// Upsert a rig entry in this part (one entry per rig index).
    pub fn set_rig(&mut self, entry: PartRig) {
        if let Some(existing) = self.rigs.iter_mut().find(|r| r.rig == entry.rig) {
            *existing = entry;
        } else {
            self.rigs.push(entry);
            self.rigs.sort_by_key(|r| r.rig);
        }
    }

    /// Remove a rig from this part.
    pub fn remove_rig(&mut self, rig: u8) -> bool {
        let before = self.rigs.len();
        self.rigs.retain(|r| r.rig != rig);
        self.rigs.len() != before
    }

    /// Upsert a stored knob value for this part.
    pub fn set_param(&mut self, node: NodeId, param: u32, value: f64) {
        if let Some(p) = self.params.iter_mut().find(|p| p.node == node && p.param == param) {
            p.value = value;
        } else {
            self.params.push(ParamValue { node, param, value });
            self.params.sort_by_key(|p| (p.node, p.param));
        }
    }
}

/// One song in the [`Setlist`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub name: String,
    /// Optional song tempo in BPM (20..=300). `None` leaves the transport BPM alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpm: Option<f32>,
    /// Optional song time signature. `None` leaves the transport signature alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_sig: Option<TimeSig>,
    /// Ordered sections of the song (at least one).
    #[serde(default)]
    pub parts: Vec<Part>,
    /// The [`Set`] this song belongs to (#273); a set's songs are consecutive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<SetId>,
}

impl Song {
    pub fn new(name: impl Into<String>, parts: Vec<Part>) -> Self {
        Self {
            name: name.into(),
            bpm: None,
            time_sig: None,
            parts,
            set: None,
        }
    }
}

/// Active position in a [`Setlist`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetlistPos {
    pub song: usize,
    pub part: usize,
}

/// A learned hardware MIDI CC that steps the setlist (rising edge: val >= 64).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FootSwitch {
    pub cc: u8,
    /// `None` matches any channel; `Some(0..=15)` matches one MIDI channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
}

impl FootSwitch {
    pub fn matches(self, channel: u8, cc: u8) -> bool {
        self.cc == (cc & 0x7F) && self.channel.is_none_or(|c| c == (channel & 0x0F))
    }
}

/// Which setlist step a foot-switch CC or MIDI learn targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FootSwitchAction {
    Next,
    Prev,
}

/// The session's setlist: ordered songs, active position, optional
/// foot-switch CC bindings, the sets that group songs, and the chosen
/// levels of rigs, buses and bindings (#273).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setlist {
    pub songs: Vec<Song>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<SetlistPos>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cc: Option<FootSwitch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_cc: Option<FootSwitch>,
    /// Sets in song order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<Set>,
    #[serde(skip_serializing_if = "Levels::is_empty")]
    pub levels: Levels,
}

impl Setlist {
    /// True when the setlist holds nothing at all (so older/plain sessions
    /// omit the `"setlist"` field in `session.json`).
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty() && self.next_cc.is_none() && self.prev_cc.is_none() && self.sets.is_empty() && self.levels.is_empty()
    }

    /// The currently active `(song, part)` if any.
    pub fn current(&self) -> Option<(&Song, &Part, SetlistPos)> {
        let pos = self.active?;
        let song = self.songs.get(pos.song)?;
        let part = song.parts.get(pos.part)?;
        Some((song, part, pos))
    }

    /// Select `(song, part)` if it exists.
    pub fn select(&mut self, song: usize, part: usize) -> Option<SetlistPos> {
        let s = self.songs.get(song)?;
        if part >= s.parts.len() {
            return None;
        }
        let pos = SetlistPos { song, part };
        self.active = Some(pos);
        Some(pos)
    }

    /// Step to the next part (crossing into the next song's first part).
    /// If nothing is active yet and songs exist, activates `(0, 0)`.
    /// Returns `Some(new_pos)` if the active position changed, `None` at the end.
    pub fn next_part(&mut self) -> Option<SetlistPos> {
        let Some(pos) = self.active else {
            return self.first_non_empty_pos().inspect(|&p| self.active = Some(p));
        };
        if let Some(song) = self.songs.get(pos.song) {
            if pos.part + 1 < song.parts.len() {
                let next = SetlistPos { song: pos.song, part: pos.part + 1 };
                self.active = Some(next);
                return Some(next);
            }
        }
        for si in (pos.song + 1)..self.songs.len() {
            if self.playable(si) {
                let next = SetlistPos { song: si, part: 0 };
                self.active = Some(next);
                return Some(next);
            }
        }
        None
    }

    /// A song stepping can land on: it has parts and its set is not skipped.
    fn playable(&self, song: usize) -> bool {
        self.songs.get(song).is_some_and(|s| !s.parts.is_empty()) && !self.song_skipped(song)
    }

    /// Step to the previous part (crossing into the previous song's last part).
    /// If nothing is active yet and songs exist, activates `(0, 0)`.
    /// Returns `Some(new_pos)` if the active position changed, `None` at the start.
    pub fn prev_part(&mut self) -> Option<SetlistPos> {
        let Some(pos) = self.active else {
            return self.first_non_empty_pos().inspect(|&p| self.active = Some(p));
        };
        if pos.part > 0 {
            let prev = SetlistPos { song: pos.song, part: pos.part - 1 };
            self.active = Some(prev);
            return Some(prev);
        }
        for si in (0..pos.song).rev() {
            let n = self.songs[si].parts.len();
            if self.playable(si) {
                let prev = SetlistPos { song: si, part: n - 1 };
                self.active = Some(prev);
                return Some(prev);
            }
        }
        None
    }

    /// Step to part 0 of the next song.
    pub fn next_song(&mut self) -> Option<SetlistPos> {
        let Some(pos) = self.active else {
            return self.first_non_empty_pos().inspect(|&p| self.active = Some(p));
        };
        for si in (pos.song + 1)..self.songs.len() {
            if self.playable(si) {
                let next = SetlistPos { song: si, part: 0 };
                self.active = Some(next);
                return Some(next);
            }
        }
        None
    }

    /// Step to part 0 of the previous song.
    pub fn prev_song(&mut self) -> Option<SetlistPos> {
        let Some(pos) = self.active else {
            return self.first_non_empty_pos().inspect(|&p| self.active = Some(p));
        };
        for si in (0..pos.song).rev() {
            if self.playable(si) {
                let prev = SetlistPos { song: si, part: 0 };
                self.active = Some(prev);
                return Some(prev);
            }
        }
        None
    }

    fn first_non_empty_pos(&self) -> Option<SetlistPos> {
        (0..self.songs.len()).find(|&si| self.playable(si)).map(|song| SetlistPos { song, part: 0 })
    }

    /// Clamp or clear `active` so it always points to a valid `(song, part)`.
    pub fn clamp_active(&mut self) {
        let Some(pos) = self.active else { return };
        if self.songs.is_empty() {
            self.active = None;
            return;
        }
        let song = pos.song.min(self.songs.len() - 1);
        let parts = self.songs[song].parts.len();
        if parts == 0 {
            self.active = self.first_non_empty_pos();
        } else {
            self.active = Some(SetlistPos { song, part: pos.part.min(parts - 1) });
        }
    }

    /// Move a song from `from` to `to`, keeping `active` pointing at the same song.
    pub fn move_song(&mut self, from: usize, to: usize) -> bool {
        if from >= self.songs.len() || to >= self.songs.len() || from == to {
            return false;
        }
        let song = self.songs.remove(from);
        self.songs.insert(to, song);
        if let Some(pos) = &mut self.active {
            if pos.song == from {
                pos.song = to;
            } else if from < to && pos.song > from && pos.song <= to {
                pos.song -= 1;
            } else if to < from && pos.song >= to && pos.song < from {
                pos.song += 1;
            }
        }
        true
    }

    /// Remove song `index`, updating `active` cleanly.
    pub fn remove_song(&mut self, index: usize) -> Option<Song> {
        if index >= self.songs.len() {
            return None;
        }
        let removed = self.songs.remove(index);
        if let Some(pos) = self.active {
            if self.songs.is_empty() {
                self.active = None;
            } else if pos.song == index {
                let song = index.min(self.songs.len() - 1);
                let part = 0;
                self.active = if self.songs[song].parts.is_empty() {
                    self.first_non_empty_pos()
                } else {
                    Some(SetlistPos { song, part })
                };
            } else if pos.song > index {
                self.active = Some(SetlistPos { song: pos.song - 1, part: pos.part });
            }
        }
        Some(removed)
    }

    /// Remap every rig index and node id (rule 44: rig add/delete, canonicalization).
    pub fn remap(
        &self,
        rig_map: impl Fn(u8) -> Option<u8>,
        node_map: impl Fn(NodeId) -> Option<NodeId>,
    ) -> Self {
        let mut out = self.clone();
        for song in &mut out.songs {
            for part in &mut song.parts {
                let mut rigs = Vec::with_capacity(part.rigs.len());
                for pr in &part.rigs {
                    if let Some(new_rig) = rig_map(pr.rig) {
                        if !rigs.iter().any(|r: &PartRig| r.rig == new_rig) {
                            rigs.push(PartRig { rig: new_rig, ..*pr });
                        }
                    }
                }
                rigs.sort_by_key(|r| r.rig);
                part.rigs = rigs;

                let mut params = Vec::with_capacity(part.params.len());
                for p in &part.params {
                    if let Some(new_node) = node_map(p.node) {
                        params.push(ParamValue { node: new_node, ..*p });
                    }
                }
                params.sort_by_key(|p| (p.node, p.param));
                params.dedup_by_key(|p| (p.node, p.param));
                part.params = params;
                part.buses = part.buses.iter().filter_map(|&b| node_map(b)).collect();
                part.buses.dedup();
            }
        }
        out.remap_levels(&rig_map, &node_map);
        out.clamp_active();
        out
    }

    /// Drop references to rigs or nodes no longer present in `graph`.
    pub fn prune_to_graph(&mut self, graph: &Graph) {
        let has_rig = |r: u8| graph.nodes.iter().any(|n| n.rig == r);
        for song in &mut self.songs {
            for part in &mut song.parts {
                part.rigs.retain(|pr| has_rig(pr.rig));
                part.params.retain(|p| graph.node(p.node).is_some());
                part.buses.retain(|&b| graph.is_bus(b));
            }
        }
        self.prune_levels(graph);
        self.clamp_active();
    }

    /// Normalize hand-edited values on load (time signatures, key bounds, etc.).
    pub fn normalize(&mut self) {
        for song in &mut self.songs {
            if let Some(bpm) = song.bpm {
                song.bpm = Some(bpm.clamp(20.0, 300.0));
            }
            if let Some(sig) = song.time_sig {
                song.time_sig = Some(TimeSig::normalized(sig.num, sig.den));
            }
            for part in &mut song.parts {
                for pr in &mut part.rigs {
                    let lo = pr.key_lo.min(127);
                    let hi = pr.key_hi.min(127);
                    pr.key_lo = lo.min(hi);
                    pr.key_hi = lo.max(hi);
                    pr.transpose = pr.transpose.clamp(-48, 48);
                    if !pr.level.is_finite() {
                        pr.level = 1.0;
                    } else {
                        pr.level = pr.level.clamp(0.0, 1.0);
                    }
                }
                part.rigs.sort_by_key(|r| r.rig);
                part.rigs.dedup_by_key(|r| r.rig);
            }
        }
        if let Some(fs) = &mut self.next_cc {
            fs.cc &= 0x7F;
            fs.channel = fs.channel.map(|c| c & 0x0F);
        }
        if let Some(fs) = &mut self.prev_cc {
            fs.cc &= 0x7F;
            fs.channel = fs.channel.map(|c| c & 0x0F);
        }
        self.normalize_levels();
        self.clamp_active();
    }

    /// Plain-language integrity check run by `Session::validate`.
    pub fn validate(&self, graph: &Graph) -> Result<()> {
        for (si, song) in self.songs.iter().enumerate() {
            if song.name.trim().is_empty() {
                bail!("Setlist song {} needs a name.", si + 1);
            }
            if song.parts.is_empty() {
                bail!("Setlist song '{}' needs at least one part.", song.name);
            }
            if let Some(bpm) = song.bpm {
                if !bpm.is_finite() || !(20.0..=300.0).contains(&bpm) {
                    bail!("Setlist song '{}' has a tempo outside 20..300 BPM.", song.name);
                }
            }
            for (pi, part) in song.parts.iter().enumerate() {
                if part.name.trim().is_empty() {
                    bail!("Setlist song '{}' part {} needs a name.", song.name, pi + 1);
                }
                for pr in &part.rigs {
                    if pr.rig as usize >= MAX_RIGS {
                        bail!(
                            "Setlist song '{}' part '{}' references rig {} (at most {} rigs).",
                            song.name,
                            part.name,
                            pr.rig + 1,
                            MAX_RIGS
                        );
                    }
                    if !pr.level.is_finite() || !(0.0..=1.0).contains(&pr.level) {
                        bail!(
                            "Setlist song '{}' part '{}' has a rig level outside 0..1.",
                            song.name,
                            part.name
                        );
                    }
                }
                for p in &part.params {
                    if graph.node(p.node).is_none() {
                        bail!(
                            "Setlist song '{}' part '{}' stores a knob for node {} which is no longer in the patch.",
                            song.name,
                            part.name,
                            p.node
                        );
                    }
                    if !p.value.is_finite() {
                        bail!(
                            "Setlist song '{}' part '{}' has a non-finite knob value.",
                            song.name,
                            part.name
                        );
                    }
                }
            }
        }
        if let Some(pos) = self.active {
            match self.songs.get(pos.song) {
                Some(s) if pos.part < s.parts.len() => {}
                _ => bail!("Setlist active position ({}, {}) is out of range.", pos.song, pos.part),
            }
        }
        for song in &self.songs {
            if song.set.is_some_and(|id| self.set(id).is_none()) {
                bail!("Setlist song '{}' is in a set that does not exist.", song.name);
            }
        }
        let levels = self.levels.rigs.iter().map(|p| p.level)
            .chain(self.levels.buses.iter().map(|p| p.level))
            .chain(self.levels.bindings.iter().map(|p| p.level));
        for level in levels {
            if let Level::Set(id) = level {
                if self.set(id).is_none() {
                    bail!("A rig, bus or binding is at the level of a set that does not exist.");
                }
            }
        }
        Ok(())
    }
}

/// Format a MIDI note number (0..=127) as a short name like `"C3"`, `"F#4"`.
pub fn note_label(note: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let n = note.min(127);
    let oct = (n as i32 / 12) - 1;
    format!("{}{oct}", NAMES[(n % 12) as usize])
}

/// Parse a MIDI note number or name (`"60"`, `"C4"`, `"F#3"`, `"Bb2"`).
pub fn parse_note(s: &str) -> Option<u8> {
    let t = s.trim();
    if let Ok(n) = t.parse::<u8>() {
        return (n <= 127).then_some(n);
    }
    let bytes = t.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let base: i32 = match bytes[0].to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let (acc, rest) = match bytes[1] {
        b'#' => (1, &t[2..]),
        b'b' | b'B' => (-1, &t[2..]),
        _ => (0, &t[1..]),
    };
    let oct: i32 = rest.parse().ok()?;
    let midi = (oct + 1) * 12 + base + acc;
    (0..=127).contains(&midi).then_some(midi as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_setlist() -> Setlist {
        let mut s1 = Song::new(
            "Opener",
            vec![
                Part::new("Verse", vec![PartRig::new(0)]),
                Part::new(
                    "Chorus",
                    vec![
                        PartRig { rig: 0, key_lo: 0, key_hi: 59, transpose: 0, level: 0.9 },
                        PartRig { rig: 1, key_lo: 60, key_hi: 127, transpose: 12, level: 0.8 },
                    ],
                ),
            ],
        );
        s1.bpm = Some(118.0);
        s1.time_sig = Some(TimeSig::normalized(4, 4));
        let mut s2 = Song::new("Ballad", vec![Part::new("Main", vec![PartRig::new(1)])]);
        s2.bpm = Some(74.0);
        s2.time_sig = Some(TimeSig::normalized(3, 4));
        Setlist { songs: vec![s1, s2], ..Setlist::default() }
    }

    #[test]
    fn stepping_crosses_song_boundaries_and_stops_at_ends() {
        let mut sl = sample_setlist();
        assert_eq!(sl.active_zones(), [RigZone::OPEN; MAX_RIGS], "inactive setlist leaves every rig open");
        assert_eq!(sl.next_part(), Some(SetlistPos { song: 0, part: 0 }));
        assert_eq!(sl.next_part(), Some(SetlistPos { song: 0, part: 1 }));
        let z = sl.active_zones();
        assert!(z[0].live && z[0].key_lo == 0 && z[0].key_hi == 59);
        assert!(z[1].live && z[1].key_lo == 60 && z[1].key_hi == 127 && z[1].transpose == 12);
        assert_eq!(z[2], RigZone::OPEN, "rig 2, which no part names, is at show level (#273)");
        assert_eq!(sl.next_part(), Some(SetlistPos { song: 1, part: 0 }));
        assert_eq!(sl.next_part(), None, "stops at the end");
        assert_eq!(sl.prev_part(), Some(SetlistPos { song: 0, part: 1 }));
        assert_eq!(sl.prev_part(), Some(SetlistPos { song: 0, part: 0 }));
        assert_eq!(sl.prev_part(), None, "stops at the start");
        assert_eq!(sl.next_song(), Some(SetlistPos { song: 1, part: 0 }));
        assert_eq!(sl.prev_song(), Some(SetlistPos { song: 0, part: 0 }));
    }

    #[test]
    fn remap_follows_rig_deletion_and_node_renumbering() {
        let mut sl = sample_setlist();
        sl.songs[0].parts[1].set_param(10, 0, 0.42);
        sl.songs[0].parts[1].set_param(20, 1, 0.77);
        sl.active = Some(SetlistPos { song: 0, part: 1 });
        // Delete rig 0 (rig 1 shifts to rig 0), node 20 shifts to 5, node 10 gone.
        let remapped = sl.remap(
            |r| if r == 0 { None } else { Some(r - 1) },
            |n| if n == 20 { Some(5) } else { None },
        );
        let chorus = &remapped.songs[0].parts[1];
        assert_eq!(chorus.rigs.len(), 1);
        assert_eq!(chorus.rigs[0].rig, 0);
        assert_eq!(chorus.rigs[0].transpose, 12);
        assert_eq!(chorus.params, vec![ParamValue { node: 5, param: 1, value: 0.77 }]);
    }

    #[test]
    fn move_and_remove_song_keep_active_cursor_sane() {
        let mut sl = sample_setlist();
        sl.active = Some(SetlistPos { song: 0, part: 1 });
        assert!(sl.move_song(0, 1));
        assert_eq!(sl.active, Some(SetlistPos { song: 1, part: 1 }));
        assert!(sl.remove_song(1).is_some());
        assert_eq!(sl.active, Some(SetlistPos { song: 0, part: 0 }));
    }

    #[test]
    fn note_label_and_parse_round_trip() {
        assert_eq!(note_label(60), "C4");
        assert_eq!(parse_note("C4"), Some(60));
        assert_eq!(parse_note("F#3"), Some(54));
        assert_eq!(parse_note("Bb2"), Some(46));
        assert_eq!(parse_note("60"), Some(60));
        assert_eq!(parse_note("128"), None);
    }
}
