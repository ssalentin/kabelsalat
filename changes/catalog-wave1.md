### Added
- **Eleven more plugins in the catalog (#173):** Plugins → Install now offers
  OB-Xf (Oberheim), Ultramaster KR-106 (Juno), Six Sines (FM), JC303 (TB-303),
  Cardinal (modular), OneTrick KEYS (modelled piano), OneTrick CHONK (electric
  bass), OneTrick CRYPTID (FM drums), AVL Drumkits (sampled kits), Starbreath
  (modelled winds for wind controllers; freeware, x86-64 only) and the x42 MIDI
  filters. Odin 2 now installs from the Arch package on both CPUs, with its
  factory patches.

### Fixed
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
