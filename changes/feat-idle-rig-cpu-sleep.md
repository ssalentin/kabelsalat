### Added
- **Idle-rig CPU sleep (#206):** non-live rigs that are no longer held and have completely rung out below −100 dBFS (honouring CLAP `sleep` / `tail` extensions and built-in delay and compensation tails) now sleep allocation-free without rendering DSP or plugins, waking in the exact audio block a note, control, or part switch targets them.
