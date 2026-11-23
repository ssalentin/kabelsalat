### Added
- **Show cues (#401):** every song and part can send MIDI program change, CC and note messages and OSC over UDP when the setlist enters it, from a thread of its own (a foot switch never waits for a UI frame, the audio thread does no I/O); agents use `get_cues` / `set_cues`.
- **MIDI Clock In (#401):** kabelsalat can follow an external MIDI clock: tempo, Start/Continue/Stop and Song Position Pointer lock the transport, and the song position is held on the sender's through tempo changes; agents use `set_midi_clock` with `follow` and `input`.
