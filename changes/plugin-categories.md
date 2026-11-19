### Added
- **Plugins organized by type:** the **Plugins** view (**F9** / **B**) groups built-in units, CLAP and LV2 plugins as **Instruments**, **Effects**, **MIDI FX** and **Utilities** instead of by format. Each type narrows by subcategory (Delay & Reverb, EQ & Filter, Dynamics, Drive & Amp, Modulation, Synths, Organs, Samplers, Analyzers, Mix & Gain, …) and by format (Built-in, CLAP, LV2). Every built-in effect and the Arpeggiator are listed there too, and LV2 cards show the author and plugin class instead of a URI.
- **Add without losing the rack:** **New rig** puts a CLAP or LV2 instrument on a rig of its own beside the others, **Add to rig** layers an instrument or chains an effect into the active rig, and **Master FX** inserts an effect on the master before the limiter. Adding an LV2 plugin used to replace the whole patch.
- **Insert picker by category:** the Mixer's **+ insert** filters by Built-in, Delay & Reverb, EQ & Filter, Dynamics, Drive & Amp, Modulation and Analyzers, lists LV2 effects without typing a filter first, groups them by category and leaves out MIDI-only plugins.
- **`list_plugins` by type:** the assistant's and MCP's `list_plugins` take `type`, `category`, `format`, `query` and `limit`, list built-in units too, and tag every plugin with its type and category; `list_instruments` tags catalog entries as instrument or effect.

### Fixed
- **CLAP effects are no longer offered as instruments:** Surge XT Effects showed under CLAP instruments with a **New rig** button, although kabelsalat cannot host CLAP effects yet. It is listed under Effects as not hostable yet, and `add_rig` refuses a CLAP id that is not an instrument.
- **Filter chips wrap whole:** in the Plugins view and the insert picker, a chip that did not fit its row ran past the window or broke into one letter per line; it now starts the next line.
- **Add MIDI strip returns to the rack**, like every other add in the Plugins view.
