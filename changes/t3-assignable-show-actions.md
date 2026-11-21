### Added
- **Every show action on a pedal or button (#277):** Setlist → Show controls lists next and previous song, next and previous part, Panic, tap tempo, tuner on/off, master mute, backing track start/stop and record start/stop. Click Learn and press a foot switch, pad, key or preset button: its CC, note or program change does that action from then on, in every song and in Show mode, and never reaches an instrument. The devices whose program change picks the song are rows of the same list. A tuner shows the note the first Audio In plays. Agents use `get_show_controls` and `set_show_control`.

### Changed
- **One tool for show controls:** `set_show_control` replaces `set_setlist_footswitch`, and `get_setlist` lists `show_controls` in place of `foot_switch`. Sessions with the old foot-switch CCs or `program_devices` load them into the unified controls list.
