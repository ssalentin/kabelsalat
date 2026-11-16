# BRAVO (obra.art/bravo) — research report for kabelsalat

*Researcher: sub-agent, inspect-only. Date: 2026-11-16. Sources cited inline; every
claim is tied to a URL or to the kabelsalat source file/line, and places where BRAVO
does not document something are flagged explicitly as "not documented".*

---

## 1. Summary — what BRAVO is

BRAVO is OBRA's proprietary "AI DAW": a free beta for **Mac (Apple Silicon or Intel,
macOS 13.3+) and Windows 10/11**, marketed as "the Claude Code of DAWs" / "the studio
that works on its own". Core pitch: you open a chat (or drive it from your own AI),
say what you want in words ("master these 10 songs to −14 for Spotify"), and it runs
the audio job **locally on your computer** — mastering, stem separation, batch
processing, mix judgement, project "LOOK" and vocal tuning. It explicitly **does not
generate songs**; everything comes from audio you brought.

Sources: https://obra.art/bravo , https://obra.art/bravo/guides/ai-daw ,
https://obra.art/bravo/changelog .

**There is no public source repository.** We searched and followed links; the GitHub
hits for "bravo/ai-daw" are unrelated projects (openDAW/andremichelle, AMEOBIUS
`opendaw-mcp`, Nathandela `long-running-harness` aka BRUTALWAV, heshengtao/ai-daw,
and `ozzaii/bravoh-daw` which is the *different* "BRAVOH" OSS). **"The mixing/audio-graph
code and the agent/tooling code are not public** — everything below about BRAVO's
internals is inferred from its marketing/guide pages, not source. Where a detail
(e.g. the master-chain DSP, or the OB-1 model) is not documented, it is said so.

Key advertised numbers (https://obra.art/bravo): ~3,000 producers on OBRA; **672
actions** OB-1 can take in a project; **90+ studio modules**; builds "almost daily".

---

## 2. Mixing architecture — BRAVO vs kabelsalat

### 2.1 What BRAVO claims (documented)

- **Full DAW mixer.** "A mixer that hides nothing: automation lanes, groups, sends
  and returns, loudness and phase meters" (https://obra.art/bravo). Separate marketing
  also lists "Arrangement, Mixer, Polyphonic tune, Vocal tune" as the four surface views.
- **Latency compensation.** "An engine you can trust: latency compensated across the
  whole graph, the same for playback and for the bounce" (https://obra.art/bravo).
  I.e. **one render path** for realtime and offline bounce — a principle we already
  hold in PLAN.md §7 ("One render path for realtime and offline bounce").
- **Master chain.** "Its master chain (gentle glue and a limiter), sets the loudness
  for where it is going, keeps the true peak under −1 dBTP, writes a 24-bit WAV"
  (https://obra.art/bravo/guides/master-a-song). Loudness targets are named presets:
  Spotify/YouTube/Tidal −14 LUFS, Apple Music −16, club −8, or an explicit number.
  The exact DSP of "gentle glue + limiter" is **not documented**.
- **Loudness/measurement tooling.** `bravo_hear` renders the mix in memory and
  measures "loudness, true peak, dynamics, tone by band and the low end in mono"
  (https://obra.art/bravo/guides/mix-and-master-with-claude). Master results report
  "integrated LUFS and true peak, before and after" (master-a-song). So the meter set
  is: integrated LUFS, true peak, tone-by-band, low-end-in-mono, dynamics.
- **Batch + export.** "masters × 10 → files" album batches (one file per take, report
  alongside), 24-bit WAV or 16-bit WAV/FLAC; stem separation (vocals/drums/bass/other)
  and per-track stem export. Non-audio jobs are not mixed: "automatic stem mixing is
  not in the beta yet" — BRAVO "shapes what is there; it does not rewrite parts"
  (https://obra.art/bravo/guides/mix-stems).
- **Plugins.** "90+ studio modules … plus your VST3 and AU plugins" (obra.art/bravo).
  Module vs plugin architecture is **not documented** beyond this.

### 2.2 kabelsalat today (source)

- **Per-rig mixer.** `RigMix { vol, pan, mute, solo }` (src/engine.rs:340), applied at
  each rig's `MasterOut` when summing, constant-power pan (`gl = sqrt(1-pan)`,
  `gr = sqrt(1+pan)`), solo mutes non-solo rigs (engine.rs:1705-1712). `Command::SetRigMix`
  clamps vol 0..1, pan −1..1 (engine.rs:1231). Fixed 8 slots, no alloc on the audio path.
- **Master bus.** `master_vol` + `master_warm` (tanh, drive `1 + 3*warm`) after a
  zero-latency `MasterLimiter` (ceiling 0.7, no lookahead) — gain staging before colour
  (engine.rs:1804-1819, 1831-1858; AGENTS.md rule 49). `out_peak` is a **peak** meter only.
- **Publication.** `GraphPub` carries `mix` + `mix_rev`, bumped on `SetRigMix` via a
  non-blocking `try_lock` retried each block (engine.rs:1547-1558) — the mixer mirrors
  remote MCP/CLI/agent edits (AGENTS.md rule 33).
- **No sends/aux** (backlog #5), **no automation** (non-goal §5 as timeline automation),
  **no LUFS/true-peak/tone-by-band metering** — `get_status().output_level` is peak
  (mcp.rs get_status). Undo exists as a snapshot `History` (session.rs:694) but see §3.

### 2.3 Comparison

| Concern | BRAVO | kabelsalat |
|---|---|---|
| Bus structure | tracks → groups → sends/returns → master | source/voice → rig bus → MasterOut (sum); no send/return |
| Gain staging | master "glue + limiter" (details unknown) | `MasterLimiter` → `tanh` warmth (explicit, rule 49) |
| Metering | LUFS-I, true peak, tone-by-band, low-end-mono, dynamics | peak (`output_level`) only |
| Loudness targets | named presets + numeric LUFS | none |
| Automation | lanes (documented) | non-goal |
| Latency comp | "whole graph, playback==bounce" | serial chains align naturally; full comp in backlog |
| Offline/batch | album batch, stem export, "bounce == playback" | record master (record_start/stop), no batch |
| Plugins | 90+ modules + VST3/AU | CLAP (Surge) + LV2 + 8 DSP nodes |

**Net:** BRAVO is a *timeline DAW* with a mastering/loudness job layer on top; kabelsalat
is a *live rack* with a per-rig mixer. The transferable pieces are not the arrangement
surface but the **measurement + "hear the result" loop**, the **one-undo-per-job**
framing, and the **loudness-normalized master target** pattern.

---

## 3. Agent architecture — BRAVO vs kabelsalat

### 3.1 BRAVO (documented)

- **Verb protocol, not a raw tool dump.** The DAW exposes four moves — **LOOK**
  (read project in one call), **CAN** ("what can I do with this?" → exact actions),
  **DO** (run a list of actions **as one undo step**, report what changed and what
  *could not* be done with reason), **ASK** (plain-text answer; a question may still
  mutate, as one undo). A fifth, **HEAR**, renders + measures + judges
  (https://obra.art/bravo/guides/ai-daw , mix-and-master-with-claude).
- **13-tool connector (MCP).** `bravo_connection, bravo_look, bravo_can, bravo_do,
  bravo_ask, bravo_hear, bravo_master_file, bravo_screen, bravo_state, bravo_catalog,
  bravo_verb, bravo_wait, bravo_project` (mix-and-master-with-claude). Note the shapes:
  - `bravo_catalog` — the full list of actions *with their arguments* (the model can
    discover the surface without guessing).
  - `bravo_can` — capability discovery *scoped to the current project/selection*.
  - `bravo_verb` — "runs ONE action through the checked path" (arg validation).
  - `bravo_wait` — block until the project changes, instead of polling.
  - `bravo_screen` — a screenshot of the window for the model.
  - `bravo_state` — whole project as data ("the details bravo_look leaves out").
- **State representation given to the model.** `LOOK` returns "tracks, their chains
  and clips, tempo, chords and what is selected"; `STATE` returns everything as data.
  The connector returns **text only** — "project state, measurements and results, and
  a picture of the window if Claude asks" (mix-and-master-with-claude). Audio is never
  uploaded; the model "hears through measurements and renders, not with ears."
- **Apply/preview/undo.** Every `DO` (a *list* of actions) is one undo step; versions
  are "signed with its name" (obra.art/bravo). Undo is a first-class, always-available
  affordance ("Every step is one undo", "Listen, keep or undo").
- **Multi-agent.** "A director and its musicians — one plans the song, up to six write
  the parts" (obra.art/bravo). Internals **not documented** (how they coordinate, and
  whether the "musicians" are sub-prompts, sub-agents, or sampling workers, is unknown).
- **Skills.** "Do it once, type @skill, it's yours" — record a manual multi-step
  procedure (e.g. "tune lead, add a 3rd/5th up on new tracks, lock doubles, my vocal
  chain") and replay it on any project; "One undo each" (obra.art/bravo). Turned into a
  named, replayable unit.
- **Model/provider.** Provider-agnostic: "Works with Claude, Codex and Cursor"; the
  user brings their own model, the connector exposes the same verb set regardless of
  which assistant asks (ai-daw). OBRA's own built-in "OB-1" is a separate, OBRA-hosted
  model with a daily allowance (larger with OBRA PRO). Local audio jobs don't touch AI.
- **Safety.** Connector is **off by default**, listens only on `127.0.0.1`, uses a
  private token regenerated at each launch (file readable only by the user), and asks
  for **per-project consent** on first touch with "Revoke all". Connected apps cannot
  record mic, open other projects, change BRAVO settings, or open plugin windows
  (mix-and-master-with-claude). These are explicit documented constraints.

### 3.2 kabelsalat today (source)

- **In-app assistant** (src/assistant.rs): DeepSeek BYOK, short system prompt, one
  worker thread, `tools()` builds **~39 flat function definitions**, `tool_choice:"auto"`,
  sequential tool loop with a 30s per-result timeout. State given to the model is the
  system prompt + `get_graph` (JSON nodes/edges), `get_rigs`, `get_status` (peak level),
  `get_matrix` — text/JSON only, no audio, no screenshot for the model.
- **MCP server** (src/mcp.rs): the same control plane re-exposed (`get_graph,
  add_node, connect, set_rig_mix, set_matrix_rule, get_matrix, add_rig, delete_rig
  (confirm), …`), with **1:1 agent parity** enforced as AGENTS.md rule 6, 25; routing
  edits update the UI mirror (rule 25); engine publishes mix/rules back (rule 33).
  Validation happens before command send (e.g. `validate_connect` mcp.rs:645,
  `refresh_remote` mcp.rs:495) — the "checked path" analogue.
- **Confirmation** exists only for destructive `delete_rig` (`confirm=true`, rule 23).
- **Undo** is implemented (`History`, session.rs:694, 128 steps, `undo`/`redo`/`can_undo`)
  but snapshots "graph + params + matrix" only (session.rs:25, 690) and is **not wired to
  the agent or MCP** — no `undo` tool in `assistant::tools()` or mcp.rs, and no "one step
  per job" grouping. Inference: the agent cannot roll back its own edits.
- **No** `can`/`catalog` capability-discovery tool, no `hear`/measure tool (only
  `get_status().output_level`), no `wait`, no `screen`-style tool, no multi-agent, no
  skills.

### 3.3 Comparison

| Concern | BRAVO | kabelsalat |
|---|---|---|
| Tool surface | 13 verbs incl. LOOK/CAN/DO/ASK/HEAR + catalog | ~39 (assistant) / 40+ (MCP) flat tools |
| State to model | tracks/chains/clips/chords/selection as data + measurements | graph JSON + rigs + peak level |
| Closed-loop check | HEAR render+measure+judge (score /100, fixes as actions) | none (peak level only) |
| Undo | every DO/ASK = one undo, signed versions | snapshot `History`, **not agent-facing**, no grouping |
| Confirmation | per-project consent + off-by-default + token | `confirm` only on delete_rig |
| Multi-agent | director + ≤6 musicians (undocumented) | single agent |
| Skills | @skill procedural capture/replay | none |
| Model/provider | user's Claude/Codex/Cursor + OBRA OB-1 | DeepSeek BYOK |

---

## 4. Ranked best-practice items worth adopting

Ranked by value-to-effort for a *live rack* (not a DAW). "Roadmap": the mapping refers to
PLAN.md §13 items — #1 backing tracks, #4 CC maps/MIDI-learn, #5 sends/aux, #6 recording,
#7 tap tempo/count-in, #8 packaging — plus "new" for things not yet on the roadmap.

### 1. A "hear the result" measurement loop (HEAR) — **M**
- **What:** add an in-memory render + measurement tool to the engine: integrated LUFS,
  true peak, low-end-in-mono, and a small tone-by-band summary; expose as `measure_mix`
  (MCP + assistant) and feed the numbers back into the model's tool result so it can
  self-correct ("too loud under 120 Hz → cut 2 dB, measure again").
- **Why:** this is BRAVO's single clearest differentiator — the model "hears through
  measurements and renders" (mix-and-master-with-claude) and every job reports
  loudness/true-peak before+after (master-a-song). Our `get_status().output_level` is
  only a peak meter, so our agent is *deaf* to loudness/balance/mono — it cannot judge
  or verify its own mixing.
- **How:** implement a block-level loudness accumulator (K-weighting + gated
  integrated LUFS; true-peak via 4× oversampled peak) on the audio thread and/or an
  offline `render_to_buf` that feeds the same `GraphEngine::render`. Publish into
  `GraphPub`/`get_status`. Add `measure_mix` to `assistant::tools()`, `exec_tool`, and
  mcp.rs (parity rule 6). A rule-based "judge" (muddy ≈ energy 200-300 Hz vs reference,
  too-wide-under-120 Hz, score /100) is a thin add on the classifier side.
- **Roadmap:** new (extends the planner of #6 Recording and the master chain in rule 49).
- **Size:** M (DSP meters are moderate; true-peak + gating is well-trodden).

### 2. "Every job is one undo" + an agent-facing `undo` tool — **M**
- **What:** group a single agent task's tool sequence into one undo unit and expose
  `undo`/`redo`/`can_undo` to both frontends.
- **Why:** BRAVO frames every `DO` (a *list* of actions) as **one undo step**, versions
  signed with the agent's name (obra.art/bravo), and the workflow is "listen, keep or
  undo". We already built the primitive — `History` in session.rs:694 (128 steps) — but
  it is not wired to the agent/MCP and snapshots only graph+params+matrix. Rule 41
  already makes routing edits release held notes first; group-level undo makes an
  agent's mis-wire recoverable in one gesture instead of N reverse tools.
- **How:** expose `undo`/`redo` in `assistant::tools()` + mcp.rs; record a `History`
  snapshot *before* an agent's first mutating call per task and treat the task as the
  commit boundary; extend the snapshot to include `GraphPub.mix` (per-rig mix) so
  mixer changes roll back too. Reuse the existing `History`/`record`/`undo`.
- **Roadmap:** new (undo was a P1 mention in PLAN §5 but never shipped to the agent).
- **Size:** M (mostly wiring; the stack already exists and is tested).

### 3. Capability discovery: `can` + `catalog` — **S**
- **What:** two cheap read-only tools: `catalog` (every action + its args) and `can`
  (the actions available *for the current rig/project/selection*), plus a `wait` for
  "signal me when the project changes".
- **Why:** `bravo_catalog` ("list of actions with their arguments") and `bravo_can`
  ("what can I do with this?") are how BRAVO keeps its 672-action surface from
  overwhelming the model and stops it hallucinating verbs. Our surface is smaller but
  grows monthly; a catalog + scoped `can` keeps the model honest and is trivially
  derived from what we already have (tools()/get_rigs/get_graph). `bravo_wait` replaces
  poll-loops, which our model otherwise emulates with repeated get_graph calls.
- **How:** derive `catalog` from `tools()`; `can` = filter by the rig in view
  (rig_snapshot.active + nodes) — mostly a JSON projection of `get_graph`/`get_rigs`.
  Add to both frontends (rule 6).
- **Roadmap:** new (extends the §2c "tool catalog documented and versioned" idea).
- **Size:** S.

### 4. Loudness-normalized master target — **S**
- **What:** a `set_master_target(t): Spotify|YouTube|Tidal(−14)|Apple(−16)|club(−8)|N` that
  drives the master stage to a target integrated LUFS with true peak ≤ −1 dBTP.
- **Why:** BRAVO's core promise is "master at the loudness you ask for" with named
  presets and a before/after readout (master-a-song). We already have a master chain
  (`MasterLimiter` + warmth, engine.rs:1804) but no target concept — this is a small,
  high-polish feature that reuses item #1's meters and makes "make me show-ready loud"
  a one-liner for the agent.
- **How:** after #1 lands, compute measured LUFS each block (or offline) and adjust
  `master_vol`/limiter ceiling toward the target. Expose `set_master_target`/`get_master_measure`
  in both frontends (parity).
- **Roadmap:** new; sits on #1. Optional short path: reuse record/session plumbing.
- **Size:** S (once #1 exists).

### 5. Sends / aux buses, made agent-legible (`set_send`) — **M**
- **What:** finish the already-queued sends/aux work (§13 #5) with an explicit
  `set_send(rig|node, bus, amount)` tool and per-bus metering, rather than raw
  `connect` graph surgery as the only path.
- **Why:** BRAVO's "sends and returns … loudness and phase meters" and "one render path
  for playback and bounce" treat sends as a first-class mixing act, not free wiring.
  For kabelsalat the graph already routes freely (rule: "free wiring already works"),
  but a dedicated `set_send` tool keeps the agent from having to invent bus nodes via
  `add_node`+`connect`, and gives send levels a defined, mirrorable location (§13 #5;
  rule 25 routing-mirror).
- **How:** when #5 is built, add the send as a typed `Command` + `GraphPub` field, then
  `set_send`/`get_sends` in both frontends (rule 6).
- **Roadmap:** extends #5 (sends/aux).
- **Size:** M (engine + UI + tools).

### 6. @skill procedural capture/replay — **M**
- **What:** record a named sequence of agent tool calls (and their resolved ids) once,
  store it (session-adjacent), and replay it against any rig, with one undo.
- **Why:** BRAVO's "@skill: do it once, it's yours — it runs your procedure on any
  project, one undo each" (obra.art/bravo) is a genuinely reusable pattern for us:
  "my vocal chain", "my reverb bus", "split bass + pad" become one-token recipes the
  agent or user can invoke. It compounds the value of rules 6/25 (parity + mirror) since
  a skill is just a macro over the same control plane.
- **How:** a small `Skill = Vec<Command>` store in `session`/`assistant`, `@skill` parsing
  in the input, `run_skill(name, rig)` tool. Reuses `Command` serialization already in
  control.rs.
- **Roadmap:** new (complements templates/load_template).
- **Size:** M.

### 7. Per-project attach consent for MCP `--attach` — **S**
- **What:** when `kabelsalat mcp --attach` connects to the live app, require an explicit
  one-time consent in the UI (and a "revoke all") before an external agent may mutate the
  project.
- **Why:** BRAVO's connector is off-by-default, localhost-only, token-gated, and asks
  per-project consent with "Revoke all" (mix-and-master-with-claude). Our `--attach` already
  proxies to the live app over a unix socket (mcp.rs Backend::Remote), but has no consent
  gate — an agent attached to a live set can mutate the user's running graph with no prompt.
  AGENTS.md rule 23 already requires confirmation for destructive *rig* actions; extending
  the same instinct to "an external process may attach" matches the user's safety bar.
- **How:** a consent handshake in `bridge.rs` (`--attach` returns a challenge; UI shows
  "Allow agent access?" once per attach; store + allow revoke).
- **Roadmap:** new (extends MCP §2c / onboarding).
- **Size:** S.

---

## 5. Things BRAVO does that we should NOT copy

- **The timeline/arranger/piano-roll/notation/video surface.** Already our non-goals
  (PLAN §5). BRAVO is a DAW; kabelsalat is a live rack. Don't adopt the arrangement or
  clip paradigm.
- **Stem separation (ML).** "four stems from any song" is an ML model running locally
  (and, for the web OBRA Mastering, in the browser). Heavy, off-scope for a rack, and
  orthogonal to live performance. (Also: not open to inspect.)
- **Automatic "one-click mix/edit the song" and OB-1's opaque DJ/fixer behaviours.**
  "It shapes what is there" but "automatic stem mixing is not in the beta yet" — even
  they gate it behind listening tests. We should stay with *user-driven* routing; the
  value transfer is measurement-and-verification, not black-box mix decisions.
- **Cloud-hosted built-in model (OB-1) with a daily allowance.** Our BYOK local
  DeepSeek already fits the user's privacy bar better (key 0600, never committed,
  AGENTS.md/PLAN). Don't introduce a cloud allowance model.
- **Multi-agent director + six "musicians".** Undocumented, high complexity, and aimed
  at *composing parts* — our product never generates material. Defer unless a concrete
  live-performance use emerges.
- **Reading other DAWs' projects (Ableton .als / FL .flp) to "learn your sound".**
  Not applicable to a Linux rack; also a privacy/parsing rabbit hole. (Note: the
  `bravoh-daw` Rust crate is the *different* "BRAVOH" project, not BRAVO.)
- **Proprietary macOS/Windows distribution + usage telemetry (beta terms mandate it).**
  We're Linux, local, no telemetry — keep it that way.
- **"The AI never *sees* your audio — it hears through measurements" as an *upload-avoidance*
  stance.** Keep the principle, but note our CLI/headless already renders real audio, so we
  *can* give the model richer (local, never-uploaded) acoustic evidence than BRAVO's text-only
  connector — item #1 exploits exactly that.

---

## 6. TL;DR (5 lines)

1. BRAVO is a proprietary Mac/Win "AI DAW" with **no public source**; its mixing (per-track
   chain → groups → sends/returns → "glue + limiter" master with loudness targets) and its
   agent (LOOK/CAN/DO/ASK/HEAR verbs, 13-tool localhost MCP, one-undo-per-job, @skill,
   per-project consent) are known only from marketing/guide pages.
2. Its most valuable, transferable ideas for kabelsalat are the **measure-then-hear loop
   (LUFS/true-peak/tone/mono + a rule-based judge)**, **one-undo-per-agent-job**, and
   **capability discovery (`can`/`catalog`)** — all three map cleanly onto our existing
   control plane and parity rules (AGENTS.md 6/25/33).
3. Ours already outperforms it in places it can't copy: real local rendering, a typed
   command/query control plane shared 1:1 by UI + MCP + agent, per-rig mixer with published
   mirror, and BYOK local inference — none of which BRAVO documents.
4. Recommend adopting in order: (1) HEAR/measure mix, (2) agent-facing undo, (3) `can`+`catalog`,
   (4) master loudness target, (5) `set_send`, (6) @skill, (7) attach consent — sizes S–M,
   largely extending roadmap #5/#6 and new items, no L-sized bets.
5. Do **not** copy the timeline/arranger, ML stem separation, black-box auto-mix, cloud OB-1
   allowance, multi-agent "musicians", foreign-DAW import, or proprietary/telemetry packaging.

Report path: /home/sebastian/.cache/ksalat-research/bravo-report.md