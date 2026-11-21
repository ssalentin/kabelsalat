//! #277: the show actions the engine does itself happen in the block their
//! control's message arrives in, and the message reaches no instrument.

use super::*;
use crate::show::{ShowAction, ShowControl, ShowMsg, NO_SHOW_CONTROLS};

/// Key 36 + i of the pedal does `actions[i]`.
fn pads(e: &mut super::super::GraphEngine, bus: &mut crate::control::CommandBus, actions: &[ShowAction]) {
    let mut controls = NO_SHOW_CONTROLS;
    for (i, &action) in actions.iter().enumerate() {
        controls[i] = Some(ShowControl { device: Some(1), channel: None, msg: ShowMsg::Note(36 + i as u8), action });
    }
    play(e, bus, Command::SetShowControls { controls });
}

/// A press and release of the pedal's key `note`: what reached an instrument.
fn hit(e: &mut super::super::GraphEngine, bus: &mut crate::control::CommandBus, note: u8) -> Vec<(u32, u8, bool)> {
    let mut log = play(e, bus, Command::NoteOn { note, velocity: 0.8, source: PEDAL, channel: 0 });
    log.extend(play(e, bus, Command::NoteOff { note, source: PEDAL, channel: 0 }));
    log
}

#[test]
fn the_engines_show_actions_run_in_the_block_and_reach_no_instrument() {
    let (mut e, mut bus) = load_two_rigs(None, None);
    let pub_ = std::sync::Arc::new(std::sync::Mutex::new(super::super::GraphPub::default()));
    e.set_graph_pub(pub_.clone());
    let (mut tx, rx) = rtrb::RingBuffer::new(2);
    let (back, _back_rx) = rtrb::RingBuffer::new(4);
    e.set_player_channels(rx, back);
    tx.push(crate::player::PlayerData::from_samples(vec![0.25; 48000 * 2], 48000)).unwrap();
    pads(&mut e, &mut bus, &[ShowAction::MasterMute, ShowAction::BackingTrack, ShowAction::Panic, ShowAction::TapTempo]);
    assert!(e.player_pub().enabled, "a loaded file plays");

    assert!(hit(&mut e, &mut bus, 36).is_empty(), "the mute key reaches no instrument");
    assert!(e.master_mute && pub_.lock().unwrap().master_mute, "and mutes the master, published");
    hit(&mut e, &mut bus, 36);
    assert!(!e.master_mute, "the next press unmutes");

    hit(&mut e, &mut bus, 37);
    assert!(!e.player_pub().enabled, "the backing track stops");
    hit(&mut e, &mut bus, 37);
    assert!(e.player_pub().enabled, "and starts again");

    let a = synth_of(&e, 0);
    play(&mut e, &mut bus, on(60));
    let log = hit(&mut e, &mut bus, 38);
    assert!(log.contains(&(a, 255, false)), "Panic lets go of the held key: {log:?}");

    // Four taps half a second of audio apart, whatever the block size.
    let mut buf = vec![0.0f32; 300 * 2];
    for _ in 0..4 {
        hit(&mut e, &mut bus, 39);
        for _ in 0..(24000 - 2 * 64) / 300 {
            e.render(&mut buf, 2);
        }
        e.render(&mut buf[..(24000 - 2 * 64) % 300 * 2], 2);
    }
    assert_eq!(e.transport.bpm, 120.0, "tap tempo counts the engine's own time");
    let presses = pub_.lock().unwrap().show_presses;
    assert_eq!([ShowAction::MasterMute, ShowAction::BackingTrack, ShowAction::Panic, ShowAction::TapTempo].map(|a| presses[a as usize]), [2, 2, 1, 4]);
}
