//! A CLAP plugin inside the test binary, written against the C ABI
//! (`clap-sys`), for what no installed plugin shows on demand: it asks the
//! host for callbacks, restarts and flushes when a test tells it to, and
//! records which thread each of its functions was called on. The host loads
//! it through the same `PluginEntry` path as a bundle from disk.
//!
//! As an instrument it puts a constant on its stereo output; `level` is the
//! value. A new activation can change its parameter list (`params`), which
//! is what a restart is for.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO};
use clap_sys::ext::params::{clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE};
use clap_sys::ext::tail::{clap_host_tail, clap_plugin_tail, CLAP_EXT_TAIL};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE};
use clap_sys::version::CLAP_VERSION;

use super::{ClapVoice, ClapVoiceMain};

pub const PROBE_ID: &CStr = c"org.kabelsalat.test-probe";
pub const PROBE_FX_ID: &CStr = c"org.kabelsalat.test-probe#fx";

/// One call the plugin received, with the thread it came on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub what: &'static str,
    pub thread: ThreadId,
}

/// What a test sets and reads on a probe instance.
pub struct Probe {
    calls: Mutex<Vec<Call>>,
    /// `process()` calls so far.
    pub blocks: AtomicU64,
    /// Parameters the next `count()` reports (ids 0..n, range 0..1).
    pub params: AtomicU32,
    /// The value on both output channels (instrument), or the gain applied
    /// to the main input port (effect), as f32 bits.
    level: AtomicU32,
    /// Input ports `(channel_count, is_main)`; empty for an instrument.
    in_ports: Vec<(u32, bool)>,
    /// Output port 0's channel count (1 = mono, 2 = stereo).
    out_channels: u32,
    /// Highest sample magnitude seen on any non-main (sidechain) input port.
    sidechain_peak: AtomicU32,
    /// The next `activate()` answers false.
    pub refuse_activation: AtomicBool,
    /// `clap_process_status` returned by `plugin_process`.
    process_status: AtomicI32,
    /// `clap_plugin_tail.get()` return value in samples (`i32::MAX as u32` = infinite).
    tail_samples: AtomicU32,
    /// Last `clap_process.steady_time` seen by `plugin_process`.
    last_steady_time: AtomicI64,
    /// When true, note-on starts a 440 Hz phase-continuous sine wave until note-off.
    sine_on_note: AtomicBool,
    note_held: AtomicBool,
    sine_phase: AtomicU64,
    host: AtomicPtr<clap_host>,
}

impl Probe {
    fn new() -> Arc<Probe> {
        Self::with_ports(Vec::new(), 2, 0.25)
    }

    fn with_ports(in_ports: Vec<(u32, bool)>, out_channels: u32, level: f32) -> Arc<Probe> {
        Arc::new(Probe {
            calls: Mutex::new(Vec::new()),
            blocks: AtomicU64::new(0),
            params: AtomicU32::new(1),
            level: AtomicU32::new(level.to_bits()),
            in_ports,
            out_channels,
            sidechain_peak: AtomicU32::new(0.0f32.to_bits()),
            refuse_activation: AtomicBool::new(false),
            process_status: AtomicI32::new(CLAP_PROCESS_CONTINUE),
            tail_samples: AtomicU32::new(0),
            last_steady_time: AtomicI64::new(-1),
            sine_on_note: AtomicBool::new(false),
            note_held: AtomicBool::new(false),
            sine_phase: AtomicU64::new(0.0f64.to_bits()),
            host: AtomicPtr::new(std::ptr::null_mut()),
        })
    }

    pub fn set_sine_on_note(&self, on: bool) {
        self.sine_on_note.store(on, Ordering::SeqCst);
        self.note_held.store(false, Ordering::SeqCst);
        self.sine_phase.store(0.0f64.to_bits(), Ordering::SeqCst);
    }

    pub fn process_blocks(&self) -> usize {
        self.blocks.load(Ordering::SeqCst) as usize
    }

    pub fn set_process_status(&self, status: clap_process_status) {
        self.process_status.store(status, Ordering::SeqCst);
    }

    pub fn set_tail_samples(&self, samples: u32) {
        self.tail_samples.store(samples, Ordering::SeqCst);
    }

    pub fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::SeqCst);
    }

    pub fn last_steady_time(&self) -> i64 {
        self.last_steady_time.load(Ordering::SeqCst)
    }

    pub fn request_process(&self) {
        let host = self.host();
        unsafe { host.request_process.unwrap()(host) }
    }

    pub fn notify_tail_changed(&self) {
        let host = self.host();
        unsafe {
            let ext = host.get_extension.unwrap()(host, CLAP_EXT_TAIL.as_ptr()) as *const clap_host_tail;
            assert!(!ext.is_null(), "the host offers no tail extension");
            (*ext).changed.unwrap()(host)
        }
    }

    pub fn sidechain_peak(&self) -> f32 {
        f32::from_bits(self.sidechain_peak.load(Ordering::SeqCst))
    }

    fn record(&self, what: &'static str) {
        self.calls.lock().unwrap().push(Call { what, thread: std::thread::current().id() });
    }

    /// Every recorded call of `what`, in order.
    pub fn calls(&self, what: &str) -> Vec<Call> {
        self.calls.lock().unwrap().iter().filter(|c| c.what == what).cloned().collect()
    }

    /// The names of all recorded calls except `process`, in order.
    pub fn sequence(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().iter().map(|c| c.what).collect()
    }

    fn host(&self) -> &clap_host {
        // SAFETY: set in `create_plugin`; the host outlives the instance,
        // and tests call this only while the instance lives.
        unsafe { &*self.host.load(Ordering::SeqCst) }
    }

    /// `clap_host.request_callback()`, from the calling thread.
    pub fn request_callback(&self) {
        let host = self.host();
        unsafe { host.request_callback.unwrap()(host) }
    }

    /// `clap_host.request_restart()`, from the calling thread.
    pub fn request_restart(&self) {
        let host = self.host();
        unsafe { host.request_restart.unwrap()(host) }
    }

    /// `clap_host_params.request_flush()`, from the calling thread.
    pub fn request_flush(&self) {
        let host = self.host();
        unsafe {
            let ext = host.get_extension.unwrap()(host, CLAP_EXT_PARAMS.as_ptr()) as *const clap_host_params;
            assert!(!ext.is_null(), "the host offers no params extension");
            (*ext).request_flush.unwrap()(host)
        }
    }
}

thread_local! {
    /// The probe the next `create_plugin` on this thread belongs to.
    static NEXT: std::cell::RefCell<Option<Arc<Probe>>> = const { std::cell::RefCell::new(None) };
}

/// A probe instance hosted like any plugin: both halves of the voice and
/// the handle the test steers it with.
pub fn host_probe() -> (ClapVoice, ClapVoiceMain, Arc<Probe>) {
    let probe = Probe::new();
    NEXT.with(|n| *n.borrow_mut() = Some(probe.clone()));
    // SAFETY: `ENTRY` is a valid CLAP entry for the life of the process.
    let entry = unsafe { clack_host::prelude::PluginEntry::load_from_raw(&ENTRY, c"/test/probe.clap") }.expect("the probe entry loads");
    let (voice, main) = ClapVoice::instantiate_entry(entry, CString::from(PROBE_ID), 48_000.0).expect("the probe instantiates");
    (voice, main, probe)
}

/// An audio-effect probe with `in_ports` (`(channels, is_main)`) and
/// `out_channels` on its main output port. Scales its main audio input by
/// `Level` (default 0.5) and measures what arrives on any other input port.
pub fn host_probe_effect(in_ports: &[(u32, bool)], out_channels: u32) -> (ClapVoice, ClapVoiceMain, Arc<Probe>) {
    let probe = Probe::with_ports(in_ports.to_vec(), out_channels, 0.5);
    NEXT.with(|n| *n.borrow_mut() = Some(probe.clone()));
    // SAFETY: `ENTRY` is a valid CLAP entry for the life of the process.
    let entry = unsafe { clack_host::prelude::PluginEntry::load_from_raw(&ENTRY, c"/test/probe.clap") }.expect("the probe entry loads");
    let (voice, main) = ClapVoice::instantiate_entry(entry, CString::from(PROBE_FX_ID), 48_000.0).expect("the effect probe instantiates");
    (voice, main, probe)
}

struct Instance {
    plugin: clap_plugin,
    probe: Arc<Probe>,
}

unsafe fn probe_of<'a>(plugin: *const clap_plugin) -> &'a Probe {
    &(*((*plugin).plugin_data as *const Instance)).probe
}

struct SyncPtrs([*const c_char; 2]);
unsafe impl Sync for SyncPtrs {}
static FEATURES: SyncPtrs = SyncPtrs([c"instrument".as_ptr(), std::ptr::null()]);
static FX_FEATURES: SyncPtrs = SyncPtrs([c"audio-effect".as_ptr(), std::ptr::null()]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PROBE_ID.as_ptr(),
    name: c"Probe".as_ptr(),
    vendor: c"kabelsalat".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1".as_ptr(),
    description: c"host test plugin".as_ptr(),
    features: FEATURES.0.as_ptr(),
};

static FX_DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PROBE_FX_ID.as_ptr(),
    name: c"Probe FX".as_ptr(),
    vendor: c"kabelsalat".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1".as_ptr(),
    description: c"host test audio effect".as_ptr(),
    features: FX_FEATURES.0.as_ptr(),
};

static ENTRY: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}
unsafe extern "C" fn entry_deinit() {}
unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if CStr::from_ptr(id) == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const clap_plugin_factory as *const c_void
    } else {
        std::ptr::null()
    }
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn factory_count(_f: *const clap_plugin_factory) -> u32 {
    2
}
unsafe extern "C" fn factory_descriptor(_f: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    match index {
        0 => &DESCRIPTOR,
        1 => &FX_DESCRIPTOR,
        _ => std::ptr::null(),
    }
}
unsafe extern "C" fn factory_create(_f: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    let cid = CStr::from_ptr(id);
    let desc: *const clap_plugin_descriptor = if cid == PROBE_ID {
        &DESCRIPTOR
    } else if cid == PROBE_FX_ID {
        &FX_DESCRIPTOR
    } else {
        return std::ptr::null();
    };
    let Some(probe) = NEXT.with(|n| n.borrow_mut().take()) else { return std::ptr::null() };
    probe.host.store(host as *mut clap_host, Ordering::SeqCst);
    let instance = Box::into_raw(Box::new(Instance {
        plugin: clap_plugin {
            desc,
            plugin_data: std::ptr::null_mut(),
            init: Some(plugin_init),
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start_processing),
            stop_processing: Some(plugin_stop_processing),
            reset: Some(plugin_reset),
            process: Some(plugin_process),
            get_extension: Some(plugin_get_extension),
            on_main_thread: Some(plugin_on_main_thread),
        },
        probe,
    }));
    (*instance).plugin.plugin_data = instance as *mut c_void;
    &(*instance).plugin
}

unsafe extern "C" fn plugin_init(_p: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn plugin_destroy(p: *const clap_plugin) {
    probe_of(p).record("destroy");
    drop(Box::from_raw((*p).plugin_data as *mut Instance));
}
unsafe extern "C" fn plugin_activate(p: *const clap_plugin, _rate: f64, _min: u32, _max: u32) -> bool {
    let probe = probe_of(p);
    if probe.refuse_activation.swap(false, Ordering::SeqCst) {
        probe.record("activate refused");
        return false;
    }
    probe.record("activate");
    true
}
unsafe extern "C" fn plugin_deactivate(p: *const clap_plugin) {
    probe_of(p).record("deactivate");
}
unsafe extern "C" fn plugin_start_processing(p: *const clap_plugin) -> bool {
    probe_of(p).record("start_processing");
    true
}
unsafe extern "C" fn plugin_stop_processing(p: *const clap_plugin) {
    probe_of(p).record("stop_processing");
}
unsafe extern "C" fn plugin_reset(_p: *const clap_plugin) {}
unsafe extern "C" fn plugin_on_main_thread(p: *const clap_plugin) {
    probe_of(p).record("on_main_thread");
}

unsafe fn apply_events(probe: &Probe, events: *const clap_input_events) {
    if events.is_null() {
        return;
    }
    let (Some(size), Some(get)) = ((*events).size, (*events).get) else { return };
    let sine_on_note = probe.sine_on_note.load(Ordering::SeqCst);
    for i in 0..size(events) {
        let header = get(events, i);
        if (*header).space_id != CLAP_CORE_EVENT_SPACE_ID {
            continue;
        }
        match (*header).type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let ev = &*(header as *const clap_event_param_value);
                if ev.param_id == 0 {
                    probe.level.store((ev.value as f32).to_bits(), Ordering::SeqCst);
                }
            }
            CLAP_EVENT_NOTE_ON if sine_on_note => {
                if !probe.note_held.swap(true, Ordering::SeqCst) {
                    probe.sine_phase.store(0.0f64.to_bits(), Ordering::SeqCst);
                }
            }
            CLAP_EVENT_NOTE_OFF if sine_on_note => {
                probe.note_held.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

unsafe extern "C" fn plugin_process(p: *const clap_plugin, process: *const clap_process) -> clap_process_status {
    let probe = probe_of(p);
    if probe.blocks.fetch_add(1, Ordering::SeqCst) == 0 {
        probe.record("first process");
    }
    let process = &*process;
    probe.last_steady_time.store(process.steady_time, Ordering::SeqCst);
    apply_events(probe, process.in_events);
    let level = f32::from_bits(probe.level.load(Ordering::SeqCst));
    let n = process.frames_count as usize;
    if probe.in_ports.is_empty() || process.audio_inputs_count == 0 {
        if probe.sine_on_note.load(Ordering::SeqCst) {
            let held = probe.note_held.load(Ordering::SeqCst);
            let mut phase = f64::from_bits(probe.sine_phase.load(Ordering::SeqCst));
            let step = 440.0 / 48_000.0;
            for port in 0..process.audio_outputs_count as usize {
                let buffer = &*process.audio_outputs.add(port);
                let mut port_phase = phase;
                for i in 0..n {
                    let s = if held {
                        let v = (2.0 * std::f64::consts::PI * port_phase).sin() as f32 * 0.5;
                        port_phase += step;
                        if port_phase >= 1.0 {
                            port_phase -= 1.0;
                        }
                        v
                    } else {
                        0.0
                    };
                    for ch in 0..buffer.channel_count as usize {
                        *(*buffer.data32.add(ch)).add(i) = s;
                    }
                }
                if port == 0 {
                    phase = port_phase;
                }
            }
            probe.sine_phase.store(phase.to_bits(), Ordering::SeqCst);
        } else {
            for port in 0..process.audio_outputs_count as usize {
                let buffer = &*process.audio_outputs.add(port);
                for ch in 0..buffer.channel_count as usize {
                    let data = *buffer.data32.add(ch);
                    std::slice::from_raw_parts_mut(data, n).fill(level);
                }
            }
        }
    } else {
        let main_in = probe.in_ports.iter().position(|&(_, is_main)| is_main).unwrap_or(0);
        let mut sc_peak = f32::from_bits(probe.sidechain_peak.load(Ordering::SeqCst));
        for port in 0..process.audio_inputs_count as usize {
            if port == main_in {
                continue;
            }
            let buffer = &*process.audio_inputs.add(port);
            for ch in 0..buffer.channel_count as usize {
                let data = std::slice::from_raw_parts(*buffer.data32.add(ch), n);
                for &s in data {
                    sc_peak = sc_peak.max(s.abs());
                }
            }
        }
        probe.sidechain_peak.store(sc_peak.to_bits(), Ordering::SeqCst);
        let in_buf = &*process.audio_inputs.add(main_in);
        let in_ch = in_buf.channel_count as usize;
        for port in 0..process.audio_outputs_count as usize {
            let out_buf = &*process.audio_outputs.add(port);
            for ch in 0..out_buf.channel_count as usize {
                let src = std::slice::from_raw_parts(*in_buf.data32.add(ch.min(in_ch.saturating_sub(1))), n);
                let dst = std::slice::from_raw_parts_mut(*out_buf.data32.add(ch), n);
                for i in 0..n {
                    dst[i] = src[i] * level;
                }
            }
        }
    }
    probe.process_status.load(Ordering::SeqCst)
}

unsafe extern "C" fn plugin_get_extension(_p: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_PARAMS {
        &PARAMS as *const clap_plugin_params as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &AUDIO_PORTS as *const clap_plugin_audio_ports as *const c_void
    } else if id == CLAP_EXT_TAIL {
        &TAIL as *const clap_plugin_tail as *const c_void
    } else {
        std::ptr::null()
    }
}

static TAIL: clap_plugin_tail = clap_plugin_tail { get: Some(tail_get) };

unsafe extern "C" fn tail_get(p: *const clap_plugin) -> u32 {
    probe_of(p).tail_samples.load(Ordering::SeqCst)
}

fn write_name(dst: &mut [c_char], name: &str) {
    for (d, b) in dst.iter_mut().zip(name.bytes().chain(std::iter::once(0))) {
        *d = b as c_char;
    }
}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) };

unsafe extern "C" fn ports_count(p: *const clap_plugin, is_input: bool) -> u32 {
    if is_input { probe_of(p).in_ports.len() as u32 } else { 1 }
}
unsafe extern "C" fn ports_get(p: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_audio_port_info) -> bool {
    let probe = probe_of(p);
    let info = &mut *info;
    if is_input {
        let Some(&(chs, is_main)) = probe.in_ports.get(index as usize) else { return false };
        info.id = index;
        write_name(&mut info.name, if is_main { "In" } else { "Sidechain" });
        info.flags = if is_main { CLAP_AUDIO_PORT_IS_MAIN } else { 0 };
        info.channel_count = chs;
        info.port_type = if chs == 1 { CLAP_PORT_MONO.as_ptr() } else { CLAP_PORT_STEREO.as_ptr() };
        info.in_place_pair = CLAP_INVALID_ID;
        return true;
    }
    if index != 0 {
        return false;
    }
    info.id = 0;
    write_name(&mut info.name, "Out");
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = probe.out_channels;
    info.port_type = if probe.out_channels == 1 { CLAP_PORT_MONO.as_ptr() } else { CLAP_PORT_STEREO.as_ptr() };
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_count(p: *const clap_plugin) -> u32 {
    probe_of(p).params.load(Ordering::SeqCst)
}
unsafe extern "C" fn params_get_info(p: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    if index >= probe_of(p).params.load(Ordering::SeqCst) {
        return false;
    }
    let info = &mut *info;
    info.id = index;
    info.flags = CLAP_PARAM_IS_AUTOMATABLE;
    info.cookie = std::ptr::null_mut();
    write_name(&mut info.name, if index == 0 { "Level" } else { "Extra" });
    write_name(&mut info.module, "");
    info.min_value = 0.0;
    info.max_value = 1.0;
    info.default_value = 0.25;
    true
}
unsafe extern "C" fn params_get_value(p: *const clap_plugin, id: u32, out: *mut f64) -> bool {
    *out = if id == 0 { f32::from_bits(probe_of(p).level.load(Ordering::SeqCst)) as f64 } else { 0.0 };
    true
}
unsafe extern "C" fn params_value_to_text(_p: *const clap_plugin, _id: u32, _value: f64, _out: *mut c_char, _cap: u32) -> bool {
    false
}
unsafe extern "C" fn params_text_to_value(_p: *const clap_plugin, _id: u32, _text: *const c_char, _out: *mut f64) -> bool {
    false
}
unsafe extern "C" fn params_flush(p: *const clap_plugin, in_: *const clap_input_events, _out: *const clap_output_events) {
    let probe = probe_of(p);
    probe.record("flush");
    apply_events(probe, in_);
}
