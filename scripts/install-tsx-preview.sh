#!/usr/bin/env bash
# Install the launcher only; never installs or executes project dependencies.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
[[ $# == 0 ]] || { echo 'Usage: bash scripts/install-tsx-preview.sh' >&2; exit 2; }
bin="$HOME/.local/bin"
mkdir -p -- "$bin"
temporary=$(mktemp "$bin/.jwm-tsx-preview.XXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
install -m755 "$ROOT/scripts/jwm-tsx-preview.mjs" "$temporary"
mv -f -- "$temporary" "$bin/jwm-tsx-preview"
printf 'Installed %s/jwm-tsx-preview. Requires Node and project-local Vite/React; installs nothing else.\n' "$bin"
