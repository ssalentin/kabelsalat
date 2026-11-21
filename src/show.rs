//! Show controls (#261): hardware MIDI messages the engine takes for a show
//! action instead of passing them to the instruments, and the setlist parts
//! the engine steps through itself, so a foot switch changes the zones in
//! the block its CC arrives in.
//!
//! The UI sends the table (`Command::SetShowControls`), every part
//! (`SetShowParts`) and the active part (`SetShowPart`); the engine publishes
//! the part it stepped to (`GraphPub::show_part`, `show_steps`) and the UI
//! sends the rest of that part a frame later: closed buses, pinned bindings,
//! tempo, levels and knobs.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::control::{Command, RigZone, MAX_RIGS};
use crate::matrix::SourceId;

/// Slots in the engine's table of show controls.
pub const MAX_SHOW_CONTROLS: usize = 16;

/// The message a show control listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShowMsg {
    /// A control change by number. It acts when it rises to 64 or above.
    Cc(u8),
}

/// What a show control does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShowAction {
    NextPart,
    PrevPart,
}

/// A MIDI message from a device, consumed by an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowControl {
    /// Hardware device index (`SourceId::Hardware`); `None` takes every device.
    pub device: Option<u32>,
    /// MIDI channel 0..=15; `None` takes every channel.
    pub channel: Option<u8>,
    pub msg: ShowMsg,
    pub action: ShowAction,
}

pub type ShowControls = [Option<ShowControl>; MAX_SHOW_CONTROLS];

pub const NO_SHOW_CONTROLS: ShowControls = [None; MAX_SHOW_CONTROLS];

impl ShowControl {
    fn takes(&self, device: u32, channel: u8, msg: ShowMsg) -> bool {
        self.msg == msg
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
}

/// The engine's show state. It neither allocates nor frees: a replaced part
/// table is still held by the app that sent it.
#[derive(Default)]
pub struct Show {
    controls: ShowControls,
    /// Per slot: the control is down (its last value was 64 or above).
    down: [bool; MAX_SHOW_CONTROLS],
    /// Per MIDI channel, one bit per CC: the last value of it the instruments
    /// got from hardware was 64 or above. A press heard before its control
    /// was bound (the press a learn binds) gets its release through, so a
    /// sustain pedal never stays down; one bound at rest passes nothing.
    held: [u128; 16],
    parts: Option<ShowParts>,
    /// The active part, as an index into `parts`.
    part: Option<usize>,
    /// Parts the engine stepped to itself.
    steps: u64,
}

impl Show {
    /// Replace the table. A slot whose control changed starts down when the
    /// instruments hold its press, so that press steps nothing.
    pub fn set_controls(&mut self, controls: ShowControls) {
        for (i, c) in controls.iter().enumerate() {
            if *c != self.controls[i] {
                self.down[i] = c.is_some_and(|c| self.held_by_instruments(c));
            }
        }
        self.controls = controls;
    }

    fn held_by_instruments(&self, c: ShowControl) -> bool {
        let ShowMsg::Cc(cc) = c.msg;
        let bit = 1u128 << (cc & 0x7F);
        match c.channel {
            Some(ch) => self.held[(ch & 0x0F) as usize] & bit != 0,
            None => self.held.iter().any(|h| h & bit != 0),
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

    /// A message from `source`, `value` its data byte (a CC's value).
    pub fn take(&mut self, source: SourceId, channel: u8, msg: ShowMsg, value: u8) -> Take {
        let SourceId::Hardware(device) = source else { return Take::Route };
        let ShowMsg::Cc(cc) = msg;
        let (held, bit) = (&mut self.held[(channel & 0x0F) as usize], 1u128 << (cc & 0x7F));
        let down = value >= 64;
        let Some(i) = self.controls.iter().position(|c| c.is_some_and(|c| c.takes(device, channel, msg))) else {
            if down { *held |= bit } else { *held &= !bit }
            return Take::Route;
        };
        let pressed = down && !self.down[i];
        self.down[i] = down;
        if !down && *held & bit != 0 {
            *held &= !bit;
            return Take::Route;
        }
        if !pressed {
            return Take::Consumed;
        }
        match self.controls[i].map(|c| c.action) {
            Some(ShowAction::NextPart) => self.step(true),
            Some(ShowAction::PrevPart) => self.step(false),
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
        let Some(to) = to else { return Take::Consumed };
        self.part = Some(to);
        self.steps += 1;
        Take::Part(parts[to].zones)
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
        use crate::setlist::{FootSwitch, Part, PartRig, Setlist, Song};
        let mut sl = Setlist::default();
        sl.songs.push(Song::new("Opener", vec![Part::new("Verse", vec![PartRig::new(0)]), Part::new("Chorus", vec![PartRig::new(1)])]));
        sl.next_cc = Some(FootSwitch { cc: 80, channel: None });
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

    #[test]
    fn the_ui_moving_the_part_moves_where_the_next_press_goes() {
        let mut s = show(&[NEXT]);
        s.set_part(Some(1));
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)));
        s.set_part(Some(7));
        cc(&mut s, 80, 0);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, OFF)), "a part the table no longer has counts as none");
    }
}
