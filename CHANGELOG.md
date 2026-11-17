# Changelog

All notable changes to kabelsalat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). While the version is `0.x`, minor
releases may still change behaviour and session files.

## [Unreleased]

### Added
- **Confirm / Cancel buttons in the chat** (#32): when the assistant asks
  before a destructive action (delete a rig, install an instrument), the chat
  shows the two buttons; the click runs or cancels it.
- **Live meters for other apps** in the mixer's System audio strips (YouTube,
  …): measured per stream with `parec --monitor-stream`, dB-scaled.
- **MCP `play_file` / `stop_file`** (#22): attached agents
  (`kabelsalat mcp --attach`) can start and stop backing tracks through the UI
  bridge; headless `stop_file` mutes the player.
- **Reset an unbound knob from its menu** (#20): right-click arms MIDI learn,
  a second right-click opens the menu (Cancel MIDI learn / Reset to default)
  instead of only cancelling.
- **setBfree plays** (#36): the LV2 host now provides the Worker extension
  (`work:schedule`); each plugin with a `worker#interface` gets a non-RT worker
  thread fed through two lock-free rings (requests and responses), with
  `work_response`/`end_run` delivered in the next `run()` cycle. This unblocks
  setBfree, which requires `urid:map` and `work:schedule` to instantiate.
- **Rename rigs:** double-click a rig chip; agent tool `rename_rig`. Rig names
  are saved with the session (#33).
- **Built-in instrument browser** (#10): an "Install" tab in the Plugins view
  shows a hand-picked catalog — setBfree, Surge XT, Dexed, Vitalium and YANES —
  each linked to its GitHub project. A distro package is preferred (prebuilt,
  signed), otherwise the instrument is built from source into `~/.clap` /
  `~/.lv2`. Installs run on a worker thread with a live log in the view and a
  plugin rescan when they finish; "Installed" badges reflect the plugin scan or
  the bundle on disk. Agent parity: `list_instruments` and `install_instrument`
  (confirm-gated) in the assistant and MCP.
- **System audio in the mixer:** every other app that is playing (YouTube in
  the browser, …) gets a strip with volume and mute (#31).
- **Output device selection** at Master out (e.g. MOTU M2 vs. speakers):
  moves the live stream without a dropout, remembered across restarts (#31).
- Agent tools `list_audio_outputs`, `set_audio_output`, `list_app_streams`,
  `set_app_stream`.
- **Audio In rack unit** (#28): plug in a microphone, guitar or audio interface
  and play it live through the rack. An Audio In node picks a device (cpal, hot
  plug rescans every 2 s — a MOTU M2 shows up as "MOTU M2", readable, not the
  raw ALSA id) and a channel (In 1 / In 2 / stereo), has gain and an input
  meter, low-latency monitoring (monitor adds it straight to the master), and a
  tuner on the face (YIN pitch detection off the audio thread, note + cents).
  The input ring is lock-free (`rtrb`), the render path stays allocation-free,
  and a small drift controller reconciles input vs output clocks (drop/insert
  around a target fill). When the interface is also the output, the input
  opens at the output sample rate so they share one clock. `kabelsalat
  audio-in-test` loops the default input to the output and reports measured
  stream latency. Agent parity: `add_node kind="audio_in"`, `set_audio_in`,
  `list_audio_devices` in the assistant and MCP; sessions save the selection.
- **The agent can measure the mix** (`measure_mix`, also in `get_status`):
  momentary, short-term and integrated loudness (LUFS), true peak, a bass
  mono check and a 4-band balance. `kabelsalat meter-test` checks it with
  real Surge audio (#11).
- **Undo for agent changes:** one request (or one burst of MCP calls) is one
  step; `undo`/`redo` tools and Ctrl+Z / Ctrl+Shift+Z.
- **External agents are visible:** "● Agent connected" in the top bar while an
  MCP client acts; one click pauses it.
- After recording, the top bar names the saved take with **Play** and **Folder**.

### Changed
- **I** toggles the info line at the top right (was D, which is also a note
  key); it now starts with the last incoming MIDI event — device, note and
  velocity, or CC number and value.

### Fixed
- **Effect chains are audible:** an LV2 instrument into a built-in effect
  (setBfree → Drive) and any effect into another effect (Delay → Reverb) were
  silent at the master. Effects now run in signal-flow order and hear every
  source wired into them.
- **Hanging LV2 notes** (setBfree): a note-off and a note-on in the same
  audio block (legato) lost the note-off, and chords played only one note.
  Every event now reaches the plugin once; atom outputs get a real buffer
  (#38).
- **Agents wiring over `mcp --attach`:** add_node → connect works at once and
  auto cable ids no longer collide; get_graph reflects edits immediately (#37).
- **Keyboard dock follows the window height** (#23).
- **LV2 instrument detection** uses the plugin's declared class; LSP effects
  no longer show up as instruments (#26).
- **System audio GAIN knob works** (#21): it sets the capture gain through
  SetParam (so it is MIDI-bindable), starts muted, and follows
  `system_audio`/`set_system_audio` changes from any agent.
- **Headless MCP knob names match the rack** (#19): one knob table for the
  rack, the in-app agent and headless MCP (`RESO`, `CEIL`; long names like
  `RESONANCE` as aliases); headless `list_params` lists the knobs. Headless
  `CEILING` no longer binds the DRIVE knob, and the limiter's params are named
  Ceiling/Drive.
- The knob context menu keys its knob by node + param, not unit/knob indices
  (#24).
- The top bar no longer runs past the window edge in narrow windows (compact
  mode, window-width based); the info line is truncated or moves to the hint row.
- The master output was 2–4 dB too quiet at full volume: level lost in the
  master limiter is now made up (peaks at −1 dBFS, never above), VOLUME and
  the templates default to 100 %, and the master meters use a dB scale.
- Vitalium (LV2) could corrupt memory and crash the app when an instance was
  removed or reloaded; instances are now kept and reused (#14).
- Agent tools no longer freeze the window while plugins are scanned for the
  first time (#15).
- A re-plugged MIDI keyboard keeps its rig binding and matrix row (#16).
- A MIDI interface re-plugged under the same name is reconnected instead of staying silent.
- Undo keeps the keyboard/hardware source switches (#17).
- Recording file names use local time instead of UTC.
- The file player reads 24-bit PCM and 32-bit float WAV (it can play the
  app's own recordings).

## [0.1.0] — 2026-11-16

The first tagged release: a working live rack for Linux.

### Added
- **Rack** with animated patch cables; **Tab** flips between the front (play)
  and the back (patch). Every unit shares one face layout.
- **Multiple rigs** that all sound at once, each with its own MIDI source and
  device; **+ Rig** adds a playable rig, also with any installed CLAP instrument.
- **MIDI matrix** with per-cell rules (channel, transpose, key/velocity range)
  and keyboard **splits**; held notes are always released when routing changes.
- **Mixer** per rig (volume, pan, mute, solo) with post-fader rig meters.
- **Software keyboard** and **assistant** as bottom panels; the top bar
  separates the main view (Rack, Sessions, Plugins, Matrix, Mixer) from them.
- **Plugin hosting:** CLAP (Surge XT, Dexed, YANES, …) and LV2 (e.g. Vitalium)
  with native in-app editors, sound browsers and presets; units show the
  loaded instrument and patch.
- **Per-knob MIDI learn:** right-click a knob, move a controller knob, bound.
- **Recording** of the master output to WAV, with the last take playable from
  the top bar; transport, metronome, count-in and tap tempo.
- **Built-in assistant** (DeepSeek, bring your own key) and an **MCP server**
  (`kabelsalat mcp`, `--attach` to drive the running app); agent changes are
  highlighted in the rack and undoable one job at a time (Ctrl+Z).
- Sessions and starter templates; system-audio capture and WAV backing tracks.
- Arch/Omarchy packaging (`packaging/PKGBUILD`).

[Unreleased]: https://github.com/ssalentin/kabelsalat/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ssalentin/kabelsalat/releases/tag/v0.1.0
