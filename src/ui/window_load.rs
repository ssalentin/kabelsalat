//! Per-setlist windowed plugin loading in the app (#207).
//!
//! Keeps only the current song and the next `load_ahead` playable songs (plus
//! show-level rigs, set-level rigs of songs in the window, aux-bus chains, and
//! master inserts) hosted in memory. Outgoing rigs stay hosted until their held
//! notes, sustain pedal, and pre-fader audio tails finish ringing out (tracked
//! by the engine's `rig_busy_mask` / `rig_held_mask`), then retire on the
//! background loader threads (`kabelsalat-clap-loader`, `kabelsalat-lv2-loader`)
//! so neither the audio thread nor the UI thread ever blocks (rule 46).

use std::collections::HashMap;

use super::*;
use crate::control::NodeId;
use crate::host::{ClapLoader, ClapLoaderResp, HostedClapMain};
use crate::setlist::{is_instrument_node, is_master_insert, is_plugin_node, SongLoadState, MAX_LOAD_AHEAD};

/// Audio output threshold fallback when no engine is attached.
const SILENCE_FLOOR: f32 = 1e-4;
/// How long an outgoing rig must stay completely silent and unheld before
/// retiring (long enough to bridge delay echoes and slow attacks).
const QUIET_RETIRE_SECS: f32 = 0.5;
/// Upper bound on how long a noisy (unheld) outgoing rig may delay retirement.
const MAX_DRAIN_SECS: f32 = 15.0;

#[derive(Clone, Copy, Debug)]
struct DrainState {
    since: f32,
    quiet_since: Option<f32>,
}

#[derive(Default)]
pub(super) struct SetlistWindowState {
    pub(super) loader: Option<ClapLoader>,
    pub(super) gen: u64,
    /// Cached CLAP plugin states (`node_id -> (plugin_id, blob)`) so unloaded
    /// songs keep their presets across unload/reload and session saves.
    pub(super) clap_states: HashMap<NodeId, (String, Vec<u8>)>,
    /// CLAP nodes currently queued or instantiating on `kabelsalat-clap-loader`.
    pub(super) clap_loading: Vec<NodeId>,
    /// CLAP nodes whose retiring background voice is saving its latest state on
    /// `kabelsalat-clap-loader`; re-queuing waits for `Retired` so edits never race.
    pub(super) clap_retiring_save: Vec<NodeId>,
    /// `ClapLoaderResp::Loaded` replies waiting for `voice_hold_rev` to clear
    /// after `LoadGraph`/`LoadRigs` so `AddBound` never races a graph swap.
    pub(super) pending_loaded: Vec<ClapLoaderResp>,
    /// Last known `voice_id -> node_id` mapping so retiring a voice knows which
    /// node's cached state to update.
    pub(super) clap_loaded_nodes: HashMap<u64, NodeId>,
    /// Attached LV2 nodes published by the engine (`GraphPub::lv2_loaded`).
    pub(super) lv2_loaded: Vec<NodeId>,
    /// In-flight LV2 nodes on `kabelsalat-lv2-loader` (`GraphPub::lv2_loading`).
    pub(super) lv2_loading: Vec<NodeId>,
    pub(super) last_lv2_rev: u64,
    /// Engine-published per-rig activity masks (pre-fader audio + held notes + sustain).
    pub(super) rig_busy_mask: u64,
    pub(super) rig_held_mask: u64,
    /// Last rig bitmask sent via `Command::SetHostedRigs` (`None` = all hosted).
    pub(super) hosted_rigs_sent: Option<u64>,
    /// Rigs that just left the window and are still ringing out held notes / tails.
    draining_rigs: HashMap<u8, DrainState>,
    /// Test hook: mark nodes as loading in UI-only tests without an engine.
    #[cfg(test)]
    pub(super) simulated_loading: Vec<NodeId>,
}

impl SetlistWindowState {
    pub(super) fn is_active_work(&self) -> bool {
        !self.clap_loading.is_empty()
            || !self.clap_retiring_save.is_empty()
            || !self.pending_loaded.is_empty()
            || !self.lv2_loading.is_empty()
            || !self.draining_rigs.is_empty()
    }
}

impl DemoApp {
    /// Reset window tracking and pre-populate cached CLAP plugin states before
    /// `apply_session` loads a new graph. If the session enables `load_ahead`,
    /// primes `Command::SetHostedRigs { for_next_load: true }` so `LoadGraph`
    /// only queues in-window LV2 plugins.
    pub(super) fn prepare_setlist_window_for_session(&mut self, s: &crate::session::Session) {
        self.setlist_window.gen = self.setlist_window.gen.wrapping_add(1);
        self.setlist_window.clap_loading.clear();
        self.setlist_window.clap_retiring_save.clear();
        self.setlist_window.clap_loaded_nodes.clear();
        self.setlist_window.draining_rigs.clear();
        self.setlist_window.clap_states.clear();
        // Retire any deferred loaded voices from the previous session cleanly.
        let stale: Vec<ClapLoaderResp> = self.setlist_window.pending_loaded.drain(..).collect();
        if let Some(loader) = &self.setlist_window.loader {
            for r in stale {
                if let ClapLoaderResp::Loaded { voice_id, node, plugin_id, slot, .. } = r {
                    loader.retire(voice_id, node, plugin_id, slot, false);
                }
            }
        }
        for st in s.plugin_states() {
            if s.graph.node(st.node).is_some_and(|n| n.kind.is_hosted_voice()) {
                self.setlist_window
                    .clap_states
                    .insert(st.node, (st.plugin_id.clone(), st.data.clone()));
            }
        }
        let mask = self.setlist.window_rigs_mask(&s.graph);
        self.setlist_window.hosted_rigs_sent = mask;
        if let Some(loader) = &self.setlist_window.loader {
            let wanted = self.setlist.window_plugin_nodes(&s.graph);
            loader.set_wanted(self.setlist_window.gen, wanted);
        }
        if mask.is_some() {
            if let Some(bus) = &self.bus {
                let _ = bus
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .send(Command::SetHostedRigs { mask, for_next_load: true });
            }
        }
    }

    /// True if `rig` currently has pre-fader audio, held notes, or sustain down.
    fn rig_is_busy(&self, rig: u8) -> bool {
        if self.voice_tx.is_some() {
            (rig as usize) < Self::MAX_RIGS
                && (self.setlist_window.rig_busy_mask & (1u64 << rig)) != 0
        } else {
            let level = self.rig_levels.get(rig as usize).copied().unwrap_or(0.0);
            level > SILENCE_FLOOR
        }
    }

    /// True if `rig` currently has a held key or sustain pedal down on the engine.
    fn rig_is_held(&self, rig: u8) -> bool {
        (rig as usize) < Self::MAX_RIGS
            && (self.setlist_window.rig_held_mask & (1u64 << rig)) != 0
    }

    /// Reconcile hosted rigs, draining outgoing rigs, LV2 `SetHostedRigs`, and
    /// background CLAP voice loading against the current `setlist` window.
    pub(super) fn sync_setlist_window(&mut self) {
        let graph = self.base_graph.clone();
        if !self.setlist.window_enabled() {
            self.setlist_window.draining_rigs.clear();
            let prev_mask = self.setlist_window.hosted_rigs_sent.take();
            if prev_mask.is_some() {
                if let Some(bus) = &self.bus {
                    let _ = bus.lock().unwrap_or_else(|e| e.into_inner()).send(
                        Command::SetHostedRigs { mask: None, for_next_load: false },
                    );
                }
                // Restore any LV2 states that were parked while unloaded (skip
                // always-hosted bus/master inserts which never unloaded).
                let prev = prev_mask.unwrap_or(0);
                let to_restore: Vec<(NodeId, Vec<u8>)> = self
                    .lv2_states
                    .iter()
                    .filter(|(&node, _)| {
                        !graph.after_bus(node)
                            && !is_master_insert(&graph, node)
                            && graph.node(node).is_some_and(|n| {
                                (n.rig as usize) < Self::MAX_RIGS && (prev & (1u64 << n.rig)) == 0
                            })
                    })
                    .map(|(&n, st)| (n, st.clone()))
                    .collect();
                for (node, state) in to_restore {
                    let _ = self.restore_lv2_state(node, state);
                }
            }
            if let Some(loader) = &self.setlist_window.loader {
                loader.set_wanted(self.setlist_window.gen, None);
            }
            // If windowing was just turned off while some CLAP nodes are unloaded,
            // load the rest in the background without blocking the UI thread.
            if self.voice_tx.is_some() {
                self.queue_missing_clap_nodes(&graph, None);
            }
            return;
        }

        let desired_rigs = self.setlist.window_rigs(&graph).unwrap_or_default();
        let now = self.ui_time;

        // Rigs that re-entered the window stop draining immediately.
        self.setlist_window
            .draining_rigs
            .retain(|r, _| !desired_rigs.contains(r));

        // Any previously hosted rig that leaves the window always enters
        // `draining_rigs` first so notes struck on the switch frame, slow-attack
        // pads, sustain pedals, and quiet tails never get cut off prematurely.
        let mut prev_hosted_rigs: Vec<u8> = Vec::new();
        if let Some(prev_mask) = self.setlist_window.hosted_rigs_sent {
            for r in 0..Self::MAX_RIGS as u8 {
                if (prev_mask & (1u64 << r)) != 0 {
                    prev_hosted_rigs.push(r);
                }
            }
        } else {
            for r in 0..self.rig_count().min(Self::MAX_RIGS) as u8 {
                prev_hosted_rigs.push(r);
            }
        }
        for &node in self.setlist_window.clap_loaded_nodes.values() {
            if let Some(n) = graph.node(node) {
                if !prev_hosted_rigs.contains(&n.rig) {
                    prev_hosted_rigs.push(n.rig);
                }
            }
        }

        let has_engine = self.voice_tx.is_some();
        for r in prev_hosted_rigs {
            if desired_rigs.contains(&r) {
                continue;
            }
            if !self.setlist_window.draining_rigs.contains_key(&r) {
                if has_engine || self.rig_is_busy(r) {
                    self.setlist_window
                        .draining_rigs
                        .insert(r, DrainState { since: now, quiet_since: None });
                }
            }
        }

        // Advance drain timers: retire once silent & unheld for QUIET_RETIRE_SECS,
        // or past MAX_DRAIN_SECS when no key/sustain pedal is held.
        let mut retired_drains = Vec::new();
        let drain_keys: Vec<u8> = self.setlist_window.draining_rigs.keys().copied().collect();
        for r in drain_keys {
            let busy = self.rig_is_busy(r);
            let held = self.rig_is_held(r);
            if let Some(st) = self.setlist_window.draining_rigs.get_mut(&r) {
                if busy {
                    st.quiet_since = None;
                } else {
                    let q = *st.quiet_since.get_or_insert(now);
                    if now - q >= QUIET_RETIRE_SECS {
                        retired_drains.push(r);
                        continue;
                    }
                }
                if !held && now - st.since >= MAX_DRAIN_SECS {
                    retired_drains.push(r);
                }
            }
        }
        for r in retired_drains {
            self.setlist_window.draining_rigs.remove(&r);
        }

        let mut eff_mask = 0u64;
        for &r in &desired_rigs {
            if (r as usize) < Self::MAX_RIGS {
                eff_mask |= 1u64 << r;
            }
        }
        for &r in self.setlist_window.draining_rigs.keys() {
            if (r as usize) < Self::MAX_RIGS {
                eff_mask |= 1u64 << r;
            }
        }

        let prev_mask = self.setlist_window.hosted_rigs_sent;
        if prev_mask != Some(eff_mask) {
            self.setlist_window.hosted_rigs_sent = Some(eff_mask);
            if let Some(bus) = &self.bus {
                let _ = bus.lock().unwrap_or_else(|e| e.into_inner()).send(
                    Command::SetHostedRigs { mask: Some(eff_mask), for_next_load: false },
                );
            }
            // Restore saved LV2 states for rigs that just entered the window
            // (sent right after SetHostedRigs in the same frame so GraphEngine
            // claims lv2_in_flight with Lv2Request::State before sync_hosted_lv2
            // runs, avoiding double-instantiation of Vitalium). Skip always-hosted
            // aux-bus / master-insert LV2s which never unloaded.
            let old = prev_mask.unwrap_or(u64::MAX);
            let newly_added = eff_mask & !old;
            if newly_added != 0 {
                let to_restore: Vec<(NodeId, Vec<u8>)> = self
                    .lv2_states
                    .iter()
                    .filter(|(&node, _)| {
                        !graph.after_bus(node)
                            && !is_master_insert(&graph, node)
                            && graph.node(node).is_some_and(|n| {
                                (n.rig as usize) < Self::MAX_RIGS
                                    && (newly_added & (1u64 << n.rig)) != 0
                            })
                    })
                    .map(|(&n, st)| (n, st.clone()))
                    .collect();
                for (node, state) in to_restore {
                    let _ = self.restore_lv2_state(node, state);
                }
            }
        }

        // Priority-ordered CLAP/VST3 nodes: window songs first (current song first),
        // then any draining rigs so we don't retire them mid-tail.
        let mut wanted_clap: Vec<NodeId> = self
            .setlist
            .window_plugin_nodes(&graph)
            .unwrap_or_default()
            .into_iter()
            .filter(|&id| graph.node(id).is_some_and(|n| n.kind.is_hosted_voice()))
            .collect();
        for n in &graph.nodes {
            if n.kind.is_hosted_voice()
                && self.setlist_window.draining_rigs.contains_key(&n.rig)
                && !wanted_clap.contains(&n.id)
            {
                wanted_clap.push(n.id);
            }
        }

        if self.voice_tx.is_some() {
            self.reconcile_windowed_clap(&graph, &wanted_clap);
        }
    }

    /// True if `node` currently has a retiring voice waiting to save its state
    /// back to `clap_states` (so queuing a reload must wait until `Retired`).
    fn clap_node_retiring(&self, node: NodeId) -> bool {
        if self.setlist_window.clap_retiring_save.contains(&node) {
            return true;
        }
        self.clap_leaving
            .iter()
            .any(|vid| self.setlist_window.clap_loaded_nodes.get(vid) == Some(&node))
    }

    /// Background-load any CLAP node in `wanted` (or all CLAP nodes when `None`)
    /// that is neither hosted, retiring, nor already in flight.
    fn queue_missing_clap_nodes(&mut self, graph: &Graph, wanted: Option<&[NodeId]>) {
        let targets: Vec<NodeId> = match wanted {
            Some(list) => list.to_vec(),
            None => graph
                .nodes
                .iter()
                .filter(|n| n.kind.is_hosted_voice())
                .map(|n| n.id)
                .collect(),
        };
        let mut to_load: Vec<(NodeId, String, Option<Vec<u8>>)> = Vec::new();
        for node in targets {
            let already_hosted = self.clap_voice_ids.iter().any(|vid| {
                !self.clap_leaving.contains(vid)
                    && self.setlist_window.clap_loaded_nodes.get(vid) == Some(&node)
            });
            let pending_attach = self.setlist_window.pending_loaded.iter().any(|r| {
                matches!(r, ClapLoaderResp::Loaded { node: n, gen, .. } if *n == node && *gen == self.setlist_window.gen)
            });
            if already_hosted
                || pending_attach
                || self.setlist_window.clap_loading.contains(&node)
                || self.clap_node_retiring(node)
            {
                continue;
            }
            let Some(n) = graph.node(node) else {
                continue;
            };
            let Some(key) = n.kind.voice_plugin_key() else {
                continue;
            };
            let state = self
                .setlist_window
                .clap_states
                .get(&node)
                .filter(|(pid, _)| {
                    if matches!(n.kind, NodeKind::Vst3 { .. }) {
                        let norm = crate::vst3::voice_key(
                            &crate::vst3::strip_vst3_key(pid).unwrap_or(pid).to_ascii_uppercase(),
                        );
                        norm == key
                    } else {
                        crate::control::clap_plugin_key(pid) == key
                    }
                })
                .map(|(_, data)| data.clone());
            to_load.push((node, key, state));
        }
        if to_load.is_empty() {
            return;
        }
        let gen = self.setlist_window.gen;
        let sr = self.sample_rate;
        let loader = self.setlist_window.loader.get_or_insert_with(ClapLoader::spawn);
        for (node, pid, state) in to_load {
            self.setlist_window.clap_loading.push(node);
            loader.request_load(node, pid, sr, state, gen);
        }
    }

    fn reconcile_windowed_clap(&mut self, graph: &Graph, wanted: &[NodeId]) {
        let _ = self.free_returned_voices();
        if let Some(loader) = &self.setlist_window.loader {
            loader.set_wanted(self.setlist_window.gen, Some(wanted.to_vec()));
        }
        // 1. Queue missing CLAP nodes in priority order (current song first).
        self.queue_missing_clap_nodes(graph, Some(wanted));

        // 2. Retire any currently hosted CLAP voice whose node is outside `wanted`.
        let count = self.clap_voice_ids.len();
        for i in 0..count {
            let vid = self.clap_voice_ids[i];
            if self.clap_leaving.contains(&vid) {
                continue;
            }
            let node = self.setlist_window.clap_loaded_nodes.get(&vid).copied();
            if let Some(nid) = node {
                if wanted.contains(&nid) {
                    continue;
                }
                // Preserve local voice state before asking the engine to hand it back.
                if let Some(m) = self._surge_main.get_mut(i) {
                    if !m.is_bg() {
                        if let Ok(blob) = m.save_state() {
                            let pid = self
                                .clap_plugin_ids
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| SURGE_ID.to_string());
                            self.setlist_window.clap_states.insert(nid, (pid, blob));
                        }
                    }
                }
            }
            if let Some(tx) = self.voice_tx.as_mut() {
                if tx.push(VoiceMsg::Retire(vid)).is_ok() {
                    self.clap_leaving.push(vid);
                }
            }
        }
        self.rebuild_clap_slot_map();
    }

    pub(super) fn rebuild_clap_slot_map(&mut self) {
        self.clap_slot_map = self
            .clap_rigs()
            .into_iter()
            .map(|(_, node)| {
                self.clap_voice_ids.iter().position(|vid| {
                    !self.clap_leaving.contains(vid)
                        && self.setlist_window.clap_loaded_nodes.get(vid) == Some(&node)
                })
            })
            .collect();
        self.rule_param_cache = None;
    }

    /// Update `clap_slot_map` and sync windowed voices from an engine voice
    /// publication when `setlist.window_enabled()` is active.
    pub(super) fn sync_clap_slots_windowed(
        &mut self,
        graph: &Graph,
        published: Option<&[(u64, u32)]>,
    ) {
        let _ = self.free_returned_voices();
        if let Some(pub_voices) = published {
            for &(vid, node) in pub_voices {
                if node != u32::MAX && graph.node(node).is_some() {
                    self.setlist_window.clap_loaded_nodes.insert(vid, node);
                }
            }
        }
        self.sync_setlist_window();
    }

    /// Drain replies from `kabelsalat-clap-loader` and advance outgoing-rig
    /// drain timers. Called once per UI frame (non-blocking).
    pub(super) fn poll_setlist_window(&mut self) {
        let mut needs_resync = self.free_returned_voices();
        let mut resps = Vec::new();
        if self.voice_hold_rev.is_none() && !self.setlist_window.pending_loaded.is_empty() {
            resps.append(&mut self.setlist_window.pending_loaded);
        }
        if let Some(loader) = &self.setlist_window.loader {
            while let Some(r) = loader.try_recv() {
                resps.push(r);
            }
        }
        for resp in resps {
            match resp {
                ClapLoaderResp::Loaded {
                    node,
                    gen,
                    voice_id,
                    plugin_id,
                    slot,
                    ctrl,
                    main_controls,
                    has_editor,
                } => {
                    self.setlist_window.clap_loading.retain(|&n| n != node);
                    if gen != self.setlist_window.gen {
                        if let Some(loader) = &self.setlist_window.loader {
                            loader.retire(voice_id, node, plugin_id, slot, false);
                        }
                        continue;
                    }
                    // Wait for any in-flight LoadGraph/LoadRigs to publish before
                    // pushing VoiceMsg::AddBound so the engine never rejects or
                    // misbinds it against the previous graph (#207).
                    if self.voice_hold_rev.is_some() {
                        self.setlist_window.pending_loaded.push(ClapLoaderResp::Loaded {
                            node,
                            gen,
                            voice_id,
                            plugin_id,
                            slot,
                            ctrl,
                            main_controls,
                            has_editor,
                        });
                        continue;
                    }
                    let wanted_now = self
                        .base_graph
                        .node(node)
                        .is_some_and(|n| n.kind.is_hosted_voice())
                        && self
                            .setlist
                            .window_plugin_nodes(&self.base_graph)
                            .map_or(true, |w| {
                                w.contains(&node)
                                    || self.base_graph.node(node).is_some_and(|n| {
                                        self.setlist_window.draining_rigs.contains_key(&n.rig)
                                    })
                            })
                        && !self.clap_voice_ids.iter().any(|vid| {
                            !self.clap_leaving.contains(vid)
                                && self.setlist_window.clap_loaded_nodes.get(vid) == Some(&node)
                        });
                    if !wanted_now {
                        if let Some(loader) = &self.setlist_window.loader {
                            loader.retire(voice_id, node, plugin_id, slot, false);
                        }
                        continue;
                    }
                    let grow = self.clap_voice_ids.len() >= self.engine_voice_cap;
                    let mut rejected_slot = None;
                    let pushed = if let Some(tx) = self.voice_tx.as_mut() {
                        if tx.slots() < 1 + grow as usize {
                            rejected_slot = Some(slot);
                            false
                        } else {
                            if grow {
                                let cap = self.engine_voice_cap * 2;
                                if let Some(Ok(mut g)) = self.graph_pub.as_ref().map(|p| p.lock()) {
                                    g.reserve_voices(cap);
                                }
                                let _ = tx.push(VoiceMsg::Grow(Vec::with_capacity(cap)));
                                self.engine_voice_cap = cap;
                            }
                            match tx.push(VoiceMsg::AddBound { node, slot }) {
                                Ok(()) => true,
                                Err(rtrb::PushError::Full(VoiceMsg::AddBound { slot, .. })) => {
                                    rejected_slot = Some(slot);
                                    false
                                }
                                Err(_) => false,
                            }
                        }
                    } else {
                        rejected_slot = Some(slot);
                        false
                    };
                    if !pushed {
                        if let (Some(slot), Some(loader)) = (rejected_slot, &self.setlist_window.loader) {
                            loader.retire(voice_id, node, plugin_id.clone(), slot, false);
                        }
                        self.plugin_msg = format!("Voice channel busy, {plugin_id} will retry");
                        needs_resync = true;
                        continue;
                    }
                    let proxy = self
                        .setlist_window
                        .loader
                        .as_ref()
                        .expect("loader exists")
                        .proxy(voice_id, main_controls, has_editor);
                    self._surge_main.push(HostedClapMain::Bg(proxy));
                    self.surge_controllers.push(ctrl);
                    self.clap_plugin_ids.push(plugin_id);
                    self.clap_voice_ids.push(voice_id);
                    self.setlist_window.clap_loaded_nodes.insert(voice_id, node);
                    self.rebuild_clap_slot_map();
                    self.reapply_attached_clap_node(node);
                }
                ClapLoaderResp::LoadFailed { node, error, .. } => {
                    self.setlist_window.clap_loading.retain(|&n| n != node);
                    self.plugin_msg = error;
                }
                ClapLoaderResp::Cancelled { node, .. } => {
                    self.setlist_window.clap_loading.retain(|&n| n != node);
                    // Re-sync in case this node re-entered the window while the
                    // cancel reply was in flight.
                    needs_resync = true;
                }
                ClapLoaderResp::Retired { node, plugin_id, saved_state, .. } => {
                    self.setlist_window.clap_retiring_save.retain(|&n| n != node);
                    if let Some(blob) = saved_state {
                        if node != NodeId::MAX {
                            self.setlist_window.clap_states.insert(node, (plugin_id, blob));
                        }
                    }
                    // If the song re-entered the window while retiring, queue its
                    // reload now that its latest state is safely cached.
                    needs_resync = true;
                }
            }
        }
        if needs_resync || !self.setlist_window.draining_rigs.is_empty() {
            self.sync_setlist_window();
        }
    }

    /// Re-apply per-node engine state (LEVEL, MIDI sources/device, and active
    /// setlist part params/plugin_params) when a background-loaded CLAP voice
    /// attaches to `node` (rule 104).
    fn reapply_attached_clap_node(&mut self, node: NodeId) {
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(u) = self.units.iter().find(|u| u.node_id == node) {
                for k in &u.knobs {
                    if let Some(p) = k.param {
                        let _ = b.send(Command::SetParam { node, param: p, value: k.value as f64 });
                    }
                }
            }
            if let Some(slot) = self.voice_slot_of_node(node) {
                if let Some(&(kb, hw)) = self.rig_sources.get(slot) {
                    let _ = b.send(Command::SetRigSource { node, keyboard: kb, hardware: hw });
                }
                if let Some(&dev) = self.rig_device.get(slot) {
                    let _ = b.send(Command::SetRigDevice { node, device: dev });
                }
            }
        }
        let graph = self.mirror_graph();
        let pparams = self.setlist.recalled_plugin_params(&graph);
        if pparams.iter().any(|p| p.node == node) {
            self.recall_part_fx_and_plugin_params(&graph);
        }
        self.clear_loading_msg_if_ready();
    }

    /// Re-apply per-node knob/drawbar/MIDI-FX params and active part recall when
    /// a windowed LV2 node finishes attaching in the engine (#207).
    pub(super) fn reapply_attached_lv2_node(&mut self, node: NodeId) {
        if let Some(bus) = &self.bus {
            let mut b = bus.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(u) = self.units.iter().find(|u| u.node_id == node) {
                for k in &u.knobs {
                    if let Some(p) = k.param {
                        let _ = b.send(Command::SetParam { node, param: p, value: k.value as f64 });
                    }
                }
            }
            for (&(n, pid), &val) in &self.node_params {
                if n == node {
                    let _ = b.send(Command::SetParam { node, param: pid, value: val as f64 });
                }
            }
        }
        let graph = self.mirror_graph();
        let pparams = self.setlist.recalled_plugin_params(&graph);
        if pparams.iter().any(|p| p.node == node) {
            self.recall_part_fx_and_plugin_params(&graph);
        }
        self.clear_loading_msg_if_ready();
    }

    fn clear_loading_msg_if_ready(&mut self) {
        if let Some((s, p, pos)) = self.setlist.current() {
            if self.song_load_state(pos.song) == SongLoadState::Ready
                && self.session_msg.starts_with("Loading '")
            {
                self.session_msg = format!("{} · {}", s.name, p.name);
            }
        }
    }

    /// Currently loaded and in-flight plugin nodes for readiness display.
    pub(super) fn plugin_load_sets(&self) -> (Vec<NodeId>, Vec<NodeId>) {
        if self.voice_tx.is_some() {
            let mut loaded: Vec<NodeId> = self
                .clap_voice_ids
                .iter()
                .filter(|vid| !self.clap_leaving.contains(vid))
                .filter_map(|vid| self.setlist_window.clap_loaded_nodes.get(vid).copied())
                .collect();
            loaded.extend_from_slice(&self.setlist_window.lv2_loaded);
            loaded.sort_unstable();
            loaded.dedup();
            let mut loading = self.setlist_window.clap_loading.clone();
            for r in &self.setlist_window.pending_loaded {
                if let ClapLoaderResp::Loaded { node, gen, .. } = r {
                    if *gen == self.setlist_window.gen {
                        loading.push(*node);
                    }
                }
            }
            loading.extend_from_slice(&self.setlist_window.lv2_loading);
            loading.sort_unstable();
            loading.dedup();
            (loaded, loading)
        } else if self.setlist.window_enabled() {
            // Headless / UI-only mode without an attached audio engine: treat
            // window songs as ready (minus any test-simulated loading nodes)
            // and out-of-window songs as unloaded.
            #[cfg(test)]
            let sim_loading = &self.setlist_window.simulated_loading;
            #[cfg(not(test))]
            let sim_loading: &[NodeId] = &[];
            let loaded: Vec<NodeId> = self
                .setlist
                .window_plugin_nodes(&self.base_graph)
                .unwrap_or_default()
                .into_iter()
                .filter(|id| !sim_loading.contains(id))
                .collect();
            (loaded, sim_loading.to_vec())
        } else {
            let loaded: Vec<NodeId> = self
                .base_graph
                .nodes
                .iter()
                .filter(|n| is_plugin_node(n))
                .map(|n| n.id)
                .collect();
            (loaded, Vec::new())
        }
    }

    /// Readiness state of `song` (`Ready`, `Loading`, or `Unloaded`).
    pub(super) fn song_load_state(&self, song: usize) -> SongLoadState {
        let (loaded, loading) = self.plugin_load_sets();
        self.setlist
            .song_load_state(song, &self.base_graph, &loaded, &loading)
    }

    /// How many hosted plugin instruments are currently resident in memory.
    pub(super) fn hosted_instrument_count(&self) -> usize {
        let (loaded, _) = self.plugin_load_sets();
        self.base_graph
            .nodes
            .iter()
            .filter(|n| is_instrument_node(&self.base_graph, n) && loaded.contains(&n.id))
            .count()
    }

    /// Non-empty notice while the active song's plugins are still loading.
    pub(super) fn active_song_loading_notice(&self) -> Option<String> {
        let (s, _, pos) = self.setlist.current()?;
        if self.song_load_state(pos.song) != SongLoadState::Ready {
            Some(format!("Loading '{}'…", s.name))
        } else {
            None
        }
    }

    /// Announce when the player jumps to a song whose rigs are not yet hosted.
    pub(super) fn announce_if_unloaded_jump(&mut self, target_song: usize, was_ready: bool) {
        if !was_ready {
            if let Some(song) = self.setlist.songs.get(target_song) {
                self.session_msg = format!("Loading '{}'… (audio keeps running)", song.name);
            }
        }
    }

    /// Execute `set_setlist_window` from the assistant or MCP.
    pub(super) fn exec_set_setlist_window(
        &mut self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let load_ahead = match args.get("load_ahead") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => {
                let n = v
                    .as_u64()
                    .or_else(|| {
                        v.as_f64()
                            .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                            .map(|f| f as u64)
                    })
                    .ok_or_else(|| {
                        format!(
                            "load_ahead must be null or an integer 0..={MAX_LOAD_AHEAD}, got {v}"
                        )
                    })?;
                if n == 0 {
                    None
                } else if n > MAX_LOAD_AHEAD as u64 {
                    return Err(format!(
                        "load_ahead must be 1..={MAX_LOAD_AHEAD} (or null/0 to disable), got {n}"
                    ));
                } else {
                    Some(n as u8)
                }
            }
        };
        self.setlist.load_ahead = load_ahead;
        self.sync_setlist_window();
        Ok(self.setlist_json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ClapId, SURGE_PLUGIN_ID};

    fn multi_rig_app(n_rigs: usize) -> (DemoApp, crate::control::CommandDrain) {
        let mut rigs = Vec::with_capacity(n_rigs);
        for i in 0..n_rigs {
            let mut g = Graph::default();
            let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let s = g.add_node(
                NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) },
                name_bytes(&format!("SYNTH {}", i + 1)),
            );
            let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, CPortKind::Midi);
            g.connect_with_id(2, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
            rigs.push(crate::control::Rig::new(format!("Rig {}", i + 1), g, None));
        }
        let merged = crate::control::merge_rigs(&rigs);
        let mut s = crate::session::Session::new("Long Gig", merged);
        s.rig_names = (0..n_rigs).map(|i| format!("Rig {}", i + 1)).collect();
        let clap_nodes: Vec<(NodeId, u8)> = s
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Clap { .. }))
            .map(|n| (n.id, n.rig))
            .collect();
        for (id, rig) in clap_nodes {
            s.set_plugin_state(id, SURGE_PLUGIN_ID, format!("patch-rig-{rig}").into_bytes());
        }
        let mut app = DemoApp::new_with_sink(None);
        let (tx, drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        app.apply_session(&s).unwrap();
        for i in 0..n_rigs {
            let res = app.exec_tool("set_setlist_song", &serde_json::json!({
                "name": format!("Song {}", i + 1),
                "parts": [{"name": "Main", "rigs": [{"rig": i}]}]
            }));
            assert!(!res.starts_with("error"), "{res}");
        }
        (app, drain)
    }

    #[test]
    fn set_setlist_window_tool_parity_validation_readiness_and_unloaded_jump() {
        // Tool parity (rules 6, 23, 66).
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"set_setlist_window"));
        assert!(UI_BRIDGE_TOOLS.contains(&"set_setlist_window"));
        assert!(!tool_is_read_only("set_setlist_window"));
        assert!(include_str!("../mcp.rs").contains("#[tool(name = \"set_setlist_window\""));

        let (mut app, mut drain) = multi_rig_app(8);
        drain.drain(|_| {});

        // Bad values are refused (rule 77).
        assert!(app.exec_tool("set_setlist_window", &serde_json::json!({"load_ahead": 99})).starts_with("error"));
        assert!(app.exec_tool("set_setlist_window", &serde_json::json!({"load_ahead": "two"})).starts_with("error"));

        // Enable load_ahead = 2 -> hosts songs [0, 1, 2] = 3 of 8 rigs, emits SetHostedRigs mask 0b111.
        let out: serde_json::Value = serde_json::from_str(
            &app.exec_tool("set_setlist_window", &serde_json::json!({"load_ahead": 2})),
        )
        .unwrap();
        assert_eq!(out["load_ahead"], 2);
        assert_eq!(out["window_songs"], serde_json::json!([0, 1, 2]));
        assert_eq!(out["hosted_instances"], 3);
        assert_eq!(out["songs"][0]["load_state"], "ready");
        assert_eq!(out["songs"][2]["load_state"], "ready");
        assert_eq!(out["songs"][3]["load_state"], "unloaded");
        assert_eq!(out["songs"][7]["load_state"], "unloaded");

        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(
            cmds.iter().any(|c| matches!(c, Command::SetHostedRigs { mask: Some(0b111), for_next_load: false })),
            "expected SetHostedRigs(0b111): {cmds:?}"
        );

        // Jumping to unloaded Song 6 (index 5) announces loading and shifts the window to [5, 6, 7].
        let jump: serde_json::Value = serde_json::from_str(
            &app.exec_tool("select_setlist_part", &serde_json::json!({"song": 5, "part": 0})),
        )
        .unwrap();
        assert_eq!(jump["window_songs"], serde_json::json!([5, 6, 7]));
        assert_eq!(jump["loading"], true, "jumping to an unloaded song reports loading: {jump}");
        assert!(
            app.session_msg.contains("Loading 'Song 6'"),
            "session_msg must announce loading: {}",
            app.session_msg
        );
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        assert!(
            cmds.iter().any(|c| matches!(c, Command::SetHostedRigs { mask: Some(0b1110_0000), for_next_load: false })),
            "expected SetHostedRigs for rigs 5,6,7: {cmds:?}"
        );

        // Stepping backwards into an unloaded song via action:"prev_song" also reports loading.
        let step_back: serde_json::Value = serde_json::from_str(
            &app.exec_tool("select_setlist_part", &serde_json::json!({"action": "prev_song"})),
        )
        .unwrap();
        assert_eq!(step_back["loading"], true, "prev_song into unloaded Song 5 reports loading: {step_back}");

        // Pinning unloaded Rig 0 to Show level updates the hosted mask immediately.
        drain.drain(|_| {});
        let pin = app.exec_tool("set_level", &serde_json::json!({"rig": 0, "level": "show"}));
        assert!(!pin.starts_with("error"), "{pin}");
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        assert!(
            cmds.iter().any(|c| matches!(c, Command::SetHostedRigs { mask: Some(m), for_next_load: false } if (m & 1) != 0)),
            "pinning Rig 0 to Show level must host Rig 0 immediately: {cmds:?}"
        );

        // Saving and reloading a windowed session keeps load_ahead AND cached plugin states of unloaded songs.
        let snap = app.capture_session("Windowed Gig").unwrap();
        assert_eq!(snap.setlist.load_ahead, Some(2));
        assert_eq!(
            snap.plugin_states().len(),
            8,
            "all 8 rigs keep their saved plugin state blobs even while unloaded"
        );
        let dir = std::env::temp_dir().join(format!("kabelsalat-window-207-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = crate::session::Session::package_path(&dir, "Windowed Gig").unwrap();
        snap.save(&pkg).unwrap();
        let loaded = crate::session::Session::load(&pkg).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.setlist.load_ahead, Some(2));
        assert_eq!(loaded.plugin_states().len(), 8);

        // Opening the reloaded windowed session primes SetHostedRigs(for_next_load: true) before LoadRigs.
        drain.drain(|_| {});
        app.apply_session(&loaded).unwrap();
        cmds.clear();
        drain.drain(|c| cmds.push(c));
        let prime_pos = cmds
            .iter()
            .position(|c| matches!(c, Command::SetHostedRigs { mask: Some(_), for_next_load: true }));
        let load_pos = cmds
            .iter()
            .position(|c| matches!(c, Command::LoadRigs { .. } | Command::LoadGraph { .. }));
        assert!(
            matches!((prime_pos, load_pos), (Some(p), Some(l)) if p < l),
            "SetHostedRigs(for_next_load: true) must precede LoadRigs/LoadGraph: {cmds:?}"
        );
    }

    #[test]
    fn setlist_ui_and_show_mode_display_window_controls_and_loading_states() {
        let (mut app, _) = multi_rig_app(6);
        app.exec_tool("set_setlist_window", &serde_json::json!({"load_ahead": 1}));
        app.select_setlist(0, 0).unwrap();
        // Simulate Song 0 loading to verify loading indicator in both Setlist and Show mode.
        let s0_node = app.setlist.plugin_nodes_needed_by_song(0, &app.base_graph)[0];
        app.setlist_window.simulated_loading = vec![s0_node];
        assert_eq!(app.song_load_state(0), SongLoadState::Loading);
        assert_eq!(app.song_load_state(1), SongLoadState::Ready);
        assert_eq!(app.song_load_state(2), SongLoadState::Unloaded);

        let ctx = egui::Context::default();
        app.stage_view = StageView::Setlist;
        for (i, size) in [Vec2::new(1280.0, 800.0), Vec2::new(781.0, 950.0)].into_iter().enumerate() {
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear(); // rule 11
            let mut texts = Vec::new();
            fn walk(shape: &Shape, out: &mut Vec<String>) {
                match shape {
                    Shape::Text(t) => out.push(t.galley.text().to_owned()),
                    Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                    _ => {}
                }
            }
            for cs in &out.shapes {
                walk(&cs.shape, &mut texts);
            }
            assert!(texts.iter().any(|t| t.contains("Load ahead")), "{size:?}: missing Load ahead control: {texts:?}");
            assert!(texts.iter().any(|t| t.contains("Loading")), "{size:?}: missing Loading state: {texts:?}");
            if i == 0 {
                assert!(texts.iter().any(|t| t.contains("Ready")), "{size:?}: missing Ready state: {texts:?}");
                assert!(texts.iter().any(|t| t.contains("Unloaded")), "{size:?}: missing Unloaded state: {texts:?}");
            }
        }

        // Show mode also announces when the active song is still loading.
        app.set_show_mode(true);
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0)));
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        assert_eq!(app.show_mode_json()["loading"], true);
    }

    /// End-to-end integration test wiring a REAL `DemoApp` + `ClapLoader` +
    /// `GraphEngine` with real Surge XT (returns early when Surge XT is absent
    /// on CI). Covers Bugs #1–#5:
    /// - `LEVEL`, `SetRigSource`, and `SetRigDevice` survive background attach.
    /// - Engine `rig_busy_mask` / `rig_held_mask` keeps an outgoing rig hosted
    ///   while a note is held, then retires it cleanly after release + quiet.
    /// - Rapid bounce Song 1 -> Song 5 -> Song 1 recovers cleanly without
    ///   stranding on `Loading…` and preserves the loaded sound state.
    #[test]
    fn demo_app_real_engine_window_attach_drain_and_bounce() {
        let Ok((probe_v, probe_m)) = ClapVoice::new(48_000.0) else {
            return; // Surge XT not installed on CI
        };
        probe_m.retire(probe_v);

        let n_rigs = 5usize;
        let mut rigs = Vec::with_capacity(n_rigs);
        for i in 0..n_rigs {
            let mut g = Graph::default();
            let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let s = g.add_node(
                NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) },
                name_bytes(&format!("SYNTH {}", i + 1)),
            );
            let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            g.connect_with_id(1, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, CPortKind::Midi);
            g.connect_with_id(2, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
            rigs.push(crate::control::Rig::new(format!("Rig {}", i + 1), g, None));
        }
        let merged = crate::control::merge_rigs(&rigs);
        let mut sess = crate::session::Session::new("Real Windowed Gig", merged);
        sess.rig_names = (0..n_rigs).map(|i| format!("Rig {}", i + 1)).collect();
        // Custom LEVEL on Rig 0 and hardware-only + device binding on Rig 0.
        let synth0 = sess
            .graph
            .nodes
            .iter()
            .find(|n| n.rig == 0 && matches!(n.kind, NodeKind::Clap { .. }))
            .unwrap()
            .id;
        sess.set_param(synth0, crate::engine::P_LEVEL, 0.42);
        sess.set_midi_sources(crate::session::MidiSources {
            node: synth0,
            keyboard: false,
            hardware: true,
            device: Some("Test Controller".into()),
        });
        for i in 0..n_rigs {
            sess.setlist.songs.push(crate::setlist::Song::new(
                format!("Song {}", i + 1),
                vec![crate::setlist::Part::new(
                    "Main",
                    vec![crate::setlist::PartRig::new(i as u8)],
                )],
            ));
        }
        sess.setlist.load_ahead = Some(1);
        sess.setlist.select(0, 0);

        let (bus_tx, drain) = crate::control::command_bus();
        let mut engine = crate::engine::GraphEngine::new(Graph::default(), drain, 48_000.0, vec![]);
        let (voice_tx, voice_rx) = rtrb::RingBuffer::<VoiceMsg>::new(64);
        let (back_tx, back_rx) = rtrb::RingBuffer::<VoiceBack>::new(64);
        engine.set_voice_channels(voice_rx, back_tx);
        let graph_pub = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
        engine.set_graph_pub(graph_pub.clone());

        let mut app = DemoApp::new_with_sink(None);
        app.hw_midi.devices.lock().unwrap().push(("Test Controller 24:0".into(), true));
        app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
        app.adopt_hw_devices();
        app.bus = Some(Arc::new(Mutex::new(bus_tx)));
        app.graph_pub = Some(graph_pub);
        app.sample_rate = 48_000.0;
        app.voice_tx = Some(voice_tx);
        app.voice_retire_rx = Some(back_rx);
        app.apply_session(&sess).unwrap();

        let mut buf = vec![0.0f32; 256 * 2];
        let step = |app: &mut DemoApp, engine: &mut crate::engine::GraphEngine, buf: &mut [f32]| {
            engine.render(buf, 2);
            app.ui_time += 256.0 / 48_000.0;
            app.sync_from_engine();
            app.poll_setlist_window();
        };

        // Wait until Song 0 and Song 1 are both Ready (window = [0, 1]).
        let t0 = std::time::Instant::now();
        while app.song_load_state(0) != SongLoadState::Ready
            || app.song_load_state(1) != SongLoadState::Ready
        {
            assert!(t0.elapsed() < std::time::Duration::from_secs(15), "window [0,1] never became Ready");
            step(&mut app, &mut engine, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // One more block so commands sent on attach are drained.
        step(&mut app, &mut engine, &mut buf);
        assert_eq!(app.song_load_state(4), SongLoadState::Unloaded);

        // Bug #1 proof: LEVEL=0.42, keyboard=false, hardware=true, device=Some(0)
        // must have reached the bound VoiceSlot on attach!
        let snap = engine.voice_settings_for_test(synth0).expect("synth0 bound in engine");
        assert!((snap.0 - 0.42).abs() < 0.02, "LEVEL lost on attach: got {}", snap.0);
        assert_eq!((snap.1, snap.2, snap.3), (false, true, Some(0)), "RigSource/RigDevice lost on attach: {snap:?}");

        // Bug #2 proof: hold a note on Rig 0 (via Hardware(0)), switch to Song 4
        // (window [4]), and verify Rig 0 stays hosted while held, then retires
        // cleanly after NoteOff + QUIET_RETIRE_SECS!
        app.bus.as_ref().unwrap().lock().unwrap().send(Command::NoteOn {
            note: 60,
            velocity: 0.9,
            source: crate::matrix::SourceId::Hardware(0),
            channel: 0,
        });
        for _ in 0..8 {
            step(&mut app, &mut engine, &mut buf);
        }
        assert!((app.setlist_window.rig_held_mask & 1) != 0, "engine must report Rig 0 held");

        app.select_setlist(4, 0).unwrap();
        for _ in 0..10 {
            step(&mut app, &mut engine, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            engine.voice_settings_for_test(synth0).is_some(),
            "held Rig 0 must NOT retire while note is held!"
        );

        // Release note and advance simulated time past QUIET_RETIRE_SECS.
        app.bus.as_ref().unwrap().lock().unwrap().send(Command::NoteOff {
            note: 60,
            source: crate::matrix::SourceId::Hardware(0),
            channel: 0,
        });
        let t1 = std::time::Instant::now();
        while engine.voice_settings_for_test(synth0).is_some()
            || app.song_load_state(4) != SongLoadState::Ready
        {
            assert!(t1.elapsed() < std::time::Duration::from_secs(15), "Rig 0 never retired or Song 4 never loaded");
            step(&mut app, &mut engine, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(app.song_load_state(4), SongLoadState::Ready);
        assert_eq!(app.song_load_state(0), SongLoadState::Unloaded);

        // Bug #3/#5 proof: rapid bounce Song 4 -> Song 0 -> Song 4 -> Song 0
        // never strands Song 0 on Loading….
        app.select_setlist(0, 0).unwrap();
        step(&mut app, &mut engine, &mut buf);
        app.select_setlist(4, 0).unwrap();
        step(&mut app, &mut engine, &mut buf);
        app.select_setlist(0, 0).unwrap();
        let t2 = std::time::Instant::now();
        while app.song_load_state(0) != SongLoadState::Ready {
            assert!(t2.elapsed() < std::time::Duration::from_secs(15), "Song 0 stranded after rapid bounce");
            step(&mut app, &mut engine, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // One more engine block so VoiceMsg::AddBound pushed by poll_setlist_window attaches.
        step(&mut app, &mut engine, &mut buf);
        assert!(engine.voice_settings_for_test(synth0).is_some());
    }
}
