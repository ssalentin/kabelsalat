### Added
- **File player: Loop, Sync, Number of Beats, Beat Offset (#84):** the backing-track
  player now matches AUM's FilePlayer. *Loop* restarts the file seamlessly (the seam
  is the file's own end→start, no gap or click). *Sync* plays the file at the
  transport tempo as varispeed from its original BPM (a "120 bpm" in the file name
  is picked up; no time-stretch). *Number of Beats* sets the loop length in beats, so
  an aligned loop stays locked to the metronome however long it runs. *Beat Offset*
  shifts the start point. With Sync on and the transport rolling, play starts at the
  next bar line (beat 0 after a count-in); a tempo change mid-play is followed with
  no phase jump. The assistant and MCP tools `play_file` and `set_player` take
  `loop`, `sync`, `beats`, `offset` and `bpm`; `get_status` reports the player.
### Changed
- **The file player reads files in the engine:** it used to stream a pre-resampled
  ring, so a sped-up or looped file could not stay on the beat. Files now decode once
  on a worker thread and the engine reads them at the exact song position.
