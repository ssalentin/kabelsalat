//! CLAP plugin hosting via clack-host + clack-extensions. Reference plugin: Surge XT.
//! Owned by the clap-host workstream.
//!
//! Public API: [`ClapVoice`] — a reusable, RT-safe Surge voice.
//! - `ClapVoice::new(sample_rate)` loads Surge XT from ~/.clap, activates it and
//!   returns `(ClapVoice, ClapVoiceMain)`.
//! - `ClapVoice` is `Send`: move it to the audio thread and call
//!   `render(&mut out)` (stereo-interleaved f32, NO allocation/locks inside).
//! - `note_on`/`note_off` push into a lock-free command ring; they lock a mutex
//!   on the *caller* side, so call them from non-RT threads (UI). For cross-thread
//!   use grab a `VoiceController` before moving the voice.
//! - `ClapVoiceMain` stays on the creating (main) thread: state save/restore,
//!   params, deactivation (CLAP [main-thread] rules). Plugin editors are
//!   native egui (parameter lists), never the plugin's own window.
//!
//! `run_test` is a demo built entirely on this API: discover ~/.clap, enumerate
//! params, play an audible arpeggio through the default cpal output,
//! save/restore state.
//!
//! Architecture (matches docs/ARCHITECTURE.md §7):
//! - main thread: plugin instance lifecycle, params/state, and what the plugin
//!   asks of the host (`ClapVoiceMain::service`) (CLAP [main-thread])
//! - audio thread: owns `ClapVoice` (the audio processor), starts and stops
//!   processing, renders blocks (CLAP [audio-thread])
//! - cpal callback: drains a lock-free `rtrb` ring into the device buffer

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window as ClapWindow};
use clack_extensions::posix_fd::{FdFlags, HostPosixFd, HostPosixFdImpl, PluginPosixFd};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_extensions::latency::{HostLatency, HostLatencyImpl, PluginLatency};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_extensions::remote_controls::{PluginRemoteControls, RemoteControlsPageBuffer};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_host::events::event_types::{
    MidiEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent, TransportEvent, TransportFlags,
};
use clack_host::events::{EventFlags, EventHeader, Match};
use clack_host::host::{HostError, HostExtensions};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, SecondsTime};

use crate::main_controls::{self, Candidate, MainControls, RemotePage};
use crate::plugin_window::PluginWindow;
use crate::transport::Transport;

/// Frames per processing block. Small enough for live latency, big enough for Surge.
const BLOCK: usize = 256;
/// Upper bound on output ports (Surge XT: main + Scene A + Scene B = 3).
/// How long the arpeggio demo runs.
const DEMO_SECONDS: u64 = 9;

// ---------------------------------------------------------------------------
// Host callback handlers (CLAP thread model: shared / main-thread / audio)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HostShared {
    // Plugin extensions, queried during initialization.
    params: OnceLock<Option<PluginParams>>,
    state: OnceLock<Option<PluginState>>,
    latency: OnceLock<Option<PluginLatency>>,
    audio_ports: OnceLock<Option<PluginAudioPorts>>,
    remote_controls: OnceLock<Option<PluginRemoteControls>>,
    gui: OnceLock<Option<PluginGui>>,
    timer: OnceLock<Option<PluginTimer>>,
    posix_fd: OnceLock<Option<PluginPosixFd>>,
    // Plugin -> host requests. The plugin may raise them from any thread,
    // its audio thread included, so they are flags and nothing else;
    // `ClapVoiceMain::service` reads them on the main thread.
    callback_requested: AtomicBool,
    restart_requested: AtomicBool,
    flush_requested: AtomicBool,
    // Editor requests. clack dispatches every HostGuiImpl callback to the
    // shared handler (they may come from any thread).
    gui_resize: Mutex<Option<GuiSize>>,
    gui_closed: AtomicBool,
}

impl<'a> SharedHandler<'a> for HostShared {
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.params.set(instance.get_extension());
        let _ = self.state.set(instance.get_extension());
        let _ = self.latency.set(instance.get_extension());
        let _ = self.audio_ports.set(instance.get_extension());
        let _ = self.remote_controls.set(instance.get_extension());
        let _ = self.gui.set(instance.get_extension());
        let _ = self.timer.set(instance.get_extension());
        let _ = self.posix_fd.set(instance.get_extension());
    }
    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::SeqCst);
    }
    /// Nothing to do: a voice a unit plays is processed in every block, and
    /// a voice no unit plays is retired, never put to sleep.
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::SeqCst);
    }
}

impl HostLogImpl for HostShared {
    fn log(&self, severity: LogSeverity, message: &str) {
        // P0: plain stderr. A real host would use a wait-free ring here.
        eprintln!("[plugin:{severity:?}] {message}");
    }
}

impl HostParamsImplShared for HostShared {
    fn request_flush(&self) {
        self.flush_requested.store(true, Ordering::SeqCst);
    }
}

impl HostGuiImpl for HostShared {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        *self.gui_resize.lock().unwrap() = Some(new_size);
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        self.gui_closed.store(true, Ordering::SeqCst);
    }
}

/// A timer the plugin registered (`timer-support`); the UI frame fires it.
struct PluginTimerSlot {
    id: u32,
    period: Duration,
    due: Instant,
}

/// Host shortest timer period: the UI frame drives timers, so faster ones
/// could not be honoured anyway.
const MIN_TIMER_PERIOD: Duration = Duration::from_millis(15);

struct HostMainThread {
    state_dirty: Cell<bool>,
    latency_changed: Cell<bool>,
    params_rescan: Cell<bool>,
    timers: RefCell<Vec<PluginTimerSlot>>,
    next_timer_id: Cell<u32>,
    fds: RefCell<Vec<(i32, FdFlags)>>,
}

impl HostMainThread {
    fn new() -> Self {
        Self {
            state_dirty: Cell::new(false),
            latency_changed: Cell::new(false),
            params_rescan: Cell::new(false),
            timers: RefCell::new(Vec::new()),
            next_timer_id: Cell::new(1),
            fds: RefCell::new(Vec::new()),
        }
    }

    /// Ids of timers that are due, rescheduled for their next tick.
    fn take_due_timers(&self, now: Instant) -> Vec<u32> {
        let mut due = Vec::new();
        for t in self.timers.borrow_mut().iter_mut() {
            if now >= t.due {
                due.push(t.id);
                t.due = now + t.period;
            }
        }
        due
    }
}

impl HostTimerImpl for HostMainThread {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        let id = self.next_timer_id.get();
        self.next_timer_id.set(id.wrapping_add(1).max(1));
        let period = Duration::from_millis(period_ms as u64).max(MIN_TIMER_PERIOD);
        self.timers.borrow_mut().push(PluginTimerSlot { id, period, due: Instant::now() + period });
        Ok(TimerId(id))
    }
    fn unregister_timer(&self, timer_id: TimerId) -> Result<(), HostError> {
        self.timers.borrow_mut().retain(|t| t.id != timer_id.0);
        Ok(())
    }
}

impl HostPosixFdImpl for HostMainThread {
    fn register_fd(&self, fd: i32, flags: FdFlags) -> Result<(), HostError> {
        let mut fds = self.fds.borrow_mut();
        fds.retain(|(f, _)| *f != fd);
        fds.push((fd, flags));
        Ok(())
    }
    fn modify_fd(&self, fd: i32, flags: FdFlags) -> Result<(), HostError> {
        if let Some(e) = self.fds.borrow_mut().iter_mut().find(|(f, _)| *f == fd) {
            e.1 = flags;
        }
        Ok(())
    }
    fn unregister_fd(&self, fd: i32) -> Result<(), HostError> {
        self.fds.borrow_mut().retain(|(f, _)| *f != fd);
        Ok(())
    }
}

impl<'a> MainThreadHandler<'a> for HostMainThread {}

impl HostStateImpl for HostMainThread {
    fn mark_dirty(&self) {
        self.state_dirty.set(true);
    }
}

impl HostLatencyImpl for HostMainThread {
    fn changed(&self) {
        self.latency_changed.set(true);
    }
}

impl HostParamsImplMainThread for HostMainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {
        self.params_rescan.set(true);
    }
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

struct TestHost;

impl HostHandlers for TestHost {
    type Shared<'a> = HostShared;
    type MainThread<'a> = HostMainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostParams>()
            .register::<HostState>()
            .register::<HostLatency>()
            .register::<HostGui>()
            .register::<HostTimer>()
            .register::<HostPosixFd>();
    }
}

#[derive(Copy, Clone)]
struct Exts {
    params: Option<PluginParams>,
    state: Option<PluginState>,
    latency: Option<PluginLatency>,
    audio_ports: Option<PluginAudioPorts>,
    remote_controls: Option<PluginRemoteControls>,
    gui: Option<PluginGui>,
    timer: Option<PluginTimer>,
    posix_fd: Option<PluginPosixFd>,
}

// ---------------------------------------------------------------------------
// ClapVoice — reusable, RT-safe Surge voice (public API)
// ---------------------------------------------------------------------------

/// A command bound for the voice, applied at the next rendered block.
#[derive(Copy, Clone)]
pub enum VoiceCmd {
    NoteOn(u16, f64),
    NoteOff(u16),
    Param(ClapId, f64),
    Midi([u8; 3]),
}

/// Event capacity per block; pushes beyond this are dropped (never allocates
/// inside `render`).
const EVENT_CAP: u32 = 256;
/// Command ring depth (UI -> audio).
const CMD_RING: usize = 1024;

/// Send handle for pushing notes/params into a [`ClapVoice`] from any non-RT
/// thread (e.g. the egui keyboard handler). Cheap to clone.
#[derive(Clone)]
pub struct VoiceController(Arc<Mutex<rtrb::Producer<VoiceCmd>>>);

impl VoiceController {
    /// Press a key (MIDI note number, velocity 0.0..=1.0).
    pub fn note_on(&self, note: u16, velocity: f64) {
        let _ = self.0.lock().unwrap().push(VoiceCmd::NoteOn(note, velocity));
    }
    /// Release a key.
    pub fn note_off(&self, note: u16) {
        let _ = self.0.lock().unwrap().push(VoiceCmd::NoteOff(note));
    }
    /// Set a plugin parameter (plain value) at the next block.
    pub fn set_param(&self, id: ClapId, value: f64) {
        let _ = self.0.lock().unwrap().push(VoiceCmd::Param(id, value));
    }
}

// Named fns (not closures) so the lifetime elision ties input and output.
fn as_mut_slice_f32(b: &mut Vec<f32>) -> &mut [f32] {
    b.as_mut_slice()
}
fn as_variable_input_f32(b: &mut Vec<f32>) -> InputChannel<'_, f32> {
    InputChannel::variable(b.as_mut_slice())
}
macro_rules! in_port {
    ($p:ident) => {
        AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only($p.iter_mut().map(as_variable_input_f32)),
        }
    };
}
macro_rules! out_port {
    ($p:ident) => {
        AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only($p.iter_mut().map(as_mut_slice_f32)),
        }
    };
}

/// Keeps a state save apart from the `process()` call that applies a state
/// load (#29). Surge XT applies a loaded state inside `process()` and its
/// save reads the patch without a lock, so a save on the main thread while
/// that block renders crashed inside the plugin.
///
/// The main side holds the gate for a save while a load is pending. The
/// audio side never waits: a block that finds the gate held is silent and
/// keeps its events for the next one.
#[derive(Default)]
struct StateGate {
    /// The audio side is inside `process()`.
    processing: AtomicBool,
    /// `process()` calls that returned.
    processed: AtomicU64,
    /// The main side wants no `process()` call to run.
    held: AtomicBool,
}

impl StateGate {
    /// Audio side, before `process()`: false while the main side holds the
    /// gate. After true, `leave` follows the call.
    fn enter(&self) -> bool {
        self.processing.store(true, Ordering::SeqCst);
        if self.held.load(Ordering::SeqCst) {
            self.processing.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    fn leave(&self) {
        self.processed.fetch_add(1, Ordering::SeqCst);
        self.processing.store(false, Ordering::SeqCst);
    }

    /// Main side: a count no `process()` call that began before now can
    /// reach or pass without one more call after it. A load enqueued before
    /// this is applied once `processed` is past it.
    fn mark(&self) -> u64 {
        let running = self.processing.load(Ordering::SeqCst) as u64;
        self.processed.load(Ordering::SeqCst) + running
    }

    /// Main side: whether a `process()` call that began after `mark` was
    /// taken has returned.
    fn passed(&self, mark: u64) -> bool {
        self.processed.load(Ordering::SeqCst) > mark
    }

    /// Main side: run `f` with no `process()` call in flight.
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        self.held.store(true, Ordering::SeqCst);
        while self.processing.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let out = f();
        self.held.store(false, Ordering::SeqCst);
        out
    }
}

/// What a voice has only while its plugin is active: the processor, and
/// the tables and buffers sized to what the plugin declared for this
/// activation. A restart (`request_restart`) moves the whole set to the main
/// thread and a new one back, so the audio thread neither allocates nor
/// frees for it.
struct Live {
    processor: PluginAudioProcessor<TestHost>,
    /// Sorted `(id, min, max)`, read once per activation for lock-free CC scaling.
    param_ranges: Vec<(u32, f64, f64)>,
    main_in: Option<usize>,
    main_out: usize,
    /// One buffer set per input port: `main_in` receives the unit's audio
    /// input when the plugin runs as an effect; sidechain ports receive silence.
    in_bufs: Vec<Vec<Vec<f32>>>,
    /// One buffer set per output port; only `main_out` is heard. Drum
    /// machines (OneTrick CRYPTID: 12 ports) put each drum on its own port
    /// and the mix on the main one.
    out_bufs: Vec<Vec<Vec<f32>>>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
}

impl Live {
    /// Activate the inactive `plugin` for `sample_rate` and size everything
    /// to the ports and parameters it declares now. Main thread.
    fn activate(plugin: &mut PluginInstance<TestHost>, exts: Exts, sample_rate: f64) -> Result<Box<Live>> {
        let (in_chs, main_in, out_chs, main_out) = inspect_audio_ports(plugin, exts.audio_ports)?;
        let audio_config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: BLOCK as u32,
        };
        let stopped = plugin.activate(|_, _| (), audio_config).context("activating the plugin")?;
        let mut param_ranges = Vec::new();
        if let Some(params) = exts.params {
            let handle = plugin.plugin_handle();
            let count = params.count(&handle);
            let mut info_buf = ParamInfoBuffer::new();
            param_ranges.reserve(count as usize);
            for i in 0..count {
                if let Some(info) = params.get_info(&handle, i, &mut info_buf) {
                    param_ranges.push((info.id.get(), info.min_value, info.max_value));
                }
            }
            param_ranges.sort_unstable_by_key(|&(id, _, _)| id);
        }
        Ok(Box::new(Live {
            processor: stopped.into(),
            param_ranges,
            main_in,
            main_out,
            in_bufs: in_chs.iter().map(|&chs| (0..chs).map(|_| vec![0.0; BLOCK]).collect()).collect(),
            out_bufs: out_chs.iter().map(|&chs| (0..chs).map(|_| vec![0.0; BLOCK]).collect()).collect(),
            in_ports: AudioPorts::with_capacity(in_chs.iter().sum(), in_chs.len()),
            out_ports: AudioPorts::with_capacity(out_chs.iter().sum(), out_chs.len()),
        }))
    }
}

/// The audio side's end of the restart hand-off (see [`MainLink`]).
struct AudioLink {
    /// The main thread wants the [`Live`] set back, processing stopped.
    stop: Arc<AtomicBool>,
    to_main: rtrb::Producer<Box<Live>>,
    from_main: rtrb::Consumer<Box<Live>>,
}

/// The main thread's end of the restart hand-off: it raises `stop`, the
/// audio side stops processing and sends its [`Live`] set, the main thread
/// deactivates and reactivates the plugin and sends a new set.
struct MainLink {
    stop: Arc<AtomicBool>,
    from_audio: rtrb::Consumer<Box<Live>>,
    to_audio: rtrb::Producer<Box<Live>>,
}

fn restart_link() -> (AudioLink, MainLink) {
    let stop = Arc::new(AtomicBool::new(false));
    let (to_main, from_audio) = rtrb::RingBuffer::new(1);
    let (to_audio, from_main) = rtrb::RingBuffer::new(1);
    (AudioLink { stop: stop.clone(), to_main, from_main }, MainLink { stop, from_audio, to_audio })
}

/// Where a restart the plugin asked for stands, on the main thread.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Restart {
    /// Active, nothing asked.
    Idle,
    /// The audio side was asked for the processor.
    Stopping,
    /// The plugin refused to activate again; it stays inactive until it
    /// asks for another restart.
    Inactive,
}

/// How often the host did what a plugin asked for (`ClapVoiceMain::served`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Served {
    /// `on_main_thread()` calls after `request_callback()`.
    pub callbacks: u64,
    /// Deactivations followed by an activation after `request_restart()`.
    pub restarts: u64,
    /// `params.flush()` calls on the main thread after `request_flush()`
    /// from an inactive plugin. An active one is flushed by `process()`.
    pub flushes: u64,
}

/// A live Surge XT voice: the CLAP audio processor plus preallocated buffers.
///
/// `Send` but `!Sync`: create it on the main thread, then move it to the audio
/// thread. `render` is RT-safe (no allocation, no locks, no syscalls). Notes
/// arrive via a lock-free ring; `note_on`/`note_off` lock a mutex on the caller
/// side and must NOT be called from the render thread — use a
/// [`VoiceController`] from the UI thread instead.
pub struct ClapVoice {
    voice_id: u64,
    plugin_id: String,
    /// True when the plugin declared itself an audio effect (or utility), not
    /// an instrument (`crate::plugins::clap_is_audio_insert`).
    is_effect: bool,
    /// None while a restart has the set on the main thread: silence.
    live: Option<Box<Live>>,
    link: AudioLink,
    cmds_tx: Arc<Mutex<rtrb::Producer<VoiceCmd>>>,
    cmds_rx: rtrb::Consumer<VoiceCmd>,
    /// Preallocated audio-thread command queue (avoids Mutex lock in `GraphEngine`).
    rt_cmds: Vec<VoiceCmd>,
    /// Optional sample-accurate sequence (frame offset, command), e.g. the
    /// host-test arpeggio. Preallocated; consumed with a cursor.
    schedule: Vec<(u64, VoiceCmd)>,
    sched_idx: usize,
    frame: u64,
    sample_rate: f64,
    ev_in: EventBuffer,
    ev_out: EventBuffer,
    process_errors: u64,
    gate: Arc<StateGate>,
    /// What the plugin reported when it was activated, or after a
    /// `host_latency.changed()` poll on the main thread, in frames.
    latency: Arc<std::sync::atomic::AtomicU32>,
}

fn entry_is_effect(entry: &PluginEntry, id_str: &str) -> bool {
    let Some(factory) = entry.get_plugin_factory() else { return false };
    for d in factory.plugin_descriptors() {
        let id = d.id().unwrap_or(c"").to_string_lossy();
        if id == id_str {
            let info = ClapPluginInfo {
                id: id.into_owned(),
                name: d.name().unwrap_or(c"").to_string_lossy().into_owned(),
                vendor: d.vendor().unwrap_or(c"").to_string_lossy().into_owned(),
                features: d.features().map(|f| f.to_string_lossy().into_owned()).collect(),
            };
            return crate::plugins::clap_is_audio_insert(&info);
        }
    }
    false
}

impl ClapVoice {
    /// The plugin's reported latency, in frames. Allocation- and lock-free.
    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Whether this voice hosts a CLAP audio effect (or utility) rather than
    /// an instrument. Allocation- and lock-free.
    pub fn is_effect(&self) -> bool {
        self.is_effect
    }

    /// Load Surge XT from the CLAP search paths, instantiate, activate and start
    /// processing at `sample_rate`. Returns the audio-side voice and the
    /// main-thread handle (state/GUI/deactivation). Call on the main thread.
    pub fn new(sample_rate: f64) -> Result<(ClapVoice, ClapVoiceMain)> {
        let (entry, plugin_id) = discover_surge()?;
        Self::instantiate_entry(entry, plugin_id, sample_rate)
    }

    /// Load any CLAP plugin by `plugin_id` from the CLAP search paths,
    /// instantiate, activate and start processing at `sample_rate`.
    pub fn new_plugin(sample_rate: f64, plugin_id: &str) -> Result<(ClapVoice, ClapVoiceMain)> {
        let (entry, cid) = discover_plugin(plugin_id)?;
        Self::instantiate_entry(entry, cid, sample_rate)
    }

    /// [`Self::new_plugin`] from one bundle file, not the search paths.
    pub fn new_from_bundle(sample_rate: f64, bundle: &std::path::Path, plugin_id: &str) -> Result<(ClapVoice, ClapVoiceMain)> {
        let (entry, cid) = load_bundle(bundle, plugin_id)?;
        Self::instantiate_entry(entry, cid, sample_rate)
    }

    fn instantiate_entry(
        entry: PluginEntry,
        plugin_id: CString,
        sample_rate: f64,
    ) -> Result<(ClapVoice, ClapVoiceMain)> {
        let id_str = plugin_id.to_string_lossy().into_owned();
        // A parked instance of a never-free plugin is reused instead of
        // instantiating a new one next to it (#52, rule 56 class). The fresh
        // `entry` we just loaded is dropped: dlopen is refcounted, and the
        // parked instance keeps its own.
        if let Some(parked) = take_parked_clap(&id_str) {
            println!("reusing parked instance: {}", parked.plugin_id);
            let ParkedClap { plugin_id, instance, exts, _entry } = parked;
            let (voice, mut main) = Self::build_voice(instance, exts, _entry, plugin_id.clone(), sample_rate)?;
            if let Some(init) = init_state(&plugin_id) {
                let _ = main.load_state(&init); // fresh-instance state
            }
            return Ok((voice, main));
        }
        let host_info = HostInfo::new(
            "kabelsalat",
            "kabelsalat",
            "https://github.com/kabelsalat",
            env!("CARGO_PKG_VERSION"),
        )?;
        let mut plugin = PluginInstance::<TestHost>::new(
            |_| HostShared::default(),
            |_| HostMainThread::new(),
            &entry,
            &plugin_id,
            &host_info,
        )
        .with_context(|| format!("instantiating {id_str}"))?;
        println!("instantiated: {id_str}");

        let exts = plugin.access_shared_handler(|h| Exts {
            params: h.params.get().copied().flatten(),
            state: h.state.get().copied().flatten(),
            latency: h.latency.get().copied().flatten(),
            audio_ports: h.audio_ports.get().copied().flatten(),
            remote_controls: h.remote_controls.get().copied().flatten(),
            gui: h.gui.get().copied().flatten(),
            timer: h.timer.get().copied().flatten(),
            posix_fd: h.posix_fd.get().copied().flatten(),
        });
        println!(
            "extensions: params={} state={} latency={} audio-ports={} remote-controls={}",
            exts.params.is_some(),
            exts.state.is_some(),
            exts.latency.is_some(),
            exts.audio_ports.is_some(),
            exts.remote_controls.is_some()
        );

        let (voice, mut main) = Self::build_voice(plugin, exts, entry, id_str.clone(), sample_rate)?;
        if clap_never_free(&id_str) {
            remember_init_state(&id_str, &mut main);
        }
        Ok((voice, main))
    }

    /// Activate `plugin`, start processing and build both halves of the
    /// voice. Shared by fresh instantiation and parked reuse.
    fn build_voice(
        mut plugin: PluginInstance<TestHost>,
        exts: Exts,
        entry: PluginEntry,
        id_str: String,
        sample_rate: f64,
    ) -> Result<(ClapVoice, ClapVoiceMain)> {
        static NEXT_VOICE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // Processing starts with the first rendered block, on the thread
        // that renders (`start_processing` is an [audio-thread] call).
        let live = Live::activate(&mut plugin, exts, sample_rate).with_context(|| format!("activating {id_str}"))?;
        // Surge only reports meaningful latency once active.
        // CLAP lets a plugin change it only while it is being activated,
        // so this value holds for as long as the voice runs.
        let latency = exts.latency.map(|l| l.get(&plugin.plugin_handle())).unwrap_or(0);
        println!("plugin-reported latency (active): {latency} samples");
        let shared_latency = Arc::new(std::sync::atomic::AtomicU32::new(latency));
        let (audio_link, main_link) = restart_link();

        let (cmds_tx, cmds_rx) = rtrb::RingBuffer::<VoiceCmd>::new(CMD_RING);
        let gate = Arc::new(StateGate::default());
        let is_effect = entry_is_effect(&entry, &id_str);
        let voice = ClapVoice {
            voice_id: NEXT_VOICE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            plugin_id: id_str.clone(),
            is_effect,
            live: Some(live),
            link: audio_link,
            cmds_tx: Arc::new(Mutex::new(cmds_tx)),
            cmds_rx,
            rt_cmds: Vec::with_capacity(EVENT_CAP as usize),
            schedule: Vec::new(),
            sched_idx: 0,
            frame: 0,
            sample_rate,
            ev_in: EventBuffer::with_capacity(EVENT_CAP as usize),
            ev_out: EventBuffer::with_capacity(EVENT_CAP as usize),
            process_errors: 0,
            gate: gate.clone(),
            latency: shared_latency.clone(),
        };
        let main_id = id_str;
        let mut main = ClapVoiceMain {
            instance: plugin,
            exts,
            main_controls: MainControls::default(),
            main_controls_rechecks: 0,
            editor: None,
            gate,
            shared_latency,
            load_mark: None,
            link: main_link,
            restart: Restart::Idle,
            served: Served::default(),
            sample_rate,
            _entry: entry,
        };
        main.refresh_main_controls();
        if let Some(state) = crate::aeolus::initial_state(&main_id) {
            main.load_state(&state).with_context(|| format!("starting {main_id} with a registration"))?;
        }
        Ok((voice, main))
    }

    /// Unique id for pairing with `ClapVoiceMain` on retirement.
    pub fn voice_id(&self) -> u64 {
        self.voice_id
    }

    /// CLAP plugin id string for this voice.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// A `Send` controller for UI threads; grab it before moving the voice.
    pub fn controller(&self) -> VoiceController {
        VoiceController(self.cmds_tx.clone())
    }

    /// Press a key. Locks a mutex — call from non-RT threads only.
    pub fn note_on(&self, note: u16, velocity: f64) {
        let _ = self.cmds_tx.lock().unwrap().push(VoiceCmd::NoteOn(note, velocity));
    }

    /// Release a key. Locks a mutex — call from non-RT threads only.
    pub fn note_off(&self, note: u16) {
        let _ = self.cmds_tx.lock().unwrap().push(VoiceCmd::NoteOff(note));
    }

    /// Push a command directly from the audio thread (lock-free, allocation-free).
    pub fn push_rt(&mut self, cmd: VoiceCmd) {
        if self.rt_cmds.len() < self.rt_cmds.capacity() {
            self.rt_cmds.push(cmd);
        }
    }

    /// Native `(min, max)` range for `id`, if reported by the plugin.
    pub fn param_range(&self, id: u32) -> Option<(f64, f64)> {
        let ranges = &self.live.as_ref()?.param_ranges;
        ranges.binary_search_by_key(&id, |&(pid, _, _)| pid).ok().map(|idx| (ranges[idx].1, ranges[idx].2))
    }

    /// Scale a normalized `0..=1` value into the parameter's range and queue it
    /// on the audio thread without locking or allocating.
    pub fn set_param_normalized_rt(&mut self, id: u32, norm: f64) -> Option<f64> {
        let (min, max) = self.param_range(id)?;
        let val = min + norm.clamp(0.0, 1.0) * (max - min);
        self.push_rt(VoiceCmd::Param(ClapId::new(id), val));
        Some(val)
    }

    /// Queue a raw MIDI 1.0 CC event on the audio thread without locking or allocating.
    pub fn send_cc_rt(&mut self, channel: u8, cc: u8, value: u8) {
        self.push_rt(VoiceCmd::Midi([0xB0 | (channel & 0x0F), cc & 0x7F, value & 0x7F]));
    }

    /// Queue one raw MIDI 1.0 channel message on the audio thread without
    /// locking or allocating (a 2-byte message leaves the third byte 0).
    pub fn send_midi_rt(&mut self, msg: [u8; 3]) {
        self.push_rt(VoiceCmd::Midi(msg));
    }

    /// Queue a MIDI programme change on the audio thread without locking or allocating.
    pub fn program_change_rt(&mut self, program: u8) {
        self.push_rt(VoiceCmd::Midi([0xC0, program & 0x7F, 0]));
    }

    /// Install a sample-accurate sequence of (frame offset, command), consumed
    /// as rendering advances. Replaces any previous schedule. Main/setup thread.
    pub fn set_schedule(&mut self, mut schedule: Vec<(u64, VoiceCmd)>) {
        schedule.sort_by_key(|(t, _)| *t);
        self.schedule = schedule;
        self.sched_idx = 0;
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Number of failed CLAP process calls (should stay 0).
    pub fn process_errors(&self) -> u64 {
        self.process_errors
    }

    /// Render `out.len() / 2` stereo-interleaved frames with the host
    /// `transport` as of the first frame. RT-safe: no allocation, no locks,
    /// no syscalls. Call from the audio thread only.
    pub fn render(&mut self, out: &mut [f32], mut transport: Transport) {
        let frames = out.len() / 2;
        let mut done = 0usize;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            self.render_block(n, None, &mut out[done * 2..(done + n) * 2], &transport);
            transport.advance(n, self.sample_rate);
            done += n;
        }
    }

    /// Process `out.len() / 2` stereo-interleaved frames of `inp` into `out`
    /// with the host `transport` as of the first frame (#181). Feeds `inp` to
    /// the plugin's main input port (L and R for stereo, L for mono) and
    /// silence to any sidechain ports; centers a mono main output to L and R.
    /// RT-safe: no allocation, no locks, no syscalls. Audio thread only.
    pub fn process(&mut self, inp: &[f32], out: &mut [f32], mut transport: Transport) {
        let frames = (out.len() / 2).min(inp.len() / 2);
        let mut done = 0usize;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            let range = done * 2..(done + n) * 2;
            self.render_block(n, Some(&inp[range.clone()]), &mut out[range], &transport);
            transport.advance(n, self.sample_rate);
            done += n;
        }
    }

    fn render_block(&mut self, n: usize, inp: Option<&[f32]>, out: &mut [f32], transport: &Transport) {
        self.hand_over();
        // No set (a restart has it) or a plugin that will not start: silence.
        // The commands wait in their queues for the block that renders.
        let Some(live) = self.live.as_deref_mut() else {
            out[..n * 2].fill(0.0);
            return;
        };
        let Live { processor, param_ranges, main_in, main_out, in_bufs, out_bufs, in_ports, out_ports } = live;
        let Ok(processor) = processor.ensure_processing_started() else {
            self.process_errors += 1;
            out[..n * 2].fill(0.0);
            return;
        };
        if !self.gate.enter() {
            out[..n * 2].fill(0.0);
            return;
        }
        self.ev_in.clear();
        self.ev_out.clear();

        // Live commands (block-quantized) + scheduled events (sample-accurate).
        for i in 0..self.rt_cmds.len() {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            push_cmd(&mut self.ev_in, param_ranges, 0, self.rt_cmds[i]);
        }
        self.rt_cmds.clear();
        while let Ok(cmd) = self.cmds_rx.pop() {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            push_cmd(&mut self.ev_in, param_ranges, 0, cmd);
        }
        while self.sched_idx < self.schedule.len()
            && self.schedule[self.sched_idx].0 < self.frame + n as u64
        {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            let (t, cmd) = self.schedule[self.sched_idx];
            push_cmd(&mut self.ev_in, param_ranges, (t - self.frame) as u32, cmd);
            self.sched_idx += 1;
        }

        // Fill input ports: when `inp` is provided, the main input port takes
        // L and R (or L for a mono input, rule 173); every other port
        // (sidechains, instrument sidechains) receives silence.
        let target_in = inp.is_some().then(|| main_in.unwrap_or(0)).filter(|&p| p < in_bufs.len());
        for (port_idx, port_bufs) in in_bufs.iter_mut().enumerate() {
            if Some(port_idx) == target_in {
                let src = inp.unwrap();
                let chs = port_bufs.len();
                if chs >= 2 {
                    for i in 0..n {
                        port_bufs[0][i] = src[i * 2];
                        port_bufs[1][i] = src[i * 2 + 1];
                    }
                    for ch in &mut port_bufs[2..] {
                        ch[..n].fill(0.0);
                    }
                } else if chs == 1 {
                    for i in 0..n {
                        port_bufs[0][i] = src[i * 2];
                    }
                }
            } else {
                for ch in port_bufs.iter_mut() {
                    ch[..n].fill(0.0);
                }
            }
        }
        // Port configs from an iterator (no heap alloc: `in_ports` and
        // `out_ports` hold room for every port and channel).
        let mut input = in_ports.with_input_buffers(in_bufs.iter_mut().map(|p| in_port!(p)));
        let mut output = out_ports.with_output_buffers(out_bufs.iter_mut().map(|p| out_port!(p)));
        // Buffers are BLOCK-sized; process exactly n frames.
        input.truncate(n as u32);
        output.truncate(n as u32);

        let status = processor.process(
            &input,
            &mut output,
            &self.ev_in.as_input(),
            &mut self.ev_out.as_output(),
            Some(self.frame),
            Some(&transport_event(transport)),
        );
        self.gate.leave();
        if status.is_err() {
            self.process_errors += 1;
        }

        // Copy the MAIN output port into `out` (stereo, or mono centered to
        // L and R); auxiliary output ports are rendered but unused. On
        // process failure output silence.
        let failed = status.is_err();
        let main_chs = &out_bufs[*main_out];
        if failed || main_chs.is_empty() {
            out[..n * 2].fill(0.0);
        } else if main_chs.len() >= 2 {
            for i in 0..n {
                out[i * 2] = main_chs[0][i];
                out[i * 2 + 1] = main_chs[1][i];
            }
        } else {
            for i in 0..n {
                let s = main_chs[0][i];
                out[i * 2] = s;
                out[i * 2 + 1] = s;
            }
        }
        self.frame += n as u64;
    }

    /// The audio side of a restart: give the set to the main thread when it
    /// asks, with processing stopped, and adopt the one it sends back.
    /// Moves two boxes; allocates and frees nothing.
    fn hand_over(&mut self) {
        if self.live.is_none() {
            self.live = self.link.from_main.pop().ok();
        }
        // The request stays up until there is a set to give: the main
        // thread may ask again before the last one it sent was adopted.
        if self.live.is_some() && self.link.stop.swap(false, Ordering::SeqCst) {
            let mut live = self.live.take().unwrap();
            live.processor.ensure_processing_stopped();
            if let Err(rtrb::PushError::Full(live)) = self.link.to_main.push(live) {
                // Cannot happen (the main thread asks once and takes the
                // set before it asks again); keep playing if it does.
                self.live = Some(live);
            }
        }
    }

    /// Stop processing and give the processor up for deactivation
    /// (`ClapVoiceMain::retire`). Runs on the main thread once nothing
    /// renders the voice any more, which makes it the plugin's audio thread
    /// for this call. None while a restart has the processor on the main side.
    fn stop(mut self) -> Option<StoppedPluginAudioProcessor<TestHost>> {
        let live = self.live.take().or_else(|| self.link.from_main.pop().ok())?;
        Some(live.processor.into_stopped())
    }
}

/// The CLAP view of the host transport: tempo, time signature, and both timelines.
fn transport_event(t: &Transport) -> TransportEvent {
    let mut flags = TransportFlags::HAS_TEMPO
        | TransportFlags::HAS_BEATS_TIMELINE
        | TransportFlags::HAS_SECONDS_TIMELINE
        | TransportFlags::HAS_TIME_SIGNATURE;
    flags.set(TransportFlags::IS_PLAYING, t.playing);
    let (bar_number, bar_start) = t.bar();
    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags,
        song_pos_beats: BeatTime::from_float(t.beats),
        song_pos_seconds: SecondsTime::from_float(t.seconds),
        tempo: t.bpm,
        tempo_inc: 0.0,
        loop_start_beats: BeatTime::from_int(0),
        loop_end_beats: BeatTime::from_int(0),
        loop_start_seconds: SecondsTime::from_int(0),
        loop_end_seconds: SecondsTime::from_int(0),
        bar_start: BeatTime::from_float(bar_start),
        bar_number,
        time_signature_numerator: t.sig.num as u16,
        time_signature_denominator: t.sig.den as u16,
    }
}

/// Whether the plugin declared parameter `id` (`ranges` is sorted by id).
fn has_param(ranges: &[(u32, f64, f64)], id: u32) -> bool {
    ranges.binary_search_by_key(&id, |&(pid, _, _)| pid).is_ok()
}

/// Queue `cmd` as a CLAP event. A parameter event for an id the plugin did
/// not declare is dropped: CLAP allows only declared ids, and Surge XT
/// dereferences the parameter it did not find (#29).
fn push_cmd(ev_in: &mut EventBuffer, params: &[(u32, f64, f64)], time: u32, cmd: VoiceCmd) {
    match cmd {
        VoiceCmd::Param(id, _) if !has_param(params, id.get()) => {}
        VoiceCmd::NoteOn(k, v) => {
            ev_in.push(&NoteOnEvent::new(time, Pckn::new(0u16, 0u16, k, k as u32), v))
        }
        VoiceCmd::NoteOff(k) => {
            ev_in.push(&NoteOffEvent::new(time, Pckn::new(0u16, 0u16, k, k as u32), 0.5))
        }
        VoiceCmd::Param(id, v) => ev_in.push(&ParamValueEvent::new(
            time,
            id,
            Pckn::new(
                Match::<u16>::All,
                Match::<u16>::All,
                Match::<u16>::All,
                Match::<u32>::All,
            ),
            v,
        )),
        VoiceCmd::Midi(data) => ev_in.push(&MidiEvent::new(time, 0, data)),
    }
}

#[cfg(test)]
pub(crate) mod probe;

#[cfg(test)]
mod host_request_tests {
    use super::probe::host_probe;
    use super::*;

    fn render(voice: &mut ClapVoice) -> [f32; BLOCK * 2] {
        let mut buf = [1.0f32; BLOCK * 2];
        voice.render(&mut buf, Transport::default());
        buf
    }

    #[test]
    fn a_requested_callback_runs_once_on_the_main_thread_without_an_editor() {
        let (voice, mut main, probe) = host_probe();
        main.service();
        assert!(probe.calls("on_main_thread").is_empty(), "no callback without a request");
        // The plugin may ask from any thread.
        std::thread::scope(|s| {
            s.spawn(|| probe.request_callback());
        });
        assert!(!main.editor_open());
        main.pump();
        main.pump();
        let calls = probe.calls("on_main_thread");
        assert_eq!(calls.len(), 1, "one request, one call");
        assert_eq!(calls[0].thread, std::thread::current().id());
        assert_eq!(main.served(), Served { callbacks: 1, ..Served::default() });
        main.retire(voice);
    }

    #[test]
    fn a_restart_request_reactivates_off_the_audio_thread_and_the_voice_plays_on() {
        let (mut voice, mut main, probe) = host_probe();
        let main_thread = std::thread::current().id();
        assert_eq!(voice.param_range(1), None);

        // The voice renders on its own thread, as in the engine; the test
        // steps it block by block.
        let (step_tx, step_rx) = mpsc::channel::<()>();
        let (out_tx, out_rx) = mpsc::channel::<f32>();
        let audio = std::thread::spawn(move || {
            while step_rx.recv().is_ok() {
                let _ = out_tx.send(render(&mut voice)[0]);
            }
            voice
        });
        let audio_thread = audio.thread().id();
        let mut block = || {
            step_tx.send(()).unwrap();
            out_rx.recv().unwrap()
        };

        assert_eq!(block(), 0.25);
        // After the restart the plugin has a second parameter.
        probe.params.store(2, Ordering::SeqCst);
        probe.request_restart();
        main.service();
        assert_eq!(probe.calls("deactivate").len(), 0, "the processor is still with the audio side");
        assert_eq!(block(), 0.0, "the block that hands the processor over is silent");
        main.service();
        assert_eq!(main.served().restarts, 1);
        assert_eq!(block(), 0.25, "the next block plays again");
        main.service();
        assert_eq!(main.served().restarts, 1, "one request, one restart");

        assert_eq!(
            probe.sequence(),
            ["activate", "start_processing", "first process", "stop_processing", "deactivate", "activate", "start_processing"]
        );
        for what in ["activate", "deactivate"] {
            assert!(probe.calls(what).iter().all(|c| c.thread == main_thread), "{what} belongs to the main thread");
        }
        for what in ["start_processing", "stop_processing"] {
            assert!(probe.calls(what).iter().all(|c| c.thread == audio_thread), "{what} belongs to the audio thread");
        }

        drop(step_tx);
        let voice = audio.join().unwrap();
        assert_eq!(voice.param_range(1), Some((0.0, 1.0)), "the new activation's parameters are known");
        assert_eq!(voice.process_errors(), 0);
        main.retire(voice);
        assert_eq!(probe.calls("destroy").len(), 1);
    }

    #[test]
    fn commands_sent_during_a_restart_reach_the_plugin_after_it() {
        let (mut voice, mut main, probe) = host_probe();
        render(&mut voice);
        probe.request_restart();
        main.service();
        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.5));
        assert_eq!(render(&mut voice)[0], 0.0);
        main.service();
        assert_eq!(render(&mut voice)[0], 0.5);
        main.retire(voice);
    }

    #[test]
    fn a_second_restart_asked_before_the_first_set_was_adopted_is_served_too() {
        let (mut voice, mut main, probe) = host_probe();
        render(&mut voice);
        probe.request_restart();
        main.service();
        render(&mut voice);
        main.service();
        // The new set waits for the audio side; the plugin asks again.
        probe.request_restart();
        main.service();
        assert_eq!(render(&mut voice)[0], 0.0, "adopted and handed over in one block");
        main.service();
        assert_eq!(main.served().restarts, 2);
        assert_eq!(render(&mut voice)[0], 0.25);
        assert_eq!(probe.calls("activate").len(), 3);
        assert_eq!(probe.calls("deactivate").len(), 2);
        main.retire(voice);
    }

    #[test]
    fn a_plugin_that_refuses_to_activate_again_stays_silent_and_is_flushed_on_the_main_thread() {
        let (mut voice, mut main, probe) = host_probe();
        render(&mut voice);

        // Active: the next process() call is the flush.
        probe.request_flush();
        main.service();
        assert!(probe.calls("flush").is_empty());

        probe.refuse_activation.store(true, Ordering::SeqCst);
        probe.request_restart();
        main.service();
        render(&mut voice);
        main.service();
        assert!(!main.is_active());
        assert_eq!(render(&mut voice)[0], 0.0);

        probe.request_flush();
        main.service();
        let flushes = probe.calls("flush");
        assert_eq!(flushes.len(), 1, "an inactive plugin is flushed by params.flush");
        assert_eq!(flushes[0].thread, std::thread::current().id());
        assert_eq!(main.served().flushes, 1);

        // It asks again and activates this time.
        probe.request_restart();
        main.service();
        assert!(main.is_active());
        assert_eq!(render(&mut voice)[0], 0.25);
        assert_eq!(main.served().restarts, 1);
        main.retire(voice);
        assert_eq!(probe.calls("destroy").len(), 1);
    }

    #[test]
    fn a_voice_retired_in_the_middle_of_a_restart_is_deactivated_once() {
        // The processor is with the voice, on its way to the main side, or
        // on its way back.
        for stage in 0..3 {
            let (mut voice, mut main, probe) = host_probe();
            render(&mut voice);
            probe.request_restart();
            main.service();
            if stage >= 1 {
                render(&mut voice);
            }
            if stage >= 2 {
                main.service();
            }
            main.retire(voice);
            let calls = probe.sequence();
            assert_eq!(calls.iter().filter(|c| **c == "activate").count(), calls.iter().filter(|c| **c == "deactivate").count(), "stage {stage}: {calls:?}");
            assert_eq!(calls.last(), Some(&"destroy"), "stage {stage}");
        }
    }

    #[test]
    fn a_stereo_clap_effect_hears_left_and_right_apart_and_feeds_sidechain_silence() {
        let (mut voice, main, probe) = super::probe::host_probe_effect(&[(2, true), (2, false)], 2);
        assert!(voice.is_effect(), "audio-effect descriptor is marked as an effect");
        let mut inp = [0.0f32; BLOCK * 2];
        for i in 0..BLOCK {
            inp[i * 2] = 0.8;
            inp[i * 2 + 1] = -0.4;
        }
        let mut out = [0.0f32; BLOCK * 2];
        voice.process(&inp, &mut out, Transport::default());
        assert!((out[0] - 0.4).abs() < 1e-6, "left scaled by 0.5: {}", out[0]);
        assert!((out[1] - -0.2).abs() < 1e-6, "right scaled by 0.5: {}", out[1]);
        assert_eq!(probe.sidechain_peak(), 0.0, "sidechain input port is fed silence");
        main.retire(voice);
    }

    #[test]
    fn a_mono_in_mono_out_clap_effect_takes_left_and_centers_to_stereo() {
        let (mut voice, main, _probe) = super::probe::host_probe_effect(&[(1, true)], 1);
        assert!(voice.is_effect());
        let mut inp = [0.0f32; BLOCK * 2];
        for i in 0..BLOCK {
            inp[i * 2] = 0.6;
            inp[i * 2 + 1] = 0.0;
        }
        let mut out = [0.0f32; BLOCK * 2];
        voice.process(&inp, &mut out, Transport::default());
        assert!((out[0] - 0.3).abs() < 1e-6, "mono out centered to L: {}", out[0]);
        assert!((out[1] - 0.3).abs() < 1e-6, "mono out centered to R: {}", out[1]);
        main.retire(voice);
    }

    #[test]
    fn real_clap_effects_stereo_mono_and_sidechain_pass_test_tone_when_installed() {
        let cases = [
            ("/var/lib/openbot/.cache/ksalat-clapfx/root/usr/lib/clap/DragonflyHallReverb.clap", "michaelwillis.dragonfly.hall"),
            ("/var/lib/openbot/.cache/ksalat-issue29/root/usr/lib/clap/Surge XT Effects.clap", "org.surge-synth-team.surge-xt-fx"),
            ("/var/lib/openbot/.cache/ksalat-clapfx/root/usr/lib/clap/lsp-plugins.clap", "in.lsp-plug.comp_delay_mono"),
            ("/var/lib/openbot/.cache/ksalat-clapfx/root/usr/lib/clap/lsp-plugins.clap", "in.lsp-plug.sc_compressor_stereo"),
        ];
        for (bundle_path, id) in cases {
            let p = std::path::Path::new(bundle_path);
            if !p.exists() {
                continue;
            }
            let (mut voice, main) = match ClapVoice::new_from_bundle(48_000.0, p, id) {
                Ok(v) => v,
                Err(e) => {
                    println!("skip {id}: {e:#}");
                    continue;
                }
            };
            assert!(voice.is_effect(), "{id} should be marked as a CLAP effect");
            let mut inp = [0.0f32; 512 * 2];
            let mut out = [0.0f32; 512 * 2];
            let mut peak = 0.0f32;
            for b in 0..40 {
                for i in 0..512 {
                    let s = (2.0 * std::f32::consts::PI * 220.0 * ((b * 512 + i) as f32 / 48_000.0)).sin() * 0.5;
                    inp[i * 2] = s;
                    inp[i * 2 + 1] = s;
                }
                voice.process(&inp, &mut out, Transport::default());
                if b > 10 {
                    peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
                }
            }
            main.retire(voice);
            println!("real CLAP effect {id}: test tone peak {peak:.3}");
            assert!(peak >= 1e-3, "{id} output was silent (peak {peak})");
        }
    }
}

#[cfg(test)]
mod state_gate_tests {
    use super::*;

    #[test]
    fn a_block_is_skipped_while_the_gate_is_held() {
        let gate = StateGate::default();
        assert!(gate.enter(), "an open gate lets the block in");
        gate.leave();
        gate.hold(|| assert!(!gate.enter(), "a held gate turns the block away"));
        assert!(gate.enter(), "and lets the next one in");
        gate.leave();
        assert_eq!(gate.processed.load(Ordering::SeqCst), 2, "a skipped block is not a processed one");
    }

    #[test]
    fn holding_the_gate_waits_for_the_block_in_flight() {
        let gate = Arc::new(StateGate::default());
        let finished = Arc::new(AtomicBool::new(false));
        let (began_tx, began_rx) = mpsc::channel();
        let block = std::thread::spawn({
            let (gate, finished) = (gate.clone(), finished.clone());
            move || {
                assert!(gate.enter());
                began_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(30));
                finished.store(true, Ordering::SeqCst);
                gate.leave();
            }
        });
        began_rx.recv().unwrap();
        gate.hold(|| assert!(finished.load(Ordering::SeqCst), "the save ran beside a block"));
        block.join().unwrap();
    }

    #[test]
    fn a_load_is_applied_by_a_block_that_began_after_it() {
        let gate = StateGate::default();
        let idle = gate.mark();
        assert!(!gate.passed(idle));
        assert!(gate.enter());
        // This block began before the load below returned: it may have
        // missed it, so its end does not settle the load.
        let in_flight = gate.mark();
        gate.leave();
        assert!(gate.passed(idle), "no block ran at the first load: this one applied it");
        assert!(!gate.passed(in_flight));
        gate.hold(|| assert!(!gate.enter()));
        assert!(!gate.passed(in_flight), "a skipped block applies nothing");
        assert!(gate.enter());
        gate.leave();
        assert!(gate.passed(in_flight));
    }
}

#[cfg(test)]
mod param_event_tests {
    use super::*;

    #[test]
    fn a_parameter_event_reaches_the_plugin_only_for_an_id_it_declared() {
        let declared = [(7u32, 0.0, 1.0), (40, -1.0, 1.0)];
        let mut events = EventBuffer::with_capacity(4);
        push_cmd(&mut events, &declared, 0, VoiceCmd::Param(ClapId::new(8), 0.5));
        assert_eq!(events.len(), 0, "an undeclared id is dropped");
        push_cmd(&mut events, &declared, 0, VoiceCmd::Param(ClapId::new(40), 0.5));
        push_cmd(&mut events, &declared, 0, VoiceCmd::NoteOn(60, 0.9));
        assert_eq!(events.len(), 2, "declared parameters and notes pass");
        push_cmd(&mut events, &[], 0, VoiceCmd::Param(ClapId::new(40), 0.5));
        assert_eq!(events.len(), 2, "a plugin without parameters gets none");
    }
}

#[cfg(test)]
mod transport_event_tests {
    use super::*;

    #[test]
    fn plugins_get_tempo_position_and_play_state() {
        let mut t = Transport::stopped(90.0);
        let stopped = transport_event(&t);
        assert_eq!(stopped.tempo, 90.0);
        assert!(!stopped.flags.contains(TransportFlags::IS_PLAYING));
        assert!(stopped.flags.contains(
            TransportFlags::HAS_TEMPO
                | TransportFlags::HAS_BEATS_TIMELINE
                | TransportFlags::HAS_SECONDS_TIMELINE
                | TransportFlags::HAS_TIME_SIGNATURE
        ));
        assert_eq!((stopped.time_signature_numerator, stopped.time_signature_denominator), (4, 4));

        t.set_playing(true);
        t.advance(48000 * 4, 48000.0); // 4 s at 90 BPM = 6 beats
        let playing = transport_event(&t);
        assert!(playing.flags.contains(TransportFlags::IS_PLAYING));
        assert!((playing.song_pos_beats.to_float() - 6.0).abs() < 1e-6);
        assert!((playing.song_pos_seconds.to_float() - 4.0).abs() < 1e-6);
        assert_eq!(playing.bar_number, 1);
        assert_eq!(playing.bar_start.to_float(), 4.0);
    }

    #[test]
    fn plugins_get_the_time_signature_and_its_bars() {
        let mut t = Transport::stopped(120.0);
        t.sig = crate::transport::TimeSig { num: 7, den: 8 };
        t.set_playing(true);
        t.advance(48000 * 4, 48000.0); // 8 beats: 2 bars of 3.5 and one beat
        let e = transport_event(&t);
        assert_eq!((e.time_signature_numerator, e.time_signature_denominator), (7, 8));
        assert_eq!((e.bar_number, e.bar_start.to_float()), (2, 7.0));
        t.sig = crate::transport::TimeSig { num: 3, den: 4 };
        let e = transport_event(&t);
        assert_eq!((e.time_signature_numerator, e.time_signature_denominator), (3, 4));
        assert_eq!((e.bar_number, e.bar_start.to_float()), (2, 6.0));
    }
}

fn x11_embedded() -> GuiConfiguration<'static> {
    GuiConfiguration { api_type: GuiApiType::X11, is_floating: false }
}

/// The registered descriptors that are ready now (never blocks).
fn ready_fds(fds: &[(i32, FdFlags)]) -> Vec<(i32, FdFlags)> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, flags)| {
            let mut events = 0;
            if flags.contains(FdFlags::READ) {
                events |= libc::POLLIN;
            }
            if flags.contains(FdFlags::WRITE) {
                events |= libc::POLLOUT;
            }
            libc::pollfd { fd, events, revents: 0 }
        })
        .collect();
    if polls.is_empty() {
        return Vec::new();
    }
    // SAFETY: `polls` is a valid, exclusively borrowed array of its length.
    if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 0) } <= 0 {
        return Vec::new();
    }
    polls
        .iter()
        .filter(|p| p.revents != 0)
        .map(|p| {
            let mut flags = FdFlags::empty();
            if p.revents & libc::POLLIN != 0 {
                flags |= FdFlags::READ;
            }
            if p.revents & libc::POLLOUT != 0 {
                flags |= FdFlags::WRITE;
            }
            if p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                flags |= FdFlags::ERROR;
            }
            (p.fd, flags)
        })
        .collect()
}

/// `kabelsalat editor-test [plugin-id...]`: open every installed CLAP
/// plugin's own editor (or the named ones), run its main-thread callbacks for
/// a while, and check that the editor attached to the host window. Hold each
/// open longer with `KABELSALAT_EDITOR_HOLD=<seconds>` (for screenshots).
pub fn run_editor_test(ids: &[String]) -> Result<()> {
    let ids: Vec<String> = if ids.is_empty() {
        let mut all: Vec<String> = list_clap_plugins().into_iter().map(|p| p.id).collect();
        all.sort();
        all.dedup();
        all
    } else {
        ids.to_vec()
    };
    let hold = std::env::var("KABELSALAT_EDITOR_HOLD").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(2.0);
    let mut failed = Vec::new();
    for id in &ids {
        let (voice, mut main) = match ClapVoice::new_plugin(48_000.0, id) {
            Ok(v) => v,
            Err(e) => {
                println!("editor-test {id}: SKIP (does not load: {e:#})");
                continue;
            }
        };
        if !main.has_editor() {
            println!("editor-test {id}: no X11 editor");
            main.retire(voice);
            continue;
        }
        let result = main.open_editor(&format!("{id} · kabelsalat editor-test")).and_then(|()| {
            let until = Instant::now() + Duration::from_secs_f64(hold);
            while Instant::now() < until && main.editor_open() {
                main.pump();
                std::thread::sleep(Duration::from_millis(15));
            }
            match main.editor_children() {
                Some(n) if n > 0 => Ok(n),
                Some(_) => Err(anyhow!("the plugin attached no window to the editor window")),
                None => Err(anyhow!("the editor closed by itself")),
            }
        });
        main.close_editor();
        let reopened = result.is_ok() && main.open_editor("reopen").is_ok() && {
            for _ in 0..10 {
                main.pump();
                std::thread::sleep(Duration::from_millis(15));
            }
            main.editor_children().is_some_and(|n| n > 0)
        };
        main.close_editor();
        match (result, reopened) {
            (Ok(n), true) => println!("editor-test {id}: PASS ({n} child window, closes and reopens)"),
            (Ok(_), false) => {
                println!("editor-test {id}: FAIL (did not reopen after close)");
                failed.push(id.clone());
            }
            (Err(e), _) => {
                println!("editor-test {id}: FAIL ({e:#})");
                failed.push(id.clone());
            }
        }
        main.retire(voice);
    }
    if failed.is_empty() {
        println!("=== editor-test PASS ===");
        Ok(())
    } else {
        bail!("editor-test FAIL: {}", failed.join(", "))
    }
}

/// Main-control polls that look again after a state load (about a second
/// at the UI's poll rate): the state lands on the audio thread a few blocks
/// later.
const MAIN_CONTROLS_RECHECKS: u8 = 10;

/// CLAP plugins whose instances must never be destroyed (#52): Dexed's
/// destructor segfaults while other plugin instances render (same class as
/// the LV2 `NEVER_FREE` list, rule 56). Retired instances are deactivated
/// and parked instead of destroyed; `ClapVoice::new_plugin` reuses them.
const CLAP_NEVER_FREE: &[&str] = &["com.digital-suburban.dexed"];

fn clap_never_free(plugin_id: &str) -> bool {
    CLAP_NEVER_FREE.iter().any(|id| plugin_id.contains(id))
}

/// A deactivated instance of a `CLAP_NEVER_FREE` plugin, kept for reuse.
/// Instances stay on the thread that created them (the plugin main thread):
/// the pool is thread-local, so an instance never crosses a thread.
struct ParkedClap {
    plugin_id: String,
    instance: PluginInstance<TestHost>,
    exts: Exts,
    _entry: PluginEntry,
}

thread_local! {
    /// Parked instances are never dropped — not even at exit.
    static CLAP_PARKED: std::cell::RefCell<Vec<ParkedClap>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Post-activation state of each never-free plugin's *first* fresh
/// instantiation: what a new instance starts with. Reloaded when a parked
/// instance is reused, so the player never hears the previous unit's patch.
static INIT_STATE: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());

fn remember_init_state(plugin_id: &str, main: &mut ClapVoiceMain) {
    let mut store = INIT_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if store.iter().any(|(id, _)| id == plugin_id) {
        return; // keep the first capture: this one may already be customized
    }
    if let Ok(state) = main.save_state() {
        store.push((plugin_id.to_string(), state));
    }
}

fn init_state(plugin_id: &str) -> Option<Vec<u8>> {
    INIT_STATE
        .lock()
        .ok()?
        .iter()
        .find(|(id, _)| id == plugin_id)
        .map(|(_, s)| s.clone())
}

/// Take a parked instance of `plugin_id`, if this thread parked one.
fn take_parked_clap(plugin_id: &str) -> Option<ParkedClap> {
    if !clap_never_free(plugin_id) {
        return None;
    }
    CLAP_PARKED.with(|pool| {
        let mut pool = pool.borrow_mut();
        let i = pool.iter().position(|p| p.plugin_id == plugin_id)?;
        Some(pool.swap_remove(i))
    })
}

/// How many instances of `plugin_id` this thread has parked (tests).
pub fn parked_clap_count(plugin_id: &str) -> usize {
    CLAP_PARKED.with(|pool| {
        pool.borrow()
            .iter()
            .filter(|p| p.plugin_id.contains(plugin_id))
            .count()
    })
}


/// Main-thread side of a [`ClapVoice`]: the CLAP plugin instance (which is
/// `!Send`), state save/restore and deactivation. Must stay on the
/// thread that called `ClapVoice::new`.
pub struct ClapVoiceMain {
    instance: PluginInstance<TestHost>,
    exts: Exts,
    /// Resolved when the instance is created, and again only when the plugin
    /// reports changed parameter info. Never per frame: it walks every param.
    main_controls: MainControls,
    /// Polls left that re-read the main controls after a state load.
    main_controls_rechecks: u8,
    /// The plugin's own editor, while it is open.
    editor: Option<ClapEditor>,
    gate: Arc<StateGate>,
    /// Shared with the audio-side [`ClapVoice`]; updated whenever the plugin
    /// announces `host_latency.changed()`.
    shared_latency: Arc<std::sync::atomic::AtomicU32>,
    /// `StateGate::mark` of the last state load, until a block applied it.
    load_mark: Option<u64>,
    link: MainLink,
    restart: Restart,
    served: Served,
    sample_rate: f64,
    _entry: PluginEntry,
}

/// An open plugin editor: the plugin's GUI embedded in a host toplevel.
/// The window must outlive the GUI, so `close_editor` destroys the GUI first.
struct ClapEditor {
    gui: PluginGui,
    window: PluginWindow,
    resizable: bool,
}

impl Drop for ClapVoiceMain {
    fn drop(&mut self) {
        self.close_editor();
    }
}

impl ClapVoiceMain {
    /// Whether the plugin has its own editor kabelsalat can open.
    pub fn has_editor(&mut self) -> bool {
        let Some(gui) = self.exts.gui else { return false };
        gui.is_api_supported(&self.instance.plugin_handle(), x11_embedded())
    }

    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// Open the plugin's own editor in a window titled `title`. A second call
    /// while it is open does nothing.
    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        let gui = self.exts.gui.ok_or_else(|| anyhow!("this plugin has no editor of its own"))?;
        let cfg = x11_embedded();
        if !gui.is_api_supported(&self.instance.plugin_handle(), cfg) {
            bail!("this plugin's editor does not run on X11");
        }
        self.instance.access_shared_handler(|h| {
            h.gui_closed.store(false, Ordering::SeqCst);
            h.gui_resize.lock().unwrap().take();
        });
        gui.create(&self.instance.plugin_handle(), cfg).map_err(|e| anyhow!("the plugin could not create its editor: {e:?}"))?;
        let handle = self.instance.plugin_handle();
        let size = gui.get_size(&handle).unwrap_or(GuiSize { width: 800, height: 600 });
        let resizable = gui.can_resize(&handle);
        let window = match PluginWindow::open(title, size.width, size.height, resizable) {
            Ok(w) => w,
            Err(e) => {
                gui.destroy(&handle);
                return Err(e);
            }
        };
        // SAFETY: the window lives in `self.editor` and `close_editor`
        // destroys the GUI before dropping it.
        let parented = unsafe { gui.set_parent(&handle, ClapWindow::from_x11_handle(window.id() as _)) };
        if let Err(e) = parented {
            gui.destroy(&handle);
            bail!("the plugin could not attach its editor: {e:?}");
        }
        // Odin 2 answers false here yet shows its editor; whether it attached
        // is what `editor-test` checks, not this answer.
        let _ = gui.show(&handle);
        self.editor = Some(ClapEditor { gui, window, resizable });
        Ok(())
    }

    pub fn close_editor(&mut self) {
        if let Some(ed) = self.editor.take() {
            let handle = self.instance.plugin_handle();
            let _ = ed.gui.hide(&handle);
            ed.gui.destroy(&handle);
            drop(ed.window);
        }
    }

    /// Serve the plugin's requests ([`Self::service`]) and, while the editor
    /// is open, run the plugin's main-thread callbacks: due
    /// timers, ready file descriptors, the window's events and resize
    /// requests. Call it every UI frame; JUCE editors (Surge) paint only from
    /// these. Without an editor nothing runs: JUCE's timers drive its one
    /// process-wide message loop, and firing them from several threads (one
    /// per test) deadlocked the unit suite.
    pub fn pump(&mut self) {
        self.service();
        if self.editor.is_none() {
            return;
        }
        let due = self.instance.access_handler(|h| h.take_due_timers(Instant::now()));
        if let Some(timer) = self.exts.timer {
            for id in due {
                timer.on_timer(&mut self.instance.plugin_handle(), TimerId(id));
            }
        }
        if let Some(posix_fd) = self.exts.posix_fd {
            let fds = self.instance.access_handler(|h| h.fds.borrow().clone());
            for (fd, flags) in ready_fds(&fds) {
                posix_fd.on_fd(&mut self.instance.plugin_handle(), fd, flags);
            }
        }
        self.pump_editor();
    }

    /// Do what the plugin asked the host for since the last call. Main
    /// thread; call it every UI frame, editor open or not (`pump` does).
    ///
    /// - `request_callback()`: one `on_main_thread()` call.
    /// - `request_restart()`: the audio side stops processing and hands its
    ///   processor over at its next block, then the plugin is deactivated
    ///   and activated here, with ports, parameter ranges and buffers read
    ///   anew, and processing starts again on the audio side. The voice is
    ///   silent in between. A voice nothing renders restarts once it is
    ///   rendered again.
    /// - `params.request_flush()`: an active plugin is flushed by its next
    ///   `process()` call, which carries the parameter events both ways. An
    ///   inactive one gets `params.flush()` here.
    pub fn service(&mut self) {
        let (callback, flush) = self.instance.access_shared_handler(|h| {
            (h.callback_requested.swap(false, Ordering::SeqCst), h.flush_requested.swap(false, Ordering::SeqCst))
        });
        if callback {
            self.instance.call_on_main_thread_callback();
            self.served.callbacks += 1;
        }
        self.step_restart();
        if flush {
            self.flush_inactive();
        }
    }

    /// What `service` did so far.
    pub fn served(&self) -> Served {
        self.served
    }

    /// Whether the plugin is active (false only after it refused to
    /// activate again in a restart).
    pub fn is_active(&self) -> bool {
        self.instance.is_active()
    }

    fn restart_requested(&self) -> bool {
        self.instance.access_shared_handler(|h| h.restart_requested.swap(false, Ordering::SeqCst))
    }

    fn step_restart(&mut self) {
        match self.restart {
            Restart::Idle => {
                if self.restart_requested() {
                    self.link.stop.store(true, Ordering::SeqCst);
                    self.restart = Restart::Stopping;
                }
            }
            Restart::Stopping => {
                let Ok(live) = self.link.from_audio.pop() else { return };
                // Requests up to here are served by this deactivation.
                self.restart_requested();
                let Live { processor, .. } = *live;
                self.instance.deactivate(processor.into_stopped());
                self.reactivate();
            }
            Restart::Inactive => {
                if self.restart_requested() {
                    self.reactivate();
                }
            }
        }
    }

    /// Activate the inactive plugin and send the audio side its new set.
    fn reactivate(&mut self) {
        match Live::activate(&mut self.instance, self.exts, self.sample_rate) {
            Ok(live) => {
                // CLAP lets a plugin change its latency on activation; publish the new value.
                let latency = self.latency();
                self.shared_latency.store(latency, Ordering::Relaxed);
                // The ring is empty: the audio side adopted the last set
                // before it could hand one back.
                let _ = self.link.to_audio.push(live);
                self.restart = Restart::Idle;
                self.served.restarts += 1;
                // Parameters may be new ones.
                self.main_controls_rechecks = MAIN_CONTROLS_RECHECKS;
            }
            Err(e) => {
                eprintln!("[host] the plugin did not activate again after its restart request: {e:#}");
                self.restart = Restart::Inactive;
            }
        }
    }

    /// `params.flush()` for an inactive plugin, with no events to send.
    /// What the plugin reports back is not kept: values are read from it.
    fn flush_inactive(&mut self) {
        let Some(params) = self.exts.params else { return };
        let Some(mut handle) = self.instance.inactive_plugin_handle() else { return };
        let (ev_in, mut ev_out) = (EventBuffer::new(), EventBuffer::new());
        params.flush(&mut handle, &ev_in.as_input(), &mut ev_out.as_output());
        self.served.flushes += 1;
    }

    /// Windows the plugin attached inside its editor window, while it is open.
    pub fn editor_children(&self) -> Option<usize> {
        self.editor.as_ref().and_then(|ed| ed.window.child_count().ok())
    }

    fn pump_editor(&mut self) {
        let plugin_closed = self.instance.access_shared_handler(|h| h.gui_closed.swap(false, Ordering::SeqCst));
        let requested = self.instance.access_shared_handler(|h| h.gui_resize.lock().unwrap().take());
        let Some(ed) = self.editor.as_mut() else { return };
        let events = ed.window.poll();
        if plugin_closed || events.close_requested {
            self.close_editor();
            return;
        }
        let handle = self.instance.plugin_handle();
        if let Some(size) = requested {
            let _ = ed.window.resize(size.width, size.height, ed.resizable);
            let _ = ed.gui.set_size(&handle, size);
        } else if let Some((w, h)) = events.resized.filter(|_| ed.resizable) {
            let size = ed.gui.adjust_size(&handle, GuiSize { width: w, height: h }).unwrap_or(GuiSize { width: w, height: h });
            let _ = ed.gui.set_size(&handle, size);
        }
    }

    /// Direct access for params inspection (main thread only).
    pub fn instance_mut(&mut self) -> &mut PluginInstance<TestHost> {
        &mut self.instance
    }

    /// Plugin-reported latency in samples (meaningful once active).
    pub fn latency(&mut self) -> u32 {
        self.exts.latency.map(|l| l.get(&self.instance.plugin_handle())).unwrap_or(0)
    }

    /// Re-read the plugin's latency if it called `host_latency.changed()`
    /// since the last poll, publishing it to the audio-side [`ClapVoice`].
    pub fn poll_latency(&mut self) -> u32 {
        if self.instance.access_handler(|h| h.latency_changed.replace(false)) {
            let frames = self.latency();
            self.shared_latency.store(frames, Ordering::Relaxed);
            frames
        } else {
            self.shared_latency.load(Ordering::Relaxed)
        }
    }

    /// Serialize the full plugin state (Surge patch XML in a `sub3` container).
    /// Allocates — main thread only.
    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        let state = self.exts.state.ok_or_else(|| anyhow!("Surge XT has no state extension"))?;
        let mut buf = Vec::new();
        let pending = self.load_pending();
        let mut handle = self.instance.plugin_handle();
        if pending {
            self.gate.hold(|| state.save(&mut handle, &mut buf))
        } else {
            state.save(&mut handle, &mut buf)
        }
        .context("saving plugin state")?;
        Ok(buf)
    }

    /// Whether a block may still be applying the last state load.
    fn load_pending(&mut self) -> bool {
        if self.load_mark.is_some_and(|mark| self.gate.passed(mark)) {
            self.load_mark = None;
        }
        self.load_mark.is_some()
    }

    /// Restore plugin state. Surge applies it on the audio thread: keep
    /// rendering (or wait a few blocks) before reading parameter values back.
    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let state = self.exts.state.ok_or_else(|| anyhow!("Surge XT has no state extension"))?;
        state.load(&mut self.instance.plugin_handle(), &mut &data[..]).context("loading plugin state")?;
        self.load_mark = Some(self.gate.mark());
        self.main_controls_rechecks = MAIN_CONTROLS_RECHECKS;
        Ok(())
    }

    /// The processor wherever a restart left it: with the voice, on its way
    /// to this side, or nowhere (the plugin is inactive).
    fn take_processor(&mut self, voice: ClapVoice) -> Option<StoppedPluginAudioProcessor<TestHost>> {
        voice.stop().or_else(|| self.link.from_audio.pop().ok().map(|live| live.processor.into_stopped()))
    }

    /// Stop `voice` and retire both halves (main thread only).
    /// `CLAP_NEVER_FREE` plugins are deactivated and parked for reuse —
    /// never destroyed: their destructor is unsafe while other plugin
    /// instances render (#52).
    pub fn retire(mut self, voice: ClapVoice) {
        let plugin_id = voice.plugin_id.clone();
        if let Some(stopped) = self.take_processor(voice) {
            self.instance.deactivate(stopped);
        }
        if clap_never_free(&plugin_id) {
            self.park(plugin_id);
        }
    }

    /// Park a deactivated never-free instance for the next
    /// `new_plugin` of the same id. If the pool cannot take it, leak it —
    /// never destroy (rule 56 class).
    fn park(self, plugin_id: String) {
        // `Drop` closes the editor and frees the instance; a parked plugin
        // is never freed, so take the fields out by hand.
        let mut me = std::mem::ManuallyDrop::new(self);
        me.close_editor();
        // SAFETY: `me` is never used or dropped again, and each field is read
        // exactly once (the editor is closed, so there is nothing else to free).
        let (instance, exts, entry, controls, gate, shared_latency, link) = unsafe {
            (
                std::ptr::read(&me.instance),
                std::ptr::read(&me.exts),
                std::ptr::read(&me._entry),
                std::ptr::read(&me.main_controls),
                std::ptr::read(&me.gate),
                std::ptr::read(&me.shared_latency),
                std::ptr::read(&me.link),
            )
        };
        drop((controls, gate, shared_latency, link));
        let parked = ParkedClap { plugin_id, instance, exts, _entry: entry };
        CLAP_PARKED.with(|pool| {
            if let Ok(mut pool) = pool.try_borrow_mut() {
                pool.push(parked);
            } else {
                std::mem::forget(parked); // poisoned pool: leak rather than free
            }
        });
    }

    /// Enumerate all plugin parameters for the native (egui) editor. Main thread
    /// only. Returns id/name/module/range/value/flags so the UI can render
    /// grouped sliders without any plugin GUI / X11 window.
    pub fn param_list(&mut self) -> Vec<ParamDesc> {
        let Some(params) = self.exts.params else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let count = params.count(&handle);
        let mut info_buf = ParamInfoBuffer::new();
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(info) = params.get_info(&handle, i, &mut info_buf) else {
                continue;
            };
            let value = params.get_value(&handle, info.id).unwrap_or(info.default_value);
            out.push(ParamDesc {
                id: info.id.get(),
                name: String::from_utf8_lossy(info.name).into_owned(),
                module: String::from_utf8_lossy(info.module).into_owned(),
                min: info.min_value,
                max: info.max_value,
                value,
                default: info.default_value,
                automatable: info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE),
                stepped: info.flags.contains(ParamInfoFlags::IS_STEPPED),
                hidden: info.flags.contains(ParamInfoFlags::IS_HIDDEN),
                readonly: info.flags.contains(ParamInfoFlags::IS_READONLY),
            });
        }
        out
    }

    /// The instrument's main controls (see `main_controls::resolve`).
    pub fn main_controls(&self) -> &MainControls {
        &self.main_controls
    }

    /// Re-read the main controls when they may have changed: the plugin
    /// announced new parameter info, or a state was loaded (a patch renames
    /// its macros; Surge applies the state on the audio thread a few blocks
    /// later and announces nothing, so the next few polls look again). True
    /// when the list changed. Call it a few times per second, not per frame.
    pub fn poll_main_controls(&mut self) -> bool {
        let announced = self.instance.access_handler(|h| h.params_rescan.replace(false));
        if !announced && self.main_controls_rechecks == 0 {
            return false;
        }
        self.main_controls_rechecks = self.main_controls_rechecks.saturating_sub(1);
        let before = std::mem::take(&mut self.main_controls);
        self.refresh_main_controls();
        before != self.main_controls
    }

    fn refresh_main_controls(&mut self) {
        let pages = self.remote_pages();
        let params = self.param_candidates();
        self.main_controls = main_controls::resolve(&pages, &params);
    }

    fn remote_pages(&mut self) -> Vec<RemotePage> {
        let Some(rc) = self.exts.remote_controls else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let mut buf = RemoteControlsPageBuffer::new();
        (0..rc.count(&handle))
            .filter_map(|i| {
                let page = rc.get(&handle, i, &mut buf)?;
                Some(RemotePage {
                    name: String::from_utf8_lossy(page.page_name).into_owned(),
                    slots: page.param_ids.map(|id| id.map(|id| id.get())),
                })
            })
            .collect()
    }

    fn param_candidates(&mut self) -> Vec<Candidate> {
        let Some(params) = self.exts.params else {
            return Vec::new();
        };
        let handle = self.instance.plugin_handle();
        let mut info_buf = ParamInfoBuffer::new();
        (0..params.count(&handle))
            .filter_map(|i| {
                let info = params.get_info(&handle, i, &mut info_buf)?;
                Some(Candidate {
                    id: info.id.get(),
                    name: String::from_utf8_lossy(info.name).into_owned(),
                    group: String::from_utf8_lossy(info.module).into_owned(),
                    min: info.min_value,
                    max: info.max_value,
                    default: info.default_value,
                    usable: info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE)
                        && !info.flags.contains(ParamInfoFlags::IS_HIDDEN)
                        && !info.flags.contains(ParamInfoFlags::IS_READONLY),
                    stepped: info.flags.contains(ParamInfoFlags::IS_STEPPED),
                    designated: false,
                })
            })
            .collect()
    }

    /// Current plain value of a single parameter (main thread only).
    pub fn param_value(&mut self, id: u32) -> Option<f64> {
        self.exts.params?.get_value(&self.instance.plugin_handle(), ClapId::new(id))
    }

    /// Format a parameter value as the plugin's own text (e.g. "440.0 Hz").
    pub fn param_text(&mut self, id: u32, value: f64) -> String {
        let Some(params) = self.exts.params else {
            return format!("{value:.3}");
        };
        let mut buf = [0u8; 256];
        params
            .value_to_text(&self.instance.plugin_handle(), ClapId::new(id), value, &mut buf)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|_| format!("{value:.3}"))
    }
}

/// A plugin parameter description for the native editor (no X11 / plugin GUI).
#[derive(Clone, Debug)]
pub struct ParamDesc {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub value: f64,
    pub default: f64,
    pub automatable: bool,
    /// Whole-number values only (a switch, a list, a preset selector).
    pub stepped: bool,
    pub hidden: bool,
    pub readonly: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_test() -> Result<()> {
    println!("=== kabelsalat host-test (CLAP via clack-host) ===");

    // 1. Audio output via cpal (ALSA -> PipeWire); the voice needs the rate.
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let supported = device.default_output_config()?;
    let sample_rate = supported.sample_rate() as f64;
    let dev_channels = supported.channels() as usize;
    let dev_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    println!("output: {dev_name} | {sample_rate} Hz | {dev_channels} ch | config: {supported:?}");
    let config: cpal::StreamConfig = supported.into();

    // 2. Load Surge through the reusable voice API (discover ~/.clap,
    //    instantiate, activate, start processing).
    let (mut voice, mut main) = ClapVoice::new(sample_rate)?;

    // 3. Parameters: print a sample, pick one automatable param to modulate.
    let params_ext = main.exts.params;
    let mod_param = inspect_params(main.instance_mut(), params_ext)?;

    // 4. State round-trip, part 1: snapshot the pristine state.
    let state_a = main.save_state()?;
    println!("state A (initial): {} bytes", state_a.len());
    let dump_state = std::env::var_os("KSALAT_DUMP_STATE").is_some();
    if dump_state {
        let _ = std::fs::write("/tmp/ksalat-state-a.bin", &state_a);
    }

    // 5. Arpeggio + param probe as a sample-accurate schedule on the voice.
    voice.set_schedule(build_schedule(sample_rate, mod_param.as_ref()));

    // 6. Processing thread: render through ClapVoice, feed the lock-free ring.
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    // 0.5 s of interleaved stereo headroom.
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(sample_rate as usize);
    let (tx, rx) = mpsc::sync_channel::<ClapVoice>(1);
    let proc_handle = {
        let stop = stop.clone();
        let stats = stats.clone();
        std::thread::Builder::new()
            .name("clap-process".into())
            .spawn(move || voice_render_loop(voice, stop, producer, stats, tx))
            .context("spawning processing thread")?
    };

    // 7. cpal stream: drain the ring (RT-safe: no alloc, no locks).
    let stream = {
        let stats = stats.clone();
        let mut consumer = consumer;
        device.build_output_stream(
            config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                stats.callbacks.fetch_add(1, Ordering::Relaxed);
                let mut frame = [0.0f32; 2];
                for out_frame in data.chunks_mut(dev_channels) {
                    let mut underrun = false;
                    for s in frame.iter_mut() {
                        match consumer.pop() {
                            Ok(v) => *s = v,
                            Err(_) => {
                                *s = 0.0;
                                underrun = true;
                            }
                        }
                    }
                    if underrun {
                        stats.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    for (i, s) in out_frame.iter_mut().enumerate() {
                        *s = frame[i % 2];
                    }
                }
            },
            |err| eprintln!("audio stream error: {err}"),
            None,
        )?
    };
    stream.play()?;
    println!("playing arpeggio (A3 C4 E4 A4 C5 ...) for {DEMO_SECONDS}s");

    // 8. Let the arpeggio play.
    std::thread::sleep(Duration::from_secs(DEMO_SECONDS));

    // 9. State round-trip, part 2 — while processing is still running, like a
    //    real host restoring a session during playback. Surge applies loaded
    //    state on the audio thread, so we give it a few blocks to settle.
    if let Some(mp) = &mod_param {
        let params = main.exts.params.ok_or_else(|| anyhow!("no params extension"))?;
        let tol = (mp.max - mp.min) * 1e-3 + 1e-9;
        let v = params
            .get_value(&main.instance_mut().plugin_handle(), mp.id)
            .ok_or_else(|| anyhow!("param {} vanished", mp.id.get()))?;
        println!(
            "param '{}' after event: {v:.6} (target {:.6})",
            mp.name, mp.target
        );
        if (v - mp.target).abs() > tol {
            bail!("param event did not round-trip through processing: {v} != {}", mp.target);
        }
        let state_b = main.save_state()?;
        if dump_state {
            let _ = std::fs::write("/tmp/ksalat-state-b.bin", &state_b);
        }
        println!("state B (modified): {} bytes, differs from A: {}", state_b.len(), state_b != state_a);
        if state_b == state_a {
            bail!("modified state is byte-identical to initial state — state save is broken");
        }
        main.load_state(&state_a)?;
        // Let the audio thread apply the restored state.
        std::thread::sleep(Duration::from_millis(300));
        main.service();
        println!("plugin requests served: {:?}", main.served());
        let v2 = params
            .get_value(&main.instance_mut().plugin_handle(), mp.id)
            .ok_or_else(|| anyhow!("param {} vanished after load", mp.id.get()))?;
        println!("param '{}' after restore: {v2:.6} (original {:.6})", mp.name, mp.orig);
        if (v2 - mp.orig).abs() > tol {
            bail!("state load did not restore param value: {v2} != {}", mp.orig);
        }
        let state_c = main.save_state()?;
        if dump_state {
            let _ = std::fs::write("/tmp/ksalat-state-c.bin", &state_c);
        }
        println!(
            "state C (re-saved): {} bytes, byte-identical to A: {}",
            state_c.len(),
            state_c == state_a
        );
        if state_c != state_a {
            eprintln!("warning: re-saved state differs byte-wise (values still round-trip)");
        }
        println!("state round-trip: OK (save -> modify -> save != A -> load A -> value restored)");
    }

    // 10. Shut down audio: stop the stream, then the processing thread, and
    //     hand the processor back to the main thread for deactivation.
    drop(stream);
    stop.store(true, Ordering::SeqCst);
    let voice = rx
        .recv_timeout(Duration::from_secs(10))
        .context("processing thread did not return the audio processor")?;
    let process_errors = proc_handle.join().map_err(|_| anyhow!("processing thread panicked"))??;

    let peak = f32::from_bits(stats.peak_bits.load(Ordering::Relaxed));
    let blocks = stats.blocks.load(Ordering::Relaxed);
    let nonquiet = stats.nonquiet.load(Ordering::Relaxed);
    let callbacks = stats.callbacks.load(Ordering::Relaxed);
    let underruns = stats.underruns.load(Ordering::Relaxed);
    println!("--- audio stats ---");
    println!("blocks processed: {blocks} ({BLOCK} frames each)");
    println!("non-quiet blocks: {nonquiet}");
    println!("peak sample:      {peak:.4}");
    println!("cpal callbacks:   {callbacks}, underrun frames: {underruns}");
    println!("CLAP process errors: {process_errors}");
    if peak < 0.005 || nonquiet < 10 {
        bail!("Surge produced (near-)silence — arpeggio not audible (peak {peak:.5})");
    }
    if process_errors > 0 {
        bail!("{process_errors} CLAP process calls failed");
    }
    println!("arpeggio: AUDIBLE (peak {peak:.4})");

    // 11. Deactivate on the main thread, then drop everything.
    main.retire(voice);
    println!("=== host-test PASS: Surge XT loads, plays, state round-trips ===");
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Standard CLAP search directories: `~/.clap`, `/usr/lib/clap`,
/// `/usr/local/lib/clap`, and `$CLAP_PATH` entries, deduplicated in order.
pub fn clap_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };
    if let Ok(home) = std::env::var("HOME") {
        push_unique(PathBuf::from(home).join(".clap"));
    }
    push_unique(PathBuf::from("/usr/lib/clap"));
    push_unique(PathBuf::from("/usr/local/lib/clap"));
    if let Ok(extra) = std::env::var("CLAP_PATH") {
        for part in extra.split(':') {
            if !part.is_empty() {
                push_unique(PathBuf::from(part));
            }
        }
    }
    dirs
}

fn clap_bundle_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for dir in clap_search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut dir_paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "clap").unwrap_or(false))
            .collect();
        dir_paths.sort();
        paths.extend(dir_paths);
    }
    paths
}

/// `dlopen` a CLAP bundle, after asking the file what it was built for:
/// glibc reports an ELF of another CPU as "No such file or directory" (#135).
fn open_bundle(path: &std::path::Path) -> Result<PluginEntry> {
    crate::binarch::ensure_loadable(path)?;
    // SAFETY: loading a CLAP bundle executes its init code. The app loads
    // only the bundle the out-of-process scan found a plugin in; the scan
    // itself runs this inside the isolated `scan-clap` child process.
    unsafe { PluginEntry::load(path) }.with_context(|| format!("failed to load CLAP bundle {}", path.display()))
}

fn load_bundle(path: &std::path::Path, id: &str) -> Result<(PluginEntry, CString)> {
    let entry = open_bundle(path)?;
    println!("bundle: {} (CLAP {})", path.display(), entry.version());
    Ok((entry, CString::new(id)?))
}

fn discover_plugin(target_id: &str) -> Result<(PluginEntry, CString)> {
    // Resolved once per process and plugin: a rack hosts one voice per CLAP
    // unit, so the same id is asked for again with every added unit. Only a
    // hit is remembered, so a later install is still found.
    static FOUND: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());
    let known = FOUND.lock().ok().and_then(|f| f.iter().find(|(id, _)| id == target_id).map(|(_, p)| p.clone()));
    let path = match known {
        Some(path) => path,
        None => {
            let (path, id) = bundle_with(|p| p.id == target_id)
                .ok_or_else(|| anyhow!("CLAP plugin '{target_id}' not found in CLAP search paths"))?;
            if let Ok(mut f) = FOUND.lock() {
                f.push((id, path.clone()));
            }
            path
        }
    };
    load_bundle(&path, target_id)
}

/// Names of the CLAP preset-discovery providers the bundle of `plugin_id`
/// offers (`clap.preset-discovery-factory`); empty when it has no such
/// factory. Loads the bundle: `sound-library-test` reports it per plugin.
pub fn clap_preset_providers(plugin_id: &str) -> Result<Vec<String>> {
    use clack_extensions::preset_discovery::factory::PresetDiscoveryFactory;
    let (path, _) = bundle_with(|p| p.id == plugin_id)
        .ok_or_else(|| anyhow!("CLAP plugin '{plugin_id}' not found in CLAP search paths"))?;
    let entry = open_bundle(&path)?;
    let Some(factory) = entry.get_factory::<PresetDiscoveryFactory>() else { return Ok(Vec::new()) };
    Ok(factory.provider_descriptors()
        .map(|d| d.name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        .collect())
}

fn discover_surge() -> Result<(PluginEntry, CString)> {
    // Resolved once per process: the voice pool asks for Surge several times.
    // Only a hit is remembered, so a later install is still found.
    static SURGE: OnceLock<(PathBuf, String)> = OnceLock::new();
    let (path, id) = match SURGE.get() {
        Some(hit) => hit.clone(),
        None => {
            let hit = bundle_with(|p| p.name == "Surge XT" && p.features.iter().any(|f| f == "instrument"))
                .ok_or_else(|| anyhow!("Surge XT (instrument) not found in CLAP search paths"))?;
            SURGE.get_or_init(|| hit).clone()
        }
    };
    load_bundle(&path, &id)
}

/// Metadata for an installed CLAP plugin (for the plugin browser).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClapPluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub features: Vec<String>,
}

/// Is a scanned CLAP plugin a MIDI effect — one that declares the
/// `note-effect` feature (what an arpeggiator or chord tool declares)?
/// Such plugins are listed; hosting them is not implemented yet.
pub fn is_note_effect(p: &ClapPluginInfo) -> bool {
    p.features.iter().any(|f| f == "note-effect")
}

/// Enumerate all CLAP plugins in ~/.clap (id, name, vendor, features).
/// One Surge patch on disk (factory / 3rd-party / user library).
#[derive(Clone, Debug, serde::Serialize)]
pub struct SurgePatch {
    pub name: String,
    pub category: String,
    pub path: std::path::PathBuf,
}

/// Every Surge patch installed (factory, 3rd-party, user dir), sorted by
/// category then name. Rare + allocating (a directory walk) — call off the
/// audio thread; the UI caches it.
pub fn list_surge_patches() -> Vec<SurgePatch> {
    let mut roots: Vec<std::path::PathBuf> = vec![
        "/usr/share/surge-xt/patches_factory".into(),
        "/usr/share/surge-xt/patches_3rdparty".into(),
    ];
    if let Some(h) = std::env::var_os("HOME") {
        let h = std::path::PathBuf::from(h);
        roots.push(h.join("Documents/Surge XT/Patches"));
        roots.push(h.join(".local/share/surge-xt/patches_user"));
    }
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<SurgePatch>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, base, out); continue; }
            if p.extension().and_then(|x| x.to_str()) != Some("fxp") { continue; }
            let name = p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
            let category = p.parent().and_then(|d| d.strip_prefix(base).ok())
                .map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
            out.push(SurgePatch { name, category, path: p });
        }
    }
    for r in &roots { walk(r, r, &mut out); }
    out.sort_by(|a, b| (a.category.to_lowercase(), a.name.to_lowercase()).cmp(&(b.category.to_lowercase(), b.name.to_lowercase())));
    out
}

/// The patch name inside a Surge state blob (`<meta name="…" …>` in the
/// sub3 XML). Surge's init patch is named "Init Saw" / "Init" etc.
pub fn patch_name_from_state(state: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(state);
    let i = text.find("<meta name=\"")? + "<meta name=\"".len();
    let end = text[i..].find('"')?;
    let name = text[i..i + end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Surge `.fxp` → the CLAP state blob `ClapVoiceMain::load_state` takes: an
/// fxp is a 60-byte VST2 fxChunkSet header ("CcnK"… "FPCh"… chunk size)
/// followed by the same `sub3` chunk Surge saves as its CLAP state.
pub fn fxp_to_state(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 64 || &bytes[0..4] != b"CcnK" || &bytes[8..12] != b"FPCh" {
        return Err(anyhow!("not a Surge .fxp (bad header)"));
    }
    let size = u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]) as usize;
    let chunk = &bytes[60..];
    if &chunk[0..4] != b"sub3" { return Err(anyhow!("fxp chunk is not a Surge sub3 patch")); }
    Ok(chunk[..size.min(chunk.len())].to_vec())
}

/// `preset-test`: load a factory patch into a real Surge instance and prove
/// (a) the state took (the saved state names the patch) and (b) the sound
/// changed (same note, different spectrum than the init patch).
pub fn run_preset_test() -> Result<()> {
    let library = crate::sounds::scan(&crate::sounds::Instrument::Clap(crate::sounds::SURGE_ID.into()));
    anyhow::ensure!(!library.sounds.is_empty(), "no Surge patches installed");
    let flute = crate::sounds::find(&library.sounds, "flute").ok_or_else(|| anyhow!("no flute patch found"))?;
    let crate::sounds::Load::SurgePatch(path) = &flute.load else { anyhow::bail!("a Surge sound is a patch file") };
    let sr = 48_000.0;
    let (mut voice, mut main) = ClapVoice::new(sr)?;
    let ctl = voice.controller();
    // Zero-crossing rate + RMS of 0.4 s of note 60 — a cheap timbre fingerprint.
    let measure = |voice: &mut ClapVoice| -> (f32, f32) {
        let mut buf = vec![0.0f32; 512 * 2];
        for _ in 0..20 { voice.render(&mut buf, Transport::default()); } // settle
        ctl.note_on(60, 0.9);
        let (mut zc, mut sq, mut n, mut prev) = (0usize, 0.0f64, 0usize, 0.0f32);
        for _ in 0..(sr as usize * 4 / 10 / 512) {
            voice.render(&mut buf, Transport::default());
            for f in buf.chunks(2) {
                let x = f[0];
                if (x > 0.0) != (prev > 0.0) { zc += 1; }
                prev = x; sq += (x * x) as f64; n += 1;
            }
        }
        ctl.note_off(60);
        for _ in 0..60 { voice.render(&mut buf, Transport::default()); } // release tail
        (zc as f32 / n as f32 * sr as f32 / 2.0, (sq / n as f64).sqrt() as f32)
    };
    let (zc_a, rms_a) = measure(&mut voice);
    let init_state = main.save_state()?;
    let data = std::fs::read(path)?;
    main.load_state(&fxp_to_state(&data)?)?;
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..20 { voice.render(&mut buf, Transport::default()); } // Surge applies state on the audio thread
    let saved = main.save_state()?;
    // The plugin now reports a different patch than the init one.
    let named = saved != init_state;
    let (zc_b, rms_b) = measure(&mut voice);
    println!("patches: {} · picked '{}' · plugin state changed: {named}", library.sounds.len(), flute.label());
    println!("init: ~{zc_a:.0} Hz zero-cross, rms {rms_a:.4} · flute: ~{zc_b:.0} Hz, rms {rms_b:.4}");
    anyhow::ensure!(named, "plugin state unchanged after loading the patch");
    anyhow::ensure!(rms_b > 1e-4, "flute patch is silent");
    let diff = (zc_a - zc_b).abs() / zc_a.max(1.0) + (rms_a - rms_b).abs() / rms_a.max(1e-6);
    anyhow::ensure!(diff > 0.15, "sound did not change (diff {diff:.3})");
    println!("preset-test: OK — '{}' loaded, timbre changed (diff {diff:.2})", flute.name);
    Ok(())
}

/// `kabelsalat state-race-test [seconds] [patch.fxp...]`: the two ways the
/// host crashed Surge XT (#29), on the installed plugin. A crash ends the
/// process with its signal, so reaching the end is the pass.
///
/// 1. A parameter event whose id the plugin does not have. Surge
///    dereferences the parameter it did not find, inside `process()`.
/// 2. States saved on the main thread while the render thread applies the
///    state loaded just before. Surge applies a load inside `process()` and
///    its save reads the patch without a lock.
///
/// Without patch files the first installed factory patches are used.
pub fn run_state_race_test(seconds: u64, patches: &[PathBuf]) -> Result<()> {
    let mut states = Vec::new();
    let paths: Vec<PathBuf> = if patches.is_empty() {
        list_surge_patches().into_iter().step_by(97).take(6).map(|p| p.path).collect()
    } else {
        patches.to_vec()
    };
    for path in &paths {
        states.push(fxp_to_state(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)?);
    }
    anyhow::ensure!(states.len() >= 2, "state-race-test needs two Surge patches (installed, or as arguments)");
    let (mut voice, mut main) = ClapVoice::new(48_000.0)?;
    let ctl = voice.controller();
    let known: Vec<u32> = main.param_list().iter().map(|p| p.id).collect();
    let unknown = (0xDEAD_0000u32..).find(|id| !known.contains(id)).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let render = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let mut buf = [0.0f32; BLOCK * 2];
            let (mut blocks, mut peak) = (0u64, 0.0f32);
            while !stop.load(Ordering::SeqCst) {
                voice.render(&mut buf, Transport::default());
                peak = buf.iter().fold(peak, |p, x| p.max(x.abs()));
                blocks += 1;
            }
            (voice, blocks, peak)
        }
    });
    for key in [48, 55, 60, 64] {
        ctl.note_on(key, 0.9);
    }
    std::thread::sleep(Duration::from_millis(50));
    ctl.set_param(ClapId::new(unknown), 0.5);
    std::thread::sleep(Duration::from_millis(50));
    println!("1. a parameter event with the unknown id {unknown:#x} was rendered");

    let end = Instant::now() + Duration::from_secs(seconds);
    let (mut loads, mut saves) = (0u64, 0u64);
    while Instant::now() < end {
        main.load_state(&states[loads as usize % states.len()])?;
        loads += 1;
        // Saves land before, inside and after the block that applies the load.
        for pause_us in [0, 150, 700] {
            std::thread::sleep(Duration::from_micros(pause_us));
            anyhow::ensure!(!main.save_state()?.is_empty(), "an empty state");
            saves += 1;
        }
        for key in [48, 55, 60, 64] {
            ctl.note_on(key, 0.9);
        }
        // What the app does every UI frame: the plugin's requests, served
        // beside the loads, the saves and the rendering.
        main.service();
    }
    stop.store(true, Ordering::SeqCst);
    let (voice, blocks, peak) = render.join().map_err(|_| anyhow!("the render thread panicked"))?;
    println!("2. {loads} loads and {saves} saves against {blocks} rendered blocks (peak {peak:.3})");
    println!("3. plugin requests served meanwhile: {:?}", main.served());
    anyhow::ensure!(voice.process_errors() == 0, "process() reported {} errors", voice.process_errors());
    anyhow::ensure!(peak > 1e-3, "the voice stayed silent: nothing was rendered between the loads");
    main.retire(voice);
    println!("state-race-test: OK");
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DexedPatch {
    pub bank: String,
    pub program: usize,
    pub name: String,
    pub syx_path: std::path::PathBuf,
}

/// Parse a standard 4104-byte 32-voice DX7 SysEx bank (`F0 43 00 09 20 00`
/// header, 32 x 128-byte packed voices, 10-byte ASCII name at bytes 118..128).
pub fn parse_dx7_bank(bank: &str, path: &std::path::Path, bytes: &[u8]) -> Vec<DexedPatch> {
    const HEADER: [u8; 6] = [0xF0, 0x43, 0x00, 0x09, 0x20, 0x00];
    if bytes.len() != 4104 || !bytes.starts_with(&HEADER) {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(32);
    for program in 0..32 {
        let start = 6 + program * 128;
        let voice = &bytes[start..start + 128];
        let name: String = voice[118..128]
            .iter()
            .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { ' ' })
            .collect::<String>()
            .trim()
            .to_string();
        out.push(DexedPatch {
            bank: bank.to_string(),
            program,
            name,
            syx_path: path.to_path_buf(),
        });
    }
    out
}

/// Discover all DX7 `.syx` cartridge patches in
/// `~/.local/share/DigitalSuburban/Dexed/Cartridges` recursively.
pub fn discover_dexed_patches() -> Vec<DexedPatch> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root = std::path::PathBuf::from(home).join(".local/share/DigitalSuburban/Dexed/Cartridges");
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<DexedPatch>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
                continue;
            }
            if !p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("syx")) {
                continue;
            }
            let bank = p
                .strip_prefix(base)
                .ok()
                .map(|r| r.with_extension("").to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string()
                });
            if let Ok(bytes) = std::fs::read(&p) {
                out.extend(parse_dx7_bank(&bank, &p, &bytes));
            }
        }
    }
    walk(&root, &root, &mut out);
    out.sort_by(|a, b| (a.bank.to_lowercase(), a.program).cmp(&(b.bank.to_lowercase(), b.program)));
    out
}

const JUCE_B64_ALPHABET: &[u8; 64] =
    b".ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+";

/// Encode bytes using JUCE's `MemoryBlock::toBase64Encoding()` format:
/// `"<len>.<custom-base64>"`.
fn juce_b64_encode(bytes: &[u8]) -> String {
    let mut out = format!("{}.", bytes.len());
    out.reserve((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let mut v: u32 = chunk[0] as u32;
        if chunk.len() > 1 {
            v |= (chunk[1] as u32) << 8;
        }
        if chunk.len() > 2 {
            v |= (chunk[2] as u32) << 16;
        }
        let nchars = chunk.len() + 1;
        for i in 0..nchars {
            let idx = ((v >> (6 * i)) & 0x3F) as usize;
            out.push(JUCE_B64_ALPHABET[idx] as char);
        }
    }
    out
}

/// Unpack a 128-byte packed DX7 SysEx voice into Dexed's 161-byte `program` blob
/// (155-byte unpacked DX7 voice + 6 zeroed Dexed extension bytes).
fn unpack_dx7_voice(packed: &[u8]) -> [u8; 161] {
    let mut out = [0u8; 161];
    if packed.len() < 128 {
        return out;
    }
    for op in 0..6 {
        let p = &packed[op * 17..op * 17 + 17];
        let u = &mut out[op * 21..op * 21 + 21];
        u[0..11].copy_from_slice(&p[0..11]);
        u[11] = p[11] & 0x03;
        u[12] = (p[11] >> 2) & 0x03;
        u[13] = p[12] & 0x07;
        u[14] = p[13] & 0x03;
        u[15] = (p[13] >> 2) & 0x07;
        u[16] = p[14];
        u[17] = p[15] & 0x01;
        u[18] = (p[15] >> 1) & 0x1F;
        u[19] = p[16];
        u[20] = (p[12] >> 3) & 0x0F;
    }
    out[126..134].copy_from_slice(&packed[102..110]);
    out[134] = packed[110] & 0x1F;
    out[135] = packed[111] & 0x07;
    out[136] = (packed[111] >> 3) & 0x01;
    out[137..141].copy_from_slice(&packed[112..116]);
    out[141] = packed[116] & 0x01;
    out[142] = (packed[116] >> 1) & 0x07;
    out[143] = (packed[116] >> 4) & 0x07;
    out[144] = packed[117];
    out[145..155].copy_from_slice(&packed[118..128]);
    out
}

fn replace_xml_attr(xml: &str, attr: &str, value: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xml.find(&needle)? + needle.len();
    let end = start + xml[start..].find('"')?;
    let mut out = String::with_capacity(xml.len() + value.len());
    out.push_str(&xml[..start]);
    out.push_str(value);
    out.push_str(&xml[end..]);
    Some(out)
}

/// Load voice `program` (0..32) of a DX7 cartridge into a Dexed CLAP instance.
pub fn load_dexed_voice(main: &mut ClapVoiceMain, syx_path: &std::path::Path, program: usize) -> Result<()> {
    let syx = std::fs::read(syx_path)?;
    anyhow::ensure!(
        syx.len() == 4104 && syx.starts_with(&[0xF0, 0x43, 0x00, 0x09, 0x20, 0x00]),
        "invalid DX7 .syx bank: {}",
        syx_path.display()
    );
    anyhow::ensure!(program < 32, "program index {program} out of range");
    let voice_start = 6 + program * 128;
    let unpacked = unpack_dx7_voice(&syx[voice_start..voice_start + 128]);
    let sysex_b64 = juce_b64_encode(&syx);
    let prog_b64 = juce_b64_encode(&unpacked);

    let state = main.save_state()?;
    let xml_start = state
        .windows(5)
        .position(|w| w == b"<?xml")
        .ok_or_else(|| anyhow!("Dexed state missing <?xml header"))?;
    let prefix = &state[..xml_start];
    let mut xml = std::str::from_utf8(&state[xml_start..])
        .map_err(|e| anyhow!("Dexed state not UTF-8: {e}"))?
        .to_string();
    xml = replace_xml_attr(&xml, "currentProgram", &program.to_string())
        .ok_or_else(|| anyhow!("Dexed state missing currentProgram"))?;
    xml = replace_xml_attr(&xml, "base64:sysex", &sysex_b64)
        .ok_or_else(|| anyhow!("Dexed state missing base64:sysex"))?;
    xml = replace_xml_attr(&xml, "base64:program", &prog_b64)
        .ok_or_else(|| anyhow!("Dexed state missing base64:program"))?;

    let mut new_state = Vec::with_capacity(prefix.len() + xml.len());
    new_state.extend_from_slice(prefix);
    new_state.extend_from_slice(xml.as_bytes());
    main.load_state(&new_state)
}

const CLAP_SCAN_PARALLELISM: usize = 4;
const CLAP_SCAN_TIMEOUT: Duration = Duration::from_secs(5);
const CLAP_SCAN_CACHE_VERSION: u32 = 2;
/// Bytes of a failing scanner's stderr kept in the error (its tail).
const SCAN_ERR_TAIL: usize = 2048;
/// Upper bound on a scanner's JSON stdout.
const SCAN_STDOUT_MAX: usize = 1 << 20;
static SCAN_CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BundleStamp {
    mtime_secs: i64,
    mtime_nanos: u32,
    size: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedBundle {
    #[serde(flatten)]
    stamp: BundleStamp,
    plugins: Vec<ClapPluginInfo>,
    /// Why this exact bundle failed to scan. A broken bundle is not rescanned
    /// (and its timeout not paid again) until its file changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ClapScanCache {
    version: u32,
    /// App version that wrote it: an upgrade rescans cached failures.
    #[serde(default)]
    app_version: String,
    entries: std::collections::BTreeMap<String, CachedBundle>,
}

impl Default for ClapScanCache {
    fn default() -> Self {
        Self {
            version: CLAP_SCAN_CACHE_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            entries: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
enum ScannerCommand {
    #[cfg_attr(test, allow(dead_code))]
    CurrentExe,
    #[cfg_attr(not(test), allow(dead_code))]
    Custom { program: PathBuf, args: Vec<String> },
    #[cfg(test)]
    InProcess,
}

impl ScannerCommand {
    fn default_for_process() -> Self {
        #[cfg(test)]
        {
            // Under `cargo test`, `current_exe()` is the libtest harness, which ignores `scan-clap`.
            Self::InProcess
        }
        #[cfg(not(test))]
        {
            Self::CurrentExe
        }
    }
}

fn bundle_stamp(path: &std::path::Path) -> Option<BundleStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let (mtime_secs, mtime_nanos) = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            (-(d.as_secs() as i64), d.subsec_nanos())
        }
    };
    Some(BundleStamp {
        mtime_secs,
        mtime_nanos,
        size: meta.len(),
    })
}

fn clap_scan_cache_path_from(
    xdg_cache_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let base = xdg_cache_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("kabelsalat").join("clap-scan.json"))
}

fn clap_scan_cache_path_from_env() -> Option<PathBuf> {
    clap_scan_cache_path_from(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn clap_scan_cache_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    clap_scan_cache_path_from_env()
}

fn load_scan_cache(path: &std::path::Path) -> ClapScanCache {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ClapScanCache::default();
    };
    match serde_json::from_str::<ClapScanCache>(&raw) {
        Ok(mut c) if c.version == CLAP_SCAN_CACHE_VERSION => {
            if c.app_version != env!("CARGO_PKG_VERSION") {
                // A new app version retries what failed; good entries stay.
                c.entries.retain(|_, e| e.error.is_none());
                c.app_version = env!("CARGO_PKG_VERSION").to_string();
            }
            c
        }
        _ => ClapScanCache::default(),
    }
}

fn save_scan_cache(path: &std::path::Path, cache: &ClapScanCache) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("cache path has no parent: {}", path.display());
    };
    std::fs::create_dir_all(parent)?;
    let body = serde_json::to_string_pretty(cache)?;
    let seq = SCAN_CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".clap-scan.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Load one CLAP bundle and read its plugin descriptors.
pub fn scan_clap_bundle(path: &std::path::Path) -> Result<Vec<ClapPluginInfo>> {
    let entry = open_bundle(path)?;
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| anyhow!("CLAP bundle {} has no plugin factory", path.display()))?;
    let mut out = Vec::new();
    for d in factory.plugin_descriptors() {
        out.push(ClapPluginInfo {
            id: d.id().unwrap_or(c"?").to_string_lossy().into_owned(),
            name: d.name().unwrap_or(c"?").to_string_lossy().into_owned(),
            vendor: d.vendor().unwrap_or(c"?").to_string_lossy().into_owned(),
            features: d.features().map(|f| f.to_string_lossy().into_owned()).collect(),
        });
    }
    Ok(out)
}

/// Reroute fd 1 to stderr so plugin init logs cannot corrupt `scan-clap` JSON stdout.
fn reroute_stdout_to_stderr() -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    unsafe {
        let saved = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if saved < 0 {
            bail!("dup(stdout) failed: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            libc::close(saved);
            bail!("dup2(stderr, stdout) failed: {}", std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_fd(saved))
    }
}

/// CLI entrypoint for `kabelsalat scan-clap <bundle-path>`.
pub fn run_scan_clap(path: &std::path::Path) -> Result<()> {
    use std::io::Write;
    // A scanner stuck in a hanging plugin must not outlive the app.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        libc::alarm(30);
    }
    let mut json_out = reroute_stdout_to_stderr()?;
    let plugins = scan_clap_bundle(path)?;
    let json = serde_json::to_string(&plugins)?;
    writeln!(json_out, "{json}")?;
    json_out.flush()?;
    Ok(())
}

fn parse_scan_output(stdout: &[u8]) -> Result<Vec<ClapPluginInfo>> {
    let text = std::str::from_utf8(stdout).context("scanner stdout is not UTF-8")?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("scanner produced empty stdout");
    }
    serde_json::from_str::<Vec<ClapPluginInfo>>(trimmed).context("scanner produced invalid JSON")
}

/// Read a pipe to EOF on its own thread, keeping at most `keep` bytes (the
/// head when `tail` is false, else the tail), and hand `(bytes, truncated)`
/// over the channel. The caller waits with a deadline: a helper process that
/// inherited the pipe can hold it open forever, and that thread is then
/// abandoned.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    reader: Option<R>,
    keep: usize,
    tail: bool,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut buf, mut truncated) = (Vec::new(), false);
        if let Some(mut r) = reader {
            let mut chunk = [0u8; 8192];
            loop {
                let n = match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if tail {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > keep {
                        buf.drain(..buf.len() - keep);
                        truncated = true;
                    }
                } else if buf.len() < keep {
                    let take = n.min(keep - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    truncated |= take < n;
                } else {
                    truncated = true; // keep draining so the child never blocks
                }
            }
        }
        let _ = tx.send((buf, truncated));
    });
    rx
}

/// Why a bundle produced no plugins. Only a `Verdict` (the scanner ran and
/// exited badly, printed garbage or hung) says something about the bundle
/// and may be cached; a `Setup` failure (spawn, wait) says nothing about it.
#[derive(Debug)]
enum ScanFail {
    Verdict(String),
    Setup(String),
}

impl std::fmt::Display for ScanFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanFail::Verdict(m) | ScanFail::Setup(m) => f.write_str(m),
        }
    }
}

fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        // Negative PID targets the child's process group (set via process_group(0))
        // so wrapper shells cannot leave grandchildren holding stdout/stderr pipes open.
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_scan_subprocess(
    program: &std::path::Path,
    prefix_args: &[String],
    bundle: &std::path::Path,
    timeout: Duration,
) -> Result<Vec<ClapPluginInfo>, ScanFail> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(prefix_args)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| ScanFail::Setup(format!("failed to spawn scanner {}: {e}", program.display())))?;
    let stdout_rx = spawn_pipe_reader(child.stdout.take(), SCAN_STDOUT_MAX, false);
    let stderr_rx = spawn_pipe_reader(child.stderr.take(), SCAN_ERR_TAIL, true);

    let deadline = Instant::now() + timeout;
    let timed_out = || ScanFail::Verdict(format!("timed out after {:.1}s", timeout.as_secs_f32()));
    let pid = child.id() as libc::pid_t;
    // Wait for the exit WITHOUT reaping (WNOWAIT): the zombie keeps the pid
    // and its process group reserved, so kill(-pid) below can never hit a
    // recycled group, whatever helpers the plugin started or detached.
    let info = loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            kill_child_tree(&mut child);
            return Err(ScanFail::Setup(format!("failed waiting on scanner: {e}")));
        }
        if unsafe { info.si_pid() } != 0 {
            break info;
        }
        if Instant::now() >= deadline {
            kill_child_tree(&mut child);
            return Err(timed_out());
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    // A short grace even at the deadline: the readers may not have sent yet.
    let remaining = || deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(200));
    let stdout = stdout_rx.recv_timeout(remaining());
    let exited_ok = info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0;
    // stderr only matters for a failure; a helper holding it must not fail a good scan.
    let stderr = if exited_ok { None } else { stderr_rx.recv_timeout(remaining()).ok() };
    // Helpers the plugin left behind die with the group; then reap.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let status = child.wait().map_err(|e| ScanFail::Setup(format!("failed reaping scanner: {e}")))?;
    if !status.success() {
        let (err, truncated) = stderr.unwrap_or_default();
        let err_tail = String::from_utf8_lossy(&err).trim().to_string();
        if err_tail.is_empty() {
            return Err(ScanFail::Verdict(format!("scanner exited with {status}")));
        }
        let more = if truncated { "…" } else { "" };
        return Err(ScanFail::Verdict(format!("scanner exited with {status}: {more}{err_tail}")));
    }
    let Ok((stdout, stdout_truncated)) = stdout else {
        return Err(timed_out());
    };
    if stdout_truncated {
        return Err(ScanFail::Verdict(format!("scanner stdout exceeded {} bytes", SCAN_STDOUT_MAX)));
    }
    parse_scan_output(&stdout).map_err(|e| ScanFail::Verdict(format!("{e:#}")))
}

fn scan_one_bundle(
    cmd: &ScannerCommand,
    bundle: &std::path::Path,
    timeout: Duration,
) -> Result<Vec<ClapPluginInfo>, ScanFail> {
    match cmd {
        // /proc/self/exe is this very binary even after it was replaced on
        // disk (rebuild, package upgrade), where current_exe() is "(deleted)".
        ScannerCommand::CurrentExe => {
            let args = vec!["scan-clap".to_string()];
            run_scan_subprocess(std::path::Path::new("/proc/self/exe"), &args, bundle, timeout)
        }
        ScannerCommand::Custom { program, args } => {
            run_scan_subprocess(program, args, bundle, timeout)
        }
        #[cfg(test)]
        ScannerCommand::InProcess => scan_clap_bundle(bundle).map_err(|e| ScanFail::Verdict(format!("{e:#}"))),
    }
}

/// Scan every bundle (cache first, then out of process) and return what each
/// one holds, `None` for a bundle that failed. `retry_failed` rescans bundles
/// whose failure is cached (the plugin browser's Refresh).
fn scan_bundles_with(
    paths: &[PathBuf],
    cmd: &ScannerCommand,
    cache_path: Option<&std::path::Path>,
    timeout: Duration,
    parallelism: usize,
    retry_failed: bool,
) -> Vec<Option<Vec<ClapPluginInfo>>> {
    let mut cache = cache_path.map(load_scan_cache).unwrap_or_default();
    let mut per_bundle: Vec<Option<Vec<ClapPluginInfo>>> = vec![None; paths.len()];
    let mut uncached: Vec<(usize, &PathBuf, Option<BundleStamp>)> = Vec::new();

    for (idx, path) in paths.iter().enumerate() {
        // Asked before the cache, so a failure cached with dlopen's "No such
        // file" also reads as what it is (#135).
        if let Some(reason) = crate::binarch::foreign(path) {
            eprintln!("skip CLAP bundle {}: {reason}", path.display());
            continue;
        }
        let stamp = bundle_stamp(path);
        let key = path.to_string_lossy();
        if let (Some(st), Some(cached)) = (&stamp, cache.entries.get(key.as_ref())) {
            if &cached.stamp == st {
                match &cached.error {
                    Some(_) if retry_failed => {}
                    Some(e) => {
                        eprintln!("skip CLAP bundle {} (cached failure): {e}", path.display());
                        continue;
                    }
                    None => {
                        per_bundle[idx] = Some(cached.plugins.clone());
                        continue;
                    }
                }
            }
        }
        uncached.push((idx, path, stamp));
    }

    if !uncached.is_empty() {
        let workers = uncached.len().min(parallelism.max(1));
        let cursor = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Option<BundleStamp>, Result<Vec<ClapPluginInfo>, ScanFail>)>> =
            Mutex::new(Vec::with_capacity(uncached.len()));

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(&(idx, path, ref stamp)) = uncached.get(i) else {
                        break;
                    };
                    let res = scan_one_bundle(cmd, path, timeout);
                    if let Ok(mut guard) = results.lock() {
                        guard.push((idx, stamp.clone(), res));
                    }
                });
            }
        });

        let mut cache_dirty = false;
        if let Ok(scanned) = results.into_inner() {
            for (idx, stamp, res) in scanned {
                let key = paths[idx].to_string_lossy().into_owned();
                match res {
                    Ok(plugins) => {
                        if let Some(st) = stamp {
                            cache.entries.insert(key, CachedBundle { stamp: st, plugins: plugins.clone(), error: None });
                            cache_dirty = true;
                        }
                        per_bundle[idx] = Some(plugins);
                    }
                    Err(e) => {
                        eprintln!("skip CLAP bundle {}: {e}", paths[idx].display());
                        if let (ScanFail::Verdict(msg), Some(st)) = (e, stamp) {
                            cache.entries.insert(key, CachedBundle { stamp: st, plugins: Vec::new(), error: Some(msg) });
                            cache_dirty = true;
                        }
                    }
                }
            }
        }

        if cache_dirty {
            if let Some(cp) = cache_path {
                if let Err(e) = save_scan_cache(cp, &cache) {
                    eprintln!("failed to save CLAP scan cache {}: {e}", cp.display());
                }
            }
        }
    }
    per_bundle
}

/// The plugins of every bundle, first-seen id wins, in bundle order.
fn dedupe_plugins(per_bundle: Vec<Option<Vec<ClapPluginInfo>>>) -> Vec<ClapPluginInfo> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for plugins in per_bundle.into_iter().flatten() {
        for p in plugins {
            if seen.insert(p.id.clone()) {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
fn list_clap_plugins_with(
    paths: &[PathBuf],
    cmd: &ScannerCommand,
    cache_path: Option<&std::path::Path>,
    timeout: Duration,
    parallelism: usize,
) -> Vec<ClapPluginInfo> {
    dedupe_plugins(scan_bundles_with(paths, cmd, cache_path, timeout, parallelism, false))
}

fn scan_installed_bundles(retry_failed: bool) -> Vec<(PathBuf, Option<Vec<ClapPluginInfo>>)> {
    let paths = clap_bundle_paths();
    let cache_path = clap_scan_cache_path();
    let per_bundle = scan_bundles_with(
        &paths,
        &ScannerCommand::default_for_process(),
        cache_path.as_deref(),
        CLAP_SCAN_TIMEOUT,
        CLAP_SCAN_PARALLELISM,
        retry_failed,
    );
    paths.into_iter().zip(per_bundle).collect()
}

/// Why each bundle on the CLAP search paths that was built for another CPU
/// or system cannot load here, one sentence per bundle. Reads file headers
/// only.
pub fn unloadable_clap_bundles() -> Vec<String> {
    clap_bundle_paths().iter().filter_map(|p| crate::binarch::foreign(p)).collect()
}

/// Every installed CLAP plugin, scanned out of process (cached).
pub fn list_clap_plugins() -> Vec<ClapPluginInfo> {
    dedupe_plugins(scan_installed_bundles(false).into_iter().map(|(_, p)| p).collect())
}

/// Like [`list_clap_plugins`], but rescans bundles whose failure is cached.
pub fn list_clap_plugins_retry_failed() -> Vec<ClapPluginInfo> {
    dedupe_plugins(scan_installed_bundles(true).into_iter().map(|(_, p)| p).collect())
}

/// The first bundle holding a plugin that matches `wanted`, found from the
/// out-of-process scan, so only that one bundle is ever loaded in the app.
fn bundle_with(wanted: impl Fn(&ClapPluginInfo) -> bool) -> Option<(PathBuf, String)> {
    scan_installed_bundles(false).into_iter().find_map(|(path, plugins)| {
        let p = plugins?.into_iter().find(|p| wanted(p))?;
        Some((path, p.id))
    })
}

/// `clap-scan-test`: list CLAP plugins out of process (cold, then from the
/// cache) and print timings + plugin ids. Never loads a bundle in this
/// process, so it stays safe with crashing or hanging bundles on CLAP_PATH.
pub fn run_clap_scan_test() -> Result<()> {
    // A private cache, so "cold" is cold and the user's cache stays untouched.
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let tmp = RemoveOnDrop(std::env::temp_dir().join(format!("kabelsalat-clap-scan-test-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    std::env::set_var("XDG_CACHE_HOME", &tmp.0);
    let t0 = Instant::now();
    let first = list_clap_plugins();
    let d1 = t0.elapsed();

    let t1 = Instant::now();
    let second = list_clap_plugins();
    let d2 = t1.elapsed();

    anyhow::ensure!(first == second, "cold and warm listings differ");
    let ids: Vec<&str> = first.iter().map(|p| p.id.as_str()).collect();
    println!(
        "clap-scan-test: OK — {} plugins ({:?}) · cold: {:.2} ms · warm: {:.2} ms",
        first.len(),
        ids,
        d1.as_secs_f64() * 1000.0,
        d2.as_secs_f64() * 1000.0,
    );
    let t2 = Instant::now();
    let retried = list_clap_plugins_retry_failed();
    println!("clap-scan-test: retry of cached failures took {:.2} ms", t2.elapsed().as_secs_f64() * 1000.0);
    anyhow::ensure!(retried == first, "retrying failures changed the listing");
    Ok(())
}

// ---------------------------------------------------------------------------
// Params / ports inspection
// ---------------------------------------------------------------------------

struct ModParam {
    id: ClapId,
    orig: f64,
    min: f64,
    max: f64,
    target: f64,
    name: String,
}

fn inspect_params(plugin: &mut PluginInstance<TestHost>, params: Option<PluginParams>) -> Result<Option<ModParam>> {
    let Some(params) = params else {
        println!("params: extension not supported");
        return Ok(None);
    };
    let count = params.count(&plugin.plugin_handle());
    println!("params: {count} total; first 10:");
    let mut info_buf = ParamInfoBuffer::new();
    let mut text_buf = [0u8; 256];
    let mut chosen: Option<ModParam> = None;
    for i in 0..count {
        let Some(info) = params.get_info(&plugin.plugin_handle(), i, &mut info_buf) else {
            continue;
        };
        let name = String::from_utf8_lossy(info.name).into_owned();
        let module = String::from_utf8_lossy(info.module).into_owned();
        let value = params.get_value(&plugin.plugin_handle(), info.id).unwrap_or(f64::NAN);
        if i < 10 {
            let text = params
                .value_to_text(&plugin.plugin_handle(), info.id, value, &mut text_buf)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_else(|_| "?".into());
            println!(
                "  [{i}] id={} {module}/{name} = {value:.4} ({text}) range {:.3}..{:.3} default {:.3}",
                info.id.get(), info.min_value, info.max_value, info.default_value
            );
        }
        // First plain automatable param is our modulation/round-trip probe.
        if chosen.is_none()
            && info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE)
            && !info.flags.contains(ParamInfoFlags::IS_READONLY)
            && !info.flags.contains(ParamInfoFlags::IS_HIDDEN)
            && info.max_value > info.min_value
        {
            let span = info.max_value - info.min_value;
            let mut target = info.min_value + 0.75 * span;
            if (target - value).abs() < span * 0.05 {
                target = info.min_value + 0.25 * span;
            }
            chosen = Some(ModParam {
                id: info.id,
                orig: value,
                min: info.min_value,
                max: info.max_value,
                target,
                name: format!("{module}/{name}"),
            });
        }
    }
    if let Some(mp) = &chosen {
        println!(
            "probe param: '{}' (id {}) {:.6} -> {:.6} via scheduled ParamValueEvent",
            mp.name, mp.id.get(), mp.orig, mp.target
        );
    }
    Ok(chosen)
}

/// Returns (input port count, input channel count, per-output-port channel
/// counts, index of the main output port). Surge XT 1.3.4 exposes 1 stereo
/// sidechain input and 3 stereo outputs (main mix + Scene A + Scene B); we
/// render all of them but only mix the main one to the device.
fn inspect_audio_ports(
    plugin: &mut PluginInstance<TestHost>,
    ports: Option<PluginAudioPorts>,
) -> Result<(Vec<usize>, Option<usize>, Vec<usize>, usize)> {
    let Some(ports) = ports else {
        println!("audio-ports: extension not supported; assuming stereo out, no in");
        return Ok((Vec::new(), None, vec![2], 0));
    };
    let mut in_chs: Vec<usize> = Vec::new();
    let mut main_in: Option<usize> = None;
    let mut out_chs: Vec<usize> = Vec::new();
    let mut main_out: Option<usize> = None;
    let mut buf = AudioPortInfoBuffer::default();
    for is_input in [true, false] {
        let n = ports.count(&plugin.plugin_handle(), is_input);
        let dir = if is_input { "in" } else { "out" };
        for i in 0..n {
            if let Some(info) = ports.get(&plugin.plugin_handle(), i, is_input, &mut buf) {
                let name = String::from_utf8_lossy(info.name).into_owned();
                let ptype = info
                    .port_type
                    .map(|t| String::from_utf8_lossy(t.0.to_bytes()).into_owned())
                    .unwrap_or_else(|| "?".into());
                let is_main = info
                    .flags
                    .contains(clack_extensions::audio_ports::AudioPortFlags::IS_MAIN);
                println!(
                    "audio {dir} port [{i}] '{name}': {} ch, type {ptype}, main={is_main}",
                    info.channel_count
                );
                if is_input {
                    if is_main && main_in.is_none() {
                        main_in = Some(in_chs.len());
                    }
                    in_chs.push(info.channel_count as usize);
                } else {
                    if is_main && main_out.is_none() {
                        main_out = Some(out_chs.len());
                    }
                    out_chs.push(info.channel_count as usize);
                }
            }
        }
    }
    let main_out = main_out
        .or_else(|| (!out_chs.is_empty()).then_some(0))
        .ok_or_else(|| anyhow!("plugin exposes no audio output port"))?;
    if out_chs[main_out] == 0 {
        bail!("main output port has 0 channels");
    }
    Ok((in_chs, main_in, out_chs, main_out))
}

// ---------------------------------------------------------------------------
// Arpeggio schedule + render loop (host-test demo, built on ClapVoice)
// ---------------------------------------------------------------------------

fn build_schedule(sample_rate: f64, mod_param: Option<&ModParam>) -> Vec<(u64, VoiceCmd)> {
    // A-minor arpeggio, eighth notes at 132 BPM, 4 cycles (~7.3 s) + release tail.
    let step = (60.0 / 132.0 / 2.0 * sample_rate) as u64;
    let notes: [u16; 8] = [57, 60, 64, 69, 72, 69, 64, 60];
    let mut schedule = Vec::new();
    for cycle in 0..4u64 {
        for (i, &key) in notes.iter().enumerate() {
            let t = (cycle * 8 + i as u64) * step;
            schedule.push((t, VoiceCmd::NoteOn(key, 0.85)));
            schedule.push((t + step * 3 / 4, VoiceCmd::NoteOff(key)));
        }
    }
    if let Some(mp) = mod_param {
        schedule.push((sample_rate as u64, VoiceCmd::Param(mp.id, mp.target)));
    }
    schedule
}

#[derive(Default)]
struct Stats {
    blocks: AtomicU64,
    nonquiet: AtomicU64,
    peak_bits: AtomicU32,
    callbacks: AtomicU64,
    underruns: AtomicU64,
}

/// Audio-side demo loop: render through the public `ClapVoice::render` API and
/// pace into the device ring. Returns the CLAP process-error count.
fn voice_render_loop(
    mut voice: ClapVoice,
    stop: Arc<AtomicBool>,
    mut producer: rtrb::Producer<f32>,
    stats: Arc<Stats>,
    tx: mpsc::SyncSender<ClapVoice>,
) -> Result<u64> {
    let mut block = [0.0f32; BLOCK * 2];
    while !stop.load(Ordering::Relaxed) {
        voice.render(&mut block, Transport::default());
        // Interleave into the ring; block (sleep) when full -> real-time pacing.
        let mut peak = 0.0f32;
        for &s in &block {
            peak = peak.max(s.abs());
            loop {
                match producer.push(s) {
                    Ok(()) => break,
                    Err(_) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
            }
        }
        stats.blocks.fetch_add(1, Ordering::Relaxed);
        if peak > 1e-4 {
            stats.nonquiet.fetch_add(1, Ordering::Relaxed);
        }
        stats.peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
    }

    // Back to the main thread for deactivation (CLAP lifecycle rule).
    let errors = voice.process_errors();
    let _ = tx.send(voice);
    Ok(errors)
}

#[cfg(test)]
mod patch_search_tests {
    use super::*;

    #[test]
    fn non_surge_clap_discovers_and_renders_when_installed() {
        let plugins = list_clap_plugins();
        let Some(dexed) = plugins.iter().find(|p| p.name.to_lowercase().contains("dexed")) else {
            return; // skip cleanly on machines without dexed-clap
        };
        let (mut voice, mut main) = ClapVoice::new_plugin(48000.0, &dexed.id)
            .expect("Dexed CLAP should instantiate");
        assert!(!main.param_list().is_empty(), "Dexed should expose parameters");
        voice.note_on(60, 0.8);
        let mut buf = vec![0.0f32; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..8 {
            voice.render(&mut buf, Transport::default());
            for &s in &buf {
                peak = peak.max(s.abs());
            }
        }
        assert!(peak > 1e-4, "Dexed should render non-zero audio (peak={peak})");
        let state = main.save_state().expect("Dexed save_state");
        println!("DEXED STATE ({} bytes): {}", state.len(), String::from_utf8_lossy(&state));
        main.retire(voice);
    }

    #[test]
    fn dexed_load_patch_changes_state_and_renders() {
        let plugins = list_clap_plugins();
        let Some(dexed) = plugins.iter().find(|p| p.id == "com.digital-suburban.dexed") else {
            return;
        };
        let patches = discover_dexed_patches();
        if patches.is_empty() {
            return;
        }
        let target = patches.iter().find(|p| p.program != 0).unwrap_or(&patches[0]);
        let (mut voice, mut main) = ClapVoice::new_plugin(48000.0, &dexed.id)
            .expect("Dexed CLAP should instantiate");
        let before = main.save_state().expect("save before");
        load_dexed_voice(&mut main, &target.syx_path, target.program).expect("load_dexed_voice");
        let mut buf = vec![0.0f32; 256 * 2];
        for _ in 0..4 {
            voice.render(&mut buf, Transport::default());
        }
        let after = main.save_state().expect("save after");
        assert_ne!(before, after, "Dexed state should change after loading patch");
        voice.note_on(60, 0.9);
        let mut peak = 0.0f32;
        for _ in 0..8 {
            voice.render(&mut buf, Transport::default());
            for &s in &buf {
                peak = peak.max(s.abs());
            }
        }
        assert!(peak > 1e-4, "Dexed should render non-zero audio after patch load (peak={peak})");
        main.retire(voice);
    }

    /// Retiring a Dexed instance parks it instead of destroying it (#52:
    /// its destructor segfaults while other instances render), and a later
    /// `new_plugin` reuses the parked instance — reset to fresh state.
    #[test]
    fn dexed_is_parked_and_reused_not_destroyed() {
        let plugins = list_clap_plugins();
        let Some(dexed) = plugins.iter().find(|p| p.id == "com.digital-suburban.dexed") else {
            return; // skip cleanly on machines without dexed-clap
        };
        let before = parked_clap_count(&dexed.id);
        let (voice, mut main) = ClapVoice::new_plugin(48000.0, &dexed.id)
            .expect("Dexed CLAP should instantiate");
        assert_eq!(parked_clap_count(&dexed.id), before, "a live instance is not parked");
        // Customize the instance so we can prove the reuse resets its state.
        let patches = discover_dexed_patches();
        let stock = main.save_state().expect("stock state");
        if let Some(target) = patches.iter().find(|p| p.program != 0) {
            load_dexed_voice(&mut main, &target.syx_path, target.program).expect("load_dexed_voice");
        }
        let custom = main.save_state().expect("customized state");
        main.retire(voice);
        assert_eq!(parked_clap_count(&dexed.id), before + 1, "retire parks a never-free instance");

        let (mut voice, mut main) = ClapVoice::new_plugin(48000.0, &dexed.id)
            .expect("Dexed CLAP should instantiate again");
        assert_eq!(parked_clap_count(&dexed.id), before, "the second instance reused the parked one");
        // The reset lands on the audio thread a few blocks later (longer on a
        // loaded machine): render until the customized patch is gone, bounded.
        // Not compared byte for byte with a fresh instance: Dexed's fresh state
        // also depends on what other instances in the process did.
        if custom != stock {
            let mut buf = vec![0.0f32; 256 * 2];
            let deadline = Instant::now() + Duration::from_secs(5);
            let reused = loop {
                for _ in 0..8 {
                    voice.render(&mut buf, Transport::default());
                }
                let state = main.save_state().expect("reused state");
                if state != custom || Instant::now() > deadline {
                    break state;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_ne!(reused, custom, "a reused instance must not keep the previous unit's patch");
        }
        voice.note_on(60, 0.8);
        let mut buf = vec![0.0f32; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..8 {
            voice.render(&mut buf, Transport::default());
            for &s in &buf {
                peak = peak.max(s.abs());
            }
        }
        assert!(peak > 1e-4, "reused Dexed should render non-zero audio (peak={peak})");
        main.retire(voice);
    }
}

#[cfg(test)]
mod clap_scan_tests {
    use super::*;
    use std::ffi::OsStr;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let seq = SCAN_CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "ksalat-{label}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sh_cmd(script: &str) -> ScannerCommand {
        ScannerCommand::Custom {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string(), "_".to_string()],
        }
    }

    #[test]
    fn parse_scan_output_handles_valid_empty_and_garbage() {
        let valid = br#"[{"id":"org.test.a","name":"Test A","vendor":"Acme","features":["instrument"]}]"#;
        let parsed = parse_scan_output(valid).expect("valid JSON should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "org.test.a");
        assert_eq!(parsed[0].features, vec!["instrument"]);

        let empty_arr = parse_scan_output(b"[]\n").expect("empty array should parse");
        assert!(empty_arr.is_empty());

        assert!(parse_scan_output(b"").is_err());
        assert!(parse_scan_output(b"   \n").is_err());
        assert!(parse_scan_output(b"not json at all").is_err());
        assert!(parse_scan_output(&[0xff, 0xfe, 0x00]).is_err());
    }

    #[test]
    fn timeout_kills_hanging_scanner_and_skips_bundle() {
        let tmp = TempDir::new("timeout");
        let bundle = tmp.0.join("hang.clap");
        std::fs::write(&bundle, b"fake").unwrap();

        let cmd = sh_cmd("sleep 10");
        let start = Instant::now();
        let plugins = list_clap_plugins_with(
            &[bundle],
            &cmd,
            None,
            Duration::from_millis(150),
            CLAP_SCAN_PARALLELISM,
        );
        let elapsed = start.elapsed();

        assert!(plugins.is_empty());
        assert!(
            elapsed < Duration::from_secs(2),
            "hanging child should be killed on timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn crash_and_garbage_bundles_are_skipped_while_valid_survive_in_order() {
        let tmp = TempDir::new("mixed");
        let good1 = tmp.0.join("01-good.clap");
        let crash = tmp.0.join("02-crash.clap");
        let garbage = tmp.0.join("03-garbage.clap");
        let good2 = tmp.0.join("04-good.clap");
        for p in [&good1, &crash, &garbage, &good2] {
            std::fs::write(p, b"fake").unwrap();
        }

        // Script branches on bundle filename ($1): crashes, emits garbage, or valid JSON
        // (with a duplicate id in good2 to verify first-seen deduplication).
        let cmd = sh_cmd(
            r#"case "$1" in
                *01-good.clap) printf '[{"id":"id.one","name":"One","vendor":"V","features":["instrument"]},{"id":"id.shared","name":"First","vendor":"V","features":[]}]\n' ;;
                *02-crash.clap) exit 1 ;;
                *03-garbage.clap) printf 'not valid json\n' ;;
                *04-good.clap) printf '[{"id":"id.shared","name":"Second","vendor":"V","features":[]},{"id":"id.two","name":"Two","vendor":"V","features":["audio-effect"]}]\n' ;;
            esac"#,
        );
        let plugins = list_clap_plugins_with(
            &[good1, crash, garbage, good2],
            &cmd,
            None,
            Duration::from_secs(2),
            CLAP_SCAN_PARALLELISM,
        );
        let ids: Vec<&str> = plugins.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["id.one", "id.shared", "id.two"]);
        assert_eq!(plugins[1].name, "First");

        let false_cmd = ScannerCommand::Custom {
            program: PathBuf::from("/bin/false"),
            args: Vec::new(),
        };
        let empty = list_clap_plugins_with(
            &[tmp.0.join("01-good.clap")],
            &false_cmd,
            None,
            Duration::from_secs(2),
            1,
        );
        assert!(empty.is_empty());
    }

    #[test]
    fn a_scanner_that_cannot_start_caches_nothing() {
        let tmp = TempDir::new("setupfail");
        let bundle = tmp.0.join("good.clap");
        std::fs::write(&bundle, b"v1").unwrap();
        let cache_file = tmp.0.join("cache").join("clap-scan.json");
        let missing = ScannerCommand::Custom { program: tmp.0.join("no-such-scanner"), args: Vec::new() };
        let none = list_clap_plugins_with(&[bundle.clone()], &missing, Some(&cache_file), Duration::from_secs(2), 1);
        assert!(none.is_empty());
        let good = sh_cmd(r#"printf '[{"id":"id.good","name":"G","vendor":"V","features":[]}]\n'"#);
        let listed = list_clap_plugins_with(&[bundle], &good, Some(&cache_file), Duration::from_secs(2), 1);
        assert_eq!(listed.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["id.good"], "a spawn error must not be cached against the bundle");
    }

    #[test]
    fn a_helper_holding_the_pipes_cannot_stall_the_listing() {
        let tmp = TempDir::new("helper");
        let bundle = tmp.0.join("helper.clap");
        std::fs::write(&bundle, b"v1").unwrap();
        let start = Instant::now();
        let out = list_clap_plugins_with(&[bundle], &sh_cmd("(sleep 6 &); printf '[]'"), None, Duration::from_millis(500), 1);
        assert!(out.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2), "inherited pipe stalled the listing for {:?}", start.elapsed());
    }

    #[test]
    fn a_helper_holding_only_stderr_does_not_fail_a_good_scan() {
        let tmp = TempDir::new("stderrhelper");
        let bundle = tmp.0.join("ok.clap");
        std::fs::write(&bundle, b"v1").unwrap();
        let start = Instant::now();
        let cmd = sh_cmd(r#"(sleep 6 >/dev/null &); printf '[{"id":"id.ok","name":"O","vendor":"V","features":[]}]\n'"#);
        let out = list_clap_plugins_with(&[bundle], &cmd, None, Duration::from_secs(2), 1);
        assert_eq!(out.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["id.ok"]);
        assert!(start.elapsed() < Duration::from_secs(1), "waited on stderr for {:?}", start.elapsed());
    }

    #[test]
    fn a_new_app_version_keeps_good_entries_and_retries_failures() {
        let tmp = TempDir::new("appver");
        let cache_file = tmp.0.join("clap-scan.json");
        let stamp = BundleStamp { mtime_secs: 1, mtime_nanos: 0, size: 1 };
        let mut old = ClapScanCache { app_version: "0.0.0-old".into(), ..Default::default() };
        let good = ClapPluginInfo { id: "id.g".into(), name: "G".into(), vendor: "V".into(), features: vec![] };
        old.entries.insert("good".into(), CachedBundle { stamp: stamp.clone(), plugins: vec![good], error: None });
        old.entries.insert("bad".into(), CachedBundle { stamp, plugins: vec![], error: Some("timed out".into()) });
        save_scan_cache(&cache_file, &old).unwrap();
        let loaded = load_scan_cache(&cache_file);
        assert_eq!(loaded.entries.keys().collect::<Vec<_>>(), vec!["good"]);
        assert_eq!(loaded.app_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn timeout_kills_the_scanners_whole_process_group() {
        let tmp = TempDir::new("pgkill");
        let bundle = tmp.0.join("hang.clap");
        std::fs::write(&bundle, b"v1").unwrap();
        let pid_file = tmp.0.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait; true", pid_file.display());
        list_clap_plugins_with(&[bundle], &sh_cmd(&script), None, Duration::from_millis(300), 1);
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let dead = (0..50).any(|_| {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let gone = state.is_empty() || state.split(") ").nth(1).map_or(false, |r| r.starts_with('Z'));
            if !gone {
                std::thread::sleep(Duration::from_millis(20));
            }
            gone
        });
        assert!(dead, "grandchild {pid} survived the scanner timeout");
    }

    /// glibc calls an ELF of another CPU "No such file or directory" (#135).
    #[test]
    fn a_bundle_built_for_another_cpu_says_so_and_is_never_handed_to_the_scanner() {
        let tmp = TempDir::new("foreign");
        let bundle = tmp.0.join("Vital.clap");
        let (machine, name) = crate::binarch::other_machine();
        std::fs::write(&bundle, crate::binarch::fake_elf(machine)).unwrap();
        let want = format!("Vital.clap is built for {name}; this machine is {}, so it cannot be loaded", std::env::consts::ARCH);

        let err = scan_clap_bundle(&bundle).expect_err("a foreign bundle cannot be scanned");
        assert_eq!(format!("{err:#}"), want);
        let err = ClapVoice::new_from_bundle(48000.0, &bundle, "audio.vital.synth").err().expect("nor loaded");
        assert_eq!(format!("{err:#}"), want);

        let spawn_log = tmp.0.join("spawns.log");
        let logging = sh_cmd(&r#"printf '%s\n' "$1" >> '__LOG__'; exit 1"#.replace("__LOG__", &spawn_log.display().to_string()));
        let cache_file = tmp.0.join("cache").join("clap-scan.json");
        let listed = list_clap_plugins_with(&[bundle.clone()], &logging, Some(&cache_file), Duration::from_secs(30), 1);
        assert!(listed.is_empty());
        assert!(!spawn_log.exists(), "the scanner was started for a bundle that can never load");
    }

    #[test]
    fn failed_bundle_is_not_rescanned_until_it_changes() {
        let tmp = TempDir::new("negcache");
        let bad = tmp.0.join("hang.clap");
        std::fs::write(&bad, b"v1").unwrap();
        let cache_file = tmp.0.join("cache").join("clap-scan.json");
        let spawn_log = tmp.0.join("spawns.log");
        let hang = sh_cmd(&r#"printf '%s\n' "$1" >> '__LOG__'; sleep 10"#.replace("__LOG__", &spawn_log.display().to_string()));
        // Only the hanging scanner needs the short timeout; the "fixed" one
        // gets a generous timeout so a loaded machine's shell spawn cannot
        // flake the third assertion (issue #64).
        let list = |cmd: &ScannerCommand, timeout: Duration| {
            let start = Instant::now();
            let out = list_clap_plugins_with(&[bad.clone()], cmd, Some(&cache_file), timeout, 1);
            (out, start.elapsed())
        };
        let spawns = || std::fs::read_to_string(&spawn_log).map_or(0, |s| s.lines().count());

        let (first, _) = list(&hang, Duration::from_millis(150));
        assert!(first.is_empty());
        assert_eq!(spawns(), 1);
        let (second, took) = list(&hang, Duration::from_millis(150));
        assert!(second.is_empty());
        assert_eq!(spawns(), 1, "a cached failure must not spawn the scanner again");
        assert!(took < Duration::from_millis(100), "cached failure still paid the timeout: {took:?}");

        std::fs::write(&bad, b"v2-fixed").unwrap();
        let fixed = sh_cmd(r#"printf '[{"id":"id.fixed","name":"F","vendor":"V","features":[]}]\n'"#);
        let (third, _) = list(&fixed, Duration::from_secs(30));
        assert_eq!(third.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["id.fixed"]);
    }

    #[test]
    fn cache_hit_does_not_spawn_and_invalidation_rescans_only_changed_bundle() {
        let tmp = TempDir::new("cache");
        let a = tmp.0.join("a.clap");
        let b = tmp.0.join("b.clap");
        std::fs::write(&a, b"bundle-a-v1").unwrap();
        std::fs::write(&b, b"bundle-b-v1").unwrap();
        let cache_file = tmp.0.join("cache").join("clap-scan.json");
        let spawn_log = tmp.0.join("spawns.log");

        let log_path = spawn_log.display().to_string();
        let scan_cmd = sh_cmd(
            &r#"printf '%s\n' "$1" >> '__LOG__'
            case "$1" in
                *a.clap) printf '[{"id":"id.a","name":"A1","vendor":"V","features":[]}]\n' ;;
                *b.clap) printf '[{"id":"id.b","name":"B1","vendor":"V","features":[]}]\n' ;;
            esac"#
            .replace("__LOG__", &log_path),
        );

        // 1. Cold cache scans both bundles and writes the cache file.
        let paths = vec![a.clone(), b.clone()];
        let first = list_clap_plugins_with(
            &paths,
            &scan_cmd,
            Some(&cache_file),
            Duration::from_secs(2),
            CLAP_SCAN_PARALLELISM,
        );
        assert_eq!(first.len(), 2);
        assert!(cache_file.exists());
        assert_eq!(std::fs::read_to_string(&spawn_log).unwrap().lines().count(), 2);

        // 2. Warm cache: even a `/bin/false` scanner succeeds with 0 spawns because nothing runs.
        let never_spawn = ScannerCommand::Custom {
            program: PathBuf::from("/bin/false"),
            args: Vec::new(),
        };
        let second = list_clap_plugins_with(
            &paths,
            &never_spawn,
            Some(&cache_file),
            Duration::from_secs(2),
            CLAP_SCAN_PARALLELISM,
        );
        assert_eq!(second, first);

        // 3. Modify only `b.clap` (different size): only `b.clap` re-scans.
        std::fs::write(&b, b"bundle-b-v2-longer").unwrap();
        std::fs::remove_file(&spawn_log).unwrap();
        let scan_v2 = sh_cmd(
            &r#"printf '%s\n' "$1" >> '__LOG__'
            case "$1" in
                *a.clap) exit 1 ;;
                *b.clap) printf '[{"id":"id.b","name":"B2","vendor":"V","features":[]}]\n' ;;
            esac"#
            .replace("__LOG__", &log_path),
        );
        let third = list_clap_plugins_with(
            &paths,
            &scan_v2,
            Some(&cache_file),
            Duration::from_secs(2),
            CLAP_SCAN_PARALLELISM,
        );
        assert_eq!(third.len(), 2);
        assert_eq!(third[0].name, "A1");
        assert_eq!(third[1].name, "B2");
        let spawned: Vec<String> = std::fs::read_to_string(&spawn_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(spawned, vec![b.display().to_string()]);
    }

    #[test]
    fn cfg_test_never_uses_user_cache_and_xdg_cache_home_is_respected() {
        assert!(clap_scan_cache_path().is_none());
        assert_eq!(
            clap_scan_cache_path_from(Some(OsStr::new("/tmp/xdg")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/tmp/xdg/kabelsalat/clap-scan.json"))
        );
        assert_eq!(
            clap_scan_cache_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.cache/kabelsalat/clap-scan.json"))
        );
    }

    #[test]
    fn corrupt_cache_falls_back_to_rescan() {
        let tmp = TempDir::new("corrupt-cache");
        let bundle = tmp.0.join("a.clap");
        std::fs::write(&bundle, b"fake").unwrap();
        let cache_file = tmp.0.join("clap-scan.json");
        std::fs::write(&cache_file, b"{corrupt json").unwrap();

        let cmd = sh_cmd(r#"printf '[{"id":"id.ok","name":"OK","vendor":"V","features":[]}]\n'"#);
        let plugins = list_clap_plugins_with(
            &[bundle],
            &cmd,
            Some(&cache_file),
            Duration::from_secs(2),
            1,
        );
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "id.ok");
    }

    #[test]
    fn bounded_parallelism_preserves_order_and_runs_concurrently() {
        let tmp = TempDir::new("parallel");
        let mut paths = Vec::new();
        for i in 0..6 {
            let p = tmp.0.join(format!("b{i}.clap"));
            std::fs::write(&p, b"fake").unwrap();
            paths.push(p);
        }

        let cmd = sh_cmd(
            r#"sleep 0.12
            name=$(basename "$1" .clap)
            printf '[{"id":"id.%s","name":"%s","vendor":"V","features":[]}]\n' "$name" "$name""#,
        );
        let start = Instant::now();
        let plugins = list_clap_plugins_with(
            &paths,
            &cmd,
            None,
            Duration::from_secs(3),
            CLAP_SCAN_PARALLELISM,
        );
        let elapsed = start.elapsed();

        let ids: Vec<&str> = plugins.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["id.b0", "id.b1", "id.b2", "id.b3", "id.b4", "id.b5"]);
        // 6 bundles × 120ms sequentially would take >=720ms; with parallelism 4 it finishes in 2 batches (~240ms).
        assert!(
            elapsed < Duration::from_millis(600),
            "expected concurrent batching under 600ms, took {elapsed:?}"
        );
    }
}