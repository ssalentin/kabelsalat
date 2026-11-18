//! MIDI matrix (task-9): any source → any destination with channel filter,
//! transpose, key/velocity ranges (splits & layers), CC mapping.
//! Owned by the matrix workstream.
//!
//! Pure and deterministic: a `Matrix` is a routing table of `Rule`s, and
//! [`Matrix::route`] is a plain function of (source, event) — no state, no
//! locks, no I/O. The engine (lead's integration step) calls it per incoming
//! event and schedules the returned `(dest, event)` pairs sample-accurately.
//!
//! Semantics (see docs/midi-matrix.md for the full contract):
//! - One source feeding many rules = LAYERS; disjoint key ranges = SPLITS.
//! - Key ranges filter the *played* note (before transpose), inclusive.
//! - Transpose clamps to 0..=127, never wraps.
//! - Velocity ranges filter sounding note-ons only; note-offs always pass so
//!   notes never hang. Velocity-0 note-ons are normalized to note-offs.
//! - Channel filter: `None` = omni, `Some(0..=15)` = that channel only;
//!   the event's channel is preserved on output.
//! - CC map: empty = all CCs pass; non-empty = whitelist of mapped
//!   controllers, forwarded unchanged. The cc→param binding lives in the
//!   rule ([`Rule::param_for_cc`]) for the destination node to apply.
//!
//! RT discipline: [`Matrix::route_into`] reuses a caller-reserved output
//! buffer, so steady-state routing never allocates. Rule edits happen
//! off-thread; the engine swaps the table at block boundaries (docs/ARCHITECTURE.md §7).

// The engine consumes this module during integration; until then the public
// API is exercised by the unit tests only. Remove once the engine wires it in.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::control::{NodeId, ParamId};

/// Rule ids are stable handles for UI/MCP edits (like `EdgeId` in the graph).
pub type RuleId = u32;

// Channel-voice status nibbles (MIDI wire format, upper nibble of byte 0).
pub const NOTE_OFF: u8 = 0x80;
pub const NOTE_ON: u8 = 0x90;
pub const POLY_AFTERTOUCH: u8 = 0xA0;
pub const CONTROL_CHANGE: u8 = 0xB0;
pub const PROGRAM_CHANGE: u8 = 0xC0;
pub const CHANNEL_AFTERTOUCH: u8 = 0xD0;
pub const PITCH_BEND: u8 = 0xE0;

/// A channel-voice MIDI message: small, `Copy`, fixed-size — safe to move
/// through rings and route without allocation. `status` is the message type
/// (upper nibble, e.g. `NOTE_ON`), `channel` is 0-based (0..=15, i.e. MIDI
/// channels 1..=16), `data` holds the two 7-bit data bytes.
///
/// System messages (clock, sysex, …) are out of P1 scope and not modeled;
/// [`MidiMsg::from_bytes`] rejects them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiMsg {
    pub status: u8,
    pub channel: u8,
    pub data: [u8; 2],
}

impl MidiMsg {
    /// Generic constructor; channel and data bytes are masked to 7/4 bits.
    pub fn new(status: u8, channel: u8, d1: u8, d2: u8) -> Self {
        Self {
            status: status & 0xF0,
            channel: channel & 0x0F,
            data: [d1 & 0x7F, d2 & 0x7F],
        }
    }

    pub fn note_on(channel: u8, note: u8, velocity: u8) -> Self {
        Self::new(NOTE_ON, channel, note, velocity)
    }

    pub fn note_off(channel: u8, note: u8, velocity: u8) -> Self {
        Self::new(NOTE_OFF, channel, note, velocity)
    }

    pub fn cc(channel: u8, controller: u8, value: u8) -> Self {
        Self::new(CONTROL_CHANGE, channel, controller, value)
    }

    pub fn program_change(channel: u8, program: u8) -> Self {
        Self::new(PROGRAM_CHANGE, channel, program, 0)
    }

    pub fn channel_aftertouch(channel: u8, pressure: u8) -> Self {
        Self::new(CHANNEL_AFTERTOUCH, channel, pressure, 0)
    }

    pub fn poly_aftertouch(channel: u8, note: u8, pressure: u8) -> Self {
        Self::new(POLY_AFTERTOUCH, channel, note, pressure)
    }

    /// Pitch bend from a signed value (-8192..=8191, 0 = center), clamped.
    pub fn pitch_bend(channel: u8, value: i16) -> Self {
        let v = (value.clamp(-8192, 8191) as i32 + 8192) as u16;
        Self::new(PITCH_BEND, channel, (v & 0x7F) as u8, ((v >> 7) & 0x7F) as u8)
    }

    /// The wire status byte (type | channel).
    pub fn status_byte(&self) -> u8 {
        self.status | (self.channel & 0x0F)
    }

    /// A sounding note-on (velocity > 0). Velocity-0 note-ons are note-offs.
    pub fn is_note_on(&self) -> bool {
        self.status == NOTE_ON && self.data[1] > 0
    }

    /// Note-off, including the velocity-0 note-on convention.
    pub fn is_note_off(&self) -> bool {
        self.status == NOTE_OFF || (self.status == NOTE_ON && self.data[1] == 0)
    }

    pub fn note(&self) -> u8 {
        self.data[0]
    }

    pub fn velocity(&self) -> u8 {
        self.data[1]
    }

    /// Signed pitch-bend value (-8192..=8191, 0 = center).
    pub fn pitch_bend_value(&self) -> i16 {
        ((self.data[1] as i16) << 7 | self.data[0] as i16) - 8192
    }

    /// Decode raw MIDI bytes (as delivered by midir / the midi.rs ring).
    /// Accepts channel-voice messages only; returns `None` for system
    /// messages, running status, or truncated input.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        let status = bytes[0];
        if status < 0x80 || status >= 0xF0 {
            return None; // data byte or system message
        }
        let kind = status & 0xF0;
        let two_byte = matches!(kind, PROGRAM_CHANGE | CHANNEL_AFTERTOUCH);
        if !two_byte && bytes.len() < 3 {
            return None;
        }
        if bytes[1] & 0x80 != 0 || (!two_byte && bytes[2] & 0x80 != 0) {
            return None;
        }
        Some(Self {
            status: kind,
            channel: status & 0x0F,
            data: [bytes[1], if two_byte { 0 } else { bytes[2] }],
        })
    }
}

/// Where an event came from. Ids are the engine's port/source handles
/// (ALSA-seq port numbers for hardware/virtual ports); the matrix itself
/// never resolves them — it only matches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceId {
    /// Hardware MIDI input (ALSA-seq port id).
    Hardware(u32),
    /// Virtual MIDI input (e.g. the `kabelsalat-in` port).
    Virtual(u32),
    /// The computer keyboard: on-screen key clicks AND laptop typing are one
    /// source. `LaptopKeys` (the old separate laptop source) still
    /// deserializes, as `Keys`, so old sessions/rules keep loading.
    #[serde(alias = "LaptopKeys")]
    Keys,
    /// File player (P2) — one id per player node.
    FilePlayer(u32),
}

/// Which parameter space a CC binding drives on the destination node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CcTarget {
    /// Plugin / node native parameter (CLAP param id, LV2 control index, or DSP/Master param).
    #[default]
    #[serde(alias = "plugin", alias = "plugin_param")]
    PluginParam,
    /// Rack unit knob (`Command::SetParam` semantics: e.g. CLAP voice `P_LEVEL`, DSP engine param, Master vol/warm).
    #[serde(alias = "knob")]
    Knob,
}

impl CcTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            CcTarget::PluginParam => "plugin",
            CcTarget::Knob => "knob",
        }
    }
}

/// How a controller value becomes the parameter value. Instruments send
/// switches and stepped controls (an LED drawbar, a slow/fast button) as
/// `steps` equal sections of 0..=127; each section lands on one value spread
/// evenly from `lo` to `hi` (7-bit units, so 64 is the middle of a knob).
/// `steps` 0 is continuous: `lo..=hi` scaled linearly. The default is the
/// identity, which MIDI learn uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CcScale {
    pub steps: u8,
    pub lo: u8,
    pub hi: u8,
}

impl Default for CcScale {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl CcScale {
    pub const IDENTITY: CcScale = CcScale { steps: 0, lo: 0, hi: 127 };

    /// `steps` sections of the controller range onto `lo..=hi`.
    pub const fn stepped(steps: u8, lo: u8, hi: u8) -> Self {
        CcScale { steps, lo, hi }
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// The normalized (0..=1) parameter value of a controller value.
    pub fn norm(&self, value: u8) -> f64 {
        let v = (value & 0x7F) as f64;
        let (lo, hi) = (self.lo.min(127) as f64, self.hi.min(127) as f64);
        let t = if self.steps >= 2 {
            let section = ((value & 0x7F) as u32 * self.steps as u32 / 128).min(self.steps as u32 - 1);
            section as f64 / (self.steps - 1) as f64
        } else {
            v / 127.0
        };
        (lo + (hi - lo) * t) / 127.0
    }
}

/// CC binding: controller number → parameter on the rule's destination node.
/// MIDI-learn fills these; the matrix routes the CC and the destination applies
/// the binding according to `target`, with the value shaped by `scale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcMapping {
    pub cc: u8,
    pub param: ParamId,
    #[serde(default)]
    pub target: CcTarget,
    #[serde(default, skip_serializing_if = "CcScale::is_identity")]
    pub scale: CcScale,
}

/// One routing rule: source → destination plus per-rule transforms.
/// Build with [`Rule::new`] and the chainable setters, then hand it to
/// [`Matrix::add_rule`] (which assigns the id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// Assigned by `Matrix::add_rule`; stable handle for edits.
    pub id: RuleId,
    pub source: SourceId,
    pub dest: NodeId,
    /// `None` = omni (any channel); `Some(ch)` = only that 0-based channel.
    pub channel: Option<u8>,
    /// Semitones added to note numbers; result clamps to 0..=127.
    pub transpose: i8,
    /// Inclusive played-note range (lo, hi) — SPLITS. Default (0, 127).
    pub key_range: (u8, u8),
    /// Inclusive velocity range (lo, hi) for sounding note-ons. Default
    /// (0, 127). Note-offs bypass this filter.
    pub vel_range: (u8, u8),
    /// CC whitelist + bindings. Empty = all CCs pass through.
    #[serde(default)]
    pub cc_map: Vec<CcMapping>,
    /// Muted rules route nothing (live performance toggle).
    pub enabled: bool,
    /// Created only to carry CC bindings (MIDI-learn on an unrouted pair):
    /// routes controller messages, never notes, until the rule is shaped.
    #[serde(default)]
    pub cc_only: bool,
}

impl Rule {
    /// A transparent rule: omni, no transpose, full ranges, no CC map.
    pub fn new(source: SourceId, dest: NodeId) -> Self {
        Self {
            id: 0,
            source,
            dest,
            channel: None,
            transpose: 0,
            key_range: (0, 127),
            vel_range: (0, 127),
            cc_map: Vec::new(),
            enabled: true,
            cc_only: false,
        }
    }

    /// Restrict to one 0-based channel (values wrap into 0..=15).
    pub fn on_channel(mut self, channel: u8) -> Self {
        self.channel = Some(channel & 0x0F);
        self
    }

    /// Accept any channel (the default).
    pub fn omni(mut self) -> Self {
        self.channel = None;
        self
    }

    pub fn transpose(mut self, semitones: i8) -> Self {
        self.transpose = semitones;
        self
    }

    /// Inclusive played-note range; reversed bounds are normalized.
    pub fn key_range(mut self, lo: u8, hi: u8) -> Self {
        self.key_range = if lo <= hi { (lo, hi) } else { (hi, lo) };
        self
    }

    /// Inclusive velocity range for sounding note-ons; reversed bounds are
    /// normalized.
    pub fn vel_range(mut self, lo: u8, hi: u8) -> Self {
        self.vel_range = if lo <= hi { (lo, hi) } else { (hi, lo) };
        self
    }

    /// Bind a controller to a destination plugin parameter. Re-mapping the same
    /// controller replaces its binding. A non-empty map acts as a CC
    /// whitelist: unmapped controllers are dropped.
    pub fn map_cc(self, cc: u8, param: ParamId) -> Self {
        self.map_cc_target(cc, param, CcTarget::PluginParam)
    }

    /// Bind a controller to a destination parameter with an explicit target space.
    pub fn map_cc_target(mut self, cc: u8, param: ParamId, target: CcTarget) -> Self {
        self.set_cc_target(cc, param, target);
        self
    }

    /// Add or replace a CC binding (PluginParam target) in place. Returns true if the map changed.
    pub fn set_cc(&mut self, cc: u8, param: ParamId) -> bool {
        self.set_cc_target(cc, param, CcTarget::PluginParam)
    }

    /// Add or replace a CC binding in place with an explicit target. Returns true if the map changed.
    pub fn set_cc_target(&mut self, cc: u8, param: ParamId, target: CcTarget) -> bool {
        self.set_cc_mapping(CcMapping { cc, param, target, scale: CcScale::IDENTITY })
    }

    /// Add or replace the binding of `m.cc` in place. Returns true if the map changed.
    pub fn set_cc_mapping(&mut self, m: CcMapping) -> bool {
        let m = CcMapping { cc: m.cc & 0x7F, ..m };
        match self.cc_map.iter_mut().find(|x| x.cc == m.cc) {
            Some(x) if *x == m => false,
            Some(x) => {
                *x = m;
                true
            }
            None => {
                self.cc_map.push(m);
                true
            }
        }
    }

    /// Remove a CC binding in place. Returns true if one existed.
    pub fn unmap_cc(&mut self, cc: u8) -> bool {
        let cc = cc & 0x7F;
        let before = self.cc_map.len();
        self.cc_map.retain(|m| m.cc != cc);
        self.cc_map.len() < before
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    /// The full CC binding for a controller, if any.
    pub fn mapping_for_cc(&self, cc: u8) -> Option<CcMapping> {
        self.cc_map.iter().find(|m| m.cc == cc).copied()
    }

    /// The parameter a controller is bound to, if any.
    pub fn param_for_cc(&self, cc: u8) -> Option<ParamId> {
        self.mapping_for_cc(cc).map(|m| m.param)
    }

    fn accepts_channel(&self, channel: u8) -> bool {
        self.channel.map_or(true, |ch| ch == channel)
    }

    fn in_key_range(&self, note: u8) -> bool {
        note >= self.key_range.0 && note <= self.key_range.1
    }

    fn in_vel_range(&self, velocity: u8) -> bool {
        velocity >= self.vel_range.0 && velocity <= self.vel_range.1
    }

    /// Apply this rule's filters and transforms to one event.
    /// `None` = the event is dropped for this rule's destination.
    /// Pure: depends only on the rule and the event.
    pub fn transform(&self, ev: MidiMsg) -> Option<MidiMsg> {
        if !self.enabled || !self.accepts_channel(ev.channel) {
            return None;
        }
        if self.cc_only && ev.status != CONTROL_CHANGE {
            return None;
        }
        match ev.status {
            NOTE_ON | NOTE_OFF => {
                // Splits partition the *played* keyboard: filter before
                // transposing, so zone boundaries don't move with transpose.
                if !self.in_key_range(ev.data[0]) {
                    return None;
                }
                let mut out = ev;
                out.data[0] = transpose_note(ev.data[0], self.transpose);
                if ev.is_note_on() {
                    if !self.in_vel_range(ev.data[1]) {
                        return None;
                    }
                } else {
                    // Note-off (incl. velocity-0 note-on): never velocity-
                    // filtered, and normalized to NOTE_OFF, so releases
                    // always reach whatever was triggered.
                    out.status = NOTE_OFF;
                }
                Some(out)
            }
            CONTROL_CHANGE => {
                if !self.cc_map.is_empty() && self.param_for_cc(ev.data[0]).is_none() {
                    return None;
                }
                Some(ev)
            }
            // Pitch bend, program change, aftertouch: channel filter only.
            _ => Some(ev),
        }
    }
}

/// Transpose a note number, clamping to the MIDI range 0..=127 (never wraps:
/// wrapping would jump octaves and scramble splits).
pub fn transpose_note(note: u8, semitones: i8) -> u8 {
    (note as i16 + semitones as i16).clamp(0, 127) as u8
}

/// The routing table. Rules are evaluated in insertion order, so routing is
/// deterministic and layer ordering is stable. One source may feed any
/// number of rules (LAYERS); disjoint key ranges across rules make SPLITS.
///
/// Serializable for sessions (task-8), mirroring the control-plane graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Matrix {
    pub rules: Vec<Rule>,
    next_id: RuleId,
}

impl Matrix {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve rule storage so block-boundary edits within capacity never
    /// allocate (docs/ARCHITECTURE.md §7 discipline, same as `Graph::reserve`).
    pub fn reserve(&mut self, rules: usize) {
        self.rules.reserve(rules);
    }

    /// Add a rule; assigns and returns its stable id.
    pub fn add_rule(&mut self, mut rule: Rule) -> RuleId {
        rule.id = self.next_id;
        self.next_id += 1;
        let id = rule.id;
        self.rules.push(rule);
        id
    }

    /// Convenience: add a transparent rule (omni, full ranges) and return
    /// its id for further editing via [`Matrix::rule_mut`].
    pub fn connect(&mut self, source: SourceId, dest: NodeId) -> RuleId {
        self.add_rule(Rule::new(source, dest))
    }

    pub fn rule(&self, id: RuleId) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    pub fn rule_mut(&mut self, id: RuleId) -> Option<&mut Rule> {
        self.rules.iter_mut().find(|r| r.id == id)
    }

    pub fn remove_rule(&mut self, id: RuleId) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() < before
    }

    /// All rules fed by a source, in insertion order. The engine uses this
    /// to look up CC bindings (`param_for_cc`) for delivered events.
    pub fn rules_from(&self, source: SourceId) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(move |r| r.source == source)
    }

    /// Route one event from `source`: every matching rule contributes its
    /// transformed event, in rule insertion order. Allocates the result
    /// vector; on the audio thread prefer [`Matrix::route_into`].
    pub fn route(&self, source: SourceId, event: MidiMsg) -> Vec<(NodeId, MidiMsg)> {
        let mut out = Vec::new();
        self.route_into(source, event, &mut out);
        out
    }

    /// Allocation-free variant: clears `out` and appends `(dest, event)`
    /// pairs. With `out` reserved up front, steady-state routing never
    /// allocates. Same ordering and semantics as [`Matrix::route`].
    pub fn route_into(&self, source: SourceId, event: MidiMsg, out: &mut Vec<(NodeId, MidiMsg)>) {
        out.clear();
        for rule in &self.rules {
            if rule.source == source {
                if let Some(transformed) = rule.transform(event) {
                    out.push((rule.dest, transformed));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURGE_A: NodeId = 1;
    const SURGE_B: NodeId = 2;
    const PAD: NodeId = 3;

    fn note_on(note: u8) -> MidiMsg {
        MidiMsg::note_on(0, note, 100)
    }

    fn dests(out: &[(NodeId, MidiMsg)]) -> Vec<NodeId> {
        out.iter().map(|(d, _)| *d).collect()
    }

    // --- SPLITS -----------------------------------------------------------

    #[test]
    fn split_at_c4_two_destinations() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59)); // below C4
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127)); // C4 and up

        // C4 = 60 goes to the upper zone; 59 (B3) to the lower.
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(59))), vec![SURGE_A]);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(60))), vec![SURGE_B]);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(0))), vec![SURGE_A]);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(127))), vec![SURGE_B]);
        // Inclusive bounds: nothing falls through the crack at the boundary.
        for note in 0..=127u8 {
            assert_eq!(m.route(SourceId::Keys, note_on(note)).len(), 1);
        }
    }

    #[test]
    fn split_note_off_follows_split() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59));
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127));
        let off = MidiMsg::note_off(0, 60, 64);
        let out = m.route(SourceId::Keys, off);
        assert_eq!(out, vec![(SURGE_B, off)]);
        let off_low = MidiMsg::note_off(0, 48, 0);
        assert_eq!(dests(&m.route(SourceId::Keys, off_low)), vec![SURGE_A]);
    }

    // --- LAYERS -----------------------------------------------------------

    #[test]
    fn layer_one_key_two_dests() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        m.connect(SourceId::Keys, SURGE_B);
        let ev = note_on(64);
        let out = m.route(SourceId::Keys, ev);
        assert_eq!(out, vec![(SURGE_A, ev), (SURGE_B, ev)]);
    }

    #[test]
    fn layer_order_is_insertion_order() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_B);
        m.connect(SourceId::Keys, SURGE_A);
        m.connect(SourceId::Keys, PAD);
        assert_eq!(
            dests(&m.route(SourceId::Keys, note_on(60))),
            vec![SURGE_B, SURGE_A, PAD]
        );
    }

    #[test]
    fn split_plus_layer_three_rules() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59)); // bass split
        m.add_rule(Rule::new(SourceId::Keys, PAD)); // pad layer, full range
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127)); // lead split
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(48))), vec![SURGE_A, PAD]);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(72))), vec![PAD, SURGE_B]);
    }

    // --- TRANSPOSE --------------------------------------------------------

    #[test]
    fn transpose_up_and_down() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).transpose(12));
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).transpose(-12));
        let out = m.route(SourceId::Keys, note_on(60));
        assert_eq!(out, vec![(SURGE_A, note_on(72)), (SURGE_B, note_on(48))]);
        // Note-offs are transposed identically so releases match.
        let out = m.route(SourceId::Keys, MidiMsg::note_off(0, 60, 0));
        assert_eq!(
            out,
            vec![
                (SURGE_A, MidiMsg::note_off(0, 72, 0)),
                (SURGE_B, MidiMsg::note_off(0, 48, 0))
            ]
        );
    }

    #[test]
    fn transpose_clamps_high() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).transpose(12));
        assert_eq!(m.route(SourceId::Keys, note_on(120))[0].1.note(), 127);
        assert_eq!(m.route(SourceId::Keys, note_on(127))[0].1.note(), 127);
        assert_eq!(transpose_note(125, 12), 127); // no wrap to 5
    }

    #[test]
    fn transpose_clamps_low() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).transpose(-12));
        assert_eq!(m.route(SourceId::Keys, note_on(5))[0].1.note(), 0);
        assert_eq!(m.route(SourceId::Keys, note_on(0))[0].1.note(), 0);
        assert_eq!(transpose_note(2, -12), 0); // no wrap to 122
    }

    #[test]
    fn transpose_extremes() {
        assert_eq!(transpose_note(60, i8::MAX), 127);
        assert_eq!(transpose_note(60, i8::MIN), 0);
        assert_eq!(transpose_note(0, i8::MIN), 0);
        assert_eq!(transpose_note(127, i8::MAX), 127);
    }

    #[test]
    fn key_range_filters_before_transpose() {
        let mut m = Matrix::new();
        // Bass zone 0..59 shifted up an octave: zone boundaries stay put.
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59).transpose(12));
        let out = m.route(SourceId::Keys, note_on(59));
        assert_eq!(out, vec![(SURGE_A, note_on(71))]);
        assert!(m.route(SourceId::Keys, note_on(60)).is_empty());
    }

    // --- CHANNEL FILTER ---------------------------------------------------

    #[test]
    fn channel_filter_passes_matching_only() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).on_channel(2));
        assert_eq!(dests(&m.route(SourceId::Keys, MidiMsg::note_on(2, 60, 100))), vec![SURGE_A]);
        assert!(m.route(SourceId::Keys, MidiMsg::note_on(3, 60, 100)).is_empty());
        assert!(m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 100)).is_empty());
        // CCs and bends are channel-filtered too.
        assert!(m.route(SourceId::Keys, MidiMsg::cc(1, 7, 100)).is_empty());
        assert_eq!(dests(&m.route(SourceId::Keys, MidiMsg::cc(2, 7, 100))), vec![SURGE_A]);
    }

    #[test]
    fn omni_passes_all_channels_and_preserves_channel() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        for ch in 0..=15u8 {
            let out = m.route(SourceId::Keys, MidiMsg::note_on(ch, 60, 100));
            assert_eq!(out, vec![(SURGE_A, MidiMsg::note_on(ch, 60, 100))]);
        }
    }

    #[test]
    fn channel_setter_masks_to_4_bits() {
        let r = Rule::new(SourceId::Keys, SURGE_A).on_channel(18); // 18 & 0x0F = 2
        assert_eq!(r.channel, Some(2));
        let ev = MidiMsg::new(NOTE_ON, 18, 60, 100); // channel wraps to 2
        assert_eq!(ev.channel, 2);
        assert!(r.transform(ev).is_some());
    }

    // --- VELOCITY RANGE ---------------------------------------------------

    #[test]
    fn velocity_range_inclusive() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).vel_range(64, 127));
        assert_eq!(dests(&m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 64))), vec![SURGE_A]);
        assert_eq!(dests(&m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 127))), vec![SURGE_A]);
        assert!(m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 63)).is_empty());
        assert!(m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 1)).is_empty());
    }

    #[test]
    fn note_off_bypasses_velocity_filter() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).vel_range(64, 127));
        // A soft release must still get through (no stuck notes).
        let off = MidiMsg::note_off(0, 60, 10);
        assert_eq!(m.route(SourceId::Keys, off), vec![(SURGE_A, off)]);
    }

    #[test]
    fn zero_velocity_note_on_becomes_note_off() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).vel_range(64, 127));
        let ev = MidiMsg::note_on(0, 60, 0);
        assert!(ev.is_note_off());
        let out = m.route(SourceId::Keys, ev);
        assert_eq!(out, vec![(SURGE_A, MidiMsg::note_off(0, 60, 0))]);
    }

    // --- CC MAPPING -------------------------------------------------------

    #[test]
    fn cc_mapping_whitelist_passes_mapped() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).map_cc(1, 5).map_cc(7, 0));
        let mod_wheel = MidiMsg::cc(0, 1, 90);
        let volume = MidiMsg::cc(0, 7, 100);
        // Forwarded unchanged; the binding is applied by the destination.
        assert_eq!(m.route(SourceId::Keys, mod_wheel), vec![(SURGE_A, mod_wheel)]);
        assert_eq!(m.route(SourceId::Keys, volume), vec![(SURGE_A, volume)]);
    }

    #[test]
    fn cc_mapping_drops_unmapped() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).map_cc(1, 5));
        assert!(m.route(SourceId::Keys, MidiMsg::cc(0, 2, 90)).is_empty());
        assert!(m.route(SourceId::Keys, MidiMsg::cc(0, 64, 127)).is_empty());
    }

    #[test]
    fn cc_empty_map_is_transparent() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        for cc in [0u8, 1, 7, 64, 127] {
            let ev = MidiMsg::cc(0, cc, 42);
            assert_eq!(m.route(SourceId::Keys, ev), vec![(SURGE_A, ev)]);
        }
    }

    #[test]
    fn cc_binding_lookup_and_remap() {
        let r = Rule::new(SourceId::Keys, SURGE_A).map_cc(1, 5).map_cc(7, 0);
        assert_eq!(r.param_for_cc(1), Some(5));
        assert_eq!(r.param_for_cc(7), Some(0));
        assert_eq!(r.param_for_cc(2), None);
        let r = r.map_cc(1, 9); // re-bind replaces, not duplicates
        assert_eq!(r.param_for_cc(1), Some(9));
        assert_eq!(r.cc_map.len(), 2);
    }

    #[test]
    fn cc_ignores_key_and_velocity_ranges() {
        let mut m = Matrix::new();
        m.add_rule(
            Rule::new(SourceId::Keys, SURGE_A)
                .key_range(60, 72)
                .vel_range(100, 127)
                .map_cc(1, 5),
        );
        let ev = MidiMsg::cc(0, 1, 10);
        assert_eq!(m.route(SourceId::Keys, ev), vec![(SURGE_A, ev)]);
    }

    // --- OTHER CHANNEL VOICE ----------------------------------------------

    #[test]
    fn pitch_bend_passes_untransformed() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).transpose(12).key_range(60, 72));
        let bend = MidiMsg::pitch_bend(0, -1234);
        assert_eq!(m.route(SourceId::Keys, bend), vec![(SURGE_A, bend)]);
    }

    #[test]
    fn program_and_aftertouch_pass() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        let pc = MidiMsg::program_change(0, 42);
        let cat = MidiMsg::channel_aftertouch(0, 90);
        let pat = MidiMsg::poly_aftertouch(0, 60, 90);
        assert_eq!(m.route(SourceId::Keys, pc), vec![(SURGE_A, pc)]);
        assert_eq!(m.route(SourceId::Keys, cat), vec![(SURGE_A, cat)]);
        assert_eq!(m.route(SourceId::Keys, pat), vec![(SURGE_A, pat)]);
    }

    #[test]
    fn pitch_bend_encoding_roundtrip_and_clamp() {
        for v in [-8192i16, -1, 0, 1, 8191] {
            assert_eq!(MidiMsg::pitch_bend(0, v).pitch_bend_value(), v);
        }
        assert_eq!(MidiMsg::pitch_bend(0, i16::MAX).pitch_bend_value(), 8191);
        assert_eq!(MidiMsg::pitch_bend(0, i16::MIN).pitch_bend_value(), -8192);
        assert_eq!(MidiMsg::pitch_bend(0, 0).data, [0, 64]); // center = 0x2000
    }

    // --- RULE MANAGEMENT --------------------------------------------------

    #[test]
    fn disabled_rule_is_skipped_and_reenable_works() {
        let mut m = Matrix::new();
        let a = m.add_rule(Rule::new(SourceId::Keys, SURGE_A).enabled(false));
        m.connect(SourceId::Keys, SURGE_B);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(60))), vec![SURGE_B]);
        m.rule_mut(a).unwrap().enabled = true;
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(60))), vec![SURGE_A, SURGE_B]);
    }

    #[test]
    fn rule_ids_unique_and_remove() {
        let mut m = Matrix::new();
        let a = m.connect(SourceId::Keys, SURGE_A);
        let b = m.connect(SourceId::Keys, SURGE_B);
        assert_ne!(a, b);
        assert!(m.remove_rule(a));
        assert!(!m.remove_rule(a)); // already gone
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(60))), vec![SURGE_B]);
        // Ids are never reused.
        let c = m.connect(SourceId::Keys, PAD);
        assert!(c > b);
    }

    #[test]
    fn rule_mut_live_edit() {
        let mut m = Matrix::new();
        let id = m.connect(SourceId::Keys, SURGE_A);
        assert_eq!(m.route(SourceId::Keys, note_on(60))[0].1.note(), 60);
        {
            let r = m.rule_mut(id).unwrap();
            r.transpose = 7;
            r.channel = Some(1);
        }
        assert!(m.route(SourceId::Keys, note_on(60)).is_empty()); // wrong channel now
        let out = m.route(SourceId::Keys, MidiMsg::note_on(1, 60, 100));
        assert_eq!(out, vec![(SURGE_A, MidiMsg::note_on(1, 67, 100))]);
        assert!(m.rule_mut(999).is_none());
    }

    #[test]
    fn reversed_ranges_are_normalized() {
        let r = Rule::new(SourceId::Keys, SURGE_A).key_range(80, 20).vel_range(120, 40);
        assert_eq!(r.key_range, (20, 80));
        assert_eq!(r.vel_range, (40, 120));
    }

    // --- SOURCE MATCHING --------------------------------------------------

    #[test]
    fn legacy_laptop_source_deserializes_as_keys() {
        let s: SourceId = serde_json::from_str(r#""LaptopKeys""#).unwrap();
        assert_eq!(s, SourceId::Keys);
        assert_eq!(serde_json::to_string(&SourceId::Keys).unwrap(), r#""Keys""#);
        // An old rule keyed on the laptop source routes like the keyboard.
        let r: Rule = serde_json::from_str(&serde_json::to_string(&Rule::new(SourceId::Keys, SURGE_A)).unwrap()
            .replace(r#""Keys""#, r#""LaptopKeys""#)).unwrap();
        assert_eq!(r.source, SourceId::Keys);
    }

    #[test]
    fn source_isolation() {
        let mut m = Matrix::new();
        m.connect(SourceId::Hardware(7), SURGE_A);
        m.connect(SourceId::Keys, SURGE_B);
        assert_eq!(dests(&m.route(SourceId::Hardware(7), note_on(60))), vec![SURGE_A]);
        assert_eq!(dests(&m.route(SourceId::Keys, note_on(60))), vec![SURGE_B]);
        assert!(m.route(SourceId::Hardware(8), note_on(60)).is_empty());
        assert!(m.route(SourceId::Virtual(7), note_on(60)).is_empty());
        // Virtual and hardware ids are distinct namespaces.
        m.connect(SourceId::Virtual(1), PAD);
        assert_eq!(dests(&m.route(SourceId::Virtual(1), note_on(60))), vec![PAD]);
        assert!(m.route(SourceId::Hardware(1), note_on(60)).is_empty());
        // File players (P2) already route.
        m.connect(SourceId::FilePlayer(0), SURGE_A);
        assert_eq!(dests(&m.route(SourceId::FilePlayer(0), note_on(60))), vec![SURGE_A]);
    }

    #[test]
    fn empty_matrix_routes_nothing() {
        let m = Matrix::new();
        assert!(m.route(SourceId::Keys, note_on(60)).is_empty());
        assert!(m.rules_from(SourceId::Keys).next().is_none());
    }

    #[test]
    fn rules_from_lists_source_rules_in_order() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        m.connect(SourceId::Hardware(2), SURGE_B);
        let b = m.connect(SourceId::Keys, PAD);
        let ids: Vec<RuleId> = m.rules_from(SourceId::Keys).map(|r| r.id).collect();
        assert_eq!(ids, vec![0, b]);
    }

    // --- DETERMINISM & RT SHAPE -------------------------------------------

    #[test]
    fn routing_is_deterministic() {
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59).transpose(-12));
        m.connect(SourceId::Keys, PAD);
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127));
        for note in 0..=127u8 {
            let ev = note_on(note);
            assert_eq!(m.route(SourceId::Keys, ev), m.route(SourceId::Keys, ev));
        }
    }

    #[test]
    fn route_into_reuses_buffer() {
        let mut m = Matrix::new();
        m.connect(SourceId::Keys, SURGE_A);
        m.connect(SourceId::Keys, SURGE_B);
        let mut out = Vec::with_capacity(8);
        m.route_into(SourceId::Keys, note_on(60), &mut out);
        assert_eq!(out.len(), 2);
        let cap = out.capacity();
        for note in 0..=127u8 {
            m.route_into(SourceId::Keys, note_on(note), &mut out);
            assert_eq!(out.len(), 2); // cleared, not appended
        }
        assert_eq!(out.capacity(), cap); // no reallocation
        m.route_into(SourceId::Hardware(9), note_on(60), &mut out);
        assert!(out.is_empty());
        // route() and route_into() agree.
        assert_eq!(m.route(SourceId::Keys, note_on(60)), {
            m.route_into(SourceId::Keys, note_on(60), &mut out);
            out.clone()
        });
    }

    // --- WIRE DECODING ----------------------------------------------------

    #[test]
    fn from_bytes_roundtrip() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0x92, 60, 100], // note on ch3
            vec![0x80, 60, 64],  // note off
            vec![0x90, 60, 0],   // vel-0 note on
            vec![0xB1, 7, 100],  // CC
            vec![0xE0, 0, 64],   // bend center
            vec![0xC5, 42],      // program change (2-byte)
            vec![0xD3, 90],      // channel aftertouch (2-byte)
            vec![0xA0, 60, 90],  // poly aftertouch
        ];
        for bytes in cases {
            let msg = MidiMsg::from_bytes(&bytes).unwrap();
            assert_eq!(msg.status_byte(), bytes[0]);
            assert_eq!(msg.data[0], bytes[1]);
            if bytes.len() > 2 {
                assert_eq!(msg.data[1], bytes[2]);
            }
        }
        assert_eq!(
            MidiMsg::from_bytes(&[0x92, 60, 100]),
            Some(MidiMsg::note_on(2, 60, 100))
        );
    }

    #[test]
    fn from_bytes_rejects_system_and_short() {
        assert!(MidiMsg::from_bytes(&[]).is_none());
        assert!(MidiMsg::from_bytes(&[0x90]).is_none());
        assert!(MidiMsg::from_bytes(&[0x90, 60]).is_none()); // truncated 3-byte
        assert!(MidiMsg::from_bytes(&[0xF8]).is_none()); // clock
        assert!(MidiMsg::from_bytes(&[0xF0, 0x7E, 0xF7]).is_none()); // sysex
        assert!(MidiMsg::from_bytes(&[0x60, 60, 100]).is_none()); // no status
        assert!(MidiMsg::from_bytes(&[0x90, 0x80, 100]).is_none()); // data MSB set
    }

    // --- SESSION SERIALIZATION --------------------------------------------

    #[test]
    fn serde_roundtrip_preserves_routing() {
        let mut m = Matrix::new();
        m.reserve(8);
        m.add_rule(
            Rule::new(SourceId::Hardware(7), SURGE_A)
                .on_channel(2)
                .key_range(0, 59)
                .transpose(-12)
                .vel_range(10, 120)
                .map_cc(1, 5),
        );
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127).enabled(false));
        let json = serde_json::to_string(&m).unwrap();
        let m2: Matrix = serde_json::from_str(&json).unwrap();
        assert_eq!(m.rules, m2.rules);
        let ev = MidiMsg::note_on(2, 48, 100);
        assert_eq!(m.route(SourceId::Hardware(7), ev), m2.route(SourceId::Hardware(7), ev));
        // Ids continue after restore.
        let mut m2 = m2;
        let id = m2.connect(SourceId::Keys, PAD);
        assert!(m2.rule(id).is_some());
        assert!(m2.rules.iter().all(|r| r.id != id || r.dest == PAD));
    }

    // --- ACCEPTANCE: two Surge layers split across the keyboard -----------

    #[test]
    fn two_surge_layers_split_acceptance() {
        // docs/ARCHITECTURE.md §5 / task-9 acceptance: two Surge layers split across the
        // keyboard — bass below C4 (shifted down an octave), lead from C4 up,
        // plus a pad layered over the whole range.
        let mut m = Matrix::new();
        m.add_rule(Rule::new(SourceId::Keys, SURGE_A).key_range(0, 59).transpose(-12));
        m.add_rule(Rule::new(SourceId::Keys, SURGE_B).key_range(60, 127));
        m.add_rule(Rule::new(SourceId::Keys, PAD));

        for note in 48..=72u8 {
            let out = m.route(SourceId::Keys, note_on(note));
            assert_eq!(out.len(), 2, "note {note}: split zone + pad layer");
            if note < 60 {
                assert_eq!(out[0], (SURGE_A, note_on(note - 12)));
                assert_eq!(out[1], (PAD, note_on(note)));
            } else {
                assert_eq!(out[0], (SURGE_B, note_on(note)));
                assert_eq!(out[1], (PAD, note_on(note)));
            }
            // Releases mirror the press exactly.
            let off = m.route(SourceId::Keys, MidiMsg::note_off(0, note, 0));
            assert_eq!(dests(&off), dests(&out));
        }
        // Hardware controller can drive the same patch independently.
        m.connect(SourceId::Hardware(3), SURGE_B);
        assert_eq!(
            dests(&m.route(SourceId::Hardware(3), note_on(60))),
            vec![SURGE_B]
        );
    }

    #[test]
    fn set_cc_unmap_cc_and_legacy_serde_roundtrip() {
        let mut r = Rule::new(SourceId::Hardware(0), SURGE_A);
        assert!(r.set_cc(74, 10));
        assert!(!r.set_cc(74, 10), "idempotent set returns false");
        assert!(r.set_cc(74, 20), "replacing param returns true");
        assert_eq!(r.param_for_cc(74), Some(20));
        assert!(r.unmap_cc(74));
        assert!(!r.unmap_cc(74), "missing cc returns false");
        assert!(r.cc_map.is_empty());

        let r2 = Rule::new(SourceId::Hardware(2), SURGE_B).map_cc(74, 55).map_cc(1, 12);
        let back: Rule = serde_json::from_str(&serde_json::to_string(&r2).unwrap()).unwrap();
        assert_eq!(back.cc_map, r2.cc_map);

        // Legacy rules saved without `cc_map` deserialize with empty transparent map.
        let legacy = r#"{"id":1,"source":"Keys","dest":1,"channel":null,"transpose":0,"key_range":[0,127],"vel_range":[0,127],"enabled":true}"#;
        let legacy_rule: Rule = serde_json::from_str(legacy).unwrap();
        assert!(legacy_rule.cc_map.is_empty());

        // Legacy CcMapping saved without `target` defaults to PluginParam.
        let legacy_map: CcMapping = serde_json::from_str(r#"{"cc":74,"param":42}"#).unwrap();
        assert_eq!(legacy_map, CcMapping { cc: 74, param: 42, target: CcTarget::PluginParam, scale: CcScale::IDENTITY });

        // Knob target round-trips and replacing target marks the rule dirty.
        let mut r3 = Rule::new(SourceId::Hardware(0), SURGE_A);
        assert!(r3.set_cc_target(7, 0, CcTarget::PluginParam));
        assert!(!r3.set_cc_target(7, 0, CcTarget::PluginParam));
        assert!(r3.set_cc_target(7, 0, CcTarget::Knob));
        assert_eq!(r3.mapping_for_cc(7), Some(CcMapping { cc: 7, param: 0, target: CcTarget::Knob, scale: CcScale::IDENTITY }));
        let back3: Rule = serde_json::from_str(&serde_json::to_string(&r3).unwrap()).unwrap();
        assert_eq!(back3.cc_map, r3.cc_map);
    }
}