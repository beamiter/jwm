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
# The two CI opt-ins deliberately use different caches and must not be mixed.
bridge_retry=${JWM_CI_BRIDGE_DOWNLOAD_RETRY:-0}
if [[ $bridge_retry == 1 && ${JWM_CI_PREFETCH_CJK:-0} == 1 ]]; then
    printf 'ci-install-deps: Bridge download retry cannot be combined with CJK prefetch\n' >&2
    exit 2
fi

# A partial update (including an unreachable repository) must not quietly
# succeed with stale indexes and allow the build to continue.
sudo apt-get "${apt_options[@]}" -o APT::Update::Error-Mode=any update
# Bridge's mpv/ffmpeg dependency closure can exceed the workflow deadline
# while the signed indexes are healthy. Retry only a timed-out transfer, not
# a package-manager error or dpkg. Both attempts use apt and the same source
# selection, proxy, signatures, and private archive directory.
if [[ $bridge_retry == 1 ]]; then
    archives=$(sudo -n /usr/bin/mktemp -d -- /var/cache/apt/archives/jwm-bridge-ci.XXXXXXXX)
    if [[ ! $archives =~ ^/var/cache/apt/archives/jwm-bridge-ci\.[a-zA-Z0-9]{8}$ ]]; then
        printf 'ci-install-deps: unexpected private archive path\n' >&2
        exit 1
    fi
    # The parent and this fresh directory are owned by root. Let apt's _apt
    # user traverse the directory to its own partial subdirectory, but not
    # rewrite the verified cache. No caller-controlled archive path is used.
    # Invoked by the EXIT trap below, not by a direct shell call.
    # shellcheck disable=SC2329
    cleanup_bridge_archives() {
        local status=$?
        trap - EXIT
        if ! sudo -n /usr/bin/rm -rf -- "$archives"; then
            printf 'ci-install-deps: could not remove private Bridge archives\n' >&2
            if ((status == 0)); then
                status=1
            fi
        fi
        exit "$status"
    }
    trap cleanup_bridge_archives EXIT
    sudo -n /usr/bin/chmod 0755 -- "$archives"
    apt_options+=(-o "Dir::Cache::archives=$archives")
    install_args=(install -y --no-install-recommends "$@")
    if sudo -n /usr/bin/timeout --kill-after=5s 120s apt-get "${apt_options[@]}" \
        --download-only "${install_args[@]}"; then
        :
    else
        rc=$?
        if ((rc != 124)); then
            exit "$rc"
        fi
        printf 'ci-install-deps: Bridge download reached 120s; retrying once from the same sources\n' >&2
        # A slow mirror can still be delivering valid packages. Leave the
        # retry six minutes while keeping both transfers below the 10-minute
        # workflow step budget, with room for update and offline install.
        sudo -n /usr/bin/timeout --kill-after=5s 360s apt-get "${apt_options[@]}" \
            --download-only "${install_args[@]}"
    fi
    sudo -n apt-get "${apt_options[@]}" --no-download "${install_args[@]}"
    exit
fi
# Only the native UI job opts in; all other callers keep the original path.
if [[ ${JWM_CI_PREFETCH_CJK:-0} == 1 ]]; then
    sudo /usr/bin/python3 "$(dirname "$0")/ci-prefetch-cjk.py" "${apt_options[@]}"
fi
sudo apt-get "${apt_options[@]}" install -y --no-install-recommends "$@"
