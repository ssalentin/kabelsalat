### Added
- **Program change picks the song (#208):** in the Setlist view, turn a MIDI
  device on under "Program change picks the song" and its preset buttons pick
  songs: program change N jumps to the first part of song N+1 in the next
  audio block. That device's program changes no longer reach its instruments;
  other devices' program changes now reach theirs (they were dropped before).
  The choice is saved by the device's name, so it survives another plug order.
