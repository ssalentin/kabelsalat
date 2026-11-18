### Added
- **DSP load and Engine info (#106):** the strip under the top bar shows
  `DSP 22 %` at its right end: the average render time as a share of one
  audio buffer. It turns amber from 70 % and red above 90 %, and adds a red
  count once the sound card reported xruns. A click opens Engine info: sample
  rate, buffer size and duration, inputs and outputs, render average and
  maximum, late blocks, xruns, the MIDI input latency and the output buffer,
  and the render time of every instrument and effect (average, maximum, share
  of the buffer), most expensive first. "Reset peaks" restarts the maximum,
  the late blocks and the xruns. The sound card's own latency and the
  audio-input latency are shown as "not measured", because kabelsalat has no
  trustworthy figure for them. The assistant and MCP agents read the same
  numbers with `get_engine_info` and reset them with `reset_engine_peaks`.

### Fixed
- **Render average read too low:** `render_avg_us` in `get_status` lost up to
  63 µs to integer division on every step, so a 50 µs render read 0 and a
  steady 300 µs one read 256. The average is now kept in nanoseconds.
- **Headless `kabelsalat mcp` reports its own buffer:** `get_status` named
  the app's 256-frame buffer while the headless engine runs 512 frames; it
  now reports the size the audio device asks for, and counts its xruns.
