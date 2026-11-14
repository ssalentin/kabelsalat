# P1 — Embedded Surge editor (one window)

Status: **landed and verified live** (commit `3370194`). The user's key requirement:
the plugin editor lives INSIDE the main kabelsalat window, not a disconnected
floating window.

## How it works

- **Main-window X11 handle:** `eframe::Frame: HasWindowHandle` (via
  `eframe::wgpu::rwh`, rwh 0.6). Under X11 winit yields `RawWindowHandle::Xlib(h)`
  → `h.window` (Xcb variant also handled). No new dependency.
- **Embedding:** the CLAP gui extension `set_parent(ClapWindow::from_x11_handle(main_window_id))`
  makes Surge create its editor as an X11 **child** of the main window — the same
  mechanism proven in P0, but parented to the real app window instead of a
  separate host window.
- **Positioning:** a reserved bottom egui region (~55% height). The X11 child is
  configured via x11rb `configure_window` in **physical pixels** (egui points ×
  pixels_per_point ≈ 2.33 under XWayland), centered, with a CLAP-correct
  `adjust_size` → `set_size` fit loop (debounced on resize).
- **Lifecycle:** per-frame X-event pump handles resize/show/hide/closed and
  `call_on_main_thread_callback`. One editor pane at a time; A/B switch; clean
  exit via `on_exit`. Survives Tab-flip (fixed region).
- **Controls:** Editor toggle buttons in the RIG INPUTS strip + **F2** (Surge A) /
  **F3** (Surge B).

## Critical fix: winit X11 forcing

winit ≥ 0.29 **removed** `WINIT_UNIX_BACKEND` — setting it (the earlier approach)
was silently ignored, and winit picks Wayland whenever `WAYLAND_DISPLAY` is set.
`run_inner` now **hides `WAYLAND_DISPLAY`** when an X display is available, which
actually forces X11 (required for embedding). Verified with Wayland env present:
log `ui: forcing X11 backend (WAYLAND_DISPLAY hidden; embedding needs XWayland)`.

## Verification (live X11 window tree)

```
0xc00004 pid=204378 class="kabelsalat" geom=2560x1600+0+0
  0x1000009 pid=204378 geom=1239x772+466+828   ← Surge editor, _XEMBED_INFO set
```
Surge sets no WM_CLASS on embedded children (P0-consistent); identity proven by
nesting under the main window + same PID + `_XEMBED_INFO` + creation synced to
`set_parent`. The process's only top-level window is the main window. Before/after
trees confirm open/close/flip/resize/switch all behave. Build green; 80 tests pass.

## Caveats / follow-ups

- In-editor zoom beyond the reserved region snaps back (region is fixed-size).
- The editor keeps X focus while clicked (egui laptop-keys gate already respects
  focus, so typing in the editor doesn't play notes).
- Follow-up: resizable editor region, per-rig simultaneous editors, embed other
  CLAP plugins' editors generically.