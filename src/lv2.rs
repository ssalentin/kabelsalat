//! LV2 plugin discovery via lilv — hand-written FFI (no bindgen/clang needed).
//!
//! First P3 milestone: enumerate the system's installed LV2 plugins. Full
//! hosting (instantiate → connect ports → run) is the follow-up; this proves
//! the lilv linkage and discovery path on which hosting builds.

use std::ffi::CStr;
use std::os::raw::{c_char, c_uint, c_void};

// Opaque lilv types.
#[repr(C)]
struct LilvWorld {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvPlugins {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvPlugin {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvNode {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvNodes {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvPluginClass {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvIter {
    _private: [u8; 0],
}
#[repr(C)]
struct LilvPort {
    _private: [u8; 0],
}

/// LV2 plugin descriptor (lv2/core/lv2.h) — function pointers we call directly.
/// `activate`/`deactivate` may be NULL per the spec.
type Lv2Handle = *mut c_void;
#[repr(C)]
struct Lv2Descriptor {
    uri: *const c_char,
    instantiate: Option<extern "C" fn(*const Lv2Descriptor, f64, *const c_char, *const *const c_void) -> Lv2Handle>,
    connect_port: Option<extern "C" fn(Lv2Handle, u32, *mut c_void)>,
    activate: Option<extern "C" fn(Lv2Handle)>,
    run: Option<extern "C" fn(Lv2Handle, u32)>,
    deactivate: Option<extern "C" fn(Lv2Handle)>,
    cleanup: Option<extern "C" fn(Lv2Handle)>,
    extension_data: Option<extern "C" fn(*const c_char) -> *const c_void>,
}

/// lilv exposes this struct in the ABI so hosts can inline run/connect_port.
#[repr(C)]
struct LilvInstance {
    lv2_descriptor: *const Lv2Descriptor,
    lv2_handle: Lv2Handle,
    pimpl: *mut c_void,
}

/// LV2 feature (lv2/core/lv2.h) — passed to instantiate as a NULL-terminated
/// array of pointers.
#[repr(C)]
struct Lv2Feature {
    uri: *const c_char,
    data: *mut c_void,
}

/// LV2_URID__map feature data (lv2/ext/urid).
#[repr(C)]
struct Lv2UridMap {
    handle: *mut c_void,
    map: extern "C" fn(*mut c_void, *const c_char) -> u32,
}

/// LV2_OPTIONS__options entry (lv2/ext/options). Array is zero-terminated.
#[repr(C)]
struct Lv2Option {
    context: u32,
    subject: u32,
    key: u32,
    size: u32,
    ty: u32,
    value: *const c_void,
}

// ---------------------------------------------------------------------------
// LV2 Worker extension (lv2/ext/worker) — non-RT work scheduled from run().
// ---------------------------------------------------------------------------

const LV2_WORKER_INTERFACE_URI: &[u8] = b"http://lv2plug.in/ns/ext/worker#interface\0";
const LV2_WORKER_SCHEDULE_URI: &[u8] = b"http://lv2plug.in/ns/ext/worker#schedule\0";

const WORKER_SUCCESS: u32 = 0;
const WORKER_ERR_NO_SPACE: u32 = 2;

/// Fixed-size worker message slot. A larger message is rejected with
/// `LV2_WORKER_ERR_NO_SPACE`. setBfree's `struct worknfo { int cmd; int status;
/// char msg[1024]; }` is 1032 bytes; 2048 leaves a comfortable margin.
const WORKER_MSG_MAX: usize = 2048;
const WORKER_RING_CAP: usize = 64;

#[derive(Clone, Copy)]
struct WorkerMsg {
    len: u32,
    data: [u8; WORKER_MSG_MAX],
}

type Lv2WorkerRespondHandle = *mut c_void;
type Lv2WorkerScheduleHandle = *mut c_void;
type Lv2WorkerRespondFn = extern "C" fn(Lv2WorkerRespondHandle, u32, *const c_void) -> u32;

/// Plugin Worker Interface (`extension_data(worker#interface)`). Field order
/// matches the C struct in lv2/worker/worker.h exactly.
#[derive(Clone, Copy)]
#[repr(C)]
struct Lv2WorkerInterface {
    work: extern "C" fn(Lv2Handle, Lv2WorkerRespondFn, Lv2WorkerRespondHandle, u32, *const c_void) -> u32,
    work_response: extern "C" fn(Lv2Handle, u32, *const c_void) -> u32,
    end_run: Option<extern "C" fn(Lv2Handle) -> u32>,
}

/// Schedule Worker Host Feature (`work:schedule`). `handle` must be callable
/// from `run()` without blocking; the plugin calls `schedule_work` from its
/// audio callback.
#[repr(C)]
struct Lv2WorkerSchedule {
    handle: Lv2WorkerScheduleHandle,
    schedule_work: extern "C" fn(Lv2WorkerScheduleHandle, u32, *const c_void) -> u32,
}

/// Everything the `schedule_work` callback reaches through the feature handle.
/// Box'd and never moved while the instance lives, so its address is stable
/// even though `Lv2Plugin` itself moves between the loader and audio threads.
struct WorkerShared {
    /// Producer half of the request ring; `None` until a worker thread exists.
    req: Option<rtrb::Producer<WorkerMsg>>,
    /// The worker thread, `unpark`-ed by `schedule_work` to wake it.
    thread: std::thread::Thread,
}

/// What the spawned worker thread needs. Raw `instance` is `!Send`, so this is
/// declared `Send` explicitly: the plugin keeps the instance alive until the
/// worker is joined in `Drop`.
struct WorkerCtx {
    iface: Lv2WorkerInterface,
    instance: Lv2Handle,
}

// SAFETY: `instance` points into the plugin, which joins this thread before the
// lilv instance is freed (or, for never-free plugins, keeps it parked forever).
unsafe impl Send for WorkerCtx {}

/// Per-instance worker: a non-RT thread that executes `interface->work()` on
/// requests the audio thread queues via `schedule_work` (rule 46 — never on the
/// audio thread, and rule 47 — it parks, never spins).
struct Lv2Worker {
    iface: Lv2WorkerInterface,
    instance: Lv2Handle,
    resp_rx: rtrb::Consumer<WorkerMsg>,
    join: Option<std::thread::JoinHandle<()>>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn copy_worker_msg(size: u32, data: *const c_void) -> WorkerMsg {
    let size = size as usize;
    let mut msg = WorkerMsg { len: size as u32, data: [0u8; WORKER_MSG_MAX] };
    if size > 0 && !data.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(data as *const u8, msg.data.as_mut_ptr(), size) };
    }
    msg
}

extern "C" fn schedule_work_cb(handle: Lv2WorkerScheduleHandle, size: u32, data: *const c_void) -> u32 {
    if handle.is_null() || size as usize > WORKER_MSG_MAX {
        return WORKER_ERR_NO_SPACE;
    }
    let msg = copy_worker_msg(size, data);
    // SAFETY: `handle` is the WorkerShared boxed in the instance and kept alive
    // for its whole life; schedule_work is the sole writer of `req`.
    let shared = unsafe { &mut *(handle as *mut WorkerShared) };
    match shared.req.as_mut() {
        Some(req) => match req.push(msg) {
            Ok(()) => {
                shared.thread.unpark();
                WORKER_SUCCESS
            }
            Err(_) => WORKER_ERR_NO_SPACE,
        },
        None => WORKER_ERR_NO_SPACE,
    }
}

extern "C" fn respond_cb(handle: Lv2WorkerRespondHandle, size: u32, data: *const c_void) -> u32 {
    if handle.is_null() || size as usize > WORKER_MSG_MAX {
        return WORKER_ERR_NO_SPACE;
    }
    let msg = copy_worker_msg(size, data);
    // SAFETY: `handle` is the worker thread's response producer, alive while
    // that thread runs (it never outlives the plugin).
    let tx = unsafe { &mut *(handle as *mut rtrb::Producer<WorkerMsg>) };
    match tx.push(msg) {
        Ok(()) => WORKER_SUCCESS,
        Err(_) => WORKER_ERR_NO_SPACE,
    }
}

/// Non-RT worker loop: runs `work()`, forwards its `respond()` calls into the
/// response ring. Parks when idle (bounded, so a lost wakeup cannot stall).
fn worker_loop(
    mut req_rx: rtrb::Consumer<WorkerMsg>,
    mut resp_tx: rtrb::Producer<WorkerMsg>,
    ctx: WorkerCtx,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let msg = match req_rx.pop() {
            Ok(m) => m,
            Err(rtrb::PopError::Empty) => {
                if req_rx.is_abandoned() {
                    return;
                }
                std::thread::park_timeout(std::time::Duration::from_millis(50));
                continue;
            }
        };
        let resp_ptr = &mut resp_tx as *mut rtrb::Producer<WorkerMsg> as *mut c_void;
        (ctx.iface.work)(ctx.instance, respond_cb, resp_ptr, msg.len, msg.data.as_ptr() as *const c_void);
    }
}

impl Lv2Worker {
    /// Deliver queued worker responses on the audio thread (before `run()`).
    fn drain_responses(&mut self) {
        loop {
            match self.resp_rx.pop() {
                Ok(msg) => {
                    (self.iface.work_response)(self.instance, msg.len, msg.data.as_ptr() as *const c_void);
                }
                Err(_) => return,
            }
        }
    }

    /// Must be called after every `run()`, whether or not a response arrived.
    fn end_run(&self) {
        if let Some(end_run) = self.iface.end_run {
            (end_run)(self.instance);
        }
    }
}

/// Host-side URID map: assigns a stable u32 to each URI string.
struct UridMap {
    map: std::collections::HashMap<String, u32>,
    next: u32,
}

extern "C" fn urid_map_cb(handle: *mut c_void, uri: *const c_char) -> u32 {
    unsafe {
        let m = &mut *(handle as *mut UridMap);
        let s = CStr::from_ptr(uri).to_string_lossy().into_owned();
        if let Some(&id) = m.map.get(&s) {
            return id;
        }
        let id = m.next;
        m.next += 1;
        m.map.insert(s, id);
        id
    }
}

#[link(name = "lilv-0")]
extern "C" {
    fn lilv_world_new() -> *mut LilvWorld;
    fn lilv_world_free(world: *mut LilvWorld);
    fn lilv_world_load_all(world: *mut LilvWorld);
    fn lilv_world_get_all_plugins(world: *const LilvWorld) -> *const LilvPlugins;
    fn lilv_plugins_begin(collection: *const LilvPlugins) -> *mut LilvIter;
    fn lilv_plugins_is_end(collection: *const LilvPlugins, iter: *const LilvIter) -> bool;
    fn lilv_plugins_get(collection: *const LilvPlugins, iter: *const LilvIter) -> *const LilvPlugin;
    fn lilv_plugins_next(collection: *const LilvPlugins, iter: *const LilvIter) -> *mut LilvIter;
    fn lilv_plugin_get_uri(plugin: *const LilvPlugin) -> *mut LilvNode;
    fn lilv_plugin_get_name(plugin: *const LilvPlugin) -> *mut LilvNode;
    fn lilv_plugin_get_author_name(plugin: *const LilvPlugin) -> *mut LilvNode;
    fn lilv_plugin_get_num_ports(plugin: *const LilvPlugin) -> c_uint;
    fn lilv_plugin_get_class(plugin: *const LilvPlugin) -> *const LilvPluginClass;
    fn lilv_plugin_class_get_uri(class: *const LilvPluginClass) -> *const LilvNode;
    fn lilv_plugin_get_value(plugin: *const LilvPlugin, predicate: *const LilvNode) -> *mut LilvNodes;
    fn lilv_nodes_begin(nodes: *const LilvNodes) -> *mut LilvIter;
    fn lilv_nodes_get(nodes: *const LilvNodes, i: *mut LilvIter) -> *const LilvNode;
    fn lilv_nodes_next(nodes: *const LilvNodes, i: *mut LilvIter) -> *mut LilvIter;
    fn lilv_nodes_is_end(nodes: *const LilvNodes, i: *mut LilvIter) -> bool;
    fn lilv_nodes_free(nodes: *mut LilvNodes);
    fn lilv_node_is_uri(value: *const LilvNode) -> bool;
    fn lilv_node_as_string(value: *const LilvNode) -> *const c_char;
    fn lilv_node_as_uri(value: *const LilvNode) -> *const c_char;
    fn lilv_plugin_get_library_uri(plugin: *const LilvPlugin) -> *const LilvNode;
    fn lilv_plugin_has_extension_data(plugin: *const LilvPlugin, uri: *const LilvNode) -> bool;
    fn lilv_file_uri_parse(uri: *const c_char, hostname: *mut *mut c_char) -> *mut c_char;
    fn lilv_free(ptr: *mut c_void);
    fn lilv_node_free(val: *mut LilvNode);
    fn lilv_new_uri(world: *mut LilvWorld, uri: *const c_char) -> *mut LilvNode;
    fn lilv_node_as_float(value: *const LilvNode) -> f32;
    fn lilv_plugin_instantiate(
        plugin: *const LilvPlugin,
        sample_rate: f64,
        features: *const *const Lv2Feature,
    ) -> *mut LilvInstance;
    fn lilv_plugin_get_port_by_index(plugin: *const LilvPlugin, index: u32) -> *const LilvPort;
    fn lilv_port_get_symbol(plugin: *const LilvPlugin, port: *const LilvPort) -> *const LilvNode;
    fn lilv_port_get_name(plugin: *const LilvPlugin, port: *const LilvPort) -> *mut LilvNode;
    fn lilv_port_is_a(plugin: *const LilvPlugin, port: *const LilvPort, class: *const LilvNode) -> bool;
    fn lilv_port_has_property(plugin: *const LilvPlugin, port: *const LilvPort, property: *const LilvNode) -> bool;
    fn lilv_port_get_value(plugin: *const LilvPlugin, port: *const LilvPort, predicate: *const LilvNode) -> *mut LilvNodes;
    fn lilv_port_get_range(
        plugin: *const LilvPlugin,
        port: *const LilvPort,
        def: *mut *mut LilvNode,
        min: *mut *mut LilvNode,
        max: *mut *mut LilvNode,
    );
    fn lilv_instance_free(instance: *mut LilvInstance);
    fn lilv_plugin_has_latency(plugin: *const LilvPlugin) -> bool;
    fn lilv_plugin_get_latency_port_index(plugin: *const LilvPlugin) -> u32;
    fn lilv_new_file_uri(world: *mut LilvWorld, host: *const c_char, path: *const c_char) -> *mut LilvNode;
    fn lilv_world_load_bundle(world: *mut LilvWorld, bundle_uri: *const LilvNode);
}

/// One MIDI event a MIDI-FX plugin wrote to its output port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxOutEvent {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    /// Raw 3-byte channel-voice MIDI (CC, pitch bend, aftertouch).
    Midi([u8; 3]),
}

#[derive(Debug, Clone, Default)]
pub struct Lv2PluginInfo {
    pub uri: String,
    pub name: String,
    pub author: String,
    /// Most specific `lv2core` class local name (e.g. `"ReverbPlugin"`,
    /// `"CompressorPlugin"`, `"InstrumentPlugin"`, `"AnalyserPlugin"`), or
    /// empty when only generic `lv2:Plugin` is declared.
    pub lv2_class: String,
    pub num_ports: u32,
    pub is_instrument: bool,
    /// A MIDI-only unit: events in, events out, no audio port at all.
    pub is_midi_fx: bool,
}

#[derive(Clone, Debug)]
pub struct Lv2ControlDesc {
    pub index: usize,
    pub symbol: String,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub value: f32,
}

#[derive(Clone, Debug)]
pub struct Lv2Preset {
    pub name: String,
    pub uri: String,
    pub values: Vec<(String, f32)>,
}

#[derive(Clone, Debug)]
struct ControlMeta {
    port_index: usize,
    symbol: String,
    name: String,
    min: f32,
    max: f32,
    default: f32,
}

/// Read `(default, min, max)` for a port, freeing the owned lilv nodes. A
/// missing default falls back to the minimum, and the default is clamped to
/// the range: a control input starts at the value the plugin declares.
unsafe fn read_port_range(plugin: *const LilvPlugin, port: *const LilvPort) -> (f32, f32, f32) {
    let mut def: *mut LilvNode = std::ptr::null_mut();
    let mut min: *mut LilvNode = std::ptr::null_mut();
    let mut max: *mut LilvNode = std::ptr::null_mut();
    lilv_port_get_range(plugin, port, &mut def, &mut min, &mut max);
    let lo = if !min.is_null() { let f = lilv_node_as_float(min); lilv_node_free(min); f } else { 0.0 };
    let hi = if !max.is_null() { let f = lilv_node_as_float(max); lilv_node_free(max); f } else { 1.0 };
    let d = if !def.is_null() { let f = lilv_node_as_float(def); lilv_node_free(def); f } else { lo };
    (d.clamp(lo.min(hi), lo.max(hi)), lo, hi)
}

/// Read metadata for all control-input ports of `plugin`.
unsafe fn read_control_meta(world: *mut LilvWorld, plugin: *const LilvPlugin) -> Vec<ControlMeta> {
    let control = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#ControlPort\0".as_ptr() as *const c_char);
    let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
    let nports = lilv_plugin_get_num_ports(plugin);
    let mut out = Vec::new();
    for i in 0..nports {
        let port = lilv_plugin_get_port_by_index(plugin, i);
        if lilv_port_is_a(plugin, port, control) && lilv_port_is_a(plugin, port, input) {
            let (default, min, max) = read_port_range(plugin, port);
            let symbol = node_borrowed(lilv_port_get_symbol(plugin, port));
            let name = node_owned(lilv_port_get_name(plugin, port));
            out.push(ControlMeta {
                port_index: i as usize,
                symbol,
                name,
                min,
                max,
                default,
            });
        }
    }
    lilv_node_free(control);
    lilv_node_free(input);
    out
}

/// Inspect control-input port descriptions for a plugin URI without instantiating it.
/// Keep a plugin's shared library mapped for the life of the process.
/// Freeing an instance makes lilv dlclose the library; Vitalium's static
/// destructors then crash in `__cxa_finalize` (4 core dumps, 2026-11-16).
/// Hosts commonly pin plugin libraries; a second load is cheaper anyway.
unsafe fn pin_plugin_library(plugin: *const LilvPlugin) {
    let lib = lilv_plugin_get_library_uri(plugin);
    if lib.is_null() {
        return;
    }
    let path = lilv_file_uri_parse(lilv_node_as_uri(lib), std::ptr::null_mut());
    if path.is_null() {
        return;
    }
    // NOLOAD: only bump the already-mapped library; NODELETE: never unmap it.
    if libc::dlopen(path, libc::RTLD_NOW | libc::RTLD_NOLOAD | libc::RTLD_NODELETE).is_null() {
        eprintln!("lv2: could not pin {:?}", std::ffi::CStr::from_ptr(path));
    }
    lilv_free(path as *mut c_void);
}

/// Tests that run Vitalium hold this: two Vitalium instances processing on
/// parallel test threads crash inside the plugin (shared global state);
/// the app only ever runs instances on its one audio thread.
#[cfg(test)]
pub(crate) static VITALIUM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes every lilv world build/load, plugin instantiation and free:
/// the app now does these from several threads (plugin scan, LV2 loader,
/// editor), and plugin libraries keep process-global state.
static LILV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lilv_guard() -> std::sync::MutexGuard<'static, ()> {
    LILV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const LV2_PARAMETERS_PREFIX: &str = "http://lv2plug.in/ns/ext/parameters#";

/// What the main-controls rule reads from a control port's metadata.
#[derive(Default)]
struct PortFacts {
    /// Every `lv2:designation` URI of the port.
    designations: Vec<String>,
    /// `pprops:notOnGUI` or `pprops:notAutomatic`.
    hidden: bool,
    /// `lv2:toggled`, `lv2:enumeration` or `lv2:integer`.
    stepped: bool,
}

/// One control input as a main-control candidate. `nth` is its ordinal among
/// the control inputs, the id `Lv2Plugin::set_control_normalized` takes.
///
/// A `param:` designation (gain, cutoffFrequency, …) marks a main control.
/// Any other designation (`lv2:freeWheeling`, `lv2:enabled`, …) is the host's
/// to drive, so that port is not a control for the player at all.
fn control_candidate(nth: usize, meta: &ControlMeta, facts: &PortFacts) -> crate::main_controls::Candidate {
    let designated = facts.designations.iter().any(|d| d.starts_with(LV2_PARAMETERS_PREFIX));
    let host_owned = !designated && !facts.designations.is_empty();
    crate::main_controls::Candidate {
        id: nth as u32,
        name: if meta.name.is_empty() { meta.symbol.clone() } else { meta.name.clone() },
        group: String::new(),
        min: meta.min as f64,
        max: meta.max as f64,
        default: meta.default as f64,
        usable: !facts.hidden && !host_owned,
        stepped: facts.stepped,
        designated,
    }
}

unsafe fn read_port_facts(world: *mut LilvWorld, plugin: *const LilvPlugin, port_index: usize) -> PortFacts {
    let uri = |u: &[u8]| lilv_new_uri(world, u.as_ptr() as *const c_char);
    let port = lilv_plugin_get_port_by_index(plugin, port_index as u32);
    let has = |u: &[u8]| {
        let node = uri(u);
        let yes = lilv_port_has_property(plugin, port, node);
        lilv_node_free(node);
        yes
    };
    let mut facts = PortFacts {
        designations: Vec::new(),
        hidden: has(b"http://lv2plug.in/ns/ext/port-props#notOnGUI\0")
            || has(b"http://lv2plug.in/ns/ext/port-props#notAutomatic\0"),
        stepped: has(b"http://lv2plug.in/ns/lv2core#toggled\0")
            || has(b"http://lv2plug.in/ns/lv2core#enumeration\0")
            || has(b"http://lv2plug.in/ns/lv2core#integer\0"),
    };
    let designation = uri(b"http://lv2plug.in/ns/lv2core#designation\0");
    let nodes = lilv_port_get_value(plugin, port, designation);
    if !nodes.is_null() {
        let mut it = lilv_nodes_begin(nodes);
        while !lilv_nodes_is_end(nodes, it) {
            let n = lilv_nodes_get(nodes, it);
            if !n.is_null() && lilv_node_is_uri(n) {
                facts.designations.push(node_borrowed(n));
            }
            it = lilv_nodes_next(nodes, it);
        }
        lilv_nodes_free(nodes);
    }
    lilv_node_free(designation);
    facts
}

/// Resolve a plugin's main controls from its port metadata, without
/// instantiating it. Loads a lilv world (blocking): call it from a worker
/// thread, or use [`main_controls_cached`] from the UI.
pub fn main_controls_for_uri(uri: &str) -> crate::main_controls::MainControls {
    let _lilv = lilv_guard();
    unsafe {
        let world = lilv_world_new();
        if world.is_null() {
            return Default::default();
        }
        lilv_world_load_all(world);
        let plugin = find_plugin(world, uri);
        let candidates: Vec<_> = if plugin.is_null() {
            Vec::new()
        } else {
            read_control_meta(world, plugin)
                .iter()
                .enumerate()
                .map(|(nth, meta)| control_candidate(nth, meta, &read_port_facts(world, plugin, meta.port_index)))
                .collect()
        };
        lilv_world_free(world);
        crate::main_controls::resolve(&[], &candidates)
    }
}

type MainControlsCache = std::sync::Mutex<Vec<(String, Option<crate::main_controls::MainControls>)>>;
static MAIN_CONTROLS: MainControlsCache = std::sync::Mutex::new(Vec::new());

/// Main controls per plugin URI, resolved once on a worker thread. `None`
/// while that thread runs: the caller asks again on a later frame. Tests
/// never read the machine's plugins here: they see only what
/// [`set_main_controls_for_test`] stored.
pub fn main_controls_cached(uri: &str) -> Option<crate::main_controls::MainControls> {
    let mut cache = MAIN_CONTROLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, found)) = cache.iter().find(|(u, _)| u == uri) {
        return found.clone();
    }
    if cfg!(test) {
        return None;
    }
    cache.push((uri.to_string(), None));
    drop(cache);
    let uri = uri.to_string();
    std::thread::spawn(move || {
        let found = main_controls_for_uri(&uri);
        store_main_controls(&uri, found);
    });
    None
}

/// [`main_controls_cached`] for callers that may block (headless MCP):
/// resolves on the calling thread when the cache has no answer yet.
pub fn main_controls_blocking(uri: &str) -> crate::main_controls::MainControls {
    if let Some(found) = main_controls_cached(uri) {
        return found;
    }
    if cfg!(test) {
        return Default::default();
    }
    let found = main_controls_for_uri(uri);
    store_main_controls(uri, found.clone());
    found
}

fn store_main_controls(uri: &str, found: crate::main_controls::MainControls) {
    let mut cache = MAIN_CONTROLS.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(u, _)| u != uri);
    cache.push((uri.to_string(), Some(found)));
}

#[cfg(test)]
pub(crate) fn set_main_controls_for_test(uri: &str, found: crate::main_controls::MainControls) {
    store_main_controls(uri, found);
}

pub fn control_descs_for_uri(uri: &str) -> Vec<Lv2ControlDesc> {
    let _lilv = lilv_guard();
    unsafe {
        let world = lilv_world_new();
        if world.is_null() {
            return Vec::new();
        }
        lilv_world_load_all(world);
        let plugin = find_plugin(world, uri);
        let descs = if plugin.is_null() {
            Vec::new()
        } else {
            read_control_meta(world, plugin)
                .into_iter()
                .enumerate()
                .map(|(index, meta)| Lv2ControlDesc {
                    index,
                    symbol: meta.symbol,
                    name: meta.name,
                    min: meta.min,
                    max: meta.max,
                    default: meta.default,
                    value: meta.default,
                })
                .collect()
        };
        lilv_world_free(world);
        descs
    }
}

/// Classify a plugin as an instrument (#26): its declared LV2 types decide
/// (`lv2:InstrumentPlugin` yes, any other specific class — Delay, Compressor,
/// Utility… — no); only a plugin that declares no specific class falls back
/// to the ports. The old port-only guess called 198 of 208 installed effects
/// instruments (LSP effects take MIDI for sidechain/control).
unsafe fn classify_instrument(world: *mut LilvWorld, plugin: *const LilvPlugin) -> bool {
    let types = plugin_types(world, plugin);
    instrument_from_types(&types).unwrap_or_else(|| {
        let (atom_in, audio_in, audio_out) = port_counts(world, plugin);
        atom_in && audio_in == 0 && audio_out > 0
    })
}

/// Is the plugin a MIDI effect (AUM "MIDI strips"): a MIDI atom input AND a
/// MIDI atom output and no audio port. Never an instrument (that class
/// verdict, rule 72, wins) — the port shape decides only among the
/// non-instrument classes, where it is a fact (no audio port, so it can never
/// be an audio effect), not a guess. Carla's MIDI Transpose/Gain/Channel
/// Filter are `lv2:UtilityPlugin`s of this shape.
unsafe fn classify_midi_fx(world: *mut LilvWorld, plugin: *const LilvPlugin) -> bool {
    if classify_instrument(world, plugin) {
        return false;
    }
    let (atom_in, atom_out, audio) = midi_shape(world, plugin);
    atom_in && atom_out && audio == 0
}

/// (has an atom input, has an atom output, audio port count).
unsafe fn midi_shape(world: *mut LilvWorld, plugin: *const LilvPlugin) -> (bool, bool, u32) {
    let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
    let atom = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
    let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
    let (mut atom_in, mut atom_out, mut n_audio) = (false, false, 0u32);
    for i in 0..lilv_plugin_get_num_ports(plugin) {
        let port = lilv_plugin_get_port_by_index(plugin, i);
        if lilv_port_is_a(plugin, port, audio) {
            n_audio += 1;
        }
        if lilv_port_is_a(plugin, port, atom) {
            if lilv_port_is_a(plugin, port, input) { atom_in = true } else { atom_out = true }
        }
    }
    lilv_node_free(audio);
    lilv_node_free(atom);
    lilv_node_free(input);
    (atom_in, atom_out, n_audio)
}

/// The class verdict: Some(true) for an instrument, Some(false) for any other
/// specific LV2 class, None when only generic types (lv2:Plugin, doap:Project)
/// are declared.
fn instrument_from_types(types: &[String]) -> Option<bool> {
    const GENERIC: &[&str] = &[
        "http://lv2plug.in/ns/lv2core#Plugin",
        "http://usefulinc.com/ns/doap#Project",
    ];
    if types.iter().any(|t| t == "http://lv2plug.in/ns/lv2core#InstrumentPlugin") {
        return Some(true);
    }
    if types.iter().any(|t| t.starts_with(LV2_CORE_PREFIX) && !GENERIC.contains(&t.as_str())) {
        return Some(false);
    }
    None
}

/// (has an atom/MIDI input, audio inputs, audio outputs).
unsafe fn port_counts(world: *mut LilvWorld, plugin: *const LilvPlugin) -> (bool, u32, u32) {
    let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
    let atom = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
    let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
    let (mut atom_in, mut audio_in, mut audio_out) = (false, 0u32, 0u32);
    for i in 0..lilv_plugin_get_num_ports(plugin) {
        let port = lilv_plugin_get_port_by_index(plugin, i);
        let is_in = lilv_port_is_a(plugin, port, input);
        if lilv_port_is_a(plugin, port, atom) && is_in {
            atom_in = true;
        }
        if lilv_port_is_a(plugin, port, audio) {
            if is_in { audio_in += 1; } else { audio_out += 1; }
        }
    }
    lilv_node_free(audio);
    lilv_node_free(atom);
    lilv_node_free(input);
    (atom_in, audio_in, audio_out)
}

/// The plugin's declared LV2 types (`a lv2:InstrumentPlugin, lv2:Plugin`):
/// every URI `rdf:type` plus the class lilv picked. Blank nodes are skipped.
unsafe fn plugin_types(world: *mut LilvWorld, plugin: *const LilvPlugin) -> Vec<String> {
    let mut out = Vec::new();
    let rdf_type = lilv_new_uri(world, b"http://www.w3.org/1999/02/22-rdf-syntax-ns#type\0".as_ptr() as *const c_char);
    let nodes = lilv_plugin_get_value(plugin, rdf_type);
    if !nodes.is_null() {
        let mut it = lilv_nodes_begin(nodes);
        while !lilv_nodes_is_end(nodes, it) {
            let n = lilv_nodes_get(nodes, it);
            if !n.is_null() && lilv_node_is_uri(n) {
                out.push(node_borrowed(n));
            }
            it = lilv_nodes_next(nodes, it);
        }
        lilv_nodes_free(nodes);
    }
    lilv_node_free(rdf_type);
    let class = lilv_plugin_get_class(plugin);
    if !class.is_null() {
        let uri = lilv_plugin_class_get_uri(class);
        if !uri.is_null() && lilv_node_is_uri(uri) {
            let u = node_borrowed(uri);
            if !out.contains(&u) { out.push(u); }
        }
    }
    out
}

/// Read a borrowed node (owned by the plugin, e.g. get_uri) — do NOT free.
unsafe fn node_borrowed(node: *const LilvNode) -> String {
    if node.is_null() {
        return String::new();
    }
    let ptr = lilv_node_as_string(node);
    if ptr.is_null() {
        String::new()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// Read an owned node (e.g. get_name) and free it.
unsafe fn node_owned(node: *mut LilvNode) -> String {
    let s = node_borrowed(node);
    if !node.is_null() {
        lilv_node_free(node);
    }
    s
}

/// Pick the most specific `lv2core` plugin class local name from a plugin's
/// `rdf:type` list (e.g. `ReverbPlugin` over `DelayPlugin`/`SimulatorPlugin`,
/// `CompressorPlugin` over `DynamicsPlugin`, `ParaEQPlugin` over `FilterPlugin`).
pub fn primary_lv2_class(types: &[String]) -> String {
    fn rank(local: &str) -> u8 {
        match local {
            "InstrumentPlugin" => 100,
            "ReverbPlugin" => 95,
            "ChorusPlugin" | "FlangerPlugin" | "PhaserPlugin" => 92,
            "ParaEQPlugin" | "MultiEQPlugin" => 90,
            "LowpassPlugin" | "HighpassPlugin" | "BandpassPlugin" | "CombPlugin" | "AllpassPlugin" => 88,
            "CompressorPlugin" | "LimiterPlugin" | "GatePlugin" | "ExpanderPlugin" | "EnvelopePlugin" => 86,
            "WaveshaperPlugin" => 84,
            "PitchPlugin" => 82,
            "AnalyserPlugin" => 80,
            "MixerPlugin" | "ConverterPlugin" | "FunctionPlugin" => 78,
            "OscillatorPlugin" | "ConstantPlugin" => 76,
            "EQPlugin" => 74,
            "FilterPlugin" => 70,
            "DistortionPlugin" => 68,
            "AmplifierPlugin" => 66,
            "DelayPlugin" => 64,
            "ModulatorPlugin" => 62,
            "SpectralPlugin" | "SpatialPlugin" => 60,
            "SimulatorPlugin" => 58,
            "DynamicsPlugin" => 54,
            "GeneratorPlugin" => 50,
            "UtilityPlugin" => 40,
            _ => 10,
        }
    }
    let mut best: Option<(&str, u8)> = None;
    for t in types {
        let Some(local) = t.strip_prefix(LV2_CORE_PREFIX) else { continue };
        if local == "Plugin" || local == "PluginBase" || !local.ends_with("Plugin") {
            continue;
        }
        let r = rank(local);
        if best.is_none_or(|(_, br)| r > br) {
            best = Some((local, r));
        }
    }
    best.map(|(s, _)| s.to_string()).unwrap_or_default()
}

/// Why the plugin's binary can never load on this machine (built for another
/// CPU or system, #135), read from the file's header.
unsafe fn foreign_binary(plugin: *const LilvPlugin) -> Option<String> {
    let lib = lilv_plugin_get_library_uri(plugin);
    if lib.is_null() {
        return None;
    }
    let path = lilv_file_uri_parse(lilv_node_as_uri(lib), std::ptr::null_mut());
    if path.is_null() {
        return None;
    }
    let file = std::path::PathBuf::from(std::ffi::CStr::from_ptr(path).to_string_lossy().into_owned());
    lilv_free(path as *mut c_void);
    crate::binarch::foreign(&file)
}

/// Discover all installed LV2 plugins (loads the lilv world, enumerates, frees).
pub fn discover() -> Vec<Lv2PluginInfo> {
    discover_with_unloadable().0
}

/// [`discover`], plus one sentence per plugin whose binary was built for
/// another CPU or system. Those are not listed: lilv reads only their
/// Turtle files, and they could never be instantiated.
pub fn discover_with_unloadable() -> (Vec<Lv2PluginInfo>, Vec<String>) {
    let _lilv = lilv_guard();
    unsafe {
        let world = lilv_world_new();
        if world.is_null() {
            return (Vec::new(), Vec::new());
        }
        lilv_world_load_all(world);
        let found = collect_plugins(world);
        lilv_world_free(world);
        found
    }
}

/// What [`discover_with_unloadable`] reports for the plugins `world` holds.
unsafe fn collect_plugins(world: *mut LilvWorld) -> (Vec<Lv2PluginInfo>, Vec<String>) {
    {
        let plugins = lilv_world_get_all_plugins(world);
        let mut out = Vec::new();
        let mut unloadable: Vec<String> = Vec::new();
        if !plugins.is_null() {
            let mut iter = lilv_plugins_begin(plugins);
            while !iter.is_null() && !lilv_plugins_is_end(plugins, iter) {
                let plugin = lilv_plugins_get(plugins, iter);
                if let Some(reason) = if plugin.is_null() { None } else { foreign_binary(plugin) } {
                    // One binary often holds several plugins: say it once.
                    if !unloadable.contains(&reason) {
                        eprintln!("skip LV2 plugin {}: {reason}", node_borrowed(lilv_plugin_get_uri(plugin)));
                        unloadable.push(reason);
                    }
                } else if !plugin.is_null() {
                    let types = plugin_types(world, plugin);
                    let is_instrument = instrument_from_types(&types).unwrap_or_else(|| {
                        let (atom_in, audio_in, audio_out) = port_counts(world, plugin);
                        atom_in && audio_in == 0 && audio_out > 0
                    });
                    let is_midi_fx = if is_instrument {
                        false
                    } else {
                        let (atom_in, atom_out, audio) = midi_shape(world, plugin);
                        atom_in && atom_out && audio == 0
                    };
                    out.push(Lv2PluginInfo {
                        uri: node_borrowed(lilv_plugin_get_uri(plugin)),
                        name: node_owned(lilv_plugin_get_name(plugin)),
                        author: node_owned(lilv_plugin_get_author_name(plugin)),
                        lv2_class: primary_lv2_class(&types),
                        num_ports: lilv_plugin_get_num_ports(plugin),
                        is_instrument,
                        is_midi_fx,
                    });
                }
                iter = lilv_plugins_next(plugins, iter);
            }
        }
        instruments_first(&mut out);
        (out, unloadable)
    }
}

/// lilv lists plugins by URI, so `urn:` instruments such as Vitalium came
/// after hundreds of `http:` effects and fell off every truncated listing:
/// the agent never saw Vitalium's URI and guessed ten wrong ones.
pub fn instruments_first(plugins: &mut [Lv2PluginInfo]) {
    plugins.sort_by_key(|p| (!p.is_instrument, !p.is_midi_fx));
}

/// CLI self-test: list installed LV2 plugins.
pub fn run_test() -> anyhow::Result<()> {
    let plugins = discover();
    println!("=== LV2 discovery (lilv FFI) ===");
    println!("{} plugins installed", plugins.len());
    for p in plugins.iter().take(40) {
        println!("  {} ({} ports) — {}", p.name, p.num_ports, p.uri);
    }
    if std::env::var_os("KABELSALAT_LV2_TYPES").is_some() {
        let _lilv = lilv_guard();
        unsafe {
            let world = lilv_world_new();
            lilv_world_load_all(world);
            let all = lilv_world_get_all_plugins(world);
            let mut it = lilv_plugins_begin(all);
            while !lilv_plugins_is_end(all, it) {
                let pl = lilv_plugins_get(all, it);
                let types: Vec<String> = plugin_types(world, pl).iter()
                    .map(|t| t.rsplit('#').next().unwrap_or(t).to_string()).collect();
                println!("TYPES {} | hint={} | {}", node_borrowed(lilv_plugin_get_uri(pl)), classify_instrument(world, pl), types.join(","));
                it = lilv_plugins_next(all, it);
            }
            lilv_world_free(world);
        }
    }
    let instruments: Vec<&Lv2PluginInfo> = plugins.iter().filter(|p| p.is_instrument).collect();
    println!("{} instruments (atom input + audio output):", instruments.len());
    for p in instruments.iter().take(20) {
        println!("  [inst] {} ({}) — {}", p.name, p.author, p.uri);
    }
    println!("By type:");
    let subs: Vec<_> = plugins.iter().map(crate::plugins::classify_lv2).collect();
    for ty in crate::plugins::PluginType::ALL {
        let per_sub: Vec<String> = crate::plugins::PluginSubtype::subtypes_for(ty)
            .iter()
            .filter_map(|sub| {
                let n = subs.iter().filter(|s| *s == sub).count();
                (n > 0).then(|| format!("{}={n}", sub.label()))
            })
            .collect();
        let n = subs.iter().filter(|s| s.plugin_type() == ty).count();
        println!("  {}: {n} ({})", ty.label(), per_sub.join(", "));
    }
    anyhow::ensure!(!plugins.is_empty(), "no LV2 plugins discovered");
    println!("lv2-test: OK — lilv linkage + discovery verified");
    Ok(())
}

/// Find a plugin whose URI contains `needle` (helper for the hosting test).
unsafe fn find_plugin(world: *mut LilvWorld, needle: &str) -> *const LilvPlugin {
    let plugins = lilv_world_get_all_plugins(world);
    if plugins.is_null() {
        return std::ptr::null();
    }
    // An exact URI first: AVL Drumkits' "…#BlondeBop" is also part of
    // "…#BlondeBopHR" and "…#BlondeBopMulti".
    let mut fallback: *const LilvPlugin = std::ptr::null();
    let mut iter = lilv_plugins_begin(plugins);
    while !iter.is_null() && !lilv_plugins_is_end(plugins, iter) {
        let plugin = lilv_plugins_get(plugins, iter);
        if !plugin.is_null() {
            let uri = node_borrowed(lilv_plugin_get_uri(plugin));
            if uri == needle {
                return plugin;
            }
            if fallback.is_null() && uri.contains(needle) {
                fallback = plugin;
            }
        }
        iter = lilv_plugins_next(plugins, iter);
    }
    fallback
}

/// Hosting self-test: instantiate a simple audio gain plugin, feed a known
/// signal, run it, and verify it produces output. Proves the LV2 audio path
/// (instantiate → connect ports → activate → run) end to end.
pub fn host_test() -> anyhow::Result<()> {
    unsafe {
        let world = lilv_world_new();
        anyhow::ensure!(!world.is_null(), "lilv_world_new failed");
        lilv_world_load_all(world);

        // LV2 core port classes.
        let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
        let control = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#ControlPort\0".as_ptr() as *const c_char);
        let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);

        // Host URID-map feature (most plugins require it for atom/option URIs).
        let mut urid = UridMap { map: std::collections::HashMap::new(), next: 1 };
        let handle = &mut urid as *mut UridMap as *mut c_void;
        let map_data = Lv2UridMap { handle, map: urid_map_cb };
        let map_feature = Lv2Feature {
            uri: b"http://lv2plug.in/ns/ext/urid#map\0".as_ptr() as *const c_char,
            data: &map_data as *const Lv2UridMap as *mut c_void,
        };

        // Options feature: sampleRate + maxBlockLength (Carla et al. require it).
        let map_uri = |s: &[u8]| urid_map_cb(handle, s.as_ptr() as *const c_char);
        let sr_key = map_uri(b"http://lv2plug.in/ns/lv2core#sampleRate\0");
        let bl_key = map_uri(b"http://lv2plug.in/ns/ext/buf-size#maxBlockLength\0");
        let float_ty = map_uri(b"http://lv2plug.in/ns/ext/atom#Float\0");
        let int_ty = map_uri(b"http://lv2plug.in/ns/ext/atom#Int\0");
        let sample_rate_val: f32 = 48000.0;
        let block_len_val: i32 = 256;
        let options = [
            Lv2Option { context: 0, subject: 0, key: sr_key, size: 4, ty: float_ty, value: &sample_rate_val as *const f32 as *const c_void },
            Lv2Option { context: 0, subject: 0, key: bl_key, size: 4, ty: int_ty, value: &block_len_val as *const i32 as *const c_void },
            Lv2Option { context: 0, subject: 0, key: 0, size: 0, ty: 0, value: std::ptr::null() },
        ];
        let options_feature = Lv2Feature {
            uri: b"http://lv2plug.in/ns/ext/options#options\0".as_ptr() as *const c_char,
            data: options.as_ptr() as *mut c_void,
        };
        let features: [*const Lv2Feature; 3] = [&map_feature, &options_feature, std::ptr::null()];

        // Try several simple audio plugins until one instantiates.
        let candidates = ["audiogain_s", "audiogain", "amp", "gx_amp", "fluida", "simple"];
        let mut plugin = std::ptr::null();
        let mut instance = std::ptr::null_mut();
        let mut chosen = String::new();
        for c in candidates {
            let p = find_plugin(world, c);
            if p.is_null() {
                continue;
            }
            let inst = lilv_plugin_instantiate(p, 48000.0, features.as_ptr());
            if !inst.is_null() {
                plugin = p;
                instance = inst;
                chosen = node_borrowed(lilv_plugin_get_uri(p));
                break;
            }
        }
        anyhow::ensure!(!instance.is_null(), "no candidate plugin could be instantiated");
        let _ = plugin;
        let desc = &*(*instance).lv2_descriptor;
        let handle = (*instance).lv2_handle;
        let connect_port = desc.connect_port.expect("LV2 descriptor has no connect_port");
        let run = desc.run.expect("LV2 descriptor has no run");

        let nports = lilv_plugin_get_num_ports(plugin);
        let frames = 256usize;
        let mut buffers: Vec<Vec<f32>> = (0..nports).map(|_| vec![0.0f32; frames]).collect();
        let mut audio_in = Vec::new();
        let mut audio_out = Vec::new();
        for i in 0..nports {
            let port = lilv_plugin_get_port_by_index(plugin, i);
            let is_audio = lilv_port_is_a(plugin, port, audio);
            let is_control = lilv_port_is_a(plugin, port, control);
            let is_in = lilv_port_is_a(plugin, port, input);
            if is_audio && is_in {
                audio_in.push(i as usize);
            } else if is_audio && !is_in {
                audio_out.push(i as usize);
            } else if is_control && is_in {
                buffers[i as usize][0] = read_port_range(plugin, port).0;
            }
            connect_port(handle, i, buffers[i as usize].as_mut_ptr() as *mut c_void);
        }
        anyhow::ensure!(!audio_in.is_empty() && !audio_out.is_empty(), "{chosen} has no audio in/out ports");

        // Feed a constant 0.5 signal.
        for &i in &audio_in {
            for s in buffers[i].iter_mut() {
                *s = 0.5;
            }
        }
        if let Some(activate) = desc.activate {
            activate(handle);
        }
        run(handle, frames as u32);
        if let Some(deactivate) = desc.deactivate {
            deactivate(handle);
        }

        let out_peak = audio_out
            .iter()
            .flat_map(|&i| buffers[i].iter())
            .fold(0.0f32, |m, s| m.max(s.abs()));
        let uri = chosen.clone();
        lilv_instance_free(instance);
        lilv_node_free(audio);
        lilv_node_free(control);
        lilv_node_free(input);
        lilv_world_free(world);

        anyhow::ensure!(out_peak > 1e-4, "LV2 plugin '{uri}' produced no output (peak {out_peak})");
        println!("lv2-host-test: OK — hosted '{uri}', ran {frames} frames, output peak {out_peak:.4}");
        Ok(())
    }
}

/// Read the plugin's `worker#interface` from its descriptor, if any.
unsafe fn query_worker_interface(desc: &Lv2Descriptor) -> Option<Lv2WorkerInterface> {
    let ext = desc.extension_data?;
    let ptr = ext(LV2_WORKER_INTERFACE_URI.as_ptr() as *const c_char);
    if ptr.is_null() {
        None
    } else {
        Some(std::ptr::read(ptr as *const Lv2WorkerInterface))
    }
}

const LV2_MIDNAM_INTERFACE_URI: &[u8] = b"http://ardour.org/lv2/midnam#interface\0";
const LV2_STATE_INTERFACE_URI: &[u8] = b"http://lv2plug.in/ns/ext/state#interface\0";
const LV2_STATE_IS_POD: u32 = 1;
const LV2_STATE_IS_PORTABLE: u32 = 2;

type Lv2StateRetrieve = extern "C" fn(*mut c_void, u32, *mut usize, *mut u32, *mut u32) -> *const c_void;

type Lv2StateStore = extern "C" fn(*mut c_void, u32, *const c_void, usize, u32, u32) -> i32;

/// `LV2_State_Interface` (lv2/state/state.h).
#[repr(C)]
struct Lv2StateInterface {
    save: Option<extern "C" fn(Lv2Handle, Lv2StateStore, *mut c_void, u32, *const *const Lv2Feature) -> i32>,
    restore: Option<extern "C" fn(Lv2Handle, Lv2StateRetrieve, *mut c_void, u32, *const *const Lv2Feature) -> i32>,
}

/// How a plugin's state is handed over: the one property it restores from,
/// and the check a value must pass before the plugin sees it.
pub struct StateFormat {
    pub key: &'static str,
    pub kind: &'static str,
    pub check: fn(&[u8]) -> Result<(), String>,
}

const ATOM_CHUNK: &str = "http://lv2plug.in/ns/ext/atom#Chunk";

/// The state format of a plugin whose state the app hands over; None for
/// every other plugin. JUCE's LV2 wrapper keeps the whole state in one
/// chunk (`urn:juce:stateBinary`); for Vitalium that chunk is a Vital patch.
pub fn state_format(uri: &str) -> Option<StateFormat> {
    (uri == crate::vital::URI).then_some(StateFormat { key: "urn:juce:stateBinary", kind: ATOM_CHUNK, check: crate::vital::check_patch })
}

/// One property a restore hands the plugin: (key URID, type URID, value).
struct StateProperty<'a> {
    key: u32,
    kind: u32,
    value: &'a [u8],
}

extern "C" fn state_store(handle: *mut c_void, key: u32, value: *const c_void, size: usize, kind: u32, _flags: u32) -> i32 {
    unsafe {
        let saved = &mut *(handle as *mut Vec<(u32, u32, Vec<u8>)>);
        saved.push((key, kind, std::slice::from_raw_parts(value as *const u8, size).to_vec()));
    }
    0
}

extern "C" fn state_retrieve(handle: *mut c_void, key: u32, size: *mut usize, kind: *mut u32, flags: *mut u32) -> *const c_void {
    unsafe {
        let props = &*(handle as *const Vec<StateProperty<'_>>);
        let Some(p) = props.iter().find(|p| p.key == key) else { return std::ptr::null() };
        if !size.is_null() { *size = p.value.len(); }
        if !kind.is_null() { *kind = p.kind; }
        if !flags.is_null() { *flags = LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE; }
        p.value.as_ptr() as *const c_void
    }
}

/// Ardour's MIDNAM interface (`extension_data(midnam#interface)`): the plugin
/// hands out a MIDNAM XML document that names its MIDI programmes.
#[repr(C)]
struct Lv2MidnamInterface {
    midnam: Option<extern "C" fn(Lv2Handle) -> *mut c_char>,
    model: Option<extern "C" fn(Lv2Handle) -> *mut c_char>,
    free: Option<extern "C" fn(*mut c_char)>,
}

/// `(MIDI programme number, name)` of every `<Patch>` in a MIDNAM document.
pub fn parse_midnam_programs(xml: &str) -> Vec<(u8, String)> {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let at = tag.find(&format!("{name}=\""))? + name.len() + 2;
        let end = tag[at..].find('"')?;
        Some(tag[at..at + end].replace("&amp;", "&").replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">"))
    };
    xml.split("<Patch ").skip(1).filter_map(|rest| {
        let tag = &rest[..rest.find('>')?];
        let program: u8 = attr(tag, "ProgramChange")?.trim().parse().ok().filter(|p| *p < 128)?;
        let name = attr(tag, "Name")?.trim().to_string();
        (!name.is_empty()).then_some((program, name))
    }).collect()
}

/// The MIDI programmes `uri` names in its MIDNAM document; empty when it does
/// not declare the interface. Instantiates the plugin to ask it, so this
/// belongs on a scan or loader thread (rule 46), never the audio or UI thread.
pub fn midi_programs(uri: &str) -> Vec<(u8, String)> {
    let declared = {
        let _lilv = lilv_guard();
        unsafe {
            let world = lilv_world_new();
            if world.is_null() {
                return Vec::new();
            }
            lilv_world_load_all(world);
            let plugin = find_plugin(world, uri);
            let ext = lilv_new_uri(world, LV2_MIDNAM_INTERFACE_URI.as_ptr() as *const c_char);
            let declared = !plugin.is_null() && lilv_plugin_has_extension_data(plugin, ext);
            lilv_node_free(ext);
            lilv_world_free(world);
            declared
        }
    };
    if !declared {
        return Vec::new();
    }
    let Ok(plugin) = Lv2Plugin::load(uri, 48000.0, 256) else { return Vec::new() };
    let programs = plugin.midnam().map(|xml| parse_midnam_programs(&xml)).unwrap_or_default();
    retire(plugin);
    programs
}

// ---------------------------------------------------------------------------
// Reusable LV2 plugin wrapper (foundation for engine rig nodes)
// ---------------------------------------------------------------------------

/// A hosted LV2 plugin instance with audio buffers, ready to process blocks.
/// Owns the lilv world + instance and the persistent URID-map feature data.
/// `process` is allocation-free (buffers preallocated); `load` and `Drop`
/// block (world load, instantiate, free) and belong on the engine's LV2
/// loader thread, never the audio thread.
pub struct Lv2Plugin {
    world: *mut LilvWorld,
    instance: *mut LilvInstance,
    buffers: Vec<Vec<f32>>,
    audio_in: Vec<usize>,
    audio_out: Vec<usize>,
    control_in: Vec<usize>,
    control_ranges: Vec<(f32, f32)>,
    control_meta: Vec<ControlMeta>,
    control_by_symbol: std::collections::HashMap<String, usize>,
    // MIDI/atom instrument support.
    atom_in: Option<usize>,
    atom_buf: Vec<u8>,
    /// The last MIDI event handed to the plugin (verification only).
    last_event: Option<[u8; 3]>,
    /// Atom OUTPUT ports (#38): a u64-backed (8-byte aligned) buffer each;
    /// the header is reset to an empty Chunk of full capacity before run().
    atom_out: Vec<(usize, Vec<u64>)>,
    urid_chunk: u32,
    urid_sequence: u32,
    urid_midi: u32,
    is_inst: bool,
    is_midi_fx: bool,
    _urid: Box<UridMap>,
    _map_data: Box<Lv2UridMap>,
    // Worker extension: the schedule feature handed to instantiate, the shared
    // handle it points into (kept alive for the instance's life), and the
    // per-instance worker thread when the plugin exposes `worker#interface`.
    _worker_schedule: Option<Box<Lv2WorkerSchedule>>,
    _worker_shared: Option<Box<WorkerShared>>,
    worker: Option<Box<Lv2Worker>>,
    block: usize,
    uri: String,
    sample_rate: f64,
    /// The control output the plugin writes its latency to
    /// (`lv2:reportsLatency`), when it has one.
    latency_port: Option<usize>,
}

/// Bundles `Lv2Plugin::load` reads beside the LV2 search path.
static EXTRA_BUNDLES: std::sync::Mutex<Vec<std::ffi::CString>> = std::sync::Mutex::new(Vec::new());

/// Let `Lv2Plugin::load` find the plugins of the bundle folder `dir`,
/// wherever it lies (rule 116: a test loads its own bundle by path, and
/// `LV2_PATH` is the whole process's).
pub fn add_bundle(dir: &std::path::Path) {
    let mut path = dir.to_string_lossy().into_owned();
    if !path.ends_with('/') {
        path.push('/');
    }
    let Ok(path) = std::ffi::CString::new(path) else { return };
    let mut extra = EXTRA_BUNDLES.lock().unwrap_or_else(|e| e.into_inner());
    if !extra.contains(&path) {
        extra.push(path);
    }
}

/// Plugins whose instances must never be freed (#14): Vitalium's own
/// threads keep running across instance teardown, so freeing (or creating
/// next to a freed one) corrupted the heap — every crash core showed a
/// Vitalium thread next to `lilv_instance_free`/`lilv_world_free` or a fresh
/// instantiate. Retired instances are parked instead and reused by `load`.
const NEVER_FREE: &[&str] = &["urn:distrho:vitalium"];

/// Parked, deactivated instances of `NEVER_FREE` plugins, reused by `load`.
/// Never dropped — not even at exit (statics are not destructed).
static PARKED: std::sync::Mutex<Vec<Lv2Plugin>> = std::sync::Mutex::new(Vec::new());

fn never_free(uri: &str) -> bool {
    NEVER_FREE.iter().any(|u| uri.contains(u))
}

/// Retire a plugin instance off the audio thread: parks `NEVER_FREE`
/// plugins for reuse, frees everything else. Use this instead of `drop`.
pub fn retire(mut p: Lv2Plugin) {
    if never_free(&p.uri) {
        p.set_active(false);
        if let Ok(mut parked) = PARKED.lock() { parked.push(p); return; }
        std::mem::forget(p); // poisoned pool: leak rather than free
        return;
    }
    drop(p);
}

/// How many instances are parked (tests).
pub fn parked_count(uri_part: &str) -> usize {
    PARKED.lock().map(|v| v.iter().filter(|p| p.uri.contains(uri_part)).count()).unwrap_or(0)
}

// SAFETY: one thread owns an instance at a time (moved over the engine's
// rings: loader → audio thread for `process`, back to the loader to free);
// world/instance creation and freeing hold `LILV_LOCK`, so lilv is never
// used from two threads at once.
unsafe impl Send for Lv2Plugin {}

impl Lv2Plugin {
    /// Load the first plugin whose URI contains `needle`, instantiate it with
    /// urid-map + options features, classify ports, and connect buffers.
    pub fn load(needle: &str, sample_rate: f64, block: usize) -> anyhow::Result<Self> {
        if let Some(p) = Self::take_parked(needle, sample_rate, block) {
            return Ok(p);
        }
        let _lilv = lilv_guard();
        unsafe {
            let world = lilv_world_new();
            anyhow::ensure!(!world.is_null(), "lilv_world_new failed");
            lilv_world_load_all(world);
            for dir in EXTRA_BUNDLES.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                let bundle = lilv_new_file_uri(world, std::ptr::null(), dir.as_ptr());
                lilv_world_load_bundle(world, bundle);
                lilv_node_free(bundle);
            }
            let plugin = find_plugin(world, needle);
            anyhow::ensure!(!plugin.is_null(), "LV2 plugin not found: {needle}");
            let uri = node_borrowed(lilv_plugin_get_uri(plugin));
            // A control output like any other: it is connected below.
            let latency_port = lilv_plugin_has_latency(plugin).then(|| lilv_plugin_get_latency_port_index(plugin) as usize);

            let mut urid = Box::new(UridMap { map: std::collections::HashMap::new(), next: 1 });
            let handle = &mut *urid as *mut UridMap as *mut c_void;
            let mut map_data = Box::new(Lv2UridMap { handle, map: urid_map_cb });
            let map_feature = Lv2Feature {
                uri: b"http://lv2plug.in/ns/ext/urid#map\0".as_ptr() as *const c_char,
                data: &mut *map_data as *mut Lv2UridMap as *mut c_void,
            };
            let map_uri = |s: &[u8]| urid_map_cb(handle, s.as_ptr() as *const c_char);
            let sr_key = map_uri(b"http://lv2plug.in/ns/lv2core#sampleRate\0");
            let bl_key = map_uri(b"http://lv2plug.in/ns/ext/buf-size#maxBlockLength\0");
            let float_ty = map_uri(b"http://lv2plug.in/ns/ext/atom#Float\0");
            let int_ty = map_uri(b"http://lv2plug.in/ns/ext/atom#Int\0");
            let sr_val: f32 = sample_rate as f32;
            let bl_val: i32 = block as i32;
            let options = [
                Lv2Option { context: 0, subject: 0, key: sr_key, size: 4, ty: float_ty, value: &sr_val as *const f32 as *const c_void },
                Lv2Option { context: 0, subject: 0, key: bl_key, size: 4, ty: int_ty, value: &bl_val as *const i32 as *const c_void },
                Lv2Option { context: 0, subject: 0, key: 0, size: 0, ty: 0, value: std::ptr::null() },
            ];
            let options_feature = Lv2Feature {
                uri: b"http://lv2plug.in/ns/ext/options#options\0".as_ptr() as *const c_char,
                data: options.as_ptr() as *mut c_void,
            };

            // Worker schedule feature: the handle points at a heap block that
            // stays put for the instance's life; the request ring is filled in
            // below once we know the plugin exposes the worker interface.
            let mut worker_shared = Box::new(WorkerShared {
                req: None,
                thread: std::thread::current(),
            });
            let worker_shared_ptr = &mut *worker_shared as *mut WorkerShared as *mut c_void;
            let worker_schedule = Box::new(Lv2WorkerSchedule {
                handle: worker_shared_ptr,
                schedule_work: schedule_work_cb,
            });
            let worker_feature = Lv2Feature {
                uri: LV2_WORKER_SCHEDULE_URI.as_ptr() as *const c_char,
                data: &*worker_schedule as *const Lv2WorkerSchedule as *mut c_void,
            };
            let features: [*const Lv2Feature; 4] = [&map_feature, &options_feature, &worker_feature, std::ptr::null()];

            if let Some(reason) = foreign_binary(plugin) {
                anyhow::bail!("{uri}: {reason}");
            }
            let instance = lilv_plugin_instantiate(plugin, sample_rate, features.as_ptr());
            anyhow::ensure!(!instance.is_null(), "instantiate failed for {uri}");
            pin_plugin_library(plugin);

            // If the plugin exposes worker#interface, start its worker thread
            // and point the schedule handle's request ring and wake at it.
            let desc0 = &*(*instance).lv2_descriptor;
            let h0 = (*instance).lv2_handle;
            let worker = query_worker_interface(desc0).map(|iface| {
                let (req_tx, req_rx) = rtrb::RingBuffer::<WorkerMsg>::new(WORKER_RING_CAP);
                let (resp_tx, resp_rx) = rtrb::RingBuffer::<WorkerMsg>::new(WORKER_RING_CAP);
                let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let shutdown2 = std::sync::Arc::clone(&shutdown);
                let ctx = WorkerCtx { iface, instance: h0 };
                let join = std::thread::Builder::new()
                    .name("kabelsalat-lv2-worker".into())
                    .spawn(move || worker_loop(req_rx, resp_tx, ctx, shutdown2))
                    .expect("spawn lv2 worker");
                worker_shared.req = Some(req_tx);
                worker_shared.thread = join.thread().clone();
                Box::new(Lv2Worker { iface, instance: h0, resp_rx, join: Some(join), shutdown })
            });

            let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
            let control = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#ControlPort\0".as_ptr() as *const c_char);
            let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
            let atom_class = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
            let urid_sequence = urid_map_cb(handle, b"http://lv2plug.in/ns/ext/atom#Sequence\0".as_ptr() as *const c_char);
            let urid_midi = urid_map_cb(handle, b"http://lv2plug.in/ns/ext/midi#MidiEvent\0".as_ptr() as *const c_char);
            let urid_chunk = urid_map_cb(handle, b"http://lv2plug.in/ns/ext/atom#Chunk\0".as_ptr() as *const c_char);
            let nports = lilv_plugin_get_num_ports(plugin);
            let mut buffers: Vec<Vec<f32>> = (0..nports).map(|_| vec![0.0f32; block]).collect();
            let mut audio_in = Vec::new();
            let mut audio_out = Vec::new();
            let mut control_in = Vec::new();
            let mut control_ranges = Vec::new();
            let mut control_meta = Vec::new();
            let mut control_by_symbol = std::collections::HashMap::new();
            let mut atom_in: Option<usize> = None;
            let mut atom_buf = vec![0u8; 4096];
            let mut atom_out: Vec<(usize, Vec<u64>)> = Vec::new();
            let desc = &*(*instance).lv2_descriptor;
            let h = (*instance).lv2_handle;
            let connect_port = desc.connect_port.expect("no connect_port");
            for i in 0..nports {
                let port = lilv_plugin_get_port_by_index(plugin, i);
                let is_audio = lilv_port_is_a(plugin, port, audio);
                let is_control = lilv_port_is_a(plugin, port, control);
                let is_atom = lilv_port_is_a(plugin, port, atom_class);
                let is_in = lilv_port_is_a(plugin, port, input);
                if is_atom && is_in {
                    atom_in = Some(i as usize);
                    init_atom_sequence(&mut atom_buf, urid_sequence);
                    connect_port(h, i, atom_buf.as_mut_ptr() as *mut c_void);
                    continue;
                }
                if is_atom {
                    let mut buf = vec![0u64; ATOM_OUT_BYTES / 8];
                    reset_atom_output(&mut buf, urid_chunk);
                    connect_port(h, i, buf.as_mut_ptr() as *mut c_void);
                    atom_out.push((i as usize, buf)); // the heap block never moves
                    continue;
                }
                if is_audio && is_in {
                    audio_in.push(i as usize);
                } else if is_audio && !is_in {
                    audio_out.push(i as usize);
                } else if is_control && is_in {
                    let (d, lo, hi) = read_port_range(plugin, port);
                    let symbol = node_borrowed(lilv_port_get_symbol(plugin, port));
                    let name = node_owned(lilv_port_get_name(plugin, port));
                    buffers[i as usize][0] = d;
                    control_in.push(i as usize);
                    control_ranges.push((lo, hi));
                    control_by_symbol.insert(symbol.clone(), i as usize);
                    control_meta.push(ControlMeta {
                        port_index: i as usize,
                        symbol,
                        name,
                        min: lo,
                        max: hi,
                        default: d,
                    });
                }
                connect_port(h, i, buffers[i as usize].as_mut_ptr() as *mut c_void);
            }
            lilv_node_free(audio);
            lilv_node_free(control);
            lilv_node_free(input);
            lilv_node_free(atom_class);
            let is_midi_fx = classify_midi_fx(world, plugin);
            anyhow::ensure!(!audio_out.is_empty() || is_midi_fx, "{uri} has no audio output");
            anyhow::ensure!(!audio_in.is_empty() || atom_in.is_some(), "{uri} has no audio/atom input");
            if let Some(activate) = desc.activate {
                activate(h);
            }
            Ok(Self { world, instance, buffers, audio_in, audio_out, control_in, control_ranges, control_meta, control_by_symbol, atom_in, atom_buf, last_event: None, atom_out, urid_chunk, urid_sequence, urid_midi, is_inst: classify_instrument(world, plugin), is_midi_fx, _urid: urid, _map_data: map_data, _worker_schedule: Some(worker_schedule), _worker_shared: Some(worker_shared), worker, block, uri, sample_rate, latency_port })
        }
    }

    /// A parked instance of `needle` at this rate/block, reset to a fresh
    /// state: re-activated, controls at their declared defaults (rule 42),
    /// audio and MIDI buffers cleared.
    fn take_parked(needle: &str, sample_rate: f64, block: usize) -> Option<Self> {
        if !never_free(needle) { return None; }
        let mut parked = PARKED.lock().ok()?;
        let i = parked.iter().position(|p| p.uri.contains(needle) && p.sample_rate == sample_rate && p.block == block)?;
        let mut p = parked.swap_remove(i);
        drop(parked);
        for m in &p.control_meta {
            if let Some(b) = p.buffers.get_mut(m.port_index) { b.fill(m.default); }
        }
        for &i in p.audio_in.iter().chain(p.audio_out.iter()) {
            if let Some(b) = p.buffers.get_mut(i) { b.fill(0.0); }
        }
        if p.atom_in.is_some() { init_atom_sequence(&mut p.atom_buf, p.urid_sequence); }
        p.last_event = None;
        p.set_active(true);
        Some(p)
    }

    /// Plugin activate/deactivate (instance run state; buffers stay).
    fn set_active(&mut self, on: bool) {
        unsafe {
            if self.instance.is_null() { return; }
            let desc = &*(*self.instance).lv2_descriptor;
            let h = (*self.instance).lv2_handle;
            match (on, desc.activate, desc.deactivate) {
                (true, Some(a), _) => a(h),
                (false, _, Some(d)) => d(h),
                _ => {}
            }
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Whether the plugin reports a latency (`lv2:reportsLatency`), which
    /// may then change with any block.
    pub fn reports_latency(&self) -> bool {
        self.latency_port.is_some()
    }

    /// The latency the plugin wrote in its last `run`, in frames; 0 before
    /// the first one and for a plugin that reports none. Allocation-free.
    pub fn latency_frames(&self) -> u32 {
        let reported = self.latency_port.and_then(|i| self.buffers.get(i)).map_or(0.0, |b| b[0]);
        if reported.is_finite() && reported > 0.0 { reported.round() as u32 } else { 0 }
    }

    /// Number of control-input ports (knobs).
    pub fn control_count(&self) -> usize {
        self.control_in.len()
    }

    /// Set the nth control port from a normalized 0..1 knob, mapped to the
    /// port's native range. Allocation-free (audio-thread safe).
    pub fn set_control_normalized(&mut self, nth: usize, value: f32) {
        if let (Some(&port), Some(&(lo, hi))) = (self.control_in.get(nth), self.control_ranges.get(nth)) {
            let v = lo + (hi - lo) * value.clamp(0.0, 1.0);
            self.buffers[port][0] = v;
        }
    }

    /// The nth control input as a normalized 0..1 knob value (allocation-free).
    pub fn control_normalized(&self, nth: usize) -> Option<f32> {
        let &port = self.control_in.get(nth)?;
        let &(lo, hi) = self.control_ranges.get(nth)?;
        let v = self.buffers[port][0];
        Some(if hi > lo { ((v - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 0.0 })
    }

    /// Describe all control-input ports with their current buffer values.
    pub fn control_descs(&self) -> Vec<Lv2ControlDesc> {
        self.control_meta
            .iter()
            .enumerate()
            .map(|(index, meta)| Lv2ControlDesc {
                index,
                symbol: meta.symbol.clone(),
                name: meta.name.clone(),
                min: meta.min,
                max: meta.max,
                default: meta.default,
                value: self.buffers[meta.port_index][0],
            })
            .collect()
    }

    /// Apply a preset by setting matching control-input port buffers by symbol.
    pub fn apply_preset(&mut self, preset: &Lv2Preset) {
        for (sym, val) in &preset.values {
            if let Some(&port) = self.control_by_symbol.get(sym) {
                self.buffers[port][0] = *val;
            }
        }
    }

    /// True if this is an instrument (atom/MIDI input + audio output).
    pub fn is_instrument(&self) -> bool {
        self.is_inst
    }

    /// True for a MIDI-only unit (MIDI in, MIDI out, no audio port).
    pub fn is_midi_fx(&self) -> bool {
        self.is_midi_fx
    }

    /// Queue one raw 3-byte channel-voice message (channel kept) for the
    /// next run(). Allocation-free; a full buffer drops the event.
    pub fn send_midi(&mut self, ev: [u8; 3]) {
        self.queue_midi(ev);
    }

    /// Append the MIDI events the plugin wrote to its first atom output in
    /// the last run() into `out` (bounded by `out`'s capacity, so a
    /// preallocated vector never allocates). Note-on/off arrive as such,
    /// other channel-voice messages as raw bytes.
    pub fn read_midi_out(&self, out: &mut Vec<FxOutEvent>) {
        let Some((_, buf)) = self.atom_out.first() else { return };
        // The chunk header was rewritten by the plugin into a sequence atom.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        let size = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let ty = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if ty != self.urid_sequence {
            return;
        }
        let end = (8 + size).min(bytes.len());
        let mut at = 16; // atom header (8) + sequence unit/pad (8)
        while at + 16 <= end && out.len() < out.capacity() {
            let n = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
            let ety = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap());
            let body = at + 16;
            if body + n > end {
                break;
            }
            if ety == self.urid_midi && n >= 3 {
                let d = [bytes[body], bytes[body + 1], bytes[body + 2]];
                let (status, ch) = (d[0] & 0xF0, d[0] & 0x0F);
                out.push(match status {
                    0x90 if d[2] > 0 => FxOutEvent::NoteOn { channel: ch, note: d[1], velocity: d[2] },
                    0x80 | 0x90 => FxOutEvent::NoteOff { channel: ch, note: d[1] },
                    0xB0 | 0xE0 | 0xA0 => FxOutEvent::Midi(d),
                    _ => {
                        at = body + ((n + 7) & !7);
                        continue;
                    }
                });
            }
            at = body + ((n + 7) & !7);
        }
    }

    /// Queue a MIDI note-on for the next run() (frame 0). Events queue up
    /// within a block, so every note of a chord reaches the plugin.
    pub fn note_on(&mut self, note: u8, velocity: u8) {
        self.note_on_ch(note, velocity, 0);
    }

    /// Queue a MIDI note-off for the next run().
    pub fn note_off(&mut self, note: u8) {
        self.note_off_ch(note, 0);
    }

    /// Note-on on a 0-based MIDI channel: the organ dock's Lower Manual
    /// plays channel 1 while the Upper plays channel 0 (#163).
    pub fn note_on_ch(&mut self, note: u8, velocity: u8, channel: u8) {
        self.queue_midi([0x90 | (channel & 0x0F), note, velocity]);
    }

    /// Note-off on a 0-based MIDI channel.
    pub fn note_off_ch(&mut self, note: u8, channel: u8) {
        self.queue_midi([0x80 | (channel & 0x0F), note, 0]);
    }

    /// Queue a MIDI control change (channel 1) for the next run().
    pub fn control_change(&mut self, cc: u8, value: u8) {
        self.queue_midi([0xB0, cc & 0x7F, value & 0x7F]);
    }

    /// Queue a channel-pressure (aftertouch) message: how wind patches read
    /// breath (allocation-free).
    pub fn channel_pressure(&mut self, value: u8) {
        self.midi(&[0xD0, value & 0x7F]);
    }

    /// Queue one channel message as it goes on the wire, 2 or 3 bytes
    /// (allocation-free).
    pub fn midi(&mut self, msg: &[u8]) {
        if self.atom_in.is_some() && msg.len() <= 3 && append_midi_event(&mut self.atom_buf, self.urid_midi, 0, msg) {
            let mut last = [0; 3];
            last[..msg.len()].copy_from_slice(msg);
            self.last_event = Some(last);
        }
    }

    /// Queue a MIDI programme change for the next run() (allocation-free).
    pub fn program_change(&mut self, program: u8) {
        let program = program.min(127);
        if self.atom_in.is_some() && append_midi_event(&mut self.atom_buf, self.urid_midi, 0, &[0xC0, program]) {
            self.last_event = Some([0xC0, program, 0]);
        }
    }

    /// The plugin's MIDNAM document, when it implements the interface.
    pub fn midnam(&self) -> Option<String> {
        unsafe {
            let desc = &*(*self.instance).lv2_descriptor;
            let ptr = (desc.extension_data?)(LV2_MIDNAM_INTERFACE_URI.as_ptr() as *const c_char);
            if ptr.is_null() {
                return None;
            }
            let iface = &*(ptr as *const Lv2MidnamInterface);
            let text = (iface.midnam?)((*self.instance).lv2_handle);
            if text.is_null() {
                return None;
            }
            let xml = CStr::from_ptr(text).to_string_lossy().into_owned();
            if let Some(free) = iface.free {
                free(text);
            }
            Some(xml)
        }
    }

    /// Restore the plugin's state from `(key URI, atom type URI, value)`
    /// properties through its `state#interface`. Blocks for as long as the
    /// plugin takes (a Vital patch rebuilds its wavetables): call it on the
    /// loader thread, before the instance goes to the audio thread.
    pub fn restore_state(&mut self, properties: &[(&str, &str, &[u8])]) -> anyhow::Result<()> {
        let restore = self.state_interface()?.restore.ok_or_else(|| anyhow::anyhow!("{} has no state restore", self.uri))?;
        // The JUCE wrapper pushes a control port to the plugin only when it
        // differs from the value it saw last; one block lets it see the
        // ports as they are now (a reused instance comes back at defaults),
        // so the first block after the restore overrides nothing.
        let silence = vec![0.0f32; 2];
        let mut out = vec![0.0f32; 2];
        self.process(&silence, &mut out, 1);
        let urid = &mut *self._urid as *mut UridMap as *mut c_void;
        let map = |uri: &str| {
            let c = std::ffi::CString::new(uri).unwrap_or_default();
            urid_map_cb(urid, c.as_ptr())
        };
        let props: Vec<StateProperty<'_>> = properties.iter().map(|(k, t, v)| StateProperty { key: map(k), kind: map(t), value: v }).collect();
        unsafe {
            let features: [*const Lv2Feature; 1] = [std::ptr::null()];
            let status = restore((*self.instance).lv2_handle, state_retrieve, &props as *const Vec<StateProperty<'_>> as *mut c_void, 0, features.as_ptr());
            anyhow::ensure!(status == 0, "{} refused the state (status {status})", self.uri);
        }
        Ok(())
    }

    /// The state property stored under `key`, as the plugin saves it now.
    /// Same thread rules as `restore_state`.
    pub fn saved_state(&mut self, key: &str) -> anyhow::Result<Vec<u8>> {
        let save = self.state_interface()?.save.ok_or_else(|| anyhow::anyhow!("{} has no state save", self.uri))?;
        let mut saved: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let c = std::ffi::CString::new(key)?;
        let key = urid_map_cb(&mut *self._urid as *mut UridMap as *mut c_void, c.as_ptr());
        unsafe {
            let features: [*const Lv2Feature; 1] = [std::ptr::null()];
            let status = save((*self.instance).lv2_handle, state_store, &mut saved as *mut _ as *mut c_void, 0, features.as_ptr());
            anyhow::ensure!(status == 0, "{} could not save its state (status {status})", self.uri);
        }
        saved.into_iter().find(|p| p.0 == key).map(|p| p.2).ok_or_else(|| anyhow::anyhow!("{} saved nothing under that key", self.uri))
    }

    fn state_interface(&self) -> anyhow::Result<&Lv2StateInterface> {
        unsafe {
            let desc = &*(*self.instance).lv2_descriptor;
            let ext = desc.extension_data.ok_or_else(|| anyhow::anyhow!("{} has no extension data", self.uri))?;
            let ptr = ext(LV2_STATE_INTERFACE_URI.as_ptr() as *const c_char);
            anyhow::ensure!(!ptr.is_null(), "{} has no state interface", self.uri);
            Ok(&*(ptr as *const Lv2StateInterface))
        }
    }

    fn queue_midi(&mut self, ev: [u8; 3]) {
        if self.atom_in.is_some() && append_midi_event(&mut self.atom_buf, self.urid_midi, 0, &ev) {
            self.last_event = Some(ev);
        }
    }

    /// The last MIDI event handed to the plugin (for verification).
    pub fn last_midi_event(&self) -> Option<[u8; 3]> {
        self.last_event
    }

    /// Bytes the plugin wrote to its first atom output in the last run()
    /// (the sequence body; 8 = an empty sequence). None without atom outputs.
    pub fn atom_output_size(&self) -> Option<u32> {
        self.atom_out.first().map(|(_, b)| (b[0] & 0xffff_ffff) as u32)
    }

    /// Process a stereo-interleaved block (allocation-free).
    pub fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        let frames = frames.min(self.block);
        // Worker spec: deliver any responses queued by the worker thread before
        // run(), then call end_run() after every run() (rule 46 — the plugin's
        // work_response/end_run run on the audio thread, in the run() context).
        if let Some(w) = self.worker.as_mut() {
            w.drain_responses();
        }
        unsafe {
            // A stereo plugin's inputs take L and R in port order; a mono
            // one takes L.
            let stereo = self.audio_in.len() >= 2;
            for (k, &i) in self.audio_in.iter().enumerate() {
                let ch = if stereo { k % 2 } else { 0 };
                for f in 0..frames {
                    self.buffers[i][f] = input.get(f * 2 + ch).copied().unwrap_or(0.0);
                }
            }
            // Atom outputs: an empty Chunk of full capacity before every run (#38).
            for (_, buf) in self.atom_out.iter_mut() {
                reset_atom_output(buf, self.urid_chunk);
            }
            let desc = &*(*self.instance).lv2_descriptor;
            (desc.run.expect("no run"))((*self.instance).lv2_handle, frames as u32);
        }
        // Inputs are consumed by run(): never deliver the same events twice.
        if self.atom_in.is_some() {
            init_atom_sequence(&mut self.atom_buf, self.urid_sequence);
        }
        if let Some(w) = self.worker.as_ref() {
            w.end_run();
        }
        for f in 0..frames {
            let l = self.audio_out.first().map(|&i| self.buffers[i][f]).unwrap_or(0.0);
            let r = self.audio_out.get(1).map(|&i| self.buffers[i][f]).unwrap_or(l);
            if f * 2 + 1 < output.len() {
                output[f * 2] = l;
                output[f * 2 + 1] = r;
            }
        }
    }
}

impl Drop for Lv2Plugin {
    fn drop(&mut self) {
        // Safety net for #14: a NEVER_FREE plugin that was dropped instead of
        // `retire`d is leaked whole (instance, world and the memory it points
        // into), never freed.
        if never_free(&self.uri) {
            self.set_active(false);
            std::mem::forget(std::mem::replace(&mut self._urid, Box::new(UridMap { map: Default::default(), next: 1 })));
            std::mem::forget(std::mem::replace(&mut self._map_data, Box::new(Lv2UridMap { handle: std::ptr::null_mut(), map: urid_map_cb })));
            std::mem::forget(std::mem::take(&mut self.buffers));
            std::mem::forget(std::mem::take(&mut self.atom_buf));
            std::mem::forget(std::mem::take(&mut self.atom_out));
            // A parked never-free instance keeps its worker thread alive too.
            std::mem::forget(self.worker.take());
            std::mem::forget(self._worker_shared.take());
            std::mem::forget(self._worker_schedule.take());
            return;
        }
        // Stop the worker thread (set the flag, wake it, join) before the lilv
        // instance is freed, so work()/work_response() can never run on a
        // dangling handle.
        let wake = self._worker_shared.as_ref().map(|s| s.thread.clone());
        if let Some(mut w) = self.worker.take() {
            w.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = &wake {
                t.unpark();
            }
            if let Some(join) = w.join.take() {
                let _ = join.join();
            }
        }
        let _lilv = lilv_guard();
        unsafe {
            if !self.instance.is_null() {
                let desc = &*(*self.instance).lv2_descriptor;
                if let Some(deactivate) = desc.deactivate {
                    deactivate((*self.instance).lv2_handle);
                }
                lilv_instance_free(self.instance);
            }
            if !self.world.is_null() {
                lilv_world_free(self.world);
            }
        }
    }
}

/// CLI self-test for the reusable wrapper: load a plugin, process a block.
pub fn voice_test() -> anyhow::Result<()> {
    let mut plugin = Lv2Plugin::load("audiogain_s", 48000.0, 256)
        .or_else(|_| Lv2Plugin::load("amp", 48000.0, 256))?;
    let input = vec![0.5f32; 256 * 2];
    let mut output = vec![0.0f32; 256 * 2];
    plugin.process(&input, &mut output, 256);
    let peak = output.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    anyhow::ensure!(peak > 1e-4, "Lv2Plugin produced no output (peak {peak})");
    // Deterministic knob check: sweeping a control must change the output
    // (direction-agnostic — port ranges/signs vary by plugin).
    if plugin.control_count() > 0 {
        let measure = |p: &mut Lv2Plugin, v: f32| -> f32 {
            p.set_control_normalized(0, v);
            let mut o = vec![0.0f32; 256 * 2];
            for _ in 0..4 {
                p.process(&input, &mut o, 256);
            }
            o.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        let peak_max = measure(&mut plugin, 1.0);
        let peak_min = measure(&mut plugin, 0.0);
        anyhow::ensure!(
            (peak_max - peak_min).abs() > 1e-3,
            "LV2 knob had no effect (max {peak_max} vs min {peak_min})"
        );
        println!("lv2-voice-test: knob control verified (control sweep {peak_min:.4} ↔ {peak_max:.4})");
    }
    println!("lv2-voice-test: OK — reusable Lv2Plugin '{}' processed 256 frames, peak {peak:.4}", plugin.uri());
    Ok(())
}

/// Capacity of each atom output port buffer, in bytes.
const ATOM_OUT_BYTES: usize = 8192;

/// Before run(), an atom OUTPUT port holds an atom:Chunk whose size is the
/// space the plugin may write (LV2 atom spec); a zeroed buffer reads as 0
/// bytes (setBfree: "LV message buffer is only 0 bytes").
fn reset_atom_output(buf: &mut [u64], chunk_urid: u32) {
    let capacity = (buf.len() * 8 - 8) as u64; // body bytes after the 8-byte header
    buf[0] = capacity | ((chunk_urid as u64) << 32); // atom.size | atom.type (LE)
}

/// Append one MIDI event to an atom:Sequence (8-byte aligned, per the LV2
/// atom spec). False (event dropped) if the buffer is full. Allocation-free.
fn append_midi_event(buf: &mut [u8], midi_urid: u32, frame: i64, midi: &[u8]) -> bool {
    let size = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize; // body bytes
    let e = 8 + size; // next event offset (body starts at 8; seq header is 8 of it)
    let padded = (16 + midi.len() + 7) & !7;
    if e + padded > buf.len() {
        return false;
    }
    buf[e..e + padded].fill(0);
    buf[e..e + 8].copy_from_slice(&frame.to_le_bytes()); // time.frames
    buf[e + 8..e + 12].copy_from_slice(&(midi.len() as u32).to_le_bytes()); // body.size
    buf[e + 12..e + 16].copy_from_slice(&midi_urid.to_le_bytes()); // body.type
    buf[e + 16..e + 16 + midi.len()].copy_from_slice(midi);
    buf[0..4].copy_from_slice(&((size + padded) as u32).to_le_bytes());
    true
}

/// Initialize an atom:Sequence buffer as empty (time unit = audio frames).
fn init_atom_sequence(buf: &mut [u8], seq_urid: u32) {
    buf[0..4].copy_from_slice(&8u32.to_le_bytes()); // atom.size = unit+pad only
    buf[4..8].copy_from_slice(&seq_urid.to_le_bytes()); // atom.type = Sequence
    buf[8..12].copy_from_slice(&0u32.to_le_bytes()); // body.unit = 0 (frames)
    buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // body.pad
}

const LV2_CORE_PREFIX: &str = "http://lv2plug.in/ns/lv2core#";
const LV2_PSET_PREFIX: &str = "http://lv2plug.in/ns/ext/presets#";
const RDFS_PREFIX: &str = "http://www.w3.org/2000/01/rdf-schema#";
const RDF_TYPE_URI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

#[derive(Debug, Clone, PartialEq)]
enum TtlTok {
    Uri(String),
    Str(String),
    Word(String),
    LBracket,
    RBracket,
    LParen,
    RParen,
    Semi,
    Comma,
    Dot,
}

fn tokenize_turtle(src: &str) -> Vec<TtlTok> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut toks = Vec::new();
    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'#' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'<' {
            i += 1;
            let start = i;
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
            let s = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            if i < len {
                i += 1;
            }
            toks.push(TtlTok::Uri(s));
            continue;
        }
        if b == b'"' {
            let triple = i + 2 < len && bytes[i + 1] == b'"' && bytes[i + 2] == b'"';
            i += if triple { 3 } else { 1 };
            let mut out = String::new();
            while i < len {
                if triple {
                    if i + 2 < len && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                        i += 3;
                        break;
                    }
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    match bytes[i] {
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        other => out.push(other as char),
                    }
                    i += 1;
                } else {
                    let ch = src[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            // Skip optional @lang or ^^datatype suffix.
            if i < len && bytes[i] == b'@' {
                i += 1;
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                    i += 1;
                }
            } else if i + 1 < len && bytes[i] == b'^' && bytes[i + 1] == b'^' {
                i += 2;
                if i < len && bytes[i] == b'<' {
                    while i < len && bytes[i] != b'>' {
                        i += 1;
                    }
                    if i < len {
                        i += 1;
                    }
                } else {
                    while i < len && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b';' | b',' | b'.' | b']' | b')') {
                        i += 1;
                    }
                }
            }
            toks.push(TtlTok::Str(out));
            continue;
        }
        match b {
            b'[' => { toks.push(TtlTok::LBracket); i += 1; continue; }
            b']' => { toks.push(TtlTok::RBracket); i += 1; continue; }
            b'(' => { toks.push(TtlTok::LParen); i += 1; continue; }
            b')' => { toks.push(TtlTok::RParen); i += 1; continue; }
            b';' => { toks.push(TtlTok::Semi); i += 1; continue; }
            b',' => { toks.push(TtlTok::Comma); i += 1; continue; }
            b'.' if !(i + 1 < len && bytes[i + 1].is_ascii_digit()) => {
                toks.push(TtlTok::Dot);
                i += 1;
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < len {
            let c = bytes[i];
            if c.is_ascii_whitespace() || matches!(c, b'[' | b']' | b'(' | b')' | b';' | b',' | b'#' | b'"' | b'<') {
                break;
            }
            if c == b'.' && !(i + 1 < len && bytes[i + 1].is_ascii_digit()) {
                break;
            }
            i += 1;
        }
        if i > start {
            toks.push(TtlTok::Word(String::from_utf8_lossy(&bytes[start..i]).into_owned()));
        } else {
            i += 1;
        }
    }
    toks
}

fn resolve_term(tok: &TtlTok, prefixes: &std::collections::HashMap<String, String>) -> Option<String> {
    match tok {
        TtlTok::Uri(u) => Some(u.clone()),
        TtlTok::Word(w) => {
            if w == "a" {
                return Some(RDF_TYPE_URI.to_string());
            }
            if let Some((pfx, local)) = w.split_once(':') {
                if let Some(base) = prefixes.get(pfx) {
                    return Some(format!("{base}{local}"));
                }
            }
            Some(w.clone())
        }
        _ => None,
    }
}

fn default_ttl_prefixes() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert("lv2".to_string(), LV2_CORE_PREFIX.to_string());
    m.insert("pset".to_string(), LV2_PSET_PREFIX.to_string());
    m.insert("rdfs".to_string(), RDFS_PREFIX.to_string());
    m.insert("rdf".to_string(), "http://www.w3.org/1999/02/22-rdf-syntax-ns#".to_string());
    m
}

struct ParsedPresetStmt {
    subject: String,
    is_preset: bool,
    applies_to: Vec<String>,
    see_also: Vec<String>,
    label: Option<String>,
    values: Vec<(String, f32)>,
}

fn parse_turtle_presets(src: &str) -> Vec<ParsedPresetStmt> {
    let toks = tokenize_turtle(src);
    let mut prefixes = default_ttl_prefixes();
    let mut out = Vec::new();
    let mut i = 0;
    let preset_type = format!("{LV2_PSET_PREFIX}Preset");
    let applies_to_pred = format!("{LV2_CORE_PREFIX}appliesTo");
    let port_pred = format!("{LV2_CORE_PREFIX}port");
    let symbol_pred = format!("{LV2_CORE_PREFIX}symbol");
    let value_pred = format!("{LV2_PSET_PREFIX}value");
    let label_pred = format!("{RDFS_PREFIX}label");
    let see_also_pred = format!("{RDFS_PREFIX}seeAlso");

    while i < toks.len() {
        // Handle @prefix / @base directives.
        if let TtlTok::Word(w) = &toks[i] {
            if w == "@prefix" || w.eq_ignore_ascii_case("PREFIX") {
                if i + 2 < toks.len() {
                    if let (TtlTok::Word(pfx), TtlTok::Uri(ns)) = (&toks[i + 1], &toks[i + 2]) {
                        let key = pfx.trim_end_matches(':').to_string();
                        prefixes.insert(key, ns.clone());
                    }
                }
                while i < toks.len() && toks[i] != TtlTok::Dot {
                    i += 1;
                }
                if i < toks.len() {
                    i += 1;
                }
                continue;
            }
            if w == "@base" || w.eq_ignore_ascii_case("BASE") {
                while i < toks.len() && toks[i] != TtlTok::Dot {
                    i += 1;
                }
                if i < toks.len() {
                    i += 1;
                }
                continue;
            }
        }

        // Collect one top-level statement up to Dot at depth 0.
        let stmt_start = i;
        let mut depth = 0usize;
        while i < toks.len() {
            match toks[i] {
                TtlTok::LBracket | TtlTok::LParen => depth += 1,
                TtlTok::RBracket | TtlTok::RParen => depth = depth.saturating_sub(1),
                TtlTok::Dot if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        let stmt = &toks[stmt_start..i];
        if i < toks.len() && toks[i] == TtlTok::Dot {
            i += 1;
        }
        if stmt.len() < 3 {
            continue;
        }

        let Some(subject) = resolve_term(&stmt[0], &prefixes) else { continue };
        let mut parsed = ParsedPresetStmt {
            subject,
            is_preset: false,
            applies_to: Vec::new(),
            see_also: Vec::new(),
            label: None,
            values: Vec::new(),
        };

        // Parse predicate-object pairs separated by Semi at depth 0.
        let mut j = 1;
        while j < stmt.len() {
            if stmt[j] == TtlTok::Semi {
                j += 1;
                continue;
            }
            let Some(pred) = resolve_term(&stmt[j], &prefixes) else {
                break;
            };
            j += 1;
            // Parse comma-separated objects until Semi or end of stmt.
            while j < stmt.len() && stmt[j] != TtlTok::Semi {
                if stmt[j] == TtlTok::Comma {
                    j += 1;
                    continue;
                }
                if stmt[j] == TtlTok::LBracket {
                    let b_start = j + 1;
                    let mut b_depth = 1usize;
                    j += 1;
                    while j < stmt.len() && b_depth > 0 {
                        match stmt[j] {
                            TtlTok::LBracket => b_depth += 1,
                            TtlTok::RBracket => b_depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    let b_end = j.saturating_sub(1);
                    if pred == port_pred && b_end > b_start {
                        let inner = &stmt[b_start..b_end];
                        let (mut sym, mut val) = (None, None);
                        let mut k = 0;
                        while k + 1 < inner.len() {
                            if inner[k] == TtlTok::Semi || inner[k] == TtlTok::Comma {
                                k += 1;
                                continue;
                            }
                            if let Some(ip) = resolve_term(&inner[k], &prefixes) {
                                if ip == symbol_pred {
                                    if let TtlTok::Str(s) = &inner[k + 1] {
                                        sym = Some(s.clone());
                                    }
                                } else if ip == value_pred {
                                    if let TtlTok::Word(w) = &inner[k + 1] {
                                        if let Ok(v) = w.parse::<f32>() {
                                            val = Some(v);
                                        }
                                    }
                                }
                            }
                            k += 2;
                        }
                        if let (Some(s), Some(v)) = (sym, val) {
                            parsed.values.push((s, v));
                        }
                    }
                } else if stmt[j] == TtlTok::LParen {
                    let mut p_depth = 1usize;
                    j += 1;
                    while j < stmt.len() && p_depth > 0 {
                        match stmt[j] {
                            TtlTok::LParen => p_depth += 1,
                            TtlTok::RParen => p_depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                } else {
                    if pred == RDF_TYPE_URI {
                        if resolve_term(&stmt[j], &prefixes).as_deref() == Some(preset_type.as_str()) {
                            parsed.is_preset = true;
                        }
                    } else if pred == applies_to_pred {
                        if let Some(u) = resolve_term(&stmt[j], &prefixes) {
                            parsed.applies_to.push(u);
                        }
                    } else if pred == see_also_pred {
                        if let Some(u) = resolve_term(&stmt[j], &prefixes) {
                            parsed.see_also.push(u);
                        }
                    } else if pred == label_pred {
                        if let TtlTok::Str(s) = &stmt[j] {
                            parsed.label = Some(s.clone());
                        }
                    }
                    j += 1;
                }
            }
        }

        if parsed.is_preset {
            out.push(parsed);
        }
    }
    out
}

fn lv2_search_paths() -> Vec<std::path::PathBuf> {
    let mut raw = Vec::new();
    if let Ok(lv2_path) = std::env::var("LV2_PATH") {
        for p in lv2_path.split(':') {
            if !p.is_empty() {
                raw.push(std::path::PathBuf::from(p));
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        raw.push(std::path::PathBuf::from(home).join(".lv2"));
    }
    raw.push(std::path::PathBuf::from("/usr/lib/lv2"));
    raw.push(std::path::PathBuf::from("/usr/local/lib/lv2"));

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in raw {
        let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

fn resolve_preset_uri(subject: &str, file_path: &std::path::Path) -> String {
    if subject.is_empty() || !subject.contains(':') {
        let canon = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
        if subject.is_empty() {
            format!("file://{}", canon.display())
        } else {
            let dir = canon.parent().unwrap_or(std::path::Path::new("/"));
            format!("file://{}", dir.join(subject).display())
        }
    } else {
        subject.to_string()
    }
}

/// Discover installed LV2 presets (`pset:Preset`) that apply to `plugin_uri`.
pub fn discover_presets(plugin_uri: &str) -> Vec<Lv2Preset> {
    let mut presets = Vec::new();
    let mut seen_uris = std::collections::HashSet::new();

    for root in lv2_search_paths() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let bundle = entry.path();
            if !bundle.is_dir() {
                continue;
            }
            let manifest_path = bundle.join("manifest.ttl");
            let Ok(manifest_src) = std::fs::read_to_string(&manifest_path) else { continue };
            for m_stmt in parse_turtle_presets(&manifest_src) {
                if !m_stmt.applies_to.is_empty() && !m_stmt.applies_to.iter().any(|u| u == plugin_uri) {
                    continue;
                }
                if m_stmt.see_also.is_empty() {
                    if m_stmt.applies_to.iter().any(|u| u == plugin_uri) && !m_stmt.values.is_empty() {
                        let uri = resolve_preset_uri(&m_stmt.subject, &manifest_path);
                        if seen_uris.insert(uri.clone()) {
                            let name = m_stmt.label.unwrap_or_else(|| uri.clone());
                            presets.push(Lv2Preset { name, uri, values: m_stmt.values });
                        }
                    }
                    continue;
                }
                for rel in m_stmt.see_also {
                    let target = bundle.join(&rel);
                    let Ok(preset_src) = std::fs::read_to_string(&target) else { continue };
                    for p_stmt in parse_turtle_presets(&preset_src) {
                        let applies = p_stmt.applies_to.iter().any(|u| u == plugin_uri)
                            || (p_stmt.applies_to.is_empty() && m_stmt.applies_to.iter().any(|u| u == plugin_uri));
                        if !applies {
                            continue;
                        }
                        let subj = if p_stmt.subject.is_empty() { &m_stmt.subject } else { &p_stmt.subject };
                        let uri = resolve_preset_uri(subj, &target);
                        if !seen_uris.insert(uri.clone()) {
                            continue;
                        }
                        let name = p_stmt.label
                            .or_else(|| m_stmt.label.clone())
                            .unwrap_or_else(|| {
                                target.file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or(&uri)
                                    .replace('_', " ")
                            });
                        presets.push(Lv2Preset {
                            name,
                            uri,
                            values: p_stmt.values,
                        });
                    }
                }
            }
        }
    }

    presets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.uri.cmp(&b.uri)));
    presets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lilv lists a plugin from its Turtle files alone, so one whose binary
    /// was built for another CPU showed up and then failed to instantiate
    /// (#135). It is left out, with the reason.
    #[test]
    fn a_plugin_built_for_another_cpu_is_not_listed_and_says_why() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-lv2-foreign-{}", std::process::id()));
        let bundle = dir.join("foreign.lv2");
        std::fs::create_dir_all(&bundle).unwrap();
        let ttl = |binary: &str, uri: &str| {
            format!(
                "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix doap: <http://usefulinc.com/ns/doap#> .\n\
                 <{uri}> a lv2:Plugin ; doap:name \"Probe\" ; lv2:binary <{binary}> .\n"
            )
        };
        let (foreign, native) = ("urn:kabelsalat:test:foreign", "urn:kabelsalat:test:native");
        std::fs::write(bundle.join("manifest.ttl"), ttl("foreign.so", foreign) + &ttl("native.so", native)).unwrap();
        let (machine, name) = crate::binarch::other_machine();
        std::fs::write(bundle.join("foreign.so"), crate::binarch::fake_elf(machine)).unwrap();
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bundle.join("native.so")).unwrap();

        let _lilv = lilv_guard();
        let (listed, unloadable) = unsafe {
            let world = lilv_world_new();
            let uri = std::ffi::CString::new(format!("file://{}/", bundle.display())).unwrap();
            let node = lilv_new_uri(world, uri.as_ptr());
            lilv_world_load_bundle(world, node);
            lilv_node_free(node);
            let found = collect_plugins(world);
            lilv_world_free(world);
            found
        };
        let _ = std::fs::remove_dir_all(&dir);
        let uris: Vec<&str> = listed.iter().map(|p| p.uri.as_str()).collect();
        assert_eq!(uris, vec![native], "only the plugin that can run here is listed");
        assert_eq!(
            unloadable,
            vec![format!("foreign.so is built for {name}; this machine is {}, so it cannot be loaded", std::env::consts::ARCH)]
        );
    }

    #[test]
    fn atom_midi_events_append_within_a_block() {
        let mut buf = vec![0u8; 256];
        let (seq, midi) = (42u32, 43u32);
        init_atom_sequence(&mut buf, seq);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 8); // empty
        assert!(append_midi_event(&mut buf, midi, 0, &[0x90, 60, 100]));
        // atom.size = 8 + pad(16+3=19 → 24) = 32
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), seq);
        // event at offset 16: time.frames = 0, body.size = 3, body.type = MidiEvent
        assert_eq!(i64::from_le_bytes(buf[16..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), midi);
        assert_eq!(&buf[32..35], &[0x90, 60, 100]);
        // A second note of the chord follows at offset 40 — not a reset.
        assert!(append_midi_event(&mut buf, midi, 0, &[0x90, 64, 100]));
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 56);
        assert_eq!(&buf[32..35], &[0x90, 60, 100], "first note kept");
        assert_eq!(&buf[56..59], &[0x90, 64, 100]);
        // Full buffer: events are dropped, never written past the end.
        let mut small = vec![0u8; 48];
        init_atom_sequence(&mut small, seq);
        assert!(append_midi_event(&mut small, midi, 0, &[0x80, 60, 0]));
        assert!(!append_midi_event(&mut small, midi, 0, &[0x80, 61, 0]));
        assert_eq!(u32::from_le_bytes(small[0..4].try_into().unwrap()), 32);
    }

    /// #38: an atom output starts every run as an empty Chunk of full capacity.
    /// #26: the declared class decides; ports only without one.
    #[test]
    fn a_param_designation_marks_a_main_control_and_any_other_one_belongs_to_the_host() {
        let meta = |name: &str| ControlMeta { port_index: 9, symbol: "sym".into(), name: name.into(), min: 0.0, max: 2.0, default: 1.0 };
        let facts = |d: &[&str]| PortFacts { designations: d.iter().map(|s| s.to_string()).collect(), ..Default::default() };

        let gain = control_candidate(3, &meta("Gain"), &facts(&["http://lv2plug.in/ns/ext/parameters#gain"]));
        assert!(gain.designated && gain.usable);
        assert_eq!((gain.id, gain.max), (3, 2.0), "the id is the control ordinal, not the port index");

        let freewheel = control_candidate(0, &meta("Freewheel"), &facts(&["http://lv2plug.in/ns/lv2core#freeWheeling"]));
        assert!(!freewheel.designated && !freewheel.usable);

        let plain = control_candidate(1, &meta(""), &facts(&[]));
        assert!(plain.usable && !plain.designated);
        assert_eq!(plain.name, "sym", "an unnamed port goes by its symbol");

        let hidden = control_candidate(2, &meta("Internal"), &PortFacts { hidden: true, ..Default::default() });
        assert!(!hidden.usable);
    }

    #[test]
    fn instrument_class_comes_from_the_declared_types() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lv2 = |c: &str| format!("http://lv2plug.in/ns/lv2core#{c}");
        let doap = "http://usefulinc.com/ns/doap#Project";
        assert_eq!(instrument_from_types(&t(&[doap, &lv2("Plugin"), &lv2("InstrumentPlugin")])), Some(true));
        assert_eq!(instrument_from_types(&t(&[doap, &lv2("Plugin"), &lv2("DelayPlugin")])), Some(false), "LSP delay takes MIDI but is an effect");
        assert_eq!(instrument_from_types(&t(&[&lv2("UtilityPlugin")])), Some(false));
        assert_eq!(instrument_from_types(&t(&[doap, &lv2("Plugin")])), None, "generic only → ports decide");
        assert_eq!(instrument_from_types(&[]), None);
    }

    #[test]
    fn atom_output_header_announces_its_capacity() {
        let mut buf = vec![0u64; ATOM_OUT_BYTES / 8];
        reset_atom_output(&mut buf, 7);
        let bytes: Vec<u8> = buf[0].to_le_bytes().to_vec();
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize, ATOM_OUT_BYTES - 8);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 7);
        assert_eq!(buf.as_ptr() as usize % 8, 0, "8-byte aligned");
    }

    /// Rule 63: lines of the document setBfree 0.8 hands out, not a retyped shape.
    #[test]
    fn midnam_programs_are_read_from_setbfrees_document() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MIDINameDocument>
  <Author>setBfree -- Robin Gareus</Author>
  <MasterDeviceNames>
    <Model>setBfree:0xffffb414d</Model>
    <ChannelNameSet Name="Upper Manual">
      <AvailableForChannels>
        <AvailableChannel Channel="1" Available="true"/>
      </AvailableForChannels>
      <PatchBank Name="Organ Presets">
      <Patch Number="000" Name="Jazz 1 all" ProgramChange="0"/>
      <Patch Number="022" Name="8' solo" ProgramChange="22"/>
      <Patch Number="064" Name="L:Flute &amp; String" ProgramChange="64"/>
      <Patch Number="085" Name="U:Flutes 8'&amp;4'" ProgramChange="85"/>
      </PatchBank>
    </ChannelNameSet>
  </MasterDeviceNames>
</MIDINameDocument>"#;
        assert_eq!(parse_midnam_programs(xml), vec![
            (0, "Jazz 1 all".to_string()), (22, "8' solo".to_string()),
            (64, "L:Flute & String".to_string()), (85, "U:Flutes 8'&4'".to_string()),
        ]);
        assert!(parse_midnam_programs("<MIDINameDocument/>").is_empty());
        assert!(parse_midnam_programs(r#"<Patch Number="1" Name="Out of range" ProgramChange="128"/>"#).is_empty());
    }

    #[test]
    fn turtle_preset_parser_extracts_ports_and_label() {
        let ttl = r#"
@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix pset: <http://lv2plug.in/ns/ext/presets#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix state: <http://lv2plug.in/ns/ext/state#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

<>
    a pset:Preset ;
    lv2:appliesTo <urn:distrho:vitalium> ;
    rdfs:label "Test Brass" ;
    lv2:port [
        lv2:symbol "band_lower_ratio" ;
        pset:value 0.9
    ] , [
        lv2:symbol "bypass" ;
        pset:value 0.0
    ] ;
    state:state [
        <urn:juce:stateBinary> "eyJmb28iOiAiYmFyIn0="^^xsd:base64Binary
    ] .
"#;
        let parsed = parse_turtle_presets(ttl);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label.as_deref(), Some("Test Brass"));
        assert_eq!(parsed[0].applies_to, vec!["urn:distrho:vitalium"]);
        assert_eq!(parsed[0].values.len(), 2);
        assert_eq!(parsed[0].values[0].0, "band_lower_ratio");
        assert!((parsed[0].values[0].1 - 0.9).abs() < 1e-6);
        assert_eq!(parsed[0].values[1].0, "bypass");
        assert!((parsed[0].values[1].1 - 0.0).abs() < 1e-6);
    }

    #[test]
    fn discover_presets_vitalium() {
        if !std::path::Path::new("/usr/local/lib/lv2/Vitalium-unfa.lv2").exists() {
            return;
        }
        let presets = discover_presets("urn:distrho:vitalium");
        assert!(!presets.is_empty(), "expected Vitalium-unfa presets");
        let brass = presets.iter().find(|p| p.name == "Analog Brass").expect("Analog Brass preset missing");
        assert!(!brass.uri.is_empty());
        assert!(!brass.values.is_empty());
    }

    #[test]
    fn vitalium_survives_repeated_load_and_free() {
        let _one_at_a_time = VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Freeing the last instance used to dlclose vitalium-lv2.so, whose
        // static destructors crash; the second load then hit freed state.
        for _ in 0..5 {
            let Ok(p) = Lv2Plugin::load("urn:distrho:vitalium", 48000.0, 256) else { return };
            retire(p);
        }
        // Parked and reused, never freed (#14): five loads, one instance.
        assert!(parked_count("vitalium") >= 1);
    }

    #[test]
    fn vitalium_apply_preset_and_render_audio() {
        let _one_at_a_time = VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut plugin) = Lv2Plugin::load("urn:distrho:vitalium", 48000.0, 256) else {
            return;
        };
        let descs = plugin.control_descs();
        assert!(!descs.is_empty(), "expected Vitalium control descs");
        assert!(descs.iter().all(|d| !d.symbol.is_empty()));

        let presets = discover_presets("urn:distrho:vitalium");
        if let Some(preset) = presets.iter().find(|p| p.name == "Analog Brass").or_else(|| presets.first()) {
            plugin.apply_preset(preset);
            let after = plugin.control_descs();
            if let Some((sym, val)) = preset.values.first() {
                if let Some(d) = after.iter().find(|d| d.symbol == *sym) {
                    assert!((d.value - *val).abs() < 1e-5, "control {sym} not updated by apply_preset");
                }
            }
        }

        plugin.note_on(60, 100);
        let input = vec![0.0f32; 256 * 2];
        let mut output = vec![0.0f32; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..20 {
            plugin.process(&input, &mut output, 256);
            peak = peak.max(output.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        plugin.note_off(60);
        assert!(peak > 1e-4, "expected non-zero audio from Vitalium, got peak={peak}");
        retire(plugin);
    }

    /// Rising zero crossings per second of the left channel.
    fn pitch_hz(plugin: &mut Lv2Plugin, note: u8) -> f32 {
        let input = vec![0.0f32; 256 * 2];
        let mut output = vec![0.0f32; 256 * 2];
        plugin.note_on(note, 100);
        let mut left: Vec<f32> = Vec::new();
        for _ in 0..80 {
            plugin.process(&input, &mut output, 256);
            left.extend(output.iter().step_by(2));
        }
        plugin.note_off(note);
        for _ in 0..40 { plugin.process(&input, &mut output, 256); }
        let tail = &left[left.len() / 2..];
        let rising = tail.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        rising as f32 * 48000.0 / tail.len() as f32
    }

    #[test]
    fn a_restored_vital_patch_is_what_vitalium_plays() {
        let _one_at_a_time = VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut plugin) = Lv2Plugin::load("urn:distrho:vitalium", 48000.0, 256) else { return };
        let init = pitch_hz(&mut plugin, 57);
        assert!((init - 220.0).abs() < 15.0, "init patch plays A3 at 220 Hz, measured {init}");
        let saved = plugin.saved_state("urn:juce:stateBinary").expect("save");
        let text = String::from_utf8_lossy(&saved);
        let mut patch: serde_json::Value = serde_json::from_str(text.trim_end_matches('\0')).expect("Vitalium saves JSON");
        patch["settings"]["osc_1_transpose"] = serde_json::json!(12.0);
        let patch = patch.to_string();
        plugin.restore_state(&[("urn:juce:stateBinary", "http://lv2plug.in/ns/ext/atom#Chunk", patch.as_bytes())]).expect("restore");
        let up = pitch_hz(&mut plugin, 57);
        assert!((up - 440.0).abs() < 30.0, "the patch's +12 transpose plays A4 at 440 Hz, measured {up}");
        retire(plugin);
    }

    #[test]
    fn reused_vitalium_starts_fresh_and_still_plays() {
        let _one_at_a_time = VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut first) = Lv2Plugin::load("urn:distrho:vitalium", 48000.0, 256) else { return };
        let d = first.control_descs().into_iter().find(|d| d.max > d.min).expect("a control");
        first.set_control_normalized(d.index, if d.default == d.max { 0.0 } else { 1.0 });
        retire(first);
        let before = parked_count("vitalium");
        let mut again = Lv2Plugin::load("urn:distrho:vitalium", 48000.0, 256).expect("reload");
        assert_eq!(parked_count("vitalium"), before - 1, "the parked instance was reused, not a new one built");
        let now = again.control_descs().into_iter().find(|c| c.index == d.index).unwrap();
        assert!((now.value - d.default).abs() < 1e-6, "controls back at their defaults: {} vs {}", now.value, d.default);
        again.note_on(60, 100);
        let (input, mut output, mut peak) = (vec![0.0f32; 512], vec![0.0f32; 512], 0.0f32);
        for _ in 0..20 {
            again.process(&input, &mut output, 256);
            peak = peak.max(output.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        assert!(peak > 1e-4, "a reused instance still plays: peak={peak}");
        retire(again);
    }

    #[test]
    fn a_stereo_effect_hears_left_and_right_apart() {
        // Carla's stereo Audio Gain passes each channel through on its own:
        // a tone on the left only must leave the right output silent. Every
        // input read the left channel before. Skips where Carla is missing.
        let Ok(mut gain) = Lv2Plugin::load("http://kxstudio.sf.net/carla/plugins/audiogain_s", 48000.0, 256) else { return };
        let block = 256usize;
        let mut inp = vec![0.0f32; block * 2];
        let mut out = vec![0.0f32; block * 2];
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for b in 0..20 {
            for i in 0..block {
                inp[i * 2] = (2.0 * std::f32::consts::PI * 440.0 * ((b * block + i) as f32 / 48000.0)).sin() * 0.5;
            }
            gain.process(&inp, &mut out, block);
            left = out.iter().step_by(2).fold(left, |m, s| m.max(s.abs()));
            right = out.iter().skip(1).step_by(2).fold(right, |m, s| m.max(s.abs()));
        }
        retire(gain);
        assert!(left > 0.1, "the left tone comes through: peak {left}");
        assert!(right < 1e-4, "the right output stays silent: peak {right}");
    }

    #[test]
    fn guitarix_amps_process_from_the_first_block() {
        // Guitarix's amps declare an `lv2:enabled` port named BYPASS with
        // default 1 (bypassed). The host must enable such a plugin on
        // instantiation: a unit the player adds starts processing (the
        // Guitar rig would pass audio untouched otherwise). Probes the real
        // plugin when the Guitarix card is installed, skips in CI.
        let _one_at_a_time = VITALIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut amp) = Lv2Plugin::load(crate::guitar::AMP, 48000.0, 512) else { return };

        fn harmonics_over_fundamental(amp: &mut Lv2Plugin, bypass: Option<f32>) -> f32 {
            let block = 512usize;
            let mut inp = vec![0.0f32; block * 2];
            let mut out = vec![0.0f32; block * 2];
            let mut l = Vec::new();
            for b in 0..(48000 / block) {
                for i in 0..block {
                    let s = (2.0 * std::f32::consts::PI * 220.0 * ((b * block + i) as f32 / 48000.0)).sin() * 0.45;
                    inp[i * 2] = s;
                    inp[i * 2 + 1] = s;
                }
                amp.process(&inp, &mut out, block);
                if b > 20 {
                    l.extend(out.iter().step_by(2).copied());
                }
            }
            let g = |x: &[f32], f: f32| {
                let k = 2.0 * std::f32::consts::PI * f / 48000.0;
                let (mut s1, mut s2) = (0.0f32, 0.0f32);
                for &v in x {
                    let s0 = v + 2.0 * k.cos() * s1 - s2;
                    s2 = s1;
                    s1 = s0;
                }
                (s1 * s1 + s2 * s2 - 2.0 * k.cos() * s1 * s2) / (x.len().max(1) as f32).powi(2)
            };
            let f0 = g(&l, 220.0);
            if f0 <= 1e-12 {
                return 0.0;
            }
            (g(&l, 440.0) + g(&l, 660.0) + g(&l, 880.0)) / f0
        }

        let by_desc = amp.control_descs().into_iter().find(|d| d.symbol == "BYPASS");
        let Some(desc) = by_desc else {
            // No enabled port: the amp crunches as it declares.
            let plain = harmonics_over_fundamental(&mut amp, None);
            assert!(plain > 0.02, "the amp transforms its input (harmonics {plain:.4})");
            retire(amp);
            return;
        };
        // The amp as the bundle ships it: enabled (per the LV2 `enabled`
        // designation, 1 = processing) but nearly clean at its defaults.
        let as_shipped = harmonics_over_fundamental(&mut amp, None);
        println!("guitarix amp as the bundle ships it (BYPASS={}): harmonics {as_shipped:.4}", desc.value);
        assert!(as_shipped > 0.0, "the enabled port must not mute the amp: {as_shipped:.4}");
        // The Guitar rig drives its amp: "Drive" (the tube stage's gain)
        // is the one control that turns the modelled clean channel into a
        // crunch — the template sets it to its maximum.
        let ctl = |p: &Lv2Plugin, sym: &str| p.control_descs().into_iter().find(|d| d.symbol == sym).expect(sym);
        let (drive, pregain, distortion, highgain) = (
            ctl(&amp, "Drive"), ctl(&amp, "PreGain"), ctl(&amp, "Distortion"), ctl(&amp, "HIGHGAIN"),
        );
        assert_eq!(drive.index, crate::guitar::AMP_DRIVE_CTRL, "the template's Drive ordinal follows the plugin's port order");
        for (sym, idx, v) in [
            ("Drive", drive.index, 1.0),
            ("Distortion", distortion.index, 0.8),
            ("HIGHGAIN", highgain.index, 1.0),
            ("PreGain", pregain.index, 0.8),
        ] {
            amp.set_control_normalized(idx, v);
            println!("guitarix amp with {sym}={v}: harmonics {:.4}", harmonics_over_fundamental(&mut amp, None));
        }
        // The template's amp: everything at the plugin's declared defaults
        // (normalized: PreGain 0 dB = 0.5, Distortion 20 = ~0.19) except Drive.
        for (idx, v) in [
            (drive.index, 1.0),
            (pregain.index, 0.5),
            (distortion.index, (20.0f32 - 1.0) / 99.0),
            (highgain.index, 0.0),
        ] {
            amp.set_control_normalized(idx, v);
        }
        let driven = harmonics_over_fundamental(&mut amp, None);
        println!("guitarix amp at the template's setting (Drive max, rest default): harmonics {driven:.4}");
        assert!(
            driven > as_shipped * 2.0 + 0.02,
            "the rig's amp setting must transform the input: as-shipped {as_shipped:.4} vs driven {driven:.4}"
        );
        retire(amp);
    }

    // -- LV2 worker plumbing -------------------------------------------------

    #[derive(Default)]
    struct TestInstanceState {
        work_data: std::sync::Mutex<Vec<u8>>,
        response_data: std::sync::Mutex<Vec<u8>>,
        end_run_count: std::sync::atomic::AtomicU32,
    }

    extern "C" fn fake_work(
        instance: Lv2Handle,
        respond: Lv2WorkerRespondFn,
        handle: Lv2WorkerRespondHandle,
        size: u32,
        data: *const c_void,
    ) -> u32 {
        unsafe {
            let st = &*(instance as *const TestInstanceState);
            if size > 0 && !data.is_null() {
                *st.work_data.lock().unwrap() = std::slice::from_raw_parts(data as *const u8, size as usize).to_vec();
            }
            respond(handle, size, data);
        }
        WORKER_SUCCESS
    }

    extern "C" fn fake_work_response(instance: Lv2Handle, size: u32, body: *const c_void) -> u32 {
        unsafe {
            let st = &*(instance as *const TestInstanceState);
            if size > 0 && !body.is_null() {
                *st.response_data.lock().unwrap() = std::slice::from_raw_parts(body as *const u8, size as usize).to_vec();
            }
        }
        WORKER_SUCCESS
    }

    extern "C" fn fake_end_run(instance: Lv2Handle) -> u32 {
        unsafe {
            let st = &*(instance as *const TestInstanceState);
            st.end_run_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        WORKER_SUCCESS
    }

    #[test]
    fn worker_ring_roundtrips_schedule_work_and_respond() {
        let state = Box::new(TestInstanceState::default());
        let instance = &*state as *const TestInstanceState as Lv2Handle;

        let mut shared = Box::new(WorkerShared { req: None, thread: std::thread::current() });
        let shared_ptr = &mut *shared as *mut WorkerShared as *mut c_void;
        let iface = Lv2WorkerInterface { work: fake_work, work_response: fake_work_response, end_run: Some(fake_end_run) };

        let (req_tx, req_rx) = rtrb::RingBuffer::<WorkerMsg>::new(WORKER_RING_CAP);
        let (resp_tx, resp_rx) = rtrb::RingBuffer::<WorkerMsg>::new(WORKER_RING_CAP);
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown2 = std::sync::Arc::clone(&shutdown);
        let ctx = WorkerCtx { iface, instance };
        let join = std::thread::spawn(move || worker_loop(req_rx, resp_tx, ctx, shutdown2));
        shared.req = Some(req_tx);
        shared.thread = join.thread().clone();
        let mut worker = Lv2Worker { iface, instance, resp_rx, join: Some(join), shutdown };

        // Audio thread: schedule a request through the feature callback.
        let payload = [0x90u8, 60, 100, 0x42];
        assert_eq!(schedule_work_cb(shared_ptr, payload.len() as u32, payload.as_ptr() as *const c_void), WORKER_SUCCESS);

        // Wait for the worker to run work() and deliver its response.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while worker.resp_rx.slots() == 0 {
            assert!(std::time::Instant::now() < deadline, "worker never responded");
            std::thread::yield_now();
        }
        assert_eq!(state.work_data.lock().unwrap().as_slice(), &payload, "work() got the scheduled data");

        // Audio thread: drain the response -> work_response() sees the data.
        worker.drain_responses();
        assert_eq!(state.response_data.lock().unwrap().as_slice(), &payload, "work_response() got the response");

        // end_run() must be callable after every run cycle.
        worker.end_run();
        assert_eq!(state.end_run_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Clean shutdown: flag + wake + join.
        worker.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.thread.unpark();
        let _ = worker.join.take().unwrap().join();
    }

    #[test]
    fn setbfree_plays_a_note() {
        let uri = "http://gareus.org/oss/lv2/b_synth";
        // Skip cleanly when the organ is not installed (not a failure).
        if !discover().iter().any(|p| p.uri == uri) {
            return;
        }
        let mut plugin = Lv2Plugin::load(uri, 48000.0, 256).expect("setBfree is installed but failed to load");
        assert!(plugin.is_instrument(), "setBfree must classify as an instrument");
        plugin.note_on(60, 100);
        let input = vec![0.0f32; 256 * 2];
        let mut output = vec![0.0f32; 256 * 2];
        let mut peak = 0.0f32;
        for _ in 0..120 {
            plugin.process(&input, &mut output, 256);
            peak = peak.max(output.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        plugin.note_off(60);
        retire(plugin);
        eprintln!("setbfree note peak: {peak:.4}");
        assert!(peak > 1e-3, "setBfree produced no audio while a key was held: peak={peak}");
    }

    #[test]
    fn primary_lv2_class_picks_most_specific_core_class() {
        let mk = |names: &[&str]| -> Vec<String> {
            names.iter().map(|n| format!("http://lv2plug.in/ns/lv2core#{n}")).collect()
        };
        assert_eq!(primary_lv2_class(&mk(&["Plugin", "DelayPlugin", "SimulatorPlugin", "ReverbPlugin"])), "ReverbPlugin");
        assert_eq!(primary_lv2_class(&mk(&["Plugin", "DynamicsPlugin", "CompressorPlugin"])), "CompressorPlugin");
        assert_eq!(primary_lv2_class(&mk(&["Plugin", "FilterPlugin", "EQPlugin", "ParaEQPlugin"])), "ParaEQPlugin");
        assert_eq!(primary_lv2_class(&mk(&["Plugin", "UtilityPlugin", "AnalyserPlugin"])), "AnalyserPlugin");
        assert_eq!(primary_lv2_class(&mk(&["Plugin", "GeneratorPlugin", "InstrumentPlugin"])), "InstrumentPlugin");
        assert_eq!(primary_lv2_class(&mk(&["Plugin"])), "");
    }
}

/// Instrument-detection self-test: load LV2 instruments and confirm atom-port
/// classification (is_instrument) and MIDI write path.
pub fn instrument_test() -> anyhow::Result<()> {
    let candidates = ["multisampler_x12", "multisampler", "oscillator", "synth"];
    let mut found = 0;
    let mut sounding = 0;
    for c in candidates {
        match Lv2Plugin::load(c, 48000.0, 256) {
            Ok(mut p) => {
                let is_inst = p.is_instrument();
                // Probe MIDI→audio: send a note and measure output.
                let mut peak = 0.0f32;
                if is_inst {
                    p.note_on(60, 100);
                    let input = vec![0.0f32; 256 * 2];
                    let mut output = vec![0.0f32; 256 * 2];
                    for _ in 0..20 {
                        p.process(&input, &mut output, 256);
                        peak = peak.max(output.iter().fold(0.0f32, |m, s| m.max(s.abs())));
                    }
                    p.note_off(60);
                }
                println!(
                    "  {} — instrument={} controls={} atom={} note_peak={:.4}",
                    p.uri(),
                    is_inst,
                    p.control_count(),
                    p.atom_in.is_some(),
                    peak
                );
                if is_inst {
                    found += 1;
                    if peak > 1e-4 {
                        sounding += 1;
                    }
                }
            }
            Err(_) => continue,
        }
    }
    anyhow::ensure!(found > 0, "no LV2 instrument detected among candidates");
    println!("lv2-instrument-test: OK — {found} instrument(s) detected, {sounding} produced audio from a MIDI note");
    Ok(())
}
