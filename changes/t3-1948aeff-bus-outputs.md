### Added
- **A bus on its own output (#147):** every rig bus and every aux bus has an
  output menu in the mixer. Pick a sound card there and that bus plays to it
  only and leaves the main mix: the click or a monitor mix on the headphones,
  the main mix on the PA. Its fader, pan, mute, solo and inserts still act on
  it. Up to four outputs besides the master's. The choice is saved with the
  session; while the device is unplugged the bus is silent and never moves to
  another output, and it plays again when the device is back. The assistant
  and MCP agents do the same with `set_bus_output` and `get_bus_outputs`.
