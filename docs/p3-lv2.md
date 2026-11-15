# LV2 hosting — P3

Status: **discovery landed** (commit `7aaff11`); audio hosting is the next step.

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

## Next: audio hosting

Build on the discovery path to actually run LV2 plugins:
1. `lilv_plugin_instantiate(plugin, sample_rate, features)` — needs a features
   array; audio-only plugins need none, atom/MIDI plugins need a URID map
   (`LV2_URID__map`) implemented host-side.
2. Enumerate ports (`lilv_plugin_get_port_by_index`, class audio/control/atom),
   connect buffers (`lilv_instance_connect_port`).
3. `lilv_instance_activate` → `lilv_instance_run(handle, n_frames)` per block →
   `lilv_instance_deactivate`.
4. Integrate as a control-plane node kind (`NodeKind::Lv2 { uri }`) alongside
   CLAP, with the same RT discipline (instantiate off-thread, run in the engine).
5. A plugin browser UI + assistant/MCP tools to add LV2 plugins to a rig.

The CLAP hosting (`host.rs`) is the template for the engine integration; LV2
mirrors it with lilv instead of clack.