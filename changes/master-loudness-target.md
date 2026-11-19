### Added
- **Loudness target for the master (#12):** the Mixer has a Loudness column
  beside Master out. Pick −14 LUFS (Spotify, YouTube, Tidal), −16 (Apple
  Music), −8 (club) or step to any value; kabelsalat trims the level in front
  of the master limiter by what your playing was missing and holds the true
  peak under −1 dBTP. The column shows the loudness before, the loudness now,
  the true peak and the trim. "Match" measures again and corrects the trim,
  and says so when the limiter has no more to give and the master VOLUME has
  to come up. The target is saved with the session and follows undo. The
  assistant and MCP agents use `set_master_target` and `get_master_target`;
  `get_status` carries the same readout.

### Fixed
- **True peak read too high:** `measure_mix` reported a peak at half scale as
  −3 dBTP instead of −6. The detector now interpolates correctly and the
  value is in dB of amplitude; `true_peak_max_db` holds the highest peak
  since the last reset.
- **Integrated loudness of dynamic playing read too low:** it averaged the
  dB values of its 400 ms blocks instead of their energy (a loud and a 10 LU
  quieter passage read 2.4 LU low).
