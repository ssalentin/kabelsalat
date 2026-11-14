# Control plane (P1 keystone, task-6)

`src/control.rs` is the single typed API behind every frontend — the rack UI,
the built-in assistant, the CLI, and the future MCP server (PLAN §2c). 1:1 MCP
parity is structural because all frontends issue the same commands.

## Model

- `Graph { nodes, edges }` — serializable (serde) for sessions.
- `NodeKind`: `Keys` (MIDI source), `Clap { plugin_id }`, `Dsp(DspKind)`,
  `SystemAudio` (P2), `MasterOut`.
- `DspKind`: Gain, Pan, Eq3, Filter, Limiter, Saturation, Delay, Reverb.
- `PortRef { node, port }`, `Edge { id, from, to, kind: Midi|Audio }`.
- `Graph::demo_patch()` — the pre-wired starter rack (Keys→Surge→Delay→Master
  plus dry Surge→Master), matching the UI's default patch.

## Commands

`AddNode`, `RemoveNode`, `Connect`, `Disconnect`, `SetParam`, `NoteOn`,
`NoteOff`, `TransportPlay`, `TransportStop`.

Validation lives in the model: self-connections and duplicate edges are
rejected; removing a node cleans its edges.

## Transport

`command_bus()` → `(CommandBus, CommandDrain)`: a 1024-slot rtrb SPSC ring.
Frontends `send` (non-blocking, drops on overflow); the engine `drain`s at
block boundaries and recompiles its render plan — never mid-block. Queries
read snapshots; nothing here touches the audio thread directly.

## Status

- Model + bus + tests: **done** (4 unit tests green).
- Engine integration (generic graph replacing the fixed-topology atomics in
  `engine.rs`): in progress — the atomics remain the interim path so the app
  stays playable during the transition.
- Frontend migration (UI patching → commands, assistant get_status → graph
  snapshot): follows engine integration.
