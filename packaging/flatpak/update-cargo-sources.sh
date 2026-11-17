#!/usr/bin/env bash
# Regenerate cargo-sources.json from Cargo.lock (run after every Cargo.lock
# change; CI fails while it is stale). Needs flatpak-cargo-generator on PATH,
# or docker, which runs it from the Flatpak builder image.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
image=ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08
if command -v flatpak-cargo-generator >/dev/null; then
  cd "$root" && flatpak-cargo-generator Cargo.lock -o packaging/flatpak/cargo-sources.json
else
  docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$root:/src" -w /src "$image" \
    flatpak-cargo-generator Cargo.lock -o packaging/flatpak/cargo-sources.json
fi
