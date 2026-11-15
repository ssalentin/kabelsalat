# kabelsalat — modern design language ("Signal")

Goal: move from the generic flat Tokyo-night look to a distinctive, luminous
control surface where **audio is light**. Dark, layered, glassy, with electric
gradient accents and glowing signal paths. Reference direction: modern AI/fintech
dashboards (deep space backgrounds, glass panels, neon data-viz, refined type).

This is an egui app — everything is painted with `egui::Painter` (meshes for
gradients, layered strokes for glow). No CSS. Keep it RT-safe (paint only).

## Palette (commit to it; derive from the subject)

Background / neutrals (cool, layered — never flat gray):
- `BG_DEEP   #070A12`  window base
- `BG_RAISED #0C1120`  raised surface
- `SURFACE   #121A2E`  glass panel base
- `SURFACE_HI#1B2740`  panel top (gradient lighter end)
- `SCRIM     #05070D`  overlays/help backdrop
- `TEXT      #E6EDF7`  primary
- `TEXT_DIM  #7C8AA5`  secondary / labels
- `HAIRLINE  #23304C`  default border

Signal accents (full chroma, separable by luminance too):
- `AUDIO_CYAN  #22D3EE`  audio signal A / primary accent
- `AUDIO_BLUE  #38BDF8`  audio signal B
- `MIDI_AMBER  #FBBF24`  MIDI (warm, clearly not audio)
- `ACCENT_VIOLET #A78BFA` selection / highlights
- `ACCENT_MAGENTA #F472B6` special / record
- `LIVE_GREEN  #34D399`  signal-active / success / playing
- `CLIP_RED    #F87171`  overload / danger
- `WARN_AMBER  #F59E0B`  warning

Rules: audio = cyan/blue family, MIDI = amber, selection = violet, active = green,
clip = red. Borders/icons/glows borrow these — nothing sits in flat default gray.
Every text/control pairing must stay readable (≥ 4.5:1 on its surface).

## Depth & atmosphere (layered, not flat)

1. **Background:** fill `BG_DEEP`; add two very-low-alpha radial aurora glows
   (cyan top-left, violet bottom-right) via a coarse mesh or stacked translucent
   circles; overlay the existing faint dot-grid in `HAIRLINE` at ~4% alpha.
2. **Glass panels (rack chassis, units, popovers):** vertical gradient
   `SURFACE_HI → SURFACE`, 1px border tinted with the unit's accent at ~35% alpha,
   a 1px top inner highlight (white ~6%), and a soft drop shadow (black ~35%,
   offset y+6, blur approximated by 2 stacked rounded rects). Corner radius 14–18.
3. **Active/selected unit:** accent edge glow (2–3 stacked strokes decreasing
   alpha) + slightly brighter surface.
4. **Cables:** keep the catenary + pulses, but render glow as 3 layered strokes
   (wide low-alpha → narrow full-color). Audio cables breathe with live level;
   MIDI pulses are amber sparks. Selected cable highlights its full path.

## Typography hierarchy

- Unit name: proportional **bold**, ~15–16px, `TEXT`.
- Section labels: small **uppercase**, letter-spaced, `TEXT_DIM`.
- Numeric readouts (BPM, latency, levels): **monospace**, accent-colored.
- Strong size contrast between header / label / value.

## Controls & feedback

- Knobs: arc track in `HAIRLINE`, value arc in the unit accent with a soft glow,
  indicator dot; drag shows a live monospace value chip.
- Buttons/toggles: glass fill, accent border on hover, clear on/off (filled accent
  vs dim), pressed depression.
- Meters: gradient fill (green→amber→red), peak-hold tick.
- Transport play: glowing green when running; beat dot pulses.
- Hover: subtle brighten + cursor change; everything interactive responds.

## Motion

- Keep the rack flip; ease cable pulses; hover/press transitions ~120ms;
  level meters smooth attack / slow release; staged fade-in on template load.

## Implementation notes

- Centralize tokens + helpers in a new `src/theme.rs` (palette consts, `glass_panel`,
  `glow_stroke`, `aurora_bg`, `value_arc`) so the painter stays consistent.
- Replace the old palette consts in `ui.rs` with the theme; migrate `draw_front`,
  `draw_back`, `draw_toolbar`, `draw_keyboard`, popovers, and the egui panels
  (rig_inputs, sessions, plugins, assistant) to the glass language.
- Keep contrast readable; keep all existing function (flip, cables, editor,
  sessions, transport) intact. Verify via build + functional tests + screenshots
  described in text (lead cannot view images).