//! System-audio capture (ARCHITECTURE: "wire YouTube playback into the mix").
//!
//! Each other application's playback stream is captured by its own
//! `parec --monitor-stream=<sink-input>` (PipeWire's PulseAudio layer, the way
//! the mixer's System audio meters read them) and summed by the engine.
//!
//! kabelsalat's own output is never one of those streams, so the capture
//! cannot feed back into itself, whichever output the sound goes to and
//! whenever that changes (#27). The capture used to read the default sink's
//! monitor, which holds everything played to that sink, our own output
//! included.
//!
//! Two halves: [`CaptureFeed`] lives on the device-scan thread (rule 48),
//! starts and stops the readers and hands each one's ring to the engine;
//! [`SystemCapture`] lives in the engine and only pops samples. Rings the
//! engine is done with travel back to the feed, so the audio thread never
//! frees one (rule 107).

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

/// Streams mixed at once; further ones wait for a free slot.
pub const MAX_STREAMS: usize = 16;
/// What `parec` is asked to deliver per chunk.
const CHUNK_MS: usize = 20;
/// A stream starts (and restarts after it ran dry) once this much is
/// buffered, so the chunked delivery never leaves a block short.
const PRIME_MS: usize = 60;
/// A backlog above this is cut back to `PRIME_MS` at once (rule 141).
const BACKLOG_MS: usize = 250;

struct Slot {
    consumer: Consumer<f32>,
    /// Waiting for `PRIME_MS` of audio before it is heard.
    priming: bool,
}

/// The engine's half: sums every captured stream. Allocation-free.
pub struct SystemCapture {
    slots: [Option<Slot>; MAX_STREAMS],
    incoming: Consumer<Consumer<f32>>,
    retired: Producer<Consumer<f32>>,
    wanted: Arc<AtomicBool>,
    /// Stereo samples (2 per frame) for `PRIME_MS` and `BACKLOG_MS`.
    prime: usize,
    backlog: usize,
}

/// The scan thread's half: one `parec` reader per captured stream.
pub struct CaptureFeed {
    readers: HashMap<u64, Child>,
    outgoing: Producer<Consumer<f32>>,
    retired: Consumer<Consumer<f32>>,
    wanted: Arc<AtomicBool>,
    sample_rate: u32,
}

/// Both halves of the capture, for the engine's sample rate.
pub fn system_capture(sample_rate: u32) -> (SystemCapture, CaptureFeed) {
    let (outgoing, incoming) = RingBuffer::new(MAX_STREAMS * 2);
    let (retired_tx, retired_rx) = RingBuffer::new(MAX_STREAMS * 4);
    let wanted = Arc::new(AtomicBool::new(false));
    let per_ms = sample_rate as usize * 2 / 1000;
    (
        SystemCapture {
            slots: std::array::from_fn(|_| None),
            incoming,
            retired: retired_tx,
            wanted: wanted.clone(),
            prime: PRIME_MS * per_ms,
            backlog: BACKLOG_MS * per_ms,
        },
        CaptureFeed { readers: HashMap::new(), outgoing, retired: retired_rx, wanted, sample_rate },
    )
}

/// The feed of the running app's engine, from where the audio is built to
/// the scan thread that drives it.
static FEED: std::sync::Mutex<Option<CaptureFeed>> = std::sync::Mutex::new(None);

pub fn install(feed: CaptureFeed) {
    if let Ok(mut f) = FEED.lock() {
        *f = Some(feed);
    }
}

pub fn take_feed() -> Option<CaptureFeed> {
    FEED.lock().ok()?.take()
}

impl SystemCapture {
    /// Whether the capture is mixed in at all (gain above 0). While it is
    /// not, the feed runs no reader.
    pub fn set_wanted(&self, wanted: bool) {
        self.wanted.store(wanted, Ordering::Relaxed);
    }

    pub fn is_wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// Fill `out` (stereo-interleaved) with the sum of the captured streams.
    pub fn read(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let n = out.len();
        for slot in self.slots.iter_mut() {
            if slot.is_none() {
                // A ring is taken only when a slot is free for it, so none
                // ever has to be turned away here.
                let Ok(consumer) = self.incoming.pop() else { continue };
                *slot = Some(Slot { consumer, priming: true });
            }
            let Some(s) = slot else { continue };
            let ended = s.consumer.is_abandoned();
            let mut have = s.consumer.slots();
            if have > self.backlog {
                // Whole stereo frames, or left and right swap.
                let cut = (have - self.prime) & !1;
                for _ in 0..cut {
                    let _ = s.consumer.pop();
                }
                have -= cut;
            }
            if s.priming && have >= self.prime {
                s.priming = false;
            }
            if have < n && !ended {
                s.priming = true;
            }
            if !s.priming || ended {
                for o in out[..have.min(n) & !1].iter_mut() {
                    *o += s.consumer.pop().unwrap_or(0.0);
                }
            }
            if ended && s.consumer.slots() < 2 {
                if let Some(done) = slot.take() {
                    if let Err(rtrb::PushError::Full(consumer)) = self.retired.push(done.consumer) {
                        *slot = Some(Slot { consumer, priming: true });
                    }
                }
            }
        }
    }
}

impl CaptureFeed {
    /// Capture exactly these sink-inputs (none while the capture is muted):
    /// start readers for new ones, stop those that left the list or whose
    /// reader exited. Blocking I/O: scan thread only.
    pub fn sync(&mut self, streams: &[u64]) {
        while self.retired.pop().is_ok() {}
        let wanted = self.wanted.load(Ordering::Relaxed);
        self.readers.retain(|idx, child| {
            let alive = matches!(child.try_wait(), Ok(None));
            if wanted && alive && streams.contains(idx) {
                return true;
            }
            let _ = child.kill();
            let _ = child.wait();
            false
        });
        if !wanted {
            return;
        }
        for &idx in streams {
            if self.readers.len() >= MAX_STREAMS {
                break;
            }
            if self.readers.contains_key(&idx) || self.outgoing.is_full() {
                continue;
            }
            // ~1 s of headroom per stream.
            let (producer, consumer) = RingBuffer::new(self.sample_rate as usize * 2);
            if let Ok(child) = spawn_reader(idx, self.sample_rate, producer) {
                let _ = self.outgoing.push(consumer);
                self.readers.insert(idx, child);
            }
        }
    }

    /// Sink-inputs with a running reader.
    pub fn capturing(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.readers.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

impl Drop for CaptureFeed {
    fn drop(&mut self) {
        for (_, mut c) in self.readers.drain() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_reader(index: u64, sample_rate: u32, producer: Producer<f32>) -> std::io::Result<Child> {
    let mut child = Command::new("parec")
        .args([
            &format!("--monitor-stream={index}"),
            "--format=float32le",
            &format!("--rate={sample_rate}"),
            "--channels=2",
            "--raw",
            &format!("--latency-msec={CHUNK_MS}"),
            "--client-name=kabelsalat-capture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("parec stdout piped");
    std::thread::Builder::new()
        .name(format!("kabelsalat-capture-{index}"))
        .spawn(move || pump(stdout, producer))?;
    Ok(child)
}

/// Parks in a blocking read (rule 47); ends, and so marks the ring as
/// abandoned, when `parec` exits.
fn pump(mut stdout: std::process::ChildStdout, mut producer: Producer<f32>) {
    // Whole stereo frames only: a pipe read can end mid-sample.
    let mut buf = [0u8; 8192];
    let mut held = 0;
    loop {
        match stdout.read(&mut buf[held..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let len = held + n;
                let whole = len - len % 8;
                // A full ring drops whole frames; real-time capture can't block.
                if producer.slots() >= whole / 4 {
                    for b in buf[..whole].chunks_exact(4) {
                        let _ = producer.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                    }
                }
                buf.copy_within(whole..len, 0);
                held = len - whole;
            }
        }
    }
}

/// Power of `freq` in a mono signal, as the amplitude of a sine that would
/// give it (Goertzel).
fn tone_amplitude(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
    let w = std::f32::consts::TAU * freq / sample_rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = x as f64 + coeff as f64 * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff as f64 * s1 * s2;
    (power.max(0.0).sqrt() * 2.0 / samples.len().max(1) as f64) as f32
}

/// `tone_amplitude` per 50 ms window (a whole number of cycles of both test
/// tones), sorted: one long window would read a tone low whenever a cut
/// backlog or a moved stream shifted its phase.
fn tone_levels(samples: &[f32], sample_rate: f32, freq: f32) -> Vec<f32> {
    let mut v: Vec<f32> = samples.chunks_exact((sample_rate * 0.05) as usize).map(|w| tone_amplitude(w, sample_rate, freq)).collect();
    v.sort_by(f32::total_cmp);
    v
}

/// `kabelsalat capture-test`: the capture against the machine's real
/// PipeWire. It opens a real output stream that plays a 1 kHz tone (our own
/// output) and starts a second process that plays 440 Hz (another app), both
/// to one null sink, then reads the capture the way the engine does. The
/// other app must be in it and our own tone must not, also after our stream
/// moved to another output, after the default output changed, and after the
/// other app moved. The sink's monitor is recorded beside it as the control:
/// it holds both tones, which is the feedback path of #27. Two null sinks are
/// loaded for the test and unloaded at the end, so nothing is heard.
pub fn run_test() -> anyhow::Result<()> {
    use crate::audio_out::selftest::{pactl, tone_app, until, Cleanup};
    use crate::audio_out::{find_our_stream, scan_and_apply, Target};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::time::Duration;

    const RATE: u32 = 48000;
    const OURS_HZ: f32 = 1000.0;
    const THEIRS_HZ: f32 = 440.0;

    let info = pactl(&["info"]).ok_or_else(|| anyhow::anyhow!("pactl info failed: no PipeWire/PulseAudio server"))?;
    println!("server: {}", info.lines().find(|l| l.starts_with("Server Name")).unwrap_or("?"));
    let mut cleanup = Cleanup::new();
    // Names of this run only: other sessions test on the same server.
    let pid = std::process::id();
    let (home, other, app) = (&format!("kabelsalat_capture_home_{pid}"), &format!("kabelsalat_capture_other_{pid}"), &format!("Test Browser {pid}"));
    cleanup.null_sink(home, "Capture-Home")?;
    cleanup.null_sink(other, "Capture-Other")?;
    cleanup.set_default_sink(home)?;

    // Our own output, the way the engine opens it, playing a 1 kHz tone.
    let device = cpal::default_host().default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let (channels, out_rate) = (config.channels as usize, config.sample_rate as f32);
    let mut phase = 0.0f32;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                phase = (phase + OURS_HZ / out_rate).fract();
                frame.fill((phase * std::f32::consts::TAU).sin() * 0.5);
            }
        },
        |_err: cpal::Error| {},
        None,
    )?;
    stream.play()?;
    cleanup.app = Some(tone_app(home, app, "(2) Backing track - Video", THEIRS_HZ)?);

    let our_stream = || {
        find_our_stream(
            &pactl(&["-f", "json", "list", "sink-inputs"])?,
            &pactl(&["-f", "json", "list", "clients"])?,
            pid,
        )
    };
    let (ours, _) = until("our own stream in pactl's list", our_stream)?;
    let theirs = until("the other app's stream", || {
        scan_and_apply(Target::Follow).streams.into_iter().find(|s| &s.app == app)
    })?;
    println!("streams: ours = sink-input {ours}, the other app = sink-input {}", theirs.index);

    let sink_of = |input: u64| -> Option<String> {
        let inputs: serde_json::Value = serde_json::from_str(&pactl(&["-f", "json", "list", "sink-inputs"])?).ok()?;
        let sink = inputs.as_array()?.iter().find(|i| i.get("index").and_then(|v| v.as_u64()) == Some(input))?.get("sink")?.as_u64()?;
        let sinks: serde_json::Value = serde_json::from_str(&pactl(&["-f", "json", "list", "sinks"])?).ok()?;
        sinks.as_array()?.iter().find(|s| s.get("index").and_then(|v| v.as_u64()) == Some(sink))?.get("name")?.as_str().map(str::to_string)
    };
    let move_to = |input: u64, sink: &str| {
        until("the stream on its new sink", || {
            let _ = pactl(&["move-sink-input", &input.to_string(), sink]);
            (sink_of(input).as_deref() == Some(sink)).then_some(())
        })
    };
    // The session manager puts a stream back on the sink a stream of its
    // name was last moved to, so both are placed here.
    move_to(ours, home)?;
    move_to(theirs.index, home)?;

    // Stereo to mono.
    fn mono(stereo: &[f32]) -> Vec<f32> {
        stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect()
    }
    // Record `secs` from a Pulse source by name (the control measurement).
    let record = |source: &str, secs: f32| -> anyhow::Result<Vec<f32>> {
        let mut child = Command::new("parec")
            .args([&format!("--device={source}"), "--format=float32le", &format!("--rate={RATE}"), "--channels=2", "--raw"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let mut out = child.stdout.take().expect("piped stdout");
        let mut bytes = vec![0u8; (RATE as f32 * secs) as usize * 8];
        let read = out.read_exact(&mut bytes);
        let _ = child.kill();
        let _ = child.wait();
        read?;
        Ok(mono(&bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect::<Vec<_>>()))
    };

    // Control: the old capture source. Both tones are in the sink's monitor.
    let monitor = record(&format!("{home}.monitor"), 1.5)?;
    let (m_ours, m_theirs) = (tone_levels(&monitor, RATE as f32, OURS_HZ)[15], tone_levels(&monitor, RATE as f32, THEIRS_HZ)[15]);
    println!("control: {home}.monitor holds our tone at {m_ours:.3} and the other app at {m_theirs:.3}");
    anyhow::ensure!(m_ours > 0.2 && m_theirs > 0.2,
        "the control must hold both tones, or this test proves nothing (ours {m_ours}, theirs {m_theirs})");

    // The capture, driven like the app drives it: one scan pass, then the
    // feed, then the engine's block-sized reads.
    let (mut capture, mut feed) = system_capture(RATE);
    // Of the scan's targets only this test's app is recorded: on a shared
    // server other programs play too, and what they play is not known.
    let pass = |feed: &mut CaptureFeed| {
        let st = scan_and_apply(Target::Follow);
        let mine: Vec<u64> = st.capturable.iter().copied().filter(|i| *i == theirs.index).collect();
        feed.sync(&mine);
        st
    };
    let listen = |capture: &mut SystemCapture, secs: f32| -> Vec<f32> {
        let mut block = [0.0f32; 512];
        let mut heard = Vec::new();
        let blocks = (RATE as f32 * secs / 256.0) as usize;
        // The engine's pace: one 256-frame block per 5.3 ms. A sleep that
        // ran long on a busy machine is not made up for with a burst of
        // reads, which no audio callback does.
        let period = Duration::from_secs_f64(256.0 / RATE as f64);
        let mut due = std::time::Instant::now();
        for _ in 0..blocks {
            capture.read(&mut block);
            heard.extend_from_slice(&block);
            due = (due + period).max(std::time::Instant::now());
            std::thread::sleep(due - std::time::Instant::now());
        }
        mono(&heard)
    };
    let check = |what: &str, capture: &mut SystemCapture, feed: &mut CaptureFeed| -> anyhow::Result<()> {
        let st = pass(feed);
        anyhow::ensure!(!st.capturable.contains(&ours), "{what}: our own stream {ours} is a capture target: {:?}", st.capturable);
        anyhow::ensure!(feed.capturing() == vec![theirs.index], "{what}: the other app {} is a capture target and has a reader, got targets {:?}, readers {:?}", theirs.index, st.capturable, feed.capturing());
        listen(capture, 0.5);
        let heard = listen(capture, 1.5);
        // The median window of each: a dropout on a busy machine splatters
        // the other app's tone across the spectrum for that one window, while
        // feedback would hold our tone in all of them (0.5 in the control).
        let c_ours = tone_levels(&heard, RATE as f32, OURS_HZ)[15];
        let c_theirs = tone_levels(&heard, RATE as f32, THEIRS_HZ)[15];
        let silent_ms = heard.chunks(48).filter(|w| w.iter().all(|s| *s == 0.0)).count();
        anyhow::ensure!(c_ours < 0.002, "{what}: our own output is in the capture at {c_ours} (feedback)");
        anyhow::ensure!(c_theirs > 0.4, "{what}: the other app must be captured near its 0.5 peak, got {c_theirs} ({silent_ms} ms silent)");
        println!("ok: {what}: the other app at {c_theirs:.3}, our own tone at {c_ours:.5}, {silent_ms} ms of 1500 silent");
        Ok(())
    };

    // 1. Muted (the default): no reader runs at all.
    pass(&mut feed);
    anyhow::ensure!(feed.capturing().is_empty(), "a muted capture runs no reader, got {:?}", feed.capturing());
    anyhow::ensure!(listen(&mut capture, 0.2).iter().all(|s| *s == 0.0), "a muted capture is silent");
    println!("ok: muted, no reader runs");

    // 2. Mixed in, both streams on the same sink: the #27 case.
    capture.set_wanted(true);
    check("both on one output", &mut capture, &mut feed)?;

    // 3. Our stream moves to another output.
    move_to(ours, other)?;
    check("our output moved to the other sink", &mut capture, &mut feed)?;

    // 4. The other app follows us there, and the default output changes too.
    move_to(theirs.index, other)?;
    cleanup.set_default_sink(other)?;
    check("both on the other sink, now the default", &mut capture, &mut feed)?;

    // 5. Our output stream is reopened (a buffer-size change does that): the
    // new stream has a new index and is not a target either.
    drop(stream);
    let device = cpal::default_host().default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let (channels, out_rate) = (config.channels as usize, config.sample_rate as f32);
    let mut phase = 0.0f32;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                phase = (phase + OURS_HZ / out_rate).fract();
                frame.fill((phase * std::f32::consts::TAU).sin() * 0.5);
            }
        },
        |_err: cpal::Error| {},
        None,
    )?;
    stream.play()?;
    let (reopened, _) = until("our reopened stream in pactl's list", || our_stream().filter(|(i, _)| *i != ours))?;
    let st = pass(&mut feed);
    anyhow::ensure!(!st.capturable.contains(&reopened), "our reopened stream {reopened} is a capture target: {:?}", st.capturable);
    let heard = { listen(&mut capture, 0.5); listen(&mut capture, 1.5) };
    let c_ours = tone_levels(&heard, RATE as f32, OURS_HZ)[15];
    anyhow::ensure!(c_ours < 0.002, "after a reopen our own output is in the capture at {c_ours}");
    println!("ok: our output reopened as sink-input {reopened}: our own tone at {c_ours:.5}");

    // 6. A helper process we start ourselves plays to the same output: it
    // is our own sound, and no target.
    let mut helper = Command::new("pacat")
        .args(["--playback", &format!("--device={other}"), "--raw", "--format=s16le", "--rate=48000", "--channels=1",
            &format!("--client-name=Helper {pid}"), "/dev/zero"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    let helper_name = format!("Helper {pid}");
    let listed = until("the helper's stream in the scan", || {
        let st = scan_and_apply(Target::Follow);
        let s = st.streams.iter().find(|s| s.app == helper_name)?.index;
        Some((s, st.capturable))
    });
    let _ = helper.kill();
    let _ = helper.wait();
    let (helper_stream, targets) = listed?;
    anyhow::ensure!(!targets.contains(&helper_stream), "a process we started is a capture target: {helper_stream} in {targets:?}");
    anyhow::ensure!(targets.contains(&theirs.index), "while the other app still is: {targets:?}");
    println!("ok: a helper process of ours (sink-input {helper_stream}) is no target");

    // 7. The other app stops: its reader goes and the capture falls silent.
    if let Some(app) = cleanup.app.take() { app.stop(); }
    until("the reader of the ended stream stopping", || { pass(&mut feed); feed.capturing().is_empty().then_some(()) })?;
    listen(&mut capture, 0.3);
    anyhow::ensure!(listen(&mut capture, 0.3).iter().all(|s| *s == 0.0), "with no other app playing the capture is silent");
    println!("ok: the other app stopped, its reader is gone and the capture is silent");

    // 8. Muted again while an app plays: the reader stops.
    cleanup.app = Some(tone_app(other, app, "Second video", THEIRS_HZ)?);
    until("a reader for the new stream", || {
        let st = scan_and_apply(Target::Follow);
        let new: Vec<u64> = st.streams.iter().filter(|s| &s.app == app).map(|s| s.index).filter(|i| st.capturable.contains(i)).collect();
        feed.sync(&new);
        (!feed.capturing().is_empty()).then_some(())
    })?;
    capture.set_wanted(false);
    pass(&mut feed);
    anyhow::ensure!(feed.capturing().is_empty(), "muting stops the readers, got {:?}", feed.capturing());
    println!("ok: muted again, the reader stopped");

    drop(stream);
    println!("capture-test: PASS");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture with one stream attached; returns that stream's write end.
    fn with_stream(capture: &mut SystemCapture, feed: &mut CaptureFeed) -> Producer<f32> {
        let (producer, consumer) = RingBuffer::new(48000 * 2);
        feed.outgoing.push(consumer).unwrap();
        capture.read(&mut [0.0; 2]);
        producer
    }

    fn push(p: &mut Producer<f32>, frames: usize, v: f32) {
        for _ in 0..frames * 2 {
            p.push(v).unwrap();
        }
    }

    #[test]
    fn streams_are_summed_and_a_dry_one_waits_for_its_buffer() {
        let (mut capture, mut feed) = system_capture(48000);
        let mut a = with_stream(&mut capture, &mut feed);
        let mut b = with_stream(&mut capture, &mut feed);
        let mut block = [9.0f32; 512];

        // Below the 60 ms start buffer nothing is heard yet.
        push(&mut a, 1000, 0.25);
        capture.read(&mut block);
        assert!(block.iter().all(|s| *s == 0.0), "a stream waits until it is primed");

        push(&mut a, 2880, 0.25);
        push(&mut b, 2880, 0.5);
        capture.read(&mut block);
        assert!(block.iter().all(|s| (*s - 0.75).abs() < 1e-6), "both streams, summed: {}", block[0]);

        // `b` runs dry: it drops out whole and comes back once it is primed.
        // (2880 frames are 11 blocks and a quarter.)
        for _ in 0..11 {
            capture.read(&mut block);
            push(&mut a, 256, 0.25);
        }
        capture.read(&mut block);
        assert!(block.iter().all(|s| (*s - 0.25).abs() < 1e-6), "only the stream that still delivers: {}", block[0]);
        push(&mut b, 256, 0.5);
        capture.read(&mut block);
        assert!(block.iter().all(|s| (*s - 0.25).abs() < 1e-6), "a block's worth is not enough to restart");
        push(&mut b, 2880, 0.5);
        push(&mut a, 512, 0.25);
        capture.read(&mut block);
        assert!(block.iter().all(|s| (*s - 0.75).abs() < 1e-6), "primed again: {}", block[0]);
    }

    #[test]
    fn a_backlog_is_cut_in_whole_stereo_frames() {
        let (mut capture, mut feed) = system_capture(48000);
        let mut p = with_stream(&mut capture, &mut feed);
        // 600 ms waiting, left = +frame number, right = -frame number.
        for f in 0..28800 {
            p.push(f as f32).unwrap();
            p.push(-(f as f32)).unwrap();
        }
        let mut block = [0.0f32; 512];
        capture.read(&mut block);
        assert!(block[0] > 0.0 && block[1] == -block[0], "left and right keep their sides: {:?}", &block[..2]);
        // What is heard now is 60 ms old, not 600.
        assert_eq!(block[0], (28800 - 2880) as f32);
    }

    #[test]
    fn an_ended_stream_plays_out_and_its_ring_goes_back_to_the_feed() {
        let (mut capture, mut feed) = system_capture(48000);
        let mut p = with_stream(&mut capture, &mut feed);
        push(&mut p, 3000, 0.5);
        drop(p);
        let mut block = [0.0f32; 512];
        let mut heard = 0;
        for _ in 0..20 {
            capture.read(&mut block);
            heard += block.iter().filter(|s| **s == 0.5).count();
        }
        assert_eq!(heard, 6000, "everything it delivered is heard");
        assert!(capture.slots.iter().all(Option::is_none), "the slot is free again");
        assert!(feed.retired.pop().is_ok(), "the ring is freed by the feed, not on the audio thread");
    }

    #[test]
    fn more_streams_than_slots_wait_for_a_free_one() {
        let (mut capture, mut feed) = system_capture(48000);
        let mut producers = Vec::new();
        for _ in 0..MAX_STREAMS + 2 {
            let (p, c) = RingBuffer::<f32>::new(64);
            feed.outgoing.push(c).unwrap();
            producers.push(p);
        }
        capture.read(&mut [0.0; 2]);
        assert_eq!(capture.slots.iter().filter(|s| s.is_some()).count(), MAX_STREAMS);
        assert_eq!(capture.incoming.slots(), 2, "the rest stay queued");
        producers.remove(0);
        capture.read(&mut [0.0; 2]);
        capture.read(&mut [0.0; 2]);
        assert_eq!(capture.incoming.slots(), 1, "a freed slot takes the next one");
    }

    #[test]
    fn a_muted_capture_starts_no_reader() {
        let (_capture, mut feed) = system_capture(48000);
        // Would spawn `parec` if the capture were wanted (rule 48: tests
        // never touch the machine's audio).
        feed.sync(&[7, 8]);
        assert!(feed.capturing().is_empty());
    }

    #[test]
    fn a_tone_reads_at_its_amplitude() {
        let tone: Vec<f32> = (0..48000).map(|i| (i as f32 * std::f32::consts::TAU * 440.0 / 48000.0).sin() * 0.5).collect();
        assert!((tone_amplitude(&tone, 48000.0, 440.0) - 0.5).abs() < 0.01);
        assert!(tone_amplitude(&tone, 48000.0, 1000.0) < 0.001);
    }
}
