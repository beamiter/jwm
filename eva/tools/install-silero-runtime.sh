#!/usr/bin/env bash
# Optional, explicit user-space install. Never starts/restarts Eva or a desktop.
set -euo pipefail
prefix=
archive=
while (( $# )); do
    case "$1" in
        --prefix) prefix=${2:?missing --prefix value}; shift 2 ;;
        --archive) archive=${2:?missing --archive value}; shift 2 ;;
        *) echo 'usage: install-silero-runtime.sh --prefix /absolute/directory [--archive /local/archive.tgz]' >&2; exit 2 ;;
    esac
done
[[ "$prefix" == /* && "$prefix" != / ]] || { echo 'an absolute --prefix directory is required' >&2; exit 2; }
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64)
        arch=x64
        archive_hash=8344d55f93d5bc5021ce342db50f62079daf39aaafb5d311a451846228be49b3
        library_hash=3da6146e14e7b8aaec625dde11d6114c7457c87a5f93d744897da8781e35c673 ;;
    Linux/aarch64)
        arch=aarch64
        archive_hash=bb76395092d150b52c7092dc6b8f2fe4d80f0f3bf0416d2f269193e347e24702
        library_hash=0afd69a0ae38c5099fd0e8604dda398ac43dee67cd9c6394b5142b19e82528de ;;
    *) echo 'helper supports Linux x86_64/aarch64; install a compatible CPU runtime manually' >&2; exit 2 ;;
esac
library="$prefix/lib/libonnxruntime.so.1.22.0"
verify_library() { printf '%s  %s\n' "$library_hash" "$library" | sha256sum --check --status; }
if [[ -e "$prefix" ]]; then
    [[ -f "$prefix/LICENSE" ]] && verify_library || { echo 'existing prefix does not match pinned runtime; choose a new directory' >&2; exit 1; }
else
    parent=$(dirname "$prefix")
    mkdir -p "$parent"
    staging=$(mktemp -d "$parent/.eva-ort-XXXXXX")
    trap 'rm -rf "$staging"' EXIT
    if [[ -z "$archive" ]]; then
        archive="$staging/runtime.tgz"
        curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
            "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-$arch-1.22.0.tgz" -o "$archive"
    fi
    printf '%s  %s\n' "$archive_hash" "$archive" | sha256sum --check --status || { echo 'runtime archive checksum mismatch' >&2; exit 1; }
    mkdir "$staging/unpacked"
    tar -xzf "$archive" -C "$staging/unpacked" --strip-components=1 --no-same-owner
    [[ -f "$staging/unpacked/LICENSE" ]] || { echo 'runtime license missing' >&2; exit 1; }
    # Verify the library before exposing the directory to a running process.
    printf '%s  %s\n' "$library_hash" "$staging/unpacked/lib/libonnxruntime.so.1.22.0" | sha256sum --check --status
    mv -T -n "$staging/unpacked" "$prefix"
    verify_library
fi
printf 'ORT_DYLIB_PATH=%s\n' "$library"
