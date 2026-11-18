### Added
- **Audio In unit with a tuner (#28):** a rack unit for any PipeWire input, with gain, a level meter, a MON switch for low-latency monitoring (about 19 ms round trip measured) and a tuner on its face. A missing input says so instead of using the laptop microphone.
- **Guitar rig (#92):** ⇧F5 loads Audio In → Scream → Delay → Reverb → Amp → Cabinet → Master from the Guitarix LV2 plugins (install the Guitarix card), and the assistant can build it for you.

### Fixed
- **Monitoring no longer lags by hundreds of milliseconds:** a backlog in the input ring is cut at once, and input drift corrections no longer swap the left and right channels.
