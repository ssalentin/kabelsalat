### Added
- **dB faders, peak hold and clip lights (#72):** every mixer fader rides the meters' dB curve with a 0 dB detent (double-click for exact unity), a dB readout, a held peak line and a clip light you click to clear.
- **Mute, solo and pan on every source (#73):** each source strip has its own M, S and pan, scoped to its rig; the engine publishes them back and sessions keep them.
- **Insert slots in the channel strip (#74):** the strip lists the effects in its cable chain; add, bypass, reorder and remove them, and the back panel's cables follow.
- **Names, colours, reorder and collapse; master inserts (#77):** rename a strip, give it a colour, drag it to reorder, collapse it to a mini strip, and put effects on the whole mix before the limiter. Everything is saved with the session and available to the agent (`set_strip_mix`, `rename_strip`, `set_strip_color`, `move_strip`, `collapse_strip`, `add_insert`, `remove_insert`, `set_node_bypass`).

### Fixed
- **Solo of one source silences the others (#73):** sources without a strip entry kept playing while another source was soloed.
