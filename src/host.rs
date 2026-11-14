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
//!   params, embedded GUI, deactivation (CLAP [main-thread] rules).
//!
//! `run_test` is a demo built entirely on this API: discover ~/.clap, enumerate
//! params, play an audible arpeggio through the default cpal output,
//! save/restore state, and EMBED the plugin GUI into a host-owned X11 window
//! (XWayland) — no detached plugin windows.
//!
//! Architecture (matches PLAN.md §7):
//! - main thread: plugin instance lifecycle, params/state/GUI (CLAP [main-thread])
//! - audio thread: owns `ClapVoice` (the `StartedPluginAudioProcessor`), renders
//!   blocks (CLAP [audio-thread])
//! - cpal callback: drains a lock-free `rtrb` ring into the device buffer

use std::cell::Cell;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window as ClapWindow,
};
use clack_extensions::latency::{HostLatency, HostLatencyImpl, PluginLatency};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::events::Match;
use clack_host::host::{HostError, HostExtensions};
use clack_host::prelude::*;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Frames per processing block. Small enough for live latency, big enough for Surge.
const BLOCK: usize = 256;
/// Upper bound on output ports (Surge XT: main + Scene A + Scene B = 3).
const MAX_OUT_PORTS: usize = 8;
/// How long the arpeggio + GUI demo runs.
const DEMO_SECONDS: u64 = 9;

// ---------------------------------------------------------------------------
// Host callback handlers (CLAP thread model: shared / main-thread / audio)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HostShared {
    // Plugin extensions, queried during initialization.
    params: OnceLock<Option<PluginParams>>,
    state: OnceLock<Option<PluginState>>,
    gui: OnceLock<Option<PluginGui>>,
    latency: OnceLock<Option<PluginLatency>>,
    audio_ports: OnceLock<Option<PluginAudioPorts>>,
    // Plugin -> host requests (lock-free flags).
    callback_requested: AtomicBool,
    restart_requested: AtomicBool,
    process_requested: AtomicBool,
    flush_requested: AtomicBool,
    // GUI requests. clack-extensions dispatches ALL HostGuiImpl callbacks to
    // the *shared* handler (they may arrive off the main thread), so this
    // state must be Send + Sync.
    resize_request: Mutex<Option<GuiSize>>,
    show_request: AtomicBool,
    hide_request: AtomicBool,
    gui_closed: AtomicBool,
}

impl<'a> SharedHandler<'a> for HostShared {
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.params.set(instance.get_extension());
        let _ = self.state.set(instance.get_extension());
        let _ = self.gui.set(instance.get_extension());
        let _ = self.latency.set(instance.get_extension());
        let _ = self.audio_ports.set(instance.get_extension());
    }
    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::SeqCst);
    }
    fn request_process(&self) {
        self.process_requested.store(true, Ordering::SeqCst);
    }
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
        *self.resize_request.lock().unwrap() = Some(new_size);
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        self.show_request.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        self.hide_request.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        self.gui_closed.store(true, Ordering::SeqCst);
    }
}

struct HostMainThread {
    state_dirty: Cell<bool>,
    latency_changed: Cell<bool>,
    params_rescan: Cell<bool>,
}

impl HostMainThread {
    fn new() -> Self {
        Self {
            state_dirty: Cell::new(false),
            latency_changed: Cell::new(false),
            params_rescan: Cell::new(false),
        }
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
            .register::<HostGui>();
    }
}

#[derive(Copy, Clone)]
struct Exts {
    params: Option<PluginParams>,
    state: Option<PluginState>,
    gui: Option<PluginGui>,
    latency: Option<PluginLatency>,
    audio_ports: Option<PluginAudioPorts>,
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

// Named fn (not a closure) so the lifetime elision ties input and output.
fn as_mut_slice_f32(b: &mut Vec<f32>) -> &mut [f32] {
    b.as_mut_slice()
}
macro_rules! out_port {
    ($p:ident) => {
        AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only($p.iter_mut().map(as_mut_slice_f32)),
        }
    };
}

/// A live Surge XT voice: the CLAP audio processor plus preallocated buffers.
///
/// `Send` but `!Sync`: create it on the main thread, then move it to the audio
/// thread. `render` is RT-safe (no allocation, no locks, no syscalls). Notes
/// arrive via a lock-free ring; `note_on`/`note_off` lock a mutex on the caller
/// side and must NOT be called from the render thread — use a
/// [`VoiceController`] from the UI thread instead.
pub struct ClapVoice {
    processor: StartedPluginAudioProcessor<TestHost>,
    cmds_tx: Arc<Mutex<rtrb::Producer<VoiceCmd>>>,
    cmds_rx: rtrb::Consumer<VoiceCmd>,
    /// Optional sample-accurate sequence (frame offset, command), e.g. the
    /// host-test arpeggio. Preallocated; consumed with a cursor.
    schedule: Vec<(u64, VoiceCmd)>,
    sched_idx: usize,
    frame: u64,
    sample_rate: f64,
    in_count: usize,
    main_out: usize,
    n_out: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: [Vec<Vec<f32>>; MAX_OUT_PORTS],
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    ev_in: EventBuffer,
    ev_out: EventBuffer,
    process_errors: u64,
}

impl ClapVoice {
    /// Load Surge XT from `~/.clap`, instantiate, activate and start processing
    /// at `sample_rate`. Returns the audio-side voice and the main-thread
    /// handle (state/GUI/deactivation). Call on the main thread.
    pub fn new(sample_rate: f64) -> Result<(ClapVoice, ClapVoiceMain)> {
        let (entry, plugin_id) = discover_surge()?;
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
        .context("instantiating Surge XT")?;
        println!("instantiated: {}", plugin_id.to_string_lossy());

        let exts = plugin.access_shared_handler(|h| Exts {
            params: h.params.get().copied().flatten(),
            state: h.state.get().copied().flatten(),
            gui: h.gui.get().copied().flatten(),
            latency: h.latency.get().copied().flatten(),
            audio_ports: h.audio_ports.get().copied().flatten(),
        });
        println!(
            "extensions: params={} state={} gui={} latency={} audio-ports={}",
            exts.params.is_some(),
            exts.state.is_some(),
            exts.gui.is_some(),
            exts.latency.is_some(),
            exts.audio_ports.is_some()
        );

        let (in_count, in_ch, out_chs, main_out) =
            inspect_audio_ports(&mut plugin, exts.audio_ports)?;

        let audio_config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: 1,
            max_frames_count: BLOCK as u32,
        };
        let stopped = plugin
            .activate(|_, _| (), audio_config)
            .context("activating Surge XT")?;
        // Surge only reports meaningful latency once active.
        let latency = exts.latency.map(|l| l.get(&plugin.plugin_handle())).unwrap_or(0);
        println!("plugin-reported latency (active): {latency} samples");
        let processor = stopped
            .start_processing()
            .map_err(|_| anyhow!("start_processing failed"))?;

        let (cmds_tx, cmds_rx) = rtrb::RingBuffer::<VoiceCmd>::new(CMD_RING);
        let n_out = out_chs.len();
        let voice = ClapVoice {
            processor,
            cmds_tx: Arc::new(Mutex::new(cmds_tx)),
            cmds_rx,
            schedule: Vec::new(),
            sched_idx: 0,
            frame: 0,
            sample_rate,
            in_count,
            main_out,
            n_out,
            in_bufs: (0..in_ch.max(1)).map(|_| vec![0.0; BLOCK]).collect(),
            out_bufs: std::array::from_fn(|i| {
                if i < n_out {
                    (0..out_chs[i]).map(|_| vec![0.0; BLOCK]).collect()
                } else {
                    vec![vec![0.0; BLOCK]]
                }
            }),
            in_ports: AudioPorts::with_capacity(in_ch.max(1), 1),
            out_ports: AudioPorts::with_capacity(out_chs.iter().sum(), n_out),
            ev_in: EventBuffer::with_capacity(EVENT_CAP as usize),
            ev_out: EventBuffer::with_capacity(EVENT_CAP as usize),
            process_errors: 0,
        };
        let main = ClapVoiceMain {
            instance: plugin,
            exts,
            _entry: entry,
        };
        Ok((voice, main))
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

    /// Render `out.len() / 2` stereo-interleaved frames. RT-safe: no
    /// allocation, no locks, no syscalls. Call from the audio thread only.
    pub fn render(&mut self, out: &mut [f32]) {
        let frames = out.len() / 2;
        let mut done = 0usize;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            self.render_block(n, &mut out[done * 2..(done + n) * 2]);
            done += n;
        }
    }

    fn render_block(&mut self, n: usize, out: &mut [f32]) {
        self.ev_in.clear();
        self.ev_out.clear();

        // Live commands (block-quantized) + scheduled events (sample-accurate).
        while let Ok(cmd) = self.cmds_rx.pop() {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            push_cmd(&mut self.ev_in, 0, cmd);
        }
        while self.sched_idx < self.schedule.len()
            && self.schedule[self.sched_idx].0 < self.frame + n as u64
        {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            let (t, cmd) = self.schedule[self.sched_idx];
            push_cmd(&mut self.ev_in, (t - self.frame) as u32, cmd);
            self.sched_idx += 1;
        }

        let in_port = AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                self.in_bufs.iter_mut().map(|b| InputChannel::constant(b.as_mut_slice())),
            ),
        };
        let mut input = if self.in_count == 1 {
            self.in_ports.with_input_buffers([in_port])
        } else {
            // Zero input ports: same concrete type, empty iterator.
            self.in_ports.with_input_buffers([in_port].into_iter().take(0))
        };
        // Stack array of port configs (no heap alloc); only the first n_out
        // entries are handed to the plugin. Destructure into disjoint locals
        // so the borrow checker accepts simultaneous per-port borrows.
        let [p0, p1, p2, p3, p4, p5, p6, p7] = &mut self.out_bufs;
        let out_arr = [
            out_port!(p0),
            out_port!(p1),
            out_port!(p2),
            out_port!(p3),
            out_port!(p4),
            out_port!(p5),
            out_port!(p6),
            out_port!(p7),
        ];
        let mut output = self.out_ports.with_output_buffers(out_arr.into_iter().take(self.n_out));
        // Buffers are BLOCK-sized; process exactly n frames.
        input.truncate(n as u32);
        output.truncate(n as u32);

        let status = self.processor.process(
            &input,
            &mut output,
            &self.ev_in.as_input(),
            &mut self.ev_out.as_output(),
            Some(self.frame),
            None,
        );
        if status.is_err() {
            self.process_errors += 1;
        }

        // Mix the MAIN output port (stereo) into `out`; Scene A/B are rendered
        // but unused. On process failure output silence.
        let failed = status.is_err();
        for i in 0..n {
            out[i * 2] = if failed { 0.0 } else { self.out_bufs[self.main_out][0][i] };
            out[i * 2 + 1] = if failed { 0.0 } else { self.out_bufs[self.main_out][1][i] };
        }
        self.frame += n as u64;
    }

    /// Stop processing and hand the processor back for main-thread
    /// deactivation (`ClapVoiceMain::deactivate`). Call on the audio thread.
    pub fn stop(self) -> StoppedPluginAudioProcessor<TestHost> {
        self.processor.stop_processing()
    }
}

fn push_cmd(ev_in: &mut EventBuffer, time: u32, cmd: VoiceCmd) {
    match cmd {
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
    }
}

/// Main-thread side of a [`ClapVoice`]: the CLAP plugin instance (which is
/// `!Send`), state save/restore, GUI access and deactivation. Must stay on the
/// thread that called `ClapVoice::new`.
pub struct ClapVoiceMain {
    instance: PluginInstance<TestHost>,
    exts: Exts,
    _entry: PluginEntry,
}

impl ClapVoiceMain {
    /// Direct access for params inspection / embedded GUI (main thread only).
    pub fn instance_mut(&mut self) -> &mut PluginInstance<TestHost> {
        &mut self.instance
    }

    /// The plugin's GUI extension, if any (for embedded-editor setup).
    pub fn gui(&self) -> Option<PluginGui> {
        self.exts.gui
    }

    /// Plugin-reported latency in samples (meaningful once active).
    pub fn latency(&mut self) -> u32 {
        self.exts.latency.map(|l| l.get(&self.instance.plugin_handle())).unwrap_or(0)
    }

    /// Serialize the full plugin state (Surge patch XML in a `sub3` container).
    /// Allocates — main thread only.
    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        let state = self.exts.state.ok_or_else(|| anyhow!("Surge XT has no state extension"))?;
        let mut buf = Vec::new();
        state.save(&mut self.instance.plugin_handle(), &mut buf).context("saving plugin state")?;
        Ok(buf)
    }

    /// Restore plugin state. Surge applies it on the audio thread: keep
    /// rendering (or wait a few blocks) before reading parameter values back.
    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let state = self.exts.state.ok_or_else(|| anyhow!("Surge XT has no state extension"))?;
        state.load(&mut self.instance.plugin_handle(), &mut &data[..]).context("loading plugin state")?;
        Ok(())
    }

    /// Deactivate the plugin after the audio thread returned the processor via
    /// `ClapVoice::stop`. Consumes both sides (CLAP lifecycle rule).
    pub fn deactivate(mut self, stopped: StoppedPluginAudioProcessor<TestHost>) {
        self.instance.deactivate(stopped);
    }
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
    let (tx, rx) = mpsc::sync_channel::<StoppedPluginAudioProcessor<TestHost>>(1);
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
    println!("playing arpeggio (A3 C4 E4 A4 C5 ...) for {DEMO_SECONDS}s — GUI embedded below");

    // 8. Embedded GUI (X11/XWayland). No detached-window fallback by design.
    ensure_display();
    let gui_ext = main.gui();
    let gui_report = run_embedded_gui(main.instance_mut(), gui_ext, Duration::from_secs(DEMO_SECONDS));
    println!("GUI: {gui_report}");

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
        main.instance_mut().call_on_main_thread_callback();
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
    let stopped = rx
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
    main.deactivate(stopped);
    println!("=== host-test PASS: Surge XT loads, plays, state round-trips ===");
    println!("GUI result: {gui_report}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn discover_surge() -> Result<(PluginEntry, CString)> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".clap");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {} (install Surge XT.clap there)", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "clap").unwrap_or(false))
        .collect();
    paths.sort();
    if paths.is_empty() {
        bail!("no .clap bundles found in {}", dir.display());
    }

    let mut chosen: Option<(PluginEntry, CString)> = None;
    for path in &paths {
        // SAFETY: loading a CLAP bundle executes its init code; we only load
        // bundles from the user's own ~/.clap directory.
        let entry = match unsafe { PluginEntry::load(path) } {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skip {}: {e}", path.display());
                continue;
            }
        };
        let Some(factory) = entry.get_plugin_factory() else {
            eprintln!("skip {}: no plugin factory", path.display());
            continue;
        };
        println!("bundle: {} (CLAP {})", path.display(), entry.version());
        let mut found: Option<CString> = None;
        for d in factory.plugin_descriptors() {
            let id = d.id().unwrap_or(c"?");
            let name = d.name().unwrap_or(c"?").to_string_lossy().into_owned();
            let vendor = d.vendor().unwrap_or(c"?").to_string_lossy().into_owned();
            let version = d.version().unwrap_or(c"?").to_string_lossy().into_owned();
            let features: Vec<String> = d
                .features()
                .map(|f| f.to_string_lossy().into_owned())
                .collect();
            println!(
                "  - {name} [{}] v{version} by {vendor} | {}",
                id.to_string_lossy(),
                features.join(", ")
            );
            if name == "Surge XT" && features.iter().any(|f| f == "instrument") {
                found = Some(id.to_owned());
            }
        }
        if let Some(id) = found {
            if chosen.is_none() {
                chosen = Some((entry, id));
            }
        }
    }
    chosen.ok_or_else(|| anyhow!("Surge XT (instrument) not found in {}", dir.display()))
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
) -> Result<(usize, usize, Vec<usize>, usize)> {
    let Some(ports) = ports else {
        println!("audio-ports: extension not supported; assuming stereo out, no in");
        return Ok((0, 0, vec![2], 0));
    };
    let mut in_count = 0usize;
    let mut in_ch = 0usize;
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
                    in_count += 1;
                    in_ch = info.channel_count as usize;
                } else {
                    if is_main {
                        main_out = Some(out_chs.len());
                    }
                    out_chs.push(info.channel_count as usize);
                }
            }
        }
    }
    if in_count > 1 {
        bail!("P0 host supports at most 1 input port, Surge reports {in_count}");
    }
    let main_out = main_out.ok_or_else(|| anyhow!("Surge exposes no main output port"))?;
    if out_chs[main_out] != 2 {
        bail!("P0 host expects a stereo main output, got {} channels", out_chs[main_out]);
    }
    if out_chs.len() > MAX_OUT_PORTS {
        bail!("P0 host supports at most {MAX_OUT_PORTS} output ports, got {}", out_chs.len());
    }
    Ok((in_count, in_ch, out_chs, main_out))
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
    tx: mpsc::SyncSender<StoppedPluginAudioProcessor<TestHost>>,
) -> Result<u64> {
    let mut block = [0.0f32; BLOCK * 2];
    while !stop.load(Ordering::Relaxed) {
        voice.render(&mut block);
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
    let stopped = voice.stop();
    let _ = tx.send(stopped);
    Ok(errors)
}

// ---------------------------------------------------------------------------
// Embedded GUI (X11 / XWayland) — host-owned parent window, no floating fallback
// ---------------------------------------------------------------------------

/// The tool shells may lack DISPLAY even though XWayland is running; find the
/// socket and point DISPLAY at it (no-op when already set).
fn ensure_display() {
    if std::env::var_os("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false) {
        return;
    }
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name.strip_prefix('X') {
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    std::env::set_var("DISPLAY", format!(":{num}"));
                    println!("DISPLAY was unset; using :{num} (XWayland socket)");
                    return;
                }
            }
        }
    }
}

struct ParentWindow {
    conn: RustConnection,
    win: u32,
}

impl ParentWindow {
    fn new(title: &str, width: u32, height: u32) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connecting to X server (DISPLAY)")?;
        let screen = &conn.setup().roots[screen_num];
        let win = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            40,
            40,
            width.min(u16::MAX as u32) as u16,
            height.min(u16::MAX as u32) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(0x0020_2020)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::SUBSTRUCTURE_NOTIFY, // observe the plugin reparenting into us
                ),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;
        conn.map_window(win)?;
        conn.flush()?;
        Ok(Self { conn, win })
    }

    fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new()
                .width(width.min(u16::MAX as u32))
                .height(height.min(u16::MAX as u32)),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Drain pending X events; flags reparent/create of children into us.
    fn pump(&self, reparented: &mut bool, created: &mut bool) -> Result<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::ReparentNotify(ev) => {
                    if ev.parent == self.win {
                        *reparented = true;
                        println!(
                            "X11: plugin window {} reparented into host window {} (embedding confirmed)",
                            ev.window, self.win
                        );
                    }
                }
                x11rb::protocol::Event::CreateNotify(ev) => {
                    if ev.parent == self.win {
                        *created = true;
                        println!(
                            "X11: plugin window {} created directly inside host window {}",
                            ev.window, self.win
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Enumerate child windows with their WM_CLASS — hard evidence that the
    /// plugin editor lives inside our host window.
    fn describe_children(&self) -> Result<Vec<(u32, String)>> {
        let tree = self.conn.query_tree(self.win)?.reply()?;
        let mut out = Vec::new();
        for &child in &tree.children {
            let reply = self
                .conn
                .get_property(false, child, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?
                .reply()?;
            let class = String::from_utf8_lossy(&reply.value).replace('\0', " ");
            out.push((child, class.trim().to_string()));
        }
        Ok(out)
    }
}

impl Drop for ParentWindow {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

fn run_embedded_gui(
    plugin: &mut PluginInstance<TestHost>,
    gui: Option<PluginGui>,
    duration: Duration,
) -> String {
    let Some(gui) = gui else {
        return "FAILED: plugin exposes no GUI extension".into();
    };
    match embed_gui(plugin, &gui, duration) {
        Ok(summary) => summary,
        Err(e) => format!("FAILED: {e:#}"),
    }
}

fn embed_gui(
    plugin: &mut PluginInstance<TestHost>,
    gui: &PluginGui,
    duration: Duration,
) -> Result<String> {
    let cfg = GuiConfiguration {
        api_type: GuiApiType::X11,
        is_floating: false, // embedded, per requirement — never floating
    };
    if let Some(pref) = gui.get_preferred_api(&plugin.plugin_handle()) {
        println!(
            "GUI preferred api: {} (floating={})",
            pref.api_type.0.to_string_lossy(),
            pref.is_floating
        );
    }
    if !gui.is_api_supported(&plugin.plugin_handle(), cfg) {
        bail!("step=is_api_supported: Surge rejected x11 embedded configuration");
    }
    gui.create(&plugin.plugin_handle(), cfg)
        .context("step=create(x11, embedded)")?;

    // From here on the GUI exists; make sure we destroy it on every exit path.
    // The host parent window must outlive gui.destroy(): destroying the X11
    // parent first would tear down the plugin's reparented child window.
    let size = gui
        .get_size(&plugin.plugin_handle())
        .unwrap_or(GuiSize { width: 900, height: 600 });
    println!("GUI initial size: {}x{}", size.width, size.height);
    let parent = ParentWindow::new("kabelsalat P0 — Surge XT (embedded)", size.width, size.height)
        .context("step=create host X11 parent window");
    let result = match &parent {
        Ok(parent) => embed_gui_inner(plugin, gui, duration, parent),
        Err(e) => Err(anyhow!("{e:#}")),
    };
    let _ = gui.hide(&plugin.plugin_handle());
    gui.destroy(&plugin.plugin_handle());
    drop(parent); // destroys the X11 window only after the plugin GUI is gone
    result
}

fn embed_gui_inner(
    plugin: &mut PluginInstance<TestHost>,
    gui: &PluginGui,
    duration: Duration,
    parent: &ParentWindow,
) -> Result<String> {
    let can_resize = gui.can_resize(&plugin.plugin_handle());
    println!("GUI can_resize: {can_resize}");

    // CLAP X11 embedding: hand the plugin our window handle; the plugin
    // reparents its editor into it (XReparentWindow on the plugin side).
    // SAFETY: `parent` outlives the GUI (destroyed before ParentWindow::drop).
    unsafe {
        gui.set_parent(&plugin.plugin_handle(), ClapWindow::from_x11_handle(parent.win as _))
            .context("step=set_parent(x11)")?;
    }
    gui.show(&plugin.plugin_handle()).context("step=show")?;
    println!("GUI embedded; running event loop for {}s", duration.as_secs());

    let deadline = Instant::now() + duration;
    let mut reparented = false;
    let mut created = false;
    let mut resizes = 0u32;
    let mut children_seen = false;
    let mut next_inspect = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        parent.pump(&mut reparented, &mut created)?;
        if !children_seen && Instant::now() >= next_inspect {
            let children = parent.describe_children()?;
            if !children.is_empty() {
                children_seen = true;
                println!("X11: host window {} children: {children:?}", parent.win);
            }
            next_inspect = Instant::now() + Duration::from_secs(1);
        }

        // Plugin-initiated resize: adjust the host window, then confirm.
        if let Some(req) = plugin.access_shared_handler(|h| h.resize_request.lock().unwrap().take()) {
            let adjusted = gui.adjust_size(&plugin.plugin_handle(), req).unwrap_or(req);
            parent.resize(adjusted.width, adjusted.height)?;
            let _ = gui.set_size(&plugin.plugin_handle(), adjusted);
            resizes += 1;
        }
        if plugin.access_shared_handler(|h| h.show_request.swap(false, Ordering::SeqCst)) {
            let _ = gui.show(&plugin.plugin_handle());
        }
        if plugin.access_shared_handler(|h| h.hide_request.swap(false, Ordering::SeqCst)) {
            let _ = gui.hide(&plugin.plugin_handle());
        }
        if plugin.access_shared_handler(|h| h.gui_closed.swap(false, Ordering::SeqCst)) {
            bail!("step=event-loop: plugin reported GUI closed unexpectedly");
        }
        if plugin.access_shared_handler(|h| h.callback_requested.swap(false, Ordering::SeqCst)) {
            plugin.call_on_main_thread_callback();
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let final_size = gui.get_size(&plugin.plugin_handle());
    let evidence = if reparented || created || children_seen {
        "child window confirmed in host window"
    } else {
        "WARNING: no child window observed"
    };
    Ok(format!(
        "EMBEDDED OK (x11/XWayland, {evidence}; reparented={reparented}, created={created}, resize requests={resizes}, final size={final_size:?})"
    ))
}