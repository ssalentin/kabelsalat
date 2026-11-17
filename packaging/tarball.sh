#!/usr/bin/env bash
# Plain binary archive: kabelsalat-<version>-linux-<arch>.tar.gz in <outdir>.
# usage: packaging/tarball.sh <binary> <version> <outdir>
set -euo pipefail
bin=$1 version=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)
arch=$(uname -m)
name="kabelsalat-$version-linux-$arch"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
install -m755 "$bin" "$stage/$name/kabelsalat"
strip "$stage/$name/kabelsalat"
install -m644 "$root/packaging/kabelsalat.desktop" "$stage/$name/"
install -m644 "$root/assets/logo.svg" "$stage/$name/kabelsalat.svg"
install -m644 "$root/assets/logo.png" "$stage/$name/kabelsalat.png"
install -m644 "$root/LICENSE" "$root/README.md" "$stage/$name/"
install -m644 "$root/packaging/INSTALL-binary.md" "$stage/$name/INSTALL.md"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
