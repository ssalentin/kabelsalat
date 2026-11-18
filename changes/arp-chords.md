### Added
- **Arpeggiator and chord unit (#90):** an Arp unit sits between your keyboard
  and a synth. Held keys become an arpeggio (up, down, up-down, random) at a
  note value synced to the tempo (1/4 to 1/32, with triplets), over 1 to 4
  octaves, with gate and swing, or in chord mode one key sounds a whole chord
  (major, minor, sus2, sus4, 7th, maj7, min7 or power). Every setting is a
  knob you can learn to a MIDI controller, and the assistant and MCP agents
  can add and tune it (`add_node` kind `arp`, `set_arp`).
- **Scale snap on the keys (#90):** pick a key and a scale (major, minor,
  dorian, mixolydian, harmonic minor, pentatonics, blues) on the Keys unit and
  out-of-scale keys play the nearest in-scale note, so no key sounds wrong.
  The assistant can set it too (`set_scale`). Both survive sessions and undo.

### Fixed
- **Keys unit controls on narrow racks (#90):** the Keys face keeps room for
  its SPLIT and SCALE controls however narrow the rack gets.
