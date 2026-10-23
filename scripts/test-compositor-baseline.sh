#!/usr/bin/env bash
# Fast policy regressions, repeated with libtest concurrency before GPU tests.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

features=${1:-backend-x11rb}
repeats=${JWM_BASELINE_REPEATS:-5}
if [[ ! $repeats =~ ^[1-9][0-9]*$ ]] || ((repeats > 100)); then
    printf 'JWM_BASELINE_REPEATS must be an integer from 1 to 100\n' >&2
    exit 2
fi
for ((run = 1; run <= repeats; run++)); do
    printf 'Compositor baseline %s/%s (%s)\n' "$run" "$repeats" "$features"
    scripts/test.sh --lib --no-default-features --features "$features" -- \
        statusbar::tests \
        queued_swipe_cannot_act_after_lock_or_configuration_change \
        gesture_swipe::tests \
        tag_manager::tests \
        --test-threads=8
done
