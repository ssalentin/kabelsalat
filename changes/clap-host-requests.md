### Fixed
- **CLAP plugins get what they ask the host for (#180):** a plugin that
  asks for a call on the main thread, for a restart or for a parameter flush
  is now served within one frame, with its editor open or closed. Before,
  such a request was noted and never answered, so a plugin that finishes a
  patch or sample load on the main thread could wait for good.
