### Fixed
- **Audio output self-test (#345):** `kabelsalat audio-out-test` now uses run-unique null sinks with `state.restore-props=false`, explicitly sets each test sink to 100 % and unmuted so saved WirePlumber volumes cannot attenuate measurements, and restores the user's configured PipeWire default sink (including unplugged devices) on exit, error, or panic.
