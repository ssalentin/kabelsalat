//! Show controls (#261, #277): hardware MIDI messages the engine takes for a
//! show action instead of passing them to the instruments, and the setlist
//! parts the engine steps through itself, so a foot switch changes the zones
//! in the block its CC arrives in, and a program change selects a song in the
//! block it arrives in (#208).
//!
//! The UI sends the table (`Command::SetShowControls`), every part
//! (`SetShowParts`) and the active part (`SetShowPart`); the engine publishes
//! the part it stepped to (`GraphPub::show_part`, `show_steps`) and the UI
//! sends the rest of that part a frame later: closed buses, pinned bindings,
//! tempo, levels and knobs. The engine does Panic, tap tempo, master mute and
//! the backing track itself; the tuner and recording belong to the app, which
//! does them in its next frame from the presses the engine counted
//! (`GraphPub::show_presses`).

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::control::{Command, RigZone, MAX_RIGS};
use crate::engine::MAX_HW_DEVICES;
use crate::matrix::SourceId;

/// Devices whose program change can select the song (#208).
pub const MAX_PROGRAM_DEVICES: usize = 14;

/// Slots in the engine's table of show controls: one per learned action,
/// then one per device whose program change selects the song.
pub const MAX_SHOW_CONTROLS: usize = ShowAction::LEARNED.len() + MAX_PROGRAM_DEVICES;

/// The message a show control listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowMsg {
    /// A control change by number. It acts when it rises to 64 or above.
    Cc(u8),
    /// A note by number. It acts on its note-on; its note-off is the release.
    Note(u8),
    /// A program change by number. Each one acts: it has no release.
    Program(u8),
    /// Every program change. Only a control listens for this; a message is
    /// always `Program(n)`. The first control in the table that takes a
    /// message wins, so one naming its program goes before this.
    AnyProgram,
}

/// What a show control does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowAction {
    NextSong,
    PrevSong,
    NextPart,
    PrevPart,
    /// All notes off on every rig (`Command::Panic`).
    Panic,
    TapTempo,
    /// The app opens or closes its tuner.
    Tuner,
    MasterMute,
    /// Start or stop the file player.
    BackingTrack,
    /// The app starts or stops recording, as its REC button does.
    Record,
    /// The backing track jumps to its next marker (#291), on the bar line
    /// when it is synced.
    NextMarker,
    PrevMarker,
    /// Loop the backing track's section playing, on and off.
    LoopSection,
    /// Stop the backing track at the end of its section playing, on and off.
    StopAtSection,
    /// Program change N selects the first part of song N+1 (#208), also of
    /// a song stepping skips. A program with no song, or a CC, selects nothing.
    SongOfProgram,
}

impl ShowAction {
    pub const ALL: [ShowAction; 15] = [
        ShowAction::NextSong,
        ShowAction::PrevSong,
        ShowAction::NextPart,
        ShowAction::PrevPart,
        ShowAction::Panic,
        ShowAction::TapTempo,
        ShowAction::Tuner,
        ShowAction::MasterMute,
        ShowAction::BackingTrack,
        ShowAction::Record,
        ShowAction::NextMarker,
        ShowAction::PrevMarker,
        ShowAction::LoopSection,
        ShowAction::StopAtSection,
        ShowAction::SongOfProgram,
    ];

    /// The actions one control each is learned for, in the order the app
    /// lists them. `SongOfProgram` is chosen per device instead.
    pub const LEARNED: [ShowAction; 14] = [
        ShowAction::NextSong,
        ShowAction::PrevSong,
        ShowAction::NextPart,
        ShowAction::PrevPart,
        ShowAction::Panic,
        ShowAction::TapTempo,
        ShowAction::Tuner,
        ShowAction::MasterMute,
        ShowAction::BackingTrack,
        ShowAction::Record,
        ShowAction::NextMarker,
        ShowAction::PrevMarker,
        ShowAction::LoopSection,
        ShowAction::StopAtSection,
    ];

    /// The name tools and sessions use.
    pub fn id(self) -> &'static str {
        match self {
            ShowAction::NextSong => "next_song",
            ShowAction::PrevSong => "prev_song",
            ShowAction::NextPart => "next_part",
            ShowAction::PrevPart => "prev_part",
            ShowAction::Panic => "panic",
            ShowAction::TapTempo => "tap_tempo",
            ShowAction::Tuner => "tuner",
            ShowAction::MasterMute => "master_mute",
            ShowAction::BackingTrack => "backing_track",
            ShowAction::Record => "record",
            ShowAction::NextMarker => "next_marker",
            ShowAction::PrevMarker => "prev_marker",
            ShowAction::LoopSection => "loop_section",
            ShowAction::StopAtSection => "stop_at_section",
            ShowAction::SongOfProgram => "song_of_program",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            ShowAction::NextSong => "Next song",
            ShowAction::PrevSong => "Previous song",
            ShowAction::NextPart => "Next part",
            ShowAction::PrevPart => "Previous part",
            ShowAction::Panic => "Panic",
            ShowAction::TapTempo => "Tap tempo",
            ShowAction::Tuner => "Tuner on/off",
            ShowAction::MasterMute => "Master mute",
            ShowAction::BackingTrack => "Backing track",
            ShowAction::Record => "Record",
            ShowAction::NextMarker => "Next marker",
            ShowAction::PrevMarker => "Previous marker",
            ShowAction::LoopSection => "Loop section",
            ShowAction::StopAtSection => "Stop at section end",
            ShowAction::SongOfProgram => "Program change picks the song",
        }
    }
}

/// A MIDI message from a device, consumed by an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowControl {
    /// Hardware device index (`SourceId::Hardware`); `None` takes every device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<u32>,
    /// MIDI channel 0..=15; `None` takes every channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    pub msg: ShowMsg,
    pub action: ShowAction,
}

pub type ShowControls = [Option<ShowControl>; MAX_SHOW_CONTROLS];

pub const NO_SHOW_CONTROLS: ShowControls = [None; MAX_SHOW_CONTROLS];

impl ShowControl {
    /// Whether this control takes `msg` from hardware device `device`.
    pub fn takes(&self, device: u32, channel: u8, msg: ShowMsg) -> bool {
        (self.msg == msg || self.msg == ShowMsg::AnyProgram && matches!(msg, ShowMsg::Program(_)))
            && self.device.is_none_or(|d| d == device)
            && self.channel.is_none_or(|c| c == channel & 0x0F)
    }
}

/// One setlist part as the engine steps through it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShowPart {
    /// Its song's index: stepping stays inside a song before it moves on.
    pub song: usize,
    /// Its song's set is skipped: a step from another song passes over it.
    pub skip: bool,
    /// What every rig takes in this part, levels applied (`Setlist::zones_of`).
    pub zones: [RigZone; MAX_RIGS],
}

/// Every setlist part in setlist order (`Command::SetShowParts`). Shared,
/// not copied: the app keeps each table it sent until the engine has let go
/// of it (`ShowSent`), so the audio thread never frees one.
#[derive(Clone, Debug, PartialEq)]
pub struct ShowParts(Arc<[ShowPart]>);

impl ShowParts {
    pub fn new(parts: Vec<ShowPart>) -> Self {
        Self(parts.into())
    }
}

impl Serialize for ShowParts {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ShowParts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::deserialize(d).map(Self::new)
    }
}

/// What the engine does with a hardware message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Take {
    /// No show control takes it: route it as before.
    Route,
    /// A show control took it; nothing else happens.
    Consumed,
    /// A show control took it and the setlist moved to a part with these zones.
    Part([RigZone; MAX_RIGS]),
    /// A show control took it for an action that is not a setlist step.
    Act(ShowAction),
}

/// The engine's show state. It neither allocates nor frees: a replaced part
/// table is still held by the app that sent it.
#[derive(Default)]
pub struct Show {
    controls: ShowControls,
    /// Per slot: the control is down (its last value was 64 or above).
    down: [bool; MAX_SHOW_CONTROLS],
    /// Per hardware device and MIDI channel, one bit per CC: the last value of
    /// it the instruments got from that device was 64 or above. A press heard
    /// before its control was bound (the press a learn binds) gets its release
    /// through, so a sustain pedal never stays down; one bound at rest passes
    /// nothing, and another device holding the same CC never collides.
    held_cc: [[u128; 16]; MAX_HW_DEVICES as usize],
    /// The same for notes: the instruments got its note-on and no note-off.
    held_note: [[u128; 16]; MAX_HW_DEVICES as usize],
    parts: Option<ShowParts>,
    /// The active part, as an index into `parts`.
    part: Option<usize>,
    /// Parts the engine stepped to itself.
    steps: u64,
    /// Presses per action (`ShowAction as usize`).
    presses: [u64; ShowAction::ALL.len()],
}

impl Show {
    /// Replace the table. A control that was already in the table keeps its
    /// down state even if it moved to another slot; a newly bound control
    /// starts down when the instruments hold its press, so that press steps
    /// nothing.
    pub fn set_controls(&mut self, controls: ShowControls) {
        let mut next_down = [false; MAX_SHOW_CONTROLS];
        for (i, c) in controls.iter().enumerate() {
            if let Some(c) = *c {
                next_down[i] = match self.controls.iter().position(|old| {
                    old.is_some_and(|o| o.device == c.device && o.channel == c.channel && o.msg == c.msg)
                }) {
                    Some(old_i) => self.down[old_i],
                    None => self.held_by_instruments(c),
                };
            }
        }
        self.down = next_down;
        self.controls = controls;
    }

    fn held_by_instruments(&self, c: ShowControl) -> bool {
        let (held, n) = match c.msg {
            ShowMsg::Cc(n) => (&self.held_cc, n),
            ShowMsg::Note(n) => (&self.held_note, n),
            ShowMsg::Program(_) | ShowMsg::AnyProgram => return false,
        };
        let bit = 1u128 << (n & 0x7F);
        let ch_held = |dev_held: &[u128; 16]| match c.channel {
            Some(ch) => dev_held[(ch & 0x0F) as usize] & bit != 0,
            None => dev_held.iter().any(|h| h & bit != 0),
        };
        match c.device {
            Some(d) => held.get(d as usize).is_some_and(ch_held),
            None => held.iter().any(ch_held),
        }
    }

    pub fn set_parts(&mut self, parts: ShowParts) {
        self.parts = Some(parts);
    }

    pub fn set_part(&mut self, part: Option<usize>) {
        self.part = part;
    }

    pub fn part(&self) -> Option<usize> {
        self.part
    }

    pub fn steps(&self) -> u64 {
        self.steps
    }

    pub fn presses(&self) -> [u64; ShowAction::ALL.len()] {
        self.presses
    }

    /// What decides how the next press and release go: the table, which
    /// controls are down and what the instruments hold.
    #[cfg(test)]
    pub fn edges(&self) -> String {
        let nz = |h: &[[u128; 16]; MAX_HW_DEVICES as usize]| -> Vec<(usize, usize, u128)> {
            h.iter()
                .enumerate()
                .flat_map(|(d, cs)| cs.iter().enumerate().filter(|&(_, &v)| v != 0).map(move |(c, &v)| (d, c, v)))
                .collect()
        };
        format!("{:?}|{:?}|{:?}|{:?}", self.controls, self.down, nz(&self.held_cc), nz(&self.held_note))
    }

    /// A message from `source`, `value` its data byte: a CC's value, 127 for
    /// a note-on and 0 for a note-off; a program change has none.
    pub fn take(&mut self, source: SourceId, channel: u8, msg: ShowMsg, value: u8) -> Take {
        let SourceId::Hardware(device) = source else { return Take::Route };
        let slot = self.controls.iter().position(|c| c.is_some_and(|c| c.takes(device, channel, msg)));
        if let Some(take) = self.edge(slot, device, channel, msg, value >= 64) {
            return take;
        }
        let Some(control) = slot.and_then(|i| self.controls[i]) else { return Take::Route };
        self.presses[control.action as usize] += 1;
        match (control.action, msg) {
            (ShowAction::NextPart, _) => self.step(true),
            (ShowAction::PrevPart, _) => self.step(false),
            (ShowAction::NextSong, _) => self.step_song(true),
            (ShowAction::PrevSong, _) => self.step_song(false),
            (ShowAction::SongOfProgram, ShowMsg::Program(n)) => self.select_song(n as usize),
            (ShowAction::SongOfProgram, _) => Take::Consumed,
            (action, _) => Take::Act(action),
        }
    }

    /// What a CC's value or a note's on/off does before its control acts:
    /// `Some` when it acts no further (no control, a release, a value held
    /// down), `None` on a press of control `slot`. A program change has no
    /// edge: every one is a press.
    fn edge(&mut self, slot: Option<usize>, device: u32, channel: u8, msg: ShowMsg, down: bool) -> Option<Take> {
        let (held, n) = match msg {
            ShowMsg::Cc(n) => (&mut self.held_cc, n),
            ShowMsg::Note(n) => (&mut self.held_note, n),
            ShowMsg::Program(_) | ShowMsg::AnyProgram => return None,
        };
        let bit = 1u128 << (n & 0x7F);
        let dev_held = held.get_mut(device as usize).map(|d| &mut d[(channel & 0x0F) as usize]);
        let Some(i) = slot else {
            if let Some(h) = dev_held {
                if down { *h |= bit } else { *h &= !bit }
            }
            return Some(Take::Route);
        };
        let pressed = down && !self.down[i];
        self.down[i] = down;
        if !down && dev_held.as_deref().is_some_and(|h| *h & bit != 0) {
            if let Some(h) = dev_held {
                *h &= !bit;
            }
            return Some(Take::Route);
        }
        (!pressed).then_some(Take::Consumed)
    }

    /// The first part of song `song`; a song without one selects nothing.
    fn select_song(&mut self, song: usize) -> Take {
        let parts = self.parts.as_ref().map_or(&[][..], |p| &p.0[..]);
        match parts.iter().position(|p| p.song == song) {
            Some(to) => self.go(to),
            None => Take::Consumed,
        }
    }

    fn go(&mut self, to: usize) -> Take {
        let parts = self.parts.as_ref().map_or(&[][..], |p| &p.0[..]);
        let Some(part) = parts.get(to) else { return Take::Consumed };
        self.part = Some(to);
        self.steps += 1;
        Take::Part(part.zones)
    }

    /// Part `n` of the song the active part belongs to, as a backing-track
    /// marker selects it (#291). `None` when nothing moves: no part active,
    /// the song has no such part, or it is the part already playing.
    pub fn go_song_part(&mut self, n: usize) -> Option<[RigZone; MAX_RIGS]> {
        let parts = self.parts.as_ref().map_or(&[][..], |p| &p.0[..]);
        let now = self.part.filter(|&p| p < parts.len())?;
        let song = parts[now].song;
        let first = parts.iter().position(|p| p.song == song)?;
        let to = first + n;
        if to == now || parts.get(to).is_none_or(|p| p.song != song) {
            return None;
        }
        match self.go(to) {
            Take::Part(zones) => Some(zones),
            _ => None,
        }
    }

    /// To the first part of the next (or previous) song, as
    /// `Setlist::next_song`/`prev_song` step: past songs stepping skips, no
    /// further than either end. With no part active, to the first part of
    /// the first song stepping does not skip.
    fn step_song(&mut self, forward: bool) -> Take {
        let parts = self.parts.as_ref().map_or(&[][..], |p| &p.0[..]);
        let starts = |q: &usize| !parts[*q].skip && (*q == 0 || parts[*q - 1].song != parts[*q].song);
        let to = match self.part.filter(|&p| p < parts.len()) {
            None => (0..parts.len()).find(starts),
            Some(p) if forward => (p + 1..parts.len()).find(starts),
            Some(p) => (0..p).rev().find(|q| starts(q) && parts[*q].song != parts[p].song),
        };
        match to {
            Some(to) => self.go(to),
            None => Take::Consumed,
        }
    }

    /// One part on, as `Setlist::next_part`/`prev_part` step: inside the
    /// song first, then to the next (or previous) song stepping does not
    /// skip, and no further than either end. With no part active, to the
    /// first part of the first such song.
    fn step(&mut self, forward: bool) -> Take {
        let parts = self.parts.as_ref().map_or(&[][..], |p| &p.0[..]);
        let playable = |q: &usize| !parts[*q].skip;
        let to = match self.part.filter(|&p| p < parts.len()) {
            None => (0..parts.len()).find(playable),
            Some(p) if forward => match parts.get(p + 1) {
                Some(next) if next.song == parts[p].song => Some(p + 1),
                _ => (p + 1..parts.len()).find(playable),
            },
            Some(p) => match p.checked_sub(1) {
                Some(q) if parts[q].song == parts[p].song => Some(q),
                _ => (0..p).rev().find(playable),
            },
        };
        match to {
            Some(to) => self.go(to),
            None => Take::Consumed,
        }
    }
}

/// What a frontend last sent the engine of its setlist, so a frame sends
/// only what changed.
#[derive(Default)]
pub struct ShowSent {
    controls: ShowControls,
    /// Every part table the engine may still hold, the one sent last at the end.
    parts: Vec<ShowParts>,
    /// The active part as the engine knows it. Set it when the engine moved
    /// the part itself, so the frontend does not send that move back.
    pub part: Option<usize>,
}

impl ShowSent {
    /// Send what changed; a command `send` refuses (bus full) is sent again
    /// next time. A part table the engine let go of is freed here.
    pub fn publish(&mut self, setlist: &crate::setlist::Setlist, mut send: impl FnMut(Command) -> bool) {
        let controls = setlist.show_controls();
        if controls != self.controls && send(Command::SetShowControls { controls }) {
            self.controls = controls;
        }
        self.parts.retain(|p| Arc::strong_count(&p.0) > 1);
        let now = setlist.show_parts();
        if self.parts.last().map_or(&[][..], |p| &p.0[..]) != &now[..] {
            let parts = ShowParts::new(now);
            if send(Command::SetShowParts { parts: parts.clone() }) {
                self.parts.push(parts);
            }
        }
        let part = setlist.active_index();
        if part != self.part && send(Command::SetShowPart { part }) {
            self.part = part;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF: RigZone = RigZone { live: false, ..RigZone::OPEN };

    fn part(a: RigZone, b: RigZone) -> [RigZone; MAX_RIGS] {
        let mut z = [RigZone::OPEN; MAX_RIGS];
        z[0] = a;
        z[1] = b;
        z
    }

    /// Each zone table as a one-part song.
    fn songs(zones: &[[RigZone; MAX_RIGS]]) -> ShowParts {
        ShowParts::new(zones.iter().enumerate().map(|(song, &zones)| ShowPart { song, skip: false, zones }).collect())
    }

    fn show(controls: &[ShowControl]) -> Show {
        let mut s = Show::default();
        let mut t = NO_SHOW_CONTROLS;
        for (slot, c) in t.iter_mut().zip(controls) {
            *slot = Some(*c);
        }
        s.set_controls(t);
        s.set_parts(songs(&[part(RigZone::OPEN, OFF), part(OFF, RigZone::OPEN), part(RigZone::OPEN, RigZone::OPEN)]));
        s
    }

    const NEXT: ShowControl = ShowControl { device: None, channel: None, msg: ShowMsg::Cc(80), action: ShowAction::NextPart };
    const PREV: ShowControl = ShowControl { device: None, channel: None, msg: ShowMsg::Cc(81), action: ShowAction::PrevPart };
    const PEDAL: SourceId = SourceId::Hardware(2);

    fn cc(s: &mut Show, cc: u8, value: u8) -> Take {
        s.take(PEDAL, 0, ShowMsg::Cc(cc), value)
    }

    #[test]
    fn a_press_steps_once_and_stops_at_either_end() {
        let mut s = show(&[NEXT, PREV]);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, OFF)), "nothing active: the first part");
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed, "held: no second step");
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed, "nor does its release reach the instruments");
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed, "at the last part");
        assert_eq!((s.part(), s.steps()), (Some(2), 3));
        assert_eq!(cc(&mut s, 81, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 81, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 81, 127), Take::Part(part(RigZone::OPEN, OFF)));
        assert_eq!(cc(&mut s, 81, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 81, 127), Take::Consumed, "at the first part");
        assert_eq!(s.part(), Some(0));
    }

    #[test]
    fn only_the_bound_message_from_the_bound_device_and_channel_is_taken() {
        let only = ShowControl { device: Some(2), channel: Some(3), ..NEXT };
        let mut s = show(&[only]);
        assert_eq!(s.take(PEDAL, 0, ShowMsg::Cc(80), 127), Take::Route, "another channel");
        assert_eq!(s.take(SourceId::Hardware(1), 3, ShowMsg::Cc(80), 127), Take::Route, "another device");
        assert_eq!(s.take(SourceId::Keys, 3, ShowMsg::Cc(80), 127), Take::Route, "the computer keyboard");
        assert_eq!(s.take(PEDAL, 3, ShowMsg::Cc(64), 127), Take::Route, "another CC");
        assert!(matches!(s.take(PEDAL, 3, ShowMsg::Cc(80), 127), Take::Part(_)));
    }

    #[test]
    fn a_press_with_no_parts_is_still_consumed() {
        let mut s = show(&[NEXT]);
        s.set_parts(songs(&[]));
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed);
        assert_eq!(s.part(), None);
    }

    #[test]
    fn the_press_a_control_is_learned_from_steps_nothing_and_its_release_gets_through() {
        let mut s = show(&[]);
        assert_eq!(cc(&mut s, 80, 127), Take::Route, "not bound yet: the instruments hear the press");
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(NEXT);
        s.set_controls(t);
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed, "the held press steps nothing");
        assert_eq!(cc(&mut s, 80, 0), Take::Route, "its release reaches the instruments, so nothing stays held");
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, OFF)));
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed, "from then on the switch is the show's alone");
    }

    #[test]
    fn a_rebind_changes_only_its_own_slot() {
        let mut s = show(&[NEXT, PREV]);
        cc(&mut s, 80, 127);
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(NEXT);
        t[1] = Some(ShowControl { msg: ShowMsg::Cc(82), ..PREV });
        s.set_controls(t);
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed, "NEXT was not rebound: it is still down");
        assert_eq!(cc(&mut s, 81, 127), Take::Route, "CC 81 is no show control any more");
        cc(&mut s, 80, 0);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 82, 127), Take::Part(part(RigZone::OPEN, OFF)), "the new PREV steps back");
    }

    #[test]
    fn a_setlist_is_sent_once_and_a_refused_command_again() {
        use crate::setlist::{Part, PartRig, Setlist, Song};
        let mut sl = Setlist::default();
        sl.songs.push(Song::new("Opener", vec![Part::new("Verse", vec![PartRig::new(0)]), Part::new("Chorus", vec![PartRig::new(1)])]));
        sl.learn(ShowAction::NextPart, None, ShowMsg::Cc(80));
        sl.select(0, 1);
        let mut sent = ShowSent::default();
        let mut out = Vec::new();
        sent.publish(&sl, |c| {
            out.push(c);
            true
        });
        assert!(matches!(&out[..], [
            Command::SetShowControls { controls },
            Command::SetShowParts { parts },
            Command::SetShowPart { part: Some(1) },
        ] if controls[0].is_some_and(|c| c.msg == ShowMsg::Cc(80) && c.action == ShowAction::NextPart) && parts.0.len() == 2));
        sent.publish(&sl, |c| {
            out.push(c);
            true
        });
        assert_eq!(out.len(), 3, "nothing changed, nothing sent");
        sl.select(0, 0);
        sent.publish(&sl, |_| false);
        sent.publish(&sl, |c| {
            out.push(c);
            true
        });
        assert!(matches!(&out[3..], [Command::SetShowPart { part: Some(0) }]), "the refused move is sent again");
    }

    #[test]
    fn a_replaced_part_table_is_freed_by_the_app_never_by_the_engine() {
        use crate::setlist::{Part, PartRig, Setlist, Song};
        let mut sl = Setlist::default();
        sl.songs.push(Song::new("Opener", vec![Part::new("Verse", vec![PartRig::new(0)])]));
        let mut engine = Show::default();
        let mut sent = ShowSent::default();
        let mut publish = |sent: &mut ShowSent, sl: &Setlist| {
            sent.publish(sl, |c| {
                if let Command::SetShowParts { parts } = c {
                    engine.set_parts(parts);
                }
                true
            })
        };
        publish(&mut sent, &sl);
        let first = Arc::downgrade(&sent.parts[0].0);
        sl.songs[0].parts[0].rigs[0].transpose = 5;
        publish(&mut sent, &sl);
        assert_eq!(first.strong_count(), 1, "the engine let go of the old table while the app still held it");
        publish(&mut sent, &sl);
        assert_eq!(first.strong_count(), 0, "the app freed it on its next publish");
        assert_eq!(sent.parts.len(), 1, "and keeps the table the engine holds");
    }

    #[test]
    fn the_engine_steps_as_the_setlist_does_past_a_skipped_set() {
        use crate::setlist::{Part, PartRig, Setlist, Song};
        let mut sl = Setlist::default();
        for (name, n) in [("A", 2), ("B", 1), ("C", 2), ("D", 1)] {
            let parts = (0..n).map(|i| Part::new(format!("{name}{i}"), vec![PartRig::new(0)])).collect();
            sl.songs.push(Song::new(name, parts));
        }
        let encore = sl.add_set("Encore");
        sl.assign_set(2, Some(encore));
        sl.sets.iter_mut().for_each(|set| set.skip = true);
        let mut engine = show(&[NEXT, PREV]);
        engine.set_parts(ShowParts::new(sl.show_parts()));
        let tap = |engine: &mut Show, sl: &mut Setlist, forward: bool| {
            let moved = if forward { sl.next_part() } else { sl.prev_part() };
            let cc_no = if forward { 80 } else { 81 };
            let took = cc(engine, cc_no, 127);
            cc(engine, cc_no, 0);
            assert_eq!(matches!(took, Take::Part(_)), moved.is_some(), "a step where the setlist steps, at {:?}", sl.active);
            assert_eq!(engine.part(), sl.active_index(), "the same part as the setlist");
        };
        for forward in [true, true, true, true, true, false, false, false, false, false] {
            tap(&mut engine, &mut sl, forward);
        }
        sl.select(2, 0);
        engine.set_part(sl.active_index());
        for forward in [true, true, false, false] {
            tap(&mut engine, &mut sl, forward);
        }
    }

    const SONG_SELECT: ShowControl = ShowControl { device: Some(2), channel: None, msg: ShowMsg::AnyProgram, action: ShowAction::SongOfProgram };

    fn program(s: &mut Show, source: SourceId, n: u8) -> Take {
        s.take(source, 0, ShowMsg::Program(n), 0)
    }

    #[test]
    fn a_program_change_selects_the_first_part_of_its_song_and_one_with_no_song_is_still_consumed() {
        let mut s = show(&[SONG_SELECT]);
        let zones = |n: u8| part(RigZone { transpose: n as i8, ..RigZone::OPEN }, OFF);
        // Song 0 has two parts, song 1 one in a skipped set, song 2 none, song 3 one.
        let parts = [(0, false, 0), (0, false, 1), (1, true, 2), (3, false, 3)];
        s.set_parts(ShowParts::new(parts.iter().map(|&(song, skip, z)| ShowPart { song, skip, zones: zones(z) }).collect()));
        assert_eq!(program(&mut s, PEDAL, 3), Take::Part(zones(3)), "program 3 is song 4");
        assert_eq!(program(&mut s, PEDAL, 0), Take::Part(zones(0)), "program 0 is song 1, at its first part");
        assert_eq!(program(&mut s, PEDAL, 0), Take::Part(zones(0)), "each program change acts: it has no release");
        assert_eq!(program(&mut s, PEDAL, 1), Take::Part(zones(2)), "a song stepping skips is still selected");
        assert_eq!((s.part(), s.steps()), (Some(2), 4));
        assert_eq!(program(&mut s, PEDAL, 2), Take::Consumed, "a song without parts selects nothing");
        assert_eq!(program(&mut s, PEDAL, 9), Take::Consumed, "nor does a program with no song");
        assert_eq!(s.part(), Some(2));
        assert_eq!(program(&mut s, SourceId::Hardware(1), 0), Take::Route, "another device's program change reaches its instruments");
        assert_eq!(program(&mut s, SourceId::Keys, 0), Take::Route, "so does the computer keyboard's");
        assert_eq!(s.take(PEDAL, 0, ShowMsg::Cc(0), 127), Take::Route, "the device's CCs are not program changes");
    }

    #[test]
    fn a_control_naming_its_program_goes_before_every_program() {
        let five = ShowControl { device: None, channel: None, msg: ShowMsg::Program(5), action: ShowAction::NextPart };
        let mut s = show(&[five, SONG_SELECT]);
        assert_eq!(program(&mut s, PEDAL, 1), Take::Part(part(OFF, RigZone::OPEN)), "program 1 selects song 2");
        assert_eq!(program(&mut s, PEDAL, 5), Take::Part(part(RigZone::OPEN, RigZone::OPEN)), "program 5 steps to the next part");
        assert_eq!(program(&mut s, SourceId::Hardware(1), 5), Take::Consumed, "on any device, here at the last part");
    }

    #[test]
    fn the_ui_moving_the_part_moves_where_the_next_press_goes() {
        let mut s = show(&[NEXT]);
        s.set_part(Some(1));
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)));
        s.set_part(Some(7));
        cc(&mut s, 80, 0);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, OFF)), "a part the table no longer has counts as none");
    }

    #[test]
    fn a_marker_selects_a_part_of_the_song_playing_and_no_other() {
        let mut s = show(&[]);
        // Song 0: parts 0, 1, 2; song 1: parts 3, 4.
        s.set_parts(songs_of(&[(0, false), (0, false), (0, false), (1, false), (1, false)]));
        assert_eq!(s.go_song_part(1), None, "no part active: a marker moves nothing");
        s.set_part(Some(4));
        let zones = |i: usize| part(RigZone { transpose: i as i8, ..RigZone::OPEN }, OFF);
        assert_eq!(s.go_song_part(0), Some(zones(3)), "part 1 of the second song");
        assert_eq!((s.part(), s.steps()), (Some(3), 1));
        assert_eq!(s.go_song_part(0), None, "already there: no second step");
        assert_eq!(s.go_song_part(2), None, "the song has two parts");
        s.set_part(Some(0));
        assert_eq!(s.go_song_part(2), Some(zones(2)));
        assert_eq!(s.go_song_part(3), None, "part 4 would be the next song's");
        assert_eq!((s.part(), s.steps()), (Some(2), 2));
    }

    fn note(s: &mut Show, n: u8, on: bool) -> Take {
        s.take(PEDAL, 0, ShowMsg::Note(n), if on { 127 } else { 0 })
    }

    const PAD: ShowControl = ShowControl { device: Some(2), channel: None, msg: ShowMsg::Note(36), action: ShowAction::NextPart };

    #[test]
    fn a_note_steps_on_its_note_on_and_its_note_off_is_consumed() {
        let mut s = show(&[PAD]);
        assert_eq!(note(&mut s, 36, true), Take::Part(part(RigZone::OPEN, OFF)));
        assert_eq!(note(&mut s, 36, false), Take::Consumed, "the release reaches no instrument");
        assert_eq!(note(&mut s, 36, true), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(note(&mut s, 37, true), Take::Route, "another key plays");
        assert_eq!(s.take(SourceId::Hardware(1), 0, ShowMsg::Note(36), 127), Take::Route, "so does the same key on another keyboard");
        assert_eq!(cc(&mut s, 36, 127), Take::Route, "a CC of that number is no note");
    }

    #[test]
    fn a_note_learned_while_its_key_is_down_lets_its_note_off_through() {
        let mut s = show(&[]);
        assert_eq!(note(&mut s, 36, true), Take::Route, "the key a learn binds sounds first");
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(PAD);
        s.set_controls(t);
        assert_eq!(note(&mut s, 36, false), Take::Route, "its note-off reaches the instrument that sounds it");
        assert_eq!(note(&mut s, 36, true), Take::Part(part(RigZone::OPEN, OFF)));
        assert_eq!(note(&mut s, 36, false), Take::Consumed);
    }

    /// Parts as `(song, skip)`, each with zones that name its place.
    fn songs_of(parts: &[(usize, bool)]) -> ShowParts {
        let zones = |i: usize| part(RigZone { transpose: i as i8, ..RigZone::OPEN }, OFF);
        ShowParts::new(parts.iter().enumerate().map(|(i, &(song, skip))| ShowPart { song, skip, zones: zones(i) }).collect())
    }

    #[test]
    fn song_steps_go_to_a_songs_first_part_past_skipped_songs() {
        let next = ShowControl { msg: ShowMsg::Cc(82), action: ShowAction::NextSong, ..NEXT };
        let prev = ShowControl { msg: ShowMsg::Cc(83), action: ShowAction::PrevSong, ..NEXT };
        let mut s = show(&[next, prev]);
        // Song 0: parts 0, 1; song 1 (skipped): 2; song 2: 3, 4.
        s.set_parts(songs_of(&[(0, false), (0, false), (1, true), (2, false), (2, false)]));
        let tap = |s: &mut Show, n: u8| {
            let took = cc(s, n, 127);
            cc(s, n, 0);
            (took != Take::Consumed).then(|| s.part()).flatten()
        };
        assert_eq!(tap(&mut s, 82), Some(0), "nothing active: the first song");
        assert_eq!(tap(&mut s, 82), Some(3), "past the skipped song");
        assert_eq!(tap(&mut s, 82), None, "at the last song");
        s.set_part(Some(4));
        assert_eq!(tap(&mut s, 83), Some(0), "from a later part, back to the song before, at its first part");
        assert_eq!(tap(&mut s, 83), None, "at the first song");
        let mut setlist = crate::setlist::Setlist::default();
        for (name, n) in [("A", 2), ("B", 1), ("C", 2)] {
            let parts = (0..n).map(|i| crate::setlist::Part::new(format!("{name}{i}"), vec![crate::setlist::PartRig::new(0)])).collect();
            setlist.songs.push(crate::setlist::Song::new(name, parts));
        }
        let set = setlist.add_set("Encore");
        setlist.assign_set(1, Some(set));
        setlist.sets[0].skip = true;
        setlist.select(2, 1);
        setlist.prev_song();
        assert_eq!(setlist.active_index(), Some(0), "the setlist's own song step lands where the engine's does");
    }

    #[test]
    fn every_other_action_is_handed_to_the_engine_once_per_press_and_counted() {
        let others: Vec<ShowControl> = [ShowAction::Panic, ShowAction::TapTempo, ShowAction::Tuner, ShowAction::MasterMute, ShowAction::BackingTrack, ShowAction::Record, ShowAction::NextMarker, ShowAction::PrevMarker, ShowAction::LoopSection, ShowAction::StopAtSection]
            .iter()
            .enumerate()
            .map(|(i, &action)| ShowControl { device: None, channel: None, msg: ShowMsg::Note(60 + i as u8), action })
            .collect();
        let mut s = show(&others);
        for c in &others {
            let ShowMsg::Note(n) = c.msg else { unreachable!() };
            assert_eq!(note(&mut s, n, true), Take::Act(c.action));
            assert_eq!(note(&mut s, n, false), Take::Consumed);
            assert_eq!(note(&mut s, n, true), Take::Act(c.action), "the next press acts again");
            note(&mut s, n, false);
            assert_eq!(s.presses()[c.action as usize], 2, "{:?}", c.action);
        }
        assert_eq!(s.part(), None, "no action of these steps the setlist");
        let program = ShowControl { device: None, channel: None, msg: ShowMsg::Program(9), action: ShowAction::Panic };
        let mut s = show(&[program]);
        assert_eq!(s.take(PEDAL, 0, ShowMsg::Program(9), 0), Take::Act(ShowAction::Panic));
        assert_eq!(s.take(PEDAL, 0, ShowMsg::Program(9), 0), Take::Act(ShowAction::Panic), "every program change is a press");
    }

    #[test]
    fn the_table_holds_every_action_and_every_program_device() {
        assert_eq!(MAX_SHOW_CONTROLS, ShowAction::LEARNED.len() + MAX_PROGRAM_DEVICES);
        assert!(ShowAction::LEARNED.iter().all(|a| ShowAction::ALL.contains(a)));
        assert_eq!(ShowAction::ALL.len(), ShowAction::LEARNED.len() + 1, "only the program devices' action is chosen per device");
        for (i, a) in ShowAction::ALL.into_iter().enumerate() {
            assert_eq!(a as usize, i, "presses are indexed by the action");
            assert_eq!(ShowAction::from_id(a.id()), Some(a));
            assert_eq!(serde_json::to_value(a).unwrap(), serde_json::json!(a.id()), "sessions and tools use one name");
        }
    }

    #[test]
    fn another_device_holding_the_same_note_or_cc_neither_leaks_releases_nor_swallows_presses() {
        let kb = SourceId::Hardware(0);
        let pad = SourceId::Hardware(1);
        let pad_note = ShowControl { device: Some(1), channel: None, msg: ShowMsg::Note(36), action: ShowAction::NextPart };
        let pad_cc = ShowControl { device: Some(1), channel: None, msg: ShowMsg::Cc(64), action: ShowAction::MasterMute };
        let mut s = show(&[]);
        assert_eq!(s.take(kb, 0, ShowMsg::Note(36), 127), Take::Route, "keyboard holds C2");
        assert_eq!(s.take(kb, 0, ShowMsg::Cc(64), 127), Take::Route, "keyboard holds sustain");
        // Bind controller B's note 36 and CC 64 while keyboard A is holding both.
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(pad_note);
        t[1] = Some(pad_cc);
        s.set_controls(t);
        assert!(matches!(s.take(pad, 0, ShowMsg::Note(36), 127), Take::Part(_)), "controller B's first press is not swallowed");
        assert_eq!(s.take(pad, 0, ShowMsg::Note(36), 0), Take::Consumed, "controller B's release never reaches keyboard A's instruments");
        assert_eq!(s.take(pad, 0, ShowMsg::Cc(64), 127), Take::Act(ShowAction::MasterMute), "controller B's CC press is not swallowed");
        assert_eq!(s.take(pad, 0, ShowMsg::Cc(64), 0), Take::Consumed, "controller B's CC release never lifts keyboard A's sustain");
        assert_eq!(s.take(kb, 0, ShowMsg::Note(36), 0), Take::Route, "keyboard A's own release still reaches instruments");
        assert_eq!(s.take(kb, 0, ShowMsg::Cc(64), 0), Take::Route, "keyboard A's own sustain-up still reaches instruments");
    }

    #[test]
    fn shifting_slots_keeps_a_held_control_down() {
        let mut s = show(&[PREV]);
        assert!(matches!(cc(&mut s, 81, 127), Take::Part(_)));
        // Adding NextPart ahead of PrevPart shifts PrevPart from slot 0 to slot 1.
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(NEXT);
        t[1] = Some(PREV);
        s.set_controls(t);
        assert_eq!(cc(&mut s, 81, 127), Take::Consumed, "still held after moving to slot 1: no second trigger");
        assert_eq!(cc(&mut s, 81, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 81, 127), Take::Part(part(RigZone::OPEN, OFF)), "after release, the next press steps back");
    }
}
