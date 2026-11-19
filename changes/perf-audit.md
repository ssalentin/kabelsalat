### Changed
- **Smaller download:** release builds are about a fifth smaller (27.5 MB to 22.4 MB stripped on aarch64). They link with full LTO and no longer carry the screen-reader bridge, which the custom-painted rack never fed.
- **Master limiter does less work per sample:** at the default WARM it computes two `tanh` instead of five. The sound is bit-identical.

### Added
- **`kabelsalat engine-bench`:** times the engine offline, with no audio device and no plugins, and prints a hash of the output so two builds can be compared.
