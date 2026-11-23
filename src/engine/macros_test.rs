//! Macros through the engine (#289, #214): one controller, read from its
//! raw MIDI bytes, moves several parameters over their own ranges.

use super::GraphEngine;
use crate::control::{command_bus, name_bytes, Command, CommandBus, DspKind, Graph, NodeId, NodeKind, PortKind, PortRef};
use crate::macros::{Curve, Macro, MacroTable, MacroTarget};
use crate::matrix::{CcTarget, MidiMsg, MixControl, SourceId};

const PEDAL: SourceId = SourceId::Hardware(0);

struct Rack {
    engine: GraphEngine,
    bus: CommandBus,
    synth: NodeId,
    filter: NodeId,
    delay: NodeId,
    master: NodeId,
}

/// Keys → synth → Filter → Delay → Master, no voice hosted.
fn rack() -> Rack {
    let mut g = Graph::default();
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let synth = g.add_node(NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") }, name_bytes("SURGE"));
    let filter = g.add_node(NodeKind::Dsp(DspKind::Filter), name_bytes("FILTER"));
    let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    assert!(g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi));
    let mut id = 2;
    for (a, b) in [(synth, filter), (filter, delay), (delay, master)] {
        assert!(g.connect_with_id(id, PortRef { node: a, port: 0 }, PortRef { node: b, port: 0 }, PortKind::Audio));
        id += 1;
    }
    let (bus, drain) = command_bus();
    let engine = GraphEngine::new(g, drain, 48000.0, vec![]);
    Rack { engine, bus, synth, filter, delay, master }
}

impl Rack {
    fn block(&mut self) {
        let mut buf = vec![0.0f32; 64 * 2];
        self.engine.render(&mut buf, 2);
    }

    /// One message as a hardware port delivers it: three raw bytes.
    fn midi(&mut self, bytes: [u8; 3]) -> Vec<(NodeId, u32, f64)> {
        let m = MidiMsg::from_bytes(&bytes).expect("a channel message");
        assert_eq!(bytes[0] & 0xF0, 0xB0, "these tests send controllers");
        self.engine.cc_log.clear();
        self.bus.send(Command::Cc { source: PEDAL, channel: m.channel, cc: m.data[0], value: m.data[1] });
        self.block();
        self.engine.cc_log.clone()
    }

    fn value_of(log: &[(NodeId, u32, f64)], node: NodeId, param: u32) -> f64 {
        log.iter().rev().find(|e| e.0 == node && e.1 == param).unwrap_or_else(|| panic!("no value for node {node} param {param} in {log:?}")).2
    }
}

/// Filter cutoff 20..80 %, the Delay's mix 0..50 % on a slow curve, and the
/// rig fader inverted from full down to a quarter.
fn swell(r: &Rack) -> Macro {
    Macro {
        id: 7,
        name: "Swell".into(),
        home: r.master,
        value: 0.0,
        morph: false,
        targets: vec![
            MacroTarget { min: 0.2, max: 0.8, ..MacroTarget::new(r.filter, 0, CcTarget::Knob) },
            MacroTarget { min: 0.0, max: 0.5, curve: Curve::Slow, ..MacroTarget::new(r.delay, crate::engine::P_MIX, CcTarget::PluginParam) },
            MacroTarget { min: 0.25, max: 1.0, invert: true, ..MacroTarget::new(r.master, MixControl::Vol.param(), CcTarget::Mix) },
        ],
    }
}

fn bind(r: &mut Rack, m: &Macro, cc: u8) {
    r.bus.send(Command::SetMacros { macros: MacroTable::new(vec![m.clone()]) });
    r.bus.send(Command::MatrixCcMap { source: PEDAL, dest: m.home, cc, param: Some(m.id), target: CcTarget::Macro, scale: Default::default() });
    r.block();
}

#[test]
fn one_cc_moves_three_parameters_over_their_own_ranges_and_one_runs_backwards() {
    let mut r = rack();
    let m = swell(&r);
    bind(&mut r, &m, 20);
    let (filter, delay, master) = (r.filter, r.delay, r.master);

    let low = r.midi([0xB0, 20, 0]);
    assert_eq!(low.len(), 3, "every target moved once: {low:?}");
    assert!((Rack::value_of(&low, filter, 0) - 0.2).abs() < 1e-6, "{low:?}");
    assert!(Rack::value_of(&low, delay, crate::engine::P_MIX).abs() < 1e-6, "{low:?}");
    assert!((r.engine.rig_mix[0].vol - 1.0).abs() < 1e-6, "the inverted fader starts at the top: {}", r.engine.rig_mix[0].vol);

    let mid = r.midi([0xB0, 20, 64]);
    let t = 64.0 / 127.0;
    assert!((Rack::value_of(&mid, filter, 0) - (0.2 + 0.6 * t)).abs() < 1e-6, "{mid:?}");
    assert!((Rack::value_of(&mid, delay, crate::engine::P_MIX) - 0.5 * t * t).abs() < 1e-6, "the slow curve: {mid:?}");
    assert!((r.engine.rig_mix[0].vol as f64 - (0.25 + 0.75 * (1.0 - t))).abs() < 1e-5, "{}", r.engine.rig_mix[0].vol);

    let high = r.midi([0xB0, 20, 127]);
    assert!((Rack::value_of(&high, filter, 0) - 0.8).abs() < 1e-6, "{high:?}");
    assert!((Rack::value_of(&high, delay, crate::engine::P_MIX) - 0.5).abs() < 1e-6, "{high:?}");
    assert!((r.engine.rig_mix[0].vol - 0.25).abs() < 1e-6, "and ends a quarter up: {}", r.engine.rig_mix[0].vol);

    // Another channel's status byte and another controller: the same macro
    // on any channel (the rule is omni), nothing on an unbound CC.
    assert_eq!(r.midi([0xB3, 20, 0]).len(), 3);
    assert!(r.midi([0xB0, 21, 127]).iter().all(|e| e.0 != filter && e.0 != delay), "CC 21 is not the macro's");
    assert!((r.engine.rig_mix[0].vol - 1.0).abs() < 1e-6);
}

#[test]
fn a_sweep_moves_every_target_monotonically() {
    let mut r = rack();
    let m = swell(&r);
    bind(&mut r, &m, 20);
    let (filter, delay) = (r.filter, r.delay);
    let (mut cutoff, mut mix, mut vol) = (f64::MIN, f64::MIN, f32::MAX);
    for v in 0..=127u8 {
        let log = r.midi([0xB0, 20, v]);
        let (c, x, f) = (Rack::value_of(&log, filter, 0), Rack::value_of(&log, delay, crate::engine::P_MIX), r.engine.rig_mix[0].vol);
        assert!(c >= cutoff && x >= mix && f <= vol, "CC value {v}: cutoff {c}, mix {x}, fader {f}");
        (cutoff, mix, vol) = (c, x, f);
    }
}

#[test]
fn a_macro_binding_leaves_the_home_units_other_controllers_alone() {
    let mut r = rack();
    let synth = r.synth;
    // The macro lives on the synth: its mod wheel and sustain still pass.
    let m = Macro { home: synth, ..swell(&r) };
    r.bus.send(Command::MatrixRule { source: PEDAL, dest: synth, channel: None, transpose: 0, key_lo: 0, key_hi: 127, vel_lo: 0, vel_hi: 127, enabled: true });
    bind(&mut r, &m, 20);
    r.engine.cc_passthrough_log.clear();
    r.midi([0xB0, 1, 90]);
    assert_eq!(r.engine.cc_passthrough_log, vec![(synth, 1, 90)], "a macro binding is no CC whitelist");
    r.engine.cc_passthrough_log.clear();
    assert_eq!(r.midi([0xB0, 20, 127]).len(), 3);
    assert!(r.engine.cc_passthrough_log.is_empty(), "the macro's own CC is not passed on to the plugin");
}

#[test]
fn set_macro_moves_the_targets_like_the_controller_and_a_target_without_a_range_stays() {
    let mut r = rack();
    let mut m = swell(&r);
    m.targets.push(MacroTarget { min: 0.4, max: 0.4, ..MacroTarget::new(r.filter, 1, CcTarget::Knob) });
    bind(&mut r, &m, 20);
    r.engine.cc_log.clear();
    r.bus.send(Command::SetMacro { id: m.id, value: 1.0 });
    r.block();
    assert_eq!(r.engine.cc_log.len(), 3, "resonance has no range and is left alone: {:?}", r.engine.cc_log);
    assert!((r.engine.rig_mix[0].vol - 0.25).abs() < 1e-6);
    r.engine.cc_log.clear();
    r.bus.send(Command::SetMacro { id: 99, value: 1.0 });
    r.block();
    assert!(r.engine.cc_log.is_empty(), "an unknown macro moves nothing");
    // A new table replaces the old one.
    r.bus.send(Command::SetMacros { macros: MacroTable::new(vec![]) });
    r.block();
    assert!(r.midi([0xB0, 20, 0]).is_empty(), "the binding of a macro that is gone moves nothing");
}

/// A morph (#214): A as each target's minimum, B as its maximum; a stepped
/// knob switches at the middle.
#[test]
fn a_morph_from_a_to_b_moves_every_stored_parameter_monotonically() {
    let mut r = rack();
    let (filter, delay, master) = (r.filter, r.delay, r.master);
    let stored = [
        (filter, 0u32, 0.10f32, 0.90f32, Curve::Linear),
        (filter, 1, 0.70, 0.20, Curve::Linear),
        (delay, crate::engine::P_TIME, 0.25, 0.75, Curve::Linear),
        // A stepped knob (two of its positions): no values in between.
        (delay, crate::engine::P_MIX, 0.0, 1.0, Curve::Switch),
    ];
    let m = Macro {
        id: 1, name: "Morph".into(), home: master, value: 0.0, morph: true,
        targets: stored.iter().map(|&(node, param, min, max, curve)| MacroTarget { min, max, curve, ..MacroTarget::new(node, param, CcTarget::Knob) }).collect(),
    };
    bind(&mut r, &m, 11);
    let mut last: Vec<f64> = Vec::new();
    for v in 0..=127u8 {
        let log = r.midi([0xB0, 11, v]);
        let now: Vec<f64> = stored.iter().map(|&(node, param, ..)| Rack::value_of(&log, node, param)).collect();
        for (i, &(_, _, a, b, curve)) in stored.iter().enumerate() {
            let (a, b) = (a as f64, b as f64);
            if let Some(&before) = last.get(i) {
                assert!(if b >= a { now[i] >= before } else { now[i] <= before }, "target {i} turns back at CC value {v}: {before} → {}", now[i]);
            }
            assert!(now[i] >= a.min(b) - 1e-6 && now[i] <= a.max(b) + 1e-6, "target {i} leaves A..B at {v}");
            if curve == Curve::Switch {
                assert!(now[i] == a || now[i] == b, "a stepped knob takes A or B, never {}", now[i]);
                assert_eq!(now[i] == b, v >= 64, "and switches at the middle (CC value {v})");
            }
        }
        if v == 0 {
            for (i, s) in stored.iter().enumerate() { assert!((now[i] - s.2 as f64).abs() < 1e-6, "state A at the heel"); }
        }
        if v == 127 {
            for (i, s) in stored.iter().enumerate() { assert!((now[i] - s.3 as f64).abs() < 1e-6, "state B at the toe"); }
        }
        last = now;
    }
}
