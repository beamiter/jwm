#!/usr/bin/env bash
# Exercise the metrics CLI tools without a running compositor.
set -Eeuo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/jwm-metrics-tools.XXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT

mkdir -p -- "$TEST_ROOT/bin"
cat > "$TEST_ROOT/bin/jwm-tool" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${JWM_TEST_EXIT:-0} != 0 ]]; then
    exit "$JWM_TEST_EXIT"
fi
printf '%s\n' "${JWM_TEST_RESPONSE-}"
MOCK
chmod +x "$TEST_ROOT/bin/jwm-tool"
export PATH="$TEST_ROOT/bin:$PATH"

valid_response='{"success":true,"data":{"fps":60.5,"frame_count":100,"avg_frame_time_ms":16.5,"max_frame_time_ms":20,"min_frame_time_ms":14,"gpu_load_percent":25,"cpu_load_percent":15,"draw_calls":3,"texture_memory_bytes":2048,"blur_cache_hits":30,"blur_cache_misses":10,"blur_cache_hit_rate":75,"temporal_blur_reuse_count":5,"temporal_blur_total_count":10,"temporal_blur_reuse_rate":50,"dirty_regions_count":1,"dirty_fraction_percent":10,"window_count":2,"blur_quality":"高质量 </script>","vrr_enabled":true,"vrr_active":false,"current_refresh_rate":60,"input_latency_avg_ms":8,"input_latency_p50_ms":7,"input_latency_p95_ms":12,"input_latency_p99_ms":18}}'

# Snapshot, equality, float formatting, and the finite monitor path.
JWM_TEST_RESPONSE=$valid_response \
    "$REPO_ROOT/tools/compare_metrics.sh" save "$TEST_ROOT/current.json" >/dev/null
jq -e '.fps == 60.5 and .window_count == 2' "$TEST_ROOT/current.json" >/dev/null
"$REPO_ROOT/tools/compare_metrics.sh" compare \
    "$TEST_ROOT/current.json" "$TEST_ROOT/current.json" > "$TEST_ROOT/equal.txt"
equal_count=$(grep -c '→ .*持平' "$TEST_ROOT/equal.txt")
[[ $equal_count == 4 ]]
if grep -q '✗' "$TEST_ROOT/equal.txt"; then
    echo "equal metrics were reported as a regression" >&2
    exit 1
fi
JWM_TEST_RESPONSE=$valid_response JWM_METRICS_INTERVAL=0 JWM_MONITOR_ITERATIONS=1 \
    "$REPO_ROOT/tools/compare_metrics.sh" monitor > "$TEST_ROOT/monitor.txt"
monitor_count=$(grep -c '→ 持平' "$TEST_ROOT/monitor.txt")
[[ $monitor_count == 3 ]]

# Invalid response envelopes and fractional counters must fail closed.
invalid_response='{"success":true,"data":{}}'
if JWM_TEST_RESPONSE=$invalid_response \
    "$REPO_ROOT/tools/compare_metrics.sh" save "$TEST_ROOT/invalid.json" \
    >/dev/null 2>&1; then
    echo "compare accepted an incomplete metrics object" >&2
    exit 1
fi
[[ ! -e $TEST_ROOT/invalid.json ]]
jq '.window_count = 2.5' "$TEST_ROOT/current.json" > "$TEST_ROOT/fractional.json"
if "$REPO_ROOT/tools/compare_metrics.sh" compare \
    "$TEST_ROOT/current.json" "$TEST_ROOT/fractional.json" >/dev/null 2>&1; then
    echo "compare accepted a fractional counter" >&2
    exit 1
fi

# Dashboard bars accept floats and summary statuses are independent.
status_response=$(jq -c '
    .data.gpu_load_percent = 90
    | .data.cpu_load_percent = 10
    | .data.input_latency_avg_ms = 35
' <<<"$valid_response")
JWM_TEST_RESPONSE=$status_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single --quick > "$TEST_ROOT/dashboard.txt"
grep -Eq 'GPU 负载.*🔴 90%' "$TEST_ROOT/dashboard.txt"
grep -Eq 'CPU 负载.*🟢 10%' "$TEST_ROOT/dashboard.txt"
grep -Eq '输入延迟.*🔴 35\.00 ms' "$TEST_ROOT/dashboard.txt"
float_response=$(jq -c '.data.gpu_load_percent = 12.5' <<<"$valid_response")
JWM_TEST_RESPONSE=$float_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single --load > "$TEST_ROOT/float.txt"
grep -q '\[===-----------------\]  12.5%' "$TEST_ROOT/float.txt"
hostile_quality_response=$(jq -c '
    .data.blur_quality =
        ("高\u001b[31m质量\nNEXT\u202e" + ("x" * 100))
' <<<"$valid_response")
JWM_TEST_RESPONSE=$hostile_quality_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single --full \
    >"$TEST_ROOT/hostile-dashboard.txt"
blur_quality_line=$(grep 'Blur 质量' "$TEST_ROOT/hostile-dashboard.txt")
[[ $blur_quality_line == *NEXT* ]]
if [[ $blur_quality_line == *$'\033'* || $blur_quality_line == *$'\u202e'* ]]; then
    echo "dashboard preserved terminal or bidi control characters" >&2
    exit 1
fi
blur_quality_value=${blur_quality_line#*: }
if ((${#blur_quality_value} > 80)); then
    echo "dashboard did not bound the displayed blur quality" >&2
    exit 1
fi
JWM_TEST_RESPONSE=$valid_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --export "$TEST_ROOT/export.json" >/dev/null
jq -e '.blur_quality == "高质量 </script>"' "$TEST_ROOT/export.json" >/dev/null
if JWM_TEST_RESPONSE=$invalid_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single >/dev/null 2>&1; then
    echo "dashboard accepted an incomplete metrics object" >&2
    exit 1
fi
if "$REPO_ROOT/tools/metrics_dashboard.sh" --export >/dev/null 2>&1; then
    echo "dashboard accepted a missing export path" >&2
    exit 1
fi
if "$REPO_ROOT/tools/metrics_dashboard.sh" --interval 0 >/dev/null 2>&1; then
    echo "dashboard accepted a zero refresh interval" >&2
    exit 1
fi
if env -u TERM JWM_TEST_RESPONSE="$valid_response" timeout 0.3 \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --interval 0.05 \
    >"$TEST_ROOT/headless.txt" 2>"$TEST_ROOT/headless.err"; then
    echo "real-time dashboard unexpectedly terminated by itself" >&2
    exit 1
else
    headless_status=$?
    if ((headless_status != 124)); then
        echo "headless dashboard failed before timeout: $headless_status" >&2
        exit 1
    fi
fi
[[ ! -s $TEST_ROOT/headless.err ]]

fractional_refresh=$(jq -c '.data.current_refresh_rate = 144.5' \
    <<<"$valid_response")
if JWM_TEST_RESPONSE=$fractional_refresh \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single >/dev/null 2>&1; then
    echo "dashboard accepted a fractional refresh-rate counter" >&2
    exit 1
fi
false_response=$(jq -c '.success = false' <<<"$valid_response")
if JWM_TEST_RESPONSE=$false_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single >/dev/null 2>&1; then
    echo "dashboard accepted success=false with otherwise valid data" >&2
    exit 1
fi
infinite_response=${valid_response/\"fps\":60.5/\"fps\":1e999}
if JWM_TEST_RESPONSE=$infinite_response \
    "$REPO_ROOT/tools/metrics_dashboard.sh" --single >/dev/null 2>&1; then
    echo "dashboard accepted a non-finite metric" >&2
    exit 1
fi

# Reports stay offline, preserve UTF-8 safely, and consume sampled averages.
mkdir -p -- "$TEST_ROOT/report"
JWM_TEST_RESPONSE=$valid_response JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/report" >/dev/null
report=$(find "$TEST_ROOT/report" -maxdepth 1 -name '*.html' -print -quit)
[[ -n $report ]]
grep -q 'TextDecoder' "$report"
if grep -Eq 'JSON\.parse\(atob|cdn\.jsdelivr|summary\.innerHTML' "$report"; then
    echo "report contains an unsafe or online-only implementation" >&2
    exit 1
fi
if grep -Fq '高质量 </script>' "$report"; then
    echo "report embedded an untrusted string outside its encoded payload" >&2
    exit 1
fi
payload=$(sed -n "s/^[[:space:]]*'\([A-Za-z0-9+\/=]*\)';$/\1/p" "$report")
[[ -n $payload ]]
printf '%s' "$payload" | base64 --decode |
    jq -e '
        .blur_quality == "高质量 </script>"
        and .sampled_avg_fps == 60.5
        and .sampled_avg_frame_time_ms == 16.5
    ' >/dev/null

# jq/JSON always emits dot-decimal numbers. If this host provides a locale
# whose decimal separator is a comma, prove every entry point remains exact
# even when LC_ALL would otherwise make Bash printf reject those values.
comma_locale=
locale_list="$TEST_ROOT/locales.list"
if command -v locale >/dev/null 2>&1 && locale -a > "$locale_list" 2>/dev/null; then
    while IFS= read -r candidate_locale; do
        candidate_decimal=$(LC_ALL=$candidate_locale locale decimal_point \
            2>/dev/null) || continue
        if [[ $candidate_decimal == , ]]; then
            comma_locale=$candidate_locale
            break
        fi
    done < "$locale_list"
fi
if [[ -n $comma_locale ]]; then
    LC_ALL=$comma_locale JWM_TEST_RESPONSE=$valid_response \
        "$REPO_ROOT/tools/metrics_dashboard.sh" --single --quick \
        > "$TEST_ROOT/locale-dashboard.txt" \
        2> "$TEST_ROOT/locale-dashboard.err"
    [[ ! -s $TEST_ROOT/locale-dashboard.err ]]
    grep -Fq '60.5 fps' "$TEST_ROOT/locale-dashboard.txt"

    LC_ALL=$comma_locale "$REPO_ROOT/tools/compare_metrics.sh" compare \
        "$TEST_ROOT/current.json" "$TEST_ROOT/current.json" \
        > "$TEST_ROOT/locale-compare.txt" \
        2> "$TEST_ROOT/locale-compare.err"
    [[ ! -s $TEST_ROOT/locale-compare.err ]]
    grep -Eq 'FPS.*60\.5.*60\.5.*\+0' \
        "$TEST_ROOT/locale-compare.txt"

    mkdir -p -- "$TEST_ROOT/locale-report"
    LC_ALL=$comma_locale JWM_TEST_RESPONSE=$valid_response \
        JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
        "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/locale-report" \
        > "$TEST_ROOT/locale-report.txt" \
        2> "$TEST_ROOT/locale-report.err"
    if grep -q 'invalid number' "$TEST_ROOT/locale-report.err"; then
        echo "report generator parsed dot-decimal JSON with the caller locale" >&2
        exit 1
    fi
    locale_report=$(find "$TEST_ROOT/locale-report" -maxdepth 1 \
        -name '*.html' -type f -print -quit)
    [[ -n $locale_report ]]
    locale_payload=$(sed -n \
        "s/^[[:space:]]*'\([A-Za-z0-9+\/=]*\)';$/\1/p" "$locale_report")
    printf '%s' "$locale_payload" | base64 --decode |
        jq -e '
            .sampled_avg_fps == 60.5
            and .sampled_avg_frame_time_ms == 16.5
        ' >/dev/null
fi

# jq-based aggregation supports valid JSON scientific notation that bc rejects.
scientific_response=$(jq -c '.data.fps = 1e-7 | .data.avg_frame_time_ms = 2e-7' \
    <<<"$valid_response")
mkdir -p -- "$TEST_ROOT/scientific"
JWM_TEST_RESPONSE=$scientific_response JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/scientific" >/dev/null
scientific_report=$(find "$TEST_ROOT/scientific" -maxdepth 1 -name '*.html' -print -quit)
scientific_payload=$(sed -n "s/^[[:space:]]*'\([A-Za-z0-9+\/=]*\)';$/\1/p" \
    "$scientific_report")
printf '%s' "$scientific_payload" | base64 --decode |
    jq -e '.sampled_avg_fps == 1e-7 and .sampled_avg_frame_time_ms == 2e-7' \
    >/dev/null

# Comparison arithmetic must also accept jq's scientific-notation rendering.
JWM_TEST_RESPONSE=$scientific_response \
    "$REPO_ROOT/tools/compare_metrics.sh" save \
    "$TEST_ROOT/scientific-baseline.json" >/dev/null
scientific_current_response=$(jq -c '.data.fps = 2e-7' \
    <<<"$scientific_response")
JWM_TEST_RESPONSE=$scientific_current_response \
    "$REPO_ROOT/tools/compare_metrics.sh" save \
    "$TEST_ROOT/scientific-current.json" >/dev/null
"$REPO_ROOT/tools/compare_metrics.sh" compare \
    "$TEST_ROOT/scientific-baseline.json" \
    "$TEST_ROOT/scientific-current.json" \
    >"$TEST_ROOT/scientific-compare.txt" \
    2>"$TEST_ROOT/scientific-compare.err"
[[ ! -s $TEST_ROOT/scientific-compare.err ]]
grep -Eq '变化:.*\(100(\.0+)?%\)' "$TEST_ROOT/scientific-compare.txt"
grep -q '✓ 性能改善' "$TEST_ROOT/scientific-compare.txt"

# A zero FPS baseline has no finite interval or percentage denominator.
jq '.fps = 0' "$TEST_ROOT/current.json" > "$TEST_ROOT/zero-fps.json"
"$REPO_ROOT/tools/compare_metrics.sh" compare \
    "$TEST_ROOT/zero-fps.json" "$TEST_ROOT/current.json" \
    > "$TEST_ROOT/zero-fps-compare.txt"
grep -q '变化:.*(N/A)' "$TEST_ROOT/zero-fps-compare.txt"
grep -Eq '理论帧间隔.*N/A.*N/A' "$TEST_ROOT/zero-fps-compare.txt"

# Concurrent generators reserve independent names and publish atomically.
mkdir -p -- "$TEST_ROOT/concurrent"
JWM_TEST_RESPONSE=$valid_response JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/concurrent" \
    > "$TEST_ROOT/concurrent-1.txt" &
report_pid_1=$!
JWM_TEST_RESPONSE=$valid_response JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/concurrent" \
    > "$TEST_ROOT/concurrent-2.txt" &
report_pid_2=$!
wait "$report_pid_1"
wait "$report_pid_2"
concurrent_html_list="$TEST_ROOT/concurrent-html.list"
if ! find "$TEST_ROOT/concurrent" -maxdepth 1 -name '*.html' -type f \
    -print > "$concurrent_html_list"; then
    echo "could not enumerate concurrent reports" >&2
    exit 1
fi
concurrent_html_count=$(wc -l < "$concurrent_html_list")
[[ $concurrent_html_count == 2 ]]
concurrent_temp_list="$TEST_ROOT/concurrent-temp.list"
if ! find "$TEST_ROOT/concurrent" -maxdepth 1 \
    -name '.jwm_performance_report.*' -print -quit \
    > "$concurrent_temp_list"; then
    echo "could not enumerate concurrent report temporary files" >&2
    exit 1
fi
if [[ -s $concurrent_temp_list ]]; then
    echo "concurrent report generation left a temporary file" >&2
    exit 1
fi

mkdir -p -- "$TEST_ROOT/invalid-report"
if JWM_TEST_RESPONSE=$invalid_response JWM_REPORT_SAMPLES=1 JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/invalid-report" \
    >/dev/null 2>&1; then
    echo "report generator accepted an incomplete metrics object" >&2
    exit 1
fi
invalid_html_list="$TEST_ROOT/invalid-html.list"
if ! find "$TEST_ROOT/invalid-report" -maxdepth 1 -name '*.html' -print -quit \
    > "$invalid_html_list"; then
    echo "could not enumerate invalid-input reports" >&2
    exit 1
fi
if [[ -s $invalid_html_list ]]; then
    echo "invalid report input still produced an HTML file" >&2
    exit 1
fi

# A destination directory created just before publication must make the
# atomic rename fail, not absorb the temporary file and produce a false
# success message. The mock races at the exact mv boundary deterministically.
mkdir -p -- "$TEST_ROOT/report-race/bin" "$TEST_ROOT/report-race/output"
real_mv=$(command -v mv)
cat > "$TEST_ROOT/report-race/bin/mv" <<'MOCK_MV'
#!/usr/bin/env bash
set -Eeuo pipefail
target=${!#}
mkdir -- "$target"
exec "$JWM_TEST_REAL_MV" "$@"
MOCK_MV
chmod +x "$TEST_ROOT/report-race/bin/mv"
if PATH="$TEST_ROOT/report-race/bin:$PATH" JWM_TEST_REAL_MV=$real_mv \
    JWM_TEST_RESPONSE=$valid_response JWM_REPORT_SAMPLES=1 \
    JWM_REPORT_INTERVAL=0 \
    "$REPO_ROOT/tools/generate_report.sh" "$TEST_ROOT/report-race/output" \
    > "$TEST_ROOT/report-race/stdout" 2> "$TEST_ROOT/report-race/stderr"; then
    echo "report publication accepted a destination-directory race" >&2
    exit 1
fi
if grep -q '报告已生成' "$TEST_ROOT/report-race/stdout"; then
    echo "failed report publication claimed success" >&2
    exit 1
fi
race_html_list="$TEST_ROOT/report-race-html.list"
if ! find "$TEST_ROOT/report-race/output" -maxdepth 1 -name '*.html' -type f \
    -print > "$race_html_list"; then
    echo "could not enumerate raced reports" >&2
    exit 1
fi
[[ ! -s $race_html_list ]]
race_temp_list="$TEST_ROOT/report-race-temp.list"
if ! find "$TEST_ROOT/report-race/output" -maxdepth 1 \
    -name '.jwm_performance_report.*' -print -quit > "$race_temp_list"; then
    echo "could not enumerate raced report temporary files" >&2
    exit 1
fi
[[ ! -s $race_temp_list ]]

echo "metrics-tools: offline contracts passed"
