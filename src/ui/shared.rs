//! Shared instruments across songs (#275) in the app: importing saved song
//! sessions without duplicating matching plugin instances, recalling per-part
//! pan / effect-chain bypasses / CLAP plugin parameters, and splitting a shared
//! rig into its own instance when a song needs a separate preset.

use super::*;
use crate::setlist::{
    find_matching_rig, is_audio_effect, merge_rig_effects_into, FxBypass, MatchResult, Part,
    PartRig, ShowInstances, Song,
};

/// Outcome of importing a saved session as a new setlist song.
#[derive(Clone, Debug)]
pub(super) struct ImportOutcome {
    pub song: usize,
    /// Rig indices placed in the new song's Main part (shared and new alike).
    pub part_rigs: Vec<usize>,
    /// Existing rig indices reused without hosting a new instrument instance.
    pub shared_rigs: Vec<usize>,
    /// Newly added rig indices (new plugin or different preset/state).
    pub new_rigs: Vec<usize>,
    /// Human-readable explanation for each rig (shared vs separate instance).
    pub reasons: Vec<String>,
}

impl DemoApp {
    /// Whole-show hosted instrument accounting for the Setlist header, per-rig
    /// badges and `get_setlist`.
    pub(super) fn show_instances(&self) -> ShowInstances {
        self.setlist.show_instances(&self.mirror_graph())
    }

    /// Snapshot current engine knobs (`params`), pinned CLAP plugin knobs
    /// (`plugin_params`), and audio-effect bypasses (`fx`) for `rigs`.
    pub(super) fn capture_part_state(
        &self,
        rigs: &[PartRig],
    ) -> (Vec<crate::session::ParamValue>, Vec<crate::session::ParamValue>, Vec<FxBypass>) {
        let params = self.capture_part_knobs(rigs);
        let mut plugin_params = Vec::new();
        let mut fx = Vec::new();
        let graph = self.mirror_graph();
        for u in &self.units {
            if !rigs.iter().any(|r| r.rig == u.rig) {
                continue;
            }
            for k in &u.knobs {
                if let Some(pin) = k.clap {
                    let value = pin.min + k.value as f64 * (pin.max - pin.min);
                    plugin_params.push(crate::session::ParamValue {
                        node: u.node_id,
                        param: pin.id,
                        value,
                    });
                }
            }
            if graph.node(u.node_id).is_some_and(|n| is_audio_effect(&graph, n)) {
                fx.push(FxBypass { node: u.node_id, bypass: self.strip_row(u.node_id).bypass });
            }
        }
        plugin_params.sort_by_key(|p| (p.node, p.param));
        plugin_params.dedup_by_key(|p| (p.node, p.param));
        fx.sort_by_key(|f| f.node);
        fx.dedup_by_key(|f| f.node);
        (params, plugin_params, fx)
    }

    /// Recall per-part effect-chain bypasses and CLAP plugin parameters for the
    /// active setlist part (#275). Called by `recall_setlist_part`.
    pub(super) fn recall_part_fx_and_plugin_params(&mut self, graph: &Graph) {
        let fx = self.setlist.recalled_fx(graph);
        for f in fx {
            self.with_strip_row(f.node, |row| row.bypass = f.bypass);
            self.send_strip_bypass(f.node);
        }
        let pparams = self.setlist.recalled_plugin_params(graph);
        if pparams.is_empty() {
            return;
        }
        let slots = self.clap_rigs();
        let rm = self.reduce_motion;
        let hold_until = self.ui_time + READBACK_HOLD_SECS;
        for p in pparams {
            if let Some(slot) = slots.iter().find(|(_, n)| *n == p.node).map(|(s, _)| *s) {
                if let Some(idx) = self.clap_slot_map.get(slot).copied().flatten() {
                    if let Some(ctl) = self.surge_controllers.get(idx) {
                        ctl.set_param(ClapId::new(p.param), p.value);
                    }
                }
            }
            if let Some(u) = self.units.iter_mut().find(|u| u.node_id == p.node) {
                for k in &mut u.knobs {
                    if let Some(pin) = k.clap {
                        if pin.id == p.param {
                            let span = (pin.max - pin.min).abs();
                            let norm = if span > 1e-9 {
                                ((p.value - pin.min) / (pin.max - pin.min)).clamp(0.0, 1.0) as f32
                            } else {
                                0.0
                            };
                            k.glide_to(norm, rm);
                            if let Some(pin) = k.clap.as_mut() {
                                pin.sent = norm;
                                pin.hold_until = hold_until;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Audio effect units belonging to `rig` (in rack order), for per-part
    /// effect-chain toggles in the Setlist view.
    pub(super) fn rig_audio_effects(&self, rig: u8) -> Vec<(u32, String, bool)> {
        let graph = self.mirror_graph();
        self.units
            .iter()
            .filter(|u| u.rig == rig && graph.node(u.node_id).is_some_and(|n| is_audio_effect(&graph, n)))
            .map(|u| (u.node_id, sentence_case(&u.name), self.strip_row(u.node_id).bypass))
            .collect()
    }

    /// Split a shared rig inside one part into its own hosted instance (so the
    /// song can load a different preset without disturbing other songs).
    pub(super) fn split_part_rig_to_own_instance(
        &mut self,
        song_idx: usize,
        part_idx: usize,
        rig: u8,
    ) -> Result<usize, String> {
        let old_nodes: Vec<u32> = {
            let mut ids: Vec<u32> = self.units.iter().filter(|u| u.rig == rig).map(|u| u.node_id).collect();
            ids.sort_unstable();
            ids
        };
        let new_rig = self.clone_rig(rig as usize, None).map_err(|e| format!("{e:#}"))?;
        let new_nodes: Vec<u32> = {
            let mut ids: Vec<u32> = self
                .units
                .iter()
                .filter(|u| u.rig as usize == new_rig)
                .map(|u| u.node_id)
                .collect();
            ids.sort_unstable();
            ids
        };
        let map_node = |id: u32| -> u32 {
            old_nodes
                .iter()
                .position(|&o| o == id)
                .and_then(|i| new_nodes.get(i).copied())
                .unwrap_or(id)
        };
        if let Some(part) = self
            .setlist
            .songs
            .get_mut(song_idx)
            .and_then(|s| s.parts.get_mut(part_idx))
        {
            if let Some(pr) = part.rigs.iter_mut().find(|r| r.rig == rig) {
                pr.rig = new_rig as u8;
            }
            part.rigs.sort_by_key(|r| r.rig);
            for p in &mut part.params {
                if old_nodes.contains(&p.node) {
                    p.node = map_node(p.node);
                }
            }
            part.params.sort_by_key(|p| (p.node, p.param));
            for p in &mut part.plugin_params {
                if old_nodes.contains(&p.node) {
                    p.node = map_node(p.node);
                }
            }
            part.plugin_params.sort_by_key(|p| (p.node, p.param));
            for f in &mut part.fx {
                if old_nodes.contains(&f.node) {
                    f.node = map_node(f.node);
                }
            }
            part.fx.sort_by_key(|f| f.node);
        }
        let song_name = self
            .setlist
            .songs
            .get(song_idx)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        if self.setlist.active == Some(crate::setlist::SetlistPos { song: song_idx, part: part_idx }) {
            self.apply_setlist_part();
        }
        self.sync_setlist_window();
        self.session_msg = format!("Separate instance '{}' for '{song_name}' (own preset)", self.rig_name(new_rig));
        Ok(new_rig)
    }

    /// "Add song from session" (#204, #275): import a saved session as a new
    /// song, sharing any existing hosted rig whose instrument(s) and saved
    /// preset/state match (when `share` is true) instead of loading duplicate
    /// plugin instances. Extra audio effects on a shared rig are spliced in and
    /// gated per part via `Part::fx`.
    pub(super) fn add_song_from_session_shared(
        &mut self,
        dir: &std::path::Path,
        session_name: &str,
        song_name: Option<&str>,
        share: bool,
    ) -> Result<ImportOutcome, String> {
        let imported = crate::session::Session::package_path(dir, session_name)
            .and_then(|p| crate::session::Session::load(&p))
            .map_err(|e| format!("Could not load session '{session_name}': {e}"))?;
        let mut imported = canonical_rig_session(&imported);
        let (imp_graphs, imp_bases) = crate::control::split_rigs(&imported.graph);
        if imp_graphs.is_empty() {
            return Err(format!("Session '{session_name}' has no rigs."));
        }
        let rules: Vec<SavedRule> = imported
            .matrix()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let used = hw_indices_in_use(&rules, &imported.rig_devices);
        let ports = imported
            .hw_ports()
            .iter()
            .filter(|(d, _)| used.contains(d))
            .map(|(&d, p)| (d, p.clone()))
            .collect();
        let map = self.hw_index_map(&ports);
        map_session_hw(&mut imported, &map);

        let mut cur = self
            .capture_session("Import")
            .map_err(|e| format!("Could not capture current rack: {e}"))?;
        let cur_n = self.rig_count();

        // Decide for each imported rig whether it shares an existing rig or
        // needs a new hosted instance.
        enum Plan {
            Share(usize),
            Append,
        }
        let mut plans = Vec::with_capacity(imp_graphs.len());
        let mut claimed_existing: Vec<usize> = Vec::new();
        let mut reasons: Vec<String> = Vec::new();
        let mut append_count = 0usize;
        for i in 0..imp_graphs.len() {
            let imp_label = imported
                .rig_names
                .get(i)
                .filter(|n| !n.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    if imp_bases.len() == 1 {
                        imported.name.clone()
                    } else {
                        format!("{} {}", imported.name, crate::control::rig_letter(i))
                    }
                });
            if share {
                match find_matching_rig(
                    &cur.graph,
                    cur.plugin_states(),
                    &cur.setlist,
                    &imported.graph,
                    i as u8,
                    imported.plugin_states(),
                ) {
                    MatchResult::Shared(existing) if !claimed_existing.contains(&(existing as usize)) => {
                        claimed_existing.push(existing as usize);
                        plans.push(Plan::Share(existing as usize));
                        reasons.push(format!(
                            "'{imp_label}' shared with '{}' (1 instance; level/pan/FX/knobs at switch)",
                            self.rig_name(existing as usize)
                        ));
                    }
                    MatchResult::DifferentPreset(other) => {
                        append_count += 1;
                        plans.push(Plan::Append);
                        reasons.push(format!(
                            "'{imp_label}' hosted as separate instance (different preset from '{}')",
                            self.rig_name(other as usize)
                        ));
                    }
                    _ => {
                        append_count += 1;
                        plans.push(Plan::Append);
                        reasons.push(format!("'{imp_label}' hosted as new instance"));
                    }
                }
            } else {
                append_count += 1;
                plans.push(Plan::Append);
                reasons.push(format!("'{imp_label}' hosted as separate instance (sharing disabled)"));
            }
        }

        if cur_n + append_count > Self::MAX_RIGS {
            return Err(format!(
                "Cannot import {} new rig(s) from '{session_name}': {cur_n} rig(s) already loaded (max {}).",
                append_count,
                Self::MAX_RIGS
            ));
        }

        // First, splice any extra audio effects for shared rigs onto `cur.graph`.
        let mut shared_node_maps: Vec<(usize, Vec<(u32, u32)>, Vec<u32>, Vec<u32>)> = Vec::new();
        let mut graph_changed = append_count > 0;
        for (i, plan) in plans.iter().enumerate() {
            let Plan::Share(existing) = *plan else { continue };
            let (nmap, unused_fx, added_fx) =
                merge_rig_effects_into(&mut cur.graph, existing as u8, &imported.graph, i as u8);
            if !added_fx.is_empty() {
                graph_changed = true;
            }
            // Preserve every existing song's effect chain when another song
            // shares `existing`: newly spliced effects stay bypassed in older
            // parts, and existing effects that the new song toggles record
            // their resting bypass state so switching back restores them.
            let mut preserve_fx: Vec<(u32, bool)> = Vec::new();
            for &nid in &unused_fx {
                let cur_byp = cur.strips().iter().find(|s| s.node == nid).is_some_and(|s| s.bypass);
                preserve_fx.push((nid, cur_byp));
            }
            for &(_, tgt_id) in &nmap {
                if !added_fx.contains(&tgt_id)
                    && cur.graph.node(tgt_id).is_some_and(|n| is_audio_effect(&cur.graph, n))
                {
                    let cur_byp = cur.strips().iter().find(|s| s.node == tgt_id).is_some_and(|s| s.bypass);
                    preserve_fx.push((tgt_id, cur_byp));
                }
            }
            for song in &mut cur.setlist.songs {
                for part in &mut song.parts {
                    if !part.rigs.iter().any(|r| r.rig as usize == existing) {
                        continue;
                    }
                    for &nid in &added_fx {
                        part.set_fx(nid, true);
                    }
                    for &(nid, cur_byp) in &preserve_fx {
                        if part.fx_bypass(nid).is_none() {
                            part.set_fx(nid, cur_byp);
                        }
                    }
                }
            }
            shared_node_maps.push((i, nmap, unused_fx, added_fx));
        }

        let title = song_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&imported.name)
            .to_string();

        // Fast path: every imported rig matched an existing hosted rig AND no
        // new effect nodes were spliced into the graph — zero reload, zero
        // plugin instantiation!
        if !graph_changed {
            let mut part_rigs = Vec::new();
            let mut shared_rigs = Vec::new();
            let mut main_part = Part::new("Main", Vec::new());
            for (i, plan) in plans.iter().enumerate() {
                let Plan::Share(existing) = *plan else { continue };
                shared_rigs.push(existing);
                part_rigs.push(existing);
                let pr = PartRig {
                    rig: existing as u8,
                    level: self.rig_mix[existing].vol,
                    pan: Some(self.rig_mix[existing].pan),
                    ..PartRig::default()
                };
                main_part.set_rig(pr);
                if let Some((_, nmap, unused_fx, _)) = shared_node_maps.iter().find(|(idx, _, _, _)| *idx == i) {
                    let map_imp = |old: u32| nmap.iter().find(|(a, _)| *a == old).map(|(_, b)| *b);
                    for p in imported.params() {
                        if let Some(tgt) = map_imp(p.node) {
                            main_part.set_param(tgt, p.param, p.value);
                        }
                    }
                    for &(imp_id, tgt_id) in nmap {
                        if cur.graph.node(tgt_id).is_some_and(|n| is_audio_effect(&cur.graph, n)) {
                            let imp_byp = imported.strips().iter().find(|s| s.node == imp_id).is_some_and(|s| s.bypass);
                            main_part.set_fx(tgt_id, imp_byp);
                        }
                    }
                    for &fx_id in unused_fx {
                        main_part.set_fx(fx_id, true);
                    }
                }
            }
            let mut song = Song::new(title, vec![main_part]);
            song.time_sig = imported.time_sig;
            self.setlist = cur.setlist;
            self.setlist.songs.push(song);
            let song_idx = self.setlist.songs.len() - 1;
            self.setlist_edit_song = Some(song_idx);
            self.sync_setlist_window();
            let hosted = self.show_instances().hosted;
            self.session_msg = format!(
                "Added '{session_name}' sharing existing instrument ({hosted} {} hosted)",
                if hosted == 1 { "instance" } else { "instances" }
            );
            return Ok(ImportOutcome {
                song: song_idx,
                part_rigs,
                shared_rigs,
                new_rigs: Vec::new(),
                reasons,
            });
        }

        // Structural path: either at least one new rig is appended or new
        // audio effects were spliced onto a shared rig.
        let voice_rows = self.voice_rows();
        let (cur_graphs, cur_bases) = crate::control::split_rigs(&cur.graph);
        let devices: Vec<Option<u32>> = {
            let snap = self.rig_snapshot.lock().ok();
            (0..cur_graphs.len())
                .map(|i| snap.as_ref().and_then(|sn| sn.rigs.get(i).and_then(|m| m.midi_device)))
                .collect()
        };
        let mut list: Vec<crate::control::Rig> = cur_graphs
            .into_iter()
            .enumerate()
            .map(|(i, g)| crate::control::Rig::new(self.rig_name(i), g, devices[i]))
            .collect();
        let first_new_rig = list.len();
        let mut appended_map: Vec<Option<usize>> = vec![None; imp_graphs.len()];
        let mut new_rigs = Vec::new();
        let mut shared_rigs = Vec::new();
        let mut part_rig_indices = Vec::new();
        for (i, g) in imp_graphs.into_iter().enumerate() {
            match plans[i] {
                Plan::Share(existing) => {
                    shared_rigs.push(existing);
                    part_rig_indices.push(existing);
                }
                Plan::Append => {
                    let base_name = imported
                        .rig_names
                        .get(i)
                        .filter(|n| !n.trim().is_empty())
                        .cloned()
                        .unwrap_or_else(|| {
                            if imp_bases.len() == 1 {
                                imported.name.clone()
                            } else {
                                format!("{} {}", imported.name, crate::control::rig_letter(i))
                            }
                        });
                    let rname: String = base_name.trim().chars().take(24).collect();
                    let idx = list.len();
                    list.push(crate::control::Rig::new(
                        rname,
                        g,
                        imported.rig_devices.get(i).copied().flatten(),
                    ));
                    appended_map[i] = Some(idx);
                    new_rigs.push(idx);
                    part_rig_indices.push(idx);
                }
            }
        }
        let new_bases = crate::control::rig_bases(&list);
        let merged = crate::control::merge_rigs(&list);
        let remap_cur = |old: u32| -> Option<u32> {
            let o = cur.graph.node(old)?.rig as usize;
            Some(new_bases[o] + (old - cur_bases[o]))
        };
        let remap_imp_appended = |old: u32| -> Option<u32> {
            let o = imported.graph.node(old)?.rig as usize;
            let dst = appended_map[o]?;
            Some(new_bases[dst] + (old - imp_bases[o]))
        };
        let mut ns = remap_session(&cur, merged, remap_cur);

        // Build the new song's Main part: params & fx for both appended and shared rigs.
        let mut main_part = Part::new("Main", Vec::new());
        for &r in &part_rig_indices {
            main_part.set_rig(PartRig::new(r as u8));
        }
        for p in imported.params() {
            if let Some(nid) = remap_imp_appended(p.node) {
                ns.set_param(nid, p.param, p.value);
                main_part.set_param(nid, p.param, p.value);
            }
        }
        for st in imported.plugin_states() {
            if let Some(nid) = remap_imp_appended(st.node) {
                ns.set_plugin_state(nid, st.plugin_id.clone(), st.data.clone());
            }
        }
        // Remap shared-rig imported params and effect bypasses through `remap_cur`.
        for (_, nmap, unused_fx, added_fx) in &shared_node_maps {
            let map_imp = |old: u32| -> Option<u32> {
                let cur_id = nmap.iter().find(|(a, _)| *a == old).map(|(_, b)| *b)?;
                remap_cur(cur_id)
            };
            for p in imported.params() {
                if let Some(nid) = map_imp(p.node) {
                    main_part.set_param(nid, p.param, p.value);
                }
            }
            for &(imp_id, cur_id) in nmap {
                if let Some(nid) = remap_cur(cur_id) {
                    if ns.graph.node(nid).is_some_and(|n| is_audio_effect(&ns.graph, n)) {
                        let imp_byp = imported.strips().iter().find(|s| s.node == imp_id).is_some_and(|s| s.bypass);
                        main_part.set_fx(nid, imp_byp);
                    }
                }
            }
            for &cur_fx in unused_fx {
                if let Some(nid) = remap_cur(cur_fx) {
                    main_part.set_fx(nid, true);
                }
            }
            for &cur_fx in added_fx {
                if let Some(nid) = remap_cur(cur_fx) {
                    // Newly spliced effects default to bypassed at rest so the
                    // current sound does not change until the new song plays;
                    // `main_part` already recorded the imported bypass above.
                    ns.set_strip(crate::session::StripSaved { node: nid, bypass: true, ..Default::default() });
                }
            }
        }

        let mut all_rules: Vec<SavedRule> = ns
            .matrix()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let imp_rules: Vec<SavedRule> = imported
            .matrix()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        for mut r in imp_rules {
            if let Some(nid) = remap_imp_appended(r.dest) {
                r.dest = nid;
                all_rules.push(r);
            }
        }
        if !all_rules.is_empty() {
            ns.set_matrix(serde_json::to_value(&all_rules).ok());
        }
        for st in imported.strips() {
            if let Some(node) = remap_imp_appended(st.node) {
                ns.set_strip(crate::session::StripSaved { node, ..*st });
            }
        }
        for (&old, label) in imported.strip_knobs() {
            if let Some(node) = remap_imp_appended(old) {
                ns.set_strip_knob(node, label.clone());
            }
        }
        for m in imported.midi_sources() {
            if let Some(node) = remap_imp_appended(m.node) {
                ns.set_midi_sources(crate::session::MidiSources { node, ..m.clone() });
            }
        }

        let mut song = Song::new(title, vec![main_part]);
        song.time_sig = imported.time_sig;
        ns.setlist.songs.push(song);
        let song_idx = ns.setlist.songs.len() - 1;

        let saved_mix = self.rig_mix;
        let saved_arms = self.arm_rigs;
        let active = self.active_rig;
        self.load_as_rigs = Some((list.clone(), active));
        self.apply_session(&ns).map_err(|e| format!("Import failed: {e}"))?;
        if let Ok(mut snap) = self.rig_snapshot.lock() {
            snap.rigs = list
                .iter()
                .map(|r| crate::control::RigMeta { name: r.name.clone(), midi_device: r.midi_device })
                .collect();
            snap.active = active;
        }
        let voice_rows: Vec<VoiceRow> = voice_rows
            .into_iter()
            .filter_map(|r| remap_cur(r.node).map(|node| VoiceRow { node, ..r }))
            .collect();
        self.restore_voice_rows(&voice_rows);
        for i in 0..first_new_rig.min(Self::MAX_RIGS) {
            self.rig_mix[i] = saved_mix[i];
            self.send_rig_mix(i);
            self.set_record_arm(Some(i), saved_arms[i]);
        }
        self.active_rig = usize::MAX;
        self.set_active_rig(active);
        self.setlist_edit_song = Some(song_idx);
        let hosted = self.show_instances().hosted;
        self.session_msg = format!(
            "Added '{session_name}' ({} shared, {} new · {hosted} {} hosted)",
            shared_rigs.len(),
            new_rigs.len(),
            if hosted == 1 { "instance" } else { "instances" },
        );
        Ok(ImportOutcome {
            song: song_idx,
            part_rigs: part_rig_indices,
            shared_rigs,
            new_rigs,
            reasons,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ClapId, SURGE_PLUGIN_ID};

    fn piano_session(name: &str, state: &[u8], fx: &[DspKind]) -> crate::session::Session {
        let mut g = Graph::default();
        let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let p = g.add_node(NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) }, name_bytes("PIANO"));
        let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let mut eid = 1u32;
        g.connect_with_id(eid, PortRef { node: k, port: 0 }, PortRef { node: p, port: 0 }, CPortKind::Midi);
        eid += 1;
        let mut prev = p;
        for kind in fx {
            let f = g.add_node(NodeKind::Dsp(kind.clone()), name_bytes(&format!("{kind:?}")));
            g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: f, port: 0 }, CPortKind::Audio);
            eid += 1;
            prev = f;
        }
        g.connect_with_id(eid, PortRef { node: prev, port: 0 }, PortRef { node: m, port: 0 }, CPortKind::Audio);
        g.connect_with_id(eid + 1, PortRef { node: prev, port: 0 }, PortRef { node: m, port: 1 }, CPortKind::Audio);
        let mut s = crate::session::Session::new(name, g);
        s.rig_names = vec!["Piano".into()];
        if !state.is_empty() {
            s.set_plugin_state(p, SURGE_PLUGIN_ID, state.to_vec());
        }
        s
    }

    #[test]
    fn importing_twenty_piano_songs_hosts_one_instance_and_separates_different_presets() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-shared-275-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let base = piano_session("Song 1", &[], &[DspKind::Reverb]);
        let mut app = DemoApp::new_with_sink(None);
        app.apply_session(&base).unwrap();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));

        // Save 19 more songs with the same piano (default state), half with Delay
        // instead of Reverb so effect-chain merging runs too.
        for i in 1..20 {
            let name = format!("PianoSong{i}");
            let fx: &[DspKind] = if i % 2 == 0 { &[DspKind::Reverb] } else { &[DspKind::Delay] };
            let s = piano_session(&name, &[], fx);
            let pkg = crate::session::Session::package_path(&dir, &name).unwrap();
            s.save(&pkg).unwrap();
        }
        // First song created directly on Rig 0.
        app.exec_tool("set_setlist_song", &serde_json::json!({
            "name": "Song 0",
            "parts": [{"name": "Main", "rigs": [{"rig": 0, "level": 0.8, "pan": -0.3}]}]
        }));
        for i in 1..20 {
            let out = app.add_song_from_session_shared(&dir, &format!("PianoSong{i}"), None, true).unwrap();
            assert_eq!(out.shared_rigs, vec![0], "song {i} shares rig 0");
            assert!(out.new_rigs.is_empty(), "song {i} adds no new piano instance");
        }
        assert_eq!(app.rig_count(), 1, "20 songs sharing one piano stay on 1 rig");
        let summary = app.show_instances();
        assert_eq!(summary.hosted, 1, "20 songs host 1 piano instance");
        assert_eq!(summary.shared, 1);
        assert_eq!(summary.rigs[0].songs.len(), 20);

        // Song 1 (Delay) vs Song 2 (Reverb) toggle effect bypasses on switch.
        let delay = app.units.iter().find(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay))).unwrap().node_id;
        let reverb = app.units.iter().find(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Reverb))).unwrap().node_id;
        drain.drain(|_| {});
        app.select_setlist(1, 0).unwrap();
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetStripBypass { node, bypass: false } if *node == delay)));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetStripBypass { node, bypass: true } if *node == reverb)));

        // Importing a song with a different preset state hosts a separate instance.
        let felt = piano_session("FeltSong", b"felt-piano-preset", &[DspKind::Reverb]);
        let felt_pkg = crate::session::Session::package_path(&dir, "FeltSong").unwrap();
        felt.save(&felt_pkg).unwrap();
        let out = app.add_song_from_session_shared(&dir, "FeltSong", None, true).unwrap();
        assert!(out.shared_rigs.is_empty());
        assert_eq!(out.new_rigs, vec![1], "different preset gets its own rig/instance");
        assert!(out.reasons[0].contains("different preset"), "{:?}", out.reasons);
        assert_eq!(app.show_instances().hosted, 2);

        // Splitting one song's shared rig into its own instance leaves the other songs shared.
        let new_rig = app.split_part_rig_to_own_instance(2, 0, 0).unwrap();
        assert_eq!(new_rig, 2);
        assert_eq!(app.setlist.songs[2].parts[0].rigs[0].rig as usize, new_rig);
        assert_eq!(app.setlist.songs[3].parts[0].rigs[0].rig, 0);
        assert_eq!(app.show_instances().hosted, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_setlist_song_recalls_pan_and_effect_chain_bypasses() {
        let base = piano_session("Gig", &[], &[DspKind::Delay, DspKind::Reverb]);
        let mut app = DemoApp::new_with_sink(None);
        app.apply_session(&base).unwrap();
        let (tx, mut drain) = crate::control::command_bus();
        app.bus = Some(Arc::new(Mutex::new(tx)));
        let delay = app.units.iter().find(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay))).unwrap().node_id;
        let reverb = app.units.iter().find(|u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Reverb))).unwrap().node_id;

        let res = app.exec_tool("set_setlist_song", &serde_json::json!({
            "name": "Ballad",
            "parts": [{
                "name": "Main",
                "rigs": [{"rig": 0, "level": 0.7, "pan": -0.45}],
                "fx": [{"node": delay, "bypass": true}, {"node": reverb, "bypass": false}]
            }]
        }));
        let v: serde_json::Value = serde_json::from_str(&res).expect("valid json");
        let pan_json = v["songs"][0]["parts"][0]["rigs"][0]["pan"].as_f64().expect("pan number");
        assert!((pan_json + 0.45).abs() < 1e-5, "unexpected pan: {pan_json}");
        assert_eq!(v["instances"]["hosted"], serde_json::json!(1));

        drain.drain(|_| {});
        app.select_setlist(0, 0).unwrap();
        let mut cmds = Vec::new();
        drain.drain(|c| cmds.push(c));
        assert!(
            cmds.iter().any(|c| matches!(c, Command::SetRigMix { rig: 0, vol: Some(vol), pan: Some(pan), .. } if (*vol - 0.7).abs() < 1e-4 && (*pan + 0.45).abs() < 1e-4)),
            "SetRigMix must recall both vol and pan: {cmds:?}"
        );
        assert!(cmds.iter().any(|c| matches!(c, Command::SetStripBypass { node, bypass: true } if *node == delay)));
        assert!(cmds.iter().any(|c| matches!(c, Command::SetStripBypass { node, bypass: false } if *node == reverb)));
    }
}
