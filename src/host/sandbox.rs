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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clack_extensions::tail::TailLength;
use clack_host::process::ProcessStatus;
use clack_host::utils::ClapId;
use serde::{Deserialize, Serialize};

use super::{ClapVoice, ClapVoiceMain, ParamDesc, VoiceCmd};
use crate::lv2::{FxOutEvent, Lv2ControlDesc, Lv2Plugin, Lv2Preset};
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
    param_edits_pending: AtomicU32,
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

/// Owned mapping of a `#[repr(C)]` shared-memory struct backed by `memfd_create`.
/// Safe to move across threads (`Send`); must only be dropped on a non-RT thread
/// because `Drop` calls `munmap` and `close`.
pub(crate) struct ShmRegion<T = ShmBlock> {
    fd: RawFd,
    ptr: *mut T,
    len: usize,
}

// SAFETY: `ShmRegion<T>` exclusively owns a process-mapped shared memory region
// whose fields follow the strict host/worker sequence protocol.
unsafe impl<T> Send for ShmRegion<T> {}

impl<T> ShmRegion<T> {
    fn create_raw(name: &[u8]) -> Result<Self> {
        let len = std::mem::size_of::<T>();
        let fd = unsafe {
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
        Self::map_fd(fd, len)
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
        Ok(Self { fd, ptr: ptr as *mut T, len })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd
    }

    #[inline]
    fn block(&self) -> &T {
        unsafe { &*self.ptr }
    }

    #[inline]
    fn block_mut(&self) -> &mut T {
        unsafe { &mut *self.ptr }
    }
}

impl<T> Drop for ShmRegion<T> {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

impl ShmRegion<ShmBlock> {
    fn create(sample_rate: f64) -> Result<Self> {
        let region = Self::create_raw(b"kabelsalat-plugin-shm\0")?;
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
        blk.param_edits_pending.store(0, Ordering::Relaxed);
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
}

const LV2_SHM_MAGIC: u32 = 0x4B53_4C32; // "KSL2"
const LV2_SHM_VERSION: u32 = 1;
pub const MAX_LV2_SHM_FRAMES: usize = 512;
const MAX_LV2_CONTROLS: usize = 512;
const MAX_LV2_MIDI_EVENTS: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Lv2ShmMidiEvent {
    len: u8,
    bytes: [u8; 3],
}

#[repr(C, align(64))]
struct Lv2ShmBlock {
    magic: u32,
    version: u32,
    host_seq: AtomicU32,
    worker_seq: AtomicU32,
    worker_alive: AtomicU32,
    worker_pid: AtomicU32,
    latency_frames: AtomicU32,
    pending_worker_resp: AtomicU32,
    sample_rate: f64,
    frames: u32,
    is_midi_fx: u32,
    control_count: u32,
    controls_dirty: u32,
    controls: [f32; MAX_LV2_CONTROLS],
    midi_in_count: u32,
    midi_in: [Lv2ShmMidiEvent; MAX_LV2_MIDI_EVENTS],
    midi_out_count: u32,
    midi_out: [Lv2ShmMidiEvent; MAX_LV2_MIDI_EVENTS],
    in_buf: [f32; MAX_LV2_SHM_FRAMES * 2],
    out_buf: [f32; MAX_LV2_SHM_FRAMES * 2],
}

type Lv2ShmRegion = ShmRegion<Lv2ShmBlock>;

impl Lv2ShmRegion {
    fn create(sample_rate: f64, is_midi_fx: bool) -> Result<Self> {
        let region = Self::create_raw(b"kabelsalat-lv2-shm\0")?;
        let blk = region.block_mut();
        blk.magic = LV2_SHM_MAGIC;
        blk.version = LV2_SHM_VERSION;
        blk.host_seq.store(0, Ordering::Relaxed);
        blk.worker_seq.store(0, Ordering::Relaxed);
        blk.worker_alive.store(1, Ordering::Relaxed);
        blk.worker_pid.store(0, Ordering::Relaxed);
        blk.latency_frames.store(0, Ordering::Relaxed);
        blk.pending_worker_resp.store(0, Ordering::Relaxed);
        blk.sample_rate = sample_rate;
        blk.is_midi_fx = u32::from(is_midi_fx);
        blk.controls_dirty = 0;
        blk.midi_in_count = 0;
        blk.midi_out_count = 0;
        Ok(region)
    }

    fn from_inherited_fd(fd: RawFd) -> Result<Self> {
        let len = std::mem::size_of::<Lv2ShmBlock>();
        let region = Self::map_fd(fd, len)?;
        let blk = region.block();
        if blk.magic != LV2_SHM_MAGIC || blk.version != LV2_SHM_VERSION {
            bail!("invalid lv2 shm header: magic={:#x} ver={}", blk.magic, blk.version);
        }
        Ok(region)
    }
}

#[inline]
fn wait_for_worker_seq(
    worker_seq: &AtomicU32,
    worker_alive: &AtomicU32,
    prev_seq: u32,
    expected_seq: u32,
    first_block: &mut bool,
    deadline_ns: u64,
) -> bool {
    let mut ready = worker_seq.load(Ordering::Acquire) == expected_seq;
    if !ready {
        for _ in 0..64 {
            std::hint::spin_loop();
            if worker_seq.load(Ordering::Acquire) == expected_seq {
                ready = true;
                break;
            }
        }
    }
    let wait_ns = if *first_block {
        *first_block = false;
        FIRST_BLOCK_DEADLINE_US * 1_000
    } else {
        deadline_ns
    };
    if !ready {
        let start = Instant::now();
        while worker_seq.load(Ordering::Acquire) != expected_seq {
            if worker_alive.load(Ordering::Acquire) == 0 {
                break;
            }
            let elapsed_ns = start.elapsed().as_nanos() as u64;
            if elapsed_ns >= wait_ns {
                break;
            }
            futex_wait_timeout(worker_seq, prev_seq, wait_ns - elapsed_ns);
        }
        ready = worker_seq.load(Ordering::Acquire) == expected_seq;
    }
    ready
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
    InitLv2 {
        uri: String,
        sample_rate: f64,
        block: usize,
        state: Option<(String, String, Vec<u8>)>,
        controls: Vec<(usize, f32)>,
        #[serde(default)]
        bundle_path: Option<String>,
    },
    Lv2SaveState {
        key: String,
    },
    Lv2OpenEditor {
        title: String,
    },
    Lv2CloseEditor,
    Lv2EditorPoll,
    Lv2SavePreset {
        name: String,
    },
    Lv2ApplyPreset {
        preset: Lv2Preset,
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
    DrainParamEdits,
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
    InitLv2Ok {
        uri: String,
        is_instrument: bool,
        is_midi_fx: bool,
        reports_latency: bool,
        has_midi_in: bool,
        latency: u32,
        controls: Vec<Lv2ControlDesc>,
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
    ParamEdits(Vec<(u32, f64)>),
    Lv2EditorStatus {
        open: bool,
        control_writes: Vec<(usize, f32, f32)>,
        midi_events: Vec<[u8; 3]>,
    },
    Lv2PresetSaved(Lv2Preset),
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
    pending_edits: Vec<(u32, f64)>,
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
            pending_edits: Vec::new(),
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
        #[cfg(target_os = "linux")]
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let mut ctrl = unsafe { UnixStream::from_raw_fd(ctrl_fd) };

    // Read the first control message BEFORE mapping `shm_fd` because CLAP/VST3
    // and LV2 use distinct shared-memory layouts (`ShmBlock` vs `Lv2ShmBlock`).
    let (plugin_id, sample_rate, init_state) = match recv_msg::<CtrlReq>(&mut ctrl)? {
        CtrlReq::Init { plugin_id, sample_rate, state } => (plugin_id, sample_rate, state),
        CtrlReq::InitLv2 { uri, sample_rate, block, state, controls, bundle_path } => {
            return run_lv2_worker(shm_fd, ctrl, uri, sample_rate, block, state, controls, bundle_path);
        }
        _ => {
            unsafe { libc::close(shm_fd); }
            bail!("expected Init or InitLv2 as first plugin-worker request");
        }
    };

    let shm = ShmRegion::<ShmBlock>::from_inherited_fd(shm_fd)?;
    shm.block().worker_pid.store(std::process::id(), Ordering::Release);
    shm.block().worker_alive.store(1, Ordering::Release);

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
    let mut pending_edits: Vec<(u32, f64)> = Vec::new();
    let pump_due = |worker_main: &mut WorkerMain,
                    pending_edits: &mut Vec<(u32, f64)>,
                    shm: &ShmRegion,
                    last_pump: &mut Instant| {
        if last_pump.elapsed() >= Duration::from_millis(16) {
            if let WorkerMain::Clap(m) = worker_main {
                m.pump();
                let lat = m.poll_latency();
                shm.block().latency_frames.store(lat, Ordering::Release);
            }
            collect_worker_edits(worker_main, pending_edits, shm);
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
                let resp = handle_worker_req(&mut worker_main, req, &mut pending_edits, &shm);
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
                pump_due(&mut worker_main, &mut pending_edits, &shm, &mut last_pump);
            }
            Err(e) => {
                if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
                    if matches!(ioe.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
                        pump_due(&mut worker_main, &mut pending_edits, &shm, &mut last_pump);
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
                if voice.take_out_param_dirty() {
                    blk.param_edits_pending.store(1, Ordering::Release);
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

fn collect_worker_edits(
    worker_main: &mut WorkerMain,
    pending: &mut Vec<(u32, f64)>,
    shm: &ShmRegion,
) {
    let edits = match worker_main {
        WorkerMain::Clap(m) => m.drain_param_edits(),
        WorkerMain::TestSynth(shared) => {
            let mut st = shared.lock().unwrap();
            std::mem::take(&mut st.pending_edits)
        }
    };
    if !edits.is_empty() {
        for (id, val) in edits {
            if pending.len() >= 512 {
                pending.remove(0);
            }
            pending.push((id, val));
        }
        shm.block().param_edits_pending.store(1, Ordering::Release);
    }
}

fn handle_worker_req(
    main: &mut WorkerMain,
    req: CtrlReq,
    pending_edits: &mut Vec<(u32, f64)>,
    shm: &ShmRegion,
) -> CtrlResp {
    if let CtrlReq::DrainParamEdits = req {
        collect_worker_edits(main, pending_edits, shm);
        shm.block().param_edits_pending.store(0, Ordering::Release);
        return CtrlResp::ParamEdits(std::mem::take(pending_edits));
    }
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
            CtrlReq::DrainParamEdits => CtrlResp::ParamEdits(Vec::new()),
            CtrlReq::Init { .. } | CtrlReq::InitLv2 { .. } | CtrlReq::Shutdown => CtrlResp::Ok,
            CtrlReq::Lv2SaveState { .. }
            | CtrlReq::Lv2OpenEditor { .. }
            | CtrlReq::Lv2CloseEditor
            | CtrlReq::Lv2EditorPoll
            | CtrlReq::Lv2SavePreset { .. }
            | CtrlReq::Lv2ApplyPreset { .. } => CtrlResp::Err("unexpected LV2 request on CLAP worker".into()),
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
                CtrlReq::CloseEditor
                | CtrlReq::Pump
                | CtrlReq::Init { .. }
                | CtrlReq::InitLv2 { .. }
                | CtrlReq::Shutdown => CtrlResp::Ok,
                CtrlReq::Lv2SaveState { .. }
                | CtrlReq::Lv2OpenEditor { .. }
                | CtrlReq::Lv2CloseEditor
                | CtrlReq::Lv2EditorPoll
                | CtrlReq::Lv2SavePreset { .. }
                | CtrlReq::Lv2ApplyPreset { .. } => CtrlResp::Err("unexpected LV2 request on test worker".into()),
                CtrlReq::DrainParamEdits => CtrlResp::ParamEdits(Vec::new()),
                CtrlReq::EditorOpen => CtrlResp::Bool(false),
            }
        }
    }
}

fn run_lv2_worker(
    shm_fd: RawFd,
    mut ctrl: UnixStream,
    uri: String,
    sample_rate: f64,
    block: usize,
    init_state: Option<(String, String, Vec<u8>)>,
    init_controls: Vec<(usize, f32)>,
    bundle_path: Option<String>,
) -> Result<()> {
    crate::lv2::load_extra_bundles_from_env();
    let shm = Lv2ShmRegion::from_inherited_fd(shm_fd)?;
    shm.block().worker_pid.store(std::process::id(), Ordering::Release);
    shm.block().worker_alive.store(1, Ordering::Release);

    let mut plugin = match Lv2Plugin::load_with_bundle(
        &uri,
        sample_rate,
        block.max(MAX_LV2_SHM_FRAMES),
        bundle_path.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            let _ = send_msg(&mut ctrl, &CtrlResp::Err(format!("{e:#}")));
            shm.block().worker_alive.store(0, Ordering::Release);
            return Err(e);
        }
    };

    if let Some((key, kind, data)) = &init_state {
        if let Err(e) = plugin.restore_state(&[(key.as_str(), kind.as_str(), data.as_slice())]) {
            let _ = send_msg(&mut ctrl, &CtrlResp::Err(format!("{e:#}")));
            shm.block().worker_alive.store(0, Ordering::Release);
            std::mem::forget(plugin);
            return Err(e);
        }
    }
    for (nth, val) in init_controls {
        plugin.set_control_native(nth, val);
    }
    if !plugin.is_midi_fx() {
        let warm_in = [0.0f32; 256 * 2];
        let mut warm_out = [0.0f32; 256 * 2];
        plugin.process(&warm_in, &mut warm_out, 256);
        plugin.process(&warm_in, &mut warm_out, 256);
    }

    let resolved_uri = plugin.uri().to_string();
    let plugin_name = plugin.name().to_string();
    let is_instrument = plugin.is_instrument();
    let is_midi_fx = plugin.is_midi_fx();
    let reports_latency = plugin.reports_latency();
    let has_midi_in = plugin.has_midi_in();
    let latency = plugin.latency_frames();
    let controls = plugin.control_descs();
    let raw_handle_addr = plugin.raw_handle() as usize;
    let raw_desc_addr = plugin.raw_descriptor() as usize;
    let shared_urid = Arc::clone(plugin.shared_urid_map());
    let port_symbols = plugin.port_symbols().to_vec();
    let control_out_ports = plugin.control_output_ports().to_vec();

    let sym_to_port: HashMap<&str, u32> = port_symbols
        .iter()
        .map(|&(idx, ref sym)| (sym.as_str(), idx as u32))
        .collect();
    let ctrl_in_ports: Vec<(usize, u32)> = controls
        .iter()
        .filter_map(|c| sym_to_port.get(c.symbol.as_str()).map(|&p| (c.index, p)))
        .collect();
    let ctrl_in_shadow = Arc::new(
        controls
            .iter()
            .map(|c| AtomicU32::new(c.value.to_bits()))
            .collect::<Vec<_>>(),
    );
    let ctrl_out_shadow = Arc::new(
        control_out_ports
            .iter()
            .map(|&p| (p as u32, AtomicU32::new(0.0f32.to_bits())))
            .collect::<Vec<_>>(),
    );
    let ui_active = Arc::new(AtomicBool::new(false));
    let (mut ui_ctrl_tx, ui_ctrl_rx) = rtrb::RingBuffer::<(usize, f32)>::new(256);
    let (mut ui_atom_tx, ui_atom_rx) = rtrb::RingBuffer::<(u32, Vec<u8>)>::new(256);
    let (dsp_atom_tx, mut dsp_atom_rx) = rtrb::RingBuffer::<(u32, Vec<u8>)>::new(256);

    {
        let blk = shm.block_mut();
        blk.is_midi_fx = u32::from(is_midi_fx);
        blk.latency_frames.store(latency, Ordering::Release);
        blk.pending_worker_resp
            .store(u32::from(plugin.has_pending_worker_response()), Ordering::Release);
        let cc = controls.len().min(MAX_LV2_CONTROLS);
        blk.control_count = cc as u32;
        for (i, d) in controls.iter().take(cc).enumerate() {
            blk.controls[i] = d.value;
        }
    }

    let plugin = Arc::new(std::sync::Mutex::new(plugin));
    let stop_audio = Arc::new(AtomicBool::new(false));
    let audio_ready = Arc::new(AtomicBool::new(false));
    let stop_bg = stop_audio.clone();
    let ready_bg = audio_ready.clone();
    let plugin_bg = plugin.clone();
    let ui_active_bg = ui_active.clone();
    let ctrl_in_bg = ctrl_in_shadow.clone();
    let ctrl_out_bg = ctrl_out_shadow.clone();
    let shm_ptr = shm.ptr as usize;
    let audio_handle = std::thread::Builder::new()
        .name("kabelsalat-worker-audio".into())
        .spawn(move || {
            let blk = unsafe { &mut *(shm_ptr as *mut Lv2ShmBlock) };
            ready_bg.store(true, Ordering::Release);
            lv2_worker_audio_loop(
                plugin_bg,
                blk,
                &stop_bg,
                &ui_active_bg,
                ui_ctrl_rx,
                ui_atom_rx,
                dsp_atom_tx,
                &ctrl_in_bg,
                &ctrl_out_bg,
            );
        })
        .context("spawn lv2 worker audio thread")?;
    while !audio_ready.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }

    send_msg(
        &mut ctrl,
        &CtrlResp::InitLv2Ok {
            uri: resolved_uri.clone(),
            is_instrument,
            is_midi_fx,
            reports_latency,
            has_midi_in,
            latency,
            controls: controls.clone(),
        },
    )?;

    ctrl.set_read_timeout(Some(Duration::from_millis(16)))?;
    let mut editor: Option<crate::lv2::ui::Lv2UiHandle> = None;
    let mut pending_ctrl_writes: Vec<(usize, f32, f32)> = Vec::new();
    let mut pending_midi_events: Vec<[u8; 3]> = Vec::new();
    let mut last_pump = Instant::now();



    loop {
        match recv_msg::<CtrlReq>(&mut ctrl) {
            Ok(CtrlReq::Shutdown) => {
                if let Some(mut h) = editor.take() {
                    h.close();
                }
                ui_active.store(false, Ordering::Release);
                let _ = send_msg(&mut ctrl, &CtrlResp::Ok);
                break;
            }
            Ok(CtrlReq::Lv2SaveState { key }) => {
                let mut guard = plugin.lock().unwrap_or_else(|e| e.into_inner());
                let resp = match guard.saved_state(&key) {
                    Ok(bytes) => CtrlResp::State(bytes),
                    Err(e) => CtrlResp::Err(format!("{e:#}")),
                };
                drop(guard);
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
            }
            Ok(CtrlReq::Lv2OpenEditor { title }) => {
                if let Some(mut h) = editor.take() {
                    h.close();
                }
                while dsp_atom_rx.pop().is_ok() {}
                pending_ctrl_writes.clear();
                pending_midi_events.clear();
                let resp = match crate::lv2::discover_ui_for_uri(&resolved_uri) {
                    Some(info) => {
                        let initial: Vec<(u32, f32)> = ctrl_in_ports
                            .iter()
                            .filter_map(|&(nth, port_idx)| {
                                ctrl_in_shadow
                                    .get(nth)
                                    .map(|a| (port_idx, f32::from_bits(a.load(Ordering::Acquire))))
                            })
                            .collect();
                        let syms_u32: Vec<(u32, String)> = port_symbols
                            .iter()
                            .map(|&(idx, ref sym)| (idx as u32, sym.clone()))
                            .collect();
                        let direct = crate::lv2::ui::Lv2DirectAccess {
                            instance: raw_handle_addr as crate::lv2::Lv2Handle,
                            descriptor: raw_desc_addr as *const crate::lv2::Lv2Descriptor,
                        };
                        let win_title = if title.is_empty() {
                            format!("{plugin_name} · kabelsalat")
                        } else {
                            title
                        };
                        match crate::lv2::ui::Lv2UiHandle::open_cohosted(
                            &resolved_uri,
                            &win_title,
                            sample_rate,
                            &initial,
                            &controls,
                            &syms_u32,
                            &info,
                            Arc::clone(&shared_urid),
                            Some(direct),
                        ) {
                            Ok(h) => {
                                editor = Some(h);
                                ui_active.store(true, Ordering::Release);
                                pump_lv2_editor(
    &mut editor,
    &mut pending_ctrl_writes,
    &mut pending_midi_events,
    &ctrl_in_ports,
    &ctrl_in_shadow,
    &ctrl_out_shadow,
    &ui_active,
    &mut dsp_atom_rx,
    &mut ui_ctrl_tx,
    &mut ui_atom_tx,
);
                                CtrlResp::Ok
                            }
                            Err(e) => CtrlResp::Err(format!("{e:#}")),
                        }
                    }
                    None => CtrlResp::Err(format!("{} has no hostable native GUI", resolved_uri)),
                };
                last_pump = Instant::now();
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
            }
            Ok(CtrlReq::Lv2CloseEditor) => {
                if let Some(mut h) = editor.take() {
                    h.close();
                }
                ui_active.store(false, Ordering::Release);
                if send_msg(&mut ctrl, &CtrlResp::Ok).is_err() {
                    break;
                }
            }
            Ok(CtrlReq::Lv2EditorPoll) => {
                pump_lv2_editor(
    &mut editor,
    &mut pending_ctrl_writes,
    &mut pending_midi_events,
    &ctrl_in_ports,
    &ctrl_in_shadow,
    &ctrl_out_shadow,
    &ui_active,
    &mut dsp_atom_rx,
    &mut ui_ctrl_tx,
    &mut ui_atom_tx,
);
                last_pump = Instant::now();
                let resp = CtrlResp::Lv2EditorStatus {
                    open: editor.is_some(),
                    control_writes: std::mem::take(&mut pending_ctrl_writes),
                    midi_events: std::mem::take(&mut pending_midi_events),
                };
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
            }
            Ok(CtrlReq::Lv2SavePreset { name }) => {
                let mut guard = plugin.lock().unwrap_or_else(|e| e.into_inner());
                let resp = match guard.save_preset_bundle(&name) {
                    Ok(preset) => CtrlResp::Lv2PresetSaved(preset),
                    Err(e) => CtrlResp::Err(format!("{e:#}")),
                };
                drop(guard);
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
            }
            Ok(CtrlReq::Lv2ApplyPreset { preset }) => {
                let mut guard = plugin.lock().unwrap_or_else(|e| e.into_inner());
                let resp = match guard.apply_preset_full(&preset) {
                    Ok(()) => {
                        for (i, d) in guard.control_descs().iter().enumerate() {
                            if let Some(a) = ctrl_in_shadow.get(i) {
                                a.store(d.value.to_bits(), Ordering::Release);
                            }
                        }
                        CtrlResp::Ok
                    }
                    Err(e) => CtrlResp::Err(format!("{e:#}")),
                };
                drop(guard);
                if send_msg(&mut ctrl, &resp).is_err() {
                    break;
                }
            }
            Ok(_) => {
                if send_msg(&mut ctrl, &CtrlResp::Ok).is_err() {
                    break;
                }
            }
            Err(e) => {
                if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
                    if matches!(ioe.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
                        if editor.is_some() && last_pump.elapsed() >= Duration::from_millis(16) {
                            pump_lv2_editor(
    &mut editor,
    &mut pending_ctrl_writes,
    &mut pending_midi_events,
    &ctrl_in_ports,
    &ctrl_in_shadow,
    &ctrl_out_shadow,
    &ui_active,
    &mut dsp_atom_rx,
    &mut ui_ctrl_tx,
    &mut ui_atom_tx,
);
                            last_pump = Instant::now();
                        }
                        continue;
                    }
                }
                break;
            }
        }
        if editor.is_some() && last_pump.elapsed() >= Duration::from_millis(16) {
            pump_lv2_editor(
    &mut editor,
    &mut pending_ctrl_writes,
    &mut pending_midi_events,
    &ctrl_in_ports,
    &ctrl_in_shadow,
    &ctrl_out_shadow,
    &ui_active,
    &mut dsp_atom_rx,
    &mut ui_ctrl_tx,
    &mut ui_atom_tx,
);
            last_pump = Instant::now();
        }
    }

    if let Some(mut h) = editor.take() {
        h.close();
    }
    ui_active.store(false, Ordering::Release);
    stop_audio.store(true, Ordering::Release);
    shm.block().worker_alive.store(0, Ordering::Release);
    futex_wake(&shm.block().host_seq);
    let _ = audio_handle.join();
    // Never run C++ plugin destructors on worker exit (`NEVER_FREE`, rule 56).
    std::mem::forget(plugin);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn pump_lv2_editor(
    editor: &mut Option<crate::lv2::ui::Lv2UiHandle>,
    pending_ctrl: &mut Vec<(usize, f32, f32)>,
    pending_midi: &mut Vec<[u8; 3]>,
    ctrl_in_ports: &[(usize, u32)],
    ctrl_in_shadow: &[AtomicU32],
    ctrl_out_shadow: &[(u32, AtomicU32)],
    ui_active: &AtomicBool,
    dsp_atom_rx: &mut rtrb::Consumer<(u32, Vec<u8>)>,
    ui_ctrl_tx: &mut rtrb::Producer<(usize, f32)>,
    ui_atom_tx: &mut rtrb::Producer<(u32, Vec<u8>)>,
) {
    let Some(ref mut ed) = editor else {
        ui_active.store(false, Ordering::Release);
        return;
    };
    for &(nth, port_idx) in ctrl_in_ports {
        if let Some(a) = ctrl_in_shadow.get(nth) {
            ed.notify_control(port_idx, f32::from_bits(a.load(Ordering::Acquire)));
        }
    }
    for &(port_idx, ref a) in ctrl_out_shadow {
        ed.notify_control(port_idx, f32::from_bits(a.load(Ordering::Acquire)));
    }
    while let Ok((port_idx, atom_bytes)) = dsp_atom_rx.pop() {
        ed.notify_atom(port_idx, &atom_bytes);
    }
    let res = ed.pump();
    for &(nth, native, _) in &res.control_writes {
        if let Some(a) = ctrl_in_shadow.get(nth) {
            a.store(native.to_bits(), Ordering::Release);
        }
        let _ = ui_ctrl_tx.push((nth, native));
    }
    for (port_idx, atom_bytes) in res.atom_writes {
        let _ = ui_atom_tx.push((port_idx, atom_bytes));
    }
    pending_ctrl.extend(res.control_writes);
    pending_midi.extend(res.midi_events);
    if res.closed {
        if let Some(mut h) = editor.take() {
            h.close();
        }
        ui_active.store(false, Ordering::Release);
    }
}

#[allow(clippy::too_many_arguments)]
fn lv2_worker_audio_loop(
    plugin: Arc<std::sync::Mutex<Lv2Plugin>>,
    blk: &mut Lv2ShmBlock,
    stop: &AtomicBool,
    ui_active: &AtomicBool,
    mut ui_ctrl_rx: rtrb::Consumer<(usize, f32)>,
    mut ui_atom_rx: rtrb::Consumer<(u32, Vec<u8>)>,
    mut dsp_atom_tx: rtrb::Producer<(u32, Vec<u8>)>,
    ctrl_in_shadow: &[AtomicU32],
    ctrl_out_shadow: &[(u32, AtomicU32)],
) {
    let mut last_seq = 0u32;
    let mut out_tmp = [0.0f32; MAX_LV2_SHM_FRAMES * 2];
    let mut in_tmp = [0.0f32; MAX_LV2_SHM_FRAMES * 2];
    let mut fx_out = Vec::with_capacity(MAX_LV2_MIDI_EVENTS);
    let mut ctrl_out_tmp = Vec::with_capacity(ctrl_out_shadow.len().max(8));
    let mut atom_out_tmp = Vec::with_capacity(32);

    while !stop.load(Ordering::Acquire) && blk.worker_alive.load(Ordering::Acquire) != 0 {
        let mut seq = blk.host_seq.load(Ordering::Acquire);
        if seq == last_seq {
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

        let frames = (blk.frames as usize).min(MAX_LV2_SHM_FRAMES);
        let n2 = frames * 2;
        let is_midi_fx = blk.is_midi_fx != 0;
        if !is_midi_fx && n2 > 0 {
            in_tmp[..n2].copy_from_slice(&blk.in_buf[..n2]);
        }

        let mut guard = plugin.lock().unwrap_or_else(|e| e.into_inner());
        if blk.controls_dirty != 0 {
            let cc = (blk.control_count as usize).min(MAX_LV2_CONTROLS);
            for i in 0..cc {
                let val = blk.controls[i];
                guard.set_control_native(i, val);
                if let Some(a) = ctrl_in_shadow.get(i) {
                    a.store(val.to_bits(), Ordering::Release);
                }
            }
            blk.controls_dirty = 0;
        }
        while let Ok((nth, val)) = ui_ctrl_rx.pop() {
            guard.set_control_native(nth, val);
            if let Some(a) = ctrl_in_shadow.get(nth) {
                a.store(val.to_bits(), Ordering::Release);
            }
        }
        while let Ok((port_idx, atom_bytes)) = ui_atom_rx.pop() {
            guard.push_atom_in_raw(port_idx as usize, &atom_bytes);
        }
        let mc = (blk.midi_in_count as usize).min(MAX_LV2_MIDI_EVENTS);
        for i in 0..mc {
            let ev = blk.midi_in[i];
            let len = (ev.len as usize).min(3);
            if len > 0 {
                guard.midi(&ev.bytes[..len]);
            }
        }
        if is_midi_fx {
            fx_out.clear();
            guard.process(&[], &mut [], frames);
            guard.read_midi_out(&mut fx_out);
            let out_n = fx_out.len().min(MAX_LV2_MIDI_EVENTS);
            blk.midi_out_count = out_n as u32;
            for (i, ev) in fx_out.iter().take(out_n).enumerate() {
                let bytes = match *ev {
                    FxOutEvent::NoteOn { channel, note, velocity } => [0x90 | (channel & 0x0F), note, velocity],
                    FxOutEvent::NoteOff { channel, note } => [0x80 | (channel & 0x0F), note, 0],
                    FxOutEvent::Midi(d) => d,
                };
                blk.midi_out[i] = Lv2ShmMidiEvent { len: 3, bytes };
            }
        } else {
            guard.process(&in_tmp[..n2], &mut out_tmp[..n2], frames);
            blk.out_buf[..n2].copy_from_slice(&out_tmp[..n2]);
            blk.midi_out_count = 0;
        }
        if ui_active.load(Ordering::Relaxed) {
            if !ctrl_out_shadow.is_empty() {
                guard.copy_control_outputs(&mut ctrl_out_tmp);
                for &(port_idx, val) in &ctrl_out_tmp {
                    if let Some((_, atom)) = ctrl_out_shadow.iter().find(|&&(p, _)| p as usize == port_idx) {
                        atom.store(val.to_bits(), Ordering::Release);
                    }
                }
            }
            guard.drain_atom_outs(&mut atom_out_tmp);
            for (port_idx, atom_bytes) in atom_out_tmp.drain(..) {
                let _ = dsp_atom_tx.push((port_idx, atom_bytes));
            }
        }
        blk.latency_frames.store(guard.latency_frames(), Ordering::Release);
        blk.pending_worker_resp
            .store(u32::from(guard.has_pending_worker_response()), Ordering::Release);
        drop(guard);

        blk.worker_seq.store(seq, Ordering::Release);
        futex_wake(&blk.worker_seq);
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
    param_edits_pending: Arc<AtomicBool>,
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
            self.param_edits_pending.store(false, Ordering::Release);
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

        let ready = wait_for_worker_seq(
            &blk.worker_seq,
            &blk.worker_alive,
            prev_seq,
            seq,
            &mut self.first_block,
            self.deadline_ns,
        );

        if ready {
            out[..n2].copy_from_slice(&blk.out_buf[..n2]);
            let lat = blk.latency_frames.load(Ordering::Acquire);
            self.latency.store(lat, Ordering::Relaxed);
            self.last_status = decode_status(blk.last_status.load(Ordering::Acquire));
            self.tail_length = decode_tail(blk.tail_frames.load(Ordering::Acquire));
            if blk.param_edits_pending.swap(0, Ordering::AcqRel) != 0 {
                self.param_edits_pending.store(true, Ordering::Release);
            }
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
    param_edits_pending: Arc<AtomicBool>,
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

fn spawn_worker_process(shm_fd: RawFd, child_fd: RawFd, label: &str) -> Result<Child> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = Command::new(&exe);
    // The worker searches the custom plugin folders its host searches (#414).
    cmd.env(crate::plugin_dirs::ENV, crate::plugin_dirs::env_value());
    if let Some(extra_vst3) = crate::vst3::scan::extra_bundles_env() {
        cmd.env("KABELSALAT_EXTRA_VST3_BUNDLES", extra_vst3);
    }
    if let Some(extra_lv2) = crate::lv2::extra_bundles_env() {
        cmd.env("KABELSALAT_EXTRA_LV2_BUNDLES", extra_lv2);
    }
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

    cmd.spawn().with_context(|| format!("spawn plugin-worker for {label}"))
}

fn spawn_worker(plugin_id: &str, sample_rate: f64, state: Option<Vec<u8>>) -> Result<SpawnedWorker> {
    let shm = ShmRegion::<ShmBlock>::create(sample_rate)?;
    let (mut parent_sock, child_sock) = UnixStream::pair().context("UnixStream::pair")?;
    let mut child = spawn_worker_process(shm.raw_fd(), child_sock.as_raw_fd(), plugin_id)?;
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
    let param_edits_pending = Arc::new(AtomicBool::new(false));

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
        param_edits_pending: param_edits_pending.clone(),
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
        param_edits_pending,
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

        let died_reason = detect_worker_death(
            &mut self.child,
            &mut self.ctrl,
            &self.crashed,
            &mut self.last_crash_reason,
            Some(&mut self.editor_open),
            self.swap_tx.slots() == 4,
        );
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

    pub fn has_pending_param_edits(&self) -> bool {
        self.param_edits_pending.load(Ordering::Acquire)
    }

    pub fn drain_param_edits(&mut self) -> Vec<(u32, f64)> {
        self.param_edits_pending.store(false, Ordering::Release);
        match self.rpc(CtrlReq::DrainParamEdits) {
            Some(CtrlResp::ParamEdits(edits)) => edits,
            _ => Vec::new(),
        }
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

fn exit_reason(status: std::process::ExitStatus) -> String {
    if let Some(sig) = status.signal() {
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
    }
}

/// Detect whether `child` has died or `crashed` indicates a hung worker,
/// killing a hung child so a replacement can be spawned cleanly.
fn detect_worker_death(
    child_slot: &mut Option<Child>,
    ctrl_slot: &mut Option<UnixStream>,
    crashed: &AtomicBool,
    last_crash_reason: &mut Option<String>,
    clear_editor: Option<&mut bool>,
    hung_check_allowed: bool,
) -> Option<String> {
    let mut died_reason: Option<String> = None;
    let mut editor_open = clear_editor;
    if let Some(child) = child_slot.as_mut() {
        match child.try_wait() {
            Ok(Some(status)) => {
                died_reason = Some(exit_reason(status));
                *child_slot = None;
                *ctrl_slot = None;
                if let Some(ed) = editor_open.as_deref_mut() {
                    *ed = false;
                }
                crashed.store(true, Ordering::Release);
            }
            Ok(None) => {}
            Err(_) => {
                died_reason = Some("wait failed".into());
                *child_slot = None;
                *ctrl_slot = None;
            }
        }
    } else if crashed.load(Ordering::Acquire) {
        died_reason = last_crash_reason.take().or_else(|| Some("worker lost".into()));
    }

    if died_reason.is_none() && hung_check_allowed && crashed.load(Ordering::Acquire) {
        if let Some(mut child) = child_slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *ctrl_slot = None;
        if let Some(ed) = editor_open {
            *ed = false;
        }
        died_reason = Some("hung (deadline exceeded)".into());
    }
    died_reason
}

// ---------------------------------------------------------------------------
// Out-of-process LV2 hosting (#413)
// ---------------------------------------------------------------------------

static NEXT_LV2_SLOT_ID: AtomicU64 = AtomicU64::new(1);

struct SpawnedLv2Worker {
    child: Child,
    ctrl: UnixStream,
    shm: Lv2ShmRegion,
    uri: String,
    is_instrument: bool,
    is_midi_fx: bool,
    reports_latency: bool,
    has_midi_in: bool,
    latency: u32,
    controls: Vec<Lv2ControlDesc>,
    pid: u32,
}

fn spawn_lv2_worker(
    uri: &str,
    sample_rate: f64,
    block: usize,
    state: Option<(String, String, Vec<u8>)>,
    controls: Vec<(usize, f32)>,
) -> Result<SpawnedLv2Worker> {
    let shm = Lv2ShmRegion::create(sample_rate, false)?;
    let (mut parent_sock, child_sock) = UnixStream::pair().context("UnixStream::pair")?;
    let mut child = spawn_worker_process(shm.raw_fd(), child_sock.as_raw_fd(), uri)?;
    let pid = child.id();
    drop(child_sock);

    parent_sock.set_read_timeout(Some(Duration::from_secs(15)))?;
    parent_sock.set_write_timeout(Some(Duration::from_secs(5)))?;

    send_msg(
        &mut parent_sock,
        &CtrlReq::InitLv2 {
            uri: uri.to_string(),
            sample_rate,
            block,
            state,
            controls,
            bundle_path: crate::lv2::bundle_path_for_uri(uri),
        },
    )?;

    match recv_msg::<CtrlResp>(&mut parent_sock) {
        Ok(CtrlResp::InitLv2Ok {
            uri: resolved_uri,
            is_instrument,
            is_midi_fx,
            reports_latency,
            has_midi_in,
            latency,
            controls,
        }) => {
            parent_sock.set_read_timeout(Some(Duration::from_secs(3)))?;
            Ok(SpawnedLv2Worker {
                child,
                ctrl: parent_sock,
                shm,
                uri: resolved_uri,
                is_instrument,
                is_midi_fx,
                reports_latency,
                has_midi_in,
                latency,
                controls,
                pid,
            })
        }
        Ok(CtrlResp::Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("lv2 worker init failed for {uri}: {e}");
        }
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("unexpected lv2 worker init reply for {uri}");
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e).with_context(|| format!("lv2 worker handshake failed for {uri}"))
        }
    }
}

/// Audio-thread side of a sandboxed LV2 plugin (#413). Allocation-free and
/// lock-free; old shared-memory regions return to `SandboxLv2Main` over
/// `retire_tx` so the audio thread never calls `munmap`.
pub struct SandboxLv2Audio {
    slot_id: u64,
    uri: String,
    is_instrument: bool,
    is_midi_fx: bool,
    reports_latency: bool,
    has_midi_in: bool,
    shm: Lv2ShmRegion,
    current_gen: u64,
    swap_rx: rtrb::Consumer<(u64, Lv2ShmRegion)>,
    retire_tx: rtrb::Producer<Lv2ShmRegion>,
    controls_meta: Vec<Lv2ControlDesc>,
    controls_by_symbol: HashMap<String, usize>,
    controls_shadow: Arc<Vec<AtomicU32>>,
    controls_dirty: bool,
    midi_in_q: Vec<Lv2ShmMidiEvent>,
    midi_out_q: Vec<Lv2ShmMidiEvent>,
    last_event: Option<[u8; 3]>,
    latency: u32,
    pending_worker_resp: bool,
    deadline_ns: u64,
    first_block: bool,
    consecutive_misses: u32,
    crashed_gen: Arc<AtomicU64>,
}

impl SandboxLv2Audio {
    pub fn slot_id(&self) -> u64 {
        self.slot_id
    }

    pub fn worker_pid(&self) -> u32 {
        self.shm.block().worker_pid.load(Ordering::Relaxed)
    }

    fn adopt_swap(&mut self) {
        while let Ok((new_gen, new_shm)) = self.swap_rx.pop() {
            let old = std::mem::replace(&mut self.shm, new_shm);
            if let Err(rtrb::PushError::Full(old)) = self.retire_tx.push(old) {
                std::mem::forget(old);
            }
            self.current_gen = new_gen;
            self.consecutive_misses = 0;
            self.first_block = true;
            self.pending_worker_resp = false;
            self.controls_dirty = true;
        }
    }

    fn is_crashed_now(&self) -> bool {
        self.crashed_gen.load(Ordering::Acquire) == self.current_gen
            || self.shm.block().worker_alive.load(Ordering::Acquire) == 0
    }

    fn mark_crashed_now(&self) {
        self.crashed_gen.store(self.current_gen, Ordering::Release);
    }

    fn queue_midi_bytes(&mut self, msg: &[u8]) {
        if !self.has_midi_in || msg.is_empty() || msg.len() > 3 {
            return;
        }
        let mut bytes = [0u8; 3];
        bytes[..msg.len()].copy_from_slice(msg);
        self.last_event = Some(bytes);
        if self.midi_in_q.len() < self.midi_in_q.capacity() {
            self.midi_in_q.push(Lv2ShmMidiEvent { len: msg.len() as u8, bytes });
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn is_instrument(&self) -> bool {
        self.is_instrument
    }

    pub fn is_midi_fx(&self) -> bool {
        self.is_midi_fx
    }

    pub fn reports_latency(&self) -> bool {
        self.reports_latency
    }

    pub fn latency_frames(&self) -> u32 {
        self.latency
    }

    pub fn control_count(&self) -> usize {
        self.controls_meta.len()
    }

    pub fn set_control_normalized(&mut self, nth: usize, value: f32) {
        let Some(meta) = self.controls_meta.get(nth) else { return };
        let Some(atom) = self.controls_shadow.get(nth) else { return };
        let v = meta.min + (meta.max - meta.min) * value.clamp(0.0, 1.0);
        atom.store(v.to_bits(), Ordering::Release);
        self.controls_dirty = true;
    }

    pub fn control_normalized(&self, nth: usize) -> Option<f32> {
        let meta = self.controls_meta.get(nth)?;
        let atom = self.controls_shadow.get(nth)?;
        let v = f32::from_bits(atom.load(Ordering::Relaxed));
        Some(if meta.max > meta.min {
            ((v - meta.min) / (meta.max - meta.min)).clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    pub fn control_descs(&self) -> Vec<Lv2ControlDesc> {
        self.controls_meta
            .iter()
            .enumerate()
            .map(|(i, meta)| {
                let mut d = meta.clone();
                if let Some(atom) = self.controls_shadow.get(i) {
                    d.value = f32::from_bits(atom.load(Ordering::Relaxed));
                }
                d
            })
            .collect()
    }

    pub fn apply_preset(&mut self, preset: &Lv2Preset) {
        for (sym, val) in &preset.values {
            if let Some(&nth) = self.controls_by_symbol.get(sym) {
                if let Some(atom) = self.controls_shadow.get(nth) {
                    atom.store(val.to_bits(), Ordering::Release);
                    self.controls_dirty = true;
                }
            }
        }
    }

    pub fn send_midi(&mut self, ev: [u8; 3]) {
        self.queue_midi_bytes(&ev);
    }

    pub fn read_midi_out(&self, out: &mut Vec<FxOutEvent>) {
        for ev in &self.midi_out_q {
            if out.len() >= out.capacity() {
                break;
            }
            let d = ev.bytes;
            let (status, ch) = (d[0] & 0xF0, d[0] & 0x0F);
            match status {
                0x90 if d[2] > 0 => out.push(FxOutEvent::NoteOn { channel: ch, note: d[1], velocity: d[2] }),
                0x80 | 0x90 => out.push(FxOutEvent::NoteOff { channel: ch, note: d[1] }),
                0xB0 | 0xE0 | 0xA0 => out.push(FxOutEvent::Midi(d)),
                _ => {}
            }
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) {
        self.note_on_ch(note, velocity, 0);
    }

    pub fn note_off(&mut self, note: u8) {
        self.note_off_ch(note, 0);
    }

    pub fn note_on_ch(&mut self, note: u8, velocity: u8, channel: u8) {
        self.queue_midi_bytes(&[0x90 | (channel & 0x0F), note, velocity]);
    }

    pub fn note_off_ch(&mut self, note: u8, channel: u8) {
        self.queue_midi_bytes(&[0x80 | (channel & 0x0F), note, 0]);
    }

    pub fn control_change(&mut self, cc: u8, value: u8) {
        self.control_change_ch(cc, value, 0);
    }

    pub fn control_change_ch(&mut self, cc: u8, value: u8, channel: u8) {
        self.queue_midi_bytes(&[0xB0 | (channel & 0x0F), cc & 0x7F, value & 0x7F]);
    }

    pub fn channel_pressure(&mut self, value: u8) {
        self.queue_midi_bytes(&[0xD0, value & 0x7F]);
    }

    pub fn midi(&mut self, msg: &[u8]) {
        self.queue_midi_bytes(msg);
    }

    pub fn program_change(&mut self, program: u8) {
        self.queue_midi_bytes(&[0xC0, program.min(127)]);
    }

    pub fn last_midi_event(&self) -> Option<[u8; 3]> {
        self.last_event
    }

    pub fn clear_last_midi_event(&mut self) {
        self.last_event = None;
    }

    pub fn has_pending_worker_response(&self) -> bool {
        self.pending_worker_resp
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        self.adopt_swap();
        self.midi_out_q.clear();
        let out_total = (frames * 2).min(output.len());
        if self.is_crashed_now() {
            if out_total > 0 {
                output[..out_total].fill(0.0);
            }
            self.midi_in_q.clear();
            self.pending_worker_resp = false;
            return;
        }
        if frames == 0 && !self.is_midi_fx {
            return;
        }

        let mut offset = 0usize;
        let mut chunk_idx = 0usize;
        while offset < frames || (frames == 0 && chunk_idx == 0) {
            let chunk = if frames == 0 {
                0
            } else {
                (frames - offset).min(MAX_LV2_SHM_FRAMES)
            };
            let n2 = chunk * 2;
            let sent_controls = chunk_idx == 0 && self.controls_dirty;
            let sent_midi = chunk_idx == 0;

            {
                let blk = self.shm.block_mut();
                blk.frames = chunk as u32;
                blk.is_midi_fx = u32::from(self.is_midi_fx);
                if sent_controls {
                    let cc = self.controls_shadow.len().min(MAX_LV2_CONTROLS);
                    blk.control_count = cc as u32;
                    for (i, atom) in self.controls_shadow.iter().take(cc).enumerate() {
                        blk.controls[i] = f32::from_bits(atom.load(Ordering::Relaxed));
                    }
                    blk.controls_dirty = 1;
                } else {
                    blk.controls_dirty = 0;
                }
                if sent_midi {
                    let mc = self.midi_in_q.len().min(MAX_LV2_MIDI_EVENTS);
                    blk.midi_in_count = mc as u32;
                    if mc > 0 {
                        blk.midi_in[..mc].copy_from_slice(&self.midi_in_q[..mc]);
                    }
                } else {
                    blk.midi_in_count = 0;
                }
                if !self.is_midi_fx && n2 > 0 {
                    let src_start = offset * 2;
                    let avail = input.len().saturating_sub(src_start).min(n2);
                    if avail > 0 {
                        blk.in_buf[..avail].copy_from_slice(&input[src_start..src_start + avail]);
                    }
                    if avail < n2 {
                        blk.in_buf[avail..n2].fill(0.0);
                    }
                }
            }

            let prev_seq = self.shm.block().worker_seq.load(Ordering::Acquire);
            let seq = self.shm.block().host_seq.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
            futex_wake(&self.shm.block().host_seq);

            let ready = {
                let blk = self.shm.block();
                wait_for_worker_seq(
                    &blk.worker_seq,
                    &blk.worker_alive,
                    prev_seq,
                    seq,
                    &mut self.first_block,
                    self.deadline_ns,
                )
            };

            if ready {
                let blk = self.shm.block();
                if sent_controls {
                    self.controls_dirty = false;
                }
                if sent_midi {
                    self.midi_in_q.clear();
                }
                if !self.is_midi_fx && n2 > 0 {
                    let dst_start = offset * 2;
                    let avail = output.len().saturating_sub(dst_start).min(n2);
                    if avail > 0 {
                        output[dst_start..dst_start + avail].copy_from_slice(&blk.out_buf[..avail]);
                    }
                }
                let moc = (blk.midi_out_count as usize).min(MAX_LV2_MIDI_EVENTS);
                for ev in &blk.midi_out[..moc] {
                    if self.midi_out_q.len() < self.midi_out_q.capacity() {
                        self.midi_out_q.push(*ev);
                    }
                }
                self.latency = blk.latency_frames.load(Ordering::Acquire);
                self.pending_worker_resp = blk.pending_worker_resp.load(Ordering::Acquire) != 0;
                self.consecutive_misses = 0;
            } else {
                self.midi_in_q.clear();
                if !self.is_midi_fx {
                    let dst_start = offset * 2;
                    if dst_start < output.len() {
                        let rem = (frames * 2).saturating_sub(dst_start).min(output.len() - dst_start);
                        output[dst_start..dst_start + rem].fill(0.0);
                    }
                }
                self.pending_worker_resp = false;
                self.consecutive_misses += 1;
                if self.shm.block().worker_alive.load(Ordering::Acquire) == 0
                    || self.consecutive_misses >= DEAD_MISS_THRESHOLD
                {
                    self.mark_crashed_now();
                }
                break;
            }

            if frames == 0 {
                break;
            }
            offset += chunk;
            chunk_idx += 1;
        }
    }
}

/// Off-RT supervisor for a sandboxed LV2 worker (#413).
pub struct SandboxLv2Main {
    slot_id: u64,
    uri: String,
    sample_rate: f64,
    block: usize,
    child: Option<Child>,
    ctrl: Option<UnixStream>,
    swap_tx: rtrb::Producer<(u64, Lv2ShmRegion)>,
    retire_rx: rtrb::Consumer<Lv2ShmRegion>,
    spawn_gen: u64,
    crashed_gen: Arc<AtomicU64>,
    controls_shadow: Arc<Vec<AtomicU32>>,
    last_state: Option<(String, String, Vec<u8>)>,
    worker_pid: u32,
    crash_count: u32,
    last_crash_reason: Option<String>,
    strip_notice: Option<String>,
    editor_open: bool,
}

impl SandboxLv2Main {
    pub fn slot_id(&self) -> u64 {
        self.slot_id
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn worker_pid(&self) -> u32 {
        self.worker_pid
    }

    pub fn crash_count(&self) -> u32 {
        self.crash_count
    }

    pub fn strip_notice(&self) -> Option<&str> {
        self.strip_notice.as_deref()
    }

    pub fn is_crashed(&self) -> bool {
        self.crashed_gen.load(Ordering::Acquire) == self.spawn_gen
    }

    pub fn editor_open(&self) -> bool {
        self.editor_open
    }

    fn rpc(&mut self, req: &CtrlReq) -> Result<CtrlResp> {
        let Some(ctrl) = self.ctrl.as_mut() else {
            bail!("lv2 worker unavailable");
        };
        if send_msg(ctrl, req).is_err() {
            self.ctrl = None;
            self.editor_open = false;
            self.crashed_gen.store(self.spawn_gen, Ordering::Release);
            if self.last_crash_reason.is_none() {
                self.last_crash_reason = Some("control socket closed".into());
            }
            bail!("lv2 control socket closed");
        }
        match recv_msg::<CtrlResp>(ctrl) {
            Ok(resp) => Ok(resp),
            Err(_) => {
                self.ctrl = None;
                self.editor_open = false;
                self.crashed_gen.store(self.spawn_gen, Ordering::Release);
                if self.last_crash_reason.is_none() {
                    self.last_crash_reason = Some("control socket error".into());
                }
                bail!("lv2 control socket error");
            }
        }
    }

    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        match self.rpc(&CtrlReq::Lv2OpenEditor { title: title.to_string() })? {
            CtrlResp::Ok => {
                self.editor_open = true;
                Ok(())
            }
            CtrlResp::Err(e) => bail!("{e}"),
            _ => bail!("unexpected reply to Lv2OpenEditor"),
        }
    }

    pub fn close_editor(&mut self) {
        if !self.editor_open {
            return;
        }
        self.editor_open = false;
        let _ = self.rpc(&CtrlReq::Lv2CloseEditor);
    }

    pub fn poll_editor(&mut self) -> Result<(bool, Vec<(usize, f32, f32)>, Vec<[u8; 3]>)> {
        match self.rpc(&CtrlReq::Lv2EditorPoll)? {
            CtrlResp::Lv2EditorStatus { open, control_writes, midi_events } => {
                self.editor_open = open;
                for &(nth, native, _) in &control_writes {
                    if let Some(a) = self.controls_shadow.get(nth) {
                        a.store(native.to_bits(), Ordering::Release);
                    }
                }
                Ok((open, control_writes, midi_events))
            }
            CtrlResp::Err(e) => {
                self.editor_open = false;
                bail!("{e}")
            }
            _ => bail!("unexpected reply to Lv2EditorPoll"),
        }
    }

    pub fn save_preset(&mut self, name: &str) -> Result<Lv2Preset> {
        match self.rpc(&CtrlReq::Lv2SavePreset { name: name.to_string() })? {
            CtrlResp::Lv2PresetSaved(preset) => Ok(preset),
            CtrlResp::Err(e) => bail!("{e}"),
            _ => bail!("unexpected reply to Lv2SavePreset"),
        }
    }

    pub fn apply_preset_full(&mut self, preset: &Lv2Preset) -> Result<()> {
        match self.rpc(&CtrlReq::Lv2ApplyPreset { preset: preset.clone() })? {
            CtrlResp::Ok => Ok(()),
            CtrlResp::Err(e) => bail!("{e}"),
            _ => bail!("unexpected reply to Lv2ApplyPreset"),
        }
    }

    pub fn save_state(&mut self, key: &str) -> Result<Vec<u8>> {
        if self.ctrl.is_none() {
            if let Some((k, _, blob)) = &self.last_state {
                if k == key {
                    return Ok(blob.clone());
                }
            }
            bail!("lv2 worker unavailable");
        }
        match self.rpc(&CtrlReq::Lv2SaveState { key: key.to_string() })? {
            CtrlResp::State(bytes) => {
                let kind = crate::lv2::state_format(&self.uri)
                    .map_or("http://lv2plug.in/ns/ext/atom#Chunk", |f| f.kind)
                    .to_string();
                self.last_state = Some((key.to_string(), kind, bytes.clone()));
                Ok(bytes)
            }
            CtrlResp::Err(e) => bail!("{e}"),
            _ => bail!("unexpected reply to Lv2SaveState"),
        }
    }

    /// Check if the LV2 worker crashed or hung; if so, restart it with its
    /// last saved state and live control values and hot-swap into the audio
    /// thread. Returns `(strip_badge, banner_msg)`.
    pub fn poll_recovery(&mut self) -> Option<(String, String)> {
        while self.retire_rx.pop().is_ok() {}

        let mut died_reason: Option<String> = None;
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    died_reason = Some(exit_reason(status));
                    self.child = None;
                    self.ctrl = None;
                    self.editor_open = false;
                    self.crashed_gen.store(self.spawn_gen, Ordering::Release);
                }
                Ok(None) => {}
                Err(_) => {
                    died_reason = Some("wait failed".into());
                    self.child = None;
                    self.ctrl = None;
                    self.editor_open = false;
                }
            }
        } else if self.is_crashed() {
            self.editor_open = false;
            died_reason = self.last_crash_reason.take().or_else(|| Some("worker lost".into()));
        }

        if died_reason.is_none() && self.is_crashed() {
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

        let controls: Vec<(usize, f32)> = self
            .controls_shadow
            .iter()
            .enumerate()
            .map(|(i, a)| (i, f32::from_bits(a.load(Ordering::Acquire))))
            .collect();

        match spawn_lv2_worker(&self.uri, self.sample_rate, self.block, self.last_state.clone(), controls) {
            Ok(w) => {
                self.child = Some(w.child);
                self.ctrl = Some(w.ctrl);
                self.worker_pid = w.pid;
                self.spawn_gen = self.spawn_gen.wrapping_add(1).max(1);
                let _ = self.swap_tx.push((self.spawn_gen, w.shm));
                let badge = format!("RESTARTED ({reason})");
                let msg = format!("LV2 plugin {} crashed ({reason}) — restarted with saved state", self.uri);
                self.strip_notice = Some(badge.clone());
                Some((badge, msg))
            }
            Err(e) => {
                let badge = format!("CRASHED ({reason})");
                let msg = format!("LV2 plugin {} crashed ({reason}) and failed to restart: {e}", self.uri);
                self.strip_notice = Some(badge.clone());
                Some((badge, msg))
            }
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

impl Drop for SandboxLv2Main {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn a new out-of-process LV2 worker and return its RT-safe audio handle
/// (`SandboxLv2Audio`) and off-RT supervisor (`SandboxLv2Main`).
pub fn spawn_sandboxed_lv2(
    uri: &str,
    sample_rate: f64,
    block: usize,
    initial_state: Option<(String, String, Vec<u8>)>,
) -> Result<(SandboxLv2Audio, SandboxLv2Main)> {
    let w = spawn_lv2_worker(uri, sample_rate, block, initial_state.clone(), Vec::new())?;
    let slot_id = NEXT_LV2_SLOT_ID.fetch_add(1, Ordering::Relaxed);
    let (swap_tx, swap_rx) = rtrb::RingBuffer::<(u64, Lv2ShmRegion)>::new(4);
    let (retire_tx, retire_rx) = rtrb::RingBuffer::<Lv2ShmRegion>::new(4);
    let crashed_gen = Arc::new(AtomicU64::new(0));
    let spawn_gen = 1u64;

    let mut controls_by_symbol = HashMap::new();
    let mut shadow_vec = Vec::with_capacity(w.controls.len());
    for (i, d) in w.controls.iter().enumerate() {
        controls_by_symbol.insert(d.symbol.clone(), i);
        shadow_vec.push(AtomicU32::new(d.value.to_bits()));
    }
    let controls_shadow = Arc::new(shadow_vec);

    let audio = SandboxLv2Audio {
        slot_id,
        uri: w.uri.clone(),
        is_instrument: w.is_instrument,
        is_midi_fx: w.is_midi_fx,
        reports_latency: w.reports_latency,
        has_midi_in: w.has_midi_in,
        shm: w.shm,
        current_gen: spawn_gen,
        swap_rx,
        retire_tx,
        controls_meta: w.controls,
        controls_by_symbol,
        controls_shadow: controls_shadow.clone(),
        controls_dirty: false,
        midi_in_q: Vec::with_capacity(MAX_LV2_MIDI_EVENTS),
        midi_out_q: Vec::with_capacity(MAX_LV2_MIDI_EVENTS),
        last_event: None,
        latency: w.latency,
        pending_worker_resp: false,
        deadline_ns: DEFAULT_DEADLINE_US * 1_000,
        first_block: true,
        consecutive_misses: 0,
        crashed_gen: crashed_gen.clone(),
    };

    let main = SandboxLv2Main {
        slot_id,
        uri: w.uri,
        sample_rate,
        block,
        child: Some(w.child),
        ctrl: Some(w.ctrl),
        swap_tx,
        retire_rx,
        spawn_gen,
        crashed_gen,
        controls_shadow,
        last_state: initial_state,
        worker_pid: w.pid,
        crash_count: 0,
        last_crash_reason: None,
        strip_notice: None,
        editor_open: false,
    };
    Ok((audio, main))
}

/// Either an in-process [`Lv2Plugin`] (`KABELSALAT_SANDBOX=0`) or an isolated
/// worker-backed [`SandboxLv2Audio`].
pub enum HostedLv2 {
    InProcess(Lv2Plugin),
    Sandbox(SandboxLv2Audio),
}

impl HostedLv2 {
    pub fn slot_id(&self) -> Option<u64> {
        match self {
            Self::InProcess(_) => None,
            Self::Sandbox(s) => Some(s.slot_id()),
        }
    }

    pub fn is_sandboxed(&self) -> bool {
        matches!(self, Self::Sandbox(_))
    }

    pub fn worker_pid(&self) -> Option<u32> {
        match self {
            Self::InProcess(_) => None,
            Self::Sandbox(s) => Some(s.worker_pid()),
        }
    }

    pub fn uri(&self) -> &str {
        match self {
            Self::InProcess(p) => p.uri(),
            Self::Sandbox(s) => s.uri(),
        }
    }

    pub fn is_instrument(&self) -> bool {
        match self {
            Self::InProcess(p) => p.is_instrument(),
            Self::Sandbox(s) => s.is_instrument(),
        }
    }

    pub fn is_midi_fx(&self) -> bool {
        match self {
            Self::InProcess(p) => p.is_midi_fx(),
            Self::Sandbox(s) => s.is_midi_fx(),
        }
    }

    pub fn reports_latency(&self) -> bool {
        match self {
            Self::InProcess(p) => p.reports_latency(),
            Self::Sandbox(s) => s.reports_latency(),
        }
    }

    pub fn latency_frames(&self) -> u32 {
        match self {
            Self::InProcess(p) => p.latency_frames(),
            Self::Sandbox(s) => s.latency_frames(),
        }
    }

    pub fn control_count(&self) -> usize {
        match self {
            Self::InProcess(p) => p.control_count(),
            Self::Sandbox(s) => s.control_count(),
        }
    }

    pub fn set_control_normalized(&mut self, nth: usize, value: f32) {
        match self {
            Self::InProcess(p) => p.set_control_normalized(nth, value),
            Self::Sandbox(s) => s.set_control_normalized(nth, value),
        }
    }

    pub fn control_normalized(&self, nth: usize) -> Option<f32> {
        match self {
            Self::InProcess(p) => p.control_normalized(nth),
            Self::Sandbox(s) => s.control_normalized(nth),
        }
    }

    pub fn control_descs(&self) -> Vec<Lv2ControlDesc> {
        match self {
            Self::InProcess(p) => p.control_descs(),
            Self::Sandbox(s) => s.control_descs(),
        }
    }

    pub fn apply_preset(&mut self, preset: &Lv2Preset) {
        match self {
            Self::InProcess(p) => p.apply_preset(preset),
            Self::Sandbox(s) => s.apply_preset(preset),
        }
    }

    pub fn send_midi(&mut self, ev: [u8; 3]) {
        match self {
            Self::InProcess(p) => p.send_midi(ev),
            Self::Sandbox(s) => s.send_midi(ev),
        }
    }

    pub fn read_midi_out(&self, out: &mut Vec<FxOutEvent>) {
        match self {
            Self::InProcess(p) => p.read_midi_out(out),
            Self::Sandbox(s) => s.read_midi_out(out),
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) {
        match self {
            Self::InProcess(p) => p.note_on(note, velocity),
            Self::Sandbox(s) => s.note_on(note, velocity),
        }
    }

    pub fn note_off(&mut self, note: u8) {
        match self {
            Self::InProcess(p) => p.note_off(note),
            Self::Sandbox(s) => s.note_off(note),
        }
    }

    pub fn note_on_ch(&mut self, note: u8, velocity: u8, channel: u8) {
        match self {
            Self::InProcess(p) => p.note_on_ch(note, velocity, channel),
            Self::Sandbox(s) => s.note_on_ch(note, velocity, channel),
        }
    }

    pub fn note_off_ch(&mut self, note: u8, channel: u8) {
        match self {
            Self::InProcess(p) => p.note_off_ch(note, channel),
            Self::Sandbox(s) => s.note_off_ch(note, channel),
        }
    }

    pub fn control_change(&mut self, cc: u8, value: u8) {
        match self {
            Self::InProcess(p) => p.control_change(cc, value),
            Self::Sandbox(s) => s.control_change(cc, value),
        }
    }

    pub fn control_change_ch(&mut self, cc: u8, value: u8, channel: u8) {
        match self {
            Self::InProcess(p) => p.control_change_ch(cc, value, channel),
            Self::Sandbox(s) => s.control_change_ch(cc, value, channel),
        }
    }

    pub fn channel_pressure(&mut self, value: u8) {
        match self {
            Self::InProcess(p) => p.channel_pressure(value),
            Self::Sandbox(s) => s.channel_pressure(value),
        }
    }

    pub fn midi(&mut self, msg: &[u8]) {
        match self {
            Self::InProcess(p) => p.midi(msg),
            Self::Sandbox(s) => s.midi(msg),
        }
    }

    pub fn program_change(&mut self, program: u8) {
        match self {
            Self::InProcess(p) => p.program_change(program),
            Self::Sandbox(s) => s.program_change(program),
        }
    }

    pub fn last_midi_event(&self) -> Option<[u8; 3]> {
        match self {
            Self::InProcess(p) => p.last_midi_event(),
            Self::Sandbox(s) => s.last_midi_event(),
        }
    }

    pub fn clear_last_midi_event(&mut self) {
        match self {
            Self::InProcess(p) => p.clear_last_midi_event(),
            Self::Sandbox(s) => s.clear_last_midi_event(),
        }
    }

    pub fn has_pending_worker_response(&self) -> bool {
        match self {
            Self::InProcess(p) => p.has_pending_worker_response(),
            Self::Sandbox(s) => s.has_pending_worker_response(),
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        match self {
            Self::InProcess(p) => p.process(input, output, frames),
            Self::Sandbox(s) => s.process(input, output, frames),
        }
    }
}

fn run_lv2_sandbox_phase() -> Result<()> {
    let (dir, uri) = crate::lv2::ui::build_fixture_bundle("sandbox-lv2")?;
    let res = (|| -> Result<()> {
        let bias: f32 = 0.5;
        let init_state = (
            "urn:kabelsalat:fixture:state_bias".to_string(),
            "http://lv2plug.in/ns/ext/atom#Chunk".to_string(),
            bias.to_le_bytes().to_vec(),
        );
        let (mut lv2, mut main) = spawn_sandboxed_lv2(&uri, 48000.0, 8192, Some(init_state))?;
        anyhow::ensure!(!lv2.is_instrument(), "gain fixture should be an effect");
        anyhow::ensure!(lv2.control_count() == 1, "expected 1 control port");
        // Gain port range is 0.0..=2.0 -> normalized 0.6 = native 1.2; times bias 0.5 = 0.6.
        lv2.set_control_normalized(0, 0.6);

        let inp = vec![1.0f32; 256 * 2];
        let mut out = vec![0.0f32; 256 * 2];
        lv2.process(&inp, &mut out, 256);
        let peak = |buf: &[f32]| buf.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let p_before = peak(&out);
        anyhow::ensure!(
            (p_before - 0.6).abs() < 1e-3,
            "sandboxed LV2 expected peak 0.6 before crash (got {p_before})"
        );

        let pid_before = main.worker_pid();
        println!("sandbox-test: killing LV2 fixture worker pid={pid_before} with SIGSEGV...");
        unsafe { libc::kill(pid_before as libc::pid_t, libc::SIGSEGV); }

        let mut recovered = None;
        for _ in 0..40 {
            lv2.process(&inp, &mut out, 256);
            if let Some(pair) = main.poll_recovery() {
                recovered = Some(pair);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (badge, msg) = recovered.ok_or_else(|| anyhow!("LV2 worker did not detect SIGSEGV and restart"))?;
        anyhow::ensure!(badge == "RESTARTED (SIGSEGV)", "unexpected badge {badge:?}");
        println!("sandbox-test: {msg}");
        anyhow::ensure!(main.worker_pid() != pid_before, "new LV2 worker must have a new PID");
        anyhow::ensure!(main.crash_count() == 1, "LV2 crash_count should be 1");
        anyhow::ensure!(main.strip_notice() == Some("RESTARTED (SIGSEGV)"));

        // Verify silence before adoption is safe AND post-adoption preserves state + control.
        lv2.process(&inp, &mut out, 256);
        let p_after = peak(&out);
        anyhow::ensure!(
            (p_after - 0.6).abs() < 1e-3,
            "recovered LV2 lost state or control value: expected 0.6, got {p_after}"
        );
        let saved = main.save_state("urn:kabelsalat:fixture:state_bias")?;
        anyhow::ensure!(saved.len() == 4, "expected 4-byte float state, got {}", saved.len());
        let saved_bias = f32::from_le_bytes(saved.try_into().unwrap());
        anyhow::ensure!((saved_bias - 0.5).abs() < 1e-5, "saved state bias mismatch: {saved_bias}");

        main.shutdown();
        Ok(())
    })();
    crate::lv2::remove_bundle(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    res
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

    run_lv2_sandbox_phase()?;

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

    #[test]
    fn sandbox_hosts_vst3_probe_fixture_and_restores_state() {
        let (bundle, synth_cid, _) = crate::vst3::build_fixture_bundle("sandbox").unwrap();
        let key = crate::vst3::voice_key(&synth_cid);
        let (mut voice, mut main) = spawn_sandboxed_clap(48000.0, &key, None).unwrap();

        let params = main.param_list();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "Gain");
        assert!((main.param_value(0).unwrap() - 1.0).abs() < 1e-4);

        let mut buf = [0.0f32; 128 * 2];
        voice.note_on(69, 1.0);
        voice.render(&mut buf, Transport::default());
        let p = crate::analysis::peak(&buf);
        assert!(p > 0.25, "sandboxed VST3 synth should emit audio: {p}");

        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.3));
        voice.render(&mut buf, Transport::default());
        assert!((main.param_value(0).unwrap() - 0.3).abs() < 1e-4);

        let blob = main.save_state().unwrap();
        assert!(blob.starts_with(b"KSV3"));
        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.9));
        voice.render(&mut buf, Transport::default());
        main.load_state(&blob).unwrap();
        assert!((main.param_value(0).unwrap() - 0.3).abs() < 1e-4);

        main.shutdown();
        let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
    }

    #[test]
    fn sandbox_drain_param_edits_rpc_delivers_edits() {
        let (bundle, synth_cid, _) = crate::vst3::build_fixture_bundle("sandbox-edits").unwrap();
        let key = crate::vst3::voice_key(&synth_cid);
        let (mut voice, mut main) = spawn_sandboxed_clap(48000.0, &key, None).unwrap();

        let mut buf = [0.0f32; 128 * 2];
        voice.push_rt(VoiceCmd::Param(ClapId::new(0), 0.421875));
        voice.render(&mut buf, Transport::default());

        assert!(
            main.has_pending_param_edits(),
            "worker audio outputParameterChanges must raise SandboxMain pending flag"
        );
        let edits = main.drain_param_edits();
        assert!(
            edits.iter().any(|&(id, val)| id == 0 && (val - 0.625).abs() < 1e-4),
            "expected (0, 0.625) across sandbox DrainParamEdits RPC, got {edits:?}"
        );
        assert!(!main.has_pending_param_edits(), "drain_param_edits clears pending flag");
        assert!(main.drain_param_edits().is_empty(), "subsequent drain is empty");

        main.shutdown();
        let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
    }

    #[test]
    fn lv2_sandbox_segv_recovery_preserves_controls_and_state() {
        run_lv2_sandbox_phase().expect("LV2 sandbox recovery phase must pass");
    }

    #[test]
    fn lv2_sandbox_in_process_fallback_works() {
        let (dir, uri) = crate::lv2::ui::build_fixture_bundle("inproc-lv2").unwrap();
        let plugin = Lv2Plugin::load(&uri, 48000.0, 8192).unwrap();
        let mut hosted = HostedLv2::InProcess(plugin);
        assert!(!hosted.is_sandboxed());
        assert_eq!(hosted.slot_id(), None);
        assert_eq!(hosted.worker_pid(), None);
        hosted.set_control_normalized(0, 0.25); // range 0..2 -> 0.5
        let inp = vec![1.0f32; 64 * 2];
        let mut out = vec![0.0f32; 64 * 2];
        hosted.process(&inp, &mut out, 64);
        assert!((out[0] - 0.5).abs() < 1e-4, "expected 0.5, got {}", out[0]);
        if let HostedLv2::InProcess(p) = hosted {
            crate::lv2::retire(p);
        }
        crate::lv2::remove_bundle(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandboxed_lv2_cohosted_ui_supports_data_access_meters_and_bidirectional_atoms() {
        let (dir, uri) = crate::lv2::ui::build_fixture_bundle("cohosted-lv2").unwrap();
        let (mut lv2, mut main) = spawn_sandboxed_lv2(&uri, 48000.0, 256, None).unwrap();
        main.open_editor("Co-hosted Fixture").unwrap();
        assert!(main.editor_open());

        let inp = vec![0.8f32; 256 * 2];
        let mut out = vec![0.0f32; 256 * 2];
        let mut writes = Vec::new();
        for _ in 0..25 {
            lv2.process(&inp, &mut out, 256);
            let (open, w, _) = main.poll_editor().unwrap();
            assert!(open, "co-hosted LV2 UI closed prematurely");
            writes.extend(w);
            if writes.iter().any(|&(nth, v, _)| nth == 0 && (v - 1.25).abs() < 1e-4) {
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }

        assert!(
            writes.iter().any(|&(nth, v, _)| nth == 0 && (v - 0.75).abs() < 1e-4),
            "expected initial UI show() write 0.75, got {writes:?}"
        );
        assert!(
            writes.iter().any(|&(nth, v, _)| nth == 0 && (v - 1.25).abs() < 1e-4),
            "expected co-hosted direct-access + meter + ping/pong handshake write 1.25, got {writes:?}"
        );

        main.close_editor();
        assert!(!main.editor_open());
        main.shutdown();
        crate::lv2::remove_bundle(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
