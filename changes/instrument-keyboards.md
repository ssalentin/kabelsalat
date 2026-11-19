### Added
- **The on-screen keyboard follows the instrument (#163):** Piano (damper and soft pedals, touch velocity), Jazz Organ (drawbars, registrations, Leslie, swell, two manuals on channels 1 and 2), Pipe Organ (stop jamb, pistons, tremulant, couplers, swell) and Synth (wheels, the patch's macros, an X/Y pad, scale FOLD, arpeggiator). It switches with the active rig or stays on the one you pin, and the session remembers the pin.
- **Expanded keyboard (Shift+V):** the keyboard takes the whole stage with the instrument's full console above the keys; Esc or DOCK brings the rack back.
- **Keys hold as long as you press them:** a pressed on-screen key sounds until you lift the pointer, drags across organ keys play a glissando, and the held chord is named in the header.
- **Agent:** `get_keyboard_state` and `set_keyboard_mode` read and switch the keyboard in the app, over MCP and in headless MCP.
