### Fixed
- **Assistant `set_cues` accepts its cue list (#468):** the argument checker refused every `cues` array with "must be array or null", so the agent could not set a song's cues. Arrays of text lines are now read as meant; a bare text or a non-text line is refused with an error the model can correct.
