//! LV2 plugin discovery via lilv — hand-written FFI (no bindgen/clang needed).
//!
//! First P3 milestone: enumerate the system's installed LV2 plugins. Full
//! hosting (instantiate → connect ports → run) is the follow-up; this proves
//! the lilv linkage and discovery path on which hosting builds.

use std::ffi::CStr;
use std::os::raw::{c_char, c_uint};

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
