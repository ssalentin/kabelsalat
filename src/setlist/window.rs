//! Per-setlist song load window (#207): keep only the current song and the next
//! few playable songs hosted in memory so long setlists stay inside RAM on small
//! machines while switching in order remains gapless.

use crate::control::{Graph, MidiFxPlugin, Node, NodeId, NodeKind, PortKind};

use super::{Level, Setlist, MAX_RIGS};

/// Default number of upcoming songs kept preloaded ahead of the active song
/// when window loading is enabled (`current + 2 = 3` songs hosted).
pub const DEFAULT_LOAD_AHEAD: u8 = 2;

/// Maximum configurable lookahead window in songs.
pub const MAX_LOAD_AHEAD: u8 = 8;

/// Readiness of one song in the setlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SongLoadState {
    /// Every plugin node the song needs is hosted and ready to play.
    Ready,
    /// At least one needed plugin node is loading in the background now.
    Loading,
    /// Outside the active load window (unloaded to save memory).
    Unloaded,
}

impl SongLoadState {
    pub fn as_str(self) -> &'static str {
        match self {
            SongLoadState::Ready => "ready",
            SongLoadState::Loading => "loading",
            SongLoadState::Unloaded => "unloaded",
        }
    }
}

/// True when `node` hosts an external plugin (CLAP synth/effect, LV2
/// instrument/effect, or LV2 MIDI effect).
pub fn is_plugin_node(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Clap { .. }
            | NodeKind::Lv2 { .. }
            | NodeKind::MidiFx { plugin: MidiFxPlugin::Lv2 { .. } }
    )
}

/// True when `node` lies on a master-insert chain (`MasterOut → FX → …`, rule 179).
pub fn is_master_insert(graph: &Graph, node: NodeId) -> bool {
    let mut frontier: Vec<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::MasterOut))
        .map(|n| n.id)
        .collect();
    let mut visited = frontier.clone();
    while let Some(cur) = frontier.pop() {
        for e in &graph.edges {
            if e.kind == PortKind::Audio && e.from.node == cur {
                if e.to.node == node {
                    return true;
                }
                if !visited.contains(&e.to.node) {
                    visited.push(e.to.node);
                    frontier.push(e.to.node);
                }
            }
        }
    }
    false
}

/// Find the aux bus node upstream of `node` (when `graph.after_bus(node)` is true).
fn upstream_bus(graph: &Graph, node: NodeId) -> Option<NodeId> {
    let mut frontier = vec![node];
    let mut visited = vec![node];
    while let Some(cur) = frontier.pop() {
        for e in &graph.edges {
            if e.kind == PortKind::Audio && e.to.node == cur {
                if graph.is_bus(e.from.node) {
                    return Some(e.from.node);
                }
                if !visited.contains(&e.from.node) {
                    visited.push(e.from.node);
                    frontier.push(e.from.node);
                }
            }
        }
    }
    None
}

impl Setlist {
    /// True when the per-setlist load window actively restricts hosted rigs.
    pub fn window_enabled(&self) -> bool {
        self.load_ahead.is_some() && !self.songs.is_empty()
    }

    /// Ordered song indices inside the active load window: the active song
    /// (or first playable song before playback starts) followed by the next
    /// `load_ahead` playable songs. When windowing is off, returns every song.
    pub fn window_songs(&self) -> Vec<usize> {
        let Some(ahead) = self.load_ahead else {
            return (0..self.songs.len()).collect();
        };
        if self.songs.is_empty() {
            return Vec::new();
        }
        let ahead = ahead.clamp(1, MAX_LOAD_AHEAD) as usize;
        let anchor = self
            .active
            .map(|p| p.song)
            .filter(|&s| s < self.songs.len())
            .or_else(|| self.first_non_empty_pos().map(|p| p.song))
            .unwrap_or(0);
        let mut out = Vec::with_capacity(1 + ahead);
        out.push(anchor);
        for si in (anchor + 1)..self.songs.len() {
            if out.len() >= 1 + ahead {
                break;
            }
            if self.playable(si) {
                out.push(si);
            }
        }
        out
    }

    /// True when `bus` is used by `song` under the show's levels.
    fn bus_needed_by_song(&self, song: usize, bus: NodeId) -> bool {
        let Some(s) = self.songs.get(song) else { return false };
        match self.bus_level(bus) {
            Level::Show => true,
            Level::Set(id) => s.set == Some(id),
            Level::Song => s.parts.iter().any(|p| p.buses.contains(&bus)),
        }
    }

    /// Every plugin node (CLAP synth/effect, LV2 instrument/effect/MIDI-FX)
    /// that `song` needs hosted to play cleanly.
    pub fn plugin_nodes_needed_by_song(&self, song: usize, graph: &Graph) -> Vec<NodeId> {
        let rigs = self.rigs_needed_by_song(song, graph);
        let mut nodes: Vec<NodeId> = graph
            .nodes
            .iter()
            .filter(|n| {
                if !is_plugin_node(n) {
                    return false;
                }
                if is_master_insert(graph, n.id) {
                    return true;
                }
                if graph.after_bus(n.id) {
                    return upstream_bus(graph, n.id)
                        .map_or(true, |bus| self.bus_needed_by_song(song, bus));
                }
                rigs.contains(&n.rig)
            })
            .map(|n| n.id)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    /// Rig indices needed by the active load window (`None` = host every rig).
    pub fn window_rigs(&self, graph: &Graph) -> Option<Vec<u8>> {
        if !self.window_enabled() {
            return None;
        }
        let mut rigs = Vec::new();
        for si in self.window_songs() {
            for r in self.rigs_needed_by_song(si, graph) {
                if !rigs.contains(&r) {
                    rigs.push(r);
                }
            }
        }
        rigs.sort_unstable();
        Some(rigs)
    }

    /// 32-bit rig bitmask for `Command::SetHostedRigs` (`None` = host every rig).
    pub fn window_rigs_mask(&self, graph: &Graph) -> Option<u32> {
        let rigs = self.window_rigs(graph)?;
        Some(rigs.into_iter().fold(0u32, |m, r| {
            if (r as usize) < MAX_RIGS { m | (1u32 << r) } else { m }
        }))
    }

    /// Plugin nodes needed by the active load window, in song-priority order
    /// (current song first, then next upcoming songs). `None` = host every node.
    pub fn window_plugin_nodes(&self, graph: &Graph) -> Option<Vec<NodeId>> {
        if !self.window_enabled() {
            return None;
        }
        let mut nodes = Vec::new();
        for si in self.window_songs() {
            for id in self.plugin_nodes_needed_by_song(si, graph) {
                if !nodes.contains(&id) {
                    nodes.push(id);
                }
            }
        }
        Some(nodes)
    }

    /// Readiness state of `song` given the engine's currently loaded plugin
    /// nodes and the loader threads' in-flight loading nodes.
    pub fn song_load_state(
        &self,
        song: usize,
        graph: &Graph,
        loaded_nodes: &[NodeId],
        loading_nodes: &[NodeId],
    ) -> SongLoadState {
        let needed = self.plugin_nodes_needed_by_song(song, graph);
        if needed.is_empty() || needed.iter().all(|id| loaded_nodes.contains(id)) {
            return SongLoadState::Ready;
        }
        if needed.iter().any(|id| loading_nodes.contains(id)) {
            return SongLoadState::Loading;
        }
        if !self.window_enabled() || self.window_songs().contains(&song) {
            SongLoadState::Loading
        } else {
            SongLoadState::Unloaded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, ClapId, Lv2Uri, PortRef, SURGE_PLUGIN_ID};
    use crate::setlist::{Part, PartRig, SetlistPos, Song};

    fn multi_rig_graph(n: usize) -> (Graph, Vec<NodeId>) {
        let mut g = Graph::default();
        let mut synths = Vec::with_capacity(n);
        let mut eid = 1u32;
        for r in 0..n {
            let k = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
            let s = g.add_node(
                NodeKind::Clap { plugin_id: ClapId::new(SURGE_PLUGIN_ID) },
                name_bytes(&format!("SYNTH {}", r + 1)),
            );
            let m = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
            for id in [k, s, m] {
                g.set_rig(id, r as u8);
            }
            g.connect_with_id(eid, PortRef { node: k, port: 0 }, PortRef { node: s, port: 0 }, PortKind::Midi);
            g.connect_with_id(eid + 1, PortRef { node: s, port: 0 }, PortRef { node: m, port: 0 }, PortKind::Audio);
            eid += 2;
            synths.push(s);
        }
        (g, synths)
    }

    #[test]
    fn window_songs_slides_with_active_song_and_skips_skipped_sets() {
        let (g, synths) = multi_rig_graph(8);
        let mut sl = Setlist::default();
        for i in 0..8 {
            sl.songs.push(Song::new(
                format!("Song {}", i + 1),
                vec![Part::new("Main", vec![PartRig::new(i as u8)])],
            ));
        }
        // Disabled by default: every rig stays hosted.
        assert!(!sl.window_enabled());
        assert_eq!(sl.window_rigs(&g), None);
        assert_eq!(sl.window_plugin_nodes(&g), None);

        sl.load_ahead = Some(2);
        assert!(sl.window_enabled());
        // Before active starts, anchors at song 0 -> [0, 1, 2].
        assert_eq!(sl.window_songs(), vec![0, 1, 2]);
        assert_eq!(sl.window_rigs(&g), Some(vec![0, 1, 2]));
        assert_eq!(sl.window_plugin_nodes(&g), Some(vec![synths[0], synths[1], synths[2]]));

        // Put songs 2 and 3 in a skipped set: window from song 1 skips them -> [1, 4, 5].
        let set_id = sl.add_set("Skipped");
        sl.songs[2].set = Some(set_id);
        sl.songs[3].set = Some(set_id);
        sl.sets.iter_mut().find(|s| s.id == set_id).unwrap().skip = true;
        sl.select(1, 0);
        assert_eq!(sl.window_songs(), vec![1, 4, 5]);
        assert_eq!(sl.window_rigs(&g), Some(vec![1, 4, 5]));

        // Jumping directly to a skipped song still anchors on it -> [2, 4, 5].
        sl.select(2, 0);
        assert_eq!(sl.window_songs(), vec![2, 4, 5]);
    }

    #[test]
    fn window_preserves_show_and_set_rigs_shared_instruments_and_master_inserts() {
        let (mut g, synths) = multi_rig_graph(5);
        // Add an LV2 master insert on rig 0's MasterOut.
        let m0 = g.nodes.iter().find(|n| n.rig == 0 && matches!(n.kind, NodeKind::MasterOut)).unwrap().id;
        let master_fx = g.add_node(
            NodeKind::Lv2 { uri: Lv2Uri::new("http://example.org/master-comp") },
            name_bytes("MASTER COMP"),
        );
        g.set_rig(master_fx, 0);
        let eid = g.edges.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        g.connect_with_id(eid, PortRef { node: m0, port: 0 }, PortRef { node: master_fx, port: 0 }, PortKind::Audio);

        let mut sl = Setlist::default();
        // Rig 0 is Show-level (e.g. always-on piano/vocal).
        sl.pin_rig(0, Level::Show);
        // Rig 1 is Set-level for Set 1 (songs 0..3).
        let set1 = sl.add_set("Set 1");
        sl.pin_rig(1, Level::Set(set1));
        for i in 0..6 {
            let rig = if i == 5 { 2 } else { i as u8 }; // Song 5 shares Rig 2 with Song 2!
            let mut s = Song::new(format!("Song {}", i + 1), vec![Part::new("Main", vec![PartRig::new(rig)])]);
            if i < 3 {
                s.set = Some(set1);
            }
            sl.songs.push(s);
        }
        sl.load_ahead = Some(1);
        sl.select(4, 0); // Window = [4, 5]: rigs 4 and 2 + show rig 0 (NOT set1 rig 1!).
        assert_eq!(sl.window_songs(), vec![4, 5]);
        assert_eq!(sl.window_rigs(&g), Some(vec![0, 2, 4]));
        // Priority order: Song 4's nodes (0, 4, master_fx) first, then Song 5's (2).
        let wnodes = sl.window_plugin_nodes(&g).unwrap();
        assert!(wnodes.contains(&synths[0]), "show rig stays hosted");
        assert!(wnodes.contains(&synths[4]), "active song rig hosted");
        assert!(wnodes.contains(&synths[2]), "next song rig hosted");
        assert!(wnodes.contains(&master_fx), "master insert stays hosted even when rig 0 is song-level");
        assert!(!wnodes.contains(&synths[1]), "out-of-window set rig is unloaded");
        assert!(!wnodes.contains(&synths[3]), "out-of-window song rig is unloaded");
        // Current song (4) comes before next song (5).
        let pos4 = wnodes.iter().position(|&id| id == synths[4]).unwrap();
        let pos2 = wnodes.iter().position(|&id| id == synths[2]).unwrap();
        assert!(pos4 < pos2, "active song's plugin nodes load before lookahead songs");

        // Song readiness: when [0, 4, master_fx] are loaded and [2] is loading:
        let loaded = [synths[0], synths[4], master_fx];
        let loading = [synths[2]];
        assert_eq!(sl.song_load_state(4, &g, &loaded, &loading), SongLoadState::Ready);
        assert_eq!(sl.song_load_state(5, &g, &loaded, &loading), SongLoadState::Loading);
        assert_eq!(sl.song_load_state(3, &g, &loaded, &loading), SongLoadState::Unloaded);

        // Serialization round-trip & normalize/validate.
        let json = serde_json::to_string(&sl).unwrap();
        assert!(json.contains("\"load_ahead\":1"), "{json}");
        let mut back: Setlist = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sl);
        back.load_ahead = Some(99);
        assert!(back.validate(&g).is_err());
        back.normalize();
        assert_eq!(back.load_ahead, Some(MAX_LOAD_AHEAD));
        back.validate(&g).unwrap();
    }

    #[test]
    fn shared_instrument_across_window_stays_ready_and_never_unloads() {
        let (g, synths) = multi_rig_graph(2);
        let mut sl = Setlist::default();
        for i in 0..10 {
            // Every song shares Rig 0; only Song 9 also uses Rig 1.
            let mut rigs = vec![PartRig::new(0)];
            if i == 9 {
                rigs.push(PartRig::new(1));
            }
            sl.songs.push(Song::new(format!("Song {}", i + 1), vec![Part::new("Main", rigs)]));
        }
        sl.load_ahead = Some(2);
        sl.active = Some(SetlistPos { song: 0, part: 0 });
        assert_eq!(sl.window_rigs(&g), Some(vec![0]));
        let loaded = [synths[0]];
        assert_eq!(sl.song_load_state(0, &g, &loaded, &[]), SongLoadState::Ready);
        assert_eq!(sl.song_load_state(5, &g, &loaded, &[]), SongLoadState::Ready, "shared piano is already hosted");
        assert_eq!(sl.song_load_state(9, &g, &loaded, &[]), SongLoadState::Unloaded, "song 9 needs unloaded rig 1");
    }
}
