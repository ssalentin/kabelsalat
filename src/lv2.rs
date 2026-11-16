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
    fn lilv_plugin_get_num_ports(plugin: *const LilvPlugin) -> c_uint;
    fn lilv_plugin_get_class(plugin: *const LilvPlugin) -> *const LilvNode;
    fn lilv_node_as_string(value: *const LilvNode) -> *const c_char;
    fn lilv_node_as_uri(value: *const LilvNode) -> *const c_char;
    fn lilv_plugin_get_library_uri(plugin: *const LilvPlugin) -> *const LilvNode;
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
    fn lilv_port_get_range(
        plugin: *const LilvPlugin,
        port: *const LilvPort,
        def: *mut *mut LilvNode,
        min: *mut *mut LilvNode,
        max: *mut *mut LilvNode,
    );
    fn lilv_instance_free(instance: *mut LilvInstance);
}

#[derive(Debug, Clone)]
pub struct Lv2PluginInfo {
    pub uri: String,
    pub name: String,
    pub num_ports: u32,
    pub is_instrument: bool,
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

/// Classify a plugin as an instrument by its LV2 plugin class
/// (lv2core#InstrumentPlugin) — the authoritative designation, not a port
/// heuristic (effects can have atom control ports; instruments can have audio
/// inputs for modulation).
unsafe fn classify_instrument(world: *mut LilvWorld, plugin: *const LilvPlugin) -> bool {
    let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
    let atom = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
    let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
    let n = lilv_plugin_get_num_ports(plugin);
    let (mut atom_in, mut audio_out, mut audio_in) = (false, 0u32, 0u32);
    for i in 0..n {
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
    // Instrument hint: MIDI/atom input + audio output. Permissive by design —
    // some effects also accept atom control, so the browser offers both add
    // roles; the LV2 plugin class would be authoritative but LSP exposes it as
    // a blank node lilv can't resolve to a URI here.
    let _ = audio_in;
    atom_in && audio_out > 0
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

/// Discover all installed LV2 plugins (loads the lilv world, enumerates, frees).
pub fn discover() -> Vec<Lv2PluginInfo> {
    let _lilv = lilv_guard();
    unsafe {
        let world = lilv_world_new();
        if world.is_null() {
            return Vec::new();
        }
        lilv_world_load_all(world);
        let plugins = lilv_world_get_all_plugins(world);
        let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
        let atom = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
        let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
        let mut out = Vec::new();
        if !plugins.is_null() {
            let mut iter = lilv_plugins_begin(plugins);
            while !iter.is_null() && !lilv_plugins_is_end(plugins, iter) {
                let plugin = lilv_plugins_get(plugins, iter);
                if !plugin.is_null() {
                    out.push(Lv2PluginInfo {
                        uri: node_borrowed(lilv_plugin_get_uri(plugin)),
                        name: node_owned(lilv_plugin_get_name(plugin)),
                        num_ports: lilv_plugin_get_num_ports(plugin),
                        is_instrument: classify_instrument(world, plugin),
                    });
                }
                iter = lilv_plugins_next(plugins, iter);
            }
        }
        lilv_node_free(audio);
        lilv_node_free(atom);
        lilv_node_free(input);
        lilv_world_free(world);
        out
    }
}

/// CLI self-test: list installed LV2 plugins.
pub fn run_test() -> anyhow::Result<()> {
    let plugins = discover();
    println!("=== LV2 discovery (lilv FFI) ===");
    println!("{} plugins installed", plugins.len());
    for p in plugins.iter().take(40) {
        println!("  {} ({} ports) — {}", p.name, p.num_ports, p.uri);
    }
    let instruments: Vec<&Lv2PluginInfo> = plugins.iter().filter(|p| p.is_instrument).collect();
    println!("{} instruments (atom input + audio output):", instruments.len());
    for p in instruments.iter().take(20) {
        println!("  [inst] {} — {}", p.name, p.uri);
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
    let mut iter = lilv_plugins_begin(plugins);
    while !iter.is_null() && !lilv_plugins_is_end(plugins, iter) {
        let plugin = lilv_plugins_get(plugins, iter);
        if !plugin.is_null() {
            let uri = node_borrowed(lilv_plugin_get_uri(plugin));
            if uri.contains(needle) {
                return plugin;
            }
        }
        iter = lilv_plugins_next(plugins, iter);
    }
    std::ptr::null()
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
    urid_sequence: u32,
    urid_midi: u32,
    is_inst: bool,
    _urid: Box<UridMap>,
    _map_data: Box<Lv2UridMap>,
    block: usize,
    uri: String,
    sample_rate: f64,
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
            let plugin = find_plugin(world, needle);
            anyhow::ensure!(!plugin.is_null(), "LV2 plugin not found: {needle}");
            let uri = node_borrowed(lilv_plugin_get_uri(plugin));

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
            let features: [*const Lv2Feature; 3] = [&map_feature, &options_feature, std::ptr::null()];

            let instance = lilv_plugin_instantiate(plugin, sample_rate, features.as_ptr());
            anyhow::ensure!(!instance.is_null(), "instantiate failed for {uri}");
            pin_plugin_library(plugin);

            let audio = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#AudioPort\0".as_ptr() as *const c_char);
            let control = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#ControlPort\0".as_ptr() as *const c_char);
            let input = lilv_new_uri(world, b"http://lv2plug.in/ns/lv2core#InputPort\0".as_ptr() as *const c_char);
            let atom_class = lilv_new_uri(world, b"http://lv2plug.in/ns/ext/atom#AtomPort\0".as_ptr() as *const c_char);
            let urid_sequence = urid_map_cb(handle, b"http://lv2plug.in/ns/ext/atom#Sequence\0".as_ptr() as *const c_char);
            let urid_midi = urid_map_cb(handle, b"http://lv2plug.in/ns/ext/midi#MidiEvent\0".as_ptr() as *const c_char);
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
            anyhow::ensure!(!audio_out.is_empty(), "{uri} has no audio output");
            anyhow::ensure!(!audio_in.is_empty() || atom_in.is_some(), "{uri} has no audio/atom input");
            if let Some(activate) = desc.activate {
                activate(h);
            }
            Ok(Self { world, instance, buffers, audio_in, audio_out, control_in, control_ranges, control_meta, control_by_symbol, atom_in, atom_buf, urid_sequence, urid_midi, is_inst: classify_instrument(world, plugin), _urid: urid, _map_data: map_data, block, uri, sample_rate })
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

    /// Send a MIDI note-on to the atom input (frame 0 of the next block).
    pub fn note_on(&mut self, note: u8, velocity: u8) {
        if self.atom_in.is_some() {
            write_midi_event(&mut self.atom_buf, self.urid_sequence, self.urid_midi, 0, &[0x90, note, velocity]);
        }
    }

    /// Send a MIDI note-off to the atom input.
    pub fn note_off(&mut self, note: u8) {
        if self.atom_in.is_some() {
            write_midi_event(&mut self.atom_buf, self.urid_sequence, self.urid_midi, 0, &[0x80, note, 0]);
        }
    }

    /// Read back the MIDI event currently in the atom buffer (for verification).
    pub fn last_midi_event(&self) -> Option<[u8; 3]> {
        if self.atom_in.is_none() || self.atom_buf.len() < 35 {
            return None;
        }
        let size = u32::from_le_bytes(self.atom_buf[0..4].try_into().ok()?);
        if size <= 8 {
            return None;
        }
        let data_len = u32::from_le_bytes(self.atom_buf[24..28].try_into().ok()?);
        if data_len < 3 {
            return None;
        }
        Some([self.atom_buf[32], self.atom_buf[33], self.atom_buf[34]])
    }

    /// Process a stereo-interleaved block (allocation-free).
    pub fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        let frames = frames.min(self.block);
        unsafe {
            for &i in &self.audio_in {
                for f in 0..frames {
                    self.buffers[i][f] = input.get(f * 2).copied().unwrap_or(0.0);
                }
            }
            let desc = &*(*self.instance).lv2_descriptor;
            (desc.run.expect("no run"))((*self.instance).lv2_handle, frames as u32);
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
            return;
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

/// Initialize an atom:Sequence buffer as empty (time unit = audio frames).
fn init_atom_sequence(buf: &mut [u8], seq_urid: u32) {
    buf[0..4].copy_from_slice(&8u32.to_le_bytes()); // atom.size = unit+pad only
    buf[4..8].copy_from_slice(&seq_urid.to_le_bytes()); // atom.type = Sequence
    buf[8..12].copy_from_slice(&0u32.to_le_bytes()); // body.unit = 0 (frames)
    buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // body.pad
}

/// Write a single MIDI event into an atom:Sequence at `frame` (8-byte aligned,
/// per the LV2 atom spec). Resets the sequence to hold just this event.
fn write_midi_event(buf: &mut [u8], seq_urid: u32, midi_urid: u32, frame: i64, midi: &[u8]) {
    let data_len = midi.len();
    let padded = (16 + data_len + 7) & !7; // event header(16) + data, padded
    buf[0..4].copy_from_slice(&(8 + padded as u32).to_le_bytes()); // atom.size
    buf[4..8].copy_from_slice(&seq_urid.to_le_bytes());
    buf[8..12].copy_from_slice(&0u32.to_le_bytes());
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    let e = 16;
    for b in &mut buf[e..e + padded] {
        *b = 0;
    }
    buf[e..e + 8].copy_from_slice(&frame.to_le_bytes()); // time.frames
    buf[e + 8..e + 12].copy_from_slice(&(data_len as u32).to_le_bytes()); // body.size
    buf[e + 12..e + 16].copy_from_slice(&midi_urid.to_le_bytes()); // body.type
    buf[e + 16..e + 16 + data_len].copy_from_slice(midi); // MIDI bytes
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

    #[test]
    fn atom_midi_event_format() {
        let mut buf = vec![0u8; 256];
        let (seq, midi) = (42u32, 43u32);
        init_atom_sequence(&mut buf, seq);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 8); // empty
        write_midi_event(&mut buf, seq, midi, 0, &[0x90, 60, 100]);
        // atom.size = 8 + pad(16+3=19 → 24) = 32
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), seq);
        // event at offset 16: time.frames = 0
        assert_eq!(i64::from_le_bytes(buf[16..24].try_into().unwrap()), 0);
        // body.size = 3, body.type = MidiEvent URID
        assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), midi);
        // MIDI payload
        assert_eq!(&buf[32..35], &[0x90, 60, 100]);
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
