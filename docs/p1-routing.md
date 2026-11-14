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

1. **Per-device hardware identity.** Currently all hardware is one `HW` class
   (`Hardware(0)` placeholder). Tag each hardware connection with its real ALSA
   port id, enumerate devices by name, and let each rig pick a *specific* device
   (rig 1 = keyboard A, rig 2 = keyboard B). Needs hardware connected to verify
   (`amidi -l` was empty during this session). Engine `source_accepted` already
   matches `Hardware(id)` — extend the per-rig filter from a bool to an allowed
   device set, and the picker from an HW toggle to a device dropdown.
2. **Audio sources as rig inputs.** File player + system-audio capture nodes
   (P2) feed a rig's chain directly, selectable as the rig input.
3. **Matrix splits/layers/transforms.** The built `matrix.rs` (37 tests) replaces
   the simple source filter for advanced routing: per-rule channel filter,
   transpose, key/velocity ranges (splits & layers), CC mapping. The source filter
   is a stepping stone; the matrix is the routing layer.

## Design note

The graph MIDI cable remains the routing gate; the source filter/matrix sits on
top. This keeps back-panel patching honest while adding AUM-style per-rig input
selection on the front. When the matrix is wired in, MIDI cables visualize matrix
rules.