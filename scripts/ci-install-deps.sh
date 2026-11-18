#!/usr/bin/env bash
# Only the runner's existing Ubuntu sources participate in these commands.
# Unrelated preinstalled vendor feeds must not gate JWM builds. Per-request
# limits reduce mirror stalls; the workflow's 10-minute step timeout also bounds
# DNS resolution and package-manager work that these HTTP limits cannot bound.
set -euo pipefail

if (($# == 0)); then
    printf 'ci-install-deps: at least one package is required\n' >&2
    exit 2
fi

# Command-scoped selection: do not remove/edit the runner's system sources,
# change its mirror/suites/signing keys, or relax apt's authentication checks.
# The override permits isolated offline fixtures without touching /etc.
ubuntu_sources=${JWM_CI_APT_SOURCES:-/etc/apt/sources.list.d/ubuntu.sources}
if [[ ! -r "$ubuntu_sources" || ! -s "$ubuntu_sources" ]]; then
    printf 'ci-install-deps: missing or empty Ubuntu source file: %s\n' "$ubuntu_sources" >&2
    exit 2
fi
apt_options=(
    -o "Dir::Etc::sourcelist=$ubuntu_sources"
    -o Dir::Etc::sourceparts=-
    -o Acquire::http::Timeout=30
    -o Acquire::https::Timeout=30
    -o Acquire::Retries=2
    -o DPkg::Lock::Timeout=60
)
# A partial update (including an unreachable repository) must not quietly
# succeed with stale indexes and allow the build to continue.
sudo apt-get "${apt_options[@]}" -o APT::Update::Error-Mode=any update
sudo apt-get "${apt_options[@]}" install -y --no-install-recommends "$@"
