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

- **Two channel stacks** (Surge A + Delay A, Surge B + Delay B) mixed to a shared
  master, on a four-column rack: KEYS | stack A | stack B | MASTER. **Tab** flips
  front↔back; the back is a live patch bay with physical, animated cables.
- **Play immediately** — laptop keyboard (A W S E D… row), on-screen keys, or a
  hardware MIDI keyboard (auto-connected). Two native Surge XT voices.
- **Per-rig inputs + keyboard splits** — choose which sources (keys/laptop/hardware)
  feed each rig; split the keyboard at C4 (or any note) across the two rigs.
- **Embedded Surge editor** — press **F2** / **F3** to open Surge A/B's editor
  *inside* the main window (X11 reparenting), not a floating window.
- **Real built-in DSP** — 8 allocation-proven nodes (gain, pan, 3-band EQ, resonant
  filter, lookahead limiter, saturation, delay, reverb).
- **System audio in the mix** — capture your output's monitor (YouTube/browser)
  and mix it in (`system_audio`), muted by default.
- **Sessions** — save/load your patch + knobs + plugin state (`save_session` /
  `load_session`), with a passing round-trip self-test.
- **Built-in assistant** — DeepSeek, bring-your-own-key (stored locally `0600`),
  with tool access to play, rewire, split, save, and capture system audio.
- **MCP server** — `kabelsalat mcp` exposes 14 tools, 1:1 with the control plane.
- Low-latency audio (PipeWire, 512-frame buffer ≈ 10.7 ms), native fractional-HiDPI.

Self-tests: `cargo run -- session-test`, `cargo run -- capture-test`,
`cargo run -- audio-test`, `cargo run -- host-test`.

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
