### Fixed
- **Monospace digit spacing without JetBrains Mono (#327):** probe JetBrains Mono across Arch, Debian/Ubuntu, Fedora, `/usr/local`, Flatpak, and user font directories, and keep egui's built-in `Hack` monospace face ahead of symbol/math fallbacks so monospace labels (such as `CC 64` in Setlist) always render digits with monospace spacing even when JetBrains Mono is not installed.
