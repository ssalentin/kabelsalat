//! VST3 plugin hosting via the `vst3` crate (`IComponent` / `IAudioProcessor` /
//! `IEditController` / `IMidiMapping` / `IPlugView` + `Linux::IRunLoop`).
//!
//! Designed to mirror Kabelsalat's RT audio guarantees: zero allocations and
//! zero locks inside [`Vst3Voice::render_block`], with a 4-state lock-free
//! [`Vst3Gate`] coordinating audio vs state save/load and teardown.

pub mod com;
pub mod scan;

use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, BusInfo, ControllerNumbers_, Event,
    Event__type0, Event_::EventTypes_, IAudioProcessor, IAudioProcessorTrait, IComponent,
    IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait, IHostApplication, IMidiMapping, IMidiMappingTrait, NoteOffEvent,
    NoteOnEvent, ParamID, ParameterInfo, ParameterInfo_::ParameterFlags_, ProcessContext,
    ProcessContext_::StatesAndFlags_, ProcessData, ProcessModes_, ProcessSetup, SpeakerArr,
    SymbolicSampleSizes_, ViewType,
};
use vst3::Steinberg::{
    FUnknown, IPlugFrame, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
    IPluginFactoryTrait, TUID, ViewRect, kPlatformTypeX11EmbedWindowID, kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComWrapper, Interface};

use self::com::{
    EVENT_LIST_CAP, HostApplication, HostComponentHandler, HostEventList, HostParamChanges,
    HostPlugFrame, MemoryStream, PARAM_CHANGES_SLOTS, PARAM_QUEUE_CAP, str_to_tchar_slice,
    string128_to_string,
};
#[allow(unused_imports)]
pub use self::scan::{
    VST3_KEY_PREFIX, Vst3PluginInfo, add_bundle, bundle_with_cid, is_vst3_key, list_vst3_plugins,
    list_vst3_plugins_retry_failed, list_vst3_plugins_with_errors, run_scan_vst3, strip_vst3_key,
    voice_key,
};
use crate::host::{ParamDesc, VoiceCmd};
use crate::main_controls::{Candidate, MainControls};
use crate::plugin_window::PluginWindow;
use crate::transport::Transport;

const BLOCK: usize = 256;
const STATE_MAGIC: &[u8; 4] = b"KSV3";

// 4-state lock-free gate: protects audio thread vs state/teardown without mutex locks on RT.
const GATE_IDLE: u8 = 0;
const GATE_IN_PROCESS: u8 = 1;
const GATE_MAIN_HELD: u8 = 2;
const GATE_RETIRED: u8 = 3;

struct Vst3Gate {
    state: AtomicU8,
}

impl Vst3Gate {
    fn new() -> Self {
        Self { state: AtomicU8::new(GATE_IDLE) }
    }

    fn try_enter_audio(&self) -> bool {
        self.state
            .compare_exchange(GATE_IDLE, GATE_IN_PROCESS, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn leave_audio(&self) {
        self.state.store(GATE_IDLE, Ordering::Release);
    }

    fn enter_main(&self, target: u8) -> bool {
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            if self
                .state
                .compare_exchange(GATE_IDLE, target, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            if self.state.load(Ordering::Relaxed) == GATE_RETIRED {
                return false;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }
}

struct GateMainGuard<'a>(&'a Vst3Gate);
impl Drop for GateMainGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.state.compare_exchange(
            GATE_MAIN_HELD,
            GATE_IDLE,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

/// Keep loaded VST3 plugin modules & factories resident (`ModuleEntry` called once).
static LOADED_FACTORIES: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());

fn load_module_factory(so_path: &Path) -> Result<ComPtr<IPluginFactory>> {
    crate::binarch::ensure_loadable(so_path)?;
    let canon = std::fs::canonicalize(so_path).unwrap_or_else(|_| so_path.to_path_buf());
    {
        let guard = LOADED_FACTORIES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, raw)) = guard.iter().find(|(p, _)| *p == canon) {
            if let Some(borrowed) = unsafe { vst3::ComRef::from_raw(*raw as *mut IPluginFactory) } {
                return Ok(borrowed.to_com_ptr());
            }
        }
    }

    let c_so = CString::new(canon.to_string_lossy().as_bytes()).context("invalid VST3 path")?;
    let dl = unsafe { libc::dlopen(c_so.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if dl.is_null() {
        let err = unsafe {
            let p = libc::dlerror();
            if p.is_null() { "dlopen failed".into() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        bail!("failed to open VST3 binary {}: {err}", canon.display());
    }

    let entry_sym = unsafe { libc::dlsym(dl, c"ModuleEntry".as_ptr()) };
    if !entry_sym.is_null() {
        type ModuleEntryFn = unsafe extern "C" fn(*mut c_void) -> bool;
        let entry_fn: ModuleEntryFn = unsafe { std::mem::transmute(entry_sym) };
        if !unsafe { entry_fn(dl) } {
            bail!("ModuleEntry returned false for {}", canon.display());
        }
    }

    let get_factory_sym = unsafe { libc::dlsym(dl, c"GetPluginFactory".as_ptr()) };
    if get_factory_sym.is_null() {
        bail!("{} exports no GetPluginFactory", canon.display());
    }
    type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;
    let get_factory: GetFactoryFn = unsafe { std::mem::transmute(get_factory_sym) };
    let raw = unsafe { get_factory() };
    let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(raw) }
        .ok_or_else(|| anyhow!("GetPluginFactory returned null for {}", canon.display()))?;

    let mut guard = LOADED_FACTORIES.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.iter().any(|(p, _)| *p == canon) {
        // Keep one ref alive cleanly alongside the open dlopen handle.
        let kept = factory.as_com_ref().to_com_ptr().into_raw() as usize;
        guard.push((canon, kept));
    }
    Ok(factory)
}

#[derive(Clone, Debug)]
struct Vst3ParamMeta {
    id: u32,
    name: String,
    module: String,
    #[allow(dead_code)]
    units: String,
    step_count: i32,
    default_norm: f64,
    value_norm: f64,
    automatable: bool,
    stepped: bool,
    hidden: bool,
    readonly: bool,
}

impl Vst3ParamMeta {
    fn to_desc(&self) -> ParamDesc {
        ParamDesc {
            id: self.id,
            name: self.name.clone(),
            module: self.module.clone(),
            min: 0.0,
            max: 1.0,
            value: self.value_norm,
            default: self.default_norm,
            automatable: self.automatable,
            stepped: self.stepped,
            hidden: self.hidden,
            readonly: self.readonly,
        }
    }
}

/// Real-time VST3 audio processor handle moved to the engine audio thread.
pub struct Vst3Voice {
    processor: ComPtr<IAudioProcessor>,
    gate: Arc<Vst3Gate>,
    latency: Arc<AtomicU32>,
    is_effect: bool,
    sample_rate: f64,
    sample_pos: u64,
    in_bus_channels: Vec<usize>,
    out_bus_channels: Vec<usize>,
    in_ch_bufs: Vec<Vec<Vec<f32>>>,
    out_ch_bufs: Vec<Vec<Vec<f32>>>,
    in_ptrs: Vec<Vec<*mut f32>>,
    out_ptrs: Vec<Vec<*mut f32>>,
    in_bus_arr: Vec<AudioBusBuffers>,
    out_bus_arr: Vec<AudioBusBuffers>,
    has_event_in: bool,
    ev_in: ComWrapper<HostEventList>,
    ev_out: ComWrapper<HostEventList>,
    in_params: ComWrapper<HostParamChanges>,
    out_params: ComWrapper<HostParamChanges>,
    valid_param_ids: Vec<u32>,
    midi_cc_map: [Option<u32>; 130],
    /// VST3 program-change parameter `(param_id, step_count)` declared via
    /// `ParameterFlags_::kIsProgramChange` (Ardour #111176).
    program_change_param: Option<(u32, i32)>,
    main_to_audio_rx: rtrb::Consumer<(u32, f64, u64)>,
    audio_to_main_tx: rtrb::Producer<(u32, f64, u64)>,
    last_main_seq: u64,
    wake_flag: Arc<AtomicBool>,
    last_tail: u32,
}

// SAFETY: Vst3Voice owns the real-time processor interface after setup and is
// moved exclusively to the audio thread; concurrent main-thread state access
// coordinates via the atomic `Vst3Gate`.
unsafe impl Send for Vst3Voice {}

impl Vst3Voice {
    pub fn is_effect(&self) -> bool {
        self.is_effect
    }

    #[allow(dead_code)]
    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    pub(crate) fn shared_latency(&self) -> Arc<AtomicU32> {
        self.latency.clone()
    }

    pub fn take_wake_request(&mut self) -> bool {
        self.wake_flag.swap(false, Ordering::AcqRel)
    }

    pub fn param_range(&self, id: u32) -> Option<(f64, f64)> {
        self.valid_param_ids.binary_search(&id).ok().map(|_| (0.0, 1.0))
    }

    pub fn tail_samples(&self) -> u32 {
        self.last_tail
    }

    pub fn has_pending_work(&self) -> bool {
        !self.main_to_audio_rx.is_empty()
    }

    pub fn sleep_skip(&mut self, frames: usize) {
        self.sample_pos = self.sample_pos.wrapping_add(frames as u64);
    }

    fn push_event(&self, ev: Event) {
        let _ = unsafe { self.ev_in.push(ev) };
    }

    fn apply_cmd(&mut self, cmd: VoiceCmd, sample_offset: i32) {
        match cmd {
            VoiceCmd::NoteOn(note, vel) => {
                if !self.has_event_in {
                    return;
                }
                let mut u = Event__type0 {
                    noteOn: NoteOnEvent {
                        channel: 0,
                        pitch: note as i16,
                        tuning: 0.0,
                        velocity: vel.clamp(0.0, 1.0) as f32,
                        length: 0,
                        noteId: note as i32,
                    },
                };
                let _ = &mut u;
                self.push_event(Event {
                    busIndex: 0,
                    sampleOffset: sample_offset,
                    ppqPosition: 0.0,
                    flags: vst3::Steinberg::Vst::Event_::EventFlags_::kIsLive as u16,
                    r#type: EventTypes_::kNoteOnEvent as u16,
                    __field0: u,
                });
            }
            VoiceCmd::NoteOff(note) => {
                if !self.has_event_in {
                    return;
                }
                self.push_event(Event {
                    busIndex: 0,
                    sampleOffset: sample_offset,
                    ppqPosition: 0.0,
                    flags: vst3::Steinberg::Vst::Event_::EventFlags_::kIsLive as u16,
                    r#type: EventTypes_::kNoteOffEvent as u16,
                    __field0: Event__type0 {
                        noteOff: NoteOffEvent {
                            channel: 0,
                            pitch: note as i16,
                            velocity: 0.0,
                            noteId: note as i32,
                            tuning: 0.0,
                        },
                    },
                });
            }
            VoiceCmd::Param(id, val) => {
                let pid = id.get();
                if self.valid_param_ids.binary_search(&pid).is_ok() {
                    let norm = if val.is_finite() { val.clamp(0.0, 1.0) } else { return };
                    unsafe { self.in_params.add_point(pid, sample_offset, norm) };
                    let _ = self.audio_to_main_tx.push((pid, norm, self.last_main_seq));
                }
            }
            VoiceCmd::Midi([b0, b1, b2]) => {
                let status = b0 & 0xF0;
                let ch = (b0 & 0x0F) as i16;
                match status {
                    0x90 if b2 > 0 && self.has_event_in => {
                        let note = (b1 & 0x7F) as i16;
                        let vel = (b2 & 0x7F) as f32 / 127.0;
                        self.push_event(Event {
                            busIndex: 0,
                            sampleOffset: sample_offset,
                            ppqPosition: 0.0,
                            flags: vst3::Steinberg::Vst::Event_::EventFlags_::kIsLive as u16,
                            r#type: EventTypes_::kNoteOnEvent as u16,
                            __field0: Event__type0 {
                                noteOn: NoteOnEvent {
                                    channel: ch,
                                    pitch: note,
                                    tuning: 0.0,
                                    velocity: vel,
                                    length: 0,
                                    noteId: (ch as i32) * 128 + note as i32,
                                },
                            },
                        });
                    }
                    0x80 | 0x90 if self.has_event_in => {
                        let note = (b1 & 0x7F) as i16;
                        self.push_event(Event {
                            busIndex: 0,
                            sampleOffset: sample_offset,
                            ppqPosition: 0.0,
                            flags: vst3::Steinberg::Vst::Event_::EventFlags_::kIsLive as u16,
                            r#type: EventTypes_::kNoteOffEvent as u16,
                            __field0: Event__type0 {
                                noteOff: NoteOffEvent {
                                    channel: ch,
                                    pitch: note,
                                    velocity: 0.0,
                                    noteId: (ch as i32) * 128 + note as i32,
                                    tuning: 0.0,
                                },
                            },
                        });
                    }
                    0xB0 => {
                        let cc = (b1 & 0x7F) as usize;
                        if let Some(pid) = self.midi_cc_map[cc] {
                            let val = (b2 & 0x7F) as f64 / 127.0;
                            unsafe { self.in_params.add_point(pid, sample_offset, val) };
                            let _ = self.audio_to_main_tx.push((pid, val, self.last_main_seq));
                        }
                    }
                    0xC0 => {
                        if let Some((pid, step_count)) = self.program_change_param {
                            let val = program_change_to_normalized(b1, step_count);
                            unsafe { self.in_params.add_point(pid, sample_offset, val) };
                            let _ = self.audio_to_main_tx.push((pid, val, self.last_main_seq));
                        }
                    }
                    0xD0 => {
                        let slot = ControllerNumbers_::kAfterTouch as usize;
                        if slot < self.midi_cc_map.len() {
                            if let Some(pid) = self.midi_cc_map[slot] {
                                let val = (b1 & 0x7F) as f64 / 127.0;
                                unsafe { self.in_params.add_point(pid, sample_offset, val) };
                                let _ = self.audio_to_main_tx.push((pid, val, self.last_main_seq));
                            }
                        }
                    }
                    0xE0 => {
                        let slot = ControllerNumbers_::kPitchBend as usize;
                        if slot < self.midi_cc_map.len() {
                            if let Some(pid) = self.midi_cc_map[slot] {
                                let raw = ((b2 as u16 & 0x7F) << 7) | (b1 as u16 & 0x7F);
                                let val = raw as f64 / 16383.0;
                                unsafe { self.in_params.add_point(pid, sample_offset, val) };
                                let _ = self.audio_to_main_tx.push((pid, val, self.last_main_seq));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Process up to `BLOCK` stereo-interleaved frames with zero allocations and zero blocking.
    pub fn render_block(
        &mut self,
        n: usize,
        inp: Option<&[f32]>,
        out: &mut [f32],
        transport: &Transport,
        cmds: &[VoiceCmd],
    ) {
        let frames = n.min(BLOCK).min(out.len() / 2);
        if frames == 0 {
            return;
        }
        if !self.gate.try_enter_audio() {
            out[..frames * 2].fill(0.0);
            self.sample_pos = self.sample_pos.wrapping_add(frames as u64);
            return;
        }

        // Prepare IO channel scratch slices (preallocated, zero allocation).
        for bus in &mut self.in_ch_bufs {
            for ch in bus {
                ch[..frames].fill(0.0);
            }
        }
        if let (Some(src), Some(main_in)) = (inp, self.in_ch_bufs.get_mut(0)) {
            let n_in_ch = main_in.len();
            if n_in_ch == 1 {
                for i in 0..frames {
                    main_in[0][i] = 0.5 * (src[i * 2] + src[i * 2 + 1]);
                }
            } else if n_in_ch >= 2 {
                for i in 0..frames {
                    main_in[0][i] = src[i * 2];
                    main_in[1][i] = src[i * 2 + 1];
                }
            }
        }
        for bus in &mut self.out_ch_bufs {
            for ch in bus {
                ch[..frames].fill(0.0);
            }
        }

        unsafe {
            self.ev_in.clear();
            self.ev_out.clear();
            self.in_params.clear();
            self.out_params.clear();
        }
        while let Ok((id, val, seq)) = self.main_to_audio_rx.pop() {
            if seq > self.last_main_seq {
                self.last_main_seq = seq;
            }
            if self.valid_param_ids.binary_search(&id).is_ok() && val.is_finite() {
                unsafe { self.in_params.add_point(id, 0, val.clamp(0.0, 1.0)) };
            }
        }
        for &cmd in cmds {
            self.apply_cmd(cmd, 0);
        }

        for (b, chs) in self.in_bus_channels.iter().enumerate() {
            self.in_bus_arr[b] = AudioBusBuffers {
                numChannels: *chs as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: self.in_ptrs[b].as_mut_ptr() },
            };
        }
        for (b, chs) in self.out_bus_channels.iter().enumerate() {
            self.out_bus_arr[b] = AudioBusBuffers {
                numChannels: *chs as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: self.out_ptrs[b].as_mut_ptr() },
            };
        }

        let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
        ctx.sampleRate = self.sample_rate;
        ctx.projectTimeSamples = self.sample_pos as i64;
        ctx.continousTimeSamples = self.sample_pos as i64;
        ctx.projectTimeMusic = transport.beats;
        ctx.tempo = transport.bpm;
        ctx.timeSigNumerator = transport.sig.num as i32;
        ctx.timeSigDenominator = transport.sig.den as i32;
        ctx.state = (StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid
            | StatesAndFlags_::kProjectTimeMusicValid
            | if transport.playing { StatesAndFlags_::kPlaying } else { 0 }) as u32;

        let mut pdata = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.in_bus_arr.len() as i32,
            numOutputs: self.out_bus_arr.len() as i32,
            inputs: if self.in_bus_arr.is_empty() { std::ptr::null_mut() } else { self.in_bus_arr.as_mut_ptr() },
            outputs: if self.out_bus_arr.is_empty() { std::ptr::null_mut() } else { self.out_bus_arr.as_mut_ptr() },
            inputParameterChanges: self.in_params.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            outputParameterChanges: self.out_params.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            inputEvents: self.ev_in.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            outputEvents: self.ev_out.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            processContext: &mut ctx,
        };

        let _ = unsafe { self.processor.process(&mut pdata) };
        self.last_tail = unsafe { self.processor.getTailSamples() };

        // Forward any parameter changes pushed by the processor back to main/UI.
        unsafe {
            let seq = self.last_main_seq;
            self.out_params.drain_last_points(|id, val| {
                let _ = self.audio_to_main_tx.push((id, val, seq));
            });
        }

        if let Some(main_out) = self.out_ch_bufs.first() {
            if main_out.len() == 1 {
                for i in 0..frames {
                    let s = if main_out[0][i].is_finite() { main_out[0][i] } else { 0.0 };
                    out[i * 2] = s;
                    out[i * 2 + 1] = s;
                }
            } else if main_out.len() >= 2 {
                for i in 0..frames {
                    let l = main_out[0][i];
                    let r = main_out[1][i];
                    out[i * 2] = if l.is_finite() { l } else { 0.0 };
                    out[i * 2 + 1] = if r.is_finite() { r } else { 0.0 };
                }
            } else {
                out[..frames * 2].fill(0.0);
            }
        } else {
            out[..frames * 2].fill(0.0);
        }

        self.sample_pos = self.sample_pos.wrapping_add(frames as u64);
        self.wake_flag.store(false, Ordering::Relaxed);
        self.gate.leave_audio();
    }
}

/// Output produced by pumping a VST3 plugin's main-thread & editor callbacks.
#[derive(Debug, Default)]
pub struct Vst3PumpResult {
    pub closed: bool,
    pub param_edits: Vec<(u32, f64)>,
    pub restarted: bool,
    pub latency_changed: bool,
}

struct Vst3Editor {
    view: ComPtr<IPlugView>,
    frame: ComWrapper<HostPlugFrame>,
    window: PluginWindow,
    resizable: bool,
}

/// Main-thread owner for an active VST3 plugin instance: parameters, state
/// serialization, main controls, latency queries, and native X11 editor window.
pub struct Vst3VoiceMain {
    info: Vst3PluginInfo,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
    controller_is_separate: bool,
    _host_app: ComWrapper<HostApplication>,
    handler: ComWrapper<HostComponentHandler>,
    gate: Arc<Vst3Gate>,
    latency: Arc<AtomicU32>,
    params: Vec<Vst3ParamMeta>,
    main_controls: MainControls,
    main_to_audio_tx: rtrb::Producer<(u32, f64, u64)>,
    audio_to_main_rx: rtrb::Consumer<(u32, f64, u64)>,
    param_seq: u64,
    param_min_seq: std::collections::HashMap<u32, u64>,
    editor: Option<Vst3Editor>,
    pending_latency_poll: bool,
    pending_param_edits: Vec<(u32, f64)>,
}

/// Convert a MIDI Program Change number (`0..=127`) into the normalized
/// `[0.0, 1.0]` parameter value for a VST3 `kIsProgramChange` parameter with
/// `step_count` steps (Ardour #111176).
pub(crate) fn program_change_to_normalized(program: u8, step_count: i32) -> f64 {
    let p = (program & 0x7F) as i32;
    if step_count > 0 {
        (p.min(step_count) as f64) / (step_count as f64)
    } else {
        (p as f64) / 127.0
    }
}

impl Vst3VoiceMain {
    /// Load, instantiate, connect, and activate a VST3 plugin by CID (`<32 hex>` or `vst3:<32 hex>`).
    pub fn load(cid_or_key: &str, sample_rate: f64) -> Result<(Vst3Voice, Self)> {
        let info = bundle_with_cid(cid_or_key)
            .ok_or_else(|| anyhow!("VST3 plugin '{}' not found", cid_or_key))?;
        Self::load_from_info(&info, sample_rate)
    }

    pub fn load_from_info(info: &Vst3PluginInfo, sample_rate: f64) -> Result<(Vst3Voice, Self)> {
        let resolved = scan::resolve_vst3_binary(&info.bundle_path).map_err(|e| anyhow!("{e}"))?;
        let factory = load_module_factory(&resolved.so_path)?;
        let cid_tuid: TUID = scan::parse_tuid(&info.cid)
            .ok_or_else(|| anyhow!("invalid VST3 CID '{}'", info.cid))?;

        let host_app = ComWrapper::new(HostApplication::new());
        let host_unknown = host_app
            .as_com_ref::<IHostApplication>()
            .map(|r| r.as_ptr() as *mut FUnknown)
            .unwrap_or(std::ptr::null_mut());

        let mut comp_raw: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            factory.createInstance(
                cid_tuid.as_ptr() as *const std::os::raw::c_char,
                IComponent::IID.as_ptr() as *const std::os::raw::c_char,
                &mut comp_raw,
            )
        };
        anyhow::ensure!(rc == kResultOk && !comp_raw.is_null(), "VST3 createInstance failed (rc={rc}) for {}", info.name);
        let component = unsafe { ComPtr::<IComponent>::from_raw(comp_raw as *mut IComponent) }
            .ok_or_else(|| anyhow!("null IComponent for {}", info.name))?;

        let rc = unsafe { component.initialize(host_unknown) };
        anyhow::ensure!(rc == kResultOk, "VST3 component initialize failed (rc={rc}) for {}", info.name);

        let processor = component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| anyhow!("VST3 {} has no IAudioProcessor", info.name))?;

        let (controller, controller_is_separate) = match component.cast::<IEditController>() {
            Some(c) => (c, false),
            None => {
                let mut ctrl_cid: TUID = [0; 16];
                let r = unsafe { component.getControllerClassId(&mut ctrl_cid) };
                anyhow::ensure!(r == kResultOk, "VST3 {} has neither single-component controller nor getControllerClassId", info.name);
                let mut ctrl_raw: *mut c_void = std::ptr::null_mut();
                let rc2 = unsafe {
                    factory.createInstance(
                        ctrl_cid.as_ptr() as *const std::os::raw::c_char,
                        IEditController::IID.as_ptr() as *const std::os::raw::c_char,
                        &mut ctrl_raw,
                    )
                };
                anyhow::ensure!(rc2 == kResultOk && !ctrl_raw.is_null(), "VST3 create controller failed (rc={rc2}) for {}", info.name);
                let ctrl = unsafe { ComPtr::<IEditController>::from_raw(ctrl_raw as *mut IEditController) }
                    .ok_or_else(|| anyhow!("null IEditController for {}", info.name))?;
                let rc3 = unsafe { ctrl.initialize(host_unknown) };
                anyhow::ensure!(rc3 == kResultOk, "VST3 controller initialize failed (rc={rc3})");
                (ctrl, true)
            }
        };

        let handler = ComWrapper::new(HostComponentHandler::new());
        if let Some(hptr) = handler.as_com_ref() {
            unsafe { controller.setComponentHandler(hptr.as_ptr()) };
        }

        if controller_is_separate {
            if let (Some(cp_a), Some(cp_b)) = (component.cast::<IConnectionPoint>(), controller.cast::<IConnectionPoint>()) {
                let _ = unsafe { cp_a.connect(cp_b.as_ptr()) };
                let _ = unsafe { cp_b.connect(cp_a.as_ptr()) };
            }
            // Initial state sync from component into split controller.
            let sync_stream = ComWrapper::new(MemoryStream::new());
            if let Some(s) = sync_stream.as_com_ref() {
                if unsafe { component.getState(s.as_ptr()) } == kResultOk {
                    let sync_read = ComWrapper::new(MemoryStream::from_bytes(sync_stream.to_vec()));
                    if let Some(sr) = sync_read.as_com_ref() {
                        let _ = unsafe { controller.setComponentState(sr.as_ptr()) };
                    }
                }
            }
        }

        // Inspect buses and activate main audio/event IO.
        let n_audio_in = unsafe { component.getBusCount(0, BusDirections_::kInput as i32) }.max(0) as usize;
        let n_audio_out = unsafe { component.getBusCount(0, BusDirections_::kOutput as i32) }.max(0) as usize;
        let n_ev_in = unsafe { component.getBusCount(1, BusDirections_::kInput as i32) }.max(0) as usize;

        let mut in_bus_channels = Vec::with_capacity(n_audio_in);
        let mut main_in_channels = 0i32;
        for i in 0..n_audio_in {
            let mut bi: BusInfo = unsafe { std::mem::zeroed() };
            let ch = if unsafe { component.getBusInfo(0, BusDirections_::kInput as i32, i as i32, &mut bi) } == kResultOk {
                bi.channelCount.max(0) as usize
            } else {
                2
            };
            if i == 0 {
                main_in_channels = ch as i32;
            }
            let _ = unsafe { component.activateBus(0, BusDirections_::kInput as i32, i as i32, if i == 0 { 1 } else { 0 }) };
            in_bus_channels.push(ch);
        }

        let mut out_bus_channels = Vec::with_capacity(n_audio_out);
        let mut main_out_channels = 0i32;
        for i in 0..n_audio_out {
            let mut bi: BusInfo = unsafe { std::mem::zeroed() };
            let ch = if unsafe { component.getBusInfo(0, BusDirections_::kOutput as i32, i as i32, &mut bi) } == kResultOk {
                bi.channelCount.max(0) as usize
            } else {
                2
            };
            if i == 0 {
                main_out_channels = ch as i32;
            }
            let _ = unsafe { component.activateBus(0, BusDirections_::kOutput as i32, i as i32, if i == 0 { 1 } else { 0 }) };
            out_bus_channels.push(ch);
        }

        let has_event_in = n_ev_in > 0;
        if has_event_in {
            let _ = unsafe { component.activateBus(1, BusDirections_::kInput as i32, 0, 1) };
        }

        let mut in_arr: Vec<u64> = in_bus_channels
            .iter()
            .map(|&ch| if ch == 1 { SpeakerArr::kMono } else { SpeakerArr::kStereo })
            .collect();
        let mut out_arr: Vec<u64> = out_bus_channels
            .iter()
            .map(|&ch| if ch == 1 { SpeakerArr::kMono } else { SpeakerArr::kStereo })
            .collect();
        let _ = unsafe {
            processor.setBusArrangements(
                in_arr.as_mut_ptr(),
                in_arr.len() as i32,
                out_arr.as_mut_ptr(),
                out_arr.len() as i32,
            )
        };

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: BLOCK as i32,
            sampleRate: sample_rate,
        };
        let sr = unsafe { processor.setupProcessing(&mut setup) };
        anyhow::ensure!(sr == kResultOk, "VST3 setupProcessing failed (rc={sr}) for {}", info.name);
        let ar = unsafe { component.setActive(1) };
        anyhow::ensure!(ar == kResultOk || ar == vst3::Steinberg::kNotImplemented, "VST3 setActive failed (rc={ar})");
        let _ = unsafe { processor.setProcessing(1) };

        let latency_frames = unsafe { processor.getLatencySamples() };
        let latency = Arc::new(AtomicU32::new(latency_frames));

        // Enumerate parameters & build main controls + MIDI CC mapping.
        let p_count = unsafe { controller.getParameterCount() }.max(0);
        let mut params = Vec::with_capacity(p_count as usize);
        let mut valid_param_ids = Vec::with_capacity(p_count as usize);
        let mut program_change_param: Option<(u32, i32)> = None;
        for i in 0..p_count {
            let mut pi: ParameterInfo = unsafe { std::mem::zeroed() };
            if unsafe { controller.getParameterInfo(i, &mut pi) } == kResultOk {
                let mut name = string128_to_string(&pi.title);
                if name.is_empty() {
                    name = string128_to_string(&pi.shortTitle);
                }
                let units = string128_to_string(&pi.units);
                let automatable = pi.flags & ParameterFlags_::kCanAutomate != 0;
                let readonly = pi.flags & ParameterFlags_::kIsReadOnly != 0;
                let hidden = pi.flags & ParameterFlags_::kIsHidden != 0;
                let stepped = pi.stepCount > 0;
                if program_change_param.is_none() && (pi.flags & ParameterFlags_::kIsProgramChange) != 0 {
                    program_change_param = Some((pi.id, pi.stepCount));
                }
                let cur = unsafe { controller.getParamNormalized(pi.id) }.clamp(0.0, 1.0);
                params.push(Vst3ParamMeta {
                    id: pi.id,
                    name,
                    module: String::new(),
                    units,
                    step_count: pi.stepCount,
                    default_norm: pi.defaultNormalizedValue.clamp(0.0, 1.0),
                    value_norm: if cur.is_finite() { cur } else { pi.defaultNormalizedValue.clamp(0.0, 1.0) },
                    automatable,
                    stepped,
                    hidden,
                    readonly,
                });
                valid_param_ids.push(pi.id);
            }
        }
        valid_param_ids.sort_unstable();
        valid_param_ids.dedup();

        let candidates: Vec<Candidate> = params
            .iter()
            .map(|p| Candidate {
                id: p.id,
                name: p.name.clone(),
                group: p.module.clone(),
                min: 0.0,
                max: 1.0,
                default: p.default_norm,
                usable: p.automatable && !p.readonly && !p.hidden,
                stepped: p.stepped,
                designated: false,
            })
            .collect();
        let main_controls = crate::main_controls::resolve(&[], &candidates);

        let mut midi_cc_map = [None; 130];
        if let Some(midi_map) = controller.cast::<IMidiMapping>() {
            for cc in 0..130usize {
                let mut pid: ParamID = 0;
                if unsafe { midi_map.getMidiControllerAssignment(0, 0, cc as i16, &mut pid) } == kResultOk
                    && valid_param_ids.binary_search(&pid).is_ok()
                {
                    midi_cc_map[cc] = Some(pid);
                }
            }
        }

        let has_inst = info.sub_categories.iter().any(|c| c.eq_ignore_ascii_case("Instrument"));
        let has_fx = info.sub_categories.iter().any(|c| c.eq_ignore_ascii_case("Fx"));
        let is_effect = !has_inst && (has_fx || (main_in_channels > 0 && main_out_channels > 0));

        let mut in_ch_bufs: Vec<Vec<Vec<f32>>> = in_bus_channels
            .iter()
            .map(|&chs| (0..chs).map(|_| vec![0.0f32; BLOCK]).collect())
            .collect();
        let mut out_ch_bufs: Vec<Vec<Vec<f32>>> = out_bus_channels
            .iter()
            .map(|&chs| (0..chs).map(|_| vec![0.0f32; BLOCK]).collect())
            .collect();
        let in_ptrs: Vec<Vec<*mut f32>> = in_ch_bufs
            .iter_mut()
            .map(|bus| bus.iter_mut().map(|ch| ch.as_mut_ptr()).collect())
            .collect();
        let out_ptrs: Vec<Vec<*mut f32>> = out_ch_bufs
            .iter_mut()
            .map(|bus| bus.iter_mut().map(|ch| ch.as_mut_ptr()).collect())
            .collect();
        let in_bus_arr = vec![unsafe { std::mem::zeroed() }; in_bus_channels.len()];
        let out_bus_arr = vec![unsafe { std::mem::zeroed() }; out_bus_channels.len()];

        let (main_to_audio_tx, main_to_audio_rx) = rtrb::RingBuffer::new(256);
        let (audio_to_main_tx, audio_to_main_rx) = rtrb::RingBuffer::new(256);
        let gate = Arc::new(Vst3Gate::new());

        let voice = Vst3Voice {
            processor: processor.clone(),
            gate: gate.clone(),
            latency: latency.clone(),
            is_effect,
            sample_rate,
            sample_pos: 0,
            in_bus_channels,
            out_bus_channels,
            in_ch_bufs,
            out_ch_bufs,
            in_ptrs,
            out_ptrs,
            in_bus_arr,
            out_bus_arr,
            has_event_in,
            ev_in: ComWrapper::new(HostEventList::new(EVENT_LIST_CAP)),
            ev_out: ComWrapper::new(HostEventList::new(EVENT_LIST_CAP)),
            in_params: ComWrapper::new(HostParamChanges::new(PARAM_CHANGES_SLOTS, PARAM_QUEUE_CAP)),
            out_params: ComWrapper::new(HostParamChanges::new(PARAM_CHANGES_SLOTS, PARAM_QUEUE_CAP)),
            valid_param_ids,
            midi_cc_map,
            program_change_param,
            main_to_audio_rx,
            audio_to_main_tx,
            last_main_seq: 0,
            wake_flag: Arc::new(AtomicBool::new(false)),
            last_tail: 0,
        };

        let main = Self {
            info: info.clone(),
            component,
            processor,
            controller,
            controller_is_separate,
            _host_app: host_app,
            handler,
            gate,
            latency,
            params,
            main_controls,
            main_to_audio_tx,
            audio_to_main_rx,
            param_seq: 0,
            param_min_seq: std::collections::HashMap::new(),
            editor: None,
            pending_latency_poll: false,
            pending_param_edits: Vec::new(),
        };

        Ok((voice, main))
    }

    #[allow(dead_code)]
    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> &Vst3PluginInfo {
        &self.info
    }

    pub fn has_editor(&mut self) -> bool {
        if self.editor.is_some() {
            return true;
        }
        let raw = unsafe { self.controller.createView(ViewType::kEditor) };
        let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(raw) }) else {
            return false;
        };
        let ok = unsafe { view.isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) } == kResultOk;
        drop(view);
        ok
    }

    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        let raw = unsafe { self.controller.createView(ViewType::kEditor) };
        let view = unsafe { ComPtr::<IPlugView>::from_raw(raw) }
            .ok_or_else(|| anyhow!("{} returned no VST3 editor view", self.info.name))?;
        if unsafe { view.isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) } != kResultOk {
            bail!("{} VST3 editor does not support X11EmbedWindowID", self.info.name);
        }

        let mut rect = ViewRect { left: 0, top: 0, right: 640, bottom: 480 };
        let _ = unsafe { view.getSize(&mut rect) };
        let w = (rect.right - rect.left).max(120).min(3840) as u32;
        let h = (rect.bottom - rect.top).max(100).min(2160) as u32;
        let can_res = unsafe { view.canResize() };
        let resizable = can_res == kResultOk || can_res == kResultTrue;

        crate::plugin_window::ensure_display();
        crate::plugin_window::ensure_x11_egl_platform();
        let mut window = PluginWindow::open(title, w, h, resizable)?;

        let frame = ComWrapper::new(HostPlugFrame::new());
        if let Some(fptr) = frame.as_com_ref::<IPlugFrame>() {
            let _ = unsafe { view.setFrame(fptr.as_ptr()) };
        }
        if let Some(scale) = view.cast::<IPlugViewContentScaleSupport>() {
            let _ = unsafe { scale.setContentScaleFactor(1.0) };
        }

        let rc = unsafe { view.attached(window.id() as usize as *mut c_void, kPlatformTypeX11EmbedWindowID) };
        if rc != kResultOk {
            let _ = unsafe { view.setFrame(std::ptr::null_mut()) };
            frame.clear_handlers();
            drop(view);
            drop(window);
            bail!("VST3 view.attached failed (rc={rc}) for {}", self.info.name);
        }

        let mut post = rect;
        if unsafe { view.getSize(&mut post) } == kResultOk {
            let nw = (post.right - post.left).max(120).min(3840) as u32;
            let nh = (post.bottom - post.top).max(100).min(2160) as u32;
            if (nw, nh) != window.size() {
                let _ = window.resize(nw, nh, resizable);
            }
        }

        self.editor = Some(Vst3Editor { view, frame, window, resizable });
        Ok(())
    }

    pub fn close_editor(&mut self) {
        if let Some(ed) = self.editor.take() {
            let _ = unsafe { ed.view.removed() };
            let _ = unsafe { ed.view.setFrame(std::ptr::null_mut()) };
            ed.frame.clear_handlers();
            drop(ed.view);
            drop(ed.window);
        }
    }

    pub fn editor_children(&self) -> Option<usize> {
        self.editor.as_ref().and_then(|ed| ed.window.child_count().ok())
    }

    fn push_main_param(&mut self, id: u32, norm: f64) {
        self.param_seq = self.param_seq.wrapping_add(1);
        self.param_min_seq.insert(id, self.param_seq);
        let _ = self.main_to_audio_tx.push((id, norm, self.param_seq));
    }

    fn drain_audio_params(&mut self) {
        while let Ok((id, val, seq)) = self.audio_to_main_rx.pop() {
            if self.param_min_seq.get(&id).is_some_and(|&min_seq| seq < min_seq) {
                continue;
            }
            if let Some(m) = self.params.iter_mut().find(|p| p.id == id) {
                m.value_norm = val;
            }
            let _ = unsafe { self.controller.setParamNormalized(id, val) };
            self.pending_param_edits.push((id, val));
        }
    }

    /// Poll X11 events, Linux run loop, parameter edits from the plugin GUI,
    /// and resize/restart flags.
    pub fn pump(&mut self) -> Vst3PumpResult {
        let mut res = Vst3PumpResult::default();
        self.drain_audio_params();

        let (edits, restart_flags) = self.handler.take();
        for (id, val) in edits {
            if let Some(m) = self.params.iter_mut().find(|p| p.id == id) {
                m.value_norm = val;
            }
            self.push_main_param(id, val);
            self.pending_param_edits.push((id, val));
            res.param_edits.push((id, val));
        }
        if restart_flags != 0 {
            res.restarted = true;
            if restart_flags & vst3::Steinberg::Vst::RestartFlags_::kLatencyChanged != 0 {
                self.pending_latency_poll = true;
                res.latency_changed = true;
            }
            if restart_flags & vst3::Steinberg::Vst::RestartFlags_::kParamValuesChanged != 0 {
                self.refresh_param_values();
            }
        }

        if let Some(ed) = &mut self.editor {
            ed.frame.pump_run_loop();
            if let Some((w, h)) = ed.frame.take_resize() {
                let _ = ed.window.resize(w, h, ed.resizable);
            }
            let events = ed.window.poll();
            if events.close_requested {
                res.closed = true;
            } else if let Some((w, h)) = events.resized.filter(|_| ed.resizable) {
                let mut vr = ViewRect { left: 0, top: 0, right: w as i32, bottom: h as i32 };
                let _ = unsafe { ed.view.onSize(&mut vr) };
            }
        }
        if res.closed {
            self.close_editor();
        }
        res
    }

    pub fn service(&mut self) -> Vec<(u32, f64)> {
        self.pump().param_edits
    }

    /// Drain parameter edits observed from the plugin editor (`performEdit`)
    /// or processor `outputParameterChanges` since the last call (GigChain #60).
    pub fn drain_param_edits(&mut self) -> Vec<(u32, f64)> {
        self.drain_audio_params();
        std::mem::take(&mut self.pending_param_edits)
    }

    pub fn poll_latency(&mut self) -> Option<u32> {
        if !self.pending_latency_poll {
            return None;
        }
        self.pending_latency_poll = false;
        let lat = unsafe { self.processor.getLatencySamples() };
        self.latency.store(lat, Ordering::Relaxed);
        Some(lat)
    }

    pub fn set_param_value(&mut self, id: u32, val: f64) {
        let norm = if val.is_finite() { val.clamp(0.0, 1.0) } else { return };
        if let Some(m) = self.params.iter_mut().find(|p| p.id == id) {
            m.value_norm = norm;
        }
        let _ = unsafe { self.controller.setParamNormalized(id, norm) };
        self.push_main_param(id, norm);
    }

    fn refresh_param_values(&mut self) {
        self.drain_audio_params();
        for p in &mut self.params {
            let v = unsafe { self.controller.getParamNormalized(p.id) };
            if v.is_finite() {
                p.value_norm = v.clamp(0.0, 1.0);
            }
        }
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        self.drain_audio_params();
        anyhow::ensure!(self.gate.enter_main(GATE_MAIN_HELD), "audio gate busy/retired during VST3 save_state");
        let _guard = GateMainGuard(&self.gate);

        let comp_stream = ComWrapper::new(MemoryStream::new());
        let rc = unsafe {
            self.component.getState(
                comp_stream.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            )
        };
        let comp_bytes = if rc == kResultOk { comp_stream.to_vec() } else { Vec::new() };

        let ctrl_stream = ComWrapper::new(MemoryStream::new());
        let ctrl_bytes = if self.controller_is_separate {
            let rc2 = unsafe {
                self.controller.getState(
                    ctrl_stream.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
                )
            };
            if rc2 == kResultOk { ctrl_stream.to_vec() } else { Vec::new() }
        } else {
            Vec::new()
        };

        let mut out = Vec::with_capacity(12 + comp_bytes.len() + ctrl_bytes.len());
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&(comp_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&comp_bytes);
        out.extend_from_slice(&(ctrl_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&ctrl_bytes);
        Ok(out)
    }

    pub fn load_state(&mut self, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(bytes.len() >= 12 && &bytes[..4] == STATE_MAGIC, "invalid VST3 KSV3 state blob");
        let comp_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        anyhow::ensure!(bytes.len() >= 12 + comp_len, "truncated VST3 component state");
        let comp_bytes = bytes[8..8 + comp_len].to_vec();
        let ctrl_off = 8 + comp_len;
        let ctrl_len = u32::from_le_bytes(bytes[ctrl_off..ctrl_off + 4].try_into().unwrap()) as usize;
        anyhow::ensure!(bytes.len() >= ctrl_off + 4 + ctrl_len, "truncated VST3 controller state");
        let ctrl_bytes = bytes[ctrl_off + 4..ctrl_off + 4 + ctrl_len].to_vec();

        anyhow::ensure!(self.gate.enter_main(GATE_MAIN_HELD), "audio gate busy/retired during VST3 load_state");
        let _guard = GateMainGuard(&self.gate);
        // Discard stale audio->main or GUI edits while holding the gate so they
        // cannot overwrite the restored state below.
        while self.audio_to_main_rx.pop().is_ok() {}
        let _ = self.handler.take();

        let comp_stream = ComWrapper::new(MemoryStream::from_bytes(comp_bytes.clone()));
        let rc = unsafe {
            self.component.setState(
                comp_stream.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            )
        };
        anyhow::ensure!(rc == kResultOk || rc == kResultTrue, "VST3 component.setState failed (rc={rc})");

        let comp_ctrl_stream = ComWrapper::new(MemoryStream::from_bytes(comp_bytes));
        let _ = unsafe {
            self.controller.setComponentState(
                comp_ctrl_stream.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
            )
        };
        if self.controller_is_separate && !ctrl_bytes.is_empty() {
            let ctrl_stream = ComWrapper::new(MemoryStream::from_bytes(ctrl_bytes));
            let _ = unsafe {
                self.controller.setState(
                    ctrl_stream.as_com_ref().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut()),
                )
            };
        }

        self.param_seq = self.param_seq.wrapping_add(1);
        let load_seq = self.param_seq;
        for p in &mut self.params {
            let v = unsafe { self.controller.getParamNormalized(p.id) };
            if v.is_finite() {
                p.value_norm = v.clamp(0.0, 1.0);
            }
            self.param_min_seq.insert(p.id, load_seq);
            let _ = self.main_to_audio_tx.push((p.id, p.value_norm, load_seq));
        }
        let lat = unsafe { self.processor.getLatencySamples() };
        self.latency.store(lat, Ordering::Relaxed);
        drop(_guard);
        Ok(())
    }

    pub fn param_list(&mut self) -> Vec<ParamDesc> {
        self.refresh_param_values();
        self.params.iter().map(Vst3ParamMeta::to_desc).collect()
    }

    pub fn param_desc(&mut self, id: u32) -> Option<ParamDesc> {
        self.refresh_param_values();
        self.params.iter().find(|p| p.id == id).map(Vst3ParamMeta::to_desc)
    }

    pub fn param_value(&mut self, id: u32) -> Option<f64> {
        self.drain_audio_params();
        let v = unsafe { self.controller.getParamNormalized(id) };
        v.is_finite().then_some(v.clamp(0.0, 1.0))
    }

    pub fn param_text_opt(&self, id: u32, value: f64) -> Option<String> {
        let mut s = [0 as vst3::Steinberg::Vst::TChar; 128];
        if unsafe { self.controller.getParamStringByValue(id, value.clamp(0.0, 1.0), &mut s) } == kResultOk {
            let text = string128_to_string(&s);
            if !text.is_empty() {
                return Some(text);
            }
        }
        None
    }

    pub fn param_text(&self, id: u32, value: f64) -> String {
        self.param_text_opt(id, value).unwrap_or_else(|| format!("{value:.3}"))
    }

    pub fn param_text_to_value(&self, id: u32, text: &str) -> Option<f64> {
        let mut buf = [0 as vst3::Steinberg::Vst::TChar; 128];
        str_to_tchar_slice(text.trim(), &mut buf);
        let mut out = 0.0f64;
        if unsafe { self.controller.getParamValueByString(id, buf.as_mut_ptr(), &mut out) } == kResultOk
            && out.is_finite()
        {
            return Some(out.clamp(0.0, 1.0));
        }
        None
    }

    pub fn param_options(&self, desc: &ParamDesc) -> Vec<(f64, String)> {
        let Some(meta) = self.params.iter().find(|p| p.id == desc.id) else {
            return Vec::new();
        };
        if meta.step_count <= 0 || meta.step_count > 64 {
            return Vec::new();
        }
        let steps = meta.step_count;
        let mut out = Vec::with_capacity(steps as usize + 1);
        for k in 0..=steps {
            let norm = (k as f64) / (steps as f64);
            let label = self.param_text(desc.id, norm);
            if !out.iter().any(|(_, l): &(f64, String)| l == &label) {
                out.push((norm, label));
            }
        }
        out
    }

    pub fn resolve_param_text(&self, desc: &ParamDesc, text: &str) -> Option<f64> {
        let raw = text.trim();
        let opts = self.param_options(desc);
        if let Some((v, _)) = opts.iter().find(|(_, l)| l.eq_ignore_ascii_case(raw)) {
            return Some(*v);
        }
        let lower = raw.to_ascii_lowercase();
        if let Some((v, _)) = opts.iter().find(|(_, l)| l.to_ascii_lowercase().contains(&lower)) {
            return Some(*v);
        }
        if let Some(v) = self.param_text_to_value(desc.id, raw) {
            return Some(v);
        }
        if let Some(pct) = raw.strip_suffix('%').and_then(|s| s.trim().parse::<f64>().ok()) {
            return Some((pct / 100.0).clamp(0.0, 1.0));
        }
        if let Ok(v) = raw.parse::<f64>() {
            return Some(v.clamp(0.0, 1.0));
        }
        None
    }

    pub fn main_controls(&self) -> &MainControls {
        &self.main_controls
    }

    pub fn poll_main_controls(&mut self) -> bool {
        false
    }

    pub fn retire(mut self) {
        self.close_editor();
        let _ = self.gate.enter_main(GATE_RETIRED);
        self.gate.state.store(GATE_RETIRED, Ordering::Release);
        let _ = unsafe { self.processor.setProcessing(0) };
        let _ = unsafe { self.component.setActive(0) };
        if self.controller_is_separate {
            if let (Some(cp_a), Some(cp_b)) = (self.component.cast::<IConnectionPoint>(), self.controller.cast::<IConnectionPoint>()) {
                let _ = unsafe { cp_a.disconnect(cp_b.as_ptr()) };
                let _ = unsafe { cp_b.disconnect(cp_a.as_ptr()) };
            }
            let _ = unsafe { self.controller.setComponentHandler(std::ptr::null_mut()) };
            let _ = unsafe { self.controller.terminate() };
        } else {
            let _ = unsafe { self.controller.setComponentHandler(std::ptr::null_mut()) };
        }
        let _ = unsafe { self.component.terminate() };
    }
}

/// Compile the pure-C VST3 fixture bundle into a temp directory and register it
/// for tests and `kabelsalat vst3-test`. Returns `(bundle_dir, synth_cid, gain_cid)`.
pub fn build_fixture_bundle(tag: &str) -> Result<(PathBuf, String, String)> {
    let base = std::env::temp_dir().join(format!("kabelsalat-vst3-fixture-{tag}-{}", std::process::id()));
    let bundle = base.join("KabelsalatProbe.vst3");
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    };
    let arch_dir = bundle.join("Contents").join(arch);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&arch_dir)?;

    let c_path = base.join("vst3_probe.c");
    let so_path = arch_dir.join("KabelsalatProbe.so");
    std::fs::write(&c_path, include_str!("fixture/vst3_probe.c"))?;

    let status = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&so_path)
        .arg(&c_path)
        .arg("-lm")
        .status()
        .context("invoking cc to compile VST3 fixture")?;
    anyhow::ensure!(status.success(), "cc failed compiling VST3 probe fixture");

    add_bundle(&bundle);
    let synth_cid = scan::format_tuid(&vst3::uid(0x4B535633, 0x00014000, 0x80000000, 0x00000001));
    let gain_cid = scan::format_tuid(&vst3::uid(0x4B535633, 0x00014000, 0x80000000, 0x00000002));
    Ok((bundle, synth_cid, gain_cid))
}

fn peak_of(buf: &[f32]) -> f32 {
    buf.iter().copied().map(f32::abs).fold(0.0, f32::max)
}

/// `kabelsalat vst3-test [cid...]`: verify VST3 scanning, audio/MIDI rendering,
/// parameter edits, state save/restore, `IMidiMapping`, `IPlugView` + `IRunLoop`,
/// and any named installed VST3 CIDs.
pub fn run_vst3_test(args: &[String]) -> Result<()> {
    let (bundle, synth_cid, gain_cid) = build_fixture_bundle("cli")?;
    let res = (|| -> Result<()> {
        let scanned = scan::scan_vst3_bundle(&bundle)?;
        anyhow::ensure!(scanned.len() == 2, "expected 2 classes in VST3 probe, got {}", scanned.len());
        anyhow::ensure!(scanned.iter().any(|p| p.cid == synth_cid && p.name == "Kabelsalat Probe Synth"));
        anyhow::ensure!(scanned.iter().any(|p| p.cid == gain_cid && p.name == "Kabelsalat Probe Gain"));
        println!("vst3-test scan: PASS (2 classes)");

        // 1. Synth MIDI + CC mapping + state round-trip
        let (mut synth_v, mut synth_m) = Vst3VoiceMain::load_from_info(&scanned[0], 48_000.0)?;
        let mut out = vec![0.0f32; BLOCK * 2];
        let tr = Transport::default();
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[VoiceCmd::NoteOn(69, 1.0)]);
        let loud = peak_of(&out);
        anyhow::ensure!(loud > 0.25, "synth note produced insufficient audio: {loud}");

        // IMidiMapping maps CC7 -> Param 0 (Gain)
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[VoiceCmd::Midi([0xB0, 7, 32])]);
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[]);
        let quiet = peak_of(&out);
        anyhow::ensure!(quiet < loud * 0.5 && quiet > 0.01, "CC7 gain drop expected: loud={loud} quiet={quiet}");

        // Save/restore state ("KSV3")
        synth_m.set_param_value(0, 0.8);
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[]);
        let saved = synth_m.save_state()?;
        anyhow::ensure!(saved.starts_with(STATE_MAGIC), "missing KSV3 header");
        synth_m.set_param_value(0, 0.1);
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[]);
        synth_m.load_state(&saved)?;
        synth_v.render_block(BLOCK, None, &mut out, &tr, &[VoiceCmd::NoteOff(69)]);
        let restored = synth_m.param_value(0).unwrap_or(0.0);
        anyhow::ensure!((restored - 0.8).abs() < 0.01, "state load_state should restore gain ~0.8, got {restored}");

        // Headless IPlugView + IPlugFrame + Linux::IRunLoop timer verification
        let raw_view = unsafe { synth_m.controller.createView(ViewType::kEditor) };
        let view = unsafe { ComPtr::<IPlugView>::from_raw(raw_view) }.context("fixture createView returned null")?;
        anyhow::ensure!(unsafe { view.isPlatformTypeSupported(kPlatformTypeX11EmbedWindowID) } == kResultOk);
        let frame = ComWrapper::new(HostPlugFrame::new());
        unsafe { view.setFrame(frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr()) };
        std::thread::sleep(Duration::from_millis(25));
        frame.pump_run_loop();
        let pumped = synth_m.service();
        anyhow::ensure!(
            pumped.iter().any(|&(id, v)| id == 0 && (v - 0.75).abs() < 1e-4),
            "expected IRunLoop timer to emit performEdit(0, 0.75), got {pumped:?}"
        );
        unsafe { view.removed() };
        unsafe { view.setFrame(std::ptr::null_mut()) };
        frame.clear_handlers();
        drop(view);
        synth_m.retire();
        println!("vst3-test synth + IMidiMapping + KSV3 state + IPlugView/IRunLoop: PASS");

        // 2. Effect audio gain check
        let (mut fx_v, fx_m) = Vst3VoiceMain::load_from_info(&scanned[1], 48_000.0)?;
        anyhow::ensure!(fx_v.is_effect(), "Probe Gain should be classified as effect");
        let inp = vec![0.6f32; BLOCK * 2];
        fx_v.render_block(BLOCK, Some(&inp), &mut out, &tr, &[VoiceCmd::Param(0.into(), 0.5)]);
        let fx_peak = peak_of(&out);
        anyhow::ensure!((fx_peak - 0.3).abs() < 0.02, "expected 0.6 * 0.5 = 0.3, got {fx_peak}");
        fx_m.retire();
        println!("vst3-test effect: PASS");

        // Optional user-requested installed VST3 CIDs
        for cid in args {
            let (mut v, m) = Vst3VoiceMain::load(cid, 48_000.0)?;
            let mut b = vec![0.0f32; BLOCK * 2];
            v.render_block(BLOCK, None, &mut b, &tr, &[]);
            println!("vst3-test {}: PASS (effect={}, params={})", m.info().name, v.is_effect(), m.params.len());
            m.retire();
        }
        println!("=== vst3-test PASS ===");
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vst3_probe_fixture_passes_audio_midi_state_and_runloop() {
        run_vst3_test(&[]).expect("vst3-test fixture should pass");
    }

    #[test]
    fn vst3_probe_param_steps_and_text_parse_work() {
        let (bundle, synth_cid, _) = build_fixture_bundle("params").unwrap();
        let (mut _v, mut m) = Vst3VoiceMain::load(&synth_cid, 48_000.0).unwrap();
        let wave = m.param_desc(1).unwrap();
        assert!(wave.stepped);
        let opts = m.param_options(&wave);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[1].1, "Tri");
        assert!((m.resolve_param_text(&wave, "Tri").unwrap() - 0.5).abs() < 1e-4);
        let gain = m.param_desc(0).unwrap();
        assert!((m.resolve_param_text(&gain, "50%").unwrap() - 0.5).abs() < 1e-4);
        m.retire();
        let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
    }

    #[test]
    fn vst3_program_change_normalization_matches_step_count() {
        // 4 programs => step_count = 3 (0..=3)
        assert!((program_change_to_normalized(0, 3) - 0.0).abs() < 1e-9);
        assert!((program_change_to_normalized(1, 3) - (1.0 / 3.0)).abs() < 1e-9);
        assert!((program_change_to_normalized(2, 3) - (2.0 / 3.0)).abs() < 1e-9);
        assert!((program_change_to_normalized(3, 3) - 1.0).abs() < 1e-9);
        // Out-of-range program clamps to the last step
        assert!((program_change_to_normalized(99, 3) - 1.0).abs() < 1e-9);
        // Continuous / unspecified step_count (0) maps 0..=127 -> 0..=1
        assert!((program_change_to_normalized(0, 0) - 0.0).abs() < 1e-9);
        assert!((program_change_to_normalized(127, 0) - 1.0).abs() < 1e-9);
    }

    /// Ardour #111176: MIDI Program Change (0xC0) translates to the plugin's
    /// `kIsProgramChange` VST3 parameter and propagates to host state.
    #[test]
    fn vst3_midi_program_change_translates_to_program_change_param() {
        let (bundle, synth_cid, _) = build_fixture_bundle("progchg").unwrap();
        let (mut v, mut m) = Vst3VoiceMain::load(&synth_cid, 48_000.0).unwrap();
        let mut out = vec![0.0f32; BLOCK * 2];
        let tr = Transport::default();

        let init_val = m.param_value(2).unwrap();
        assert_eq!(m.param_text(2, init_val), "Init");

        // Program 2 ("Pad") on channel 0 -> normalized 2/3.
        v.render_block(BLOCK, None, &mut out, &tr, &[VoiceCmd::Midi([0xC0, 2, 0])]);
        let val = m.param_value(2).unwrap();
        assert!((val - (2.0 / 3.0)).abs() < 1e-4, "expected program 2 -> 2/3, got {val}");
        assert_eq!(m.param_text(2, val), "Pad");

        // Program 3 ("Lead") on channel 4 -> normalized 1.0.
        v.render_block(BLOCK, None, &mut out, &tr, &[VoiceCmd::Midi([0xC4, 3, 0])]);
        let val3 = m.param_value(2).unwrap();
        assert!((val3 - 1.0).abs() < 1e-4, "expected program 3 -> 1.0, got {val3}");
        assert_eq!(m.param_text(2, val3), "Lead");

        m.retire();
        let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
    }
}
