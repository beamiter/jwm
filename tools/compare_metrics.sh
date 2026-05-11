#!/bin/bash
# JWM Metrics Comparison Tool - 性能数据对比分析

set -e

BASELINE_FILE="${1:-baseline.json}"
CURRENT_FILE="${2:-current.json}"

# 获取当前指标
get_metrics() {
    jwm-tool msg get_metrics --raw 2>/dev/null | jq '.data' || echo '{}'
}

# 保存指标快照
save_metrics() {
    local file=$1
    local m=$(get_metrics)
    echo "$m" | jq '.' > "$file"
    echo "✓ 指标已保存到: $file"
}

# 性能对比
compare_metrics() {
    if [ ! -f "$BASELINE_FILE" ] || [ ! -f "$CURRENT_FILE" ]; then
        echo "❌ 错误：基线文件或当前文件不存在"
        echo "使用: $0 save <baseline-file>"
        echo "      $0 compare <baseline-file> <current-file>"
        exit 1
    fi

    local baseline=$(cat "$BASELINE_FILE")
    local current=$(cat "$CURRENT_FILE")

    # 提取指标
    local baseline_fps=$(echo "$baseline" | jq '.fps')
    local current_fps=$(echo "$current" | jq '.fps')
    local baseline_latency=$(echo "$baseline" | jq '.input_latency_avg_ms')
    local current_latency=$(echo "$current" | jq '.input_latency_avg_ms')
    local baseline_gpu=$(echo "$baseline" | jq '.gpu_load_percent')
    local current_gpu=$(echo "$current" | jq '.gpu_load_percent')
    local baseline_blur=$(echo "$baseline" | jq '.blur_cache_hit_rate')
    local current_blur=$(echo "$current" | jq '.blur_cache_hit_rate')

    # 计算变化
    local fps_change=$(echo "scale=2; ($current_fps - $baseline_fps)" | bc)
    local latency_change=$(echo "scale=2; ($baseline_latency - $current_latency)" | bc)
    local gpu_change=$(echo "scale=2; ($baseline_gpu - $current_gpu)" | bc)
    local blur_change=$(echo "scale=2; ($current_blur - $baseline_blur)" | bc)

    # 计算百分比变化
    local fps_pct=$(echo "scale=1; ($fps_change / $baseline_fps * 100)" | bc 2>/dev/null || echo "0")
    local latency_pct=$(echo "scale=1; ($latency_change / $baseline_latency * 100)" | bc 2>/dev/null || echo "0")
    local gpu_pct=$(echo "scale=1; (-$gpu_change / $baseline_gpu * 100)" | bc 2>/dev/null || echo "0")
    local blur_pct=$(echo "scale=1; ($blur_change / $baseline_blur * 100)" | bc 2>/dev/null || echo "0")

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
  变化:     $fps_change fps ($fps_pct%)
  $([ $(echo "$fps_change > 0" | bc) -eq 1 ] && echo "✓ 性能改善" || echo "✗ 性能下降")

【输入延迟】
  基线:     ${baseline_latency}ms
  当前:     ${current_latency}ms
  改善:     $latency_change ms ($latency_pct%)
  $([ $(echo "$latency_change > 0" | bc) -eq 1 ] && echo "✓ 延迟降低" || echo "✗ 延迟增加")

【GPU 负载】
  基线:     ${baseline_gpu}%
  当前:     ${current_gpu}%
  降低:     $gpu_change% ($gpu_pct%)
  $([ $(echo "$gpu_change > 0" | bc) -eq 1 ] && echo "✓ 负载降低" || echo "✗ 负载增加")

【Blur 缓存命中率】
  基线:     ${baseline_blur}%
  当前:     ${current_blur}%
  提升:     $blur_change% ($blur_pct%)
  $([ $(echo "$blur_change > 0" | bc) -eq 1 ] && echo "✓ 命中率提升" || echo "✗ 命中率下降")

═══════════════════════════════════════════════════════════════════

📈 详细指标对比表
═══════════════════════════════════════════════════════════════════

EOF

    # 详细对比表
    printf "%-35s | %15s | %15s | %15s\n" "指标" "基线" "当前" "变化"
    printf "%-35s | %15s | %15s | %15s\n" "---" "---" "---" "---"

    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "FPS" "$baseline_fps" "$current_fps" "$fps_change"
    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "平均帧时间 (ms)" \
        "$(echo "1000/$baseline_fps" | bc -l | xargs printf '%.2f')" \
        "$(echo "1000/$current_fps" | bc -l | xargs printf '%.2f')" \
        "$(echo "1000/$baseline_fps - 1000/$current_fps" | bc -l | xargs printf '%+.2f')"

    local baseline_max=$(echo "$baseline" | jq '.max_frame_time_ms')
    local current_max=$(echo "$current" | jq '.max_frame_time_ms')
    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "最大帧时间 (ms)" "$baseline_max" "$current_max" "$(echo "$current_max - $baseline_max" | bc)"

    local baseline_avg=$(echo "$baseline" | jq '.avg_frame_time_ms')
    local current_avg=$(echo "$current" | jq '.avg_frame_time_ms')
    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "平均帧时间 (ms)" "$baseline_avg" "$current_avg" "$(echo "$current_avg - $baseline_avg" | bc)"

    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "平均延迟 (ms)" "$baseline_latency" "$current_latency" "$latency_change"

    local baseline_p95=$(echo "$baseline" | jq '.input_latency_p95_ms')
    local current_p95=$(echo "$current" | jq '.input_latency_p95_ms')
    printf "%-35s | %15.2f | %15.2f | %+15.2f\n" "P95 延迟 (ms)" "$baseline_p95" "$current_p95" "$(echo "$current_p95 - $baseline_p95" | bc)"

    printf "%-35s | %15d%% | %15d%% | %+15d%%\n" "GPU 负载" "$baseline_gpu" "$current_gpu" "$(echo "$current_gpu - $baseline_gpu" | bc)"

    printf "%-35s | %15d%% | %15d%% | %+15d%%\n" "CPU 负载" \
        "$(echo "$baseline" | jq '.cpu_load_percent')" \
        "$(echo "$current" | jq '.cpu_load_percent')" \
        "$(echo "$(echo "$current" | jq '.cpu_load_percent') - $(echo "$baseline" | jq '.cpu_load_percent')" | bc)"

    printf "%-35s | %15.1f%% | %15.1f%% | %+15.1f%%\n" "Blur 缓存命中率" "$baseline_blur" "$current_blur" "$blur_change"

    local baseline_windows=$(echo "$baseline" | jq '.window_count')
    local current_windows=$(echo "$current" | jq '.window_count')
    printf "%-35s | %15d | %15d | %+15d\n" "窗口数量" "$baseline_windows" "$current_windows" "$(echo "$current_windows - $baseline_windows" | bc)"

    local baseline_dirty=$(echo "$baseline" | jq '.dirty_fraction_percent')
    local current_dirty=$(echo "$current" | jq '.dirty_fraction_percent')
    printf "%-35s | %15.1f%% | %15.1f%% | %+15.1f%%\n" "脏区域占比" "$baseline_dirty" "$current_dirty" "$(echo "$current_dirty - $baseline_dirty" | bc)"

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
    echo "📊 实时性能监控 - 按 Ctrl+C 退出"
    echo "将显示相对于首次快照的变化"
    echo ""

    # 获取初始快照
    local first=$(get_metrics)
    local first_fps=$(echo "$first" | jq '.fps')
    local first_gpu=$(echo "$first" | jq '.gpu_load_percent')
    local first_latency=$(echo "$first" | jq '.input_latency_avg_ms')

    while true; do
        sleep 1
        clear

        local current=$(get_metrics)
        local current_fps=$(echo "$current" | jq '.fps')
        local current_gpu=$(echo "$current" | jq '.gpu_load_percent')
        local current_latency=$(echo "$current" | jq '.input_latency_avg_ms')

        local fps_diff=$(echo "$current_fps - $first_fps" | bc)
        local gpu_diff=$(echo "$current_gpu - $first_gpu" | bc)
        local latency_diff=$(echo "$first_latency - $current_latency" | bc)

        echo "╔════════════════════════════════════════════════════════════════════╗"
        echo "║           JWM 实时性能监控 - $(date '+%H:%M:%S')                        ║"
        echo "╚════════════════════════════════════════════════════════════════════╝"
        echo ""
        echo "  FPS:      $current_fps fps    $([ $(echo "$fps_diff > 0" | bc) -eq 1 ] && echo "📈 +$fps_diff" || echo "📉 $fps_diff")"
        echo "  GPU 负载: ${current_gpu}%     $([ $(echo "$gpu_diff > 0" | bc) -eq 1 ] && echo "📈 +${gpu_diff}%" || echo "📉 ${gpu_diff}%")"
        echo "  延迟:     ${current_latency}ms    $([ $(echo "$latency_diff > 0" | bc) -eq 1 ] && echo "✓ 改善 -$latency_diff ms" || echo "✗ 恶化 +$latency_diff ms")"
    done
}

# 主程序
case "${1:-help}" in
    save)
        if [ -z "$2" ]; then
            echo "❌ 错误：需要指定文件名"
            echo "用法: $0 save <file>"
            exit 1
        fi
        save_metrics "$2"
        ;;
    compare)
        if [ -z "$2" ] || [ -z "$3" ]; then
            echo "❌ 错误：需要两个文件进行对比"
            echo "用法: $0 compare <baseline-file> <current-file>"
            exit 1
        fi
        compare_metrics "$2" "$3"
        ;;
    monitor)
        monitor_changes
        ;;
    -h|--help|help)
        show_help
        ;;
    *)
        echo "❌ 未知命令: $1"
        show_help
        exit 1
        ;;
esac
