# kabelsalat — architecture and decisions

**An AUM-style live plugin host for Linux, in Rust — where the cables are the show.**

This document explains how kabelsalat is built and why. It is not a task list:
planned work, bugs and ideas are tracked as
[GitHub issues](https://github.com/ssalentin/kabelsalat/issues) grouped into
[milestones](https://github.com/ssalentin/kabelsalat/milestones), released
changes are in [CHANGELOG.md](../CHANGELOG.md), CI and release packaging in
[PACKAGING.md](PACKAGING.md), and the working rules for contributors and
agents are in [AGENTS.md](../AGENTS.md).

## Module map (src/)

- `control.rs` — the typed control plane: `Command` (one API for UI, assistant,
  CLI and MCP), `Graph`/`Node`/`Edge` (nodes carry their `rig`), `Rig`,
  `RigSnapshot`, `merge_rigs`/`split_rigs`, the lock-free `command_bus()` (rtrb).
- `engine_info.rs` — engine load: `EngineCounters` (the audio callback's
  atomics, read once), `NodeTimes` (lock-free render time per unit) and the
  `EngineStats` snapshot the DSP readout, Engine Info and `get_engine_info`
  all show.
- `engine.rs` — `GraphEngine`: voices → DSP → LV2 → per-rig mix → master
  limiter → master saturation. Sums every MasterOut (one per rig). Publishes
  `GraphPub` (graph, rules, mix, voice binding) and edge levels to the UI; the
  render path is allocation- and lock-free, structural ops apply at block
  boundaries. One voice per Clap node, any number: the engine binds voices
  to nodes (`bind_free_voices`: graph order, lowest free voice id of the
  node's plugin) and publishes the binding; the voice host sends voices,
  larger tables and retire requests through `VoiceMsg` and frees what comes
  back through `VoiceBack`, so the audio thread never allocates or frees
  for a voice.
- `arp.rs` — the arpeggiator / chord unit and the Keys scale snap (issue #90).
  `ArpState` is one fixed-size state per `NodeKind::Arp` node: held keys
  (reference counted), the generated notes that are down, and a step clock in
  SONG-CLOCK BEATS (the engine feeds `tick` the transport position while the
  master plays, a free-running phase at the same BPM otherwise; rule 43). The
  unit sits between a Keys source and the synths like an effect:
  `Graph::wire_arp` replaces the rig's Keys → synth MIDI cables by Keys → arp
  → synth. Modes up / down / up-down / random step the pattern (rate 1/4 …
  1/32 incl. triplets, octaves 1–4, gate, swing); chord mode sounds one
  triad / 7th / power chord per held key and holds it with the key. Every
  routing edit that can change an arp note's off path releases through
  `GraphEngine::release_all`, which cascades into what the arp feeds
  (`engine/held_notes.rs` checks it). Scale snap is a property of the Keys node
  (`Node::scale`, `Command::SetKeysScale`): `snap_note` is a pure function of
  the played note, so a release meets the pitch its note-on snapped to.
  Knobs live in `engine::knob_specs`; `knob_values_from_args` is the one
  parser of the `set_arp` agent/MCP tool. `kabelsalat arp-test` renders two
  bars through a real Surge voice.
- `host.rs` — CLAP via clack-host: `ClapVoice` (audio side) + `ClapVoiceMain`
  (main thread), `VoiceController` (RT-safe notes/params), out-of-process
  plugin scan with a cache, Surge/Dexed patch files.
  plugin scan with a cache, Surge/Dexed patch helpers. The app is the voice
  host (`DemoApp::sync_clap_slots`, on the UI thread = the plugins' main
  thread): it instantiates a voice for every CLAP unit the engine has none
  for and retires voices no unit plays. Headless `kabelsalat mcp` hosts
  only its two startup voices. What a plugin asks of the host is a flag it
  may raise from any thread; `ClapVoiceMain::service` serves the flags on
  the main thread (every UI frame through `pump`, every 33 ms in headless
  `mcp`): `request_callback` gets one `on_main_thread()`, `request_flush`
  is served by the next `process()` or, for an inactive plugin, by
  `params.flush()`, and `request_restart` moves the voice's `Live` set
  (processor, buffers, parameter ranges) to the main thread, which
  deactivates, activates and sends a new set. `start_processing` and
  `stop_processing` run on the thread that renders. The test plugin in
  `src/host/probe.rs` records the thread of every call.
- `lv2.rs` — hand-written lilv FFI; LV2 effects and instruments, presets,
  MIDNAM programme names, loader thread, `LILV_LOCK`.
- `plugins.rs` — what kind of plugin each one is. Every built-in unit, CLAP
  and LV2 plugin becomes one classified `Entry` (`PluginType`: instrument,
  effect, MIDI FX, utility; `PluginSubtype`: Synths, Organs, Delay & Reverb,
  Dynamics, Analyzers, …). LV2 plugins are sorted by the scan's instrument /
  MIDI flags (rule 72) and then their lv2core class, CLAP plugins by their
  declared features. The Plugins stage, the Mixer's insert picker, the in-app
  assistant and MCP `list_plugins` all filter this one list
  (`PluginListFilter`), so they never disagree about a plugin. Both CLAP
  instruments and CLAP audio effects / utilities can be hosted (as rack units,
  strip inserts, aux bus inserts and master inserts); CLAP note effects are
  listed and marked as not hostable yet.
- `sounds.rs` — one sound library per instrument a rig can hold. The
  instrument (`Instrument`: a CLAP id or an LV2 URI) is the key and
  `Instrument::source` the table: Surge `.fxp` patches, Dexed DX7 cartridge
  voices, an LV2 plugin's `pset:Preset` files plus the MIDI programmes its
  MIDNAM document names (setBfree), or a plugin's own stepped "Preset"
  parameter (YANES). Aeolus has no presets and starts with every stop off:
  `aeolus.rs` holds its organ's stop table and a set of registrations, each
  written as the plugin's JSON state; a freshly instantiated Aeolus gets the
  default registration in `ClapVoice::instantiate_entry`. Its stops are no
  parameters, so the Aeolus face's stop panel (`aeolus_rects`) changes one
  stop by taking the plugin's state, changing that stop (`aeolus::with_stop`)
  and loading it back; it reads what is drawn (`aeolus::drawn`) once a
  second and after each change. Each `Sound` carries its `Load` (plugin state, parameter
  value, LV2 preset, programme change), and `load` sends each along its own
  path. `Libraries` scans on a thread of its own and keeps the results;
  `search` / `find` are the one search for every library, and the
  `list_sounds` / `load_sound` tools of the app, MCP and the Jev plan all
  answer from here.
- `main_controls.rs` — the few parameters a plugin's designer marked as most
  important: one `MainControls` list per plugin, the rule that finds it
  (`resolve`), knob labels, and name lookup for agents and CC bindings.
- `dsp.rs` — built-in processors (gain, pan, EQ, filter, limiter, saturation,
  delay, reverb, metronome). A processor reports its latency
  (`Processor::latency_frames`; the Limiter's 5 ms lookahead).
- `engine/pdc.rs` — delay compensation, see "Delay compensation and the
  analysis harness" below.
- `analysis.rs` / `engine/probe.rs` / `engine/analysis_test.rs` — the audio
  analysis harness (issue #13), same section.
- `matrix.rs` — MIDI routing: sources × destinations, per-cell rules (channel,
  transpose, key/velocity range), splits, CC maps; allocation-free `route_into`.
  A CC binding carries a `CcScale`: a stepped control (a switch, an LED
  drawbar) lands on the target's own steps.
- `instruments.rs` — instrument profiles: what every controller of a hardware
  instrument means. A profile is a [MIDI Guide](https://midi.guide) CSV
  (`instruments/*.csv`, compiled in, plus `~/.config/kabelsalat/instruments/`)
  with one more column, `role` (`drawbar.upper.3`, `rotary.speed`, …).
  `target` is the one table of which rack parameter a role drives. The app
  gives each hardware port a profile (picked in the matrix, or matched by the
  port name) and keeps the matrix CC bindings in line with it
  (`DemoApp::sync_instrument_maps`). Agents pick it too
  (`list_instrument_profiles`, `set_device_instrument`, #56); the app and
  headless MCP share `instruments::wanted` / `changes` / `listing`, so both
  bind and report the same. Headless MCP opens no MIDI ports: there a profile
  is set per device index and lasts for the server.
- `transport.rs` — song clock, tempo, time signature, count-in, tap tempo.
  The song position stays in quarter notes; `TimeSig` (#87) only decides the
  bar length (`bar_beats`) and the beat the metronome clicks (`beat_len`).
  Everything that waits for a bar asks `next_bar_beats`, and the CLAP clock
  reads the signature from `Transport::sig`. MIDI clock has no signature
  message (24 clocks per quarter, song position in sixteenths), so
  `clocksend.rs` is untouched by it.
- `clocksend.rs` / `alsaseq.rs` — MIDI Clock Send (#85): kabelsalat as the master
  beat clock. The engine publishes its song position and a monotonic stamp every
  audio block (`ClockPos`); the clock thread derives the 24-ppqn grid from that
  and hands each tick to an ALSA sequencer queue with an absolute time (lookahead
  200 ms), so the kernel delivers it, not a sleep loop and never a UI frame.
  `alsaseq` is a thin binding to `/dev/snd/seq` (no libasound); a queue's stamps
  count from its own start, so it calibrates that epoch once. Start goes out
  when the song starts from the beginning; Continue (after an optional Song
  Position Pointer) when one starts from a position. A negative net offset (sync
  offset plus the latency mode: none, output, input) pre-rolls the engine's
  start. Wiring is a row in the matrix view (one cell per MIDI out port, by name,
  rule 58); settings and wiring are app preferences in config.json, not session
  content, so loads and undo never touch them. Check:
  `kabelsalat clock-test` (virtual ports only).
- `record.rs` / `player.rs` / `capture.rs` / `midi.rs` — master recording to
  WAV, WAV backing-track player, system-audio capture, MIDI I/O and hot-plug.
  The player decodes on its own thread; the engine owns its clock and reads the
  file at the song position (loop, tempo sync as varispeed, Number of Beats,
  Beat Offset, start at the next bar), so a synced loop cannot drift.
- `meter.rs` — master loudness meter for the agent's `measure_mix`: momentary,
  short-term and gated integrated LUFS (ITU-R BS.1770-4, non-overlapping 400 ms
  gating blocks, integration window ≈ 13.6 min), 4× true peak, low-end mono
  check, 4-band summary; allocation-free, published via `GraphPub.meter`.
  It also holds the master's loudness target (#12, `MasterTarget`):
  `set_master_target` makes the engine move a trim in front of the master
  limiter by the difference between the target and the integrated loudness
  measured so far (`matched`, a pure function every frontend shares), then
  restart the meter so the next reading is the "after". A step is sized by
  the loudness the previous step yielded per dB, and stops when the limiter
  gives nothing back. A guard after the chain turns the output down by any
  true-peak excess over −1 dBTP (after the fact: the master has no
  lookahead). Trim and guard are exactly 1.0 without a target. The state is
  published as `GraphPub.master_target` and saved with the session.
- `listen.rs` — the audio judge behind the agent's `listen` tool (#35): an
  armed master tap (pushes only while a listen records), the clip sent as mono
  16 kHz WAV with the meter's numbers to an audio-capable OpenRouter model
  (`agent_listen_model`, default Gemini 3.8 Flash, `reasoning: low`), which
  answers in text (0-10 per attribute plus what it heard). The acting model
  and Jev read that text, never the audio (rule 82). `listen-test` checks the
  model ranks known reference clips (real Surge patches) the right way round.
- `ui.rs` / `theme.rs` — eframe/egui app: rack with physics cables and flip,
  rigs strip, mixer, matrix, sessions, plugin browser, native plugin editors,
  bottom dock (keyboard or assistant); `theme.rs` is the single palette source
  (design spec: [design-reference-analysis.md](design-reference-analysis.md)).
- `bridge.rs` — unix socket `$XDG_RUNTIME_DIR/kabelsalat.sock` between the
  running app and `kabelsalat mcp --attach` / `kabelsalat send`.
- `mcp.rs` / `assistant.rs` — MCP server (headless or attached) and the
  built-in assistant; both expose the same tools (AGENTS.md rule 6). The
  assistant's worker asks a decisions model first (`SHORTCUTS`: one row per
  request it settles, each a plan of tool calls) and hands the rest to a chat
  model. `llm.rs` is the one client and the route per role (decide, act).
  The agent takes text only: there is no image input.
- `session.rs` — sessions (multi-rig), templates.

## 0. Decision log

- **2026-11-18** — **MIDI strips (AUM parity, #86).** A MIDI effect is a graph
  node (`NodeKind::MidiFx`: a built-in transposer, or an LV2 plugin that has a
  MIDI atom in and out and no audio port) whose output is a matrix source
  (`SourceId::MidiFx(node)`). Notes travel source → unit → instrument: the
  engine's `pump_midi_fx` feeds each unit's bounded queue into its plugin
  before the voices render and routes what it emits through the matrix, with
  no allocation on the audio thread (LV2 instances load on the loader thread
  like any LV2 unit). Adding a unit re-points the rig's Keys → instrument MIDI
  cables through it; removing one heals them. Because a unit can change what
  its own note-off means (a transposer's knob), every edit of a unit's path
  releases what is held through it first (rule 41; checked by the
  `Alphabet::Fx` held-notes search and, with a real plugin, by
  `kabelsalat midifx-test`). CLAP note effects are discovered
  (`list_midi_fx`, `clap_not_hostable_yet`) but not hosted yet.

- **2026-11-17** — **Instrument profiles, not per-knob learn** (user: map a whole
  Nord Electro 2, drawbars included). Other hosts do this with a per-device
  definition: Logic's Vintage B3 ships controller presets per organ (one is
  the Nord Electro 2's), Bitwig and Ableton load a controller script per port,
  Mixxx keeps XML mappings matched by device name, and Cakewalk/REAPER read
  instrument definition files that name every CC. kabelsalat reads the open
  MIDI Guide CSV format (one file per device, the community dataset) and adds a
  `role` column, because the dataset names controls freely ("Drawbar 1" on a
  Stage 3, "16' upper drawbar" in the Electro 2 manual) and names alone cannot
  say what a control does. The profile binds through ordinary matrix CC
  bindings, so sessions, the mixer mirror and agents see nothing new. A DIN
  instrument sits behind a USB interface whose port name hides it, so the
  user picks the profile per port; a USB instrument is matched by its name.

- **2026-11-14** — Original plan was a GarageBand-style DAW; pivoted after the user
  clarified the real need: an [AUM](https://kymatica.com/apps/aum)-like live host —
  wire plugins and audio sources together, route MIDI anywhere, play live.
- **2026-11-14** — Installed and tried Element 1.2.0 (+ Carla, Surge XT VST3, LSP LV2)
  as the existing baseline. Verdict: *"really outdated UI"*. Decision: **build our own**,
  with a modern interface as a first-class requirement.
- **2026-11-14** — Name chosen: **kabelsalat** ("cable salad"), knowingly accepting
  existing projects with the same name (felixroos/kabelsalat JS audio graphs,
  ThePixelPilgrim/kabelsalat Rust terminal, a macOS Kabelsalat VST). User's call.
- **2026-11-14** — **Animated wiring promoted to signature feature** (user request).
- **2026-11-14** — **GarageBand added as second muse** (user request): approachability,
  instant sound, smart controls, playful feedback (§2b).
- **2026-11-14** — **System audio capture requested** (user): wire YouTube/browser
  playback into the mix as a source node (P2, §7).
- **2026-11-14** — **Slick, effortless UI + full MCP server** (user request): ease of
  use is a top-level requirement; a built-in MCP server must expose every feature
  1:1 through the same control plane the UI uses (§2c).
- **2026-11-14** — **Embedded plugin editors required** (user): no scattered plugin
  windows — editors open inside the host window (§6). Host runs under XWayland so
  X11 reparenting works. *Superseded 2026-11-17:* editors are native egui
  parameter views, and the app runs as a native Wayland client.
- **2026-11-17** — **Plugin GUIs in their own windows** (user): the plugin's
  own editor opens on request ("Open plugin window" in the editor pane) as a
  separate XWayland dialog window that kabelsalat creates and the plugin
  embeds into. The main app stays a native Wayland client; the native egui
  pane stays the default editor.
- **2026-11-14** — **Reason's rack flip adopted as interaction model** (user):
  labeled rack units, Tab flips to the back panel where the cables live (§2a).
- **2026-11-14** — **Built-in assistant requested** (user): DeepSeek-powered, BYOK,
  tool access to the app (rewire/setup/fixes). Same control plane as MCP (§2c);
  key stored locally 0600, never committed. Shipped in the dev preview.
- **2026-11-14** — **AUM operating model reaffirmed** (user): multiple independent
  rigs, each taking input from a different MIDI device or audio source, freely
  wired and mixed. Per-rig input selection is core (§1).
- **2026-11-15** — **Design reference corrected to Hurricane** (user): the Signal
  dark-glass pass had been specified without seeing the user's Dribbble link (bot
  wall). User pointed at the Hurricane Creative Agency landing page; DeepSeek vision
  analysis → light editorial language (§0.1, docs/design-reference-analysis.md).
- **2026-11-15** — **Agent teams + DeepSeek vision adopted** (user request): work runs
  as analyst → implementer → lead; visual facts come from DeepSeek-V4.1-Flash via the
  app's BYOK key, not from guessing.
- **2026-11-15** — **Single roadmap in PLAN.md** (user): all work tracking lived in §13
  of PLAN.md — no GitHub tickets; BACKLOG.md merged there.
- **2026-11-16** — **Roadmap moves to GitHub** (user): PLAN.md is split — planned
  work, bugs and ideas become GitHub issues with milestones (#11–#27 created from
  §13), this document keeps architecture and decisions, the working guide moves
  to AGENTS.md. One place per item, never mirrored. MIT license, v0.1.0 tagged.
- **2026-11-15** — **UI evolution pass** (user): crossed-wires logo used in-app
  (masthead/help/icon), real chip buttons instead of underlined text, multi-color
  cable jackets (family-safe palettes), then **all-dark chrome** ("the whole
  background dark, not white"), matrix as a main-stage view instead of a window,
  assistant as a toggleable wide bottom panel, watermark removed. Spec overrides:
  docs/design-reference-analysis.md §0.
- **2026-11-15** — **Evidence + rules** (user): AGENTS.md requires screenshot
  proof for every visual change, and every fixed error becomes a written rule
  so it is never repeated.
- **2026-11-15** — **Orchestrator mode** (user): the lead briefs/delegates/reviews/
  verifies; implementation runs in Claude subagents (pstack model sheet at
  ~/.config/t3-pstack/models.md), not in the lead's own edits.
- **2026-11-15** — **Production DAW direction** (user): start/stop must be a
  property of each rack instrument that supports it (metronome, audio
  playback) — per-node transport, not only a global one. The big "flip rack ·
  Tab" pill is removed (Tab lives in the help overlay); the help overlay gets
  a redesign; the masthead logo must be dezent (≤16px, left of the wordmark).
- **2026-11-15** — **Split is a keyboard property** (user): the "Split @ C4"
  chip leaves the top strip; split becomes a property of the Keys (MIDI
  keyboard) unit, and a split keyboard exposes TWO MIDI outs (low/high zones)
  that are wired to rigs like any other ports — explicit graph routing
  replaces the global auto-split.
- **2026-11-15** — **Tempo belongs to the metronome** (user): the Transport
  menu/window is removed; BPM is set on the metronome unit's face (drag +
  steppers). Space/masthead stay as master transport; set_bpm tools stay.
- **2026-11-15** — **One Keyboard source** (user): on-screen keys and laptop
  keys are the same source ("the keyboard") — merged in the matrix, rig
  source toggles (K/H), routing and sessions; no separate Laptop row.
- **2026-11-15** — **Rigs are added/deleted in the app** (user): "+ Rig" in the
  always-visible RIGS strip builds a playable Keys→Surge→Master rig (voice pool
  of 4 Surge instances, idle voices not rendered); each rig chip has × with a
  "Delete rig?" confirmation. Surviving rigs keep patch, knobs, synth state,
  device binding and mix (split_rigs → edit → merge_rigs → one LoadRigs).
- **2026-11-17** — **No voice pool** ("remove the hard limit of surge
  instances", #2): voices are hosted on demand, one per CLAP unit, and
  retired when the unit goes; no idle spares (a warm Surge instance loads in
  about 40 ms, a first one in about 170 ms, measured by `many-voices-test`).
  No cap on the count: the engine's voice table grows by a table swap. CPU is
  the only limit, visible as `render_max_us` / `render_late` in get_status.
- **2026-11-15** — **Mixer is its own view** (user: "volume of the rigs at the
  top doesn't make sense"): vol/pan/M/S left the strip; the Mixer stage
  (chip / F10) shows every source → its rig bus → Master out as channel strips.
- **2026-11-15** — **The agent operates globally** (user): get_rigs / add_rig /
  delete_rig (confirm) / select_rig, add_node takes rig=N — in-app agent and MCP.
- **2026-11-15** — **Brand in the top bar** (user): the RIG INPUTS strip (K/H
  per synth) is gone; the crossed-wires mark + wordmark sit there instead,
  and the stage Play button is removed entirely (user choice): master
  start/stop is Space, per-unit Play chips stay. Per-synth keyboard /
  hardware routing lives in the MIDI matrix.
- **2026-11-15** — **Stability/latency pass** (goal): render thread promoted to
  realtime via rtkit (worst render 26 ms → 1 ms, 0 xruns under full CPU load),
  buffer 512 → 256 frames (10.7 → 5.3 ms), dev profile optimized (deps -O2),
  UI scale from the compositor (1.6, not winit's 2.33 DPI guess), background
  repaint throttled to 30 fps (UI CPU 24% → 12% unfocused), get_status reports
  render_avg_us / render_max_us / render_late.
- **2026-11-18** — **DSP load and Engine Info** (#106, AUM parity): one
  `engine_info::EngineStats` snapshot feeds the DSP readout, the Engine
  Info popup and `get_engine_info`; nothing else reads the callback counters.
  The readout sits at the right end of the rig strip, not in the bar above
  it: at the 760 px minimum that bar has 46 px left once the agent chip
  shows, so a readout there pushed SYNC out of the window.
  The render path times every voice and effect into `NodeTimes`, a fixed
  table of atomics written in render order (row i = the i-th unit of the
  block, so no search and no allocation; rows restart when the order
  changes). Cost: one clock read per voice and two per effect, not
  measurable in `many-voices-test` (median block 503 µs before and after, 10
  voices, six alternating runs each), so every block is timed. The sound
  card's output latency and the audio-input latency are shown as "not
  measured": cpal's `playback − callback` reads 185 ms through pipewire-alsa
  on a one-quantum graph (`audio-in-test`), and no other figure is measured.
- **2026-11-19** — **The assistant runs on OpenRouter only** (user): one key
  (`openrouter_api_key`) reaches every model, so `llm` has no endpoint or
  key table any more: a role (`Decide`, `Act`, `Listen`) is a model id, and an
  id without a vendor (left from the direct DeepSeek endpoint) falls back to
  the role's default. `assistant_enabled` in config.json takes the assistant
  out of the UI. The live status line is derived in one place
  (`DemoApp::live_status`) from the worker's phase, the tool batch and
  `listen::stage()`, the process-wide stage of the one master recording.
- **2026-11-20** — **A bus on its own output (#147)**: the engine holds a
  table of buses that leave the main mix (`Command::SetBusOutput`: a rig bus
  or an aux bus, and a PipeWire sink by name in fixed bytes), published in
  `GraphPub::bus_outs`, mirrored by the mixer and saved with the session. Up
  to `MAX_OUTS` (4) distinct sinks each take an output slot: a stereo mix, an
  `rtrb` ring and an epoch, all allocated in the constructor. In `render` a
  routed rig's post-fader sum, or a routed bus's return (the end of its insert
  chain, `return_slot`), adds into its slot's mix instead of the master's; the
  mix passes the master's last knee and goes into the ring. A full ring drops
  the block, so a missing device costs nothing. Off the audio thread,
  `audio_out::BusOutputs` keeps one `pw-cat --target <sink>` per slot and a
  thread that feeds it from the ring. The stream is `node.dont-reconnect`:
  when its sink goes PipeWire pauses it instead of moving it to another
  output, and `BusOutputs` drops the stream on its next sink check; the bus
  stays silent until the sink is listed again. A slot that changes hands
  moves its epoch first and the old feeder stops before it writes another
  bus's samples. Master and bus streams are separate PipeWire streams, on
  separate clocks when the sinks are separate cards: the feeder cuts a
  backlog above 40 ms, and `audio-out-test` prints how far apart the two
  arrive.
- **2026-11-19** — **Sends and aux buses (#5)**: an aux bus is a node
  (`DspKind::Bus`) with an OUT jack and no input. What feeds it is the send
  table (`Command::SetSend`: a rig or a node, a level, pre or post fader),
  published in `GraphPub::sends`, mirrored by the mixer and saved with the
  session. The cables from the bus are its insert chain, as on any strip, and
  what reaches a master from there joins the mix past the rig faders, so a
  post-fader send is not scaled by its own rig fader twice. A rig's send
  exists only once the rig is summed, so the render path runs in two passes:
  sources and their effects, the rig sums (tapped on both sides of the rig
  fader), then the buses and every effect downstream of one
  (`mark_after_bus`), then the returns. A bus or a node of its chain cannot
  send (no feedback, no block of delay anywhere). Mute and solo silence a
  source's sends in both modes; a bus's solo silences the dry rigs instead of
  the other sources of its rig. Limits: `MAX_BUSES` 8, `MAX_SENDS` 64, both
  allocated in the constructor. A send level is a learnable mixer control
  (`MixControl::Send(slot)`, the slot being the bus's place among the
  patch's buses), bound on the strip it leaves from. Not in it: a bus output other than the master
  (#31). Check with
  `kabelsalat send-test` (two Surge rigs into one reverb bus).
- **2026-11-15** — **No squished racks** (user): the assistant panel only takes
  the height the rack can spare; big patches keep readable units and pan
  (wheel / trackpad) inside a clipped view with hairline pan indicators.

## 1. Goal

A Linux-native application where you wire together plugins and audio sources in a
graph, route MIDI anywhere, and play live with low latency — AUM's workflow on the
desktop: mixer channels with insert FX and sends, a MIDI matrix with
splits/layers/filters, file players for backing tracks, synchronized recording,
built-in processing nodes, session save/restore. **No timeline.** The graph is the
instrument — and it moves.

**Two muses.** AUM supplies the workflow (routing, mixer, live hosting); GarageBand
supplies the soul — approachable, playful, instant music-making. Opening kabelsalat
should produce sound in under a minute, no manual required.

**Operating model (AUM, user's explicit north star).** You build **multiple
independent rigs** (channel stacks). Each rig picks its **own input** — a specific
hardware MIDI device, a virtual port, the on-screen keys, or an **audio source**
(system playback / file player) — runs through its own instrument + FX chain, and
mixes to the master. Rig 1 can be driven by MIDI keyboard A while rig 2 is driven
by keyboard B or a backing track, simultaneously. Free wiring on the back panel;
per-rig input selection on the front. This is the defining behavior, not a
nice-to-have: one shared keyboard fanning out is only the demo, the real app routes
**any source → any rig**.

Reference plugin for all hosting work: **Surge XT** (user's choice; ARM-native CLAP).

## 2. Signature feature: animated wiring

The cables are not lines — they are patch cables, and they behave like it:

- **Sag:** every cable hangs in a catenary-like curve between its ports, like a real
  patch lead. Length-aware: long runs droop more.
- **Signal flow:** MIDI events launch visible pulses that travel source → destination
  along the cable; audio cables glow/breathe with their live level. You *see* the
  signal path while you play.
- **Rope physics:** each cable is an 18-point Verlet rope (`src/rope.rs`) under
  gravity, ends pinned to the jacks, stepped at a fixed 120 Hz; dragging a node or
  grabbing a cable makes it swing and settle, an unplugged end falls free.
  Plugging/unplugging animates. The graph feels alive, never static.
- **Organ face (`organ.rs`):** setBfree has no LV2 control ports, only MIDI
  CCs. Its controls are ordinary knob parameters (`organ::P_BASE + n`, in
  `engine::knob_specs`), which the engine turns into the plugin's CCs; the nine
  drawbars are hidden knobs painted as faders. Values sent while the instance
  loads are parked and replayed (`organ_pending`).
- **Theme:** cable colors follow the Omarchy accent palette; audio vs MIDI visually
  distinct; selected cables highlight their full path.
- **RT-safe by construction:** animation is driven purely by lock-free snapshots
  (per-edge levels, event rings) from the audio thread. The UI never touches audio
  state; animation load never affects sound.
- **Performance budget:** tessellation caps per cable, level-of-detail when zoomed
  out, animation decoupled from audio callback rate.

Prototype in P0 (mini rack with Tab flip), full implementation on the real graph in P1.

## 2a. The rack flip — Reason's interaction model

The graph is presented as a **rack of labeled hardware-style units** (Reason):

- **Front (perform):** device faces with names, colored label strips, macro knobs,
  meters — the GarageBand-friendly performance view.
- **Back (patch):** press **Tab** and the rack flips around to reveal the patch
  bay: jacks per device and the full cable salad dangling between them.
- **The flip is animated** (slick vertical flip with easing); a visible hint
  (`Tab ⇄ flip rack`) keeps it discoverable.
- **Cables are physical:** grab one and it swings; shake the rack and they all
  sway — a toy, on purpose. Patch by dragging jack → jack; unplug with a tug.
- Front stays clean while playing; signal-activity glow shows which units are
  live. One graph, two faces — no separate "patchbay window".

## 2b. UX principles — the GarageBand inheritance

- **Instant sound:** first launch opens a playable starter template (pre-wired
  synth + reverb); friendly empty states with big, obvious "add" affordances.
- **Smart controls:** per-instrument macro knobs — many plugin parameters mapped to
  a few expressive controls. Made for live playing; MIDI-learn binds them.
  The knobs are the plugin's own main controls (`main_controls::resolve`):
  CLAP remote-controls pages first, then LV2 `param:` designations, then the
  first usable continuous parameters with macros in front. A CLAP list is
  read on the plugin's main thread when the instance is created and again
  after a state load; an LV2 list is read per URI on a worker thread. The
  face polls CLAP values back at 10 Hz. An LV2 port is not read back, so its
  knob mirrors what the app sent (the engine does not publish port values).
- **Big, friendly controls:** touch-sized targets, rounded cards, playful visual
  feedback — instruments and cables react visibly when played.
- **Templates:** one-click starting points ("Lead + Reverb", "Drums + Bass",
  "Ambient Pad") so the graph is never a blank page of fear.
- **No manual required:** sensible defaults everywhere, `?` shortcut overlay,
  discoverable interactions, errors explained in plain language in-window.
- **One big record button** (P2): count-in, then immediate playback of the take.
- **Slickness bar:** 60fps everywhere, consistent design tokens (spacing, radius,
  type from the Omarchy theme), micro-interactions on every action, drag-and-drop
  as the primary verb, undo for everything. Operating it should feel effortless.
- **One window:** plugin editors open inline (panel/drawer inside the main window,
  AUM-style). Nothing scatters across the desktop; host chrome stays around every
  editor. Detached windows are a flagged fallback, never the default.

## 2c. Agent control — full MCP server, 1:1

kabelsalat is agent-native: a built-in MCP (Model Context Protocol) server exposes
**every** feature of the app — anything a human can do in the UI, an agent can do.

- **One control plane.** All mutations are typed commands (AddNode, Connect,
  LoadPlugin, SetParam, SetMacro, MatrixRoute, TransportPlay, StartRecord,
  SetSystemAudioSource, SaveSession, …) and all reads are snapshot queries
  (GetGraph, ListPlugins, GetParams, GetMeters, GetTransport, …). The egui UI and
  the MCP server are *sibling frontends* to this single API — 1:1 parity is
  structural, not aspirational.
- **Implementation:** [rmcp](https://github.com/modelcontextprotocol/rust-sdk)
  (official Tier-1 Rust SDK, `#[tool]` macros, JSON Schema via schemars).
  `kabelsalat mcp` runs the server over stdio; streamable HTTP optional later.
  Tools mirror commands; resources expose graph/session state; prompts offer
  recipes ("build a split synth layer"). Tool catalog documented and versioned.
- **RT-safe:** MCP commands enter the same lock-free command ring as UI commands;
  queries read snapshots. Agents never touch the audio thread directly.
- **Parity test:** CI enumerates UI actions vs MCP tools and fails on drift.
- **Plain CLI too:** every command also available as `kabelsalat <cmd> --json`
  (Omarchy convention), sharing the same control plane.

## 3. What already exists (verified survey, Nov 2026)

| App | Formats | Live UX | MIDI routing | On this machine |
|---|---|---|---|---|
| [Element](https://github.com/kushview/element) 1.2.0 | LV2, VST3, CLAP (upstream), LADSPA | node canvas, subgraphs — closest to AUM, **dated UI** | free graph, per-node key ranges, setlist; no matrix | `extra` aarch64 ✓ installed |
| [Carla](https://github.com/falkTX/Carla) 2.5.10 | LV2, VST2/3, LADSPA, DSSI; CLAP dev-only | patchbay + rack | port wiring + CC; no splits/layers/matrix | `extra` aarch64 ✓ installed |
| [Bespoke Synth](https://github.com/BespokeSynth/BespokeSynth) | VST3, LV2 | modern live canvas, instrument-not-mixer | canvas modules | `extra/bespokesynth` aarch64 |
| [MOD Desktop](https://github.com/mod-audio/mod-desktop) | LV2 only | browser pedalboards | CC bindings | x86_64 only |
| [Maolan](https://github.com/maolan/maolan) (Rust, BSD-2) | CLAP, VST3, LV2 out-of-process | DAW + plugin graphs | graph routing incl. MIDI | ARM untested; reference for hosting |
| qpwgraph / Helvum / RaySession | — | wire apps/devices, not plugins | — | available |

Commercial live hosts (Gig Performer, Cantabile, Camelot, MainStage): **no Linux versions**.
**Gap we fill:** AUM's mixer + MIDI matrix + live focus + a modern, alive interface.

## 4. Verified environment & plugin ecosystem

MacBook Pro M1 · Asahi Linux · **aarch64** · 8 cores / 16 GB · Rust 1.100 (rustup,
`~/.cargo/bin`) · PipeWire 1.6.9 + pipewire-alsa · jack2 present (pipewire-jack
optional later) · Omarchy/Hyprland (Wayland + XWayland) · theme colors.toml present.

ARM64-native plugins: **Surge XT 1.3.4** (VST3/CLAP/LV2; `extra/surge-xt-vst3`
installed, CLAP variant available), **LSP 1.2.35** (LV2 installed, CLAP available),
**x42** (LV2), **Dexed**, **ChowDSP** (Flathub). u-he: no ARM builds. Vital:
source-only. **No box64/emulation for live audio** — native ARM plugins only.

## 5. Scope — AUM feature mapping

| AUM feature | Phase |
|---|---|
| Graph canvas with **animated cables** (§2) | P0 proto / P1 full |
| Channels: source + insert slots (pre/post fader), buses, sends | P1 |
| Hardware in/out nodes (stereo first) | P1 |
| **System audio source** — capture desktop playback (YouTube etc.) into the graph | P2 |
| **Full MCP server** — 1:1 agent access to every feature via the shared control plane | API P1 / server P3 |
| **MIDI matrix**: any→any, channel filter, transpose, key-range splits/layers, CC map | P1 |
| Built-in nodes: gain/pan, parametric EQ, LP/HP + DJ filter, limiter, saturation, M/S | P1 core / P3 rest |
| Sessions (incl. plugin state), undo, autosave | P1 |
| On-screen keyboard + external MIDI controllers | P0/P1 |
| FilePlayer (loop, transport-synced) | P2 |
| Transport clock, metronome, count-in | P2 |
| Recorder: master + per-channel stems, synchronized | P2 |
| Latency compensation | P2 |
| MIDI-learn (mixer + plugin params) | P2 |
| **CLAP hosting** (Surge, LSP, Dexed, Chow) | P0/P1 |
| LV2 hosting (x42, Calf, Guitarix…) | P3 |
| VST3 hosting | later (study Maolan) |
| Plugin GUIs: own windows first (X11/XWayland), embed later | P0 / later |
| Scenes/setlist for live | P4 |
| MIDI clock send (24 ppqn, Start/Stop, SPP, offset + latency modes) | done (#85) |
| Ableton Link, MIDI clock receive, OSC | later |

**Non-goals:** timeline/arranger, piano roll, notation, video, x86 bridging, VST2, AU.
Fixed limits for RT safety: 64 nodes, 8 inserts/channel, stereo buses (multichannel later).

## 6. Hosting strategy

1. **CLAP-first via [clack-host](https://github.com/prokopyl/clack)** — safe Rust,
   active; covers Surge/LSP/Dexed/Chow natively on ARM. Notes, params, state,
   latency, GUI extension.
2. **LV2 second via lilv** — battle-tested C lib, huge ARM ecosystem.
3. **VST3 later** — no mature Rust hosting crate; study Maolan (BSD-2) or vst3-sys.
4. **In-process first** (lowest latency); out-of-process crash isolation later
   (Maolan pattern). Autosave sessions meanwhile.
5. **Plugin UIs — embedded, not detached (user requirement):** the CLAP GUI
   extension is built for embedding: the plugin creates an X11 editor window which
   the host reparents into its own. kabelsalat therefore runs under XWayland
   (`WINIT_UNIX_BACKEND=x11`) and embeds editors inline — click a node, its editor
   opens in a panel/drawer inside the main window, theme-matched chrome around it.
   CLAP resize negotiation (`can_resize`/`adjust_size`) handles sizing; the same
   mechanism later covers VST3 (X11) and LV2 (suil). P0 proves reparenting with
   Surge; P1 integrates into the egui canvas (reserved rect → `XReparentWindow`).
   Detached windows only as a flagged fallback for pathological plugins.

## 7. Architecture

```text
crates/ (P1 split; P0 lives in a single spike crate)
  kabelsalat-app/      # egui: graph canvas + animated cables, mixer, matrix, keyboard
  kabelsalat-engine/   # graph → RenderPlan, transport, scheduler, latency comp
  kabelsalat-host/     # clack CLAP (later lilv LV2, VST3); scan, state, GUI windows
  kabelsalat-dsp/      # built-in nodes (fundsp + custom)
  kabelsalat-io/       # cpal duplex, midir, system-audio capture, file streaming, recorder
  kabelsalat-project/  # session model, serde/RON, undo commands
  kabelsalat-control/  # typed command/query API — the single control plane
  kabelsalat-mcp/      # rmcp server: tools/resources mirroring the control plane 1:1
src/main.rs            # clap CLI: `kabelsalat open|mcp|<cmd> --json`
```

- **Frontends are siblings:** egui UI, MCP server, and CLI all call the same control
  plane (`kabelsalat-control`); every command flows through one lock-free ring into
  the engine. This is what guarantees 1:1 MCP coverage (§2c).
- **Display:** native Wayland (winit), at the compositor's fractional scale.
  The default editors are native egui parameter views. A plugin's own GUI is
  X11 on Linux, so it opens in a host-owned XWayland toplevel
  (`plugin_window.rs`, a `_NET_WM_WINDOW_TYPE_DIALOG` so window managers float
  it); the UI frame drives the plugin's `timer-support` and
  `posix-fd-support` callbacks, which JUCE editors paint from.
- **Threads:** UI (egui) · audio callback (cpal, RT) · MIDI in (midir) · disk pool.
  UI→audio: `rtrb` command rings. Audio→UI: `triple_buffer` snapshots (meters,
  playhead, **per-edge levels for cable animation**) + event ring (notes, xruns).
- **Graph model:** nodes with typed ports (audio stereo / MIDI); channel strip =
  composite node (source → inserts pre/post → fader → sends → bus). Edits compile a
  new `RenderPlan` off-thread; audio thread swaps at block boundaries.
- **RT rules:** no alloc/locks/syscalls/logging in the callback; preallocated
  buffers; fixed limits; allocator guard in debug; soak test in CI.
- **MIDI:** midir ALSA-seq ports (hardware + virtual). Matrix = per-block routing
  table with sample-accurate scheduling: channel filter, transpose, key/velocity
  range (splits/layers), CC map. MIDI-learn binds CCs to any parameter.
- **Latency comp:** nodes report latency; parallel paths are aligned at every
  summing point (built-in units today, see below).
- **System audio:** capture the default output's monitor via PipeWire's PulseAudio
  compatibility (works today, no extra packages); per-app stream selection later via
  native PipeWire (pipewire-rs) or pipewire-jack input ports. Feedback guard: exclude
  our own output from capture (dedicated sink / node filter), warn in the UI.
- **One render path** for realtime and offline bounce.
- **Sessions:** `name.ksalat/` package: `session.ron` + plugin state + copied assets;
  atomic temp-then-rename; originals never modified.

### Delay compensation and the analysis harness

**Delay compensation** (`engine/pdc.rs`, issues #13 and #76). A latent unit
(the Limiter's lookahead: 240 frames at 48 kHz; a hosted plugin's reported
latency) delays its path; where that path meets a parallel one, the two
would be that far apart (a hollow, comb-filtered sum).

*Where latency comes from* (`GraphEngine::replan_delays`): a built-in
processor's `latency_frames`; an LV2 plugin's `lv2:reportsLatency` port,
which it writes in every `run` (`Lv2Plugin::latency_frames`, read after each
block into `Lv2Slot::latency`); a CLAP voice's `latency` extension, read on
the main thread when the voice is activated and refreshed whenever the plugin
calls `host_latency.changed()` (`ClapVoice::latency` via a shared atomic). One
unit counts for at most `pdc::MAX_FRAMES` (16384).

*The plan* (`Planner::plan`, a pure function of graph, sends and latencies):
every node knows how late each source's sound is at its output (`arr`, a
table of node × source; a source is a node nothing is cabled into, a bus for
its return). At every summing point (an effect's input, `sum_audio_into`; a
rig's master, render step 3) the inputs that share a source form a set, and
each set is shifted until its sources line up, as `(source, destination,
frames)` delays. Inputs that share no source are left alone: a latent effect
on one instrument never delays another (the played note path gains nothing
it does not need), an instrument's own latency moves nothing, and a latent
unit nothing is cabled into delays nobody. One number per node cannot say
this (two sources through one shared unit, one of them latent before it).
The plan follows the engine's effect order (`next_effect`), so the cable
that closes a feedback loop (it carries the previous block) neither counts
nor gets a delay, and the plan ends. Sends are not cables, so the plan takes
them beside the graph. A return has to line up with the dry sound of whoever
sent into the bus, not with the master its cable ends at: a whole-rig send
taps the rig's aligned sum and needs nothing; a send of one source waits on
its way into the bus as long as its dry sound takes from the tap to the
rig's sum (`(source, bus, frames)`, applied in `BusTaps::tap`); a latent
unit in a bus's own chain makes the senders' rigs wait after their sends are
tapped (`(master, master, frames)`, `Pdc::rig_out`). Without a latent unit
in a bus's chain, rig sends change nothing and rigs never wait for each
other. Master inserts, the file player, system audio and the Audio In
monitor are serial or outside the graph and are not part of it.

*When it replans.* On every structural edit (`publish_graph`), which may
allocate like the edit itself. When a hosted plugin reports another latency
(a knob on the plugin) or a send starts or stops sounding, the engine replans
without allocating: the planner keeps its tables between plans, and while the
graph holds a unit whose latency can change (or a bus beside any latent unit),
`Pdc` keeps every candidate line (`Planner::cand`) preallocated (`MAX_FRAMES`
when varying, or the sum of fixed latencies for bus sends); a line the plan
does not use is parked at 0 frames and costs nothing per block. A graph with built-in units only keeps
exactly the plan's lines, and with no latent unit there is no line at all
and every sum is the plain sum (`engine-bench` hashes the same). A line
whose delay is unchanged keeps what it holds; another delay starts from
silence. A bypassed latent unit keeps its delay (its input runs through a
ring of its latency), so a bypass toggle moves nothing in time.

*What the UI shows* (rule 33): `GraphPub::latency` lists how late each
strip sounds (sources, rigs by their master, buses), published with the
graph and, for a run-time change, under `try_lock` within reserved room.
The mixer draws it under the fader.

*Proof.* The tests and `analysis-test` host `engine/fixture/latent.c`, an
LV2 plugin that delays by its Delay control and reports that as its latency
(built with `cc`, loaded by path through `lv2::add_bundle`), on the app's
load path (`Probe::hosting`): cross-correlation puts the latent path at lag
0 after every change, and a counting allocator sees no allocation while the
engine follows one.

**Analysis harness** (issue #13). `analysis.rs` holds pure measures with no
dependency: a radix-2 FFT and `Spectrum` (Hann window) with
`fundamental_hz`, `centroid_hz`, `share_above` and `inharmonic_share`
(aliasing), waveform measures (`hard_saturation_share`, `flat_top_run`,
`non_fundamental_share`), deterministic `white`/`pink` noise, and a
normalised cross-correlation (`xcorr_peak`, `presence`). `engine/probe.rs`
feeds signals into Audio In nodes through the real input ring, one block
before each render, and taps the master or any node's output block; a
routing check is a table of `(feed, tap, present)` rows, so new buses and
sends add rows. The unit tests (crackle regression, noise routing, plan and
alignment) run in CI without plugins; `kabelsalat analysis-test [--save
<dir>]` runs the same scenarios, prints every measured number, a "listen
for" line per check that stays by ear, and with `--save` writes those clips
(16-bit WAV, read back by the player). With Surge XT installed it also
measures a played note's pitch, brightness and aliasing.

## 8. Build phases (history)

The phases the project was built in. The current roadmap lives in
[GitHub issues and milestones](https://github.com/ssalentin/kabelsalat/milestones).

### P0 — Spike (3–5 days) · go/no-go gate
- cpal duplex through PipeWire, no xruns over 5 min; latency numbers
- **Surge XT CLAP via clack-host: play notes → sound**; GUI **embedded** in an X11
  parent window (reparenting proof, not a detached window); state round-trip
- midir input (hardware or virtual port) + egui on-screen keyboard
- **Rack-flip prototype:** mini rack with labeled units, Tab flip to the back,
  sagging physics cables, traveling pulses — prove the signature feel
- In parallel: Element/Carla remain installed as the functional baseline
- **Done when:** you can play Surge live through our spike, and the cables feel alive.

### P1 — Live core (~2–3 weeks)
- Rack front/back views with full animated wiring; device browser; add/remove/wire
- Channel strips, buses/sends, hardware I/O; built-in gain/pan/EQ/filter/limiter
- MIDI matrix with splits/layers; sessions + undo; starter templates + friendly empty
  state; **control-plane command API** (the single API behind UI, MCP, and CLI)
- **Done when:** two Surge layers split across the keyboard, one through EQ + reverb
  bus — save, reopen, play live.

### P2 — Performance (~2 weeks)
- FilePlayer, transport + metronome + count-in; recorder (master + stems);
  **system audio source** (PipeWire monitor capture, e.g. YouTube into a channel)
- Latency compensation; MIDI-learn; smart controls (macro knobs per instrument)
- **Done when:** live set over a backing loop, recorded cleanly.

### P3 — Ecosystem (~2–3 weeks)
- LV2 hosting (lilv); plugin scan/browser/presets; remaining built-in nodes;
  **MCP server** (rmcp, stdio) exposing the full control plane
- **Done when:** mixed CLAP + LV2 session stable for an hour.

### P4 — Live polish & packaging (~1–2 weeks)
- Scenes/setlist, autosave; MCP 1:1 parity tests + tool catalog docs; Omarchy theme
  following; keyboard-first (`Space`
  transport, `?` overlay, `Q` quit-with-confirm); PKGBUILD + `.desktop` + icon;
  headless CLI; README with screenshot
- **Done when:** installed from the launcher, gig-ready.

**Later:** Ableton Link · MIDI clock · OSC · VST3 · out-of-process hosting ·
cpal native-PipeWire host (`pipewire` feature) · multichannel · MPE · 96 kHz.

Estimates: playable host ≈ 3–4 weeks · AUM-core ≈ 6 weeks · full ≈ 3 months (evenings).

## 9. Risks & mitigations

| Risk | Mitigation |
|---|---|
| clack young; Surge hosting edge cases | P0 targets Surge specifically; Maolan as reference/fallback |
| Plugin GUIs X11/XWayland quirks | own-window model first; verify in P0 |
| RT violations on graph changes | off-thread RenderPlan + block-boundary swap; allocator guard; soak test |
| Animation costs UI frame time | tessellation caps, LOD, snapshot-driven; never on audio thread |
| In-process plugin crash | autosave; out-of-process later |
| ARM plugin ecosystem thinner | verified list §4; CLAP-first; no emulation |
| System-audio feedback loop (capturing our own output) | exclude own stream; dedicated sink or per-app capture; UI warning |
| Plugin embedding quirks (resize, focus, HiDPI under XWayland) | CLAP resize negotiation; per-plugin fallback flag; XWayland scaling config; P0 proof with Surge |
| Scope creep toward DAW | non-goals §5; the graph is the product |

## 10. Omarchy conventions

Rust overrides the omarchy-app skill's C++/Qt stack (explicit user request); the
rest stands: short lowercase name for binary/package/`.desktop`/icon/app id ·
`~/Work/kabelsalat`, git · keyboard-first with `?` overlay · live theme accent from
`colors.toml` (watch + re-arm), corners follow Hyprland rounding · portal file
dialogs (`ashpd`) · atomic saves, originals untouched · errors surfaced in-window ·
headless `--json` CLI · PKGBUILD → launcher.

## 11. Open items

- **MIDI hardware:** tested with an M-Wave FM-1 (USB, keys + knobs); the on-screen keyboard and
  virtual ALSA ports remain the fallback.
- pipewire-jack pro-audio mode: optional later (replaces jack2)

## 12. References

- AUM — https://kymatica.com/apps/aum
- clack — https://github.com/prokopyl/clack · Maolan — https://github.com/maolan/maolan
- Element — https://github.com/kushview/element · Carla — https://github.com/falkTX/Carla
- Surge XT — https://surge-synthesizer.github.io/
- cpal — https://github.com/RustAudio/cpal · midir — https://docs.rs/midir · fundsp — https://docs.rs/fundsp
- egui — https://github.com/emilk/egui · rtrb — https://docs.rs/rtrb · ashpd — https://docs.rs/ashpd
- rmcp (official Rust MCP SDK) — https://github.com/modelcontextprotocol/rust-sdk
- Design reference — https://dribbble.com/shots/19623638-Hurricane-Creative-Agency-Landing-page
  (frames: docs/design-ref/; analysis: docs/design-reference-analysis.md)
