//! Shared instruments across songs (#275).
//!
//! Kabelsalat hosts every rig in one rack (`Graph`), and each setlist `Part`
//! holds a view (`PartRig`, `Part::fx`, `Part::params`, `Part::plugin_params`)
//! onto those hosted rigs. Sharing one piano across twenty songs therefore needs
//! no second alias layer: all twenty songs point at the same rig, each with its
//! own level, pan, split, velocity layer, effect-chain bypasses and knob values,
//! while hosting one plugin instance.
//!
//! Per-song sound differences follow one clear rule:
//! - **Same plugin + same saved preset/state**: one shared instance; knob,
//!   level, pan, split and effect-chain differences are applied at the switch
//!   without cutting held notes.
//! - **Different plugin or different saved preset/state**: separate hosted
//!   instances (reloading plugin state mid-show resets plugin voices).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Level, PartRig, Setlist, MAX_RIGS};
use crate::control::{clap_plugin_key, DspKind, Graph, Node, NodeId, NodeKind, PortKind, PortRef};
use crate::session::PluginState;

/// Per-part bypass state of one audio effect node on a rig's chain (#275).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxBypass {
    pub node: NodeId,
    pub bypass: bool,
}

/// How one hosted rig is used across the setlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharingKind {
    /// One hosted instance used by multiple songs (or at show/set level).
    Shared,
    /// Another rig in the show hosts the same plugin (separate preset/state).
    SeparatePreset,
    /// Used by at most one song and no other rig hosts the same plugin.
    Single,
}

impl SharingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SharingKind::Shared => "shared",
            SharingKind::SeparatePreset => "separate_preset",
            SharingKind::Single => "single",
        }
    }
}

/// How per-song sound differences reach a rig's instrument(s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundChange {
    /// Level, pan, split, effect chain and knob values apply at the switch.
    KnobsAtSwitch,
    /// Separate hosted instance for its own preset/state.
    OwnInstance,
}

impl SoundChange {
    pub fn as_str(self) -> &'static str {
        match self {
            SoundChange::KnobsAtSwitch => "knobs_at_switch",
            SoundChange::OwnInstance => "own_instance",
        }
    }
}

/// Hosted instrument summary for one rig in the show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RigInstances {
    pub rig: u8,
    pub nodes: Vec<NodeId>,
    pub plugins: Vec<String>,
    pub level: Level,
    pub songs: Vec<usize>,
    pub sharing: SharingKind,
    pub sound_change: SoundChange,
    pub has_part_knobs: bool,
    pub has_part_fx: bool,
}

/// Whole-show hosted instrument accounting (UI, `get_setlist`, and #207).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShowInstances {
    pub hosted: usize,
    pub shared: usize,
    pub separate_presets: usize,
    pub rigs: Vec<RigInstances>,
}

impl ShowInstances {
    /// Short header badge for the Setlist view, e.g. `"1 instance · 20 songs"`.
    pub fn summary_label(&self, song_count: usize) -> String {
        let inst = if self.hosted == 1 { "instance" } else { "instances" };
        let songs = if song_count == 1 { "song" } else { "songs" };
        if self.shared > 0 && song_count > 0 {
            format!("{} {inst} ({} shared) · {song_count} {songs}", self.hosted, self.shared)
        } else if song_count > 0 {
            format!("{} {inst} · {song_count} {songs}", self.hosted)
        } else {
            format!("{} {inst} hosted", self.hosted)
        }
    }

    pub fn to_json(&self, rig_name: impl Fn(usize) -> String) -> Value {
        let rigs: Vec<Value> = self
            .rigs
            .iter()
            .map(|r| {
                json!({
                    "rig": r.rig,
                    "name": rig_name(r.rig as usize),
                    "nodes": r.nodes,
                    "plugins": r.plugins,
                    "songs": r.songs,
                    "song_count": r.songs.len(),
                    "sharing": r.sharing.as_str(),
                    "sound_changes": r.sound_change.as_str(),
                    "has_part_knobs": r.has_part_knobs,
                    "has_part_fx": r.has_part_fx,
                })
            })
            .collect();
        json!({
            "hosted": self.hosted,
            "shared": self.shared,
            "separate_presets": self.separate_presets,
            "rigs": rigs,
        })
    }
}

/// True when `node` is a hosted plugin instrument (CLAP synth or LV2 instrument).
pub fn is_instrument_node(graph: &Graph, node: &Node) -> bool {
    let has_audio_in = graph.edges.iter().any(|e| e.kind == PortKind::Audio && e.to.node == node.id);
    match &node.kind {
        NodeKind::Clap { .. } => !has_audio_in,
        NodeKind::Lv2 { .. } => {
            if crate::organ::is_organ(&node.kind) {
                return true;
            }
            let has_midi_in = graph.edges.iter().any(|e| e.kind == PortKind::Midi && e.to.node == node.id);
            let has_audio_out = graph.edges.iter().any(|e| e.kind == PortKind::Audio && e.from.node == node.id);
            has_midi_in || (!has_audio_in && has_audio_out)
        }
        _ => false,
    }
}

/// True when `node` is an audio effect node whose bypass a part can toggle.
pub fn is_audio_effect(graph: &Graph, node: &Node) -> bool {
    match &node.kind {
        NodeKind::Dsp(k) => !matches!(k, DspKind::Metronome | DspKind::Bus),
        NodeKind::Lv2 { .. } => !is_instrument_node(graph, node),
        NodeKind::Clap { .. } => graph.edges.iter().any(|e| e.kind == PortKind::Audio && e.to.node == node.id),
        _ => false,
    }
}

fn plugin_key(kind: &NodeKind) -> Option<String> {
    match kind {
        NodeKind::Clap { plugin_id } => Some(clap_plugin_key(plugin_id.as_str()).to_string()),
        NodeKind::Lv2 { uri } => Some(uri.as_str().to_string()),
        _ => None,
    }
}

/// Ordered instrument signature of one rig: `(plugin_key, saved_state_bytes)`
/// for every hosted instrument node in that rig.
pub type InstrumentSig = Vec<(String, Option<Vec<u8>>)>;

pub fn rig_instrument_signature(graph: &Graph, rig: u8, states: &[PluginState]) -> InstrumentSig {
    let mut nodes: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| n.rig == rig && is_instrument_node(graph, n))
        .collect();
    nodes.sort_by_key(|n| n.id);
    nodes
        .into_iter()
        .filter_map(|n| {
            let key = plugin_key(&n.kind)?;
            let data = states
                .iter()
                .find(|st| st.node == n.id)
                .map(|st| st.data.clone());
            Some((key, data))
        })
        .collect()
}

/// Result of checking whether an imported rig can share an existing rig's
/// hosted instrument instance(s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchResult {
    /// Same plugin(s) and identical saved state: reuse this existing rig.
    Shared(u8),
    /// Same plugin(s) exist on this rig, but saved preset/state differs: host separately.
    DifferentPreset(u8),
    /// No existing rig hosts the same instrument plugin(s): host separately.
    DifferentPlugin,
}

/// Check whether `imp_rig` of `imp_graph` can share an existing rig in `cur_graph`.
///
/// Rigs explicitly pinned to Show or Set level are left untouched so song
/// switches never override a pinned show-level rig (#273).
pub fn find_matching_rig(
    cur_graph: &Graph,
    cur_states: &[PluginState],
    setlist: &Setlist,
    imp_graph: &Graph,
    imp_rig: u8,
    imp_states: &[PluginState],
) -> MatchResult {
    let imp_sig = rig_instrument_signature(imp_graph, imp_rig, imp_states);
    if imp_sig.is_empty() {
        return MatchResult::DifferentPlugin;
    }
    let cur_rigs = cur_graph
        .nodes
        .iter()
        .map(|n| n.rig as usize + 1)
        .max()
        .unwrap_or(0)
        .min(MAX_RIGS);
    let mut preset_diff: Option<u8> = None;
    for r in 0..cur_rigs as u8 {
        if matches!(setlist.rig_pinned(r), Some(Level::Show | Level::Set(_))) {
            continue;
        }
        let cur_sig = rig_instrument_signature(cur_graph, r, cur_states);
        if cur_sig.len() != imp_sig.len() {
            continue;
        }
        let same_plugins = cur_sig.iter().zip(&imp_sig).all(|(a, b)| a.0 == b.0);
        if !same_plugins {
            continue;
        }
        let same_states = cur_sig.iter().zip(&imp_sig).all(|(a, b)| a.1 == b.1);
        if same_states {
            return MatchResult::Shared(r);
        }
        if preset_diff.is_none() {
            preset_diff = Some(r);
        }
    }
    match preset_diff {
        Some(r) => MatchResult::DifferentPreset(r),
        None => MatchResult::DifferentPlugin,
    }
}

/// Merge an imported rig's instruments and audio effect chain onto an existing
/// shared rig (`target_rig`) without adding any instrument instance.
///
/// Returns `(node_map, unused_existing_fx, newly_added_fx)`:
/// - `node_map`: pairs `(imported_node_id, target_node_id)` for instruments and
///   effects so imported params/knobs map cleanly onto `target_rig`.
/// - `unused_existing_fx`: audio effect nodes already on `target_rig` that the
///   imported rig does not use (should be bypassed in the new song's part).
/// - `newly_added_fx`: audio effect nodes newly spliced into `target_rig`
///   (should be bypassed in existing parts that use `target_rig`, and active
///   in the new song's part).
pub fn merge_rig_effects_into(
    target_graph: &mut Graph,
    target_rig: u8,
    imp_graph: &Graph,
    imp_rig: u8,
) -> (Vec<(NodeId, NodeId)>, Vec<NodeId>, Vec<NodeId>) {
    let mut node_map: Vec<(NodeId, NodeId)> = Vec::new();

    // 1. Pair instrument nodes 1:1 in node-id order.
    let mut tgt_insts: Vec<NodeId> = target_graph
        .nodes
        .iter()
        .filter(|n| n.rig == target_rig && is_instrument_node(target_graph, n))
        .map(|n| n.id)
        .collect();
    tgt_insts.sort_unstable();
    let mut imp_insts: Vec<NodeId> = imp_graph
        .nodes
        .iter()
        .filter(|n| n.rig == imp_rig && is_instrument_node(imp_graph, n))
        .map(|n| n.id)
        .collect();
    imp_insts.sort_unstable();
    for (imp_id, tgt_id) in imp_insts.into_iter().zip(tgt_insts) {
        node_map.push((imp_id, tgt_id));
    }

    // 2. Pair audio effects by NodeKind; splice any unmatched imported effects
    //    into `target_rig` before its MasterOut.
    let mut tgt_fx: Vec<(NodeId, NodeKind)> = target_graph
        .nodes
        .iter()
        .filter(|n| n.rig == target_rig && is_audio_effect(target_graph, n))
        .map(|n| (n.id, n.kind.clone()))
        .collect();
    tgt_fx.sort_by_key(|(id, _)| *id);
    let mut imp_fx: Vec<&Node> = imp_graph
        .nodes
        .iter()
        .filter(|n| n.rig == imp_rig && is_audio_effect(imp_graph, n))
        .collect();
    imp_fx.sort_by_key(|n| n.id);

    let mut matched_tgt = vec![false; tgt_fx.len()];
    let mut to_add: Vec<&Node> = Vec::new();
    for fx in imp_fx {
        if let Some(i) = tgt_fx
            .iter()
            .enumerate()
            .position(|(i, (_, k))| !matched_tgt[i] && *k == fx.kind)
        {
            matched_tgt[i] = true;
            node_map.push((fx.id, tgt_fx[i].0));
        } else {
            to_add.push(fx);
        }
    }

    let unused_existing_fx: Vec<NodeId> = tgt_fx
        .iter()
        .enumerate()
        .filter(|(i, _)| !matched_tgt[*i])
        .map(|(_, (id, _))| *id)
        .collect();

    let mut newly_added_fx = Vec::with_capacity(to_add.len());
    if !to_add.is_empty() {
        let master = target_graph
            .nodes
            .iter()
            .find(|n| n.rig == target_rig && matches!(n.kind, NodeKind::MasterOut))
            .map(|n| n.id);
        let mut next_edge = target_graph.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        for fx in to_add {
            let new_id = target_graph.add_node(fx.kind.clone(), fx.name);
            target_graph.set_rig(new_id, target_rig);
            if let Some(m) = master {
                // Splice right before MasterOut: reroute every audio edge from
                // this rig into MasterOut to feed `new_id`, then wire `new_id`
                // to MasterOut (preserving stereo port wiring).
                let incoming: Vec<(u32, PortRef, u8)> = target_graph
                    .edges
                    .iter()
                    .filter(|e| {
                        e.kind == PortKind::Audio
                            && e.to.node == m
                            && target_graph.node(e.from.node).is_some_and(|n| n.rig == target_rig)
                    })
                    .map(|e| (e.id, e.from, e.to.port))
                    .collect();
                let mut seen_from = Vec::new();
                let mut master_ports = Vec::new();
                for (eid, from, mport) in incoming {
                    target_graph.disconnect(eid);
                    if !seen_from.contains(&from) {
                        seen_from.push(from);
                        target_graph.connect_with_id(
                            next_edge,
                            from,
                            PortRef { node: new_id, port: 0 },
                            PortKind::Audio,
                        );
                        next_edge += 1;
                    }
                    if !master_ports.contains(&mport) {
                        master_ports.push(mport);
                    }
                }
                if master_ports.is_empty() {
                    master_ports.push(0);
                }
                for mport in master_ports {
                    target_graph.connect_with_id(
                        next_edge,
                        PortRef { node: new_id, port: 0 },
                        PortRef { node: m, port: mport },
                        PortKind::Audio,
                    );
                    next_edge += 1;
                }
            }
            node_map.push((fx.id, new_id));
            newly_added_fx.push(new_id);
        }
    }

    (node_map, unused_existing_fx, newly_added_fx)
}

impl Setlist {
    /// Which songs (0-based indices) use `rig` under the show's levels.
    pub fn songs_using_rig(&self, rig: u8) -> Vec<usize> {
        match self.rig_level(rig) {
            Level::Show => (0..self.songs.len()).collect(),
            Level::Set(id) => self
                .songs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.set == Some(id))
                .map(|(i, _)| i)
                .collect(),
            Level::Song => self
                .songs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.parts.iter().any(|p| p.rigs.iter().any(|r| r.rig == rig)))
                .map(|(i, _)| i)
                .collect(),
        }
    }

    /// Every rig index that `song` needs loaded (#207): show-level rigs +
    /// set-level rigs of `song.set` + song-level rigs named by any part of `song`.
    pub fn rigs_needed_by_song(&self, song: usize, graph: &Graph) -> Vec<u8> {
        let Some(s) = self.songs.get(song) else { return Vec::new() };
        let rig_count = graph
            .nodes
            .iter()
            .map(|n| n.rig as usize + 1)
            .max()
            .unwrap_or(0)
            .min(MAX_RIGS);
        (0..rig_count as u8)
            .filter(|&r| match self.rig_level(r) {
                Level::Show => true,
                Level::Set(id) => s.set == Some(id),
                Level::Song => s.parts.iter().any(|p| p.rigs.iter().any(|pr| pr.rig == r)),
            })
            .collect()
    }

    /// Hosted instrument node ids that `song` needs loaded (#207).
    pub fn instances_needed_by_song(&self, song: usize, graph: &Graph) -> Vec<NodeId> {
        let rigs = self.rigs_needed_by_song(song, graph);
        let mut nodes: Vec<NodeId> = graph
            .nodes
            .iter()
            .filter(|n| rigs.contains(&n.rig) && is_instrument_node(graph, n))
            .map(|n| n.id)
            .collect();
        nodes.sort_unstable();
        nodes
    }

    /// Exact accounting of every hosted instrument instance in the show (#275).
    pub fn show_instances(&self, graph: &Graph) -> ShowInstances {
        let rig_count = graph
            .nodes
            .iter()
            .map(|n| n.rig as usize + 1)
            .max()
            .unwrap_or(0)
            .min(MAX_RIGS);
        let mut per_rig: Vec<(u8, Vec<NodeId>, Vec<String>)> = Vec::new();
        for r in 0..rig_count as u8 {
            let mut nodes: Vec<&Node> = graph
                .nodes
                .iter()
                .filter(|n| n.rig == r && is_instrument_node(graph, n))
                .collect();
            nodes.sort_by_key(|n| n.id);
            if nodes.is_empty() {
                continue;
            }
            let ids: Vec<NodeId> = nodes.iter().map(|n| n.id).collect();
            let plugins: Vec<String> = nodes.iter().filter_map(|n| plugin_key(&n.kind)).collect();
            per_rig.push((r, ids, plugins));
        }

        let mut hosted = 0usize;
        let mut shared = 0usize;
        let mut separate_presets = 0usize;
        let mut rigs = Vec::with_capacity(per_rig.len());

        for (i, (r, nodes, plugins)) in per_rig.iter().enumerate() {
            let level = self.rig_level(*r);
            let songs = self.songs_using_rig(*r);
            let rig_nodes: Vec<NodeId> = graph
                .nodes
                .iter()
                .filter(|n| n.rig == *r)
                .map(|n| n.id)
                .collect();
            let has_part_knobs = self.songs.iter().flat_map(|s| &s.parts).any(|p| {
                p.params.iter().any(|v| rig_nodes.contains(&v.node))
                    || p.plugin_params.iter().any(|v| rig_nodes.contains(&v.node))
            });
            let has_part_fx = self
                .songs
                .iter()
                .flat_map(|s| &s.parts)
                .any(|p| p.fx.iter().any(|f| rig_nodes.contains(&f.node)));
            let other_has_same_plugin = per_rig
                .iter()
                .enumerate()
                .any(|(j, (_, _, other_p))| j != i && other_p.iter().any(|k| plugins.contains(k)));
            let is_shared = songs.len() > 1 || (!self.songs.is_empty() && !matches!(level, Level::Song));
            let sharing = if is_shared {
                SharingKind::Shared
            } else if other_has_same_plugin {
                SharingKind::SeparatePreset
            } else {
                SharingKind::Single
            };
            let sound_change = match sharing {
                SharingKind::Shared => SoundChange::KnobsAtSwitch,
                SharingKind::SeparatePreset | SharingKind::Single => {
                    if has_part_knobs || has_part_fx {
                        SoundChange::KnobsAtSwitch
                    } else {
                        SoundChange::OwnInstance
                    }
                }
            };
            let count = nodes.len();
            hosted += count;
            match sharing {
                SharingKind::Shared => shared += count,
                SharingKind::SeparatePreset => separate_presets += count,
                SharingKind::Single => {}
            }
            rigs.push(RigInstances {
                rig: *r,
                nodes: nodes.clone(),
                plugins: plugins.clone(),
                level,
                songs,
                sharing,
                sound_change,
                has_part_knobs,
                has_part_fx,
            });
        }

        ShowInstances { hosted, shared, separate_presets, rigs }
    }

    /// Move a Show- or Set-level rig to Song level while keeping it in every
    /// song where it already played, so individual songs can give that one
    /// hosted instance their own level, pan, split and effect chain (#275).
    pub fn share_rig_across_songs(&mut self, rig: u8, default_level: f32, default_pan: Option<f32>) {
        let level = self.rig_level(rig);
        let default_level = if default_level.is_finite() { default_level.clamp(0.0, 1.0) } else { 1.0 };
        let default_pan = default_pan.filter(|p| p.is_finite()).map(|p| p.clamp(-1.0, 1.0));
        for song in &mut self.songs {
            let played = match level {
                Level::Show => true,
                Level::Set(id) => song.set == Some(id),
                Level::Song => false,
            };
            if !played {
                continue;
            }
            for part in &mut song.parts {
                if !part.rigs.iter().any(|r| r.rig == rig) {
                    part.set_rig(PartRig {
                        rig,
                        level: default_level,
                        pan: default_pan,
                        ..PartRig::default()
                    });
                }
            }
        }
        self.pin_rig(rig, Level::Song);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, ClapId, SURGE_PLUGIN_ID};
    use crate::setlist::{Part, PartRig, SetlistPos, Song};

    fn piano_rig(fx: &[DspKind]) -> Graph {
        let mut g = Graph::default();
        let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let p = g.add_node(NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) }, name_bytes("PIANO"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut eid = 1u32;
        g.connect_with_id(eid, PortRef { node: k, port: 0 }, PortRef { node: p, port: 0 }, PortKind::Midi);
        eid += 1;
        let mut prev = p;
        for kind in fx {
            let f = g.add_node(NodeKind::Dsp(kind.clone()), name_bytes(&format!("{kind:?}")));
            g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: f, port: 0 }, PortKind::Audio);
            eid += 1;
            prev = f;
        }
        g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
        g.connect_with_id(eid + 1, PortRef { node: prev, port: 0 }, PortRef { node: m, port: 1 }, PortKind::Audio);
        g
    }

    #[test]
    fn twenty_songs_sharing_one_piano_host_one_instance_with_per_song_view() {
        let g = piano_rig(&[DspKind::Delay, DspKind::Reverb]);
        let delay = g.nodes.iter().find(|n| n.kind == NodeKind::Dsp(DspKind::Delay)).unwrap().id;
        let reverb = g.nodes.iter().find(|n| n.kind == NodeKind::Dsp(DspKind::Reverb)).unwrap().id;
        let piano = g.nodes.iter().find(|n| matches!(n.kind, NodeKind::Clap { .. })).unwrap().id;

        let mut sl = Setlist::default();
        for i in 0..20usize {
            let pan = -0.5 + (i as f32 / 19.0);
            let mut pr = PartRig {
                rig: 0,
                key_lo: if i % 2 == 0 { 0 } else { 48 },
                key_hi: 127,
                level: 0.6 + 0.01 * i as f32,
                pan: Some(pan),
                ..PartRig::default()
            };
            pr.transpose = if i % 4 == 0 { 0 } else { -12 };
            let mut part = Part::new("Main", vec![pr]);
            part.set_fx(delay, i % 2 == 0);
            part.set_fx(reverb, i % 2 != 0);
            part.set_plugin_param(piano, 42, i as f64 / 20.0);
            sl.songs.push(Song::new(format!("Song {}", i + 1), vec![part]));
        }
        sl.normalize();
        sl.validate(&g).unwrap();

        let summary = sl.show_instances(&g);
        assert_eq!(summary.hosted, 1, "20 songs sharing one piano host 1 instance");
        assert_eq!(summary.shared, 1);
        assert_eq!(summary.separate_presets, 0);
        assert_eq!(summary.rigs[0].songs.len(), 20);
        assert_eq!(summary.rigs[0].sharing, SharingKind::Shared);
        assert_eq!(summary.rigs[0].sound_change, SoundChange::KnobsAtSwitch);
        assert!(summary.rigs[0].has_part_knobs && summary.rigs[0].has_part_fx);
        assert_eq!(summary.summary_label(20), "1 instance (1 shared) · 20 songs");
        for i in 0..20 {
            assert_eq!(sl.rigs_needed_by_song(i, &g), vec![0]);
            assert_eq!(sl.instances_needed_by_song(i, &g), vec![piano]);
        }

        sl.active = Some(SetlistPos { song: 1, part: 0 });
        assert_eq!(sl.recalled_fx(&g), vec![FxBypass { node: delay, bypass: false }, FxBypass { node: reverb, bypass: true }]);
        assert_eq!(sl.recalled_plugin_params(&g).len(), 1);
        let back: Setlist = serde_json::from_str(&serde_json::to_string(&sl).unwrap()).unwrap();
        assert_eq!(back, sl);
    }

    #[test]
    fn matching_shares_identical_preset_and_separates_different_preset() {
        let mut g = piano_rig(&[DspKind::Reverb]);
        let piano = g.nodes.iter().find(|n| matches!(n.kind, NodeKind::Clap { .. })).unwrap().id;
        let grand = vec![PluginState { node: piano, plugin_id: SURGE_PLUGIN_ID.into(), data: b"grand-piano-v1".to_vec() }];
        let felt = vec![PluginState { node: piano, plugin_id: SURGE_PLUGIN_ID.into(), data: b"felt-piano-v1".to_vec() }];
        let mut sl = Setlist::default();
        sl.songs.push(Song::new("Ballad", vec![Part::new("Main", vec![PartRig::new(0)])]));

        let imp_same = piano_rig(&[DspKind::Filter, DspKind::Delay]);
        assert_eq!(find_matching_rig(&g, &grand, &sl, &imp_same, 0, &grand), MatchResult::Shared(0));
        assert_eq!(find_matching_rig(&g, &grand, &sl, &imp_same, 0, &felt), MatchResult::DifferentPreset(0));

        // Merging Funk's Filter+Delay onto Rig 0 keeps 1 piano instance and wires the new effects.
        let (map, unused, added) = merge_rig_effects_into(&mut g, 0, &imp_same, 0);
        assert_eq!(g.nodes.iter().filter(|n| is_instrument_node(&g, n)).count(), 1);
        assert_eq!(unused.len(), 1, "existing Reverb is unused by Funk");
        assert_eq!(added.len(), 2, "Filter and Delay spliced in");
        assert_eq!(map.len(), 3, "piano + Filter + Delay mapped");
        let master = g.nodes.iter().find(|n| matches!(n.kind, NodeKind::MasterOut)).unwrap().id;
        assert!(g.has_route(added[1], master, PortKind::Audio), "last added effect feeds MasterOut");
        assert!(g.has_route(unused[0], added[0], PortKind::Audio), "existing Reverb feeds first added effect");
    }

    #[test]
    fn share_rig_across_songs_keeps_a_show_level_instrument_in_every_song() {
        let g = piano_rig(&[]);
        let mut sl = Setlist {
            songs: vec![
                Song::new("One", vec![Part::new("Main", vec![])]),
                Song::new("Two", vec![Part::new("Verse", vec![]), Part::new("Chorus", vec![])]),
            ],
            ..Setlist::default()
        };
        sl.pin_rig(0, Level::Show);
        assert_eq!(sl.rig_level(0), Level::Show);
        sl.share_rig_across_songs(0, 0.8, Some(-0.2));
        assert_eq!(sl.rig_level(0), Level::Song);
        assert_eq!(sl.songs_using_rig(0), vec![0, 1]);
        assert_eq!(sl.songs[0].parts[0].rigs[0].pan, Some(-0.2));
        assert_eq!(sl.songs[1].parts[1].rigs[0].level, 0.8);
        assert_eq!(sl.show_instances(&g).hosted, 1);
    }
}
