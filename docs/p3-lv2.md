# LV2 hosting — P3

Status: **discovery + audio hosting + engine integration landed** (commits
`7aaff11`, `db53b84`, `06ac691`, `25287a2`, `bf90611`). LV2 effects run in the
live mix; a plugin-browser UI is the remaining polish.

## What works

`src/lv2.rs` discovers the system's installed LV2 plugins via **hand-written
lilv FFI** — no bindgen/clang required (clang isn't installed). It links
`-llilv-0` directly with `extern "C"` declarations over opaque lilv types.

- `discover()` loads the lilv world, enumerates all plugins, and returns
  `{ uri, name, num_ports }` for each.
- `kabelsalat lv2-test` lists them. **Verified: 221 plugins** discovered (Carla,
  LSP, x42, etc.), correct names/URIs/port counts, no crash.

### FFI gotcha (fixed)

lilv node ownership differs by function: `lilv_plugin_get_uri` returns a
**shared** node that must NOT be freed; `lilv_plugin_get_name` returns an
**owned** node that MUST be freed. Freeing the shared URI node segfaults.
`node_borrowed` vs `node_owned` handle this correctly.

## Audio hosting — LANDED (commit db53b84)

`kabelsalat lv2-host-test` instantiates a real plugin and runs it on audio,
end to end. **Verified: hosted an LSP plugin, ran 256 frames, output peak
0.5000 for a 0.5 input (unity) — the audio path works.**

Key implementation points:
- `lilv_instance_activate/run/deactivate/connect_port` are **inline wrappers**
  in lilv.h, NOT exported symbols. We call the **LV2 descriptor function
  pointers** directly via the exposed `LilvInstance { lv2_descriptor,
  lv2_handle }` struct (`Lv2Descriptor` mirrors lv2/core/lv2.h; activate/
  deactivate may be NULL).
- Most plugins require the **URID-map feature**. We implement a host-side
  `UridMap` (HashMap URI→u32) with an `extern "C"` map callback, passed as a
  NULL-terminated `LV2_Feature` array to `lilv_plugin_instantiate`.
- The **options feature** (`LV2_OPTIONS__options`) is also provided: a
  zero-terminated `LV2_Options_Option` array with `sampleRate` (float) and
  `maxBlockLength` (int), keys/types mapped via the URID map. **Carla plugins
  now host too** (commit `06ac691`): `lv2-host-test` runs Carla audiogain_s,
  output 0.317 for a 0.5 input at the plugin's default gain.
- Ports are classified via `lilv_port_is_a` against lv2core AudioPort/
  ControlPort/InputPort nodes; audio ports get buffers, control inputs are set
  to their `lilv_port_get_range` default.

## Engine integration — LANDED (commit bf90611)

- `NodeKind::Lv2 { uri: Lv2Uri }` in the control plane (`Lv2Uri` is a 128-byte
  fixed buffer with string serde, so graphs/sessions carry the URI).
- The engine builds an `Lv2Slot` (reusable `Lv2Plugin`, `unsafe impl Send`) for
  every Lv2 node in `new`/`LoadGraph`, and in `render` sums the instruments
  wired into it, processes, and mixes the output to the master — same flow as
  built-in DSP. `Lv2Plugin` is created off the hot path (rare structural op).
- MCP `add_node` accepts `kind:"lv2"` + `uri`; `get_graph` reports lv2 nodes.
- The rack renders Lv2 nodes with a generic effect face.
- **Verified:** `kabelsalat lv2-engine-test` runs Keys→Surge→LV2(audiogain_s)→
  Master for 30 blocks, peak 0.27 — audio flows through the hosted LV2 node.
  Live app demo still plays (no regression); 88 tests pass.

## Remaining polish

1. LV2 instrument (MIDI/atom) support — needs an event buffer + URID atom map.
2. Per-port knob control of LV2 parameters (currently runs at defaults).
3. A plugin-browser UI to add LV2/CLAP plugins to a rig (discovery exists).
3. MIDI/atom ports via the URID map + an event buffer (for LV2 instruments).
4. A plugin browser UI + assistant/MCP tools to add LV2 plugins to a rig.

The CLAP hosting (`host.rs`) is the template for the engine integration; LV2
mirrors it with lilv instead of clack.