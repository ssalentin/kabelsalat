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

**13 tools, 1:1 with the control plane:**
`get_graph`, `get_status`, `add_node`, `remove_node`, `connect`, `disconnect`,
`set_param`, `set_rig_source`, `set_split`, `note_on`, `note_off`, `transport_play`,
`transport_stop`.

Because every tool maps to the same typed `Command`/query the UI and assistant
use, agent access is structurally 1:1 with the app (PLAN §2c) — no parallel API.

## Verified

- `initialize` → serverInfo `kabelsalat 0.0.1`, protocol `2024-11-05`.
- `tools/list` → all 13 tools with JSON schemas.
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