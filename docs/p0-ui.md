# P0 — Animated wiring demo (`ui-demo`)

The signature feature (docs/ARCHITECTURE.md §2) in Reason-rack form: **one window, two sides
of a mini rack.** Front = perform, back = patch. The cables are the show.

## Run

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="$HOME/.cache/ksalat-ui-target"   # keep /tmp (tmpfs) free
cargo run -- ui-demo
```

Native Wayland (winit/eframe 0.37, wgpu renderer), app id `kabelsalat`.
Opens on the **front** panel with the simulated band already playing.
Viewport: 1100×750 requested, maximized; the rack layout adapts to any
window size (displays/keyboard shrink or hide gracefully when narrow).

**HiDPI:** eframe's detected scale is trusted (1.6 on the fractional-scale
eDP-1). Only if detection reports ~1.0 do we fall back to the native 1.6 —
forcing a higher `pixels_per_point` makes elements oversized.

## The rack

| Unit | Role | Front | Back jacks |
|---|---|---|---|
| KEYS | MIDI source | clickable mini keyboard, RATE / VEL knobs | MIDI OUT |
| SURGE XT | synth (CLAP stand-in) | animated waveform display, TONE / LEVEL | MIDI IN, OUT |
| DELAY | insert FX | echo-dot display, TIME / MIX | IN, MIDI, OUT |
| MASTER OUT | stereo bus | live LED meter, VOLUME / WARM | L, R |

Demo patch (pre-wired on the back): Keys→Surge (melody), Keys→Delay (tap),
Surge→Delay, Delay→Master L, Surge→Master R (dry). MIDI cables are yellow
`#FFD60A`; audio cables alternate blue `#7aa2f7` / green `#9ece6a` — the
`assets/logo.svg` palette on canvas `#15161e`.

## Controls

**Everywhere**

| Input | Action |
|---|---|
| `Tab` (or the *flip rack* toolbar button) | flip the rack, front ⇄ back |
| `Space` (or the yellow play button) | play / pause the simulated 96 BPM band |
| `?` / `F1` | help overlay · `Esc` closes / cancels |
| `D` | fps / cable / pulse stats · `R` reset all knobs |
| `Del` / `Backspace` | unplug the selected cable |

**Front (perform)**

- Drag a knob vertically to change it (right-click resets one knob).
  Knobs really drive the simulation: RATE = pattern speed, VEL = pulse
  velocity, TONE/WARM = cable color warmth, LEVEL/MIX/VOLUME = cable levels
  and the master meter.
- Click the piano keys on KEYS to fire pulses down the MIDI cables — you
  immediately see the signal path light up all the way to the master meter.
- Click any unit body to "ping" it (Keys fires a 4-note burst).

**Back (patch)**

- **Drag jack → jack** to patch (or click jack, then click a matching jack).
  Compatible jacks glow and pulse while a wire is pending; `Esc` / right-click
  cancels. One cable per input; outputs can fan out.
- **Grab a cable** anywhere and shake it — damped-spring physics, it swings
  and settles. Tug near an end to yank that plug out.
- **Drag a plugged jack** to pull its plug; release far from the jack to
  unplug (the cable drops and fades), release near to snap it back.
- Click a plugged jack to unplug. Click a cable to highlight its **whole
  signal path** (upstream + downstream); everything else dims.

## Audio-integration seam (task-4 handoff)

The UI already emits note events — no render-path changes needed to add sound:

- `pub struct NoteEvent { note: u8, velocity: f32 }` — semitone (0 = C),
  normalized velocity.
- `pub type NoteSink = Box<dyn FnMut(NoteEvent) + Send>`.
- `pub fn run_demo_with_note_sink(sink: Option<NoteSink>)` — same demo, with
  every interactive key press delivered to the sink (fire-and-forget; the
  render path never blocks on it). `run_demo()` = sink `None`.
- **Single note entry point: `DemoApp::fire_keys(semitone, amp)`** — lights
  the key, launches the visual pulses, and calls the sink. Route laptop keys
  and hardware MIDI through this one function and both sound *and* the cable
  animation follow automatically. On-screen key clicks already call it
  (`Hit::Key` in `handle_input`).
- The simulated band (`MELODY`/`TAP` scheduler in `update`) is visual-only;
  route it through the sink too if the demo should auto-play audibly.

## How the cables work

- Each cable is a cubic Bézier. Its two control points are **damped-spring
  particles** (k = 130, ζ ≈ 0.3, semi-implicit Euler at ≤ 1/240 s substeps)
  anchored to a rest shape: distance-aware downward sag
  (`18 + 0.20·dist`, capped) plus a lateral bulge for vertical runs, so
  rack cables loop outward like real patch leads.
- Endpoint motion feeds inertial impulses into the particles → dragging a
  plug or shaking a cable whips it; the springs settle it with a few visible
  oscillations. Plugging drops the cable in with a bounce + port flash;
  unplugging detaches the end and lets it fall with gravity and fade.
- **Pulses**: a swung-eighth 96 BPM pattern (accents + a rest) launches dots
  that travel by *arc length* (fixed 49-sample LUT per cable), with a bright
  de Casteljau sub-curve trail. Arrivals flash the destination unit's LED and
  attack the envelopes of its outgoing audio cables.
- **Audio breathing**: per-cable envelope (fast attack, ~0.3 s release) plus
  a slow idle shimmer; level modulates stroke width, glow and sag slightly.
- **Flip**: `Tab` runs a 0.55 s squash-swap-expand (ease-in cubic →
  ease-out-back overshoot). All rack geometry passes through a scale
  transform; text fades while squashed. Landing on the back gives every
  cable a small thump impulse so the rack feels like it physically arrives.

## Performance notes

- **No per-frame allocation storms**: cables render as
  `epaint::CubicBezierShape` (4 points, zero heap); pulse slots, LUTs and
  highlight sets are fixed arrays; all text galleys are cached (laid out once
  on the first frame — egui loads fonts only after the first `Context::run`);
  the only regular allocations are egui's internal shape buffers and a 4 Hz
  stats galley.
- **Modest tessellation**: LUT capped at 48 segments per cable, scaled by
  on-screen size (LOD); epaint flattens the Béziers adaptively in screen
  space. Background dot grid is capped at 2200 dots.
- Animation is decoupled from any audio callback: the demo is a pure
  simulation (in P1 it will read lock-free snapshots/rings only), so UI load
  can never affect sound.
- Continuous `request_repaint`, vsync-paced by the compositor (60 fps on this
  M1/Hyprland machine; `D` shows live fps). Debug build; release will be
  faster still.

## Verified (2026-11-14, Asahi M1 / Hyprland, eDP-1 2560×1600 @ 1.6)

- `cargo build`: clean; zero warnings from `src/ui.rs`.
- Window opens native Wayland (`xwayland: 0`, class `kabelsalat`), canvas
  pixel-sampled at exactly `#15161e`.
- Front panel pixel-verified: mini keyboard (~50k white-key pixels), knob
  arcs, unit label strips, live master meter, toolbar (play + beat dot +
  flip button), hint bar.
- `Tab` flip verified both directions (front → back → front); back panel
  pixel-verified: jack sockets + plugs and all three cable colors
  (yellow ≈ 7.7k px, blue ≈ 1.6k px, green ≈ 1.9k px in a 2560×1600 capture).
- Adaptive layout checked at 1600×1000 and at a 781×950 tiled window.
- Note: Hyprland may tile/maximize per its own rules; the dev-preview
  harness manages the final window placement.

## Known limits (P0 scope)

- Simulated band only — real MIDI/audio snapshot wiring lands in P1.
- 4 fixed units, 64-cable cap, one cable per input (AUM-style fan-out on outs).
- No session save; this is the feel prototype for the go/no-go gate.