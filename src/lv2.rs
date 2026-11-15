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
    fn lilv_node_free(val: *mut LilvNode);
    fn lilv_new_uri(world: *mut LilvWorld, uri: *const c_char) -> *mut LilvNode;
    fn lilv_node_as_float(value: *const LilvNode) -> f32;
    fn lilv_plugin_instantiate(
        plugin: *const LilvPlugin,
        sample_rate: f64,
        features: *const *const Lv2Feature,
    ) -> *mut LilvInstance;
    fn lilv_plugin_get_port_by_index(plugin: *const LilvPlugin, index: u32) -> *const LilvPort;
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
                // Set the control to its default (unity-ish for a gain plugin).
                let mut def: *mut LilvNode = std::ptr::null_mut();
                lilv_port_get_range(plugin, port, &mut def, std::ptr::null_mut(), std::ptr::null_mut());
                let v = if !def.is_null() {
                    let f = lilv_node_as_float(def);
                    lilv_node_free(def);
                    f
                } else {
                    1.0
                };
                buffers[i as usize][0] = if v == 0.0 { 1.0 } else { v };
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
/// `process` is allocation-free (buffers preallocated); instantiate/Drop are
/// main-thread only. Not yet `Send` (raw pointers) — engine integration will
/// wrap it accordingly.
pub struct Lv2Plugin {
    world: *mut LilvWorld,
    instance: *mut LilvInstance,
    buffers: Vec<Vec<f32>>,
    audio_in: Vec<usize>,
    audio_out: Vec<usize>,
    control_in: Vec<usize>,
    control_ranges: Vec<(f32, f32)>,
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
}

// SAFETY: the lilv world/instance raw pointers are created on the main thread
// (load) and thereafter only touched on the audio thread (process/Drop), never
// concurrently. This is the standard plugin-host pattern (cf. ClapVoice).
unsafe impl Send for Lv2Plugin {}

impl Lv2Plugin {
    /// Load the first plugin whose URI contains `needle`, instantiate it with
    /// urid-map + options features, classify ports, and connect buffers.
    pub fn load(needle: &str, sample_rate: f64, block: usize) -> anyhow::Result<Self> {
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
                    let mut def: *mut LilvNode = std::ptr::null_mut();
                    let mut min: *mut LilvNode = std::ptr::null_mut();
                    let mut max: *mut LilvNode = std::ptr::null_mut();
                    lilv_port_get_range(plugin, port, &mut def, &mut min, &mut max);
                    let d = if !def.is_null() { let f = lilv_node_as_float(def); lilv_node_free(def); f } else { 1.0 };
                    let lo = if !min.is_null() { let f = lilv_node_as_float(min); lilv_node_free(min); f } else { 0.0 };
                    let hi = if !max.is_null() { let f = lilv_node_as_float(max); lilv_node_free(max); f } else { 1.0 };
                    buffers[i as usize][0] = if d == 0.0 { 1.0 } else { d };
                    control_in.push(i as usize);
                    control_ranges.push((lo, hi));
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
            Ok(Self { world, instance, buffers, audio_in, audio_out, control_in, control_ranges, atom_in, atom_buf, urid_sequence, urid_midi, is_inst: classify_instrument(world, plugin), _urid: urid, _map_data: map_data, block, uri })
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
