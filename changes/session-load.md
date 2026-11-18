### Fixed
- **Loading a session with an LV2 unit took 3 to 4 seconds (#55):** every rack
  rebuild scanned all installed LV2 plugins on the UI thread to tell whether
  the unit is an instrument. It now uses the background plugin scan, and the
  setBfree session loads in under 1 ms of UI time (was 2.6 to 4.0 s). Agent
  edits of such a rack no longer freeze the window either.
