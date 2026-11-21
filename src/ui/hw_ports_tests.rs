//! #227: the hardware MIDI device indices a session or rig preset saves
//! (matrix rules, rig devices) follow their port names into another run,
//! where the devices may come up in another scan order or not at all.

use super::rig_switch_tests::{list_midi_ports, FM1_PORT, U6_PORT};
use super::*;

const ORGAN_URI: &str = "urn:test:organ";
const PAD_PORT: &str = "nanoPAD2:nanoPAD2 MIDI 1";

fn surge() -> NodeKind {
    NodeKind::Clap { plugin_id: crate::control::ClapId::new(SURGE_ID) }
}

fn organ() -> NodeKind {
    NodeKind::Lv2 { uri: Lv2Uri::new(ORGAN_URI) }
}

/// Keys → `unit` → Master.
fn rig(name: &str, kind: NodeKind, unit: &str) -> crate::control::Rig {
    let mut g = Graph::default();
    g.reserve(4, 4);
    let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
    let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
    let node = g.add_node(kind, name_bytes(unit));
    g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node, port: 0 }, CPortKind::Midi);
    g.connect_with_id(2, PortRef { node, port: 0 }, PortRef { node: master, port: 0 }, CPortKind::Audio);
    crate::control::Rig::new(name, g, None)
}

fn session_of(rigs: &[crate::control::Rig]) -> crate::session::Session {
    let mut s = crate::session::Session::new("rigs", crate::control::merge_rigs(rigs));
    s.rig_names = rigs.iter().map(|r| r.name.clone()).collect();
    s
}

/// A new app with `ports` listed, after it opened `session`: the app and
/// the commands the engine got.
fn opened(session: &crate::session::Session, ports: &[(&str, &str)]) -> (DemoApp, Vec<Command>) {
    let mut app = DemoApp::new_with_sink(None);
    list_midi_ports(&mut app, ports);
    let (tx, mut drain) = crate::control::command_bus();
    app.bus = Some(Arc::new(Mutex::new(tx)));
    app.apply_session(session).unwrap();
    let mut sent = Vec::new();
    drain.drain(|c| sent.push(c));
    (app, sent)
}

fn saved_and_loaded(app: &mut DemoApp, name: &str) -> crate::session::Session {
    let pkg = crate::session::Session::package_path(&DemoApp::sessions_dir(), name).unwrap();
    app.capture_session(name).unwrap().save(&pkg).unwrap();
    crate::session::Session::load(&pkg).unwrap()
}

fn unit(app: &DemoApp, name: &str) -> usize {
    app.units.iter().position(|u| u.name == name).unwrap_or_else(|| panic!("no unit {name}"))
}

fn node(app: &DemoApp, name: &str) -> u32 {
    app.units[unit(app, name)].node_id
}

fn port_of(app: &DemoApp, d: u32) -> String {
    midi_port_key(&app.hw_devices[d as usize]).to_string()
}

fn index_of(app: &DemoApp, port: &str) -> u32 {
    app.hw_devices.iter().position(|n| midi_port_key(n) == port).expect("listed") as u32
}

/// The devices whose matrix cell into `name` is lit.
fn devices_into(app: &DemoApp, name: &str) -> Vec<String> {
    let u = unit(app, name);
    (0..app.hw_devices.len() as u32).filter(|&d| app.matrix_cell_on(u, MatrixRow::HwDev(d))).map(|d| port_of(app, d)).collect()
}

/// The devices a CC binding into `name` listens to.
fn cc_sources(app: &DemoApp, name: &str) -> Vec<String> {
    let dest = node(app, name);
    app.rule_edits.iter()
        .filter(|((_, d), e)| *d == dest && !e.cc_map.is_empty())
        .filter_map(|((s, _), _)| match s { SourceId::Hardware(d) => Some(port_of(app, *d)), _ => None })
        .collect()
}

fn after_load(sent: &[Command]) -> &[Command] {
    let load = sent.iter().rposition(|c| matches!(c, Command::LoadRigs { .. } | Command::LoadGraph { .. })).expect("a load");
    &sent[load + 1..]
}

/// The hardware indices the engine routes into `node`: the load routes
/// every one, and each MatrixRule after it turns one on or off.
fn engine_hw_routes(sent: &[Command], node: u32) -> Vec<u32> {
    let after = after_load(sent);
    (0..crate::engine::MAX_HW_DEVICES).filter(|&d| {
        after.iter().rev().find_map(|c| match c {
            Command::MatrixRule { source, dest, enabled, .. } if *source == SourceId::Hardware(d) && *dest == node => Some(*enabled),
            _ => None,
        }).unwrap_or(true)
    }).collect()
}

fn engine_cc_sources(sent: &[Command], node: u32) -> Vec<SourceId> {
    after_load(sent).iter().filter_map(|c| match c {
        Command::MatrixCcMap { source, dest, .. } if *dest == node => Some(*source),
        _ => None,
    }).collect()
}

/// The device the engine was last told `node` listens to after the load.
fn engine_inputs_device(sent: &[Command], node: u32) -> Option<u32> {
    after_load(sent).iter().rev().find_map(|c| match c {
        Command::SetRigDevice { node: n, device } if *n == node => Some(*device),
        _ => None,
    }).flatten()
}

fn loaded_rig_devices(sent: &[Command]) -> Vec<Option<u32>> {
    sent.iter().rev().find_map(|c| match c {
        Command::LoadRigs { rigs, .. } => Some(rigs.iter().map(|r| r.midi_device).collect()),
        _ => None,
    }).expect("a LoadRigs")
}

/// What the engine publishes after a `load_rigs` that binds `rig` to `dev`.
fn bind_rig(app: &mut DemoApp, rig: usize, dev: Option<u32>) {
    app.rig_snapshot.lock().unwrap().rigs[rig].midi_device = dev;
    app.adopt_rig_devices();
}

/// Keyboard, hardware and device of a synth, the device by name.
fn synth_inputs(app: &DemoApp, name: &str) -> (bool, bool, Option<String>) {
    let slot = app.voice_slot_of_node(node(app, name)).expect("a CLAP synth");
    let (keyboard, hardware) = app.rig_sources[slot];
    (keyboard, hardware, app.rig_device[slot].map(|d| port_of(app, d)))
}

/// An organ (LV2) on the U6MIDI Pro and a CC binding from the FM-1 on the
/// Surge, saved while the FM-1 was device 0, the U6MIDI Pro device 1, and
/// a pad nothing depends on device 2.
fn saved_organ_on_the_u6_and_a_cc_from_the_fm1(name: &str) -> crate::session::Session {
    let rigs = [rig("Synth", surge(), "SURGE"), rig("Organ", organ(), "ORGAN")];
    let (mut app, _) = opened(&session_of(&rigs), &[(FM1_PORT, "24:0"), (U6_PORT, "32:0"), (PAD_PORT, "40:0")]);
    app.matrix_toggle(unit(&app, "ORGAN"), MatrixRow::HwDev(1), true);
    let s = unit(&app, "SURGE");
    let level = app.units[s].knob_index("LEVEL").and_then(|k| app.units[s].knobs[k].param).expect("a LEVEL knob");
    app.set_cc_binding(SourceId::Hardware(0), node(&app, "SURGE"), 74, Some(level), crate::matrix::CcTarget::Knob);
    assert_eq!(devices_into(&app, "ORGAN"), [U6_PORT]);
    assert_eq!(cc_sources(&app, "SURGE"), [FM1_PORT]);
    saved_and_loaded(&mut app, name)
}

/// An LV2 instrument bound to one device is 15 "off" rules for the other
/// indices, and a learned CC binding is a rule of its controller's index.
/// Both stayed on the index, so another scan order moved them to the
/// other keyboard.
#[test]
fn an_lv2_device_and_a_cc_binding_follow_their_ports_into_another_scan_order() {
    let loaded = saved_organ_on_the_u6_and_a_cc_from_the_fm1("hw-ports-swapped");
    let (app, sent) = opened(&loaded, &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);

    assert_eq!(devices_into(&app, "ORGAN"), [U6_PORT], "the organ plays from the U6MIDI Pro only");
    assert_eq!(engine_hw_routes(&sent, node(&app, "ORGAN")), [0], "device 0 in this scan");
    assert_eq!(cc_sources(&app, "SURGE"), [FM1_PORT], "CC 74 still comes from the FM-1");
    assert_eq!(engine_cc_sources(&sent, node(&app, "SURGE")), [SourceId::Hardware(1)]);
}

/// A binding's level (#273) is pinned by its controller's device: it
/// follows the port into another scan order with the binding itself.
#[test]
fn a_pinned_binding_level_follows_its_port_into_another_scan_order() {
    use crate::setlist::{Binding, Level};
    let mut loaded = saved_organ_on_the_u6_and_a_cc_from_the_fm1("hw-ports-level");
    let surge = loaded.graph.nodes.iter().find(|n| crate::control::name_str(&n.name) == "SURGE").unwrap().id;
    loaded.setlist.pin_binding(Binding { source: SourceId::Hardware(0), dest: surge, cc: 74 }, Level::Show);
    let (app, _) = opened(&loaded, &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);
    let fm1 = index_of(&app, FM1_PORT);
    assert_eq!(app.setlist.levels.bindings[0].of.source, SourceId::Hardware(fm1), "the pin names the FM-1's index in this run");
    assert!(app.setlist.binding_pinned(Binding { source: SourceId::Hardware(fm1), dest: node(&app, "SURGE"), cc: 74 }));
}

/// A device the session depends on but that is not plugged in keeps its
/// own, unplugged place: its rules never land on another keyboard. One the
/// session does not depend on is not listed at all.
#[test]
fn a_device_missing_at_load_keeps_its_rules_in_a_place_of_its_own() {
    let loaded = saved_organ_on_the_u6_and_a_cc_from_the_fm1("hw-ports-missing");
    assert!(!loaded.hw_ports().values().any(|p| p == PAD_PORT));
    let (app, sent) = opened(&loaded, &[(FM1_PORT, "24:0")]);
    assert_eq!(app.hw_devices.len(), 2, "the FM-1 and the U6MIDI Pro, no pad");

    let u6 = index_of(&app, U6_PORT);
    assert_ne!(u6, 0, "not the FM-1's index");
    assert_eq!(app.hw_live.get(u6 as usize), Some(&false), "listed as not plugged in");
    assert_eq!(devices_into(&app, "ORGAN"), [U6_PORT]);
    assert_eq!(engine_hw_routes(&sent, node(&app, "ORGAN")), [u6]);
    assert_eq!(cc_sources(&app, "SURGE"), [FM1_PORT]);
    assert_eq!(engine_cc_sources(&sent, node(&app, "SURGE")), [SourceId::Hardware(0)]);

    // No place left in a full device list: the organ and the binding hear
    // no device at all, never one of the 16 listed.
    let others: Vec<String> = (0..crate::engine::MAX_HW_DEVICES).map(|i| format!("Port {i}:Port {i} MIDI 1")).collect();
    let full: Vec<(&str, &str)> = others.iter().map(|n| (n.as_str(), "20:0")).collect();
    let (app, sent) = opened(&loaded, &full);
    assert_eq!(engine_hw_routes(&sent, node(&app, "ORGAN")), Vec::<u32>::new());
    assert!(devices_into(&app, "ORGAN").is_empty());
    let cc = engine_cc_sources(&sent, node(&app, "SURGE"));
    assert!(matches!(cc[..], [SourceId::Hardware(d)] if d >= crate::engine::MAX_HW_DEVICES), "{cc:?}");
}

/// The device whose program change selects the song (#208) is saved by
/// its port: in another scan order it is still that device, and while it
/// is not plugged in it never becomes another one.
#[test]
fn a_program_change_device_follows_its_port_into_another_scan_order() {
    let (mut app, _) = opened(&session_of(&[rig("Synth", surge(), "SURGE")]), &[(FM1_PORT, "24:0"), (U6_PORT, "32:0")]);
    app.setlist.set_program_selects_song(1, true);
    let loaded = saved_and_loaded(&mut app, "hw-ports-program");
    assert_eq!(loaded.hw_ports().get(&1).map(String::as_str), Some(U6_PORT));

    let (app, _) = opened(&loaded, &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);
    assert_eq!(app.setlist.program_devices, [0], "the U6MIDI Pro, device 0 now");
    let (app, _) = opened(&loaded, &[(FM1_PORT, "24:0")]);
    let u6 = index_of(&app, U6_PORT);
    assert_ne!(u6, 0);
    assert_eq!(app.setlist.program_devices, [u6], "unplugged, never the FM-1");
}

/// "Add song from session" takes a session's rigs, not its setlist: a
/// device only its program-change choice names is not listed in the gig.
#[test]
fn an_imported_song_lists_no_device_only_its_sessions_program_change_named() {
    let (mut app, _) = opened(&session_of(&[rig("Synth", surge(), "SURGE")]), &[(FM1_PORT, "24:0"), (U6_PORT, "32:0")]);
    app.setlist.set_program_selects_song(1, true);
    saved_and_loaded(&mut app, "hw-ports-program-import");

    let (mut gig, _) = opened(&session_of(&[rig("Organ", organ(), "ORGAN")]), &[(FM1_PORT, "24:0")]);
    gig.add_song_from_session_in(&DemoApp::sessions_dir(), "hw-ports-program-import", None).unwrap();
    let listed: Vec<&str> = gig.hw_devices.iter().map(|n| midi_port_key(n)).collect();
    assert_eq!(listed, [FM1_PORT], "no unplugged U6MIDI Pro row");
    assert!(gig.setlist.program_devices.is_empty());
}

/// A file from before #227 names no ports: its indices load as they are.
#[test]
fn a_session_without_port_names_loads_its_indices_as_saved() {
    let mut older = saved_organ_on_the_u6_and_a_cc_from_the_fm1("hw-ports-older");
    assert!(!older.hw_ports().is_empty());
    older.set_hw_ports(Default::default());
    let (app, sent) = opened(&older, &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);
    assert_eq!(engine_hw_routes(&sent, node(&app, "ORGAN")), [1]);
    assert_eq!(engine_cc_sources(&sent, node(&app, "SURGE")), [SourceId::Hardware(0)]);
}

/// A rig bound to one device (`load_rigs`) was saved nowhere: a reopened
/// session gave its LV2 instruments and MIDI effects every device again.
#[test]
fn a_rigs_device_is_saved_and_follows_its_port() {
    let rigs = [rig("Synth", surge(), "SURGE"), rig("Organ", organ(), "ORGAN")];
    let (mut app, _) = opened(&session_of(&rigs), &[(FM1_PORT, "24:0"), (U6_PORT, "32:0")]);
    bind_rig(&mut app, 1, Some(1));
    let loaded = saved_and_loaded(&mut app, "hw-ports-rig-device");

    let (app, sent) = opened(&loaded, &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);
    assert_eq!(loaded_rig_devices(&sent), [None, Some(0)], "the organ's rig on the U6MIDI Pro, device 0 now");
    let shown: Vec<Option<u32>> = app.rig_snapshot.lock().unwrap().rigs.iter().map(|m| m.midi_device).collect();
    assert_eq!(shown, [None, Some(0)]);

    let (app, sent) = opened(&loaded, &[(FM1_PORT, "24:0")]);
    let u6 = index_of(&app, U6_PORT);
    assert_ne!(u6, 0);
    assert_eq!(loaded_rig_devices(&sent), [None, Some(u6)], "unplugged, never the FM-1");

    // One rig bound to a device loads as a rig list: LoadGraph has no place
    // for it. Its synth, which the session gives no row of its own, shows
    // what the engine plays: the rig's device only.
    let mut single = session_of(&[rig("Synth", surge(), "SURGE")]);
    single.rig_names.clear();
    single.rig_devices = vec![Some(3)];
    single.set_hw_ports([(3, U6_PORT.to_string())].into());
    let (app, sent) = opened(&single, &[(U6_PORT, "28:0")]);
    assert_eq!(loaded_rig_devices(&sent), [Some(0)]);
    assert_eq!(synth_inputs(&app, "SURGE"), (false, true, Some(U6_PORT.into())));
}

/// A rig preset names its rig's device, and loading it leaves every other
/// rig's synths on their own devices: `apply_rig_preset` put back only the
/// keyboard and hardware switches, so a synth bound to one keyboard
/// listened to all hardware after a `load_rig`.
#[test]
fn a_rig_preset_keeps_its_device_and_the_other_rigs_synths_keep_theirs() {
    let rigs = [rig("Lead", surge(), "LEAD"), rig("Bass", surge(), "BASS")];
    let (mut app, _) = opened(&session_of(&rigs), &[(FM1_PORT, "24:0"), (U6_PORT, "32:0")]);
    bind_rig(&mut app, 1, Some(1));
    let dir = app.rig_presets_dir.clone();
    app.save_rig_preset_to_dir(&dir, 1, "Bass on the U6").unwrap();
    let lead = app.capture_rig_preset(0, "Lead").unwrap();
    assert!(lead.session.hw_ports().is_empty(), "a rig that uses no device names none");

    let (mut app, _) = opened(&session_of(&rigs), &[(U6_PORT, "28:0"), (FM1_PORT, "36:0")]);
    app.matrix_toggle(unit(&app, "LEAD"), MatrixRow::HwDev(1), true);
    assert_eq!(synth_inputs(&app, "LEAD"), (true, true, Some(FM1_PORT.into())));
    let (tx, mut drain) = crate::control::command_bus();
    app.bus = Some(Arc::new(Mutex::new(tx)));
    for (target, rig) in [(Some(1), 1), (None, 2)] {
        assert_eq!(app.load_rig_preset_from_dir(&dir, "Bass on the U6", target).unwrap(), rig);
        let mut sent = Vec::new();
        drain.drain(|c| sent.push(c));
        assert_eq!(synth_inputs(&app, "LEAD"), (true, true, Some(FM1_PORT.into())), "rig A's synth keeps the FM-1, load into {target:?}");
        assert_eq!(engine_inputs_device(&sent, node(&app, "LEAD")), Some(index_of(&app, FM1_PORT)));
        assert_eq!(loaded_rig_devices(&sent)[rig], Some(0), "the preset's rig on the U6MIDI Pro, device 0 now");
    }
    let bass = app.units.iter().filter(|u| u.name == "BASS").map(|u| u.node_id).last().unwrap();
    let slot = app.voice_slot_of_node(bass).unwrap();
    assert_eq!(app.rig_device[slot].map(|d| port_of(&app, d)), Some(U6_PORT.into()), "the preset's synth follows its rig");
}
