# kabelsalat — live audio/MIDI wiring, alive

**An AUM-style live plugin host for Linux, in Rust — where the cables are the show.**

Status: **core app complete + Hurricane redesign (polished) + multi-rig (backend & UI + per-rig mixer) + hot-wire MIDI matrix with live rule editing + MCP→UI sync landed; dynamic CLAP loading & backing tracks next.** P0–P3 done. Working: data-driven rack (Tab flip, animated cables), 8-node DSP, per-rig MIDI routing + splits + per-device binding, sessions, **native egui plugin param editor (NO X11 — double-click a unit)**, system-audio capture, file player, **metronome as a normal rack node**, visual transport, **Ctrl+drag rack rearrange**, master stereo+MONO inputs, live audio/MIDI latency reporting, MCP server (headless + live-app attach) + DeepSeek assistant, CLAP (Surge) + LV2 effect/instrument hosting, plugin browser, starter templates, **RIGS strip (+ Rig / × delete with confirmation) + Mixer view (F10: sources → rig bus → master) + 1–9/←→ view switching (F4 two-rig demo)**, **global agent rig control (get/add/delete/select_rig)**, **big patches pan instead of squishing**, **hot-wire MIDI matrix (F11): any source → any rig, live, with per-cell rule shaping (right-click)**, **MCP/CLI remote edits (graph + rules) sync into the UI live**. **Hurricane visual redesign landed & polished** (src/theme.rs: warm paper + one near-black hero rack panel, flat fills, hairlines, tomato accent — docs/design-reference-analysis.md; DeepSeek-vision verdict 8.5/10). 217 unit tests + many CLI self-tests pass. Next: (1) dynamic CLAP loading (#2), (2) backing tracks (#1), (3) backlog (§13). · Name: `kabelsalat` · Location: `~/Work/kabelsalat` · See **§0.1 Session handoff** below. **Work tracking: PLAN.md §13 roadmap, mirrored as GitHub feature issues #1–#8 (user request 2026-11-15).**

## 0.1 Session handoff (read me first in a new session)

**Repo:** `~/Work/kabelsalat` → `github.com/ssalentin/kabelsalat` (branch `main`, all pushed).
Rust, single binary. Build: `export PATH="$HOME/.cargo/bin:$PATH"; cargo build`. Run the app:
`WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000 ./target/debug/kabelsalat`.
Tests: `cargo test` (217). CLI self-tests: `session-test`, `capture-test`, `player-test`,
`metro-test`, `mono-master-test`, `multi-rig-test`, `multi-rig-load-test`, `add-node-test`,
`lv2-test`, `lv2-host-test`, `lv2-voice-test`, `lv2-engine-test`, `lv2-instrument-test`,
`lv2-instrument-engine-test`, `audio-test`, `host-test`. MCP: `kabelsalat mcp` (headless) or
`kabelsalat mcp --attach` (drive the running app); one-shot CLI: `kabelsalat send '<json>'`.

**Architecture (src/):**
- `control.rs` — typed control plane: `Command`, `Graph`/`Node`/`Edge` (Node has `rig:u8`),
  `NodeKind`, `DspKind` (incl. `Metronome`), `Rig`, `RigSnapshot`/`RigMeta`, `merge_rigs`,
  lock-free `command_bus()` (rtrb). One API for UI/assistant/CLI/MCP.
- `engine.rs` — `GraphEngine`: voices→DSP→LV2→master. Sums **all** MasterOut nodes (multi-rig).
  Master has stereo (ports 0/1) + MONO (port 2, centered). Metronome is a DSP node driven by
  transport. `LoadGraph`/`LoadRigs` rebuild slots + matrix; `set_rig_snapshot` publishes rig state.
  RT discipline: render path is allocation-free; structural ops may allocate at block boundary.
  Publishes `GraphPub {rev, graph}` + `RigSnapshot` on structural ops only (never the render
  path) — the UI adopts remote MCP/CLI changes via `sync_from_engine` (divergence check).
- `host.rs` — CLAP via clack-host: `ClapVoice` (audio, Send) + `ClapVoiceMain` (main-thread, !Send),
  `VoiceController` (RT-safe note/param), `param_list()`/`param_text()` for the native editor.
  **2 Surge voices** created at startup (main-thread-only); LoadGraph remaps them in node-id order.
- `lv2.rs` — hand-written lilv FFI (no bindgen/clang). `Lv2Plugin` (effects + instruments via
  atom/MIDI), `discover()` (classifies instruments), `merge`-friendly.
- `dsp.rs` — 8 allocation-proven processors + Metronome placeholder.
- `matrix.rs` — MIDI routing: `SourceId::{Keys,LaptopKeys,Hardware(dev),...}`, per-rig rules,
  splits/transpose/filters. Allocation-free `route_into`.
- `theme.rs` — **Hurricane design language, all-dark chrome** (user pivot
  2026-11-15; docs/design-reference-analysis.md §0 overrides): warm near-black
  surfaces, one tomato accent. Tokens: PAPER/PAPER_SINK/INK/INK_2/HAIRLINE
  (dark values), DARK_PANEL/DARK_PANEL_HI/CREAM/CREAM_DIM/HAIRLINE_DARK,
  ACCENT (tomato) + text-safe variants, AUDIO_CREAM/MIDI_BLUE/LIVE_TEAL/
  CLIP_RED, per-family CABLE_AUDIO_PALETTE/CABLE_MIDI_PALETTE. Flat painters:
  `paper_bg` (flat dark, no watermark), `crossed_wires` (brand mark),
  `drop_shadow` (floating sheets only), `inset_keyline`, flat `value_arc`,
  stepped `meter_color`; `chip_button` (real buttons), `TrackedLabel`
  per-char galley machinery (letter-spacing), `tracked_label`/`section_label`/
  `primary_button` widgets, `visuals()` (light), `install_fonts` (Noto Sans reg/med/bold
  + JetBrains Mono; Medium = display, Bold = flip pill only).
- `ui.rs` — eframe/egui. Data-driven rack (`build_rack`), Tab flip, animated cables, Ctrl+drag
  rearrange, **native egui param editor (double-click a Clap unit; NO X11)**, plugin browser (F9),
  templates (F5/F6/F7, F8 demo), sessions, transport popover (F10/Space), stats (fps/cables/midi ms).
  Hurricane painting: flat plates + 2px accent/hairline strips, teal LEDs, single-stroke cables
  (audio cream/round terminals, MIDI blue/square), flat pulse dots, paper masthead toolbar with
  the single tomato flip pill (`flip_rect()` shared with hit-test), tracked micro-caps labels,
  sentence-case unit names, dark help card. Holds `shared_graph`, `rig_snapshot`, `surge_controllers`.
- `bridge.rs` — unix socket `$XDG_RUNTIME_DIR/kabelsalat.sock`; `spawn_with_rigs`; queries
  `get_graph`/`get_status`/`get_rigs`; `BUFFER_FRAMES=512`, `XRUNS`, `MIDI_LATENCY_US` statics.
- `mcp.rs` — rmcp server, ~19 tools incl. `load_rigs`/`get_rigs`/`add_node`/`connect`/`list_plugins`.
- `assistant.rs` — DeepSeek BYOK (key at `~/.config/kabelsalat/config.json`, 0600, NEVER commit).
- `session.rs` — `Session` + `MultiRigSession` atomic save/load; templates.
- `capture.rs`/`player.rs`/`midi.rs` — system-audio monitor capture, WAV file player, midir I/O.

**Hard-won conventions / gotchas:**
- **No X11 child window** (user requirement). The plugin editor is native egui, not reparented.
  The app still runs under XWayland for fractional HiDPI (run_inner hides WAYLAND_DISPLAY); ppp≈2.33.
- Notes are **absolute MIDI** through `fire_keys`/the sink (a +60/-60 clamp bug was fixed).
- CLAP voice pool = 2 Surge voices; >2 CLAP synths needs dynamic loading (backlog). LV2 instruments
  can extend. LV2 instrument classification is a permissive port hint (class is a blank node).
- Tab is stripped in `raw_input_hook` (flip only, never moves focus). Laptop keys don't fire when
  the assistant text field has focus (`egui_wants_keyboard_input`).
- System-audio monitor capture can feed back if playing to the same sink (muted by default).
- **Hyprland window-mapping glitch:** app process can run while its window stops listing; relaunch
  cleanly (`pkill -x kabelsalat; rm -f /run/user/1000/kabelsalat.sock; relaunch`). Focus via
  `hyprctl` (signature dir: `/run/user/1000/hypr/<sig>`).
- Lead owns git commits + Cargo.toml; subagents use `CARGO_TARGET_DIR=$HOME/.cache/ksalat-*-target`,
  never /tmp (tmpfs). Verify before committing; keep the running app on the old binary until approved.
- **Visual review = DeepSeek vision via the app's BYOK key** (verified working): key at
  `~/.config/kabelsalat/config.json` field `deepseek_api_key` (NEVER print/commit). POST
  `https://api.deepseek.com/chat/completions`, model `deepseek-flash` (text+image;
  `deepseek-v4-pro` is text-only), OpenAI-style `image_url` data-URL parts, **max_tokens ≥ 4000**
  (reasoning eats small budgets → empty content). Downscale images first (magick). DSH subagents
  here are qwen4-max-preview (no image input) — they drive the API via bash, they don't see images.
- **egui 0.37 deadlock gotcha:** `Context` is ONE RwLock — never call `Painter::add/galley`
  inside `ctx.fonts_mut`/`ui.fonts_mut` (re-entrant write → 10s-timeout panic). Layout under
  the lock, paint after releasing it (see `theme::tracked_label`).
- **Screenshot harness:** `scripts/capture.sh <outdir> [binary]` launches `ui-demo`, injects
  Tab/Space/?/Esc/F10 via `scripts/xtest_key.py` (ctypes XTEST — needs proper restype/argtypes),
  captures front/back/playing/help/transport with `grim -o eDP-1` (env: WAYLAND_DISPLAY=wayland-1
  DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000). Verify with ImageMagick histograms/token counts.
  Dribbble is bot-walled: fetch shot pages via `https://r.jina.ai/<url>`, images from
  `cdn.dribbble.com/userupload/...` (strip resize params for full res).
- **XTEST keysym gotchas:** F4 = 0xffc1 (NOT 0xffc3 = F6 — cost an hour once). winit
  maps F-keys by evdev scancode, not the X keymap; the evdev F-row is NOT contiguous:
  F1–F10 = X keycodes 67–76, **F11/F12 = 95/96**. `scripts/xtest_key.py` handles this.
  Mouse clicks: XTestFakeMotionEvent + XTestFakeButtonEvent (screen-native coords).
  egui window rects are WINDOW-relative — add the app window's screen origin
  (hyprctl `at:` × monitor scale) before clicking. Hyprland 0.56 Lua dispatch:
  `hyprctl dispatch 'hl.dsp.window.fullscreen({window="address:0x…", mode="fullscreen"})'`
  (mode is a STRING; the demo tiles next to the running app otherwise).

**Next builds (in order):**
1. **Dynamic non-Surge CLAP loading** — arbitrary CLAP instruments beyond the two
   startup Surge voices (main-thread instantiation + voice-delivery side channel).
2. **Backing tracks** (GitHub #1) — deck UI + YouTube/URL sources.
3. **Backlog** — MIDI-learn/CC maps, sends/aux, recording, tap tempo (§13).

**Hot-wire MIDI matrix — LANDED 2026-11-15.** F11 / top-strip "Matrix" opens the
sources × instrument-rigs grid (Hurricane: paper sheet, hairline cells, teal = routed).
Rows: on-screen keys, laptop keys, Hardware-all + one row per detected device (radio
group per rig: one device / all / off). Columns: CLAP voices + LV2 instruments
(classified at build_rack). Cells reflect the engine's real gates (MIDI cable +
source filter + device binding); clicking hot-wires live — turning a cell ON also
plugs a missing Keys→instrument MIDI cable; LV2 columns toggle the cable itself.
Row activity LEDs, split-state footer, per-rig cable indicators. Assistant parity:
`get_matrix` / `set_matrix_cell` tools (MCP already had set_rig_source/device/split).
Verified: 118 tests (6 new matrix tests) + live mouse-click on a cell toggled exactly
one route (pixel-verified). Screenshot: docs/shots/midi-matrix.png.

**Multi-rig UI — LANDED 2026-11-15.** Overview strip (rig name + bound MIDI device +
live level + tomato active underline) appears above the rack when >1 rig is loaded;
**1–9 / ←→ / click** switch the big view (VISUAL ONLY — all rigs keep sounding; the
engine matrix routes by source, never by UI selection). `Unit.rig` tags from
`merge_rigs`; `compute_groups`/layout/draw/hit-test filter to the active rig; hidden
units keep `Rect::NOTHING`; cables of all rigs stay live. Held laptop keys drain on
focus loss (vibez lesson). **F4 / Sessions / assistant `load_template multi_rig`** load
a two-rig demo (LoadRigs). `set_active_rig` keeps `rig_snapshot.active` in sync for
`get_rigs`. Verified: 112 tests (5 new multi-rig UI tests) + multi-rig engine
self-tests + live key-driven switching (frame-diff + exact restore). Screenshots:
docs/shots/multirig-{a,b}.png.

**Hurricane redesign — LANDED 2026-11-15** (agent team: design-analyst → implementer → lead).
History: the earlier "Signal" dark-glass pass was built from a spec written WITHOUT seeing the
user's reference (Dribbble bot-blocked the fetch). The user's actual target is the
[Hurricane Creative Agency landing page](https://dribbble.com/shots/19623638-Hurricane-Creative-Agency-Landing-page)
(frames in `docs/design-ref/`). DeepSeek-V4.1-Flash vision analyzed the frames + pixel-measured
the palette → `docs/design-reference-analysis.md` (the authoritative spec: tokens, per-component
instructions, contrast table, keep-list). Implementation: light paper chrome, rack = the single
near-black hero panel, flat fills/hairlines, one tomato accent, semantic cream/blue/teal/red from
the reference imagery, tracked micro-caps + Medium display type, single flip pill. Verified:
107 tests + session-test; pixel analysis (paper/dark/tomato dominance, MIDI blue on back, teal
when playing, zero old-palette px); DeepSeek vision comparison verdict "close in spirit" with
polish gaps recorded in §13. Fixed during integration: round/square terminal shapes (§3 vs §4.12
brief inconsistency) and the fonts-lock deadlock (above).

**Docs:** `docs/design-reference-analysis.md` (**current design spec**), `docs/design-language.md`
(superseded Signal spec — history only), `docs/vibez-notes.md` (reference-app lessons),
`docs/p0-report.md` (latency/go-no-go), `docs/p3-lv2.md`, `docs/p3-mcp.md`, `docs/control-plane.md`,
`docs/p2-transport.md`, and per-feature p1/p2 docs. **Roadmap/queue: §13 of this file** (BACKLOG.md
merged here 2026-11-15; no GitHub tickets).

## 0. Decision log

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
  X11 reparenting works.
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
- **2026-11-15** — **Single roadmap in PLAN.md** (user): all work tracking lives in §13
  of this file — no GitHub tickets; BACKLOG.md merged here.
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
- **Physics-lite:** dragging a node makes its cables swing and settle (damped spring);
  plugging/unplugging animates. The graph feels alive, never static.
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
| Ableton Link, MIDI clock, OSC | later |

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
- **Display:** the app runs under XWayland (X11) so plugin editors can be reparented
  into the main window — CLAP/VST3/LV2 Linux GUIs are X11; native-Wayland plugin
  GUIs don't exist in practice yet.
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
- **Latency comp:** nodes report latency (CLAP extension); align parallel paths at
  outputs. Uncompensated first (serial chains align naturally).
- **System audio:** capture the default output's monitor via PipeWire's PulseAudio
  compatibility (works today, no extra packages); per-app stream selection later via
  native PipeWire (pipewire-rs) or pipewire-jack input ports. Feedback guard: exclude
  our own output from capture (dedicated sink / node filter), warn in the UI.
- **One render path** for realtime and offline bounce.
- **Sessions:** `name.ksalat/` package: `session.ron` + plugin state + copied assets;
  atomic temp-then-rename; originals never modified.

## 8. Roadmap

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

- **MIDI hardware:** which controller/keyboard? (P0 testing; on-screen keyboard +
  virtual ALSA ports as fallback)
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

## 13. Living roadmap (single source of truth — no GitHub tickets)

The north star is AUM's operating model: **multiple independent rigs, each bound to its
own MIDI device or audio source, all sounding at once, freely wired and mixed** — with
GarageBand approachability and Reason's rack flip, in the Hurricane visual language.
Everything queued lives here; update this section as work lands.

### In progress / next

1. **Dynamic non-Surge CLAP loading & runtime lifecycle** — GitHub #2. Load arbitrary
   CLAP instruments and effects (beyond the startup Surge pool):
   - Two-phase runtime loader (bg `PluginEntry::load` → UI/main-thread `new` +
     optional pre-`activate` `load_state` + `activate` → command-ring handoff to
     `GraphEngine::voices`), scanning `CLAP_PATH` + `/usr/lib/clap` + `~/.clap`.
   - Safe main-thread retirement: audio thread calls `stop_processing()` and
     returns removed voices via a bounded retirement ring for `deactivate()`/`destroy()`.
   - Sandboxed out-of-process plugin scanning (`--scan-clap` with timeout).
   - Audio-thread hardening: `SIGXCPU` overrun guard (`guard_current_thread` +
     `OVERRUN_REST`), `catch_unwind` around `process()`, `restart_requested`
     reconfiguration, `clap_event_transport` (BPM/play state) + sorted timestamps,
     and main-thread `on_main_thread` callback pumping.
2. **Backing tracks** — GitHub #1. Deck UI + YouTube/URL sources (see backlog).

### Recently landed

- **Per-instrument transport + cleanup round** (2026-11-15, Opus 5.5 delegates,
  157 tests + session/metro/multi-rig/rig-mix self-tests OK): metronome nodes
  get a face Play/Stop chip + beat LED, per-node clock in the engine, global
  transport = master fan-out, Node.playing persisted, assistant/MCP
  set_node_playing parity (23 MCP tools). Flip pill REMOVED (Tab flips, help
  documents it); help overlay redesigned (two columns, key chips, brand mark);
  matrix typography fixed (18px header, no-wrap columns); ON-chip cream
  softened (CHIP_ON #E6E4DC). Lead fixes: assistant remove_node parity (the
  agent could not delete the duplicate metronome — MCP already had it),
  Stack A/B labels in single-rig mode + clearer one-rig hint + matrix header
  no false "rig A". Screenshot-verified: help/matrix/front/back (ksalat-clean2).
  In flight: audio-chain viz + key-LED snap + label shrink-to-fit + no-flash
  on agent edits (Brief C). Queued: split-as-keyboard-property with two MIDI
  outs (D), rig overview bar with mini layouts + cycling numbers (E).
- **Cable re-plug + rig-switch fixes** (2026-11-15, Opus 5.5 delegate, 142
  tests): dropping a held plug on a compatible jack MOVES it (release_plug/
  replug_cable, Disconnect→Connect via publish_patch); Drag/Hit hold cable
  ids (rule 10); Shift+drag fans out; refusals surfaced. Rig keys 1–9/←→ were
  killed by focused vol/pan DragValues (egui_wants_keyboard_input gate) —
  fields now drag_only, gate = text_edit_focused (rule 12), one-rig hint.
  **Live XTEST proof**: plug dragged Delay A IN → Delay B IN, moved and stuck
  (before/after screenshots). Crafted logo landed: MarkStyle Brand/Mono,
  woven crossing + plug hardware ≥32px, dezent 16px Mono masthead mark,
  SVG/painter geometry-parity test, transparent icon corners. Rules 12–13.
- **UI evolution pass — dark chrome, logo, buttons, cable salad** (2026-11-15,
  user-directed; orchestrator mode: lead briefs/reviews, Claude subagents
  implement): **all-dark chrome** (user pivot: "the whole background dark, not
  white" — theme token values flipped, rack one surface step above the window
  base); **crossed-wires brand mark** (theme::crossed_wires + assets/logo.svg,
  masthead/help card/window icon, transparent-corner RGBA); **real chip
  buttons** replace all underlined-text toggles/nav/selectors (ON = cream +
  dark text, explicit per-state RichText colors — override_text_color gotcha);
  **family-safe cable palettes** (audio warm: cream/amber/rose/sage · MIDI
  cool: blue/cyan/violet, rig-relative id key, warming shift only on cream);
  assistant **get_graph** tool (parity gap: it could not inspect → could not
  wire). AGENTS.md: screenshot-evidence requirement + "every fixed error
  becomes a rule" (10 rules). Spec overrides: docs/design-reference-analysis.md
  §0. Verified: 125 tests + screenshots (front/back/help/playing/transport).
  **Batch 2 landed & verified** (Opus 5.5 implementer, 127 tests): inline
  matrix main-stage view (F11/Tab/Esc guards, rule editor inline), assistant
  as toggleable wide bottom panel (F12, hidden by default), watermark removed,
  chip/box padding + global install_spacing. Screenshot-proven in the running
  app. In flight: plug re-plug fix (root cause: release handler never
  re-plugged; drag state held stale indices) + crafted logo redesign (woven
  crossing, plug hardware — user: "quite lazy, needs to be better"; steered:
  dezent ≤16px masthead variant). DeepSeek-vision verdict on batch 2: 6/10 —
  findings routed: flip-pill removal + matrix typography + chip softness into
  the cleanup brief; teal-as-live-semantics defended (documented, keep); rack
  module spacing logged for a later pass. Queued next round: flip-pill
  removal + help redesign (brief A), per-instrument transport (brief B,
  production DAW requirement).
- **Per-rig mixer + rule publication** (2026-11-15): overview strip now has
  vol/pan/mute/solo per rig (`Command::SetRigMix`, constant-power pan with unity
  center, solo mutes non-solo rigs, applied at each rig's MasterOut; resets on
  loads). `rig-mix-test` self-test verifies mute/solo/vol/pan on real Surge voices.
  GraphPub now carries the matrix rules and bumps on MatrixRule/SetSplit, so the
  UI mirrors **remote** rule edits too (parity gap closed). MCP/assistant
  `set_rig_mix` tools. 125 tests.
- **Matrix rule editing** (2026-11-15): right-click any matrix cell → live rule
  editor (channel filter, transpose ±24, key/velocity ranges with note names,
  enable/mute, reset). `Command::MatrixRule` applies in place (creates missing
  rules) and releases held notes on the destination (stateless-matrix safety,
  docs/midi-matrix.md). Non-default rules show a tomato corner dot + summary
  tooltip. Assistant `set_matrix_rule` + MCP `set_matrix_rule` parity. CC maps
  still await MIDI-learn. Verified: 123 tests (engine: in-place edit, reversed-
  range normalization, transposed/filtered routing, rule creation; UI: tool
  round-trip) + live render trace. Known gap: rule indicators mirror UI/assistant
  edits only (see next #3).
- **MCP→UI live sync** (2026-11-15): engine publishes `GraphPub {rev, graph}` on
  structural ops that actually change the graph (loads/add/remove/connect/disconnect;
  duplicate Connects do NOT bump rev). UI polls per frame and adopts the engine graph
  only when it diverges from its own mirror — UI-originated edits converge, so no
  rebuild loops and cable animations survive. Remote loads also reset per-rig source
  filters from rig meta. Verified live: bridge AddNode + LoadGraph adopted by the
  running UI (get_graph mirror + screenshots). 121 tests.
- **Hurricane design polish** (2026-11-15, user-requested "it looks unpolished → fix"):
  shadowless rack/masthead/help (shadows only on floating windows + drag chip); OPEN
  masthead — 30px Medium wordmark + asterisk directly on paper, no box; plates = the
  chassis near-black family with faint hairline rules easing to ACCENT on hover (~120ms);
  CREAM_MID #B4B4B0 rack micro-labels (crisp contrast); near-sharp corners everywhere
  (sheets/controls r2, windows r4, hero r36, ONE capsule pill CTA with inset keyline);
  36px BPM readout; roomier top strip (40px, no caption clutter, underline toggles —
  no dots) + rack spacing (pad 20/gap 12); assistant input = recessed hairline field;
  staged fade-in on load. DeepSeek-vision verdict: **8.5/10, "a deliberate, polished
  instrument in the reference's language"** (was "really unpolished"). Deliberate
  retentions vs the reviewer's notes: MIDI blue (audio≠MIDI live semantics, brief §3)
  and the pill's inset cream keyline (the reference's own signature CTA treatment,
  brief §2.3). Before/after: docs/shots/polish-before-after.png.
- **Hot-wire MIDI matrix** (2026-11-15): F11 sources×rigs grid, live cell toggling
  (cable + source filter + device radio), LV2 instrument columns, assistant
  get_matrix/set_matrix_cell. 118 tests. See §0.1.
- **Multi-rig UI** (2026-11-15): overview strip (name/device/level/active underline),
  1–9/←→/click view switching (visual only, all rigs sound), per-rig grouping +
  draw/hit filtering, held-key drain on focus loss, F4 two-rig demo (LoadRigs),
  assistant `multi_rig` template, `rig_snapshot.active` sync. 112 tests.
- **Hurricane visual redesign** (2026-11-15): see §0.1 + docs/design-reference-analysis.md.
- **Native egui plugin param editor — NO X11** (double-click a unit; 775 Surge params
  grouped into modules, searchable, RT-safe sliders).
- **Multi-rig backend** (Rig/merge/LoadRigs/per-rig device binding/rig snapshot).
- **Metronome as a normal rack node** · **Ctrl+drag rack rearrange** · **MIDI latency
  reporting** · **Master MONO input** · LV2 instrument hosting + plugin browser.

### Backlog (after the above)

- **Backing tracks: local files, YouTube, URL sources + deck UI** — GitHub #1.
  Engine player exists (src/player.rs, assistant play_file only); needs a deck UI
  (portal file picker, seek/loop/gain, drag-drop), yt-dlp-backed YouTube/URL audio
  (optional dependency, ToS posture documented), and a wireable source node. RT
  discipline: workers + lock-free rings, the existing player is the template.
- **LV2 instrument audible playback** — GitHub #3. Routing verified; confirm sound
  with a real MIDI synth; fix the permissive port-based classification if needed.
- **MIDI matrix: CC maps + MIDI-learn** — GitHub #4. Engine cc_map exists; expose in
  the rule editor with learn mode; fixed-size/side-channel command design.
- **Sends / aux buses** — GitHub #5. Dedicated send controls (free wiring already works).
- **Recording** — GitHub #6. Synchronized master capture (and per-rig stems).
- **Tap tempo + count-in** — GitHub #7.
- **P4 packaging** — GitHub #8. PKGBUILD + `.desktop` + icon, README screenshot
  (docs/shots/hurricane-*.png), first-run polish, Omarchy theme accent following.

### Known limits / caveats

- CLAP voice pool is 2 Surge voices (main-thread-only); >2 CLAP synths awaits dynamic
  loading. LV2 instruments can extend this.
- LV2 instrument classification is a permissive port-based hint (the authoritative LV2
  class is a blank node lilv can't resolve here).
- Matrix rules (incl. CC maps / MIDI-learn bindings) are not saved in sessions yet:
  `Session::matrix` exists but `capture_session`/`apply_session` ignore it (#4 review).
- A cold CLAP scan (first launch, or Refresh with a hanging bundle) runs on the UI
  thread: up to 5 s per wave of 4 hanging bundles. Move to a background thread.
- `transport-test` flaked once in 11 runs (Surge step 4 instead of 2); cause unknown.
- CLAP host hardening probes (2026-11-16): across host-test, audio-test and the
  Dexed/Vitalium/Surge tests no plugin called `request_restart` or logged; Dexed
  issues `request_callback` on the main thread only (already pumped). Restart
  handling and an RT-safe log ring wait for a plugin that needs them.
- System-audio capture of the output monitor can feed back if kabelsalat plays to the
  same sink (muted by default).

### Research

- **vibez** (`~/src/vibez`, Rust + iced) — study its multi-instrument / MIDI routing /
  session approach and adapt what fits (docs/vibez-notes.md).
