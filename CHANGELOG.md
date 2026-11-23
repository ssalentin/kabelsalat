# Changelog

All notable changes to kabelsalat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). While the version is `0.x`, minor
releases may still change behaviour and session files.

## [Unreleased]

## [0.3.1] — 2026-11-23

### Added
- **Song notes and ChordPro charts in Show mode:** attach lyrics, cues, or ChordPro (`[Chord]`, `{comment:...}`) charts per song in the Setlist editor and read them on stage in Show mode with highlighted chords and keyboard scrolling.
- **VST3 Program Change support:** MIDI Program Change messages (`0xC0`) now translate to the plugin's `kIsProgramChange` unit parameter so hardware pedals and keyboards switch VST3 presets cleanly.
- **Startup realtime-priority warning:** logs a clear warning at startup if the audio thread is running without realtime priority (across PipeWire, JACK, and ALSA), pointing to Audio Doctor.
- **Raspberry Pi / ARM64 and backing-track band guides:** added dedicated sections in `docs/GUIDE.md` for headless ARM64 rigs and live backing-track band workflows.
- **Out-of-process plugin sandboxing (#209):** CLAP plugins now run in isolated `kabelsalat plugin-worker` helper processes communicating over anonymous shared memory (`memfd_create` + cross-process futex) and a control socketpair. If a plugin crashes or hangs past its block deadline, only that strip goes silent while every other rig keeps playing without an xrun, and the strip automatically restarts with its last saved state and displays a recovery badge. Set `KABELSALAT_SANDBOX=0` to disable sandboxing; run `kabelsalat sandbox-test` to test `SIGSEGV` recovery and measure IPC round-trip latency at 64 and 256 frames.
- **Unsampled Piano CPU & room profiles in Sounds:** added five selectable profiles (`Full`, `Balanced`, `Light`, `Dry`, `Light Dry`) under Unsampled Piano's Sound library and assistant `load_sound` so players on laptops or low-buffer setups can trade built-in room/sympathetic physics for lower single-threaded DSP load (~25% vs ~76% under dense pedalled chords, ~0.4% vs ~7.2% idle on Apple M1).
- **Live parameter sync from native CLAP and VST3 editors (#418):** parameter edits initiated inside a plugin's native editor (`performEdit`, `outputParameterChanges`, or CLAP `out_events`) now propagate across `plugin-worker` shared memory into Kabelsalat's parameter editor, pinned rack/strip quick knobs, active setlist part parameter cache, and learned MIDI CC feedback without echo loops.
- **LV2 out-of-process plugin sandboxing (#413):** hosted LV2 instruments, audio effects, and MIDI FX now run inside isolated `kabelsalat plugin-worker` processes over `memfd_create` shared memory and futex wakeups, automatically restarting with saved state (`LV2_State_Interface`) and live control port values after a worker crash or hang and displaying a `RESTARTED` / `CRASHED` badge on the rack strip (disable with `KABELSALAT_SANDBOX=0`).
- **VST3 plugin hosting & yabridge support (#285):** Scan and host Linux-native and yabridge-bridged Windows `.vst3` instruments and audio effects out-of-process inside the per-plugin `plugin-worker` sandbox with shared-memory audio, state save/restore (`KSV3`), MIDI CC mapping (`IMidiMapping`), parameter automation, and native `IPlugView` X11 editor windows (`kabelsalat vst3-test`).
- **LV2 native GUI windows (#70):** Host LV2 `ui:X11UI` views inside a dedicated host `PluginWindow` (with `ui:showInterface` and `ui:idleInterface` support) toggled from the unit context menu or LV2 parameter editor, keeping parameter controls and MIDI events synchronized bidirectionally with the rack and engine (`kabelsalat lv2-ui-test`).

### Fixed
- **On-screen Stop demo button (#398):** while any rig demo phrase is playing (including one started by the Assistant), a `■ Stop demo` button now appears in the top bar (always visible even with the Left Rail closed) and in the Assistant panel header so you can stop playback with one click.
- **CLAP plugin GUI opening reliability:** set `DISPLAY` (probing for the first reachable X11/XWayland socket) and `EGL_PLATFORM=x11` before `gui.create`, sync the host X11 window with the X server before `set_parent`, resize to post-attach plugin dimensions, fall back to floating X11/Wayland CLAP GUIs when embedded X11 is unsupported, allow double-clicking parameter-less CLAP plugins with native GUIs, and prevent sandboxed plugin workers from starving editor `pump()` callbacks or timing out slow GUI initialization.
- **Repeated Note On on a held note:** retriggering an already-held note now emits Note Off before Note On per voice/LV2 slot so a single Note Off never leaves an orphan voice sounding.
- **CLAP plugin editor parameter feedback:** plugin-initiated `ParamValueEvent`s from CLAP `out_events` are now drained lock-free to the host instead of being discarded between audio blocks.
- **Plugin scanner crash vs kill handling:** killing the app or scanner with `SIGKILL`/`SIGTERM` during plugin loading no longer negatively caches (blocklists) healthy CLAP/VST3 plugins.
- **Backing track stop-at-end:** starting a setlist song's backing track now always resets loop mode to off so a song stops cleanly at the end instead of looping unexpectedly.
- **MIDI device name readability:** long hardware MIDI device names in Settings, Show Controls, Levels, and the MIDI Matrix now truncate cleanly with full-name hover tooltips.
- **Setlist F12 shortcut documentation and test (#382):** documented `F12` for opening the Setlist in `docs/GUIDE.md` so the guide, tooltips, and help card all agree, and added a regression test confirming `F12` opens Setlist while `C` toggles the assistant dock.

## [0.3.0] — 2026-11-22

### Added
- **Assistant plugin & parameter intelligence:** the built-in assistant and MCP server can now inspect any built-in DSP, CLAP, LV2, or catalog plugin (`inspect_plugin`), browse parameters with current values, musical display text, ranges, units, stepped options, module summaries, and modified-only diffs (`list_params`), directly shape parameters using musical text or normalized/native values (`set_param`), and reset all or filtered parameters back to defaults (`reset_params`).
- **Full built-in DSP, setBfree organ & LV2 metadata:** every native parameter of built-in DSP units (including Delay feedback/ping-pong/damping, Reverb size/damping/width, EQ3 shelf/mid frequencies & Q, Filter resonance/mode, Limiter ceiling/drive/release, Saturation drive/mix/output), all 13 setBfree organ controls, and LV2 port units/flags/scale points are now inspectable, editable by name or musical text, and preserved across sessions and undo.
- **Native PipeWire, JACK (`pipewire-jack`), and ALSA backend selection (#349):** kabelsalat now opens a native PipeWire audio node (`kabelsalat`) by default via `libpipewire-0.3.so.0` (no `pipewire-alsa` bridge required) and lets you switch live between **Auto**, **PipeWire**, **JACK (`pipewire-jack`)**, and **ALSA** (with direct ALSA PCM device selection) in **Settings → Audio**, via `KABELSALAT_AUDIO_BACKEND`, or through the `set_audio_backend` agent/MCP tool.
- **Audio Doctor in Settings → Audio (#211):** checks PipeWire quantum and sample rate against kabelsalat's configured buffer, realtime audio priority (`rtkit` / `RLIMIT_RTPRIO` / live render thread), CPU governor / power profile, USB audio autosuspend, and buffer xrun headroom — each with a plain explanation and a non-blocking **Fix** action (no terminal needed). Includes a guided **Round-trip latency** measurement in milliseconds, a cancellable 60-second **Live xrun check** that diagnoses likely causes, a direct **Open Audio Doctor** link from Pre-show check audio/buffer warnings, a read-only `audio_doctor` agent / MCP tool, and `kabelsalat audio-doctor [--json]`.
- **Back-panel cable ergonomics (#303):** Hovering a jack on the back of the rack now traces all cables connected to it while dimming unrelated routes, and shows a live port tooltip with connected source/destination labels and signal readouts (dBFS for audio, active/recent notes for MIDI). Added persistent **Cable opacity** (30–100%) and **Cable tension** controls in **Settings → Display** and on the back panel, plus a right-click menu on cables and occupied jacks to cycle family-safe jacket colors or unplug all cables on a jack.
- **CI performance budgets (#280):** headless benchmarks for audio block render time, UI frame time and vertex counts, built-in 4-rig session load time, start-up to first frame, and default-rack memory now emit JSON and are enforced in CI on both `x86_64` and `aarch64` against versioned budgets and release baselines (`docs/perf-budgets.json`, `docs/ARCHITECTURE.md`).
- **Idle-rig CPU sleep (#206):** non-live rigs that are no longer held and have completely rung out below −100 dBFS (honouring CLAP `sleep` / `tail` extensions and built-in delay and compensation tails) now sleep allocation-free without rendering DSP or plugins, waking in the exact audio block a note, control, or part switch targets them.
- **Precision knob controls (#304):** hold **Shift** while dragging a rack or
  mixer knob for 10× fine-tune adjustment without mid-drag jumps (keeping
  **Ctrl+drag** for unit rearranging), **double-click** any rack knob to reset
  it to its default, see formatted physical units (`Hz`, `ms`, `dB`, `%`,
  semitones, and CLAP plugin readouts) on hover and while dragging, and type an
  exact value inside the knob context menu without disturbing quick MIDI learn.
- **Per-unit live CPU meter overlay on the rack (#302):** press **F3** (or toggle **Rack CPU · F3** in Engine info) to show a compact live CPU micro-bar and `%` / `µs` badge on every visible rack unit, front and back, reading lock-free per-node render times with cream/amber (> 15 %)/red (> 30 %) load thresholds.
- **Rack unit context menu and visible bypass state (#305):** Right-clicking any unit plate on the front or back of the rack opens a context menu to **Bypass / Enable** (`Ctrl+E`), **Duplicate unit** (`Ctrl+D`, copying parameters and plugin state), **Initialize parameters**, **Randomize parameters** (`Ctrl+R`, bounded musically as one undo step), **Copy / Paste settings** between compatible units, **Unplug all cables**, **Open plugin GUI / Parameters**, or **Remove unit**. Bypassed units dim on both front and back faces with a crisp `BYPASSED` chip (clickable to re-enable), stay synced with the mixer and agents, release held notes safely, and preserve delay compensation. The assistant and MCP server expose matching `duplicate_node`, `reset_node_params`, and `randomize_node_params` tools.
- **MIDI learn and hardware sync on instrument interfaces (#241):** right-click
  any control on the expanded instrument console, keyboard dock, or organ rack
  face — including the swell pedal (CC 11), drawbars, rotary speed, percussion,
  drive, reverb, sustain/damper (CC 64), soft pedal (CC 67), mod wheel (CC 1),
  arp, and macro knobs — to arm MIDI learn, open its binding/range menu, or
  clear its mapping. Both learned bindings (`target="cc"` / `"knob"` / `"plugin"`)
  and transparent passthrough hardware CCs now sync directly to the on-screen
  pedals, buttons, wheels, drawbars, and knobs.
- **Setlist (#204):** organize your gig into songs and parts (Verse, Chorus, Solo) over the loaded rigs and switch without cutting held notes or reverb tails. Each song carries a name and optional tempo and time signature; each part picks which rigs take new notes, with per-rig key range, transpose, level and stored knob values. Outgoing rigs keep sounding whatever keys or sustain pedal are held until released, while new notes reach only the new part's rigs from the same audio block on. Build and edit songs in the new **Setlist** view (**F12**), step parts from the top bar, with **PageUp / PageDown** (**Shift+PageUp / Shift+PageDown** for songs, compatible with page-turner pedals), or from a learned pedal or button (#277). **Add song from session** imports a saved session's rigs into the current gig as a new song. The setlist saves with the session, follows rig edits and undo, and is available to the assistant and MCP agents (`get_setlist`, `set_setlist_song`, `select_setlist_part`, `add_song_from_session`).
- **Every show action on a pedal or button (#277):** Setlist → Show controls lists next and previous song, next and previous part, Panic, tap tempo, tuner on/off, master mute, backing track start/stop and record start/stop. Click Learn and press a foot switch, pad, key or preset button: its CC, note or program change does that action from then on, in every song and in Show mode, and never reaches an instrument. The devices whose program change picks the song are rows of the same list. A tuner shows the note the first Audio In plays. Agents use `get_show_controls` and `set_show_control`.
- **Crash report and restart (#279):** when kabelsalat crashes it writes a report to `~/.local/state/kabelsalat/crashes/` (version, cause, plugins loaded, last engine commands, stack frames; no folder or file names of yours, nothing sent) and starts again by itself on the session and setlist part it was on. The restarted app offers to open the report folder or a prefilled GitHub issue. `KABELSALAT_WATCHDOG=0` runs without the restart.
- **Four more instruments on the keyboard (#198):** E-Piano, Bass, Wind and
  Drums each get their own keyboard surface and full-window console: tines
  that ring on the e-piano, a bass neck that shows and plays the notes, a
  breath control (CC 2, CC 11 or aftertouch) for winds, and drum pads and a
  kit seen from above that sound the moment you hit them.
- **Every rig has an instrument type (#198):** piano, e-piano, jazz organ,
  pipe organ, synth, bass, wind or drums, from its plugin or the sound it
  plays, or marked by hand. The mark is saved with the session and kept when
  you add, delete, clone or undo rigs. The left rail shows each rig's type.
- **Long setlists — keep only the next songs loaded (#207):** per-setlist **Load ahead** option (`Setlist::load_ahead`, `set_setlist_window` tool) that hosts only the active song and the next 1–8 playable songs (plus show-level rigs, active set-level rigs, shared instruments, aux-bus chains, and master inserts) so memory follows the window instead of the length of the gig. Upcoming CLAP and LV2 plugins load and warm up on background loader threads (`kabelsalat-clap-loader` and `kabelsalat-lv2-loader`), outgoing rigs stay hosted until held notes and effect tails finish ringing out before retiring, the song list shows each song's **Ready / Loading / Unloaded** state, and jumping directly to an unloaded song announces that it is loading and loads it in the background without blocking the audio thread. Added `kabelsalat setlist-window-test` to measure real Surge XT memory scaling and gapless in-order switching.
- **Up to 32 rigs (#205):** a session now holds up to 32 rigs instead of 8, so
  a setlist can keep every sound of a gig loaded. Each rig has its own mixer
  bus, record arm and stem, the Left Rail and the top bar scroll to every rig,
  and rigs past Z are named AA, AB, … Sessions saved with up to 8 rigs load as
  before.
- **Pre-show check (#278):** **Setlist → Check show** finds what could go wrong before the first song, and changes nothing: a MIDI device the show binds that is not plugged in, a chosen output or Audio In device that is not connected, a plugin that is missing or does not load, a rig that makes no sound (each rig plays its demo phrase in an offline engine of its own), a missing backing-track file, little disk space for recording, a buffer without headroom, and running on battery. Every check says ok, warning or problem, and what to do. Agents use the `preshow_check` tool; `kabelsalat preshow-check <session.ksalat>` prints the same list and exits with status 1 on a problem.
- **A backing track per song (#278):** a setlist song can name a WAV backing track (`set_setlist_song` with `backing`); selecting the song starts it in the file player, and a song without one stops it.
- **Program change picks the song (#208):** in the Setlist view, turn a MIDI
  device on under "Program change picks the song" and its preset buttons pick
  songs: program change N jumps to the first part of song N+1 in the next
  audio block. That device's program changes no longer reach its instruments;
  other devices' program changes now reach theirs (they were dropped before).
  The choice is saved by the device's name, so it survives another plug order.
- **Rack edit lock (#363):** lock the rack against accidental edits from the top-bar padlock button or **Ctrl+L** (agents/MCP: `set_rack_locked`, reported in `get_status`, and saved with the session). While locked, playing, transport, rig switching, setlist stepping, and flipping front/back to inspect wiring keep working, while patching, unplugging, reordering, adding/deleting units or rigs, rack knob edits, and mutating agent calls are blocked.
- **Save the last minutes after the fact (#215):** kabelsalat keeps the last
  two minutes of the master and of every armed rig's stem in memory. Right-click
  REC and choose "Save the last 2:00" to write them as a take folder, exactly as
  you heard them. Settings → Audio → Keep the last sets the length (1 to 10
  minutes, or off; off by default on machines with under 3 GB of memory). The
  assistant and MCP agents save it with `record_save_last`, and
  `record_status` reports what is kept.
- **Share one instrument between songs (#275):** songs and parts can share a single hosted instrument instance with per-part level, pan, split, velocity layer, effect-chain bypasses, and stored rack/plugin knobs — all applied smoothly on switch without cutting held notes (including a 5 ms dry/wet ramp for zero-latency effect bypasses). **Add song from session** automatically reuses matching hosted instruments (same plugin and saved preset) while keeping different presets as separate instances, and the Setlist view shows the exact count of hosted instrument instances, whether differences apply at the switch or use a separate instance, **Own instance** to fork a rig for a different preset, and **Vary per song** to share a show-level rig across songs.
- **Show, set and song levels (#273):** a vocal mic, a master reverb or a volume pedal can now stay through the whole show. Every rig, aux bus and MIDI controller binding lives at show level (no song switch touches it: it keeps the whole keyboard, its fader and its knobs), at the level of a set, or at song level (only where the setlist's parts use it). Songs can be grouped into named sets, which move or are skipped as a whole. Choose levels and build sets under **Setlist → Sets & levels**, or on the mixer's bus strips; a part can now also name the buses it uses. Older sessions load with everything at show level, except the rigs their setlist's parts already use, which keep playing only in those parts. Agents use `get_setlist`, `set_level` and `set_setlist_set`; `kabelsalat levels-test` renders ten song switches under a show-level vocal and reverb.
- **Show mode (#210):** a locked full-window stage screen with the current song and part in large type, the next song, tempo, the master meter, one meter per live rig, a clock and a Panic button. No click or key edits the patch, the mix or the setlist while it is open; PageUp / PageDown and foot switches still step parts, and holding Esc for a second, then letting go, leaves. Agents open and leave it with `set_show_mode`.
- **Panic (#210):** all notes off on every rig, on every MIDI channel, with pedals up and arps and demo phrases stopped (`panic` for agents).
- **`kabelsalat setlist-switch-test` (#274):** renders part and song switches
  under a held chord with real Surge XT voices and measures them: no gap or
  click in the held chord, the delay and reverb tails ring on, and the first
  note of the next song starts in the block that switched (at most one 5.3 ms
  buffer plus Surge's own onset of up to 1 ms, against a 50 ms budget), by the
  app and by the foot switch.
- **Show mode status pills, next part & F2 shortcut:** Show mode (`F2`) now previews the upcoming part in teal, surfaces top `REC`, `TRACK`, `CLICK`, and crash-recovery pills, scales cleanly down to `760×520`, and restores Show mode after crash recovery.
- **Setlist backing-track picker & part controls:** songs can attach `.wav` tracks via **Pick WAV…** (scanning Sessions/Audio) or drag-and-drop; parts now support **↑**, **↓**, **Dup**, and **Clear knobs**; pre-show failing rows link directly (`Matrix →`, `Mixer →`, `Install →`, `Rack →`, `Settings →`, `Fix track →`); **Ctrl+T** and clickable `TUNE` on Audio In toggle the tuner.
- **Retrospective chip & Plugins clarity:** a dedicated **Retro** save chip sits next to `REC`, **MIDI FX** is a top-level Plugins catalog category alongside clearer **Layer in rig** / **Install** prompts, and Mixer pins `Kabelsalat` + `Master out` when 4+ rigs overflow.
- **Velocity layers and a floating split (#276):** each rig of a setlist part now has a velocity range beside its key range, so a soft pad can sit under a hard-struck piano, and a velocity curve (linear, soft for a lighter touch, hard for a firmer one). Its **Float** setting makes the key range's edges follow your hands: a bass run that climbs a few keys past the split stays on the bass, and the right hand coming down stays on its own sound, up to the set number of keys. Releases always reach whatever a key started, whatever the part, layer or split did while it was held. Edit them in the Setlist view next to each rig's keys; they save with the session and the assistant and MCP agents set them through `set_setlist_song` (`vel_lo`, `vel_hi`, `vel_curve`, `float`).

### Changed
- **Refreshed front-of-rack README hero screenshot (#385):** updated `docs/shots/front.png` from the current UI and restored it as the README hero image.
- **One tool for show controls:** `set_show_control` replaces `set_setlist_footswitch`, and `get_setlist` lists `show_controls` in place of `foot_switch`. Sessions with the old foot-switch CCs or `program_devices` load them into the unified controls list.
- **One instrument type everywhere (#198):** the Plugins browser groups and
  labels instruments by the same type the keyboard uses (Pianos, Organs,
  Basses, Winds, Drums), the catalog names each card's type, the demo phrase
  follows a rig's type and its mark, and the agent reads and sets the type of
  any rig (`get_keyboard_state`, `set_keyboard_mode` with `rig`, `get_rigs`,
  `list_plugins`).
- **Rack units name their plugin (#198):** an LV2 instrument's subtitle is
  the plugin's name instead of "lv2 synth".
- **A shorter README with a quick start:** the README now gets a new player
  from download to first notes in four steps, lists what kabelsalat can do in
  one line per feature, and adds install options at a glance and a
  troubleshooting list. Every feature's full description moved to the new
  user guide, `docs/GUIDE.md`.
- **Safer live shortcuts, undo & delete confirmations:** moved knob reset to **Shift+R** (selected unit or active rig, synced to engine with undo), assigned **C** to Assistant and **F12** cleanly to Setlist, extended **Ctrl+Z** undo to manual cable, template, setlist, rig, and bus edits, and added two-step confirms before deleting songs, parts, aux buses, or rig presets.
- **On-stage UX & contrast polish:** upgraded dark-surface warnings/learn prompts to legible high-contrast amber (`ACCENT_BRIGHT`), showed patch feedback notices inline in non-Rack stage headers, added Enter-to-save and clickable `openrouter.ai/keys` link in the Assistant onboarding bar, and showed hardware device names in **Sets & levels**.

### Fixed
- **Assistant picks named instrument plugins instead of Surge:** asking the assistant for a specific instrument plugin (such as Unsampled Piano, Dexed, or Odin 2) now loads that plugin instead of falling back to a Surge XT preset.
- **Rig solo is exclusive (#256):** soloing a rig now un-solos every other
  rig, from the mixer's S button, a MIDI controller, the assistant, MCP or the
  CLI alike. Solo of a source within a rig still stacks.
- **Audio output self-test (#345):** `kabelsalat audio-out-test` now uses run-unique null sinks with `state.restore-props=false`, explicitly sets each test sink to 100 % and unmuted so saved WirePlumber volumes cannot attenuate measurements, and restores the user's configured PipeWire default sink (including unplugged devices) on exit, error, or panic.
- **Release tagging and PR conflict handling (#271, #341):** `tag-release.yml` now also triggers via `workflow_run` when CI passes on a `release-*` PR so bot auto-merges tag `vX.Y.Z` and start `release.yml`, `resolve-conflicts.yml` labels conflicting PRs `merge-conflict` without pushing bot commits that strand required checks, and `scripts/sync-branch.sh` fixes `LC_ALL=C` overlap sorting and preserves exit-3 overlap warnings across separate rebase and `--push` runs.
- **Help overlay on small windows (#111):** The keyboard shortcut card (`?` / `F1`) now fits inside narrow and short windows (such as 760×520, with or without the Navigator open) by stacking into a single scrollable column inside the stage while keeping its two-column layout on wider windows.
- **LV2 parameter units (#348):** inline blank-node `units:unit` definitions (such as LSP Compressor gain ports) now show their real unit symbol (e.g. `G`) instead of raw lilv blank-node IDs like `133b11`.
- **Per-synth MIDI device in rig presets and song import (#322):** saving and loading a rig preset or importing a song via "Add song from session" now preserves each CLAP synth's per-synth MIDI device binding and input switches instead of falling back to all hardware.
- **Monospace digit spacing without JetBrains Mono (#327):** probe JetBrains Mono across Arch, Debian/Ubuntu, Fedora, `/usr/local`, Flatpak, and user font directories, and keep egui's built-in `Hack` monospace face ahead of symbol/math fallbacks so monospace labels (such as `CC 64` in Setlist) always render digits with monospace spacing even when JetBrains Mono is not installed.
- **Agent eval `organ_no_pad` false failure (#346):** `EvalRig::load` now waits for async LV2 loads to settle and clears load-time organ face CC sync (`CC 91 = 25`) from the LV2 MIDI baseline before each eval turn, so read-only `organ_no_pad` runs pass while unintended agent program changes, notes, or parameter CCs still fail.
- **Unsampled Piano / `baseview` plugin GUIs on Wayland (#391):** set `EGL_PLATFORM=x11` at startup when `WAYLAND_DISPLAY` is set so Mesa EGL does not mistake an X11 plugin editor's display pointer for a Wayland display and crash on open.
- **Catalog install status after completion (#392):** `list_instruments` now reports `"installing": false` as soon as an instrument install finishes instead of staying stuck on `true` while the Plugins log stays visible.
- **Settings → Audio**: stop repeating `Active:` under Audio backend and Audio output when the dropdown already shows that value, show the JACK server's buffer size read-only, and replace the cryptic `Active: N frames` line under Buffer size with an explicit override notice only when the audio server forces a different quantum.
- **Top-bar collapsed rig pills:** inactive numbered rig pills now show live audio output as a bottom horizontal teal meter bar instead of a middle teal dot, so they never show two identical green dots when a setlist is active (the single top-right dot belongs exclusively to setlist live/out-of-song status, and out-of-song inactive pills also dim). Collapsed pill widths no longer jump while a rig sounds.
- **System-audio capture can no longer feed back into itself (#27):** capture now records each other playing app's stream directly (`parec --monitor-stream`, excluding kabelsalat and any process it started) instead of the output sink's monitor. kabelsalat's own output is never captured, whichever sound card is selected and after switching outputs or reopening the audio stream, and no capture reader runs while muted.
- **Dexed default patch guidance:** the assistant now knows Dexed's factory default (`Say Again.`) is an octave-bouncing echo FX patch and loads a playable DX7 cartridge voice when adding a Dexed rig (`add_rig` also warns if Dexed is added without a `sound`), and the sound browser labels Dexed's unloaded default accurately as `Say Again. (Dexed default)`.
- **Plugin windows open centred on the active monitor:** opening a plugin's own GUI on Omarchy/Hyprland (especially with multiple monitors or scaled displays under XWayland) now centres the window on the focused RandR monitor instead of the combined X11 root screen, so it no longer lands at the side of the display.
- **Ignore chattering hardware MIDI CCs during learn and per port:** continuously oscillating or stuck hardware CCs (such as a scratchy potentiometer) no longer hijack knob, switch, or show-control MIDI learn. In the MIDI Matrix, any chattering CC offers a one-click `Ignore CC N` chip on its hardware port row that drops that CC at hardware MIDI intake (saved per port in `config.json` as `ignored_ccs`) so it never reaches plugins, knobs, or MIDI learn.
- **Aligned Install catalog plugin cards:** cards in each row of the Plugins → Install grid now share a uniform height, aligned metadata and action button rows, and wrapped action lines that never stretch a card past its column width.
- **Device bindings survive another scan order (#227):** an LV2 instrument bound to one MIDI device, routes and MIDI-learned CC bindings from a hardware controller, and a rig bound to one device now come back on the same keyboard when a session or rig preset is reopened, even when the devices are found in another order or one is not plugged in. A rig's device binding is now saved with the session, and loading a rig preset no longer sends the other rigs' synths back to all hardware.
- **A setlist foot switch no longer plays the instruments (#261):** the CC
  learned for Next or Previous part steps the setlist and reaches no
  instrument, so a foot switch on CC 64 no longer holds the sustain pedal.
  The new part's zones take effect in the audio block the pedal press
  arrives in, without waiting for the screen.
- **Setlist header at narrow windows:** the subtitle no longer runs under the step buttons and the live song; it is shortened instead.
- **A part that changes a rig's level no longer clicks a held chord (#274):**
  a rig fader or an instrument's LEVEL that a part recalls (or the mixer, a
  MIDI controller or an agent moves) now takes 5 ms to arrive instead of
  jumping in one sample. Muting a rig, soloing another or pulling a fader to
  zero fades it out the same way, and unmuting fades it back in.
- **Rack-lock & modal Escape gaps:** blocked `F4`–`F8`, Left Rail, Matrix, and Mixer mutations while the rack is locked, closed param/dialog leaks on lock, fixed drums laptop keys mapping to GM C2 (MIDI 36), and kept `Escape` closing open rig modals cleanly while clearing two-step confirmations first.
- **Unsampled Piano presets installed with the plugin:** Installing Unsampled Piano from the Plugins browser now also copies its shipped `presets/` folder into `~/.local/share/unsampled-piano/presets` so the plugin editor finds them.

## [0.2.0] — 2026-11-20

### Added
- **MIDI-controlled parameters appear on the interface automatically:** binding
  a MIDI CC (via MIDI learn, the MIDI Matrix, an agent/MCP tool, or a loaded
  session) to a CLAP or LV2 parameter that is not currently shown on the unit's
  face now adds it as a face knob (with its `CC n` badge, live value tracking,
  and right-click MIDI menu), yielding an unbound main control when needed and
  prioritizing MIDI-controlled knobs on narrow faces. Clearing the binding
  removes the auto-added knob and restores the unit's main controls.
- **Adaptive input range on MIDI learn:** moving a controller during MIDI
  learn (e.g. a wind controller's bite CC spanning 64–104) tracks the min and
  max values seen while learning, shows the live range on the badge, and
  adapts the binding's input curve once the controller settles (1.5 s, or
  press Enter / click *Finish MIDI learn*) so that partial travel steers the
  full knob. The learned *Input range* appears in the knob's right-click menu
  (editable 0–127, preserved when capping output *Controller range*, and
  reset by *Full range*), is saved with the session, and is exposed to agents
  via `set_cc_map` (`in_min`/`in_max`) and `list_cc_maps` (`input_range`).
- **Controller range on a MIDI-mapped knob:** right-click a bound knob and
  set MIN / MAX under *Controller range*. The controller's whole travel then
  spans only that stretch, so an expression pedal on organ drive can be held
  below 60 % and still use its full sweep. The knob shows the reachable
  stretch in blue between two ticks, *Full range* lifts the limit, and the
  range is saved with the session. Agents set it with `set_cc_map`
  `min`/`max`, and `list_cc_maps` reports it.
- **MIDI strip on the software keyboard:** the keyboard dock now carries a
  pitch-bend wheel (springs back to centre), a modulation wheel (CC 1, holds
  its value), a sustain pedal (CC 64) and a velocity slider (the Keys unit's
  VEL knob), like GarageBand's. They go out as the Keys source, so every rig
  the keys feed responds, and they light the Keys unit's MIDI cables. Pitch
  bend from a hardware controller now reaches the instruments too, and CC 1 /
  CC 64 reach LV2 instruments as they already reached CLAP ones (#97).
- **The assistant can listen (#35).** A new `listen` tool records a few
  seconds of the master and sends them to an audio-capable model on
  OpenRouter (Gemini 3.8 Flash by default; `agent_listen_model` in
  config.json picks another). The model scores the sound 0-10 on the words
  of your request ("8-bit", "heroic", "warm") and says what it heard and how
  to move toward them. The agent plays notes, listens, changes one thing and
  listens again. A listen takes the recording plus about 3 s. It needs an
  OpenRouter key; without one the agent is pointed at `measure_mix`. MCP
  agents get the same `listen` tool, headless and attached.
  `kabelsalat listen-test` checks that the model ranks known reference clips
  the right way round and prints its latency.
- **Plugin windows:** a CLAP instrument's own editor (Surge XT, Odin 2,
  Dexed, YANES, Aeolus) opens from "Open plugin window" at the top of its
  editor pane, in a window of its own, and closes from the same button or the
  window's close button. kabelsalat now runs the plugin's timers and file
  descriptor callbacks, which these editors paint from.
  `kabelsalat editor-test` opens, checks and closes every installed editor.
- **Vital patches in Vitalium:** Vitalium (the Linux port of Vital) now plays
  `.vital` patches, wavetables and modulation included. Its sound list in the
  editor and the assistant's `list_sounds` / `load_sound` show every patch in
  Vitalium's folder. Drop a `.vitalbank` or `.vital` file on the window to add
  it (or run `kabelsalat vital-import <file>`). A loaded patch is kept in saved
  sessions, through rig edits and through undo.
- **Instrument profiles: map a whole hardware instrument at once.** Pick the
  instrument for a hardware port in the MIDI matrix (a port whose name names
  the instrument is matched by itself) and its knobs, drawbars and switches
  drive the rack, with no MIDI learn per control. The first profile is the
  **Nord Electro 2**: its nine upper drawbars, rotary slow/fast, percussion and
  overdrive drive the setBfree organ face. Profiles are MIDI Guide CSV files
  with a `role` column; more go in `~/.config/kabelsalat/instruments/`.
- **Odin 2 and Floe in the instrument catalog** (Plugins → Install). Odin 2
  installs from its prebuilt release on x86_64 and builds from source on
  aarch64; its 260 factory patches are fetched too, so the assistant can list,
  search and load them. Floe installs from its release on x86_64; it publishes
  no aarch64 Linux build, and its card says so there. Release downloads are
  checked against a pinned SHA-256.
- **Aeolus in the instrument catalog:** a pipe organ with additive pipe
  models, several divisions of stops and a convolution reverb. It installs
  from its v0.3.0 release on x86_64 and builds from source on aarch64.
  Aeolus starts with every stop off, so kabelsalat starts it with a
  principal chorus drawn, and `list_sounds` / `load_sound` offer 12
  registrations (principals, flutes, celeste, reeds, full organ). The
  Aeolus unit has a stop panel like a console's stop jamb: Manual III, II,
  I and Pedal down the left, and each stop of the shown division with an
  indicator light; a click draws it or pushes it in. The panel reads the stops
  back from the plugin, so it follows sound and session loads.
- **Catalog cards say what you install:** a short description, the licence,
  how it is installed on this machine, and buttons that open the source code
  and the project's website. `list_instruments` reports the same fields.
- **GitHub stars on catalog cards:** each card shows its project's star count
  (★ 235, ★ 3.9k), read live from GitHub once per run when the Install tab
  opens. Without internet access, or when GitHub refuses, cards show no stars.
- **Sessions save themselves (#51):** with *Autosave sessions* on (Settings,
  on by default) the open session is written every 20 s when it changed, and
  on quit; a patch that was never saved goes to `Autosave`. Loading a session
  or saving one makes it the open session; a starter template never replaces
  it. Each saved session has an **Overwrite** action (with a confirm) that
  saves the current patch over it, and the open session is marked in the list.
- **AppImage download links in every release:** release notes link the
  x86_64 and aarch64 AppImages directly, and the Release workflow can attach
  packages to a tag that was released before it existed (`-f tag=v0.1.0`).
  The README links the newest AppImage for each architecture directly; the
  AppImages are named `kabelsalat-<arch>.AppImage`, and AppImageUpdate or
  Gear Lever update them in place.
- **Organ face for setBfree:** nine drawbars to pull (16′ … 1′) and LESLIE
  (stop/slow/fast), PERC, DRIVE and REVERB knobs. setBfree only listens to MIDI
  controllers, so the face sends its CCs; values are saved with the session,
  work with MIDI learn, and a loaded programme shows its registration.
- **Octave per split zone:** a split Keys face shows LOW and HIGH octave
  steppers (−3…+3). A bass below the split no longer has to sound as deep as
  the low keys it is played on; the assistant sets the LOW zone an octave up
  when it puts a bass there. Saved with the session.

- **`ProgramChange` engine command:** a MIDI programme change to one
  instrument node (how setBfree changes its registration), and
  `kabelsalat sound-library-test`, which lists, searches and loads one sound
  per installed catalog instrument through the real engine.
- **Main controls on every instrument:** a synth's face shows up to four of
  the controls its designer marked as most important, left of `LEVEL`, as
  many as the unit is wide. They come from the plugin's CLAP remote-controls
  pages (Surge XT: the patch's named macros, else the scene mixer), from LV2
  ports with a `param:` designation, or else from its first continuous
  parameters, macros first (Dexed, YANES, Vitalium). The knobs drive the real
  parameters, are MIDI-learnable, follow the plugin's value (a loaded patch
  moves and renames them), and LV2 ones are saved with the session. The
  agent's `list_params` returns the same list as `main_controls` ahead of the
  full parameter list, and a control's name binds with
  `set_cc_map knob=<name>`. `kabelsalat main-controls-test` prints each
  installed instrument's main controls and moves one through the engine.
- **Settings view (`,` / top bar `Settings`):** added a main-stage Settings view
  for configuring per-role agent models (`agent_decide_model` with On/Off switch,
  `agent_act_model`, `agent_perceive_model`) via themed dropdowns or custom model
  IDs with effective route and key status, saving/removing DeepSeek and
  OpenRouter API keys (masked, restarting the assistant worker immediately),
  and editing preferences (`ui_reduce_motion`, count-in on/bars, audio output,
  plus read-only detected UI scale).
- **Packages for every release, on x86_64 and aarch64:** a Flatpak bundle
  (`io.github.ssalentin.kabelsalat`), an AppImage, a plain `.tar.gz` and an
  Arch package, built by GitHub Actions on each `vX.Y.Z` tag and attached to
  the release. Every push and pull request builds and runs the unit suite on
  both architectures. See `docs/PACKAGING.md`.
- `kabelsalat --version` and `kabelsalat --help`.
- **Live agent run UI & rack animations:** docked assistant panel sizes cleanly to show >= 3 transcript turns at minimum window height, replaces static `"thinking…"` with an animated phase/route indicator (`Jev · deciding`, `DeepSeek · step N`, `Gemini · looking at the image`), renders tool action chips under user messages with rule-7 padding, outcome states (`Running`, `Done`, `NotApplied`, `Failed`), collapsing (`+N more`), and one-line run cost. Agent rack edits glide knobs visually (~200 ms, engine sound updated immediately), draw added cables in from source to destination, pulse touched unit highlights once and fade removed units/cables (all disabled when `ui_reduce_motion` is `true` in `config.json`), and fix faint zero-alpha `AGENT` badge boxes on untouched units. Extended `--demo-agent` (with `--demo-agent-anim` / `Shift+F12` replay) for full offline UI captures.
- **Single-rig presets** (#34): save one rig (units, cables, knobs, plugin
  states, matrix routes, mix, name) as a named `.ksalat-rig` preset in
  `~/kabelsalat-rig-presets` and load it later as a new rig or replacing an
  existing rig, independent of whole sessions. Available from the Rigs strip
  ("Save rig" / "Load rig" and rig-chip context menu) and as agent/MCP tools
  `save_rig`, `load_rig`, and `list_rig_presets`.
- **Jev decides first:** the in-app agent asks TypeSafe's Jev decisions model
  (`~typesafe/jev-latest`, OpenRouter's decisions API) about every request.
  A single clear action (tempo, a rig's volume / mute / solo, keyboard split,
  add / delete / show a rig, transport, undo / redo, flip) runs from that one
  decision, measured at 0.2–0.5 s instead of 2–2.5 s through a chat model.
  Everything else goes to the chat model as before. Needs
  `openrouter_api_key` in `config.json`; `agent_decide_model: "off"` turns it
  off.
- **More requests settled by Jev's decision:** a rig a bit quieter or louder
  (one step of about 3 dB from where it is), pan left or right, add one
  built-in effect to a rig (wired in front of its Master), remove a rig's
  effect (the gap is closed), load a sound by kind (pad, bass, lead, keys,
  organ, strings, brass, pluck, bells, FX: the patch search gets the user's
  words first, then the kind), add or start the metronome, start and stop
  recording. Each is a plan of the same tool calls a chat model would make,
  so Confirm / Cancel, undo and Agent Activity behave the same. Measured
  with `agent-eval` at 0.3–0.4 s per request instead of 2–6 s. A request
  Jev is unsure of, a question, or a request for two changes still goes to
  the chat model.
- **Images for the assistant:** drop a PNG or JPEG on the window, or paste
  one into the open chat. It shows as a chip above the input (a click
  removes it) and goes out with the next message. A turn with an image is
  answered by the seeing model (`agent_perceive_model`, default
  `google/gemini-3.8-flash` on OpenRouter), which has the same tools. An
  image is sent only during its own turn; later turns keep one line that
  names it.
- **`look_at_rack`:** the assistant and MCP agents (`kabelsalat mcp
  --attach`) can ask for a screenshot of the app window, scaled to at most
  1400 px and PNG-encoded off the UI thread. MCP gets it as image content.
- **`kabelsalat jev-test --image <file> <request>`** asks Jev about one
  request without and with an image.
- **Model routes per role** in `config.json`: `agent_decide_model`,
  `agent_act_model` (also set by `/model` in the chat) and
  `agent_perceive_model`. Ids with a vendor prefix (`vendor/model`) go to
  OpenRouter, bare ids to DeepSeek.
- **`kabelsalat agent-eval`** runs the real agent loop over 26 scripted
  requests on a headless app with an offline engine and reports pass rate,
  model steps, tool calls, wall time and the models that answered.
  **`kabelsalat jev-test`** shows what Jev decides for sample requests.
- **Agent Activity shows the cost of the last request:** model steps, model
  time and the model that answered.
- **Confirm / Cancel buttons in the chat** (#32): when the assistant asks
  before a destructive action (delete a rig, install an instrument), the chat
  shows the two buttons; the click runs or cancels it.
- **Live meters for other apps** in the mixer's System audio strips (YouTube,
  …): measured per stream with `parec --monitor-stream`, dB-scaled.
- **MCP `play_file` / `stop_file`** (#22): attached agents
  (`kabelsalat mcp --attach`) can start and stop backing tracks through the UI
  bridge; headless `stop_file` mutes the player.
- **Reset an unbound knob from its menu** (#20): right-click arms MIDI learn,
  a second right-click opens the menu (Cancel MIDI learn / Reset to default)
  instead of only cancelling.
- **setBfree plays** (#36): the LV2 host now provides the Worker extension
  (`work:schedule`); each plugin with a `worker#interface` gets a non-RT worker
  thread fed through two lock-free rings (requests and responses), with
  `work_response`/`end_run` delivered in the next `run()` cycle. This unblocks
  setBfree, which requires `urid:map` and `work:schedule` to instantiate.
- **Rename rigs:** double-click a rig chip; agent tool `rename_rig`. Rig names
  are saved with the session (#33).
- **Built-in instrument browser** (#10): an "Install" tab in the Plugins view
  shows a hand-picked catalog — setBfree, Surge XT, Dexed, Vitalium and YANES —
  each linked to its GitHub project. A distro package is preferred (prebuilt,
  signed), otherwise the instrument is built from source into `~/.clap` /
  `~/.lv2`. Installs run on a worker thread with a live log in the view and a
  plugin rescan when they finish; "Installed" badges reflect the plugin scan or
  the bundle on disk. Agent parity: `list_instruments` and `install_instrument`
  (confirm-gated) in the assistant and MCP.
- **System audio in the mixer:** every other app that is playing (YouTube in
  the browser, …) gets a strip with volume and mute (#31).
- **Output device selection** at Master out (e.g. MOTU M2 vs. speakers):
  moves the live stream without a dropout, remembered across restarts (#31).
- Agent tools `list_audio_outputs`, `set_audio_output`, `list_app_streams`,
  `set_app_stream`.
- **Audio In rack unit** (#28): plug in a microphone, guitar or audio interface
  and play it live through the rack. An Audio In node picks a device (cpal, hot
  plug rescans every 2 s — a MOTU M2 shows up as "MOTU M2", readable, not the
  raw ALSA id) and a channel (In 1 / In 2 / stereo), has gain and an input
  meter, low-latency monitoring (monitor adds it straight to the master), and a
  tuner on the face (YIN pitch detection off the audio thread, note + cents).
  The input ring is lock-free (`rtrb`), the render path stays allocation-free,
  and a small drift controller reconciles input vs output clocks (drop/insert
  around a target fill). When the interface is also the output, the input
  opens at the output sample rate so they share one clock. `kabelsalat
  audio-in-test` loops the default input to the output and reports measured
  stream latency. Agent parity: `add_node kind="audio_in"`, `set_audio_in`,
  `list_audio_devices` in the assistant and MCP; sessions save the selection.
- **The agent can measure the mix** (`measure_mix`, also in `get_status`):
  momentary, short-term and integrated loudness (LUFS), true peak, a bass
  mono check and a 4-band balance. `kabelsalat meter-test` checks it with
  real Surge audio (#11).
- **Undo for agent changes:** one request (or one burst of MCP calls) is one
  step; `undo`/`redo` tools and Ctrl+Z / Ctrl+Shift+Z.
- **External agents are visible:** "● Agent connected" in the top bar while an
  MCP client acts; one click pauses it.
- After recording, the top bar names the saved take with **Play** and **Folder**.
- **Switch the assistant off:** Settings → Agent → Models has an **Assistant** switch. Off takes the assistant out of the top bar and the dock (keys C and F12 no longer open it). MCP and CLI agents keep working.
- **Choose the listening model:** the model that hears your sound when the assistant checks it is now on the Models page, next to the chat model and the decisions model. Each takes any OpenRouter model id.
- **See when the assistant listens:** while it records the master, a bar shows how far the recording is (in the assistant dock, on the top bar's Assistant button and on the rack's hint line, so you see it with the keyboard dock open too). It tells you when nothing sounds yet, and afterwards that the listening model is judging what it heard.
- **Arpeggiator and chord unit (#90):** an Arp unit sits between your keyboard
  and a synth. Held keys become an arpeggio (up, down, up-down, random) at a
  note value synced to the tempo (1/4 to 1/32, with triplets), over 1 to 4
  octaves, with gate and swing, or in chord mode one key sounds a whole chord
  (major, minor, sus2, sus4, 7th, maj7, min7 or power). Every setting is a
  knob you can learn to a MIDI controller, and the assistant and MCP agents
  can add and tune it (`add_node` kind `arp`, `set_arp`).
- **Scale snap on the keys (#90):** pick a key and a scale (major, minor,
  dorian, mixolydian, harmonic minor, pentatonics, blues) on the Keys unit and
  out-of-scale keys play the nearest in-scale note, so no key sounds wrong.
  The assistant can set it too (`set_scale`). Both survive sessions and undo.
- **Configurable audio buffer size (#160):** adjust the output buffer size (64,
  128, 256, 512, 1024, or 2048 frames; default 256) live with **− / +** in
  Engine info, from **Settings → Buffer size**, or via the assistant/MCP tool
  `set_buffer_size`. Smaller buffers lower latency; larger buffers add headroom
  to avoid audio dropouts (xruns). Reopens the audio stream without losing the
  rack and persists in `config.json`.
- **MIDI Clock Send (#85):** kabelsalat can be the master MIDI clock. Switch it
  on in the matrix view, wire it to any MIDI output port (or connect an app to
  the virtual "MIDI Clock" port) and your hardware follows the transport: 24
  clocks per beat, Start when the song starts, Stop when it stops, optional
  Song Position Pointer with Continue. A sync offset in milliseconds and three
  latency modes (none, output, input) line the clock up with the audio; a
  negative offset adds a pre-roll by itself. The clock is timed by the ALSA
  sequencer from the engine's own song position, so it does not wait for the UI.
  The assistant and MCP get `get_midi_clock` and `set_midi_clock`.
- **MIDI effects in the matrix (#86):** an LV2 MIDI plugin (note transposer, arpeggiator, …) is now a "MIDI strip" unit. Notes go source → MIDI effect → instrument through the matrix, the strip's face shows the plugin's own main controls (learnable from a hardware controller), and a built-in transposer is always available. Agents get `list_midi_fx` and `add_node` kind `midi_fx`.
- **File player: Loop, Sync, Number of Beats, Beat Offset (#84):** the backing-track
  player now matches AUM's FilePlayer. *Loop* restarts the file seamlessly (the seam
  is the file's own end→start, no gap or click). *Sync* plays the file at the
  transport tempo as varispeed from its original BPM (a "120 bpm" in the file name
  is picked up; no time-stretch). *Number of Beats* sets the loop length in beats, so
  an aligned loop stays locked to the metronome however long it runs. *Beat Offset*
  shifts the start point. With Sync on and the transport rolling, play starts at the
  next bar line (beat 0 after a count-in); a tempo change mid-play is followed with
  no phase jump. The assistant and MCP tools `play_file` and `set_player` take
  `loop`, `sync`, `beats`, `offset` and `bpm`; `get_status` reports the player.
- **Per-rig stems (#6):** the mixer's rig bus strips get a record-arm button (**R**), and Master out gets **ARM**. One REC press records every armed target at once: the master mix and each armed rig's post-fader sum, each in its own 32-bit float WAV named after the rig, all in one take folder. Pressed while the transport rolls, REC starts and stops on the next bar (press stop twice to stop at once). Stopping the transport keeps the take running, so effect tails are captured. Agents arm targets with `set_record_arm`; `record_status` lists the stems and arms.
- **Eleven more plugins in the catalog (#173):** Plugins → Install now offers
  OB-Xf (Oberheim), Ultramaster KR-106 (Juno), Six Sines (FM), JC303 (TB-303),
  Cardinal (modular), OneTrick KEYS (modelled piano), OneTrick CHONK (electric
  bass), OneTrick CRYPTID (FM drums), AVL Drumkits (sampled kits), Starbreath
  (modelled winds for wind controllers; freeware, x86-64 only) and the x42 MIDI
  filters. Odin 2 now installs from the Arch package on both CPUs, with its
  factory patches.
- **CLAP audio effects (#181):** CLAP audio effects and utilities (Surge XT Effects, Dragonfly Reverb, Airwindows Consolidated, LSP, Chow, …) run as rack effect units, mixer strip inserts, aux-bus inserts and master inserts, in stereo or mono, with sidechain inputs fed silence. Dragonfly Reverb and Airwindows Consolidated join the install catalog.
- **Clone rig:** duplicate any rig (rack) into a new independent rig from the Left Rail `⋯` / right-click menu or the inline rig pill right-click menu. Copies units, cables, knob values, plugin states, matrix routes, mixer strips, sends, bus outputs, MIDI device binding, voice switches, patch names, and rig mix, and names the clone `<name> Copy` (`<name> Copy 2`, …). Agents and MCP callers can use `clone_rig`.
- **DSP load and Engine info (#106):** the strip under the top bar shows
  `DSP 22 %` at its right end: the average render time as a share of one
  audio buffer. It turns amber from 70 % and red above 90 %, and adds a red
  count once the sound card reported xruns. A click opens Engine info: sample
  rate, buffer size and duration, inputs and outputs, render average and
  maximum, late blocks, xruns, the MIDI input latency and the output buffer,
  and the render time of every instrument and effect (average, maximum, share
  of the buffer), most expensive first. "Reset peaks" restarts the maximum,
  the late blocks and the xruns. The sound card's own latency and the
  audio-input latency are shown as "not measured", because kabelsalat has no
  trustworthy figure for them. The assistant and MCP agents read the same
  numbers with `get_engine_info` and reset them with `reset_engine_peaks`.
- **Akai EWI 4000s and EWI 5000 instrument profiles:** built-in MIDI Guide
  profiles (`instruments/akai-ewi-4000s.csv`, `instruments/akai-ewi-5000.csv`)
  covering breath (CC 2, plus CC 34/43 Hi-Res LSB on EWI 5000), glide
  (CC 5 / CC 65, plus CC 104 legato time on EWI 5000), hold/sostenuto
  (CC 64 / CC 66), modulation, expression and filter CCs. Both profiles bind
  CC 2 breath to `CcTarget::ChannelPressure` on CLAP and LV2 synths so breath
  drives both CC 2 and aftertouch wind patches while leaving glide, sustain
  and other performance CCs passing through. The EWI 5000 auto-matches its
  USB MIDI port (`EWI5000`).
- **Audio In unit with a tuner (#28):** a rack unit for any PipeWire input, with gain, a level meter, a MON switch for low-latency monitoring (about 19 ms round trip measured) and a tuner on its face. A missing input says so instead of using the laptop microphone.
- **Guitar rig (#92):** ⇧F5 loads Audio In → Scream → Delay → Reverb → Amp → Cabinet → Master from the Guitarix LV2 plugins (install the Guitarix card), and the assistant can build it for you.
- **The on-screen keyboard follows the instrument (#163):** Piano (damper and soft pedals, touch velocity), Jazz Organ (drawbars, registrations, Leslie, swell, two manuals on channels 1 and 2), Pipe Organ (stop jamb, pistons, tremulant, couplers, swell) and Synth (wheels, the patch's macros, an X/Y pad, scale FOLD, arpeggiator). It switches with the active rig or stays on the one you pin, and the session remembers the pin.
- **Expanded keyboard (Shift+V):** the whole window becomes the instrument, a full-screen console per keyboard type (piano action, tonewheel organ with drawbars and rotary speaker, pipe-organ console with drawknobs and pipe front, synth panel with display and X/Y pad) that moves as you play; Esc or DOCK brings the rack back.
- **Keys hold as long as you press them:** a pressed on-screen key sounds until you lift the pointer, drags across organ keys play a glissando, and the held chord is named in the header.
- **Agent:** `get_keyboard_state` and `set_keyboard_mode` read and switch the keyboard in the app, over MCP and in headless MCP.
- **Loudness target for the master (#12):** the Mixer has a Loudness column
  beside Master out. Pick −14 LUFS (Spotify, YouTube, Tidal), −16 (Apple
  Music), −8 (club) or step to any value; kabelsalat trims the level in front
  of the master limiter by what your playing was missing and holds the true
  peak under −1 dBTP. The column shows the loudness before, the loudness now,
  the true peak and the trim. "Match" measures again and corrects the trim,
  and says so when the limiter has no more to give and the master VOLUME has
  to come up. The target is saved with the session and follows undo. The
  assistant and MCP agents use `set_master_target` and `get_master_target`;
  `get_status` carries the same readout.
- **MIDI learn for the mixer (#75):** right-click a fader, a pan field, or a
  mute or solo button in the mixer and move a control on your MIDI controller
  to bind it, on source strips, rig buses and the master. A learned fader
  shows its CC beside the dB readout, and the control follows your hardware on
  screen. Bindings are saved with the session and stay with their rig when
  rigs are added or deleted. The assistant and MCP agents bind them too
  (`set_cc_map` / `learn_cc` with `mix: vol | pan | mute | solo`).
- **dB faders, peak hold and clip lights (#72):** every mixer fader rides the meters' dB curve with a 0 dB detent (double-click for exact unity), a dB readout, a held peak line and a clip light you click to clear.
- **Mute, solo and pan on every source (#73):** each source strip has its own M, S and pan, scoped to its rig; the engine publishes them back and sessions keep them.
- **Insert slots in the channel strip (#74):** the strip lists the effects in its cable chain; add, bypass, reorder and remove them, and the back panel's cables follow.
- **Names, colours, reorder and collapse; master inserts (#77):** rename a strip, give it a colour, drag it to reorder, collapse it to a mini strip, and put effects on the whole mix before the limiter. Everything is saved with the session and available to the agent (`set_strip_mix`, `rename_strip`, `set_strip_color`, `move_strip`, `collapse_strip`, `add_insert`, `remove_insert`, `set_node_bypass`).
- **Quick knob per plugin in the mixer channel strip (#107):** every source and insert slot in the mixer now carries a small assignable knob at its right end with the parameter's caption beside it, linked directly to the rack unit's knob (and engine/plugin parameter). It defaults to a plugin's first main control or an effect's `MIX` knob, resets on double-click, and right-click opens a dropdown to choose any knob of that unit; assignments are saved with the session and exposed to agents via `get_strip_knobs` / `set_strip_knob`.
- **Navigator (#108):** press **Q** or click **Navigator** in the top bar for a side list of every rig, its channels in their strip colours and each channel's plugins (instrument, then inserts, marked CLAP, LV2 or built-in). **CHANNELS / PLUGINS / ALL** filter the list and the chevron folds a channel. Clicking an entry switches to its rig and selects the unit in the rack (panning to it), or scrolls to and flashes its strip in the mixer. Agents read the same list with `list_channels`.
- **`kabelsalat engine-bench`:** times the engine offline, with no audio device and no plugins, and prints a hash of the output so two builds can be compared.
- **Plugins organized by type:** the **Plugins** view (**F9** / **B**) groups built-in units, CLAP and LV2 plugins as **Instruments**, **Effects**, **MIDI FX** and **Utilities** instead of by format. Each type narrows by subcategory (Delay & Reverb, EQ & Filter, Dynamics, Drive & Amp, Modulation, Synths, Organs, Samplers, Analyzers, Mix & Gain, …) and by format (Built-in, CLAP, LV2). Every built-in effect and the Arpeggiator are listed there too, and LV2 cards show the author and plugin class instead of a URI.
- **Add without losing the rack:** **New rig** puts a CLAP or LV2 instrument on a rig of its own beside the others, **Add to rig** layers an instrument or chains an effect into the active rig, and **Master FX** inserts an effect on the master before the limiter. Adding an LV2 plugin used to replace the whole patch.
- **Insert picker by category:** the Mixer's **+ insert** filters by Built-in, Delay & Reverb, EQ & Filter, Dynamics, Drive & Amp, Modulation and Analyzers, lists LV2 effects without typing a filter first, groups them by category and leaves out MIDI-only plugins.
- **`list_plugins` by type:** the assistant's and MCP's `list_plugins` take `type`, `category`, `format`, `query` and `limit`, list built-in units too, and tag every plugin with its type and category; `list_instruments` tags catalog entries as instrument or effect.
- **README stays current:** PRs labelled `enhancement` (new features) must
  update `README.md` in the same change — CI fails them otherwise, and
  warns when a feature changes UI code without new screenshots.
- **Other apps' strips say whose they are (#31):** each System audio strip in the mixer now shows the app and what it plays above its fader (a long title is cut, the full text is in the tooltip). Before, a strip was a fader with no name.
- **`kabelsalat audio-out-test`:** checks the output selection and the System audio strips against the machine's real PipeWire: it plays a silent stream and a test tone into two temporary outputs, moves the stream to a chosen output and back, and turns the tone down, mutes it and meters it.
- **A bus on its own output (#147):** every rig bus and every aux bus has an
  output menu in the mixer. Pick a sound card there and that bus plays to it
  only and leaves the main mix: the click or a monitor mix on the headphones,
  the main mix on the PA. Its fader, pan, mute, solo and inserts still act on
  it. Up to four outputs besides the master's. The choice is saved with the
  session; while the device is unplugged the bus is silent and never moves to
  another output, and it plays again when the device is back. The assistant
  and MCP agents do the same with `set_bus_output` and `get_bus_outputs`.
- **Sends and aux buses (#5):** add an aux bus in the mixer and send to it from any rig bus or source strip, before or after that strip's fader. The bus is a full channel with its own fader, pan, mute, solo, meter and insert slots, so several rigs can share one reverb or delay; it returns to the master past the rig faders. Send levels learn a MIDI controller like the faders. Sends are saved with the session and available to the agent (`add_bus`, `set_send`, `get_sends`, `remove_bus`).
- **Hosted plugins are delay-compensated (#76):** a plugin that reports a latency (LV2 and CLAP; a look-ahead compressor, a linear-phase EQ) no longer puts its path out of phase with the dry sound beside it. When the plugin changes its latency while you play, the paths line up again at once.
- **Latency per strip (#76):** a mixer strip that sounds late shows by how much, in milliseconds under its fader.
- **Agents pick a port's instrument profile (#56):** the assistant and MCP
  agents can say which hardware instrument sits on a MIDI port, as the MIDI
  matrix's profile picker does: "the Nord Electro 2 is on the second port"
  binds its drawbars, rotary, percussion and drive to the organ in one step.
  `list_instrument_profiles` shows every profile, what it would drive in the
  current rack and what each port has now; `set_device_instrument` picks one
  (or `none`). The pick shows in the matrix and its bindings are saved with
  the session.
- **Parallel paths stay in phase (#13):** when a signal splits and one path
  runs through a Limiter (it looks 5 ms ahead to catch peaks), the other
  paths now wait for it, so a dry sound and its limited copy add up cleanly
  at the master or at an effect both feed, instead of 5 ms apart and hollow.
  A bypassed Limiter keeps its delay, so switching it never shifts the mix.
- **`kabelsalat analysis-test` (#13):** checks the engine by what it sounds
  like: a note's pitch, brightness and aliasing from its spectrum, the
  master's crackle with a loud chord, routing proven with noise, and the
  delay compensation. `--save <dir>` writes the clips that stay a matter of
  ear, each named with what to listen for.
- **Live demo phrase per rig (#200):** press **▶** on any rig in the Left Rail (or call `play_demo` / `stop_demo`) to loop a live note phrase matched to the rig's sound (keys, bass, lead, pad, organ, drums, or wind with breath) on the engine clock at the session tempo, so you can audition and tweak a rack without a keyboard.
- **Time signature (#87):** the clock is no longer fixed to 4/4. Pick the beats per bar (1 to 12) and the note that counts as one beat (2, 4, 8 or 16) under **Settings → Session**, or step through the common ones (4/4, 3/4, 6/8, 5/4, 7/8, 2/4) with the new chip on the metronome. The metronome clicks once per beat, eighths in 6/8, and plays the first beat of every bar higher. The count-in lasts whole bars of the signature, REC and a synced backing track start and stop on its bar lines, and hosted plugins are told the signature and where the bar began. It is saved with the session; older sessions keep whatever is set. Agents use `set_time_signature` and read it in `get_status`.
- **Unsampled Piano in the plugin catalog (#247):** added [Unsampled Piano](https://unsampled.mgth.fr) (`fr.mgth.unsampled-piano`, GPL-3.0-or-later), a physically modelled acoustic grand piano CLAP plugin with felt hammers, coupled stiff strings, modal soundboard, lid/room reflections, sympathetic resonance and full pedal support (prebuilt release on x86-64, built from source on aarch64).
- **Version and commit in the app:** Settings shows the version and the commit
  the build came from (`kabelsalat 0.1.0 (abc1234)`), and so does
  `kabelsalat --version`. Builds without a git checkout can pass the commit as
  `KABELSALAT_COMMIT`; otherwise it reads `unknown`.
- **Breath from a wind controller reaches your instruments (#68):** channel aftertouch, how an EWI or another wind controller sends breath, now plays CLAP and LV2 instruments. Wind patches like the Electrowinds Vital bank open with your breath instead of staying silent. Poly aftertouch reaches them too, and follows the split and transpose of the key it presses. `kabelsalat wind-test [patch]` plays a Vital wind patch with a breath swell through the engine.

### Changed
- **Native Wayland:** the app no longer forces XWayland and needs no X
  server. It renders at the compositor's fractional scale (sharp at 1.6x),
  opens maximized as it asks to, and text input goes through the Wayland
  input method instead of fcitx's X bridge. The Flatpak drops its X11
  socket. `KABELSALAT_UI_SCALE` still overrides the scale.
- **MIDI matrix sources read like devices:** hardware rows show the device name
  ("ESI MIDIMATE eX") instead of ALSA's "client:port 24:0" string, with a port
  chip only when a device has several ports and further ports joined to the
  first by a tree line. Each source carries a kind badge (KEYS, ANY HW, HW with
  a DIN socket in MIDI blue), hardware sits under a "Hardware MIDI · n of m
  ports live" header, and unplugged devices dim with an UNPLUGGED chip. The rig
  strip uses the same device names.
- **Rope cables:** patch cables are now Verlet ropes (gravity, slack, plugged
  ends pinned, a free end falls) instead of spring-driven Bézier curves.
  Grabbing a cable pulls its nearest point; the ghost wire while patching is a
  rope too. Stepped at a fixed 120 Hz, so the feel is frame-rate independent.
- **Themed dropdowns (`theme::Dropdown`):** replaced all three `egui::ComboBox`
  selectors (MIDI matrix CC parameter picker, assistant header chat-model
  picker, mixer audio output) with a Hurricane-styled dropdown matching the
  knob context menu (`DARK_PANEL`, `1px HAIRLINE` border, 4 px radius, soft
  shadow, `LIVE_TEAL` selection mark, scrollable list, Esc-to-close, truncated
  labels with full hover tooltips).

- The assistant's model picker is labelled "chat:" and no longer offers the
  `typesafe/jev-router` chat router when a DeepSeek key exists (it read like
  "turn Jev on" and made chat steps 2-8 s). A "Jev decides first" note shows
  that Jev's fast decisions run on their own.
- **`agent_act_model` replaces `assistant_model`** in `config.json`; the old
  field is no longer read. A request stops after 24 model steps with a
  message instead of running on.
- **I** toggles the info line at the top right (was D, which is also a note
  key); it now starts with the last incoming MIDI event — device, note and
  velocity, or CC number and value.
- **The assistant needs only an OpenRouter key:** one key reaches every model. The separate DeepSeek key is no longer used; a DeepSeek key left in your config is ignored (the app tells you so where it asks for the new key), and a model setting from that time ("deepseek-chat") falls back to the default. DeepSeek's models remain available through OpenRouter.
- **New default chat model:** `anthropic/claude-haiku-5.5`. Jev still decides simple requests first, and `google/gemini-3.8-flash` still listens.
- **The assistant says what it is doing:** the line under a running request now reads "Reading your request", "Thinking", "Planning the next step · after Load sound · Warm pad" or names the tool it waits for, with the model and the seconds so far, instead of "step 2".
- **The file player reads files in the engine:** it used to stream a pre-resampled
  ring, so a sped-up or looped file could not stay on the beat. Files now decode once
  on a worker thread and the engine reads them at the exact song position.
- **Nightly builds:** every package of the current `main` is published each
  night as the `nightly` pre-release on GitHub; stable releases stay "latest".
- **Rack side rails:** the rails at the rack's left and right edges are now
  narrow channels cut into the panel, with a fine edge, and they run exactly
  as tall as the units beside them, instead of soft rounded bars.
- **Smaller download:** release builds are about a fifth smaller (27.5 MB to 22.4 MB stripped on aarch64). They link with full LTO and no longer carry the screen-reader bridge, which the custom-painted rack never fed.
- **Master limiter does less work per sample:** at the default WARM it computes two `tanh` instead of five. The sound is bit-identical.
- **Reverbs and big racks cost less CPU:** a Reverb unit takes about half the processing time it did inside a full rack, and the engine no longer walks every cable once per unit and source in every block. A rack of 4 rigs with 20 effects renders in about three quarters of the time; the sound is unchanged, sample for sample.
- **Settings in pages, the assistant's apart:** Settings lists its pages in two
  groups. **App** holds Audio (output, buffer size), Session (autosave,
  count-in) and Display (reduce motion, UI scale). **Agent** holds Models and
  API keys, marked in the assistant's teal and "assistant only", so it is
  clear which settings change the sound and which change only the assistant.
  Each page shows what is set there at a glance, a missing key is flagged in
  the list, and a model without its key offers a button to that key. Small
  windows show the pages as a tab row.
- **Only the same sound waits (#76):** delay compensation used to make every source of a rig wait for a latent effect on one of them. Now only the paths that carry the same sound wait for each other, so an effect with latency on one instrument adds no latency to the others.
- **Metronome accent (#87):** the first click of each bar is now higher (1.5 kHz, the others stay at 1 kHz). The clicks sit where they always did.
- **Unified top bar, Left Rail & Session Library:** merged the permanent second rig strip and the right-side Navigator into one adaptive top bar and one collapsible Left Rail (`Q`). The top bar now always shows the open session name and autosave indicator (clicking opens a quick Save / Sessions / Templates popover, `Ctrl+O`), displays four live workspace views (`Rack`, `Mixer`, `Matrix`, `Plugins`) with `Settings` as an icon button next to `DSP`, and reveals compact inline rig pills (`1 · Name`, numbered pills, `+`) only when the Left Rail is collapsed so performance mode stays clean and taller. Opening the Left Rail (`Q`) shows the full rig, channel, plugin and Aux bus tree with live level bars, MIDI/audio input badges, `+ Rig`, and per-rig `⋯` actions alongside a unified `Library` tab for Sessions, Rig Presets and Starter Templates.

### Removed
- **Image input for the assistant:** dropping or pasting an image into the
  chat, the `look_at_rack` screenshot tool (in-app and MCP), the vision model
  role (`agent_perceive_model` in Settings and config.json) and
  `jev-test --image` are gone. The agent works from text and the rack's own
  data. Dropped `.vital` / `.vitalbank` files are still imported.

### Fixed
- **The Load rig preset dialog has a clean layout.** "Load into" lists the new
  rig and every rig to replace as one row each with a radio mark; long rig
  names no longer wrap letter by letter into tall columns.
- **A new delay echoes instead of distorting:** the TIME knob started at
  21 ms with feedback, a metallic comb that sounded like distortion. Its
  range is now 50 ms to 2 s, so a new delay starts at about 220 ms. Saved
  delays keep their knob position and land on a longer time.
- **Effects can be inserted from the mixer again:** "+ insert" opened its
  effect picker and closed it in the same click, and a picked effect was
  dropped. The picker now stays open and the chosen effect joins the
  strip's or the master's insert chain. Clicking a strip's name opens a
  rename dialog that stays open until Rename or Cancel (it vanished after
  one frame).
- **The assistant can add Vitalium (and any LV2 instrument):** the plugin
  list it reads stopped after the first 30 of about 300 LV2 plugins, and
  Vitalium was not among them, so it guessed addresses that do not exist and
  no Vitalium patch could be loaded. Instruments now come first in that
  list, and a wrong address is refused with the right ones.
- **A MIDI device turned off for an LV2 instrument stays off:** with two rigs
  on two keyboards, the keyboard of the CLAP rig also played an LV2
  instrument such as setBfree, when its cells were set while the plugin was
  still loading (session load, new rig) (#123).
- **Meters no longer read hot at normal levels:** the meters' colour zones
  were left on the old linear scale, so anything above −24 dBFS showed
  tomato and anything above −3 dBFS red. Tomato now starts at −6 dBFS and
  red only at the master's −1 dBFS ceiling.
- **Small and tiled windows:** the rack fits windows down to the 760 px
  minimum instead of panning with Keys and Master cut off; a short, wide
  window lays each instrument chain out in one row; the Mixer, Matrix,
  Plugins and Settings sheets give their margins to content, scroll instead
  of painting into the keyboard or assistant dock, and the master meter no
  longer stacks its dB marks. The rig strip hint is shown only where it fits.
- **UI scale on a second monitor:** the scale follows the monitor the window
  is on (it kept the laptop's 1.6x on a 1.0x external screen).
- **An instrument the assistant adds plays at once:** `add_node` wires a
  CLAP instrument or an LV2 instrument (setBfree, Vitalium, …) like a new
  rig's synth: the rig's Keys into it and it into the rig's Master. A
  setBfree added to a rig was left with only its MIDI cable and stayed
  silent. The assistant also no longer believes LV2 instruments are
  "coming soon".
- **MIDI matrix, LV2 instrument columns:** a click in a setBfree (or any LV2
  instrument) column lit or cleared every cell of it, because all its rows
  showed the MIDI cable. Each cell now routes its own source, with the same
  keyboard switch and hardware radio group as a Surge column.
- **A failed catalog install could not be retried:** the next attempt found
  the earlier clone in the build cache and `git clone` refused it. Every
  install now starts from a fresh cache folder.
- **A stuck source could silence everything:** an instrument outputting a
  constant (DC) filled the master meter while every rig went quiet, and sent
  the offset to the speakers. The master now blocks DC.
- **Matrix routes and MIDI-learn bindings were lost after agent edits:** an
  agent change to the rack dropped them from the app, and the next rig add or
  delete removed them from the engine too. They now survive both.
- **MIDI-learned knobs did not move on screen:** a hardware CC bound to a
  knob changed the sound, but the knob stood still. It now follows the
  controller.
- **Master WARM acted like a volume knob:** turning it up made quiet playing
  up to 7.7 dB louder. WARM now changes only the colour (saturation); the
  loudness stays that of the default setting.
- **Plugin knobs that drove the wrong thing or nothing:** every LV2 unit had
  a `GAIN` knob wired to the plugin's first control port, whatever that port
  was (Vitalium: its freewheel switch; setBfree: nothing), and a CLAP synth
  had a `TONE` knob that only tinted its cable. Both are gone: a plugin knob
  is now always one of the plugin's own controls, under the plugin's name.
- **Arch package:** declares the MIT license, installs `LICENSE` and takes
  its version from the git tag (`0.1.0.r<n>.g<sha>`, was `0.0.1.r…`).
- **Crash with more synths than voices:** an undo (or a source/device edit)
  in a session with five or more CLAP units panicked. Units past the voice
  pool now have no per-voice state, and agent tools say so.
- **The agent browses every instrument's own sounds.** `list_sounds` and
  `load_sound` searched Surge patches whatever a rig held, so a YANES,
  setBfree or Vitalium rig was offered Surge patches that mean nothing to it
  (and a load wrote a Surge patch into the wrong plugin or failed with "no
  CLAP synth"). Both tools now take `rig` (and `node`), find that rig's
  instrument and work in its library: Surge patches, Dexed DX7 cartridge
  voices, an LV2 instrument's presets and MIDI programmes (setBfree's 110
  organ registrations, Vitalium's presets), or a plugin's own preset list
  (YANES' 146 presets on its "Preset" parameter). The answer names the
  instrument and the library; an instrument without sounds says which
  parameters shape it; a rig with different instruments asks for the node.
  The same holds for MCP (attached and headless) and for the Jev sound
  shortcut. Check with `kabelsalat sound-library-test`.
- **A listed sound loads exactly that sound:** `load_sound` with a label from
  `list_sounds` could load another patch that shared a word with it, because
  factory patches were preferred before the exact label was tried.
- **A shortcut that hands over to the chat model no longer fails with a
  DeepSeek 400:** when a Jev plan ran tools and then left the request to the
  chat model (no sound matched, a rig needed a node named), DeepSeek's
  thinking models rejected the conversation ("reasoning_content … must be
  passed back") and the user got an error instead of an answer.
- **Sound libraries are scanned off the UI thread** (rule 46): the first
  `list_sounds` walked about 3000 patch files on it. An agent call that
  arrives during a scan waits for it.
- **Any number of CLAP instruments** (#2): every Surge XT, Dexed, YANES or
  other CLAP unit in the rack gets its own sounding voice. The app hosts a
  voice when a unit appears (add rig, add instrument, template, session or
  rig preset load, undo) and frees it when the unit goes. The fixed pool of
  four Surge voices is gone: a fifth synth is no longer silent, "+ Rig"
  always builds Keys → Surge → Master, and the keyboard/hardware switches,
  the pinned MIDI device and the patch name work for every CLAP unit.
  `kabelsalat many-voices-test` hosts eight Surge XT plus Dexed and YANES,
  plays them all and frees them.

- **The app and the engine could disagree on which synth plays which unit:**
  after a CLAP unit was removed (agent, MCP), sounds, parameter edits and
  saved state of the remaining synth went to the freed instance. The engine
  now publishes which voice plays which unit and the app follows it.
- **Input switches survive rig edits and undo:** adding or deleting a rig
  turned every synth's keyboard/hardware switch back on, and undo lost a
  synth's pinned MIDI device. A synth that had its keyboard switched off
  no longer stays deaf after a template or session load.
- **Crash with more synths than voices:** an undo (or a source/device edit)
  in a session with five or more CLAP units panicked.
- **Held notes stay lit** on the software keyboard and the Keys unit for as
  long as they are pressed, from a hardware keyboard too (they only blinked).
- **Assistant panel polish:** gave destructive `Confirm` buttons an outlined danger treatment (`DANGER_BG` + `CLIP_RED` border + `CLIP_BRIGHT` text) distinct from `Send`, removed the duplicate live status from the header, switched action chips to rig display names (`rig_name`), hid read-only steps from the chip row (counting them in the cost line), raised contrast on role labels, chip text, `Cancel`, step metadata, and the attachment `×`, aligned `Confirm`/`Cancel` and the attachment chip to the transcript column, and replaced debug pixel-dimension text with a cached 32 px attachment thumbnail texture.

- **`transport-test` step detector flake** (#18): Surge's "Gate Chord" patch
  clears `trigmask` bit 12 (tied envelope), but step 12 still opens the filter
  over the previous step and delay (~8% of onset energy). Scoring on the true
  silent steps (`9, 10, 15`) removes the ~5–8% false-rotation rate across runs.
- **"Rig 1" is the first rig for the chat model too:** it took the user's
  "rig 1" for the tool argument `rig=1` (the second rig) in 2 of 22 runs of
  a two-change request. The system prompt now states the mapping.
- **Several `disconnect` calls in one agent step all take effect:** every
  second one reported `NOT applied` and its cable stayed, because the next
  call ran before the engine had answered the one before it.
- **The agent's `disconnect` works:** the cable came back in the same audio
  block because the rack re-sent it every frame; the tool reported success.
- **No more doubled units from the agent:** a `get_graph` right after
  `add_node` showed the patch from before the edit, so the model added the
  unit again. Tool calls that the engine applies now finish before the next
  call runs, and their results name the node and cable ids they created or
  removed (or say that nothing changed).
- **The agent survives a flaky network:** timeouts, connection errors, 429
  and 5xx are retried up to three times (honoring `Retry-After`) instead of
  ending the turn.
- **Wrong tool calls are corrected, not guessed:** an unknown tool or
  arguments that do not fit the tool get an error the model can act on; they
  used to run with default values.
- **Long chats stay fast:** the conversation sent to the model is capped
  (about 15k tokens); old tool results are shortened and the oldest turns
  dropped whole.
- **Effect chains are audible:** an LV2 instrument into a built-in effect
  (setBfree → Drive) and any effect into another effect (Delay → Reverb) were
  silent at the master. Effects now run in signal-flow order and hear every
  source wired into them.
- **Hanging LV2 notes** (setBfree): a note-off and a note-on in the same
  audio block (legato) lost the note-off, and chords played only one note.
  Every event now reaches the plugin once; atom outputs get a real buffer
  (#38).
- **Agents wiring over `mcp --attach`:** add_node → connect works at once and
  auto cable ids no longer collide; get_graph reflects edits immediately (#37).
- **Keyboard dock follows the window height** (#23).
- **LV2 instrument detection** uses the plugin's declared class; LSP effects
  no longer show up as instruments (#26).
- **System audio GAIN knob works** (#21): it sets the capture gain through
  SetParam (so it is MIDI-bindable), starts muted, and follows
  `system_audio`/`set_system_audio` changes from any agent.
- **Headless MCP knob names match the rack** (#19): one knob table for the
  rack, the in-app agent and headless MCP (`RESO`, `CEIL`; long names like
  `RESONANCE` as aliases); headless `list_params` lists the knobs. Headless
  `CEILING` no longer binds the DRIVE knob, and the limiter's params are named
  Ceiling/Drive.
- The knob context menu keys its knob by node + param, not unit/knob indices
  (#24).
- The top bar no longer runs past the window edge in narrow windows (compact
  mode, window-width based); the info line is truncated or moves to the hint row.
- The master output was 2–4 dB too quiet at full volume: level lost in the
  master limiter is now made up (peaks at −1 dBFS, never above), VOLUME and
  the templates default to 100 %, and the master meters use a dB scale.
- Vitalium (LV2) could corrupt memory and crash the app when an instance was
  removed or reloaded; instances are now kept and reused (#14).
- Agent tools no longer freeze the window while plugins are scanned for the
  first time (#15).
- A re-plugged MIDI keyboard keeps its rig binding and matrix row (#16).
- A MIDI interface re-plugged under the same name is reconnected instead of staying silent.
- Undo keeps the keyboard/hardware source switches (#17).
- Recording file names use local time instead of UTC.
- The file player reads 24-bit PCM and 32-bit float WAV (it can play the
  app's own recordings).
- **A CC the assistant binds shows on the knob (#130):** when the assistant bound a controller to a built-in unit's knob (for example an expression pedal on a Filter's cutoff), the sound followed but the knob showed no `CC` tag and its menu could not clear the binding. It now looks and behaves like one made with MIDI learn, including bindings made before the update.
- **Keys unit controls on narrow racks (#90):** the Keys face keeps room for
  its SPLIT and SCALE controls however narrow the rack gets.
- **CLAP plugins with long ids load:** ids were cut at 31 bytes, so Cardinal
  (`studio.kx.distrho.cardinal#synth`) could not be hosted. Sessions saved
  before still load.
- **CLAP instruments with more than 8 outputs load:** drum machines that put
  each drum on its own output (OneTrick CRYPTID: 12) were refused.
- **The catalog sees Surge XT, Dexed and setBfree installed from Arch:** their
  cards looked for bundle names the packages do not use. Dexed and setBfree
  now install only their plugin packages (`dexed-clap`, `setbfree-lv2`).
- **An LV2 plugin loads by its exact URI first:** a URI that is part of
  another one (`…#BlondeBop`, `…#BlondeBopHR`) could load the wrong plugin.
- **CLAP plugins get what they ask the host for (#180):** a plugin that
  asks for a call on the main thread, for a restart or for a parameter flush
  is now served within one frame, with its editor open or closed. Before,
  such a request was noted and never answered, so a plugin that finishes a
  patch or sample load on the main thread could wait for good.
- **Left Rail `⋯` rig menu button:** clicking the `⋯` icon on a rig row now opens its action menu reliably instead of being swallowed by a duplicate paint-pass click target.
- **Render average read too low:** `render_avg_us` in `get_status` lost up to
  63 µs to integer division on every step, so a 50 µs render read 0 and a
  steady 300 µs one read 256. The average is now kept in nanoseconds.
- **Headless `kabelsalat mcp` reports its own buffer:** `get_status` named
  the app's 256-frame buffer while the headless engine runs 512 frames; it
  now reports the size the audio device asks for, and counts its xruns.
- **A new Vitalium unit starts with the init patch and in silence (#174):** after a Vitalium unit was removed, the next one could play the removed unit's patch, and a note that was still held when the unit went kept sounding in the new one.
- **Monitoring no longer lags by hundreds of milliseconds:** a backlog in the input ring is cut at once, and input drift corrections no longer swap the left and right channels.
- **The software keyboard is full height again:** the pitch and mod wheels stand beside the keys and SUSTAIN and VEL sit in the keyboard's title row, so the keys get back the height the strip had taken (86 → 136 pt at 1600x1000), and in small windows the rack gets that room back.
- **Stereo LV2 effects keep the right channel:** every input of an LV2
  effect was fed the left channel, so a stereo effect played L in both
  sides. Inputs now take left and right in order; mono effects still take
  the left channel.
- **Master inserts sound like the same effect in a strip (#154):** an effect
  after Master OUT was processed twice per block, once on silence. A Delay
  there played its echoes chopped and at the wrong time, a Limiter let go
  between blocks, and every master insert ran twice. Each insert
  now runs once, on the mix.
- **True peak read too high:** `measure_mix` reported a peak at half scale as
  −3 dBTP instead of −6. The detector now interpolates correctly and the
  value is in dB of amplitude; `true_peak_max_db` holds the highest peak
  since the last reset.
- **Integrated loudness of dynamic playing read too low:** it averaged the
  dB values of its 400 ms blocks instead of their energy (a loud and a 10 LU
  quieter passage read 2.4 LU low).
- **Solo of one source silences the others (#73):** sources without a strip entry kept playing while another source was soloed.
- **Dexed no longer crashes the test suite (#52):** its destructor segfaults
  while other plugin instances render, so a retired Dexed is parked and reused
  by the next one, never destroyed.
- **Plugins page header (#65):** the "New <plugin> rig" confirmation has its
  own row under the subtitle instead of being painted over it.
- **Pipe organ register-to-pipe mapping and bass compass:** the Pipe Organ console's pipe facade now spans the full 8-octave compass (C0–C8) with C-octave markings on the impost and maps every drawn stop's pitch offsets (32', 16', 8', 5⅓', 4', 2⅔', 2', 1⅗', 1⅓', 1' and multi-rank mixtures, with flue vs reed glow) across all 128 MIDI notes, so lower tones and sub-octave stops light dedicated bass pipes and silent registrations keep the pipes dark.
- **A reopened session keeps each synth's inputs (#124):** which sources play
  a CLAP synth (keyboard on or off, hardware on or off, one MIDI device) is
  now saved with the session. Before, every synth came back listening to the
  keyboard and all hardware. The device is remembered by its name, so a
  re-plug or another order of devices at the next start does not move the
  synth to another keyboard, and a device that is not plugged in when the
  session opens stays the synth's device and plays as soon as it is back.
- **Plugins built for another CPU say so (#135):** a CLAP or LV2 plugin built for a different processor (an x86_64 `Vital.clap` on an ARM machine) was reported as "No such file or directory". kabelsalat now reads what the file was built for and says, for example, "Vital.clap is built for x86_64; this machine is aarch64, so it cannot be loaded". The Plugins view names such plugins, LV2 plugins that cannot run here are no longer listed, and a catalog card no longer calls one "Installed".
- **CLAP effects are no longer offered as instruments:** Surge XT Effects showed under CLAP instruments with a **New rig** button, although kabelsalat cannot host CLAP effects yet. It is listed under Effects as not hostable yet, and `add_rig` refuses a CLAP id that is not an instrument.
- **Filter chips wrap whole:** in the Plugins view and the insert picker, a chip that did not fit its row ran past the window or broke into one letter per line; it now starts the next line.
- **Add MIDI strip returns to the rack**, like every other add in the Plugins view.
- **Loading a session with an LV2 unit took 3 to 4 seconds (#55):** every rack
  rebuild scanned all installed LV2 plugins on the UI thread to tell whether
  the unit is an instrument. It now uses the background plugin scan, and the
  setBfree session loads in under 1 ms of UI time (was 2.6 to 4.0 s). Agent
  edits of such a rack no longer freeze the window either.
- **Symbols no longer show as boxes:** the REC dot, arrows, check marks and other UI symbols are drawn with Noto Sans Symbols 2, Math and Symbols behind Noto Sans; Noto is also found in the Debian/Ubuntu and Fedora font folders.
- **"System default" gives the sound back to the system's output (#31):** after you had picked a sound card at Master out, choosing System default left kabelsalat playing to that card. It now moves back, and a move PipeWire did not carry out is asked for again.
- **A strip keeps its own peak hold when another app stops playing (#31):** the remaining strips moved up and took over the held peak and clip light of the app that had stopped.
- **Text fields no longer go dead (#30):** the assistant's input, the search
  fields and every other text field stopped taking characters while the rest
  of the app still reacted. A focused field typed through the desktop's
  input method (fcitx5, ibus), so nothing arrived when that program stopped
  answering. Text fields now read the keyboard directly; umlauts, dead keys
  and Compose work as your keyboard layout defines them. If you write with an
  input method (Chinese, Japanese, Korean), start kabelsalat with
  `KABELSALAT_IME=1`.
- **Pinning knobs from a CLAP synth's parameter window (e.g. Starbreath):**
  long parameter names no longer push the `+ knob` button past the scroll
  area's clip edge where it could not be clicked, and clicking `+ knob` when
  all 4 auto-selected main-control slots are full now replaces the last
  auto-selected main knob instead of silently failing (`editor_status` is also
  shown inside the parameter window).
- **Surge XT no longer crashes the app from the audio thread (#29):** a knob
  or editor slider that reached a Surge voice with another plugin's parameter
  crashed Surge inside its audio processing, and so did saving a session or
  an undo step in the moment a sound was still loading. Both are now kept
  away from the plugin.
- **Solo kept a source's effects:** soloing a source silenced every effect
  in its rig, so a soloed synth or input lost its own delay, reverb or amp
  and only its dry signal played. Effects now pass what their sources send.
- **Imported Vital patches show up without a restart (#136):** `kabelsalat vital-import` added patches on disk, but a running kabelsalat kept its old sound list until it was restarted. The sound library now notices that Vitalium's patch folders changed and reads them again on the next search or load. The same holds for patches copied in by hand, for Odin 2 soundbanks, and for `kabelsalat mcp`.

## [0.1.0] — 2026-11-16

The first tagged release: a working live rack for Linux.

### Added
- **Rack** with animated patch cables; **Tab** flips between the front (play)
  and the back (patch). Every unit shares one face layout.
- **Multiple rigs** that all sound at once, each with its own MIDI source and
  device; **+ Rig** adds a playable rig, also with any installed CLAP instrument.
- **MIDI matrix** with per-cell rules (channel, transpose, key/velocity range)
  and keyboard **splits**; held notes are always released when routing changes.
- **Mixer** per rig (volume, pan, mute, solo) with post-fader rig meters.
- **Software keyboard** and **assistant** as bottom panels; the top bar
  separates the main view (Rack, Sessions, Plugins, Matrix, Mixer) from them.
- **Plugin hosting:** CLAP (Surge XT, Dexed, YANES, …) and LV2 (e.g. Vitalium)
  with native in-app editors, sound browsers and presets; units show the
  loaded instrument and patch.
- **Per-knob MIDI learn:** right-click a knob, move a controller knob, bound.
- **Recording** of the master output to WAV, with the last take playable from
  the top bar; transport, metronome, count-in and tap tempo.
- **Built-in assistant** (DeepSeek, bring your own key) and an **MCP server**
  (`kabelsalat mcp`, `--attach` to drive the running app); agent changes are
  highlighted in the rack and undoable one job at a time (Ctrl+Z).
- Sessions and starter templates; system-audio capture and WAV backing tracks.
- Arch/Omarchy packaging (`packaging/PKGBUILD`).

[Unreleased]: https://github.com/ssalentin/kabelsalat/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/ssalentin/kabelsalat/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ssalentin/kabelsalat/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ssalentin/kabelsalat/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ssalentin/kabelsalat/releases/tag/v0.1.0
