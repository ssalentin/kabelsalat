### Added
- **Four more instruments on the keyboard (#198):** E-Piano, Bass, Wind and
  Drums each get their own keyboard surface and full-window console: tines
  that ring on the e-piano, a bass neck that shows and plays the notes, a
  breath control (CC 2, CC 11 or aftertouch) for winds, and drum pads and a
  kit seen from above that sound the moment you hit them.
- **Every rig has an instrument type (#198):** piano, e-piano, jazz organ,
  pipe organ, synth, bass, wind or drums, from its plugin or the sound it
  plays, or marked by hand. The mark is saved with the session and kept when
  you add, delete, clone or undo rigs. The left rail shows each rig's type.

### Changed
- **One instrument type everywhere (#198):** the Plugins browser groups and
  labels instruments by the same type the keyboard uses (Pianos, Organs,
  Basses, Winds, Drums), the catalog names each card's type, the demo phrase
  follows a rig's type and its mark, and the agent reads and sets the type of
  any rig (`get_keyboard_state`, `set_keyboard_mode` with `rig`, `get_rigs`,
  `list_plugins`).
- **Rack units name their plugin (#198):** an LV2 instrument's subtitle is
  the plugin's name instead of "lv2 synth".
