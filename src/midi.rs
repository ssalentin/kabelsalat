//! MIDI I/O via midir (ALSA sequencer).
//! Owned by the audio-io workstream. P0: enumerate ports, create a virtual
//! input port, log incoming events; later: feed the MIDI matrix.
//!
//! `kabelsalat midi-test`:
//! 1. enumerates all ALSA-seq input and output ports,
//! 2. creates the virtual input port **kabelsalat-in**,
//! 3. verifies reception by sending test notes from a second midir client
//!    in this process (a C-major arpeggio + CC + pitch bend),
//! 4. keeps listening for external events until the window closes — send
//!    your own with e.g. `aplaymidi -p <client:port> song.mid` (the port
//!    address is printed via `aconnect -l`).
//!
//! `KSALAT_MIDI_LISTEN_SECS` (default 5) sets the listen window.
//! `KSALAT_MIDI_NO_SELFSEND=1` skips the built-in sender and only listens —
//! for verifying external sources (aplaymidi, hardware controllers).
//!
//! The midir callback runs on midir's own thread, not the audio callback,
//! but we still keep it allocation-free: events go into a preallocated
//! `rtrb` ring as fixed-size records; decoding/logging happens on the main
//! thread (PLAN.md §7 discipline, ready for the future MIDI matrix).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use midir::os::unix::VirtualInput;
use midir::{Ignore, MidiInput, MidiOutput};

/// Longest message prefix we capture (P0: channel voice + most system msgs;
/// full sysex streaming comes with the matrix in P1).
const MAX_MSG_LEN: usize = 32;
const RING_CAPACITY: usize = 512;

#[derive(Clone, Copy)]
struct MidiEvent {
    /// ALSA-seq timestamp, microseconds.
    stamp_us: u64,
    /// Full message length (may exceed the captured prefix).
    len: u32,
    bytes: [u8; MAX_MSG_LEN],
}

impl Default for MidiEvent {
    fn default() -> Self {
        Self {
            stamp_us: 0,
            len: 0,
            bytes: [0; MAX_MSG_LEN],
        }
    }
}

impl MidiEvent {
    fn captured(&self) -> &[u8] {
        &self.bytes[..(self.len as usize).min(MAX_MSG_LEN)]
    }

    fn describe(&self) -> String {
        let b = self.captured();
        if b.is_empty() {
            return "<empty>".into();
        }
        let ch = (b[0] & 0x0F) + 1;
        match b[0] & 0xF0 {
            0x90 if b.len() >= 3 && b[2] > 0 => {
                format!("note on  ch{ch} note {} vel {}", b[1], b[2])
            }
            0x80 | 0x90 if b.len() >= 3 => {
                format!("note off ch{ch} note {} vel {}", b[1], b[2])
            }
            0xB0 if b.len() >= 3 => format!("CC ch{ch} cc {} val {}", b[1], b[2]),
            0xE0 if b.len() >= 3 => {
                let val = (b[2] as i16) << 7 | b[1] as i16;
                format!("pitch bend ch{ch} {val}")
            }
            _ => format!("raw {:02X?}", b),
        }
    }
}

pub fn run_test() -> anyhow::Result<()> {
    let listen_secs = std::env::var("KSALAT_MIDI_LISTEN_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5)
        .max(2);

    // 1. Enumerate ALSA-seq ports --------------------------------------------
    let mut midi_in = MidiInput::new("kabelsalat")?;
    midi_in.ignore(Ignore::None);
    println!("ALSA-seq input ports:");
    for port in midi_in.ports() {
        println!("  - {}", midi_in.port_name(&port)?);
    }
    let probe = MidiOutput::new("kabelsalat-probe")?;
    println!("ALSA-seq output ports:");
    for port in probe.ports() {
        println!("  - {}", probe.port_name(&port)?);
    }
    drop(probe);

    // 2. Virtual input port "kabelsalat-in" ----------------------------------
    let (mut producer, mut consumer) = rtrb::RingBuffer::<MidiEvent>::new(RING_CAPACITY);
    let received = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let (rx_count, rx_dropped) = (received.clone(), dropped.clone());
    let conn = midi_in.create_virtual(
        "kabelsalat-in",
        move |stamp_us, message, _data| {
            // Allocation-free: fixed-size record into a preallocated ring.
            let len = message.len().min(MAX_MSG_LEN);
            let mut event = MidiEvent {
                stamp_us,
                len: message.len() as u32,
                bytes: [0; MAX_MSG_LEN],
            };
            event.bytes[..len].copy_from_slice(&message[..len]);
            if producer.push(event).is_err() {
                rx_dropped.fetch_add(1, Ordering::Relaxed);
            }
            rx_count.fetch_add(1, Ordering::Relaxed);
        },
        (),
    )
    // ConnectError<MidiInput> is !Sync (it hands the client back); stringify.
    .map_err(|e| anyhow::anyhow!("could not create virtual port kabelsalat-in: {e}"))?;
    println!("created virtual input port: kabelsalat-in");
    if let Ok(out) = std::process::Command::new("aconnect").arg("-l").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line.contains("kabelsalat") {
                println!("  {line}");
                for sub in lines.by_ref() {
                    if sub.starts_with("client ") {
                        break;
                    }
                    println!("  {sub}");
                }
            }
        }
    }
    println!(
        "external senders: aplaymidi -p <client:port> file.mid ({} s listen window)",
        listen_secs
    );

    // 3. Second midir client sends test notes --------------------------------
    let no_selfsend = matches!(
        std::env::var("KSALAT_MIDI_NO_SELFSEND").as_deref(),
        Ok("1") | Ok("true")
    );
    std::thread::sleep(Duration::from_millis(300)); // let ALSA-seq register the port
    let sequence: &[(&[u8], u64)] = &[
        (&[0x90, 60, 100], 120), // C4 on
        (&[0x80, 60, 0], 30),    // C4 off
        (&[0x90, 64, 100], 120), // E4 on
        (&[0x80, 64, 0], 30),    // E4 off
        (&[0x90, 67, 100], 120), // G4 on
        (&[0x80, 67, 0], 30),    // G4 off
        (&[0xB0, 7, 100], 30),   // CC7 volume
        (&[0xE0, 0, 64], 30),    // pitch bend center
    ];
    if !no_selfsend {
        let sender = MidiOutput::new("kabelsalat-test-sender")?;
        let target = sender
            .ports()
            .into_iter()
            .find(|p| {
                sender
                    .port_name(p)
                    .map(|n| n.contains("kabelsalat-in"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("virtual port kabelsalat-in not visible to sender"))?;
        let mut out_conn = sender
            .connect(&target, "kabelsalat-test-send")
            .map_err(|e| anyhow::anyhow!("could not connect sender to kabelsalat-in: {e}"))?;
        println!("sending {} test messages from second midir client...", sequence.len());
        for (message, gap_ms) in sequence {
            out_conn.send(message)?;
            std::thread::sleep(Duration::from_millis(*gap_ms));
        }
        drop(out_conn);
    } else {
        println!("self-send disabled; listening for external events only...");
    }

    // 4. Drain the ring, log events, verify ----------------------------------
    let expected = if no_selfsend { 0 } else { sequence.len() };
    let deadline = Instant::now() + Duration::from_secs(listen_secs);
    let mut logged: Vec<MidiEvent> = Vec::new();
    let mut complete_at: Option<Instant> = None;
    while Instant::now() < deadline {
        while let Ok(event) = consumer.pop() {
            println!(
                "  [{:>9.3} ms] {}",
                event.stamp_us as f64 / 1000.0,
                event.describe()
            );
            logged.push(event);
        }
        if expected > 0 && logged.len() >= expected && complete_at.is_none() {
            // Linger briefly so external events (aplaymidi, hardware) land too.
            complete_at = Some(Instant::now() + Duration::from_secs(1));
        }
        if let Some(until) = complete_at {
            if Instant::now() >= until {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    while let Ok(event) = consumer.pop() {
        println!(
            "  [{:>9.3} ms] {}",
            event.stamp_us as f64 / 1000.0,
            event.describe()
        );
        logged.push(event);
    }
    drop(conn);

    // 5. Verdict --------------------------------------------------------------
    let mut note_ons: Vec<u8> = Vec::new();
    let mut cc_count = 0usize;
    for event in &logged {
        let b = event.captured();
        if b.len() >= 3 {
            match b[0] & 0xF0 {
                0x90 if b[2] > 0 => note_ons.push(b[1]),
                0xB0 => cc_count += 1,
                _ => {}
            }
        }
    }
    let total = received.load(Ordering::Relaxed);
    let lost = dropped.load(Ordering::Relaxed);
    println!(
        "received: {total} events ({} logged, {lost} dropped)",
        logged.len()
    );
    if no_selfsend {
        if total == 0 {
            anyhow::bail!("midi-test FAIL: no external events received in {listen_secs} s");
        }
        println!("midi-test: PASS (external events received on kabelsalat-in)");
        return Ok(());
    }
    for note in [60u8, 64, 67] {
        if !note_ons.contains(&note) {
            anyhow::bail!("midi-test FAIL: note-on {note} not received");
        }
    }
    if cc_count == 0 {
        anyhow::bail!("midi-test FAIL: no CC received");
    }
    if logged.len() < expected {
        anyhow::bail!(
            "midi-test FAIL: only {} of {expected} test messages received",
            logged.len()
        );
    }
    println!("midi-test: PASS (virtual port kabelsalat-in receives events)");
    Ok(())
}
