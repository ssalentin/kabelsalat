//! Audio output backends: native PipeWire, JACK (`pipewire-jack` / `jackd`),
//! and ALSA (`cpal`), with runtime backend switching (#349).
//!
//! Native PipeWire (`libpipewire-0.3.so.0`) and JACK (`libjack.so.0`, plus
//! PipeWire's private `pipewire-0.3/jack/libjack.so.0` paths) are loaded at
//! runtime via `libc::dlopen` so the binary has zero new link-time shared
//! library dependencies across tarballs, AppImages, Flatpak, and pure-ALSA
//! systems.
//!
//! RT discipline: all audio process callbacks are allocation- and lock-free.

use std::ffi::{c_char, c_int, c_ulong, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio_out::OutputDevice;

/// User-selectable audio backend preference (persisted in `config.json` as
/// `audio_backend` and overridable via `KABELSALAT_AUDIO_BACKEND`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AudioBackendChoice {
    /// Try native PipeWire, then JACK (`pipewire-jack`), then ALSA.
    #[default]
    Auto,
    /// Native PipeWire (`libpipewire-0.3.so.0`): creates a routable PipeWire
    /// audio node directly without requiring `pipewire-alsa`.
    PipeWire,
    /// JACK (`libjack.so.0` / `pipewire-jack`): registers a JACK client with
    /// `out_1`/`out_2` ports and auto-connects to physical playback ports.
    Jack,
    /// ALSA via `cpal`, with direct ALSA PCM device selection.
    Alsa,
}

impl AudioBackendChoice {
    pub const CHOICES: &'static [AudioBackendChoice] = &[
        AudioBackendChoice::Auto,
        AudioBackendChoice::PipeWire,
        AudioBackendChoice::Jack,
        AudioBackendChoice::Alsa,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::PipeWire => "pipewire",
            Self::Jack => "jack",
            Self::Alsa => "alsa",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::PipeWire => "PipeWire",
            Self::Jack => "JACK (pipewire-jack)",
            Self::Alsa => "ALSA",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "pipewire" | "pw" => Some(Self::PipeWire),
            "jack" | "pipewire-jack" | "pw-jack" => Some(Self::Jack),
            "alsa" => Some(Self::Alsa),
            _ => None,
        }
    }
}

/// Which backend is actually driving the live audio output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveBackend {
    PipeWire,
    Jack,
    Alsa,
}

impl ActiveBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PipeWire => "pipewire",
            Self::Jack => "jack",
            Self::Alsa => "alsa",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PipeWire => "PipeWire",
            Self::Jack => "JACK",
            Self::Alsa => "ALSA",
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::PipeWire => 1,
            Self::Jack => 2,
            Self::Alsa => 3,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::PipeWire),
            2 => Some(Self::Jack),
            3 => Some(Self::Alsa),
            _ => None,
        }
    }
}

/// Render callback invoked from the RT audio thread:
/// `(interleaved_out, channels, optional_output_latency_ns)`.
pub type RenderCb = Box<dyn FnMut(&mut [f32], usize, Option<u64>) + Send + 'static>;
/// Error / xrun callback invoked when the stream reports an error or xrun.
pub type ErrorCb = Box<dyn FnMut(String) + Send + 'static>;

/// A live output stream owned by `AudioStreamHost`. Dropping it stops audio
/// cleanly and joins its audio thread.
pub enum OutputStream {
    PipeWire(PipeWireStream),
    Jack(JackStream),
    Alsa(cpal::Stream),
}

/// Result of opening a live output stream.
pub struct OpenedStream {
    pub stream: OutputStream,
    pub backend: ActiveBackend,
    pub sample_rate: f64,
    pub buffer_frames: u32,
    pub channels: usize,
    pub device_label: String,
}

/// Resolve the effective backend choice, honoring `KABELSALAT_AUDIO_BACKEND`
/// when set.
pub fn effective_choice(persisted: AudioBackendChoice) -> AudioBackendChoice {
    if let Ok(v) = std::env::var("KABELSALAT_AUDIO_BACKEND") {
        if let Some(c) = AudioBackendChoice::parse(&v) {
            return c;
        }
    }
    persisted
}

/// Probe a sensible initial sample rate before constructing `GraphEngine` and
/// startup CLAP voices.
pub fn probe_sample_rate(choice: AudioBackendChoice, target_device: Option<&str>) -> f64 {
    let choice = effective_choice(choice);
    match choice {
        AudioBackendChoice::Jack => probe_jack_sample_rate().unwrap_or(48000.0),
        AudioBackendChoice::Alsa => probe_alsa_sample_rate(target_device).unwrap_or(48000.0),
        AudioBackendChoice::PipeWire | AudioBackendChoice::Auto => {
            if pw_api().is_ok() {
                // PipeWire natively runs at 48 kHz by default and resamples seamlessly
                // if a sink uses another rate.
                48000.0
            } else if let Some(sr) = probe_jack_sample_rate() {
                sr
            } else {
                probe_alsa_sample_rate(target_device).unwrap_or(48000.0)
            }
        }
    }
}

/// Open an audio output stream on the requested backend.
///
/// - `Auto`: tries PipeWire → JACK (`pipewire-jack`) → ALSA, returning the
///   first that succeeds (or the combined error reasons if all fail).
/// - Explicit `PipeWire`, `Jack`, or `Alsa`: tries only that backend and
///   returns a clear diagnostic error on failure.
pub fn open_stream(
    choice: AudioBackendChoice,
    target_device: Option<&str>,
    expected_rate: Option<f64>,
    buffer_frames: u32,
    render: RenderCb,
    on_error: ErrorCb,
) -> anyhow::Result<OpenedStream> {
    let choice = effective_choice(choice);
    match choice {
        AudioBackendChoice::PipeWire => {
            open_pipewire(target_device, expected_rate, buffer_frames, render, on_error)
        }
        AudioBackendChoice::Jack => {
            open_jack(target_device, expected_rate, buffer_frames, render, on_error)
        }
        AudioBackendChoice::Alsa => {
            open_alsa(target_device, expected_rate, buffer_frames, render, on_error)
        }
        AudioBackendChoice::Auto => {
            // Wrap callbacks so we can retry across backends without moving them on error.
            let render = Arc::new(Mutex::new(render));
            let on_error = Arc::new(Mutex::new(on_error));
            let mut errs = Vec::new();

            let pw_render = render.clone();
            let pw_err = on_error.clone();
            match open_pipewire(
                target_device,
                expected_rate,
                buffer_frames,
                Box::new(move |buf, ch, lat| {
                    if let Ok(mut f) = pw_render.try_lock() {
                        f(buf, ch, lat);
                    } else {
                        buf.fill(0.0);
                    }
                }),
                Box::new(move |msg| {
                    if let Ok(mut f) = pw_err.try_lock() {
                        f(msg);
                    }
                }),
            ) {
                Ok(opened) => return Ok(opened),
                Err(e) => errs.push(format!("PipeWire: {e}")),
            }

            let jack_render = render.clone();
            let jack_err = on_error.clone();
            match open_jack(
                target_device,
                expected_rate,
                buffer_frames,
                Box::new(move |buf, ch, lat| {
                    if let Ok(mut f) = jack_render.try_lock() {
                        f(buf, ch, lat);
                    } else {
                        buf.fill(0.0);
                    }
                }),
                Box::new(move |msg| {
                    if let Ok(mut f) = jack_err.try_lock() {
                        f(msg);
                    }
                }),
            ) {
                Ok(opened) => return Ok(opened),
                Err(e) => errs.push(format!("JACK: {e}")),
            }

            let alsa_render = render;
            let alsa_err = on_error;
            match open_alsa(
                target_device,
                expected_rate,
                buffer_frames,
                Box::new(move |buf, ch, lat| {
                    if let Ok(mut f) = alsa_render.try_lock() {
                        f(buf, ch, lat);
                    } else {
                        buf.fill(0.0);
                    }
                }),
                Box::new(move |msg| {
                    if let Ok(mut f) = alsa_err.try_lock() {
                        f(msg);
                    }
                }),
            ) {
                Ok(opened) => Ok(opened),
                Err(e) => {
                    errs.push(format!("ALSA: {e}"));
                    anyhow::bail!("no audio backend available ({})", errs.join("; "))
                }
            }
        }
    }
}

// ===========================================================================
// 1. Native PipeWire backend (dlopen libpipewire-0.3.so.0)
// ===========================================================================

const PW_VERSION_STREAM_EVENTS: u32 = 2;
const PW_DIRECTION_OUTPUT: u32 = 1;
const PW_ID_ANY: u32 = 0xffff_ffff;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_STREAM_FLAG_RT_PROCESS: u32 = 1 << 4;

const PW_STREAM_STATE_ERROR: i32 = -1;
const PW_STREAM_STATE_PAUSED: i32 = 2;
const PW_STREAM_STATE_STREAMING: i32 = 3;

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_AUDIO_FORMAT: u32 = 0x10001;
const SPA_FORMAT_AUDIO_RATE: u32 = 0x10003;
const SPA_FORMAT_AUDIO_CHANNELS: u32 = 0x10004;
const SPA_MEDIA_TYPE_AUDIO: u32 = 1;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
#[cfg(target_endian = "little")]
const SPA_AUDIO_FORMAT_F32: u32 = 0x11b;
#[cfg(target_endian = "big")]
const SPA_AUDIO_FORMAT_F32: u32 = 0x11c;

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaData {
    r#type: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
    time: u64,
}

#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<extern "C" fn(*mut c_void)>,
    state_changed: Option<extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<extern "C" fn(*mut c_void)>,
    drained: Option<extern "C" fn(*mut c_void)>,
    command: Option<extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<extern "C" fn(*mut c_void)>,
}

/// 8-byte aligned binary `SPA_PARAM_EnumFormat` pod for stereo f32 at `rate` Hz.
#[repr(C, align(8))]
struct SpaAudioFormatPod {
    // spa_pod header (8 bytes): body size = 8 (object body) + 5 * 24 (props) = 128
    size: u32,
    pod_type: u32,
    // spa_pod_object_body (8 bytes)
    obj_type: u32,
    obj_id: u32,
    // 5 x spa_pod_prop (each 24 bytes: key, flags, val.size, val.type, val, pad)
    props: [[u32; 6]; 5],
}

impl SpaAudioFormatPod {
    fn stereo_f32(rate: u32) -> Self {
        let prop = |key: u32, ty: u32, val: u32| [key, 0, 4, ty, val, 0];
        Self {
            size: 8 + 5 * 24,
            pod_type: SPA_TYPE_OBJECT,
            obj_type: SPA_TYPE_OBJECT_FORMAT,
            obj_id: SPA_PARAM_ENUM_FORMAT,
            props: [
                prop(SPA_FORMAT_MEDIA_TYPE, SPA_TYPE_ID, SPA_MEDIA_TYPE_AUDIO),
                prop(SPA_FORMAT_MEDIA_SUBTYPE, SPA_TYPE_ID, SPA_MEDIA_SUBTYPE_RAW),
                prop(SPA_FORMAT_AUDIO_FORMAT, SPA_TYPE_ID, SPA_AUDIO_FORMAT_F32),
                prop(SPA_FORMAT_AUDIO_RATE, SPA_TYPE_INT, rate),
                prop(SPA_FORMAT_AUDIO_CHANNELS, SPA_TYPE_INT, 2),
            ],
        }
    }
}

struct PwApi {
    pw_init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    pw_thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    pw_thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pw_thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    pw_thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    pw_properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    pw_properties_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    pw_stream_new_simple: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        *const PwStreamEvents,
        *mut c_void,
    ) -> *mut c_void,
    pw_stream_connect: unsafe extern "C" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        *mut *const c_void,
        u32,
    ) -> c_int,
    pw_stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    pw_stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    pw_stream_destroy: unsafe extern "C" fn(*mut c_void),
}

unsafe impl Send for PwApi {}
unsafe impl Sync for PwApi {}

unsafe fn load_sym<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, String> {
    let ptr = libc::dlsym(handle, name.as_ptr());
    if ptr.is_null() {
        return Err(format!("missing symbol {}", name.to_string_lossy()));
    }
    Ok(std::mem::transmute_copy(&ptr))
}

fn pw_api() -> Result<&'static PwApi, String> {
    static API: OnceLock<Result<PwApi, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let handle = libc::dlopen(c"libpipewire-0.3.so.0".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            return Err("libpipewire-0.3.so.0 not found".into());
        }
        let api = PwApi {
            pw_init: load_sym(handle, c"pw_init")?,
            pw_thread_loop_new: load_sym(handle, c"pw_thread_loop_new")?,
            pw_thread_loop_get_loop: load_sym(handle, c"pw_thread_loop_get_loop")?,
            pw_thread_loop_start: load_sym(handle, c"pw_thread_loop_start")?,
            pw_thread_loop_stop: load_sym(handle, c"pw_thread_loop_stop")?,
            pw_thread_loop_lock: load_sym(handle, c"pw_thread_loop_lock")?,
            pw_thread_loop_unlock: load_sym(handle, c"pw_thread_loop_unlock")?,
            pw_thread_loop_destroy: load_sym(handle, c"pw_thread_loop_destroy")?,
            pw_properties_new_string: load_sym(handle, c"pw_properties_new_string")?,
            pw_properties_set: load_sym(handle, c"pw_properties_set")?,
            pw_stream_new_simple: load_sym(handle, c"pw_stream_new_simple")?,
            pw_stream_connect: load_sym(handle, c"pw_stream_connect")?,
            pw_stream_dequeue_buffer: load_sym(handle, c"pw_stream_dequeue_buffer")?,
            pw_stream_queue_buffer: load_sym(handle, c"pw_stream_queue_buffer")?,
            pw_stream_destroy: load_sym(handle, c"pw_stream_destroy")?,
        };
        (api.pw_init)(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(api)
    })
    .as_ref()
    .map_err(Clone::clone)
}

struct PwState {
    api: &'static PwApi,
    stream: *mut c_void,
    render: RenderCb,
    on_error: ErrorCb,
    default_frames: usize,
    sample_rate: f64,
    state: Arc<AtomicI32>,
    last_err: Arc<Mutex<Option<String>>>,
    pinned: bool,
}

unsafe impl Send for PwState {}

extern "C" fn pw_state_changed_cb(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    if data.is_null() {
        return;
    }
    let st = unsafe { &mut *(data as *mut PwState) };
    st.state.store(state, Ordering::Release);
    if state == PW_STREAM_STATE_ERROR {
        let msg = if error.is_null() {
            "PipeWire stream error".to_string()
        } else {
            unsafe { CStr::from_ptr(error).to_string_lossy().into_owned() }
        };
        if let Ok(mut e) = st.last_err.try_lock() {
            *e = Some(msg.clone());
        }
        (st.on_error)(msg);
    }
}

extern "C" fn pw_process_cb(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let st = unsafe { &mut *(data as *mut PwState) };
    if st.stream.is_null() {
        return;
    }
    if !st.pinned {
        st.pinned = true;
        pin_current_thread_to_p_cores();
    }
    unsafe {
        let pw_buf = (st.api.pw_stream_dequeue_buffer)(st.stream);
        if pw_buf.is_null() {
            return;
        }
        let spa_buf = (*pw_buf).buffer;
        if !spa_buf.is_null() && (*spa_buf).n_datas > 0 && !(*spa_buf).datas.is_null() {
            let d = &mut *(*spa_buf).datas;
            if !d.data.is_null() && !d.chunk.is_null() {
                const CH: usize = 2;
                const STRIDE: usize = CH * std::mem::size_of::<f32>();
                let max_frames = (d.maxsize as usize) / STRIDE;
                let req = (*pw_buf).requested as usize;
                let frames = if req > 0 {
                    req.min(max_frames)
                } else {
                    st.default_frames.min(max_frames)
                };
                if frames > 0 {
                    let out = std::slice::from_raw_parts_mut(d.data as *mut f32, frames * CH);
                    let lat_ns = ((frames as f64 / st.sample_rate.max(1.0)) * 1e9) as u64;
                    (st.render)(out, CH, Some(lat_ns));
                    (*d.chunk).offset = 0;
                    (*d.chunk).stride = STRIDE as i32;
                    (*d.chunk).size = (frames * STRIDE) as u32;
                }
            }
        }
        (st.api.pw_stream_queue_buffer)(st.stream, pw_buf);
    }
}

pub struct PipeWireStream {
    api: &'static PwApi,
    tloop: *mut c_void,
    stream: *mut c_void,
    _events: Box<PwStreamEvents>,
    state: *mut PwState,
}

unsafe impl Send for PipeWireStream {}

impl Drop for PipeWireStream {
    fn drop(&mut self) {
        unsafe {
            (self.api.pw_thread_loop_lock)(self.tloop);
            if !self.stream.is_null() {
                (self.api.pw_stream_destroy)(self.stream);
                self.stream = std::ptr::null_mut();
            }
            (self.api.pw_thread_loop_unlock)(self.tloop);
            (self.api.pw_thread_loop_stop)(self.tloop);
            (self.api.pw_thread_loop_destroy)(self.tloop);
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
                self.state = std::ptr::null_mut();
            }
        }
    }
}

fn looks_like_alsa_pcm(name: &str) -> bool {
    name == "default"
        || name == "pipewire"
        || name == "pulse"
        || name.starts_with("hw:")
        || name.starts_with("plughw:")
        || name.starts_with("sysdefault:")
        || name.starts_with("front:")
        || name.starts_with("dmix:")
}

fn open_pipewire(
    target_device: Option<&str>,
    expected_rate: Option<f64>,
    buffer_frames: u32,
    render: RenderCb,
    on_error: ErrorCb,
) -> anyhow::Result<OpenedStream> {
    let api = pw_api().map_err(|e| anyhow::anyhow!(e))?;
    let sample_rate = expected_rate.unwrap_or(48000.0).max(8000.0);
    let rate_u32 = sample_rate.round() as u32;
    let buf_frames = buffer_frames.max(64);

    unsafe {
        let tloop = (api.pw_thread_loop_new)(c"kabelsalat-pw".as_ptr(), std::ptr::null());
        if tloop.is_null() {
            anyhow::bail!("pw_thread_loop_new failed");
        }
        let loop_ptr = (api.pw_thread_loop_get_loop)(tloop);
        let props = (api.pw_properties_new_string)(c"".as_ptr());
        if props.is_null() {
            (api.pw_thread_loop_destroy)(tloop);
            anyhow::bail!("pw_properties_new_string failed");
        }
        let set_prop = |k: &CStr, v: &str| {
            if let Ok(cv) = CString::new(v) {
                (api.pw_properties_set)(props, k.as_ptr(), cv.as_ptr());
            }
        };
        set_prop(c"media.type", "Audio");
        set_prop(c"media.category", "Playback");
        set_prop(c"media.role", "Music");
        set_prop(c"application.name", "Kabelsalat");
        set_prop(c"node.name", "kabelsalat");
        set_prop(c"node.description", "Kabelsalat");
        set_prop(c"node.latency", &format!("{buf_frames}/{rate_u32}"));
        if let Some(sink) = target_device.filter(|s| !s.is_empty() && !looks_like_alsa_pcm(s)) {
            set_prop(c"target.object", sink);
        }

        let events = Box::new(PwStreamEvents {
            version: PW_VERSION_STREAM_EVENTS,
            destroy: None,
            state_changed: Some(pw_state_changed_cb),
            control_info: None,
            io_changed: None,
            param_changed: None,
            add_buffer: None,
            remove_buffer: None,
            process: Some(pw_process_cb),
            drained: None,
            command: None,
            trigger_done: None,
        });

        let state_flag = Arc::new(AtomicI32::new(0));
        let last_err = Arc::new(Mutex::new(None));
        let state_ptr = Box::into_raw(Box::new(PwState {
            api,
            stream: std::ptr::null_mut(),
            render,
            on_error,
            default_frames: buf_frames as usize,
            sample_rate,
            state: state_flag.clone(),
            last_err: last_err.clone(),
            pinned: false,
        }));

        let stream = (api.pw_stream_new_simple)(
            loop_ptr,
            c"Kabelsalat".as_ptr(),
            props,
            &*events as *const PwStreamEvents,
            state_ptr as *mut c_void,
        );
        if stream.is_null() {
            drop(Box::from_raw(state_ptr));
            (api.pw_thread_loop_destroy)(tloop);
            anyhow::bail!("pw_stream_new_simple failed (is the PipeWire daemon running?)");
        }
        (*state_ptr).stream = stream;

        let pod = SpaAudioFormatPod::stereo_f32(rate_u32);
        let mut param_ptr: *const c_void = &pod as *const SpaAudioFormatPod as *const c_void;
        let flags = PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS;
        let rc = (api.pw_stream_connect)(
            stream,
            PW_DIRECTION_OUTPUT,
            PW_ID_ANY,
            flags,
            &mut param_ptr,
            1,
        );
        if rc < 0 {
            (api.pw_stream_destroy)(stream);
            drop(Box::from_raw(state_ptr));
            (api.pw_thread_loop_destroy)(tloop);
            anyhow::bail!("pw_stream_connect failed (errno {})", -rc);
        }

        if (api.pw_thread_loop_start)(tloop) < 0 {
            (api.pw_stream_destroy)(stream);
            drop(Box::from_raw(state_ptr));
            (api.pw_thread_loop_destroy)(tloop);
            anyhow::bail!("pw_thread_loop_start failed");
        }

        let pw_stream = PipeWireStream {
            api,
            tloop,
            stream,
            _events: events,
            state: state_ptr,
        };

        // Wait briefly for PipeWire to confirm connection (or report an error).
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            let st = state_flag.load(Ordering::Acquire);
            if st == PW_STREAM_STATE_PAUSED || st == PW_STREAM_STATE_STREAMING {
                break;
            }
            if st == PW_STREAM_STATE_ERROR {
                let msg = last_err
                    .lock()
                    .ok()
                    .and_then(|e| e.clone())
                    .unwrap_or_else(|| "PipeWire stream error".into());
                drop(pw_stream);
                anyhow::bail!(msg);
            }
            if Instant::now() >= deadline {
                drop(pw_stream);
                anyhow::bail!("timed out connecting to PipeWire daemon");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        eprintln!(
            "audio: host=PipeWire node=kabelsalat rate={} ch=2 buffer={} frames ({:.1} ms)",
            sample_rate,
            buf_frames,
            buf_frames as f64 / sample_rate * 1000.0,
        );

        Ok(OpenedStream {
            stream: OutputStream::PipeWire(pw_stream),
            backend: ActiveBackend::PipeWire,
            sample_rate,
            buffer_frames: buf_frames,
            channels: 2,
            device_label: target_device
                .filter(|s| !s.is_empty() && !looks_like_alsa_pcm(s))
                .unwrap_or("PipeWire default")
                .to_string(),
        })
    }
}

// ===========================================================================
// 2. JACK / pipewire-jack backend (dlopen libjack.so.0)
// ===========================================================================

const JACK_NO_START_SERVER: c_int = 0x01;
const JACK_PORT_IS_INPUT: c_ulong = 0x01;
const JACK_PORT_IS_OUTPUT: c_ulong = 0x02;
const JACK_PORT_IS_PHYSICAL: c_ulong = 0x04;
const JACK_DEFAULT_AUDIO_TYPE: &CStr = c"32 bit float mono audio";

/// Candidate `libjack.so.0` paths, including PipeWire's private JACK library
/// locations so `pipewire-jack` works even without launching via `pw-jack`.
const JACK_LIB_CANDIDATES: &[&CStr] = &[
    c"libjack.so.0",
    c"/usr/lib/x86_64-linux-gnu/pipewire-0.3/jack/libjack.so.0",
    c"/usr/lib/aarch64-linux-gnu/pipewire-0.3/jack/libjack.so.0",
    c"/usr/lib64/pipewire-0.3/jack/libjack.so.0",
    c"/usr/lib/pipewire-0.3/jack/libjack.so.0",
    c"libjack.so",
];

struct JackApi {
    client_open: unsafe extern "C" fn(*const c_char, c_int, *mut c_int, ...) -> *mut c_void,
    client_close: unsafe extern "C" fn(*mut c_void) -> c_int,
    get_sample_rate: unsafe extern "C" fn(*mut c_void) -> u32,
    get_buffer_size: unsafe extern "C" fn(*mut c_void) -> u32,
    port_register: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_ulong, c_ulong) -> *mut c_void,
    port_get_buffer: unsafe extern "C" fn(*mut c_void, u32) -> *mut c_void,
    port_name: unsafe extern "C" fn(*const c_void) -> *const c_char,
    get_ports: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_ulong) -> *mut *const c_char,
    connect: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
    set_process_callback: unsafe extern "C" fn(*mut c_void, extern "C" fn(u32, *mut c_void) -> c_int, *mut c_void) -> c_int,
    set_xrun_callback: unsafe extern "C" fn(*mut c_void, extern "C" fn(*mut c_void) -> c_int, *mut c_void) -> c_int,
    on_shutdown: unsafe extern "C" fn(*mut c_void, extern "C" fn(*mut c_void), *mut c_void),
    activate: unsafe extern "C" fn(*mut c_void) -> c_int,
    deactivate: unsafe extern "C" fn(*mut c_void) -> c_int,
}

unsafe impl Send for JackApi {}
unsafe impl Sync for JackApi {}

fn jack_api() -> Result<&'static JackApi, String> {
    static API: OnceLock<Result<JackApi, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let mut handle = std::ptr::null_mut();
        for &path in JACK_LIB_CANDIDATES {
            let h = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if !h.is_null() {
                handle = h;
                break;
            }
        }
        if handle.is_null() {
            return Err("libjack.so.0 / pipewire-jack not installed".into());
        }
        Ok(JackApi {
            client_open: load_sym(handle, c"jack_client_open")?,
            client_close: load_sym(handle, c"jack_client_close")?,
            get_sample_rate: load_sym(handle, c"jack_get_sample_rate")?,
            get_buffer_size: load_sym(handle, c"jack_get_buffer_size")?,
            port_register: load_sym(handle, c"jack_port_register")?,
            port_get_buffer: load_sym(handle, c"jack_port_get_buffer")?,
            port_name: load_sym(handle, c"jack_port_name")?,
            get_ports: load_sym(handle, c"jack_get_ports")?,
            connect: load_sym(handle, c"jack_connect")?,
            free: load_sym(handle, c"jack_free")?,
            set_process_callback: load_sym(handle, c"jack_set_process_callback")?,
            set_xrun_callback: load_sym(handle, c"jack_set_xrun_callback")?,
            on_shutdown: load_sym(handle, c"jack_on_shutdown")?,
            activate: load_sym(handle, c"jack_activate")?,
            deactivate: load_sym(handle, c"jack_deactivate")?,
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

fn probe_jack_sample_rate() -> Option<f64> {
    let api = jack_api().ok()?;
    unsafe {
        let mut status: c_int = 0;
        let client = (api.client_open)(c"Kabelsalat-probe".as_ptr(), JACK_NO_START_SERVER, &mut status);
        if client.is_null() {
            return None;
        }
        let sr = (api.get_sample_rate)(client);
        (api.client_close)(client);
        (sr > 0).then_some(sr as f64)
    }
}

struct JackState {
    api: &'static JackApi,
    out_l: *mut c_void,
    out_r: *mut c_void,
    scratch: Vec<f32>,
    sample_rate: f64,
    render: RenderCb,
    on_error: ErrorCb,
    shutdown: AtomicBool,
    pinned: bool,
}

unsafe impl Send for JackState {}

extern "C" fn jack_process_cb(nframes: u32, arg: *mut c_void) -> c_int {
    if arg.is_null() {
        return 0;
    }
    let st = unsafe { &mut *(arg as *mut JackState) };
    if st.shutdown.load(Ordering::Relaxed) {
        return 0;
    }
    if !st.pinned {
        st.pinned = true;
        pin_current_thread_to_p_cores();
    }
    let n = nframes as usize;
    if n == 0 {
        return 0;
    }
    unsafe {
        let l_ptr = (st.api.port_get_buffer)(st.out_l, nframes) as *mut f32;
        let r_ptr = (st.api.port_get_buffer)(st.out_r, nframes) as *mut f32;
        if l_ptr.is_null() || r_ptr.is_null() {
            return 0;
        }
        let out_l = std::slice::from_raw_parts_mut(l_ptr, n);
        let out_r = std::slice::from_raw_parts_mut(r_ptr, n);
        let lat_ns = ((n as f64 / st.sample_rate.max(1.0)) * 1e9) as u64;
        let max_chunk = (st.scratch.len() / 2).max(1);
        let mut off = 0;
        while off < n {
            let chunk = (n - off).min(max_chunk);
            let buf = &mut st.scratch[..chunk * 2];
            (st.render)(buf, 2, Some(lat_ns));
            for i in 0..chunk {
                out_l[off + i] = buf[i * 2];
                out_r[off + i] = buf[i * 2 + 1];
            }
            off += chunk;
        }
    }
    0
}

extern "C" fn jack_xrun_cb(arg: *mut c_void) -> c_int {
    if !arg.is_null() {
        let st = unsafe { &mut *(arg as *mut JackState) };
        (st.on_error)("JACK xrun".into());
    }
    0
}

extern "C" fn jack_shutdown_cb(arg: *mut c_void) {
    if !arg.is_null() {
        let st = unsafe { &mut *(arg as *mut JackState) };
        st.shutdown.store(true, Ordering::Relaxed);
        (st.on_error)("JACK server shut down".into());
    }
}

pub struct JackStream {
    api: &'static JackApi,
    client: *mut c_void,
    state: *mut JackState,
}

unsafe impl Send for JackStream {}

impl Drop for JackStream {
    fn drop(&mut self) {
        unsafe {
            if !self.client.is_null() {
                (self.api.deactivate)(self.client);
                (self.api.client_close)(self.client);
                self.client = std::ptr::null_mut();
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
                self.state = std::ptr::null_mut();
            }
        }
    }
}

fn open_jack(
    _target_device: Option<&str>,
    _expected_rate: Option<f64>,
    _buffer_frames: u32,
    render: RenderCb,
    on_error: ErrorCb,
) -> anyhow::Result<OpenedStream> {
    let api = jack_api().map_err(|e| anyhow::anyhow!(e))?;
    unsafe {
        let mut status: c_int = 0;
        let client = (api.client_open)(c"Kabelsalat".as_ptr(), JACK_NO_START_SERVER, &mut status);
        if client.is_null() {
            anyhow::bail!("could not connect to JACK / pipewire-jack server (status 0x{status:x})");
        }
        let sr = (api.get_sample_rate)(client).max(8000) as f64;
        let buf = (api.get_buffer_size)(client).max(32);

        let out_l = (api.port_register)(
            client,
            c"out_1".as_ptr(),
            JACK_DEFAULT_AUDIO_TYPE.as_ptr(),
            JACK_PORT_IS_OUTPUT,
            0,
        );
        let out_r = (api.port_register)(
            client,
            c"out_2".as_ptr(),
            JACK_DEFAULT_AUDIO_TYPE.as_ptr(),
            JACK_PORT_IS_OUTPUT,
            0,
        );
        if out_l.is_null() || out_r.is_null() {
            (api.client_close)(client);
            anyhow::bail!("jack_port_register failed");
        }

        let scratch_frames = (buf as usize).max(8192);
        let state_ptr = Box::into_raw(Box::new(JackState {
            api,
            out_l,
            out_r,
            scratch: vec![0.0; scratch_frames * 2],
            sample_rate: sr,
            render,
            on_error,
            shutdown: AtomicBool::new(false),
            pinned: false,
        }));

        (api.set_process_callback)(client, jack_process_cb, state_ptr as *mut c_void);
        (api.set_xrun_callback)(client, jack_xrun_cb, state_ptr as *mut c_void);
        (api.on_shutdown)(client, jack_shutdown_cb, state_ptr as *mut c_void);

        if (api.activate)(client) != 0 {
            (api.client_close)(client);
            drop(Box::from_raw(state_ptr));
            anyhow::bail!("jack_activate failed");
        }

        // Auto-connect out_1 / out_2 to the first two physical playback ports.
        let ports = (api.get_ports)(
            client,
            std::ptr::null(),
            JACK_DEFAULT_AUDIO_TYPE.as_ptr(),
            JACK_PORT_IS_PHYSICAL | JACK_PORT_IS_INPUT,
        );
        if !ports.is_null() {
            let src_l = (api.port_name)(out_l);
            let src_r = (api.port_name)(out_r);
            let dst_0 = *ports;
            if !dst_0.is_null() {
                if !src_l.is_null() {
                    (api.connect)(client, src_l, dst_0);
                }
                let dst_1 = *ports.add(1);
                if !dst_1.is_null() && !src_r.is_null() {
                    (api.connect)(client, src_r, dst_1);
                }
            }
            (api.free)(ports as *mut c_void);
        }

        eprintln!(
            "audio: host=JACK client=Kabelsalat rate={} ch=2 buffer={} frames ({:.1} ms)",
            sr,
            buf,
            buf as f64 / sr * 1000.0,
        );

        Ok(OpenedStream {
            stream: OutputStream::Jack(JackStream {
                api,
                client,
                state: state_ptr,
            }),
            backend: ActiveBackend::Jack,
            sample_rate: sr,
            buffer_frames: buf,
            channels: 2,
            device_label: "JACK (Kabelsalat:out_1/2)".into(),
        })
    }
}

// ===========================================================================
// 3. ALSA backend (cpal)
// ===========================================================================

fn alsa_device_pcm_id(dev: &cpal::Device) -> Option<String> {
    if let Ok(id) = dev.id() {
        let s = id.to_string();
        let pcm = s.strip_prefix("alsa:").unwrap_or(&s).trim();
        if !pcm.is_empty() {
            return Some(pcm.to_string());
        }
    }
    dev.description().ok().map(|d| d.name().to_string())
}

/// Enumerate ALSA output PCMs for direct ALSA backend selection.
/// Runs blocking ALSA queries — call from the scan thread, never per UI frame.
pub fn list_alsa_output_devices() -> Vec<OutputDevice> {
    let host = cpal::default_host();
    let default_id = host
        .default_output_device()
        .and_then(|d| alsa_device_pcm_id(&d))
        .unwrap_or_else(|| "default".into());
    let Ok(devs) = host.output_devices() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dev in devs {
        let Some(raw) = alsa_device_pcm_id(&dev) else { continue };
        if !keep_alsa_pcm(&raw) {
            continue;
        }
        let desc = dev
            .description()
            .ok()
            .map(|d| d.name().to_string())
            .filter(|s| !s.is_empty() && s != &raw);
        let is_default = raw == default_id || raw == "default";
        let label = match desc {
            Some(d) if raw != "default" && raw != "pipewire" && raw != "pulse" => format!("{d} ({raw})"),
            _ => pretty_alsa_name(&raw),
        };
        out.push(OutputDevice {
            raw,
            name: label,
            default: is_default,
        });
    }
    out.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
    out.dedup_by(|a, b| a.raw == b.raw);
    out
}

/// Keep useful playback PCMs and drop noisy surround/IEC958/null virtual aliases.
pub fn keep_alsa_pcm(raw: &str) -> bool {
    if raw.is_empty() || raw == "null" {
        return false;
    }
    for skip in [
        "surround21:",
        "surround40:",
        "surround41:",
        "surround50:",
        "surround51:",
        "surround71:",
        "iec958:",
        "spdif:",
        "dsnoop:",
        "usbstream:",
        "speex",
        "samplerate",
        "upmix",
        "vdownmix",
        "lavrate",
    ] {
        if raw.starts_with(skip) {
            return false;
        }
    }
    true
}

pub fn pretty_alsa_name(raw: &str) -> String {
    match raw {
        "default" => "ALSA Default".into(),
        "pipewire" => "ALSA → PipeWire".into(),
        "pulse" => "ALSA → PulseAudio".into(),
        _ => raw.to_string(),
    }
}

fn find_alsa_device(host: &cpal::Host, target: Option<&str>) -> anyhow::Result<cpal::Device> {
    if let Some(name) = target.filter(|s| !s.is_empty()) {
        if let Ok(devs) = host.output_devices() {
            for dev in devs {
                let id_match = alsa_device_pcm_id(&dev).is_some_and(|id| id == name);
                let desc_match = dev.description().ok().is_some_and(|d| d.name() == name);
                if id_match || desc_match {
                    return Ok(dev);
                }
            }
        }
    }
    host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no ALSA output device found"))
}

fn probe_alsa_sample_rate(target: Option<&str>) -> Option<f64> {
    let host = cpal::default_host();
    let dev = find_alsa_device(&host, target).ok()?;
    let cfg = dev.default_output_config().ok()?;
    Some(cfg.sample_rate() as f64)
}

fn open_alsa(
    target_device: Option<&str>,
    expected_rate: Option<f64>,
    buffer_frames: u32,
    mut render: RenderCb,
    mut on_error: ErrorCb,
) -> anyhow::Result<OpenedStream> {
    let host = cpal::default_host();
    let device = find_alsa_device(&host, target_device)?;
    let dev_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "default".into());
    let cfg = device.default_output_config()?;
    let sample_rate = expected_rate.unwrap_or(cfg.sample_rate() as f64);
    let ch = cfg.channels() as usize;
    let mut config: cpal::StreamConfig = cfg.into();
    config.sample_rate = sample_rate as u32;
    config.buffer_size = cpal::BufferSize::Fixed(buffer_frames);
    eprintln!(
        "audio: host={} device={} rate={} ch={} buffer={} frames ({:.1} ms)",
        host.id().name(),
        dev_name,
        sample_rate,
        ch,
        buffer_frames,
        buffer_frames as f64 / sample_rate * 1000.0,
    );
    let mut rt_tried = false;
    let mut _rt_handle: Option<audio_thread_priority::RtPriorityHandle> = None;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let d = ts.playback.duration_since(ts.callback);
            let lat_ns = (!d.is_zero()).then_some(d.as_nanos() as u64);
            // One-time realtime promotion via rtkit (first callback only, rule 29).
            if !rt_tried {
                rt_tried = true;
                pin_current_thread_to_p_cores();
                if std::env::var_os("KABELSALAT_NO_RT").is_none() {
                    match audio_thread_priority::promote_current_thread_to_real_time(
                        sample_rate as u32,
                        sample_rate as u32,
                    ) {
                        Ok(h) => {
                            _rt_handle = Some(h);
                            eprintln!("audio: render thread promoted to realtime (rtkit)");
                        }
                        Err(e) => {
                            eprintln!("audio: realtime promotion unavailable ({e}); running at normal priority");
                        }
                    }
                }
            }
            render(data, ch, lat_ns);
        },
        move |e| {
            on_error(format!("{e}"));
        },
        None,
    )?;
    stream.play()?;
    Ok(OpenedStream {
        stream: OutputStream::Alsa(stream),
        backend: ActiveBackend::Alsa,
        sample_rate,
        buffer_frames,
        channels: ch,
        device_label: dev_name,
    })
}

/// Pin the current (audio render) thread to the fastest CPU cores on
/// heterogeneous systems (e.g. Apple M1 Firestorm P-cores vs Icestorm E-cores).
pub(crate) fn pin_current_thread_to_p_cores() {
    #[cfg(target_os = "linux")]
    {
        let mut freqs: Vec<(usize, u64)> = Vec::new();
        for cpu in 0..libc::CPU_SETSIZE as usize {
            let p = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            let Ok(s) = std::fs::read_to_string(&p) else { continue };
            if let Ok(f) = s.trim().parse::<u64>() {
                freqs.push((cpu, f));
            }
        }
        let Some(max_f) = freqs.iter().map(|&(_, f)| f).max() else { return };
        let Some(min_f) = freqs.iter().map(|&(_, f)| f).min() else { return };
        if max_f <= min_f {
            return;
        }
        let p_cores: Vec<usize> = freqs
            .iter()
            .filter(|&&(_, f)| f == max_f)
            .map(|&(c, _)| c)
            .collect();
        if p_cores.is_empty() {
            return;
        }
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            for &c in &p_cores {
                libc::CPU_SET(c, &mut set);
            }
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 {
                eprintln!(
                    "audio: pinned render thread to P-cores {p_cores:?} ({} MHz)",
                    max_f / 1000
                );
            }
        }
    }
}

/// CLI self-test (`kabelsalat audio-backend-test`): verifies that native
/// PipeWire creates a real `kabelsalat` audio node in PipeWire and runs audio
/// callbacks cleanly, and verifies ALSA playback + JACK availability.
pub fn run_test() -> anyhow::Result<()> {
    println!("=== audio-backend-test ===");

    // 1. Native PipeWire test
    let pw_cbs = Arc::new(AtomicU64::new(0));
    let pw_errs = Arc::new(AtomicU64::new(0));
    let cbs = pw_cbs.clone();
    let errs = pw_errs.clone();
    let mut phase = 0.0f64;
    let opened = open_stream(
        AudioBackendChoice::PipeWire,
        None,
        Some(48000.0),
        256,
        Box::new(move |buf, ch, _| {
            cbs.fetch_add(1, Ordering::Relaxed);
            for frame in buf.chunks_mut(ch) {
                phase = (phase + 440.0 / 48000.0).fract();
                let s = (phase * std::f64::consts::TAU).sin() as f32 * 0.01;
                for x in frame {
                    *x = s;
                }
            }
        }),
        Box::new(move |e| {
            errs.fetch_add(1, Ordering::Relaxed);
            eprintln!("pw test error: {e}");
        }),
    )?;
    assert_eq!(opened.backend, ActiveBackend::PipeWire);

    // Check that PipeWire actually shows our node!
    let mut node_seen = false;
    for _ in 0..20 {
        if let Ok(out) = std::process::Command::new("pw-cli").args(["ls", "Node"]).output() {
            let txt = String::from_utf8_lossy(&out.stdout);
            if txt.contains("node.name = \"kabelsalat\"") {
                node_seen = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let n_cb = pw_cbs.load(Ordering::Relaxed);
    let n_err = pw_errs.load(Ordering::Relaxed);
    drop(opened);
    println!("PipeWire: node_seen={node_seen}, callbacks={n_cb}, errors={n_err}");
    anyhow::ensure!(node_seen, "PipeWire did not list a 'kabelsalat' node");
    anyhow::ensure!(n_cb > 0, "PipeWire stream ran 0 callbacks");
    anyhow::ensure!(n_err == 0, "PipeWire stream reported {n_err} errors");

    // 2. ALSA test
    let alsa_devs = list_alsa_output_devices();
    println!("ALSA devices ({}): {:?}", alsa_devs.len(), alsa_devs.iter().map(|d| &d.raw).collect::<Vec<_>>());
    let alsa_cbs = Arc::new(AtomicU64::new(0));
    let cbs = alsa_cbs.clone();
    let opened = open_stream(
        AudioBackendChoice::Alsa,
        None,
        Some(48000.0),
        256,
        Box::new(move |buf, _, _| {
            cbs.fetch_add(1, Ordering::Relaxed);
            buf.fill(0.0);
        }),
        Box::new(|_| {}),
    )?;
    assert_eq!(opened.backend, ActiveBackend::Alsa);
    std::thread::sleep(Duration::from_millis(250));
    let n_alsa = alsa_cbs.load(Ordering::Relaxed);
    drop(opened);
    println!("ALSA: callbacks={n_alsa}");
    anyhow::ensure!(n_alsa > 0, "ALSA stream ran 0 callbacks");

    // 3. JACK probe (optional on machines without pipewire-jack / jackd)
    match jack_api() {
        Ok(_) => println!("JACK library: loaded"),
        Err(e) => println!("JACK library: unavailable ({e})"),
    }

    println!("audio-backend-test: OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_choice_round_trips_and_accepts_aliases() {
        for &c in AudioBackendChoice::CHOICES {
            assert_eq!(AudioBackendChoice::parse(c.as_str()), Some(c));
        }
        assert_eq!(AudioBackendChoice::parse("pw"), Some(AudioBackendChoice::PipeWire));
        assert_eq!(AudioBackendChoice::parse("pipewire-jack"), Some(AudioBackendChoice::Jack));
        assert_eq!(AudioBackendChoice::parse("pw-jack"), Some(AudioBackendChoice::Jack));
        assert_eq!(AudioBackendChoice::parse("ALSA"), Some(AudioBackendChoice::Alsa));
        assert_eq!(AudioBackendChoice::parse("unknown"), None);
    }

    #[test]
    fn active_backend_u8_round_trips() {
        for b in [ActiveBackend::PipeWire, ActiveBackend::Jack, ActiveBackend::Alsa] {
            assert_eq!(ActiveBackend::from_u8(b.to_u8()), Some(b));
        }
        assert_eq!(ActiveBackend::from_u8(0), None);
    }

    #[test]
    fn alsa_pcm_filter_keeps_hardware_and_default_and_drops_surround_noise() {
        assert!(keep_alsa_pcm("default"));
        assert!(keep_alsa_pcm("pipewire"));
        assert!(keep_alsa_pcm("hw:CARD=M2,DEV=0"));
        assert!(keep_alsa_pcm("plughw:CARD=M2,DEV=0"));
        assert!(keep_alsa_pcm("sysdefault:CARD=M2"));
        assert!(!keep_alsa_pcm("null"));
        assert!(!keep_alsa_pcm("surround51:CARD=PCH,DEV=0"));
        assert!(!keep_alsa_pcm("iec958:CARD=PCH,DEV=0"));
        assert!(!keep_alsa_pcm("dsnoop:CARD=PCH,DEV=0"));
    }

    #[test]
    fn spa_audio_format_pod_has_valid_size_and_alignment() {
        let pod = SpaAudioFormatPod::stereo_f32(48000);
        assert_eq!(std::mem::align_of::<SpaAudioFormatPod>(), 8);
        assert_eq!(std::mem::size_of::<SpaAudioFormatPod>(), 8 + 8 + 5 * 24);
        assert_eq!(pod.size, 8 + 5 * 24);
        assert_eq!(pod.pod_type, SPA_TYPE_OBJECT);
        assert_eq!(pod.obj_type, SPA_TYPE_OBJECT_FORMAT);
        assert_eq!(pod.obj_id, SPA_PARAM_ENUM_FORMAT);
        assert_eq!(pod.props[3][4], 48000);
        assert_eq!(pod.props[4][4], 2);
    }
}
