# kabelsalat — feature backlog

Living queue. The north star is AUM's operating model: **multiple independent
rigs, each bound to its own MIDI device or audio source, all sounding at once,
freely wired and mixed** — with GarageBand approachability and Reason's rack
flip.

## In progress / next

- **Multi-rig UI** (the big one). Backend is **DONE**: Rig model + merge + `LoadRigs`
  + per-rig device binding + rig snapshot exposed to UI/MCP (`get_rigs`/`load_rigs`).
  Each rig is a full rack bound to its own MIDI input (organ / keyboard / EWI / synth
  controllers); all rigs sound simultaneously, mixed to master. **Remaining: the UI** —
  an always-visible overview strip (rig name + device + active highlight + level),
  **number keys 1–9 + arrow keys** to switch the big view (visual only), big view = the
  active rig's rack. Held keys must remember rig+pitch so switching doesn't orphan
  note-offs (see docs/vibez-notes.md).
## Recently landed

- **Signal visual redesign** (docs/design-language.md): `src/theme.rs` palette +
  glass_panel/glow_rect/aurora_bg/value_arc/gradient helpers; rack front/back, toolbar,
  keyboard, meters, knobs (live drag value chip), cables (3-layer glow, violet
  selection), help scrim, all egui panels/windows restyled; Noto Sans + JetBrains Mono
  with bold unit names; glowing-green transport play; meter gradient + peak hold.
  Verified: 107 tests + self-tests + pixel-analysis of front/back/transport/help
  screenshots (docs/shots/). Remaining motion polish: staged fade-in on template
  load, ~120ms hover easing on custom-painted controls.

- **Native egui plugin param editor — NO X11** (double-click a unit; 775 Surge params
  grouped into modules, searchable, RT-safe sliders). X11 embedding removed entirely.
- **Multi-rig backend** (Rig/merge/LoadRigs/per-rig device binding/rig snapshot; 107 tests).
- **Metronome as a normal rack node** (DspKind::Metronome wired to master; transport-driven).
- **Ctrl+drag rack rearrange** with auto-reflow (cables stay attached).
- **MIDI latency** in the stats bar + `get_status` (rolling-avg hw input delay).
- Fix unplayable low notes (absolute MIDI notes through the key path).
- Toolbar pill no longer overlaps the top menu.
- Master out: **MONO input** (port 2) centered to L/R (`mono-master-test`).
- Live latency reporting in `get_status` (buffer ms + xruns).
- Assistant patching tools (load_template / add_node / connect / disconnect).
- Dynamic `add_node` builds DSP/LV2 processors so added nodes sound.
- LV2 instrument hosting (atom/MIDI routing verified) + plugin browser.

## Backlog (next up)

- **Hot-wire MIDI channels on the go** — an AUM-style **MIDI-in vs sink matrix
  view** to patch any MIDI input (device + channel) to any accepting rig live.
  Prereq: the multi-rig overview. This is the defining AUM routing surface.
- **Dynamic non-Surge CLAP loading** — load arbitrary CLAP instruments (today the
  engine remaps its two startup Surge voices; other CLAP shows "coming soon").
  Needs main-thread instantiation + a voice-delivery side channel.
- **LV2 instrument audible playback** — routing is verified; confirm sound with a
  real MIDI synth (installed LV2 instruments are samplers/generators).
- **Per-rig volume/pan/mute/solo** on the overview strip (AUM mixer feel).
- **MIDI matrix rules in the UI** — transpose / key-range / velocity-range / CC
  maps exist in the engine; expose them per rig.
- **Sends / aux buses** — dedicated send controls (free wiring already works).
- **Recording** — synchronized capture of the master (and per-rig stems).
- **Tap tempo + count-in** for the metronome.

## Known limits / caveats

- CLAP voice pool is 2 Surge voices (main-thread-only); >2 CLAP synths awaits
  dynamic loading. LV2 instruments can extend this.
- LV2 instrument classification is a permissive port-based hint (the authoritative
  LV2 class is a blank node lilv can't resolve here).
- System-audio capture of the output monitor can feed back if kabelsalat plays to
  the same sink (muted by default).

## Research

- **vibez** (`~/src/vibez`, Rust + iced) — study its multi-instrument / MIDI
  routing / session approach and adapt what fits (in progress).