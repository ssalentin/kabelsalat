# kabelsalat — feature backlog

Living queue. The north star is AUM's operating model: **multiple independent
rigs, each bound to its own MIDI device or audio source, all sounding at once,
freely wired and mixed** — with GarageBand approachability and Reason's rack
flip.

## In progress

- **Multi-rig sessions** (the big one). Each rig is a full rack
  (keys→synths→FX→master) bound to its own MIDI input (organ / keyboard / EWI /
  synth controllers). All rigs sound simultaneously, mixed to master. An
  always-visible **overview** of every rig; **number keys 1–9 and arrow keys**
  switch which rig the big view shows (visual only — audio keeps running).
  - Engine: sum all rig masters — **landed** (`multi-rig-test`).
  - Backend: Rig model + merge + `LoadRigs` + per-rig device routing — in work.
  - UI: overview strip + switching + big active-rig view — queued.
- **Ctrl+drag to rearrange rack units** with auto-layout reflow — in work.

## Recently landed

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