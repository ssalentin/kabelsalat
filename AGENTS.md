# AGENTS.md — kabelsalat

## Evidence requirement (user, 2026-11-15)

**Every change must be proven with evidence — and any UI/visual change MUST be
proven with screenshots.** Do not claim a visual change works without showing
it. Before/after screenshots are expected for design work; embed them in the
reply by absolute path so the user can see them inline.

How to capture (verified harness, see "Working on kabelsalat" below):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build
scripts/capture.sh <outdir> [binary]   # ui-demo: front/back/playing/help/transport
```

- Environment: `WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000`
  (the harness sets these itself). Single frames of the real app: `scripts/shot.sh out.png [pid]`
  (window buffer only; never `grim -o`/`-g`, rule 108).
- Key injection: `scripts/xtest_key.py` (XTEST; F11/F12 = keycodes 95/96, F4 = 0xffc1).
- Visual review = DeepSeek vision via the app's BYOK key
  (`~/.config/kabelsalat/config.json`, field `deepseek_api_key` — NEVER print or
  commit it). POST `https://api.deepseek.com/chat/completions`, model
  `deepseek-flash` (text+image), OpenAI-style `image_url` data-URL parts,
  **max_tokens ≥ 4000**. Downscale images first (`magick`).
- Non-visual changes: `cargo test` (unit suite) plus the relevant CLI self-test
  (`session-test`, `multi-rig-test`, `rig-mix-test`, … — list below).

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
   (per-family palettes) — never mix the family hues; the WARM warming
   shift applies only to the neutral cream jacket, so tomato stays
   selection-only. Audio palette = cream/amber/rose/sand (a sage green
   slipped in and broke the warm family — check new hues against it).
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
9. **Screenshot captures:** take them with `scripts/shot.sh` (`grim -T`, the
   window's own buffer); `grim -o eDP-1` grabbed the whole output with whatever
   lay on top. Verify the frame actually shows the app before claiming evidence.
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
    `standard_rig_graph` (Keys→Surge→Master, wired) and the app hosts a
    voice for its synth on demand (`sync_clap_slots`; there is no voice
    pool and no cap). A plugin that cannot load says so (`plugin_msg`).
    Never ship a bare Keys+Master, and never keep spare voices: a voice no
    unit plays is retired.
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
32. **No physics on unlaid geometry:** `build_rack` resets unit rects to
    `Rect::NOTHING` (infinite); a key handler that rebuilds the rack runs
    AFTER the frame's `layout`, so the same frame's spring step seeded NaN
    control points and every cable of an F4/F5–F8 load was invisible on the
    back (headless tests loaded outside a frame and missed it). Skip
    non-finite endpoints, heal non-finite springs, and test loads through
    the real key path (`frame(.., key(Key::F4))`).
33. **Every engine state the UI shows is published back:** the mixer showed
    stale vol/pan after an MCP/CLI SetRigMix because only the graph and rules
    were published. The engine publishes the mix into `GraphPub` (`mix` +
    `mix_rev`) via a non-blocking `try_lock` (retried next block — never a
    graph clone on the audio thread); `sync_from_engine` adopts it.
34. **Hit-test in paint order, topmost first:** black keys lie on the whites —
    test them first (`black_key_rect`), or clicks on a black key play the
    white underneath. Share one rect function between draw and hit-test.
35. **Labels that cables can cross get a plate:** paint order alone is not
    enough — a stroke running through glyphs makes "L" unreadable. Jack labels
    sit on a CHASSIS-colored backing so cables pass visibly behind them.
36. **One rack size across rigs:** lay every rig out with the tallest rig's
    row count (`stack_columns` per rig) and stretch shorter stacks to the
    column height — sizing per rig made Rig B visibly smaller than Rig A
    and the rack jumped on every switch.
37. **Tests never touch the network or the user's config:** `load_key()` and
    `save_config_field()` are no-ops under `cfg(test)`. The assistant's
    startup `/models` fetch ran once per test DemoApp with the real key, and
    the leftover reqwest/tokio threads aborted the test binary at exit.
38. **`RLIMIT_RTTIME` must allow plugin state loads:** pass `sample_rate`
    (clamped by `rtkit` to 200 ms) rather than `BUFFER_FRAMES` (~5.3 ms) to
    `promote_current_thread_to_real_time`; Surge XT applies `load_state` inside
    `process()`, which exceeds 5.3 ms and triggers `SIGXCPU` on `SCHED_RR`.
39. **Share one sound-search helper with alias fallback:** every library
    (Surge, Dexed, LV2, plugin presets) is searched by `sounds::search` and
    `sounds::find`, from `ui.rs`, `mcp.rs` and the self-tests; they try
    literal matches first, then instrument aliases (`wurlitzer` → `wurli`,
    `rhodes` → `suitcase`/`ep`, etc.) so common queries don't miss and leave
    Surge on the init saw patch.
40. **`AddNode` for `Clap` must wire matrix + bind idle voices:** adding a
    CLAP node (`Command::AddNode`) left it disconnected in `Matrix` and
    unbound in `voices`, and `sync_from_engine` skipped `sync_clap_slots` when
    `mirror == g.graph` — connect `Keys`/`Hardware` and bind idle matching
    voices in `AddNode`, and run `sync_clap_slots` even on `same` syncs.
41. **Every edit that can change a held note's note-off path releases first:**
    the matrix is stateless, so `Disconnect` (Keys MIDI cable), `LoadGraph`/
    `LoadRigs`, `RemoveNode`, `SetRigSource`/`SetRigDevice` (like split and
    rule edits) call `release_all` before changing routing. Unplugging a cable
    or loading a template while holding a key left Surge droning. Run the
    `verify-held-notes` check (`cargo test --bin kabelsalat held_notes`) on any
    routing change, and add new routing commands to its alphabet.
42. **LV2 control inputs start at the plugin's declared `lv2:default`**
    (clamped to the range; missing default = minimum), never a "0 → 1.0"
    unity hack: it silenced Vitalium and the LSP oscillator through the
    engine. `lv2-instrument-engine-test` must report an audible peak.
43. **One tempo→beat factor:** everything that counts beats (the song clock
    hosted plugins get, each metronome's phase) advances by
    `Transport::beats_per_frame`. The metronome counted samples per beat, so a
    tempo change mid-beat left its click 1/16 beat off the beats a
    tempo-synced plugin sees. Check with `kabelsalat transport-test` (real
    Surge) and `cargo test --bin kabelsalat song_clock master_started`.
44. **Node-id keyed session data goes through `remap_session`:** a rig-tagged
    session is canonicalized (`canonical_rig_session`: split → merge, the
    layout LoadRigs produces) before `apply_session` uses it; knobs, plugin
    states and matrix routes follow their node via one remap. A removed
    rig-A unit shifted rig B's ids, and a restored route landed on Master.
45. **WAV writers count samples, never floored frames:** the audio thread's
    L/R push can split across two ring drains, so `n / channels` per drain
    under-counted the header by 2–151 frames while the data was right.
    `record-test` reads the file back and compares it sample for sample.
46. **Plugin I/O never runs on the audio or UI thread:** LV2 instantiation
    (~170 ms for Vitalium), preset parsing and instance frees go through the
    engine's LV2 loader thread (`spawn_lv2_loader`), and plugin discovery
    runs on a background scan (`start_plugin_scan`). Every lilv world
    build/load/instantiate/free holds `LILV_LOCK` — concurrent lilv use from
    two threads corrupted the heap. Check with `kabelsalat lv2-async-test`.
47. **Worker threads park, never spin; only failures clear a live flag:** the
    recorder's writer busy-spun a full core while the ring was empty, and its
    Stop path cleared `recording` after a quick Stop→Start had set it again
    (take 2 went silent). Sleep/park when idle; clear state only on error.
48. **Device discovery runs on its own thread, never per UI frame:** MIDI
    inputs were scanned only at startup, so a keyboard plugged in later
    (M-Wave FM-1) was never seen; a rescan in `update()` would stall while
    the window is idle. `spawn_hw_midi` owns the connections and rescans
    every 2 s; unplugged devices keep their index. Tests never scan the
    machine's hardware (a plugged keyboard broke a matrix layout test).
49. **Gain staging before colour:** the master `tanh` warmth saw the raw sum
    of every voice; 8–16 held notes spent 2.5–4.9 % of samples in hard
    saturation ("burning crackle"). A zero-latency `MasterLimiter` holds the
    sum at 0.7 first (0 % hard saturation for held notes; a big chord's
    onset still touches it for 2–5 ms, masked by the attack). Never add
    lookahead on the master (it adds to every played note's latency).
50. **Push only on a green gate you checked explicitly:** gate commits and
    pushes on `cargo test` exiting 0 AND its output containing
    `test result: ok` — never on `grep "test result"` (it also matches
    `FAILED`) and never with `;` between test and push. Twice a red run was
    pushed this way (f2659d7, ad36805).
51. **The CC that completes a MIDI learn is applied, not swallowed:** the
    engine sees a hardware CC (HwDirect) before the UI frame sends the new
    `MatrixCcMap`, so the knob adopts the value WITHOUT setting `sent` and
    publish sends one catch-up SetParam. Skipping it left sound and knob at
    the old value until the controller moved again.
52. **Faces use the face grid, never per-face offsets:** content starts at
    `FACE_X` (the title's left edge), keeps `FACE_PAD_R`/`FACE_PAD_B`, and the
    knob row sits on `knob_row_y` (unit bottom) so rows line up across
    columns. Per-face magic numbers put Master's meter on its border and
    labels against the bottom edge. `face_layout_tests` audits every face
    for padding and overlaps at three window sizes — extend `items()` when
    adding a face element.
53. **Rig meters are post-fader:** a muted rig, or any rig while another is
    soloed, reads silent in the rig strip (pre-fader edge levels made a
    working solo look broken).
54. **Names users read use local time; the app plays what it writes:** take
    names were UTC (a 16:31 take named 15:31), and the file player read only
    16-bit WAV while the recorder writes 32-bit float. Any file format the
    app writes must round-trip through its own reader (wav_format_tests).
55. **Window commands only on your own, focused window:** `hyprctl dispatch`
    acts on the ACTIVE window — a probe fullscreened the user's editor. Check
    `hyprctl activewindow` shows your pid immediately before any dispatch
    (this Hyprland uses Lua: `hyprctl dispatch 'hl.dsp.window.fullscreen()'`),
    and crop screenshots to the window geometry, not the whole output.
56. **Never free a Vitalium instance — retire it:** its JUCE threads outlive
    instance teardown, so freeing (or instantiating next to a freed one)
    corrupted the heap: 10/16 suite runs crashed, 0/16 with the Vitalium tests
    skipped. `lv2::retire` parks `NEVER_FREE` plugins and `Lv2Plugin::load`
    reuses them (re-activated, controls at defaults); `Drop` leaks them as a
    safety net. Retire LV2 instances with `lv2::retire`, never `drop`.
57. **UI-thread code never scans plugins:** agent tools on the UI thread use
    the background scan's lists (`plugin_lists_ready`); a cold CLAP scan there
    froze the UI for up to 5 s per wave of hanging bundles.
58. **Identify MIDI devices by name without ALSA's client:port suffix:**
    midir names end in "24:0", which changes on a re-plug; matching the full
    name turned a re-plugged keyboard into a new device (`midi_port_key`).
59. **An undo snapshot holds everything `apply_session` resets:** the
    per-voice keyboard/hardware switches were missing, so undoing a later
    job silently turned a switched-off source back on. Test undo with a
    non-default value — a default one passes even when nothing is restored.
60. **Audio inputs are PipeWire sources, opened by node name:** cpal's ALSA
    host listed duplicate/0-channel/null devices under non-unique names,
    "no device" opened the first list entry, and a missing device silently
    fell back to the laptop mic. List sources via `pactl -f json` (monitors
    out), capture with `pw-record --target <node>`, and a missing device is
    "not available" — never another input.
61. **A MIDI device is live only while ALSA still lists our subscription:**
    after a USB re-plug the MIDIMATE came back under the identical name
    ("… 28:0") but without the subscription, so the name check said
    "connected" and the keyboard stayed dead. The hot-plug pass reads
    /proc/asound/seq/clients (`alsa_port_subscribed`) and reconnects stale
    ports.
62. **Level after the limiter is made up, and the last stage is bounded:**
    the limiter (ceiling 0.7) fed `tanh` with no make-up, so the master
    peaked at −2.3…−4.4 dBFS at full volume ("the master is so low"). Make-up
    maps the ceiling to −1 dBFS; because the zero-latency limiter overshoots
    on chord onsets for ~1 ms, a final soft knee (`MasterLimiter::knee`,
    transparent below −3 dBFS) keeps every sample ≤ −1 dBFS — make-up alone
    measured +3.3 dBFS. Meters showing what users hear are dB-scaled
    (`meter_db_pos`), never linear amplitude.
63. **Parsers of external tool output are tested on the tool's REAL output:**
    pactl prints a sink-input's `client` as a string ("29124") while the
    client's `index` is a number; a hand-typed test JSON used numbers, passed,
    and the app never found its own stream. Paste real output (trimmed) into
    the test, never retype its shape.
64. **Layout modes follow the window width, never `ui.available_width()`:**
    the latter follows the previous frame's content width, so the top bar's
    compact mode flipped between frames and its buttons jumped under the
    mouse (clicks missed). Decide from `ctx.content_rect()`; test narrow
    windows by clicking, not only by measuring.
65. **Session-level metadata travels the whole load path:** rig names were
    never saved, and a single rig loads via LoadGraph (no rig list), so
    renamed rigs came back as "Rig A/B". Anything stored beside the graph
    goes through `capture_session` → `remap_session` → the LoadRigs/LoadGraph
    decision, is used once (never leaks into a later load), and the UI
    mirror (`rig_snapshot`) follows what the engine will publish.
66. **An MCP tool forwarded to the app is on `UI_BRIDGE_TOOLS`:** the
    instrument tools called `ui_bridge_tool("install_instrument")` but the
    app's serve list did not include them, so attached agents got "not
    served". `ui_bridge_tools_cover_mcp` reads mcp.rs and fails on any gap.
67. **Knob names live in one table:** `engine::knob_specs` defines every
    rack knob (label, default, param, aliases); `make_unit`, the UI resolver
    and headless MCP all read it. Headless MCP used `dsp_param_choices` names,
    so `knob="RESO"` failed and `CEILING` bound the DRIVE knob. Add knobs
    there, never as a literal in a face or a resolver. A plugin's own
    controls are the second table: its `MainControls` list, resolved by
    `MainControls::find` in the app and in headless MCP alike (rule 90).
68. **A knob on the face drives something, or is not bindable:** the
    system-audio GAIN knob was visual only (no SetParam path). A knob with
    `param: Some(..)` must reach the engine via `apply_engine_param`, and
    any engine state it shows is published back (here `GraphPub::system_gain`).
69. **LV2 event inputs append and clear; outputs announce capacity:** each
    note became the WHOLE atom sequence and it was never cleared, so a
    note-off and a note-on in one block (legato) lost the note-off and
    setBfree droned; chords played only their last note. `queue_midi`
    appends, `process` re-inits the input after `run()`, and every atom
    OUTPUT gets an 8-byte-aligned Chunk header of full capacity before
    `run()` (#38). `lv2-instrument-engine-test` plays chord + legato.
70. **Agents read the engine's graph, and structural edits wait for it:**
    attached MCP validated ids against the UI's lagging copy, so
    add_node → connect said "node does not exist" and auto cable ids
    collided (#37). The bridge answers get_graph from `GraphPub`, and a
    structural `cmd` returns once the engine published it (≤ 250 ms).
71. **Panels that follow the window use `exact_size`:** egui keeps a
    panel's first `default_size`, so the keyboard dock never resized
    (#23). Derive the size from `ctx.content_rect()` each frame.
72. **LV2 instruments are what they declare:** `lv2:InstrumentPlugin` in
    the plugin's `rdf:type` decides; any other lv2core class means effect.
    The port guess (MIDI in + audio out) called 198 of 208 effects
    instruments; it is only the fallback for class-less plugins (#26).
73. **Every effect hears every source wired into it, in signal order:**
    built-in effects summed only CLAP voices and audio inputs, and LV2 ran
    after them, so setBfree → Drive and any effect → effect chain were
    silent at the master while the input cable still glowed. Render step
    2b runs DSP and LV2 effects in one dependency order (`next_effect`)
    and sums via `sum_audio_into`; add any new node type as a source there.
    `lv2-instrument-engine-test` plays setBfree through a Saturation unit.
74. **Check a plugin test mask against rendered audio, not XML flags alone:**
    Surge's `Gate Chord` clears `trigmask` bit 12 (no envelope retrigger),
    yet step 12 opens the filter (`s12 = 0.833`) over step 11's envelope +
    delay (~8% of onset energy), so treating bit 12 as silent shrank the
    rotation margin until free-running unison phases flipped steps (#18).
    Verify step/gate masks against actual rendered energy before shipping.
75. **Agent tool calls wait for the engine:** `add_node`, `connect`,
    `disconnect` and `remove_node` are applied by the engine, so in
    `poll_assistant` each is a barrier: the next call of the batch and the
    result wait until the rack adopted the publication, and the result names
    the ids it created or removed, or says `NOT applied`. A `get_graph` right
    after `add_node` read the stale mirror and the model added the unit twice.
    Add any new engine-applied tool to `ENGINE_EDIT_TOOLS`.
76. **A tool that removes a cable unplugs it in the rack too:**
    `publish_patch` re-sends every living rack cable each frame, so the
    assistant's `disconnect` (a bus command only) was undone in the same
    engine block while it reported success. Use `unplug_cable`
    (`the_disconnect_tool_removes_the_cable_for_good`).
77. **Model tool calls are checked against the tool table, never defaulted:**
    the worker's `parse_tool_call` answers an unknown tool or off-schema
    arguments with an `error:` result the model can correct; `exec_tool`
    silently used defaults. A tool's `required` list must match the flow the
    prompt describes: `delete_rig` required `confirm` although the Confirm
    flow starts without it (`the_tool_table_fits_the_argument_checker`).
78. **Know what a model id is before it becomes a default:**
    `typesafe/jev-router` is a router to chat models (answers came from Kimi
    and Haiku at 1.2–9.6 s per step); Jev itself (`~typesafe/jev-latest`) is
    a DECISIONS model on `/api/alpha/decisions` (typed questions in,
    probabilities out, about 0.2 s) and rejects chat completions. Read the
    provider's model entry (`architecture.output_modalities`), then run
    `kabelsalat jev-test` and `kabelsalat agent-eval` before changing a
    route's default. Never claim an agent speedup that agent-eval did not
    measure.
79. **Build output never goes to tmpfs, also not as a copy:** a 690 MB debug
    binary copied into a scratch directory under `/tmp` filled the volume
    and every command lost its output. Downloads too: 220 MB of plugin
    archives in the session scratchpad (`/tmp/claude-*`) filled it again. Keep binaries and eval output under
    `~/.cache/ksalat-*` and strip copies.
80. **An engine barrier opens on the engine's answer, never on the rack's
    own move:** `disconnect` unplugs its cable in the rack at once, so the
    changed cable ids released the barrier before the engine had published.
    The next `disconnect` of the step then ran while that publication was on
    its way, `sync_from_engine` rebuilt the rack from it with the second
    cable plugged in, and `publish_patch` re-connected it: every second
    disconnect of a step reported `NOT applied`. `EngineEdit` keeps the
    adopted `last_graph_rev` and reports "applied" only once it moved
    (`every_disconnect_of_one_step_is_applied`).
81. **Tell a model the base of an index it passes:** users say "rig 1",
    tools take the 0-based `rig`. The chat model put a reverb into rig B
    for "rig 1" in 2 of 22 runs; with the mapping stated in the system
    prompt, 0 of 24 (`agent-eval --task reverb_and_tempo --runs 24`).
82. **A turn with an image skips the decisions model:** measured with
    `kabelsalat jev-test --image <file> <request>`. Jev reads a written note
    (action `tempo` at p = 1.00) but its typed answers cannot return the
    value the note states, and what it sees leaks into the other answers (a
    mixer screenshot made it name rig 1 at p = 0.95 and a keys sound at
    0.88 for a request that names neither). The seeing model
    (`agent_perceive_model`) acts for that turn; re-measure before sending
    images to Jev.
83. **Assistant dock uses `exact_size`, a 176 px floor, and a clipped single-row header:**
    `default_size` froze the panel at its first-frame height and `150` px left
    one transcript line clipped by the header (`bug1_transcript_shows_three_lines_unclipped_at_min_size_and_781x950`).
84. **Fade `AGENT` badges out before their dark text vanishes:**
    painting `AGENT_COLOR_TEXT` (`#1C1C1C`) on `alpha(AGENT_COLOR, t)` over a
    `#1C1C1C` plate leaves an empty dark rectangle for `t <= 0.38`
    (`bug2_untouched_units_paint_no_agent_badge_rect`).
85. **Destructive confirms use `theme::danger_button(..., false)` (`DANGER_BG` + `CLIP_RED` border + `CLIP_BRIGHT` text), never `theme::primary_button`:**
    `ACCENT_DEEP` belongs to non-destructive CTAs like `Send`; destructive confirms (`delete_rig`) must stay visually distinct (`agent_panel_defect1_danger_confirm_differs_from_send`).
86. **Live agent status renders once under the running transcript turn, and read-only steps stay out of action chips:**
    duplicating `phase_label()` in the header repeats the model route twice, and `tool_is_read_only` steps belong in `read_count` / the cost line, never as action chips (`agent_panel_defect4_read_only_tools_omitted_from_chip_row`).
87. **Transcript controls indent by `TRANSCRIPT_TEXT_INDENT`, and attachment thumbnails drop their `TextureHandle` on clear:**
    align `Confirm`/`Cancel` and the attachment chip to the message text column (`78.0` px), load the ~32 px thumbnail once per `Image::bytes` Arc (`sync_chat_thumb`), and clear `chat_thumb` on removal/submit so egui frees the texture (`agent_panel_defect6_alignment_and_thumbnail_texture_lifecycle`).
88. **A key shows lit for as long as it is held, from every source:** only
    laptop keys counted as held, so a note held on the FM-1 (or any hardware
    keyboard) blinked for one 0.09 s LED tail and the key went dark while it
    still sounded. `notes_down` keeps one bit per source per note, set on
    note-on and cleared on note-off in `fire_keys_with`
    (`held_notes_stay_lit_on_the_dock_until_released`).
89. **A plugin knob is one of the plugin's own controls, under the plugin's
    name:** every LV2 unit had a `GAIN` knob on control port 0, whatever that
    port was (Vitalium: `lv2_freewheel`; setBfree has no control port, so the
    knob moved nothing), and a CLAP unit had a visual-only `TONE`. Plugin
    knobs come from `main_controls::resolve` (remote-controls pages, then
    `param:` designations, then the first usable continuous parameters,
    macros first); a port with any other designation (`lv2:freeWheeling`,
    `lv2:enabled`) is the host's and is never offered to the player. Check
    with `kabelsalat main-controls-test`.
90. **Re-read a plugin's controls after a state load, without waiting to be
    told:** Surge XT names its macros per patch ("M1: Cutoff") and announces
    no `params.rescan`, so the list read at instantiation kept "M1: -" after
    a patch load. `ClapVoiceMain::load_state` arms the next
    `poll_main_controls` calls, and the face rebuilds its main knobs when the
    list changed (`real_surge_voices_get_main_knobs_that_drive_and_follow_the_plugin`).
91. **A control the plugin left unnamed is not a main control:** only 274 of
    3008 installed Surge patches name a macro; the others report "M1: -",
    which moves nothing. `resolve` skips a slot whose name has no letter or
    digit after its tag and continues with the next remote-controls page.
92. **Find a knob by label or id, never by position:** plugin knobs sit in
    front of a unit's own knobs, and how many there are follows the plugin
    and the unit's width. `units[1].knobs[1]` stopped being LEVEL; use
    `Unit::knob_index("LEVEL")` or `cc_target()`, and never rebuild a unit's
    knob row while one of its knobs is dragged (rule 10).

93. **A sound tool works in the library of the rig's own instrument:**
    `list_sounds` / `load_sound` searched Surge patches for every rig, so a
    YANES or setBfree rig got Surge results and a load went into the wrong
    plugin. The instrument is the key (`sounds::Instrument`, from the node's
    kind), `Instrument::source` is the one table of where its sounds come
    from, and every answer names the instrument and library. A new
    instrument kind gets a `Source` there, never a branch in a tool. Check
    with `kabelsalat sound-library-test` (every installed catalog instrument
    lists, searches, loads one sound through the engine and sounds
    different) and `agent-eval --task yanes_bass` / `organ_jazz`.
94. **Find out where a plugin keeps its sounds before writing a browser:**
    YANES has no preset files and no CLAP preset-discovery factory; its 146
    presets are the values of a stepped "Preset" parameter. setBfree has no
    LV2 presets; its 110 registrations are MIDI programmes it names in a
    MIDNAM document (`lv2::midi_programs`). Instantiate the plugin, list its
    parameters with their value texts and its extension data, and read its
    source, before assuming a file format.
95. **What `list_sounds` lists, `load_sound` loads:** `find` tries the exact
    label before any preference. Factory patches were preferred first, so
    the listed "Third/Pads/Warm Pad" loaded the factory "Pads/Warm Pad"
    (`a_listed_label_loads_exactly_that_sound`).
96. **A search word no sound of the library holds is left out, never
    fuzzy-ranked:** every setBfree programme is an organ and none is called
    one, so "jazz organ" found nothing. `sounds::search` drops such words
    (and reports them as `words_no_sound_has`) only when the full query
    finds nothing; words the library does hold are never dropped, so a
    "wobbly growling bass" still falls back to the kind as before.
97. **A library scan never runs on the UI thread, and an agent call waits
    for it:** `sounds::Libraries` scans on its own thread (the first
    `list_sounds` walked about 3000 patch files on the UI thread) and
    `build_rack` starts the scans of the rack's instruments. In
    `poll_assistant` a sound tool whose library is still scanning is held
    (`Waiting::SoundScan`) instead of answering "try again", which would
    cost a model step. A preset parameter is read from the live plugin on
    the UI thread, which is the CLAP main thread
    (`a_sound_tool_waits_for_the_library_scan`).
98. **A request that names one sound is not "a kind of sound":** widening
    the Jev `load_sound` criterion (so "a jazz drawbar preset" settles
    there) also pulled in "Load the patch Pads/Dreamscape", whose plan would
    fall back to any pad. The plan hands a request with a `Category/Name`
    label to the chat model. Compare `jev-test` outcomes for every sample
    before and after changing a criterion.

99. **A tool call a plan wrote carries `reasoning_content`:** a Jev plan
    that hands over after running tools puts its calls into the
    conversation; DeepSeek's thinking models (`deepseek-flash`) answer 400
    ("reasoning_content … must be passed back") to an assistant tool call
    whose id they did not issue, unless it has that field (empty is
    enough; measured against the API, and OpenRouter's Gemini accepts it).
    Every hand-over from a plan failed this way, for Surge too. `agent-eval
    --task organ_no_pad` runs that path.
100. **Rows in a fixed-width column take their widths from the column:**
    size a flexible field as the remaining width minus the measured buttons
    (`theme::chip_button_width` / `primary_button_width`), never a fixed
    width in `horizontal_wrapped`. In Settings at 781 px, the model row
    asked for 150 + 240 px plus Apply in a 328 px column. Apply wrapped
    letter by letter, the column grew, and the right column, including
    the UI-scale value, was pushed past the window. A status line of
    several colors is one `LayoutJob` label: wrapped labels' galleys start
    at the row's left edge (`leading_space`) and overlap.
    `assert_settings_text_layout` checks containment and no overlap at
    781x950, 760x520 and 1280x900.
101. **egui tests that read popup paint advance `RawInput::time`:** an
    Area fades in, so a popup's first frames paint its fill alpha-scaled
    (`DARK_PANEL` read as `#0707073C` and looked like a wrong fill). Set
    `raw.time` past `animation_time` each frame. Click as a user does
    (move, press, release on separate frames, `click_at`): egui ignores a
    click that lands on the frame right after its popup closed.
102. **Per-voice state is a slot table with one entry per CLAP unit:**
    `rig_sources` / `rig_device` / `patch_names` are refit to the CLAP
    units by `fit_voice_tables` (in `build_rack`), any number of them.
    Resolve a node with `voice_slot_of_node` (None only for non-CLAP
    nodes), never index by a rig index or assume a size: fixed
    `[_; VOICE_POOL]` arrays panicked on an undo with 5+ CLAP units, and
    unknown nodes silently mapped to slot 0
    (`any_number_of_clap_units_have_their_own_voice_state`).
103. **The engine binds voices, the app follows:** which hosted voice plays
    which Clap node is the engine's decision (`bind_free_voices`),
    published as `GraphPub::voices`; `sync_clap_slots` maps units from it.
    The app matched plugin ids in its own order, so after a remote
    RemoveNode it edited the freed voice while the engine played the other
    one. Where the app must know the binding before the engine answers (a
    load it sends, to restore plugin state), it uses the engine's rule
    `control::bind_voices`, never a guess of its own
    (`the_app_follows_the_engines_binding_after_remote_edits`).
104. **Engine state of a node starts over when a voice is bound to it:**
    level, keyboard/hardware switches and device live in the `VoiceSlot`
    but belong to the node; `VoiceSlot::bind` resets them. A voice kept
    "keyboard off" across a template load while the UI showed it on.
105. **MIDI routes belong to the Clap node, not to its voice:**
    `GraphEngine::new` wired routes per startup voice, so a node whose
    voice is hosted later never got a note (found by `many-voices-test`).
    LoadGraph, AddNode and the constructor all route per Clap node.
106. **Per-voice UI state travels by node id through every reload:** a rig
    add/delete renumbers nodes and `apply_session` resets the switches, so
    `edit_rigs` carries `VoiceRow`s through its id remap and the undo
    snapshot holds them (device included). Adding a rig silently turned
    every switched-off keyboard back on (rule 59 class).
107. **The audio thread neither allocates nor frees for a voice:** the host
    builds each `VoiceSlot` (with its render buffer), sends
    `VoiceMsg::Grow` with a larger table ahead of the `Add` that would not
    fit (and reserves the published list, `GraphPub::reserve_voices`), and
    frees whatever comes back through `VoiceBack`. The old path
    dropped a replaced voice's buffer in `render` and went silent past 32
    voices. Voice messages are drained before every structural command,
    so what the host sent before a load is in place when the load binds.
108. **No pixel of another window reaches the repo:** `grim -o eDP-1` and
    region crops captured browser tabs with the user's e-mail address,
    chat transcripts, desktop crash notifications, and other apps' audio
    streams in the mixer; they were pushed to the public repo. Capture with
    `scripts/shot.sh` only (`grim -T` on the kabelsalat window: no other
    window, bar or notification can appear). Every committed image is listed
    by hash in `docs/images.sha256`; `scripts/check-images.sh` fails the
    pre-commit hook (`git config core.hooksPath .githooks`, once per clone)
    and CI for any other image. Add a hash only after opening the image and
    seeing kabelsalat and nothing else (also no device, account or file
    names that are not the app's). Evidence screenshots for replies stay in
    the scratchpad, never in the repo; never `git add -A` an image.
109. **A knob the hardware moves moves on screen too:** the face draws
    `display_value`, which only advances during a glide; `apply_cc_to_knobs`
    set `value` alone, so every CC-learned knob (Master WARM, Surge LEVEL, …)
    changed the sound while it stood still. Move it with `Knob::set_immediate`
    and assert `display_value` in tests, never `value` alone
    (`a_learned_cc_moves_the_drawn_master_warm_knob`).
110. **A colour stage keeps its loudness:** WARM's make-up pinned only the
    full-scale peak, but below the limiter ceiling `tanh` is almost linear, so
    WARM 0→100 % raised quiet playing by 7.7 dB and read as a volume knob.
    `MasterLimiter::process` matches the output RMS to the default-WARM chain
    (200 ms window); check any drive/saturation change with
    `warm_keeps_the_loudness_of_the_default_warm`, not only with peaks.
111. **An adoption is not a load:** `build_rack` clears `rule_edits` (loads
    start fresh), but `sync_from_engine` also rebuilds through it after any
    agent/MCP edit, right after mirroring the engine's rules. The UI lost every
    CC binding and route edit; learned knobs stopped following, and the next
    rig add/delete captured an empty matrix and loaded it into the engine.
    The sync rebuild keeps the rules it mirrored
    (`cc_bindings_survive_an_agent_edit_and_a_rig_add`).
112. **The master never passes DC:** a source stuck at a constant (Surge on
    rig 3's MONO input held ~1.0) pinned the shared limiter, so the meter and
    cables read full while every rig was ducked to silence, and the speakers
    got the offset. `MasterLimiter::process` high-passes each channel at 5 Hz
    before the limiter (`a_stuck_constant_is_blocked_and_does_not_duck_the_music`).
    When the output meter is full and nothing is heard, compare `get_status`
    `band_db` (all energy below 120 Hz = DC) and mute rigs one at a time.
113. **A value sent to an LV2 node before its instance is live is parked,
    not dropped:** LV2 plugins load on the loader thread (rule 46), so a fresh
    setBfree's face values (SetParam right after the load) found no instance
    and vanished: the face showed one registration, the organ played its own.
    `organ_pending` (fixed size, no allocation) holds them and `attach_lv2`
    replays them; `LoadGraph` clears it. Check with `kabelsalat organ-test`,
    which sends the registration before the instance exists.
114. **A catalog route names the CPU it runs on:** this machine is aarch64,
    and upstream Linux releases (Odin 2, Floe) are x86_64 only, so an entry
    lists routes best first (`routes`) and `local_route` takes the first that
    `runs_on` this CPU; with none, the card says so and offers no Install.
    Check `uname -m` and `file` a bundle before trusting a prebuilt one.
115. **An install step that clones starts from a fresh folder:** a retry
    after a failed install found the earlier clone in the build cache and
    `git clone` refused it. `plan` puts `rm -rf` of its own cache folder
    before every clone or download (`fresh_dir`).
116. **A test of an installed bundle loads that bundle by path:** the
    catalog test played the user's `~/.clap/Odin2.clap` instead of the one it
    had just installed, because the host searches `~/.clap` before
    `$CLAP_PATH`. Use `ClapVoice::new_from_bundle`, never a search by id.
117. **A row that starts with a label sets its height first:** egui centres
    each widget on the row as tall as it is so far, so an "Installed" label
    before 26 px buttons sat 3 to 6 px high (and a wrapping layout ignores
    the cross alignment altogether). `ui.set_min_height(26.0)` before the
    label (`a_catalog_source_code_button_opens_the_repo` compares centres).

114. **A release has its packages, or the workflow says why:** v0.1.0 was
    tagged before `release.yml` existed, so its release shipped without one
    file and nothing failed. Release notes link each AppImage by URL
    (`release-notes.sh` fails when there is none); a tag without packages is
    backfilled with `gh workflow run release.yml -f tag=vX.Y.Z`. After a
    tag push, check `gh release view vX.Y.Z --json assets`.
115. **Packaging scripts resolve their path arguments before any `cd`:**
    `appimage.sh` changed into its work directory and then handed linuxdeploy
    the caller's relative `./target/release/kabelsalat` ("No such file"), so
    the first release run failed on both arches. Use `realpath` on every
    path argument at the top of a script that changes directory.
118. **Plugin actions that change the rack return to `StageView::Rack` on success:**
    `add_clap_rig`, `add_lv2_instrument`, `add_lv2_effect`, and
    `add_metronome_to_active_rig` left `stage_view` on `StageView::Plugins`,
    so clicking "New rig" appeared to do nothing (`plugins_stage_new_rig_switches_to_rack`).

## Working on kabelsalat

- **Where things live (one place each, never mirrored):** planned work, bugs
  and ideas = [GitHub issues](https://github.com/ssalentin/kabelsalat/issues)
  with area labels (`engine`, `ui`, `agent`, `plugins`, `midi`, `mixing`,
  `testing`) and milestones per release; how and why it is built =
  `docs/ARCHITECTURE.md`; CI, packages and releases = `docs/PACKAGING.md`;
  released changes = `CHANGELOG.md` (add yours under
  **Unreleased**); rules = this file. Read open issues with
  `gh issue list -R ssalentin/kabelsalat`; reference them in commits
  (`Fixes #12`). A finished task closes its issue; a new idea or a found bug
  becomes an issue, not a note in a doc.
- **Releases:** SemVer tags `vX.Y.Z` on `main`. Pushing the tag builds every
  package (Flatpak, AppImage, tarball, Arch, for x86_64 and aarch64) and
  publishes the GitHub release from that version's CHANGELOG block; the steps
  before the tag are in `docs/PACKAGING.md` ("Cut a release").
- **Build / run:** `export PATH="$HOME/.cargo/bin:$PATH"; cargo build`, then
  `WAYLAND_DISPLAY=wayland-1 DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000 ./target/debug/kabelsalat`.
  The running app owns `$XDG_RUNTIME_DIR/kabelsalat.sock`; a background task
  that starts it is killed at its time limit — say so to the user.
- **Tests:** `cargo test --bin kabelsalat` (unit suite) and CLI self-tests with
  real plugins: `audio-test`, `host-test`, `preset-test`, `clap-scan-test`,
  `session-test`, `capture-test`, `player-test`, `record-test`, `metro-test`,
  `transport-test`, `edge-level-test`, `add-node-test`, `many-voices-test`, `mono-master-test`,
  `multi-rig-test`, `multi-rig-load-test`, `rig-mix-test`, `cc-map-test`,
  `main-controls-test`,
  `lv2-test`, `lv2-host-test`, `lv2-voice-test`, `lv2-engine-test`,
  `lv2-instrument-test`, `lv2-instrument-engine-test`, `lv2-async-test`,
  `sound-library-test`, `organ-test`, `catalog-install-test [id...]`
  (`./target/debug/kabelsalat <name>`; the last one downloads and builds).
- **Agent changes:** `kabelsalat agent-eval [--model <id>] [--runs N]
  [--task <name>] [--no-jev]` runs the in-app agent over 29 scripted requests
  (headless app, offline engine, real Surge voices, plus YANES and setBfree
  for `yanes_bass` / `organ_jazz` / `organ_no_pad`; it calls the configured
  model APIs and costs a few cents; three requests carry an image, and
  `look_at_rack` gets `docs/shots/mixer.png` in place of a window) and
  `kabelsalat jev-test` shows Jev's decisions (`--image <file> <request>`
  asks about one request without and with an image). A new shortcut row
  needs an eval task and a `JEV_SAMPLES` line. Surge logs to stdout: filter
  with
  `| grep -E '^(  [a-z]|\||agent-eval)'`. Unit tests of the loop run on
  `llm::fake::FakeTransport` (rule 37).
- **Agents:** `kabelsalat mcp` (headless) or `kabelsalat mcp --attach` (drive
  the running app); one-shot: `kabelsalat send '<json>'`.
- **Screenshots of the real app:** launch, wait until `hyprctl activewindow`
  shows your pid, fullscreen it with `hyprctl dispatch 'hl.dsp.window.fullscreen()'`
  (rule 55), inject keys with `scripts/xtest_key.py` (F4 = 0xffc1, F1–F10 = X
  keycodes 67–76, F11/F12 = 95/96 — the script handles it), then
  `scripts/shot.sh out.png <pid>` (rule 108).
  Mouse clicks need screen coords: window origin × monitor scale + egui pos.
- **Hyprland window glitch:** the app process can run while its window stops
  listing; relaunch cleanly (`pkill -x kabelsalat; rm -f $XDG_RUNTIME_DIR/kabelsalat.sock`).

## Ground rules

- **One branch per session (user, 2026-11-17):** the user runs several
  sessions in parallel. Before your first edit, put your work on its own
  branch in its own worktree:
  `git worktree add ../kabelsalat-<topic> -b <topic> main`, then work only
  there. Never edit on `main`. A branch alone is not enough: sessions that
  share one checkout share its working tree, so a `git switch` in one moves
  every other session's files too. Check `git branch --show-current` and
  `pwd` before editing.
- **Move the session into its worktree (user, 2026-11-18):** creating the
  worktree is not enough. Switch the session itself into it before any
  edit, build or git command (Claude Code: `EnterWorktree` with
  `path: ../kabelsalat-<topic>`; other harnesses: their equivalent, or
  start the session there). Every later command must run with the
  worktree as its working directory, on the feature branch. Never rely on
  a `cd` per command from the `main` checkout: the shell falls back to
  `main` after each call, and one command without the `cd` edits `main`.
  One session edited `src/ui.rs` on `main` this way, and the diff then had
  to be moved by patch. If you find your work on `main`, move it to a
  branch at once (patch or WIP commit; never a bare `git stash`, the stash
  is shared across worktrees), then revert `main`.
- **Finish the work:** when the work is finished, commit it and push the
  branch (`git push -u origin <topic>`, gated per rule 50), then open a
  pull request against `main` (`gh pr create`) that references its issue
  and shows the evidence, then queue it with `gh pr merge --auto --squash`
  (see "Merging"). Never leave finished work only on the local machine or
  only on a branch. Remove the worktree once its PR is merged.
- **Merging (user, 2026-11-17):** `main` is protected: no direct pushes
  (admins included), the branch must be up to date, and the required checks
  `test (x86_64)`, `test (aarch64)`, `flatpak cargo-sources.json is current`
  and `human gate` must pass. GitHub merges a queued auto-merge PR once they
  do; never merge by hand around a red or pending check, and never
  `gh pr merge` without `--auto`. `human gate`
  (`scripts/ci/human-gate.sh`, run from `main` via `pull_request_target`,
  so a PR cannot edit it to pass) holds only PRs that change the rules
  agents work under or what ships: `AGENTS.md`, `.github/`, `.githooks/`,
  `scripts/ci/`, `scripts/check-images.sh`, `Cargo.toml`, `packaging/`.
  Everything else merges on green CI, so the evidence rules above are what
  keeps it safe. Add the `needs-human` label to a PR whose risk neither the
  tests nor its evidence cover.
  Such a PR waits until the user adds `human-approved`; **agents never add
  or remove `human-approved`**, and a PR body for one lists the evidence the
  user needs (screenshots, self-test output). Self-tests with real plugins
  and audio do not run in CI, so their output belongs in every PR body that
  changes the code they cover. When `main` moves, update the branch
  (`gh pr update-branch`) so the queued merge can proceed.
- Design language: **Hurricane** — `docs/design-reference-analysis.md` is the
  spec; `src/theme.rs` is the single palette source. All-dark warm chrome,
  one near-black hero rack panel, one tomato accent (CTA/selection only).
- **No X11 child windows** — plugin editors are native egui.
- **egui 0.37 deadlock:** never call `Painter::add`/`galley` inside
  `ctx.fonts_mut`/`ui.fonts_mut` — layout under the lock, paint after release.
- RT discipline: the audio render path is allocation-free; UI animation reads
  lock-free snapshots only.
- The lead owns git commits and `Cargo.toml`. Subagents build with
  `CARGO_TARGET_DIR=$HOME/.cache/ksalat-*-target` (never /tmp — tmpfs).
- Keep the running app on the old binary until a change is verified.