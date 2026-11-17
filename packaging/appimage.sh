#!/usr/bin/env bash
# AppImage built with linuxdeploy: kabelsalat-<arch>.AppImage and its .zsync in
# <outdir>. The name carries no version, so releases/latest/download/<name> is a
# fixed link, and the embedded update information lets AppImageUpdate (Gear
# Lever, AppImageLauncher) fetch the newest release's changed blocks.
# Bundles lilv and its libraries; libasound, glibc and the display stack come
# from the host (linuxdeploy's excludelist), so pipewire-alsa keeps working.
# usage: packaging/appimage.sh <binary> <version> <outdir>
set -euo pipefail
bin=$(realpath "$1") version=$2 out=$(mkdir -p "$3" && realpath "$3")
root=$(cd "$(dirname "$0")/.." && pwd)
repo=${GITHUB_REPOSITORY:-ssalentin/kabelsalat}
repo_owner=${repo%%/*} repo_name=${repo#*/}
arch=$(uname -m)
tag=1-alpha-20251107-1
case $arch in
  x86_64) sum=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d ;;
  aarch64) sum=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff ;;
  *) echo "no linuxdeploy build for $arch" >&2; exit 1 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tool="$work/linuxdeploy-$arch.AppImage"
curl -sSfL -o "$tool" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$tag/linuxdeploy-$arch.AppImage"
echo "$sum  $tool" | sha256sum -c -
chmod +x "$tool"
cp "$root/assets/logo.svg" "$work/kabelsalat.svg"
mkdir -p "$out"
(
  cd "$work"
  APPIMAGE_EXTRACT_AND_RUN=1 LINUXDEPLOY_OUTPUT_VERSION="$version" \
    LDAI_OUTPUT="kabelsalat-$arch.AppImage" \
    LDAI_UPDATE_INFORMATION="gh-releases-zsync|$repo_owner|$repo_name|latest|kabelsalat-$arch.AppImage.zsync" \
    "$tool" --appdir AppDir --executable "$bin" \
      --desktop-file "$root/packaging/kabelsalat.desktop" \
      --icon-file kabelsalat.svg --output appimage
)
mv "$work/kabelsalat-$arch.AppImage" "$work/kabelsalat-$arch.AppImage.zsync" "$out/"
echo "$out/kabelsalat-$arch.AppImage"
