# Transport / metronome — P2

Status: **landed** (commit `1c7fd74`).

A BPM click track for live playing. The engine generates a decaying 1 kHz burst
on each beat while the transport is playing, mixed into the master (RT-safe, no
allocation). `Command::TransportPlay` / `TransportStop` / `SetBpm { bpm }`
(20..300, default 96).

- Assistant tools: `transport_play`, `transport_stop`, `set_bpm`.
- MCP tools: `transport_play`, `transport_stop`, `set_bpm`.
- **Verified:** `kabelsalat metro-test` — 6 click-blocks in 1 s @120 BPM, silent
  after stop. 88 unit tests pass.

Follow-ups: visual transport/BPM control in the rack, count-in, sync to file
players, tap tempo.
