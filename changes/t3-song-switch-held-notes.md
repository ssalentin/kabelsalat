### Fixed
- **A part that changes a rig's level no longer clicks a held chord (#274):**
  a rig fader or an instrument's LEVEL that a part recalls (or the mixer, a
  MIDI controller or an agent moves) now takes 5 ms to arrive instead of
  jumping in one sample. Muting a rig, soloing another or pulling a fader to
  zero fades it out the same way, and unmuting fades it back in.

### Added
- **`kabelsalat setlist-switch-test` (#274):** renders part and song switches
  under a held chord with real Surge XT voices and measures them: no gap or
  click in the held chord, the delay and reverb tails ring on, and the first
  note of the next song starts in the block that switched (at most one 5.3 ms
  buffer plus Surge's own onset of up to 1 ms, against a 50 ms budget), by the
  app and by the foot switch.
