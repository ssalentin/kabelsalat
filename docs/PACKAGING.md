# Packaging and releases

How kabelsalat is tested on GitHub, which packages a release ships and how to
cut one. Workflows live in `.github/workflows/`, and every packaging step is a
script that also runs on a developer machine (in docker).

## CI on every pull request and on main

`.github/workflows/ci.yml` runs once per pull request push (`pull_request`) and
again on `main` after each merge, on x86_64 (`ubuntu-24.04`) and aarch64
(`ubuntu-24.04-arm`). A PR waits about 4 minutes for it. CI is the technical
sanity check that lets agents merge on their own: build, unit suite, images,
changelog fragments. Packaging is checked by the nightly build, not per PR.

- `cargo build --locked`, then `scripts/ci/test.sh`: `cargo test --bin
  kabelsalat`, green only when cargo exits 0 and prints `test result: ok`
  (AGENTS.md rule 50).
- The runners have no sound card, display, MIDI device or plugin, and the
  suite needs none (rule 37). Ubuntu's `setbfree` package is installed so
  `lv2::tests::setbfree_plays_a_note` plays a real LV2 instrument through
  lilv. The tests that need Surge XT, Dexed or Vitalium return early when the
  plugin is missing, so on CI they pass without testing anything:
  `engine::tests::surge_cc_moves_real_param`,
  `host::tests::non_surge_clap_discovers_and_renders_when_installed`,
  `host::tests::dexed_load_patch_changes_state_and_renders`,
  `lv2::tests::discover_presets_vitalium`,
  `lv2::tests::vitalium_survives_repeated_load_and_free`,
  `lv2::tests::vitalium_apply_preset_and_render_audio`,
  `lv2::tests::reused_vitalium_starts_fresh_and_still_plays`,
  `ui::tests::vitalium_and_dexed_editor_support`. Run them, and the CLI
  self-tests listed in AGENTS.md, on a machine with those plugins.
- `scripts/check-images.sh` (AGENTS.md rule 108) and `scripts/release.py
  check` (changelog fragments parse) run first; both take under a second.
- `cargo fmt --check` is not run: the tree is not rustfmt-formatted, and a
  one-off reformat would conflict with every open PR. Do it in a quiet moment,
  then make it a required check.

### Branch protection and a red main

`main` requires `test (x86_64)` and `test (aarch64)`, but not that a branch
is up to date: with several agents' PRs open, "up to date" made every merge
re-run every other PR's CI, one after another. A PR merges as soon as its own run is green.

The price is that two PRs, green on their own, can break `main` together.
CI on `main` catches that within minutes, and `.github/workflows/main-health.yml`
opens one issue labelled `main-red` with the failing run (and closes it when
`main` is green again). Fix forward with a PR, or revert the breaking merge.

### Conflicts between parallel PRs

Parallel PRs append to the same lists, above all the AGENTS.md rules. After
every merge to `main`, `.github/workflows/resolve-conflicts.yml` merges `main`
into each open PR that now conflicts, when every conflict is an insertion on
both sides of a Markdown file: `scripts/ci/resolve_appends.py` keeps both
(two rules may then share a number; refer to rules by their test name too).
It pushes the merge and dispatches CI. Any other conflict, including every
conflict in code, gets the label `merge-conflict` and a comment; the branch's
author resolves it (the script also works locally inside a conflicted
`git merge`). Changelog entries are fragments (below), so they never conflict.

## Release packages

`.github/workflows/release.yml` runs on a `v*` tag (pushed, or created by
`tag-release.yml`), every night at 03:17 UTC on `main` (the `nightly`
pre-release, below), and by hand from the Actions tab (`workflow_dispatch`,
version `<crate version>-dev.<sha>`, no release). Each package is built natively for x86_64 and aarch64:

| Package | Built by | Built on | Notes |
| --- | --- | --- | --- |
| `kabelsalat-<v>-linux-<arch>.tar.gz` | `packaging/tarball.sh` | Ubuntu 22.04 (glibc 2.35) | Binary, desktop file, icon, `INSTALL.md` with the runtime libraries to install. |
| `kabelsalat-<arch>.AppImage` (+ `.zsync`) | `packaging/appimage.sh` ([linuxdeploy](https://github.com/linuxdeploy/linuxdeploy) `1-alpha-20251107-1`, checksum-pinned) | Ubuntu 22.04 | Bundles lilv, serd, sord, sratom; uses the host's glibc, ALSA (so `pipewire-alsa` works) and display stack. |
| `kabelsalat-<v>-<arch>.flatpak` | `packaging/flatpak/build.sh` | `ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08` | Single-file bundle, see below. |
| `kabelsalat-git-<v>.r<n>.g<sha>-1-<arch>.pkg.tar.*` | `packaging/arch-makepkg.sh` | `archlinux:base-devel`, `menci/archlinuxarm:base-devel` | `packaging/PKGBUILD` built from the tagged commit. |

**Recommended download: the AppImage.** It runs unsandboxed, so it finds the
user's CLAP/LV2 plugins, its audio thread gets realtime priority through rtkit
(AGENTS.md rule 29) and `kabelsalat mcp --attach` reaches it. The Flatpak is a
secondary format: in its sandbox rtkit cannot promote the audio thread (no
realtime until the app uses the Realtime portal), plugins needing a newer glibc
than the runtime do not load, and host-side agents cannot attach. README and
every release's notes point to the AppImage.

Each job smoke-tests its package with `kabelsalat --version`. The release job
adds `SHA256SUMS`, then creates the GitHub release, or uploads to it when the
release already exists. Its notes (`scripts/ci/release-notes.sh <v> dist <tag>
<repo>`) are the tag's CHANGELOG block plus a **Download** section that links
each AppImage directly (x86_64 first), then the other packages and
`SHA256SUMS`; a release without an AppImage fails.

The AppImage name carries no version (as Neovim's does not), so README's
links to `releases/latest/download/kabelsalat-<arch>.AppImage` always fetch
the newest; the version is the release tag and `kabelsalat --version`. Each
AppImage embeds the update information
`gh-releases-zsync|<owner>|<repo>|latest|kabelsalat-<arch>.AppImage.zsync` and
ships its `.zsync`, so AppImageUpdate, Gear Lever and AppImageLauncher update
an installed copy by downloading only the changed blocks. The smoke test
checks both. The tarballs keep the version in their names.

### Packages for an existing tag

A tag pushed before this workflow existed (v0.1.0) has a release without
packages. Run **Release** from the Actions tab with `tag` set to it, or:

```sh
gh workflow run release.yml -R ssalentin/kabelsalat --ref main -f tag=v0.1.0
```

The tarball and AppImage jobs build the tag's source with this ref's packaging
scripts and attach the files to that release, rewriting its notes with the
download links. Flatpak and Arch are skipped: their manifests build from the
current tree, not from the tag.

## Flatpak

App id `io.github.ssalentin.kabelsalat`, runtime `org.freedesktop.Platform`
26.08, Rust from `org.freedesktop.Sdk.Extension.rust-stable`. The manifest
`packaging/flatpak/io.github.ssalentin.kabelsalat.yml` builds lv2, zix, serd,
sord, sratom and lilv (the runtime has none of them), then kabelsalat offline
from `cargo-sources.json` (generated from `Cargo.lock` by `build.sh`).

Install and run a bundle:

```sh
flatpak install --user kabelsalat-<v>-<arch>.flatpak
flatpak run io.github.ssalentin.kabelsalat
```

Sandbox permissions, and what they are for:

- Wayland and `dri`: the window. The app is a native Wayland client; it
  needs no X server.
- `pulseaudio` socket and `xdg-run/pipewire-0`: the runtime's `pactl`, `parec`,
  `paplay` and `pw-record` (system-audio capture, audio inputs, app streams)
  run inside the sandbox and talk to the host's sound server. No
  `flatpak-spawn --host` is needed.
- `--device=all`: `/dev/snd` for ALSA output and the ALSA sequencer (MIDI and
  hot-plug). Flatpak has no narrower device class for sound.
- `network`: the assistant's model APIs.
- `xdg-config/kabelsalat`, `~/kabelsalat-sessions`,
  `~/kabelsalat-rig-presets`, `xdg-music/kabelsalat`: the same config,
  sessions, presets and recordings as a native install. `~/.cache/kabelsalat`
  (scan cache) is private to the sandbox.
- Plugins: `~/.clap` and `~/.lv2` read-write (source installs land there),
  the host's `/usr` read-only through `host-os` (`CLAP_PATH` and `LV2_PATH`
  point at `/run/host/usr/{lib,lib64,local/lib}/{clap,lv2}`), and Flathub's
  `org.freedesktop.LinuxAudio.Plugins.*` extensions at
  `/app/extensions/Plugins`.

Known limits:

- Plugins from the host's `/usr/lib` are built against the host's libraries
  but load against the runtime's. A plugin that needs a newer glibc or
  libstdc++ than the 26.08 runtime fails to load. Plugins in `~/.clap`,
  `~/.lv2` and the LinuxAudio extensions are the reliable path.
- No realtime render thread: `audio_thread_priority` asks rtkit to promote
  the sandbox's thread id, which rtkit cannot resolve on the host, so the
  manifest does not grant `org.freedesktop.RealtimeKit1`. Realtime in the
  sandbox needs the `org.freedesktop.portal.Realtime` portal.
- `kabelsalat mcp --attach` from the host cannot reach the sandboxed app's
  socket; run it inside the sandbox with
  `flatpak run --command=kabelsalat io.github.ssalentin.kabelsalat mcp --attach`.
- The instrument catalog's "is it installed" probe looks at `/usr/lib/clap`
  and `/usr/lib/lv2`, which are empty in the sandbox.

## Build a package locally

Each script runs on the host it targets, as in CI. With docker:

```sh
# Flatpak (privileged: flatpak-builder needs bubblewrap)
docker run --rm --privileged -v "$PWD:/src" -w /src \
  ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08 \
  packaging/flatpak/build.sh 0.1.0 dist .flatpak-builder
# Arch package (x86_64: archlinux:base-devel)
docker run --rm -v "$PWD:/src" -w /src menci/archlinuxarm:base-devel \
  bash -c 'pacman --disable-sandbox -Sy --noconfirm git && packaging/arch-makepkg.sh dist'
```

The tarball and AppImage scripts take a built binary:
`packaging/tarball.sh target/release/kabelsalat 0.1.0 dist` and
`packaging/appimage.sh target/release/kabelsalat 0.1.0 dist`. Keep build
output out of `/tmp` (AGENTS.md rule 79).

## Nightly

Every night the release workflow builds all packages from `main` (skipped
when `main` has not moved) and replaces the GitHub pre-release `nightly`, tag
included, with them. Its notes are `scripts/release.py notes`: what the next
release will say. A pre-release is never "latest", so the fixed
`releases/latest/download/…` links and AppImage self-updates keep serving the
last stable release. A failed nightly is a packaging bug to fix before the
next release.

## Releases

Every PR with a user-visible change adds a changelog fragment,
`changes/<branch>.md` (format in `changes/README.md`), instead of editing
`CHANGELOG.md`: parallel PRs never conflict on it. Entries still under
`## [Unreleased]` in `CHANGELOG.md` are folded in too.

Cut a release with one action:

```sh
gh workflow run prepare-release.yml -f bump=minor   # patch | minor | major | X.Y.Z
```

1. `prepare-release.yml` runs `scripts/release.py prepare <bump>`: it bumps
   `Cargo.toml` and `Cargo.lock`, folds the fragments and the Unreleased
   block into `## [X.Y.Z] — <date>` (with compare links), adds the metainfo
   `<release>` entry and deletes the fragments. It opens the PR
   `release-X.Y.Z`, queues it for auto-merge and dispatches CI on it (a PR
   opened with `GITHUB_TOKEN` starts no workflow by itself).
2. When it merges, `tag-release.yml` sees a `Cargo.toml` version without a
   tag, tags the merge commit `vX.Y.Z` and dispatches `release.yml` on it.
3. `release.yml` checks that the tag matches `Cargo.toml`, that the CHANGELOG
   has notes for it and that the metainfo lists it, then builds every package
   and publishes the release. Check it with `gh release view vX.Y.Z --json
   assets`.

The same works without the workflow: run `scripts/release.py prepare minor`
on a `release-X.Y.Z` branch, open the PR and queue it; step 2 follows on
merge.

While the version is `0.x`, a release with new features is a minor, one with
only fixes a patch.
