# Changelog

All notable changes to kabelsalat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). While the version is `0.x`, minor
releases may still change behaviour and session files.

## [Unreleased]

### Added
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

### Removed
- **Image input for the assistant:** dropping or pasting an image into the
  chat, the `look_at_rack` screenshot tool (in-app and MCP), the vision model
  role (`agent_perceive_model` in Settings and config.json) and
  `jev-test --image` are gone. The agent works from text and the rack's own
  data. Dropped `.vital` / `.vitalbank` files are still imported.

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

### Fixed
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

### Fixed
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

### Added
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

### Changed
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

### Fixed
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

[Unreleased]: https://github.com/ssalentin/kabelsalat/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ssalentin/kabelsalat/releases/tag/v0.1.0
