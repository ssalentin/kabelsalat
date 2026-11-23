### Fixed
- **A passed plugin self-test no longer aborts on exit (#422):** `vst3-test` and `editor-test` with Six Sines (VST3) printed PASS and then died with "terminate called without an active exception". A process that loaded plugin code now leaves without running the plugin libraries' static destructors.
- **`editor-test` across all plugins under Wayland (#421):** the EGL platform for plugin editors is now fixed before any plugin library is loaded, so Unsampled Piano no longer crashes after other plugins ran first.
