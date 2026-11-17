#!/usr/bin/env bash
# Build the Flatpak bundle kabelsalat-<version>-<arch>.flatpak in <outdir>.
# Runs inside ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08
# (privileged). The runtime and SDK come preinstalled in that image; the Rust
# extension goes into a user installation, because updating the image's
# system installation fails on overlayfs (EXDEV).
# usage: packaging/flatpak/build.sh <version> <outdir> [state-dir]
set -euo pipefail
version=$1 out=$(realpath -m "$2") state=$(realpath -m "${3:-.flatpak-builder}")
here=$(cd "$(dirname "$0")" && pwd)
app=io.github.ssalentin.kabelsalat
arch=$(flatpak --default-arch)
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user -y --noninteractive flathub org.freedesktop.Sdk.Extension.rust-stable//26.08
mkdir -p "$out" "$state"
flatpak-builder --disable-rofiles-fuse --force-clean --state-dir="$state" \
  --repo="$state/repo" "$state/build" "$here/$app.yml"
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
  "$state/repo" "$out/kabelsalat-$version-$arch.flatpak" "$app"
echo "$out/kabelsalat-$version-$arch.flatpak"
