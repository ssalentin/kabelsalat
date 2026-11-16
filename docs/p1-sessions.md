# Sessions, undo, starter templates (P1, task-8)

`src/session.rs` persists the control-plane graph (docs/ARCHITECTURE.md §7 "Sessions") and
delivers the GarageBand inheritance (docs/ARCHITECTURE.md §2b): one-click starter templates,
undo for every edit, and saves that never corrupt a user's work.

Built entirely on `src/control.rs` — the session stores the same `Graph` the
engine renders, so what you hear is what gets saved. No new dependencies
(serde + serde_json + anyhow, all pre-existing).

## Package layout

A session is a `name.ksalat/` **directory package**:

```text
My Song.ksalat/
  session.json        # format version, name, graph, knob values, matrix slot, state refs
  state/
    node-1.bin        # opaque plugin blob, one file per Clap node
```

- `session.json` embeds the control-plane `Graph` verbatim (serde), plus
  `params` (the persisted form of `SetParam`), an optional `matrix` slot, and
  `states` references. Blobs stay in separate binary files — the manifest is
  human-readable and the blobs are never interpreted.
- `format: 1` guards compatibility; loading a newer version fails with a
  plain-language error instead of misreading data.
- `Session::package_path(parent, name)` builds `parent/name.ksalat` and rejects
  names with slashes / `..` / empty.

## Plugin state: caller captures, session stores

The session **never calls host code**. The UI captures blobs on the main
thread and hands them in:

```rust
// Save: main thread, per Clap node.
let blob = clap_voice_main.save_state()?;          // host API (src/host.rs)
session.set_plugin_state(node_id, plugin_id, blob);
session.save(&package)?;

// Load: restore each blob, re-issue knobs as commands.
let session = Session::load(&package)?;
for st in session.plugin_states() {
    clap_voice_main.load_state(&st.data)?;         // host API
}
for p in session.params() {
    bus.send(Command::SetParam { node: p.node, param: p.param, value: p.value });
}
```

Blobs are opaque `Vec<u8>` — the round-trip test uses non-UTF8 bytes to prove
the session never parses them. `validate()` checks each blob belongs to an
existing Clap node whose `plugin_id` matches, so a blob can't be restored into
the wrong plugin.

## Atomic saves, originals untouched

`Session::save` never writes into an existing package:

1. Validate the session (plain-language errors on any integrity problem).
2. Stage a **complete new package** in a hidden sibling dir
   (`.<name>.staging-<pid>-<seq>-<nanos>`), same filesystem as the target.
   Every file is written temp-then-renamed and fsynced; the manifest is
   written last, so a half-staged dir is never a valid package.
3. Swap by directory rename: old package → hidden backup, staging → target,
   then remove the backup. If the swap fails, the backup is renamed back.
4. Any failure cleans up staging and leaves the user's original byte-identical
   (covered by `failed_save_keeps_original_untouched`).

Load defends against hand-edited manifests: state-file paths must be relative
with no `..` components (`safe_relative`), so a manifest can never reach
outside its package.

## Undo / redo

Snapshot-based (`History`), robust for structural edits (add/remove node,
plug/unplug) as well as knob turns:

```rust
let mut history = History::new();          // 128-step cap, bounded memory
history.record(&session);                  // BEFORE each user-visible edit
session.graph.add_node(kind, name);        // mutate freely
// ...
history.undo(&mut session);                // false when nothing to undo
history.redo(&mut session);
```

- A snapshot restores **graph + params + matrix slot**. Plugin blobs and the
  session name are deliberately not snapshotted: blobs are captured on demand
  at save time, renaming is not an edit.
- `record` clears the redo branch (a fresh edit forks history) and evicts the
  oldest snapshot at the cap.

## MIDI matrix slot

`Session::matrix()` / `set_matrix()` hold an opaque `serde_json::Value`.
task-9 serializes its routing table into this slot; the session persists it
verbatim without understanding it, so the two workstreams stay decoupled.

## Starter templates (docs/ARCHITECTURE.md §2b)

`Template::ALL` → `Lead + Reverb`, `Drums + Bass`, `Ambient Pad`. Each
`Template::graph()` is a pre-wired `Graph` from existing node kinds
(Keys / Clap / Dsp / MasterOut); `Template::session()` adds default knob
values so it sounds good immediately. All use Surge XT
(`org.surge-synth-team.surge-xt`), the ARM-native reference plugin.

| Template | Patch | Character |
|---|---|---|
| Lead + Reverb | Keys → Surge → Reverb → Master, dry Surge → Master | singing lead, lush tail |
| Drums + Bass | Keys → {Drums, Bass} Surge layers → Gain / Filter → Master | two layers, one keyboard |
| Ambient Pad | Keys → Surge → Delay → Reverb → Master, dry → Master | instant atmosphere |

Templates pick their own sequential edge ids (the `Graph` counter is private
to the control plane). `templates_are_valid_and_playable` asserts every edge
references real nodes, ids are unique, and there is a MIDI path Keys→synth and
an audio path synth→Master. Drums+Bass is layered for now; key splits arrive
with the MIDI matrix (task-9). Per-node DSP param tables land with task-7 —
the session stores opaque `(node, param, value)` triples either way.

## Integration notes (task-10)

- The UI owns a `Session` + `History`; back-panel edits call `history.record`
  then mutate `session.graph` (or issue control-plane commands and mirror them
  into the session). Knob moves call `session.set_param`.
- Before save, capture each Clap node's blob via `ClapVoiceMain::save_state`.
- After `RemoveNode`, call `session.prune_to_graph()` to drop orphaned
  params/blobs (validate rejects them otherwise, with a hint to prune).
- On load, restore blobs + re-issue `SetParam` commands (snippet above), then
  the engine recompiles its render plan at the next block boundary.

## Verification

`CARGO_TARGET_DIR=$HOME/.cache/ksalat-session-target cargo test session::`
→ **9 passed, 0 failed**:

- `save_load_round_trip` — graph, params, matrix, blobs, and node-id counters
  survive save→load exactly.
- `overwrite_replaces_package_and_leaves_no_stale_files` — re-save drops removed
  blobs, no staging/backup litter.
- `failed_save_keeps_original_untouched` — invalid save leaves the original
  byte-identical.
- `load_rejects_escape_paths_missing_files_and_future_versions` — path-traversal,
  missing-blob, and newer-format guards.
- `undo_redo_restores_graph_params_and_matrix`, `undo_respects_the_limit`,
  `undo_does_not_touch_blobs_or_name` — snapshot semantics.
- `templates_are_valid_and_playable` — all three templates valid, wired, and
  round-trip.
- `prune_and_package_path_guards` — orphan cleanup + name validation.

`cargo build` green; `control::` tests still 5/5 (no regression). The session
module carries `#![allow(dead_code)]` until task-10 wires it into the UI/CLI,
matching the convention in `src/matrix.rs`.

## App integration (landed)

The session model is now wired into the app via the assistant (commit `526665d`):
- `save_session { name }` — captures the current patch (graph from live cables),
  knob values (the 8-param map), and Surge plugin state (`ClapVoiceMain::save_state`)
  into `~/kabelsalat-sessions/<name>.ksalat`.
- `load_session { name }` — restores knob values + Surge plugin state (same-topology
  recall; arbitrary-graph load arrives with the dynamic rack).
- `list_sessions` — lists saved packages.
- `kabelsalat session-test` — end-to-end self-test: capture→save→reset→load→restore,
  asserts knobs + edge count round-trip. **Passes** (8 edges, 3 knobs restored).

Still to come: a visual save/load UI, starter-template loading (needs the dynamic
rack to render arbitrary graphs), and undo/redo wired to UI actions.

## Visual Sessions window (landed, commit 16a0a1b)

A **Sessions** button in the RIG INPUTS strip opens an egui window: name field +
"Save current patch", and a list of saved packages each with a **Load** button.
Uses the same capture/apply logic as the assistant tools — no assistant needed.
Templates still require the dynamic rack (their graphs differ from the fixed
6-unit layout); that is the next major UI step.

## Dynamic rack + starter templates (landed, commit 2f313c4)

The rack is now **data-driven**: `build_rack(Graph, params)` rebuilds units,
cables, and an adaptive layout from any control-plane graph (KEYS left, MASTER
right, one column per instrument + its DSP chain). Generic DSP faces render
Reverb/Gain/Filter/Eq3/Limiter/Saturation/Pan.

The 3 starter templates load via the Sessions window picker or **F5** (Lead+
Reverb), **F6** (Drums+Bass), **F7** (Ambient Pad); **F8** restores the demo.
Loading rebuilds the rack, applies defaults, and sends `Command::LoadGraph` +
`SetParam` to the engine (which remaps the Surge voices onto the new instrument
nodes and rebuilds the DSP + MIDI matrix).

Verified: extended `session-test` asserts exact unit/cable/layout counts for all
3 templates + demo restore; live GUI F5 → bridge get_graph shows KEYS/LEAD/
REVERB/MASTER (4 nodes), a note plays (output 0.33), F8 restores the demo.
Saved-session load now rebuilds full topology (was knob-only recall).
