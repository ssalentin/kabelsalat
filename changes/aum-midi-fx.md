### Added
- **MIDI effects in the matrix (#86):** an LV2 MIDI plugin (note transposer, arpeggiator, …) is now a "MIDI strip" unit. Notes go source → MIDI effect → instrument through the matrix, the strip's face shows the plugin's own main controls (learnable from a hardware controller), and a built-in transposer is always available. Agents get `list_midi_fx` and `add_node` kind `midi_fx`.
