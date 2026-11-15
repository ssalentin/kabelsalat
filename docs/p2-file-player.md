# File player (backing tracks) — P2

Status: **landed and verified** (commit `8c0ef64`). Play a backing track into the
live mix.

## How it works

- `src/player.rs`: a decode thread reads a **16-bit PCM WAV** (mono/stereo),
  linearly resamples to the engine rate, upmixes to stereo, and streams f32
  samples into a lock-free `rtrb` ring — the same RT pattern as `capture.rs`.
- Control is split to keep variable-length paths off the fixed-size command bus:
  the engine owns the `PlayerSink` (ring consumer); the UI owns the
  `PlayerControl` (a channel to the decode thread) and calls `play(path)` /
  `stop()`. The decode thread checks for stop/new-file between blocks.
- The engine pops the ring and mixes `player × player_gain` into the master.
  `Command::SetPlayer { gain }` sets the gain (0 mutes).
- Tools: assistant `play_file { path, gain }`, `stop_file`, `set_player { gain }`;
  MCP `set_player { gain }` (loading a file is GUI-side since paths can't cross
  the fixed-size bus).

## Verification

- `kabelsalat player-test` — plays `/usr/share/sounds/alsa/Front_Center.wav` and
  asserts audio flows through the ring. **Passes**: 50924 nonzero samples
  decoded and streamed.
- App startup clean (decode thread idle until a file is played).
- Build green; 85 unit tests pass.

## Follow-ups

- Compressed formats (mp3/flac/ogg) via **symphonia** (cached in the registry).
- Transport integration (play/pause/seek, loop), per-rig routing instead of
  master-only, a visual file-player channel, and MCP `play_file` via a bridge op.