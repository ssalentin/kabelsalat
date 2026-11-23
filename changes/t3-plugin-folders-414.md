### Added
- **Your own plugin folders (#414):** Settings → Plugins lists custom CLAP, VST3
  and LV2 search folders. Add, edit or remove one without touching environment
  variables; the folders are remembered, every change rescans your plugins in
  the background, and a folder that is missing right now is marked. The
  assistant and MCP agents use the same lists (`list_plugin_folders`,
  `add_plugin_folder`, `remove_plugin_folder`). `CLAP_PATH`, `VST3_PATH` and
  `LV2_PATH` keep working.
