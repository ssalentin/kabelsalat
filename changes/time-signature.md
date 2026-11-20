### Added
- **Time signature (#87):** the clock is no longer fixed to 4/4. Pick the beats per bar (1 to 12) and the note that counts as one beat (2, 4, 8 or 16) under **Settings → Session**, or step through the common ones (4/4, 3/4, 6/8, 5/4, 7/8, 2/4) with the new chip on the metronome. The metronome clicks once per beat, eighths in 6/8, and plays the first beat of every bar higher. The count-in lasts whole bars of the signature, REC and a synced backing track start and stop on its bar lines, and hosted plugins are told the signature and where the bar began. It is saved with the session; older sessions keep whatever is set. Agents use `set_time_signature` and read it in `get_status`.

### Changed
- **Metronome accent (#87):** the first click of each bar is now higher (1.5 kHz, the others stay at 1 kHz). The clicks sit where they always did.
