# P3 — MCP server (1:1 agent access)

Status: **headless server landed** (`kabelsalat mcp`). GUI bridge is the follow-up.

## What works

`kabelsalat mcp` builds the graph engine + control plane headless (no GUI) and
serves MCP over **stdio** via **rmcp 3.10**. It owns a local `Graph` model (from
`Graph::demo_patch()`) and a running `GraphEngine` fed by the control-plane bus.
Mutating tools validate → send the matching `Command` → mirror the local model;
query tools read the model + engine output level. Live cpal output (PipeWire,
512 frames, 2 Surge voices); a dummy render-loop fallback keeps tools working
even without an audio device.

**14 tools, 1:1 with the control plane:**
`get_graph`, `get_status`, `add_node`, `remove_node`, `connect`, `disconnect`,
`set_param`, `set_rig_source`, `set_split`, `set_system_audio`, `note_on`, `note_off`, `transport_play`,
`transport_stop`.

Because every tool maps to the same typed `Command`/query the UI and assistant
use, agent access is structurally 1:1 with the app (docs/ARCHITECTURE.md §2c) — no parallel API.

## Verified

- `initialize` → serverInfo `kabelsalat 0.0.1`, protocol `2024-11-05`.
- `tools/list` → all 14 tools with JSON schemas.
- `tools/call get_graph` → 6 nodes, 8 edges (ids 1,3,4,5,6,7,8,9), correct kinds.
- `note_on`, `get_status` (audio_live, sample_rate), plus 17 extended checks
  (auto edge-id, duplicate/self-connect rejection, remove cleans edges,
  transport state) and forced-audio-failure fallback.
- `cargo build` green; 78 unit tests pass. Commit `cc0dd4c`.

## Implementation notes

- rmcp pattern: `#[tool_router] impl McpServer` + `#[tool]` fns taking
  `Parameters<T>` (T: Deserialize + JsonSchema), `#[tool_handler] impl
  ServerHandler`, served via `server.serve((tokio::io::stdin(), BufWriter<File>))`.
- **stdout purity:** host.rs logs to stdout during plugin load, so fd 1 is
  rerouted to stderr and MCP is served on a private dup (libc). Keeps the
  JSON-RPC stream clean.
- Deps added: rmcp, tokio (rt-multi-thread, macros), libc, schemars.

## Caveats / follow-ups

- Dynamically added clap/dsp nodes join the graph but **don't sound yet** — the
  engine creates voices/processors at startup only. Off-thread plugin loading
  for dynamic AddNode is the follow-up (documented in the tool description).
- **GUI bridge:** connect MCP to the *running* GUI app (shared control plane over
  a unix socket) so agents control what the user sees, not a separate headless
  engine. This is the next MCP milestone.
## MCP→GUI bridge (landed, commit f1f7f0f)

The MCP server can now control the **running GUI app**, not just a headless
engine — true 1:1 access to what the user sees.

- **Bridge:** `src/bridge.rs` runs a unix-socket server in the app at
  `$XDG_RUNTIME_DIR/kabelsalat.sock` (fallback `~/.cache/kabelsalat.sock`).
  Newline-delimited JSON: `{"op":"cmd","cmd":<Command>}` → forwarded to the
  control-plane bus; `{"op":"query","name":"get_graph"|"get_status"}` → reads a
  shared graph snapshot the UI mirrors from its live cables. `Command` gained
  serde derives so it crosses the socket as JSON. Stale sockets are removed on
  start; a live socket is never stolen.
- **Attach mode:** `kabelsalat mcp --attach` serves the SAME 14 tools over stdio,
  but proxies each to the bridge socket instead of a local engine. One
  `#[tool_router]` set; `State.backend` is `Local(bus)` or `Remote` — identical
  names, schemas, and error semantics. Fails fast with "start the app" if the
  socket is absent.
- **CLI:** `kabelsalat send '<json>'` sends one request to the live app and prints
  the reply (handy for scripting/tests).

### Verified (live app)
- `send get_graph` → live 6-node / 8-edge patch.
- `send {"SetSystemAudio":{"gain":0.5}}` → ok; a `NoteOn` raised the live
  `output_level` 0 → 0.44, proving commands reach the engine.
- `mcp --attach` → initialize + tools/list (14) + tools/call get_graph (live).
- 85 unit tests pass (80 + 5 bridge tests).

### Caveats
- Attach `get_status` omits `transport_playing` (bridge doesn't expose it yet).
- MCP-added nodes reach the engine but the GUI mirror shows its fixed 6 units
  (dynamic-node rendering is the dynamic-rack follow-up).

## Dynamic patching sounds (commit b599259)

`add_node` now builds the processor for added **dsp** and **lv2** nodes, so they
sound immediately once wired with `connect` (previously they joined the graph
silently). `remove_node` cleans up the corresponding slots. CLAP voices remain
main-thread-only, so new clap nodes sound via `load_graph`'s voice remap.
**Verified:** `kabelsalat add-node-test` (dynamically added gain node, peak
0.665); MCP `add_node` grows the graph (6→7 nodes); 89 unit tests pass.
