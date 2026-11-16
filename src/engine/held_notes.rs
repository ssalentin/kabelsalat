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
//! Run + interpret: `.claude/skills/verify-held-notes/SKILL.md`.

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, Command, Graph, NodeKind, PortKind, PortRef};
use crate::matrix::SourceId;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

const KEYS: u32 = 0;
const SURGE_A: u32 = 1;
const SURGE_B: u32 = 3;
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    On(u8),
    Off(u8),
    Edit(Edit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    /// SetNodeSplit on the Keys node.
    Split(Option<u8>),
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
}

const EDITS: [Edit; 14] = [
    Edit::Split(None),
    Edit::Split(Some(60)),
    Edit::Split(Some(48)),
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

impl Edit {
    fn kind(self) -> Kind {
        match self {
            Edit::Split(_) => Kind::Split,
            Edit::Rule { .. } => Kind::Matrix,
            Edit::Unplug(_) | Edit::Plug { .. } => Kind::Cable,
            Edit::Remove(_) => Kind::Node,
            Edit::LoadStart => Kind::Load,
        }
    }

    fn command(self) -> Command {
        match self {
            Edit::Split(point) => Command::SetNodeSplit { node: KEYS, point },
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
            Edit::LoadStart => Command::LoadGraph { graph: start_graph() },
        }
    }
}

/// Keys(0) → Surge A(1) and Surge B(3) over MIDI cables 1 and 6, both into
/// Master(2). The demo patch's MIDI topology without its Delay nodes (audio
/// effects do not take part in note routing).
fn start_graph() -> Graph {
    let mut g = Graph::default();
    g.reserve(16, 16);
    let clap = || NodeKind::Clap { plugin_id: name_bytes("org.surge-synth-team.surge-xt") };
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

/// Engine + the observer's view of what is held and what is sounding.
struct Run {
    engine: GraphEngine,
    /// Held key → routing-edit kinds applied since its note-on.
    held: BTreeMap<u8, BTreeSet<Kind>>,
    /// (instrument, sounding pitch, key that started it).
    sounding: BTreeSet<(u32, u8, u8)>,
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
    fn new() -> Self {
        let (_bus, drain) = command_bus();
        let mut engine = GraphEngine::new(Graph::default(), drain, 48000.0, vec![]);
        engine.apply(Command::LoadGraph { graph: start_graph() });
        engine.note_log.clear();
        Run { engine, held: BTreeMap::new(), sounding: BTreeSet::new() }
    }

    fn enabled(&self, step: Step) -> bool {
        match step {
            Step::On(k) => !self.held.contains_key(&k),
            Step::Off(k) => self.held.contains_key(&k),
            Step::Edit(_) => true,
        }
    }

    /// Apply one step through the real engine. Returns the notes a released
    /// key left sounding (empty = fine).
    fn step(&mut self, step: Step) -> Vec<(u32, u8)> {
        let (cmd, key) = match step {
            Step::On(k) => (Command::NoteOn { note: k, velocity: 0.8, source: SourceId::Keys }, Some(k)),
            Step::Off(k) => (Command::NoteOff { note: k, source: SourceId::Keys }, Some(k)),
            Step::Edit(e) => (e.command(), None),
        };
        self.engine.note_log.clear();
        self.engine.apply(cmd);
        for &(dest, note, on) in &self.engine.note_log {
            if on {
                self.sounding.insert((dest, note, key.expect("note-on only from a key")));
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
                let left: Vec<(u32, u8)> =
                    self.sounding.iter().filter(|s| s.2 == k).map(|s| (s.0, s.1)).collect();
                left
            }
            Step::Edit(e) => {
                for kinds in self.held.values_mut() {
                    kinds.insert(e.kind());
                }
                Vec::new()
            }
        }
    }

    /// Everything that decides future behaviour, as a comparable key.
    fn fingerprint(&self) -> String {
        let routing = serde_json::to_string(&(
            &self.engine.graph.nodes,
            &self.engine.graph.edges,
            &self.engine.matrix.rules,
        ))
        .expect("serialize state");
        format!("{routing}|{:?}|{:?}", self.held, self.sounding)
    }
}

fn describe(step: Step) -> String {
    match step {
        Step::On(k) => format!("NoteOn {k}"),
        Step::Off(k) => format!("NoteOff {k}"),
        Step::Edit(Edit::Split(p)) => format!("SetNodeSplit {p:?}"),
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
        Step::Edit(Edit::Remove(_)) => "RemoveNode Surge B".to_string(),
        Step::Edit(Edit::LoadStart) => "LoadGraph start patch".to_string(),
    }
}

fn replay(path: &[Step]) -> Run {
    let mut run = Run::new();
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

    let start = Run::new();
    let mut seen: HashSet<String> = HashSet::from([start.fingerprint()]);
    let mut queue: VecDeque<Vec<Step>> = VecDeque::from([Vec::new()]);
    let mut stuck: Vec<Stuck> = Vec::new();

    let mut complete = true;
    'search: while let Some(path) = queue.pop_front() {
        let here = replay(&path);
        for &step in &alphabet {
            if !here.enabled(step) {
                continue;
            }
            let kinds = match step {
                Step::Off(k) => here.held[&k].clone(),
                _ => BTreeSet::new(),
            };
            let mut next = replay(&path);
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
