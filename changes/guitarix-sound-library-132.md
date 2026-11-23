### Fixed
- **Guitarix is an effect, not a sound-library instrument (#132):** `sound-library-test` asked the Guitarix amp suite for presets and failed ("no LV2 presets found"). It has none, so the test skips effect cards and says so; the card and docs/PLUGINS.md say the same.
