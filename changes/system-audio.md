### Added
- **Other apps' strips say whose they are (#31):** each System audio strip in the mixer now shows the app and what it plays above its fader (a long title is cut, the full text is in the tooltip). Before, a strip was a fader with no name.
- **`kabelsalat audio-out-test`:** checks the output selection and the System audio strips against the machine's real PipeWire: it plays a silent stream and a test tone into two temporary outputs, moves the stream to a chosen output and back, and turns the tone down, mutes it and meters it.

### Fixed
- **"System default" gives the sound back to the system's output (#31):** after you had picked a sound card at Master out, choosing System default left kabelsalat playing to that card. It now moves back, and a move PipeWire did not carry out is asked for again.
- **A strip keeps its own peak hold when another app stops playing (#31):** the remaining strips moved up and took over the held peak and clip light of the app that had stopped.
