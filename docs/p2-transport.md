# Transport / metronome — P2

Status: **landed** (engine `1c7fd74`, visual control `32cdc0b`).

A BPM click track for live playing. The engine generates a decaying 1 kHz burst
on each beat while the transport is playing, mixed into the master (RT-safe, no
allocation). `Command::TransportPlay` / `TransportStop` / `SetBpm { bpm }`
(20..300, default 96).

- Assistant tools: `transport_play`, `transport_stop`, `set_bpm`.
- MCP tools: `transport_play`, `transport_stop`, `set_bpm`.
- **Verified:** `kabelsalat metro-test` — 6 click-blocks in 1 s @120 BPM, silent
  after stop. 88 unit tests pass.

Visual control (commit `32cdc0b`): a **Transport** popover (top-strip button or
**F10**) with a Play/Stop toggle, a BPM −/number/+ stepper (20..300), and a live
beat dot. The toolbar Play button and **Space** drive the same transport. Verified
live via the bridge: periodic click peaks on Play, silence on Stop.

Follow-ups: count-in, sync to file players, tap tempo.
