# P0 — CLAP hosting spike (Surge XT via clack-host)

Workstream: `clap-host` · task-2 · 2026-11-14 · **Result: PASS**

`kabelsalat host-test` loads Surge XT 1.3.4 (ARM-native CLAP), plays an audible
arpeggio through the default PipeWire output, round-trips plugin state, and
**embeds** the Surge editor into a host-owned X11 window (XWayland). No detached
plugin windows — embedding is the acceptance criterion and it works.

## 1. What was verified (evidence)

Run: `cargo build --release && kabelsalat host-test`
(`CARGO_TARGET_DIR=$HOME/.cache/ksalat-host-target`, release profile; keep cargo
targets off the 7.7 G `/tmp` tmpfs).

| Check | Result |
|---|---|
| Discovery in `~/.clap` | `Surge XT.clap` bundle, CLAP entry version 1.2.0; descriptor `org.surge-synth-team.surge-xt` v1.3.4, features `instrument, synthesizer, stereo, free and open source` |
| Extensions | params ✓ state ✓ gui ✓ latency ✓ audio-ports ✓ (all queried during `initializing`) |
| Params | **775** parameters; names/modules/values/`value_to_text` read correctly (e.g. `Global & FX/Send FX 1 Return = 1.0 (0.00 dB)`) |
| Audio ports | in: `Sidechain` (stereo); out: `Output` (stereo, **main**), `Scene A`, `Scene B` (stereo each) |
| Latency | plugin reports **0 samples** (active). Querying before activation triggers a `HostMisbehaving` log from Surge — don't do it |
| Arpeggio | A-minor arp (A3 C4 E4 A4 C5 …), 132 BPM eighths, 4 cycles ≈ 7.3 s + tail; 256-frame blocks; **peak 0.35–0.38, ~1215 non-quiet blocks, 0 underrun frames** over ~9 s (≈1900 blocks, ≈900 cpal callbacks) |
| Param events | scheduled `ParamValueEvent` (Macro M1: 0 → 0.75 at t=1 s) applied sample-accurately; `get_value` confirms 0.75 afterwards |
| State round-trip | save A (50310 B) → modify via event → save B (51971 B, ≠ A) → load A **during processing** → value restored to 0.0 → save C. **OK** |
| GUI | `create(x11, is_floating=false)` + `set_parent` + `show` all succeed; initial size **1597×995**, `can_resize=true`; **child window confirmed inside the host window** (see §4) |

State byte-diff A vs C (1 byte longer) is fully explained, not a round-trip bug:
- header byte 4: `sub3f…` → `sub3g…` (Surge streaming-version marker bumps after a load cycle);
- `<instanceZoomFactor v="-1"/>` → `v="175"` — opening the GUI persists its zoom into `dawExtraState`.
All parameter values round-trip exactly. State format: 16-byte `sub3` header + Surge patch XML (`<patch revision="24">`, init patch "Init Saw").

## 2. Plugin install (no sudo)

```sh
pacman -Sp surge-xt-clap        # prints mirror URL (also lists deps)
curl -sSLO https://arm-stable-mirror.omarchy.org/aarch64/extra/surge-xt-clap-1.3.4-7-aarch64.pkg.tar.xz
tar -xf surge-xt-clap-1.3.4-7-aarch64.pkg.tar.xz
mkdir -p ~/.clap && cp "usr/lib/clap/Surge XT.clap" ~/.clap/
```

`ldd` clean — all shared libs (fmt, samplerate, freetype, …) already present;
`libmad`/`qtractor` listed by `pacman -Sp` are not needed at runtime.
The bundle also contains `Surge XT Effects.clap` (not installed; host-test picks
the descriptor named "Surge XT" with feature `instrument`).

## 3. Architecture of the spike (src/host.rs)

Matches PLAN.md §7 thread model:

- **Main thread** — `PluginInstance<TestHost>` (it is `!Send`): discovery,
  instantiation, params/state/GUI calls, `call_on_main_thread_callback()` pump.
- **Processing thread** — owns `StartedPluginAudioProcessor` (Send), renders
  256-frame blocks with sample-accurate note/param events from a precomputed
  schedule, pushes interleaved f32 into a lock-free `rtrb` ring (0.5 s headroom);
  ring-full sleep paces it to realtime. All buffers preallocated; per-block port
  configs live in a stack array (no heap alloc in steady state).
- **cpal callback** — drains the ring into the device buffer; no alloc/locks;
  counts callbacks/underruns. Default device: 48 kHz stereo F32 via ALSA→PipeWire.
- Shutdown order: drop stream → stop flag → `stop_processing()` → processor sent
  back over a channel → `deactivate()` on main thread (CLAP lifecycle rule).

Host handlers: `HostShared` (extensions + request flags + GUI requests),
`HostMainThread` (dirty/latency/rescan cells), `AudioProcessor = ()`.
Declared host extensions: log, params, state, latency, gui.

## 3.1 ClapVoice — reusable, RT-safe voice API (task-5)

`src/host.rs` exposes the hosting logic as a reusable API; `run_test` is now a
demo built entirely on it (no duplicated hosting logic). Verified: host-test
PASS through the API (peak 0.35, 0 underruns, 0 CLAP process errors, state
round-trip OK, embedded GUI OK).

```rust
use crate::host::{ClapVoice, ClapVoiceMain, VoiceController, VoiceCmd};

// Main thread: load Surge XT from ~/.clap, activate, start processing.
let (mut voice, mut main) = ClapVoice::new(sample_rate)?;   // e.g. 48_000.0
let ctl: VoiceController = voice.controller();              // Send + Clone

// Move `voice` to the audio thread / cpal callback. render is RT-safe:
// no allocation, no locks, no syscalls; any buffer length (chunked at 256).
voice.render(&mut data);            // data: &mut [f32], stereo-interleaved

// UI thread (keyboard handler): block-quantized note commands via lock-free
// ring. note_on/note_off lock a mutex on the CALLER side — never call them
// from the render thread; use the controller there.
ctl.note_on(60, 0.8);
ctl.note_off(60);
ctl.set_param(param_id, 0.5);       // ClapId, plain value

// Optional sample-accurate sequences (host-test arpeggio uses this):
voice.set_schedule(vec![(0, VoiceCmd::NoteOn(57, 0.85)), (2400, VoiceCmd::NoteOff(57))]);

// Main thread only (ClapVoiceMain is !Send — CLAP [main-thread] rules):
let blob: Vec<u8> = main.save_state()?;   // Surge patch XML in sub3 container
main.load_state(&blob)?;                  // applied on the audio thread: keep
                                          // rendering ~300 ms before reading values
main.latency();                           // samples, meaningful once active
main.instance_mut();                      // params inspection / embedded GUI
main.gui();                               // Option<PluginGui>

// Shutdown: audio thread stops, main thread deactivates.
let stopped = voice.stop();               // audio thread
main.deactivate(stopped);                 // main thread, consumes both sides
```

Threading contract:
- `ClapVoice`: `Send`, `!Sync` — create on main, move to exactly one audio thread.
- `VoiceController`: `Send + Sync + Clone` — any non-RT thread.
- `ClapVoiceMain`: `!Send` — stays on the creating thread (state/GUI/deactivate).
- Command ring: 1024 slots; events per block capped at 256 (overflow dropped,
  never allocates in `render`). Notes are block-quantized (≤ 5.3 ms at 256
  frames / 48 kHz); `set_schedule` events are sample-accurate.
- `render` mixes Surge's `IS_MAIN` stereo output; Scene A/B ports are rendered
  but discarded. Process failures output silence and increment
  `voice.process_errors()`.

## 4. GUI embedding (the de-risked path for P1)

Sequence that works under Hyprland/XWayland:

1. `ensure_display()`: tool shells may lack `DISPLAY`; if unset, find
   `/tmp/.X11-unix/X<n>` and set `DISPLAY=:n` (here `:0`). The final app should
   run with `WINIT_UNIX_BACKEND=x11` and inherit a proper DISPLAY.
2. `gui.is_api_supported(x11, floating=false)` → true; `gui.create(...)` → ok.
3. `gui.get_size()` → 1597×995 (Surge default at 175 % zoom); create the host
   parent window at that size with **x11rb 0.14** (`RustConnection`, pure Rust,
   no libX11 linking), `SUBSTRUCTURE_NOTIFY` selected.
4. `unsafe { gui.set_parent(Window::from_x11_handle(win)) }` → ok; `gui.show()` → ok.
5. **Surge creates its editor window directly as a child of the host window**
   (observed `CreateNotify`, window 8388617 inside host window 10485760;
   `query_tree` confirms the child). It does **not** use `XReparentWindow`, so
   hosts must not rely on `ReparentNotify` alone as embedding evidence.
6. Event loop: pump X events, honor `request_resize` (via `adjust_size` +
   `configure_window` + `set_size`), `request_show/hide`, `closed`, and call
   `call_on_main_thread_callback()` when requested. Surge issued 0 resize
   requests during the 9 s demo.
7. Teardown order matters: `gui.hide()` → `gui.destroy()` **before** destroying
   the X11 parent (destroying the parent first would tear down the plugin's
   child window underneath it).

No floating/detached fallback exists in the code, per requirement. Screenshot
of a live run: `/tmp/ksalat-gui.png` (grim, Wayland capture; transient).

## 5. clack API quirks found (0.2.0 family)

- **`clack-host` contains no extensions.** Params/state/gui/latency live in
  `clack-extensions`, which has **no default features**: you need
  `features = ["all-extensions", "clack-host"]` — `all-extensions` alone enables
  the modules but cfg's out every host-side impl (implicit optional-dep feature
  `clack-host`). Cost us one round-trip with lead; now in Cargo.toml.
- **`HostGuiImpl` goes on the Shared handler**, not MainThread — clack dispatches
  all GUI host callbacks through `host.shared()` (they may arrive off-main).
  GUI request state must be `Send + Sync` (we use `Mutex<Option<GuiSize>>` +
  atomics). Most other extension traits (state/latency/params-main) are on
  MainThread; `HostParamsImplShared::request_flush` on Shared.
- **Surge applies loaded state on the audio thread.** `state.load()` while
  processing is stopped returns success but `get_value` still shows old values;
  load during playback (or run a few blocks afterwards) and values settle
  within ~300 ms. Hosts should restore state while the graph runs.
- **Latency before activation** → Surge logs `HostMisbehaving` (needs sample
  rate). Query only when active.
- **Surge has 3 output ports** (main + Scene A/B). Hosts must enumerate
  audio-ports, render all ports, and mix the `IS_MAIN` one; assuming a single
  stereo output fails.
- Buffer wrapping is generic-heavy: `AudioPorts::with_{input,output}_buffers`
  wants `ExactSizeIterator`s of `AudioPortBuffer`. For a runtime port count, a
  stack array + `.into_iter().take(n)` works; `array::from_fn` cannot return
  values borrowing the closure's captures, and `Vec`-of-configs across loop
  iterations trips the borrow checker. Destructure `&mut [Vec<…>; N]` into
  disjoint locals first (macro fragment must be `tt`/`ident`, not `expr`, or
  constant-index disjointness is lost).
- `PluginEntry::load` is `unsafe` (executes plugin init); keep entries alive
  while instances exist; `PluginInstance` is `!Send`, processors are `Send`.
- cpal 0.18 / midir 0.11 / eframe 0.37 API drift vs older docs:
  `SampleRate = u32`, `Device::description().name()`, `build_output_stream`
  takes `StreamConfig` by value (see src/audio.rs, the canonical reference).

## 6. Latency & performance numbers

- Plugin-reported latency: **0 samples** (Surge, default patch, 48 kHz).
- Block size 256 frames ≈ 5.3 ms; ring headroom 0.5 s (spike only — P1 will
  shrink to 1–2 blocks); **0 underruns** across ~900 callbacks / 9 s.
- Effective output buffering: cpal default config (PipeWire ALSA bridge),
  device period measured by the audio-io workstream (docs/p0-audio.md).

## 7. Reproduce

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$HOME/.cache/ksalat-host-target
cd ~/Work/kabelsalat
cargo build --release
"$CARGO_TARGET_DIR/release/kabelsalat" host-test   # 9 s: arp + embedded Surge GUI
KSALAT_DUMP_STATE=1 ... host-test                  # also dumps /tmp/ksalat-state-{a,b,c}.bin
```

Requires: `~/.clap/Surge XT.clap` (§2), running XWayland (`:0`), PipeWire.

## 8. Follow-ups for P1

- Shrink ring to 1–2 blocks; move scheduling into the engine's RenderPlan.
- Handle `request_restart`/`request_process` properly (currently flags only).
- Session storage: keep Surge state blobs verbatim (they are deterministic
  apart from GUI side-effects like zoom — expect byte drift, compare values).
- Embed editors into reserved egui rects: needs the egui window's X11 handle
  (`WINIT_UNIX_BACKEND=x11`) + child-window positioning; the CLAP-side path
  proven here (create → set_parent → show → resize pump) stays identical.
- `note-ports`, `preset-discovery`, `state-context` extensions available in
  clack-extensions but not yet used.