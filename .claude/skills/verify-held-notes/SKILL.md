---
name: verify-held-notes
description: Exhaustively checks that releasing a key always silences what it started, across every interleaving of split, MIDI-matrix, MIDI-cable and template-load edits, by driving the real GraphEngine::apply. Use when a change touches note routing in src/engine.rs (NoteOn/NoteOff, keys_gate, release_all, SetNodeSplit, MatrixRule, Connect/Disconnect, LoadGraph/LoadRigs), src/matrix.rs (Rule::transform, route_into) or Graph::set_split/connect/disconnect in src/control.rs, or when chasing a stuck/hanging note.
---

# Verify held notes

**Property:** once a key is released, nothing that key started is still
sounding on any instrument, whatever routing edits happened while it was held.
**Harm if broken:** a stuck note drones until the rack is reloaded (live, on
stage).

The matrix is stateless (`src/matrix.rs` routes each note-off by the rules *at
release time*), so every routing edit that can run while a key is held must
either keep the note-off path identical or release what is sounding first.
Unit tests check single orderings; this check covers all of them.

## Run

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --bin kabelsalat held_notes_are_released -- --nocapture
```

About 25 s in debug. The check lives in `src/engine/held_notes.rs`
(registered by `#[cfg(test)] mod held_notes;` in `src/engine.rs`). Subagents
add `CARGO_TARGET_DIR=$HOME/.cache/ksalat-<name>-target` (never /tmp).

Expected output at the sync commit:

```
held-notes: 93363 reachable states explored to fixpoint
KNOWN FINDING {Cable}: ...   1. NoteOn 59  2. Disconnect cable 1  3. NoteOff 59
KNOWN FINDING {Load}:  ...   1. MatrixRule Keys→Surge A transpose +12 ...  2. NoteOn 59  3. LoadGraph start patch  4. NoteOff 59
held-notes: 10 combined kind-set(s) attributed to known findings
test result: ok
```

## How it works (enough to judge a result)

- A breadth-first search over the **real** `GraphEngine::apply`
  (`render` drains every command into it, `src/engine.rs` `fn render`). No hand
  model of the engine: each state is rebuilt by replaying its shortest command
  path into a fresh engine.
- Start patch (`start_graph`): Keys(0) → Surge A(1) by MIDI cable 1, Keys →
  Surge B(3) by cable 6, both into Master(2), loaded with `LoadGraph` so the
  matrix gets its default Keys rules. Same MIDI topology as `demo_patch`,
  without the Delay nodes.
- Alphabet: `NoteOn`/`NoteOff` for keys 59 and 60 (a key is only released if
  held, only pressed if not, like a real keyboard), and the routing edits in
  `EDITS` plus `LoadStart`:
  - `SetNodeSplit`: None, 60, 48.
  - `MatrixRule` Keys→A: transpose 0, transpose +12, disabled. Keys→B: transpose 0, transpose −12.
  - `Disconnect` of cables 1 and 6.
  - `Connect`: cable 1 to Keys port 0, cable 6 to port 0 (LOW) and port 1 (HIGH).
  - `LoadGraph` of the start patch.
- "Sounding" is read from the test-only `note_log` (delivered note-on,
  note-off, and the `255` all-notes-off marker of `release_all`). A note-off
  silences every note of that pitch on that instrument.
- The state fingerprint is graph nodes + edges + matrix rules + held keys +
  sounding notes. The search runs until no new state appears (the **fixpoint**),
  so every interleaving of the alphabet, at any length, is covered. If it hits
  `explore(200_000)`'s bound it prints what it found, says `STOPPED AT BOUND`,
  and fails. A bound never passes.
- Each stuck note is labelled with the routing-edit **kinds** (Split, Matrix,
  Cable, Load) applied while its key was held. Root causes print with their
  shortest trace. Kind-sets that contain a `KNOWN` set are counted, not
  printed.

## Reading a result

- **`VIOLATION {...}` with a trace:** a release leaves a note sounding. Read
  the trace as the steps a user takes. `still sounding (instrument, pitch)`
  says which node id and pitch. Before calling it a product bug, rule out a
  modeling error:
  1. Is every step something the app can send? UI, MCP (`src/mcp.rs`) and the
     bridge all send these same `Command`s.
  2. Does the real voice behave like the log says? The log is written after
     `keys_gate` but **before** `source_accepted`. A violation that only
     exists because of the source filter is outside this model (see Not
     covered). To confirm with sound, render real Surge XT the way the
     `lv2_engine_test`-style self-tests do (`ClapVoice::new(sr)`, then
     `GraphEngine::new(Graph::demo_patch(), drain, sr, vec![(1, voice)])`).
     Compare the peak 5–6 s after the note-off with and without the edit. A
     stuck note stays around 0.2–0.3, a released one falls to 0.0.
  3. If it is real, report it with the trace. Do **not** weaken the property,
     drop an edit from the alphabet, or add to `KNOWN` to make it pass, unless
     the user agrees it is an accepted open finding.
- **`known finding(s) ... no longer reproduce`:** a change fixed one. Confirm
  the fix is intended, then delete that entry from `KNOWN`.
- **`state bound hit before fixpoint`:** the state space grew. That usually
  means notes now pile up (e.g. a release was removed). Look at the
  violations printed above it first. Raise the bound only if the alphabet was
  deliberately extended.
- **Compile error in `held_notes.rs`:** `Command`, `Graph` or the private
  `GraphEngine` fields it reads (`graph`, `matrix`, `note_log`, `apply`)
  changed. Update the check to the new API without changing what it asserts.

## Open findings (confirmed with real Surge XT audio at the sync commit)

1. **Cable:** hold a key, unplug (or move, rule 13 = Disconnect + Connect) the
   Keys→Surge MIDI cable, release the key: the note-off is dropped by
   `keys_gate` because the cable is gone, and Surge keeps sounding.
   `Command::Disconnect` releases nothing. Real audio: peak 0.31 five seconds
   after release (0.00 without the unplug).
2. **Load:** with a transposed rule (Keys→Surge A +12), hold 59 (Surge plays
   71), load a template (`LoadGraph`), release: the rebuilt matrix sends the
   note-off as 59, 71 stays on. `LoadGraph`/`LoadRigs` release nothing, and the
   CLAP voice survives the reload (remapped by plugin id). Real audio: peak
   0.23 (0.00 without the load).

Product code was not changed for these. A fix would likely call `release_all`
on the old destinations in `Disconnect` (MIDI edges from a Keys node) and on
every instrument before the `LoadGraph` rebuild. After a fix, remove the
entry from `KNOWN` and add the rule to AGENTS.md.

## Coverage record

| Check covers | Where |
|---|---|
| Note dispatch | `src/engine.rs`: `apply` arms `NoteOn`, `NoteOff`; `keys_gate`; `release_all` |
| Routing edits | `src/engine.rs`: `apply` arms `SetNodeSplit`, `MatrixRule`, `Connect`, `Disconnect`, `LoadGraph` |
| Matrix | `src/matrix.rs`: `Matrix::route_into`, `Rule::transform`, `transpose_note` |
| Graph edits | `src/control.rs`: `Graph::set_split`, `connect_with_id`, `disconnect`, `split_port` |

**Sync commit: `1772263`.** At that commit the check was confirmed to describe
the code above. When any listed function changes after it, re-read the change
against "How it works". If the check still describes the code, rerun it and
only then move this commit forward (`git log -1 --format=%h`). If it no
longer does (for example a new routing command, or note-offs now tracked per
key), extend `EDITS`/`Step` first.

## Not covered (a pass says nothing about these)

- **Sources other than `SourceId::Keys`.** Hardware MIDI (`Hardware(dev)`,
  the `HwDirect` thread) takes the same `apply` path but is not in the
  alphabet.
- **The voice-level source filter.** `source_accepted`, `SetRigSource`,
  `SetRigDevice`: the check runs with no CLAP voice slots. By reading the code,
  `SetRigSource` turning a source off while a key is held blocks that key's
  note-off the same way. Unverified.
- **Voice binding changes.** `RemoveNode`, `AddNode`, `LoadGraph` of a
  *different* graph, `LoadRigs` (multi-rig merge, several Keys nodes), and
  idle voices being re-bound later.
- **Detail limits:**
  - Two keys only (59, 60).
  - Splits only at None, 60 and 48.
  - Velocity ranges are not varied.
  - A note-off silences all voices of that pitch on an instrument. Duplicate
    same-pitch note-ons are not counted separately.
- **Audio.** Only note delivery. Release tails, CLAP/LV2 plugin behaviour and
  the RT path are separate.
