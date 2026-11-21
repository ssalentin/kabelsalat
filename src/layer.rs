//! Velocity layers and floating splits of a rig zone (#276): what a setlist
//! part's rig takes besides its key range. A rig plays only note-ons struck
//! inside its velocity range, through its velocity curve, and the edges of
//! its key range follow the hands by up to `float` keys. Pure and fixed-size:
//! the engine keeps one [`Hands`] for all rigs and calls these on the audio
//! thread. Releases are never gated here, so nothing a layer started can hang.

use serde::{Deserialize, Serialize};

use crate::control::RigZone;

/// The most keys an edge of a key range floats: an octave.
pub const FLOAT_MAX: u8 = 12;

/// Floating edges [`Hands`] holds: both edges of every rig zone.
const MAX_EDGES: usize = 16;

/// How a rig turns the velocity a key was struck with into the velocity it
/// plays: `Soft` lifts quiet playing, `Hard` asks for a firmer touch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VelCurve {
    #[default]
    Linear,
    Soft,
    Hard,
}

impl VelCurve {
    pub const ALL: [VelCurve; 3] = [VelCurve::Linear, VelCurve::Soft, VelCurve::Hard];

    pub fn as_str(self) -> &'static str {
        match self {
            VelCurve::Linear => "linear",
            VelCurve::Soft => "soft",
            VelCurve::Hard => "hard",
        }
    }

    pub fn parse(name: &str) -> Result<Self, String> {
        let q = name.trim().to_lowercase();
        Self::ALL.into_iter().find(|c| c.as_str() == q)
            .ok_or_else(|| format!("unknown velocity curve '{name}' (expected 'linear', 'soft' or 'hard')"))
    }

    /// The curve after this one, for a chip that steps through them.
    pub fn next(self) -> Self {
        match self {
            VelCurve::Linear => VelCurve::Soft,
            VelCurve::Soft => VelCurve::Hard,
            VelCurve::Hard => VelCurve::Linear,
        }
    }

    /// A normalized velocity (0..=1) through the curve.
    pub fn shape(self, v: f64) -> f64 {
        let v = v.clamp(0.0, 1.0);
        match self {
            VelCurve::Linear => v,
            VelCurve::Soft => v.sqrt(),
            VelCurve::Hard => v * v,
        }
    }

    /// A 7-bit note-on velocity through the curve. A struck key never comes
    /// out at 0, which MIDI reads as a release.
    pub fn shape7(self, v: u8) -> u8 {
        if self == VelCurve::Linear || v == 0 {
            return v;
        }
        (self.shape(v as f64 / 127.0) * 127.0).round().clamp(1.0, 127.0) as u8
    }
}

/// Where the hands last played around one edge of a key range: the last
/// note-on below the edge and the last one at or above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Edge {
    below: Option<u8>,
    above: Option<u8>,
}

impl Edge {
    /// Does a note-on of `note` land at or above the edge whose lowest upper
    /// key is `at`? Within `float` keys of the edge the note goes to the
    /// other side's hand when that hand is itself within those keys and
    /// nearer than the hand on the note's own side: a run that crosses the
    /// edge by less than `float` keys stays with the hand that played it.
    fn above(&self, at: u8, float: u8, note: u8) -> bool {
        let (n, at, f) = (note as i16, at as i16, float as i16);
        let plain = n >= at;
        let band = at - f..at + f;
        if !band.contains(&n) {
            return plain;
        }
        let (own, other) = if plain { (self.above, self.below) } else { (self.below, self.above) };
        let dist = |h: u8| (h as i16 - n).abs();
        match other {
            Some(h) if band.contains(&(h as i16)) && own.is_none_or(|o| dist(h) < dist(o)) => !plain,
            _ => plain,
        }
    }

    fn play(&mut self, at: u8, float: u8, note: u8) {
        if self.above(at, float, note) {
            self.above = Some(note);
        } else {
            self.below = Some(note);
        }
    }
}

/// One floating edge: its lowest upper key, how far it floats, and where
/// the hands last played around it. `float` 0 marks a free slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct FloatEdge {
    at: u8,
    float: u8,
    hands: Edge,
}

/// Where the hands last played around every floating edge of the rig zones.
/// Rigs that split at the same key with the same float share one edge, so a
/// key near it plays on exactly one of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Hands {
    edges: [FloatEdge; MAX_EDGES],
}

/// The edges of a zone's key range that exist, by their lowest upper key.
fn edges_of(zone: &RigZone) -> impl Iterator<Item = u8> {
    [(zone.key_lo > 0).then_some(zone.key_lo), (zone.key_hi < 127).then(|| zone.key_hi + 1)].into_iter().flatten()
}

impl Hands {
    fn edge(&self, at: u8, float: u8) -> Edge {
        self.edges.iter().find(|e| e.float == float && e.at == at).map_or(Edge::default(), |e| e.hands)
    }

    /// Is a note-on of `played` inside the zone's key range, with its edges
    /// floated toward the hands?
    pub fn inside(&self, zone: &RigZone, played: u8) -> bool {
        let f = zone.float;
        let lo = zone.key_lo == 0 || self.edge(zone.key_lo, f).above(zone.key_lo, f, played);
        let hi = zone.key_hi >= 127 || !self.edge(zone.key_hi + 1, f).above(zone.key_hi + 1, f, played);
        lo && hi
    }

    /// A note-on of `played` reached a rig of `zone`: move the hands around
    /// its edges. A second rig with the same edge moves it again to the same
    /// place (the hand that took the note is then on it).
    pub fn play(&mut self, zone: &RigZone, played: u8) {
        let f = zone.float;
        if f == 0 {
            return;
        }
        for at in edges_of(zone) {
            let slot = self.edges.iter().position(|e| e.float == f && e.at == at)
                .or_else(|| self.edges.iter().position(|e| e.float == 0));
            if let Some(i) = slot {
                let e = &mut self.edges[i];
                (e.at, e.float) = (at, f);
                e.hands.play(at, f, played);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A split at C4 (60) with both sides floating `float` keys.
    fn split(float: u8) -> (RigZone, RigZone) {
        (RigZone { key_hi: 59, float, ..RigZone::OPEN }, RigZone { key_lo: 60, float, ..RigZone::OPEN })
    }

    /// Play `notes` one by one through both sides; which side took each.
    fn run(float: u8, notes: &[u8]) -> Vec<char> {
        let (low, high) = split(float);
        let mut hands = Hands::default();
        notes.iter().map(|&n| {
            let side = match (hands.inside(&low, n), hands.inside(&high, n)) {
                (true, false) => 'L',
                (false, true) => 'H',
                both => panic!("note {n} went to {both:?}: a split must give each note one side"),
            };
            hands.play(&low, n);
            hands.play(&high, n);
            side
        }).collect()
    }

    #[test]
    fn a_run_that_crosses_the_split_by_less_than_the_float_stays_on_one_side() {
        assert_eq!(run(4, &[55, 57, 59, 60, 61, 63]), "LLLLLL".chars().collect::<Vec<_>>(), "up from below");
        assert_eq!(run(4, &[65, 62, 60, 58, 56]), "HHHHH".chars().collect::<Vec<_>>(), "down from above");
        assert_eq!(run(4, &[59, 63, 64]), "LLH".chars().collect::<Vec<_>>(), "four keys past the edge is the other side");
        assert_eq!(run(0, &[59, 60, 61]), "LHH".chars().collect::<Vec<_>>(), "no float: a plain split");
    }

    #[test]
    fn the_left_hand_keeps_its_run_while_the_right_hand_plays_far_above() {
        // Left hand climbs past C4 while the right hand plays a chord at C5.
        assert_eq!(run(4, &[55, 72, 59, 76, 61, 72, 62]), "LHLHLHL".chars().collect::<Vec<_>>());
        // A hand far from the split never pulls a note across it.
        assert_eq!(run(4, &[40, 72, 58, 61]), "LHLL".chars().collect::<Vec<_>>());
    }

    #[test]
    fn a_note_between_both_hands_goes_to_the_nearer_one() {
        // Left hand at 58, right hand at 62: 61 is the right hand's, 59 the left's.
        assert_eq!(run(4, &[50, 64, 58, 62, 61, 59]), "LHLHHL".chars().collect::<Vec<_>>());
        // Left hand at 61 after its run, right hand at 64: 62 stays left.
        assert_eq!(run(4, &[59, 61, 64, 62]), "LLHL".chars().collect::<Vec<_>>());
    }

    #[test]
    fn an_open_edge_never_floats() {
        let open = RigZone { float: 4, ..RigZone::OPEN };
        let mut h = Hands::default();
        for n in [0, 3, 60, 124, 127] {
            assert!(h.inside(&open, n));
            h.play(&open, n);
        }
        assert_eq!(h, Hands::default(), "a range open at both ends has no edge to move");
    }

    #[test]
    fn a_rig_that_hears_only_one_side_still_shares_the_edge() {
        // Only the low rig hears the run (the high rig's cable is out), then
        // both hear again: they still agree which side each key is on.
        let (low, high) = split(4);
        let mut hands = Hands::default();
        for n in [57, 59, 61] {
            hands.play(&low, n);
        }
        for n in [62, 60, 58, 63, 64] {
            assert_ne!(hands.inside(&low, n), hands.inside(&high, n), "{n} plays on exactly one side");
            hands.play(&low, n);
            hands.play(&high, n);
        }
    }

    #[test]
    fn curves_keep_the_ends_and_bend_the_middle() {
        for c in VelCurve::ALL {
            assert_eq!(c.shape7(127), 127, "{c:?} keeps full velocity");
            assert_eq!(c.shape7(0), 0, "{c:?} keeps a release a release");
            assert!(c.shape7(1) >= 1, "{c:?} never turns a struck key into a release");
            assert_eq!(VelCurve::parse(c.as_str()), Ok(c));
        }
        assert_eq!(VelCurve::Linear.shape7(64), 64);
        assert!(VelCurve::Soft.shape7(64) > 85, "soft lifts a medium touch: {}", VelCurve::Soft.shape7(64));
        assert!(VelCurve::Hard.shape7(64) < 40, "hard lowers a medium touch: {}", VelCurve::Hard.shape7(64));
        assert!(VelCurve::parse("loud").is_err());
        assert_eq!(serde_json::to_value(VelCurve::Soft).unwrap(), serde_json::json!("soft"));
    }
}
