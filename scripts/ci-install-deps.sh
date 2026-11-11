#!/usr/bin/env bash
# CI runners keep their configured official Ubuntu repositories. Per-request
# limits reduce mirror stalls; the workflow's 10-minute step timeout also bounds
# DNS resolution and package-manager work that these HTTP limits cannot bound.
set -euo pipefail

if (($# == 0)); then
    printf 'ci-install-deps: at least one package is required\n' >&2
    exit 2
fi

apt_options=(
    -o Acquire::http::Timeout=30
    -o Acquire::https::Timeout=30
    -o Acquire::Retries=2
    -o DPkg::Lock::Timeout=60
)
# A partial update (including an unreachable repository) must not quietly
# succeed with stale indexes and allow the build to continue.
sudo apt-get "${apt_options[@]}" -o APT::Update::Error-Mode=any update
sudo apt-get "${apt_options[@]}" install -y --no-install-recommends "$@"
