### Fixed
- **A setlist foot switch no longer plays the instruments (#261):** the CC
  learned for Next or Previous part steps the setlist and reaches no
  instrument, so a foot switch on CC 64 no longer holds the sustain pedal.
  The new part's zones take effect in the audio block the pedal press
  arrives in, without waiting for the screen.
