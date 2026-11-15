# AGENTS.md — kabelsalat

## Evidence requirement (user, 2026-11-15)

**Every change must be proven with evidence — and any UI/visual change MUST be
proven with screenshots.** Do not claim a visual change works without showing
it. Before/after screenshots are expected for design work; embed them in the
reply by absolute path so the user can see them inline.

How to capture (verified harness, PLAN.md §0.1):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build
scripts/capture.sh <outdir> [binary]   # ui-demo: front/back/playing/help/transport
```

- Environment: `WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000`
  (the harness sets these itself). Single frames of the real app: `grim -o eDP-1 out.png`.
- Key injection: `scripts/xtest_key.py` (XTEST; F11/F12 = keycodes 95/96, F4 = 0xffc1).
- Visual review = DeepSeek vision via the app's BYOK key
  (`~/.config/kabelsalat/config.json`, field `deepseek_api_key` — NEVER print or
  commit it). POST `https://api.deepseek.com/chat/completions`, model
  `deepseek-flash` (text+image), OpenAI-style `image_url` data-URL parts,
  **max_tokens ≥ 4000**. Downscale images first (`magick`).
- Non-visual changes: `cargo test` (unit suite) plus the relevant CLI self-test
  (`session-test`, `multi-rig-test`, `rig-mix-test`, … — see PLAN.md §0.1).

## Every fixed error becomes a rule (user, 2026-11-15)

**When you fix a bug, append a one-line rule to the list below so the same
error is never repeated.** Rules must be specific and checkable (what to do /
never do, and why), not vague advice. Review your diff against this list
before committing; if a fix contradicts a rule, update the rule deliberately.

Learned so far (2026-11-15, UI polish pass):

1. **egui text color:** `Visuals::override_text_color` overrides widget
   `fg_stroke` — a button whose fill equals the override color gets invisible
   text. Always set an explicit `RichText::color` per button state.
2. **Raw RGBA assets:** render SVG→RGBA with `magick -background none` and
   verify corner alpha is 0 (`magick -size 64x64 rgba:file -format
   "%[pixel:p{0,0}]" info:`); opaque white corners show as a white square on
   dark taskbars.
3. **Cable colors:** keep audio jackets warm/neutral and MIDI jackets cool
   (per-family palettes) — never mix the family hues; the TONE/WARM warming
   shift applies only to the neutral cream jacket, so tomato stays
   selection-only.
4. **Rig-stable identity:** `merge_rigs` offsets absolute node/edge ids per
   rig — anything visual keyed on ids (colors, …) must use rig-relative ids,
   or it reshuffles when another rig grows.
5. **No decoration over function:** watermarks/ambient marks must never
   overlap panels or controls (the bottom-right "spider" was removed).
6. **Agent parity:** every tool named in a prompt or tool description must
   exist in BOTH `assistant::tools()` and `exec_tool` (and MCP). The assistant
   could not wire racks because `get_graph` was referenced but missing — an
   agent that cannot inspect the graph cannot act on it.
7. **Boxed text needs breathing room:** chips/mini-buttons get explicit
   padding (≥ 8px horizontal, ≥ 4px vertical) and a min size larger than the
   galley; never let text touch a box boundary.
8. **One window:** primary views (MIDI matrix) render in the main stage, not
   floating windows; secondary panels (assistant) are toggleable and hidden
   by default, with a layout suited to their content (wide bottom panel for
   chat text, not a narrow column).
9. **Screenshot captures:** `grim -o eDP-1` grabs the whole output — focus the
   target window first (hyprctl) or the shot shows whatever is on top; verify
   the frame actually shows the app before claiming evidence.
10. **Interaction state holds ids, not indices:** `update()` retains/removes
    dying cables, which shifts `self.cables` indices mid-gesture — Drag/Hit
    state must store cable ids and resolve them at each use, or the wrong
    cable gets unplugged.
11. **egui tests:** after `ctx.run_ui`, call `out.textures_delta.clear()`
    before dropping the frame output — dropping pending texture updates
    panics.
12. **Rack shortcuts yield only to `text_edit_focused()`**, never
    `egui_wants_keyboard_input()` — any focused DragValue makes the latter
    true and silently kills every rack key; strip/mixer number fields must
    not hold focus (drag_only).
13. **Dropping a held plug on a compatible jack MOVES it** (Disconnect(old)
    then Connect(new) via the frame publish); never just unplug.
14. **Note-off never re-lights a key LED:** only light on velocity > 0;
    re-lighting on release restarts the whole decay tail.
15. **`build_rack` is also the sync-adoption path:** never put load-only
    effects (like the fade-in) in it; use begin_load_fade() in the explicit
    load paths only.
16. **With an engine attached, audio cable visuals come only from published
    per-edge levels:** never fake them from MIDI arrivals, or a muted or
    unrouted synth looks live.
17. **Shrink-to-fit labels quantize font size to 0.5px:** any size that
    follows the window width must be quantized, or the font atlas grows
    with every resize.
18. **Rig views group only their own units:** `compute_groups` must skip
    claimed (hidden-rig) units when it opens instrument columns — another
    rig's synth left an empty gap column and shoved stacks against Master.
19. **A new rig is playable or says why:** "+ Rig"/add_rig builds
    `standard_rig_graph` (Keys→Surge→Master, wired) from the voice pool
    (`VOICE_POOL`); idle pool voices skip `render`. Never ship a bare
    Keys+Master just because the startup voices are used up.
20. **`build_rack` resets view + mixer (active_rig = 0, rig_mix = unity):**
    restore remapped mix / the active rig AFTER `apply_session`/`build_rack`,
    never before — and the engine-sync rebuild restores the snapshot's
    active rig.
21. **Rig-tagged graphs load via `LoadRigs`, never `LoadGraph`:** a graph
    with several rig tags (saved multi-rig session, rig edit) sent as
    LoadGraph leaves the engine with an empty rig list while the UI shows
    rigs.
22. **Panels yield to the rack, the rack never squishes:** bottom panels get
    only the height the rack can spare (`rack_need_h`); units keep
    MIN_UNIT_H/MIN_MID_W and an oversized rack pans (wheel) inside a clip —
    hits outside the clip are Background.
23. **The agent operates globally:** every rig action the UI offers (add,
    delete, select, mix, add_node into rig N) is an agent tool in
    `assistant::tools()` + `exec_tool` + MCP; destructive ones need
    `confirm=true` after asking the user.
24. **Live-app automation is focus-guarded:** the user may be using the
    desktop — send XTEST keys/clicks and take `grim` evidence only after
    checking `hyprctl activewindow` is kabelsalat (scripts must abort
    otherwise); a stray F12 went to another window once.
25. **Routing tools update the UI mirror too:** every tool that changes
    engine routing (`set_rig_source`, …) must also update the UI state it
    drives (`rig_sources`), or chips and matrix show stale state.
26. **UI scale = the compositor's monitor scale (`ui_scale_target`):**
    never trust winit's XWayland DPI guess (2.33 on the 1.6x eDP-1) —
    it inflated every element ~46% and squished the rack. Check
    `ui scale: first-frame ppp=` in the log; `KABELSALAT_UI_SCALE` overrides.
27. **Size scroll areas explicitly in fixed-height panels:** set
    `min_scrolled_height` AND `max_height` to the measured remainder —
    egui's default 64px minimum pushed the assistant header out of its
    panel. Test layouts at the panel's MINIMUM size, not the default.
28. **One display name per unit:** UI text shows `sentence_case(&u.name)`
    everywhere (face, mixer, rig inputs, matrix) — raw graph names are
    upper case.
29. **The render thread must be realtime:** cpal does not promote
    pipewire-alsa (IOPLUG) streams — the callback promotes itself once via
    rtkit (`audio_thread_priority`). At normal priority the UI preempted it
    for up to 26 ms (> the buffer) → xruns. Verify with
    `ps -L -o cls,rtprio,comm` (cpal_alsa_out = RR) and get_status
    `render_max_us`/`render_late` before touching BUFFER_FRAMES (now 256).
30. **Meters have a noise floor:** smoothed levels never reach 0 — light
    segments via `lit_segments` (< 0.001 = dark), never a raw `ceil`.
31. **Sound never waits for a UI frame:** input that makes sound (hardware
    MIDI) goes to the engine bus from its own thread (`HwDirect`); the UI
    frame only animates it (`fire_keys_with(.., send=false)`). Throttling
    repaints must never add input latency. Live check:
    `KABELSALAT_MIDI_THROUGH=1` + `aseqsend -p 14:0 90 3C 64`.

## Ground rules

- Read **PLAN.md §0.1 (session handoff)** before working; the roadmap and all
  conventions live in PLAN.md (§13 is the single source of truth, mirrored as
  GitHub issues #1–#8).
- Design language: **Hurricane** — `docs/design-reference-analysis.md` is the
  spec; `src/theme.rs` is the single palette source. Light warm-paper chrome,
  one near-black hero rack panel, one tomato accent (CTA/selection only).
- **No X11 child windows** — plugin editors are native egui.
- **egui 0.37 deadlock:** never call `Painter::add`/`galley` inside
  `ctx.fonts_mut`/`ui.fonts_mut` — layout under the lock, paint after release.
- RT discipline: the audio render path is allocation-free; UI animation reads
  lock-free snapshots only.
- The lead owns git commits and `Cargo.toml`. Subagents build with
  `CARGO_TARGET_DIR=$HOME/.cache/ksalat-*-target` (never /tmp — tmpfs).
- Keep the running app on the old binary until a change is verified.