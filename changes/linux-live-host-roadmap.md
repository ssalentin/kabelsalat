### Added
- **Song notes and ChordPro charts in Show mode:** attach lyrics, cues, or ChordPro (`[Chord]`, `{comment:...}`) charts per song in the Setlist editor and read them on stage in Show mode with highlighted chords and keyboard scrolling.
- **VST3 Program Change support:** MIDI Program Change messages (`0xC0`) now translate to the plugin's `kIsProgramChange` unit parameter so hardware pedals and keyboards switch VST3 presets cleanly.
- **Startup realtime-priority warning:** logs a clear warning at startup if the audio thread is running without realtime priority (across PipeWire, JACK, and ALSA), pointing to Audio Doctor.
- **Raspberry Pi / ARM64 and backing-track band guides:** added dedicated sections in `docs/GUIDE.md` for headless ARM64 rigs and live backing-track band workflows.

### Fixed
- **Repeated Note On on a held note:** retriggering an already-held note now emits Note Off before Note On per voice/LV2 slot so a single Note Off never leaves an orphan voice sounding.
- **CLAP plugin editor parameter feedback:** plugin-initiated `ParamValueEvent`s from CLAP `out_events` are now drained lock-free to the host instead of being discarded between audio blocks.
- **Plugin scanner crash vs kill handling:** killing the app or scanner with `SIGKILL`/`SIGTERM` during plugin loading no longer negatively caches (blocklists) healthy CLAP/VST3 plugins.
- **Backing track stop-at-end:** starting a setlist song's backing track now always resets loop mode to off so a song stops cleanly at the end instead of looping unexpectedly.
- **MIDI device name readability:** long hardware MIDI device names in Settings, Show Controls, Levels, and the MIDI Matrix now truncate cleanly with full-name hover tooltips.
