//! Output-stream recovery (#402): a bumped USB cable, a restarted sound
//! server or a patchbay that pulled our links must not need a restart.
//!
//! The [`Supervisor`] owns the live output stream. Its own thread (rule 48:
//! device I/O never runs per UI frame, and frames stop while the window is
//! hidden) watches the stream's [`Health`]: a fault the backend reported, or
//! a render callback that stopped coming. A lost stream is dropped there and
//! opened again with the parameters it ran with (backend, device, sample
//! rate, buffer size) until it plays again. The UI only drains [`Event`]s
//! for its notice; it never waits for a device.
//!
//! What PipeWire does on its own is left to it: a chosen sink that goes
//! away moves the stream to the fallback sink, and the output scan
//! (`audio_out::scan_and_apply`, rule 159) moves it back when the sink
//! returns. This module covers what was left: the stream itself dying, and
//! a stream nobody drives any more.

use crate::audio_backend::{ActiveBackend, AudioBackendChoice};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Milliseconds on one process-wide monotonic clock (never 0 after the
/// first call, so 0 can mean "never").
pub fn now_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// What one open stream tells its watcher. A new one per open, so a late
/// callback of a dropped stream never speaks for its successor.
#[derive(Default)]
pub struct Health {
    lost: AtomicBool,
    reason: Mutex<Option<String>>,
    last_cb_ms: AtomicU64,
    callbacks: AtomicU64,
}

impl Health {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// From the render callback: two atomic stores, nothing else.
    #[inline]
    pub fn beat(&self) {
        self.last_cb_ms.store(now_ms(), Ordering::Relaxed);
        self.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// From a backend's error path: the stream will not play again.
    pub fn mark_lost(&self, why: &str) {
        if let Ok(mut r) = self.reason.try_lock() {
            r.get_or_insert_with(|| why.to_string());
        }
        self.lost.store(true, Ordering::Release);
    }

    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    fn reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|r| r.clone())
    }
}

/// What a stream was opened with, and is opened with again.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub choice: AudioBackendChoice,
    pub target: Option<String>,
    pub sample_rate: f64,
    pub buffer_frames: u32,
}

/// An open stream of any kind (`S` is `audio_backend::OutputStream` in the
/// app and a stand-in in tests).
pub struct Opened<S> {
    pub stream: S,
    pub backend: ActiveBackend,
    pub sample_rate: f64,
    pub buffer_frames: u32,
    pub device_label: String,
}

/// A stream the supervisor can ask to put back connections it lost while
/// it kept running (JACK ports another client disconnected). Returns what
/// it did, for the notice.
pub trait Supervised: Send + 'static {
    fn mend_routing(&mut self) -> Option<String> {
        None
    }
}

pub type Opener<S> = Box<dyn FnMut(&Params, Arc<Health>) -> anyhow::Result<Opened<S>> + Send>;

/// What is open right now, as the UI shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub backend: ActiveBackend,
    pub device_label: String,
    pub sample_rate: f64,
    pub buffer_frames: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The stream stopped; the supervisor is opening it again.
    Lost { device: String, reason: String },
    /// It plays again. `changed` says what differs from before, if anything.
    Recovered { info: Info, after_ms: u64, changed: Option<String> },
    /// Connections were put back while the stream kept running.
    Mended(String),
}

impl Event {
    /// The one-line stage notice for this event.
    pub fn notice(&self) -> String {
        match self {
            Event::Lost { device, reason } => format!("Audio output lost ({device}: {reason}) — reconnecting…"),
            Event::Recovered { info, after_ms, changed } => {
                let secs = *after_ms as f32 / 1000.0;
                match changed {
                    Some(c) => format!("Audio output is back on {} after {secs:.1} s — {c}", info.device_label),
                    None => format!("Audio output is back on {} after {secs:.1} s", info.device_label),
                }
            }
            Event::Mended(what) => format!("Audio routing restored: {what}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How often the watcher looks.
    pub poll_ms: u64,
    /// A running stream without a callback for this long is lost.
    pub stall_ms: u64,
    /// A reopened stream that is not driven is replaced after this long.
    pub idle_reopen_ms: u64,
    /// Longest wait between two attempts.
    pub max_backoff_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self { poll_ms: 200, stall_ms: 2000, idle_reopen_ms: 5000, max_backoff_ms: 2000 }
    }
}

/// Wait before attempt `n` (0 = the first retry): 250 ms doubling to the cap.
pub fn backoff_ms(attempt: u32, max: u64) -> u64 {
    (250u64 << attempt.min(8)).min(max)
}

fn explicit(b: ActiveBackend) -> AudioBackendChoice {
    match b {
        ActiveBackend::PipeWire => AudioBackendChoice::PipeWire,
        ActiveBackend::Jack => AudioBackendChoice::Jack,
        ActiveBackend::Alsa => AudioBackendChoice::Alsa,
    }
}

struct Slot<S> {
    stream: Option<S>,
    health: Arc<Health>,
    params: Params,
    info: Info,
    opener: Opener<S>,
    opened_at: u64,
    /// When the stream was lost; None while it plays.
    lost_at: Option<u64>,
    attempts: u32,
    next_try: u64,
    last_error: Option<String>,
    /// Stall checks start over from here (after the watcher itself slept).
    grace_from: u64,
    /// What played before the loss, to say what a recovery changed.
    was: Option<Info>,
}

struct Shared<S> {
    slot: Mutex<Slot<S>>,
    events: Mutex<VecDeque<Event>>,
    lost: AtomicBool,
    recoveries: AtomicU32,
    stop: AtomicBool,
    timing: Timing,
}

pub struct Supervisor<S: Supervised> {
    shared: Arc<Shared<S>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<S: Supervised> Supervisor<S> {
    /// Take over a stream that was just opened with `params` and `health`.
    pub fn start(opened: Opened<S>, health: Arc<Health>, params: Params, opener: Opener<S>, timing: Timing) -> Self {
        let mut sup = Self::idle(opened, health, params, opener, timing);
        let shared = sup.shared.clone();
        sup.thread = std::thread::Builder::new()
            .name("kabelsalat-audio-watch".into())
            .spawn(move || {
                let mut prev = now_ms();
                while !shared.stop.load(Ordering::Acquire) {
                    // Parked between looks (rule 47); `drop` unparks.
                    std::thread::park_timeout(std::time::Duration::from_millis(shared.timing.poll_ms));
                    if shared.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let now = now_ms();
                    // The watcher itself stood still (system suspend, a
                    // stopped process): the callback's age says nothing yet.
                    let slept = now.saturating_sub(prev) > shared.timing.poll_ms + 1000;
                    prev = now;
                    step(&shared, now, slept);
                }
            })
            .map_err(|e| eprintln!("audio: stream watcher failed to start: {e}"))
            .ok();
        sup
    }

    /// The same without a thread: tests call [`Self::step_at`].
    fn idle(opened: Opened<S>, health: Arc<Health>, params: Params, opener: Opener<S>, timing: Timing) -> Self {
        let now = now_ms();
        let info = Info {
            backend: opened.backend,
            device_label: opened.device_label,
            sample_rate: opened.sample_rate,
            buffer_frames: opened.buffer_frames,
        };
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                stream: Some(opened.stream),
                health,
                params,
                info,
                opener,
                opened_at: now,
                lost_at: None,
                attempts: 0,
                next_try: 0,
                last_error: None,
                grace_from: now,
                was: None,
            }),
            events: Mutex::new(VecDeque::new()),
            lost: AtomicBool::new(false),
            recoveries: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            timing,
        });
        Self { shared, thread: None }
    }

    #[cfg(test)]
    fn step_at(&self, now: u64) {
        step(&self.shared, now, false);
    }

    /// Is the output down right now (the watcher is reopening it)?
    pub fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Acquire)
    }

    /// How often a lost stream came back in this run.
    pub fn recoveries(&self) -> u32 {
        self.shared.recoveries.load(Ordering::Relaxed)
    }

    /// What happened since the last call (oldest first).
    pub fn take_events(&self) -> Vec<Event> {
        self.shared.events.lock().map(|mut e| e.drain(..).collect()).unwrap_or_default()
    }

    pub fn info(&self) -> Option<Info> {
        self.shared.slot.lock().ok().map(|s| s.info.clone())
    }

    /// Callbacks of the stream that is open now (0 = none, or not driven).
    pub fn callbacks(&self) -> u64 {
        self.shared.slot.lock().map(|s| if s.stream.is_some() { s.health.callbacks() } else { 0 }).unwrap_or(0)
    }

    /// A change the user asked for: close the stream, change the parameters
    /// and open it again. When that fails the previous parameters are opened
    /// again; when those fail too the watcher keeps trying them.
    pub fn reopen_with(&self, change: impl FnOnce(&mut Params)) -> Result<Info, String> {
        let mut guard = self.shared.slot.lock().map_err(|_| "audio stream state is poisoned".to_string())?;
        let slot = &mut *guard;
        let prev = slot.params.clone();
        drop(slot.stream.take());
        change(&mut slot.params);
        let now = now_ms();
        let wanted = slot.params.clone();
        match open_into(slot, &wanted, now) {
            Ok(()) => {
                self.set_playing(slot);
                Ok(slot.info.clone())
            }
            Err(err) => {
                slot.params = prev.clone();
                if open_into(slot, &prev, now).is_ok() {
                    self.set_playing(slot);
                } else if slot.lost_at.is_none() {
                    slot.lost_at = Some(now);
                    slot.was = None;
                    slot.attempts = 0;
                    slot.next_try = now;
                    self.shared.lost.store(true, Ordering::Release);
                    push_event(&self.shared, Event::Lost { device: slot.info.device_label.clone(), reason: err.clone() });
                }
                Err(err)
            }
        }
    }

    fn set_playing(&self, slot: &mut Slot<S>) {
        slot.lost_at = None;
        slot.attempts = 0;
        slot.last_error = None;
        self.shared.lost.store(false, Ordering::Release);
    }
}

impl<S: Supervised> Drop for Supervisor<S> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
        // The stream stops with its owner, not with the last `Arc`.
        if let Ok(mut slot) = self.shared.slot.lock() {
            drop(slot.stream.take());
        }
    }
}

fn push_event<S>(shared: &Shared<S>, ev: Event) {
    eprintln!("audio: {}", ev.notice());
    if let Ok(mut q) = shared.events.lock() {
        q.push_back(ev);
        while q.len() > 32 {
            q.pop_front();
        }
    }
}

/// Open with `params` into the slot (a new `Health`), keeping what opened.
fn open_into<S>(slot: &mut Slot<S>, params: &Params, now: u64) -> Result<(), String> {
    let health = Health::new();
    match (slot.opener)(params, health.clone()) {
        Ok(o) => {
            slot.stream = Some(o.stream);
            slot.health = health;
            slot.info = Info {
                backend: o.backend,
                device_label: o.device_label,
                sample_rate: o.sample_rate,
                buffer_frames: o.buffer_frames,
            };
            slot.opened_at = now;
            slot.grace_from = now;
            Ok(())
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// One look at the stream: notice a loss, or work on getting it back.
fn step<S: Supervised>(shared: &Shared<S>, now: u64, slept: bool) {
    let Ok(mut guard) = shared.slot.lock() else { return };
    let slot = &mut *guard;
    let t = shared.timing;
    if slept {
        slot.grace_from = now;
    }
    if slot.lost_at.is_none() {
        let since = slot.health.last_cb_ms.load(Ordering::Relaxed).max(slot.opened_at).max(slot.grace_from);
        let why = if slot.health.is_lost() {
            Some(slot.health.reason().unwrap_or_else(|| "the stream ended".into()))
        } else if now.saturating_sub(since) > t.stall_ms {
            Some(format!("no audio callback for {:.1} s", now.saturating_sub(since) as f32 / 1000.0))
        } else {
            None
        };
        let Some(why) = why else {
            if let Some(msg) = slot.stream.as_mut().and_then(S::mend_routing) {
                push_event(shared, Event::Mended(msg));
            }
            return;
        };
        // Dropped here, on the watcher: a close can block on a dead device.
        drop(slot.stream.take());
        slot.was = Some(slot.info.clone());
        slot.lost_at = Some(now);
        slot.attempts = 0;
        slot.next_try = now;
        slot.last_error = None;
        shared.lost.store(true, Ordering::Release);
        push_event(shared, Event::Lost { device: slot.info.device_label.clone(), reason: why });
    }
    let lost_at = slot.lost_at.unwrap_or(now);
    if slot.stream.is_some() {
        // Opened by an earlier attempt: it counts once it is driven.
        if !slot.health.is_lost() && slot.health.callbacks() > 0 {
            let mut changed = Vec::new();
            if let Some(was) = slot.was.take() {
                if (slot.info.sample_rate - was.sample_rate).abs() > 1.0 {
                    changed.push(format!("now {:.0} Hz (was {:.0})", slot.info.sample_rate, was.sample_rate));
                }
                if slot.info.buffer_frames != was.buffer_frames {
                    changed.push(format!("buffer now {} frames (was {})", slot.info.buffer_frames, was.buffer_frames));
                }
            }
            slot.lost_at = None;
            slot.attempts = 0;
            slot.last_error = None;
            shared.lost.store(false, Ordering::Release);
            shared.recoveries.fetch_add(1, Ordering::Relaxed);
            push_event(shared, Event::Recovered {
                info: slot.info.clone(),
                after_ms: now.saturating_sub(lost_at),
                changed: (!changed.is_empty()).then(|| changed.join(", ")),
            });
            return;
        }
        // Open but silent: the server may link it any moment. Replace it
        // only when it failed, or after a while.
        let failed = slot.health.is_lost();
        if !(failed && now >= slot.next_try) && now.saturating_sub(slot.opened_at) < t.idle_reopen_ms {
            return;
        }
        drop(slot.stream.take());
    }
    if now < slot.next_try {
        return;
    }
    // The backend that played, never "Auto": while a sound server restarts,
    // Auto would take the raw ALSA device and keep the server off it.
    let mut params = slot.params.clone();
    params.choice = explicit(slot.info.backend);
    if let Err(e) = open_into(slot, &params, now) {
        if slot.last_error.as_deref() != Some(e.as_str()) {
            eprintln!("audio: reopening {} failed: {e}", slot.info.device_label);
        }
        slot.last_error = Some(e);
    }
    slot.next_try = now + backoff_ms(slot.attempts, t.max_backoff_ms);
    slot.attempts = slot.attempts.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that counts how many are alive.
    struct Fake {
        alive: Arc<AtomicU32>,
        mend: Option<String>,
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            self.alive.fetch_sub(1, Ordering::Relaxed);
        }
    }
    impl Supervised for Fake {
        fn mend_routing(&mut self) -> Option<String> {
            self.mend.take()
        }
    }

    struct World {
        alive: Arc<AtomicU32>,
        /// Does the device exist (can a stream be opened)?
        present: Arc<AtomicBool>,
        /// The health of the stream opened last, to drive or fail it.
        last: Arc<Mutex<Option<Arc<Health>>>>,
        opens: Arc<Mutex<Vec<Params>>>,
        /// What a reopened device reports as its buffer size.
        buffer: Arc<AtomicU32>,
    }

    fn world() -> (World, Supervisor<Fake>) {
        let w = World {
            alive: Arc::new(AtomicU32::new(0)),
            present: Arc::new(AtomicBool::new(true)),
            last: Arc::new(Mutex::new(None)),
            opens: Arc::new(Mutex::new(Vec::new())),
            buffer: Arc::new(AtomicU32::new(128)),
        };
        let (alive, present, last, opens, buffer) = (w.alive.clone(), w.present.clone(), w.last.clone(), w.opens.clone(), w.buffer.clone());
        let mut opener: Opener<Fake> = Box::new(move |p, h| {
            opens.lock().unwrap().push(p.clone());
            anyhow::ensure!(present.load(Ordering::Relaxed), "device is gone");
            alive.fetch_add(1, Ordering::Relaxed);
            *last.lock().unwrap() = Some(h);
            Ok(Opened {
                stream: Fake { alive: alive.clone(), mend: None },
                backend: ActiveBackend::PipeWire,
                sample_rate: p.sample_rate,
                buffer_frames: buffer.load(Ordering::Relaxed),
                device_label: "Interface".into(),
            })
        });
        let params = Params { choice: AudioBackendChoice::Auto, target: Some("iface".into()), sample_rate: 48000.0, buffer_frames: 128 };
        let health = Health::new();
        let opened = opener(&params, health.clone()).unwrap();
        w.opens.lock().unwrap().clear();
        let sup = Supervisor::idle(opened, health, params, opener, Timing::default());
        (w, sup)
    }

    fn beat(w: &World) {
        w.last.lock().unwrap().as_ref().unwrap().beat();
    }

    fn fail(w: &World, why: &str) {
        w.last.lock().unwrap().as_ref().unwrap().mark_lost(why);
    }

    #[test]
    fn a_stream_that_plays_is_left_alone() {
        let (w, sup) = world();
        let t0 = now_ms();
        for i in 0..50 {
            beat(&w);
            // Stamp the test's clock, not the real one.
            w.last.lock().unwrap().as_ref().unwrap().last_cb_ms.store(t0 + i * 200, Ordering::Relaxed);
            sup.step_at(t0 + i * 200);
        }
        assert!(sup.take_events().is_empty());
        assert_eq!(w.alive.load(Ordering::Relaxed), 1);
        assert!(w.opens.lock().unwrap().is_empty(), "nothing was reopened");
    }

    #[test]
    fn a_lost_stream_is_reopened_with_its_parameters_until_the_device_is_back() {
        let (w, sup) = world();
        let t0 = now_ms();
        beat(&w);
        w.present.store(false, Ordering::Relaxed);
        fail(&w, "device unplugged");
        sup.step_at(t0);
        assert!(sup.is_lost());
        assert_eq!(w.alive.load(Ordering::Relaxed), 0, "the dead stream is closed");
        let ev = sup.take_events();
        assert_eq!(ev, vec![Event::Lost { device: "Interface".into(), reason: "device unplugged".into() }]);
        // Gone for a minute: attempts keep coming, spaced by the backoff cap.
        for i in 1..=300 {
            sup.step_at(t0 + i * 200);
        }
        let tries = w.opens.lock().unwrap().len();
        assert!((25..=40).contains(&tries), "about one attempt per 2 s over a minute, got {tries}");
        assert!(sup.take_events().is_empty(), "one notice for one loss, however long it lasts");
        assert!(sup.is_lost());
        // Back: the next attempt opens it, and it counts once it is driven.
        w.present.store(true, Ordering::Relaxed);
        let t1 = t0 + 61_000;
        sup.step_at(t1 + 2000);
        assert_eq!(w.alive.load(Ordering::Relaxed), 1);
        assert!(sup.is_lost(), "open is not yet playing");
        beat(&w);
        sup.step_at(t1 + 2200);
        assert!(!sup.is_lost());
        assert_eq!(sup.recoveries(), 1);
        match &sup.take_events()[..] {
            [Event::Recovered { info, changed: None, .. }] => {
                assert_eq!((info.buffer_frames, info.sample_rate, info.device_label.as_str()), (128, 48000.0, "Interface"));
            }
            other => panic!("expected one Recovered, got {other:?}"),
        }
        for p in w.opens.lock().unwrap().iter() {
            assert_eq!(p.choice, AudioBackendChoice::PipeWire, "the backend that played, never Auto");
            assert_eq!((p.target.as_deref(), p.sample_rate, p.buffer_frames), (Some("iface"), 48000.0, 128));
        }
    }

    #[test]
    fn a_stream_nobody_drives_is_replaced() {
        let (w, sup) = world();
        let t0 = now_ms();
        beat(&w);
        sup.step_at(t0 + 1900);
        assert!(!sup.is_lost(), "inside the stall time");
        sup.step_at(t0 + 2300);
        assert!(sup.is_lost());
        assert!(matches!(&sup.take_events()[..], [Event::Lost { reason, .. }] if reason.starts_with("no audio callback")));
        assert_eq!(w.alive.load(Ordering::Relaxed), 1, "a new stream is open already");
        // The new one is linked and driven: recovered, no second reopen.
        beat(&w);
        sup.step_at(t0 + 2500);
        assert!(!sup.is_lost());
        assert_eq!(w.opens.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_reopened_stream_that_stays_silent_is_kept_for_a_while_and_tells_once() {
        let (w, sup) = world();
        let t0 = now_ms();
        sup.step_at(t0 + 2300);
        assert_eq!(sup.take_events().len(), 1);
        // No sink at all: the stream opens but is never driven.
        for i in 0..100 {
            sup.step_at(t0 + 2500 + i * 200);
        }
        let opens = w.opens.lock().unwrap().len();
        assert!((3..=6).contains(&opens), "one reopen per 5 s over 20 s, got {opens}");
        assert!(sup.take_events().is_empty(), "no notice per attempt");
        assert!(sup.is_lost());
        assert_eq!(w.alive.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_watcher_that_slept_does_not_call_the_stream_stalled() {
        let (w, sup) = world();
        let t0 = now_ms();
        beat(&w);
        // An hour of suspend: the first look after it starts the clock over.
        step(&sup.shared, t0 + 3_600_000, true);
        assert!(!sup.is_lost());
        step(&sup.shared, t0 + 3_600_000 + 1900, false);
        assert!(!sup.is_lost());
        step(&sup.shared, t0 + 3_600_000 + 2300, false);
        assert!(sup.is_lost(), "still silent a stall time later: now it is lost");
        let _ = w;
    }

    #[test]
    fn a_device_that_returns_with_another_buffer_size_says_so() {
        let (w, sup) = world();
        let t0 = now_ms();
        fail(&w, "JACK server shut down");
        w.buffer.store(512, Ordering::Relaxed);
        sup.step_at(t0);
        beat(&w);
        sup.step_at(t0 + 200);
        let ev = sup.take_events();
        assert!(matches!(&ev[..], [Event::Lost { .. }, Event::Recovered { changed: Some(c), .. }] if c == "buffer now 512 frames (was 128)"), "{ev:?}");
        assert!(ev[1].notice().contains("Audio output is back on Interface"), "{}", ev[1].notice());
    }

    #[test]
    fn a_user_change_that_fails_goes_back_and_a_dead_device_is_left_to_the_watcher() {
        let (w, sup) = world();
        // A buffer size the device refuses: the previous one plays on.
        let (present, opens) = (w.present.clone(), w.opens.clone());
        let _ = (present, opens);
        let ok = sup.reopen_with(|p| p.buffer_frames = 256).unwrap();
        assert_eq!(ok.device_label, "Interface");
        assert_eq!(w.alive.load(Ordering::Relaxed), 1);
        assert_eq!(w.opens.lock().unwrap().last().unwrap().buffer_frames, 256);
        // The device is gone when the user changes something: both opens
        // fail, the parameters stay the old ones, and the watcher retries.
        w.present.store(false, Ordering::Relaxed);
        let err = sup.reopen_with(|p| p.buffer_frames = 64).unwrap_err();
        assert_eq!(err, "device is gone");
        assert!(sup.is_lost());
        assert_eq!(w.alive.load(Ordering::Relaxed), 0);
        w.present.store(true, Ordering::Relaxed);
        let t = now_ms();
        sup.step_at(t + 10);
        beat(&w);
        sup.step_at(t + 210);
        assert!(!sup.is_lost());
        assert_eq!(w.opens.lock().unwrap().last().unwrap().buffer_frames, 256, "the size that last worked");
    }

    #[test]
    fn mended_routing_is_reported_and_the_stream_kept() {
        let (w, sup) = world();
        sup.shared.slot.lock().unwrap().stream.as_mut().unwrap().mend = Some("out_1/out_2 → system:playback_1/2".into());
        beat(&w);
        sup.step_at(now_ms());
        assert_eq!(sup.take_events(), vec![Event::Mended("out_1/out_2 → system:playback_1/2".into())]);
        assert_eq!(w.alive.load(Ordering::Relaxed), 1);
        assert!(w.opens.lock().unwrap().is_empty());
    }

    #[test]
    fn dropping_the_supervisor_closes_the_stream_and_ends_the_watcher() {
        let (w, sup) = world();
        drop(sup);
        assert_eq!(w.alive.load(Ordering::Relaxed), 0);
        // With a thread: it ends at once, not after a poll.
        let (w, idle) = world();
        let slot = idle.shared.slot.lock().unwrap().stream.take().unwrap();
        let params = idle.shared.slot.lock().unwrap().params.clone();
        let opened = Opened { stream: slot, backend: ActiveBackend::PipeWire, sample_rate: 48000.0, buffer_frames: 128, device_label: "Interface".into() };
        let sup = Supervisor::start(opened, Health::new(), params, Box::new(|_, _| anyhow::bail!("never")), Timing { poll_ms: 60_000, ..Timing::default() });
        let t = std::time::Instant::now();
        drop(sup);
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(w.alive.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_backoff_doubles_to_its_cap() {
        let waits: Vec<u64> = (0..6).map(|n| backoff_ms(n, 2000)).collect();
        assert_eq!(waits, [250, 500, 1000, 2000, 2000, 2000]);
    }
}

/// `kabelsalat hotplug-test` (#402): the output stream on a real PipeWire,
/// played to a temporary null sink, loses its sink, then its own node (as
/// when the server restarts or a device's driver goes), and comes back each
/// time, playing the same tone at the same rate and buffer without a
/// restart. Then the MIDI half (`ui::hotplug_midi_selftest`).
pub fn run_test() -> anyhow::Result<()> {
    use crate::audio_backend::{self as ab, OutputStream};
    use crate::audio_out::selftest::{pactl, until, Cleanup};
    use crate::audio_out::{capture_sink, tone_amp};
    use std::time::Duration;

    const HZ: f32 = 440.0;
    let mut ran = Vec::new();
    match pactl(&["info"]) {
        None => println!("hotplug-test audio: SKIP — no PipeWire/PulseAudio server (pactl info failed)"),
        Some(info) => {
            println!("server: {}", info.lines().find(|l| l.starts_with("Server Name")).unwrap_or("?"));
            let mut cleanup = Cleanup::new();
            let pid = std::process::id();
            let home = format!("kabelsalat_hotplug_home_{pid}");
            let spare = format!("kabelsalat_hotplug_spare_{pid}");
            let home_module = cleanup.null_sink(&home, "Hotplug-Home")?;
            cleanup.null_sink(&spare, "Hotplug-Spare")?;
            cleanup.set_default_sink(&spare)?;

            let opener: Opener<OutputStream> = Box::new(|p, health| {
                let beat = health.clone();
                let mut phase = 0.0f32;
                let rate = p.sample_rate as f32;
                let armed = Arc::new(AtomicBool::new(false));
                let armed_cb = armed.clone();
                let o = ab::open_stream(
                    p.choice,
                    p.target.as_deref(),
                    Some(p.sample_rate),
                    p.buffer_frames,
                    Box::new(move |buf, ch, _| {
                        for frame in buf.chunks_mut(ch) {
                            phase = (phase + HZ / rate).fract();
                            frame.fill((phase * std::f32::consts::TAU).sin() * 0.25);
                        }
                        beat.beat();
                    }),
                    Box::new(move |f| {
                        if let ab::StreamFault::Lost(m) = f {
                            if armed_cb.load(Ordering::Acquire) {
                                health.mark_lost(&m);
                            }
                        }
                    }),
                )?;
                armed.store(true, Ordering::Release);
                Ok(Opened { stream: o.stream, backend: o.backend, sample_rate: o.sample_rate, buffer_frames: o.buffer_frames, device_label: o.device_label })
            });
            let params = Params { choice: AudioBackendChoice::PipeWire, target: Some(home.clone()), sample_rate: 48000.0, buffer_frames: 256 };
            let mut opener = opener;
            let health = Health::new();
            let opened = opener(&params, health.clone())?;
            let sup = Supervisor::start(opened, health, params, opener, Timing::default());
            let tone_on = |sink: &str| -> anyhow::Result<f32> { Ok(tone_amp(&capture_sink(sink, 0.5)?, HZ)) };
            until("our stream to play", || (sup.callbacks() > 10).then_some(()))?;
            let a = tone_on(&home)?;
            anyhow::ensure!(a > 0.1, "the tone is not on {home} ({a:.4})");
            println!("ok: playing on {home}: tone {a:.3}");

            // 1. The sink goes away (interface unplugged): PipeWire moves the
            //    stream to the default; it keeps playing, no reopen needed.
            cleanup.unload_module(&home_module)?;
            std::thread::sleep(Duration::from_millis(500));
            let s = tone_on(&spare)?;
            anyhow::ensure!(s > 0.1, "with its sink gone the stream is silent ({s:.4} on {spare})");
            anyhow::ensure!(!sup.is_lost(), "a moved stream is not a lost one");
            println!("ok: sink removed: the stream plays on on the fallback sink ({s:.3})");
            // ...and back: the output scan takes it home (rule 159).
            cleanup.null_sink(&home, "Hotplug-Home")?;
            until("the stream back on its sink", || {
                let st = crate::audio_out::scan_and_apply(crate::audio_out::Target::Sink(&home));
                (st.current.as_deref() == Some(home.as_str())).then_some(())
            })?;
            std::thread::sleep(Duration::from_millis(300));
            let a = tone_on(&home)?;
            anyhow::ensure!(a > 0.1, "back on {home} but silent ({a:.4})");
            println!("ok: sink back: the stream is on {home} again ({a:.3})");

            // 2. The stream itself dies: its node is destroyed server-side,
            //    as when PipeWire restarts or a driver goes. Repeated, so a
            //    reopened stream is watched as well as the first.
            for round in 1..=2 {
                let node = our_node_id().ok_or_else(|| anyhow::anyhow!("our PipeWire node is not listed"))?;
                let t0 = std::time::Instant::now();
                let ok = std::process::Command::new("pw-cli").args(["destroy", &node.to_string()])
                    .stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
                anyhow::ensure!(ok, "pw-cli destroy {node} failed");
                until("the loss to be seen", || sup.take_events().into_iter().find(|e| matches!(e, Event::Lost { .. })))
                    .map(|e| println!("ok: round {round}: {}", e.notice()))?;
                let back = until("the stream to come back", || {
                    sup.take_events().into_iter().find_map(|e| match e { Event::Recovered { info, .. } => Some(info), _ => None })
                })?;
                anyhow::ensure!((back.sample_rate, back.buffer_frames) == (48000.0, 256), "came back at {} Hz / {} frames", back.sample_rate, back.buffer_frames);
                anyhow::ensure!(our_node_id().is_some_and(|n| n != node), "no new node after the reopen");
                std::thread::sleep(Duration::from_millis(300));
                let a = tone_on(&home)?;
                anyhow::ensure!(a > 0.1, "recovered but silent on {home} ({a:.4})");
                println!("ok: round {round}: back on {} after {:.2} s at {} Hz / {} frames, tone {a:.3} on {home}",
                    back.device_label, t0.elapsed().as_secs_f32(), back.sample_rate, back.buffer_frames);
            }
            anyhow::ensure!(sup.recoveries() == 2, "{} recoveries", sup.recoveries());
            drop(sup);
            ran.push("audio");
        }
    }
    match crate::ui::hotplug_midi_selftest()? {
        None => ran.push("midi"),
        Some(why) => println!("hotplug-test midi: SKIP — {why}"),
    }
    anyhow::ensure!(!ran.is_empty(), "nothing could run here");
    println!("hotplug-test: OK ({})", ran.join(", "));
    Ok(())
}

/// The PipeWire node id of our output stream (`node.name = kabelsalat`).
fn our_node_id() -> Option<u64> {
    let out = std::process::Command::new("pw-dump").output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pid = std::process::id().to_string();
    v.as_array()?.iter().find(|o| {
        let p = &o["info"]["props"];
        p["node.name"] == "kabelsalat" && p["application.process.id"].as_str().map_or_else(
            || p["application.process.id"].as_u64().map(|n| n.to_string()) == Some(pid.clone()),
            |s| s == pid,
        )
    })?["id"].as_u64()
}
