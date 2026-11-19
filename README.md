<p align="center">
  <img src="assets/logo.svg" width="140" alt="kabelsalat logo — two crossed patch cables with a signal pulse at the crossing"/>
</p>

<h1 align="center">kabelsalat</h1>

<p align="center"><strong>A live rack for Linux: plug in instruments, wire them up, play.</strong></p>

<p align="center">
  <a href="https://github.com/ssalentin/kabelsalat/releases"><img alt="Release" src="https://img.shields.io/github/v/release/ssalentin/kabelsalat?sort=semver"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <a href="CONTRIBUTING.md"><img alt="PRs welcome" src="https://img.shields.io/badge/PRs-welcome-brightgreen.svg"></a>
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-lightgrey.svg">
</p>

<p align="center">
  <img src="docs/shots/front.png" width="100%" alt="The kabelsalat rack: a keyboard source, a Surge XT synth with a delay, and the master output with its level meter"/>
</p>

kabelsalat turns your computer into a live setup like the ones built with
[AUM](https://kymatica.com/apps/aum) on the iPad: synths and effects sit in a
rack, connected by patch cables that you can see and grab, and everything you
play goes straight to the speakers. Several setups ("rigs") can sound at once,
each played from its own keyboard or controller.

## What you can do

### Play right away
Open the app and play with your laptop keyboard, the on-screen keyboard, or any
USB MIDI keyboard — devices are found while the app runs, just plug them in.
Starter setups (**F5–F8**) give you a lead with reverb, drums and bass, or an
ambient pad with one key.

The on-screen keyboard carries the controls a real keyboard has: a pitch-bend
wheel that springs back to centre, a modulation wheel, a sustain pedal and a
velocity slider. They reach every rig the keys feed, and hardware pitch bend,
modulation and sustain reach your instruments the same way.

<p align="center"><img src="docs/shots/keyboard.png" width="85%" alt="The rack with the full-width software keyboard opened at the bottom"/></p>

### Patch with real cables
Press **Tab** to turn the rack around and patch on the back. Drag from jack to
jack to connect, drag a plug to move it, pull it off to unplug. The cables
sag and swing, and light up when signal flows through them.

<p align="center"><img src="docs/shots/back.png" width="85%" alt="The back of the rack: MIDI and audio cables between Keys, Surge, Delay and Master"/></p>

### Run several rigs at once
**+ Rig** adds an independent setup with its own synth and output — for example
bass on one keyboard and pads on another. All rigs play together; switch the one
you are looking at with **1–9**. Any installed CLAP or LV2 instrument works
(Surge XT, Dexed, Vitalium, …), and each unit shows which instrument and sound
it has loaded.

### Browse and install plugins by type
Press **F9** (or **B**) to open **Plugins**, organized by what each unit does
rather than by plugin format: **Instruments**, **Effects**, **MIDI FX**,
**Utilities** and the **Install** catalog. Narrow **Effects** by subcategory
(**Delay & Reverb**, **EQ & Filter**, **Dynamics**, **Drive & Amp**,
**Modulation**, **Pitch & Spatial**, **Multi-FX**) or filter by format
(**Built-in**, **CLAP**, **LV2**). Add an instrument as a **New rig** alongside
your existing rigs or layer it into the active rig, insert any effect onto the
active rig or straight onto the **Master FX** bus, and filter the mixer's
**+ insert** picker by the same effect categories. The assistant and MCP agents
search the same taxonomy with `list_plugins`.

<p align="center"><img src="docs/shots/plugins.png" width="85%" alt="The Plugins view organized by type (Instruments, Effects, MIDI FX, Utilities) with effect subcategory and format filters"/></p>

### Put MIDI effects in front of an instrument
A **MIDI strip** sits between your keys and the instrument and changes the notes
on the way: the built-in **MIDI transpose**, or any installed LV2 MIDI plugin
(note transposers, filters, arpeggiators — pick one in **Plugins** and press
*Add MIDI strip*). The strip shows the plugin's own main controls, they can be
learned from a hardware knob like any other, and a note released while you turn
a knob never hangs. The assistant can add strips too (`list_midi_fx`, `add_node`
with kind `midi_fx`).

<p align="center"><img src="docs/shots/midi-strip.png" width="85%" alt="The rack with a Transpose MIDI strip between Keys and the two Surge synths"/></p>

### Play your guitar
Plug a guitar (or a microphone) into your audio interface and press **⇧F5**: the
Guitar rig loads **Audio In → Scream → Delay → Reverb → Amp → Cabinet →
Master**. The Audio In unit picks any input by name, has a gain knob, a live
level meter, a **MON** switch for monitoring through the rig, and a built-in
**tuner** (note and cents) on its face. The stomps, amp and cabinet are the
Guitarix LV2 plugins from the Plugins browser (install the *Guitarix* card).
Monitoring goes through the same low-latency path as everything else; a missing
input says so instead of falling back to the laptop microphone.

<p align="center"><img src="docs/shots/guitar-rig.png" width="85%" alt="The Guitar rig: Audio In with level meter and tuner, Scream, Delay, Reverb, Amp and Cabinet in one stack beside Master out"/></p>

### Mix
Every source gets its own channel strip: a fader in dB with a 0 dB detent
(double-click for exact unity), a held peak and a clip light, plus pan, mute
and solo. Each strip lists the effects on its cable chain as insert slots —
add, bypass, reorder or remove them — and the master has inserts that run on
the whole mix before the limiter. Every source and insert slot has a quick
parameter knob beside it (defaulting to the plugin's first main control or an
effect's mix; right-click to reassign, double-click to reset). Rename a strip,
give it a colour, drag it to reorder or collapse it; names, colours and quick
knob assignments are saved with the session.

**Master to a loudness.** Beside Master out, the Loudness column takes a
target: −14 LUFS (Spotify, YouTube, Tidal), −16 (Apple Music), −8 (club), or
any value in 1 LU steps. Play your loudest passage first; picking a target
then trims the level in front of the master limiter by what is missing, and
the true peak is held under −1 dBTP. The column shows the loudness before,
the loudness now, the true peak and the trim. **Match** measures what you
played since and corrects the trim; if the limiter has no more to give, the
readout says so and asks for more master VOLUME. The target is saved with
the session. The assistant and MCP agents do the same with
`set_master_target` and read the numbers with `get_master_target`.

Parallel paths stay in phase. When a signal splits and meets again, and one
path runs through a unit that looks ahead (the Limiter waits 5 ms to catch
peaks), the other paths wait for it, so a dry sound and its limited copy add
up cleanly instead of sounding hollow. A bypassed Limiter keeps its delay, so
switching it in and out never shifts anything in time.

<p align="center"><img src="docs/shots/mixer.png" width="85%" alt="The mixer: a channel strip per source with dB faders, pan, mute and solo, the rig bus, and the master with its inserts"/></p>

### Share an effect with sends
**+ Aux bus** under the mixer's master strip adds a bus: a channel that nothing is cabled into.
Every rig bus and every source strip gets a **SENDS** row for it. Turn the
row's knob to send some of that signal to the bus (double-click: off or 0 dB)
and click **PRE** to tap it before the strip's fader, so the send keeps its
level when the fader moves. Put a reverb or a delay on the bus as an insert
and several rigs share it. The bus has its own fader, pan, mute and solo and
returns to the master past the rig faders; soloing it leaves only the returns.
Right-click a send knob to bind it to a hardware controller, like the faders.
Sends are saved with the session.

<p align="center"><img src="docs/shots/sends.png" width="85%" alt="The mixer with an aux bus called Verb carrying a reverb, and SENDS rows under the rig buses and source strips that feed it"/></p>

### Mix the other apps and pick your sound card
The backing track in your browser sits in the same mixer. Under **System
audio** every other app that is playing gets its own strip, named after the
app and what it plays, with a live meter, a volume fader and a mute button, so
you can turn YouTube down under your rig without leaving kabelsalat. At
**Master out** a menu lists your sound cards: pick your audio interface instead
of the laptop speakers and the sound moves there at once, without a dropout.
The choice is remembered for the next start, and **System default** hands the
sound back to the system's output. The assistant and MCP agents do the same
(`list_app_streams`, `set_app_stream`, `list_audio_outputs`,
`set_audio_output`). This needs PipeWire with its Pulse layer (`pactl`,
`parec`).

<p align="center"><img src="docs/shots/system-audio.png" width="85%" alt="The mixer with a System audio section: a Firefox strip turned down and a muted Spotify strip, each with a meter, beside Master out with the MOTU M2 chosen as the output"/></p>

### Navigate rigs, sessions and plugins
One top bar holds the session, the rigs and the views. The **session pill**
on the left (`● Untitled ▾`, teal once saved) opens a small popover to save,
load or delete sessions and start from a template (**Ctrl+O**). While the
left rail is closed, the bar shows the rigs as compact pills: the active one
as `1 · Lead` with a live level bar, the others as numbers with an activity
dot, and **+** adds a rig.

Press **Q** (or the rail button left of the session pill) to open the
**left rail**. Its **Rigs** tab lists every rig with its MIDI input, level and
a **⋯** menu (rename, save as or replace from a rig preset, MIDI input, delete), its channels in their mixer
colours and their plugin chains (**CLAP**, **LV2** and **built-in**); filter
with **ALL**, **CHANNELS** and **PLUGINS**. Clicking an entry switches to that
rig and selects the unit in the rack or scrolls and flashes its strip in the
mixer. The **Library** tab keeps sessions, rig presets and starter templates
in one place.

### Keep an eye on the engine
Beside your rigs, **DSP** shows how much of each audio buffer the engine needs
to render your rack. It turns amber from 70 % and red above 90 %, and counts
the dropouts (xruns) the sound card reported. Click it for **Engine info**:
sample rate and buffer, inputs and outputs, late blocks and xruns, the
latencies kabelsalat measures, and the render time of every instrument and
effect, most expensive first, so you see which unit to swap when the load
climbs. Adjust the audio **Buffer size** (64–2048 frames, default 256) live
with **− / +** in Engine info or from **Settings → Audio** — smaller lowers latency,
larger prevents xruns — and the choice is remembered across restarts. The
assistant and MCP agents read and adjust the same engine settings
(`get_engine_info`, `set_buffer_size`).

### Route MIDI anywhere
The MIDI matrix decides which keyboard plays which rig. Each connection can be
shaped: only some keys (keyboard splits), another MIDI channel, transposed, or
only loud notes.

<p align="center"><img src="docs/shots/matrix.png" width="85%" alt="The MIDI matrix: keyboard and hardware sources routed to two rigs"/></p>

### Be the clock
Switch on MIDI Clock Send in the matrix view and kabelsalat becomes the master
clock: 24 clocks per beat plus Start and Stop go to the MIDI outputs you wire
(drum machines, synths) and to a virtual port other apps can connect to. A sync
offset and three latency modes (none, output, input) line the clock up with what
you hear.

### Control knobs and the mixer from your controller
Right-click any knob, turn a knob on your MIDI controller — bound. The knob
shows its CC number and follows your hardware from then on.

The mixer learns the same way. Right-click a fader, a pan field, or a mute or
solo button on a source strip, a rig bus or the master, then move a fader or
press a button on your controller. A learned fader shows its CC beside the dB
readout, and every learned control gets a blue outline. Mute and solo follow
the button: on while it is latched (CC value 64 or more), off when it is
released. Right-click a learned control to limit a fader's range, learn it
again or clear it. The bindings are saved with the session and stay with their
rig when you add or delete rigs.

<p align="center"><img src="docs/shots/mixer-midi-learn.png" width="85%" alt="The mixer with learned controls: rig bus faders showing CC 7 and CC 8 beside their dB readouts, and blue outlines on the learned faders, mute buttons and a pan field"/></p>

### Record your set
**REC** records exactly what you hear into a WAV file in `~/Music/kabelsalat/`.
After stopping you can play the take back or open its folder; **SYNC** starts
and stops the recording with the transport. For stems, press **R** on a rig's
bus strip in the mixer: every armed rig records its own post-fader WAV, named
after the rig, into the same take folder as the master mix (**ARM** under
Master out). Pressed while the transport rolls, REC starts and stops on the
next bar; stopping the transport keeps recording so reverb tails are not cut.

### Play along with a backing track
Ask the assistant (or an MCP agent) to play a WAV file and it mixes into the
master like AUM's FilePlayer: **loop** restarts it seamlessly, **sync** plays it
at the transport tempo from its original BPM (a "120 bpm" in the file name is
picked up; speed changes, pitch follows, no time-stretch), **beats** sets the loop
length in beats so an aligned loop never drifts off the metronome, and **offset**
shifts the start. With sync on and the transport rolling, it starts on the next
bar line.

### Ask the assistant — or your own AI agent
The built-in assistant (bring your own [OpenRouter](https://openrouter.ai/keys)
key, entered under **Settings → Agent → API key**) can set things up for you:
"add a reverb to this rig", "split the keys at C4", "load a warm pad". One key
reaches every model: **Settings → Agent → Models** picks the chat model, the
decisions model and the listening model, or takes any OpenRouter model id. While
it works, the assistant says what it is doing ("Planning the next step · after
Load sound"), and when it listens to your sound a bar shows how long it still
records, in the dock, on the top bar's button and on the rack's hint line.
Don't want it? Switch **Assistant** off on the same page and it leaves the top
bar and the dock. Everything
an agent changes lights up in the rack, and **Ctrl+Z** undoes the whole request
in one step. External AI agents (Claude, …) can drive the running app through
the built-in MCP server; the top bar shows when one is connected and pauses it
with one click.

<p align="center"><img src="docs/shots/assistant.png" width="85%" alt="The assistant panel at the bottom with quick starters, next to the rack"/></p>

Press **?** in the app for every shortcut.

## Install

**AppImage (recommended)** — download the latest release for your machine:

- [kabelsalat-x86_64.AppImage](https://github.com/ssalentin/kabelsalat/releases/latest/download/kabelsalat-x86_64.AppImage) (most PCs and laptops)
- [kabelsalat-aarch64.AppImage](https://github.com/ssalentin/kabelsalat/releases/latest/download/kabelsalat-aarch64.AppImage) (ARM: Raspberry Pi 4/5, Asahi, Snapdragon)

```sh
chmod +x kabelsalat-*.AppImage
./kabelsalat-*.AppImage
```

Older versions and the other packages are on the
[releases page](https://github.com/ssalentin/kabelsalat/releases).

Text fields read the keyboard directly, not through an input method. To
write Chinese, Japanese or Korean, start it with `KABELSALAT_IME=1`.

It runs unsandboxed, so it finds the CLAP/LV2 plugins installed on your system
and gets realtime audio priority like a native install. Install the plugins
from your distribution (`surge-xt` for the default synth).

**Arch / Omarchy** (builds the package with a launcher entry and icon):

```sh
git clone https://github.com/ssalentin/kabelsalat.git
cd kabelsalat/packaging
makepkg -si
```

Recommended: `surge-xt-clap` (the default synth). Optional: `dexed-clap`,
`lv2-plugins`.

**Flatpak / tarball** — also attached to every release. The Flatpak runs in a
sandbox: no realtime audio priority yet and `kabelsalat mcp --attach` from the
host cannot reach it; see [docs/PACKAGING.md](docs/PACKAGING.md).

**From source** — needs Rust (rustup), `alsa-lib`, `lilv`, and PipeWire or ALSA:

```sh
git clone https://github.com/ssalentin/kabelsalat.git
cd kabelsalat
cargo run --release
```

## Contributing

Pull requests are welcome! Start with [CONTRIBUTING.md](CONTRIBUTING.md) and
issues labelled
[`good first issue`](https://github.com/ssalentin/kabelsalat/labels/good%20first%20issue)
or [`help wanted`](https://github.com/ssalentin/kabelsalat/labels/help%20wanted).
Changes are listed in [CHANGELOG.md](CHANGELOG.md); planned work is in the
[issues and milestones](https://github.com/ssalentin/kabelsalat/milestones), and
how it is built in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## Technical details

- **Engine:** Rust, a lock-free and allocation-free render path on a realtime
  thread (rtkit), 256-frame buffer (≈ 5.3 ms at 48 kHz). Hardware MIDI reaches
  the engine from its own thread, so sound never waits for a UI frame. A
  zero-latency master limiter sits before the master saturation.
- **Plugins:** CLAP via [clack](https://github.com/prokopyl/clack) (scanned out of
  process, so a crashing plugin cannot take the app down), LV2 via lilv on a
  loader thread. Plugin editors are native egui panels — no floating plugin
  windows.
- **Graph and rigs:** every rig is a graph of nodes and cables; rigs are merged
  into one engine graph and mixed per rig. Routing changes always release held
  notes first (checked exhaustively by the `verify-held-notes` test).
- **UI:** egui/eframe, physics-simulated cables, follows the
  [Omarchy](https://omarchy.org) desktop theme, native fractional HiDPI.
- **Agents:** `kabelsalat mcp` (headless) or `kabelsalat mcp --attach` (drives the
  running app) exposes the same control plane the UI uses — every UI action has
  an agent tool. `kabelsalat send '<json>'` is a one-shot CLI client.
- **Testing:** a unit suite (`cargo test --bin kabelsalat`) plus CLI self-tests
  that run real plugins (`record-test`, `rig-mix-test`, `cc-map-test`,
  `transport-test`, …). `analysis-test` checks the engine by what it sounds
  like: a note's pitch, brightness and aliasing from its spectrum, the
  master's crackle, routing proven with noise, and delay compensation by
  cross-correlation. Rules learned from real bugs are kept in
  [AGENTS.md](AGENTS.md).
- **Platforms:** developed and tested on Asahi Linux (Arch, ARM64); x86_64 Linux builds the same way.

## The name

*Kabelsalat* is German for "cable salad" — the tangled mess behind every desk.
This app turns it into something you want to look at.

## Inspiration & prior art

- [AUM](https://kymatica.com/apps/aum) (Kymatica) — the live workflow this brings to Linux
- Reason (Reason Studios) — the rack flip: front to play, back to patch
- GarageBand (Apple) — instant sound, no manual needed
- [Surge XT](https://surge-synthesizer.github.io/) — reference instrument
- [clack](https://github.com/prokopyl/clack), [Maolan](https://github.com/maolan/maolan) — Rust plugin hosting
- [Element](https://github.com/kushview/element), [Carla](https://github.com/falkTX/Carla) — the Linux hosts that showed the gap

## License

[MIT](LICENSE) © Sebastian Salentin and contributors.
