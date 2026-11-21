//! Sessions, undo, starter templates (task-8, docs/ARCHITECTURE.md §2b/§7).
//!
//! A session is the control-plane [`Graph`] plus knob/param values, opaque
//! plugin state blobs, and an optional MIDI-matrix config slot. It persists
//! as a `name.ksalat/` *package directory*:
//!
//! ```text
//! My Song.ksalat/
//!   session.json        # format version, name, graph, params, matrix, state refs
//!   state/
//!     node-1.bin        # opaque plugin blobs, one file per Clap node
//! ```
//!
//! Blobs are captured by the caller on the main thread via
//! `ClapVoiceMain::save_state` and handed to [`Session::set_plugin_state`];
//! this module never calls host code. On restore the caller iterates
//! [`Session::plugin_states`] and feeds each blob to `ClapVoiceMain::load_state`.
//!
//! Saves are atomic: a fresh package is staged in a hidden sibling directory
//! (every file written temp-then-renamed and fsynced), then swapped into place
//! with directory renames. An existing package is never written into — it is
//! moved aside, replaced, and only then removed. A failed save leaves the
//! user's original package untouched.
//!
//! Undo/redo is snapshot-based ([`History`]): graph + params + matrix are
//! cloned before each edit; plugin blobs are *not* snapshotted (they are
//! captured on demand at save time).

#![allow(dead_code)] // public API lands in the UI/CLI with task-10 integration

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control::{
    name_bytes, AudioInChannel, DspKind, EdgeId, Graph, Lv2Uri, NodeId, NodeKind,
    ParamId, PortKind, PortRef, Rig,
};
// Param ids are per-node; these mirror the engine's current knob table
// (task-10 generalizes it — the session stores opaque (node, param, value)
// triples either way).
use crate::engine::{RigMix, P_LEVEL, P_MIX, P_TIME, P_VOL, P_WARM};

/// On-disk format version. Bump on breaking layout changes; `load` rejects
/// versions it does not understand with a plain-language error.
pub const FORMAT_VERSION: u32 = 1;
/// The manifest inside a package.
pub const SESSION_FILE: &str = "session.json";
/// Subdirectory holding opaque plugin state blobs.
pub const STATE_DIR: &str = "state";
/// Package directory extension.
pub const PACKAGE_EXT: &str = "ksalat";
/// The manifest inside a single-rig preset package.
pub const RIG_PRESET_FILE: &str = "rig.json";
/// Package directory extension for single-rig presets.
pub const RIG_PRESET_EXT: &str = "ksalat-rig";
/// On-disk format version for single-rig preset manifests.
pub const RIG_PRESET_FORMAT_VERSION: u32 = 1;

/// The Surge XT CLAP id — the reference plugin (docs/ARCHITECTURE.md §1), native on ARM.
const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One knob/param value: `SetParam { node, param, value }` at rest.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamValue {
    pub node: NodeId,
    pub param: ParamId,
    pub value: f64,
}

/// An opaque plugin state blob for one Clap node. `data` is whatever
/// `ClapVoiceMain::save_state` produced — the session never interprets it.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginState {
    pub node: NodeId,
    pub plugin_id: String,
    pub data: Vec<u8>,
}

/// A complete session: the patch, its knob values, plugin state, and the
/// MIDI-matrix config slot (task-9 fills the slot; the session keeps it opaque).
#[derive(Clone, Debug)]
pub struct Session {
    pub name: String,
    pub graph: Graph,
    params: Vec<ParamValue>, // sorted by (node, param), unique
    states: Vec<PluginState>, // sorted by node, unique
    matrix: Option<Value>,
    /// Rig names in rig order (#33); empty = the default "Rig A/B/…".
    pub rig_names: Vec<String>,
    /// Mixer strip state keyed by node (#72–77): a source's pan/mute/solo,
    /// an FX node's bypass, the strip's colour and collapse flag. Node-keyed
    /// like `params` — `remap_session` carries each entry to its node's new
    /// id across rig edits and reloads (rule 44).
    strips: Vec<StripSaved>,
    /// Source strips in display order, flat across rigs (a rig's strips are
    /// the entries whose node is in that rig; nodes missing from the list
    /// sort last in unit order). Keyed by node id, remapped with the strips.
    strip_order: Vec<NodeId>,
    /// Explicit mixer quick-knob overrides (#107), keyed by node id (knob
    /// label or parameter name/id). Unlisted nodes use their default quick knob.
    strip_knobs: BTreeMap<NodeId, String>,
    /// Sends into aux buses (issue #5), keyed by node and rig like the
    /// engine's table; `remap_session` carries them across reloads.
    sends: Vec<crate::control::AuxSend>,
    /// Buses that play to their own output (#147), keyed by rig and node
    /// like the sends. The sink is kept by name, also while it is unplugged.
    bus_outs: Vec<crate::control::BusOut>,
    /// The master's loudness target with its trim (#12). `None` = this
    /// session says nothing about it (older files, templates, rig edits
    /// built from parts): a load then leaves the engine's target alone.
    pub master_target: Option<crate::meter::MasterTarget>,
    /// Whether Kabelsalat's master mix to Master out is muted.
    pub master_mute: bool,
    /// Each rig's instrument-type mark in rig order (#198); `None` = the
    /// rig follows its instrument, and the list is empty while no rig is
    /// marked. Travels by rig like `rig_names`.
    pub rig_types: Vec<Option<crate::keyboard::KeyboardType>>,
    /// The time signature (#87). `None` = this session says nothing about
    /// it (older files, templates): a load then leaves the clock's as it is.
    pub time_sig: Option<crate::transport::TimeSig>,
    /// Which sources play each CLAP synth (#124), keyed by node like
    /// `params` (rule 44). A synth without a row listens to the keyboard and
    /// all hardware, which is also what an older file means.
    midi_sources: Vec<MidiSources>,
    /// The gig's setlist (#204): ordered songs and parts over the loaded rigs.
    pub setlist: crate::setlist::Setlist,
    /// The port name of each hardware MIDI device index the session's
    /// routing depends on (#227): matrix rules and rig devices hold indices,
    /// which are only places in the scan of the run that saved them. A load
    /// moves them through [`HwIndexMap`].
    hw_ports: BTreeMap<u32, String>,
    /// Each rig's hardware MIDI device in rig order (#227); `None` = the
    /// keyboard and every device. Empty while no rig is bound to one.
    pub rig_devices: Vec<Option<u32>>,
}

/// One CLAP synth's input selection (#124): the keyboard switch, the
/// hardware switch and the one hardware device it listens to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiSources {
    pub node: NodeId,
    #[serde(default = "default_true")]
    pub keyboard: bool,
    #[serde(default = "default_true")]
    pub hardware: bool,
    /// The device's port name without ALSA's client:port numbers, which
    /// change on a re-plug (rule 58). `None` = every device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl MidiSources {
    /// True for a synth nobody changed: keyboard and all hardware.
    pub fn is_default(&self) -> bool {
        self.keyboard && self.hardware && self.device.is_none()
    }
}

const HW_DEVICES: usize = crate::engine::MAX_HW_DEVICES as usize;

/// Where each hardware MIDI device index of a saved session lands in this
/// run (#227). A port the session names goes to its index here, or to a
/// place of its own while it is not plugged in, or, with the device list
/// full, past the list where no device sends. Every other index stays where
/// it is while that is free, else takes a free one, so the 15 "off" rules
/// of an LV2 instrument bound to one device still leave exactly that device
/// on.
pub struct HwIndexMap([u32; HW_DEVICES]);

impl HwIndexMap {
    /// `listed`: this run's ports by index, without ALSA's client:port
    /// numbers. `place`: a new place for a port that is not listed (or
    /// whose listed ones are taken), None when the list is full.
    pub fn new(saved: &BTreeMap<u32, String>, listed: &[&str], mut place: impl FnMut(&str) -> Option<u32>) -> Self {
        let mut to = [None; HW_DEVICES];
        let named = saved.iter().filter(|(&i, _)| (i as usize) < HW_DEVICES);
        for (&i, port) in named.clone() {
            if listed.get(i as usize) == Some(&port.as_str()) {
                to[i as usize] = Some(i);
            }
        }
        for (&i, port) in named {
            if to[i as usize].is_some() {
                continue;
            }
            let listed_at = (0..listed.len()).find(|&d| listed[d] == port && !to.contains(&Some(d as u32)));
            let d = listed_at.map(|d| d as u32).or_else(|| place(port));
            let d = d.filter(|&d| (d as usize) < HW_DEVICES && !to.contains(&Some(d)));
            to[i as usize] = Some(d.unwrap_or(HW_DEVICES as u32 + i));
        }
        let mut free: Vec<u32> = (0..HW_DEVICES as u32).filter(|d| !to.contains(&Some(*d))).collect();
        for (i, d) in to.iter_mut().enumerate() {
            if let (None, Some(k)) = (*d, free.iter().position(|&f| f == i as u32)) {
                *d = Some(free.remove(k));
            }
        }
        let mut free = free.into_iter();
        Self(to.map(|d| d.or_else(|| free.next()).expect("one free index per unnamed one")))
    }

    pub fn get(&self, d: u32) -> u32 {
        self.0.get(d as usize).copied().unwrap_or(d)
    }

    /// The devices of this run no saved index lands on (only while a named
    /// device went past a full list).
    pub fn unclaimed(&self) -> impl Iterator<Item = u32> + '_ {
        (0..HW_DEVICES as u32).filter(|d| !self.0.contains(d))
    }
}

/// One strip's persisted state (#72–77). Defaults mirror the engine's
/// `StripMix::DEFAULT` plus UI-local colour/collapse.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripSaved {
    pub node: NodeId,
    /// Pan −1 (L) … 1 (R).
    pub pan: f32,
    pub mute: bool,
    /// Solo scope: within the node's rig (#73).
    pub solo: bool,
    /// Insert bypass (#74) — an FX node's flag.
    pub bypass: bool,
    /// Strip colour as 0xRRGGBB (#77); None = the plain chrome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    /// Strip collapsed to its mini form (#77).
    pub collapsed: bool,
}

impl Default for StripSaved {
    fn default() -> Self {
        Self { node: 0, pan: 0.0, mute: false, solo: false, bypass: false, color: None, collapsed: false }
    }
}

impl StripSaved {
    /// True while nothing differs from a node nobody ever touched.
    pub fn is_plain(&self) -> bool {
        *self == Self { node: self.node, ..Self::default() }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new("Untitled", Graph::default())
    }
}

impl Session {
    pub fn new(name: impl Into<String>, graph: Graph) -> Self {
        Self {
            name: name.into(),
            graph,
            params: Vec::new(),
            states: Vec::new(),
            matrix: None,
            rig_names: Vec::new(),
            strips: Vec::new(),
            strip_order: Vec::new(),
            strip_knobs: BTreeMap::new(),
            sends: Vec::new(),
            bus_outs: Vec::new(),
            master_target: None,
            master_mute: false,
            rig_types: Vec::new(),
            time_sig: None,
            midi_sources: Vec::new(),
            setlist: crate::setlist::Setlist::default(),
            hw_ports: BTreeMap::new(),
            rig_devices: Vec::new(),
        }
    }

    /// True when both hold the same patch, knobs, plugin states, routes, rig
    /// names, mixer strips, and setlist; the session name is not compared,
    /// nor are the port names, which a device plugged in later only adds to.
    pub fn same_content(&self, other: &Session) -> bool {
        self.params == other.params
            && self.states == other.states
            && self.matrix == other.matrix
            && self.rig_names == other.rig_names
            && self.strips == other.strips
            && self.strip_order == other.strip_order
            && self.strip_knobs == other.strip_knobs
            && self.sends == other.sends
            && self.bus_outs == other.bus_outs
            && self.master_target == other.master_target
            && self.master_mute == other.master_mute
            && self.rig_types == other.rig_types
            && self.time_sig == other.time_sig
            && self.midi_sources == other.midi_sources
            && self.setlist == other.setlist
            && self.rig_devices == other.rig_devices
            && serde_json::to_value(&self.graph).ok() == serde_json::to_value(&other.graph).ok()
    }

    // -- knobs / params ----------------------------------------------------

    pub fn param(&self, node: NodeId, param: ParamId) -> Option<f64> {
        self.params
            .iter()
            .find(|p| p.node == node && p.param == param)
            .map(|p| p.value)
    }

    /// Upsert a knob value (the persisted form of `Command::SetParam`).
    pub fn set_param(&mut self, node: NodeId, param: ParamId, value: f64) {
        match self
            .params
            .iter_mut()
            .find(|p| p.node == node && p.param == param)
        {
            Some(p) => p.value = value,
            None => {
                self.params.push(ParamValue { node, param, value });
                self.params.sort_by_key(|p| (p.node, p.param));
            }
        }
    }

    pub fn params(&self) -> &[ParamValue] {
        &self.params
    }

    // -- plugin state blobs --------------------------------------------------

    pub fn plugin_state(&self, node: NodeId) -> Option<&PluginState> {
        self.states.iter().find(|s| s.node == node)
    }

    pub fn plugin_states(&self) -> &[PluginState] {
        &self.states
    }

    /// Store a blob captured by the caller (main thread,
    /// `ClapVoiceMain::save_state`). Replaces any previous blob for the node.
    pub fn set_plugin_state(
        &mut self,
        node: NodeId,
        plugin_id: impl Into<String>,
        data: Vec<u8>,
    ) {
        let plugin_id = plugin_id.into();
        match self.states.iter_mut().find(|s| s.node == node) {
            Some(s) => {
                s.plugin_id = plugin_id;
                s.data = data;
            }
            None => {
                self.states.push(PluginState {
                    node,
                    plugin_id,
                    data,
                });
                self.states.sort_by_key(|s| s.node);
            }
        }
    }

    // -- MIDI matrix slot (task-9) -------------------------------------------

    /// Opaque matrix config. task-9 serializes its routing table here; the
    /// session persists it verbatim without understanding it.
    pub fn matrix(&self) -> Option<&Value> {
        self.matrix.as_ref()
    }

    pub fn set_matrix(&mut self, matrix: Option<Value>) {
        self.matrix = matrix;
    }

    // -- input selection of CLAP synths (#124) -------------------------------

    /// The synths whose input selection is not the default, sorted by node.
    pub fn midi_sources(&self) -> &[MidiSources] {
        &self.midi_sources
    }

    /// Upsert one synth's input selection. A default one has no row.
    pub fn set_midi_sources(&mut self, row: MidiSources) {
        self.midi_sources.retain(|m| m.node != row.node);
        if !row.is_default() {
            self.midi_sources.push(row);
            self.midi_sources.sort_by_key(|m| m.node);
        }
    }

    // -- hardware MIDI device names (#227) -----------------------------------

    pub fn hw_ports(&self) -> &BTreeMap<u32, String> {
        &self.hw_ports
    }

    pub fn set_hw_ports(&mut self, ports: BTreeMap<u32, String>) {
        self.hw_ports = ports;
    }

    // -- mixer strips (#72–77) ----------------------------------------------

    /// One node's persisted strip state, if any.
    pub fn strip(&self, node: NodeId) -> Option<&StripSaved> {
        self.strips.iter().find(|s| s.node == node)
    }

    /// Upsert one node's strip state (the persisted form of
    /// `Command::SetStripMix` / `SetStripBypass` plus the UI-local colour and
    /// collapse flag). A plain (all-default) entry is dropped: an untouched
    /// node has no row.
    pub fn set_strip(&mut self, strip: StripSaved) {
        match self.strips.iter_mut().find(|s| s.node == strip.node) {
            Some(s) => *s = strip,
            None => {
                if !strip.is_plain() {
                    self.strips.push(strip);
                    self.strips.sort_by_key(|s| s.node);
                }
            }
        }
        self.strips.retain(|s| !s.is_plain());
    }

    /// Sends into aux buses (issue #5).
    pub fn sends(&self) -> &[crate::control::AuxSend] {
        &self.sends
    }

    /// Replace the sends. One entry per (source, bus); entries that do
    /// nothing, point at a node the patch lacks or at a non-bus are dropped.
    pub fn set_sends(&mut self, sends: Vec<crate::control::AuxSend>) {
        self.sends.clear();
        for s in sends {
            if !s.is_default() && !self.sends.iter().any(|o| o.from == s.from && o.bus == s.bus) {
                self.sends.push(s);
            }
        }
        self.prune_sends();
    }

    /// Buses that play to their own output (#147).
    pub fn bus_outs(&self) -> &[crate::control::BusOut] {
        &self.bus_outs
    }

    /// Replace the bus outputs. One entry per bus; entries for a rig or an
    /// aux bus the patch lacks are dropped.
    pub fn set_bus_outs(&mut self, outs: Vec<crate::control::BusOut>) {
        self.bus_outs.clear();
        for o in outs {
            if !o.sink.is_none() && !self.bus_outs.iter().any(|x| x.bus == o.bus) {
                self.bus_outs.push(o);
            }
        }
        self.prune_bus_outs();
    }

    fn prune_bus_outs(&mut self) {
        let g = &self.graph;
        self.bus_outs.retain(|o| match o.bus {
            crate::control::SendSource::Rig(r) => g.nodes.iter().any(|n| n.rig == r),
            crate::control::SendSource::Node(n) => g.is_bus(n),
        });
    }

    fn prune_sends(&mut self) {
        let g = &self.graph;
        self.sends.retain(|s| {
            g.is_bus(s.bus) && match s.from {
                crate::control::SendSource::Rig(r) => g.nodes.iter().any(|n| n.rig == r),
                crate::control::SendSource::Node(n) => g.node(n).is_some(),
            }
        });
    }

    /// All strip rows (sorted by node).
    pub fn strips(&self) -> &[StripSaved] {
        &self.strips
    }

    /// Source strips' display order (flat; per-rig sections derive from it).
    pub fn strip_order(&self) -> &[NodeId] {
        &self.strip_order
    }

    /// Set the strips' display order; unknown node ids are dropped.
    pub fn set_strip_order(&mut self, order: Vec<NodeId>) {
        self.strip_order = order.into_iter().filter(|n| self.graph.node(*n).is_some()).collect();
    }

    /// Explicit mixer quick-knob assignment (#107) for one node, if any.
    pub fn strip_knob(&self, node: NodeId) -> Option<&str> {
        self.strip_knobs.get(&node).map(String::as_str)
    }

    /// All explicit mixer quick-knob assignments (#107), keyed by node id.
    pub fn strip_knobs(&self) -> &BTreeMap<NodeId, String> {
        &self.strip_knobs
    }

    /// Set (or clear when empty) one node's mixer quick-knob assignment (#107).
    pub fn set_strip_knob(&mut self, node: NodeId, knob: String) {
        let trimmed = knob.trim();
        if trimmed.is_empty() {
            self.strip_knobs.remove(&node);
        } else {
            self.strip_knobs.insert(node, trimmed.to_string());
        }
    }

    // -- integrity -------------------------------------------------------------

    /// Drop params/blobs whose node no longer exists (call after `RemoveNode`).
    pub fn prune_to_graph(&mut self) {
        self.params.retain(|p| self.graph.node(p.node).is_some());
        self.states.retain(|s| self.graph.node(s.node).is_some());
        self.strips.retain(|s| self.graph.node(s.node).is_some());
        self.strip_order.retain(|n| self.graph.node(*n).is_some());
        self.strip_knobs.retain(|n, _| self.graph.node(*n).is_some());
        self.midi_sources.retain(|m| self.graph.node(m.node).is_some());
        self.prune_sends();
        self.prune_bus_outs();
        self.setlist.prune_to_graph(&self.graph);
    }

    /// Plain-language integrity check, run on save and load: every edge must
    /// reference real nodes, ids must be unique, params/blobs must belong to
    /// existing nodes, and blobs must match their node's plugin.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The session needs a name before it can be saved.");
        }
        let mut node_ids: Vec<NodeId> = self.graph.nodes.iter().map(|n| n.id).collect();
        node_ids.sort_unstable();
        if node_ids.windows(2).any(|w| w[0] == w[1]) {
            bail!("The patch contains duplicate node ids — it cannot be saved.");
        }
        let mut edge_ids: Vec<EdgeId> = self.graph.edges.iter().map(|e| e.id).collect();
        edge_ids.sort_unstable();
        if edge_ids.windows(2).any(|w| w[0] == w[1]) {
            bail!("The patch contains duplicate cable ids — it cannot be saved.");
        }
        for e in &self.graph.edges {
            if e.from.node == e.to.node {
                bail!("A cable plugs a node into itself — unplug it and try again.");
            }
            if self.graph.node(e.from.node).is_none() || self.graph.node(e.to.node).is_none() {
                bail!(
                    "Cable {} references a node that no longer exists.",
                    e.id
                );
            }
        }
        for p in &self.params {
            if self.graph.node(p.node).is_none() {
                bail!(
                    "A knob value belongs to node {} which is no longer in the patch. \
                     Call prune_to_graph() after removing nodes.",
                    p.node
                );
            }
            if !p.value.is_finite() {
                bail!("A knob value is not a finite number (node {}).", p.node);
            }
        }
        for s in &self.states {
            match self.graph.node(s.node) {
                Some(n) => match &n.kind {
                    NodeKind::Clap { plugin_id } if plugin_id.as_str() == s.plugin_id => {}
                    NodeKind::Clap { plugin_id } => bail!(
                        "The state blob for node {} was saved by '{}' but the node now \
                         hosts '{}'.",
                        s.node,
                        s.plugin_id,
                        plugin_id.as_str()
                    ),
                    NodeKind::Lv2 { uri } if uri.as_str() == s.plugin_id => {}
                    NodeKind::Lv2 { uri } => bail!(
                        "The state blob for node {} was saved by '{}' but the node now \
                         hosts '{}'.",
                        s.node,
                        s.plugin_id,
                        uri.as_str()
                    ),
                    _ => bail!(
                        "Node {} has a plugin state blob but is not a plugin.",
                        s.node
                    ),
                },
                None => bail!(
                    "A plugin state blob belongs to node {} which is no longer in the \
                     patch. Call prune_to_graph() after removing nodes.",
                    s.node
                ),
            }
            if s.plugin_id.is_empty() {
                bail!("The state blob for node {} has no plugin id.", s.node);
            }
        }
        for s in &self.strips {
            if self.graph.node(s.node).is_none() {
                bail!(
                    "A mixer strip belongs to node {} which is no longer in the patch. \
                     Call prune_to_graph() after removing nodes.",
                    s.node
                );
            }
            if !s.pan.is_finite() || !(-1.0..=1.0).contains(&s.pan) {
                bail!("The mixer strip of node {} has a pan outside -1..1.", s.node);
            }
        }
        for n in &self.strip_order {
            if self.graph.node(*n).is_none() {
                bail!(
                    "The strip order lists node {} which is no longer in the patch.",
                    n
                );
            }
        }
        for n in self.strip_knobs.keys() {
            if self.graph.node(*n).is_none() {
                bail!(
                    "The strip quick-knob assignment references node {} which is no longer in the patch.",
                    n
                );
            }
        }
        for s in &self.sends {
            if !self.graph.is_bus(s.bus) {
                bail!("A send goes to node {} which is not an aux bus of the patch.", s.bus);
            }
            if let crate::control::SendSource::Node(n) = s.from {
                if self.graph.node(n).is_none() {
                    bail!("A send comes from node {n} which is no longer in the patch.");
                }
            }
            if !s.level.is_finite() || !(0.0..=1.0).contains(&s.level) {
                bail!("The send into bus {} has a level outside 0..1.", s.bus);
            }
        }
        self.setlist.validate(&self.graph)?;
        Ok(())
    }

    // -- package I/O -----------------------------------------------------------

    /// `parent/name.ksalat` with a plain-language check on the name.
    pub fn package_path(parent: &Path, name: &str) -> Result<PathBuf> {
        validate_package_path(parent, name, PACKAGE_EXT, "session")
    }

    /// Atomically save into the package directory `package` (e.g.
    /// `~/sessions/My Song.ksalat`). The previous package, if any, is replaced
    /// wholesale and is never modified in place; on any error it stays intact.
    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "session", |staging| self.write_package(staging))
    }

    /// Write manifest + blobs into a fresh staging directory. Files are
    /// written temp-then-renamed and fsynced; the manifest goes last so a
    /// half-written staging dir is never a valid package.
    fn write_package(&self, dir: &Path) -> Result<()> {
        let refs = write_state_files(dir, &self.states)?;
        let manifest = SessionFile {
            format: FORMAT_VERSION,
            name: self.name.clone(),
            graph: self.graph.clone(),
            params: self.params.clone(),
            matrix: self.matrix.clone(),
            states: refs,
            rig_names: self.rig_names.clone(),
            strips: self.strips.clone(),
            strip_order: self.strip_order.clone(),
            strip_knobs: self.strip_knobs.clone(),
            sends: self.sends.clone(),
            bus_outs: self.bus_outs.clone(),
            master_target: self.master_target,
            master_mute: self.master_mute,
            rig_types: self.rig_types.clone(),
            keyboard_mode: None,
            time_signature: self.time_sig,
            midi_sources: self.midi_sources.clone(),
            setlist: self.setlist.clone(),
            hw_ports: self.hw_ports.clone(),
            rig_devices: self.rig_devices.clone(),
        };
        let json = serde_json::to_vec_pretty(&manifest)?;
        write_atomic(&dir.join(SESSION_FILE), &json)?;
        Ok(())
    }

    /// Load a package. The graph, knobs, matrix slot, and blobs come back
    /// exactly as saved; the caller restores blobs via `ClapVoiceMain::load_state`
    /// and re-issues knob values as `SetParam` commands.
    pub fn load(package: &Path) -> Result<Session> {
        let manifest_path = package.join(SESSION_FILE);
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat session package?",
                manifest_path.display()
            )
        })?;
        let manifest: SessionFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid session JSON.", manifest_path.display()))?;
        if manifest.format != FORMAT_VERSION {
            bail!(
                "This session uses format version {} but this build understands version {}. \
                 It may have been written by a newer kabelsalat.",
                manifest.format,
                FORMAT_VERSION
            );
        }
        let states = read_state_files(package, &manifest.states, "session")?;
        let rig_types = rig_types_of(&manifest);
        let mut setlist = manifest.setlist;
        setlist.normalize();
        let session = Session {
            name: manifest.name,
            graph: manifest.graph,
            params: manifest.params,
            states,
            matrix: manifest.matrix,
            rig_names: manifest.rig_names,
            strips: manifest.strips,
            strip_order: manifest.strip_order,
            strip_knobs: manifest.strip_knobs,
            sends: manifest.sends,
            bus_outs: manifest.bus_outs,
            master_target: manifest.master_target,
            master_mute: manifest.master_mute,
            rig_types,
            // A hand-edited file cannot give the clock a bar it cannot count.
            time_sig: manifest.time_signature.map(|s| crate::transport::TimeSig::normalized(s.num, s.den)),
            midi_sources: manifest.midi_sources,
            setlist,
            hw_ports: manifest.hw_ports,
            rig_devices: manifest.rig_devices,
        };
        session.validate()?;
        Ok(session)
    }
}

fn validate_package_path(parent: &Path, name: &str, ext: &str, kind: &str) -> Result<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
    {
        bail!("'{name}' is not a usable {kind} name — avoid slashes and dots-only names.");
    }
    Ok(parent.join(format!("{trimmed}.{ext}")))
}

fn stage_package(package: &Path, kind: &str, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let basename = package
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("The {kind} package path has no usable directory name."))?;
    let parent = package.parent().unwrap_or(Path::new(""));
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create the folder {}.", parent.display()))?;
    if package.exists() && !package.is_dir() {
        bail!(
            "{} already exists and is not a {kind} package — choose another name.",
            package.display()
        );
    }
    let staging = parent.join(format!(".{basename}.staging-{}", unique_suffix()));
    fs::create_dir(&staging)
        .with_context(|| format!("Could not stage the {kind} at {}.", staging.display()))?;
    let result = write(&staging).and_then(|()| swap_package(package, &staging));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn write_state_files(dir: &Path, states: &[PluginState]) -> Result<Vec<StateRef>> {
    let mut refs = Vec::new();
    if !states.is_empty() {
        let state_dir = dir.join(STATE_DIR);
        fs::create_dir(&state_dir)?;
        for s in states {
            let rel = format!("{STATE_DIR}/node-{}.bin", s.node);
            write_atomic(&dir.join(&rel), &s.data)
                .with_context(|| format!("Could not write plugin state for node {}.", s.node))?;
            refs.push(StateRef {
                node: s.node,
                plugin_id: s.plugin_id.clone(),
                file: rel,
            });
        }
    }
    Ok(refs)
}

fn read_state_files(package: &Path, refs: &[StateRef], kind: &str) -> Result<Vec<PluginState>> {
    let mut states = Vec::new();
    for r in refs {
        let rel = safe_relative(&r.file)?;
        let data = fs::read(package.join(&rel)).with_context(|| {
            format!(
                "The {kind} is missing its plugin state file '{}' (node {}).",
                r.file, r.node
            )
        })?;
        states.push(PluginState {
            node: r.node,
            plugin_id: r.plugin_id.clone(),
            data,
        });
    }
    Ok(states)
}

// ---------------------------------------------------------------------------
// On-disk manifest (blobs live in separate files, referenced by path)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct SessionFile {
    format: u32,
    name: String,
    graph: Graph,
    #[serde(default)]
    params: Vec<ParamValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    matrix: Option<Value>,
    #[serde(default)]
    states: Vec<StateRef>,
    /// Rig names (older sessions have none → default names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rig_names: Vec<String>,
    /// Mixer strips (#72–77): a source's pan/mute/solo, an FX's bypass, the
    /// strip colour/collapse. Older sessions have none → engine defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    strips: Vec<StripSaved>,
    /// Strip display order (older sessions have none → unit order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    strip_order: Vec<NodeId>,
    /// Explicit mixer quick-knob overrides (#107), keyed by node id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    strip_knobs: BTreeMap<NodeId, String>,
    /// Sends into aux buses (issue #5); older sessions have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sends: Vec<crate::control::AuxSend>,
    /// Buses on their own output (#147); older sessions have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bus_outs: Vec<crate::control::BusOut>,
    /// The master's loudness target (#12); older sessions have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    master_target: Option<crate::meter::MasterTarget>,
    /// Whether Kabelsalat's master mix is muted; older sessions default to false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    master_mute: bool,
    /// Rig instrument-type marks (#198); absent while no rig is marked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rig_types: Vec<Option<crate::keyboard::KeyboardType>>,
    /// The one keyboard pin of sessions before #198: read, never written.
    #[serde(default, skip_serializing)]
    keyboard_mode: Option<crate::keyboard::KeyboardMode>,
    /// The time signature (#87); older sessions have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_signature: Option<crate::transport::TimeSig>,
    /// Input selection of CLAP synths (#124); older sessions have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    midi_sources: Vec<MidiSources>,
    /// The setlist (#204); older/plain sessions omit it.
    #[serde(default, skip_serializing_if = "crate::setlist::Setlist::is_empty")]
    setlist: crate::setlist::Setlist,
    /// Port names of the device indices in use (#227); older sessions have
    /// none, and their indices load as they are.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hw_ports: BTreeMap<u32, String>,
    /// Rig devices (#227); older sessions have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rig_devices: Vec<Option<u32>>,
}

/// A file's rig marks: its own, or an old session's single keyboard pin,
/// which marked whatever the dock played (#163), so every rig of it.
fn rig_types_of(manifest: &SessionFile) -> Vec<Option<crate::keyboard::KeyboardType>> {
    match (manifest.rig_types.is_empty(), manifest.keyboard_mode.and_then(|m| m.pinned())) {
        (true, Some(t)) => {
            let rigs = manifest.graph.nodes.iter().map(|n| n.rig as usize + 1).max().unwrap_or(1);
            vec![Some(t); rigs]
        }
        _ => manifest.rig_types.clone(),
    }
}

#[derive(Serialize, Deserialize)]
struct StateRef {
    node: NodeId,
    plugin_id: String,
    file: String,
}

/// Reject absolute paths and anything with `..`/root/prefix components so a
/// hand-edited manifest can never reach outside the package.
fn safe_relative(file: &str) -> Result<PathBuf> {
    let p = Path::new(file);
    if file.is_empty() || p.is_absolute() {
        bail!("The session references an invalid state file path '{file}'.");
    }
    if !p
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("The session state file '{file}' points outside the package — refusing to load it.");
    }
    Ok(p.to_path_buf())
}

// ---------------------------------------------------------------------------
// Atomic file writes
// ---------------------------------------------------------------------------

fn unique_suffix() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

/// Write `bytes` to `path` via a unique temp file + fsync + rename, so readers
/// never see a partial file. Cleans up the temp file on failure.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("atomic write target has no file name")?;
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", unique_suffix()));
    let write = || -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Best-effort directory fsync so the rename itself is durable.
fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Swap a fully written staging package into place. The previous package, if
/// any, is moved aside first and only removed once the new one is live; on a
/// failed swap the original is restored. Shared by [`Session::save`] and
/// [`MultiRigSession::save`].
fn swap_package(package: &Path, staging: &Path) -> Result<()> {
    let basename = package
        .file_name()
        .and_then(|s| s.to_str())
        .context("The session package path has no usable directory name.")?;
    let parent = package.parent().unwrap_or(Path::new(""));
    if package.exists() {
        let backup = parent.join(format!(".{basename}.old-{}", unique_suffix()));
        fs::rename(package, &backup).with_context(|| {
            format!("Could not move the old session aside ({}).", package.display())
        })?;
        match fs::rename(staging, package) {
            Ok(()) => {
                // The new package is live; old-copy cleanup is best-effort.
                let _ = fs::remove_dir_all(&backup);
            }
            Err(e) => {
                // Put the original back before reporting the failure.
                let _ = fs::rename(&backup, package);
                return Err(e).with_context(|| {
                    format!("Could not put the new session at {}.", package.display())
                });
            }
        }
    } else {
        fs::rename(staging, package).with_context(|| {
            format!("Could not put the new session at {}.", package.display())
        })?;
    }
    sync_dir(parent);
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-rig sessions (task-22): Vec<Rig> + active, same atomic package I/O
// ---------------------------------------------------------------------------

/// The manifest inside a multi-rig package.
pub const MULTI_RIG_FILE: &str = "rigs.json";
/// On-disk format version for multi-rig manifests.
pub const MULTI_RIG_FORMAT_VERSION: u32 = 1;

/// A multi-rig session: several independent rigs (each a full rack graph bound
/// to a hardware MIDI device) plus the UI-selected rig index. Persists as a
/// `name.ksalat/` package with a `rigs.json` manifest, atomically like
/// [`Session`]. Single-graph [`Session`] packages keep working unchanged;
/// [`MultiRigSession::load`] falls back to wrapping one as a single rig.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiRigSession {
    pub name: String,
    pub rigs: Vec<Rig>,
    #[serde(default)]
    pub active: usize,
}

impl MultiRigSession {
    pub fn new(name: impl Into<String>, rigs: Vec<Rig>, active: usize) -> Self {
        Self { name: name.into(), rigs, active }
    }

    /// The merged patch the engine plays (`Command::LoadRigs` does this too).
    pub fn merged_graph(&self) -> Graph {
        crate::control::merge_rigs(&self.rigs)
    }

    /// Plain-language integrity check, run on save and load.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The session needs a name before it can be saved.");
        }
        if self.rigs.is_empty() {
            bail!("A multi-rig session needs at least one rig.");
        }
        if self.active >= self.rigs.len() {
            bail!(
                "The active rig index {} is out of range ({} rigs).",
                self.active,
                self.rigs.len()
            );
        }
        for (i, rig) in self.rigs.iter().enumerate() {
            if rig.name.trim().is_empty() {
                bail!("Rig {} needs a name before the session can be saved.", i + 1);
            }
            let g = &rig.graph;
            let mut node_ids: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
            node_ids.sort_unstable();
            if node_ids.windows(2).any(|w| w[0] == w[1]) {
                bail!("Rig '{}' contains duplicate node ids.", rig.name);
            }
            let mut edge_ids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
            edge_ids.sort_unstable();
            if edge_ids.windows(2).any(|w| w[0] == w[1]) {
                bail!("Rig '{}' contains duplicate cable ids.", rig.name);
            }
            for e in &g.edges {
                if e.from.node == e.to.node {
                    bail!("Rig '{}': a cable plugs a node into itself.", rig.name);
                }
                if g.node(e.from.node).is_none() || g.node(e.to.node).is_none() {
                    bail!(
                        "Rig '{}': cable {} references a node that no longer exists.",
                        rig.name,
                        e.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Atomically save into the package directory `package`, mirroring
    /// [`Session::save`]: staged in a hidden sibling, swapped by rename, the
    /// previous package untouched on any error.
    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "session", |staging| {
            let manifest = MultiRigFile {
                format: MULTI_RIG_FORMAT_VERSION,
                session: self.clone(),
            };
            let json = serde_json::to_vec_pretty(&manifest)?;
            write_atomic(&staging.join(MULTI_RIG_FILE), &json)
        })
    }

    /// Load a multi-rig package. Falls back to a single-graph [`Session`]
    /// package (wrapped as one rig with no device binding) so old sessions
    /// open in multi-rig mode.
    pub fn load(package: &Path) -> Result<MultiRigSession> {
        let manifest_path = package.join(MULTI_RIG_FILE);
        if !manifest_path.exists() {
            let s = Session::load(package)?;
            return Ok(MultiRigSession {
                name: s.name,
                rigs: vec![Rig::new("Rig 1", s.graph, None)],
                active: 0,
            });
        }
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat multi-rig package?",
                manifest_path.display()
            )
        })?;
        let manifest: MultiRigFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid multi-rig JSON.", manifest_path.display()))?;
        if manifest.format != MULTI_RIG_FORMAT_VERSION {
            bail!(
                "This multi-rig session uses format version {} but this build understands \
                 version {}. It may have been written by a newer kabelsalat.",
                manifest.format,
                MULTI_RIG_FORMAT_VERSION
            );
        }
        manifest.session.validate()?;
        Ok(manifest.session)
    }
}

/// On-disk multi-rig manifest (rig graphs are self-contained; plugin state
/// blobs are captured per rig by the caller, like single-graph sessions).
#[derive(Serialize, Deserialize)]
struct MultiRigFile {
    format: u32,
    #[serde(flatten)]
    session: MultiRigSession,
}

// ---------------------------------------------------------------------------
// Single-rig presets (#34): one rig's patch, knobs, blobs, matrix, mix, name
// ---------------------------------------------------------------------------

/// Per-CLAP-node input switches and loaded patch display name inside a [`RigPreset`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigClapMeta {
    pub node: NodeId,
    #[serde(default = "default_true")]
    pub keyboard: bool,
    #[serde(default = "default_true")]
    pub hardware: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_name: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A saved single-rig configuration: one rig-relative [`Session`] (`node.rig == 0`
/// for every node) plus the rig's display name, mixer settings, hardware MIDI
/// device binding, and per-synth source switches / patch name.
#[derive(Clone, Debug)]
pub struct RigPreset {
    pub name: String,
    pub rig_name: String,
    pub session: Session,
    pub mix: RigMix,
    /// An index in the space `session.hw_ports()` names (#227).
    pub midi_device: Option<u32>,
    pub clap_meta: Vec<RigClapMeta>,
}

impl RigPreset {
    pub fn new(name: impl Into<String>, rig_name: impl Into<String>, session: Session) -> Self {
        Self {
            name: name.into(),
            rig_name: rig_name.into(),
            session,
            mix: RigMix::DEFAULT,
            midi_device: None,
            clap_meta: Vec::new(),
        }
    }

    pub fn package_path(parent: &Path, name: &str) -> Result<PathBuf> {
        validate_package_path(parent, name, RIG_PRESET_EXT, "rig preset")
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The rig preset needs a name before it can be saved.");
        }
        self.session.validate()?;
        if self.session.graph.nodes.iter().any(|n| n.rig != 0) {
            bail!("A rig preset must hold one rig-relative graph (node.rig == 0).");
        }
        if !self.mix.vol.is_finite() || !self.mix.pan.is_finite() {
            bail!("The rig mixer settings contain a non-finite number.");
        }
        for m in &self.clap_meta {
            match self.session.graph.node(m.node) {
                Some(n) if matches!(n.kind, NodeKind::Clap { .. }) => {}
                _ => bail!("Rig preset metadata references node {} which is not a CLAP synth.", m.node),
            }
        }
        Ok(())
    }

    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "rig preset", |staging| {
            let refs = write_state_files(staging, self.session.plugin_states())?;
            let manifest = RigPresetFile {
                format: RIG_PRESET_FORMAT_VERSION,
                name: self.name.clone(),
                rig_name: self.rig_name.clone(),
                graph: self.session.graph.clone(),
                params: self.session.params().to_vec(),
                matrix: self.session.matrix().cloned(),
                states: refs,
                mix: self.mix,
                midi_device: self.midi_device,
                clap_meta: self.clap_meta.clone(),
                strip_knobs: self.session.strip_knobs().clone(),
                rig_type: self.session.rig_types.first().copied().flatten(),
                hw_ports: self.session.hw_ports().clone(),
            };
            let json = serde_json::to_vec_pretty(&manifest)?;
            write_atomic(&staging.join(RIG_PRESET_FILE), &json)
        })
    }

    pub fn load(package: &Path) -> Result<RigPreset> {
        let manifest_path = package.join(RIG_PRESET_FILE);
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat rig preset?",
                manifest_path.display()
            )
        })?;
        let manifest: RigPresetFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid rig preset JSON.", manifest_path.display()))?;
        if manifest.format != RIG_PRESET_FORMAT_VERSION {
            bail!(
                "This rig preset uses format version {} but this build understands version {}. \
                 It may have been written by a newer kabelsalat.",
                manifest.format,
                RIG_PRESET_FORMAT_VERSION
            );
        }
        let states = read_state_files(package, &manifest.states, "rig preset")?;
        let mut session = Session::new(manifest.name.clone(), manifest.graph);
        for p in manifest.params {
            session.set_param(p.node, p.param, p.value);
        }
        for s in states {
            session.set_plugin_state(s.node, s.plugin_id, s.data);
        }
        session.set_matrix(manifest.matrix);
        for (node, knob) in manifest.strip_knobs {
            session.set_strip_knob(node, knob);
        }
        session.rig_types = manifest.rig_type.map(|t| vec![Some(t)]).unwrap_or_default();
        session.set_hw_ports(manifest.hw_ports);
        let preset = RigPreset {
            name: manifest.name,
            rig_name: manifest.rig_name,
            session,
            mix: manifest.mix,
            midi_device: manifest.midi_device,
            clap_meta: manifest.clap_meta,
        };
        preset.validate()?;
        Ok(preset)
    }
}

#[derive(Serialize, Deserialize)]
struct RigPresetFile {
    format: u32,
    name: String,
    #[serde(default)]
    rig_name: String,
    graph: Graph,
    #[serde(default)]
    params: Vec<ParamValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    matrix: Option<Value>,
    #[serde(default)]
    states: Vec<StateRef>,
    #[serde(default)]
    mix: RigMix,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    midi_device: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clap_meta: Vec<RigClapMeta>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    strip_knobs: BTreeMap<NodeId, String>,
    /// The rig's instrument-type mark (#198); absent while it follows its instrument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rig_type: Option<crate::keyboard::KeyboardType>,
    /// Port names of the device indices in `matrix` and `midi_device` (#227).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hw_ports: BTreeMap<u32, String>,
}

/// Default directory for saved rig presets (sibling to `~/kabelsalat-sessions`).
pub fn rig_presets_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join("kabelsalat-rig-presets");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Sorted list of saved rig preset names in `dir`.
pub fn list_rig_presets(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == RIG_PRESET_EXT) {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Delete a saved rig preset package by name from `dir`.
pub fn delete_rig_preset(dir: &Path, name: &str) -> Result<()> {
    let path = RigPreset::package_path(dir, name)?;
    if !path.exists() {
        bail!("rig preset '{name}' does not exist");
    }
    fs::remove_dir_all(&path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Undo / redo (snapshot-based)
// ---------------------------------------------------------------------------

/// What a snapshot restores: the patch, knob values, and matrix slot.
/// Plugin blobs and the session name are deliberately not snapshotted —
/// blobs are captured on demand at save time, and renaming is not an edit.
#[derive(Clone)]
struct Snapshot {
    graph: Graph,
    params: Vec<ParamValue>,
    matrix: Option<Value>,
    setlist: crate::setlist::Setlist,
}

/// Snapshot undo stack. Usage: `history.record(&session)` *before* each
/// user-visible mutation, then mutate `session.graph` / params freely.
/// `undo`/`redo` swap whole snapshots — robust for any edit, including
/// structural ones (add/remove node, plug/unplug).
pub struct History {
    past: VecDeque<Snapshot>,
    future: VecDeque<Snapshot>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// 128 undo steps — generous for live patching, bounded in memory
    /// (the graph is capped at 64 nodes).
    pub fn new() -> Self {
        Self::with_limit(128)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            past: VecDeque::new(),
            future: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    fn snapshot(s: &Session) -> Snapshot {
        Snapshot {
            graph: s.graph.clone(),
            params: s.params.clone(),
            matrix: s.matrix.clone(),
            setlist: s.setlist.clone(),
        }
    }

    fn restore(s: &mut Session, snap: Snapshot) {
        s.graph = snap.graph;
        s.params = snap.params;
        s.matrix = snap.matrix;
        s.setlist = snap.setlist;
    }

    /// Record the current state before an edit. Clears the redo branch
    /// (a fresh edit forks history) and evicts the oldest snapshot at the cap.
    pub fn record(&mut self, session: &Session) {
        self.future.clear();
        if self.past.len() == self.limit {
            self.past.pop_front();
        }
        self.past.push_back(Self::snapshot(session));
    }

    /// Restore the previous state; returns false when there is nothing to undo.
    pub fn undo(&mut self, session: &mut Session) -> bool {
        match self.past.pop_back() {
            Some(snap) => {
                self.future.push_back(Self::snapshot(session));
                Self::restore(session, snap);
                true
            }
            None => false,
        }
    }

    /// Re-apply the last undone edit; returns false when there is nothing to redo.
    pub fn redo(&mut self, session: &mut Session) -> bool {
        match self.future.pop_back() {
            Some(snap) => {
                self.past.push_back(Self::snapshot(session));
                Self::restore(session, snap);
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    pub fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
    }
}

// ---------------------------------------------------------------------------
// Starter templates (docs/ARCHITECTURE.md §2b: the graph is never a blank page of fear)
// ---------------------------------------------------------------------------

/// One-click starting points. Each builds a pre-wired, playable [`Graph`]
/// from a small hand-picked set of plugins ([`Template::plugins`], catalog
/// ids) plus the built-in DSP units, with sensible default knob values and
/// plugin states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// Aeolus with the full organ drawn, in a cathedral-sized reverb.
    CathedralOrgan,
    /// setBfree with the classic jazz registration (88 8000 000), percussion
    /// and a slow Leslie, in a small club room.
    JazzOrgan,
    /// setBfree with every drawbar out, driven through the Guitarix amp and
    /// cabinet: the rock organ sound.
    RockOrgan,
    /// Slow Surge pad into long delay and big reverb — instant atmosphere.
    AmbientPad,
    /// Singing Surge lead through a slap delay and reverb, dry path kept.
    SynthLead,
    /// GarageBand's guitar story (issue #92): a live input through stomps
    /// into an amp and cabinet. Audio In picks the input device on its face
    /// (default = PipeWire's default source) and carries the tuner.
    GuitarRig,
}

impl Template {
    pub const ALL: [Template; 6] = [
        Template::CathedralOrgan,
        Template::JazzOrgan,
        Template::RockOrgan,
        Template::AmbientPad,
        Template::SynthLead,
        Template::GuitarRig,
    ];

    /// A template whose instrument is a live input, not the keyboard: the
    /// validity test asks for Audio In → effects → Master instead of
    /// Keys → synth → Master.
    pub fn is_guitar(self) -> bool {
        matches!(self, Template::GuitarRig)
    }

    /// The name the agent tools take (`load_template`).
    pub fn id(self) -> &'static str {
        match self {
            Template::CathedralOrgan => "cathedral_organ",
            Template::JazzOrgan => "jazz_organ",
            Template::RockOrgan => "rock_organ",
            Template::AmbientPad => "ambient_pad",
            Template::SynthLead => "synth_lead",
            Template::GuitarRig => "guitar_rig",
        }
    }

    /// A template by [`Template::id`] or a short alias, any case.
    pub fn from_name(name: &str) -> Option<Template> {
        let name = name.trim().to_lowercase();
        let alias = match name.as_str() {
            "cathedral" | "church" | "church_organ" | "pipe_organ" => Template::CathedralOrgan,
            "jazz" | "jazz_rig" => Template::JazzOrgan,
            "rock" => Template::RockOrgan,
            "ambient" | "pad" => Template::AmbientPad,
            "lead" | "lead_reverb" => Template::SynthLead,
            "guitar" => Template::GuitarRig,
            _ => return Template::ALL.into_iter().find(|t| t.id() == name),
        };
        Some(alias)
    }

    pub fn title(self) -> &'static str {
        match self {
            Template::CathedralOrgan => "Cathedral Organ",
            Template::JazzOrgan => "Jazz Organ",
            Template::RockOrgan => "Rock Organ",
            Template::AmbientPad => "Ambient Pad",
            Template::SynthLead => "Synth Lead",
            Template::GuitarRig => "Guitar Rig",
        }
    }

    /// One line for the template list: the signal chain.
    pub fn blurb(self) -> &'static str {
        match self {
            Template::CathedralOrgan => "Pipe organ, full stops, cathedral reverb",
            Template::JazzOrgan => "Tonewheel 88 8000 000, percussion, slow Leslie",
            Template::RockOrgan => "Tonewheel, all drawbars, through a cranked amp",
            Template::AmbientPad => "Surge pad, long delay, big reverb",
            Template::SynthLead => "Surge lead, slap delay, reverb",
            Template::GuitarRig => "Live input, stomps, amp, cabinet",
        }
    }

    /// Catalog ids (`catalog::find`) of the plugins the template needs.
    pub fn plugins(self) -> &'static [&'static str] {
        match self {
            Template::CathedralOrgan => &["aeolus"],
            Template::JazzOrgan => &["setbfree"],
            Template::RockOrgan => &["setbfree", "guitarix"],
            Template::AmbientPad | Template::SynthLead => &["surge-xt"],
            Template::GuitarRig => &["guitarix"],
        }
    }

    /// The pre-wired patch. Edge ids are assigned sequentially from 1.
    pub fn graph(self) -> Graph {
        self.build().0
    }

    /// A ready-to-play session: patch + default knob values + plugin states.
    pub fn session(self) -> Session {
        let (graph, defaults, states) = self.build();
        let mut s = Session::new(self.title(), graph);
        for (node, param, value) in defaults {
            s.set_param(node, param, value);
        }
        for (node, plugin_id, data) in states {
            s.set_plugin_state(node, plugin_id, data);
        }
        s
    }

    #[allow(clippy::type_complexity)]
    fn build(self) -> (Graph, Vec<(NodeId, ParamId, f64)>, Vec<(NodeId, &'static str, Vec<u8>)>) {
        let mut g = Graph::default();
        g.reserve(64, 256);
        let mut defaults = Vec::new();
        let mut states = Vec::new();
        // Templates pick their own edge ids (Graph's counter is private to
        // the control plane); sequential ids are unique by construction.
        let mut eid: EdgeId = 1;
        let mut link = |g: &mut Graph, from: NodeId, to: NodeId, to_port: u8, kind: PortKind| {
            let ok = g.connect_with_id(
                eid,
                PortRef { node: from, port: 0 },
                PortRef { node: to, port: to_port },
                kind,
            );
            assert!(ok, "template wiring must be valid");
            eid += 1;
        };
        let surge = || NodeKind::Clap {
            plugin_id: crate::control::ClapId::new(SURGE_ID),
        };

        match self {
            Template::CathedralOrgan => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let organ = g.add_node(
                    NodeKind::Clap { plugin_id: crate::control::ClapId::new(crate::aeolus::AEOLUS_ID) },
                    name_bytes("PIPE ORGAN"),
                );
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("CATHEDRAL"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, organ, 0, PortKind::Midi);
                link(&mut g, organ, verb, 0, PortKind::Audio);
                link(&mut g, verb, master, 0, PortKind::Audio);
                let full = crate::aeolus::REGISTRATIONS
                    .iter()
                    .find(|r| r.name == "Full Organ")
                    .expect("Aeolus has a Full Organ registration");
                states.push((organ, crate::aeolus::AEOLUS_ID, crate::aeolus::state(full)));
                defaults.extend([
                    (organ, P_LEVEL, 0.8),
                    (verb, 0, 0.7), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.0),
                ]);
            }
            Template::JazzOrgan | Template::RockOrgan => {
                let rock = self == Template::RockOrgan;
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let organ = g.add_node(
                    NodeKind::Lv2 { uri: Lv2Uri::new(crate::organ::B_SYNTH) },
                    name_bytes("TONEWHEEL"),
                );
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, organ, 0, PortKind::Midi);
                // Jazz: 88 8000 000 with third-harmonic percussion and the
                // Leslie on its slow chorale. Rock: every drawbar out, the
                // Leslie fast, the organ's own overdrive up.
                let bars: [u8; crate::organ::DRAWBARS] =
                    if rock { [8; crate::organ::DRAWBARS] } else { crate::organ::DEFAULT_DRAWBARS };
                for (i, &pos) in bars.iter().enumerate() {
                    defaults.push((organ, crate::organ::drawbar_param(i), f64::from(crate::organ::drawbar_value(pos))));
                }
                let (leslie, perc, drive, room) = if rock { (1.0, 0.0, 0.6, 0.15) } else { (0.5, 0.5, 0.0, 0.25) };
                defaults.extend([
                    (organ, crate::organ::P_LESLIE, leslie),
                    (organ, crate::organ::P_PERC, perc),
                    (organ, crate::organ::P_DRIVE, drive),
                    (organ, crate::organ::P_REVERB, room),
                    (master, P_VOL, 0.75),
                    (master, P_WARM, if rock { 0.2 } else { 0.1 }),
                ]);
                if rock {
                    let amp = g.add_node(
                        NodeKind::Lv2 { uri: Lv2Uri::new(crate::guitar::AMP) },
                        name_bytes("AMP"),
                    );
                    let cab = g.add_node(
                        NodeKind::Lv2 { uri: Lv2Uri::new(crate::guitar::CABINET) },
                        name_bytes("CABINET"),
                    );
                    link(&mut g, organ, amp, 0, PortKind::Audio);
                    link(&mut g, amp, cab, 0, PortKind::Audio);
                    link(&mut g, cab, master, 0, PortKind::Audio);
                    defaults.extend([
                        (amp, crate::guitar::AMP_DRIVE_CTRL as ParamId, 0.7),
                        (amp, crate::guitar::AMP_PREGAIN_CTRL as ParamId, 0.5),
                    ]);
                } else {
                    link(&mut g, organ, master, 0, PortKind::Audio);
                }
            }
            Template::SynthLead => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let lead = g.add_node(surge(), name_bytes("LEAD"));
                let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("SLAP DELAY"));
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, lead, 0, PortKind::Midi);
                link(&mut g, lead, delay, 0, PortKind::Audio);
                link(&mut g, delay, verb, 0, PortKind::Audio);
                link(&mut g, verb, master, 0, PortKind::Audio);
                link(&mut g, lead, master, 1, PortKind::Audio); // dry
                defaults.extend([
                    (lead, P_LEVEL, 0.85),
                    (delay, P_TIME, 0.25),
                    (delay, P_MIX, 0.25),
                    (verb, 0, 0.35), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.1),
                ]);
            }
            Template::AmbientPad => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let pad = g.add_node(surge(), name_bytes("PAD"));
                let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, pad, 0, PortKind::Midi);
                link(&mut g, pad, delay, 0, PortKind::Audio);
                link(&mut g, delay, verb, 0, PortKind::Audio);
                link(&mut g, verb, master, 0, PortKind::Audio);
                link(&mut g, pad, master, 1, PortKind::Audio); // dry
                defaults.extend([
                    (pad, P_LEVEL, 0.7),
                    (delay, P_TIME, 0.6),
                    (delay, P_MIX, 0.45),
                    (verb, 0, 0.6), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.05),
                ]);
            }
            // Audio In → screamer → delay → reverb (stomps) → amp → cabinet →
            // Master. The guitarix LV2s come with the catalog's Guitarix card;
            // until it is installed the three units say so on their faces and
            // the rest of the rig (Audio In, Delay, Reverb) already works.
            Template::GuitarRig => {
                let ain = g.add_node(
                    NodeKind::AudioIn { device: None, channels: AudioInChannel::MonoL },
                    name_bytes("AUDIO IN"),
                );
                let scream = g.add_node(
                    NodeKind::Lv2 { uri: Lv2Uri::new(crate::guitar::SCREAM) },
                    name_bytes("SCREAM"),
                );
                let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
                let amp = g.add_node(
                    NodeKind::Lv2 { uri: Lv2Uri::new(crate::guitar::AMP) },
                    name_bytes("AMP"),
                );
                let cab = g.add_node(
                    NodeKind::Lv2 { uri: Lv2Uri::new(crate::guitar::CABINET) },
                    name_bytes("CABINET"),
                );
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, ain, scream, 0, PortKind::Audio);
                link(&mut g, scream, delay, 0, PortKind::Audio);
                link(&mut g, delay, verb, 0, PortKind::Audio);
                link(&mut g, verb, amp, 0, PortKind::Audio);
                link(&mut g, amp, cab, 0, PortKind::Audio);
                link(&mut g, cab, master, 0, PortKind::Audio);
                defaults.extend([
                    // The amp's tube stage driven to its crunch (guitar.rs:
                    // Drive is the one control that turns the modelled clean
                    // channel into an amp — the plugin's default 0.25 is
                    // nearly clean).
                    (amp, crate::guitar::AMP_DRIVE_CTRL as ParamId, 1.0),
                    (amp, crate::guitar::AMP_PREGAIN_CTRL as ParamId, 0.8),
                    // The screamer at full drive pushes the amp into its
                    // crunch (its one control, port 0).
                    (scream, 0, 1.0),
                    (delay, P_TIME, 0.5), // ~1/8th at 96 BPM (316 ms)
                    (delay, P_MIX, 0.35),
                    (verb, 0, 0.3), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.1),
                ]);
            }
        }
        (g, defaults, states)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test packages live under the cargo target dir (never /tmp — small
    /// tmpfs on this machine); fall back to the repo target dir.
    fn test_dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let d = base.join("session-tests").join(format!("{tag}-{}", unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Everything that must survive a round trip, as comparable JSON
    /// (Graph/Node/Edge don't derive PartialEq; serde does the comparing).
    fn content(s: &Session) -> Value {
        json!({
            "name": s.name,
            "graph": serde_json::to_value(&s.graph).unwrap(),
            "params": serde_json::to_value(s.params()).unwrap(),
            "matrix": s.matrix().cloned().unwrap_or(Value::Null),
        })
    }

    fn blobs(s: &Session) -> Vec<(NodeId, String, Vec<u8>)> {
        s.plugin_states()
            .iter()
            .map(|p| (p.node, p.plugin_id.clone(), p.data.clone()))
            .collect()
    }

    fn sample_session() -> Session {
        let mut s = Template::SynthLead.session();
        s.name = "Round Trip".into();
        s.set_param(1, P_LEVEL, 0.42); // overwrite a default
        s.set_matrix(Some(json!({"routes": [{"from": "keys", "to": 1, "transpose": -12}]})));
        // Opaque, non-UTF8 bytes — the session must not interpret them.
        s.set_plugin_state(1, SURGE_ID, vec![0x00, 0xff, 0x80, 0x43, 0x4c, 0x41, 0x50, 0x01]);
        s
    }

    #[test]
    fn save_load_round_trip() {
        let dir = test_dir("roundtrip");
        let pkg = dir.join("Round Trip.ksalat");
        let original = sample_session();
        original.save(&pkg).unwrap();

        // Package layout: manifest + one blob file, nothing else.
        assert!(pkg.join(SESSION_FILE).is_file());
        assert!(pkg.join(STATE_DIR).join("node-1.bin").is_file());

        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(content(&original), content(&loaded));
        assert_eq!(blobs(&original), blobs(&loaded));
        // Node-id counters survive too: new nodes don't collide.
        let mut g = loaded.graph.clone();
        let existing: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
        let id = g.add_node(NodeKind::Keys, name_bytes("LATE"));
        assert!(!existing.contains(&id));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_replaces_package_and_leaves_no_stale_files() {
        let dir = test_dir("overwrite");
        let pkg = dir.join("Set.ksalat");
        let mut first = sample_session();
        first.save(&pkg).unwrap();

        // Second save: blob removed, param changed — old blob must not linger.
        first.states.clear();
        first.set_param(1, P_LEVEL, 0.1);
        first.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert!(loaded.plugin_states().is_empty());
        assert!(!pkg.join(STATE_DIR).join("node-1.bin").exists());
        assert_eq!(loaded.param(1, P_LEVEL), Some(0.1));
        // No staging/backup litter next to the package.
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty(), "staging/backup dirs left behind: {litter:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_keeps_original_untouched() {
        let dir = test_dir("failed-save");
        let pkg = dir.join("Precious.ksalat");
        let good = sample_session();
        good.save(&pkg).unwrap();
        let before = fs::read(pkg.join(SESSION_FILE)).unwrap();

        // Orphaned blob → validation fails → original must be byte-identical.
        let mut bad = good.clone();
        bad.set_plugin_state(99, SURGE_ID, vec![1, 2, 3]);
        assert!(bad.save(&pkg).is_err());
        assert_eq!(fs::read(pkg.join(SESSION_FILE)).unwrap(), before);
        assert_eq!(blobs(&Session::load(&pkg).unwrap()), blobs(&good));
        // No staging litter from the failed attempt.
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_escape_paths_missing_files_and_future_versions() {
        let dir = test_dir("bad-load");
        let pkg = dir.join("Evil.ksalat");
        fs::create_dir_all(pkg.join(STATE_DIR)).unwrap();
        let manifest = json!({
            "format": FORMAT_VERSION,
            "name": "Evil",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
            "states": [{"node": 0, "plugin_id": "x", "file": "../outside.bin"}],
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        let err = Session::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("outside the package"), "{err}");

        // Missing blob file.
        let manifest = json!({
            "format": FORMAT_VERSION,
            "name": "Evil",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
            "states": [{"node": 0, "plugin_id": "x", "file": "state/node-0.bin"}],
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        assert!(Session::load(&pkg).is_err());

        // Future format version.
        let manifest = json!({
            "format": FORMAT_VERSION + 1,
            "name": "Future",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        let err = Session::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("newer kabelsalat"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_time_signature_round_trips_and_old_sessions_have_none() {
        use crate::transport::TimeSig;
        let dir = std::env::temp_dir().join(format!("ksalat-timesig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("t.ksalat");
        let mut s = Session::new("t", Graph::demo_patch());
        s.time_sig = Some(TimeSig { num: 7, den: 8 });
        s.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(loaded.time_sig, Some(TimeSig { num: 7, den: 8 }));
        assert!(loaded.same_content(&s));
        let mut other = s.clone();
        other.time_sig = Some(TimeSig::FOUR_FOUR);
        assert!(!other.same_content(&s), "another signature is another session");
        // A file edited by hand to a bar the clock cannot count is made countable.
        let file = pkg.join(SESSION_FILE);
        let json = std::fs::read_to_string(&file).unwrap();
        assert!(json.contains("\"time_signature\""), "{json}");
        let mut v: Value = serde_json::from_str(&json).unwrap();
        v["time_signature"] = serde_json::json!({"num": 99, "den": 5});
        std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().time_sig, Some(TimeSig { num: 12, den: 4 }));
        // A session that says nothing has none (rule 165).
        s.time_sig = None;
        let pkg2 = dir.join("plain.ksalat");
        s.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join(SESSION_FILE)).unwrap();
        assert!(!json.contains("time_signature"), "absent when unset: old readers stay happy");
        assert_eq!(Session::load(&pkg2).unwrap().time_sig, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_master_target_round_trips_and_old_sessions_have_none() {
        use crate::meter::MasterTarget;
        let dir = std::env::temp_dir().join(format!("ksalat-mastertarget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("t.ksalat");
        let mut s = Session::new("t", Graph::demo_patch());
        let target = MasterTarget { lufs: Some(-14.0), gain_db: 6.5, before_lufs: Some(-20.5), guard_db: -0.3, last_point: None, limited: false };
        s.master_target = Some(target);
        s.save(&pkg).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().master_target, Some(target));
        // "Off" is a statement too: a load turns a target off again.
        s.master_target = Some(MasterTarget::OFF);
        s.save(&pkg).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().master_target, Some(MasterTarget::OFF));
        s.master_target = None;
        let pkg2 = dir.join("plain.ksalat");
        s.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join(SESSION_FILE)).unwrap();
        assert!(!json.contains("master_target"), "absent when unset: old readers stay happy");
        assert_eq!(Session::load(&pkg2).unwrap().master_target, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn master_mute_round_trips_and_defaults_false() {
        let dir = std::env::temp_dir().join(format!("ksalat-mastermute-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("muted.ksalat");
        let mut s = Session::new("m", Graph::demo_patch());
        s.master_mute = true;
        s.save(&pkg).unwrap();
        assert!(Session::load(&pkg).unwrap().master_mute);
        s.master_mute = false;
        let pkg2 = dir.join("unmuted.ksalat");
        s.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join(SESSION_FILE)).unwrap();
        assert!(!json.contains("master_mute"), "absent when false: old readers stay happy");
        assert!(!Session::load(&pkg2).unwrap().master_mute);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rig_types_round_trip_and_are_absent_while_no_rig_is_marked() {
        use crate::keyboard::KeyboardType;
        let dir = std::env::temp_dir().join(format!("ksalat-rigtypes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("kb.ksalat");
        let mut s = Session::new("kb", Graph::demo_patch());
        s.rig_types = vec![Some(KeyboardType::PipeOrgan), None, Some(KeyboardType::Drums)];
        s.save(&pkg).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().rig_types, s.rig_types);
        let json = std::fs::read_to_string(pkg.join(SESSION_FILE)).unwrap();
        assert!(json.contains("\"drums\""), "saved by machine name: {json}");
        assert!(!json.contains("keyboard_mode"), "the old pin is never written");
        s.rig_types.clear();
        let pkg2 = dir.join("plain.ksalat");
        s.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join(SESSION_FILE)).unwrap();
        assert!(!json.contains("rig_types"), "absent when no rig is marked: old readers stay happy");
        assert!(Session::load(&pkg2).unwrap().rig_types.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_sessions_keyboard_pin_marks_every_rig() {
        use crate::control::{merge_rigs, standard_rig_graph, Rig};
        use crate::keyboard::KeyboardType;
        let dir = std::env::temp_dir().join(format!("ksalat-oldpin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let graph = merge_rigs(&[
            Rig::new("A", standard_rig_graph(true), None),
            Rig::new("B", standard_rig_graph(true), None),
        ]);
        let pkg = dir.join("old.ksalat");
        Session::new("old", graph).save(&pkg).unwrap();
        // What a session saved before #198 holds: one pin for the dock.
        let file = pkg.join(SESSION_FILE);
        let pinned = |mode: &str| {
            let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            v["keyboard_mode"] = serde_json::json!(mode);
            std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
            Session::load(&pkg).unwrap().rig_types
        };
        assert_eq!(pinned("jazz_organ"), vec![Some(KeyboardType::JazzOrgan); 2]);
        assert_eq!(pinned("pipe_organ"), vec![Some(KeyboardType::PipeOrgan); 2]);
        assert!(pinned("auto").is_empty(), "auto marks nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rig_names_round_trip_through_the_package() {
        let dir = std::env::temp_dir().join(format!("ksalat-rignames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new("names", Graph::demo_patch());
        s.rig_names = vec!["Bass".into(), "Pads".into()];
        let pkg = dir.join("names.ksalat");
        s.save(&pkg).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().rig_names, vec!["Bass".to_string(), "Pads".to_string()]);
        // Older packages (no field) load with default names.
        let mut plain = Session::new("plain", Graph::demo_patch());
        plain.rig_names.clear();
        let pkg2 = dir.join("plain.ksalat");
        plain.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join("session.json")).unwrap();
        assert!(!json.contains("rig_names"), "absent when empty: old readers stay happy");
        assert!(Session::load(&pkg2).unwrap().rig_names.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn midi_sources_round_trip_through_the_package() {
        let dir = std::env::temp_dir().join(format!("ksalat-midisources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let one_device = MidiSources { node: 1, keyboard: false, hardware: true, device: Some("U6MIDI Pro:U6MIDI Pro MIDI 1".into()) };
        let keyboard_only = MidiSources { node: 4, keyboard: true, hardware: false, device: None };
        let mut s = Session::new("inputs", Graph::demo_patch());
        s.set_midi_sources(keyboard_only.clone());
        s.set_midi_sources(one_device.clone());
        let pkg = dir.join("inputs.ksalat");
        s.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(loaded.midi_sources(), [one_device, keyboard_only.clone()]);
        assert!(!loaded.same_content(&Session::new("inputs", Graph::demo_patch())), "an input selection is content");

        // A synth back on the keyboard and all hardware has no row, and a
        // row that names only its node means the same.
        s.set_midi_sources(MidiSources { node: 1, keyboard: true, hardware: true, device: None });
        assert_eq!(s.midi_sources(), [keyboard_only]);
        let bare: MidiSources = serde_json::from_str(r#"{"node": 1}"#).unwrap();
        assert!(bare.is_default());
        // A row leaves with its node.
        s.graph.remove_node(4);
        s.prune_to_graph();
        assert!(s.midi_sources().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const FM1: &str = "M-Wave FM-1:M-Wave FM-1 MIDI 1";
    const U6: &str = "U6MIDI Pro:U6MIDI Pro MIDI 1";

    fn ports(named: &[(u32, &str)]) -> BTreeMap<u32, String> {
        named.iter().map(|&(i, p)| (i, p.to_string())).collect()
    }

    fn mapped(m: &HwIndexMap) -> Vec<u32> {
        (0..HW_DEVICES as u32).map(|d| m.get(d)).collect()
    }

    fn is_permutation(m: &HwIndexMap) -> bool {
        let mut all = mapped(m);
        all.sort_unstable();
        all == (0..HW_DEVICES as u32).collect::<Vec<_>>()
    }

    #[test]
    fn device_indices_follow_their_port_into_another_scan_order() {
        let saved = ports(&[(0, FM1), (1, U6)]);
        let same = HwIndexMap::new(&saved, &[FM1, U6], |_| panic!("both are listed"));
        assert_eq!(mapped(&same), (0..HW_DEVICES as u32).collect::<Vec<_>>(), "the same scan keeps every index");

        let swapped = HwIndexMap::new(&saved, &[U6, FM1], |_| panic!("both are listed"));
        assert_eq!((swapped.get(0), swapped.get(1)), (1, 0));
        assert!(is_permutation(&swapped));

        // A device not plugged in gets the place the app gives it; an index
        // nobody named stays where it is, or takes the one that came free.
        let mut placed = Vec::new();
        let missing = HwIndexMap::new(&saved, &[FM1], |p| { placed.push(p.to_string()); Some(1) });
        assert_eq!(placed, [U6]);
        assert_eq!((missing.get(0), missing.get(1), missing.get(2)), (0, 1, 2));
        let elsewhere = HwIndexMap::new(&ports(&[(1, U6)]), &[FM1], |_| Some(5));
        assert_eq!((elsewhere.get(0), elsewhere.get(1), elsewhere.get(2), elsewhere.get(5)), (0, 5, 2, 1));
        assert!(is_permutation(&elsewhere));

        // A full device list has no place: the device goes past it, never
        // onto a listed one, and the listed device left over is unclaimed.
        let full = HwIndexMap::new(&ports(&[(3, U6)]), &[FM1; HW_DEVICES], |_| None);
        assert!(full.get(3) >= HW_DEVICES as u32);
        assert_eq!(full.unclaimed().collect::<Vec<_>>(), [3]);
        assert_eq!(missing.unclaimed().count(), 0);

        // Two identical keyboards keep apart, also while one is unplugged.
        let twins = HwIndexMap::new(&ports(&[(1, FM1), (3, FM1)]), &[FM1, U6, FM1], |_| None);
        assert_eq!((twins.get(1), twins.get(3)), (0, 2));
        assert!(is_permutation(&twins));
        let one_twin = HwIndexMap::new(&ports(&[(0, FM1), (1, FM1)]), &[FM1, U6], |_| Some(2));
        assert_eq!((one_twin.get(0), one_twin.get(1)), (0, 2), "the second FM-1 waits in a place of its own");

        // A file without names (before #227) keeps its indices.
        let older = HwIndexMap::new(&BTreeMap::new(), &[U6, FM1], |_| None);
        assert_eq!(mapped(&older), (0..HW_DEVICES as u32).collect::<Vec<_>>());
        assert_eq!(older.get(40), 40, "beyond the device list an index passes as it is");
    }

    #[test]
    fn port_names_and_rig_devices_round_trip_through_session_and_rig_preset() {
        let dir = std::env::temp_dir().join(format!("ksalat-hwports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new("devices", Graph::demo_patch());
        s.set_hw_ports(ports(&[(0, FM1), (2, U6)]));
        s.rig_devices = vec![None, Some(2)];
        let pkg = dir.join("devices.ksalat");
        s.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(loaded.hw_ports(), &ports(&[(0, FM1), (2, U6)]));
        assert_eq!(loaded.rig_devices, [None, Some(2)]);
        assert!(!loaded.same_content(&Session::new("devices", Graph::demo_patch())), "a rig device is content");

        let mut preset = RigPreset::new("Bass", "Bass", Session::new("Bass", Graph::demo_patch()));
        preset.session.set_hw_ports(ports(&[(2, U6)]));
        preset.midi_device = Some(2);
        let rpkg = RigPreset::package_path(&dir, "Bass").unwrap();
        preset.save(&rpkg).unwrap();
        let back = RigPreset::load(&rpkg).unwrap();
        assert_eq!((back.midi_device, back.session.hw_ports()), (Some(2), &ports(&[(2, U6)])));

        // Files that name no ports write no such field and load as before.
        let plain = dir.join("plain.ksalat");
        Session::new("plain", Graph::demo_patch()).save(&plain).unwrap();
        let text = std::fs::read_to_string(plain.join(SESSION_FILE)).unwrap();
        assert!(!text.contains("hw_ports") && !text.contains("rig_devices"));
        let older = Session::load(&plain).unwrap();
        assert!(older.hw_ports().is_empty() && older.rig_devices.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_redo_restores_graph_params_and_matrix() {
        let mut s = Template::SynthLead.session();
        let mut h = History::new();
        let start = content(&s);

        // Edit 1: add a node + cable, turn a knob, set matrix.
        h.record(&s);
        let extra = s.graph.add_node(NodeKind::Dsp(DspKind::Limiter), name_bytes("LIMITER"));
        assert!(s.graph.connect_with_id(
            50,
            PortRef { node: 2, port: 0 },
            PortRef { node: extra, port: 0 },
            PortKind::Audio,
        ));
        s.set_param(1, P_LEVEL, 0.2);
        s.set_matrix(Some(json!({"x": 1})));
        let edited = content(&s);
        assert_ne!(start, edited);

        // Undo → exactly the starting state; redo → exactly the edited state.
        assert!(h.undo(&mut s));
        assert_eq!(content(&s), start);
        assert!(h.can_redo());
        assert!(h.redo(&mut s));
        assert_eq!(content(&s), edited);
        assert!(!h.can_redo());

        // A fresh edit after an undo forks history: the redo branch is dropped.
        assert!(h.undo(&mut s)); // back to start; redo now offers `edited`
        assert_eq!(content(&s), start);
        assert!(h.can_redo());
        h.record(&s); // fork: the pending redo is discarded
        s.set_param(1, P_LEVEL, 0.9);
        assert!(!h.can_redo());
        assert!(h.undo(&mut s));
        assert_eq!(content(&s), start);

        // Empty history is a no-op, not a panic.
        h.clear();
        assert!(!h.undo(&mut s));
        assert!(!h.redo(&mut s));
    }

    #[test]
    fn undo_respects_the_limit() {
        let mut s = Session::default();
        let mut h = History::with_limit(3);
        for i in 0..5 {
            h.record(&s);
            s.set_param(0, 0, i as f64);
        }
        // Only the last 3 edits are undoable.
        for _ in 0..3 {
            assert!(h.undo(&mut s));
        }
        assert!(!h.can_undo());
        assert_eq!(s.param(0, 0), Some(1.0)); // edit #2's snapshot (0-based i=1)
    }

    #[test]
    fn undo_does_not_touch_blobs_or_name() {
        let mut s = sample_session();
        let mut h = History::new();
        h.record(&s);
        s.graph.add_node(NodeKind::Keys, name_bytes("X"));
        s.name = "Renamed".into();
        assert!(h.undo(&mut s));
        assert_eq!(s.name, "Renamed"); // names are not edits
        assert_eq!(blobs(&s), blobs(&sample_session())); // blobs not snapshotted
    }

    #[test]
    fn templates_are_valid_and_playable() {
        for t in Template::ALL {
            let s = t.session();
            s.validate().unwrap_or_else(|e| panic!("{}: {e}", t.title()));
            let g = &s.graph;
            assert!(!g.nodes.is_empty());

            // Every edge references real nodes; no self/duplicate cables.
            for e in &g.edges {
                assert!(g.node(e.from.node).is_some(), "{}: dangling from", t.title());
                assert!(g.node(e.to.node).is_some(), "{}: dangling to", t.title());
                assert_ne!(e.from.node, e.to.node);
            }
            let mut ids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
            ids.sort_unstable();
            assert!(ids.windows(2).all(|w| w[0] != w[1]));

            // Playable shape: every template makes sound into a master —
            // keyboard templates Keys → Clap synth → master (MIDI + audio),
            // the guitar rig Audio In → effects → master (audio only).
            let keys: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::Keys)).map(|n| n.id).collect();
            let inputs: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::AudioIn { .. })).map(|n| n.id).collect();
            let synths: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::Clap { .. }) || crate::organ::is_organ(&n.kind))
                .map(|n| n.id).collect();
            let masters: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).collect();
            assert!(!masters.is_empty());
            if t.is_guitar() {
                assert!(!inputs.is_empty(), "{}: no Audio In", t.title());
            } else {
                assert!(!keys.is_empty() && !synths.is_empty(), "{}: no keys/synth", t.title());
                assert!(synths.iter().any(|&syn| keys.iter().any(|&k| g.has_route(k, syn, PortKind::Midi))));
            }

            // Audio reachability: the template's sound source reaches a
            // master over audio edges.
            let reaches_master = |syn: NodeId| -> bool {
                let mut seen = vec![syn];
                let mut stack = vec![syn];
                while let Some(n) = stack.pop() {
                    if masters.contains(&n) {
                        return true;
                    }
                    for e in &g.edges {
                        if e.kind == PortKind::Audio && e.from.node == n && !seen.contains(&e.to.node) {
                            seen.push(e.to.node);
                            stack.push(e.to.node);
                        }
                    }
                }
                false
            };
            let sources: Vec<NodeId> = if t.is_guitar() { inputs } else { synths };
            assert!(sources.iter().any(|&syn| reaches_master(syn)), "{}: no audio path to master", t.title());

            // Default knobs reference real nodes and are finite.
            for p in s.params() {
                assert!(g.node(p.node).is_some());
                assert!(p.value.is_finite());
            }
            // Templates round-trip too.
            let dir = test_dir("template");
            let pkg = dir.join(format!("{}.ksalat", t.title()));
            s.save(&pkg).unwrap();
            let loaded = Session::load(&pkg).unwrap();
            assert_eq!(content(&s), content(&loaded));
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn templates_name_their_plugins_and_answer_to_their_ids() {
        for t in Template::ALL {
            assert!(!t.plugins().is_empty(), "{} needs a plugin", t.title());
            for id in t.plugins() {
                assert!(crate::catalog::find(id).is_some(), "{}: '{id}' is not in the catalog", t.title());
            }
            assert_eq!(Template::from_name(t.id()), Some(t));
            assert_eq!(Template::from_name(&t.id().to_uppercase()), Some(t));
        }
        assert_eq!(Template::from_name("church"), Some(Template::CathedralOrgan));
        assert_eq!(Template::from_name("jazz"), Some(Template::JazzOrgan));
        assert_eq!(Template::from_name("drums_bass"), None);
    }

    #[test]
    fn the_cathedral_organ_starts_with_the_full_organ_drawn() {
        let s = Template::CathedralOrgan.session();
        let organ = s.graph.nodes.iter().find(|n| matches!(&n.kind, NodeKind::Clap { .. })).unwrap();
        let st = s.plugin_state(organ.id).expect("the organ carries a registration");
        assert_eq!(st.plugin_id, crate::aeolus::AEOLUS_ID);
        let full = crate::aeolus::REGISTRATIONS.iter().find(|r| r.name == "Full Organ").unwrap();
        assert_eq!(crate::aeolus::drawn(&st.data), crate::aeolus::drawn(&crate::aeolus::state(full)));
        assert_ne!(crate::aeolus::drawn(&st.data), crate::aeolus::drawn(&crate::aeolus::initial_state(crate::aeolus::AEOLUS_ID).unwrap()));
    }

    #[test]
    fn the_organ_templates_set_their_registrations() {
        let bars = |t: Template| -> Vec<u8> {
            let s = t.session();
            let organ = s.graph.nodes.iter().find(|n| crate::organ::is_organ(&n.kind)).unwrap().id;
            (0..crate::organ::DRAWBARS)
                .map(|i| crate::organ::drawbar_position(s.param(organ, crate::organ::drawbar_param(i)).unwrap() as f32))
                .collect()
        };
        assert_eq!(bars(Template::JazzOrgan), vec![8, 8, 8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(bars(Template::RockOrgan), vec![8; 9]);
        // The rock organ runs through the guitar amp and cabinet.
        let g = Template::RockOrgan.graph();
        for uri in [crate::guitar::AMP, crate::guitar::CABINET] {
            assert!(g.nodes.iter().any(|n| matches!(&n.kind, NodeKind::Lv2 { uri: u } if u.as_str() == uri)), "{uri}");
        }
    }

    // --- Multi-rig sessions (task-22) ---------------------------------------

    /// One rig: Keys(0) → Surge(1) → Master(2), cables 1 and 2.
    fn mini_rig(name: &str, dev: Option<u32>) -> Rig {
        let mut g = Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE_ID) }, name_bytes("SYNTH"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        assert!(g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi));
        assert!(g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio));
        Rig::new(name, g, dev)
    }

    fn rig_content(s: &MultiRigSession) -> Value {
        json!({
            "name": s.name,
            "active": s.active,
            "rigs": s.rigs.iter().map(|r| json!({
                "name": r.name,
                "midi_device": r.midi_device,
                "graph": serde_json::to_value(&r.graph).unwrap(),
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn multi_rig_save_load_round_trip() {
        let dir = test_dir("multi-rig-roundtrip");
        let pkg = dir.join("Two Rigs.ksalat");
        let original = MultiRigSession::new(
            "Two Rigs",
            vec![mini_rig("A", Some(2)), mini_rig("B", None)],
            1,
        );
        original.save(&pkg).unwrap();
        assert!(pkg.join(MULTI_RIG_FILE).is_file());

        let loaded = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(rig_content(&original), rig_content(&loaded));
        // The merged graph matches the control-plane merge exactly.
        assert_eq!(
            serde_json::to_value(loaded.merged_graph()).unwrap(),
            serde_json::to_value(crate::control::merge_rigs(&original.rigs)).unwrap()
        );
        // Overwrite: change active + device, no stale manifest content.
        let mut edited = loaded;
        edited.active = 0;
        edited.rigs[1].midi_device = Some(7);
        edited.save(&pkg).unwrap();
        let reloaded = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(reloaded.active, 0);
        assert_eq!(reloaded.rigs[1].midi_device, Some(7));
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty(), "staging/backup dirs left behind: {litter:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_failed_save_keeps_original() {
        let dir = test_dir("multi-rig-failed-save");
        let pkg = dir.join("Precious Rigs.ksalat");
        let good = MultiRigSession::new("Good", vec![mini_rig("A", Some(2))], 0);
        good.save(&pkg).unwrap();
        let before = fs::read(pkg.join(MULTI_RIG_FILE)).unwrap();

        // Out-of-range active index → validation fails → original untouched.
        let mut bad = good.clone();
        bad.active = 5;
        assert!(bad.save(&pkg).is_err());
        assert_eq!(fs::read(pkg.join(MULTI_RIG_FILE)).unwrap(), before);
        // Empty rig list and empty names are rejected too.
        assert!(MultiRigSession::new("X", vec![], 0).save(&pkg).is_err());
        assert!(MultiRigSession::new("X", vec![mini_rig("  ", None)], 0).save(&pkg).is_err());
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_load_wraps_single_graph_session() {
        // Old single-graph packages open in multi-rig mode as one rig.
        let dir = test_dir("multi-rig-fallback");
        let pkg = dir.join("Old Song.ksalat");
        let single = sample_session();
        single.save(&pkg).unwrap();
        let multi = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(multi.name, single.name);
        assert_eq!(multi.rigs.len(), 1);
        assert_eq!(multi.active, 0);
        assert_eq!(multi.rigs[0].midi_device, None);
        assert_eq!(
            serde_json::to_value(&multi.rigs[0].graph).unwrap(),
            serde_json::to_value(&single.graph).unwrap()
        );
        // Single-graph Session::load still works on the same package.
        assert!(Session::load(&pkg).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_load_rejects_future_versions() {
        let dir = test_dir("multi-rig-bad-load");
        let pkg = dir.join("Future Rigs.ksalat");
        fs::create_dir_all(&pkg).unwrap();
        let manifest = json!({
            "format": MULTI_RIG_FORMAT_VERSION + 1,
            "name": "Future",
            "rigs": [],
            "active": 0,
        });
        fs::write(pkg.join(MULTI_RIG_FILE), manifest.to_string()).unwrap();
        let err = MultiRigSession::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("newer kabelsalat"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_and_package_path_guards() {
        let mut s = sample_session();
        s.graph.remove_node(1); // the Surge node
        assert!(s.validate().is_err()); // orphaned param + blob
        s.prune_to_graph();
        s.validate().unwrap();
        assert!(s.plugin_states().is_empty());
        assert!(s.param(1, P_LEVEL).is_none());

        let parent = Path::new("/home/user/sessions");
        assert_eq!(
            Session::package_path(parent, "My Song").unwrap(),
            Path::new("/home/user/sessions/My Song.ksalat")
        );
        for bad in ["", "  ", "..", "a/b", "a\\b"] {
            assert!(Session::package_path(parent, bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rig_preset_round_trip_list_and_guards() {
        let dir = test_dir("rig-presets");
        let mut s = sample_session();
        s.name = "Lead Preset".into();
        let mut preset = RigPreset::new("Lead Preset", "Lead", s);
        preset.mix = RigMix { vol: 0.65, pan: -0.35, mute: false, solo: true };
        preset.midi_device = Some(3);
        preset.clap_meta = vec![RigClapMeta {
            node: 1,
            keyboard: false,
            hardware: true,
            patch_name: Some("Wurli".into()),
        }];

        let pkg = RigPreset::package_path(&dir, "Lead Preset").unwrap();
        preset.save(&pkg).unwrap();
        assert!(pkg.join(RIG_PRESET_FILE).is_file());
        assert!(pkg.join(STATE_DIR).join("node-1.bin").is_file());
        assert_eq!(list_rig_presets(&dir), vec!["Lead Preset".to_string()]);

        let loaded = RigPreset::load(&pkg).unwrap();
        assert_eq!(loaded.name, "Lead Preset");
        assert_eq!(loaded.rig_name, "Lead");
        assert_eq!(loaded.mix, preset.mix);
        assert_eq!(loaded.midi_device, Some(3));
        assert_eq!(loaded.clap_meta, preset.clap_meta);
        assert_eq!(content(&loaded.session), content(&preset.session));
        assert_eq!(blobs(&loaded.session), blobs(&preset.session));
        assert!(loaded.session.rig_types.is_empty(), "an unmarked rig loads unmarked");

        // A marked rig's preset keeps the mark (#198).
        let mut marked = preset.clone();
        marked.session.rig_types = vec![Some(crate::keyboard::KeyboardType::Bass)];
        let pkg_marked = RigPreset::package_path(&dir, "Marked").unwrap();
        marked.save(&pkg_marked).unwrap();
        assert_eq!(RigPreset::load(&pkg_marked).unwrap().session.rig_types, vec![Some(crate::keyboard::KeyboardType::Bass)]);
        crate::session::delete_rig_preset(&dir, "Marked").unwrap();

        let mut bad = preset.clone();
        bad.session.graph.nodes[0].rig = 2;
        assert!(bad.validate().is_err());

        let evil = dir.join("Evil.ksalat-rig");
        fs::create_dir_all(&evil).unwrap();
        fs::write(
            evil.join(RIG_PRESET_FILE),
            json!({
                "format": RIG_PRESET_FORMAT_VERSION,
                "name": "Evil",
                "graph": serde_json::to_value(Graph::default()).unwrap(),
                "states": [{"node": 0, "plugin_id": "x", "file": "../leak.bin"}],
            })
            .to_string(),
        )
        .unwrap();
        assert!(RigPreset::load(&evil).unwrap_err().to_string().contains("outside the package"));
        fs::write(
            evil.join(RIG_PRESET_FILE),
            json!({
                "format": RIG_PRESET_FORMAT_VERSION + 1,
                "name": "Future",
                "graph": serde_json::to_value(Graph::default()).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        assert!(RigPreset::load(&evil).unwrap_err().to_string().contains("newer kabelsalat"));

        delete_rig_preset(&dir, "Lead Preset").unwrap();
        delete_rig_preset(&dir, "Evil").unwrap();
        assert!(list_rig_presets(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_knobs_round_trip_and_prune() {
        let dir = test_dir("strip-knobs");
        let mut s = sample_session();
        s.set_strip_knob(1, "OSC 1 VOL".into());
        s.set_strip_knob(2, "MIX".into());
        s.set_strip_knob(2, "   ".into()); // empty clears
        assert_eq!(s.strip_knob(1), Some("OSC 1 VOL"));
        assert_eq!(s.strip_knob(2), None);

        let pkg = Session::package_path(&dir, "Quick Knobs").unwrap();
        s.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(loaded.strip_knob(1), Some("OSC 1 VOL"));
        assert!(loaded.same_content(&s));

        // Removing the node makes validate fail until pruned.
        let mut orphaned = loaded.clone();
        orphaned.graph.remove_node(1);
        assert!(orphaned.validate().is_err());
        orphaned.prune_to_graph();
        orphaned.validate().unwrap();
        assert!(orphaned.strip_knobs().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn setlist_round_trip_and_prune() {
        use crate::setlist::{FootSwitch, Part, PartRig, SetlistPos, Song};
        let dir = test_dir("setlist");
        let mut s = sample_session();
        let mut verse = Part::new(
            "Verse",
            vec![PartRig { rig: 0, key_lo: 36, key_hi: 84, transpose: -12, level: 0.8 }],
        );
        verse.set_param(1, P_LEVEL, 0.65);
        let mut song = Song::new("Opener", vec![verse]);
        song.bpm = Some(124.0);
        song.time_sig = Some(crate::transport::TimeSig::normalized(6, 8));
        s.setlist.songs.push(song);
        s.setlist.active = Some(SetlistPos { song: 0, part: 0 });
        s.setlist.next_cc = Some(FootSwitch { cc: 64, channel: None });
        s.setlist.prev_cc = Some(FootSwitch { cc: 67, channel: Some(0) });

        let pkg = Session::package_path(&dir, "Gig").unwrap();
        s.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(loaded.setlist, s.setlist);
        assert!(loaded.same_content(&s));

        // Removing the node referenced by a part's stored knob fails validate until pruned.
        let mut orphaned = loaded.clone();
        orphaned.graph.remove_node(1);
        assert!(orphaned.validate().is_err());
        orphaned.prune_to_graph();
        orphaned.validate().unwrap();
        assert!(orphaned.setlist.songs[0].parts[0].params.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
