//! Exhaustive held-note check — every reachable routing state, not samples.
//!
//! Property: once a key is released, nothing that key started is still
//! sounding on any instrument, whatever routing edits (split, matrix rule,
//! MIDI cable, template load) happened while it was held.
//! Harm if broken: a stuck note drones until the rack is reloaded.
//!
//! Method: breadth-first search over the REAL `GraphEngine::apply` (the same
//! function `render` drains every command into). Each state is the engine's
//! graph + matrix plus the held keys and the sounding notes; the search runs
//! until no new state appears, so every interleaving of the alphabet below is
//! covered at every length. Sounding notes are observed through the
//! test-only `note_log` (delivered note-on / note-off / all-notes-off).
//!
//! Two scenarios share the machinery. The routing scenario is the plain rack
//! (Keys → two synths). The arp scenario (issue #90) starts from the same rack
//! and adds time (`Tick`), the arp unit (inserted, tuned, unplugged, removed)
//! and the Keys scale snap to the alphabet; there a generated note belongs to
//! no key, so the property is stronger: when the LAST key is released nothing
//! sounds at all.
//!
//! The MIDI-FX scenarios (issue #86) start from a rack with a MIDI strip (a
//! built-in transposer) between the Keys and Surge A, and add the unit's own
//! edits: its knob, its matrix legs, its cables, its removal and the rig's
//! source switch. The routing edits of the plain rack run once more with the
//! strip present. The full product of both halves has no fixpoint a test can
//! reach, so the halves run separately and the FX half records every edit as
//! one kind (which kinds a key saw is part of the state).
//!
//! Run + interpret: `.claude/skills/verify-held-notes/SKILL.md`.

use super::GraphEngine;
use crate::arp::{Scale, ScaleChoice};
use crate::control::{command_bus, name_bytes, Command, Graph, MidiFxPlugin, NodeKind, PortKind, PortRef};
use crate::matrix::SourceId;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

const KEYS: u32 = 0;
const SURGE_A: u32 = 1;
const SURGE_B: u32 = 3;
/// The arp node the scenario's `AddArp` creates (the next free id).
const ARP: u32 = 4;
/// The MIDI-FX unit (built-in transposer) of the MIDI-FX scenarios.
const FX: u32 = 4;
/// Marks a note the arp generated (no key owns it).
const GENERATED: u8 = 255;
/// Played keys: either side of the C4 split point, and both ≥ 48.
const NOTES: [u8; 2] = [59, 60];

/// What kind of routing edit happened while a key was held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Kind {
    Split,
    Matrix,
    Cable,
    Node,
    Load,
    Scale,
    Arp,
    /// A MIDI-FX unit's own edit (knob, rule, cable, removal).
    Fx,
    /// A rig source switch (keyboard / hardware) of an instrument.
    Source,
    /// Starting or stopping a rig demo phrase (#200).
    Demo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    On(u8),
    Off(u8),
    Edit(Edit),
    /// Time passes: every arp steps through a window that holds its next onset.
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    /// SetNodeSplit on the Keys node.
    Split(Option<u8>),
    /// SetZoneOctave on the Keys node (acts only while split).
    ZoneOctave { zone: u8, octave: i8 },
    /// MatrixRule Keys → dest (full key/velocity range).
    Rule { dest: u32, transpose: i8, enabled: bool },
    /// Disconnect a MIDI cable by edge id (1 = Keys→A, 6 = Keys→B).
    Unplug(u32),
    /// Connect a MIDI cable Keys.port → dest with this edge id.
    Plug { id: u32, port: u8, dest: u32 },
    /// RemoveNode of an instrument.
    Remove(u32),
    /// LoadGraph of the start patch (template load).
    LoadStart,
    /// SetKeysScale on the Keys node.
    Scale(Option<ScaleChoice>),
    /// AddNode of an arp, wired like the app's `add_node` (takes over the
    /// rig's Keys → synth cables). Once per load (see `Run::arp_added`).
    AddArp,
    /// SetParam on the arp node; `pct` = knob value in percent.
    ArpParam { param: u32, pct: u8 },
    /// Connect a MIDI cable `from` → `to` (port 0 both) with this edge id.
    Wire { id: u32, from: u32, to: u32 },
    /// The transposer's SEMIS knob (SetParam), in semitones.
    FxSemis(i8),
    /// MatrixRule Keys → FX (the unit's input leg).
    RuleFxIn { enabled: bool },
    /// MatrixRule FX → Surge A (the unit's output leg), with a transpose.
    RuleFxOut { transpose: i8, enabled: bool },
    /// Connect cable 8 (Keys → FX) or 9 (FX → Surge A).
    PlugFx(u32),
    /// RemoveNode of the MIDI-FX unit (the chain heals).
    RemoveFx,
    /// SetRigSource on Surge A: keyboard on/off.
    Source { keyboard: bool },
    /// Start a demo phrase on rig 0 (#200).
    DemoPlay(crate::demo::PhraseKind),
    /// Stop the running demo phrase (#200).
    DemoStop,
}

/// Which scenario a search runs: the plain rack, the arp (#90), the rack
/// with a MIDI strip (#86), or the rig demo phrase player (#200).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Routing,
    Arp,
    RoutingWithStrip,
    Fx,
    Demo,
}

/// C# major pentatonic: B (59) snaps down to A#, C (60) up to C#. Two keys
/// that snapped to ONE pitch would share its note (releasing one rightly
/// leaves the other's sounding), which the observer cannot attribute.
const PENTATONIC: ScaleChoice = ScaleChoice { scale: Scale::MajorPentatonic, key: 1 };

const EDITS: [Edit; 15] = [
    Edit::Split(None),
    Edit::Split(Some(60)),
    Edit::Split(Some(48)),
    Edit::ZoneOctave { zone: 0, octave: 1 },
    Edit::Rule { dest: SURGE_A, transpose: 0, enabled: true },
    Edit::Rule { dest: SURGE_A, transpose: 12, enabled: true },
    Edit::Rule { dest: SURGE_A, transpose: 0, enabled: false },
    Edit::Rule { dest: SURGE_B, transpose: 0, enabled: true },
    Edit::Rule { dest: SURGE_B, transpose: -12, enabled: true },
    Edit::Unplug(1),
    Edit::Unplug(6),
    Edit::Plug { id: 1, port: 0, dest: SURGE_A },
    Edit::Plug { id: 6, port: 0, dest: SURGE_B },
    Edit::Plug { id: 6, port: 1, dest: SURGE_B },
    Edit::Remove(SURGE_B),
];
// LoadStart is appended separately so the edit list stays readable.

/// Edits of the MIDI-FX scenario: the unit's own, with the few routing edits
/// that change what reaches it.
const FX_EDITS: [Edit; 18] = [
    Edit::Split(None),
    Edit::Split(Some(60)),
    Edit::Rule { dest: SURGE_A, transpose: 0, enabled: false },
    Edit::Rule { dest: SURGE_A, transpose: 0, enabled: true },
    Edit::Unplug(1),
    Edit::Unplug(8),
    Edit::Unplug(9),
    Edit::Plug { id: 1, port: 0, dest: SURGE_A },
    Edit::FxSemis(12),
    Edit::FxSemis(0),
    Edit::RuleFxIn { enabled: false },
    Edit::RuleFxIn { enabled: true },
    Edit::RuleFxOut { transpose: 12, enabled: true },
    Edit::RuleFxOut { transpose: 0, enabled: false },
    Edit::PlugFx(8),
    Edit::PlugFx(9),
    Edit::RemoveFx,
    Edit::Source { keyboard: false },
];

/// Edits of the arp scenario. Knob percents land on the stepped knobs'
/// positions: MODE 0 = up, 75 = chord.
const ARP_EDITS: [Edit; 7] = [
    Edit::Scale(Some(PENTATONIC)),
    Edit::AddArp,
    Edit::ArpParam { param: crate::arp::P_ARP_MODE, pct: 0 },
    Edit::ArpParam { param: crate::arp::P_ARP_MODE, pct: 75 },
    Edit::Remove(ARP),
    Edit::Unplug(9),
    // The UI's stale mirror re-sending the cable the arp took over.
    Edit::Wire { id: 1, from: KEYS, to: SURGE_A },
];

/// Edits of the rig demo scenario (#200): demo start/stop, time, and one
/// edit of each routing kind (split, zone octave, scale, matrix rule, cable,
/// node removal, arp take-over, source switch, template load).
const DEMO_EDITS: [Edit; 11] = [
    Edit::DemoPlay(crate::demo::PhraseKind::Lead),
    Edit::DemoStop,
    Edit::Split(Some(72)),
    Edit::ZoneOctave { zone: 0, octave: 1 },
    Edit::Scale(Some(PENTATONIC)),
    Edit::Rule { dest: SURGE_A, transpose: 12, enabled: true },
    Edit::Unplug(1),
    Edit::Plug { id: 1, port: 0, dest: SURGE_A },
    Edit::Remove(SURGE_B),
    Edit::AddArp,
    Edit::Source { keyboard: false },
];

impl Edit {
    fn kind(self) -> Kind {
        match self {
            Edit::Split(_) | Edit::ZoneOctave { .. } => Kind::Split,
            Edit::Rule { .. } => Kind::Matrix,
            Edit::Unplug(_) | Edit::Plug { .. } => Kind::Cable,
            Edit::Remove(_) => Kind::Node,
            Edit::LoadStart => Kind::Load,
            Edit::Scale(_) => Kind::Scale,
            Edit::AddArp | Edit::ArpParam { .. } => Kind::Arp,
            Edit::Wire { .. } => Kind::Cable,
            Edit::FxSemis(_) | Edit::RuleFxIn { .. } | Edit::RuleFxOut { .. } | Edit::PlugFx(_) | Edit::RemoveFx => Kind::Fx,
            Edit::Source { .. } => Kind::Source,
            Edit::DemoPlay(_) | Edit::DemoStop => Kind::Demo,
        }
    }

    fn command(self, scn: Scenario) -> Command {
        match self {
            Edit::Split(point) => Command::SetNodeSplit { node: KEYS, point },
            Edit::ZoneOctave { zone, octave } => Command::SetZoneOctave { node: KEYS, zone, octave },
            Edit::Rule { dest, transpose, enabled } => Command::MatrixRule {
                source: SourceId::Keys,
                dest,
                channel: None,
                transpose,
                key_lo: 0,
                key_hi: 127,
                vel_lo: 0,
                vel_hi: 127,
                enabled,
            },
            Edit::Unplug(id) => Command::Disconnect(id),
            Edit::Remove(id) => Command::RemoveNode(id),
            Edit::Plug { id, port, dest } => Command::Connect {
                id,
                from: PortRef { node: KEYS, port },
                to: PortRef { node: dest, port: 0 },
                kind: PortKind::Midi,
            },
            Edit::LoadStart => Command::LoadGraph { graph: start_graph_for(scn) },
            Edit::Scale(scale) => Command::SetKeysScale { node: KEYS, scale },
            Edit::AddArp => Command::AddNode { kind: NodeKind::Arp, name: name_bytes("ARP"), rig: 0, wire: true },
            Edit::ArpParam { param, pct } => Command::SetParam { node: ARP, param, value: pct as f64 / 100.0 },
            Edit::Wire { id, from, to } => Command::Connect {
                id,
                from: PortRef { node: from, port: 0 },
                to: PortRef { node: to, port: 0 },
                kind: PortKind::Midi,
            },
            Edit::FxSemis(semis) => Command::SetParam {
                node: FX,
                param: super::P_MIDIFX_SEMI,
                value: 0.5 + semis as f64 / (2.0 * super::MIDIFX_SEMI_MAX as f64),
            },
            Edit::RuleFxIn { enabled } => Command::MatrixRule {
                source: SourceId::Keys,
                dest: FX,
                channel: None,
                transpose: 0,
                key_lo: 0,
                key_hi: 127,
                vel_lo: 0,
                vel_hi: 127,
                enabled,
            },
            Edit::RuleFxOut { transpose, enabled } => Command::MatrixRule {
                source: SourceId::MidiFx(FX),
                dest: SURGE_A,
                channel: None,
                transpose,
                key_lo: 0,
                key_hi: 127,
                vel_lo: 0,
                vel_hi: 127,
                enabled,
            },
            Edit::PlugFx(8) => Command::Connect {
                id: 8,
                from: PortRef { node: KEYS, port: 0 },
                to: PortRef { node: FX, port: 0 },
                kind: PortKind::Midi,
            },
            Edit::PlugFx(id) => Command::Connect {
                id,
                from: PortRef { node: FX, port: 0 },
                to: PortRef { node: SURGE_A, port: 0 },
                kind: PortKind::Midi,
            },
            Edit::RemoveFx => Command::RemoveNode(FX),
            Edit::Source { keyboard } => Command::SetRigSource { node: SURGE_A, keyboard, hardware: true },
            Edit::DemoPlay(phrase) => Command::DemoPlay { rig: 0, phrase },
            Edit::DemoStop => Command::DemoStop,
        }
    }
}

/// Keys(0) → Surge A(1) and Surge B(3) over MIDI cables 1 and 6, both into
/// Master(2). The demo patch's MIDI topology without its Delay nodes (audio
/// effects do not take part in note routing).
fn start_graph() -> Graph {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let clap = || NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") };
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let a = g.add_node(clap(), name_bytes("SURGE A"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
    let b = g.add_node(clap(), name_bytes("SURGE B"));
    assert_eq!((keys, a, master, b), (KEYS, SURGE_A, 2, SURGE_B));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: a, port: 0 }, PortKind::Midi);
    g.connect_with_id(6, PortRef { node: keys, port: 0 }, PortRef { node: b, port: 0 }, PortKind::Midi);
    g.connect_with_id(3, PortRef { node: a, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(7, PortRef { node: b, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g
}

/// Keys(0) → MIDI-FX transposer(4) → Surge A(1), and Keys → Surge B(3) over
/// MIDI cables 8+9 and 6, both synths into Master(2): the plain rack with a
/// MIDI strip in Surge A's chain. Cable 1 (Keys → A) is absent: the unit
/// replaced it, and the alphabet can plug it back.
fn start_graph_with_strip() -> Graph {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let clap = || NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") };
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let a = g.add_node(clap(), name_bytes("SURGE A"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
    let b = g.add_node(clap(), name_bytes("SURGE B"));
    let fx = g.add_node(NodeKind::MidiFx { plugin: MidiFxPlugin::Builtin { semis: 0 } }, name_bytes("TRANSPOSE"));
    assert_eq!((keys, a, master, b, fx), (KEYS, SURGE_A, 2, SURGE_B, FX));
    g.connect_with_id(8, PortRef { node: keys, port: 0 }, PortRef { node: fx, port: 0 }, PortKind::Midi);
    g.connect_with_id(9, PortRef { node: fx, port: 0 }, PortRef { node: a, port: 0 }, PortKind::Midi);
    g.connect_with_id(6, PortRef { node: keys, port: 0 }, PortRef { node: b, port: 0 }, PortKind::Midi);
    g.connect_with_id(3, PortRef { node: a, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g.connect_with_id(7, PortRef { node: b, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
    g
}

fn start_graph_for(scn: Scenario) -> Graph {
    match scn {
        Scenario::RoutingWithStrip | Scenario::Fx => start_graph_with_strip(),
        Scenario::Routing | Scenario::Arp | Scenario::Demo => start_graph(),
    }
}

/// Engine + the observer's view of what is held and what is sounding.
struct Run {
    engine: GraphEngine,
    /// Held key → routing-edit kinds applied since its note-on.
    held: BTreeMap<u8, BTreeSet<Kind>>,
    /// (instrument, sounding pitch, key that started it).
    sounding: BTreeSet<(u32, u8, u8)>,
    /// Arp scenario: chords of two held keys can share a pitch (ref-counted
    /// by the arp), so a release is judged only when the LAST key goes up.
    arp_scenario: bool,
    scn: Scenario,
    /// An arp was added since the last load: each AddArp would otherwise mint
    /// a fresh node id (an unbounded state space), so there is one per load.
    arp_added: bool,
}

/// A key whose release left notes sounding.
pub(super) struct Stuck {
    /// Routing-edit kinds between the key's note-on and note-off.
    pub(super) kinds: BTreeSet<Kind>,
    /// Shortest command sequence from the start patch, ending in the note-off.
    pub(super) trace: Vec<String>,
    /// (instrument, pitch) still sounding after the release.
    pub(super) left: Vec<(u32, u8)>,
}

pub(super) struct Report {
    pub(super) states: usize,
    /// False when `max_states` stopped the search before its fixpoint.
    pub(super) complete: bool,
    /// One shortest counterexample per distinct `kinds` set.
    pub(super) stuck: Vec<Stuck>,
}

impl Run {
    fn new(scn: Scenario) -> Self {
        let (_bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::default(), drain, 48000.0, vec![]);
        engine.apply(Command::LoadGraph { graph: start_graph_for(scn) });
        engine.note_log.clear();
        Run { engine, held: BTreeMap::new(), sounding: BTreeSet::new(), arp_scenario: scn == Scenario::Arp, scn, arp_added: false }
    }

    fn enabled(&self, step: Step) -> bool {
        match step {
            Step::On(k) => !self.held.contains_key(&k),
            Step::Off(k) => self.held.contains_key(&k),
            Step::Edit(Edit::AddArp) => !self.arp_added,
            Step::Edit(_) | Step::Tick => true,
        }
    }

    /// Apply one step through the real engine. Returns the notes a released
    /// key left sounding (empty = fine).
    fn step(&mut self, step: Step) -> Vec<(u32, u8)> {
        let (cmd, key) = match step {
            Step::On(k) => (Some(Command::NoteOn { note: k, velocity: 0.8, source: SourceId::Keys, channel: 0 }), Some(k)),
            Step::Off(k) => (Some(Command::NoteOff { note: k, source: SourceId::Keys, channel: 0 }), Some(k)),
            Step::Edit(e) => (Some(e.command(self.scn)), None),
            Step::Tick => (None, None),
        };
        self.engine.note_log.clear();
        match cmd {
            Some(cmd) => self.engine.apply(cmd),
            None => self.engine.arp_test_step(),
        }
        // A MIDI-FX unit's queue is processed at the next block: pump it, like
        // `render` does, so what the unit emits reaches the log.
        self.engine.pump_midi_fx(64);
        for &(dest, note, on) in &self.engine.note_log {
            if on {
                // A note-on from an edit or a tick is generated (chord
                // re-sound, arp step): no key owns it.
                self.sounding.insert((dest, note, key.unwrap_or(GENERATED)));
            } else if note == 255 {
                self.sounding.retain(|s| s.0 != dest); // release_all marker
            } else {
                self.sounding.retain(|s| !(s.0 == dest && s.1 == note));
            }
        }
        match step {
            Step::On(k) => {
                self.held.insert(k, BTreeSet::new());
                Vec::new()
            }
            Step::Off(k) => {
                self.held.remove(&k);
                // The released key's own notes must be gone; when it was the
                // last key held and no demo is playing, so must everything
                // the arp or demo generated.
                let left: Vec<(u32, u8)> = self.sounding.iter()
                    .filter(|s| if self.arp_scenario {
                        self.held.is_empty()
                    } else if self.scn == Scenario::Demo {
                        (self.held.is_empty() && self.engine.demo.is_none()) || s.2 == k
                    } else {
                        s.2 == k
                    })
                    .map(|s| (s.0, s.1)).collect();
                left
            }
            Step::Edit(e) => {
                match e {
                    Edit::AddArp => self.arp_added = true,
                    Edit::LoadStart => self.arp_added = false,
                    _ => {}
                }
                // The MIDI-FX and demo searches record every edit as one kind:
                // which kinds a key saw is part of the state, and nine kinds
                // would multiply it past what a test can exhaust.
                let kind = match self.scn {
                    Scenario::Fx => Kind::Fx,
                    Scenario::Demo => Kind::Demo,
                    _ => e.kind(),
                };
                for kinds in self.held.values_mut() {
                    kinds.insert(kind);
                }
                // Invariant for the demo phrase player: once the demo is
                // stopped (by DemoStop or by any routing edit) and all keys
                // are released, nothing sounds at all.
                if self.scn == Scenario::Demo && self.held.is_empty() && self.engine.demo.is_none() {
                    return self.sounding.iter().map(|s| (s.0, s.1)).collect();
                }
                Vec::new()
            }
            Step::Tick => {
                if self.scn == Scenario::Demo && self.held.is_empty() && self.engine.demo.is_none() {
                    return self.sounding.iter().map(|s| (s.0, s.1)).collect();
                }
                Vec::new()
            }
        }
    }

    /// Everything that decides future behaviour, as a comparable key.
    fn fingerprint(&self) -> u64 {
        let routing = serde_json::to_string(&(
            &self.engine.graph.nodes,
            &self.engine.graph.edges,
            &self.engine.matrix.rules,
        ))
        .expect("serialize state");
        let arps: Vec<String> = self.engine.arp.iter().map(|a| a.fingerprint()).collect();
        let fx: Vec<i8> = self.engine.midi_fx.iter().map(|s| s.semis).collect();
        let demo = self.engine.demo.as_ref().map(|d| d.fingerprint());
        // Hashed: 100k states of a kilobyte of JSON each is memory the check
        // does not need to keep.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{routing}|{fx:?}|{:?}|{:?}|{arps:?}|{demo:?}|{}", self.held, self.sounding, self.arp_added).hash(&mut h);
        h.finish()
    }
}

fn describe(step: Step) -> String {
    match step {
        Step::On(k) => format!("NoteOn {k}"),
        Step::Off(k) => format!("NoteOff {k}"),
        Step::Edit(Edit::Split(p)) => format!("SetNodeSplit {p:?}"),
        Step::Edit(Edit::ZoneOctave { zone, octave }) => format!("SetZoneOctave zone {zone} {octave:+}"),
        Step::Edit(Edit::Rule { dest, transpose, enabled }) => {
            let name = if dest == SURGE_A { "Surge A" } else { "Surge B" };
            format!("MatrixRule Keys→{name} transpose {transpose:+} enabled {enabled}")
        }
        Step::Edit(Edit::Unplug(id)) => format!("Disconnect cable {id}"),
        Step::Edit(Edit::Plug { id, port, dest }) => {
            let name = if dest == SURGE_A { "Surge A" } else { "Surge B" };
            let port = if port == 0 { "LOW/main" } else { "HIGH" };
            format!("Connect cable {id} Keys {port} → {name}")
        }
        Step::Edit(Edit::Remove(id)) => match id {
            ARP => "RemoveNode Arp".to_string(),
            SURGE_A => "RemoveNode Surge A".to_string(),
            _ => "RemoveNode Surge B".to_string(),
        },
        Step::Edit(Edit::LoadStart) => "LoadGraph start patch".to_string(),
        Step::Edit(Edit::Scale(None)) => "SetKeysScale off".to_string(),
        Step::Edit(Edit::Scale(Some(c))) => format!("SetKeysScale {}", c.label()),
        Step::Edit(Edit::AddArp) => "AddNode Arp (wired into the rig)".to_string(),
        Step::Edit(Edit::ArpParam { param, pct }) => format!("SetParam arp {param} = {pct}%"),
        Step::Edit(Edit::Wire { id, from, to }) => format!("Connect cable {id} node {from} → node {to}"),
        Step::Edit(Edit::FxSemis(n)) => format!("SetParam MIDI-FX transpose {n:+}"),
        Step::Edit(Edit::RuleFxIn { enabled }) => format!("MatrixRule Keys→MIDI-FX enabled {enabled}"),
        Step::Edit(Edit::RuleFxOut { transpose, enabled }) => {
            format!("MatrixRule MIDI-FX→Surge A transpose {transpose:+} enabled {enabled}")
        }
        Step::Edit(Edit::PlugFx(8)) => "Connect cable 8 Keys → MIDI-FX".to_string(),
        Step::Edit(Edit::PlugFx(_)) => "Connect cable 9 MIDI-FX → Surge A".to_string(),
        Step::Edit(Edit::RemoveFx) => "RemoveNode MIDI-FX".to_string(),
        Step::Edit(Edit::Source { keyboard }) => format!("SetRigSource Surge A keyboard {keyboard}"),
        Step::Edit(Edit::DemoPlay(kind)) => format!("DemoPlay rig 0 {kind:?}"),
        Step::Edit(Edit::DemoStop) => "DemoStop".to_string(),
        Step::Tick => "Tick (time passes)".to_string(),
    }
}

fn replay(path: &[Step], scn: Scenario) -> Run {
    let mut run = Run::new(scn);
    for &s in path {
        run.step(s);
    }
    run
}

/// Breadth-first search to a fixpoint, or until `max_states` (then
/// `complete` is false and the caller must fail — a bound never passes).
pub(super) fn explore(max_states: usize) -> Report {
    let mut alphabet: Vec<Step> = NOTES.iter().flat_map(|&k| [Step::On(k), Step::Off(k)]).collect();
    alphabet.extend(EDITS.iter().map(|&e| Step::Edit(e)));
    alphabet.push(Step::Edit(Edit::LoadStart));
    explore_with(&alphabet, max_states, Scenario::Routing)
}

/// The arp scenario (issue #90): the plain rack plus time, the arp unit and
/// the scale snap. Smaller routing edit set than `EDITS` (the routing
/// scenario already covers those exhaustively) so the product stays finite.
pub(super) fn explore_arp(max_states: usize) -> Report {
    let mut alphabet: Vec<Step> = NOTES.iter().flat_map(|&k| [Step::On(k), Step::Off(k)]).collect();
    alphabet.push(Step::Tick);
    alphabet.extend(ARP_EDITS.iter().map(|&e| Step::Edit(e)));
    alphabet.push(Step::Edit(Edit::LoadStart));
    explore_with(&alphabet, max_states, Scenario::Arp)
}

/// The plain routing edits with a MIDI strip in the chain (issue #86).
pub(super) fn explore_routing_with_strip(max_states: usize) -> Report {
    let mut alphabet: Vec<Step> = NOTES.iter().flat_map(|&k| [Step::On(k), Step::Off(k)]).collect();
    alphabet.extend(EDITS.iter().map(|&e| Step::Edit(e)));
    alphabet.push(Step::Edit(Edit::LoadStart));
    explore_with(&alphabet, max_states, Scenario::RoutingWithStrip)
}

/// The MIDI strip's own edits (issue #86).
pub(super) fn explore_fx(max_states: usize) -> Report {
    let mut alphabet: Vec<Step> = NOTES.iter().flat_map(|&k| [Step::On(k), Step::Off(k)]).collect();
    alphabet.extend(FX_EDITS.iter().map(|&e| Step::Edit(e)));
    alphabet.push(Step::Edit(Edit::LoadStart));
    explore_with(&alphabet, max_states, Scenario::Fx)
}

/// The rig demo phrase player (issue #200): demo start/stop, time, held key,
/// and routing edits of every kind. Invariant: whenever the demo is stopped
/// and all keys are released, nothing sounds.
pub(super) fn explore_demo(max_states: usize) -> Report {
    let mut alphabet: Vec<Step> = vec![Step::On(60), Step::Off(60), Step::Tick];
    alphabet.extend(DEMO_EDITS.iter().map(|&e| Step::Edit(e)));
    alphabet.push(Step::Edit(Edit::LoadStart));
    explore_with(&alphabet, max_states, Scenario::Demo)
}

fn explore_with(alphabet: &[Step], max_states: usize, scn: Scenario) -> Report {
    let start = Run::new(scn);
    let mut seen: HashSet<u64> = HashSet::from([start.fingerprint()]);
    let mut queue: VecDeque<Vec<Step>> = VecDeque::from([Vec::new()]);
    let mut stuck: Vec<Stuck> = Vec::new();

    let mut complete = true;
    'search: while let Some(path) = queue.pop_front() {
        let here = replay(&path, scn);
        for &step in alphabet {
            if !here.enabled(step) {
                continue;
            }
            let kinds = match step {
                Step::Off(k) => here.held[&k].clone(),
                _ => BTreeSet::new(),
            };
            let mut next = replay(&path, scn);
            let left = next.step(step);
            let mut trace = path.clone();
            trace.push(step);
            if !left.is_empty() {
                // BFS order: the first hit per kind-set is a shortest one.
                if !stuck.iter().any(|s| s.kinds == kinds) {
                    stuck.push(Stuck { kinds, trace: trace.iter().map(|&s| describe(s)).collect(), left });
                }
                continue; // don't explore past a violation
            }
            if seen.insert(next.fingerprint()) {
                if seen.len() > max_states {
                    complete = false;
                    break 'search;
                }
                queue.push_back(trace);
            }
        }
    }
    Report { states: seen.len(), complete, stuck }
}

/// Kind-sets whose stuck notes are open, reported findings (see the skill).
/// Any stuck note whose kinds contain a KNOWN set is attributed to it; every
/// other stuck note fails the check. A fix must remove its entry.
pub(super) const KNOWN: &[&[Kind]] = &[];

#[test]
fn held_notes_are_released_with_the_arp_and_the_scale_snap() {
    let report = explore_arp(20_000);
    let reach = if report.complete { "explored to fixpoint" } else { "explored, STOPPED AT BOUND" };
    eprintln!("held-notes (arp): {} reachable states {reach}", report.states);
    for s in &report.stuck {
        eprintln!("VIOLATION {:?}: still sounding (instrument, pitch) {:?}", s.kinds, s.left);
        for (i, line) in s.trace.iter().enumerate() {
            eprintln!("  {}. {line}", i + 1);
        }
    }
    assert!(report.stuck.is_empty(), "stuck notes with the arp (traces above)");
    assert!(report.complete, "state bound hit before fixpoint: the arp alphabet's state space grew");
}

#[test]
fn held_notes_are_released_under_every_edit_with_a_midi_strip() {
    for (what, report) in [("routing edits", explore_routing_with_strip(400_000)), ("strip edits", explore_fx(400_000))] {
        let reach = if report.complete { "explored to fixpoint" } else { "explored, STOPPED AT BOUND" };
        eprintln!("held-notes (MIDI strip, {what}): {} reachable states {reach}", report.states);
        for s in &report.stuck {
            eprintln!("VIOLATION {:?}: still sounding (instrument, pitch) {:?}", s.kinds, s.left);
            for (i, line) in s.trace.iter().enumerate() {
                eprintln!("  {}. {line}", i + 1);
            }
        }
        assert!(report.stuck.is_empty(), "stuck notes with a MIDI strip, {what} (traces above)");
        assert!(report.complete, "state bound hit before fixpoint: the MIDI strip's {what} state space grew");
    }
}

#[test]
fn held_notes_are_released_under_every_routing_edit() {
    let report = explore(200_000);
    let reach = if report.complete { "explored to fixpoint" } else { "explored, STOPPED AT BOUND" };
    eprintln!("held-notes: {} reachable states {reach}", report.states);
    let known: Vec<BTreeSet<Kind>> = KNOWN.iter().map(|k| k.iter().copied().collect()).collect();
    let found: Vec<&BTreeSet<Kind>> = report.stuck.iter().map(|s| &s.kinds).collect();
    let mut new = Vec::new();
    let mut attributed = 0;
    for s in &report.stuck {
        let explained = known.iter().any(|k| k.is_subset(&s.kinds));
        // Print only root causes: kind-sets with no smaller stuck kind-set.
        let minimal = !found.iter().any(|f| *f != &s.kinds && f.is_subset(&s.kinds));
        if explained && !minimal {
            attributed += 1;
            continue;
        }
        let tag = if explained { "KNOWN FINDING" } else { "VIOLATION" };
        eprintln!("{tag} {:?}: still sounding (instrument, pitch) {:?}", s.kinds, s.left);
        for (i, line) in s.trace.iter().enumerate() {
            eprintln!("  {}. {line}", i + 1);
        }
        if !explained {
            new.push(s.kinds.clone());
        }
    }
    eprintln!("held-notes: {attributed} combined kind-set(s) attributed to known findings");
    assert!(new.is_empty(), "stuck notes under routing edits {new:?} (traces above)");
    assert!(report.complete, "state bound hit before fixpoint: the alphabet's state space grew");
    let fixed: Vec<_> = known.iter().filter(|k| !found.contains(k)).collect();
    assert!(fixed.is_empty(), "known finding(s) {fixed:?} no longer reproduce: remove from KNOWN");
}

#[test]
fn held_notes_and_demo_phrases_are_released_under_routing_edits() {
    let report = explore_demo(100_000);
    let reach = if report.complete { "explored to fixpoint" } else { "explored, STOPPED AT BOUND" };
    eprintln!("held-notes (demo): {} reachable states {reach}", report.states);
    for s in &report.stuck {
        eprintln!("VIOLATION {:?}: still sounding (instrument, pitch) {:?}", s.kinds, s.left);
        for (i, line) in s.trace.iter().enumerate() {
            eprintln!("  {}. {line}", i + 1);
        }
    }
    assert!(report.stuck.is_empty(), "stuck notes with the demo phrase player (traces above)");
    assert!(report.complete, "state bound hit before fixpoint: the demo alphabet's state space grew");
}
