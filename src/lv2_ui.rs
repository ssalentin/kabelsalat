//! LV2 native GUI hosting (`ui:X11UI` embedded in `PluginWindow`, plus
//! `ui:showInterface` floating fallback and `ui:idleInterface` pump loop).

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use super::{
    discover, discover_ui_for_uri, port_symbols_for_uri, control_descs_for_uri,
    Lv2ControlDesc, Lv2Feature, Lv2Plugin, Lv2UiInfo, Lv2UridMap,
};
use crate::plugin_window::PluginWindow;

const LV2_UI_PARENT_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#parent\0";
const LV2_UI_RESIZE_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#resize\0";
const LV2_UI_PORT_MAP_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#portMap\0";
const LV2_UI_PORT_SUBSCRIBE_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#portSubscribe\0";
const LV2_UI_TOUCH_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#touch\0";
const LV2_UI_IDLE_INTERFACE_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#idleInterface\0";
const LV2_UI_SHOW_INTERFACE_URI: &[u8] = b"http://lv2plug.in/ns/extensions/ui#showInterface\0";
const LV2_URID_MAP_URI: &[u8] = b"http://lv2plug.in/ns/ext/urid#map\0";
const LV2_URID_UNMAP_URI: &[u8] = b"http://lv2plug.in/ns/ext/urid#unmap\0";
const LV2_OPTIONS_URI: &[u8] = b"http://lv2plug.in/ns/ext/options#options\0";

const LV2_UI_FLOAT_PROTOCOL: &str = "http://lv2plug.in/ns/extensions/ui#floatProtocol";
const LV2_ATOM_EVENT_TRANSFER: &str = "http://lv2plug.in/ns/ext/atom#eventTransfer";
const LV2_MIDI_EVENT: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";

const LV2UI_INVALID_PORT_INDEX: u32 = u32::MAX;

type Lv2UiWidget = *mut c_void;
type Lv2UiHandlePtr = *mut c_void;
type Lv2UiController = *mut c_void;
type Lv2UiFeatureHandle = *mut c_void;

type Lv2UiWriteFn = extern "C" fn(
    controller: Lv2UiController,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
);

#[repr(C)]
struct Lv2UiDescriptor {
    uri: *const c_char,
    instantiate: Option<
        extern "C" fn(
            descriptor: *const Lv2UiDescriptor,
            plugin_uri: *const c_char,
            bundle_path: *const c_char,
            write_function: Lv2UiWriteFn,
            controller: Lv2UiController,
            widget: *mut Lv2UiWidget,
            features: *const *const Lv2Feature,
        ) -> Lv2UiHandlePtr,
    >,
    cleanup: Option<extern "C" fn(ui: Lv2UiHandlePtr)>,
    port_event: Option<
        extern "C" fn(
            ui: Lv2UiHandlePtr,
            port_index: u32,
            buffer_size: u32,
            format: u32,
            buffer: *const c_void,
        ),
    >,
    extension_data: Option<extern "C" fn(uri: *const c_char) -> *const c_void>,
}

#[repr(C)]
struct Lv2UiResize {
    handle: Lv2UiFeatureHandle,
    ui_resize: Option<extern "C" fn(handle: Lv2UiFeatureHandle, width: c_int, height: c_int) -> c_int>,
}

#[repr(C)]
struct Lv2UiPortMap {
    handle: Lv2UiFeatureHandle,
    port_index: Option<extern "C" fn(handle: Lv2UiFeatureHandle, symbol: *const c_char) -> u32>,
}

#[repr(C)]
struct Lv2UiPortSubscribe {
    handle: Lv2UiFeatureHandle,
    subscribe: Option<
        extern "C" fn(
            handle: Lv2UiFeatureHandle,
            port_index: u32,
            port_protocol: u32,
            features: *const *const Lv2Feature,
        ) -> u32,
    >,
    unsubscribe: Option<
        extern "C" fn(
            handle: Lv2UiFeatureHandle,
            port_index: u32,
            port_protocol: u32,
            features: *const *const Lv2Feature,
        ) -> u32,
    >,
}

#[repr(C)]
struct Lv2UiTouch {
    handle: Lv2UiFeatureHandle,
    touch: Option<extern "C" fn(handle: Lv2UiFeatureHandle, port_index: u32, grabbed: bool)>,
}

#[repr(C)]
struct Lv2UiIdleInterface {
    idle: Option<extern "C" fn(ui: Lv2UiHandlePtr) -> c_int>,
}

#[repr(C)]
struct Lv2UiShowInterface {
    show: Option<extern "C" fn(ui: Lv2UiHandlePtr) -> c_int>,
    hide: Option<extern "C" fn(ui: Lv2UiHandlePtr) -> c_int>,
}

#[repr(C)]
struct Lv2UridUnmap {
    handle: *mut c_void,
    unmap: extern "C" fn(handle: *mut c_void, urid: u32) -> *const c_char,
}

#[repr(C)]
struct Lv2Option {
    context: u32,
    subject: u32,
    key: u32,
    size: u32,
    ty: u32,
    value: *const c_void,
}

/// Events collected by [`Lv2UiHandle::pump`] on the UI thread.
#[derive(Debug, Default)]
pub struct Lv2UiPumpResult {
    pub closed: bool,
    /// Control writes from the plugin UI: `(control_nth, native_val, norm_0_1)`.
    pub control_writes: Vec<(usize, f32, f32)>,
    /// 3-byte MIDI channel-voice messages emitted via `atom:eventTransfer`.
    pub midi_events: Vec<[u8; 3]>,
}

/// Persistent URID map supporting both `map` and `unmap` for an LV2 UI.
struct UiUridMap {
    map: HashMap<String, u32>,
    by_id: Vec<CString>,
}

impl UiUridMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            // 0 is invalid per LV2 URID spec; start at 1.
            by_id: vec![CString::default()],
        }
    }

    fn map_str(&mut self, uri: &str) -> u32 {
        if let Some(&id) = self.map.get(uri) {
            return id;
        }
        let Ok(cs) = CString::new(uri) else { return 0 };
        let id = self.by_id.len() as u32;
        self.by_id.push(cs);
        self.map.insert(uri.to_string(), id);
        id
    }

    fn unmap_id(&self, urid: u32) -> *const c_char {
        if urid == 0 {
            return std::ptr::null();
        }
        self.by_id
            .get(urid as usize)
            .map_or(std::ptr::null(), |s| s.as_ptr())
    }
}

extern "C" fn ui_urid_map_cb(handle: *mut c_void, uri: *const c_char) -> u32 {
    if handle.is_null() || uri.is_null() {
        return 0;
    }
    unsafe {
        let m = &mut *(handle as *mut UiUridMap);
        let s = CStr::from_ptr(uri).to_string_lossy();
        m.map_str(&s)
    }
}

extern "C" fn ui_urid_unmap_cb(handle: *mut c_void, urid: u32) -> *const c_char {
    if handle.is_null() {
        return std::ptr::null();
    }
    unsafe {
        let m = &*(handle as *const UiUridMap);
        m.unmap_id(urid)
    }
}

struct UiShared {
    urid_map: Box<UiUridMap>,
    port_to_control: HashMap<u32, (usize, f32, f32)>,
    symbol_to_port: HashMap<String, u32>,
    urid_float_protocol: u32,
    urid_event_transfer: u32,
    urid_midi_event: u32,
    outgoing_controls: Vec<(usize, f32, f32)>,
    outgoing_midi: Vec<[u8; 3]>,
    grabbed_ports: HashSet<u32>,
    pending_resize: Option<(u32, u32)>,
}

extern "C" fn ui_write_cb(
    controller: Lv2UiController,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
) {
    if controller.is_null() || buffer.is_null() {
        return;
    }
    unsafe {
        let shared = &mut *(controller as *mut UiShared);
        if (port_protocol == 0 || port_protocol == shared.urid_float_protocol)
            && buffer_size as usize == std::mem::size_of::<f32>()
        {
            let val = *(buffer as *const f32);
            if !val.is_finite() {
                return;
            }
            if let Some(&(nth, min, max)) = shared.port_to_control.get(&port_index) {
                let hi = if max > min { max } else { min + 1.0 };
                let clamped = val.clamp(min, hi);
                let norm = ((clamped - min) / (hi - min).max(1e-6)).clamp(0.0, 1.0);
                shared.outgoing_controls.push((nth, clamped, norm));
            }
        } else if port_protocol == shared.urid_event_transfer && buffer_size >= 8 {
            // LV2_Atom header: u32 size, u32 type.
            let bytes = std::slice::from_raw_parts(buffer as *const u8, buffer_size as usize);
            let atom_size = u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4])) as usize;
            let atom_type = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
            if atom_type == shared.urid_midi_event && atom_size > 0 && bytes.len() >= 8 + atom_size {
                let payload = &bytes[8..8 + atom_size];
                if !payload.is_empty() && payload.len() <= 3 {
                    let mut msg = [0u8; 3];
                    msg[..payload.len()].copy_from_slice(payload);
                    shared.outgoing_midi.push(msg);
                }
            }
        }
    }
}

extern "C" fn ui_resize_cb(handle: Lv2UiFeatureHandle, width: c_int, height: c_int) -> c_int {
    if handle.is_null() || width <= 0 || height <= 0 {
        return 1;
    }
    unsafe {
        let shared = &mut *(handle as *mut UiShared);
        shared.pending_resize = Some((width as u32, height as u32));
    }
    0
}

extern "C" fn ui_port_index_cb(handle: Lv2UiFeatureHandle, symbol: *const c_char) -> u32 {
    if handle.is_null() || symbol.is_null() {
        return LV2UI_INVALID_PORT_INDEX;
    }
    unsafe {
        let shared = &*(handle as *const UiShared);
        let Ok(sym) = CStr::from_ptr(symbol).to_str() else {
            return LV2UI_INVALID_PORT_INDEX;
        };
        shared
            .symbol_to_port
            .get(sym)
            .copied()
            .unwrap_or(LV2UI_INVALID_PORT_INDEX)
    }
}

extern "C" fn ui_subscribe_cb(
    _handle: Lv2UiFeatureHandle,
    _port_index: u32,
    _port_protocol: u32,
    _features: *const *const Lv2Feature,
) -> u32 {
    0
}

extern "C" fn ui_unsubscribe_cb(
    _handle: Lv2UiFeatureHandle,
    _port_index: u32,
    _port_protocol: u32,
    _features: *const *const Lv2Feature,
) -> u32 {
    0
}

extern "C" fn ui_touch_cb(handle: Lv2UiFeatureHandle, port_index: u32, grabbed: bool) {
    if handle.is_null() {
        return;
    }
    unsafe {
        let shared = &mut *(handle as *mut UiShared);
        if grabbed {
            shared.grabbed_ports.insert(port_index);
        } else {
            shared.grabbed_ports.remove(&port_index);
        }
    }
}

/// A live native LV2 plugin UI window + callbacks.
pub struct Lv2UiHandle {
    desc: *const Lv2UiDescriptor,
    ui: Lv2UiHandlePtr,
    idle_iface: Option<Lv2UiIdleInterface>,
    show_iface: Option<Lv2UiShowInterface>,
    resize_iface: Option<Lv2UiResize>,
    window: Option<PluginWindow>,
    resizable: bool,
    shared: Box<UiShared>,
    control_to_port: HashMap<usize, (u32, f32, f32)>,
    _urid_map_data: Box<Lv2UridMap>,
    _urid_unmap_data: Box<Lv2UridUnmap>,
    _resize_feature_data: Box<Lv2UiResize>,
    _port_map_data: Box<Lv2UiPortMap>,
    _port_sub_data: Box<Lv2UiPortSubscribe>,
    _touch_data: Box<Lv2UiTouch>,
    last_sent: HashMap<u32, f32>,
    closed: bool,
}

impl Lv2UiHandle {
    /// Open an LV2 plugin's native GUI using pre-discovered metadata.
    ///
    /// Embeds `ui:X11UI` inside a host [`PluginWindow`], or calls
    /// `ui:showInterface` for standalone/floating LV2 UIs.
    pub fn open(
        plugin_uri: &str,
        title: &str,
        sample_rate: f64,
        initial_controls: &[(u32, f32)],
        controls: &[Lv2ControlDesc],
        port_symbols: &[(u32, String)],
        info: &Lv2UiInfo,
    ) -> Result<Self> {
        crate::binarch::ensure_loadable(Path::new(&info.binary_path))
            .map_err(|reason| anyhow!("{reason}"))?;

        let c_binary = CString::new(info.binary_path.as_str()).context("invalid LV2 UI path")?;
        let dl_handle = unsafe { libc::dlopen(c_binary.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if dl_handle.is_null() {
            let err = unsafe {
                let p = libc::dlerror();
                if p.is_null() {
                    "dlopen failed".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            anyhow::bail!("could not load LV2 UI library {}: {err}", info.binary_path);
        }

        let sym = unsafe { libc::dlsym(dl_handle, c"lv2ui_descriptor".as_ptr()) };
        if sym.is_null() {
            anyhow::bail!("{} exports no lv2ui_descriptor", info.binary_path);
        }
        type DescriptorFn = extern "C" fn(u32) -> *const Lv2UiDescriptor;
        let get_desc: DescriptorFn = unsafe { std::mem::transmute(sym) };

        let mut found_desc: *const Lv2UiDescriptor = std::ptr::null();
        for idx in 0..512 {
            let d = get_desc(idx);
            if d.is_null() {
                break;
            }
            let uri_ptr = unsafe { (*d).uri };
            if !uri_ptr.is_null() {
                let u = unsafe { CStr::from_ptr(uri_ptr).to_string_lossy() };
                if u == info.ui_uri {
                    found_desc = d;
                    break;
                }
            }
        }
        if found_desc.is_null() {
            anyhow::bail!("{} does not expose LV2 UI {}", info.binary_path, info.ui_uri);
        }

        let mut urid_map = Box::new(UiUridMap::new());
        let urid_float_protocol = urid_map.map_str(LV2_UI_FLOAT_PROTOCOL);
        let urid_event_transfer = urid_map.map_str(LV2_ATOM_EVENT_TRANSFER);
        let urid_midi_event = urid_map.map_str(LV2_MIDI_EVENT);
        let key_window_title = urid_map.map_str("http://lv2plug.in/ns/extensions/ui#windowTitle");
        let key_update_rate = urid_map.map_str("http://lv2plug.in/ns/extensions/ui#updateRate");
        let key_scale_factor = urid_map.map_str("http://lv2plug.in/ns/extensions/ui#scaleFactor");
        let key_sample_rate = urid_map.map_str("http://lv2plug.in/ns/ext/parameters#sampleRate");
        let ty_string = urid_map.map_str("http://lv2plug.in/ns/ext/atom#String");
        let ty_float = urid_map.map_str("http://lv2plug.in/ns/ext/atom#Float");

        let symbol_to_port: HashMap<String, u32> = port_symbols
            .iter()
            .map(|(idx, sym)| (sym.clone(), *idx))
            .collect();
        let mut port_to_control = HashMap::with_capacity(controls.len());
        let mut control_to_port = HashMap::with_capacity(controls.len());
        for c in controls {
            if let Some(&port_idx) = symbol_to_port.get(&c.symbol) {
                port_to_control.insert(port_idx, (c.index, c.min, c.max));
                control_to_port.insert(c.index, (port_idx, c.min, c.max));
            }
        }

        let mut shared = Box::new(UiShared {
            urid_map,
            port_to_control,
            symbol_to_port,
            urid_float_protocol,
            urid_event_transfer,
            urid_midi_event,
            outgoing_controls: Vec::new(),
            outgoing_midi: Vec::new(),
            grabbed_ports: HashSet::new(),
            pending_resize: None,
        });

        let urid_handle = &mut *shared.urid_map as *mut UiUridMap as *mut c_void;
        let mut urid_map_data = Box::new(Lv2UridMap {
            handle: urid_handle,
            map: ui_urid_map_cb,
        });
        let mut urid_unmap_data = Box::new(Lv2UridUnmap {
            handle: urid_handle,
            unmap: ui_urid_unmap_cb,
        });
        let shared_ptr = &mut *shared as *mut UiShared as *mut c_void;
        let mut resize_feature_data = Box::new(Lv2UiResize {
            handle: shared_ptr,
            ui_resize: Some(ui_resize_cb),
        });
        let mut port_map_data = Box::new(Lv2UiPortMap {
            handle: shared_ptr,
            port_index: Some(ui_port_index_cb),
        });
        let mut port_sub_data = Box::new(Lv2UiPortSubscribe {
            handle: shared_ptr,
            subscribe: Some(ui_subscribe_cb),
            unsubscribe: Some(ui_unsubscribe_cb),
        });
        let mut touch_data = Box::new(Lv2UiTouch {
            handle: shared_ptr,
            touch: Some(ui_touch_cb),
        });

        let mut window = if info.is_x11 {
            crate::plugin_window::ensure_display();
            crate::plugin_window::ensure_x11_egl_platform();
            Some(PluginWindow::open(title, 640, 480, info.resizable)?)
        } else {
            None
        };

        let c_title = CString::new(title.replace('\0', " ")).unwrap_or_default();
        let update_rate_val: f32 = 30.0;
        let scale_factor_val: f32 = 1.0;
        let sample_rate_val: f32 = sample_rate as f32;
        let options = [
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_window_title,
                size: (c_title.as_bytes().len() + 1) as u32,
                ty: ty_string,
                value: c_title.as_ptr() as *const c_void,
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_update_rate,
                size: 4,
                ty: ty_float,
                value: &update_rate_val as *const f32 as *const c_void,
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_scale_factor,
                size: 4,
                ty: ty_float,
                value: &scale_factor_val as *const f32 as *const c_void,
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: key_sample_rate,
                size: 4,
                ty: ty_float,
                value: &sample_rate_val as *const f32 as *const c_void,
            },
            Lv2Option {
                context: 0,
                subject: 0,
                key: 0,
                size: 0,
                ty: 0,
                value: std::ptr::null(),
            },
        ];

        let map_feat = Lv2Feature {
            uri: LV2_URID_MAP_URI.as_ptr() as *const c_char,
            data: &mut *urid_map_data as *mut Lv2UridMap as *mut c_void,
        };
        let unmap_feat = Lv2Feature {
            uri: LV2_URID_UNMAP_URI.as_ptr() as *const c_char,
            data: &mut *urid_unmap_data as *mut Lv2UridUnmap as *mut c_void,
        };
        let port_map_feat = Lv2Feature {
            uri: LV2_UI_PORT_MAP_URI.as_ptr() as *const c_char,
            data: &mut *port_map_data as *mut Lv2UiPortMap as *mut c_void,
        };
        let port_sub_feat = Lv2Feature {
            uri: LV2_UI_PORT_SUBSCRIBE_URI.as_ptr() as *const c_char,
            data: &mut *port_sub_data as *mut Lv2UiPortSubscribe as *mut c_void,
        };
        let touch_feat = Lv2Feature {
            uri: LV2_UI_TOUCH_URI.as_ptr() as *const c_char,
            data: &mut *touch_data as *mut Lv2UiTouch as *mut c_void,
        };
        let options_feat = Lv2Feature {
            uri: LV2_OPTIONS_URI.as_ptr() as *const c_char,
            data: options.as_ptr() as *mut c_void,
        };
        let resize_feat = Lv2Feature {
            uri: LV2_UI_RESIZE_URI.as_ptr() as *const c_char,
            data: &mut *resize_feature_data as *mut Lv2UiResize as *mut c_void,
        };
        let parent_feat = window.as_ref().map(|w| Lv2Feature {
            uri: LV2_UI_PARENT_URI.as_ptr() as *const c_char,
            data: w.id() as usize as *mut c_void,
        });

        let mut feature_ptrs: Vec<*const Lv2Feature> = vec![
            &map_feat,
            &unmap_feat,
            &port_map_feat,
            &port_sub_feat,
            &touch_feat,
            &options_feat,
            &resize_feat,
        ];
        if let Some(ref p) = parent_feat {
            feature_ptrs.push(p);
        }
        feature_ptrs.push(std::ptr::null());

        let c_plugin_uri = CString::new(plugin_uri).context("invalid plugin URI")?;
        let mut bundle = info.bundle_path.clone();
        if !bundle.ends_with('/') {
            bundle.push('/');
        }
        let c_bundle = CString::new(bundle).context("invalid bundle path")?;

        let instantiate = unsafe { (*found_desc).instantiate }
            .ok_or_else(|| anyhow!("LV2 UI {} has no instantiate()", info.ui_uri))?;
        let mut widget: Lv2UiWidget = std::ptr::null_mut();
        let ui_handle = instantiate(
            found_desc,
            c_plugin_uri.as_ptr(),
            c_bundle.as_ptr(),
            ui_write_cb,
            shared_ptr,
            &mut widget,
            feature_ptrs.as_ptr(),
        );
        if ui_handle.is_null() {
            anyhow::bail!("LV2 UI {} failed to instantiate", info.ui_uri);
        }

        let ext_fn = unsafe { (*found_desc).extension_data };
        let idle_iface = ext_fn.and_then(|f| unsafe {
            let p = f(LV2_UI_IDLE_INTERFACE_URI.as_ptr() as *const c_char);
            (!p.is_null()).then(|| std::ptr::read(p as *const Lv2UiIdleInterface))
        });
        let show_iface = ext_fn.and_then(|f| unsafe {
            let p = f(LV2_UI_SHOW_INTERFACE_URI.as_ptr() as *const c_char);
            (!p.is_null()).then(|| std::ptr::read(p as *const Lv2UiShowInterface))
        });
        let resize_iface = ext_fn.and_then(|f| unsafe {
            let p = f(LV2_UI_RESIZE_URI.as_ptr() as *const c_char);
            (!p.is_null()).then(|| std::ptr::read(p as *const Lv2UiResize))
        });

        if let Some(win) = window.as_mut() {
            if let Some((rw, rh)) = shared.pending_resize.take() {
                let _ = win.resize(rw, rh, info.resizable);
            } else if !widget.is_null() {
                if let Some((cw, ch)) = win.child_geometry(widget as usize as u32) {
                    let _ = win.resize(cw, ch, info.resizable);
                }
            }
        } else {
            let Some(ref show) = show_iface else {
                if let Some(cleanup) = unsafe { (*found_desc).cleanup } {
                    cleanup(ui_handle);
                }
                anyhow::bail!("LV2 UI {} supports neither X11UI nor ui:showInterface", info.ui_uri);
            };
            let show_fn = show.show.ok_or_else(|| anyhow!("LV2 UI {} showInterface has no show()", info.ui_uri))?;
            if show_fn(ui_handle) != 0 {
                if let Some(cleanup) = unsafe { (*found_desc).cleanup } {
                    cleanup(ui_handle);
                }
                anyhow::bail!("LV2 UI {} refused show()", info.ui_uri);
            }
        }

        let mut last_sent = HashMap::with_capacity(initial_controls.len());
        if let Some(port_event) = unsafe { (*found_desc).port_event } {
            for &(port_idx, val) in initial_controls {
                port_event(
                    ui_handle,
                    port_idx,
                    std::mem::size_of::<f32>() as u32,
                    0,
                    &val as *const f32 as *const c_void,
                );
                last_sent.insert(port_idx, val);
            }
        }

        // Pin `dl_handle` (never dlclose): C++ / X11 / GTK / Qt UI toolkits keep
        // static state whose destructors race if unloaded mid-session.
        let _ = dl_handle;

        Ok(Self {
            desc: found_desc,
            ui: ui_handle,
            idle_iface,
            show_iface,
            resize_iface,
            window,
            resizable: info.resizable,
            shared,
            control_to_port,
            _urid_map_data: urid_map_data,
            _urid_unmap_data: urid_unmap_data,
            _resize_feature_data: resize_feature_data,
            _port_map_data: port_map_data,
            _port_sub_data: port_sub_data,
            _touch_data: touch_data,
            last_sent,
            closed: false,
        })
    }

    /// Push a control port value change into the plugin UI unless the user is
    /// currently grabbing that control (`ui:touch`) or the value already matches.
    pub fn notify_control(&mut self, port_index: u32, native_value: f32) {
        if self.closed || self.ui.is_null() {
            return;
        }
        if self.shared.grabbed_ports.contains(&port_index) {
            return;
        }
        if self
            .last_sent
            .get(&port_index)
            .is_some_and(|&prev| (prev - native_value).abs() < 1e-6)
        {
            return;
        }
        self.last_sent.insert(port_index, native_value);
        if let Some(port_event) = unsafe { (*self.desc).port_event } {
            port_event(
                self.ui,
                port_index,
                std::mem::size_of::<f32>() as u32,
                0,
                &native_value as *const f32 as *const c_void,
            );
        }
    }

    /// Push a normalized `[0.0, 1.0]` control value into the plugin UI by
    /// control index (`c.index`), scaling without world discovery.
    pub fn notify_control_nth(&mut self, control_nth: usize, norm_0_1: f32) {
        let Some(&(port_idx, min, max)) = self.control_to_port.get(&control_nth) else {
            return;
        };
        let hi = if max > min { max } else { min + 1.0 };
        let native = min + norm_0_1.clamp(0.0, 1.0) * (hi - min);
        self.notify_control(port_idx, native);
    }

    /// Push a native control value into the plugin UI by control index (`c.index`).
    pub fn notify_control_nth_native(&mut self, control_nth: usize, native_value: f32) {
        let Some(&(port_idx, min, max)) = self.control_to_port.get(&control_nth) else {
            return;
        };
        let hi = if max > min { max } else { min + 1.0 };
        self.notify_control(port_idx, native_value.clamp(min, hi));
    }

    /// Run one frame of the LV2 UI's X11 window events, resize requests, and
    /// `ui:idleInterface`, draining any control/MIDI writes back to the host.
    pub fn pump(&mut self) -> Lv2UiPumpResult {
        let mut out = Lv2UiPumpResult::default();
        if self.closed || self.ui.is_null() {
            out.closed = true;
            return out;
        }
        if let Some(win) = self.window.as_mut() {
            let events = win.poll();
            if events.close_requested {
                self.close();
                out.closed = true;
                return out;
            }
            if let Some((w, h)) = events.resized.filter(|_| self.resizable) {
                if let Some(ref resize) = self.resize_iface {
                    if let Some(f) = resize.ui_resize {
                        f(self.ui, w as c_int, h as c_int);
                    }
                }
            }
            if let Some((rw, rh)) = self.shared.pending_resize.take() {
                let _ = win.resize(rw, rh, self.resizable);
            }
        }
        if let Some(ref idle) = self.idle_iface {
            if let Some(f) = idle.idle {
                if f(self.ui) != 0 {
                    self.close();
                    out.closed = true;
                }
            }
        }
        out.control_writes = std::mem::take(&mut self.shared.outgoing_controls);
        out.midi_events = std::mem::take(&mut self.shared.outgoing_midi);
        out
    }

    /// Hide and destroy the LV2 UI before dropping the parent X11 window.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if !self.ui.is_null() {
            if let Some(ref show) = self.show_iface {
                if let Some(hide) = show.hide {
                    hide(self.ui);
                }
            }
            if let Some(cleanup) = unsafe { (*self.desc).cleanup } {
                cleanup(self.ui);
            }
            self.ui = std::ptr::null_mut();
        }
        // Destroy the embedded UI before dropping the parent X11 window.
        self.window.take();
    }
}

impl Drop for Lv2UiHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// Compile the pure-C LV2 UI test fixture into a temporary `.lv2` bundle and
/// register it with [`super::add_bundle`].
pub fn build_fixture_bundle(tag: &str) -> Result<(std::path::PathBuf, String)> {
    let dir = std::env::temp_dir().join(format!("kabelsalat-lv2-ui-fixture-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let c_path = dir.join("fixture.c");
    let so_path = dir.join("fixture.so");
    std::fs::write(&c_path, include_str!("lv2_ui_fixture.c"))?;

    let status = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&so_path)
        .arg(&c_path)
        .status()
        .context("invoking cc to compile LV2 UI fixture")?;
    anyhow::ensure!(status.success(), "cc failed compiling LV2 UI fixture");

    let plugin_uri = "urn:kabelsalat:fixture:ui_gain".to_string();
    let ui_uri = "urn:kabelsalat:fixture:ui_gain#ui";
    std::fs::write(
        dir.join("manifest.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             @prefix ui: <http://lv2plug.in/ns/extensions/ui#> .\n\
             <{plugin_uri}> a lv2:Plugin ;\n\
                 lv2:binary <fixture.so> ;\n\
                 rdfs:seeAlso <plugin.ttl> .\n\
             <{ui_uri}> a ui:UI ;\n\
                 ui:binary <fixture.so> .\n"
        ),
    )?;
    std::fs::write(
        dir.join("plugin.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
             @prefix ui: <http://lv2plug.in/ns/extensions/ui#> .\n\
             <{plugin_uri}> a lv2:Plugin, lv2:AmplifierPlugin ;\n\
                 doap:name \"Kabelsalat LV2 UI Fixture\" ;\n\
                 ui:ui <{ui_uri}> ;\n\
                 lv2:port [\n\
                     a lv2:InputPort, lv2:AudioPort ;\n\
                     lv2:index 0 ; lv2:symbol \"in_l\" ; lv2:name \"In L\" ;\n\
                 ] , [\n\
                     a lv2:InputPort, lv2:AudioPort ;\n\
                     lv2:index 1 ; lv2:symbol \"in_r\" ; lv2:name \"In R\" ;\n\
                 ] , [\n\
                     a lv2:OutputPort, lv2:AudioPort ;\n\
                     lv2:index 2 ; lv2:symbol \"out_l\" ; lv2:name \"Out L\" ;\n\
                 ] , [\n\
                     a lv2:OutputPort, lv2:AudioPort ;\n\
                     lv2:index 3 ; lv2:symbol \"out_r\" ; lv2:name \"Out R\" ;\n\
                 ] , [\n\
                     a lv2:InputPort, lv2:ControlPort ;\n\
                     lv2:index 4 ; lv2:symbol \"gain\" ; lv2:name \"Gain\" ;\n\
                     lv2:default 1.0 ; lv2:minimum 0.0 ; lv2:maximum 2.0 ;\n\
                 ] .\n\
             <{ui_uri}> a ui:UI ;\n\
                 ui:extensionData ui:idleInterface, ui:showInterface ;\n\
                 ui:requiredFeature <http://lv2plug.in/ns/ext/urid#map> .\n"
        ),
    )?;
    super::add_bundle(&dir);
    Ok((dir, plugin_uri))
}

/// `kabelsalat lv2-ui-test [uri...]`: verify native LV2 GUI hosting on the
/// headless fixture (and any named installed LV2 plugins).
pub fn run_lv2_ui_test(uris: &[String]) -> Result<()> {
    let (dir, fixture_uri) = build_fixture_bundle("cli")?;
    let res = (|| -> Result<()> {
        let info = discover_ui_for_uri(&fixture_uri)
            .ok_or_else(|| anyhow!("discover_ui_for_uri found no UI for {fixture_uri}"))?;
        let controls = control_descs_for_uri(&fixture_uri);
        let symbols = port_symbols_for_uri(&fixture_uri);
        let mut handle = Lv2UiHandle::open(
            &fixture_uri,
            "LV2 UI Fixture",
            48_000.0,
            &[(4, 1.0)],
            &controls,
            &symbols,
            &info,
        )?;
        let first = handle.pump();
        anyhow::ensure!(!first.closed, "fixture UI closed prematurely");
        anyhow::ensure!(
            first.control_writes.iter().any(|&(nth, v, _)| nth == 0 && (v - 0.75).abs() < 1e-4),
            "fixture show() should emit 0.75 on port 4 (got {:?})",
            first.control_writes
        );
        handle.notify_control(4, 1.9);
        let second = handle.pump();
        anyhow::ensure!(
            second.control_writes.iter().any(|&(nth, v, _)| nth == 0 && (v - 0.25).abs() < 1e-4),
            "fixture port_event(1.9) should echo 0.25 (got {:?})",
            second.control_writes
        );
        let third = handle.pump();
        anyhow::ensure!(third.closed, "fixture idle() should close after 2 ticks");
        println!("lv2-ui-test {fixture_uri}: PASS (portMap, port_event, write_function, showInterface, idleInterface)");

        for uri in uris {
            let Some(ui_info) = discover_ui_for_uri(uri) else {
                println!("lv2-ui-test {uri}: SKIP (no native UI)");
                continue;
            };
            let ctrls = control_descs_for_uri(uri);
            let syms = port_symbols_for_uri(uri);
            let init: Vec<(u32, f32)> = ctrls
                .iter()
                .filter_map(|c| syms.iter().find(|(_, s)| s == &c.symbol).map(|&(p, _)| (p, c.default)))
                .collect();
            let mut ui = Lv2UiHandle::open(uri, &format!("{uri} · kabelsalat"), 48_000.0, &init, &ctrls, &syms, &ui_info)?;
            for _ in 0..20 {
                if ui.pump().closed {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            ui.close();
            println!("lv2-ui-test {uri}: PASS (X11={})", ui_info.is_x11);
        }
        // Also check discover() sets has_ui for installed plugins if any exist.
        let all = discover();
        let with_ui = all.iter().filter(|p| p.has_ui).count();
        println!("lv2-ui-test: {with_ui}/{} installed LV2 plugins advertise a native UI", all.len());
        let _ = Lv2Plugin::load(&fixture_uri, 48_000.0, 256);
        println!("=== lv2-ui-test PASS ===");
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lv2_ui_fixture_opens_syncs_controls_and_closes_via_idle() {
        run_lv2_ui_test(&[]).expect("lv2-ui-test fixture passes headlessly");
    }

    #[test]
    fn lv2_ui_refuses_foreign_cpu_binary_before_dlopen() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-lv2-ui-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (machine, arch_name) = crate::binarch::other_machine();
        let so_path = dir.join("foreign_ui.so");
        std::fs::write(&so_path, crate::binarch::fake_elf(machine)).unwrap();

        let info = Lv2UiInfo {
            ui_uri: "urn:example:ui".into(),
            bundle_path: format!("{}/", dir.display()),
            binary_path: so_path.to_string_lossy().into_owned(),
            is_x11: false,
            resizable: false,
        };
        let err = match Lv2UiHandle::open("urn:example:plugin", "Test", 48_000.0, &[], &[], &[], &info) {
            Ok(_) => panic!("foreign LV2 UI binary must be refused"),
            Err(e) => format!("{e:#}"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains(arch_name), "unexpected error: {err}");
    }
}
