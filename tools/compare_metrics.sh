#!/usr/bin/env bash
# JWM Metrics Comparison Tool - 性能数据对比分析

set -Eeuo pipefail

# JSON numbers always use a dot. LC_ALL would override LC_NUMERIC, so preserve
# its nonnumeric categories before making Bash printf parse jq deterministically.
if [[ -n ${LC_ALL:-} ]]; then
    export LC_CTYPE=$LC_ALL
    export LC_COLLATE=$LC_ALL
    export LC_TIME=$LC_ALL
    export LC_MONETARY=$LC_ALL
    export LC_MESSAGES=$LC_ALL
    unset LC_ALL
fi
export LC_NUMERIC=C

# 获取当前指标
get_metrics() {
    local metrics
    metrics="$(jwm-tool msg get_metrics --raw 2>/dev/null |
        jq -e '
            select(type == "object" and .success == true)
            | .data
            | select(type == "object")
            | select([.. | numbers] | all(.[]; isfinite))
        ')" || return 1
    [[ -n $metrics ]] || return 1
    printf '%s\n' "$metrics"
}

require_commands() {
    local command
    for command in "$@"; do
        command -v "$command" >/dev/null 2>&1 || {
            echo "❌ 缺少必需命令: $command" >&2
            return 1
        }
    done
}

validate_metrics() {
    jq -e '
        def finite_nonnegative:
            type == "number" and . >= 0 and . <= 1.7976931348623157e308;
        def percentage: finite_nonnegative and . <= 100;

        type == "object"
        and ([
            .fps, .avg_frame_time_ms, .max_frame_time_ms,
            .input_latency_avg_ms, .input_latency_p95_ms
        ] | all(.[]; finite_nonnegative))
        and ([
            .gpu_load_percent, .cpu_load_percent,
            .blur_cache_hit_rate, .dirty_fraction_percent
        ] | all(.[]; percentage))
        and (.window_count | finite_nonnegative and floor == .)
    ' >/dev/null <<<"$1"
}

subtract_numbers() {
    if [[ $1 == N/A || $2 == N/A ]]; then
        printf 'N/A\n'
    else
        jq -enr --argjson left "$1" --argjson right "$2" '$left - $right'
    fi
}

percent_change() {
    local change=$1 baseline=$2
    jq -enr --argjson change "$change" --argjson baseline "$baseline" '
        if $baseline == 0 then
            if $change == 0 then 0 else "N/A" end
        else
            $change / $baseline * 100
        end
    '
}

format_number() {
    if [[ $1 == N/A ]]; then
        printf 'N/A\n'
    else
        printf '%.6g\n' "$1"
    fi
}

format_signed_number() {
    if [[ $1 == N/A ]]; then
        printf 'N/A\n'
    else
        printf '%+.6g\n' "$1"
    fi
}

format_percent() {
    if [[ $1 == N/A ]]; then
        printf 'N/A\n'
    else
        printf '%.6g%%\n' "$1"
    fi
}

format_signed_percent() {
    if [[ $1 == N/A ]]; then
        printf 'N/A\n'
    else
        printf '%+.6g%%\n' "$1"
    fi
}

trend_label() {
    local change=$1 better=$2 worse=$3 equal=$4
    if jq -en --argjson change "$change" '$change > 0' >/dev/null; then
        printf '%s\n' "$better"
    elif jq -en --argjson change "$change" '$change < 0' >/dev/null; then
        printf '%s\n' "$worse"
    else
        printf '%s\n' "$equal"
    fi
}

frame_interval() {
    local fps=$1
    jq -enr --argjson fps "$fps" '
        if $fps == 0 then "N/A" else 1000 / $fps end
    '
}

# 保存指标快照
save_metrics() {
    local file=$1
    local m
    require_commands jwm-tool jq
    m="$(get_metrics)" || {
        echo "❌ 无法从正在运行的 JWM 获取指标" >&2
        return 1
    }
    validate_metrics "$m" || {
        echo "❌ JWM 返回了无效指标" >&2
        return 1
    }
    jq '.' <<<"$m" > "$file"
    echo "✓ 指标已保存到: $file"
}

# 性能对比
compare_metrics() {
    local baseline_file=$1
    local current_file=$2
    local baseline current
    local baseline_fps baseline_avg baseline_max baseline_latency baseline_p95
    local baseline_gpu baseline_cpu baseline_blur baseline_windows baseline_dirty
    local current_fps current_avg current_max current_latency current_p95
    local current_gpu current_cpu current_blur current_windows current_dirty
    local fps_delta latency_delta gpu_delta blur_delta
    local fps_change latency_change gpu_change blur_change
    local fps_pct latency_pct gpu_pct blur_pct
    local fps_label latency_label gpu_label blur_label
    local baseline_interval current_interval
    local interval_delta max_delta avg_delta latency_detail_delta p95_delta
    local gpu_detail_delta cpu_delta window_delta dirty_delta

    require_commands jq

    if [ ! -f "$baseline_file" ] || [ ! -f "$current_file" ]; then
        echo "❌ 错误：基线文件或当前文件不存在"
        echo "使用: $0 save <baseline-file>"
        echo "      $0 compare <baseline-file> <current-file>"
        return 1
    fi

    baseline="$(<"$baseline_file")"
    current="$(<"$current_file")"
    if ! validate_metrics "$baseline" || ! validate_metrics "$current"; then
        echo "❌ 指标文件缺少必需的数值字段或不是有效 JSON" >&2
        return 1
    fi

    IFS=$'\t' read -r baseline_fps baseline_avg baseline_max \
        baseline_latency baseline_p95 baseline_gpu baseline_cpu baseline_blur \
        baseline_windows baseline_dirty < <(
        jq -r '[
            .fps, .avg_frame_time_ms, .max_frame_time_ms,
            .input_latency_avg_ms, .input_latency_p95_ms,
            .gpu_load_percent, .cpu_load_percent, .blur_cache_hit_rate,
            .window_count, .dirty_fraction_percent
        ] | @tsv' <<<"$baseline"
    )
    IFS=$'\t' read -r current_fps current_avg current_max \
        current_latency current_p95 current_gpu current_cpu current_blur \
        current_windows current_dirty < <(
        jq -r '[
            .fps, .avg_frame_time_ms, .max_frame_time_ms,
            .input_latency_avg_ms, .input_latency_p95_ms,
            .gpu_load_percent, .cpu_load_percent, .blur_cache_hit_rate,
            .window_count, .dirty_fraction_percent
        ] | @tsv' <<<"$current"
    )

    # 计算变化
    fps_delta="$(subtract_numbers "$current_fps" "$baseline_fps")" || return 1
    latency_delta="$(subtract_numbers "$baseline_latency" "$current_latency")" || return 1
    gpu_delta="$(subtract_numbers "$baseline_gpu" "$current_gpu")" || return 1
    blur_delta="$(subtract_numbers "$current_blur" "$baseline_blur")" || return 1
    fps_change="$(format_number "$fps_delta")"
    latency_change="$(format_number "$latency_delta")"
    gpu_change="$(format_number "$gpu_delta")"
    blur_change="$(format_number "$blur_delta")"

    # 计算百分比变化
    fps_pct="$(percent_change "$fps_delta" "$baseline_fps")" || return 1
    latency_pct="$(percent_change "$latency_delta" "$baseline_latency")" || return 1
    gpu_pct="$(percent_change "$gpu_delta" "$baseline_gpu")" || return 1
    blur_pct="$(percent_change "$blur_delta" "$baseline_blur")" || return 1
    fps_pct="$(format_percent "$fps_pct")"
    latency_pct="$(format_percent "$latency_pct")"
    gpu_pct="$(format_percent "$gpu_pct")"
    blur_pct="$(format_percent "$blur_pct")"
    fps_label="$(trend_label "$fps_delta" '✓ 性能改善' '✗ 性能下降' '→ 性能持平')" || return 1
    latency_label="$(trend_label "$latency_delta" '✓ 延迟降低' '✗ 延迟增加' '→ 延迟持平')" || return 1
    gpu_label="$(trend_label "$gpu_delta" '✓ 负载降低' '✗ 负载增加' '→ 负载持平')" || return 1
    blur_label="$(trend_label "$blur_delta" '✓ 命中率提升' '✗ 命中率下降' '→ 命中率持平')" || return 1
    baseline_interval="$(frame_interval "$baseline_fps")" || return 1
    current_interval="$(frame_interval "$current_fps")" || return 1
    interval_delta="$(subtract_numbers "$current_interval" "$baseline_interval")" || return 1
    max_delta="$(subtract_numbers "$current_max" "$baseline_max")" || return 1
    avg_delta="$(subtract_numbers "$current_avg" "$baseline_avg")" || return 1
    latency_detail_delta="$(subtract_numbers "$current_latency" "$baseline_latency")" || return 1
    p95_delta="$(subtract_numbers "$current_p95" "$baseline_p95")" || return 1
    gpu_detail_delta="$(subtract_numbers "$current_gpu" "$baseline_gpu")" || return 1
    cpu_delta="$(subtract_numbers "$current_cpu" "$baseline_cpu")" || return 1
    window_delta="$(subtract_numbers "$current_windows" "$baseline_windows")" || return 1
    dirty_delta="$(subtract_numbers "$current_dirty" "$baseline_dirty")" || return 1

    # 输出报告
    cat << EOF
╔════════════════════════════════════════════════════════════════════╗
║              JWM 性能对比分析报告                                  ║
╚════════════════════════════════════════════════════════════════════╝

📊 关键指标对比
═══════════════════════════════════════════════════════════════════

【FPS 帧率】
  基线:     $baseline_fps fps
  当前:     $current_fps fps
  变化:     $fps_change fps ($fps_pct)
  $fps_label

【输入延迟】
  基线:     ${baseline_latency}ms
  当前:     ${current_latency}ms
  改善:     $latency_change ms ($latency_pct)
  $latency_label

【GPU 负载】
  基线:     ${baseline_gpu}%
  当前:     ${current_gpu}%
  降低:     $gpu_change% ($gpu_pct)
  $gpu_label

【Blur 缓存命中率】
  基线:     ${baseline_blur}%
  当前:     ${current_blur}%
  提升:     $blur_change% ($blur_pct)
  $blur_label

═══════════════════════════════════════════════════════════════════

📈 详细指标对比表
═══════════════════════════════════════════════════════════════════

EOF

    # 详细对比表
    printf "%-35s | %15s | %15s | %15s\n" "指标" "基线" "当前" "变化"
    printf "%-35s | %15s | %15s | %15s\n" "---" "---" "---" "---"

    printf "%-35s | %15s | %15s | %15s\n" "FPS" \
        "$(format_number "$baseline_fps")" "$(format_number "$current_fps")" \
        "$(format_signed_number "$fps_delta")"
    printf "%-35s | %15s | %15s | %15s\n" "理论帧间隔 (ms)" \
        "$(format_number "$baseline_interval")" \
        "$(format_number "$current_interval")" \
        "$(format_signed_number "$interval_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "最大帧时间 (ms)" \
        "$(format_number "$baseline_max")" "$(format_number "$current_max")" \
        "$(format_signed_number "$max_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "平均帧时间 (ms)" \
        "$(format_number "$baseline_avg")" "$(format_number "$current_avg")" \
        "$(format_signed_number "$avg_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "平均延迟 (ms)" \
        "$(format_number "$baseline_latency")" \
        "$(format_number "$current_latency")" \
        "$(format_signed_number "$latency_detail_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "P95 延迟 (ms)" \
        "$(format_number "$baseline_p95")" "$(format_number "$current_p95")" \
        "$(format_signed_number "$p95_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "GPU 负载" \
        "$(format_percent "$baseline_gpu")" "$(format_percent "$current_gpu")" \
        "$(format_signed_percent "$gpu_detail_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "CPU 负载" \
        "$(format_percent "$baseline_cpu")" "$(format_percent "$current_cpu")" \
        "$(format_signed_percent "$cpu_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "Blur 缓存命中率" \
        "$(format_percent "$baseline_blur")" "$(format_percent "$current_blur")" \
        "$(format_signed_percent "$blur_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "窗口数量" \
        "$(format_number "$baseline_windows")" \
        "$(format_number "$current_windows")" \
        "$(format_signed_number "$window_delta")"

    printf "%-35s | %15s | %15s | %15s\n" "脏区域占比" \
        "$(format_percent "$baseline_dirty")" \
        "$(format_percent "$current_dirty")" \
        "$(format_signed_percent "$dirty_delta")"

    echo ""
    echo "═══════════════════════════════════════════════════════════════════"
    echo ""
}

# 帮助
show_help() {
    cat << EOF
用法: $0 <command> [args...]

命令:
    save <file>              保存当前指标快照
    compare <baseline> <current>  对比两个指标快照
    monitor                  实时监控性能变化
    -h, --help              显示此帮助

示例:
    # 保存基线
    $0 save baseline_before.json

    # 进行优化...

    # 保存优化后的指标
    $0 save current_after.json

    # 对比结果
    $0 compare baseline_before.json current_after.json
EOF
}

# 实时监控变化
monitor_changes() {
    local interval=${JWM_METRICS_INTERVAL:-1}
    local limit=${JWM_MONITOR_ITERATIONS:-0}
    local iteration=0
    local first current first_fps first_gpu first_latency
    local current_fps current_gpu current_latency
    local fps_diff gpu_improvement latency_improvement
    local fps_marker gpu_marker latency_marker

    require_commands jwm-tool jq
    [[ $interval =~ ^[0-9]+([.][0-9]+)?$ ]] || {
        echo "JWM_METRICS_INTERVAL must be a non-negative number." >&2
        return 2
    }
    [[ $limit =~ ^[0-9]+$ ]] || {
        echo "JWM_MONITOR_ITERATIONS must be a non-negative integer." >&2
        return 2
    }

    echo "📊 实时性能监控 - 按 Ctrl+C 退出"
    echo "将显示相对于首次快照的变化"
    echo ""

    # 获取初始快照
    first="$(get_metrics)" || {
        echo "❌ 无法从正在运行的 JWM 获取指标" >&2
        return 1
    }
    validate_metrics "$first" || {
        echo "❌ JWM 返回了无效指标" >&2
        return 1
    }
    IFS=$'\t' read -r first_fps first_gpu first_latency < <(
        jq -r '[.fps, .gpu_load_percent, .input_latency_avg_ms] | @tsv' <<<"$first"
    )

    while ((limit == 0 || iteration < limit)); do
        sleep "$interval"
        if [[ -t 1 ]]; then
            # Do not depend on TERM/terminfo: a pseudo-terminal may be present
            # even when launchers deliberately omit TERM.
            printf '\033[2J\033[H'
        fi

        current="$(get_metrics)" || {
            echo "❌ 无法从正在运行的 JWM 获取指标" >&2
            return 1
        }
        validate_metrics "$current" || {
            echo "❌ JWM 返回了无效指标" >&2
            return 1
        }
        IFS=$'\t' read -r current_fps current_gpu current_latency < <(
            jq -r '[.fps, .gpu_load_percent, .input_latency_avg_ms] | @tsv' <<<"$current"
        )

        fps_diff="$(subtract_numbers "$current_fps" "$first_fps")" || return 1
        gpu_improvement="$(subtract_numbers "$first_gpu" "$current_gpu")" || return 1
        latency_improvement="$(subtract_numbers "$first_latency" "$current_latency")" || return 1
        fps_marker="$(trend_label "$fps_diff" "📈 +$fps_diff" "📉 $fps_diff" '→ 持平')" || return 1
        gpu_marker="$(trend_label "$gpu_improvement" "✓ 降低 ${gpu_improvement}%" "✗ 增加 ${gpu_improvement#-}%" '→ 持平')" || return 1
        latency_marker="$(trend_label "$latency_improvement" "✓ 改善 ${latency_improvement}ms" "✗ 恶化 ${latency_improvement#-}ms" '→ 持平')" || return 1

        echo "╔════════════════════════════════════════════════════════════════════╗"
        echo "║           JWM 实时性能监控 - $(date '+%H:%M:%S')                        ║"
        echo "╚════════════════════════════════════════════════════════════════════╝"
        echo ""
        echo "  FPS:      $current_fps fps    $fps_marker"
        echo "  GPU 负载: ${current_gpu}%     $gpu_marker"
        echo "  延迟:     ${current_latency}ms    $latency_marker"
        ((iteration += 1))
    done
}

# 主程序
command=${1:-help}
if (($# > 0)); then
    shift
fi
case "$command" in
    save)
        if (($# != 1)); then
            echo "❌ 错误：需要指定文件名"
            echo "用法: $0 save <file>"
            exit 2
        fi
        save_metrics "$1"
        ;;
    compare)
        if (($# != 2)); then
            echo "❌ 错误：需要两个文件进行对比"
            echo "用法: $0 compare <baseline-file> <current-file>"
            exit 2
        fi
        compare_metrics "$1" "$2"
        ;;
    monitor)
        (($# == 0)) || { echo "❌ monitor 不接受位置参数" >&2; exit 2; }
        monitor_changes
        ;;
    -h|--help|help)
        (($# == 0)) || { echo "❌ help 不接受位置参数" >&2; exit 2; }
        show_help
        ;;
    *)
        echo "❌ 未知命令: $command"
        show_help
        exit 1
        ;;
esac
