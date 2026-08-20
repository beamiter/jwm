#!/usr/bin/env bash
# Keep every executable Bash helper parseable and free of high-signal
# ShellCheck warnings. Discovery is automatic so new scripts cannot silently
# fall outside the CI gate.
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$REPO_ROOT"

declare -a bash_scripts=()

while IFS= read -r -d '' script; do
    IFS= read -r shebang < "$script" || true
    [[ $shebang == '#!'*bash* ]] || continue
    bash_scripts+=("$script")
done < <(find scripts -type f -perm -u=x -print0 | sort -z)

((${#bash_scripts[@]} > 0)) || {
    printf 'lint-shell: no executable Bash scripts found\n' >&2
    exit 1
}

shellcheck --severity=warning "${bash_scripts[@]}"
