### Fixed
- **A reopened session keeps each synth's inputs (#124):** which sources play
  a CLAP synth (keyboard on or off, hardware on or off, one MIDI device) is
  now saved with the session. Before, every synth came back listening to the
  keyboard and all hardware. The device is remembered by its name, so a
  re-plug or another order of devices at the next start does not move the
  synth to another keyboard, and a device that is not plugged in when the
  session opens stays the synth's device and plays as soon as it is back.
