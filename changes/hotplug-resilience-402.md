### Added
- **Stage hot-plug resilience (#402):** a lost audio output (interface unplugged, PipeWire or JACK restarted, a stalled stream) is reopened by itself with its backend, output, sample rate and buffer size; JACK output ports another client disconnected are connected back; an audio input comes back when its device does; a stage notice and Show-mode indicator say what happened.

### Fixed
- **Unplugged keyboards no longer leave notes droning (#402):** keys and the sustain pedal a MIDI device was holding when it was unplugged are released at once.
