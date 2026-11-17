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
//! Architecture (matches docs/ARCHITECTURE.md §7):
//! - main thread: plugin instance lifecycle, params/state/GUI (CLAP [main-thread])
//! - audio thread: owns `ClapVoice` (the `StartedPluginAudioProcessor`), renders
//!   blocks (CLAP [audio-thread])
//! - cpal callback: drains a lock-free `rtrb` ring into the device buffer

use std::cell::Cell;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
use crate::transport::{Transport, BEATS_PER_BAR};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateWindowAux,
    EventMask, PropMode, WindowClass,
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
    remote_controls: OnceLock<Option<PluginRemoteControls>>,
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
        let _ = self.remote_controls.set(instance.get_extension());
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
    remote_controls: Option<PluginRemoteControls>,
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
    voice_id: u64,
    plugin_id: String,
    processor: StartedPluginAudioProcessor<TestHost>,
    cmds_tx: Arc<Mutex<rtrb::Producer<VoiceCmd>>>,
    cmds_rx: rtrb::Consumer<VoiceCmd>,
    /// Preallocated audio-thread command queue (avoids Mutex lock in `GraphEngine`).
    rt_cmds: Vec<VoiceCmd>,
    /// Sorted `(id, min, max)` table populated once at instantiation for lock-free CC scaling.
    param_ranges: Vec<(u32, f64, f64)>,
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

    fn instantiate_entry(
        entry: PluginEntry,
        plugin_id: CString,
        sample_rate: f64,
    ) -> Result<(ClapVoice, ClapVoiceMain)> {
        static NEXT_VOICE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id_str = plugin_id.to_string_lossy().into_owned();
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
            gui: h.gui.get().copied().flatten(),
            latency: h.latency.get().copied().flatten(),
            audio_ports: h.audio_ports.get().copied().flatten(),
            remote_controls: h.remote_controls.get().copied().flatten(),
        });
        println!(
            "extensions: params={} state={} gui={} latency={} audio-ports={} remote-controls={}",
            exts.params.is_some(),
            exts.state.is_some(),
            exts.gui.is_some(),
            exts.latency.is_some(),
            exts.audio_ports.is_some(),
            exts.remote_controls.is_some()
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
        let processor = stopped
            .start_processing()
            .map_err(|_| anyhow!("start_processing failed"))?;

        let (cmds_tx, cmds_rx) = rtrb::RingBuffer::<VoiceCmd>::new(CMD_RING);
        let n_out = out_chs.len();
        let voice = ClapVoice {
            voice_id: NEXT_VOICE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            plugin_id: id_str,
            processor,
            cmds_tx: Arc::new(Mutex::new(cmds_tx)),
            cmds_rx,
            rt_cmds: Vec::with_capacity(EVENT_CAP as usize),
            param_ranges,
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
        let mut main = ClapVoiceMain {
            instance: plugin,
            exts,
            main_controls: MainControls::default(),
            main_controls_rechecks: 0,
            _entry: entry,
        };
        main.refresh_main_controls();
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
        self.param_ranges
            .binary_search_by_key(&id, |&(pid, _, _)| pid)
            .ok()
            .map(|idx| (self.param_ranges[idx].1, self.param_ranges[idx].2))
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
            self.render_block(n, &mut out[done * 2..(done + n) * 2], &transport);
            transport.advance(n, self.sample_rate);
            done += n;
        }
    }

    fn render_block(&mut self, n: usize, out: &mut [f32], transport: &Transport) {
        self.ev_in.clear();
        self.ev_out.clear();

        // Live commands (block-quantized) + scheduled events (sample-accurate).
        for i in 0..self.rt_cmds.len() {
            if self.ev_in.len() >= EVENT_CAP {
                break;
            }
            push_cmd(&mut self.ev_in, 0, self.rt_cmds[i]);
        }
        self.rt_cmds.clear();
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
            Some(&transport_event(transport)),
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

/// The CLAP view of the host transport: tempo, 4/4, and both timelines.
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
        time_signature_numerator: BEATS_PER_BAR,
        time_signature_denominator: 4,
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
        VoiceCmd::Midi(data) => ev_in.push(&MidiEvent::new(time, 0, data)),
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
}

/// Main-control polls that look again after a state load (about a second
/// at the UI's poll rate): the state lands on the audio thread a few blocks
/// later.
const MAIN_CONTROLS_RECHECKS: u8 = 10;

/// Main-thread side of a [`ClapVoice`]: the CLAP plugin instance (which is
/// `!Send`), state save/restore, GUI access and deactivation. Must stay on the
/// thread that called `ClapVoice::new`.
pub struct ClapVoiceMain {
    instance: PluginInstance<TestHost>,
    exts: Exts,
    /// Resolved when the instance is created, and again only when the plugin
    /// reports changed parameter info. Never per frame: it walks every param.
    main_controls: MainControls,
    /// Polls left that re-read the main controls after a state load.
    main_controls_rechecks: u8,
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
        self.main_controls_rechecks = MAIN_CONTROLS_RECHECKS;
        Ok(())
    }

    /// Deactivate the plugin after the audio thread returned the processor via
    /// `ClapVoice::stop`. Consumes both sides (CLAP lifecycle rule).
    pub fn deactivate(mut self, stopped: StoppedPluginAudioProcessor<TestHost>) {
        self.instance.deactivate(stopped);
    }

    /// Stop `voice` and deactivate both halves (main thread only).
    pub fn retire(self, voice: ClapVoice) {
        let stopped = voice.stop();
        self.deactivate(stopped);
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

fn load_bundle(path: &std::path::Path, id: &str) -> Result<(PluginEntry, CString)> {
    // SAFETY: loading a CLAP bundle executes its init code. Only the bundle
    // the out-of-process scan found the plugin in is loaded here.
    let entry = unsafe { PluginEntry::load(path) }
        .with_context(|| format!("failed to load CLAP bundle {}", path.display()))?;
    println!("bundle: {} (CLAP {})", path.display(), entry.version());
    Ok((entry, CString::new(id)?))
}

fn discover_plugin(target_id: &str) -> Result<(PluginEntry, CString)> {
    let (path, id) = bundle_with(|p| p.id == target_id)
        .ok_or_else(|| anyhow!("CLAP plugin '{target_id}' not found in CLAP search paths"))?;
    load_bundle(&path, &id)
}

/// Names of the CLAP preset-discovery providers the bundle of `plugin_id`
/// offers (`clap.preset-discovery-factory`); empty when it has no such
/// factory. Loads the bundle: `sound-library-test` reports it per plugin.
pub fn clap_preset_providers(plugin_id: &str) -> Result<Vec<String>> {
    use clack_extensions::preset_discovery::factory::PresetDiscoveryFactory;
    let (path, _) = bundle_with(|p| p.id == plugin_id)
        .ok_or_else(|| anyhow!("CLAP plugin '{plugin_id}' not found in CLAP search paths"))?;
    // SAFETY: as in `load_bundle`: the scan found the plugin in this bundle.
    let entry = unsafe { PluginEntry::load(&path) }.with_context(|| format!("failed to load CLAP bundle {}", path.display()))?;
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
    let mut measure = |voice: &mut ClapVoice| -> (f32, f32) {
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
    // SAFETY: loading a CLAP bundle executes its init code; production listing
    // runs this inside the isolated `scan-clap` child process.
    let entry = unsafe { PluginEntry::load(path) }
        .with_context(|| format!("failed to load CLAP bundle {}", path.display()))?;
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

// ---------------------------------------------------------------------------
// EmbeddedEditor — plugin GUI inside the app's MAIN window (P1, task-15)
// ---------------------------------------------------------------------------
//
// Same CLAP sequence as the P0 spike (create → set_parent → show → resize
// pump), but the parent is the egui main window itself (X11 id obtained from
// eframe via raw-window-handle), not a host-owned top-level. The editor is an
// X11 CHILD of the main window, positioned by the UI into a reserved region
// (physical pixels). One dedicated x11rb connection per editor discovers and
// configures the child; CLAP gui requests are drained from HostShared.
//
// The parent window is NOT owned here — never destroy it. Teardown order is
// gui.hide() → gui.destroy() (the plugin destroys its child itself); the X
// connection may go away at any time afterwards.

/// A rectangle in the main window's X11 coordinate space (physical pixels).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EditorRect {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

/// Result of [`EmbeddedEditor::pump`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditorStatus {
    Ok,
    /// The plugin closed/destroyed its own GUI; the UI should drop the editor.
    Closed,
}

pub struct EmbeddedEditor {
    conn: RustConnection,
    /// Main app window (borrowed, NOT owned).
    parent: u32,
    gui: PluginGui,
    /// The plugin's editor window inside `parent`, once discovered.
    child: Option<u32>,
    /// Main-window children present before set_parent (discovery baseline).
    baseline: Vec<u32>,
    /// Plugin-side editor size (what Surge draws, physical px).
    size: GuiSize,
    /// Last region requested by the UI + when it last changed (debounce).
    last_rect: Option<EditorRect>,
    rect_changed: Instant,
    /// Last geometry actually applied to the child (avoid configure spam).
    last_geom: Option<(i16, i16, u16, u16)>,
    mapped: bool,
    /// Plugin reported closed/destroyed — skip hide/destroy on teardown.
    gui_gone: bool,
    next_evidence: Instant,
}

impl EmbeddedEditor {
    /// Create the plugin GUI and embed it into `parent` (the main window's
    /// X11 id), fitted to `rect`. Call on the main thread. On success the
    /// editor is shown and (usually) already positioned; [`Self::pump`] keeps
    /// it in sync afterwards.
    pub fn open(main: &mut ClapVoiceMain, parent: u32, rect: EditorRect) -> Result<Self> {
        ensure_display();
        let gui = main
            .gui()
            .ok_or_else(|| anyhow!("plugin exposes no GUI extension"))?;
        let cfg = GuiConfiguration {
            api_type: GuiApiType::X11,
            is_floating: false, // embedded into the main window — never floating
        };
        if !gui.is_api_supported(&main.instance_mut().plugin_handle(), cfg) {
            bail!("Surge rejected x11 embedded GUI configuration");
        }
        gui.create(&main.instance_mut().plugin_handle(), cfg)
            .context("step=create(x11, embedded)")?;
        // GUI exists from here on: destroy it on every error path.
        let result = Self::open_inner(main, gui, parent, rect);
        if result.is_err() {
            let _ = gui.hide(&main.instance_mut().plugin_handle());
            gui.destroy(&main.instance_mut().plugin_handle());
        }
        result
    }

    fn open_inner(
        main: &mut ClapVoiceMain,
        gui: PluginGui,
        parent: u32,
        rect: EditorRect,
    ) -> Result<Self> {
        let (conn, _screen) = x11rb::connect(None).context("connecting to X server (DISPLAY)")?;
        let baseline = conn
            .query_tree(parent)
            .context("query_tree(main window)")?
            .reply()
            .context("query_tree reply")?
            .children;

        // CLAP X11 embedding: hand Surge the MAIN window's id. Surge creates
        // its editor directly as a child of it (P0: CreateNotify, no reparent).
        // SAFETY: the main window outlives the GUI (eframe owns it; the editor
        // is destroyed on toggle-off / app exit before the window goes away).
        unsafe {
            gui.set_parent(
                &main.instance_mut().plugin_handle(),
                ClapWindow::from_x11_handle(parent as _),
            )
            .context("step=set_parent(main window)")?;
        }
        gui.show(&main.instance_mut().plugin_handle())
            .context("step=show")?;

        let mut ed = Self {
            conn,
            parent,
            gui,
            child: None,
            baseline,
            size: GuiSize { width: 900, height: 600 },
            last_rect: None,
            rect_changed: Instant::now(),
            last_geom: None,
            mapped: true,
            gui_gone: false,
            next_evidence: Instant::now(),
        };

        // Discover the child synchronously (Surge creates it during
        // set_parent/show) so it can be positioned before the first frame.
        let deadline = Instant::now() + Duration::from_millis(600);
        while ed.child.is_none() && Instant::now() < deadline {
            ed.discover_child();
            if ed.child.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if ed.child.is_none() {
            eprintln!("editor: no child window yet after 600 ms — will keep looking in pump()");
        }
        ed.fit_and_place(main, rect, true);
        ed.log_evidence("open");
        Ok(ed)
    }

    /// The plugin's editor X11 window, once discovered (for UI status).
    pub fn child(&self) -> Option<u32> {
        self.child
    }

    /// Per-frame maintenance: drain X events + CLAP gui requests, keep the
    /// child fitted to `rect` (physical px in the main window). Main thread.
    pub fn pump(&mut self, main: &mut ClapVoiceMain, rect: EditorRect) -> EditorStatus {
        if self.gui_gone {
            return EditorStatus::Closed;
        }
        // 1. X events on our dedicated connection (child StructureNotify).
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => match event {
                    x11rb::protocol::Event::CreateNotify(ev) if ev.parent == self.parent => {
                        if self.child.is_none() {
                            self.adopt_child(ev.window);
                        }
                    }
                    x11rb::protocol::Event::ReparentNotify(ev) if ev.parent == self.parent => {
                        if self.child.is_none() {
                            self.adopt_child(ev.window);
                        }
                    }
                    x11rb::protocol::Event::DestroyNotify(ev) if Some(ev.window) == self.child => {
                        eprintln!("editor: child window {} destroyed by plugin", ev.window);
                        self.child = None;
                        self.gui_gone = true;
                        return EditorStatus::Closed;
                    }
                    _ => {}
                },
                Ok(None) => break,
                Err(e) => {
                    eprintln!("editor: X event poll failed: {e}");
                    break;
                }
            }
        }
        // 2. Child not seen yet? Poll the tree (covers missed events).
        if self.child.is_none() {
            self.discover_child();
        }
        // 3. CLAP gui requests (dispatched to the shared handler by clack).
        let req = main
            .instance_mut()
            .access_shared_handler(|h| h.resize_request.lock().unwrap().take());
        if let Some(req) = req {
            let target = self.adjusted_fit(main, clamp_size(req, rect), rect);
            let _ = self.gui.set_size(&main.instance_mut().plugin_handle(), target);
            self.size = self
                .gui
                .get_size(&main.instance_mut().plugin_handle())
                .unwrap_or(target);
            self.last_geom = None; // force reconfigure
            eprintln!("editor: request_resize {req:?} -> {target:?}");
        }
        if main.instance_mut().access_shared_handler(|h| h.show_request.swap(false, Ordering::SeqCst)) {
            let _ = self.gui.show(&main.instance_mut().plugin_handle());
            self.set_mapped(true);
        }
        if main.instance_mut().access_shared_handler(|h| h.hide_request.swap(false, Ordering::SeqCst)) {
            let _ = self.gui.hide(&main.instance_mut().plugin_handle());
            self.set_mapped(false);
        }
        if main.instance_mut().access_shared_handler(|h| h.gui_closed.swap(false, Ordering::SeqCst)) {
            self.gui_gone = true;
            return EditorStatus::Closed;
        }
        if main.instance_mut().access_shared_handler(|h| h.callback_requested.swap(false, Ordering::SeqCst)) {
            main.instance_mut().call_on_main_thread_callback();
        }
        // 4. Keep the child fitted + centered in the reserved region.
        self.fit_and_place(main, rect, false);
        // 5. Periodic hard evidence: the main window's child list.
        if Instant::now() >= self.next_evidence {
            self.log_evidence("pump");
            self.next_evidence = Instant::now() + Duration::from_secs(5);
        }
        EditorStatus::Ok
    }

    /// Hide + destroy the plugin GUI (the plugin destroys its X11 child).
    /// Consumes the editor; the X connection is dropped afterwards.
    pub fn close(mut self, main: &mut ClapVoiceMain) {
        if !self.gui_gone {
            let _ = self.gui.hide(&main.instance_mut().plugin_handle());
            self.gui.destroy(&main.instance_mut().plugin_handle());
        }
        self.child = None;
        eprintln!("editor: closed (gui destroyed, main window {} untouched)", self.parent);
    }

    // -- internals ----------------------------------------------------------

    /// Find a new child of the main window (vs. the pre-embed baseline) and
    /// adopt it as the editor. Prefer Surge's WM_CLASS; else the largest new
    /// window. Selects StructureNotify on the child to observe its lifecycle.
    fn discover_child(&mut self) {
        let Ok(reply) = self.conn.query_tree(self.parent).map(|c| c.reply()) else {
            return;
        };
        let Ok(tree) = reply else { return };
        let mut best: Option<(u32, u32)> = None; // (window, area)
        for &child in &tree.children {
            if self.baseline.contains(&child) {
                continue;
            }
            let class = self.window_class(child);
            let area = self
                .conn
                .get_geometry(child)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|g| g.width as u32 * g.height as u32)
                .unwrap_or(0);
            if class.to_lowercase().contains("surge") {
                self.adopt_child(child);
                return;
            }
            if best.map_or(true, |(_, a)| area > a) {
                best = Some((child, area));
            }
        }
        if let Some((child, _)) = best {
            self.adopt_child(child);
        }
    }

    fn adopt_child(&mut self, child: u32) {
        self.child = Some(child);
        self.baseline.push(child); // never re-adopt
        // Observe the child's own lifecycle (destroy/configure). This does not
        // touch the main window's event mask (winit owns that).
        let _ = self.conn.change_window_attributes(
            child,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        );
        let _ = self.conn.flush();
        eprintln!(
            "editor: X11 child {} (WM_CLASS {:?}) lives INSIDE main window {} — embedded",
            child,
            self.window_class(child),
            self.parent
        );
    }

    fn window_class(&self, win: u32) -> String {
        self.conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).replace('\0', " ").trim().to_string())
            .unwrap_or_default()
    }

    fn set_mapped(&mut self, mapped: bool) {
        if self.mapped == mapped {
            return;
        }
        self.mapped = mapped;
        if let Some(child) = self.child {
            let _ = if mapped {
                self.conn.map_window(child)
            } else {
                self.conn.unmap_window(child)
            };
            let _ = self.conn.flush();
        }
    }

    /// Fit the editor to the region (debounced while the region is actively
    /// resizing) and center the child in it. `force` skips the debounce.
    fn fit_and_place(&mut self, main: &mut ClapVoiceMain, rect: EditorRect, force: bool) {
        if rect.w < 16 || rect.h < 16 {
            return;
        }
        let region_changed = self.last_rect != Some(rect);
        if region_changed {
            if force {
                self.rect_changed = Instant::now() - Duration::from_secs(1);
            } else if self.last_rect.is_none() {
                self.rect_changed = Instant::now();
            }
            self.last_rect = Some(rect);
        }
        // Re-fit the plugin size only once the region settles (150 ms), so
        // live window resizes don't re-zoom Surge every frame.
        let settled = self.rect_changed.elapsed() >= Duration::from_millis(150);
        if (region_changed && settled) || force {
            let desired = GuiSize { width: rect.w as u32, height: rect.h as u32 };
            let target = self.adjusted_fit(main, desired, rect);
            if target != self.size || force {
                let _ = self.gui.set_size(&main.instance_mut().plugin_handle(), target);
                self.size = self
                    .gui
                    .get_size(&main.instance_mut().plugin_handle())
                    .unwrap_or(target);
            }
            self.last_geom = None;
        }
        // Center the child in the region; never exceed it (X children are
        // only clipped by the main window, not by our reserved region).
        let Some(child) = self.child else { return };
        let cw = self.size.width.min(rect.w as u32) as u16;
        let ch = self.size.height.min(rect.h as u32) as u16;
        let cx = rect.x + ((rect.w.saturating_sub(cw)) / 2) as i16;
        let cy = rect.y + ((rect.h.saturating_sub(ch)) / 2) as i16;
        let geom = (cx, cy, cw, ch);
        if self.last_geom != Some(geom) {
            let _ = self.conn.configure_window(
                child,
                &ConfigureWindowAux::new()
                    .x(cx as i32)
                    .y(cy as i32)
                    .width(cw as u32)
                    .height(ch as u32),
            );
            let _ = self.conn.flush();
            self.last_geom = Some(geom);
        }
        if !self.mapped {
            self.set_mapped(true);
        }
    }

    /// CLAP-correct size fitting: `set_size` must receive a size approved by
    /// `adjust_size` (Surge snaps to its zoom steps and may round UP). Shrink
    /// the request proportionally and re-adjust until it fits the region.
    fn adjusted_fit(&self, main: &mut ClapVoiceMain, desired: GuiSize, rect: EditorRect) -> GuiSize {
        let mut want = desired;
        let mut last = desired;
        for _ in 0..8 {
            let adj = self
                .gui
                .adjust_size(&main.instance_mut().plugin_handle(), want)
                .unwrap_or(want);
            last = adj;
            if adj.width <= rect.w as u32 && adj.height <= rect.h as u32 {
                return adj;
            }
            let s = (rect.w as f64 / adj.width as f64).min(rect.h as f64 / adj.height as f64);
            want = GuiSize {
                width: ((want.width as f64 * s) as u32).max(1),
                height: ((want.height as f64 * s) as u32).max(1),
            };
        }
        last // zoom granularity may leave a few px overflow; centered anyway
    }

    /// Hard evidence: enumerate the main window's children with WM_CLASS and
    /// geometry (x11rb query_tree — the xwininfo -tree equivalent).
    fn log_evidence(&self, when: &str) {
        let Ok(tree) = self.conn.query_tree(self.parent).map(|c| c.reply()) else {
            return;
        };
        let Ok(tree) = tree else { return };
        let mut lines = Vec::new();
        for &child in &tree.children {
            let geom = self
                .conn
                .get_geometry(child)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|g| format!("{}x{}+{}+{}", g.width, g.height, g.x, g.y))
                .unwrap_or_else(|| "?".into());
            lines.push(format!(
                "    child {} WM_CLASS {:?} geom {}{}",
                child,
                self.window_class(child),
                geom,
                if Some(child) == self.child { "  <-- Surge editor" } else { "" }
            ));
        }
        eprintln!(
            "editor evidence [{when}]: main window {} has {} child window(s):\n{}",
            self.parent,
            tree.children.len(),
            lines.join("\n")
        );
    }
}

/// Clamp a plugin size to the reserved region (never cover the rack).
fn clamp_size(size: GuiSize, rect: EditorRect) -> GuiSize {
    GuiSize {
        width: size.width.min(rect.w as u32).max(1),
        height: size.height.min(rect.h as u32).max(1),
    }
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

    #[test]
    fn failed_bundle_is_not_rescanned_until_it_changes() {
        let tmp = TempDir::new("negcache");
        let bad = tmp.0.join("hang.clap");
        std::fs::write(&bad, b"v1").unwrap();
        let cache_file = tmp.0.join("cache").join("clap-scan.json");
        let spawn_log = tmp.0.join("spawns.log");
        let hang = sh_cmd(&r#"printf '%s\n' "$1" >> '__LOG__'; sleep 10"#.replace("__LOG__", &spawn_log.display().to_string()));
        let list = |cmd: &ScannerCommand| {
            let start = Instant::now();
            let out = list_clap_plugins_with(&[bad.clone()], cmd, Some(&cache_file), Duration::from_millis(150), 1);
            (out, start.elapsed())
        };
        let spawns = || std::fs::read_to_string(&spawn_log).map_or(0, |s| s.lines().count());

        let (first, _) = list(&hang);
        assert!(first.is_empty());
        assert_eq!(spawns(), 1);
        let (second, took) = list(&hang);
        assert!(second.is_empty());
        assert_eq!(spawns(), 1, "a cached failure must not spawn the scanner again");
        assert!(took < Duration::from_millis(100), "cached failure still paid the timeout: {took:?}");

        std::fs::write(&bad, b"v2-fixed").unwrap();
        let fixed = sh_cmd(r#"printf '[{"id":"id.fixed","name":"F","vendor":"V","features":[]}]\n'"#);
        let (third, _) = list(&fixed);
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