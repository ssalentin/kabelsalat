### Added
- **LV2 plugin scan cache and faster sandbox load:** cache scanned LV2 plugin metadata under `$XDG_CACHE_HOME/kabelsalat/lv2-scan.json` (automatically invalidated when bundles or search paths change) and load individual bundles directly inside `plugin-worker`.
- **Co-hosted sandboxed LV2 GUIs (`instance-access`, `data-access`, meters & Atom/Patch events):** native LV2 GUIs run inside the plugin's sandbox worker process with direct DSP instance/data access, shared URID mapping, live control-output feedback, and bidirectional lock-free Atom event forwarding.
- **Full LV2 presets and user preset saving:** restore LV2 presets that carry `state:state` files and properties via `lilv`, and save user presets as standard LV2 bundles under `~/.lv2/`.
- **Quick-add picker page/filter navigation:** support `PageUp`, `PageDown`, `Home`, `End`, and `Tab` / `Shift+Tab` category filter cycling in the quick-add picker.

### Fixed
- **LV2 native UI filtering & X11 window handling:** filter out GTK wrapper bridge UIs (`lv2-gtk-ui-bridge` / `lv2-gtk2-ui-bridge`), skip LV2 UIs whose `requiredFeature`s are unsupported, advertise `XdndAware`/`XdndProxy`, forward child `WM_NORMAL_HINTS`, and resize child X11 windows directly when `ui:resize` is absent.
- **LV2 DSP compliance (`sampleRate`, `urid#unmap`, `resize-port:minimumSize`, multi-atom, `state` path features):** pass the standard `parameters#sampleRate` option alongside `urid#unmap` and `buf-size` options, size atom buffers by `resize-port:minimumSize` with 8-byte alignment, route MIDI to ports declaring `midi:MidiEvent`, pre-check `lv2:requiredFeature`, and pass `state#mapPath`/`makePath`/`freePath`.
