# vibez — research notes (adapt for kabelsalat)

Source: `~/src/vibez` (Rust + iced, ~280k LOC DAW). Best refs:
`docs/ARCHITECTURE.md`, `docs/PATTERNS.md`. Studied to inform the multi-rig feature.

## What to adapt

- **Bridge:** two `rtrb` rings — `EngineCommand` UI→audio (drained at top of each
  callback), `EngineEvent` audio→UI (polled per frame). No shared mutable state.
  kabelsalat already has the command ring; an event ring (levels, meters, note
  activity) would let the overview show live per-rig state without locks.
- **Rig ≈ channel strip bundle:** vibez `EngineTrack` = instrument + FX chain +
  sends + gain/pan + mute/solo + meters + preallocated scratch
  (`vibez-engine/src/mixer.rs`). Tracks sum to master + return buses. Fixed
  capacities reserved up front; UI refuses to exceed them. → keep fixed rig/voice
  capacities and preallocated buffers.
- **Overview/big-view split:** mix workspace = horizontal row of always-visible
  channel strips; the selected track drives a shared detail panel
  (`app/views_mixer.rs`, `views_detail.rs`). This is exactly the overview strip +
  big active-rig view kabelsalat wants. (vibez has no number-key rig switching —
  digits are pads — so we design that ourselves.)
- **Session:** versioned document; newer readers reject older-incompatible data
  rather than discard; keep runtime state out of the persisted doc. → session =
  `Vec<Rig>` + MIDI matrix; no runtime state persisted.

## Critical correctness lessons

- **Held keys remember their rig + pitch.** Each held computer key stores the
  target rig and original pitch so switching the active rig can't orphan a
  note-off; focus loss drains all held keys (`app/keyboard.rs`). → do this per rig
  in kabelsalat's multi-rig switching (otherwise notes hang when you switch).
- **Bind MIDI devices to rigs, never to UI selection.** vibez forwards MIDI to the
  *selected* track (its gap); kabelsalat's per-rig device matrix is the right
  model — keep it.
- **Push MIDI from the midir callback thread straight into the ring**, not via a
  UI tick (vibez's UI-tick polling adds ≤16 ms jitter). kabelsalat already queues
  hardware MIDI off the callback — keep that.
- **Never drop plugin instances in the audio callback.** vibez returns them to the
  UI thread via `EngineEvent::Dispose*`. kabelsalat's `ClapVoiceMain` is `!Send`
  and stays on the main thread — already correct; preserve this for multi-rig.

## vibez's gap (we already solve it)

- Single MIDI port only; channel info dropped; CC/pitchbend/aftertouch ignored;
  multi-port "still to come". kabelsalat's per-device matrix + the planned
  hot-wire MIDI matrix view go beyond this.