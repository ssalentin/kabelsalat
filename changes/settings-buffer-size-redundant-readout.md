### Fixed
- **Settings → Audio**: stop repeating `Active:` under Audio backend and Audio output when the dropdown already shows that value, show the JACK server's buffer size read-only, and replace the cryptic `Active: N frames` line under Buffer size with an explicit override notice only when the audio server forces a different quantum.
