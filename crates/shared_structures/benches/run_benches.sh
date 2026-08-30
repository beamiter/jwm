#!/usr/bin/env bash
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
readonly REPO_ROOT
readonly BACKENDS=(futex semaphore eventfd)

usage() {
    echo "Usage: $0 [--clean]"
    echo ""
    echo "Environment:"
    echo "  RUN_STRESS=1  also run the long-running stress benchmark for every backend"
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ "${1:-}" == "--clean" ]]; then
    rm -rf -- "${REPO_ROOT}/target/criterion"
    shift
fi

if (($# != 0)); then
    usage >&2
    exit 2
fi

cd -- "${REPO_ROOT}"

for backend in "${BACKENDS[@]}"; do
    echo "Running ring-buffer benchmarks with only the ${backend} backend enabled..."
    cargo bench \
        --no-default-features \
        --features "${backend}" \
        --bench ring_buffer_bench \
        -- \
        --save-baseline "${backend}"
done

echo "Running direct, same-process comparison of all synchronization backends..."
cargo bench --all-features --bench ring_buffer_bench -- strategy_

if [[ "${RUN_STRESS:-0}" == "1" ]]; then
    for backend in "${BACKENDS[@]}"; do
        echo "Running stress benchmarks with only the ${backend} backend enabled..."
        cargo bench \
            --no-default-features \
            --features "${backend}" \
            --bench stress_test
    done
fi

if command -v critcmp >/dev/null 2>&1; then
    critcmp futex semaphore
    critcmp futex eventfd
    critcmp semaphore eventfd
else
    echo "critcmp is not installed; Criterion HTML reports are under target/criterion/."
fi
