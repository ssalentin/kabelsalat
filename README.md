<p align="center">
  <img src="assets/logo.svg" width="160" alt="kabelsalat logo — three sagging patch cables with signal pulses"/>
</p>

<h1 align="center">kabelsalat</h1>

<p align="center"><strong>Live audio/MIDI wiring, alive.</strong></p>

A live plugin host for Linux, written in Rust — [AUM](https://kymatica.com/apps/aum)'s
routing workflow with GarageBand's approachability. Wire plugins and audio sources
into a graph, route MIDI anywhere, and play live — while the signal flows through
cables that **sag, swing, and glow**.

> **Status: working dev preview.** Two playable channel stacks, embedded Surge
> editor, system-audio capture, and an MCP server are live. Still evolving — the
> plan and progress are public: [PLAN.md](PLAN.md).

## The idea

Linux has capable plugin hosts, but none combine AUM's live workflow with a modern
interface. kabelsalat is that gap:

- **Animated wiring is the signature.** Cables hang like real patch leads, MIDI
  notes launch visible pulses along them, audio cables breathe with their level,
  and dragging a node makes its cables swing and settle. You *see* your signal path.
- **Reason's rack flip** — labeled rack units up front; press **Tab** to flip the
  rack and patch the cables on the back. Cables swing, dangle, and beg to be grabbed.
- **GarageBand-inspired approachability** — playable starter templates, smart macro
  controls, slick 60fps interactions, instant sound. No manual required.
- **Agent-native** — a full MCP server gives AI agents 1:1 access to every feature,
  through the same control plane the UI uses. Plus a **built-in assistant**
  (DeepSeek, bring-your-own-key) that can inspect, play, and help set things up.
- **One window, no scatter** — plugin editors open embedded inside the app
  (AUM-style), never as disconnected windows.
- **System audio in the mix** — wire YouTube/browser playback straight into a
  channel (PipeWire monitor capture).
- **MIDI matrix** — route any source to any destination with channel filters,
  transpose, and key-range **splits & layers** for live playing.
- **Mixer channels** — source nodes, insert FX (pre/post fader), sends, and buses.
- **CLAP-first plugin hosting** (Surge XT is the reference plugin), LV2 next.
- **Built for ARM64 too** — developed on Asahi Linux (M1), native plugins only.
- Follows the [Omarchy](https://omarchy.org) desktop theme, keyboard-first.

No timeline, no arranger. The graph is the instrument.

## What works today

- **Data-driven rack** — a graph of nodes wired by physical, animated cables.
  **Tab** flips front (perform) ↔ back (patch bay). Starts as two channel stacks
  (Surge + Delay each) mixed to a shared master; the layout adapts to any patch.
- **Starter templates** — **F5** Lead+Reverb, **F6** Drums+Bass, **F7** Ambient
  Pad, **F8** demo. Instant, playable starting points (GarageBand-style).
- **Play immediately** — laptop keyboard (A W S E D… row), on-screen keys, or a
  hardware MIDI keyboard (auto-connected). Native Surge XT voices.
- **Per-rig MIDI routing** — choose which sources (keys/laptop/hardware) and which
  specific device feed each rig; split the keyboard across rigs; the MIDI matrix
  handles filters/transpose.
- **CLAP + LV2 hosting** — Surge XT (CLAP) plus LV2 effects in the live mix, with
  knob control. **F9** opens a plugin browser to add them; **F2/F3** embed the
  Surge editor *inside* the main window (X11 reparenting), not a floating window.
- **Real built-in DSP** — 8 allocation-proven nodes (gain, pan, 3-band EQ, resonant
  filter, lookahead limiter, saturation, delay, reverb).
- **System audio + backing tracks** — capture your output's monitor (YouTube/
  browser) into the mix, and play WAV file players.
- **Metronome** — BPM click track with play/stop/tempo control.
- **Sessions** — save/load patch + knobs + plugin state, with a passing round-trip
  self-test; full-topology recall.
- **Built-in assistant** — DeepSeek, bring-your-own-key (stored locally `0600`),
  with tool access to play, route, split, save, capture audio, and more.
- **MCP server** — `kabelsalat mcp` (headless) or `kabelsalat mcp --attach` (drive
  the running app) exposes 17 tools, 1:1 with the control plane; `kabelsalat send`
  is a one-shot CLI client.
- Low-latency audio (PipeWire, 512-frame buffer ≈ 10.7 ms), native fractional-HiDPI.

Self-tests: `cargo run -- session-test`, `cargo run -- capture-test`,
`cargo run -- player-test`, `cargo run -- metro-test`, `cargo run -- lv2-test`,
`cargo run -- lv2-host-test`, `cargo run -- lv2-voice-test`,
`cargo run -- lv2-engine-test`, `cargo run -- audio-test`, `cargo run -- host-test`.

## Build & run

Requires Rust (rustup), ALSA development headers (`alsa-lib`), and PipeWire or
ALSA. Surge XT CLAP is the reference instrument (installed to `~/.clap`).

```sh
git clone git@github.com:ssalentin/kabelsalat.git
cd kabelsalat
cargo run        # opens the app: two stacks, play with your laptop keyboard
```

The app runs under XWayland (needed for embedded plugin editors and sharp
fractional-HiDPI rendering); it forces this automatically.

## Roadmap

| Phase | Milestone |
|---|---|
| **P0** ✅ | Spike: audio I/O, Surge XT CLAP hosted via [clack](https://github.com/prokopyl/clack), MIDI in, animated-cable prototype |
| **P1** 🔨 | Live core: two-stack rack + flip, multi-voice engine, DSP, MIDI matrix + splits, per-rig inputs, sessions, embedded editor |
| **P2** | Performance: file players, transport/metronome, synchronized recorder, MIDI-learn, system-audio capture ✅ |
| **P3** 🔨 | Ecosystem: MCP server ✅, MCP→GUI bridge, LV2 hosting, plugin browser |
| **P4** | Polish: scenes/setlist, theming, packaging for the app launcher |

Architecture, prior-art survey, and risk notes live in [PLAN.md](PLAN.md).

## The name

*Kabelsalat* is German for "cable salad" — the tangled mess behind every desk.
This app turns it into something beautiful.

## Inspiration & prior art

- [AUM](https://kymatica.com/apps/aum) (Kymatica) — the workflow this brings to Linux
- Reason (Reason Studios) — the rack flip: front to perform, back to patch
- GarageBand (Apple) — the approachability bar: instant sound, smart controls, playfulness
- [Surge XT](https://surge-synthesizer.github.io/) — reference instrument
- [clack](https://github.com/prokopyl/clack), [Maolan](https://github.com/maolan/maolan) — Rust plugin hosting
- [Element](https://github.com/kushview/element), [Carla](https://github.com/falkTX/Carla) — the existing Linux hosts that showed us the gap
