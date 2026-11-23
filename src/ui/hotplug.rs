//! Stage hot-plug (#402): what the app says and does when an audio device
//! or a MIDI controller goes away and comes back mid-show.
//!
//! The work itself runs elsewhere: the output stream is reopened by its
//! supervisor thread (`audio_recover`), MIDI inputs are reconnected by the
//! MIDI thread (`rescan_hw_midi`, rules 48, 58, 61). This module turns what
//! they did into a notice and a Show-mode indicator, releases what an
//! unplugged keyboard still held, and reopens an audio input whose device
//! returned.

use super::*;

/// How long a hot-plug notice stays (the ordinary patch notice: 2.5 s).
const NOTICE_SECS: f32 = 6.0;
/// MIDI devices found while the app starts are not news.
const STARTUP_QUIET_SECS: f32 = 5.0;
/// A dead input is retried no sooner: the device scan (every 2 s) must
/// have looked once more, or a stale list would open another source.
const INPUT_RETRY_SECS: f32 = 2.5;
const INPUT_RETRY_MAX_SECS: f32 = 20.0;

#[derive(Default)]
pub(super) struct Hotplug {
    /// The output stream is down and being reopened.
    audio_lost: bool,
    /// Show-mode indicator after an event: (label, colour, until).
    flash: Option<(&'static str, Color32, f32)>,
    /// Audio In nodes without a live capture: (first seen dead, attempts).
    inputs_down: std::collections::HashMap<u32, (f32, u32)>,
    inputs_check_at: f32,
}

/// What changed for one hardware MIDI device between two device lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MidiChange {
    Lost(String),
    Back(String),
    New(String),
}

/// Compare two device lists (name, plugged in) by index: an index is one
/// device for the whole run (`match_midi_device`).
pub(super) fn midi_changes(old: &[(String, bool)], new: &[(String, bool)]) -> Vec<MidiChange> {
    let label = |raw: &str| midi_port_name(raw).device;
    let mut out = Vec::new();
    for (i, (name, live)) in new.iter().enumerate() {
        match (old.get(i), *live) {
            (Some((_, true)), false) => out.push(MidiChange::Lost(label(name))),
            (Some((_, false)), true) => out.push(MidiChange::Back(label(name))),
            (None, true) => out.push(MidiChange::New(label(name))),
            _ => {}
        }
    }
    out
}

/// One notice line for a pass's MIDI changes.
pub(super) fn midi_notice(changes: &[MidiChange]) -> Option<String> {
    let names = |pick: fn(&MidiChange) -> Option<&str>| -> Vec<&str> { changes.iter().filter_map(pick).collect() };
    let lost = names(|c| if let MidiChange::Lost(n) = c { Some(n) } else { None });
    let back = names(|c| if let MidiChange::Back(n) = c { Some(n) } else { None });
    let new = names(|c| if let MidiChange::New(n) = c { Some(n) } else { None });
    let mut parts = Vec::new();
    if !lost.is_empty() {
        parts.push(format!("{} disconnected — held notes released", lost.join(", ")));
    }
    if !back.is_empty() {
        parts.push(format!("{} reconnected — its bindings are live again", back.join(", ")));
    }
    if !new.is_empty() {
        parts.push(format!("{} connected", new.join(", ")));
    }
    (!parts.is_empty()).then(|| format!("MIDI: {}", parts.join(" · ")))
}

/// Is a dead input due for another try? `listed` = the device scan sees its
/// source (or it follows the default source).
pub(super) fn input_retry_due(dead_for: f32, attempts: u32, listed: bool) -> bool {
    let wait = (INPUT_RETRY_SECS * 2f32.powi(attempts.min(8) as i32)).min(INPUT_RETRY_MAX_SECS);
    listed && dead_for >= wait
}

/// Release what device `dev` still holds: an unplugged keyboard never sends
/// its note-offs, so a chord held when the cable came out would drone until
/// a restart. Goes the way hardware MIDI goes (engine first, then the UI
/// queue, so the key LEDs go dark too).
pub(super) fn release_hw_device(dev: u32, queues: &HwQueues, direct: &HwDirect) -> usize {
    let (notes, pedals) = direct.take_held(dev);
    let mut n = 0;
    for note in 0..128u8 {
        if notes[(note / 64) as usize] & (1u64 << (note % 64)) != 0 {
            hw_midi_in(dev, &[0x80, note, 0], queues, direct);
            n += 1;
        }
    }
    for ch in 0..16u8 {
        if pedals & (1 << ch) != 0 {
            hw_midi_in(dev, &[0xB0 | ch, 64, 0], queues, direct);
        }
    }
    n
}

impl DemoApp {
    fn hotplug_notice(&mut self, msg: String) {
        self.patch_notice = Some((msg, self.ui_time + NOTICE_SECS));
    }

    /// Once per frame: adopt what the output's supervisor did and look after
    /// the audio inputs. Cheap when nothing happened (one lock, no I/O).
    pub(super) fn track_hotplug(&mut self) {
        self.track_audio_out();
        self.track_audio_inputs();
    }

    fn track_audio_out(&mut self) {
        let Some(host) = &mut self.audio_host else { return };
        let events = host.sup.take_events();
        if events.is_empty() {
            return;
        }
        use crate::audio_recover::Event;
        let mut recovered = false;
        for ev in &events {
            match ev {
                Event::Lost { .. } => {
                    self.hotplug.audio_lost = true;
                    self.audio_backend_err = Some("Audio output lost — reconnecting…".into());
                }
                Event::Recovered { info, changed, .. } => {
                    host.adopt(info);
                    self.hotplug.audio_lost = false;
                    self.hotplug.flash = Some(("AUDIO BACK", theme::LIVE_TEAL, self.ui_time + NOTICE_SECS));
                    self.active_audio_backend = Some(info.backend);
                    self.buffer_frames = info.buffer_frames;
                    self.audio_backend_err = changed.as_ref().map(|c| format!("After reconnecting: {c}"));
                    recovered = true;
                }
                Event::Mended(_) => {
                    self.hotplug.flash = Some(("ROUTING RESTORED", theme::LIVE_TEAL, self.ui_time + NOTICE_SECS));
                }
            }
        }
        if recovered {
            // The new stream goes to the chosen output at once, not at the
            // scan's next pass.
            if let Some(tx) = &self.stream_cmds {
                let _ = tx.send(crate::audio_out::StreamCmd::Resync);
            }
            self.engine_stats_at = f32::NEG_INFINITY;
        }
        if let Some(last) = events.last() {
            self.hotplug_notice(last.notice());
        }
    }

    /// An Audio In whose capture ended (its device went away) is opened
    /// again when the device scan lists the device again. Until then it is
    /// "not available", never another input (rule 60).
    fn track_audio_inputs(&mut self) {
        if self.audio_in_tx.is_none() || self.ui_time < self.hotplug.inputs_check_at {
            return;
        }
        let now = self.ui_time;
        self.hotplug.inputs_check_at = now + 1.0;
        let nodes: Vec<(u32, Option<String>)> = self.base_graph.nodes.iter()
            .filter_map(|n| match &n.kind {
                NodeKind::AudioIn { device, .. } => Some((n.id, device.clone())),
                _ => None,
            })
            .collect();
        self.hotplug.inputs_down.retain(|id, _| nodes.iter().any(|(n, _)| n == id));
        if nodes.is_empty() {
            return;
        }
        let listed: Vec<String> = match self.input_devices.try_lock() {
            Ok(d) => d.iter().map(|d| d.raw.clone()).collect(),
            Err(_) => return,
        };
        let mut due = false;
        let mut lost = Vec::new();
        for (id, device) in &nodes {
            let live = self.audio_in_live.get(id).is_some_and(|l| l.load(Ordering::Relaxed));
            if live {
                self.hotplug.inputs_down.remove(id);
                continue;
            }
            let was_open = self.audio_in_opened.contains_key(id);
            let known = self.hotplug.inputs_down.contains_key(id);
            let (since, attempts) = *self.hotplug.inputs_down.entry(*id).or_insert((now, 0));
            if was_open && !known {
                lost.push(self.audio_in_label(device.as_deref()));
            }
            let present = device.as_ref().is_none_or(|d| listed.contains(d));
            due |= input_retry_due(now - since, attempts, present);
        }
        if let Some(name) = lost.first() {
            self.hotplug.flash = Some(("INPUT LOST", theme::CLIP_BRIGHT, now + NOTICE_SECS));
            self.hotplug_notice(format!("Audio input {name} disconnected — it reopens when the device is back"));
        }
        if !due {
            return;
        }
        self.sync_audio_in_captures();
        let mut back = Vec::new();
        for (id, device) in &nodes {
            let Some(entry) = self.hotplug.inputs_down.get_mut(id) else { continue };
            if self.audio_in_live.get(id).is_some_and(|l| l.load(Ordering::Relaxed)) {
                back.push(self.audio_in_label(device.as_deref()));
                self.hotplug.inputs_down.remove(id);
            } else {
                *entry = (now, entry.1.saturating_add(1));
            }
        }
        if let Some(name) = back.first() {
            self.hotplug.flash = Some(("INPUT BACK", theme::LIVE_TEAL, now + NOTICE_SECS));
            self.hotplug_notice(format!("Audio input {name} is back"));
        }
    }

    fn audio_in_label(&self, device: Option<&str>) -> String {
        match device {
            Some(d) => crate::audio_in::pretty_device_name(d),
            None => "(system default)".into(),
        }
    }

    /// From `adopt_hw_devices`: say what the MIDI thread's pass changed.
    pub(super) fn note_midi_changes(&mut self, old: &[(String, bool)], new: &[(String, bool)]) {
        let mut changes = midi_changes(old, new);
        if self.ui_time < STARTUP_QUIET_SECS {
            changes.retain(|c| !matches!(c, MidiChange::New(_)));
        }
        let Some(msg) = midi_notice(&changes) else { return };
        let lost = changes.iter().any(|c| matches!(c, MidiChange::Lost(_)));
        self.hotplug.flash = Some(if lost {
            ("MIDI LOST", theme::CLIP_BRIGHT, self.ui_time + NOTICE_SECS)
        } else {
            ("MIDI BACK", theme::LIVE_TEAL, self.ui_time + NOTICE_SECS)
        });
        self.hotplug_notice(msg);
    }

    /// Show mode's indicators: a lost output stays on until it is back,
    /// everything else shows for a few seconds.
    pub(super) fn hotplug_indicators(&self) -> Vec<(&'static str, Color32)> {
        let mut out = Vec::new();
        if self.hotplug.audio_lost {
            out.push(("NO AUDIO OUT · reconnecting", theme::CLIP_BRIGHT));
        }
        if let Some((label, color, until)) = self.hotplug.flash {
            if until > self.ui_time && !(self.hotplug.audio_lost && label == "AUDIO BACK") {
                out.push((label, color));
            }
        }
        out
    }
}

/// `hotplug-test`'s MIDI part, on a real ALSA sequencer: a virtual
/// keyboard is found by the app's own hot-plug pass, plays, is unplugged
/// while a key and the pedal are down, and comes back under a new client
/// number as the same device. `Ok(None)` = ran; `Ok(Some(why))` = skipped.
pub(crate) fn midi_selftest() -> anyhow::Result<Option<String>> {
    const NAME: &str = "Hotplug Test Keys";
    let virtual_keys = || -> anyhow::Result<midir::MidiOutputConnection> {
        use midir::os::unix::VirtualOutput;
        let out = midir::MidiOutput::new(NAME).map_err(|e| anyhow::anyhow!("{e}"))?;
        out.create_virtual(NAME).map_err(|e| anyhow::anyhow!("{e}"))
    };
    let mut keys = match virtual_keys() {
        Ok(k) => k,
        Err(e) => return Ok(Some(format!("no ALSA sequencer on this machine ({e})"))),
    };
    let hw = Arc::new(HwMidi::default());
    let queues = HwQueues {
        notes: Arc::new(Mutex::new(VecDeque::new())),
        ccs: Arc::new(Mutex::new(VecDeque::new())),
        programs: Arc::new(Mutex::new(VecDeque::new())),
    };
    let direct = Arc::new(HwDirect::default());
    let (bus, mut drain) = crate::control::command_bus();
    *direct.bus.lock().unwrap() = Some(Arc::new(Mutex::new(bus)));
    let mut conns = Vec::new();
    let ours = |hw: &HwMidi| -> Option<(usize, String, bool)> {
        hw.devices.lock().unwrap().iter().enumerate()
            .find(|(_, (n, _))| n.contains(NAME))
            .map(|(i, (n, l))| (i, n.clone(), *l))
    };
    let heard = |drain: &mut crate::control::CommandDrain, what: &str, want: &dyn Fn(&Command) -> bool| -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut seen = Vec::new();
        loop {
            let mut hit = false;
            drain.drain(|c| {
                hit |= want(&c);
                seen.push(format!("{c:?}"));
            });
            if hit {
                return Ok(());
            }
            anyhow::ensure!(std::time::Instant::now() < deadline, "{what}: not received (got {seen:?})");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    rescan_hw_midi(&hw, &mut conns, &queues, &direct);
    let (dev, name, live) = ours(&hw).ok_or_else(|| anyhow::anyhow!("the hot-plug pass did not find '{NAME}'"))?;
    anyhow::ensure!(live, "'{name}' was found but not connected");
    let src = SourceId::Hardware(dev as u32);
    println!("ok: found and connected '{name}' as hardware device {dev}");

    keys.send(&[0x90, 60, 100]).map_err(|e| anyhow::anyhow!("{e}"))?;
    heard(&mut drain, "note-on", &|c| matches!(c, Command::NoteOn { note: 60, source, .. } if *source == src))?;
    keys.send(&[0xB0, 64, 127]).map_err(|e| anyhow::anyhow!("{e}"))?;
    heard(&mut drain, "sustain down", &|c| matches!(c, Command::Cc { cc: 64, value: 127, source, .. } if *source == src))?;
    println!("ok: a held key (C4) and the sustain pedal reached the engine bus");

    // The cable comes out with the key and the pedal still down.
    keys.close();
    std::thread::sleep(std::time::Duration::from_millis(200));
    rescan_hw_midi(&hw, &mut conns, &queues, &direct);
    let (dev2, _, live) = ours(&hw).ok_or_else(|| anyhow::anyhow!("the unplugged device left the list"))?;
    anyhow::ensure!(dev2 == dev && !live, "unplugged: index {dev2} (was {dev}), live {live}");
    let mut off = (false, false);
    drain.drain(|c| match c {
        Command::NoteOff { note: 60, source, .. } if source == src => off.0 = true,
        Command::Cc { cc: 64, value: 0, source, .. } if source == src => off.1 = true,
        _ => {}
    });
    anyhow::ensure!(off.0, "the held key was not released when its device was unplugged");
    anyhow::ensure!(off.1, "the sustain pedal was not lifted when its device was unplugged");
    println!("ok: unplugged while held: the device keeps index {dev}, its key and pedal were released");

    // Back, as a new ALSA client (another client number, as after a re-plug).
    let _spacer = midir::MidiOutput::new("Hotplug Test Spacer").map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut keys = virtual_keys()?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    rescan_hw_midi(&hw, &mut conns, &queues, &direct);
    let (dev3, name3, live) = ours(&hw).ok_or_else(|| anyhow::anyhow!("the re-plugged device is not listed"))?;
    anyhow::ensure!(dev3 == dev && live, "re-plugged '{name3}': index {dev3} (was {dev}), live {live}");
    anyhow::ensure!(hw.devices.lock().unwrap().iter().filter(|(n, _)| n.contains(NAME)).count() == 1, "the re-plugged keyboard became a second device");
    keys.send(&[0x90, 64, 90]).map_err(|e| anyhow::anyhow!("{e}"))?;
    heard(&mut drain, "note-on after the re-plug", &|c| matches!(c, Command::NoteOn { note: 64, source, .. } if *source == src))?;
    keys.send(&[0x80, 64, 0]).map_err(|e| anyhow::anyhow!("{e}"))?;
    heard(&mut drain, "note-off after the re-plug", &|c| matches!(c, Command::NoteOff { note: 64, source, .. } if *source == src))?;
    println!("ok: re-plugged as '{name3}' (was '{name}'): same device {dev}, it plays without a restart");
    let msg = midi_notice(&midi_changes(&[(name.clone(), false)], &[(name3, true)])).unwrap_or_default();
    println!("ok: notice: {msg}");
    keys.close();
    for c in conns.into_iter().flatten() {
        c.close();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FM1: &str = "M-Wave FM-1:M-Wave FM-1 MIDI 1 24:0";
    const FM1_REPLUGGED: &str = "M-Wave FM-1:M-Wave FM-1 MIDI 1 32:0";
    const PEDAL: &str = "FS-1-WL:FS-1-WL MIDI 1 28:0";

    fn rig() -> (HwQueues, Arc<HwDirect>, crate::control::CommandDrain) {
        let queues = HwQueues {
            notes: Arc::new(Mutex::new(VecDeque::new())),
            ccs: Arc::new(Mutex::new(VecDeque::new())),
            programs: Arc::new(Mutex::new(VecDeque::new())),
        };
        let direct = Arc::new(HwDirect::default());
        let (bus, drain) = crate::control::command_bus();
        *direct.bus.lock().unwrap() = Some(Arc::new(Mutex::new(bus)));
        (queues, direct, drain)
    }

    fn drained(drain: &mut crate::control::CommandDrain) -> Vec<Command> {
        let mut out = Vec::new();
        drain.drain(|c| out.push(c));
        out
    }

    /// The bug this closes: a chord held when the USB cable comes out never
    /// gets its note-offs, and the pedal never comes up.
    #[test]
    fn an_unplugged_keyboard_lets_go_of_its_held_keys_and_pedal_and_of_nothing_else() {
        let (queues, direct, mut drain) = rig();
        hw_midi_in(0, &[0x90, 60, 100], &queues, &direct);
        hw_midi_in(0, &[0x90, 64, 100], &queues, &direct);
        hw_midi_in(0, &[0x90, 67, 100], &queues, &direct);
        hw_midi_in(0, &[0x80, 64, 0], &queues, &direct); // released by hand
        hw_midi_in(0, &[0x90, 72, 0], &queues, &direct); // note-on velocity 0 = off
        hw_midi_in(0, &[0xB2, 64, 127], &queues, &direct); // sustain down, channel 3
        hw_midi_in(1, &[0x90, 48, 100], &queues, &direct); // another keyboard
        hw_midi_in(1, &[0xB0, 64, 127], &queues, &direct);
        drained(&mut drain);
        queues.notes.lock().unwrap().clear();
        queues.ccs.lock().unwrap().clear();

        assert_eq!(release_hw_device(0, &queues, &direct), 2);
        let sent = drained(&mut drain);
        let src = SourceId::Hardware(0);
        let offs: Vec<u8> = sent.iter().filter_map(|c| match c {
            Command::NoteOff { note, source, .. } if *source == src => Some(*note),
            _ => None,
        }).collect();
        assert_eq!(offs, [60, 67], "the two keys still down, not the released one");
        assert!(sent.iter().any(|c| matches!(c, Command::Cc { source, channel: 2, cc: 64, value: 0 } if *source == src)), "{sent:?}");
        assert_eq!(sent.len(), 3, "nothing for the other keyboard: {sent:?}");
        // The UI hears it too, so the key LEDs go dark (rule 88).
        let ui: Vec<(u8, f32)> = queues.notes.lock().unwrap().iter().map(|(e, _)| (e.note, e.velocity)).collect();
        assert_eq!(ui, [(60, 0.0), (67, 0.0)]);
        assert_eq!(queues.ccs.lock().unwrap().len(), 1);
        // Once: a second pass has nothing left to release.
        assert_eq!(release_hw_device(0, &queues, &direct), 0);
        assert!(drained(&mut drain).is_empty());
        // The other keyboard still holds its key.
        assert_eq!(release_hw_device(1, &queues, &direct), 1);
    }

    #[test]
    fn device_changes_are_told_by_index_and_named_as_people_read_them() {
        let old = vec![(FM1.to_string(), true), (PEDAL.to_string(), true)];
        let gone = vec![(FM1.to_string(), false), (PEDAL.to_string(), true)];
        assert_eq!(midi_changes(&old, &gone), [MidiChange::Lost("M-Wave FM-1".into())]);
        assert_eq!(midi_notice(&midi_changes(&old, &gone)).unwrap(), "MIDI: M-Wave FM-1 disconnected — held notes released");
        // Back under another ALSA client number: the same device (rule 58).
        let back = vec![(FM1_REPLUGGED.to_string(), true), (PEDAL.to_string(), true)];
        assert_eq!(midi_changes(&gone, &back), [MidiChange::Back("M-Wave FM-1".into())]);
        assert_eq!(midi_notice(&midi_changes(&gone, &back)).unwrap(), "MIDI: M-Wave FM-1 reconnected — its bindings are live again");
        // A device a session named but that is not plugged in is no news.
        let listed = vec![(FM1.to_string(), true), (PEDAL.to_string(), true), ("nanoPAD2:nanoPAD2 MIDI 1".to_string(), false)];
        assert!(midi_changes(&old, &listed).is_empty());
        assert_eq!(midi_notice(&[]), None);
        let both = midi_changes(&old, &[(FM1.to_string(), false), (PEDAL.to_string(), false)]);
        assert_eq!(midi_notice(&both).unwrap(), "MIDI: M-Wave FM-1, FS-1-WL disconnected — held notes released");
    }

    #[test]
    fn the_app_says_when_a_controller_goes_and_comes_back_and_show_mode_flags_it() {
        let mut app = DemoApp::new_with_sink(None);
        app.ui_time = 60.0;
        *app.hw_midi.devices.lock().unwrap() = vec![(FM1.to_string(), true)];
        app.hw_midi.rev.fetch_add(1, Ordering::Release);
        app.adopt_hw_devices();
        assert_eq!(app.active_patch_notice(), Some("MIDI: M-Wave FM-1 connected"));

        app.hw_midi.devices.lock().unwrap()[0].1 = false;
        app.hw_midi.rev.fetch_add(1, Ordering::Release);
        app.adopt_hw_devices();
        assert_eq!(app.active_patch_notice(), Some("MIDI: M-Wave FM-1 disconnected — held notes released"));
        assert_eq!(app.hotplug_indicators(), [("MIDI LOST", theme::CLIP_BRIGHT)]);
        app.ui_time += 4.0;
        assert!(app.active_patch_notice().is_some(), "longer than an ordinary notice: it is read from a stage");

        *app.hw_midi.devices.lock().unwrap() = vec![(FM1_REPLUGGED.to_string(), true)];
        app.hw_midi.rev.fetch_add(1, Ordering::Release);
        app.adopt_hw_devices();
        assert_eq!(app.active_patch_notice(), Some("MIDI: M-Wave FM-1 reconnected — its bindings are live again"));
        assert_eq!(app.hotplug_indicators(), [("MIDI BACK", theme::LIVE_TEAL)]);
        assert_eq!(app.hw_live, [true]);
        app.ui_time += NOTICE_SECS + 0.1;
        assert!(app.hotplug_indicators().is_empty() && app.active_patch_notice().is_none(), "and gone again");
    }

    #[test]
    fn devices_found_at_startup_are_not_announced() {
        let mut app = DemoApp::new_with_sink(None);
        app.ui_time = 0.5;
        *app.hw_midi.devices.lock().unwrap() = vec![(FM1.to_string(), true)];
        app.hw_midi.rev.fetch_add(1, Ordering::Release);
        app.adopt_hw_devices();
        assert_eq!(app.active_patch_notice(), None);
        assert!(app.hotplug_indicators().is_empty());
    }

    #[test]
    fn a_dead_input_is_retried_only_once_its_device_is_listed_again_and_ever_more_slowly() {
        assert!(!input_retry_due(2.0, 0, true), "the device scan has not looked again yet");
        assert!(input_retry_due(2.6, 0, true));
        assert!(!input_retry_due(600.0, 0, false), "not listed: not available, never another input");
        assert!(!input_retry_due(4.0, 1, true) && input_retry_due(5.1, 1, true));
        assert!(!input_retry_due(19.0, 9, true) && input_retry_due(20.1, 9, true), "capped at 20 s");
    }
}
