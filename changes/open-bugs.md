### Fixed
- **Dexed no longer crashes the test suite (#52):** its destructor segfaults
  while other plugin instances render, so a retired Dexed is parked and reused
  by the next one, never destroyed.
- **Plugins page header (#65):** the "New <plugin> rig" confirmation has its
  own row under the subtitle instead of being painted over it.
