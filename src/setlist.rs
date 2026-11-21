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

mod controls;
mod levels;
pub use controls::msg_label;
pub use levels::{part_buses, source_str, Binding, Gates, Level, Levels, Set, SetId};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::control::{Graph, NodeId, RigZone};
use crate::layer::{VelCurve, FLOAT_MAX};
use crate::session::ParamValue;
use crate::show::{ShowAction, ShowControl, ShowControls, ShowMsg, ShowPart, NO_SHOW_CONTROLS};
pub use crate::show::MAX_PROGRAM_DEVICES;
use crate::transport::TimeSig;

pub use crate::control::MAX_RIGS;

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
    /// Struck velocity range (inclusive, 0..=127) that plays this rig: a
    /// velocity layer (#276).
    pub vel_lo: u8,
    pub vel_hi: u8,
    /// What the rig makes of the struck velocity.
    pub curve: VelCurve,
    /// Keys each edge of the key range follows the hands (0..=12, 0 = fixed).
    pub float: u8,
}

impl Default for PartRig {
    fn default() -> Self {
        Self {
            rig: 0,
            key_lo: 0,
            key_hi: 127,
            transpose: 0,
            level: 1.0,
            vel_lo: 0,
            vel_hi: 127,
            curve: VelCurve::Linear,
            float: 0,
        }
    }
}

impl PartRig {
    pub fn new(rig: u8) -> Self {
        Self { rig, ..Self::default() }
    }

    /// Convert to the engine's [`RigZone`] (always `live: true`, ranges ordered).
    pub fn to_zone(self) -> RigZone {
        let (lo, hi) = (self.key_lo.min(127), self.key_hi.min(127));
        let (vlo, vhi) = (self.vel_lo.min(127), self.vel_hi.min(127));
        RigZone {
            live: true,
            key_lo: lo.min(hi),
            key_hi: lo.max(hi),
            transpose: self.transpose.clamp(-48, 48),
            vel_lo: vlo.min(vhi),
            vel_hi: vlo.max(vhi),
            curve: self.curve,
            float: self.float.min(FLOAT_MAX),
        }
    }

    /// The velocity layer and floating split an agent tool names; what it
    /// leaves out stays as it is.
    pub fn set_layer(&mut self, vel_lo: Option<u64>, vel_hi: Option<u64>, curve: Option<&str>, float: Option<u64>) -> Result<(), String> {
        if let Some(c) = curve {
            self.curve = VelCurve::parse(c)?;
        }
        let (lo, hi) = (vel_lo.map_or(self.vel_lo, |v| v.min(127) as u8), vel_hi.map_or(self.vel_hi, |v| v.min(127) as u8));
        (self.vel_lo, self.vel_hi) = (lo.min(hi), lo.max(hi));
        if let Some(f) = float {
            self.float = f.min(FLOAT_MAX as u64) as u8;
        }
        Ok(())
    }

    /// [`PartRig::set_layer`] from a tool's part-rig entry (`vel_lo`,
    /// `vel_hi`, `vel_curve`, `float`). A value of the wrong type is refused,
    /// never defaulted (rule 77).
    pub fn set_layer_json(&mut self, r: &serde_json::Value) -> Result<(), String> {
        use serde_json::Value;
        let num = |k: &str| -> Result<Option<u64>, String> {
            match r.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v.as_u64()
                    .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0).map(|f| f as u64))
                    .map(Some)
                    .ok_or_else(|| format!("{k} must be a whole number, got {v}")),
            }
        };
        let curve = match r.get("vel_curve") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str().ok_or_else(|| format!("vel_curve must be 'linear', 'soft' or 'hard', got {v}"))?),
        };
        self.set_layer(num("vel_lo")?, num("vel_hi")?, curve, num("float")?)
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
    /// The backing-track file (absolute path to a WAV) the file player
    /// starts when the song is selected (#278).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<String>,
}

impl Song {
    pub fn new(name: impl Into<String>, parts: Vec<Part>) -> Self {
        Self {
            name: name.into(),
            bpm: None,
            time_sig: None,
            parts,
            set: None,
            backing: None,
        }
    }
}

/// Active position in a [`Setlist`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetlistPos {
    pub song: usize,
    pub part: usize,
}

/// A foot switch of a session saved before #277: a CC from any device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FootSwitch {
    pub cc: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
}

/// The session's setlist: ordered songs, active position, the show
/// controls, the sets that group songs, and the chosen levels of rigs,
/// buses and bindings (#273).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setlist {
    pub songs: Vec<Song>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<SetlistPos>,
    /// Every show action's learned control, then the devices whose program
    /// change N selects song N+1 (#208), in table order (`normalize`).
    /// They work in every song: show controls are at show level (#273).
    /// Devices are indices: a session names their ports (`hw_ports`, #227).
    #[serde(rename = "show_controls", skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<ShowControl>,
    /// The foot switches of a session saved before #277, read only:
    /// `normalize` moves them into `controls`.
    #[serde(skip_serializing)]
    pub(crate) next_cc: Option<FootSwitch>,
    #[serde(skip_serializing)]
    pub(crate) prev_cc: Option<FootSwitch>,
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
        self.songs.is_empty()
            && self.controls.is_empty()
            && self.next_cc.is_none()
            && self.prev_cc.is_none()
            && self.sets.is_empty()
            && self.levels.is_empty()
    }

    /// The currently active `(song, part)` if any.
    pub fn current(&self) -> Option<(&Song, &Part, SetlistPos)> {
        let pos = self.active?;
        let song = self.songs.get(pos.song)?;
        let part = song.parts.get(pos.part)?;
        Some((song, part, pos))
    }

    /// Every part in setlist order as the engine steps through it on a foot
    /// switch (#261): its song, whether stepping passes over that song, and
    /// its level-aware zones. Songs without parts add nothing.
    pub fn show_parts(&self) -> Vec<ShowPart> {
        self.songs
            .iter()
            .enumerate()
            .flat_map(|(i, song)| {
                let skip = self.song_skipped(i);
                song.parts.iter().map(move |part| ShowPart { song: i, skip, zones: self.zones_of(song, part) })
            })
            .collect()
    }

    /// The active part's index in `show_parts`.
    pub fn active_index(&self) -> Option<usize> {
        let (_, _, pos) = self.current()?;
        Some(self.songs[..pos.song].iter().map(|s| s.parts.len()).sum::<usize>() + pos.part)
    }

    /// The position of `show_parts()[index]`.
    pub fn pos_at(&self, index: usize) -> Option<SetlistPos> {
        let mut first = 0;
        for (song, s) in self.songs.iter().enumerate() {
            if index < first + s.parts.len() {
                return Some(SetlistPos { song, part: index - first });
            }
            first += s.parts.len();
        }
        None
    }

    /// The show controls as the engine's table. `controls` is in table
    /// order: the learned actions first, so a control naming one program
    /// goes before a device that takes every program.
    pub fn show_controls(&self) -> ShowControls {
        let mut t = NO_SHOW_CONTROLS;
        for (slot, c) in t.iter_mut().zip(&self.controls) {
            *slot = Some(*c);
        }
        t
    }

    /// The control learned for `action`.
    pub fn control_of(&self, action: ShowAction) -> Option<ShowControl> {
        self.controls.iter().copied().find(|c| c.action == action && c.action != ShowAction::SongOfProgram)
    }

    /// Learn `msg` from `device` for `action` (one of `ShowAction::LEARNED`),
    /// in place of its control. Another action that took the same message
    /// lets go of it, so one press never does two things. Returns that action.
    pub fn learn(&mut self, action: ShowAction, device: Option<u32>, msg: ShowMsg) -> Option<ShowAction> {
        let control = ShowControl { device, channel: None, msg, action };
        let overlaps = |c: &ShowControl| c.msg == msg && (c.device.is_none() || device.is_none() || c.device == device);
        let taken = self.controls.iter().find(|c| c.action != action && c.action != ShowAction::SongOfProgram && overlaps(c)).map(|c| c.action);
        self.controls.retain(|c| c.action == ShowAction::SongOfProgram || (c.action != action && !overlaps(c)));
        self.controls.push(control);
        self.sort_controls();
        taken
    }

    /// Clear the control learned for `action`.
    pub fn forget(&mut self, action: ShowAction) {
        self.controls.retain(|c| c.action != action || action == ShowAction::SongOfProgram);
    }

    /// Whether device `d`'s program change selects the song (#208).
    pub fn program_selects_song(&self, d: u32) -> bool {
        self.controls.iter().any(|c| c.action == ShowAction::SongOfProgram && c.device == Some(d))
    }

    /// The devices whose program change selects the song.
    pub fn program_devices(&self) -> impl Iterator<Item = u32> + '_ {
        self.controls.iter().filter(|c| c.action == ShowAction::SongOfProgram).filter_map(|c| c.device)
    }

    /// Past `MAX_PROGRAM_DEVICES`, turning one on does nothing.
    pub fn set_program_selects_song(&mut self, d: u32, on: bool) {
        self.controls.retain(|c| !(c.action == ShowAction::SongOfProgram && c.device == Some(d)));
        if on && self.program_devices().count() < MAX_PROGRAM_DEVICES {
            self.controls.push(ShowControl { device: Some(d), channel: None, msg: ShowMsg::AnyProgram, action: ShowAction::SongOfProgram });
            self.sort_controls();
        }
    }

    /// Table order: learned actions as `ShowAction::LEARNED` lists them,
    /// then program devices by index.
    fn sort_controls(&mut self) {
        let rank = |c: &ShowControl| (ShowAction::ALL.iter().position(|&a| a == c.action), c.device);
        self.controls.sort_by_key(rank);
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
        let next = SetlistPos { song: self.upcoming_song()?, part: 0 };
        self.active = Some(next);
        Some(next)
    }

    /// The song [`Self::next_song`] steps to: the first playable song after
    /// the active one, or the first playable song when none is active.
    pub fn upcoming_song(&self) -> Option<usize> {
        let from = self.active.map_or(0, |p| p.song + 1);
        (from..self.songs.len()).find(|&si| self.playable(si))
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
        self.settle_moved_song(to);
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
                    let (vlo, vhi) = (pr.vel_lo.min(127), pr.vel_hi.min(127));
                    (pr.vel_lo, pr.vel_hi) = (vlo.min(vhi), vlo.max(vhi));
                    pr.float = pr.float.min(FLOAT_MAX);
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
        self.normalize_controls();
        self.normalize_levels();
        self.clamp_active();
    }

    /// One control per learned action and one per program device, at most
    /// as many as the engine's table has slots, in table order. A session
    /// saved before #277 brings its foot switches.
    fn normalize_controls(&mut self) {
        for (fs, action) in [(self.next_cc.take(), ShowAction::NextPart), (self.prev_cc.take(), ShowAction::PrevPart)] {
            if let Some(fs) = fs.filter(|_| self.control_of(action).is_none()) {
                self.controls.push(ShowControl { device: None, channel: fs.channel, msg: ShowMsg::Cc(fs.cc), action });
            }
        }
        for c in &mut self.controls {
            c.channel = c.channel.map(|ch| ch & 0x0F);
            c.msg = match c.msg {
                ShowMsg::Cc(n) => ShowMsg::Cc(n & 0x7F),
                ShowMsg::Note(n) => ShowMsg::Note(n & 0x7F),
                ShowMsg::Program(n) => ShowMsg::Program(n & 0x7F),
                ShowMsg::AnyProgram => ShowMsg::AnyProgram,
            };
        }
        let program_row = |c: &ShowControl| c.msg == ShowMsg::AnyProgram && c.device.is_some();
        self.controls.retain(|c| (c.action == ShowAction::SongOfProgram) == program_row(c));
        self.sort_controls();
        let mut seen = Vec::new();
        self.controls.retain(|c| {
            let key = if c.action == ShowAction::SongOfProgram { (c.action, c.device) } else { (c.action, None) };
            let first = !seen.contains(&key);
            seen.push(key);
            first
        });
        let mut programs = 0;
        self.controls.retain(|c| {
            programs += usize::from(c.action == ShowAction::SongOfProgram);
            programs <= MAX_PROGRAM_DEVICES || c.action != ShowAction::SongOfProgram
        });
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

/// A song's backing track as a tool gives it: an absolute path, or
/// nothing (None or empty clears it). The file need not exist yet: the
/// pre-show check says when it is missing (#278).
pub fn backing_path(path: Option<&str>) -> Result<Option<String>, String> {
    match path.map(str::trim).filter(|p| !p.is_empty()) {
        None => Ok(None),
        Some(p) if std::path::Path::new(p).is_absolute() => Ok(Some(p.to_string())),
        Some(p) => Err(format!("backing must be the absolute path of a WAV file, not '{p}'")),
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
                        PartRig { rig: 0, key_lo: 0, key_hi: 59, transpose: 0, level: 0.9, ..PartRig::default() },
                        PartRig { rig: 1, key_lo: 60, key_hi: 127, transpose: 12, level: 0.8, ..PartRig::default() },
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

    /// #208: each device chosen for it becomes a control that takes every
    /// program change from that device, after the learned actions.
    #[test]
    fn devices_chosen_for_program_changes_become_song_select_controls_and_are_saved() {
        let mut sl = sample_setlist();
        sl.learn(ShowAction::PrevPart, Some(2), ShowMsg::Cc(81));
        sl.set_program_selects_song(4, true);
        sl.set_program_selects_song(1, true);
        sl.set_program_selects_song(4, true);
        assert_eq!(sl.program_devices().collect::<Vec<_>>(), [1, 4]);
        let song_select = |d| Some(ShowControl { device: Some(d), channel: None, msg: ShowMsg::AnyProgram, action: ShowAction::SongOfProgram });
        let t = sl.show_controls();
        assert!(t[0].is_some_and(|c| c.msg == ShowMsg::Cc(81) && c.action == ShowAction::PrevPart));
        assert_eq!(t[1..4], [song_select(1), song_select(4), None]);

        let text = serde_json::to_string(&sl).unwrap();
        assert!(text.contains(r#"{"device":1,"msg":"any_program","action":"song_of_program"}"#), "{text}");
        assert_eq!(serde_json::from_str::<Setlist>(&text).unwrap(), sl);
        sl.set_program_selects_song(1, false);
        sl.set_program_selects_song(4, false);
        sl.forget(ShowAction::PrevPart);
        assert!(!serde_json::to_string(&sl).unwrap().contains("show_controls"), "nothing chosen, nothing saved");
        let mut only = Setlist::default();
        only.set_program_selects_song(0, true);
        assert!(!only.is_empty(), "a setlist that only chose a device is saved");

        for action in ShowAction::LEARNED {
            sl.learn(action, None, ShowMsg::Note(action as u8));
        }
        for d in 0..16 {
            sl.set_program_selects_song(d, true);
        }
        assert_eq!(sl.program_devices().count(), MAX_PROGRAM_DEVICES, "no more devices than the table has slots");
        let t = sl.show_controls();
        assert!(sl.program_devices().all(|d| t.contains(&song_select(d))), "every device that shows On selects songs");
        assert!(ShowAction::LEARNED.iter().all(|&a| t.iter().flatten().any(|c| c.action == a)), "and every action keeps its control");
    }

    /// #277: one control per action; learning a message another action had
    /// moves it, so one press never does two things.
    #[test]
    fn a_learned_message_belongs_to_one_action() {
        let mut sl = Setlist::default();
        assert_eq!(sl.learn(ShowAction::NextPart, Some(1), ShowMsg::Cc(80)), None);
        assert_eq!(sl.learn(ShowAction::NextPart, Some(1), ShowMsg::Note(36)), None, "a relearn replaces the action's control");
        assert_eq!(sl.control_of(ShowAction::NextPart).map(|c| c.msg), Some(ShowMsg::Note(36)));
        assert_eq!(sl.learn(ShowAction::Panic, None, ShowMsg::Note(36)), Some(ShowAction::NextPart));
        assert_eq!(sl.control_of(ShowAction::NextPart), None);
        sl.learn(ShowAction::Record, Some(2), ShowMsg::Note(37));
        assert_eq!(sl.learn(ShowAction::Tuner, Some(3), ShowMsg::Note(37)), None, "the same note on another device is another control");
        assert_eq!(sl.controls.len(), 3);
        assert_eq!(sl.show_controls().iter().flatten().map(|c| c.action).collect::<Vec<_>>(), [ShowAction::Panic, ShowAction::Tuner, ShowAction::Record]);
    }

    /// A session saved before #277 kept two foot switches beside the list.
    #[test]
    fn a_session_with_the_old_foot_switches_loads_them_as_controls() {
        let mut sl: Setlist = serde_json::from_str(r#"{"next_cc":{"cc":80},"prev_cc":{"cc":209,"channel":3}}"#).unwrap();
        sl.normalize();
        assert_eq!(sl.control_of(ShowAction::NextPart), Some(ShowControl { device: None, channel: None, msg: ShowMsg::Cc(80), action: ShowAction::NextPart }));
        assert_eq!(sl.control_of(ShowAction::PrevPart), Some(ShowControl { device: None, channel: Some(3), msg: ShowMsg::Cc(81), action: ShowAction::PrevPart }));
        let text = serde_json::to_string(&sl).unwrap();
        assert!(!text.contains("next_cc") && text.contains(r#""msg":{"cc":80}"#), "{text}");
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
        assert_eq!(sl.upcoming_song(), Some(1));
        assert_eq!(sl.next_song(), Some(SetlistPos { song: 1, part: 0 }));
        assert_eq!(sl.upcoming_song(), None, "the last song has none after it");
        assert_eq!(sl.prev_song(), Some(SetlistPos { song: 0, part: 0 }));
    }

    #[test]
    fn the_engines_part_table_steps_like_next_part() {
        let mut sl = sample_setlist();
        sl.songs.insert(1, Song::new("Empty", vec![]));
        let parts = sl.show_parts();
        assert_eq!(parts.len(), 3, "a song without parts adds none");
        assert_eq!(sl.active_index(), None);
        let mut i = 0;
        sl.next_part();
        loop {
            assert_eq!((sl.active_index(), sl.pos_at(i)), (Some(i), sl.active));
            assert_eq!(parts[i].zones, sl.active_zones(), "the level-aware zones of the part");
            if sl.next_part().is_none() {
                break;
            }
            i += 1;
        }
        assert_eq!((i, sl.active), (2, Some(SetlistPos { song: 2, part: 0 })));
        assert_eq!(sl.pos_at(3), None);
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
    fn a_velocity_layer_and_a_floating_split_reach_the_zone_and_survive_a_save() {
        let mut pad = PartRig::new(1);
        pad.set_layer(Some(90), Some(20), Some("soft"), Some(40)).unwrap();
        assert_eq!((pad.vel_lo, pad.vel_hi, pad.curve, pad.float), (20, 90, VelCurve::Soft, FLOAT_MAX), "ranges ordered, float capped");
        pad.set_layer(None, Some(100), None, None).unwrap();
        assert_eq!((pad.vel_lo, pad.vel_hi, pad.curve), (20, 100, VelCurve::Soft), "what a call leaves out stays");
        assert!(pad.set_layer(None, None, Some("loud"), None).is_err());
        let z = pad.to_zone();
        assert_eq!((z.vel_lo, z.vel_hi, z.curve, z.float), (20, 100, VelCurve::Soft, FLOAT_MAX));
        let back: PartRig = serde_json::from_str(&serde_json::to_string(&pad).unwrap()).unwrap();
        assert_eq!(back, pad, "the layer is saved with the session");
        let mut tool = PartRig::new(0);
        tool.set_layer_json(&serde_json::json!({"vel_lo": 60.0, "vel_curve": "hard", "float": 3})).unwrap();
        assert_eq!((tool.vel_lo, tool.curve, tool.float), (60, VelCurve::Hard, 3), "a whole float is a number");
        for bad in [serde_json::json!({"vel_lo": "60"}), serde_json::json!({"vel_hi": 0.5}), serde_json::json!({"float": -2}), serde_json::json!({"vel_curve": 1})] {
            assert!(PartRig::new(0).set_layer_json(&bad).is_err(), "{bad} is refused, never defaulted");
        }
        let mut sl = sample_setlist();
        let pr = &mut sl.songs[0].parts[0].rigs[0];
        (pr.vel_lo, pr.vel_hi, pr.float) = (100, 40, 30);
        sl.normalize();
        let pr = sl.songs[0].parts[0].rigs[0];
        assert_eq!((pr.vel_lo, pr.vel_hi, pr.float), (40, 100, FLOAT_MAX), "a hand-edited session loads ordered and capped");
        let old: PartRig = serde_json::from_str(r#"{"rig":0,"key_lo":0,"key_hi":59,"transpose":0,"level":0.9}"#).unwrap();
        assert_eq!(old.to_zone(), RigZone { key_hi: 59, ..RigZone::OPEN }, "a part saved before #276 plays every velocity, with a fixed split");
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
