### Added
- **Macros (#289):** one named control drives any number of parameters (plugin parameters, rack knobs, mixer controls), each with its own range, curve and invert. Bind it to a pedal like a knob; `set_macro`, `list_macros` and `list_cc_maps` report every target. Saved with the session and undoable.
- **Scene morph (#214):** store a rig's knobs as state A and B and fade between them with a MORPH knob or a learned pedal; stepped knobs switch at the middle.
