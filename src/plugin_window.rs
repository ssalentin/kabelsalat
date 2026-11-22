//! A host-owned X11 toplevel that a plugin's own editor draws into.
//!
//! Linux plugin GUIs (CLAP `x11`, LV2 `X11UI`) only embed into an X11 window.
//! The app itself is a Wayland client, so each open editor gets its own
//! window through XWayland: kabelsalat creates it, titles it, hands its id to
//! the plugin as the parent, and closes the editor when the user closes it.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// What happened to the window since the last [`PluginWindow::poll`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WindowEvents {
    pub close_requested: bool,
    /// The window's size changed (the user or the window manager resized it).
    pub resized: Option<(u32, u32)>,
}

/// An active X11 RandR monitor rectangle in XWayland/X11 root coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorRect {
    name: String,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    primary: bool,
}

pub struct PluginWindow {
    conn: RustConnection,
    win: u32,
    wm_protocols: Atom,
    wm_delete: Atom,
    size: (u32, u32),
    pos: (i16, i16),
}

impl PluginWindow {
    /// Open a toplevel of `width` x `height`. `resizable` false pins min = max
    /// size, so tiling window managers float it at the plugin's own size.
    pub fn open(title: &str, width: u32, height: u32, resizable: bool) -> Result<Self> {
        ensure_display();
        ensure_x11_egl_platform();
        let (conn, screen_num) =
            x11rb::connect(None).context("no X server for plugin windows (is XWayland running?)")?;
        let screen = &conn.setup().roots[screen_num];
        let (w, h) = (clamp16(width), clamp16(height));
        // Centred on the active monitor in X11/XWayland coordinates: Hyprland
        // places a floating X11 window where it asks to be (and at 0,0 it
        // covered the top bar), while centering on the whole root screen put
        // the window at the screen edge on multi-monitor / scaled setups.
        let (mx, my, mw, mh) = target_monitor_rect(&conn, screen.root)
            .unwrap_or((0, 0, screen.width_in_pixels, screen.height_in_pixels));
        let (x, y) = center_in_rect(mx, my, mw, mh, w, h);
        let win = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(0x001c_1c1c)
                .event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        let atom = |name: &str| -> Result<Atom> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let wm_protocols = atom("WM_PROTOCOLS")?;
        let wm_delete = atom("WM_DELETE_WINDOW")?;
        let net_wm_name = atom("_NET_WM_NAME")?;
        let utf8 = atom("UTF8_STRING")?;
        conn.change_property32(PropMode::REPLACE, win, wm_protocols, AtomEnum::ATOM, &[wm_delete])?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?;
        conn.change_property8(PropMode::REPLACE, win, net_wm_name, utf8, title.as_bytes())?;
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"kabelsalat-plugin\0kabelsalat-plugin\0",
        )?;
        // A plugin editor is a dialog: window managers float it at its own
        // size instead of tiling it (Hyprland tiled a plain toplevel).
        let wm_type = atom("_NET_WM_WINDOW_TYPE")?;
        let dialog = atom("_NET_WM_WINDOW_TYPE_DIALOG")?;
        conn.change_property32(PropMode::REPLACE, win, wm_type, AtomEnum::ATOM, &[dialog])?;
        let window = Self { conn, win, wm_protocols, wm_delete, size: (w as u32, h as u32), pos: (x, y) };
        window.set_size_hints(resizable, true)?;
        window.conn.map_window(win)?;
        window.conn.flush()?;
        Ok(window)
    }

    /// The X11 window id a plugin embeds into.
    pub fn id(&self) -> u32 {
        self.win
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Resize to what the plugin asked for.
    pub fn resize(&mut self, width: u32, height: u32, resizable: bool) -> Result<()> {
        self.size = (clamp16(width) as u32, clamp16(height) as u32);
        self.set_size_hints(resizable, false)?;
        self.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new().width(self.size.0).height(self.size.1),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// How many windows the plugin put inside ours (1 once its editor attached).
    pub fn child_count(&self) -> Result<usize> {
        Ok(self.conn.query_tree(self.win)?.reply()?.children.len())
    }

    /// Drain pending X events without blocking.
    pub fn poll(&mut self) -> WindowEvents {
        let mut out = WindowEvents::default();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                Event::ClientMessage(ev)
                    if ev.window == self.win
                        && ev.type_ == self.wm_protocols
                        && ev.data.as_data32()[0] == self.wm_delete =>
                {
                    out.close_requested = true;
                }
                Event::DestroyNotify(ev) if ev.window == self.win => out.close_requested = true,
                Event::ConfigureNotify(ev) if ev.window == self.win => {
                    let size = (ev.width as u32, ev.height as u32);
                    if size != self.size && size.0 > 0 && size.1 > 0 {
                        self.size = size;
                        out.resized = Some(size);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// WM_NORMAL_HINTS: the program's position (centred on initial open) and
    /// size and, for a fixed-size editor, min = max = its size.
    fn set_size_hints(&self, resizable: bool, include_position: bool) -> Result<()> {
        const P_POSITION: u32 = 1 << 2;
        const P_SIZE: u32 = 1 << 3;
        const P_MIN_SIZE: u32 = 1 << 4;
        const P_MAX_SIZE: u32 = 1 << 5;
        let (w, h) = self.size;
        let mut hints = [0u32; 18];
        hints[0] = P_SIZE;
        if include_position {
            hints[0] |= P_POSITION;
            hints[1] = self.pos.0 as i32 as u32;
            hints[2] = self.pos.1 as i32 as u32;
        }
        hints[3] = w;
        hints[4] = h;
        if !resizable {
            hints[0] |= P_MIN_SIZE | P_MAX_SIZE;
            hints[5] = w;
            hints[6] = h;
            hints[7] = w;
            hints[8] = h;
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            self.win,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        )?;
        Ok(())
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

fn clamp16(v: u32) -> u16 {
    v.clamp(1, u16::MAX as u32) as u16
}

/// Active monitor's `(x, y, w, h)` in X11/XWayland root coordinates.
fn target_monitor_rect(conn: &RustConnection, root: u32) -> Option<(i16, i16, u16, u16)> {
    let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let monitors: Vec<MonitorRect> = reply
        .monitors
        .into_iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .map(|m| {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_default();
            MonitorRect { name, x: m.x, y: m.y, w: m.width, h: m.height, primary: m.primary }
        })
        .collect();
    if monitors.is_empty() {
        return None;
    }
    let focused = if monitors.len() > 1 { hyprland_focused_monitor() } else { None };
    let pointer = if monitors.len() > 1 && focused.is_none() {
        conn.query_pointer(root).ok().and_then(|c| c.reply().ok()).map(|p| (p.root_x, p.root_y))
    } else {
        None
    };
    let m = pick_monitor(&monitors, focused.as_deref(), pointer)?;
    Some((m.x, m.y, m.w, m.h))
}

/// On Hyprland (including Omarchy), ask which monitor currently has focus so
/// multi-monitor XWayland windows centre on the monitor where the user is.
fn hyprland_focused_monitor() -> Option<String> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return None;
    }
    let out = std::process::Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&out.stdout).ok()?;
    parse_hyprland_focused_monitor(text)
}

fn parse_hyprland_focused_monitor(json: &str) -> Option<String> {
    let monitors: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    monitors.into_iter().find_map(|m| {
        if m.get("focused")?.as_bool()? {
            m.get("name")?.as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn pick_monitor<'a>(
    monitors: &'a [MonitorRect],
    focused_name: Option<&str>,
    pointer: Option<(i16, i16)>,
) -> Option<&'a MonitorRect> {
    if monitors.len() == 1 {
        return monitors.first();
    }
    if let Some(name) = focused_name {
        if let Some(m) = monitors.iter().find(|m| m.name == name) {
            return Some(m);
        }
    }
    if let Some((px, py)) = pointer {
        if let Some(m) = monitors.iter().find(|m| {
            let (px, py) = (px as i32, py as i32);
            let (mx, my) = (m.x as i32, m.y as i32);
            px >= mx && px < mx + m.w as i32 && py >= my && py < my + m.h as i32
        }) {
            return Some(m);
        }
    }
    monitors.iter().find(|m| m.primary).or_else(|| monitors.first())
}

fn center_in_rect(mx: i16, my: i16, mw: u16, mh: u16, w: u16, h: u16) -> (i16, i16) {
    let x = mx as i32 + ((mw as i32 - w as i32) / 2).max(0);
    let y = my as i32 + ((mh as i32 - h as i32) / 2).max(0);
    (
        x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    )
}

/// A shell started outside the desktop session may lack DISPLAY while
/// XWayland is running: point it at the first X socket. Plugins open their
/// own X connections from DISPLAY, so it must be set before they create a GUI.
fn ensure_display() {
    if std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") else { return };
    let mut displays: Vec<u32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('X')?.parse().ok())
        .collect();
    displays.sort_unstable();
    if let Some(n) = displays.first() {
        std::env::set_var("DISPLAY", format!(":{n}"));
    }
}

fn should_set_x11_egl_platform(
    wayland_display: Option<&std::ffi::OsStr>,
    egl_platform: Option<&std::ffi::OsStr>,
) -> bool {
    wayland_display.is_some_and(|v| !v.is_empty()) && !egl_platform.is_some_and(|v| !v.is_empty())
}

/// Set `EGL_PLATFORM=x11` under Wayland when unset (#391).
///
/// Plugins built on `baseview` (e.g. Unsampled Piano) call legacy
/// `eglGetDisplay(x11_display)`; when `WAYLAND_DISPLAY` is set and
/// `EGL_PLATFORM` is unset, Mesa EGL guesses the Wayland platform and
/// dereferences the X11 display pointer as a `wl_display`, crashing with
/// `SIGSEGV`. `eframe`/`wgpu` uses Vulkan or explicit `eglGetPlatformDisplay`,
/// so keeping the main window on native Wayland is unaffected.
pub fn ensure_x11_egl_platform() {
    if should_set_x11_egl_platform(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("EGL_PLATFORM").as_deref(),
    ) {
        std::env::set_var("EGL_PLATFORM", "x11");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dual_monitors() -> Vec<MonitorRect> {
        vec![
            MonitorRect { name: "eDP-1".into(), x: 0, y: 0, w: 2560, h: 1600, primary: false },
            MonitorRect { name: "DP-1".into(), x: 2560, y: 0, w: 1920, h: 1080, primary: false },
        ]
    }

    #[test]
    fn parse_hyprland_focused_monitor_picks_focused_output() {
        let json = r#"[
            {"id": 0, "name": "eDP-1", "focused": false},
            {"id": 1, "name": "DP-1", "focused": true}
        ]"#;
        assert_eq!(parse_hyprland_focused_monitor(json).as_deref(), Some("DP-1"));
        assert_eq!(parse_hyprland_focused_monitor("[]"), None);
        assert_eq!(parse_hyprland_focused_monitor("not json"), None);
    }

    #[test]
    fn pick_monitor_prefers_focused_then_pointer_then_primary() {
        let mons = dual_monitors();
        assert_eq!(pick_monitor(&mons, Some("DP-1"), Some((100, 100))).unwrap().name, "DP-1");
        assert_eq!(pick_monitor(&mons, Some("eDP-1"), Some((3000, 100))).unwrap().name, "eDP-1");
        assert_eq!(pick_monitor(&mons, None, Some((3000, 100))).unwrap().name, "DP-1");
        assert_eq!(pick_monitor(&mons, None, Some((100, 100))).unwrap().name, "eDP-1");
        assert_eq!(pick_monitor(&mons, None, None).unwrap().name, "eDP-1");

        let mut with_primary = dual_monitors();
        with_primary[1].primary = true;
        assert_eq!(pick_monitor(&with_primary, None, None).unwrap().name, "DP-1");
    }

    #[test]
    fn center_in_rect_centres_on_each_monitor_and_clamps_oversized() {
        // DP-1 in XWayland with force_zero_scaling beside eDP-1 (2560x1600)
        assert_eq!(center_in_rect(2560, 0, 1920, 1080, 866, 674), (3087, 203));
        // eDP-1 itself
        assert_eq!(center_in_rect(0, 0, 2560, 1600, 866, 674), (847, 463));
        // Oversized plugin clamps to the monitor's top-left rather than negative offset
        assert_eq!(center_in_rect(2560, 100, 1920, 1080, 2400, 1200), (2560, 100));
    }

    #[test]
    fn x11_egl_platform_only_sets_under_wayland_when_unset() {
        let s = std::ffi::OsStr::new;
        assert!(should_set_x11_egl_platform(Some(s("wayland-1")), None));
        assert!(should_set_x11_egl_platform(Some(s("wayland-1")), Some(s(""))));
        assert!(!should_set_x11_egl_platform(Some(s("wayland-1")), Some(s("wayland"))));
        assert!(!should_set_x11_egl_platform(Some(s("wayland-1")), Some(s("x11"))));
        assert!(!should_set_x11_egl_platform(None, None));
        assert!(!should_set_x11_egl_platform(Some(s("")), None));
    }
}
