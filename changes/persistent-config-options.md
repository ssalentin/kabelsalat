### Added
- **Permanent configuration & MIDI Show controls (#437):**
  - Added **Settings → Show** with persistent Show mode preferences
    (`Exit hold time`, `Clock format`, `Song notes & charts`, `Rig meters`,
    `Start in Show mode`), `ALSA Midi Through`, and permanent MIDI Show
    controls saved by stable port name (`midi_port_key`) in
    `~/.config/kabelsalat/config.json`.
  - Added the **`Show mode`** (`show_mode`) bindable Show action, plus inline
    manual **Edit** alongside **Learn** (CC, Note, or Program Change, optional
    MIDI channel filter, and specific hardware port) that preserves offline
    bindings across hot-plug and USB port re-ordering.
  - Added persistent preferences in Settings for **Reopen last session** and
    **Sync recording with transport** (**Session**), **UI scale**,
    **Rack CPU overlay**, and **System input method (IME)** (**Display**), and
    **Plugin sandboxing** (**Plugins**).
  - Added `get_config` and `set_config` tools for the assistant and MCP
    server, and extended `set_show_control` / `get_show_controls` with
    `show_mode`, `channel`, `device_port`, and `scope` (`global` | `session`).

### Changed
- **Merged Show controls:** Permanent global bindings in `config.json` merge
  with per-session overrides in `Setlist::controls` and survive loading
  templates or switching sessions; **Setlist → Show controls** links directly
  to **Settings → Show** (`Settings →`).
