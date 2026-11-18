# MIDI matrix (P1, task-9)

`src/matrix.rs` is the MIDI routing core from docs/ARCHITECTURE.md §5: **any source → any
destination**, with per-rule channel filter, transpose, key/velocity ranges
(splits & layers), and CC mapping. It is a **pure, deterministic, standalone
module** — no engine, I/O, locks, or threads. Engine integration (feeding it
from `midi.rs` / the on-screen keys and scheduling its output sample-accurately)
is the lead's step; the contract for that is at the bottom of this doc.

## Model

| Type | What it is |
|---|---|
| `MidiMsg` | One channel-voice message. `Copy`, fixed-size: `status` (type nibble `0x80`–`0xE0`), `channel` (0-based 0..=15), `data: [u8; 2]` (7-bit). Constructors: `note_on`, `note_off`, `cc`, `pitch_bend` (signed −8192..=8191), `program_change`, `channel_aftertouch`, `poly_aftertouch`. `from_bytes` decodes raw midir/ALSA bytes; system messages and sysex are out of P1 scope and rejected. |
| `SourceId` | Where an event came from: `Hardware(port)`, `Virtual(port)`, `Keys` (on-screen), `LaptopKeys`, `FilePlayer(id)` (P2). Ids are opaque handles — the matrix matches them, never resolves them. |
| `Rule` | One routing entry: `source → dest` (a control-plane `NodeId`) plus `channel: Option<u8>`, `transpose: i8`, `key_range: (u8, u8)`, `vel_range: (u8, u8)`, `cc_map: Vec<CcMapping>`, `enabled: bool`. Chainable builders: `Rule::new(src, dest).on_channel(2).key_range(0, 59).transpose(-12).map_cc(1, 5)`. |
| `CcMapping` | `cc: u8 → param: ParamId` on the rule's destination node, with a `target` space and a `scale` (`CcScale`). |
| `CcScale` | How a controller value becomes the parameter value: `steps` equal sections of 0..=127 (0 = continuous) spread over `lo..=hi`. The default is the identity (MIDI learn); instrument profiles (`instruments.rs`) use steps so a slow/fast switch lands on SLOW or FAST. |
| `Matrix` | The routing table (`rules: Vec<Rule>`, serde-serializable for sessions). `add_rule` assigns stable `RuleId`s; `connect(src, dest)` adds a transparent rule; `rule_mut(id)` edits live; `remove_rule(id)` deletes. |

Core API:

```rust
let out: Vec<(NodeId, MidiMsg)> = matrix.route(source, event);
// RT variant — reuses a reserved buffer, never allocates in steady state:
matrix.route_into(source, event, &mut out);
```

## Semantics (the contract)

Rules are evaluated **in insertion order**; output order is stable and
deterministic. One source feeding many rules = **LAYERS**; disjoint key ranges
across rules = **SPLITS**. Duplicate rules are allowed and produce duplicate
events (the table does exactly what it says).

Per event, each matching rule applies, in order:

1. **Enabled + source match** — disabled (`enabled = false`) rules route
   nothing; a rule only sees events from its own `SourceId`.
2. **Channel filter** — `None` = omni (any channel), `Some(ch)` = that 0-based
   channel only. Applies to *all* message types. The event's channel is
   **preserved** on output (filter, not remap).
3. **Note messages** (`NoteOn`/`NoteOff`):
   - **Key range** filters the **played** note, *before* transpose, bounds
     inclusive. Split zones therefore partition the physical keyboard and
     don't move when a zone is transposed.
   - **Transpose** adds semitones and **clamps to 0..=127 — never wraps**
     (wrapping would jump octaves and scramble splits). Note-offs are
     transposed identically so releases match their presses.
   - **Velocity range** (inclusive) filters **sounding note-ons only**
     (`velocity > 0`). Note-offs bypass it so a note can always be released.
   - **Velocity-0 note-ons are normalized to `NoteOff`** (MIDI convention)
     and bypass the velocity filter.
4. **CC** — an **empty `cc_map` is transparent** (all CCs pass). A non-empty
   map is a **whitelist**: only mapped controllers pass, forwarded
   **unchanged** (CC7 stays CC7). The cc→param binding is rule metadata
   (`rule.param_for_cc(cc)`) that the destination applies — see integration
   below. Key/velocity ranges never apply to CCs.
5. **Poly aftertouch** presses one played key, so it takes that key's path:
   filtered by the key range and transposed like a note.
6. **Everything else** (pitch bend, program change, channel aftertouch, which
   is how wind controllers send breath) passes after the channel filter,
   untransformed.

**Stateless by design:** the matrix tracks no held notes. If rules change
while notes are held (range/transpose edits), a release can be orphaned —
the engine should send all-notes-off to affected destinations when it swaps
the table (integration note below).

## Examples

### Split at C4 (two destinations, disjoint ranges)

C4 = MIDI note 60. "Split at C4" = C4 and above go to the upper zone.

```rust
let mut m = Matrix::new();
m.add_rule(Rule::new(SourceId::Keys, surge_a).key_range(0, 59));   // bass
m.add_rule(Rule::new(SourceId::Keys, surge_b).key_range(60, 127)); // lead
m.route(SourceId::Keys, MidiMsg::note_on(0, 59, 100)); // → [(surge_a, …59)]
m.route(SourceId::Keys, MidiMsg::note_on(0, 60, 100)); // → [(surge_b, …60)]
```

### Layer (one key → two destinations)

```rust
m.connect(SourceId::Keys, surge_a);
m.connect(SourceId::Keys, pad);
// note-on 64 → [(surge_a, ev), (pad, ev)]  (insertion order)
```

### Split + layer + per-zone transpose (the task-9 acceptance patch)

```rust
m.add_rule(Rule::new(SourceId::Keys, surge_a).key_range(0, 59).transpose(-12));
m.add_rule(Rule::new(SourceId::Keys, surge_b).key_range(60, 127));
m.add_rule(Rule::new(SourceId::Keys, pad)); // full-range layer
// note 48 → surge_a at 36 (bass, octave down) + pad at 48
// note 72 → surge_b at 72 + pad at 72
```

### Channel filter + velocity range + CC map

```rust
m.add_rule(
    Rule::new(SourceId::Hardware(7), synth)
        .on_channel(2)          // only MIDI channel 3 (0-based 2)
        .vel_range(64, 127)     // soft touches dropped; releases always pass
        .map_cc(1, P_CUTOFF)    // mod wheel → cutoff param
        .map_cc(7, P_LEVEL),    // volume → level param; other CCs dropped
);
```

## Engine integration contract (for the lead)

- **Feeding:** decode raw bytes with `MidiMsg::from_bytes` (the `midi.rs`
  ring already delivers fixed-size records), tag with the right `SourceId`
  (`Hardware(port)` / `Virtual(port)` for ALSA-seq, `Keys` for the on-screen
  keyboard), call `route_into` per event. Sample-accurate scheduling stays
  in the engine: the matrix is a pure per-event function, so events can be
  routed at their exact sample offset within the block.
- **RT discipline:** reserve the output `Vec` once (`route_into` only clears
  and pushes); reserve rule storage with `Matrix::reserve`. Edit/swap the
  table off-thread at block boundaries, exactly like the `RenderPlan`
  (docs/ARCHITECTURE.md §7). No allocation, locks, or syscalls inside routing itself.
- **CC bindings:** delivered CC events are unchanged MIDI; look up the
  binding via `matrix.rules_from(source)` → `rule.param_for_cc(cc)` and
  apply it as a `SetParam { node: rule.dest, param, value }`-style update.
  MIDI-learn (P2) populates `map_cc` — the binding primitive is already here.
- **Rule edits while playing:** send all-notes-off (CC 123) to affected
  destinations when swapping a changed table, since the matrix is stateless.
- **Sessions:** `Matrix` is serde-ready. `session.rs` (task-8) keeps an
  opaque `matrix: Option<serde_json::Value>` slot — plug
  `serde_json::to_value(&matrix)` / `from_value` into it; ids and routing
  survive the round-trip (tested).

## Tests & verification

37 unit tests in `src/matrix.rs` cover: C4 split (incl. every note 0..=127
landing in exactly one zone), note-off split symmetry, layering and ordering,
split+layer combos, transpose up/down/clamp-high/clamp-low/`i8` extremes,
key-range-before-transpose, channel filter + omni + channel preservation,
inclusive velocity ranges, note-off velocity bypass, velocity-0
normalization, CC whitelist/transparent/binding-lookup/remap, CC ignoring
key/vel ranges, pitch-bend/program/aftertouch passthrough, bend encoding,
disable/re-enable, source isolation across all `SourceId` variants, rule
id/removal/live-edit, reversed-range normalization, determinism,
`route_into` buffer reuse (no realloc), wire decoding round-trips and
rejection of system/truncated input, serde round-trip, and the full
**two-Surge-layers-split acceptance** scenario.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
CARGO_TARGET_DIR=$HOME/.cache/ksalat-matrix-target cargo test matrix::
CARGO_TARGET_DIR=$HOME/.cache/ksalat-matrix-target cargo build
```

No new dependencies (serde/serde_json were already in `Cargo.toml`).