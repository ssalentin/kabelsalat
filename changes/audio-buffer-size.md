### Added
- **Configurable audio buffer size (#160):** adjust the output buffer size (64,
  128, 256, 512, 1024, or 2048 frames; default 256) live with **− / +** in
  Engine info, from **Settings → Buffer size**, or via the assistant/MCP tool
  `set_buffer_size`. Smaller buffers lower latency; larger buffers add headroom
  to avoid audio dropouts (xruns). Reopens the audio stream without losing the
  rack and persists in `config.json`.
