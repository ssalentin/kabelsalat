//! Send scenes (#415): what a setlist part sets on the send effects, for a
//! singer or guitarist whose input rig runs through the whole show.
//!
//! A part's [`SendScene`] names send levels into aux buses, the bypass of
//! the effects in the buses' chains and their knobs (the bus fader, a
//! reverb's size, a delay's time). A switch to the part recalls them whatever
//! level the sending rig lives at: a show-level vocal rig keeps its fader and
//! its own effects (#273) and still goes from a dry verse to a wet chorus.
//! The engine ramps each send level over `gain_ramp::RAMP_S`, and a bus keeps
//! running with nothing sent into it, so the tail of the last part rings on.
//!
//! A scene is a full picture of the sends the scenes of the setlist know: a
//! send another part's scene names and this one does not is closed
//! ([`Setlist::recalled_scene`]). A part without a scene leaves the sends
//! alone, and a send no scene names is never touched.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{is_audio_effect, FxBypass, ParamValue, Part, Setlist};
use crate::control::{AuxSend, Command, Graph, NodeId, SendSource, MAX_RIGS};

/// One send level a part recalls: `level` (0..=1) of `from` into aux bus `bus`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartSend {
    pub from: SendSource,
    pub bus: NodeId,
    pub level: f32,
}

/// What a part sets on the send effects (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SendScene {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<PartSend>,
    /// Bypass of effects in the buses' chains.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fx: Vec<FxBypass>,
    /// Knobs of the buses and of the effects in their chains.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamValue>,
}

fn source_key(s: SendSource) -> (u8, u32) {
    match s {
        SendSource::Rig(r) => (0, r as u32),
        SendSource::Node(n) => (1, n),
    }
}

/// Is `node` a send effect: an aux bus or a unit of a bus's chain?
pub fn is_send_fx(graph: &Graph, node: NodeId) -> bool {
    graph.is_bus(node) || graph.after_bus(node)
}

impl SendScene {
    /// Upsert one send level (one entry per source and bus).
    pub fn set_send(&mut self, from: SendSource, bus: NodeId, level: f32) {
        let level = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
        match self.sends.iter_mut().find(|s| s.from == from && s.bus == bus) {
            Some(s) => s.level = level,
            None => self.sends.push(PartSend { from, bus, level }),
        }
        self.sort();
    }

    /// The level this scene gives one send, if it names it.
    pub fn level(&self, from: SendSource, bus: NodeId) -> Option<f32> {
        self.sends.iter().find(|s| s.from == from && s.bus == bus).map(|s| s.level)
    }

    /// One entry per send, effect and knob, in a stable order.
    pub fn sort(&mut self) {
        self.sends.sort_by_key(|s| (s.bus, source_key(s.from)));
        self.sends.dedup_by_key(|s| (s.bus, source_key(s.from)));
        self.fx.sort_by_key(|f| f.node);
        self.fx.dedup_by_key(|f| f.node);
        self.params.sort_by_key(|p| (p.node, p.param));
        self.params.dedup_by_key(|p| (p.node, p.param));
    }

    /// The scene the mixer shows right now: every open send, and from
    /// `fx` and `params` what belongs to a bus or its chain.
    pub fn capture(graph: &Graph, sends: &[AuxSend], fx: &[FxBypass], params: &[ParamValue]) -> Self {
        let mut scene = SendScene {
            sends: sends
                .iter()
                .filter(|s| graph.is_bus(s.bus) && s.level > 0.0)
                .map(|s| PartSend { from: s.from, bus: s.bus, level: s.level })
                .collect(),
            fx: fx.iter().filter(|f| graph.after_bus(f.node)).copied().collect(),
            params: params.iter().filter(|p| is_send_fx(graph, p.node)).copied().collect(),
        };
        scene.sort();
        scene
    }

    /// The commands that put this scene into the engine: what a frontend
    /// without mirrors of its own (headless MCP, a self-test) sends.
    pub fn commands(&self) -> Vec<Command> {
        let mut out: Vec<Command> = self.sends.iter().map(|s| Command::SetSend { from: s.from, bus: s.bus, level: Some(s.level), pre: None }).collect();
        out.extend(self.fx.iter().map(|f| Command::SetStripBypass { node: f.node, bypass: f.bypass }));
        out.extend(self.params.iter().map(|p| Command::SetParam { node: p.node, param: p.param, value: p.value }));
        out
    }

    /// Drop what `graph` no longer has, clamp the rest.
    pub fn prune(&mut self, graph: &Graph) {
        self.sends.retain(|s| {
            graph.is_bus(s.bus)
                && match s.from {
                    SendSource::Rig(r) => graph.nodes.iter().any(|n| n.rig == r),
                    SendSource::Node(n) => graph.node(n).is_some() && !is_send_fx(graph, n),
                }
        });
        self.fx.retain(|f| graph.after_bus(f.node));
        self.params.retain(|p| is_send_fx(graph, p.node));
    }

    /// Rig indices and node ids moved by a rig add or delete (rule 44).
    pub fn remap(&self, rig_map: &impl Fn(u8) -> Option<u8>, node_map: &impl Fn(NodeId) -> Option<NodeId>) -> Self {
        let mut out = SendScene {
            sends: self
                .sends
                .iter()
                .filter_map(|s| {
                    let from = match s.from {
                        SendSource::Rig(r) => SendSource::Rig(rig_map(r)?),
                        SendSource::Node(n) => SendSource::Node(node_map(n)?),
                    };
                    Some(PartSend { from, bus: node_map(s.bus)?, level: s.level })
                })
                .collect(),
            fx: self.fx.iter().filter_map(|f| Some(FxBypass { node: node_map(f.node)?, bypass: f.bypass })).collect(),
            params: self.params.iter().filter_map(|p| Some(ParamValue { node: node_map(p.node)?, ..*p })).collect(),
        };
        out.sort();
        out
    }

    pub fn normalize(&mut self) {
        for s in &mut self.sends {
            s.level = if s.level.is_finite() { s.level.clamp(0.0, 1.0) } else { 0.0 };
        }
        self.params.retain(|p| p.value.is_finite());
        self.sort();
    }

    /// A node this scene names that `graph` does not have.
    pub fn missing_node(&self, graph: &Graph) -> Option<NodeId> {
        let from = self.sends.iter().filter_map(|s| match s.from {
            SendSource::Node(n) => Some(n),
            SendSource::Rig(_) => None,
        });
        self.sends
            .iter()
            .map(|s| s.bus)
            .chain(from)
            .chain(self.fx.iter().map(|f| f.node))
            .chain(self.params.iter().map(|p| p.node))
            .find(|&n| graph.node(n).is_none())
    }

    /// What is wrong with this scene on `graph`, in plain language.
    pub fn check(&self, graph: &Graph) -> Result<(), String> {
        for s in &self.sends {
            if !graph.is_bus(s.bus) {
                return Err(format!("node {} is not an aux bus (get_sends lists them)", s.bus));
            }
            match s.from {
                SendSource::Rig(r) if r as usize >= MAX_RIGS || !graph.nodes.iter().any(|n| n.rig == r) => {
                    return Err(format!("no rig {r} in the patch"));
                }
                SendSource::Node(n) if graph.node(n).is_none() => return Err(format!("node {n} is not in the patch")),
                SendSource::Node(n) if is_send_fx(graph, n) => {
                    return Err(format!("node {n} is a bus or one of a bus's inserts: it cannot send into a bus"));
                }
                _ => {}
            }
            if !s.level.is_finite() || !(0.0..=1.0).contains(&s.level) {
                return Err("a send level must be 0..1".to_string());
            }
        }
        for f in &self.fx {
            let ok = graph.after_bus(f.node) && graph.node(f.node).is_some_and(|n| is_audio_effect(graph, n));
            if !ok {
                return Err(format!("node {} is not an effect in an aux bus's chain", f.node));
            }
        }
        for p in &self.params {
            if !is_send_fx(graph, p.node) {
                return Err(format!("node {} is not an aux bus or an effect in a bus's chain", p.node));
            }
            if !p.value.is_finite() {
                return Err("a knob value must be a finite number".to_string());
            }
        }
        Ok(())
    }

    /// A scene as a tool gives it: `{sends:[{rig|node, bus, level}],
    /// fx:[{node, bypass}], params:[{node, param, value}]}`. Each list may be
    /// left out. A value of the wrong type is refused, never defaulted
    /// (rule 77).
    pub fn from_json(v: &Value, graph: &Graph) -> Result<Self, String> {
        if !v.is_object() {
            return Err(format!("a send scene is an object with sends, fx and params, got {v}"));
        }
        let list = |key: &str| -> Result<&[Value], String> {
            match v.get(key) {
                None | Some(Value::Null) => Ok(&[]),
                Some(Value::Array(a)) => Ok(a),
                Some(other) => Err(format!("{key} must be a list, got {other}")),
            }
        };
        let id = |e: &Value, key: &str| -> Result<Option<u32>, String> {
            match e.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(x) => x.as_u64().filter(|n| *n <= u32::MAX as u64).map(|n| Some(n as u32)).ok_or_else(|| format!("{key} must be a whole number, got {x}")),
            }
        };
        let mut scene = SendScene::default();
        for e in list("sends")? {
            let from = match (id(e, "rig")?, id(e, "node")?) {
                (Some(r), None) if (r as usize) < MAX_RIGS => SendSource::Rig(r as u8),
                (Some(r), None) => return Err(format!("no rig {r}")),
                (None, Some(n)) => SendSource::Node(n),
                _ => return Err("a send names its source: 'rig' (0-based rig index) or 'node' (one source's node id), not both".to_string()),
            };
            let bus = id(e, "bus")?.ok_or("a send needs 'bus' (the aux bus's node id; get_sends lists them)")?;
            let level = e.get("level").and_then(Value::as_f64).ok_or("a send needs 'level' (0..1)")?;
            if !(0.0..=1.0).contains(&level) {
                return Err(format!("a send level must be 0..1, got {level}"));
            }
            scene.set_send(from, bus, level as f32);
        }
        for e in list("fx")? {
            let node = id(e, "node")?.ok_or("an fx entry needs 'node'")?;
            let bypass = e.get("bypass").and_then(Value::as_bool).ok_or("an fx entry needs 'bypass' (true or false)")?;
            scene.fx.push(FxBypass { node, bypass });
        }
        for e in list("params")? {
            let node = id(e, "node")?.ok_or("a params entry needs 'node'")?;
            let param = id(e, "param")?.ok_or("a params entry needs 'param' (the knob's param id)")?;
            let value = e.get("value").and_then(Value::as_f64).ok_or("a params entry needs 'value'")?;
            scene.params.push(ParamValue { node, param, value });
        }
        scene.sort();
        scene.check(graph)?;
        Ok(scene)
    }

    pub fn to_json(&self, name: &impl Fn(NodeId) -> String) -> Value {
        let sends: Vec<Value> = self
            .sends
            .iter()
            .map(|s| {
                let mut e = json!({ "bus": s.bus, "bus_name": name(s.bus), "level": s.level });
                match s.from {
                    SendSource::Rig(r) => e["rig"] = json!(r),
                    SendSource::Node(n) => {
                        e["node"] = json!(n);
                        e["node_name"] = json!(name(n));
                    }
                }
                e
            })
            .collect();
        let fx: Vec<Value> = self.fx.iter().map(|f| json!({ "node": f.node, "name": name(f.node), "bypass": f.bypass })).collect();
        let params: Vec<Value> = self.params.iter().map(|p| json!({ "node": p.node, "name": name(p.node), "param": p.param, "value": p.value })).collect();
        json!({ "sends": sends, "fx": fx, "params": params })
    }

    /// One line for the part's row: "Hall 50 % · Echo 30 %", or "dry".
    pub fn label(&self, name: &impl Fn(NodeId) -> String) -> String {
        let mut buses: Vec<NodeId> = self.sends.iter().filter(|s| s.level > 0.0).map(|s| s.bus).collect();
        buses.dedup();
        if buses.is_empty() {
            return "dry".to_string();
        }
        buses
            .iter()
            .map(|&b| {
                let top = self.sends.iter().filter(|s| s.bus == b).map(|s| s.level).fold(0.0, f32::max);
                format!("{} {:.0} %", name(b), top * 100.0)
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// A part's scene as `set_setlist_song` gives it (`scene`): `None` when the
/// part names none, so a part without one leaves the sends alone.
pub fn part_scene(part: &Value, graph: &Graph) -> Result<Option<SendScene>, String> {
    match part.get("scene") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => SendScene::from_json(v, graph).map(Some),
    }
}

impl Setlist {
    /// What a switch to the active part sets on the send effects: its scene,
    /// with every send another part's scene names and this one does not
    /// closed. Empty when the part has no scene. Only what `graph` still has.
    pub fn recalled_scene(&self, graph: &Graph) -> SendScene {
        let Some(mut scene) = self.current().and_then(|(_, p, _)| p.scene.clone()) else { return SendScene::default() };
        let others = self.songs.iter().flat_map(|s| &s.parts).filter_map(|p| p.scene.as_ref()).flat_map(|s| &s.sends);
        for s in others {
            if scene.level(s.from, s.bus).is_none() {
                scene.sends.push(PartSend { from: s.from, bus: s.bus, level: 0.0 });
            }
        }
        scene.sort();
        scene.prune(graph);
        scene
    }

    /// The part a tool names by `song` and `part` (0-based), or the active one.
    fn scene_part(&mut self, args: &Value) -> Result<&mut Part, String> {
        let idx = |key: &str| -> Result<Option<usize>, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v.as_u64().map(|n| Some(n as usize)).ok_or_else(|| format!("{key} must be a 0-based index, got {v}")),
            }
        };
        let (song, part) = match (idx("song")?, idx("part")?) {
            (Some(s), Some(p)) => (s, p),
            (None, None) => self.active.map(|a| (a.song, a.part)).ok_or("no part is active: name the part with song and part (0-based; see get_setlist)")?,
            _ => return Err("name the part with both song and part (0-based), or leave both out for the active part".to_string()),
        };
        let n_songs = self.songs.len();
        let s = self.songs.get_mut(song).ok_or_else(|| format!("no song at index {song} (setlist has {n_songs})"))?;
        let n_parts = s.parts.len();
        s.parts.get_mut(part).ok_or_else(|| format!("song {song} has no part {part} (it has {n_parts})"))
    }

    /// The `set_part_sends` tool (#415), for the app and headless MCP alike.
    /// `capture: true` stores `now` (the mixer's current scene); `sends`,
    /// `fx` and `params` are merged on top of it or of the part's scene;
    /// `clear: true` removes the scene. Returns what it did.
    pub fn apply_part_sends(&mut self, graph: &Graph, args: &Value, now: impl FnOnce() -> SendScene) -> Result<String, String> {
        let flag = |key: &str| -> Result<bool, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(false),
                Some(v) => v.as_bool().ok_or_else(|| format!("{key} must be true or false, got {v}")),
            }
        };
        let (capture, clear) = (flag("capture")?, flag("clear")?);
        // A chat model gives each list as a JSON array string.
        let list = |key: &str| -> Result<Value, String> {
            match args.get(key) {
                Some(Value::String(text)) => serde_json::from_str(text).map_err(|e| format!("{key} must be a JSON array: {e}")),
                other => Ok(other.cloned().unwrap_or(Value::Null)),
            }
        };
        let given = SendScene::from_json(&json!({ "sends": list("sends")?, "fx": list("fx")?, "params": list("params")? }), graph)?;
        let named = !(given.sends.is_empty() && given.fx.is_empty() && given.params.is_empty());
        if clear && (capture || named) {
            return Err("clear removes the part's scene: do not combine it with capture, sends, fx or params".to_string());
        }
        if !clear && !capture && !named {
            return Err("nothing to do: give sends (and fx, params), capture: true, or clear: true".to_string());
        }
        let part = self.scene_part(args)?;
        if clear {
            part.scene = None;
            return Ok(format!("Part '{}' has no send scene now: a switch to it leaves the sends alone", part.name));
        }
        let mut scene = if capture { now() } else { part.scene.clone().unwrap_or_default() };
        for s in &given.sends {
            scene.set_send(s.from, s.bus, s.level);
        }
        for f in &given.fx {
            scene.fx.retain(|x| x.node != f.node);
            scene.fx.push(*f);
        }
        for p in &given.params {
            scene.params.retain(|x| !(x.node == p.node && x.param == p.param));
            scene.params.push(*p);
        }
        scene.sort();
        scene.check(graph)?;
        let name = part.name.clone();
        let what = format!("{} sends, {} effect bypasses, {} knobs", scene.sends.len(), scene.fx.len(), scene.params.len());
        part.scene = Some(scene);
        Ok(format!("Part '{name}' recalls {what} on a switch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{name_bytes, DspKind, NodeKind, PortKind, PortRef};
    use crate::setlist::{Song, SetlistPos};

    /// Rig 0: Audio In → Master, with a bus HALL → Reverb → Master.
    fn rack() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::default();
        g.reserve(16, 32);
        let mic = g.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::MonoL }, name_bytes("MIC"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let hall = g.add_node(NodeKind::Dsp(DspKind::Bus), name_bytes("HALL"));
        let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
        g.connect_with_id(1, PortRef { node: mic, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        g.connect_with_id(2, PortRef { node: hall, port: 0 }, PortRef { node: verb, port: 0 }, PortKind::Audio);
        g.connect_with_id(3, PortRef { node: verb, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        (g, mic, hall, verb)
    }

    fn two_parts() -> Setlist {
        let mut sl = Setlist::default();
        sl.songs.push(Song::new("Song", vec![Part::new("Verse", Vec::new()), Part::new("Chorus", Vec::new())]));
        sl.active = Some(SetlistPos { song: 0, part: 0 });
        sl
    }

    #[test]
    fn a_scene_closes_the_sends_only_another_parts_scene_names() {
        let (g, mic, hall, _) = rack();
        let mut sl = two_parts();
        // The verse was captured dry, the chorus with the hall open.
        sl.songs[0].parts[0].scene = Some(SendScene::default());
        let mut wet = SendScene::default();
        wet.set_send(SendSource::Node(mic), hall, 0.6);
        sl.songs[0].parts[1].scene = Some(wet);
        assert_eq!(sl.recalled_scene(&g).sends, vec![PartSend { from: SendSource::Node(mic), bus: hall, level: 0.0 }], "the dry verse closes the chorus's send");
        sl.select(0, 1);
        assert_eq!(sl.recalled_scene(&g).sends, vec![PartSend { from: SendSource::Node(mic), bus: hall, level: 0.6 }]);
        // A part without a scene leaves the sends alone.
        sl.songs[0].parts[0].scene = None;
        sl.select(0, 0);
        assert_eq!(sl.recalled_scene(&g), SendScene::default());
    }

    #[test]
    fn a_scene_goes_through_json_and_the_session_file() {
        let (g, mic, hall, verb) = rack();
        let v = json!({
            "sends": [{ "node": mic, "bus": hall, "level": 0.5 }, { "rig": 0, "bus": hall, "level": 0.25 }],
            "fx": [{ "node": verb, "bypass": true }],
            "params": [{ "node": verb, "param": 0, "value": 0.8 }, { "node": hall, "param": 0, "value": 0.7 }],
        });
        let scene = SendScene::from_json(&v, &g).unwrap();
        assert_eq!(scene.level(SendSource::Node(mic), hall), Some(0.5));
        assert_eq!(scene.level(SendSource::Rig(0), hall), Some(0.25));
        let name = |n: NodeId| g.node(n).map_or_else(String::new, |x| crate::control::name_str(&x.name).to_string());
        assert_eq!(SendScene::from_json(&scene.to_json(&name), &g).unwrap(), scene, "what get_setlist prints, the tool takes back");
        assert_eq!(scene.label(&name), "HALL 50 %");
        let mut part = Part::new("Chorus", Vec::new());
        part.scene = Some(scene.clone());
        let back: Part = serde_json::from_str(&serde_json::to_string(&part).unwrap()).unwrap();
        assert_eq!(back.scene, Some(scene));
        // A part saved before #415 has no scene, and one without writes none.
        let old: Part = serde_json::from_str(r#"{"name":"Verse"}"#).unwrap();
        assert_eq!(old.scene, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("scene"));
    }

    #[test]
    fn a_scene_refuses_what_is_no_send_effect() {
        let (g, mic, hall, verb) = rack();
        let bad = [
            json!({ "sends": [{ "node": mic, "bus": mic, "level": 0.5 }] }),
            json!({ "sends": [{ "node": verb, "bus": hall, "level": 0.5 }] }),
            json!({ "sends": [{ "node": mic, "bus": hall, "level": 1.5 }] }),
            json!({ "sends": [{ "node": mic, "bus": hall }] }),
            json!({ "sends": [{ "rig": 0, "node": mic, "bus": hall, "level": 0.5 }] }),
            json!({ "sends": [{ "rig": 3, "bus": hall, "level": 0.5 }] }),
            json!({ "fx": [{ "node": mic, "bypass": true }] }),
            json!({ "fx": [{ "node": verb, "bypass": "yes" }] }),
            json!({ "params": [{ "node": mic, "param": 0, "value": 0.5 }] }),
            json!({ "sends": "all" }),
        ];
        for v in bad {
            assert!(SendScene::from_json(&v, &g).is_err(), "{v} is refused");
        }
    }

    #[test]
    fn a_scene_follows_a_remap_and_drops_what_is_gone() {
        let (g, mic, hall, verb) = rack();
        let mut scene = SendScene::default();
        scene.set_send(SendSource::Node(mic), hall, 0.5);
        scene.set_send(SendSource::Rig(0), hall, 0.25);
        scene.fx.push(FxBypass { node: verb, bypass: true });
        scene.params.push(ParamValue { node: verb, param: 0, value: 0.8 });
        let moved = scene.remap(&|r| Some(r + 1), &|n| (n != verb).then_some(n + 10));
        assert_eq!(moved.sends, vec![
            PartSend { from: SendSource::Rig(1), bus: hall + 10, level: 0.25 },
            PartSend { from: SendSource::Node(mic + 10), bus: hall + 10, level: 0.5 },
        ]);
        assert!(moved.fx.is_empty() && moved.params.is_empty(), "a removed effect takes its entries along");
        // The bus is removed: nothing of the scene is left.
        let mut g2 = g.clone();
        g2.remove_node(hall);
        scene.prune(&g2);
        assert_eq!(scene, SendScene::default());
    }

    #[test]
    fn the_tool_sets_merges_captures_and_clears_a_parts_scene() {
        let (g, mic, hall, verb) = rack();
        let mut sl = two_parts();
        let none = SendScene::default;
        let msg = sl.apply_part_sends(&g, &json!({ "song": 0, "part": 1, "sends": [{ "node": mic, "bus": hall, "level": 0.6 }] }), none).unwrap();
        assert!(msg.contains("Chorus") && msg.contains("1 sends"), "{msg}");
        // The active part by default; a second call merges.
        sl.select(0, 1);
        sl.apply_part_sends(&g, &json!({ "fx": format!("[{{\"node\":{verb},\"bypass\":false}}]") }), none).unwrap();
        let scene = sl.songs[0].parts[1].scene.clone().unwrap();
        assert_eq!((scene.sends.len(), scene.fx.len()), (1, 1));
        // Capture takes the mixer's scene, and what the call names goes on top.
        let mixer = || SendScene { sends: vec![PartSend { from: SendSource::Rig(0), bus: hall, level: 0.3 }], ..SendScene::default() };
        sl.apply_part_sends(&g, &json!({ "song": 0, "part": 0, "capture": true, "params": [{ "node": hall, "param": 0, "value": 0.9 }] }), mixer).unwrap();
        let verse = sl.songs[0].parts[0].scene.clone().unwrap();
        assert_eq!(verse.level(SendSource::Rig(0), hall), Some(0.3));
        assert_eq!(verse.params.len(), 1);
        sl.apply_part_sends(&g, &json!({ "song": 0, "part": 0, "clear": true }), none).unwrap();
        assert_eq!(sl.songs[0].parts[0].scene, None);
        for bad in [json!({}), json!({ "song": 0, "capture": true }), json!({ "song": 4, "part": 0, "capture": true }), json!({ "clear": true, "capture": true }), json!({ "capture": "yes" })] {
            assert!(sl.apply_part_sends(&g, &bad, none).is_err(), "{bad} is refused");
        }
    }
}
