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
    fn lilv_node_as_string(value: *const LilvNode) -> *const c_char;
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
                    });
                }
                iter = lilv_plugins_next(plugins, iter);
            }
        }
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
