### Added
- **MIDI learn for the mixer (#75):** right-click a fader, a pan field, or a
  mute or solo button in the mixer and move a control on your MIDI controller
  to bind it, on source strips, rig buses and the master. A learned fader
  shows its CC beside the dB readout, and the control follows your hardware on
  screen. Bindings are saved with the session and stay with their rig when
  rigs are added or deleted. The assistant and MCP agents bind them too
  (`set_cc_map` / `learn_cc` with `mix: vol | pan | mute | solo`).
