# Changelog

All notable changes to kabelsalat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). While the version is `0.x`, minor
releases may still change behaviour and session files.

## [Unreleased]

### Added
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
