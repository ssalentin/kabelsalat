### Added
- **Send scenes per setlist part (#415):** a part can store send levels into aux buses and the bypass and knobs of the buses' effects, recalled on a switch whatever level the rig lives at, so a show-level vocal or guitar rig goes from a dry verse to a wet chorus. Send levels ramp over 5 ms and tails ring on. Setlist: **Save sends** / **Clear sends**; agents and MCP: `set_part_sends`, `scene` in `get_setlist` and `set_setlist_song`.
- **Stage Sends template (Shift+F8, #415):** a singer's or guitarist's rig (Audio In, tone, limiter) with Hall and Echo buses and a Verse / Chorus / Solo song, each part with its own send scene.

### Changed
- **Send levels never step (#415):** a send changed by a controller, the mixer, an agent or a part switch now ramps over 5 ms instead of jumping in one sample.
