### Added
- **CI performance budgets (#280):** headless benchmarks for audio block render time, UI frame time and vertex counts, built-in 4-rig session load time, start-up to first frame, and default-rack memory now emit JSON and are enforced in CI on both `x86_64` and `aarch64` against versioned budgets and release baselines (`docs/perf-budgets.json`, `docs/ARCHITECTURE.md`).
