### Added
- **MIDI learn and hardware sync on instrument interfaces (#241):** right-click
  any control on the expanded instrument console, keyboard dock, or organ rack
  face — including the swell pedal (CC 11), drawbars, rotary speed, percussion,
  drive, reverb, sustain/damper (CC 64), soft pedal (CC 67), mod wheel (CC 1),
  arp, and macro knobs — to arm MIDI learn, open its binding/range menu, or
  clear its mapping. Both learned bindings (`target="cc"` / `"knob"` / `"plugin"`)
  and transparent passthrough hardware CCs now sync directly to the on-screen
  pedals, buttons, wheels, drawbars, and knobs.
