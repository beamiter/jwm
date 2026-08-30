#!/usr/bin/env bash
# Keep every executable Bash helper parseable and ShellCheck-clean. Discovery
# is automatic so new scripts cannot silently fall outside the CI gate. Enable
# masked-return checks to catch assertion-shaped `! command` mistakes; SC2312
# is excluded because extraction/formatting substitutions are validated by the
# metrics schema and their dedicated offline contract suite.
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$REPO_ROOT"

declare -a bash_scripts=()
script_list=$(mktemp "${TMPDIR:-/tmp}/jwm-bash-scripts.XXXXXX")
trap 'rm -f -- "$script_list"' EXIT

if ! find . \
    -type d \( -name .git -o -name target -o -name node_modules \) -prune -o \
    -type f -perm -u=x -print0 |
    sort -z > "$script_list"; then
    printf 'lint-shell: executable-script discovery failed\n' >&2
    exit 1
fi

while IFS= read -r -d '' script; do
    IFS= read -r shebang < "$script" || true
    [[ $shebang == '#!'*bash* ]] || continue
    bash_scripts+=("$script")
done < "$script_list"

((${#bash_scripts[@]} > 0)) || {
    printf 'lint-shell: no executable Bash scripts found\n' >&2
    exit 1
}

shellcheck --enable=check-extra-masked-returns --exclude=SC2312 \
    "${bash_scripts[@]}"
