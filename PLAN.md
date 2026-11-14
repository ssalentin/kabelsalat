# kabelsalat — live audio/MIDI wiring, alive

**An AUM-style live plugin host for Linux, in Rust — where the cables are the show.**

Status: P0 spike · Name: `kabelsalat` (binary = package = `.desktop` = icon = Wayland app id) · Location: `~/Work/kabelsalat`

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

## 1. Goal

A Linux-native application where you wire together plugins and audio sources in a
graph, route MIDI anywhere, and play live with low latency — AUM's workflow on the
desktop: mixer channels with insert FX and sends, a MIDI matrix with
splits/layers/filters, file players for backing tracks, synchronized recording,
built-in processing nodes, session save/restore. **No timeline.** The graph is the
instrument — and it moves.

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

Prototype in P0 (demo graph), full implementation on the real graph in P1.

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
5. **Plugin UIs:** CLAP GUI on Linux is X11 → XWayland under Hyprland. Own top-level
   windows first (Carla-proven); canvas embedding later.

## 7. Architecture

```text
crates/ (P1 split; P0 lives in a single spike crate)
  kabelsalat-app/      # egui: graph canvas + animated cables, mixer, matrix, keyboard
  kabelsalat-engine/   # graph → RenderPlan, transport, scheduler, latency comp
  kabelsalat-host/     # clack CLAP (later lilv LV2, VST3); scan, state, GUI windows
  kabelsalat-dsp/      # built-in nodes (fundsp + custom)
  kabelsalat-io/       # cpal duplex, midir, file streaming, recorder workers
  kabelsalat-project/  # session model, serde/RON, undo commands
src/main.rs            # clap CLI: `kabelsalat open [session]`, headless --json cmds
```

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
- **One render path** for realtime and offline bounce.
- **Sessions:** `name.ksalat/` package: `session.ron` + plugin state + copied assets;
  atomic temp-then-rename; originals never modified.

## 8. Roadmap

### P0 — Spike (3–5 days) · go/no-go gate
- cpal duplex through PipeWire, no xruns over 5 min; latency numbers
- **Surge XT CLAP via clack-host: play notes → sound**; GUI in own window; state round-trip
- midir input (hardware or virtual port) + egui on-screen keyboard
- **Animated cable prototype:** demo graph with sagging splines, traveling pulses,
  swing-on-drag — prove the signature feel
- In parallel: Element/Carla remain installed as the functional baseline
- **Done when:** you can play Surge live through our spike, and the cables feel alive.

### P1 — Live core (~2–3 weeks)
- Real graph canvas with full animated wiring; node browser; add/remove/wire
- Channel strips, buses/sends, hardware I/O; built-in gain/pan/EQ/filter/limiter
- MIDI matrix with splits/layers; sessions + undo
- **Done when:** two Surge layers split across the keyboard, one through EQ + reverb
  bus — save, reopen, play live.

### P2 — Performance (~2 weeks)
- FilePlayer, transport + metronome + count-in; recorder (master + stems)
- Latency compensation; MIDI-learn
- **Done when:** live set over a backing loop, recorded cleanly.

### P3 — Ecosystem (~2–3 weeks)
- LV2 hosting (lilv); plugin scan/browser/presets; remaining built-in nodes
- **Done when:** mixed CLAP + LV2 session stable for an hour.

### P4 — Live polish & packaging (~1–2 weeks)
- Scenes/setlist, autosave; Omarchy theme following; keyboard-first (`Space`
  transport, `?` overlay, `Q` quit-with-confirm); PKGBUILD + `.desktop` + icon;
  headless CLI; README with screenshot
- **Done when:** installed from the launcher, gig-ready.

**Later:** Ableton Link · MIDI clock · OSC · VST3 · out-of-process hosting ·
embedded plugin UIs · multichannel · MPE · 96 kHz.

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
