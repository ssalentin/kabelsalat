### Added
- **MIDI Clock Send (#85):** kabelsalat can be the master MIDI clock. Switch it
  on in the matrix view, wire it to any MIDI output port (or connect an app to
  the virtual "MIDI Clock" port) and your hardware follows the transport: 24
  clocks per beat, Start when the song starts, Stop when it stops, optional
  Song Position Pointer with Continue. A sync offset in milliseconds and three
  latency modes (none, output, input) line the clock up with the audio; a
  negative offset adds a pre-roll by itself. The clock is timed by the ALSA
  sequencer from the engine's own song position, so it does not wait for the UI.
  The assistant and MCP get `get_midi_clock` and `set_midi_clock`.
