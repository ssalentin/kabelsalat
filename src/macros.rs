//! Macros (#289): one named control that drives several parameters, each
//! over its own range, with its own curve and direction. Scene morphing
//! (#214) is a macro whose targets are a rig's knobs, with state A as each
//! target's minimum and state B as its maximum.
//!
//! A macro lives on a home node: its knob sits on that unit's face, and a
//! controller is bound to it like to any knob, as a [`CcTarget::Macro`]
//! binding on the matrix rule `(source, home)` whose `param` is the macro's
//! id. The engine holds the table (`Command::SetMacros`) and applies every
//! target in the block the CC arrives in (rule 31); frontends mirror the
//! same values onto their knobs with [`MacroTarget::value`].

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::control::{Command, NodeId, ParamId};
use crate::matrix::CcTarget;

/// Most macros of one session (a guard against a runaway tool call, not a
/// table size: the engine's table is as long as the session needs).
pub const MAX_MACROS: usize = 64;

/// How a target follows the macro's travel. Every curve is monotonic and
/// maps 0 to 0 and 1 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    /// Slow start, fast end (t²): most of the change in the last third.
    Slow,
    /// Fast start, slow end.
    Fast,
    /// Eases in and out (smoothstep).
    Smooth,
    /// Minimum below the middle, maximum from the middle on: how a stepped
    /// knob takes part in a morph (#214).
    Switch,
}

impl Curve {
    pub const ALL: [Curve; 5] = [Curve::Linear, Curve::Slow, Curve::Fast, Curve::Smooth, Curve::Switch];

    pub fn as_str(self) -> &'static str {
        match self {
            Curve::Linear => "linear",
            Curve::Slow => "slow",
            Curve::Fast => "fast",
            Curve::Smooth => "smooth",
            Curve::Switch => "switch",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Curve::Linear => "Linear",
            Curve::Slow => "Slow start",
            Curve::Fast => "Fast start",
            Curve::Smooth => "Smooth",
            Curve::Switch => "Switch",
        }
    }

    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        match s.map(str::trim).map(str::to_lowercase).as_deref() {
            None | Some("") | Some("linear") | Some("lin") => Ok(Curve::Linear),
            Some("slow") | Some("exp") | Some("exponential") => Ok(Curve::Slow),
            Some("fast") | Some("log") | Some("logarithmic") => Ok(Curve::Fast),
            Some("smooth") | Some("s") | Some("s-curve") => Ok(Curve::Smooth),
            Some("switch") | Some("step") => Ok(Curve::Switch),
            Some(o) => Err(format!("unknown curve '{o}' (expected linear, slow, fast, smooth or switch)")),
        }
    }

    /// The curve at `t` (0..=1).
    pub fn shape(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::Slow => t * t,
            Curve::Fast => 1.0 - (1.0 - t) * (1.0 - t),
            Curve::Smooth => t * t * (3.0 - 2.0 * t),
            Curve::Switch => if t < 0.5 { 0.0 } else { 1.0 },
        }
    }
}

/// One parameter a macro drives: `(node, param)` in the space `target`
/// names (a plugin parameter, a rack knob or a mixer control, as in a CC
/// binding), from `min` at the macro's 0 to `max` at its 1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MacroTarget {
    pub node: NodeId,
    pub param: ParamId,
    #[serde(default)]
    pub target: CcTarget,
    /// Normalized (0..=1) value at the macro's start. May be above `max`.
    #[serde(default)]
    pub min: f32,
    #[serde(default = "one")]
    pub max: f32,
    #[serde(default)]
    pub curve: Curve,
    /// The target runs against the control: `max` at 0, `min` at 1.
    #[serde(default)]
    pub invert: bool,
}

fn one() -> f32 {
    1.0
}

impl MacroTarget {
    pub fn new(node: NodeId, param: ParamId, target: CcTarget) -> Self {
        Self { node, param, target, min: 0.0, max: 1.0, curve: Curve::Linear, invert: false }
    }

    /// The target's normalized value with the macro at `v` (0..=1).
    pub fn value(&self, v: f64) -> f64 {
        let t = if self.invert { 1.0 - v.clamp(0.0, 1.0) } else { v.clamp(0.0, 1.0) };
        let (lo, hi) = (self.min.clamp(0.0, 1.0) as f64, self.max.clamp(0.0, 1.0) as f64);
        lo + (hi - lo) * self.curve.shape(t)
    }

    /// False for a target the macro never moves (both ends alike): a morph
    /// leaves the knobs alone that A and B agree on, so a hand on one of
    /// them is not pulled back.
    pub fn moves(&self) -> bool {
        (self.max - self.min).abs() > 1e-6
    }

    /// Same parameter, whatever the range.
    pub fn same_dest(&self, node: NodeId, param: ParamId, target: CcTarget) -> bool {
        self.node == node && self.param == param && self.target == target
    }
}

/// A named control and what it drives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    /// Stable within a session; the `param` of its CC bindings.
    pub id: u32,
    pub name: String,
    /// The node whose face shows the knob and whose matrix rules bind it.
    pub home: NodeId,
    /// Where the control stands (0..=1). Not part of the engine's table.
    #[serde(default)]
    pub value: f32,
    /// A rig's morph control (#214): targets are that rig's knobs, `min` is
    /// state A and `max` state B.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub morph: bool,
    #[serde(default)]
    pub targets: Vec<MacroTarget>,
}

impl Macro {
    /// Everything the engine acts on is alike (the knob's position is not).
    pub fn same_table_entry(&self, other: &Macro) -> bool {
        self.id == other.id && self.home == other.home && self.targets == other.targets
    }

    /// Add a target or replace the one with the same destination.
    pub fn set_target(&mut self, t: MacroTarget) {
        match self.targets.iter_mut().find(|x| x.same_dest(t.node, t.param, t.target)) {
            Some(x) => *x = t,
            None => self.targets.push(t),
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id, "name": self.name, "node": self.home, "value": self.value, "morph": self.morph,
            "targets": self.targets.iter().map(target_json).collect::<Vec<_>>(),
        })
    }
}

pub fn target_json(t: &MacroTarget) -> serde_json::Value {
    serde_json::json!({
        "node": t.node, "param": t.param, "target": t.target.as_str(),
        "min": t.min, "max": t.max, "curve": t.curve.as_str(), "invert": t.invert,
    })
}

/// The macro an agent names: its id, or its name (case-insensitive).
pub fn find<'a>(macros: &'a [Macro], key: &serde_json::Value) -> Result<&'a Macro, String> {
    let hit = match key {
        serde_json::Value::Number(n) => n.as_u64().and_then(|id| macros.iter().find(|m| m.id as u64 == id)),
        serde_json::Value::String(s) => {
            let q = s.trim();
            macros.iter().find(|m| m.name.eq_ignore_ascii_case(q))
                .or_else(|| q.parse::<u32>().ok().and_then(|id| macros.iter().find(|m| m.id == id)))
        }
        _ => return Err("pass 'macro': its id or its name (list_macros lists them)".into()),
    };
    hit.ok_or_else(|| {
        let names: Vec<String> = macros.iter().map(|m| format!("{} ('{}')", m.id, m.name)).collect();
        if names.is_empty() { format!("no macro {key}: there are none yet (set_macro makes one)") }
        else { format!("no macro {key}; macros: {}", names.join(", ")) }
    })
}

/// The first id no macro uses.
pub fn free_id(macros: &[Macro]) -> u32 {
    (1..).find(|id| !macros.iter().any(|m| m.id == *id)).unwrap_or(1)
}

/// Macros with every node id moved through `remap` (rule 44). A macro whose
/// home is gone goes; a target whose node is gone goes alone.
pub fn remap(macros: &[Macro], remap: impl Fn(NodeId) -> Option<NodeId>) -> Vec<Macro> {
    macros.iter().filter_map(|m| {
        let home = remap(m.home)?;
        let targets = m.targets.iter().filter_map(|t| Some(MacroTarget { node: remap(t.node)?, ..*t })).collect();
        Some(Macro { home, targets, ..m.clone() })
    }).collect()
}

/// The engine's macro table (`Command::SetMacros`). Shared, not copied: the
/// frontend keeps each table it sent until the engine has let go of it
/// ([`MacrosSent`]), so the audio thread never frees one (rule 197).
#[derive(Clone, Debug, PartialEq)]
pub struct MacroTable(Arc<[Macro]>);

impl MacroTable {
    pub fn new(macros: Vec<Macro>) -> Self {
        Self(macros.into())
    }

    pub fn get(&self, id: u32) -> Option<&Macro> {
        self.0.iter().find(|m| m.id == id)
    }
}

impl Serialize for MacroTable {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for MacroTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::deserialize(d).map(Self::new)
    }
}

/// What a frontend last sent the engine of its macros, so a frame sends the
/// table only when a macro changed (a moved knob is no change).
#[derive(Default)]
pub struct MacrosSent {
    /// Every table the engine may still hold, the one sent last at the end.
    tables: Vec<MacroTable>,
}

impl MacrosSent {
    /// Send the table when it differs from the last one sent. A table the
    /// engine let go of is freed here.
    pub fn publish(&mut self, macros: &[Macro], mut send: impl FnMut(Command) -> bool) {
        self.tables.retain(|t| Arc::strong_count(&t.0) > 1);
        let last = self.tables.last().map_or(&[][..], |t| &t.0[..]);
        if last.len() == macros.len() && last.iter().zip(macros).all(|(a, b)| a.same_table_entry(b)) {
            return;
        }
        let table = MacroTable::new(macros.iter().map(|m| Macro { value: 0.0, name: String::new(), ..m.clone() }).collect());
        if send(Command::SetMacros { macros: table.clone() }) {
            self.tables.push(table);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curve_is_monotonic_and_keeps_its_ends() {
        for c in Curve::ALL {
            assert_eq!(c.shape(0.0), 0.0, "{c:?}");
            assert_eq!(c.shape(1.0), 1.0, "{c:?}");
            let mut last = 0.0;
            for i in 0..=127 {
                let v = c.shape(i as f64 / 127.0);
                assert!(v >= last, "{c:?} falls at {i}");
                last = v;
            }
            assert_eq!(Curve::parse(Some(c.as_str())), Ok(c));
        }
        assert_eq!(Curve::Switch.shape(0.49), 0.0);
        assert_eq!(Curve::Switch.shape(0.5), 1.0, "a stepped knob switches at the middle");
    }

    #[test]
    fn a_target_spans_its_own_range_and_an_inverted_one_runs_back() {
        let t = MacroTarget { min: 0.2, max: 0.6, ..MacroTarget::new(1, 0, CcTarget::Knob) };
        assert!((t.value(0.0) - 0.2).abs() < 1e-6 && (t.value(1.0) - 0.6).abs() < 1e-6);
        assert!((t.value(0.5) - 0.4).abs() < 1e-6);
        let inv = MacroTarget { invert: true, ..t };
        assert!((inv.value(0.0) - 0.6).abs() < 1e-6 && (inv.value(1.0) - 0.2).abs() < 1e-6);
        // A range written upside down runs down too.
        let down = MacroTarget { min: 0.9, max: 0.1, ..t };
        assert!(down.value(0.0) > down.value(1.0));
        assert!(!MacroTarget { min: 0.3, max: 0.3, ..t }.moves());
    }

    #[test]
    fn a_macro_follows_its_nodes_through_a_remap_and_loses_only_dead_targets() {
        let m = Macro {
            id: 3, name: "Swell".into(), home: 4, value: 0.5, morph: false,
            targets: vec![MacroTarget::new(2, 7, CcTarget::Knob), MacroTarget::new(9, 1, CcTarget::Mix)],
        };
        let out = remap(&[m.clone()], |n| match n { 4 => Some(14), 2 => Some(12), _ => None });
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].home, out[0].id, out[0].value), (14, 3, 0.5));
        assert_eq!(out[0].targets, vec![MacroTarget::new(12, 7, CcTarget::Knob)]);
        assert!(remap(&[m], |n| (n != 4).then_some(n)).is_empty(), "a macro without its home unit goes");
    }

    #[test]
    fn the_table_is_sent_when_a_target_changes_and_not_when_the_knob_moves() {
        let mut macros = vec![Macro { id: 1, name: "M".into(), home: 1, value: 0.0, morph: false, targets: vec![MacroTarget::new(1, 0, CcTarget::Knob)] }];
        let mut sent = MacrosSent::default();
        let mut out = Vec::new();
        let mut publish = |sent: &mut MacrosSent, macros: &[Macro], out: &mut Vec<Command>| sent.publish(macros, |c| { out.push(c); true });
        publish(&mut sent, &macros, &mut out);
        assert_eq!(out.len(), 1);
        macros[0].value = 0.7;
        macros[0].name = "Renamed".into();
        publish(&mut sent, &macros, &mut out);
        assert_eq!(out.len(), 1, "a moved or renamed knob is not a new table");
        macros[0].targets[0].max = 0.5;
        publish(&mut sent, &macros, &mut out);
        assert_eq!(out.len(), 2);
        // The engine dropped the first table when it took the second.
        out.remove(0);
        publish(&mut sent, &macros, &mut out);
        assert_eq!(sent.tables.len(), 1, "a table the engine let go of is freed by the sender");
        let json = serde_json::to_string(&macros[0]).unwrap();
        assert_eq!(serde_json::from_str::<Macro>(&json).unwrap(), macros[0]);
        assert_eq!(find(&macros, &serde_json::json!("renamed")).unwrap().id, 1);
        assert_eq!(find(&macros, &serde_json::json!(1)).unwrap().id, 1);
        assert!(find(&macros, &serde_json::json!("nope")).is_err());
        assert_eq!(free_id(&macros), 2);
    }
}
