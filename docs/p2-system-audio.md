# System-audio capture ("wire YouTube into the mix")

Status: **landed and verified** (commit `2e5ec8e`). An explicit objective: capture
system playback (browser/YouTube/etc.) and mix it into the live rig.

## How it works

- `src/capture.rs`: `SystemCapture` spawns **`parec`** (PipeWire's PulseAudio
  compat — no native PipeWire dep / clang needed) on a **monitor** source,
  `--format=float32le --rate=48000 --channels=2 --raw`, and pumps the PCM into a
  lock-free `rtrb` ring on its own thread.
- Source auto-detection: `SystemCapture::default_monitor()` = `pactl
  get-default-sink` + `.monitor` (captures whatever plays to the default output).
- The engine pops the ring (RT-safe) and mixes `capture × system_gain` into the
  master sum. **Gain defaults to 0 (muted).** `Command::SetSystemAudio { gain }`
  raises/lowers it.
- Started once at app startup (off the audio thread); the audio callback only
  pops samples and multiplies — no allocation, no subprocess work on the RT path.
- Tools: assistant `system_audio { gain }`, MCP `set_system_audio { gain }`.

## Feedback caveat (important)

A sink's **monitor captures everything played to that sink, including
kabelsalat's own output** if it targets the same sink. Raising the capture gain
while monitoring the output sink can create a feedback loop. Mitigations:
- Default gain is 0 (muted) — the user opts in.
- To avoid the loop: capture a *different* source, or route kabelsalat to a
  separate output sink so the monitored sink excludes it.
- A future enhancement: auto-detect the loop (capture source == output sink
  monitor) and warn/duck.

## Verification

- `kabelsalat capture-test` — generates a 440 Hz tone, plays it to the default
  sink via `paplay`, captures via the monitor, asserts audio flows through the
  ring. **Passes**: 81782 nonzero samples captured from
  `audio_effect.j293-convolver.monitor`.
- App startup log: `system audio: capturing <monitor> (muted; raise gain to mix in)`.
- Build green; 80 unit tests pass.

## Follow-ups

- Per-source selection (list monitor sources, pick one) instead of only default.
- Feedback auto-guard.
- A visual system-audio channel/gain control in the rack.
- Capture latency compensation (the monitor path adds a little latency).