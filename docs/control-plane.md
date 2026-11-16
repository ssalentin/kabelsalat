# Control plane (P1 keystone, task-6)

`src/control.rs` is the single typed API behind every frontend — the rack UI,
the built-in assistant, the CLI, and the future MCP server (docs/ARCHITECTURE.md §2c). 1:1 MCP
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

- Model + bus + tests: **done** (unit tests green).
- Engine integration: **done** — `GraphEngine` is multi-voice. Each Clap node
  owns a Surge voice; each Dsp node owns a real built-in processor (`dsp.rs`,
  allocation-proven). `render` walks graph edges: voices render, feed the DSP
  nodes they're wired to, and everything wired to the master is summed. Notes
  fan out to every instrument with a Keys→instrument MIDI cable. Unplugging a
  cable silences that path independently.
- Starter patch (`Graph::demo_patch`): TWO channel stacks (Surge A + Delay A,
  Surge B + Delay B) mixed to a shared master. Node ids 0=Keys, 1=Surge A,
  2=Delay A, 3=Master, 4=Surge B, 5=Delay B; cable ids 1,3,4,5 (A) and 6,7,8,9 (B).
- Param ids: P_LEVEL=0 (instrument), P_TIME=1 / P_MIX=2 (delay, translated to
  the processor's native ids), P_VOL=3 / P_WARM=4 (master).
- Frontend migration: UI patching/knobs already issue commands; the multi-stack
  UI (two visible columns) is in progress. Assistant graph queries and the MCP
  server follow.
- Dynamic AddNode(Clap) with off-thread plugin loading: follow-up (voices are
  currently created at startup).
