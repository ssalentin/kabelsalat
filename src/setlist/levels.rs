//! Show, set and song levels (#273), and the sets that group songs.
//!
//! Every rig, send bus and controller binding lives at one [`Level`]:
//!
//! - **Show**: no song or part switch touches it. A show-level rig always
//!   takes notes over the whole keyboard, its fader and knobs are never
//!   recalled, a show-level bus always takes its sends, and a show-level
//!   binding works in every part, also while its rig is out of the song.
//! - **Set**: as show, but only in the songs of one [`Set`]; outside it the rig
//!   takes no new notes, the bus no new signal, the binding moves nothing.
//! - **Song**: the parts decide. A rig plays and a bus takes signal in the
//!   parts that name it; a binding works while its destination's rig plays.
//!
//! Without a [`Pin`], a rig or bus that a part names is at song level and one
//! no part names is at show level; a binding follows its destination's rig,
//! except a mixer binding, which is at show level (it always worked). A
//! session saved before levels has no pins: everything no part names loads at
//! show level, and the rigs an older setlist's parts name keep playing only in
//! those parts, as they did.
//!
//! What is not live takes nothing new and keeps what it holds: held notes and
//! effect tails ring out, as at any part switch (#204).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{ParamValue, Part, PartRig, Setlist, Song, MAX_RIGS};
use crate::control::{Command, Graph, NodeId, RigZone};
use crate::matrix::{CcTarget, SourceId};

/// Stable handle of a [`Set`]; never reused within one setlist.
pub type SetId = u32;

/// A named group of consecutive songs, moved or skipped as a whole.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Set {
    pub id: SetId,
    pub name: String,
    /// Stepping (next/previous part or song) jumps over the set's songs;
    /// picking one of them directly still plays it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip: bool,
}

/// Where a rig, send bus or controller binding lives (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Show,
    Set(SetId),
    Song,
}

/// A controller binding by what it is: the controller `cc` from `source`
/// bound on `dest` (one matrix rule's `cc_map` entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Binding {
    pub source: SourceId,
    pub dest: NodeId,
    pub cc: u8,
}

/// A level someone chose for one item, overriding the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin<K> {
    pub of: K,
    pub level: Level,
}

/// Every chosen level of the show: rigs by 0-based rig index, buses by
/// their node id, bindings by [`Binding`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Levels {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rigs: Vec<Pin<u8>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buses: Vec<Pin<NodeId>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Pin<Binding>>,
}

impl Levels {
    pub fn is_empty(&self) -> bool {
        self.rigs.is_empty() && self.buses.is_empty() && self.bindings.is_empty()
    }
}

fn pinned<K: PartialEq>(pins: &[Pin<K>], of: &K) -> Option<Level> {
    pins.iter().find(|p| p.of == *of).map(|p| p.level)
}

fn pin<K: PartialEq>(pins: &mut Vec<Pin<K>>, of: K, level: Level) {
    match pins.iter_mut().find(|p| p.of == of) {
        Some(p) => p.level = level,
        None => pins.push(Pin { of, level }),
    }
}

/// What a part switch sets besides tempo, rig faders and knobs.
#[derive(Clone, Debug, PartialEq)]
pub struct Gates {
    pub zones: [RigZone; MAX_RIGS],
    /// Buses that take no new signal in this part.
    pub closed_buses: Vec<NodeId>,
    /// Each pinned binding and whether it works in this part. Unpinned
    /// bindings keep the engine's default.
    pub bindings: Vec<(Binding, bool)>,
}

impl Gates {
    /// The commands that put these gates into the engine, in order.
    pub fn commands(&self) -> Vec<Command> {
        let mut out = vec![Command::SetRigZones { zones: self.zones }, Command::ClearShowGates];
        out.extend(self.closed_buses.iter().map(|&bus| Command::SetBusLive { bus, live: false }));
        out.extend(self.bindings.iter().map(|&(b, live)| Command::SetBindingLive {
            source: b.source,
            dest: b.dest,
            cc: b.cc,
            live,
        }));
        out
    }
}

/// Does something at `level` take part in `song`? `named`: the part names
/// it (a song-level rig or bus).
fn live_in(song: &Song, level: Level, named: bool) -> bool {
    match level {
        Level::Show => true,
        Level::Set(id) => song.set == Some(id),
        Level::Song => named,
    }
}

impl Setlist {
    pub fn set(&self, id: SetId) -> Option<&Set> {
        self.sets.iter().find(|s| s.id == id)
    }

    /// The set song `song` belongs to.
    pub fn set_of(&self, song: usize) -> Option<&Set> {
        self.songs.get(song).and_then(|s| s.set).and_then(|id| self.set(id))
    }

    fn named_rig(&self, rig: u8) -> bool {
        self.songs.iter().flat_map(|s| &s.parts).any(|p| p.rigs.iter().any(|r| r.rig == rig))
    }

    fn named_bus(&self, bus: NodeId) -> bool {
        self.songs.iter().flat_map(|s| &s.parts).any(|p| p.buses.contains(&bus))
    }

    pub fn rig_level(&self, rig: u8) -> Level {
        pinned(&self.levels.rigs, &rig).unwrap_or(if self.named_rig(rig) { Level::Song } else { Level::Show })
    }

    pub fn bus_level(&self, bus: NodeId) -> Level {
        pinned(&self.levels.buses, &bus).unwrap_or(if self.named_bus(bus) { Level::Song } else { Level::Show })
    }

    /// A binding's level: its pin, else show for a mixer binding or one on a
    /// master or aux bus (`follows_no_rig`), else the level of its rig.
    pub fn binding_level(&self, b: Binding, target: CcTarget, graph: &Graph) -> Level {
        if let Some(l) = pinned(&self.levels.bindings, &b) {
            return l;
        }
        match graph.node(b.dest) {
            Some(n) if target != CcTarget::Mix && !follows_no_rig(graph, b.dest) => self.rig_level(n.rig),
            _ => Level::Show,
        }
    }

    /// Does binding `b` move anything in the active part? The engine decides
    /// the same from the gates (`Command::SetBindingLive` for pinned ones,
    /// its zone rule for the others); the app's knob mirror asks this.
    pub fn binding_works(&self, b: Binding, target: CcTarget, graph: &Graph) -> bool {
        let zones = self.active_zones();
        match pinned(&self.levels.bindings, &b) {
            Some(level) => self.pinned_binding_live(b, level, graph, &zones),
            None if target == CcTarget::Mix || follows_no_rig(graph, b.dest) => true,
            None => graph.node(b.dest).map_or(true, |n| zones.get(n.rig as usize).is_none_or(|z| z.live)),
        }
    }

    /// A pinned binding in the active part. At song level a binding on an
    /// aux bus works while the bus takes signal, any other while its rig plays.
    fn pinned_binding_live(&self, b: Binding, level: Level, graph: &Graph, zones: &[RigZone; MAX_RIGS]) -> bool {
        match level {
            Level::Song if graph.is_bus(b.dest) => {
                let named = self.current().is_some_and(|(_, p, _)| p.buses.contains(&b.dest));
                self.live_at(self.bus_level(b.dest), named)
            }
            Level::Song => graph.node(b.dest).map_or(true, |n| zones.get(n.rig as usize).is_none_or(|z| z.live)),
            level => self.live_at(level, false),
        }
    }

    pub fn binding_pinned(&self, b: Binding) -> bool {
        pinned(&self.levels.bindings, &b).is_some()
    }

    pub fn pin_rig(&mut self, rig: u8, level: Level) {
        pin(&mut self.levels.rigs, rig, level);
    }

    pub fn pin_bus(&mut self, bus: NodeId, level: Level) {
        pin(&mut self.levels.buses, bus, level);
    }

    pub fn pin_binding(&mut self, b: Binding, level: Level) {
        pin(&mut self.levels.bindings, b, level);
    }

    /// Does something at `level` take part in the active part? `named`: the
    /// part names it (a song-level rig or bus). With no active part
    /// everything is live.
    fn live_at(&self, level: Level, named: bool) -> bool {
        self.current().is_none_or(|(song, _, _)| live_in(song, level, named))
    }

    /// The zone table for the engine right now (every rig open without an
    /// active part).
    pub fn active_zones(&self) -> [RigZone; MAX_RIGS] {
        match self.current() {
            Some((song, part, _)) => self.zones_of(song, part),
            None => [RigZone::OPEN; MAX_RIGS],
        }
    }

    /// The zone table of `part` of `song`: a show-level rig, and a set-level
    /// rig inside its set, always takes the whole keyboard; a song-level rig
    /// takes what the part gives it, or nothing.
    pub fn zones_of(&self, song: &Song, part: &Part) -> [RigZone; MAX_RIGS] {
        let mut zones = [RigZone::OPEN; MAX_RIGS];
        for (rig, z) in zones.iter_mut().enumerate() {
            let rig = rig as u8;
            let entry = part.rigs.iter().find(|r| r.rig == rig);
            *z = match self.rig_level(rig) {
                Level::Song => entry.map_or(RigZone { live: false, ..RigZone::OPEN }, |r| r.to_zone()),
                level if live_in(song, level, false) => RigZone::OPEN,
                _ => RigZone { live: false, ..RigZone::OPEN },
            };
        }
        zones
    }

    /// Every gate of the active part (everything open without one).
    pub fn gates(&self, graph: &Graph) -> Gates {
        let named_bus = |bus: NodeId| self.current().is_some_and(|(_, p, _)| p.buses.contains(&bus));
        let closed_buses = graph
            .nodes
            .iter()
            .filter(|n| graph.is_bus(n.id))
            .filter(|n| !self.live_at(self.bus_level(n.id), named_bus(n.id)))
            .map(|n| n.id)
            .collect();
        let zones = self.active_zones();
        let bindings = self.levels.bindings.iter().map(|p| (p.of, self.pinned_binding_live(p.of, p.level, graph, &zones))).collect();
        Gates { zones, closed_buses, bindings }
    }

    /// The active part's rig entries a switch recalls (fader level): only
    /// song-level rigs; show- and set-level rigs are never touched.
    pub fn recalled_rigs(&self) -> Vec<PartRig> {
        let Some((_, part, _)) = self.current() else { return Vec::new() };
        part.rigs.iter().filter(|r| self.rig_level(r.rig) == Level::Song).copied().collect()
    }

    /// The active part's stored knobs whose unit is in a song-level rig.
    pub fn recalled_params(&self, graph: &Graph) -> Vec<ParamValue> {
        let Some((_, part, _)) = self.current() else { return Vec::new() };
        part.params
            .iter()
            .filter(|p| graph.node(p.node).is_some_and(|n| self.rig_level(n.rig) == Level::Song))
            .copied()
            .collect()
    }

    /// The active part's stored CLAP plugin knobs whose unit is in a song-level rig (#275).
    pub fn recalled_plugin_params(&self, graph: &Graph) -> Vec<ParamValue> {
        let Some((_, part, _)) = self.current() else { return Vec::new() };
        part.plugin_params
            .iter()
            .filter(|p| graph.node(p.node).is_some_and(|n| self.rig_level(n.rig) == Level::Song))
            .copied()
            .collect()
    }

    /// The active part's stored effect-chain bypasses whose effect is in a song-level rig (#275).
    pub fn recalled_fx(&self, graph: &Graph) -> Vec<super::FxBypass> {
        let Some((_, part, _)) = self.current() else { return Vec::new() };
        part.fx
            .iter()
            .filter(|f| graph.node(f.node).is_some_and(|n| self.rig_level(n.rig) == Level::Song))
            .copied()
            .collect()
    }

    /// Whether rig `rig` has an explicit level pin (Show, Set or Song).
    pub(super) fn rig_pinned(&self, rig: u8) -> Option<Level> {
        pinned(&self.levels.rigs, &rig)
    }

    /// True when stepping jumps over song `song` (its set is skipped).
    pub fn song_skipped(&self, song: usize) -> bool {
        self.set_of(song).is_some_and(|s| s.skip)
    }

    /// A new, empty set; returns its id.
    pub fn add_set(&mut self, name: &str) -> SetId {
        let id = self.sets.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let name = match name.trim() {
            "" => format!("Set {}", self.sets.len() + 1),
            n => n.to_string(),
        };
        self.sets.push(Set { id, name, skip: false });
        id
    }

    /// The rig a new song or part starts with: `preferred` (the rig in view),
    /// else the first rig, at song level. While no rig is at song level yet
    /// (a new setlist), the first of them that is not pinned. `None`: the
    /// part starts empty, so a show-level rig is never pulled into a song,
    /// and out of every other song, by accident.
    pub fn default_part_rig(&self, preferred: u8, rigs: usize) -> Option<u8> {
        let rigs = rigs.min(MAX_RIGS) as u8;
        let order = || std::iter::once(preferred).chain(0..rigs).filter(move |&r| r < rigs);
        if (0..rigs).any(|r| self.rig_level(r) == Level::Song) {
            order().find(|&r| self.rig_level(r) == Level::Song)
        } else {
            order().find(|&r| pinned(&self.levels.rigs, &r).is_none())
        }
    }

    /// After song `at` moved: inside another set's block it joins that set;
    /// it stays in its own set while it still touches one of its songs, and
    /// otherwise leaves it. Then every set is one block again.
    pub(super) fn settle_moved_song(&mut self, at: usize) {
        if self.sets.is_empty() || at >= self.songs.len() {
            return;
        }
        let set_at = |i: Option<usize>| i.and_then(|i| self.songs.get(i)).and_then(|s| s.set);
        let (prev, next) = (set_at(at.checked_sub(1)), set_at(Some(at + 1)));
        let own = self.songs[at].set;
        self.songs[at].set = match (prev, next) {
            (Some(p), Some(n)) if p == n => Some(p),
            _ if own.is_some() && (prev == own || next == own) => own,
            _ => None,
        };
        self.group_sets();
    }

    /// Put song `song` into `set` (or into none), at the end of the set's
    /// songs, so a set stays one block. Returns the song's new index.
    pub fn assign_set(&mut self, song: usize, set: Option<SetId>) -> Option<usize> {
        if song >= self.songs.len() || set.is_some_and(|id| self.set(id).is_none()) {
            return None;
        }
        let mut order: Vec<usize> = (0..self.songs.len()).filter(|&i| i != song).collect();
        let at = match set {
            Some(id) => order.iter().rposition(|&i| self.songs[i].set == Some(id)).map(|p| p + 1),
            None => None,
        }
        .unwrap_or_else(|| order.iter().position(|&i| i > song).unwrap_or(order.len()));
        order.insert(at, song);
        self.songs[song].set = set;
        self.permute(&order);
        let moved = order.iter().position(|&i| i == song)?;
        let grouped = self.group_sets();
        Some(grouped.iter().position(|&i| i == moved).unwrap_or(moved))
    }

    /// Make exactly `songs` (indices) the members of set `id`; the set's
    /// block lands where its first member is.
    pub fn set_members(&mut self, id: SetId, songs: &[usize]) -> Result<(), String> {
        if self.set(id).is_none() {
            return Err(format!("no set with id {id}"));
        }
        if let Some(&bad) = songs.iter().find(|&&s| s >= self.songs.len()) {
            return Err(format!("no song at index {bad} (setlist has {})", self.songs.len()));
        }
        for (i, s) in self.songs.iter_mut().enumerate() {
            if songs.contains(&i) {
                s.set = Some(id);
            } else if s.set == Some(id) {
                s.set = None;
            }
        }
        self.group_sets();
        Ok(())
    }

    /// Move set `id` one block up or down (a block is a set, or one song in
    /// no set). Returns the new order (new index → old index) when it moved.
    pub fn move_set(&mut self, id: SetId, up: bool) -> Option<Vec<usize>> {
        let mut blocks: Vec<Vec<usize>> = Vec::new();
        for (i, s) in self.songs.iter().enumerate() {
            match blocks.last_mut() {
                Some(b) if s.set.is_some() && self.songs[b[0]].set == s.set => b.push(i),
                _ => blocks.push(vec![i]),
            }
        }
        let at = blocks.iter().position(|b| self.songs[b[0]].set == Some(id))?;
        let other = if up { at.checked_sub(1)? } else { at + 1 };
        if other >= blocks.len() {
            return None;
        }
        blocks.swap(at, other);
        let order: Vec<usize> = blocks.into_iter().flatten().collect();
        self.permute(&order);
        self.order_sets_by_songs();
        Some(order)
    }

    /// Remove set `id`: its songs stay, in no set, and whatever was pinned
    /// to it goes back to its default level.
    pub fn remove_set(&mut self, id: SetId) -> bool {
        let before = self.sets.len();
        self.sets.retain(|s| s.id != id);
        for s in &mut self.songs {
            if s.set == Some(id) {
                s.set = None;
            }
        }
        self.levels.rigs.retain(|p| p.level != Level::Set(id));
        self.levels.buses.retain(|p| p.level != Level::Set(id));
        self.levels.bindings.retain(|p| p.level != Level::Set(id));
        self.sets.len() != before
    }

    /// Reorder the songs: position `i` gets the song that was at `order[i]`.
    fn permute(&mut self, order: &[usize]) {
        let mut old: Vec<Option<super::Song>> = std::mem::take(&mut self.songs).into_iter().map(Some).collect();
        self.songs = order.iter().filter_map(|&i| old.get_mut(i).and_then(Option::take)).collect();
        if let Some(pos) = &mut self.active {
            if let Some(new) = order.iter().position(|&i| i == pos.song) {
                pos.song = new;
            }
        }
    }

    /// Gather each set's songs at the position of its first song, keeping
    /// their order, and list the sets in song order. Returns the order it
    /// applied (new index → old index).
    pub(super) fn group_sets(&mut self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.songs.len());
        let mut placed = vec![false; self.songs.len()];
        for i in 0..self.songs.len() {
            if placed[i] {
                continue;
            }
            match self.songs[i].set {
                None => {
                    placed[i] = true;
                    order.push(i);
                }
                set => {
                    for j in i..self.songs.len() {
                        if !placed[j] && self.songs[j].set == set {
                            placed[j] = true;
                            order.push(j);
                        }
                    }
                }
            }
        }
        if order.iter().enumerate().any(|(i, &o)| i != o) {
            self.permute(&order);
        }
        self.order_sets_by_songs();
        order
    }

    fn order_sets_by_songs(&mut self) {
        let first = |id: SetId| self.songs.iter().position(|s| s.set == Some(id)).unwrap_or(usize::MAX);
        let mut keyed: Vec<(usize, Set)> = self.sets.drain(..).map(|s| (0, s)).collect();
        for (k, s) in &mut keyed {
            *k = first(s.id);
        }
        keyed.sort_by_key(|(k, _)| *k);
        self.sets = keyed.into_iter().map(|(_, s)| s).collect();
    }

    /// Load-time repair of sets and pins (called by `normalize`).
    pub(super) fn normalize_levels(&mut self) {
        let mut seen = Vec::new();
        self.sets.retain(|s| {
            let fresh = !seen.contains(&s.id);
            seen.push(s.id);
            fresh
        });
        for (i, s) in self.sets.iter_mut().enumerate() {
            if s.name.trim().is_empty() {
                s.name = format!("Set {}", i + 1);
            }
        }
        let known = |l: &Level| !matches!(l, Level::Set(id) if !seen.contains(id));
        for s in &mut self.songs {
            if s.set.is_some_and(|id| !seen.contains(&id)) {
                s.set = None;
            }
        }
        self.levels.rigs.retain(|p| known(&p.level) && (p.of as usize) < MAX_RIGS);
        self.levels.buses.retain(|p| known(&p.level));
        self.levels.bindings.retain(|p| known(&p.level));
        dedup_pins(&mut self.levels.rigs);
        dedup_pins(&mut self.levels.buses);
        dedup_pins(&mut self.levels.bindings);
        self.group_sets();
    }

    /// Follow a rig add/delete or a node renumbering (rule 44).
    pub(super) fn remap_levels(&mut self, rig_map: &impl Fn(u8) -> Option<u8>, node_map: &impl Fn(NodeId) -> Option<NodeId>) {
        let source_map = |s: SourceId| match s {
            SourceId::MidiFx(n) => node_map(n).map(SourceId::MidiFx),
            other => Some(other),
        };
        self.levels.rigs = self.levels.rigs.iter().filter_map(|p| Some(Pin { of: rig_map(p.of)?, level: p.level })).collect();
        self.levels.buses = self.levels.buses.iter().filter_map(|p| Some(Pin { of: node_map(p.of)?, level: p.level })).collect();
        self.levels.bindings = self
            .levels
            .bindings
            .iter()
            .filter_map(|p| {
                let of = Binding { source: source_map(p.of.source)?, dest: node_map(p.of.dest)?, ..p.of };
                Some(Pin { of, level: p.level })
            })
            .collect();
        dedup_pins(&mut self.levels.rigs);
        dedup_pins(&mut self.levels.buses);
        dedup_pins(&mut self.levels.bindings);
    }

    /// Drop pins of rigs and nodes `graph` no longer has.
    pub(super) fn prune_levels(&mut self, graph: &Graph) {
        let has_rig = |r: u8| graph.nodes.iter().any(|n| n.rig == r);
        self.levels.rigs.retain(|p| has_rig(p.of));
        self.levels.buses.retain(|p| graph.is_bus(p.of));
        self.levels.bindings.retain(|p| graph.node(p.of.dest).is_some());
    }

    /// The level as agent tools read it: `"show"`, `"song"` or `"set"` with
    /// the set's name.
    pub fn level_json(&self, level: Level) -> Value {
        match level {
            Level::Show => json!({"level": "show"}),
            Level::Song => json!({"level": "song"}),
            Level::Set(id) => json!({"level": "set", "set": self.set(id).map(|s| s.name.as_str())}),
        }
    }

    /// A short label: "Show", "Song" or the set's name.
    pub fn level_label(&self, level: Level) -> String {
        match level {
            Level::Show => "Show".into(),
            Level::Song => "Song".into(),
            Level::Set(id) => self.set(id).map_or_else(|| "Set".into(), |s| s.name.clone()),
        }
    }

    /// Sets and levels for `get_setlist`. `bindings`: every controller
    /// binding of the matrix with its target; `bus_name` names a bus node.
    pub fn show_json(&self, graph: &Graph, bindings: &[(Binding, CcTarget)], bus_name: impl Fn(NodeId) -> String) -> Value {
        let sets: Vec<Value> = self
            .sets
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let songs: Vec<usize> = (0..self.songs.len()).filter(|&j| self.songs[j].set == Some(s.id)).collect();
                json!({"index": i, "name": s.name, "skip": s.skip, "songs": songs})
            })
            .collect();
        let rig_count = graph.nodes.iter().map(|n| n.rig as usize + 1).max().unwrap_or(0).min(MAX_RIGS);
        let rigs: Vec<Value> = (0..rig_count as u8)
            .map(|r| {
                let mut v = self.level_json(self.rig_level(r));
                v["rig"] = json!(r);
                v["pinned"] = json!(pinned(&self.levels.rigs, &r).is_some());
                v
            })
            .collect();
        let buses: Vec<Value> = graph
            .nodes
            .iter()
            .filter(|n| graph.is_bus(n.id))
            .map(|n| {
                let mut v = self.level_json(self.bus_level(n.id));
                v["bus"] = json!(n.id);
                v["name"] = json!(bus_name(n.id));
                v["pinned"] = json!(pinned(&self.levels.buses, &n.id).is_some());
                v
            })
            .collect();
        let binds: Vec<Value> = bindings
            .iter()
            .map(|&(b, target)| {
                let mut v = self.level_json(self.binding_level(b, target, graph));
                v["source"] = json!(source_str(b.source));
                v["node"] = json!(b.dest);
                v["cc"] = json!(b.cc);
                v["target"] = json!(target.as_str());
                v["pinned"] = json!(self.binding_pinned(b));
                v
            })
            .collect();
        json!({"sets": sets, "levels": {"rigs": rigs, "buses": buses, "bindings": binds}})
    }

    /// Resolve a set named in a tool call: its name (case-insensitive) or
    /// its 0-based index in `sets`.
    fn find_set(&self, v: &Value) -> Result<SetId, String> {
        let index = |i: u64| self.sets.get(i as usize);
        let found = match v {
            Value::String(name) => self
                .sets
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
                .or_else(|| name.trim().parse().ok().and_then(index)),
            Value::Number(n) => n.as_u64().and_then(index),
            _ => None,
        };
        found.map(|s| s.id).ok_or_else(|| {
            let names: Vec<&str> = self.sets.iter().map(|s| s.name.as_str()).collect();
            format!("no set {v} (sets: {})", if names.is_empty() { "none yet, set_setlist_set makes one".into() } else { names.join(", ") })
        })
    }

    /// `set_level`: pin one rig, bus or binding to show, song or a set.
    /// `bindings` lists the matrix's bindings, to refuse one that is not there.
    pub fn apply_set_level(&mut self, graph: &Graph, bindings: &[(Binding, CcTarget)], args: &Value) -> Result<String, String> {
        let mut next = self.clone();
        let msg = next.set_level_from(graph, bindings, args)?;
        *self = next;
        Ok(msg)
    }

    fn set_level_from(&mut self, graph: &Graph, bindings: &[(Binding, CcTarget)], args: &Value) -> Result<String, String> {
        let level = match args.get("level").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
            Some("show") => Level::Show,
            Some("song") => Level::Song,
            Some("set") => Level::Set(self.find_set(args.get("set").ok_or("level 'set' needs set (its name or index)")?)?),
            other => return Err(format!("level must be 'show', 'song' or 'set', not {other:?}")),
        };
        let label = self.level_label(level);
        if let Some(r) = args.get("rig").and_then(Value::as_u64) {
            let rigs = graph.nodes.iter().map(|n| n.rig as u64 + 1).max().unwrap_or(0);
            if r >= rigs || r as usize >= MAX_RIGS {
                return Err(format!("no rig {r} (rigs are 0..{})", rigs.saturating_sub(1)));
            }
            self.pin_rig(r as u8, level);
            return Ok(format!("rig {r} is at {label} level"));
        }
        if let Some(b) = args.get("bus").and_then(Value::as_u64) {
            if !graph.is_bus(b as NodeId) {
                return Err(format!("node {b} is not an aux bus (get_sends lists them)"));
            }
            self.pin_bus(b as NodeId, level);
            return Ok(format!("bus {b} is at {label} level"));
        }
        if let Some(dest) = args.get("node").and_then(Value::as_u64) {
            let dest = dest as NodeId;
            let cc = args.get("cc").and_then(Value::as_u64).filter(|&c| c <= 127).ok_or("a binding needs cc (0..127) with its node")? as u8;
            let source = parse_source(args.get("source").and_then(Value::as_str).unwrap_or("keys"))?;
            let b = Binding { source, dest, cc };
            if !bindings.iter().any(|(x, _)| *x == b) {
                return Err(format!("no binding of CC {cc} from {} on node {dest} (list_cc_maps lists them)", source_str(source)));
            }
            self.pin_binding(b, level);
            return Ok(format!("CC {cc} on node {dest} is at {label} level"));
        }
        Err("give rig, bus, or a binding's node and cc (and source, default keys)".into())
    }

    /// `set_setlist_set`: make, rename, fill, skip, move or remove a set.
    /// Nothing changes when any part of the call is refused.
    pub fn apply_setlist_set(&mut self, args: &Value) -> Result<String, String> {
        let mut next = self.clone();
        let msg = next.setlist_set_from(args)?;
        *self = next;
        Ok(msg)
    }

    fn setlist_set_from(&mut self, args: &Value) -> Result<String, String> {
        let id = match args.get("set") {
            Some(v) if !v.is_null() => self.find_set(v)?,
            _ => {
                let name = args.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty());
                self.add_set(name.ok_or("name is required when making a set")?)
            }
        };
        if args.get("remove").and_then(Value::as_bool) == Some(true) {
            self.remove_set(id);
            return Ok("set removed; its songs stay".into());
        }
        if let (Some(name), Some(_)) = (args.get("name").and_then(Value::as_str), args.get("set")) {
            let name = name.trim();
            if name.is_empty() {
                return Err("set name cannot be empty".into());
            }
            if let Some(s) = self.sets.iter_mut().find(|s| s.id == id) {
                s.name = name.to_string();
            }
        }
        if let Some(songs) = args.get("songs").filter(|v| !v.is_null()) {
            self.set_members(id, &song_list(songs)?)?;
        }
        if let Some(skip) = args.get("skip").and_then(Value::as_bool) {
            if let Some(s) = self.sets.iter_mut().find(|s| s.id == id) {
                s.skip = skip;
            }
        }
        match args.get("move").and_then(Value::as_str) {
            Some("up") => {
                self.move_set(id, true).ok_or("the set is first already")?;
            }
            Some("down") => {
                self.move_set(id, false).ok_or("the set is last already")?;
            }
            Some(other) => return Err(format!("move must be 'up' or 'down', not '{other}'")),
            None => {}
        }
        let name = self.set(id).map(|s| s.name.clone()).unwrap_or_default();
        Ok(format!("set '{name}' updated"))
    }
}

/// Song indices as a JSON array or a list string ("0, 1, 2" or "[0,1,2]").
fn song_list(v: &Value) -> Result<Vec<usize>, String> {
    let bad = || format!("songs must be 0-based song indices, like \"0,1,2\", not {v}");
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_u64().map(|n| n as usize).ok_or_else(bad)).collect(),
        Value::String(s) => s
            .trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace())
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.parse::<usize>().map_err(|_| bad()))
            .collect(),
        _ => Err(bad()),
    }
}

/// A master or aux bus: a mix control that no rig's zone gates.
fn follows_no_rig(graph: &Graph, node: NodeId) -> bool {
    graph.is_bus(node) || graph.node(node).is_some_and(|n| matches!(n.kind, crate::control::NodeKind::MasterOut))
}

fn dedup_pins<K: PartialEq>(pins: &mut Vec<Pin<K>>) {
    let mut out: Vec<Pin<K>> = Vec::with_capacity(pins.len());
    for p in pins.drain(..) {
        if !out.iter().any(|q| q.of == p.of) {
            out.push(p);
        }
    }
    *pins = out;
}

/// A binding source as tools spell it: `keys`, `hw:<device>`,
/// `virtual:<port>`, `file_player:<id>` or `midi_fx:<node>`.
pub fn source_str(s: SourceId) -> String {
    match s {
        SourceId::Keys => "keys".into(),
        SourceId::Hardware(d) => format!("hw:{d}"),
        SourceId::Virtual(p) => format!("virtual:{p}"),
        SourceId::FilePlayer(p) => format!("file_player:{p}"),
        SourceId::MidiFx(n) => format!("midi_fx:{n}"),
    }
}

fn parse_source(s: &str) -> Result<SourceId, String> {
    let s = s.trim().to_ascii_lowercase();
    let (tag, arg) = s.split_once(':').map_or((s.as_str(), None), |(t, a)| (t, Some(a)));
    let n = || arg.and_then(|a| a.parse::<u32>().ok()).ok_or_else(|| format!("source '{s}' needs a number after ':'"));
    match tag {
        "keys" | "keyboard" | "laptop" => Ok(SourceId::Keys),
        "hw" | "hardware" => Ok(SourceId::Hardware(n()?)),
        "virtual" => Ok(SourceId::Virtual(n()?)),
        "file_player" => Ok(SourceId::FilePlayer(n()?)),
        "midi_fx" => Ok(SourceId::MidiFx(n()?)),
        _ => Err(format!("unknown source '{s}' (keys | hw:<device> | virtual:<port> | file_player:<id> | midi_fx:<node>)")),
    }
}

/// The `buses` of one part in a tool call: aux bus node ids.
pub fn part_buses(part: &Value, graph: &Graph) -> Result<Vec<NodeId>, String> {
    let Some(list) = part.get("buses").and_then(Value::as_array) else { return Ok(Vec::new()) };
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        let b = v.as_u64().ok_or("part buses are aux bus node ids")? as NodeId;
        if !graph.is_bus(b) {
            return Err(format!("node {b} is not an aux bus (get_sends lists them)"));
        }
        if !out.contains(&b) {
            out.push(b);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, DspKind, NodeKind};
    use crate::setlist::{Part, PartRig, SetlistPos, Song};

    /// Three rigs (a Keys node and a master each) and one aux bus in rig 0.
    fn three_rigs() -> (Graph, NodeId) {
        let mut g = Graph::default();
        for rig in 0..3u8 {
            for kind in [NodeKind::Keys, NodeKind::MasterOut] {
                let n = g.add_node(kind, name_bytes("UNIT"));
                g.set_rig(n, rig);
            }
        }
        let bus = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("HALL"));
        (g, bus)
    }

    fn song(name: &str, rigs: &[u8]) -> Song {
        Song::new(name, vec![Part::new("Main", rigs.iter().map(|&r| PartRig::new(r)).collect())])
    }

    fn at(sl: &mut Setlist, song: usize) -> &mut Setlist {
        sl.active = Some(SetlistPos { song, part: 0 });
        sl
    }

    const OFF: RigZone = RigZone { live: false, ..RigZone::OPEN };

    /// A setlist as v0.2.0 wrote it (#204): songs and parts, nothing else.
    const V0_2_SETLIST: &str = r#"{"songs":[
        {"name":"Opener","bpm":118.0,"parts":[{"name":"Verse","rigs":[{"rig":1,"key_lo":0,"key_hi":127,"transpose":0,"level":0.9}]}]},
        {"name":"Ballad","parts":[{"name":"Main","rigs":[{"rig":2,"key_lo":36,"key_hi":96,"transpose":-12,"level":0.7}]}]}
    ],"active":{"song":0,"part":0},"next_cc":{"cc":64}}"#;

    #[test]
    fn a_setlist_saved_before_levels_plays_as_it_did_with_everything_else_at_show_level() {
        let (g, bus) = three_rigs();
        let mut sl: Setlist = serde_json::from_str(V0_2_SETLIST).unwrap();
        sl.normalize();
        assert!(sl.levels.is_empty() && sl.sets.is_empty(), "nothing is pinned, no set exists");
        assert_eq!(sl.rig_level(0), Level::Show, "a rig no part names is at show level");
        assert_eq!((sl.rig_level(1), sl.rig_level(2)), (Level::Song, Level::Song), "the rigs the parts name stay song by song");
        assert_eq!(sl.bus_level(bus), Level::Show);
        let pedal = Binding { source: SourceId::Hardware(0), dest: 0, cc: 7 };
        assert_eq!(sl.binding_level(pedal, CcTarget::Mix, &g), Level::Show);
        assert_eq!(sl.binding_level(pedal, CcTarget::Knob, &g), Level::Show, "a binding on a show-level rig is at show level");
        // The parts gate exactly as v0.2.0 did, and rig 0 now plays in both.
        let z = at(&mut sl, 0).active_zones();
        assert_eq!((z[0], z[1], z[2]), (RigZone::OPEN, RigZone::OPEN, OFF));
        let z = at(&mut sl, 1).active_zones();
        assert_eq!((z[0], z[1].live, z[2]), (RigZone::OPEN, false, RigZone { key_lo: 36, key_hi: 96, transpose: -12, ..RigZone::OPEN }));
        assert!(sl.gates(&g).closed_buses.is_empty());
        let saved = serde_json::to_string(&sl).unwrap();
        assert!(!saved.contains("levels") && !saved.contains("sets"), "an unchanged old setlist saves in the old shape: {saved}");
        assert!(Setlist::default().is_empty(), "a session without a setlist still writes none");
    }

    #[test]
    fn a_show_level_rig_is_untouched_by_every_switch_even_where_a_part_names_it() {
        let (g, _) = three_rigs();
        let mut sl = Setlist { songs: vec![song("A", &[0, 1]), song("B", &[1])], ..Setlist::default() };
        sl.songs[0].parts[0].rigs[0] = PartRig { rig: 0, key_lo: 60, key_hi: 72, transpose: 5, level: 0.2, ..PartRig::default() };
        sl.songs[0].parts[0].params.push(ParamValue { node: 0, param: 0, value: 0.3 });
        sl.songs[0].parts[0].params.push(ParamValue { node: 2, param: 0, value: 0.4 });
        sl.pin_rig(0, Level::Show);
        for s in 0..2 {
            assert_eq!(at(&mut sl, s).active_zones()[0], RigZone::OPEN, "song {s}: the whole keyboard, no transpose");
        }
        at(&mut sl, 0);
        assert_eq!(sl.recalled_rigs().iter().map(|r| r.rig).collect::<Vec<_>>(), vec![1], "its fader is never recalled");
        assert_eq!(sl.recalled_params(&g).iter().map(|p| p.node).collect::<Vec<_>>(), vec![2], "nor its knobs");
        sl.pin_rig(0, Level::Song);
        assert_eq!(sl.active_zones()[0], RigZone { key_lo: 60, key_hi: 72, transpose: 5, ..RigZone::OPEN }, "back at song level the part's entry plays again");
    }

    #[test]
    fn a_set_level_rig_and_bus_play_through_their_set_and_rest_outside_it() {
        let (g, bus) = three_rigs();
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[2]), song("C", &[1])], ..Setlist::default() };
        let acoustic = sl.add_set("Acoustic");
        sl.set_members(acoustic, &[0, 1]).unwrap();
        sl.pin_rig(0, Level::Set(acoustic));
        sl.pin_bus(bus, Level::Set(acoustic));
        for (s, inside) in [(0, true), (1, true), (2, false)] {
            at(&mut sl, s);
            assert_eq!(sl.active_zones()[0].live, inside, "song {s}");
            assert_eq!(sl.gates(&g).closed_buses.is_empty(), inside, "song {s}");
        }
    }

    #[test]
    fn a_song_level_bus_takes_signal_only_where_a_part_names_it() {
        let (g, bus) = three_rigs();
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[1])], ..Setlist::default() };
        assert_eq!(sl.bus_level(bus), Level::Show);
        sl.songs[1].parts[0].buses.push(bus);
        assert_eq!(sl.bus_level(bus), Level::Song, "a bus a part names is at song level");
        assert_eq!(at(&mut sl, 0).gates(&g).closed_buses, vec![bus]);
        assert!(at(&mut sl, 1).gates(&g).closed_buses.is_empty());
        sl.active = None;
        assert!(sl.gates(&g).closed_buses.is_empty(), "no active part: everything open");
    }

    #[test]
    fn pinned_bindings_are_sent_with_whether_they_work_in_the_part() {
        let (g, _) = three_rigs();
        let organ_rig_node = g.nodes.iter().find(|n| n.rig == 2).unwrap().id;
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[2])], ..Setlist::default() };
        let pedal = Binding { source: SourceId::Hardware(0), dest: organ_rig_node, cc: 11 };
        let swell = Binding { source: SourceId::Hardware(0), dest: organ_rig_node, cc: 4 };
        sl.pin_binding(pedal, Level::Show);
        sl.pin_binding(swell, Level::Song);
        assert_eq!(at(&mut sl, 0).gates(&g).bindings, vec![(pedal, true), (swell, false)], "the organ is out of song A");
        assert_eq!(at(&mut sl, 1).gates(&g).bindings, vec![(pedal, true), (swell, true)]);
        let cmds = sl.gates(&g).commands();
        assert!(matches!(cmds[0], Command::SetRigZones { .. }) && matches!(cmds[1], Command::ClearShowGates), "zones first, then a clean slate");
        assert!(cmds.iter().any(|c| matches!(c, Command::SetBindingLive { cc: 11, live: true, .. })));
    }

    #[test]
    fn sets_stay_one_block_move_and_skip_as_a_whole() {
        let names = |sl: &Setlist| sl.songs.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(" ");
        let mut sl = Setlist { songs: ["A", "B", "C", "D", "E"].iter().map(|n| song(n, &[1])).collect(), ..Setlist::default() };
        let one = sl.add_set("One");
        let two = sl.add_set("Two");
        sl.set_members(one, &[0, 3]).unwrap();
        assert_eq!(names(&sl), "A D B C E", "a set's songs are kept together, where its first song is");
        sl.active = Some(SetlistPos { song: 2, part: 0 }); // B
        assert_eq!(sl.assign_set(4, Some(two)), Some(4));
        assert_eq!(sl.assign_set(3, Some(two)), Some(4), "a song joins the end of its set's block");
        assert_eq!(names(&sl), "A D B E C");
        assert_eq!(sl.sets.iter().map(|s| s.id).collect::<Vec<_>>(), vec![one, two], "sets are listed in song order");
        let order = sl.move_set(two, true).unwrap();
        assert_eq!(names(&sl), "A D E C B", "the whole set moves past its neighbour");
        assert_eq!(order, vec![0, 1, 3, 4, 2]);
        assert_eq!(sl.active, Some(SetlistPos { song: 4, part: 0 }), "the active song follows the move");
        assert_eq!(sl.move_set(two, true).map(|_| names(&sl)).as_deref(), Some("E C A D B"));
        assert!(sl.move_set(two, true).is_none(), "first already");

        sl.sets.iter_mut().find(|s| s.id == one).unwrap().skip = true;
        sl.active = Some(SetlistPos { song: 1, part: 0 }); // C, in set Two
        assert_eq!(sl.next_part(), Some(SetlistPos { song: 4, part: 0 }), "stepping jumps over the skipped set");
        assert_eq!(sl.prev_song(), Some(SetlistPos { song: 1, part: 0 }));
        assert_eq!(sl.select(2, 0), Some(SetlistPos { song: 2, part: 0 }), "a skipped song can still be picked");

        sl.pin_rig(0, Level::Set(one));
        assert!(sl.remove_set(one));
        assert!(sl.songs.iter().all(|s| s.set != Some(one)), "its songs stay, in no set");
        assert_eq!(sl.rig_level(0), Level::Show, "what was pinned to it goes back to its default");
    }

    #[test]
    fn a_new_part_never_pulls_a_show_level_rig_into_a_song() {
        let mut sl = Setlist::default();
        assert_eq!(sl.default_part_rig(0, 3), Some(0), "a new setlist starts with the rig in view");
        sl.pin_rig(0, Level::Show);
        assert_eq!(sl.default_part_rig(0, 3), Some(1), "a pinned show rig is skipped");
        sl.levels.rigs.clear();
        sl.songs.push(song("A", &[1]));
        assert_eq!(sl.default_part_rig(0, 3), Some(1), "rig 0, at show level, stays out; a song-level rig is taken");
        let mut all_show = Setlist::default();
        (0..3).for_each(|r| all_show.pin_rig(r, Level::Show));
        assert_eq!(all_show.default_part_rig(0, 3), None, "every rig at show level: the part starts empty");
    }

    #[test]
    fn a_moved_song_joins_the_set_it_lands_in_and_sets_stay_blocks() {
        let names = |sl: &Setlist| sl.songs.iter().map(|s| format!("{}{}", s.name, if s.set.is_some() { "*" } else { "" })).collect::<Vec<_>>().join(" ");
        let mut sl = Setlist { songs: ["A", "B", "C"].iter().map(|n| song(n, &[1])).collect(), ..Setlist::default() };
        let set = sl.add_set("One");
        sl.set_members(set, &[0, 1]).unwrap();
        assert!(sl.move_song(2, 1));
        assert_eq!(names(&sl), "A* C* B*", "moved into the middle of a set, it joins it");
        assert!(sl.move_song(1, 0));
        assert_eq!(names(&sl), "C* A* B*", "at the block's edge it stays in its set");
        assert_eq!(sl.move_set(set, false), None, "the set is one block: nothing to move past");
        let mut sl = Setlist { songs: ["A", "B", "C"].iter().map(|n| song(n, &[1])).collect(), ..Setlist::default() };
        let set = sl.add_set("One");
        sl.set_members(set, &[0, 1]).unwrap();
        assert!(sl.move_song(0, 2));
        assert_eq!(names(&sl), "B* C A", "moved away from its set, it leaves it");
    }

    #[test]
    fn a_binding_on_a_master_or_a_bus_follows_no_rig() {
        let (g, bus) = three_rigs();
        let master2 = g.nodes.iter().find(|n| n.rig == 2 && matches!(n.kind, NodeKind::MasterOut)).unwrap().id;
        let keys2 = g.nodes.iter().find(|n| n.rig == 2 && matches!(n.kind, NodeKind::Keys)).unwrap().id;
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[2])], ..Setlist::default() };
        at(&mut sl, 0);
        let b = |dest| Binding { source: SourceId::Hardware(0), dest, cc: 7 };
        assert_eq!(sl.binding_level(b(master2), CcTarget::Knob, &g), Level::Show, "the master volume is no rig's");
        assert!(sl.binding_works(b(master2), CcTarget::Knob, &g), "it works while rig 2 is out of the song");
        assert!(!sl.binding_works(b(keys2), CcTarget::Knob, &g), "a knob of rig 2 waits");
        sl.pin_bus(bus, Level::Song);
        sl.pin_binding(b(bus), Level::Song);
        assert!(!sl.binding_works(b(bus), CcTarget::Knob, &g), "a song-level bus binding follows the bus, not the bus's rig");
        sl.songs[0].parts[0].buses.push(bus);
        assert!(sl.binding_works(b(bus), CcTarget::Knob, &g));
    }

    #[test]
    fn levels_and_sets_follow_a_rig_delete_and_a_renumbering() {
        let mut sl = Setlist { songs: vec![song("A", &[1, 2])], ..Setlist::default() };
        let set = sl.add_set("Encore");
        sl.set_members(set, &[0]).unwrap();
        sl.pin_rig(0, Level::Show);
        sl.pin_rig(2, Level::Set(set));
        sl.pin_bus(20, Level::Song);
        sl.songs[0].parts[0].buses.push(20);
        sl.pin_binding(Binding { source: SourceId::MidiFx(20), dest: 21, cc: 1 }, Level::Show);
        // Delete rig 0: rig 2 becomes rig 1; nodes 20 → 10, 21 → 11.
        let out = sl.remap(|r| r.checked_sub(1), |n| n.checked_sub(10));
        assert_eq!(out.levels.rigs, vec![Pin { of: 1, level: Level::Set(set) }]);
        assert_eq!(out.levels.buses, vec![Pin { of: 10, level: Level::Song }]);
        assert_eq!(out.songs[0].parts[0].buses, vec![10]);
        assert_eq!(out.levels.bindings[0].of, Binding { source: SourceId::MidiFx(10), dest: 11, cc: 1 });
        assert_eq!(out.songs[0].set, Some(set));
    }

    #[test]
    fn a_setlist_with_sets_and_levels_round_trips_and_repairs_bad_references() {
        let (g, bus) = three_rigs();
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[2])], ..Setlist::default() };
        let set = sl.add_set("Main set");
        sl.set_members(set, &[0, 1]).unwrap();
        sl.pin_rig(0, Level::Show);
        sl.pin_bus(bus, Level::Set(set));
        sl.pin_binding(Binding { source: SourceId::Keys, dest: 1, cc: 7 }, Level::Song);
        let json = serde_json::to_string(&sl).unwrap();
        let mut back: Setlist = serde_json::from_str(&json).unwrap();
        back.normalize();
        assert_eq!(back, sl);
        back.validate(&g).unwrap();
        // A hand-edited file naming a set that is not there.
        let broken = json.replace(&format!("\"set\":{set}"), "\"set\":99");
        let mut b: Setlist = serde_json::from_str(&broken).unwrap();
        assert!(b.validate(&g).is_err(), "validate names the bad set reference");
        b.normalize();
        assert!(b.songs.iter().all(|s| s.set.is_none()) && b.validate(&g).is_ok());
    }

    #[test]
    fn the_level_tools_pin_report_and_refuse_in_plain_words() {
        let (g, bus) = three_rigs();
        let pedal = Binding { source: SourceId::Hardware(0), dest: 1, cc: 11 };
        let bindings = [(pedal, CcTarget::Knob)];
        let mut sl = Setlist { songs: vec![song("A", &[1]), song("B", &[2])], ..Setlist::default() };
        assert!(sl.apply_setlist_set(&json!({"songs": "0,1"})).is_err(), "a new set needs a name");
        assert!(sl.apply_setlist_set(&json!({"name": "Encore", "songs": "99"})).is_err());
        assert!(sl.sets.is_empty(), "a refused call leaves no half-made set behind");
        sl.apply_setlist_set(&json!({"name": "Acoustic", "songs": "[1]"})).unwrap();
        sl.apply_setlist_set(&json!({"set": "acoustic", "skip": true, "name": "Unplugged"})).unwrap();
        assert_eq!((sl.sets[0].name.as_str(), sl.sets[0].skip, sl.songs[1].set), ("Unplugged", true, Some(sl.sets[0].id)));
        sl.apply_set_level(&g, &bindings, &json!({"rig": 0, "level": "set", "set": "0"})).unwrap();
        assert_eq!(sl.rig_level(0), Level::Set(sl.sets[0].id), "a set by index");
        sl.apply_set_level(&g, &bindings, &json!({"bus": bus, "level": "song"})).unwrap();
        sl.apply_set_level(&g, &bindings, &json!({"node": 1, "cc": 11, "source": "hw:0", "level": "show"})).unwrap();
        assert_eq!(sl.binding_level(pedal, CcTarget::Knob, &g), Level::Show);
        let err = |args: Value| sl.clone().apply_set_level(&g, &bindings, &args).unwrap_err();
        assert!(err(json!({"rig": 7, "level": "show"})).contains("no rig 7"));
        assert!(err(json!({"bus": 0, "level": "show"})).contains("not an aux bus"));
        assert!(err(json!({"node": 1, "cc": 12, "level": "show"})).contains("no binding"));
        assert!(err(json!({"rig": 0, "level": "set", "set": "Encore"})).contains("no set"));
        assert!(err(json!({"rig": 0, "level": "gig"})).contains("'show', 'song' or 'set'"));
        let shown = sl.show_json(&g, &bindings, |_| "Hall".into());
        assert_eq!(shown["levels"]["rigs"][0]["level"], "set");
        assert_eq!(shown["levels"]["rigs"][0]["set"], "Unplugged");
        assert_eq!(shown["levels"]["buses"][0]["name"], "Hall");
        assert_eq!(shown["levels"]["bindings"][0]["source"], "hw:0");
        assert_eq!(shown["sets"][0]["songs"], json!([1]));
        sl.apply_setlist_set(&json!({"set": "Unplugged", "remove": true})).unwrap();
        assert!(sl.sets.is_empty());
    }
}
