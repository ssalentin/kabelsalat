#!/usr/bin/env bash
# shellcheck disable=SC1091,SC2154 # depends, makedepends, source, _pkgname come from the PKGBUILD
# Build packaging/PKGBUILD from this checkout (not from GitHub's main) in an
# Arch Linux container, as root: installs the PKGBUILD's dependencies, seeds
# makepkg's git mirror with the checked-out commit and runs makepkg --holdver
# as an unprivileged user. The package lands in <outdir>.
# usage: packaging/arch-makepkg.sh <outdir>
set -euo pipefail
out=$(realpath -m "$1")
root=$(cd "$(dirname "$0")/.." && pwd)
work=/var/tmp/kabelsalat-makepkg
rm -rf "$work"
mkdir -p "$work" "$out"
cp "$root/packaging/PKGBUILD" "$work/"

source "$work/PKGBUILD"
pacman -Syu --noconfirm --needed base-devel git "${depends[@]}" "${makedepends[@]}"

git config --global --add safe.directory '*'
git clone --quiet --mirror "$root" "$work/$_pkgname"
git -C "$work/$_pkgname" config remote.origin.url "${source[0]#git+}"
git -C "$work/$_pkgname" update-ref refs/heads/makepkg-ci "$(git -C "$root" rev-parse HEAD)"
git -C "$work/$_pkgname" symbolic-ref HEAD refs/heads/makepkg-ci

id builder >/dev/null 2>&1 || useradd -m builder
chown -R builder "$work"
su builder -c "cd '$work' && git config --global --add safe.directory '*' && makepkg --holdver --noconfirm"
cp "$work"/*.pkg.tar.* "$out/"
ls -l "$out"/*.pkg.tar.*
