//! Macros and rig morphs in the app (#289, #214): the macro's knob on its
//! home unit's face, the on-screen mirror of every target a macro moves, the
//! morph's two stored states, and the agent tools. The engine applies the
//! values (`Command::SetMacros` / `SetMacro`, or the controller bound with
//! `CcTarget::Macro`); everything here keeps the drawn knobs in step.

use super::*;
use crate::macros::{self, Curve, Macro, MacroTarget};

/// Which stored state of a morph an edit writes (#214).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MorphState {
    A,
    B,
}

impl DemoApp {
    /// Send the engine the macro table when a macro changed, and the value
    /// of a macro knob the hand moved.
    pub(super) fn publish_macros(&mut self) {
        let moved: Vec<(u32, f32)> = self.units.iter()
            .flat_map(|u| u.knobs.iter())
            .filter_map(|k| {
                let id = k.macro_id?;
                let m = self.macros.iter().find(|m| m.id == id)?;
                ((k.value - m.value).abs() > 1e-6).then_some((id, k.value))
            })
            .collect();
        for (id, v) in moved {
            self.set_macro_value(id, v as f64);
        }
        let Some(bus) = &self.bus else { return };
        let mut b = bus.lock().unwrap_or_else(|e| e.into_inner());
        self.macros_sent.publish(&self.macros, |cmd| b.send(cmd));
    }

    /// Move macro `id` to `v` from the screen or a tool: the targets follow
    /// on screen and the engine gets the value.
    pub(super) fn set_macro_value(&mut self, id: u32, v: f64) {
        self.mirror_macro(id, v, false);
        if let Some(bus) = &self.bus {
            let _ = bus.lock().map(|mut b| b.send(Command::SetMacro { id, value: v }));
        }
    }

    /// Show macro `id` at `v`: its own knob and every target's drawn knob
    /// (`set_immediate`, so the face draws it at once, rule 109). The engine
    /// applied or applies the same values; `resend` sends the mixer targets
    /// too (the controller that completed a learn, rule 51).
    pub(super) fn mirror_macro(&mut self, id: u32, v: f64, resend: bool) {
        let v = v.clamp(0.0, 1.0);
        let Some(m) = self.macros.iter_mut().find(|m| m.id == id) else { return };
        m.value = v as f32;
        let targets: Vec<MacroTarget> = m.targets.iter().filter(|t| t.moves()).copied().collect();
        let hold_until = self.ui_time + READBACK_HOLD_SECS;
        for u in &mut self.units {
            for k in u.knobs.iter_mut().filter(|k| k.macro_id == Some(id)) {
                k.set_immediate(v as f32);
            }
        }
        for t in targets {
            let norm = t.value(v);
            if t.target == CcTarget::Mix {
                if let Some(ctl) = MixControl::from_param(t.param) {
                    self.apply_mix_cc(t.node, ctl, norm, resend);
                }
                continue;
            }
            let norm = norm as f32;
            for u in self.units.iter_mut().filter(|u| u.node_id == t.node) {
                let is_clap = matches!(u.node_kind, UnitKind::Clap);
                for k in &mut u.knobs {
                    if k.macro_id.is_some() { continue; }
                    let by_param = t.target == CcTarget::Knob || !is_clap;
                    if by_param && k.param == Some(t.param) {
                        k.set_immediate(norm);
                        k.sent = Some(k.value);
                    } else if t.target == CcTarget::PluginParam {
                        if let Some(pin) = k.clap.as_mut().filter(|p| p.id == t.param) {
                            pin.hold_until = hold_until;
                            pin.sent = norm;
                            k.set_immediate(norm);
                        }
                    }
                }
            }
        }
    }

    /// One face knob per macro, on its home unit (and none left over for a
    /// macro that is gone).
    pub(super) fn sync_macro_knobs(&mut self) {
        let macros = self.macros.clone();
        for u in &mut self.units {
            u.knobs.retain(|k| k.macro_id.is_none_or(|id| macros.iter().any(|m| m.id == id && m.home == u.node_id)));
            for m in macros.iter().filter(|m| m.home == u.node_id) {
                let label: String = m.name.to_uppercase().chars().take(8).collect();
                match u.knobs.iter_mut().find(|k| k.macro_id == Some(m.id)) {
                    Some(k) => {
                        if k.label != label { k.label = label; k.label_g = None; }
                    }
                    None => {
                        let mut k = knob("MACRO", 0.0, None);
                        k.label = label;
                        k.macro_id = Some(m.id);
                        k.set_immediate(m.value);
                        u.knobs.push(k);
                    }
                }
            }
        }
    }

    fn macro_home_rig(&self, home: u32) -> Option<u8> {
        self.units.iter().find(|u| u.node_id == home).map(|u| u.rig)
    }

    /// The morph macro of rig `rig`, if one was stored.
    pub(super) fn morph_of(&self, rig: u8) -> Option<usize> {
        self.macros.iter().position(|m| m.morph && self.macro_home_rig(m.home) == Some(rig))
    }

    /// Store the rig's knobs as state A or B of its morph (#214). Every
    /// knob of the rig's units takes part (rack knobs and pinned plugin
    /// parameters); a stepped knob switches at the middle. Returns how many
    /// parameters the morph holds.
    pub(super) fn store_morph(&mut self, rig: u8, state: MorphState) -> Result<usize, String> {
        let home = self.units.iter().find(|u| u.rig == rig && matches!(u.node_kind, UnitKind::Master))
            .map(|u| u.node_id)
            .ok_or_else(|| format!("rig {rig} has no Master out unit"))?;
        let mut snap: Vec<(u32, u32, CcTarget, f32, bool)> = Vec::new();
        for u in self.units.iter().filter(|u| u.rig == rig) {
            for k in u.knobs.iter().filter(|k| k.macro_id.is_none()) {
                if let Some(pin) = k.clap {
                    snap.push((u.node_id, pin.id, CcTarget::PluginParam, k.value, k.steps >= 2));
                } else if let Some(p) = k.param {
                    snap.push((u.node_id, p, CcTarget::Knob, k.value, k.steps >= 2));
                }
            }
        }
        let idx = match self.morph_of(rig) {
            Some(i) => i,
            None => {
                let id = macros::free_id(&self.macros);
                self.macros.push(Macro { id, name: "Morph".into(), home, value: 0.0, morph: true, targets: Vec::new() });
                self.macros.len() - 1
            }
        };
        let m = &mut self.macros[idx];
        for (node, param, target, value, stepped) in snap {
            let mut t = m.targets.iter().find(|t| t.same_dest(node, param, target)).copied()
                .unwrap_or(MacroTarget { min: value, max: value, ..MacroTarget::new(node, param, target) });
            match state {
                MorphState::A => t.min = value,
                MorphState::B => t.max = value,
            }
            t.curve = if stepped { Curve::Switch } else { Curve::Linear };
            m.set_target(t);
        }
        let n = m.targets.len();
        self.sync_macro_knobs();
        Ok(n)
    }

    fn macro_args_target(&self, args: &serde_json::Value, m: Option<&Macro>) -> Result<Option<MacroTarget>, String> {
        let has_dest = ["knob", "mix", "param"].iter().any(|k| args.get(*k).is_some());
        if !has_dest {
            return Ok(None);
        }
        let node = args.get("target_node").and_then(|v| v.as_u64()).map(|n| n as u32)
            .ok_or("a target needs 'target_node' (the unit whose parameter it is)")?;
        if !self.units.iter().any(|u| u.node_id == node) {
            return Err(format!("no unit with node id {node}"));
        }
        let mut target = Self::parse_cc_target(args.get("target").and_then(|v| v.as_str()))?;
        if matches!(target, CcTarget::MidiCc | CcTarget::ChannelPressure | CcTarget::Macro) {
            return Err("a macro drives parameters ('plugin', 'knob' or 'mix'), not MIDI messages".into());
        }
        let param = self.tool_cc_param(node, args, &mut target)?.ok_or("a target needs 'param', 'knob' or 'mix'")?;
        let mut t = m.and_then(|m| m.targets.iter().find(|t| t.same_dest(node, param, target)).copied())
            .unwrap_or_else(|| MacroTarget::new(node, param, target));
        let unit = |key: &str| match args.get(key) {
            None => Ok(None),
            Some(v) => v.as_f64().filter(|x| (0.0..=1.0).contains(x)).map(|x| Some(x as f32))
                .ok_or(format!("'{key}' must be a number 0..1")),
        };
        if let Some(v) = unit("min")? { t.min = v; }
        if let Some(v) = unit("max")? { t.max = v; }
        if args.get("curve").is_some() {
            t.curve = Curve::parse(args.get("curve").and_then(|v| v.as_str()))?;
        }
        if let Some(b) = args.get("invert").and_then(|v| v.as_bool()) { t.invert = b; }
        Ok(Some(t))
    }

    pub(super) fn exec_macro_tool(&mut self, name: &str, args: &serde_json::Value, origin: AgentOrigin, now: f32) -> String {
        match name {
            "list_macros" => serde_json::json!({
                "macros": self.macros.iter().map(Macro::json).collect::<Vec<_>>(),
            }).to_string(),
            "set_macro" => {
                let existing = match args.get("macro") {
                    Some(k) => match macros::find(&self.macros, k) {
                        Ok(m) => Some(m.id),
                        Err(e) => return format!("error: {e}"),
                    },
                    None => None,
                };
                let idx = match existing {
                    Some(id) => self.macros.iter().position(|m| m.id == id).unwrap_or(0),
                    None => {
                        let Some(home) = args.get("node").and_then(|v| v.as_u64()).map(|n| n as u32) else {
                            return "error: a new macro needs 'node': the unit whose face shows its knob".into();
                        };
                        if !self.units.iter().any(|u| u.node_id == home) {
                            return format!("error: no unit with node id {home}");
                        }
                        if self.macros.len() >= macros::MAX_MACROS {
                            return format!("error: at most {} macros", macros::MAX_MACROS);
                        }
                        let id = macros::free_id(&self.macros);
                        let name = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
                            .map_or_else(|| format!("Macro {id}"), str::to_string);
                        self.macros.push(Macro { id, name, home, value: 0.0, morph: false, targets: Vec::new() });
                        self.macros.len() - 1
                    }
                };
                let target = match self.macro_args_target(args, Some(&self.macros[idx])) {
                    Ok(t) => t,
                    Err(e) => {
                        if existing.is_none() { self.macros.remove(idx); }
                        return format!("error: {e}");
                    }
                };
                let id = self.macros[idx].id;
                if existing.is_some() {
                    if let Some(n) = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
                        self.macros[idx].name = n.to_string();
                    }
                }
                if let Some(t) = target {
                    if args.get("remove_target").and_then(|v| v.as_bool()) == Some(true) {
                        self.macros[idx].targets.retain(|x| !x.same_dest(t.node, t.param, t.target));
                    } else {
                        self.macros[idx].set_target(t);
                    }
                }
                self.sync_macro_knobs();
                if let Some(v) = args.get("value").and_then(|v| v.as_f64()) {
                    self.set_macro_value(id, v);
                }
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                let home = self.macros[idx].home;
                self.agent_activity.record(origin, format!("Macro {} → {}", self.macros[idx].name, self.unit_display_name(home)), vec![home], now);
                self.macros[idx].json().to_string()
            }
            "delete_macro" => {
                let Some(k) = args.get("macro") else { return "error: delete_macro needs 'macro'".into() };
                let id = match macros::find(&self.macros, k) {
                    Ok(m) => m.id,
                    Err(e) => return format!("error: {e}"),
                };
                let homes: Vec<(SourceId, u32)> = self.rule_edits.iter()
                    .filter(|(_, e)| e.cc_map.iter().any(|m| m.target == CcTarget::Macro && m.param == id))
                    .map(|(k, _)| *k).collect();
                for (src, node) in homes {
                    self.clear_knob_bindings(node, id, CcTarget::Macro);
                    let _ = src;
                }
                self.macros.retain(|m| m.id != id);
                self.sync_macro_knobs();
                serde_json::json!({"deleted": id}).to_string()
            }
            "set_morph" => {
                let Some(rig) = args.get("rig").and_then(|v| v.as_u64()).map(|r| r as usize) else {
                    return "error: set_morph needs 'rig' (0-based)".into();
                };
                if rig >= self.rig_count() { return format!("error: rig {rig} out of range ({} rigs)", self.rig_count()); }
                let rig = rig as u8;
                let mut stored = None;
                match args.get("store").and_then(|v| v.as_str()).map(str::to_lowercase).as_deref() {
                    None => {}
                    Some("a") => stored = Some(self.store_morph(rig, MorphState::A)),
                    Some("b") => stored = Some(self.store_morph(rig, MorphState::B)),
                    Some(o) => return format!("error: 'store' is 'a' or 'b', not '{o}'"),
                }
                if let Some(Err(e)) = stored { return format!("error: {e}"); }
                if let Some(v) = args.get("value").and_then(|v| v.as_f64()) {
                    let Some(i) = self.morph_of(rig) else { return "error: this rig has no stored morph: store state 'a' and 'b' first".into() };
                    let id = self.macros[i].id;
                    self.set_macro_value(id, v);
                }
                if self.graph_pub.is_some() { self.pending_agent_origin = Some((origin, now + 1.0)); }
                match self.morph_of(rig) {
                    Some(i) => self.macros[i].json().to_string(),
                    None => "error: this rig has no stored morph yet: pass store 'a' or 'b'".into(),
                }
            }
            _ => format!("error: unknown macro tool {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> DemoApp {
        DemoApp::new_with_sink(None)
    }

    fn json(s: String) -> serde_json::Value {
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("not JSON ({e}): {s}"))
    }

    fn unit_of(app: &DemoApp, f: impl Fn(&Unit) -> bool) -> (usize, u32) {
        let i = app.units.iter().position(f).expect("the demo rack has this unit");
        (i, app.units[i].node_id)
    }

    fn knob_val(app: &DemoApp, node: u32, label: &str) -> (f32, f32) {
        let u = app.units.iter().find(|u| u.node_id == node).unwrap();
        let k = &u.knobs[u.knob_index(label).unwrap_or_else(|| panic!("no knob {label}"))];
        (k.value, k.display_value)
    }

    #[test]
    fn one_macro_moves_three_drawn_knobs_over_their_ranges_and_survives_reload_and_undo() {
        let mut app = app();
        let (_, delay) = unit_of(&app, |u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay)));
        let (_, master) = unit_of(&app, |u| matches!(u.node_kind, UnitKind::Master));
        let made = json(app.exec_tool("set_macro", &serde_json::json!({"node": master, "name": "Swell"})));
        let id = made["id"].as_u64().unwrap();
        for (knob, min, max, invert) in [("TIME", 0.2, 0.8, false), ("MIX", 0.1, 0.9, true)] {
            let r = app.exec_tool("set_macro", &serde_json::json!({
                "macro": "swell", "target_node": delay, "knob": knob, "min": min, "max": max, "invert": invert,
            }));
            assert!(!r.starts_with("error"), "{knob}: {r}");
        }
        // The third: the rig fader, from full down to a quarter.
        let r = app.exec_tool("set_macro", &serde_json::json!({
            "macro": "swell", "target_node": master, "mix": "vol", "min": 1.0, "max": 0.25,
        }));
        assert!(!r.starts_with("error"), "{r}");
        let listed = json(app.exec_tool("list_macros", &serde_json::json!({})));
        assert_eq!(listed["macros"][0]["targets"].as_array().unwrap().len(), 3, "{listed}");
        app.exec_tool("set_macro", &serde_json::json!({"macro": id, "value": 1.0}));
        let (t, t_disp) = knob_val(&app, delay, "TIME");
        let (m, m_disp) = knob_val(&app, delay, "MIX");
        assert!((t - 0.8).abs() < 1e-5 && (t_disp - 0.8).abs() < 1e-5, "TIME drawn at its max: {t} {t_disp}");
        assert!((m - 0.1).abs() < 1e-5 && (m_disp - 0.1).abs() < 1e-5, "the inverted MIX drawn at its min: {m} {m_disp}");
        assert!((knob_val(&app, master, "SWELL").1 - 1.0).abs() < 1e-5, "the macro's own knob follows");
        assert!((app.rig_mix[0].vol - 0.25).abs() < 1e-5, "the mixer target follows too: {}", app.rig_mix[0].vol);

        // Save and reload through the session, and undo a delete.
        let s = app.capture_session("m").unwrap();
        assert_eq!(s.macros.len(), 1);
        let mut other = DemoApp::new_with_sink(None);
        other.apply_session(&s).unwrap();
        assert_eq!(other.macros, app.macros, "the macro is in the saved session");
        assert!(other.units.iter().any(|u| u.knob_index("SWELL").is_some()), "and its knob is back on the face");
        app.push_user_undo();
        app.exec_tool("delete_macro", &serde_json::json!({"macro": id}));
        assert!(app.macros.is_empty() && app.units.iter().all(|u| u.knob_index("SWELL").is_none()));
        app.undo_agent(true).unwrap();
        assert_eq!(app.macros.len(), 1, "undo brings the macro back");
        assert_eq!(app.macros[0].targets.len(), 3);
    }

    #[test]
    fn a_morph_from_a_to_b_moves_every_stored_knob_monotonically_and_the_drawn_knobs_follow() {
        let mut app = app();
        let (di, delay) = unit_of(&app, |u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay)));
        let rig = app.units[di].rig as usize;
        let set = |app: &mut DemoApp, label: &str, v: f32| {
            let i = app.units.iter().position(|u| u.node_id == delay).unwrap();
            let k = app.units[i].knob_index(label).unwrap();
            app.set_unit_knob(i, k, v);
        };
        set(&mut app, "TIME", 0.2);
        set(&mut app, "MIX", 0.8);
        let a = json(app.exec_tool("set_morph", &serde_json::json!({"rig": rig, "store": "a"})));
        assert_eq!(a["morph"], true);
        set(&mut app, "TIME", 0.9);
        set(&mut app, "MIX", 0.1);
        app.exec_tool("set_morph", &serde_json::json!({"rig": rig, "store": "b"}));
        let mut last = (f32::MIN, f32::MAX);
        for step in 0..=20 {
            app.exec_tool("set_morph", &serde_json::json!({"rig": rig, "value": step as f64 / 20.0}));
            let (time, time_drawn) = knob_val(&app, delay, "TIME");
            let (fdbk, fdbk_drawn) = knob_val(&app, delay, "MIX");
            assert_eq!((time, fdbk), (time_drawn, fdbk_drawn), "the face draws what the morph set (display_value)");
            assert!(time >= last.0 && fdbk <= last.1, "step {step}: TIME {time}, FDBK {fdbk}");
            last = (time, fdbk);
        }
        assert!((last.0 - 0.9).abs() < 1e-5 && (last.1 - 0.1).abs() < 1e-5, "B at the end: {last:?}");
        app.exec_tool("set_morph", &serde_json::json!({"rig": rig, "value": 0.0}));
        assert!((knob_val(&app, delay, "TIME").1 - 0.2).abs() < 1e-5, "A at the start");
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat macros_shots -- --ignored"]
    fn macros_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let size = Vec2::new(1280.0, 800.0);
        let mut before = app();
        crate::ui::offline_shots::shot(&mut before, size, &dir.join("before-front.png"));

        let mut a = app();
        let (_, delay) = unit_of(&a, |u| matches!(u.node_kind, UnitKind::Dsp(DspKind::Delay)));
        let (_, master) = unit_of(&a, |u| matches!(u.node_kind, UnitKind::Master));
        a.exec_tool("set_macro", &serde_json::json!({"node": master, "name": "Swell"}));
        a.exec_tool("set_macro", &serde_json::json!({"macro": "swell", "target_node": delay, "knob": "TIME", "min": 0.2, "max": 0.8}));
        a.exec_tool("set_macro", &serde_json::json!({"macro": "swell", "target_node": delay, "knob": "MIX", "min": 0.1, "max": 0.9, "invert": true}));
        a.exec_tool("set_macro", &serde_json::json!({"macro": "swell", "value": 0.7}));
        crate::ui::offline_shots::shot(&mut a, size, &dir.join("after-macro-front.png"));

        let mut m = app();
        let surge = m.units.iter().find(|u| matches!(u.node_kind, UnitKind::Clap)).unwrap().node_id;
        m.unit_menu = Some(UnitMenu { node: surge, pos: Pos2::new(430.0, 210.0), side: Side::Front });
        crate::ui::offline_shots::shot(&mut m, size, &dir.join("after-unit-menu.png"));
    }
}
