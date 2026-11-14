<p align="center">
  <img src="assets/logo.svg" width="160" alt="kabelsalat logo — three sagging patch cables with signal pulses"/>
</p>

<h1 align="center">kabelsalat</h1>

<p align="center"><strong>Live audio/MIDI wiring, alive.</strong></p>

An [AUM](https://kymatica.com/apps/aum)-style live plugin host for Linux, written in
Rust. Wire plugins and audio sources into a graph, route MIDI anywhere, and play
live — while the signal flows through cables that **sag, swing, and glow**.

> **Status: early development (P0 spike).** Not usable as a host yet.
> The plan is public: [PLAN.md](PLAN.md).

## The idea

Linux has capable plugin hosts, but none combine AUM's live workflow with a modern
interface. kabelsalat is that gap:

- **Animated wiring is the signature.** Cables hang like real patch leads, MIDI
  notes launch visible pulses along them, audio cables breathe with their level,
  and dragging a node makes its cables swing and settle. You *see* your signal path.
- **MIDI matrix** — route any source to any destination with channel filters,
  transpose, and key-range **splits & layers** for live playing.
- **Mixer channels** — source nodes, insert FX (pre/post fader), sends, and buses.
- **CLAP-first plugin hosting** (Surge XT is the reference plugin), LV2 next.
- **Built for ARM64 too** — developed on Asahi Linux (M1), native plugins only.
- Follows the [Omarchy](https://omarchy.org) desktop theme, keyboard-first.

No timeline, no arranger. The graph is the instrument.

## What works today

- P0 audio spike: device enumeration and a test tone through PipeWire, with
  callback/latency statistics (`cargo run`).

## Build & run

Requires Rust (rustup), ALSA development headers (`alsa-lib`), and PipeWire or ALSA.

```sh
git clone git@github.com:ssalentin/kabelsalat.git
cd kabelsalat
cargo run        # plays a short, quiet 440 Hz test tone
```

## Roadmap

| Phase | Milestone |
|---|---|
| **P0** | Spike: audio I/O, Surge XT CLAP hosted via [clack](https://github.com/prokopyl/clack), MIDI in, animated-cable prototype |
| **P1** | Live core: graph canvas, channel strips, MIDI matrix with splits/layers, sessions |
| **P2** | Performance: file players, transport/metronome, synchronized recorder, MIDI-learn |
| **P3** | Ecosystem: LV2 hosting, plugin browser, more built-in nodes |
| **P4** | Polish: scenes/setlist, theming, packaging for the app launcher |

Architecture, prior-art survey, and risk notes live in [PLAN.md](PLAN.md).

## The name

*Kabelsalat* is German for "cable salad" — the tangled mess behind every desk.
This app turns it into something beautiful.

## Inspiration & prior art

- [AUM](https://kymatica.com/apps/aum) (Kymatica) — the workflow this brings to Linux
- [Surge XT](https://surge-synthesizer.github.io/) — reference instrument
- [clack](https://github.com/prokopyl/clack), [Maolan](https://github.com/maolan/maolan) — Rust plugin hosting
- [Element](https://github.com/kushview/element), [Carla](https://github.com/falkTX/Carla) — the existing Linux hosts that showed us the gap
