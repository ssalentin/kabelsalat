# AGENTS.md — kabelsalat

## Evidence requirement (user, 2026-11-15)

**Every change must be proven with evidence — and any UI/visual change MUST be
proven with screenshots.** Do not claim a visual change works without showing
it. Before/after screenshots are expected for design work; embed them in the
reply by absolute path so the user can see them inline.

**This machine's setup lives outside the repo:** if
`~/.config/kabelsalat/agents.local.md` exists, read it before you build, run
or capture the app. It holds the display environment, the compositor's
window commands, the input tools, the key for visual review and the attached
MIDI hardware. Keep machine- and person-specific details there, never in this
file (the repo is public, and other contributors have other machines).

How to capture (verified harness, see "Working on kabelsalat" below):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build
scripts/capture.sh <outdir> [binary]   # ui-demo: front/back/playing/help/transport
```

- Single frames of the real app: `scripts/shot.sh out.png [pid]`
  (window buffer only; never `grim -o`/`-g`, rule 108).
- Key injection: `wtype` (Wayland virtual keyboard), e.g. `wtype -k F4`.
- Visual review = a vision model looking at the downscaled screenshots
  (endpoint, model and key: the local setup file; never print or commit a key).
- Non-visual changes: the unit tests of the code you changed (filtered
  `cargo test --bin kabelsalat <filter>`) plus the relevant CLI self-test
  (`session-test`, `multi-rig-test`, `rig-mix-test`, … — list below). The
  full suite runs in CI, not here (next section).

## Test locally what only this machine can test (user, 2026-11-18)

**CI builds and tests everything; this machine tests what CI cannot, as far
as it has the hardware (next section).** The
full unit suite runs on every push in CI (`test (x86_64)`, `test (aarch64)`),
and release builds and packages run in the release workflows. Running them
locally too only doubles the time and the machine's load. This holds for
every agent and subagent.

- **Never run locally:** the whole unit suite (`cargo test --bin kabelsalat`
  with no filter, `scripts/ci/test.sh`), `cargo build --release`, or
  packaging builds (Flatpak, AppImage, Arch, docker). Exception: you are
  reproducing a CI failure of exactly that step.
- **Run locally:** `cargo build` (debug; the self-tests and screenshots need
  the binary) and the tests of what you changed:
  `cargo test --bin kabelsalat <module or test name>` for the touched
  modules and every test a rule names for that area (e.g. `held_notes`,
  rule 41). Plus everything CI cannot do and this machine can: CLI
  self-tests with real plugins, the plugin tests that return early on CI
  (`docs/PACKAGING.md`), `agent-eval`, and, where the machine has them,
  audio and MIDI hardware and screenshots of the real app. What it cannot
  do becomes the PR's bench test (next section).
- **CI is the gate for the rest:** push once the targeted tests are green
  (rule 50); a PR merges only on green required checks. When CI goes red,
  read its log (`gh run view --log-failed`) and fix that failure; do not
  answer it with a local full-suite run.

## Bench tests: hand over what this machine cannot test (user, 2026-11-19)

**Most development runs on headless servers that work around the clock.
What they cannot test is handed to a machine that can, as a GitHub issue,
and never passed off as tested.**

- **What this machine can test** is one line in its local setup file
  (`~/.config/kabelsalat/agents.local.md`), e.g. `bench: audio midi display
  human`. No file or no such line: none of the four, a headless machine.

  | Need | Means | Examples |
  |---|---|---|
  | `audio` | a real sound card or audio interface | `audio-test`, `audio-in-test`, `monitor-latency-test`, xruns, latency figures |
  | `midi` | a hardware MIDI device | hot-plug, device binding, a controller's real messages |
  | `display` | the real app in a desktop session | `scripts/shot.sh`, key injection, plugin editor windows |
  | `human` | a person who listens, plays or looks | "does it sound right", feel under the fingers |

  A headless machine still runs everything else: unit tests, the offline
  engine, self-tests with the plugins it has, `offline_shots` images
  (rule 148), `listen-test`.
- **Every PR body has a `## Bench test` section** (it is in the PR
  template). Write `none: <reason>` when nothing is left, including when
  this machine has the hardware and you ran the test (its output goes under
  Evidence). Otherwise write a `Needs:` line with the needs from the table,
  then the test: exact commands and steps, and under `Expect:` what to see,
  hear or measure. The reader is an agent or person who never saw your PR,
  so name the device, the session or template to load, and the numbers.
  "Not tested with a physical controller" in the evidence is not a handover
  (#138 merged that way and nobody was told).
- **The issue opens itself.** When the PR merges, `bench.yml`
  (`scripts/ci/bench.py`, self-tested on #138's real body) turns the section
  into one issue labelled `bench`, `bench:<need>` and the PR's area labels.
  Do not open one by hand for a PR. A PR that merged without a valid section
  gets an issue anyway, asking for the steps: settle it from the PR's diff.
  A check that belongs to no PR uses the "Bench test" issue template.
- **A bench test does not hold the merge; it holds the release.** PRs still
  merge on green CI. `prepare-release.yml` refuses while an issue labelled
  `bench` or `bench-failed` is open; releasing anyway (`-f skip_bench=true`)
  is the user's call, never an agent's.
- **Prefer a test the server can run.** Before you write a bench test, ask
  whether the same thing can be checked headless: raw MIDI bytes, the
  offline engine, a rendered file. A bench test is for what truly needs the
  device or the person.
- **On a machine with the devices,** work the queue with the `bench-session`
  skill (`.claude/skills/bench-session/SKILL.md`): one build of `main`, every
  open issue the connected devices allow, the output as a comment, close on
  a pass, a bug labelled `bench-failed` on a fail.

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
    desktop — send `wtype` keys or `scripts/click.sh` clicks and take `grim` evidence only after
    checking `hyprctl activewindow` is kabelsalat (scripts must abort
    otherwise); a stray F12 went to another window once.
25. **Routing tools update the UI mirror too:** every tool that changes
    engine routing (`set_rig_source`, …) must also update the UI state it
    drives (`rig_sources`), or chips and matrix show stale state.
26. **UI scale = the compositor's scale, as winit reports it on Wayland:**
    the app is a native Wayland client and never forces XWayland, whose DPI
    guess (2.33 on the 1.6x eDP-1) inflated every element ~46% and squished
    the rack. Only `KABELSALAT_UI_SCALE`, tests and windowless renders (1.6)
    override it (`ui_scale_override`). Check `ui scale: first-frame ppp=` in
    the log and the window's buffer size (2560x1600 maximized on eDP-1).
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
    pushes on your targeted `cargo test --bin kabelsalat <filter>` runs
    exiting 0 AND their output containing `test result: ok` with at least
    one test run (a filter that matches nothing is not a gate) — never on
    `grep "test result"` (it also matches `FAILED`) and never with `;`
    between test and push. Twice a red run was pushed this way (f2659d7,
    ad36805). The full suite is CI's gate (`scripts/ci/test.sh`), not a
    local step (see "Test locally what only this machine can test").
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
82. **Jev decides from text, never from media:** with an image attached
    (measured before image input was removed), Jev read a written note
    (action `tempo` at p = 1.00) but its typed answers could not return the
    value the note stated, and what it saw leaked into the other answers (a
    mixer screenshot made it name rig 1 at p = 0.95 and a keys sound at
    0.88 for a request that named neither). A model that perceives
    (hears, sees) writes what it found as text; Jev decides from that text.
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
87. **Transcript controls indent by `TRANSCRIPT_TEXT_INDENT`:**
    align `Confirm`/`Cancel` to the message text column (`78.0` px) (`agent_panel_defect6_confirm_aligns_with_the_transcript_column`).
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

118. **A release has its packages, or the workflow says why:** v0.1.0 was
    tagged before `release.yml` existed, so its release shipped without one
    file and nothing failed. Release notes link each AppImage by URL
    (`release-notes.sh` fails when there is none); a tag without packages is
    backfilled with `gh workflow run release.yml -f tag=vX.Y.Z`. After a
    tag push, check `gh release view vX.Y.Z --json assets`.
119. **Packaging scripts resolve their path arguments before any `cd`:**
    `appimage.sh` changed into its work directory and then handed linuxdeploy
    the caller's relative `./target/release/kabelsalat` ("No such file"), so
    the first release run failed on both arches. Use `realpath` on every
    path argument at the top of a script that changes directory.
118. **A list of choices is one row per choice, never a wrapping chip row:**
    the load-rig dialog put "Replace N · name" chips in `horizontal_wrapped`;
    the last ones wrapped letter by letter into towers and the modal grew to
    856 px. Options whose labels carry user text (rig, preset, device names)
    are full-width rows with truncated labels (`load_target_row`); test the
    dialog with long names (`load_rig_dialog_keeps_its_width_with_many_long_rig_names`).
118. **Plugin actions that change the rack return to `StageView::Rack` on success:**
    `add_clap_rig`, `add_lv2_instrument`, `add_lv2_effect`, and
    `add_metronome_to_active_rig` left `stage_view` on `StageView::Plugins`,
    so clicking "New rig" appeared to do nothing (`plugins_stage_new_rig_switches_to_rack`).
118. **A plugin editor is judged by its window, not by `gui.show`'s answer:**
    Odin 2 returns false from `show` yet attaches and paints its editor;
    treating that as failure refused a working GUI. `editor-test` checks the
    plugin put a child window into ours, and that it closes and reopens.
119. **Plugin editor windows are dialogs:** Hyprland tiled a plain X11
    toplevel (781x950, the plugin squeezed to fit); `PluginWindow` sets
    `_NET_WM_WINDOW_TYPE_DIALOG`, so it floats at the plugin's own size. Its
    class is `kabelsalat-plugin`; `scripts/shot.sh <out> <pid> <title>`
    captures one.
120. **Plugin timers run only while that plugin's editor is open:** JUCE's
    timers drive one message loop per process; running every Surge's timers
    each frame deadlocked the unit suite, whose tests host Surge on parallel
    threads (`voice_host_tests` hung 3 of 4 runs, 12 of 12 passed after).
    `ClapVoiceMain::pump` returns at once without an editor.
118. **An instrument a tool adds is wired like a rig's synth:** the agent
    put setBfree into rig A with `add_node` and connected only its MIDI
    cable, so the organ rendered into nothing (its strip LED moved, the rig
    bus stayed dark). `AddNode { wire: true }` (Clap always; LV2 when the
    scan says `is_instrument`) runs `Graph::wire_instrument` in the engine
    and in headless MCP's mirror alike. Keep prompts in step with what the
    app hosts: "LV2 instruments are coming soon" made the model give up on a
    working organ
    (`an_lv2_instrument_the_agent_adds_to_a_rig_reaches_that_rigs_master`).
120. **Zero-latency master limiting catches peaks on sample 0 and leaves headroom:**
    a 1 ms attack let chord onsets overshoot `MasterLimiter::CEILING` into
    `tanh` + `knee` clipping, `master_vol = 1.0` left zero headroom below
    −1 dBFS, and `meter_color` turned `CLIP_RED` at −6 dBFS (`0.9` on the
    dB meter). `gain_for` drops instantly when `target < gain` (120 ms
    release prevents waveform tracking), `P_VOL` defaults to `0.75`
    (−2.5 dB, keeping chords below `KNEE`), and `meter_color` marks clip
    red only above `0.95` (−3 dBFS).
118. **Check a plugin state before the plugin restores it:** Vitalium opens a
    modal alert box for a patch it cannot parse, and that box waits for a
    message loop no thread here runs, so the restoring thread hung for good
    (a hand-written patch without `sample`/`wavetables`/`lfos`). Every state
    goes through its `lv2::state_format` check (`vital::check_patch`) on the
    loader thread before `restore_state`; test patches start from the
    plugin's own saved state, never a hand-written fragment.
121. **Delete build output you no longer need:** parallel sessions left a
    dozen `~/.cache/ksalat-*-target` dirs (4 to 7 GB each) and 5.2 GB of
    copied test binaries in an old session's scratchpad. `/tmp` (tmpfs,
    held in RAM) reached 98 %, swap filled, and a build killed the session.
    When your PR is merged or your work is abandoned, `rm -rf` your own
    `CARGO_TARGET_DIR` and the binaries you copied. Before a long build,
    check `df -h /tmp ~` and `free -h`. You may delete another session's
    target dir only when no process uses it (`pgrep -fa <dir>` is empty)
    and its branch is merged.
122. **A column sized beside another takes what is left in the row:** the
    Sessions list was sized `avail - left - 25`, but the row's item spacing
    sat between the columns too, so the Delete button ran 6 pt past the
    sheet. Size the last column from `ui.available_width()` at the point you
    allocate it (`overwrite_in_the_sessions_list_replaces_that_session_after_a_confirm`
    checks the row buttons stay inside the sheet).
120. **An instrument starts playable:** Aeolus starts with every stop off,
    so the catalog install built, loaded and scanned it, and middle C peaked
    at 0.000. Play a note on a fresh instance of every new catalog
    instrument (`catalog-install-test <id>`); one that starts silent gets a
    starting state (`aeolus::initial_state`, applied where every CLAP voice
    is instantiated) and its sounds as a `Source` (rule 93).
121. **Test a face element in the rack the app opens with:** the Aeolus
    stop panel passed its layout tests on a lone, tall unit, yet in the
    real app (two instruments stacked over their delays, units 250 px
    tall) it found no room and was not drawn at all. A face test asserts
    the element IS shown in a stacked default rack at 1280x800 and
    1600x1000, not only that what is shown does not overlap.

123. **Every instrument column of the matrix has one route per cell:** an
    LV2 instrument column drew every row from its MIDI cable, so one click
    lit or cleared the whole column. LV2 cells are the enabled flags of their
    `(source, node)` matrix rules, shown and toggled with the same keyboard
    switch + hardware radio group as CLAP cells (`matrix_cell_on`,
    `toggled_sources`). Test a new column kind with
    `an_lv2_column_toggles_one_cell_like_a_clap_column`.
124. **A small window shows the whole app, not a corner of it:** the rack
    had fixed 200/220 px column floors and 60 px margins, so below ~950 px
    it panned and cut Keys and Master off; stage sheets kept 30 px margins
    and a 200 px minimum that painted into the docks; the mixer's dB marks
    stacked on a short meter. Column floors come from the faces
    (`face_min_w`: own knobs, title clear of the LED, Master's meter), a
    small stage switches to `RackChrome::TIGHT`, a short but wide stage
    lays the stacks out in one row (`rack_flat`), and sheets clip to
    themselves. `every_view_fits_small_windows` fails on any text overlap,
    text outside the window or sideways rack overflow in every view at six
    window sizes × three docks; `face_layout_tests` audits faces at the same
    sizes. See the result without the desktop:
    `KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat window_size_shots -- --ignored`
    (CPU-rasterized real frames; symbol glyphs the test fonts lack show as boxes).
125. **UI scale follows the monitor the window is on:** the launch-time
    focused monitor's scale ran a window on the 1.0x external screen at the
    laptop's 1.6x (a 1368 px window laid out as 855 px). As a native Wayland
    client (rule 26) the app takes winit's per-surface scale, which changes
    when the window moves; never poll `hyprctl` for it.
126. **Tracked text advances by a width that is never zero:**
    `layout_tracked` used `glyph_width`, which answered 0 in a busy test
    run and stacked "kabelsalat" on one spot; the laid-out glyph's width
    is the fallback.
127. **A rack rebuild never scans plugins:** `build_rack` called
    `lv2::discover()` to tell whether an LV2 unit is an instrument, so every
    load (and every agent edit) of a rack with an LV2 unit classified all
    ~200 installed plugins on the UI thread: 2.6 to 4.0 s per session load.
    `classify_lv2_units` reads the background scan's list and runs again when
    the scan lands. Time loads with `kabelsalat session-load-bench <pkg>`.
128. **`max_tokens` counts a thinking model's hidden reasoning:** the
    listening model got 400 and spent 242 to 326 of them reasoning, so
    answers stopped mid-line ("SCORES\n8-bit: 0\nsoft") and read as missing
    scores. Give reasoning models ≥ 4000 and cap the thinking with
    `reasoning.effort` instead; check `usage.completion_tokens_details`.
129. **Blocking HTTP never runs on the MCP server's async runtime:** the
    headless MCP `listen` built a `reqwest::blocking` client inside an rmcp
    tool and the process panicked ("Cannot drop a runtime in a context where
    blocking is not allowed"). Run it on a `std::thread` and join.
130. **A reference the model judges must be the thing it names:** synthetic
    naive saws alias, so every model heard the "fanfare" as chiptune and
    `listen-test` failed for the reference, not the model. Reference clips
    are real instrument patches rendered through the engine.
131. **An eval harness wires every path the live app wires:** `EvalRig` had
    no `note_sink`, so the agent's `play_note` reached nothing and every
    agent-eval master was silent (`listen` heard −120 dBFS). Before trusting
    an eval of a new tool, check that what it observes is live there.
132. **A feature PR ships its README entry, and fresh shots when users can
    see the difference:** the plugin-editor windows (#71) merged without a
    line in "What you can do", and the README's feature list quietly went
    stale. A PR labelled `enhancement` updates `README.md` (its entry under
    "What you can do", or "Install") in the same change, and when the feature
    changes what the app shows or how it is played, re-captures the
    screenshots that entry shows (`scripts/shot.sh` into `docs/shots/`,
    rule 108 review). `.github/workflows/readme-check.yml`
    (`scripts/ci/readme-check.sh`, self-tested on real `gh api` output with
    `--selftest`) fails an `enhancement` PR without a README change and warns
    when `src/ui.rs`/`src/theme.rs` changed without a screenshot — label
    only real features, so the fail stays meaningful.
133. **A silence check flushes the master's tails first:** the master's DC
    blocker and limiter ring on for ~100 ms after a cut, and effects slew their
    gain over ~10 ms, so `rig-mix-test`'s "vol 0 is silent" failed on `main`
    (0.003 residual, peak over the first blocks). Render and discard ~30
    blocks before measuring what must be silent; compare levels of a decaying
    note only after retriggering it (`shot` in the strip-mix section).
134. **Solo silences sources that have no strip entry:** `strip_gains_of`
    returned "untouched" for a source without an entry, so soloing source B
    left source A playing at full level (0.36 vs 0.22 peak). Treat a missing
    entry as the default mix and let the rig's solo flag decide
    (`another_sources_solo_silences_an_untouched_source`).
135. **A draggable widget's id is keyed by what it is, never by where it is:**
    a live strip reorder moved the strip under the pointer, and the
    position-derived id handed the drag to whichever strip now sat in the old
    slot, so two strips swapped back and forth. Use
    `Id::new(("strip_name", node))` with `ui.interact`, and hit-test the
    previous frame's rects (`mixer_strip_rects_prev`): the current frame's
    list only holds the strips already drawn
    (`dragging_a_strip_name_reorders_it_live`).
136. **A 1-based slot is 1-based everywhere:** `Move { to }` was documented
    and tested as the slot shown on the strip (1 = first) but inserted at a
    0-based index, so "move to slot 1" landed second. Convert once, at the
    edit (`to.clamp(1, len) - 1`), and send the shown number from the UI.

141. **Input-ring discards are whole stereo frames, and a backlog is cut at
    once:** `DropOne` popped ONE sample of an interleaved ring (swapping L and
    R from then on) and trimmed a startup backlog one sample per block, so
    monitoring ran 330-570 ms late. `Drift::catch_up` cuts everything above
    the target in whole frames; `monitor-latency-test` measures the loop
    itself (about 19 ms median, 10 ms best, once it recirculates).
142. **A UI frame runs a heavy estimator once, never per chunk:** the tuner's
    YIN (millions of multiply-adds) ran for each 512-sample chunk of a
    one-second backlog, so loading the Guitar rig froze the window until the
    compositor offered to terminate it; `pump_audio_in_tuners` also called
    `process(&[])`, which always answered "no pitch". `Tuner::push` is cheap,
    `estimate_if_due` runs at most every 50 ms
    (`a_backlog_costs_one_estimate_not_one_per_chunk`). The tuner's feed is
    one L+R-mixed sample per frame, never the interleaved stream.
143. **Shift variants of a key are asked before the plain key:** egui matches
    `Modifiers::NONE` logically, so Shift+F5 was consumed as F5 (Lead +
    Reverb) and the Guitar rig never loaded
    (`shift_f5_loads_the_guitar_rig_and_plain_f5_the_lead`).
144. **A self-test of LV2 units follows the app's load path:** `LoadGraph` on
    an engine with `spawn_lv2_loader`, wait until every LV2 slot attached
    (rule 46), then feed. Building the engine with the graph instantiated
    the plugins inside the constructor and aborted with heap corruption in
    about one run in four; the measurement also raced the attach. Judge
    "transformed" by the share of power outside the fundamental
    (`non_fundamental_share`), not a single Goertzel bin of a decaying tone.

128. **Dock controls are painted from the theme, never stock `egui::Slider`:**
    the VEL slider's rail has the panel's own fill on the dark dock, so only a
    tiny handle showed. Use a `PAPER_SINK` track + `CREAM` fill like the strip's
    wheels (`kb_strip_sends_the_standard_midi_signals` drives it by pointer),
    and look at the screenshot before calling a control done.
133. **A plugin whose destructor crashes is parked, never freed (CLAP too):**
    Dexed's destructor segfaulted while other instances rendered (#52, the rule
    56 class for LV2). `CLAP_NEVER_FREE` ids are deactivated and parked on
    retire (the editor closed first; `Drop` is bypassed with `ManuallyDrop`),
    and `ClapVoice::new_plugin` reuses them, reloading the first instance's
    state. Add a plugin to that list instead of dropping it.
134. **A column's width floor comes from its faces, and a face with fixed-size
    content says so:** the uniform mid width put the organ's LESLIE knob over
    the next unit at 781 px (#58). `face_min_w` feeds `mid_floor`; extend it
    when a face gets new fixed-size content
    (`organ_next_to_a_synth_stack_fits_at_the_default_window_size`).
135. **A test of a plugin's state never compares against a "fresh" baseline
    other tests can move, and it polls with a deadline:** Dexed's fresh state
    differs by length depending on what other instances in the process did, so
    `init == reused` failed 1 run in 3 in the full suite (#64,
    `dexed_is_parked_and_reused_not_destroyed`). Assert the change you made
    (customized state replaced), wait on the audio thread with a bounded poll,
    and give only the step that must time out a short timeout. Stress a new
    test with a busy CPU and repeated full-suite runs before shipping it.
136. **A transient message never shares a row with a title:** a
    right-to-left layout inside `horizontal_wrapped` paints over the previous
    labels at 781 px (#65). Title and subtitle go in a plain `horizontal` row
    (subtitle truncated), status text gets its own wrapped row
    (`plugins_header_message_never_overlaps_the_subtitle`).


128. **A looped read that lands on the file end wraps, never "heals":** at the
    seam a derived position can equal `frames` by float rounding; the NaN-heal
    path restarted the file from its Beat Offset and silently re-anchored a
    synced loop one pass out of phase. Only non-finite positions heal; looped
    ones `rem_euclid` (`beat_offset_shifts_the_start_point`). A synced start
    armed in the block the transport starts waits for the NEXT bar by design:
    arm it before `TransportPlay` to land on beat 0.
137. **One knob-value ↔ position mapping for stepped knobs:** the face
    snaps to `round(v * (n-1))`, but the engine floored `v * n` and the
    agent tool divided by `n`, so a MODE/RATE/OCT value the tool wrote could
    land one step off (OCT 2 became 1, the default RATE was no position at
    all). `arp::step_index` / `step_value` are the only mapping, and a
    stepped knob's default is an exact position (`arp-test`, `arp::tests`).
138. **A rewire the engine does itself outlives the UI's stale mirror:**
    `AddNode` of an arp removes the rig's Keys → synth cables, but the UI
    re-sends every cable of ITS rack each frame until it adopts the new
    graph, and that `Connect` plugged the keys back past the arp
    (`agent-eval --task arp_updown` 0/3). The engine remembers the ids it took
    over (`arp_taken`, fixed size, cleared by a load) and ignores their
    Connect (`an_arp_takes_over_the_keys_cables_and_a_stale_reconnect_cannot_undo_it`).
139. **Every MIDI destination kind is routed in the constructor, in
    LoadGraph and in AddNode:** `GraphEngine::new` built Keys/hardware rules
    for CLAP and LV2 nodes only, so a rack that STARTS with an arp had no
    route into it (0 steps in `arp-test`). Add a new destination kind to all
    three, and test it by starting an engine with that graph.
140. **A pattern starts at its first note, not at the grid index:** the arp
    began at `grid_step % len`, so a chord pressed mid-bar began on its
    second or third note. Keep the grid index (onset, swing parity) and the
    pattern position apart (`a_press_mid_bar_starts_the_pattern_on_its_first_note`).
    In the held-notes check, judge arps by "the last release silences
    everything", never per key: chords of two held keys can share a pitch.
145. **A hosted MIDI effect's knob move releases what is held through it:**
    Carla's MIDI Transpose maps a note-off with the CURRENT semitones, so
    moving the knob between a key's note-on (+7 → 67) and note-off (−5 → 55)
    left the instrument's 67 sounding. `apply_midi_fx_param` calls
    `release_all` when a hosted plugin's value really changes
    (`kabelsalat midifx-test` runs the real plugin through the engine).
146. **An exhaustive search stores hashes, never state text, and splits
    its alphabet:** the held-notes search kept each state's JSON (6.7 GB at
    800 k states), the OOM killer ended the session's shells again and
    again, and the full product of routing × MIDI-FX edits has no reachable
    fixpoint. States are hashes, the halves run as separate scenarios
    (`Scenario::{Routing, Arp, RoutingWithStrip, Fx}`, 30–100 k states each),
    and a long run goes in `systemd-run --user -p MemoryMax=3G`.
147. **An integer moving average keeps what its division drops:**
    `RENDER_AVG_US` did `avg - avg/64 + us/64` in whole microseconds, and
    both divisions dropped up to 63 µs: a 50 µs render read 0 and a steady
    300 µs one read 256, so the DSP load of a light rack read 0 %. Average in
    nanoseconds with `engine_info::ema_ns` (a signed step, rounded towards
    the sample), and test a new average on a constant input from below and
    from above (`the_average_converges_on_small_and_large_times`).
148. **A headless shot starts from an app that never ran a frame:**
    `offline_shots::shot` makes a new egui context and font atlas, but the
    app keeps galleys (wordmark, knob captions, hints) laid out in the
    atlas of the context it ran in before. `engine_info_shots` and
    `mixer_parity_shots` ran frames first and drew those as glyph soup;
    build a fresh app per image and open every PNG before using it as
    evidence.
149. **Never end a turn on a build or test you have not read the end of:**
    on #6, three turns in a row said "running `cargo test`, waiting for the
    output" and ended; the background job died with the session, nobody
    read a result, and the work stalled for hours. Run builds and tests in
    the FOREGROUND with the maximum timeout (600000 ms; split into
    `--no-run` then the run if needed). If one must run in the background,
    block on it (`t3_background_wait`) before replying, and report its exit
    code and `test result:` line. "Waiting for the output" is never a
    final message.
150. **Compare plugin audio only inside one render:** two Surge instances
    fed the same notes differ in almost every sample (free-running
    oscillator phases), so a stem compared against a second, soloed run
    failed on 124770 of 124800 samples. `record-test` rebuilds the master
    from the stems of the same take (`master_chain_from_silence`) instead.

151. **A `while let Some(..)` over a source that is always `Some` needs a
    bound, and a test count that is short is a hang:** the clock's pre-roll
    loop asked `next_grid_k` for the next tick and that answered `Some` for
    every later tick, so two `clocksend` tests grew a `Vec` until the runner
    killed them (CI exit 143 after 24 to 47 s of silence, and the local
    session with it). Bound every such loop by its lookahead window, and when
    a filtered run shows fewer `ok` lines than `N passed` would, or no
    `test result:` line, the missing tests hang: find them before pushing
    (`test result: ok. 17 passed` for `clocksend`).
152. **A scale change moves every threshold drawn on that scale:** the
    meters went from linear amplitude to dB (`meter_db_pos`, rule 62) but
    `theme::meter_color` kept its linear stops (0.6, 0.95), which on the dB
    scale meant tomato from −24 dBFS and red from −3 dBFS, so every rack
    playing at a normal level read as overloaded. Colour stops are dB values
    (tomato −6, red at the −1 dBFS ceiling);
    `healthy_levels_meter_cream_and_only_the_ceiling_is_red` checks them.
153. **A popup that closes on "a click outside" runs before the widget that
    opens it:** the mixer's insert picker ran after the strips, so the
    release that opened it was already a click outside, and it closed in
    its first frame; its pick also went into the `edits` list after that
    list had been applied. No effect could be inserted from the mixer. The
    rename modal `take()`-ed its state and never put it back, so it lived
    one frame. Run such popups first in the frame, push edits before they
    are drained, keep a popup's state until it is decided, and test a popup
    by clicking its opener and an item through real frames
    (`an_effect_picked_in_the_insert_picker_lands_in_the_strip_chain`,
    `a_clicked_strip_name_opens_a_rename_that_stays_until_decided`).

153. **One matrix rule per (source, destination), and a connect keeps it:**
    `Matrix::connect` appended a rule every time. An LV2 instrument attaches
    after its loader thread finishes (rule 46), and `attach_lv2` connected
    every source again after the UI had already turned one device off for it.
    The new, enabled rule sat next to the disabled one, so the U6MIDI Pro
    played a rig bound to another keyboard. Default routes go through
    `connect`, which keeps an existing rule with its shape and enabled flag
    (`a_device_turned_off_before_the_lv2_instance_loads_stays_off`).
154. **A truncated plugin list shows instruments first, and an unknown URI
    is refused with the real ones:** lilv sorts by URI, so Vitalium
    (`urn:distrho:vitalium`) came after 296 `http:` effects and fell off
    `list_plugins` (first 30 in the app, 60 in MCP). The agent never saw it,
    guessed ten URIs, and each `add_node` "succeeded" as a dead unit.
    `lv2::discover` orders instruments, then MIDI effects, first
    (`instruments_first`), and `add_node` with an uninstalled LV2 URI names
    the installed instruments
    (`the_agent_finds_an_lv2_instrument_behind_hundreds_of_effects`).
155. **Before a push, run every test module of each file you changed:** a
    new refusal in headless `add_node` was checked with three hand-picked
    tests and pushed; CI then failed on an `mcp::tests` test that adds a
    made-up LV2 URI (`cargo test --bin kabelsalat -- mcp::` would have
    shown it). Filter by module path, not by test name.
156. **One binding has one identity, whatever its spelling:** off a CLAP
    unit, CC target `plugin` and `knob` with the same param reach the same
    engine param, but the knob tag and the knob menu matched the target
    strictly. The assistant's `set_cc_map {param: 0}` (target plugin) on a
    Filter drove CUTOFF without a `CC 11` tag, unlike MIDI learn (target
    knob). Every "is this knob bound?" question goes through
    `DemoApp::binds_knob`
    (`an_agent_param_binding_on_a_built_in_unit_shows_like_a_learned_one`).
156. **Decode a MIDI message by its type before its length:** the hardware
    input dropped everything shorter than 3 bytes before looking at the
    status, so channel aftertouch (2 bytes: how wind controllers send breath)
    never reached an instrument. `hw_expression` reads the status first and
    each type's own length; an LV2 plugin gets a message at its wire length
    (`MidiMsg::wire`), never padded to 3 bytes
    (`a_wind_controllers_breath_and_bend_become_engine_commands`,
    `kabelsalat wind-test`).
157. **A knob's whole travel stays inside what the control is for:** the
    Delay's TIME mapped 0..1 onto 1 ms..2 s, so the face default 0.4 meant
    21 ms with feedback, a resonant comb that sounded distorted on every new
    delay. TIME now spans `DELAY_MIN_S`..`DELAY_MAX_S` (50 ms..2 s). When a
    DSP param range changes, check the knob default and every template value
    against it in physical units
    (`the_delay_time_knob_always_gives_an_echo_never_a_comb`).
158. **Text fields type without the desktop's input method:** a focused
    egui `TextEdit` sets `PlatformOutput::ime`, eframe then enables Wayland
    text-input, and every character of every text field waits for the
    input-method process (fcitx5). With that process stalled, all text fields
    went dead while the rest of the app reacted (#30; the XWayland build had
    the same dependency through XIM, and its workaround was dropped with the
    move to Wayland). `frame_ui` clears `ime` last in the frame unless
    `KABELSALAT_IME=1`
    (`a_focused_text_field_types_without_the_desktops_input_method`). Live
    check: headless sway + fcitx5, type with `wtype`, `kill -STOP` fcitx5,
    type again.
159. **A `pactl` move that answered "ok" may not have happened:** on
    PipeWire, `move-sink-input` returns success before the stream is
    relinked, and right after a default-sink change it sometimes does
    nothing at all. The output scan reported the wanted sink as current, so
    the UI showed a card the sound was not on; and "System default" after a
    choice never moved the stream back (it stayed on the M2, and WirePlumber
    restored that target on the next start). `scan_and_apply` reports only
    what `pactl` lists, a chosen sink is re-applied every pass, and a dropped
    choice is remembered (`audio_out::remember`) until a pass has seen the
    stream on the default sink. Check with `kabelsalat audio-out-test` on a
    machine with PipeWire.
160. **A strip for something outside the patch names it on the strip:** the
    System audio strips showed a fader, a meter and "M", and the app's name
    only in a tooltip, so two playing apps could not be told apart. App and
    title are fitted lines above the fader
    (`another_apps_strip_names_the_app_and_its_fader_and_mute_reach_the_stream`).
    Their meter key is the stream's index (rule 135), never the slot in the row.
161. **A CLAP voice sends a parameter event only for an id its plugin
    declared:** Surge XT dereferences the parameter it did not find, so one
    event with another plugin's id (a knob or editor slider whose voice
    mapping moved) was a SIGSEGV inside `process()` on the audio thread
    (#29). `push_cmd` checks the voice's own `param_ranges`; never add a
    path that puts a parameter event into `ev_in` around it.
162. **A state save never runs beside the block that applies a state load:**
    Surge XT applies `state.load` inside `process()` and its save reads the
    patch without a lock, so `save_state` right after `load_state` (undo
    snapshot, session capture) crashed inside the plugin. `StateGate` holds
    such a save until no block is in flight and the voice renders silence
    meanwhile; the audio thread never waits. Check with
    `kabelsalat state-race-test [seconds] [patch.fxp...]` (real Surge: SIGSEGV
    or abort within seconds before the fix).
163. **A meter is tested at a level you can state in advance:** the master
    meter's true peak was `10·log10` of an amplitude (a −6 dBFS sine read −3)
    behind an interpolator built from `sinc(d)`, no low-pass at all (the same
    sine read −4.2); its only test asked for "more than the sample peak" and
    passed. Integrated loudness averaged the gating blocks' dB values, not
    their energy, and read dynamic material 2.4 LU low. Every meter value gets
    a test with a known answer (`true_peak_of_a_half_scale_sine_reads_minus_6_dbtp`,
    `integrated_loudness_averages_energy_not_db`); peaks are `amp_db`, mean
    squares `to_db`.
164. **A control loop through the limiter measures its own yield:** the
    loudness target added the missing LU as dB of trim, but into the limiter
    a dB gives 0.4 LU or less, so "club" was 1.3 LU short after four matches,
    and at the default VOLUME it is out of reach altogether. `MasterTarget::matched`
    divides a step by the yield of the previous one and stops (`limited`, with
    a readout that says why) when a raise gave back under a third. Check any
    change to the master chain with `kabelsalat target-test` (real Surge).
165. **What a session does not state, a load leaves alone:** the loudness
    target is `Option` in `Session`: `None` (template, older file, a session
    built from parts) keeps the engine's target, `Some(OFF)` turns it off.
    A plain default would have switched the target off on every rig add,
    because those rebuild the session from parts (rule 65 class).
166. **An envelope's window is longer than a cycle of what it follows, and a
    flaky measurement is judged by its margin:** `transport-test`'s onset
    detector took the RMS of one 64-frame hop (1.3 ms), shorter than a cycle
    of the held chord, so most "onsets" were waveform cycles. The gate grid
    scored 0.27 and a harmonic of the loudness pattern came within 0.5 % of
    it, so 4 of 74 runs read 2.48 Hz (#18; blamed on "the first run after a
    build", which it never depended on). `ONSET_WINDOW_HOPS` spans 21 ms
    (`onsets_of_a_gated_low_chord_sit_on_the_gate_grid`). Before blaming
    timing, dump the measured signal of 40+ runs and look at how far the
    passing ones are from the threshold.

167. **Tests never list or write the user's sessions:** `sessions_dir()`
    pointed at `~/kabelsalat-sessions` in tests too, so the session popover
    and Library of every headless shot listed the user's real session names,
    and a test autosave would have written there. Under `cfg(test)` it is a
    per-process temp dir; check any new path helper the same way (rule 37).

## Working on kabelsalat

- **Where things live (one place each, never mirrored):** planned work, bugs
  and ideas = [GitHub issues](https://github.com/ssalentin/kabelsalat/issues)
  with area labels (`engine`, `ui`, `agent`, `plugins`, `midi`, `mixing`,
  `testing`) and milestones per release; how and why it is built =
  `docs/ARCHITECTURE.md`; CI, packages and releases = `docs/PACKAGING.md`;
  released changes = `CHANGELOG.md`, but add yours as a fragment
  `changes/<branch>.md` (`changes/README.md`; never edit CHANGELOG.md in a
  feature PR, parallel PRs conflict there); rules = this file. Read open issues with
  `gh issue list -R ssalentin/kabelsalat`; reference them in commits
  (`Fixes #12`). A finished task closes its issue; a new idea or a found bug
  becomes an issue, not a note in a doc.
- **New features (user, 2026-11-18):** a feature is an issue labelled
  `enhancement`, and the PR that implements it carries the label too. That
  PR updates `README.md` in the same change — the feature's entry under
  "What you can do" (or "Install") — and, when it changes what users see,
  fresh screenshots for that entry (`scripts/shot.sh`, rule 108 review).
  The `readme-check` workflow (rule 132) fails it otherwise and warns when
  UI code changed without a screenshot.
- **Releases:** SemVer tags `vX.Y.Z` on `main`, cut with
  `gh workflow run prepare-release.yml -f bump=minor` (or patch/major): it
  opens the release PR, its merge is tagged, and the tag builds every package
  (Flatpak, AppImage, tarball, Arch, for x86_64 and aarch64) and publishes
  the GitHub release. A `nightly` pre-release of `main` is rebuilt every
  night. Details: `docs/PACKAGING.md` ("Releases", "Nightly").
- **Build / run:** `cargo build`, then `./target/debug/kabelsalat` in the
  desktop session's environment (this machine's values: local setup file).
  The running app owns `$XDG_RUNTIME_DIR/kabelsalat.sock`; a background task
  that starts it is killed at its time limit — say so to the user.
- **Tests:** locally `cargo test --bin kabelsalat <filter>` for the code you
  changed (the full suite runs in CI only, see the section near the top) and
  CLI self-tests with real plugins: `audio-test`, `host-test`, `preset-test`, `clap-scan-test`,
  `session-test`, `capture-test`, `player-test`, `record-test`, `metro-test`,
  `transport-test`, `edge-level-test`, `add-node-test`, `many-voices-test`, `mono-master-test`,
  `multi-rig-test`, `multi-rig-load-test`, `rig-mix-test`, `send-test`, `cc-map-test`,
  `audio-out-test` (needs PipeWire with `pactl`, `parec` and `pacat`; loads two
  temporary null sinks, so nothing is heard),
  `state-race-test [seconds] [patch.fxp...]`,
  `main-controls-test`, `engine-bench` (offline render timing and output hash),
  `meter-test`, `target-test` (the master's loudness target on a real phrase),
  `lv2-test`, `lv2-host-test`, `lv2-voice-test`, `lv2-engine-test`,
  `lv2-instrument-test`, `lv2-instrument-engine-test`, `midifx-test`, `lv2-async-test`,
  `sound-library-test [id=query...]`, `organ-test`, `vital-test <patch.vital> [out.wav]`,
  `listen-test [--model <id>] [--runs N] [--audio-only] [--reasoning <effort|default>] [--rate <Hz|native>]`
  (calls the listening model; prints rankings and latency),
  `catalog-install-test [id...]`
  (`./target/debug/kabelsalat <name>`; the last one downloads and builds).
- **Agent changes:** `kabelsalat agent-eval [--model <id>] [--runs N]
  [--task <name>] [--no-jev]` runs the in-app agent over 26 scripted requests
  (headless app, offline engine, real Surge voices, plus YANES and setBfree
  for `yanes_bass` / `organ_jazz` / `organ_no_pad`; it calls the configured
  model APIs and costs a few cents; `listen_bright` also calls the listening
  model) and `kabelsalat jev-test` shows Jev's
  decisions. A new shortcut row
  needs an eval task and a `JEV_SAMPLES` line. Surge logs to stdout: filter
  with
  `| grep -E '^(  [a-z]|\||agent-eval)'`. Unit tests of the loop run on
  `llm::fake::FakeTransport` (rule 37).
- **Agents:** `kabelsalat mcp` (headless) or `kabelsalat mcp --attach` (drive
  the running app); one-shot: `kabelsalat send '<json>'`.
- **Screenshots of the real app:** launch, wait until the compositor shows
  your pid as the active window, fullscreen it (rule 55; commands: local
  setup file), inject keys with `wtype -k F4` (rule 24), then
  `scripts/shot.sh out.png <pid>` (rule 108). Click with
  `scripts/click.sh <pid> <x> <y>` (window-relative logical px = egui
  points; needs `wlrctl`).
- **Window glitch:** the app process can run while its window stops
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
- **Merging (user, 2026-11-18):** `main` is protected: no direct pushes
  (admins included), and the required checks `test (x86_64)` and
  `test (aarch64)` must pass.
  Branches need not be up to date, so a green PR merges at once. GitHub merges a queued auto-merge PR once they
  do; never merge by hand around a red or pending check, and never
  `gh pr merge` without `--auto`. Everything merges on green CI, so the
  evidence rules above are what keeps it safe. Self-tests with real plugins
  and audio do not run in CI, so their output belongs in every PR body that
  changes the code they cover; what this machine could not run goes under
  `## Bench test` ("Bench tests" above). Update a branch only when it conflicts with
  `main`; `resolve-conflicts.yml` merges `main` into PRs whose conflicts are
  appended Markdown lines by itself, so `git pull` before you push again, and
  resolve a PR labelled `merge-conflict` yourself. An open `main-red` issue means `main` is broken: fixing it comes
  before new work (`docs/PACKAGING.md`, "Branch protection and a red main").
- Design language: **Hurricane** — `docs/design-reference-analysis.md` is the
  spec; `src/theme.rs` is the single palette source. All-dark warm chrome,
  one near-black hero rack panel, one tomato accent (CTA/selection only).
- **Plugin GUIs only in their own windows:** the main window is native
  Wayland and never embeds X11. A plugin's own editor opens in a separate
  `PluginWindow` (XWayland) on request; check with `kabelsalat editor-test`.
- **egui 0.37 deadlock:** never call `Painter::add`/`galley` inside
  `ctx.fonts_mut`/`ui.fonts_mut` — layout under the lock, paint after release.
- RT discipline: the audio render path is allocation-free; UI animation reads
  lock-free snapshots only.
- The lead owns git commits and `Cargo.toml`. Subagents build with
  `CARGO_TARGET_DIR=$HOME/.cache/ksalat-*-target` (never /tmp — tmpfs).
- Keep the running app on the old binary until a change is verified.