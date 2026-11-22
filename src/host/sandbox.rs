//! Out-of-process CLAP plugin hosting (#209): each plugin runs in its own
//! `kabelsalat plugin-worker` helper process communicating over an anonymous
//! Linux shared-memory block (`memfd_create` + `mmap(MAP_SHARED)`) with
//! cross-process futex wakeups for RT audio, plus a `UnixStream` socketpair
//! for non-RT control RPC (params, state save/restore, X11 plugin editor).
//!
//! If a plugin crashes (`SIGSEGV`, `SIGABRT`, hang past deadline):
//! - The audio thread sees a deadline miss / dead flag and outputs clean
//!   silence for THAT strip only (zero allocations, zero locks, zero xruns
//!   on every other rig).
//! - The UI/main-thread supervisor detects the dead worker, spawns a fresh
//!   worker with the unit's last saved state, and hot-swaps the new shared
//!   memory handle into the audio thread via a wait-free `rtrb` ring.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clack_extensions::tail::TailLength;
use clack_host::process::ProcessStatus;
use clack_host::utils::ClapId;
use serde::{Deserialize, Serialize};

use super::{ClapVoice, ClapVoiceMain, ParamDesc, VoiceCmd};
use crate::main_controls::{MainControl, MainControls, MainSource};
use crate::transport::{TimeSig, Transport};

/// Built-in synthetic plugin ID handled directly inside `plugin-worker` so
/// unit tests, CI, and `kabelsalat sandbox-test` can exercise full multi-process
/// audio, state persistence, `kill -SEGV` recovery, and IPC latency without
/// requiring external `.clap` bundles on disk.
pub const SANDBOX_TEST_PLUGIN_ID: &str = "org.kabelsalat.sandbox-test-synth";

/// Whether out-of-process sandboxing is enabled for hosted CLAP plugins in the
/// app (`KABELSALAT_SANDBOX=0` disables it and falls back to in-process).
pub fn sandbox_enabled() -> bool {
    match std::env::var("KABELSALAT_SANDBOX") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

const SHM_MAGIC: u32 = 0x4B53_4C54; // "KSLT"
const SHM_VERSION: u32 = 1;
pub const MAX_SHM_FRAMES: usize = 512;
const MAX_SHM_EVENTS: usize = 256;

/// Consecutive deadline misses before the audio side stops waiting on a hung
/// worker and lets the supervisor restart it.
const DEAD_MISS_THRESHOLD: u32 = 3;

/// Default per-block wait deadline on the audio thread (microseconds).
/// Bounded below a 256-frame @ 48 kHz buffer (5333 µs) so a dying/hung plugin
/// never causes an xrun on other rigs.
const DEFAULT_DEADLINE_US: u64 = 4_000;

/// Generous one-time deadline for the very first block after spawning a fresh
/// worker (absorbs OS thread scheduling jitter on loaded CI runners).
const FIRST_BLOCK_DEADLINE_US: u64 = 50_000;

// ---------------------------------------------------------------------------
// Shared-memory ABI (host audio thread <-> worker audio thread)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ShmEvent {
    frame_offset: u32,
    /// 0 = NoteOn, 1 = NoteOff, 2 = Param, 3 = Midi
    kind: u32,
    a: u32,
    b: u32,
    val: f64,
}

impl ShmEvent {
    fn from_cmd(frame_offset: u32, cmd: VoiceCmd) -> Self {
        match cmd {
            VoiceCmd::NoteOn(note, vel) => Self { frame_offset, kind: 0, a: note as u32, b: 0, val: vel },
            VoiceCmd::NoteOff(note) => Self { frame_offset, kind: 1, a: note as u32, b: 0, val: 0.0 },
            VoiceCmd::Param(id, v) => Self { frame_offset, kind: 2, a: id.get(), b: 0, val: v },
            VoiceCmd::Midi([s, d1, d2]) => Self {
                frame_offset,
                kind: 3,
                a: (s as u32) | ((d1 as u32) << 8) | ((d2 as u32) << 16),
                b: 0,
                val: 0.0,
            },
        }
    }

    fn to_cmd(self) -> Option<VoiceCmd> {
        match self.kind {
            0 => Some(VoiceCmd::NoteOn(self.a as u16, self.val)),
            1 => Some(VoiceCmd::NoteOff(self.a as u16)),
            2 => Some(VoiceCmd::Param(ClapId::new(self.a), self.val)),
            3 => Some(VoiceCmd::Midi([
                (self.a & 0xff) as u8,
                ((self.a >> 8) & 0xff) as u8,
                ((self.a >> 16) & 0xff) as u8,
            ])),
            _ => None,
        }
    }
}

#[repr(C, align(64))]
struct ShmBlock {
    magic: u32,
    version: u32,
    host_seq: AtomicU32,
    worker_seq: AtomicU32,
    worker_alive: AtomicU32,
    worker_pid: AtomicU32,
    latency_frames: AtomicU32,
    last_status: AtomicU32,
    tail_frames: AtomicU32,
    wake_requested: AtomicU32,
    sample_rate: f64,
    frames: u32,
    has_input: u32,
    transport_playing: u32,
    transport_sig_num: u8,
    transport_sig_den: u8,
    _pad: [u8; 2],
    transport_bpm: f64,
    transport_beats: f64,
    transport_seconds: f64,
    event_count: u32,
    events: [ShmEvent; MAX_SHM_EVENTS],
    in_buf: [f32; MAX_SHM_FRAMES * 2],
    out_buf: [f32; MAX_SHM_FRAMES * 2],
}

fn encode_status(s: ProcessStatus) -> u32 {
    match s {
        ProcessStatus::Continue => 1,
        ProcessStatus::ContinueIfNotQuiet => 2,
        ProcessStatus::Tail => 3,
        ProcessStatus::Sleep => 4,
    }
}

fn decode_status(v: u32) -> ProcessStatus {
    match v {
        2 => ProcessStatus::ContinueIfNotQuiet,
        3 => ProcessStatus::Tail,
        4 => ProcessStatus::Sleep,
        _ => ProcessStatus::Continue,
    }
}

fn encode_tail(t: TailLength) -> u32 {
    match t {
        TailLength::Infinite => u32::MAX - 1,
        TailLength::Finite(n) => n.min(u32::MAX - 2),
    }
}

fn decode_tail(v: u32) -> Option<TailLength> {
    if v == u32::MAX {
        None
    } else if v == u32::MAX - 1 {
        Some(TailLength::Infinite)
    } else {
        Some(TailLength::Finite(v))
    }
}

// ---------------------------------------------------------------------------
// Linux futex + memfd shared memory helpers
// ---------------------------------------------------------------------------

#[inline]
fn futex_wake(atom: &AtomicU32) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            atom as *const AtomicU32,
            libc::FUTEX_WAKE,
            1i32,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

#[inline]
fn futex_wait_timeout(atom: &AtomicU32, expected: u32, timeout_ns: u64) {
    #[cfg(target_os = "linux")]
    unsafe {
        let ts = libc::timespec {
            tv_sec: (timeout_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (timeout_ns % 1_000_000_000) as libc::c_long,
        };
        libc::syscall(
            libc::SYS_futex,
            atom as *const AtomicU32,
            libc::FUTEX_WAIT,
            expected,
            &ts as *const libc::timespec,
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

/// Owned mapping of a [`ShmBlock`]. Safe to move across threads (`Send`),
/// must only be dropped on a non-RT thread because `Drop` calls `munmap` and
/// `close`.
pub(crate) struct ShmRegion {
    fd: RawFd,
    ptr: *mut ShmBlock,
    len: usize,
}

// SAFETY: `ShmRegion` owns a process-mapped shared memory region whose fields
// follow the strict host/worker sequence protocol.
unsafe impl Send for ShmRegion {}

impl ShmRegion {
    fn create(sample_rate: f64) -> Result<Self> {
        let len = std::mem::size_of::<ShmBlock>();
        let fd = unsafe {
            let name = b"kabelsalat-plugin-shm\0";
            let fd = libc::syscall(
                libc::SYS_memfd_create,
                name.as_ptr() as *const libc::c_char,
                libc::MFD_CLOEXEC as u32,
            ) as RawFd;
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).context("memfd_create");
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err).context("ftruncate memfd");
            }
            fd
        };
        let region = Self::map_fd(fd, len)?;
        let blk = region.block_mut();
        blk.magic = SHM_MAGIC;
        blk.version = SHM_VERSION;
        blk.host_seq.store(0, Ordering::Relaxed);
        blk.worker_seq.store(0, Ordering::Relaxed);
        blk.worker_alive.store(1, Ordering::Relaxed);
        blk.worker_pid.store(0, Ordering::Relaxed);
        blk.latency_frames.store(0, Ordering::Relaxed);
        blk.last_status.store(1, Ordering::Relaxed);
        blk.tail_frames.store(u32::MAX, Ordering::Relaxed);
        blk.wake_requested.store(0, Ordering::Relaxed);
        blk.sample_rate = sample_rate;
        Ok(region)
    }

    fn from_inherited_fd(fd: RawFd) -> Result<Self> {
        let len = std::mem::size_of::<ShmBlock>();
        let region = Self::map_fd(fd, len)?;
        let blk = region.block();
        if blk.magic != SHM_MAGIC || blk.version != SHM_VERSION {
            bail!("invalid plugin shm header: magic={:#x} ver={}", blk.magic, blk.version);
        }
        Ok(region)
    }

    fn map_fd(fd: RawFd, len: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd); }
            return Err(err).context("mmap plugin shm");
        }
        Ok(Self { fd, ptr: ptr as *mut ShmBlock, len })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd
    }

    #[inline]
    fn block(&self) -> &ShmBlock {
        unsafe { &*self.ptr }
    }

    #[inline]
    fn block_mut(&self) -> &mut ShmBlock {
        unsafe { &mut *self.ptr }
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

// ---------------------------------------------------------------------------
// Control RPC protocol over UnixStream
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
enum CtrlReq {
    Init {
        plugin_id: String,
        sample_rate: f64,
        state: Option<Vec<u8>>,
    },
    SaveState,
    LoadState { data: Vec<u8> },
    ParamList,
    ParamDesc { id: u32 },
    ParamValue { id: u32 },
    ParamText { id: u32, value: f64 },
    ParamTextOpt { id: u32, value: f64 },
    ParamTextToValue { id: u32, text: String },
    ParamOptions { param: ParamDesc },
    ResolveParamText { param: ParamDesc, text: String },
    PollMainControls,
    PollLatency,
    OpenEditor { title: String },
    CloseEditor,
    EditorOpen,
    Pump,
    Shutdown,
}

#[derive(Serialize, Deserialize)]
enum CtrlResp {
    InitOk {
        is_effect: bool,
        latency: u32,
        main_controls: MainControls,
        has_editor: bool,
        param_ranges: Vec<(u32, f64, f64)>,
    },
    Ok,
    Err(String),
    State(Vec<u8>),
    Params(Vec<ParamDesc>),
    OptParam(Option<ParamDesc>),
    OptF64(Option<f64>),
    Text(String),
    OptText(Option<String>),
    Options(Vec<(f64, String)>),
    MainControlsPoll {
        changed: bool,
        controls: Option<MainControls>,
    },
    Latency(u32),
    Bool(bool),
}

fn send_msg<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    let len = (bytes.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn recv_msg<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 32 * 1024 * 1024 {
        bail!("control message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

// ---------------------------------------------------------------------------
// Built-in synthetic worker plugin for tests, CI, and sandbox-test
// ---------------------------------------------------------------------------

struct TestSynthState {
    sample_rate: f64,
    phase: f64,
    freq_hz: f64,
    gain: f32,
    held_note: Option<u16>,
    /// Incremented on every rendered block.
    blocks: u64,
}

impl TestSynthState {
    fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            phase: 0.0,
            freq_hz: 440.0,
            gain: 0.4,
            held_note: None,
            blocks: 0,
        }
    }

    fn apply_cmd(&mut self, cmd: VoiceCmd) {
        match cmd {
            VoiceCmd::NoteOn(note, vel) => {
                self.held_note = Some(note);
                self.freq_hz = 440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0);
                if vel > 0.0 {
                    self.gain = (vel as f32).clamp(0.05, 1.0) * 0.5;
                }
            }
            VoiceCmd::NoteOff(note) => {
                if self.held_note == Some(note) {
                    self.held_note = None;
                }
            }
            VoiceCmd::Param(id, v) => {
                if id.get() == 0 {
                    self.gain = (v as f32).clamp(0.0, 1.0);
                } else if id.get() == 1 {
                    self.freq_hz = v.clamp(20.0, 4000.0);
                }
            }
            VoiceCmd::Midi([status, d1, d2]) => {
                let kind = status & 0xf0;
                if kind == 0x90 && d2 > 0 {
                    self.apply_cmd(VoiceCmd::NoteOn(d1 as u16, d2 as f64 / 127.0));
                } else if kind == 0x80 || (kind == 0x90 && d2 == 0) {
                    self.apply_cmd(VoiceCmd::NoteOff(d1 as u16));
                }
            }
        }
    }

    fn render(&mut self, out: &mut [f32]) {
        self.blocks += 1;
        let frames = out.len() / 2;
        if self.held_note.is_none() && self.gain <= 0.0 {
            out.fill(0.0);
            return;
        }
        if self.held_note.is_some() {
            let inc = std::f64::consts::TAU * self.freq_hz / self.sample_rate.max(1.0);
            for i in 0..frames {
                let s = (self.phase.sin() as f32) * self.gain;
                out[i * 2] = s;
                out[i * 2 + 1] = s;
                self.phase = (self.phase + inc) % std::f64::consts::TAU;
            }
        } else {
            out.fill(0.0);
        }
    }

    fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(&self.gain.to_le_bytes());
        out.extend_from_slice(&self.freq_hz.to_le_bytes());
        let note: i32 = self.held_note.map_or(-1, |n| n as i32);
        out.extend_from_slice(&note.to_le_bytes());
        out
    }

    fn load_state(&mut self, data: &[u8]) {
        if data.len() >= 16 {
            self.gain = f32::from_le_bytes(data[0..4].try_into().unwrap()).clamp(0.0, 1.0);
            self.freq_hz = f64::from_le_bytes(data[4..12].try_into().unwrap()).clamp(20.0, 4000.0);
            let note = i32::from_le_bytes(data[12..16].try_into().unwrap());
            self.held_note = if note >= 0 { Some(note as u16) } else { None };
        }
    }

    fn param_list(&self) -> Vec<ParamDesc> {
        vec![
            ParamDesc {
                id: 0,
                name: "Gain".into(),
                module: "Synth".into(),
                min: 0.0,
                max: 1.0,
                value: self.gain as f64,
                default: 0.4,
                automatable: true,
                stepped: false,
                hidden: false,
                readonly: false,
            },
            ParamDesc {
                id: 1,
                name: "Frequency".into(),
                module: "Synth".into(),
                min: 20.0,
                max: 4000.0,
                value: self.freq_hz,
                default: 440.0,
                automatable: true,
                stepped: false,
                hidden: false,
                readonly: false,
            },
        ]
    }

    fn main_controls(&self) -> MainControls {
        MainControls {
            source: MainSource::RemoteControls,
            controls: vec![
                MainControl {
                    id: 0,
                    name: "Gain".into(),
                    page: "Main".into(),
                    min: 0.0,
                    max: 1.0,
                    default: 0.4,
                },
                MainControl {
                    id: 1,
                    name: "Frequency".into(),
                    page: "Main".into(),
                    min: 20.0,
                    max: 4000.0,
                    default: 440.0,
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Worker process entry point (`kabelsalat plugin-worker <shm_fd> <ctrl_fd>`)
// ---------------------------------------------------------------------------

enum WorkerAudio {
    Clap(ClapVoice),
    TestSynth(Arc<std::sync::Mutex<TestSynthState>>),
}

enum WorkerMain {
    Clap(ClapVoiceMain),
    TestSynth(Arc<std::sync::Mutex<TestSynthState>>),
}

/// Entry point of the isolated `kabelsalat plugin-worker` child process.
pub fn run_plugin_worker(shm_fd: RawFd, ctrl_fd: RawFd) -> Result<()> {
    // Terminate immediately on fatal plugin faults or `kill -SEGV` without
    // stalling on Rust's stack-overflow SIGSEGV handler or writing huge core
    // dumps mid-gig.
    unsafe {
        libc::signal(libc::SIGSEGV, libc::SIG_DFL);
        libc::signal(libc::SIGBUS, libc::SIG_DFL);
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
    }
    let shm = ShmRegion::from_inherited_fd(shm_fd)?;
    let mut ctrl = unsafe { UnixStream::from_raw_fd(ctrl_fd) };
    shm.block().worker_pid.store(std::process::id(), Ordering::Release);
    shm.block().worker_alive.store(1, Ordering::Release);

    // Wait for Init.
    let (plugin_id, sample_rate, init_state) = match recv_msg::<CtrlReq>(&mut ctrl)? {
        CtrlReq::Init { plugin_id, sample_rate, state } => (plugin_id, sample_rate, state),
        _ => bail!("expected Init as first plugin-worker request"),
    };

    let (worker_audio, mut worker_main, init_resp) = if plugin_id == SANDBOX_TEST_PLUGIN_ID {
        let mut st = TestSynthState::new(sample_rate);
        if let Some(blob) = &init_state {
            st.load_state(blob);
        }
        let mc = st.main_controls();
        let ranges = vec![(0, 0.0, 1.0), (1, 20.0, 4000.0)];
        let shared = Arc::new(std::sync::Mutex::new(st));
        (
            WorkerAudio::TestSynth(shared.clone()),
            WorkerMain::TestSynth(shared),
            CtrlResp::InitOk {
                is_effect: false,
                latency: 0,
                main_controls: mc,
                has_editor: false,
                param_ranges: ranges,
            },
        )
    } else {
        match ClapVoice::new_plugin(sample_rate, &plugin_id) {
            Ok((mut voice, mut main)) => {
                if let Some(blob) = &init_state {
                    let _ = main.load_state(blob);
                }
                // Warm up 2 silent blocks on a separate audio thread so
                // start_processing and state-load XML parsing settle before
                // the host starts timing blocks.
                std::thread::scope(|s| {
                    s.spawn(|| {
                        let mut warm = [0.0f32; 256 * 2];
                        let mut t = Transport::default();
                        voice.render(&mut warm, t);
                        t.advance(256, sample_rate);
                        voice.render(&mut warm, t);
                    });
                });
                main.service();
                main.poll_main_controls();
                let is_effect = voice.is_effect();
                let latency = voice.latency();
                let main_controls = main.main_controls().clone();
                let has_editor = main.has_editor();
                let param_ranges = main
                    .param_list()
                    .into_iter()
                    .map(|p| (p.id, p.min, p.max))
                    .collect();
                shm.block().latency_frames.store(latency, Ordering::Release);
                (
                    WorkerAudio::Clap(voice),
                    WorkerMain::Clap(main),
                    CtrlResp::InitOk {
                        is_effect,
                        latency,
                        main_controls,
                        has_editor,
                        param_ranges,
                    },
                )
            }
            Err(e) => {
                let _ = send_msg(&mut ctrl, &CtrlResp::Err(format!("{e:#}")));
                shm.block().worker_alive.store(0, Ordering::Release);
                return Err(e);
            }
        }
    };

    // Spawn the worker's dedicated audio thread BEFORE replying InitOk so the
    // very first render block from the host never races thread startup.
    let stop_audio = Arc::new(AtomicBool::new(false));
    let audio_ready = Arc::new(AtomicBool::new(false));
    let stop_bg = stop_audio.clone();
    let ready_bg = audio_ready.clone();
    let shm_ptr = shm.ptr as usize;
    let audio_handle = std::thread::Builder::new()
        .name("kabelsalat-worker-audio".into())
        .spawn(move || {
            let blk = unsafe { &mut *(shm_ptr as *mut ShmBlock) };
            ready_bg.store(true, Ordering::Release);
            worker_audio_loop(worker_audio, blk, &stop_bg);
        })
        .context("spawn worker audio thread")?;
    while !audio_ready.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }

    send_msg(&mut ctrl, &init_resp)?;

    // Main thread: serve control RPC and pump CLAP main-thread callbacks/editor.
    // Pump on every loop turn once 16 ms have elapsed, NOT only on read timeout:
    // while an editor is open the UI polls `EditorOpen` every frame (<=16 ms),
    // which would otherwise prevent `recv_msg` from ever timing out and starve
    // the plugin's X11/timer/posix_fd event loop.
    ctrl.set_read_timeout(Some(Duration::from_millis(16)))?;
    let mut last_pump = Instant::now();
    let pump_due = |worker_main: &mut WorkerMain, shm: &ShmRegion, last_pump: &mut Instant| {
        if last_pump.elapsed() >= Duration::from_millis(16) {
            if let WorkerMain::Clap(m) = worker_main {
                m.pump();
                let lat = m.poll_latency();
                shm.block().latency_frames.store(lat, Ordering::Release);
            }
            *last_pump = Instant::now();
        }
    };
    loop {
        match recv_msg::<CtrlReq>(&mut ctrl) {
            Ok(CtrlReq::Shutdown) => {
                let _ = send_msg(&mut ctrl, &CtrlResp::Ok);
                break;
            }
            Ok(req) => {
                let resp = handle_worker_req(&mut worker_main, req);
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
                pump_due(&mut worker_main, &shm, &mut last_pump);
            }
            Err(e) => {
                if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
                    if matches!(ioe.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
                        pump_due(&mut worker_main, &shm, &mut last_pump);
                        continue;
                    }
                }
                // Parent closed socket or died: exit cleanly so we never orphan.
                break;
            }
        }
    }

    stop_audio.store(true, Ordering::Release);
    shm.block().worker_alive.store(0, Ordering::Release);
    futex_wake(&shm.block().host_seq);
    let _ = audio_handle.join();
    // Leak any ClapVoiceMain on worker exit rather than risking never-free
    // plugin destructor crashes at shutdown (rules 56, 133).
    if let WorkerMain::Clap(m) = worker_main {
        std::mem::forget(m);
    }
    Ok(())
}

fn worker_audio_loop(mut audio: WorkerAudio, blk: &mut ShmBlock, stop: &AtomicBool) {
    let mut last_seq = 0u32;
    let mut out_tmp = [0.0f32; MAX_SHM_FRAMES * 2];
    let mut in_tmp = [0.0f32; MAX_SHM_FRAMES * 2];

    while !stop.load(Ordering::Acquire) && blk.worker_alive.load(Ordering::Acquire) != 0 {
        let mut seq = blk.host_seq.load(Ordering::Acquire);
        if seq == last_seq {
            // Brief spin before sleeping in futex for lowest round-trip latency.
            for _ in 0..64 {
                std::hint::spin_loop();
                seq = blk.host_seq.load(Ordering::Acquire);
                if seq != last_seq {
                    break;
                }
            }
        }
        if seq == last_seq {
            futex_wait_timeout(&blk.host_seq, last_seq, 25_000_000);
            continue;
        }
        last_seq = seq;

        let frames = (blk.frames as usize).min(MAX_SHM_FRAMES);
        let n2 = frames * 2;
        let has_input = blk.has_input != 0;
        let ev_count = (blk.event_count as usize).min(MAX_SHM_EVENTS);
        let transport = Transport {
            playing: blk.transport_playing != 0,
            bpm: blk.transport_bpm,
            beats: blk.transport_beats,
            seconds: blk.transport_seconds,
            sig: TimeSig::normalized(blk.transport_sig_num, blk.transport_sig_den),
        };

        if has_input {
            in_tmp[..n2].copy_from_slice(&blk.in_buf[..n2]);
        }

        match &mut audio {
            WorkerAudio::Clap(voice) => {
                for i in 0..ev_count {
                    if let Some(cmd) = blk.events[i].to_cmd() {
                        voice.push_rt(cmd);
                    }
                }
                if has_input {
                    voice.process(&in_tmp[..n2], &mut out_tmp[..n2], transport);
                } else {
                    voice.render(&mut out_tmp[..n2], transport);
                }
                blk.latency_frames.store(voice.latency(), Ordering::Release);
                blk.last_status.store(encode_status(voice.last_process_status()), Ordering::Release);
                let tail = voice.tail_length().map_or(u32::MAX, encode_tail);
                blk.tail_frames.store(tail, Ordering::Release);
                if voice.take_wake_request() {
                    blk.wake_requested.store(1, Ordering::Release);
                }
            }
            WorkerAudio::TestSynth(shared) => {
                if let Ok(mut st) = shared.lock() {
                    for i in 0..ev_count {
                        if let Some(cmd) = blk.events[i].to_cmd() {
                            st.apply_cmd(cmd);
                        }
                    }
                    st.render(&mut out_tmp[..n2]);
                } else {
                    out_tmp[..n2].fill(0.0);
                }
                blk.last_status.store(1, Ordering::Release);
            }
        }

        blk.out_buf[..n2].copy_from_slice(&out_tmp[..n2]);
        blk.worker_seq.store(seq, Ordering::Release);
        futex_wake(&blk.worker_seq);
    }
}

fn handle_worker_req(main: &mut WorkerMain, req: CtrlReq) -> CtrlResp {
    match main {
        WorkerMain::Clap(m) => match req {
            CtrlReq::SaveState => match m.save_state() {
                Ok(b) => CtrlResp::State(b),
                Err(e) => CtrlResp::Err(format!("{e:#}")),
            },
            CtrlReq::LoadState { data } => match m.load_state(&data) {
                Ok(()) => CtrlResp::Ok,
                Err(e) => CtrlResp::Err(format!("{e:#}")),
            },
            CtrlReq::ParamList => CtrlResp::Params(m.param_list()),
            CtrlReq::ParamDesc { id } => CtrlResp::OptParam(m.param_desc(id)),
            CtrlReq::ParamValue { id } => CtrlResp::OptF64(m.param_value(id)),
            CtrlReq::ParamText { id, value } => CtrlResp::Text(m.param_text(id, value)),
            CtrlReq::ParamTextOpt { id, value } => CtrlResp::OptText(m.param_text_opt(id, value)),
            CtrlReq::ParamTextToValue { id, text } => CtrlResp::OptF64(m.param_text_to_value(id, &text)),
            CtrlReq::ParamOptions { param } => CtrlResp::Options(m.param_options(&param)),
            CtrlReq::ResolveParamText { param, text } => CtrlResp::OptF64(m.resolve_param_text(&param, &text)),
            CtrlReq::PollMainControls => {
                let changed = m.poll_main_controls();
                let controls = if changed { Some(m.main_controls().clone()) } else { None };
                CtrlResp::MainControlsPoll { changed, controls }
            }
            CtrlReq::PollLatency => CtrlResp::Latency(m.poll_latency()),
            CtrlReq::OpenEditor { title } => match m.open_editor(&title) {
                Ok(()) => CtrlResp::Ok,
                Err(e) => CtrlResp::Err(format!("{e:#}")),
            },
            CtrlReq::CloseEditor => {
                m.close_editor();
                CtrlResp::Ok
            }
            CtrlReq::EditorOpen => {
                if m.editor_open() {
                    m.pump();
                }
                CtrlResp::Bool(m.editor_open())
            }
            CtrlReq::Pump => {
                m.pump();
                CtrlResp::Ok
            }
            CtrlReq::Init { .. } | CtrlReq::Shutdown => CtrlResp::Ok,
        },
        WorkerMain::TestSynth(shared) => {
            let mut st = shared.lock().unwrap();
            match req {
                CtrlReq::SaveState => CtrlResp::State(st.save_state()),
                CtrlReq::LoadState { data } => {
                    st.load_state(&data);
                    CtrlResp::Ok
                }
                CtrlReq::ParamList => CtrlResp::Params(st.param_list()),
                CtrlReq::ParamDesc { id } => CtrlResp::OptParam(st.param_list().into_iter().find(|p| p.id == id)),
                CtrlReq::ParamValue { id } => CtrlResp::OptF64(match id {
                    0 => Some(st.gain as f64),
                    1 => Some(st.freq_hz),
                    _ => None,
                }),
                CtrlReq::ParamText { value, .. } => CtrlResp::Text(format!("{value:.3}")),
                CtrlReq::ParamTextOpt { value, .. } => CtrlResp::OptText(Some(format!("{value:.3}"))),
                CtrlReq::ParamTextToValue { text, .. } => CtrlResp::OptF64(text.trim().parse().ok()),
                CtrlReq::ParamOptions { .. } => CtrlResp::Options(Vec::new()),
                CtrlReq::ResolveParamText { text, .. } => CtrlResp::OptF64(text.trim().parse().ok()),
                CtrlReq::PollMainControls => CtrlResp::MainControlsPoll { changed: false, controls: None },
                CtrlReq::PollLatency => CtrlResp::Latency(0),
                CtrlReq::OpenEditor { .. } => CtrlResp::Err("test synth has no editor".into()),
                CtrlReq::CloseEditor | CtrlReq::Pump | CtrlReq::Init { .. } | CtrlReq::Shutdown => CtrlResp::Ok,
                CtrlReq::EditorOpen => CtrlResp::Bool(false),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host-side audio half (`SandboxVoice`) & supervisor (`SandboxMain`)
// ---------------------------------------------------------------------------

/// Audio-thread side of a sandboxed plugin. Allocation-free, lock-free,
/// never calls `munmap` (old regions return to `SandboxMain` over `retire_tx`).
pub(crate) struct SandboxVoice {
    shm: ShmRegion,
    swap_rx: rtrb::Consumer<ShmRegion>,
    retire_tx: rtrb::Producer<ShmRegion>,
    param_ranges: HashMap<u32, (f64, f64)>,
    deadline_ns: u64,
    first_block: bool,
    consecutive_misses: u32,
    crashed: Arc<AtomicBool>,
    latency: Arc<AtomicU32>,
    last_status: ProcessStatus,
    tail_length: Option<TailLength>,
}

impl SandboxVoice {
    pub(crate) fn param_range(&self, id: u32) -> Option<(f64, f64)> {
        self.param_ranges.get(&id).copied()
    }

    pub(crate) fn last_process_status(&self) -> ProcessStatus {
        self.last_status
    }

    pub(crate) fn tail_length(&self) -> Option<TailLength> {
        self.tail_length
    }

    pub(crate) fn take_wake_request(&mut self) -> bool {
        self.shm.block().wake_requested.swap(0, Ordering::AcqRel) != 0
    }

    /// Adopt any restarted worker's shared memory region without blocking or
    /// freeing on the audio thread.
    fn adopt_swap(&mut self) {
        while let Ok(new_shm) = self.swap_rx.pop() {
            let old = std::mem::replace(&mut self.shm, new_shm);
            if let Err(rtrb::PushError::Full(old)) = self.retire_tx.push(old) {
                // Ring full (should never happen with cap 4); avoid munmap on RT thread.
                std::mem::forget(old);
            }
            self.consecutive_misses = 0;
            self.first_block = true;
            self.crashed.store(false, Ordering::Release);
            self.last_status = ProcessStatus::Continue;
        }
    }

    pub(crate) fn render_block(
        &mut self,
        n: usize,
        inp: Option<&[f32]>,
        out: &mut [f32],
        transport: &Transport,
        cmds: &[VoiceCmd],
    ) {
        self.adopt_swap();
        let n = n.min(MAX_SHM_FRAMES);
        let n2 = n * 2;
        if self.crashed.load(Ordering::Acquire)
            || self.shm.block().worker_alive.load(Ordering::Acquire) == 0
        {
            out[..n2].fill(0.0);
            return;
        }

        let blk = self.shm.block_mut();
        blk.frames = n as u32;
        blk.has_input = u32::from(inp.is_some());
        blk.transport_playing = u32::from(transport.playing);
        blk.transport_bpm = transport.bpm;
        blk.transport_beats = transport.beats;
        blk.transport_seconds = transport.seconds;
        blk.transport_sig_num = transport.sig.num;
        blk.transport_sig_den = transport.sig.den;

        let ev_n = cmds.len().min(MAX_SHM_EVENTS);
        blk.event_count = ev_n as u32;
        for i in 0..ev_n {
            blk.events[i] = ShmEvent::from_cmd(0, cmds[i]);
        }
        if let Some(src) = inp {
            blk.in_buf[..n2].copy_from_slice(&src[..n2]);
        }

        let prev_seq = blk.worker_seq.load(Ordering::Acquire);
        let seq = blk.host_seq.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        futex_wake(&blk.host_seq);

        // Brief spin before sleeping in futex.
        let mut ready = blk.worker_seq.load(Ordering::Acquire) == seq;
        if !ready {
            for _ in 0..64 {
                std::hint::spin_loop();
                if blk.worker_seq.load(Ordering::Acquire) == seq {
                    ready = true;
                    break;
                }
            }
        }
        let wait_ns = if self.first_block {
            self.first_block = false;
            FIRST_BLOCK_DEADLINE_US * 1_000
        } else {
            self.deadline_ns
        };
        if !ready {
            let start = Instant::now();
            while blk.worker_seq.load(Ordering::Acquire) != seq {
                if blk.worker_alive.load(Ordering::Acquire) == 0 {
                    break;
                }
                let elapsed_ns = start.elapsed().as_nanos() as u64;
                if elapsed_ns >= wait_ns {
                    break;
                }
                futex_wait_timeout(&blk.worker_seq, prev_seq, wait_ns - elapsed_ns);
            }
            ready = blk.worker_seq.load(Ordering::Acquire) == seq;
        }

        if ready {
            out[..n2].copy_from_slice(&blk.out_buf[..n2]);
            let lat = blk.latency_frames.load(Ordering::Acquire);
            self.latency.store(lat, Ordering::Relaxed);
            self.last_status = decode_status(blk.last_status.load(Ordering::Acquire));
            self.tail_length = decode_tail(blk.tail_frames.load(Ordering::Acquire));
            self.consecutive_misses = 0;
        } else {
            out[..n2].fill(0.0);
            self.consecutive_misses += 1;
            if blk.worker_alive.load(Ordering::Acquire) == 0
                || self.consecutive_misses >= DEAD_MISS_THRESHOLD
            {
                self.crashed.store(true, Ordering::Release);
            }
        }
    }
}

/// UI / main-thread supervisor for an out-of-process plugin worker.
pub struct SandboxMain {
    plugin_id: String,
    sample_rate: f64,
    child: Option<Child>,
    ctrl: Option<UnixStream>,
    swap_tx: rtrb::Producer<ShmRegion>,
    retire_rx: rtrb::Consumer<ShmRegion>,
    crashed: Arc<AtomicBool>,
    last_state: Option<Vec<u8>>,
    main_controls: MainControls,
    has_editor: bool,
    editor_open: bool,
    latency: u32,
    worker_pid: u32,
    crash_count: u32,
    last_crash_reason: Option<String>,
    /// Short strip status message (e.g. `"Restarted after SIGSEGV"`).
    strip_notice: Option<String>,
}

struct SpawnedWorker {
    child: Child,
    ctrl: UnixStream,
    shm: ShmRegion,
    is_effect: bool,
    latency: u32,
    main_controls: MainControls,
    has_editor: bool,
    param_ranges: HashMap<u32, (f64, f64)>,
    pid: u32,
}

fn spawn_worker(plugin_id: &str, sample_rate: f64, state: Option<Vec<u8>>) -> Result<SpawnedWorker> {
    let shm = ShmRegion::create(sample_rate)?;
    let (mut parent_sock, child_sock) = UnixStream::pair().context("UnixStream::pair")?;
    let shm_fd = shm.raw_fd();
    let child_fd = child_sock.as_raw_fd();

    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = Command::new(&exe);
    #[cfg(test)]
    {
        cmd.arg("--exact")
            .arg("host::sandbox::worker_test_entry")
            .arg("--ignored")
            .arg("--nocapture")
            .env("KABELSALAT_TEST_WORKER_SHM_FD", shm_fd.to_string())
            .env("KABELSALAT_TEST_WORKER_CTRL_FD", child_fd.to_string());
    }
    #[cfg(not(test))]
    {
        cmd.arg("plugin-worker")
            .arg(shm_fd.to_string())
            .arg(child_fd.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // SAFETY: `fcntl(fd, F_SETFD, 0)` is async-signal-safe and clears
    // FD_CLOEXEC only in the forked child before exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(shm_fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(child_fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().with_context(|| format!("spawn plugin-worker for {plugin_id}"))?;
    let pid = child.id();
    drop(child_sock);

    parent_sock.set_read_timeout(Some(Duration::from_secs(15)))?;
    parent_sock.set_write_timeout(Some(Duration::from_secs(5)))?;

    send_msg(
        &mut parent_sock,
        &CtrlReq::Init {
            plugin_id: plugin_id.to_string(),
            sample_rate,
            state,
        },
    )?;

    match recv_msg::<CtrlResp>(&mut parent_sock) {
        Ok(CtrlResp::InitOk {
            is_effect,
            latency,
            main_controls,
            has_editor,
            param_ranges,
        }) => {
            parent_sock.set_read_timeout(Some(Duration::from_secs(3)))?;
            Ok(SpawnedWorker {
                child,
                ctrl: parent_sock,
                shm,
                is_effect,
                latency,
                main_controls,
                has_editor,
                param_ranges: param_ranges.into_iter().map(|(id, mn, mx)| (id, (mn, mx))).collect(),
                pid,
            })
        }
        Ok(CtrlResp::Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("plugin-worker init failed for {plugin_id}: {e}");
        }
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("unexpected plugin-worker init reply for {plugin_id}");
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e).with_context(|| format!("plugin-worker handshake failed for {plugin_id}"))
        }
    }
}

/// Spawn a new out-of-process plugin worker and return its audio-thread
/// [`ClapVoice`] and main-thread [`SandboxMain`] supervisor.
pub fn spawn_sandboxed_clap(
    sample_rate: f64,
    plugin_id: &str,
    initial_state: Option<Vec<u8>>,
) -> Result<(ClapVoice, SandboxMain)> {
    let w = spawn_worker(plugin_id, sample_rate, initial_state.clone())?;
    let (swap_tx, swap_rx) = rtrb::RingBuffer::<ShmRegion>::new(4);
    let (retire_tx, retire_rx) = rtrb::RingBuffer::<ShmRegion>::new(4);
    let crashed = Arc::new(AtomicBool::new(false));
    let latency_atom = Arc::new(AtomicU32::new(w.latency));

    let sv = SandboxVoice {
        shm: w.shm,
        swap_rx,
        retire_tx,
        param_ranges: w.param_ranges,
        deadline_ns: DEFAULT_DEADLINE_US * 1_000,
        first_block: true,
        consecutive_misses: 0,
        crashed: crashed.clone(),
        latency: latency_atom.clone(),
        last_status: ProcessStatus::Continue,
        tail_length: None,
    };

    let voice = ClapVoice::from_sandbox(plugin_id.to_string(), w.is_effect, sample_rate, latency_atom, sv);
    let main = SandboxMain {
        plugin_id: plugin_id.to_string(),
        sample_rate,
        child: Some(w.child),
        ctrl: Some(w.ctrl),
        swap_tx,
        retire_rx,
        crashed,
        last_state: initial_state,
        main_controls: w.main_controls,
        has_editor: w.has_editor,
        editor_open: false,
        latency: w.latency,
        worker_pid: w.pid,
        crash_count: 0,
        last_crash_reason: None,
        strip_notice: None,
    };
    Ok((voice, main))
}

impl SandboxMain {
    pub fn worker_pid(&self) -> u32 {
        self.worker_pid
    }

    pub fn crash_count(&self) -> u32 {
        self.crash_count
    }

    pub fn strip_notice(&self) -> Option<&str> {
        self.strip_notice.as_deref()
    }

    fn rpc(&mut self, req: CtrlReq) -> Option<CtrlResp> {
        let ctrl = self.ctrl.as_mut()?;
        if send_msg(ctrl, &req).is_err() {
            self.mark_dead("control socket closed");
            return None;
        }
        match recv_msg::<CtrlResp>(ctrl) {
            Ok(r) => Some(r),
            Err(_) => {
                self.mark_dead("control socket error");
                None
            }
        }
    }

    fn rpc_with_timeout(&mut self, req: CtrlReq, timeout: Duration) -> Option<CtrlResp> {
        let ctrl = self.ctrl.as_mut()?;
        let _ = ctrl.set_read_timeout(Some(timeout));
        let res = if send_msg(ctrl, &req).is_err() {
            Err(())
        } else {
            recv_msg::<CtrlResp>(ctrl).map_err(|_| ())
        };
        let _ = ctrl.set_read_timeout(Some(Duration::from_secs(3)));
        match res {
            Ok(r) => Some(r),
            Err(()) => {
                self.mark_dead("control socket error");
                None
            }
        }
    }

    fn mark_dead(&mut self, reason: &str) {
        self.ctrl = None;
        self.editor_open = false;
        self.crashed.store(true, Ordering::Release);
        if self.last_crash_reason.is_none() {
            self.last_crash_reason = Some(reason.to_string());
        }
    }

    /// Check if the worker process crashed or hung; if so, restart it with the
    /// last saved state and hot-swap the new shared memory region into the
    /// audio thread. Returns a human-readable message when a restart happens.
    pub fn poll_recovery(&mut self) -> Option<String> {
        // Always drain retired shared memory regions on the main thread.
        while self.retire_rx.pop().is_ok() {}

        let mut died_reason: Option<String> = None;
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let why = if let Some(sig) = status.signal() {
                        match sig {
                            libc::SIGSEGV => "SIGSEGV".to_string(),
                            libc::SIGABRT => "SIGABRT".to_string(),
                            libc::SIGKILL => "SIGKILL".to_string(),
                            libc::SIGBUS => "SIGBUS".to_string(),
                            libc::SIGFPE => "SIGFPE".to_string(),
                            other => format!("signal {other}"),
                        }
                    } else if let Some(code) = status.code() {
                        format!("exit {code}")
                    } else {
                        "terminated".to_string()
                    };
                    died_reason = Some(why);
                    self.child = None;
                    self.ctrl = None;
                    self.editor_open = false;
                    self.crashed.store(true, Ordering::Release);
                }
                Ok(None) => {}
                Err(_) => {
                    died_reason = Some("wait failed".into());
                    self.child = None;
                    self.ctrl = None;
                }
            }
        } else if self.crashed.load(Ordering::Acquire) {
            died_reason = self.last_crash_reason.take().or_else(|| Some("worker lost".into()));
        }

        // Hung worker (missed consecutive audio deadlines while still alive).
        if died_reason.is_none() && self.crashed.load(Ordering::Acquire) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            self.ctrl = None;
            self.editor_open = false;
            died_reason = Some("hung (deadline exceeded)".into());
        }

        let reason = died_reason?;
        self.crash_count += 1;
        self.last_crash_reason = Some(reason.clone());

        match spawn_worker(&self.plugin_id, self.sample_rate, self.last_state.clone()) {
            Ok(w) => {
                self.child = Some(w.child);
                self.ctrl = Some(w.ctrl);
                self.worker_pid = w.pid;
                self.main_controls = w.main_controls;
                self.has_editor = w.has_editor;
                self.latency = w.latency;
                let _ = self.swap_tx.push(w.shm);
                let msg = format!("Plugin {} crashed ({reason}) — restarted with saved state", self.plugin_id);
                self.strip_notice = Some(format!("RESTARTED ({reason})"));
                Some(msg)
            }
            Err(e) => {
                let msg = format!("Plugin {} crashed ({reason}) and failed to restart: {e}", self.plugin_id);
                self.strip_notice = Some(format!("CRASHED ({reason})"));
                Some(msg)
            }
        }
    }

    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    pub fn editor_open(&self) -> bool {
        self.editor_open
    }

    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        // First-time X11/OpenGL editor initialization in heavy plugins can
        // exceed the 3 s control RPC timeout; give it the same 15 s budget as Init.
        match self.rpc_with_timeout(
            CtrlReq::OpenEditor { title: title.to_string() },
            Duration::from_secs(15),
        ) {
            Some(CtrlResp::Ok) => {
                self.editor_open = true;
                Ok(())
            }
            Some(CtrlResp::Err(e)) => bail!("{e}"),
            _ => bail!("plugin worker unavailable"),
        }
    }

    pub fn close_editor(&mut self) {
        let _ = self.rpc(CtrlReq::CloseEditor);
        self.editor_open = false;
    }

    pub fn pump(&mut self) {
        let _ = self.poll_recovery();
        if self.editor_open {
            if let Some(CtrlResp::Bool(open)) = self.rpc(CtrlReq::EditorOpen) {
                self.editor_open = open;
            }
        }
    }

    pub fn poll_latency(&mut self) -> u32 {
        if let Some(CtrlResp::Latency(l)) = self.rpc(CtrlReq::PollLatency) {
            self.latency = l;
        }
        self.latency
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        match self.rpc(CtrlReq::SaveState) {
            Some(CtrlResp::State(b)) => {
                self.last_state = Some(b.clone());
                Ok(b)
            }
            Some(CtrlResp::Err(e)) => bail!("{e}"),
            _ => self
                .last_state
                .clone()
                .ok_or_else(|| anyhow!("plugin worker unavailable")),
        }
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        self.last_state = Some(data.to_vec());
        match self.rpc(CtrlReq::LoadState { data: data.to_vec() }) {
            Some(CtrlResp::Ok) => Ok(()),
            Some(CtrlResp::Err(e)) => bail!("{e}"),
            _ => bail!("plugin worker unavailable"),
        }
    }

    pub fn param_list(&mut self) -> Vec<ParamDesc> {
        match self.rpc(CtrlReq::ParamList) {
            Some(CtrlResp::Params(p)) => p,
            _ => Vec::new(),
        }
    }

    pub fn main_controls(&self) -> &MainControls {
        &self.main_controls
    }

    pub fn poll_main_controls(&mut self) -> bool {
        match self.rpc(CtrlReq::PollMainControls) {
            Some(CtrlResp::MainControlsPoll { changed, controls }) => {
                if let Some(mc) = controls {
                    self.main_controls = mc;
                }
                changed
            }
            _ => false,
        }
    }

    pub fn param_desc(&mut self, id: u32) -> Option<ParamDesc> {
        match self.rpc(CtrlReq::ParamDesc { id }) {
            Some(CtrlResp::OptParam(p)) => p,
            _ => None,
        }
    }

    pub fn param_value(&mut self, id: u32) -> Option<f64> {
        match self.rpc(CtrlReq::ParamValue { id }) {
            Some(CtrlResp::OptF64(v)) => v,
            _ => None,
        }
    }

    pub fn param_text(&mut self, id: u32, value: f64) -> String {
        match self.rpc(CtrlReq::ParamText { id, value }) {
            Some(CtrlResp::Text(t)) => t,
            _ => format!("{value:.3}"),
        }
    }

    pub fn param_text_opt(&mut self, id: u32, value: f64) -> Option<String> {
        match self.rpc(CtrlReq::ParamTextOpt { id, value }) {
            Some(CtrlResp::OptText(t)) => t,
            _ => None,
        }
    }

    pub fn param_text_to_value(&mut self, id: u32, text: &str) -> Option<f64> {
        match self.rpc(CtrlReq::ParamTextToValue { id, text: text.to_string() }) {
            Some(CtrlResp::OptF64(v)) => v,
            _ => None,
        }
    }

    pub fn param_options(&mut self, p: &ParamDesc) -> Vec<(f64, String)> {
        match self.rpc(CtrlReq::ParamOptions { param: p.clone() }) {
            Some(CtrlResp::Options(o)) => o,
            _ => Vec::new(),
        }
    }

    pub fn resolve_param_text(&mut self, p: &ParamDesc, text: &str) -> Option<f64> {
        match self.rpc(CtrlReq::ResolveParamText { param: p.clone(), text: text.to_string() }) {
            Some(CtrlResp::OptF64(v)) => v,
            _ => None,
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(mut ctrl) = self.ctrl.take() {
            let _ = send_msg(&mut ctrl, &CtrlReq::Shutdown);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        while self.retire_rx.pop().is_ok() {}
    }
}

impl Drop for SandboxMain {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// CLI self-test & IPC latency benchmark (`kabelsalat sandbox-test`)
// ---------------------------------------------------------------------------

/// Measure median / p95 / max round-trip IPC hop time in microseconds for a
/// sandboxed worker at `block_frames`.
pub fn measure_ipc_roundtrip(block_frames: usize, iters: usize) -> Result<(f64, f64, f64)> {
    let (mut voice, mut main) = spawn_sandboxed_clap(48000.0, SANDBOX_TEST_PLUGIN_ID, None)?;
    voice.note_on(69, 0.8);
    let mut buf = vec![0.0f32; block_frames * 2];
    let t = Transport::default();
    // Warm up 16 blocks.
    for _ in 0..16 {
        voice.render(&mut buf, t);
    }
    let mut times_us = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        voice.render(&mut buf, t);
        times_us.push(t0.elapsed().as_nanos() as f64 / 1000.0);
    }
    main.shutdown();
    times_us.sort_by(|a, b| a.total_cmp(b));
    let med = times_us[times_us.len() / 2];
    let p95 = times_us[(times_us.len() * 95) / 100];
    let max = *times_us.last().unwrap_or(&0.0);
    Ok((med, p95, max))
}

/// `kabelsalat sandbox-test`:
/// 1. Runs two sandboxed rigs side by side, sends `kill -SEGV` to Rig 1's
///    worker while both play, verifies Rig 2 keeps sounding uninterrupted,
///    Rig 1 outputs clean silence, and Rig 1 automatically restarts with its
///    saved state and resumes sounding.
/// 2. Measures the IPC hop round-trip cost at 64 frames and 256 frames.
pub fn run_sandbox_test() -> Result<()> {
    let sr = 48000.0;
    let (mut v1, mut m1) = spawn_sandboxed_clap(sr, SANDBOX_TEST_PLUGIN_ID, None)?;
    let (mut v2, mut m2) = spawn_sandboxed_clap(sr, SANDBOX_TEST_PLUGIN_ID, None)?;

    // Give Rig 1 a distinct saved state (gain = 0.75, A4 = 440 Hz, held note 69)
    // so we prove state restoration after crash.
    v1.note_on(69, 0.9);
    v2.note_on(60, 0.8);
    let mut b1 = [0.0f32; 256 * 2];
    let mut b2 = [0.0f32; 256 * 2];
    let t = Transport::default();
    v1.render(&mut b1, t);
    v2.render(&mut b2, t);
    let saved = m1.save_state()?;
    anyhow::ensure!(!saved.is_empty(), "Rig 1 state should be non-empty");

    let peak = |buf: &[f32]| buf.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    anyhow::ensure!(peak(&b1) > 0.1, "Rig 1 must sound before crash (peak={})", peak(&b1));
    anyhow::ensure!(peak(&b2) > 0.1, "Rig 2 must sound before crash (peak={})", peak(&b2));

    let pid1 = m1.worker_pid();
    println!("sandbox-test: killing Rig 1 worker pid={pid1} with SIGSEGV while Rig 2 plays...");
    unsafe { libc::kill(pid1 as libc::pid_t, libc::SIGSEGV); }

    // Wait briefly for kernel to deliver SIGSEGV and reap via poll_recovery.
    let mut restarted_msg = None;
    for _ in 0..40 {
        v1.render(&mut b1, t);
        v2.render(&mut b2, t);
        anyhow::ensure!(
            peak(&b2) > 0.1,
            "Rig 2 dropped out while Rig 1 crashed! peak={}",
            peak(&b2)
        );
        if let Some(msg) = m1.poll_recovery() {
            restarted_msg = Some(msg);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let msg = restarted_msg.ok_or_else(|| anyhow!("Rig 1 did not detect SIGSEGV and restart"))?;
    println!("sandbox-test: {msg}");
    anyhow::ensure!(m1.worker_pid() != pid1, "new worker must have a new PID");
    anyhow::ensure!(m1.crash_count() == 1, "crash_count should be 1");

    // Next render block adopts the restarted worker with its saved state
    // (which includes the held note) and immediately sounds again.
    v1.render(&mut b1, t);
    v2.render(&mut b2, t);
    anyhow::ensure!(peak(&b1) > 0.1, "Rig 1 must sound again after restart (peak={})", peak(&b1));
    anyhow::ensure!(peak(&b2) > 0.1, "Rig 2 must remain sounding (peak={})", peak(&b2));

    m1.shutdown();
    m2.shutdown();

    let (med64, p95_64, max64) = measure_ipc_roundtrip(64, 500)?;
    let (med256, p95_256, max256) = measure_ipc_roundtrip(256, 500)?;
    println!(
        "sandbox-test: IPC round-trip (500 blocks): 64f med={med64:.1}µs p95={p95_64:.1}µs max={max64:.1}µs | 256f med={med256:.1}µs p95={p95_256:.1}µs max={max256:.1}µs"
    );
    println!("sandbox-test: PASS");
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests (including real multi-process SIGSEGV recovery)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[test]
#[ignore = "internal test-binary plugin-worker entry point"]
fn worker_test_entry() {
    let shm_fd: RawFd = std::env::var("KABELSALAT_TEST_WORKER_SHM_FD")
        .expect("KABELSALAT_TEST_WORKER_SHM_FD")
        .parse()
        .expect("valid shm fd");
    let ctrl_fd: RawFd = std::env::var("KABELSALAT_TEST_WORKER_CTRL_FD")
        .expect("KABELSALAT_TEST_WORKER_CTRL_FD")
        .parse()
        .expect("valid ctrl fd");
    run_plugin_worker(shm_fd, ctrl_fd).expect("plugin-worker exited cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segv_silences_one_strip_leaves_other_rig_playing_and_restarts_with_state() {
        run_sandbox_test().expect("sandbox-test must pass");
    }

    #[test]
    fn sandbox_param_rpc_and_state_roundtrip() {
        let (mut voice, mut main) = spawn_sandboxed_clap(48000.0, SANDBOX_TEST_PLUGIN_ID, None).unwrap();
        let params = main.param_list();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "Gain");
        assert!((main.param_value(0).unwrap() - 0.4).abs() < 1e-4);

        // Set gain via RT command and verify state captures it.
        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.75));
        let mut buf = [0.0f32; 128 * 2];
        voice.render(&mut buf, Transport::default());
        assert!((main.param_value(0).unwrap() - 0.75).abs() < 1e-4);

        let blob = main.save_state().unwrap();
        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.1));
        voice.render(&mut buf, Transport::default());
        assert!((main.param_value(0).unwrap() - 0.1).abs() < 1e-4);

        main.load_state(&blob).unwrap();
        assert!((main.param_value(0).unwrap() - 0.75).abs() < 1e-4);
        main.shutdown();
    }
}
