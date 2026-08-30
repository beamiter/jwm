#!/usr/bin/env bash
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
cd -- "${SCRIPT_DIR}"

usage() {
    echo "Usage: $0 [address|thread|memory ...]"
    echo ""
    echo "Defaults to AddressSanitizer and ThreadSanitizer. MemorySanitizer is opt-in"
    echo "because uninstrumented native libraries can produce false positives."
    echo ""
    echo "Environment:"
    echo "  SANITIZER_BACKENDS='futex semaphore eventfd'  select backend matrix"
    echo "  SANITIZER_TARGET=<rust-target>                 override the host target"
    echo "  RUST_TEST_THREADS=<n>                          default: 1"
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "san.sh supports this crate's Linux IPC backends only." >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    echo "cargo and rustc are required." >&2
    exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to select a nightly toolchain and rust-src." >&2
    exit 1
fi

readonly TARGET="${SANITIZER_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
if [[ -z "${TARGET}" ]]; then
    echo "could not determine a Rust host target; set SANITIZER_TARGET explicitly." >&2
    exit 1
fi

if ! cargo +nightly --version >/dev/null 2>&1 || ! rustc +nightly --version >/dev/null 2>&1; then
    echo "a complete nightly Rust toolchain is required; run:" >&2
    echo "  rustup toolchain install nightly --profile minimal --component rust-src" >&2
    exit 1
fi

NIGHTLY_SYSROOT="$(rustc +nightly --print sysroot)"
readonly NIGHTLY_SYSROOT
if [[ ! -d "${NIGHTLY_SYSROOT}/lib/rustlib/src/rust/library" ]]; then
    echo "nightly rust-src is required; run: rustup component add rust-src --toolchain nightly" >&2
    exit 1
fi

if (($# > 0)); then
    sanitizers=("$@")
else
    sanitizers=(address thread)
fi

IFS=' ' read -r -a backends <<< "${SANITIZER_BACKENDS:-futex semaphore eventfd}"
if ((${#backends[@]} == 0)); then
    echo "SANITIZER_BACKENDS must contain at least one backend." >&2
    exit 2
fi

for sanitizer in "${sanitizers[@]}"; do
    case "${sanitizer}" in
        address | thread) ;;
        memory)
            if [[ "${TARGET}" != x86_64-unknown-linux-gnu ]]; then
                echo "MemorySanitizer is only enabled here for x86_64-unknown-linux-gnu." >&2
                exit 2
            fi
            ;;
        *)
            echo "unknown sanitizer: ${sanitizer}" >&2
            usage >&2
            exit 2
            ;;
    esac

    for backend in "${backends[@]}"; do
        case "${backend}" in
            futex | semaphore | eventfd) ;;
            *)
                echo "unknown backend: ${backend}" >&2
                exit 2
                ;;
        esac

        echo "Running ${sanitizer} sanitizer with the ${backend} backend (${TARGET})..."
        env \
            CARGO_TARGET_DIR="${SCRIPT_DIR}/target/sanitizers/${sanitizer}/${backend}" \
            RUSTFLAGS="-Zsanitizer=${sanitizer}" \
            cargo +nightly test \
                -Zbuild-std \
                --target "${TARGET}" \
                --no-default-features \
                --features "${backend}" \
                --lib \
                --tests \
                -- \
                --test-threads="${RUST_TEST_THREADS:-1}"
    done
done
