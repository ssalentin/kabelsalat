# kabelsalat — design reference analysis: "Hurricane" (light editorial)

Target reference: **Hurricane Creative Agency Landing page** — Dribbble shot
<https://dribbble.com/shots/19623638-Hurricane-Creative-Agency-Landing-page>.
This document replaces the "Signal" dark-glass direction (docs/design-language.md),
which was based on a wrong reference. The user explicitly wants THIS reference's
direction: a **light, warm, editorial** design language.

**Method / provenance.** All visual facts below come from the DeepSeek vision API
(**DeepSeek-V4.1-Flash**, API model id `deepseek-flash`): three per-frame analyses
of `docs/design-ref/hurricane-{1,2,3}-api.jpg` (1600×1200), one verification pass
over a labeled contact sheet of 23 crops, and one gap analysis of the current app
screenshots (`docs/shots/signal-{front,back}.png`). Hex values marked **[measured]**
were additionally sampled pixel-exactly with ImageMagick from the lossless
3200×2400 originals (`hurricane-{1,2,3}.png`) and cross-checked against the vision
model's readings of the same crops; values marked **[derived]** are computed
variants (mixes/brightenings) introduced for contrast or app semantics. Frame 1 is
the straight-on view and the color ground truth; frames 2–3 show the same page in
tilted "paper flat-lay" presentation (the tilt/rotation is Dribbble staging, NOT
part of the UI language).

---

## 0. USER OVERRIDES — 2026-11-15 (authoritative over the sections below)

The user evolved the language after the light pass landed. These rules win
wherever they conflict with §1–§5 below (which remain the reference for
structure, typography, shape scarcity and the rack's internal painting):

1. **All-dark chrome.** The whole app lives on warm near-black surfaces
   (`PAPER #151412` window base, `PAPER_SINK #100F0E` recessed, rack
   `DARK_PANEL #1C1C1C` one step above). Text: `INK = CREAM #EFEEE9`,
   `INK_2 = CREAM_MID #B4B4B0`, rules `HAIRLINE #3A3835`. The legacy token
   NAMES are kept; their values are the dark side. No light paper anywhere.
2. **Brand mark = crossed wires** (`theme::crossed_wires`, assets/logo.svg):
   two S-curved patch cables crossing in an X (tomato over MIDI blue), plug
   caps with jack cores, cream pulse at the crossing. Replaces the asterisk
   everywhere (masthead, help card, window icon). No ambient watermark —
   decoration must never overlap panels (the old bottom-right mark was
   removed as clutter).
3. **Real buttons, no underlined text.** Top-strip toggles/nav, HW selector
   and rig selectors are chip buttons (`theme::chip_button`): ON = cream fill
   + near-black text, OFF = raised dark chip + hairline. Tomato stays
   reserved for the flip CTA, selection and primary actions. Explicit
   `RichText::color` per state (override_text_color would hide ON labels).
   Boxed text gets ≥8px/4px padding — never touch the boundary.
4. **Cable salad, family-safe.** Audio jackets: cream/amber/rose/sage (warm);
   MIDI jackets: blue/cyan/violet (cool) — `CABLE_AUDIO_PALETTE` /
   `CABLE_MIDI_PALETTE`, keyed on RIG-RELATIVE node ids (stable across
   reloads). Terminal shape (round/square) stays the primary audio≠MIDI cue.
   The TONE/WARM warming shift applies ONLY to the cream jacket. Selection
   still swaps to ACCENT and dims the rest.
5. **One window.** The MIDI matrix is a MAIN-STAGE view (Matrix chip / F11
   switches rack ↔ matrix; rule editor inline beside the grid), not a
   floating window. The assistant is a toggleable wide BOTTOM panel (chip /
   F12), hidden by default — chat text needs width, not a narrow column.
6. **Agent parity.** The assistant exposes `get_graph` (and every tool its
   prompt names) so it can inspect and wire the patch 1:1 with UI/MCP.

---

## 1. Reference summary (per frame)

### Frame 1 — straight-on hero (`hurricane-1-api.jpg`)
A warm off-white agency landing page on a neutral gray presentation canvas.
- **Nav:** logo left (orange six-spoke asterisk + "Hurricane" near-black +
  "creative" muted gray), centered text links (Work / Offices / About us /
  Contact), orange social icons right; 1px hairline under the header.
- **Hero:** oversized two-line headline "We create impactful / experiences story"
  (~108–112px at 1600w, medium weight, sentence case, tight tracking), left
  aligned; tiny right-aligned uppercase address block ("35 N DEARBORN ST /
  CHICAGO / IL 60601") creates extreme size contrast; 1px hairline divider below.
- **Support row:** circular portrait avatar + outlined circular arrow button,
  small body paragraph, big stat "950+" with orange plus and tiny orange
  "Project Completed" label; a **torn-paper collage** image (blue/orange/black/
  cream print fragments with cropped letterforms) occupies the center-right.
- **CTA:** oversized tomato-orange **pill** "Get Started!" with cream text and a
  cream asterisk, overlapping the collage's lower-right; thin cream inset keyline
  ~2px inside the pill edge.
- **Dark section:** a near-black card with **large rounded top corners (~38–44px)**
  begins below; two columns split by a 1px vertical rule; cream editorial copy
  left, orange asterisk + oversized lowercase "the hurricane" right.
- Adjacent cropped artboards show a service list with huge words ("Production",
  "Placement", "Strategies" in orange, "Research") in muted gray, and grayscale
  product photography (Nike) / storefront photos (Supreme: red awning, teal glass).

### Frame 2 — rotated flat-lay (`hurricane-2-api.jpg`)
The same page as layered paper sheets rotated ~12–14° over a cool gray canvas,
with soft shadows under the sheets. Additionally visible: an **"Our Areas of
Expertise" service list** — huge numerals (01, 02, 03…) + oversized service names,
1px hairline row separators, **the active row in orange**; rounded portfolio photo
cards (18–24px radius) with orange project titles and gray metadata; an organic
orange product card with a 3D can. Confirms: flat fills, no UI gradients, one
accent, hairline grid, pill CTA with inset double keyline.

### Frame 3 — tilted variant (`hurricane-3-api.jpg`)
Same page, ~8–10° tilt, cropped. Confirms the dark panel's internal layout
(cream body copy with **orange highlighted phrases**, 1px vertical divider,
orange asterisk, oversized cropped lowercase wordmark), the tracked-cap address
block, and ambient oversized pale-gray service typography in the background.
Palette/type/shape readings agree with frames 1–2.

### What the three frames agree on (cross-frame synthesis)
The **essence** of the Hurricane language, in 8 bullets:

1. **Warm paper, not white.** A bone/cream canvas (`#F4F3EE`) with near-black ink
   (`#1C1C1C`). Warm-neutral, editorial, print-inspired — never blue-tinted,
   never pure gray, never dark-mode.
2. **One hot accent, used sparingly but boldly.** Tomato-orange `#EF4825` is the
   only UI accent: logo asterisk, primary CTA, active list row, highlighted
   words, small labels, icon strokes, the "+" in stats. Everything else is
   ink/cream/warm-gray.
3. **Exactly one large inverted dark panel.** Near-black `#1C1C1C`, big radius
   (32–48px), holding the most important content, cream text + orange highlights.
   A single hard light/dark break — not six small dark cards.
4. **Flat fills, hairline structure.** No UI gradients, no glass/blur, no neon
   glow, no shadows on controls. Hierarchy comes from 1px hairlines
   (`#BEBEBE` on light, ≈`#868684` on dark), tonal steps, and generous negative
   space. The only shadow is a soft large-blur one under whole floating sheets.
5. **Extreme type-scale contrast.** One neo-grotesque family (Neue Montreal /
   Neue Haas Grotesk / Suisse feel): oversized display headlines at weight
   400–550, sentence case, tracking ≈ −0.035em, leading 0.92–1.05; against tiny
   uppercase micro-labels with wide tracking (+0.12…+0.16em). Numbers are large
   and medium-weight. No serif, no mono, no italics.
6. **Strict shape hierarchy.** Pages sharp (0–4px); dark panel and photo cards
   rounded (18–48px); **exactly one pill shape** (the CTA) with a ~2px inset
   cream keyline; circular icon buttons (outline 1.5–2px, or filled orange).
7. **Asterisk/spark motif** as the recurring brand mark (logo, CTA ornament,
   dark-panel decoration); editorial hairline grid; asymmetric layout; oversized
   cropped typography as ambient texture.
8. **Texture lives in imagery, not chrome.** Torn-paper collage, grayscale
   photography, print grain appear only inside image regions. The UI itself is
   clean flat paper.

**What makes it distinctive vs generic dashboards:** no grid of equal glass
cards; one hero inversion instead of pervasive dark mode; typography is the main
graphic element; single-accent discipline instead of a neon rainbow; print/
editorial devices (hairlines, tracked caps, big numerals, collage) instead of
glows and gradients.

---

## 2. Extracted design language

### 2.1 Palette (reference ground truth)

| Token (ref) | Hex | Provenance | Role in the reference |
|---|---|---|---|
| PAPER | `#F4F3EE` | [measured] page field, headline bg, nav bg | Page/canvas surface (warm off-white) |
| PAPER_2 | `#DAD9D4` | [measured] adjacent artboard (`#D2D1CD`–`#DAD9D5`) | Secondary sheet / darker paper context |
| INK | `#1C1C1C` | [measured] headline ink, stat numerals, dark panel | Primary text AND dark panel surface (same value) |
| INK_2 | `#646464` | [measured] nav `#656565`, body `#636363`, address `#616161` | Secondary text, captions, nav |
| MUTED | `#A7A29F` | [measured] logo "creative" | Decorative muted gray (never informational text) |
| HAIRLINE_LIGHT | `#BEBEBE` | [measured] 1px rules at nav & hero dividers | Dividers on paper (1px, with AA edge `#D9D8D6`) |
| DARK_PANEL | `#1C1C1C` | [measured] identical in all 3 frames | The inverted hero panel |
| CREAM_ON_DARK | `#EFEEE9` | [measured] dark-panel body text | Primary text on dark |
| HAIRLINE_DARK | `#868684` | [measured] vertical rule in dark panel (≈ cream @ 50%) | Dividers on dark (1px) |
| ACCENT | `#EF4825` | [measured] CTA fill, asterisks, stat plus, active row | The single tomato-orange accent |
| ON_ACCENT | `#FFFBFA` | [measured] CTA text pixels | Cream-white text/glyphs on orange |
| COLLAGE_BLUE | `#1C5A8B` | [measured] torn-paper blue band | Imagery only in ref; adopted for app MIDI (see §3) |
| STORE_TEAL | `#2E9E9B` | [measured] Supreme storefront glass | Imagery only in ref; adopted for app "live" (see §3) |
| AWNING_RED | `#D9251C` | [measured] Supreme awning | Imagery only in ref; adopted for app clip/danger (see §3) |

Vision-model estimates that the pixel measurements corrected: paper is more
neutral than the model's `#F6F1E8` guess; ink is `#1C1C1C` not `#151515`;
secondary grays are neutral (`#646464`) not warm-brown; the light hairline is
`#BEBEBE` (not `#D8D2C8`); the dark-panel divider is a **mid-gray ≈ cream@50%**
(`#868684`), not `#3B3B3B`. The contact-sheet verification pass confirmed the
system: *"light warm-neutral editorial theme built from cream, neutral grays,
near-black panels, and one tomato-orange accent"*; the only saturated blue is
imagery content, not a UI accent.

### 2.2 Typography rules
- **One family:** contemporary neo-grotesque (PP Neue Montreal / Neue Haas
  Grotesk / Söhne / Suisse Int'l feel). No serif, no mono, no italics anywhere.
- **Display/headlines:** weight 400–550 (medium, NOT bold), sentence case,
  tracking −0.035…−0.045em, leading 0.92–1.05. Hero ~108–150px at 1600w;
  dark-panel display 66–150px; service words 48–72px.
- **Body:** regular 400, 12–14px, leading ~1.25, sentence case.
- **Micro-labels:** UPPERCASE, 9–12px, tracking +0.12…+0.16em (address block,
  captions, "Project Completed").
- **Numbers:** large, medium weight (stat "950+" 54–84px); the plus/sign may be
  accent-colored while digits stay ink.
- **Nav/links:** plain text 14–16px, no boxes, no underlines; active item in
  orange. Logo wordmark ~18–24px medium with a muted second word.

### 2.3 Shape / radius rules (at 1600w)
- Page sheets: 0–4px (essentially sharp).
- Dark hero panel: **32–48px** (frame 1: 38–44; frame 3: 32–48).
- Photo/image cards: 18–24px (up to 30px on presentation tiles).
- CTA: **full pill** (radius = height/2), with a **~2px inset cream keyline**
  a few px inside the edge — the signature button treatment.
- Icon buttons: perfect circles, outline 1.5–2px stroke (orange or ink), or
  filled orange with cream glyph. Avatars: circles.
- No other pill/chip shapes exist in the reference — shape scarcity is deliberate.

### 2.4 Depth rules
- **Flat fills everywhere.** No UI gradients, no glass/blur, no glow, no bevels.
- **Borders:** only 1px hairlines (`#BEBEBE` light / `#868684` dark). Cards are
  borderless; structure is drawn with rules, not boxes.
- **Shadows:** only very soft, large-blur, low-opacity shadows under whole
  floating sheets (the presentation mockups). Controls, panels, buttons: none.
- **Texture:** paper grain / torn edges / photographic grain only inside imagery.

### 2.5 Accent usage rules
- Orange = "the important/active thing": primary CTA, active list row, highlighted
  words, logo mark, small labels, icon strokes, stat plus.
- Cream-white on orange for CTA text (large type); orange on dark for highlights;
  orange on paper for small marks — never large orange fields on paper.
- Everything else stays ink / cream / warm-gray. Unit or category identity is
  carried by **typography and position**, not by a rainbow of hues.

---

## 3. Adaptation to kabelsalat (light, faithful, functionally separable)

**Decision: go light.** The window becomes warm paper; the rack chassis becomes
the reference's single inverted dark panel (the rack IS the hero content). All
egui chrome (top strip, side panels, windows, popovers) lives on paper. This is
the reference's own logic — one hard light/dark inversion — applied to an audio
rack, and it matches the gap analysis' top-priority recommendation.

**Semantic separability (required).** The reference UI has only one accent, but a
live audio rack must keep audio≠MIDI cables and active≠selection≠clip states
distinct. We therefore adopt a **minimal, reference-sourced expansion**: every
extra hue is sampled from the reference shot's own imagery (collage blue,
storefront teal, awning red), kept muted/editorial, and assigned one job. No
neon, no glows — separability comes from hue + luminance + terminal shape.

| Semantic | Color | Source | Why distinct |
|---|---|---|---|
| Audio cables | cream `#EFEEE9`, **round** terminals | ref cream-on-dark | neutral "paper signal"; high luminance |
| MIDI cables | blue `#5B8FD0`, **square** terminals | collage blue `#1C5A8B` brightened for dark bg | hue + luminance + shape vs audio |
| Selection / drag / primary CTA | tomato `#EF4825` | ref accent | the reference's "highlight" meaning |
| Active / playing / signal-live | teal `#2E9E9B` | Supreme storefront glass | cool "on-air" LED, ≠ orange/red |
| Clip / danger / record | red `#D9251C` | Supreme awning | deeper, cooler red ≠ tomato accent |
| Warning | deep tomato `#C0391B` (paper) / `#EF4825` (dark) | accent darkened for text contrast | reads as "hot accent", text-safe |

Record/special (old magenta) merges into the red family — the reference has no
magenta, and record is conventionally red. Audio "B" (old blue) merges into the
single cream audio family; Surge's two rigs stay distinguishable by label.

### 3.1 Token mapping for `src/theme.rs`

Replace the Signal consts. Old name → new value(s); where a token served two
contexts (rack paint vs egui chrome), the role splits as noted.

| Signal const (old) | New token | Hex | Role |
|---|---|---|---|
| `BG_DEEP #070A12` | `PAPER` | `#F4F3EE` | window base, egui `panel_fill` |
| `BG_RAISED #0C1120` | `PAPER_SINK` | `#ECEAE3` [derived: paper darkened ~3%] | recessed fields, code bg, stats chip |
| `SURFACE #121A2E` | `DARK_PANEL` | `#1C1C1C` | rack chassis, help card (egui windows use `PAPER`) |
| `SURFACE_HI #1B2740` | `DARK_PANEL_HI` | `#232120` [derived warm whisper step] | unit plates, rails, plugs (egui widget fills use `PAPER`/`PAPER_SINK`) |
| `SCRIM #05070D` | `SCRIM` | `#1C1C1C` (still applied at α 0.62) | help backdrop |
| `TEXT #E6EDF7` | `INK` / `CREAM` | `#1C1C1C` on paper · `#EFEEE9` on dark | rack paint uses `CREAM`; egui `override_text_color = INK` |
| `TEXT_DIM #7C8AA5` | `INK_2` / `CREAM_DIM` | `#646464` on paper · `#8E8E8C` on dark | secondary text (both pass 4.5:1, §5) |
| `HAIRLINE #23304C` | `HAIRLINE` / `HAIRLINE_DARK` | `#BEBEBE` on paper · `#8E8E8C` on dark | 1px rules (dark value refined from measured `#868684` so it also passes as text) |
| `AUDIO_CYAN #22D3EE` | `AUDIO_CREAM` | `#EFEEE9` | audio cables/jacks on dark; primary-accent role on paper moves to `ACCENT` |
| `AUDIO_BLUE #38BDF8` | — (retired) | → `AUDIO_CREAM` | audio is one family now |
| `MIDI_AMBER #FBBF24` | `MIDI_BLUE` / `MIDI_BLUE_DEEP` | `#5B8FD0` on dark · `#1C5A8B` on paper | MIDI cables/jacks; paper variant also = hyperlink color |
| `ACCENT_VIOLET #A78BFA` | `ACCENT` | `#EF4825` | selection, drag, CTA, active rule, asterisk |
| `ACCENT_MAGENTA #F472B6` | — (retired) | → `CLIP_RED` | record/special joins red family |
| `LIVE_GREEN #34D399` | `LIVE_TEAL` / `LIVE_TEAL_DEEP` | `#2E9E9B` on dark · `#1F7A70` on paper | active/playing/beat/signal LED |
| `CLIP_RED #F87171` | `CLIP_RED` | `#D9251C` | clip graphics (meter top, plug-overload) |
| `WARN_AMBER #F59E0B` | `ACCENT_DEEP` | `#C0391B` | warning text on paper; on dark use `ACCENT` |

**New supporting tokens** (all [derived] from measured values):

| Token | Hex | Role |
|---|---|---|
| `ON_ACCENT` | `#FFFBFA` | text/glyphs on accent fills; on vivid `ACCENT` only at large-text sizes (§5) |
| `ACCENT_BRIGHT` | `#F1552E` | small orange **text** on dark plates (vivid accent is 4.29:1 there — graphics only) |
| `CLIP_TEXT` | `#B91C14` | danger **text** on paper |
| `CLIP_BRIGHT` | `#F0554A` | danger **text** on dark |
| `MUTED` | `#A7A29F` | decorative only (watermark, logo-style second word); never informational text (2.27:1) |
| `METER_TRACK` | `#353535` [measured blend: cream @ 12% on dark] | unlit meter segments |
| `WATERMARK` | `#E7E5DF` [derived] | optional ambient oversized asterisk/word on paper |

**Accent usage rule (port of §2.5):** vivid `ACCENT #EF4825` for graphics, fills,
rules, arcs, and large text; `ACCENT_DEEP #C0391B` for small filled buttons that
carry cream text and for warning text on paper; `ACCENT_BRIGHT #F1552E` for small
orange text on dark. One pill shape in the whole app (the flip CTA).

### 3.2 ui.rs local alias remap (lines 53–69)

```
BG→PAPER · YELLOW→MIDI_BLUE · BLUE→ACCENT · GREEN→LIVE_TEAL · PURPLE→ACCENT
ORANGE→ACCENT (cable warming) · CHASSIS→DARK_PANEL · RAIL→DARK_PANEL_HI
PLATE_FRONT→DARK_PANEL_HI · PLATE_BACK→DARK_PANEL_HI · BORDER→HAIRLINE_DARK
RING→CREAM_DIM(#8E8E8C) · PLUG→DARK_PANEL_HI · TEXT→CREAM · SUBTEXT→CREAM_DIM
WHITE→#FFFBFA · BLACK→#1C1C1C
```
(`PLATE_BACK` may stay one whisper darker: `#201F1E`, optional.)

---

## 4. Per-component instructions (`src/ui.rs` + `src/theme.rs`)

General: **delete every glow, gradient, glass and texture pass.** Concretely,
retire or rewrite these theme helpers: `gradient_rect` → flat `rect_filled`
(keep the function only if a caller still needs it; no panel should call it);
`glass_panel` → `flat_panel` (fill + optional 1px hairline + soft shadow);
`top_highlight` → delete calls; `glow_rect` → delete calls (replace with inset
keyline, below); `aurora_bg` → `paper_bg`; `drop_shadow` → re-tune (§4.1);
`value_arc` → flat variant (§4.6); `meter_color` → stepped (§4.8).
Add one new helper: **`inset_keyline(painter, rect, radius, color, width=1.5,
inset=2.5)`** = `rect_stroke(rect.expand(-inset), radius-inset, …, Inside)` —
the reference CTA's inner cream line; it becomes the universal focus/selection
treatment on dark, and the flip pill's signature detail.

### 4.1 Background & shadows
- `paper_bg`: flat `PAPER` fill. **Remove** the two aurora glows and the dot grid
  entirely. Optional ambient signature: one oversized 6-spoke asterisk (3 lines
  through a center at 0°/60°/120°, stroke `WATERMARK` 2px, radius ~140px)
  bleeding off the bottom-right of the window, or a cropped giant word
  "kabelsalat" in `WATERMARK`. Purely decorative, behind everything.
- `drop_shadow` (the ONLY shadow, under floating sheets): two stacked rounded
  rects, ink-based instead of black: `alpha(INK, 0.08)` expand 4 / offset y+8,
  then `alpha(INK, 0.10)` expand 2 / offset y+5. Apply to: rack chassis, toolbar
  card, help card. egui `window_shadow`: offset [0,8], blur 24, spread 0,
  `alpha(INK, 0.10)`; `popup_shadow`: [0,4], blur 12, `alpha(INK, 0.10)`.

### 4.2 Rack chassis (`draw`, lines ~3032–3058)
- Fill `DARK_PANEL`, **radius 36**, flat (no gradient), **no border**, no top
  highlight. Soft shadow per §4.1. This is the single inverted hero panel.
- Rails (`draw_rails`): `DARK_PANEL_HI` strips, radius 4. **Remove the screws.**

### 4.3 Unit plates — front (`draw_front`)
- Plate: flat `DARK_PANEL_HI`, **radius 8**, no stroke at rest (the whisper tonal
  step separates it from the chassis; plates stay discrete draggable objects).
  Hover: 1px `HAIRLINE_DARK` stroke. Drag: 2px `ACCENT` **inset keyline**
  (inset 2.5) — no outer glow.
- Active unit (led > 0.05): **no glow**. Instead the label strip turns accent and
  the micro-label brightens (below).
- Label strip: shrink from 7px to **2px wide**, same position (x+12, y+12,
  h−24), radius 1: color `ACCENT` when the unit is active or dragged, else
  `HAIRLINE_DARK`. (Reference: active service row gets the orange rule.)
- Name: Noto Sans **Medium 15px**, `CREAM`, **sentence case** ("Keys", "Surge A",
  "Delay A", "Master out") at (32,16). Subtitle: tracked micro-caps **10px**,
  `CREAM_DIM`, +0.14em, at (32,40).
- Activity LED (top-right): flat disc r 3.2, `LIVE_TEAL` at α 0.25+0.75·led;
  optional flat halo disc r 7.5 `LIVE_TEAL` α 0.15·led (no glow stack). When the
  unit is dragged/selected, LED color = `ACCENT`.

### 4.4 Unit plates — back (`draw_back`)
- Same plate treatment as front (fill `DARK_PANEL_HI`, radius 8, drag keyline).
- Back label: tracked micro-caps 10px `CREAM_DIM`.
- **Paint order fix:** plates → cables → jacks → plugs → pulses → **unit labels
  last** (move the `self.text(...back_g...)` call after `draw_pulses`) so cables
  never cross titles (a specific defect in the current back screenshot).

### 4.5 Label strips & micro-labels (global)
- All section/jack/knob captions: UPPERCASE, 9–10px, tracking **+0.14em**,
  `CREAM_DIM` on dark / `INK_2` on paper. egui has no letter-spacing API:
  implement `tracked_label(text, size, tracking_em, color)` by laying out
  per-character galleys with advance = char_width + tracking_em·size (cache the
  per-char galleys in `ensure_galleys`; paint-only, RT-safe). For display type,
  approximate −0.03em by advance = char_width − 0.03·size (clamp ≥ 0.9·char_width).
- `theme::section_label` → uppercase, 10px, tracked, `INK_2` (was small/strong).

### 4.6 Knobs (`draw_knob` + `theme::value_arc`)
- Disc: **unfilled** circle r 17, stroke **1.5px `HAIRLINE_DARK`**; hover: stroke
  `CREAM`. No fill bead, no gradient, no hover halo (delete the α 0.15 disc).
- Value arc (270° from 135°, geometry unchanged): track = `HAIRLINE_DARK` α 0.35,
  2px; value arc = solid `ACCENT` **2.4px, no glow pass** (delete the 5.5px
  α 0.18 stroke); end dot r 2.2 `ACCENT` (flat).
- Pointer: keep center→0.62r needle, **2px `CREAM`**.
- Drag chip: **paper sticker** — fill `PAPER`, radius 6, no border, soft shadow;
  value text Noto Sans **Medium 13px `INK`** (was mono cyan). Same position.
- Knob label: tracked micro-caps 10px `CREAM_DIM` (geometry unchanged).
- Active/dragging knob: 2px `ACCENT` inset keyline around the disc's bounding
  rect (reuse the CTA motif as the universal focus state).

### 4.7 Keyboard (`draw_keyboard`)
- Geometry, key count, hit areas: **unchanged**.
- White keys: flat `#FFFBFA`, radius 3, **1px `DARK_PANEL` stroke** (creates the
  hairline gap between keys). Hover: `#F0EDE6`. Sounding/lit: solid `ACCENT`
  fill (the reference's "highlight" moment; velocity still drives LED timing).
- Black keys: flat `#1C1C1C` + **1px `HAIRLINE_DARK` stroke** (so they read
  against the dark plate), radius 2.5. Lit: solid `ACCENT`.
- No gloss, no bevel, no shadow, no yellow mix.

### 4.8 Master meter (`draw_meter` + `theme::meter_color`)
- Keep 14 segments, geometry, peak-hold behavior. Segments: radius **1**, flat.
- `meter_color(t)`: **stepped, not gradient** — t < 0.6 → `CREAM`;
  0.6 ≤ t < 0.9 → `ACCENT`; t ≥ 0.9 → `CLIP_RED`.
- Unlit segments: `METER_TRACK #353535`. Peak tick: 2px, `ACCENT`, or `CLIP_RED`
  when peak ≥ 0.9.
- Optional editorial "big number": current dB/peak readout above the bar in
  Noto Sans Medium 16px `CREAM`.

### 4.9 DSP / Surge / Delay displays
- Surge bars (`draw_surge_display`): alternate `CREAM` / `CREAM_DIM`, radius 2,
  flat (no blue pair, no alpha breathing beyond what exists).
- Delay dots (`draw_delay_display`): `ACCENT` flat circles (was violet); keep
  sizes/animation.
- Generic DSP bar (`draw_dsp_display`): track `METER_TRACK`, fill `ACCENT`
  (α 0.5+0.5·level may stay — flat color, no glow).

### 4.10 Cables (`draw_cables`, `cable_color`)
- **One flat stroke per cable, round caps. Delete all layered glow strokes.**
- Audio: `AUDIO_CREAM`, width 3.0 + 1.0·level (keep level-responsive breathing).
  MIDI: `MIDI_BLUE`, width 2.8. Hover/grab: +0.8 width (keep).
- `cable_color` warming (TONE/WARM knobs): mix audio cream toward `ACCENT` by
  `tone*0.20 + warm*0.15` (subtle flat warm shift; keeps the knob feedback).
- Selection: selected cable = solid `ACCENT`, width +0.5; **all other cables drop
  to α 0.28** when a selection exists (keep `cable_alpha`). Delete the violet
  under-glow and the bright centerline — the color swap + dimming is the highlight.
- No dashed strokes anywhere (reference vocabulary = hairlines + inset keylines).

### 4.11 Pulses (`draw_pulses`)
- **Single flat dot**, r 3.5+2·amp, fill = cable color mixed 0.5 toward
  `#FFFBFA` (audio ≈ `#F7F6F2`, MIDI ≈ `#A8C4E8`). **Delete** the glow ellipse
  and the trail bezier. Silent channels carry no pulses (motion = information).

### 4.12 Jacks & plugs (`draw_jacks`, `draw_plugs`, `draw_pending`)
- Socket 24×24 r 5: fill `DARK_PANEL`, stroke **1.2px `HAIRLINE_DARK`** (flat, no
  gloss). Inner hole r 6.5: fill `#101010`, stroke 1.4px `HAIRLINE_DARK`.
  Center dot r 2.5: family color (`AUDIO_CREAM` / `MIDI_BLUE`).
- Compatible drop-target ring: **`ACCENT`** 1.8px (was kind-colored), keep the
  ±1.5px pulse. Hover disc α 0.16 family color (flat) may stay.
- Jack labels: tracked micro-caps 9px `CREAM_DIM`, painted last (§4.4).
- Plug-in flash rings: keep, family color, flat 1.6px.
- Plugs: body 20×16 fill `DARK_PANEL_HI`, stroke 1px `HAIRLINE_DARK`;
  band 16×3.5 in the cable family color. Flat. **Terminal shape per §3:
  audio body r8 (round capsule) + round jack dot; MIDI body r1 (square) +
  4.5px square jack dot** (lead resolution of the §3/§4.12 inconsistency).
- Pending cable: single flat stroke 2.8px family color (delete the 6.5px
  under-stroke); cursor dot r 6.5 + ring r 9.5 stroke 1.6 (keep).

### 4.13 Toolbar (`draw_toolbar`) → flat masthead card
Keep the floating-card geometry (376×48 at (16, top+16)) so hit tests stay valid;
restyle, don't relocate:
- Card: flat `PAPER`, **radius 14**, 1px `HAIRLINE` stroke, soft shadow (§4.1).
  No gradient, no top highlight, no blue tint.
- **Asterisk + wordmark:** draw a 6-spoke orange asterisk (3 lines through center
  at 0°/60°/120°, `ACCENT` 2px, radius 7) at ≈(28, top+40); wordmark "kabelsalat"
  Noto Sans **Medium 22px `INK`**, tracking −0.03em, at (80, top+22) — the
  editorial display moment. State line ("playing"/"stopped"): tracked micro-caps
  10px `INK_2` at (80, top+42); prepend a 6px `LIVE_TEAL` dot when playing.
- **Play button** (center (40, top+40), r 15 — keep hit area): idle = `PAPER`
  fill + 1.5px `INK` ring + `INK` triangle; running = **`LIVE_TEAL` fill + `INK`
  stop square** (ink-on-teal 5.26:1; cream would be only 3.15:1). Beat dot: `LIVE_TEAL`,
  keep thump animation. No green glow ring.
- **Flip button = the app's single pill:** rect (226, top+24, 150, 32), radius 16,
  fill `ACCENT`; **inset cream keyline** 1.5px `ON_ACCENT` α 0.9 at inset 2.5;
  label "flip rack · Tab" Noto Sans **Bold 19px `ON_ACCENT`** centered (19px bold
  = WCAG large text, so cream-on-tomato 3.63:1 ≥ 3.0 passes). Drop the drawn ⇄
  arrows (redundant; frees width). Hover: fill `mix(ACCENT, INK, 0.08)`.
  Update `Hit::Flip` rect to match. If the measured galley exceeds ~128px,
  widen the pill (right edge ≤ 376) or shorten the label to "flip · Tab" —
  keep the size ≥ 18.66px bold so the cream-on-tomato large-text rule holds.

### 4.14 Top egui strip (rig inputs / nav row in `ui()`)
- Paper background; **remove the chip pills** (Keys/Laptop/HW): plain text
  toggles 13px — on = `INK` with a 6px `ACCENT` dot before; off = `INK_2`.
- Nav items (Sessions / Plugins / Transport / Split): plain 13px `INK` text
  buttons separated by 1px `HAIRLINE` verticals; active/open = `ACCENT_DEEP`
  text + 2px `ACCENT` underline (reference active-row logic). No rings, no chips.
- Italic captions ("which sources feed each rig") → regular 12px `INK_2`.
  **No italics anywhere.**

### 4.15 Hint & stats bars
- `draw_hint`: **remove the pill container** → plain caption line, 12.5px `INK_2`,
  sentence case, at (16, bottom−24). No fill, no border.
- `draw_stats`: keep a small telemetry chip (functional exception): `PAPER_SINK`
  fill, radius 8, 1px `HAIRLINE`, **JetBrains Mono 11px `INK_2`** (the only mono
  left in the app — telemetry, not editorial content).

### 4.16 Help overlay (`draw_help`)
- Scrim: `alpha(SCRIM, 0.62)` (ink over paper → warm darkening).
- Card 560×352: `DARK_PANEL`, **radius 28**, soft shadow, no border — a second
  appearance of the hero-panel motif, acceptable for a modal.
- Title: Noto Sans Medium 22px `CREAM` + orange asterisk before it; body: 14px
  `CREAM` (key names may be `ACCENT_BRIGHT`); footer: tracked micro-caps 10px
  `CREAM_DIM`. Replace `glass_panel` with flat fill.

### 4.17 egui shell (`theme::visuals()`)
Start from `egui::Visuals::light()`:
```
panel_fill = PAPER            window_fill = PAPER
window_corner_radius = 14     window_stroke = 1px HAIRLINE
window_shadow = [0,8] blur 24 alpha(INK,0.10)
popup_shadow  = [0,4] blur 12 alpha(INK,0.10)
override_text_color = INK     hyperlink_color = MIDI_BLUE_DEEP
selection.bg_fill = alpha(ACCENT,0.25)   selection.stroke = 1px ACCENT
extreme_bg_color = PAPER_SINK code_bg_color = PAPER_SINK
warn_fg_color = ACCENT_DEEP   error_fg_color = CLIP_TEXT
widgets corner_radius = 8:
  noninteractive: bg PAPER, weak PAPER, fg 1px INK_2
  inactive:       bg PAPER, weak PAPER_SINK, fg 1px INK, bg_stroke 1px HAIRLINE
  hovered:        bg PAPER_SINK, weak #E4E2DA, fg INK, bg_stroke 1px MUTED
  active:         bg alpha(ACCENT,0.15), weak alpha(ACCENT,0.10), fg INK,
                  bg_stroke 1px ACCENT
  open:           bg PAPER_SINK, weak PAPER_SINK, fg INK, bg_stroke 1px HAIRLINE
```
Primary action buttons (Send, Load, Add, Capture): fill `ACCENT_DEEP`, text
`ON_ACCENT` (5.32:1), radius 8, no border, hover `mix(ACCENT_DEEP, INK, 0.08)`.
Destructive buttons: text `CLIP_TEXT`, border 1px `CLIP_TEXT`.

### 4.18 Windows & panels (rig_inputs, sessions, plugins, transport, assistant, param editor)
All inherit §4.17. Shared rules:
- Window titles: Noto Sans Medium 16px `INK`, sentence case. Section labels:
  tracked micro-caps 10px `INK_2`.
- **Lists = the reference service list:** rows separated by 1px `HAIRLINE`;
  active/selected row = `ACCENT_DEEP` text + 2px `ACCENT` left rule; row numbers
  (if shown) Noto Sans Medium, larger than the row text.
- Transport window: BPM as a big number — Noto Sans Medium 24px `INK`; playing
  indicator `LIVE_TEAL` dot + tracked caps; play/stop button per §4.13 rules
  (small version: `LIVE_TEAL` fill + `INK` glyph).
- Text inputs: `PAPER_SINK` fill, 1px `HAIRLINE`, radius 8, `INK` text; focus
  border `ACCENT`.
- Assistant panel: heading/provider/input hierarchy maps to `INK` / `INK_2` /
  field; status lines regular `INK_2` (no italics); Send = primary button.
- Param editor: param names micro-caps `INK_2`, values Noto Sans Medium 13px
  `INK`, sliders use `ACCENT` fill on `HAIRLINE` track.
- Empty states (assistant, patch bay): the one place to import reference texture
  — an optional rounded 18–24px grayscale image/torn-paper card. Optional.
- Note: the maroon "Process crashed" toast visible in the back screenshot is an
  OS notification captured in the shot, **not app UI** — do not restyle it.

### 4.19 Fonts (`theme::install_fonts`)
Installed system faces (verified under `/usr/share/fonts`): **Noto Sans**
Regular/Medium/Bold (+Black/Light/Thin) in `noto/`, **JetBrains Mono** Nerd Font
Regular/Bold in `TTF/`, Liberation, Adwaita, iA Writer, omarchy icon font.
- The reference's neo-grotesque (Neue Montreal / Neue Haas) is not installed;
  **Noto Sans is the closest available match** — keep the existing loader paths.
- Display/headings/unit names: **Noto Sans Medium** (weight 500 ≈ reference
  400–550). Do NOT use Bold for unit names (reference avoids heavy bold); Bold
  is reserved for the flip-pill large label.
- Body/labels: Noto Sans Regular. Telemetry only: JetBrains Mono.
- Tracking via the `tracked_label` helper (§4.5); sizes per component above.
  Scale note: reference sizes are for a 1600w landing page; the app scales the
  *contrast* down (22px wordmark vs 10px micro-caps), not the absolute hero size.

### 4.20 Suggested implementation order
1. theme.rs tokens + `visuals()` + `paper_bg`/`drop_shadow`/`inset_keyline`
   (whole egui chrome flips light in one step).
2. Chassis + plates + rails (the hero inversion).
3. Cables/jacks/plugs/pulses (back side reads correctly).
4. Knobs/keyboard/meter/displays (front side).
5. Toolbar masthead + hint/stats + help overlay.
6. Typography pass (tracked labels, sentence case, sizes) + top strip.
7. Screenshot both sides; verify against §5 table and the keep-list.

---

## 5. Contrast check (WCAG 2.x, computed)

Requirement: every **text** pairing ≥ 4.5:1 (large text ≥ 3.0:1); non-text
graphics ≥ 3.0:1. Values computed from the exact hexes above.

### On PAPER `#F4F3EE` (L≈0.895)
| Pair | Ratio | Verdict |
|---|---|---|
| INK `#1C1C1C` text | **15.34** | pass |
| INK_2 `#646464` text | **5.33** | pass |
| INK_2 on PAPER_SINK `#ECEAE3` | **4.92** | pass |
| MIDI_BLUE_DEEP `#1C5A8B` text/links | **6.55** | pass |
| LIVE_TEAL_DEEP `#1F7A70` text | **4.64** | pass |
| CLIP_TEXT `#B91C14` text | **5.84** | pass |
| ACCENT_DEEP `#C0391B` text (warn) | **4.92** | pass |
| ACCENT `#EF4825` graphics only | 3.36 | ≥3.0 graphics pass; **not for small text** |
| CLIP_RED `#D9251C` graphics only | 4.47 | graphics pass; small text uses CLIP_TEXT |
| MUTED `#A7A29F` | 2.27 | **decorative only — never informational text** |
| HAIRLINE `#BEBEBE` | 1.67 | divider (non-text, no requirement) |
| INK on selection bg `alpha(ACCENT,0.25)`→`#F3C8BC` | **11.20** | pass |

### On DARK_PANEL `#1C1C1C` / plates `#232120`
| Pair | Ratio (panel / plate) | Verdict |
|---|---|---|
| CREAM `#EFEEE9` text | **14.67 / 13.80** | pass |
| CREAM_DIM `#8E8E8C` text | **5.19 / 4.88** | pass |
| ACCENT `#EF4825` small text | 4.56 / 4.29 | panel pass; **on plates use ACCENT_BRIGHT** |
| ACCENT_BRIGHT `#F1552E` small text | **4.93 / 4.64** | pass |
| MIDI_BLUE `#5B8FD0` text/cables | **5.10 / 4.80** | pass |
| LIVE_TEAL `#2E9E9B` text/LED | **5.26 / 4.94** | pass |
| CLIP_RED `#D9251C` graphics | 3.43 / 3.23 | graphics pass; text uses CLIP_BRIGHT |
| CLIP_BRIGHT `#F0554A` text | **4.95 / 4.66** | pass |
| AUDIO_CREAM cables `#EFEEE9` | 14.67 / 13.80 | pass |
| HAIRLINE_DARK `#8E8E8C` | 5.19 / 4.88 | pass (also usable as dim text) |
| METER_TRACK `#353535` | 1.39 | unlit segments (decorative, fine) |

### On accent fills
| Pair | Ratio | Verdict |
|---|---|---|
| ON_ACCENT `#FFFBFA` on ACCENT `#EF4825` | 3.63 | **large text only** (≥24px regular or ≥18.66px bold — flip pill is 19px bold ✓) |
| INK `#1C1C1C` on ACCENT | 4.56 | pass (alternative for small glyphs) |
| ON_ACCENT on ACCENT_DEEP `#C0391B` | **5.32** | pass — use for all small filled buttons |
| INK on LIVE_TEAL `#2E9E9B` (play glyph) | **5.26** | pass — use this |
| ON_ACCENT on LIVE_TEAL | 3.15 | only meets the 3:1 graphics floor; prefer INK glyphs on teal |

Rules encoded from this table: small orange text on paper → `ACCENT_DEEP`;
small orange text on dark plates → `ACCENT_BRIGHT`; cream on vivid orange only
at large-text sizes; danger text → `CLIP_TEXT` (paper) / `CLIP_BRIGHT` (dark);
`MUTED` is decoration only.

---

## 6. Keep-list — what must NOT change

**Interactions (all of them):**
- Unit drag/reorder, knob drag (vertical), cable grab/re-plug/unplug, jack
  patching with compatibility checks, pending-cable cursor behavior.
- On-screen keyboard click + laptop-key mapping (`handle_laptop_key`), octave
  shifts, velocity.
- Tab flip (front/back), transport play/stop, BPM changes, template/session
  load, plugin browser add, assistant send/tool-exec, param editor toggles.
- All hover states and cursor changes (re-skinned, never removed).

**Real-time safety:**
- Paint-only frame path: no locks, no I/O, no allocation-heavy work per frame;
  galleys cached in `ensure_galleys` (the new tracked-label galleys must be
  cached the same way); font files read once at startup with graceful fallback.

**Physics & animation:**
- Cable catenary + damped spring (`SPRING_K=130`, `SPRING_D=6.8`, semi-implicit
  Euler substeps), arc-length LUT (`MAX_SAMPLES`), pulse spawn/propagation
  (`MAX_PULSES`), cable age/dying fades (`cable_alpha` timing).
- Flip animation: `FLIP_SECS=0.35`, `Xform` squash transform, `text_alpha` fade,
  `ease_out_back` — untouched.
- Meter attack/release smoothing, beat-dot thump, LED breathing, plug-in flash
  rings, delay-dot animation — keep timing, change only colors/shapes.

**Geometry:**
- Rack layout math (`layout`, `unit_rects`, `knob_center`, `key_rect`,
  `jack_pos`), key count/positions, knob radii/arc sweep (270° from 135°), meter
  segment count, jack socket sizes. Only the flip-button rect changes (§4.13),
  and its hit test must be updated to match.

**Semantics:**
- Audio vs MIDI remain distinguishable (cream+round vs blue+square);
  active/playing (teal) ≠ selection (tomato) ≠ clip/danger (red);
  clip still reads instantly on the meter; selection still dims the rest.

---

## Appendix: analysis log
- Call 1–3: per-frame design analysis (palette/type/shape/depth/components/
  signature + crop targets) of `hurricane-{1,2,3}-api.jpg`.
- Call 4: verification of 23 labeled crops (contact sheet) — confirmed roles and
  the single-accent system; pixel values cross-checked with ImageMagick
  histograms of the same regions from the lossless originals.
- Call 5: gap analysis of `signal-front.jpg` / `signal-back.jpg` against the
  extracted language — top-10 prioritized gaps, chassis/cable/knob/toolbar
  recommendations (integrated in §3–4; its "MIDI = orange" suggestion was
  superseded by the separability requirement: orange is reserved for selection,
  MIDI takes the reference's collage blue).
- Contrast ratios: computed in Python (WCAG relative luminance), all pairings
  listed in §5.
