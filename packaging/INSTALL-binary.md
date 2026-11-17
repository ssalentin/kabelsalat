# kabelsalat binary archive

This archive holds the `kabelsalat` binary for one CPU architecture, its
desktop entry and its icon. It is built on Ubuntu 22.04 and needs glibc 2.35
or newer.

## Runtime dependencies

Install these with your distribution's package manager:

| Needed for | Arch Linux | Debian / Ubuntu | Fedora |
| --- | --- | --- | --- |
| LV2 hosting | `lilv` | `liblilv-0-0` | `lilv-libs` |
| Audio and MIDI | `alsa-lib` | `libasound2` | `alsa-lib` |
| Realtime audio (rtkit) | `dbus` | `libdbus-1-3` | `dbus-libs` |
| Window | `libxkbcommon`, `wayland`, `libglvnd` | `libxkbcommon0`, `libwayland-client0`, `libegl1` | `libxkbcommon`, `libwayland-client`, `libglvnd-egl` |

Optional:

- PipeWire with its ALSA and Pulse layers (`pipewire-alsa`, `pipewire-pulse`)
  for low-latency output, audio inputs (`pw-record`) and system-audio
  capture (`pactl`, `parec`).
- Plugins. Surge XT (CLAP) is the default synth. Any CLAP plugin in
  `~/.clap` or `/usr/lib/clap` and any LV2 plugin in `~/.lv2` or
  `/usr/lib/lv2` shows up in the plugin browser.

## Install for one user

```sh
install -Dm755 kabelsalat ~/.local/bin/kabelsalat
install -Dm644 kabelsalat.desktop ~/.local/share/applications/kabelsalat.desktop
install -Dm644 kabelsalat.svg ~/.local/share/icons/hicolor/scalable/apps/kabelsalat.svg
```

`kabelsalat --version` prints the version and `kabelsalat --help` lists the
command-line modes.
