//! A host-owned X11 toplevel that a plugin's own editor draws into.
//!
//! Linux plugin GUIs (CLAP `x11`, LV2 `X11UI`) only embed into an X11 window.
//! The app itself is a Wayland client, so each open editor gets its own
//! window through XWayland: kabelsalat creates it, titles it, hands its id to
//! the plugin as the parent, and closes the editor when the user closes it.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
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

pub struct PluginWindow {
    conn: RustConnection,
    win: u32,
    wm_protocols: Atom,
    wm_delete: Atom,
    size: (u32, u32),
    pos: (u32, u32),
}

impl PluginWindow {
    /// Open a toplevel of `width` x `height`. `resizable` false pins min = max
    /// size, so tiling window managers float it at the plugin's own size.
    pub fn open(title: &str, width: u32, height: u32, resizable: bool) -> Result<Self> {
        ensure_display();
        let (conn, screen_num) =
            x11rb::connect(None).context("no X server for plugin windows (is XWayland running?)")?;
        let screen = &conn.setup().roots[screen_num];
        let (w, h) = (clamp16(width), clamp16(height));
        // Centred on the X screen: Hyprland places a floating X11 window where
        // it asks to be, and at 0,0 the editor covered the app's top bar.
        let x = ((screen.width_in_pixels as i32 - w as i32) / 2).max(0) as i16;
        let y = ((screen.height_in_pixels as i32 - h as i32) / 2).max(0) as i16;
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
        let window = Self { conn, win, wm_protocols, wm_delete, size: (w as u32, h as u32), pos: (x as u32, y as u32) };
        window.set_size_hints(resizable)?;
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
        self.set_size_hints(resizable)?;
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

    /// WM_NORMAL_HINTS: the program's position (centred) and size and, for a
    /// fixed-size editor, min = max = its size.
    fn set_size_hints(&self, resizable: bool) -> Result<()> {
        const P_POSITION: u32 = 1 << 2;
        const P_SIZE: u32 = 1 << 3;
        const P_MIN_SIZE: u32 = 1 << 4;
        const P_MAX_SIZE: u32 = 1 << 5;
        let (w, h) = self.size;
        let mut hints = [0u32; 18];
        hints[0] = P_POSITION | P_SIZE;
        hints[1] = self.pos.0;
        hints[2] = self.pos.1;
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
