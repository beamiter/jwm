#!/usr/bin/env bash
set -euo pipefail

runtime_dir="${XDG_RUNTIME_DIR:-/tmp/jwm-$(id -u)}"
state="$runtime_dir/jwm-video-demo-recovery.json"
if [[ ! -f "$state" ]]; then
  echo "No JWM video recovery state found."
  exit 0
fi

if command -v jq >/dev/null 2>&1; then
  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    if [[ -r "/proc/$pid/comm" ]] && [[ "$(<"/proc/$pid/comm")" == "jwm-demo-client" ]]; then
      kill "$pid" 2>/dev/null || true
    fi
  done < <(jq -r '.demo_pids[]?' "$state")
  original_tag="$(jq -r '.original_tag // empty' "$state")"
  original_layout="$(jq -r '.original_layout // empty' "$state")"
else
  echo "jq is unavailable; stopping recording only. Config backups are preserved in the run directory." >&2
  original_tag=""
  original_layout=""
fi

jwm-tool msg stop_recording >/dev/null 2>&1 || true
jwm-tool msg reload_config >/dev/null 2>&1 || true
if [[ -n "$original_tag" ]]; then jwm-tool msg view --args "{\"tag\":$original_tag}" >/dev/null 2>&1 || true; fi
if [[ -n "$original_layout" ]]; then jwm-tool msg setlayout --args "{\"layout\":\"$original_layout\"}" >/dev/null 2>&1 || true; fi
rm -f "$state" "$runtime_dir/jwm-video-demo.lock"
echo "JWM video session recovery completed."
