#!/usr/bin/env bash
# Build the upstream bitmap-copy fix without replacing the system WebKitGTK.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
work=${WEBKIT_BUILD_DIR:-"$repo/target/webkit-transparent"}
prefix=${WEBKIT_INSTALL_PREFIX:-"$HOME/.local/opt/jwm-webkitgtk-2.50.4"}
mkdir -p "$work"
archive="$work/webkitgtk-2.50.4.tar.xz"
source_dir="$work/webkitgtk-2.50.4"
if [[ ! -f "$archive" ]]; then
  curl --fail --location --retry 2 https://webkitgtk.org/releases/webkitgtk-2.50.4.tar.xz -o "$archive.part"
  mv "$archive.part" "$archive"
fi
echo "d3bfa473845acfab72635bada5e0d134fda6792c5b95c5c5cd141b46125bd8e4  $archive" | sha256sum --check --status
if [[ ! -d "$source_dir" ]]; then tar -xf "$archive" -C "$work"; fi
patch_file="$repo/vendor/webkitgtk/shareable-bitmap-copy.patch"
if patch --dry-run --silent -d "$source_dir" -p1 < "$patch_file"; then
  patch -d "$source_dir" -p1 < "$patch_file"
else
  patch --dry-run --silent --reverse -d "$source_dir" -p1 < "$patch_file"
fi
cmake -S "$source_dir" -B "$work/build" -G Ninja \
  -DPORT=GTK -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_FLAGS_RELEASE="-O2 -DNDEBUG" -DCMAKE_CXX_FLAGS_RELEASE="-O2 -DNDEBUG" \
  -DCMAKE_C_COMPILER="${CC:-gcc-12}" -DCMAKE_CXX_COMPILER="${CXX:-g++-12}" \
  -DCMAKE_INSTALL_PREFIX="$prefix" -DCMAKE_INSTALL_LIBDIR=lib \
  -DUSE_GTK4=OFF -DUSE_SOUP2=OFF -DENABLE_DOCUMENTATION=OFF \
  -DENABLE_INTROSPECTION=OFF -DENABLE_MINIBROWSER=OFF -DENABLE_GAMEPAD=OFF \
  -DUSE_WOFF2=OFF -DUSE_AVIF=OFF -DUSE_JPEGXL=OFF \
  -DUSE_FLITE=ON -DENABLE_SPEECH_SYNTHESIS=ON -DENABLE_SPELLCHECK=ON \
  -DUSE_SYSTEM_SYSPROF_CAPTURE=OFF -DUSE_LIBBACKTRACE=OFF
cmake --build "$work/build" --parallel "${WEBKIT_BUILD_JOBS:-16}"
cmake --install "$work/build"
