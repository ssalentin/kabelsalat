### Fixed
- **Agent eval `organ_no_pad` false failure (#346):** `EvalRig::load` now waits for async LV2 loads to settle and clears load-time organ face CC sync (`CC 91 = 25`) from the LV2 MIDI baseline before each eval turn, so read-only `organ_no_pad` runs pass while unintended agent program changes, notes, or parameter CCs still fail.
