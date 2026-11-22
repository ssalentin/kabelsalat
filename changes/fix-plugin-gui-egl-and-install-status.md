### Fixed
- **Unsampled Piano / `baseview` plugin GUIs on Wayland (#391):** set `EGL_PLATFORM=x11` at startup when `WAYLAND_DISPLAY` is set so Mesa EGL does not mistake an X11 plugin editor's display pointer for a Wayland display and crash on open.
- **Catalog install status after completion (#392):** `list_instruments` now reports `"installing": false` as soon as an instrument install finishes instead of staying stuck on `true` while the Plugins log stays visible.
