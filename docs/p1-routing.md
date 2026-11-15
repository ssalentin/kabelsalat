# P1 — Per-rig input routing (AUM operating model)

Status: **source-class selection landed**; per-device identity + audio sources next.

## What works now

- **Source-tagged notes.** `Command::NoteOn/NoteOff` carry a `matrix::SourceId`
  (`Keys`, `LaptopKeys`, `Hardware(port)`, `Virtual`, `FilePlayer`). Every input
  path tags its source: on-screen keys → `Keys`, laptop keyboard → `LaptopKeys`,
  hardware MIDI → `Hardware(..)`, assistant `play_note` → `Keys`.
- **Per-rig source filter.** Each instrument voice has `src_keys/src_laptop/src_hw`
  flags (default all true). A note reaches a rig only if (a) the Keys→rig MIDI
  cable is plugged (graph gate, "what you patch is what you hear") AND (b) the
  rig accepts the note's source. `Command::SetRigSource { node, keys, laptop,
  hardware }` sets the flags.
- **Visual picker.** A top strip ("RIG INPUTS") with Keys / Laptop / HW toggles
  per rig (A = node 1, B = node 4). Toggling sends `SetRigSource`.
- **Assistant tool.** `set_rig_source { node, keys, laptop, hardware }` — same
  control plane, so MCP gets it for free later.

Verified: build green, 78 tests pass, two rigs play independently, source
filtering gates correctly. Commits `fab4280` (routing) + `d8a990e` (picker).

## Next increments (the full AUM vision)

1. **Per-device hardware identity — LANDED (commit 564c3c9).** Each hardware
   connection is tagged with a device index (`Hardware(idx)`); devices are
   enumerated by name. The engine pre-routes device ids 0..16 and a per-rig
   `rig_device: Option<u32>` filter selects which one is heard (`SetRigDevice`
   command). A per-rig "HW: All / HW: <name>" cycle button appears in the RIG
   INPUTS strip when devices are connected; assistant `set_rig_device` and MCP
   `set_rig_device` too. 2 unit tests verify the device filter + matrix routing.
   Live verification needs a keyboard connected (`amidi -l` was empty here).
2. **Audio sources as rig inputs.** File player + system-audio capture nodes
   (P2) feed a rig's chain directly, selectable as the rig input.
3. **Matrix splits/layers/transforms.** The built `matrix.rs` (37 tests) is now
   wired into the engine as the MIDI routing layer: default rules mirror "every
   source plays every rig", and `Command::SetSplit { point }` narrows key ranges
   in place (RT-safe). **Keyboard split landed** (commit `56b2b20`): a "Split @ C4"
   toggle in the RIG INPUTS strip + assistant `set_split` tool — rig A plays below
   the split, rig B at/above; note-offs follow their zone. 2 engine tests verify
   the partition. Still to expose: arbitrary per-rule layers, transpose, and CC
   mapping via the matrix (the routing layer is in place; these are UI/config).

## Design note

The graph MIDI cable remains the routing gate; the source filter/matrix sits on
top. This keeps back-panel patching honest while adding AUM-style per-rig input
selection on the front. When the matrix is wired in, MIDI cables visualize matrix
rules.