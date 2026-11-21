//! Show controls (#261): hardware MIDI messages the engine takes for a show
//! action instead of passing them to the instruments, and the setlist parts
//! the engine steps through itself, so a foot switch changes the zones in
//! the block its CC arrives in.
//!
//! The UI sends the table (`Command::SetShowControls`), every part's zones
//! (`SetShowParts`) and the active part (`SetShowPart`); the engine publishes
//! the part it stepped to (`GraphPub::show_part`, `show_steps`) and the UI
//! recalls the rest of that part (tempo, levels, knobs).

use serde::{Deserialize, Serialize};

use crate::control::{Command, RigZone};
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

/// What the engine does with a hardware message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Take {
    /// No show control takes it: route it as before.
    Route,
    /// A show control took it; nothing else happens.
    Consumed,
    /// A show control took it and the setlist moved to a part with these zones.
    Part([RigZone; 8]),
}

/// The engine's show state. Allocation-free: `set_parts` keeps the table it
/// is given and hands back the one it replaces.
#[derive(Default)]
pub struct Show {
    controls: ShowControls,
    /// Per slot: the control is down (its last value was 64 or above).
    down: [bool; MAX_SHOW_CONTROLS],
    /// Per slot: the next release still reaches the instruments, once. A
    /// control learned from a press, or bound while it was pressed, has
    /// reached them already; without its release a sustain pedal would stay
    /// down.
    release_owed: [bool; MAX_SHOW_CONTROLS],
    /// Every part's zones, in setlist order.
    parts: Vec<[RigZone; 8]>,
    /// The active part, as an index into `parts`.
    part: Option<usize>,
    /// Parts the engine stepped to itself.
    steps: u64,
}

impl Show {
    /// Replace the table. A slot whose control changed starts up, and owes
    /// its next release to the instruments.
    pub fn set_controls(&mut self, controls: ShowControls) {
        for (i, c) in controls.iter().enumerate() {
            if *c != self.controls[i] {
                self.down[i] = false;
                self.release_owed[i] = c.is_some();
            }
        }
        self.controls = controls;
    }

    pub fn set_parts(&mut self, parts: Vec<[RigZone; 8]>) -> Vec<[RigZone; 8]> {
        std::mem::replace(&mut self.parts, parts)
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
        let Some(i) = self.controls.iter().position(|c| c.is_some_and(|c| c.takes(device, channel, msg))) else {
            return Take::Route;
        };
        let down = value >= 64;
        let pressed = down && !self.down[i];
        self.down[i] = down;
        if !down && std::mem::take(&mut self.release_owed[i]) {
            return Take::Route;
        }
        if !pressed {
            return Take::Consumed;
        }
        match self.controls[i].map(|c| c.action) {
            Some(ShowAction::NextPart) => self.step(1),
            Some(ShowAction::PrevPart) => self.step(-1),
            None => Take::Consumed,
        }
    }

    /// Move by `by` parts and stop at either end. With no part active, any
    /// step goes to the first part (as `Setlist::next_part` does).
    fn step(&mut self, by: isize) -> Take {
        let Some(last) = self.parts.len().checked_sub(1) else { return Take::Consumed };
        let to = match self.part {
            None => 0,
            Some(p) => p.min(last).saturating_add_signed(by).min(last),
        };
        if self.part == Some(to) {
            return Take::Consumed;
        }
        self.part = Some(to);
        self.steps += 1;
        Take::Part(self.parts[to])
    }
}

/// What a frontend last sent the engine of its setlist, so a frame sends
/// only what changed.
#[derive(Default)]
pub struct ShowSent {
    controls: ShowControls,
    parts: Vec<[RigZone; 8]>,
    /// The active part as the engine knows it. Set it when the engine moved
    /// the part itself, so the frontend does not send that move back.
    pub part: Option<usize>,
}

impl ShowSent {
    /// Send what changed; a command `send` refuses (bus full) is sent again
    /// next time.
    pub fn publish(&mut self, setlist: &crate::setlist::Setlist, mut send: impl FnMut(Command) -> bool) {
        let controls = setlist.show_controls();
        if controls != self.controls && send(Command::SetShowControls { controls }) {
            self.controls = controls;
        }
        let parts = setlist.part_zones();
        if parts != self.parts && send(Command::SetShowParts { parts: parts.clone() }) {
            self.parts = parts;
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

    fn part(a: RigZone, b: RigZone) -> [RigZone; 8] {
        let mut z = [RigZone::OPEN; 8];
        z[0] = a;
        z[1] = b;
        z
    }

    fn show(controls: &[ShowControl]) -> Show {
        let mut s = Show::default();
        let mut t = NO_SHOW_CONTROLS;
        for (slot, c) in t.iter_mut().zip(controls) {
            *slot = Some(*c);
        }
        s.set_controls(t);
        s.set_parts(vec![part(RigZone::OPEN, OFF), part(OFF, RigZone::OPEN), part(RigZone::OPEN, RigZone::OPEN)]);
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
        assert_eq!(cc(&mut s, 80, 0), Take::Route, "the first release after binding is owed");
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed, "later releases are the switch's own");
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed);
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed, "at the last part");
        assert_eq!((s.part(), s.steps()), (Some(2), 3));
        assert_eq!(cc(&mut s, 81, 127), Take::Part(part(OFF, RigZone::OPEN)));
        assert_eq!(cc(&mut s, 81, 0), Take::Route);
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
        s.set_parts(Vec::new());
        assert_eq!(cc(&mut s, 80, 127), Take::Consumed);
        assert_eq!(s.part(), None);
    }

    #[test]
    fn rebinding_a_slot_owes_one_release_and_an_unchanged_slot_keeps_its_state() {
        let mut s = show(&[NEXT, PREV]);
        cc(&mut s, 80, 127);
        cc(&mut s, 80, 0);
        cc(&mut s, 81, 127);
        let mut t = NO_SHOW_CONTROLS;
        t[0] = Some(NEXT);
        t[1] = Some(ShowControl { msg: ShowMsg::Cc(82), ..PREV });
        s.set_controls(t);
        assert_eq!(cc(&mut s, 80, 0), Take::Consumed, "NEXT was not rebound: its release was paid");
        assert_eq!(cc(&mut s, 82, 0), Take::Route, "the new PREV owes one");
        assert_eq!(cc(&mut s, 81, 0), Take::Route, "CC 81 is no show control any more");
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
        ] if controls[0].is_some_and(|c| c.msg == ShowMsg::Cc(80) && c.action == ShowAction::NextPart) && parts.len() == 2));
        out.clear();
        sent.publish(&sl, |c| {
            out.push(c);
            true
        });
        assert!(out.is_empty(), "nothing changed, nothing sent");
        sl.select(0, 0);
        sent.publish(&sl, |_| false);
        sent.publish(&sl, |c| {
            out.push(c);
            true
        });
        assert!(matches!(&out[..], [Command::SetShowPart { part: Some(0) }]), "the refused move is sent again");
    }

    #[test]
    fn the_ui_moving_the_part_moves_where_the_next_press_goes() {
        let mut s = show(&[NEXT]);
        s.set_part(Some(1));
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)));
        s.set_part(Some(7));
        cc(&mut s, 80, 0);
        assert_eq!(cc(&mut s, 80, 127), Take::Part(part(RigZone::OPEN, RigZone::OPEN)), "a stale part past the end clamps");
    }
}
