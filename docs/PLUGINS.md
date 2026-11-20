# Plugin compatibility and test status

Single source of truth for which plugins have been tested in kabelsalat, what
works, and what is still untested. Every entry in `src/catalog.rs` (`CATALOG`)
must have a row here (enforced by
`catalog::tests::every_catalog_entry_is_tracked_in_docs_plugins_md`).

Update this file whenever you add a catalog plugin, run a plugin test on real
hardware or another CPU, or fix a plugin-specific bug.

## Legend

| Symbol | Meaning |
|---|---|
| **Works** | Verified with the real plugin binary (automated self-test, unit test, or live app). |
| **Headless** | Verified headlessly (`catalog-install-test`, `analysis-test`, or unit test) — interactive app/hardware play still untested. |
| **Partial** | Loads or works in part; see Notes for what is missing or blocked. |
| **Untested** | Not yet run against the real plugin binary (install plan / SHA-256 may still be unit-tested). |
| **N/A** | Does not apply (e.g. sound library for an effect, or native GUI window for LV2). |

Most plugin runs to date were on **aarch64 (Arch Linux / Asahi)**. Unless noted
otherwise, **x86_64** prebuilt release and Arch package runs are still untested
(CI runs `setbfree` on Ubuntu x86_64 and aarch64).

---

## Catalog instruments (18)

| Card (`id`) | Format | Install | Audio / Engine | Sound library | State / Session | Plugin GUI | Notes & untested gaps |
|---|---|---|---|---|---|---|---|
| **setBfree** (`setbfree`) | LV2 | **Works** (`aarch64` Arch; CI Ubuntu `x86_64`+`aarch64`) | **Works** (`organ-test`, `lv2-instrument-engine-test`, CI `setbfree_plays_a_note`) | **Works** (MIDNAM programs) | **Works** (control ports) | **N/A** (custom organ face) | Custom drawbar/Leslie/percussion face. Arch `x86_64` package untested. |
| **Surge XT** (`surge-xt`) | CLAP | **Works** (`aarch64` Arch) | **Works** (reference plugin; `host-test`, `many-voices-test`, `state-race-test`, `analysis-test`, `agent-eval`) | **Works** (~3000 `.fxp` patches) | **Works** (`session-test`, `state-race-test`) | **Works** (`editor-test` + live XWayland) | Reference instrument. Macro names follow loaded patch. `x86_64` untested. |
| **Dexed** (`dexed`) | CLAP | **Works** (`aarch64` Arch) | **Works** (`dexed_load_patch_changes_state_and_renders`) | **Works** (DX7 `.syx` cartridges) | **Works** (CLAP state + `.syx` write) | **Works** (`editor-test`) | In `CLAP_NEVER_FREE` (#52): parked and reused because its destructor segfaults while other plugins render. `x86_64` untested. |
| **Vitalium** (`vitalium`) | LV2 | **Works** (`aarch64` source) | **Works** (`lv2-instrument-engine-test`, `midifx-test`, `vital-test` w/ aftertouch) | **Works** (`.vital` + `.vitalbank` drop + LV2 presets) | **Works** (`urn:juce:stateBinary` JSON) | **N/A** (LV2 GUI not hosted) | In LV2 `NEVER_FREE` (rule 56). `vital::check_patch` guards against modal-alert hangs (rule 118). Open test issue #174; `x86_64` source build untested. |
| **YANES** (`yanes`) | CLAP | **Works** (`aarch64` source) | **Works** (`sound-library-test`, `agent-eval` `yanes_bass`) | **Works** (146 built-in presets via `PresetParameter`) | **Partial** (standard CLAP path; not stress-tested) | **Works** (`editor-test`) | `x86_64` source build untested. |
| **Odin 2** (`odin2`) | CLAP | **Works** (`aarch64` Arch + GitHub `Factory Presets` data) | **Works** (`catalog-install-test`, `sound-library-test`) | **Works** (260 `.odin` `ValueTree` patches) | **Works** (`.odin` → CLAP state) | **Works** (`editor-test`) | Returns `false` from `gui.show` while still showing its editor (handled by host). `x86_64` untested. |
| **OB-Xf** (`ob-xf`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 0.331) | **Untested** | **Untested** | **Untested** | Interactive install/play in open bench test #186. State, GUI window, `x86_64` untested. |
| **Ultramaster KR-106** (`kr106`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 2.963) | **Untested** | **Untested** | **Untested** | Hot raw output (2.963 peak) — master limiter listening check in open bench test #186. State, GUI window, `x86_64` untested. |
| **Six Sines** (`six-sines`) | CLAP | **Headless** (`aarch64` source); **Untested** (`x86_64` release `v1.2.1`) | **Headless** (`aarch64` probe chord peak 0.385) | **Untested** | **Untested** | **Untested** | `x86_64` prebuilt release is SHA-256/plan-tested only. Interactive play, state, GUI window untested. |
| **JC303** (`jc303`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 0.279) | **Untested** | **Untested** | **Untested** | Interactive play, state, GUI window, `x86_64` untested. |
| **Cardinal** (`cardinal`) | CLAP | **Partial** (`aarch64` Arch `cardinal-clap` loads; needs `cardinal-data`) | **Untested** (silent without `cardinal-data`) | **N/A** (modular patch) | **Untested** | **Untested** | Full 32-byte `ClapId` (`studio.kx.distrho.cardinal#synth`) loads cleanly. Playback with `cardinal-data` installed is open bench test #186; GUI window needed for patching. |
| **OneTrick KEYS** (`ot-keys`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 0.338) | **Untested** | **Untested** | **Untested** | Interactive install/play in open bench test #186. State, GUI window, `x86_64` untested. |
| **OneTrick CHONK** (`ot-chonk`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 0.281) | **Untested** | **Untested** | **Untested** | Interactive play, state, GUI window, `x86_64` untested. |
| **OneTrick CRYPTID** (`ot-cryptid`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (probe chord peak 0.812; 12 output ports) | **Untested** | **Untested** | **Untested** | Plays GM drum notes (36/38). Interactive install/play in open bench test #186. State, GUI window, `x86_64` untested. |
| **AVL Drumkits** (`avldrums`) | LV2 | **Headless** (`aarch64` source via `catalog-install-test`) | **Headless** (all 5 kits sound, peaks 0.516–0.685) | **N/A** (5 kit URIs) | **Untested** | **N/A** (LV2 GUI not hosted) | Async SF2 sample load handled with wall-clock wait (rule 178). Interactive play, state, `x86_64` untested. |
| **Starbreath** (`starbreath`) | CLAP | **Untested** (`x86_64` release `v0.9.0`; no `aarch64` release) | **Partial** (hand-installed `aarch64` beta passes `catalog-install-test` w/ CC2 breath) | **Untested** | **Untested** | **Untested** | Proprietary freeware for wind controllers (breath on CC2/CC11/aftertouch). Public `x86_64` release download & playback is open bench test #186. |
| **Floe** (`floe`) | CLAP | **Untested** (`x86_64` release `v2.0.5` only; plan/SHA-256 unit-tested) | **Untested** | **Untested** | **Untested** | **Untested** | Requires external sample libraries from floe.audio. No `aarch64` build; never run on `x86_64` yet. |
| **Aeolus** (`aeolus`) | CLAP | **Works** (`aarch64` source); **Untested** (`x86_64` release `v0.3.0`) | **Works** (`aarch64` `catalog-install-test` peak 0.281, `sound-library-test`) | **Works** (12 built-in registrations) | **Works** (initial principal chorus JSON + stop toggle readback) | **Works** (`editor-test`) | Custom stop-jamb panel on rack face verified in real app (#66). `x86_64` prebuilt release untested at runtime. |

---

## Catalog effects (3)

| Card (`id`) | Format | Install | Audio / Engine | State / Session | Plugin GUI | Notes & untested gaps |
|---|---|---|---|---|---|---|
| **Guitarix** (`guitarix`) | LV2 | **Works** (`aarch64` Arch) | **Works** (`guitar-rig-test`, `guitarix_amps_process_from_the_first_block`) | **Works** (control ports) | **N/A** (LV2 GUI not hosted) | Powers the `⇧F5` Guitar rig (`Audio In → Scream → Delay → Reverb → Amp → Cabinet → Master`). Browser add-to-rig flow in open bench test #177; `x86_64` untested. |
| **Dragonfly Reverb** (`dragonfly-reverb`) | CLAP | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (Hall reverb: `catalog-install-test` peak 0.564, `analysis-test` `clap_effect`) | **Untested** | **Untested** | Hall tested; Room, Plate and Early Reflections bundles not individually tested. Interactive rig/aux/master use, state, GUI window, `x86_64` untested. |
| **Airwindows Consolidated** (`airwindows`) | CLAP | **Untested** (`x86_64` release + source; plan/SHA-256 unit-tested) | **Untested** | **Untested** | **Untested** | Added in #197; neither the `x86_64` prebuilt release nor the `aarch64` source build has been installed or run yet. |

---

## Catalog MIDI FX (1)

| Card (`id`) | Format | Install | MIDI / Engine | State / Session | Plugin GUI | Notes & untested gaps |
|---|---|---|---|---|---|---|
| **x42 MIDI filters** (`x42-midifilter`) | LV2 | **Headless** (`aarch64` Arch via `catalog-install-test`) | **Headless** (all 6 catalog URIs pass notes out) | **Untested** | **N/A** (LV2 GUI not hosted) | Bundle ships 36 MIDI tools; the 6 catalog URIs (`midichord`, `miditranspose`, `monolegato`, `velocityscale`, `mapcc`, `midistrum`) passed `catalog-install-test`. Remaining 30 tools, live controller use, and `x86_64` untested. |

---

## Other tested plugins (not in catalog)

| Plugin | Format | What works | What is untested |
|---|---|---|---|
| **Surge XT Effects** (`org.surge-synth-team.surge-xt-fx`) | CLAP | **Headless** audio processing with stereo input + sidechain (`real_clap_effects_stereo_mono_and_sidechain_pass_test_tone_when_installed`, peak 0.471 in #197). | Interactive rig/aux/master use, state round-trip, GUI window. |
| **LSP Plugins** (`in.lsp-plug.comp_delay_mono`, `in.lsp-plug.sc_compressor_stereo`, LV2 effects) | CLAP & LV2 | **Headless** CLAP mono-in/mono-out and stereo+sidechain effects (peak 0.500 in #197); LV2 effects (`lv2-engine-test`, `lv2-test`). | Mixer `+ insert` picker flow (open bench test #177), CLAP GUI windows, `x86_64`. |
| **Carla MIDI Transpose** | LV2 | **Headless** MIDI strip routing into real Vitalium (`midifx-test` in #114), including knob changes while holding notes with no hung notes. | Interactive UI use, `x86_64`. |

---

## Untested or unsupported areas

- **`x86_64` runtime installs:** Prebuilt `x86_64` release routes (`six-sines`, `starbreath`, `floe`, `aeolus`, `airwindows`) and Arch `x86_64` packages are covered by install-plan and SHA-256 unit tests, but have not been run on an `x86_64` host (only Ubuntu `setbfree` runs in `x86_64` CI).
- **CLAP note-effect plugins:** Discovered and listed, but hosting is deferred (#115).
- **LV2 native X11 GUIs:** Not hosted yet (LV2 plugins use kabelsalat's native egui controls and custom faces such as setBfree's organ face).
- **VST3 plugins:** Not hosted.
- **Open plugin bench tests:**
  - `#177` — Plugins browser add-to-rig / Master FX / mixer `+ insert` with Surge XT, Guitarix, and LSP.
  - `#186` — Cardinal (`cardinal-data`), Starbreath (`x86_64` release), and live app play of OB-Xf, KR-106, OneTrick KEYS, and OneTrick CRYPTID.

---

## How to verify plugins locally

Build once (`cargo build`), then run the relevant self-tests against installed
or fetched plugins:

```sh
# Fetch (without root) and play catalog cards:
./target/debug/kabelsalat catalog-install-test [id...]

# Sound libraries (patches, cartridges, registrations, Vital patches):
./target/debug/kabelsalat sound-library-test [id=query...]

# CLAP native X11 editor windows (open, attach, close, reopen):
./target/debug/kabelsalat editor-test [plugin_id...]

# Engine & host self-tests with real plugins:
./target/debug/kabelsalat host-test
./target/debug/kabelsalat organ-test
./target/debug/kabelsalat guitar-rig-test
./target/debug/kabelsalat midifx-test
./target/debug/kabelsalat lv2-instrument-engine-test
./target/debug/kabelsalat analysis-test
```

When a run confirms or changes a plugin's status, update the tables above in
the same PR.
