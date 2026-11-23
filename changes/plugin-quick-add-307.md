### Added
- **Plugin favorites and recents (#307):** star any built-in, CLAP, VST3 or LV2 plugin on its card in the Plugins view; the new **★ Favorites** and **Recent** filters show your starred plugins and the twelve you added last. Both are kept in the config file and are there after a restart.
- **Quick-add on the rack (#307):** press **Enter** or right-click empty rack space for a fuzzy-search picker that adds an instrument, effect, MIDI FX or utility to the rig in view without leaving the Rack view (Shift+Enter: a new rig, or a master insert). Favorites and recents head the list.
- **Agents (#307):** `set_plugin_favorite` stars a plugin, and `list_plugins` takes `shelf: "favorites"` or `"recent"`.
