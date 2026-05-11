#!/bin/bash
# JWM Metrics Dashboard - 实时性能监控
# 完整展现所有 compositor metrics

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
NC='\033[0m' # No Color

# 获取当前时间
timestamp() {
    date '+%Y-%m-%d %H:%M:%S'
}

# 获取 metrics
get_metrics() {
    jwm-tool msg get_metrics --raw 2>/dev/null | jq '.data' || echo '{}'
}

# 格式化字节为可读格式
format_bytes() {
    local bytes=$1
    if [ $bytes -lt 1024 ]; then
        echo "${bytes}B"
    elif [ $bytes -lt $((1024*1024)) ]; then
        echo "$((bytes / 1024))KB"
    elif [ $bytes -lt $((1024*1024*1024)) ]; then
        echo "$((bytes / (1024*1024)))MB"
    else
        echo "$((bytes / (1024*1024*1024)))GB"
    fi
}

# 绘制简单柱状图
draw_bar() {
    local percent=$1
    local width=20
    local filled=$((percent * width / 100))
    local empty=$((width - filled))

    printf "["
    printf "%${filled}s" | tr ' ' '='
    printf "%${empty}s" | tr ' ' '-'
    printf "] %3d%%\n" $percent
}

# 核心指标展示
show_fps_metrics() {
    local m=$1
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}📊 FPS & 时间指标${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local fps=$(echo "$m" | jq -r '.fps // 0')
    local avg_frame=$(echo "$m" | jq -r '.avg_frame_time_ms // 0')
    local max_frame=$(echo "$m" | jq -r '.max_frame_time_ms // 0')
    local min_frame=$(echo "$m" | jq -r '.min_frame_time_ms // 0')
    local frame_count=$(echo "$m" | jq -r '.frame_count // 0')

    printf "  %-25s: ${GREEN}%.1f fps${NC}\n" "当前帧率" "$fps"
    printf "  %-25s: %.2f ms\n" "平均帧时间" "$avg_frame"
    printf "  %-25s: %.2f ms\n" "最大帧时间" "$max_frame"
    printf "  %-25s: %.2f ms\n" "最小帧时间" "$min_frame"
    printf "  %-25s: %d\n" "总帧数" "${frame_count%.*}"
    echo ""
}

# 负载指标展示
show_load_metrics() {
    local m=$1
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${MAGENTA}⚡ 负载指标${NC}"
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local gpu=$(echo "$m" | jq -r '.gpu_load_percent // 0')
    local cpu=$(echo "$m" | jq -r '.cpu_load_percent // 0')

    printf "  %-25s: " "GPU 负载"
    draw_bar "$gpu"
    printf "  %-25s: " "CPU 负载"
    draw_bar "$cpu"
    echo ""
}

# Blur 缓存指标
show_blur_cache_metrics() {
    local m=$1
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}🎯 Blur 缓存指标${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local hits=$(echo "$m" | jq -r '.blur_cache_hits // 0')
    local misses=$(echo "$m" | jq -r '.blur_cache_misses // 0')
    local rate=$(echo "$m" | jq -r '.blur_cache_hit_rate // 0')

    printf "  %-25s: %d\n" "缓存命中" "${hits%.*}"
    printf "  %-25s: %d\n" "缓存未命中" "${misses%.*}"
    printf "  %-25s: %.1f%%\n" "命中率" "$rate"
    echo ""
}

# Temporal Blur 指标 (P4)
show_temporal_blur_metrics() {
    local m=$1
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${YELLOW}⏱️  Temporal Blur 指标 (P4)${NC}"
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local reuse=$(echo "$m" | jq -r '.temporal_blur_reuse_count // 0')
    local total=$(echo "$m" | jq -r '.temporal_blur_total_count // 0')
    local rate=$(echo "$m" | jq -r '.temporal_blur_reuse_rate // 0')

    printf "  %-25s: %d\n" "复用计数" "${reuse%.*}"
    printf "  %-25s: %d\n" "总计数" "${total%.*}"
    printf "  %-25s: %.1f%%\n" "复用率" "$rate"
    echo ""
}

# 渲染指标
show_render_metrics() {
    local m=$1
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}🎨 渲染指标${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local draws=$(echo "$m" | jq -r '.draw_calls // 0')
    local mem=$(echo "$m" | jq -r '.texture_memory_bytes // 0')
    local windows=$(echo "$m" | jq -r '.window_count // 0')
    local dirty=$(echo "$m" | jq -r '.dirty_regions_count // 0')
    local dirty_frac=$(echo "$m" | jq -r '.dirty_fraction_percent // 0')
    local blur_quality=$(echo "$m" | jq -r '.blur_quality // "unknown"')

    printf "  %-25s: %d\n" "绘制调用" "${draws%.*}"
    printf "  %-25s: %s\n" "纹理内存" "$(format_bytes ${mem%.*})"
    printf "  %-25s: %d\n" "窗口数量" "${windows%.*}"
    printf "  %-25s: %d\n" "脏区域数" "${dirty%.*}"
    printf "  %-25s: %.1f%%\n" "脏区域占比" "$dirty_frac"
    printf "  %-25s: %s\n" "Blur 质量" "$blur_quality"
    echo ""
}

# VRR 指标
show_vrr_metrics() {
    local m=$1
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}🎮 VRR 指标${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local vrr_enabled=$(echo "$m" | jq -r '.vrr_enabled // false')
    local vrr_active=$(echo "$m" | jq -r '.vrr_active // false')
    local refresh=$(echo "$m" | jq -r '.current_refresh_rate // 0')

    printf "  %-25s: %s\n" "VRR 启用" "$([ "$vrr_enabled" = "true" ] && echo "${GREEN}✓ 是${NC}" || echo "✗ 否")"
    printf "  %-25s: %s\n" "VRR 活跃" "$([ "$vrr_active" = "true" ] && echo "${GREEN}✓ 是${NC}" || echo "✗ 否")"
    printf "  %-25s: %d Hz\n" "当前刷新率" "${refresh%.*}"
    echo ""
}

# 输入延迟指标
show_input_latency_metrics() {
    local m=$1
    echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${RED}⌨️  输入延迟指标${NC}"
    echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local avg=$(echo "$m" | jq -r '.input_latency_avg_ms // 0')
    local p50=$(echo "$m" | jq -r '.input_latency_p50_ms // 0')
    local p95=$(echo "$m" | jq -r '.input_latency_p95_ms // 0')
    local p99=$(echo "$m" | jq -r '.input_latency_p99_ms // 0')

    printf "  %-25s: %.2f ms\n" "平均延迟" "$avg"
    printf "  %-25s: %.2f ms\n" "P50 延迟" "$p50"
    printf "  %-25s: %.2f ms\n" "P95 延迟" "$p95"
    printf "  %-25s: %.2f ms\n" "P99 延迟" "$p99"
    echo ""
}

# 综合指标概览
show_summary() {
    local m=$1
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}📋 综合概览${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local fps=$(echo "$m" | jq -r '.fps // 0')
    local gpu=$(echo "$m" | jq -r '.gpu_load_percent // 0')
    local cpu=$(echo "$m" | jq -r '.cpu_load_percent // 0')
    local blur_rate=$(echo "$m" | jq -r '.blur_cache_hit_rate // 0')
    local input_avg=$(echo "$m" | jq -r '.input_latency_avg_ms // 0')

    # 性能评级
    local fps_status="🟢"
    if (( $(echo "$fps < 30" | bc -l) )); then fps_status="🔴"; fi
    if (( $(echo "$fps >= 30 && $fps < 60" | bc -l) )); then fps_status="🟡"; fi

    local gpu_status="🟢"
    if (( $(echo "$gpu > 80" | bc -l) )); then gpu_status="🔴"; fi
    if (( $(echo "$gpu > 60" | bc -l) )); then gpu_status="🟡"; fi

    local input_status="🟢"
    if (( $(echo "$input_avg > 30" | bc -l) )); then input_status="🔴"; fi
    if (( $(echo "$input_avg > 20" | bc -l) )); then input_status="🟡"; fi

    printf "  %-25s: %s %.1f fps\n" "帧率性能" "$fps_status" "$fps"
    printf "  %-25s: %s %.0f%%\n" "GPU 负载" "$gpu_status" "$gpu"
    printf "  %-25s: %s %.0f%%\n" "CPU 负载" "$gpu_status" "$cpu"
    printf "  %-25s: %.1f%%\n" "Blur 缓存命中率" "$blur_rate"
    printf "  %-25s: %s %.2f ms\n" "输入延迟" "$input_status" "$input_avg"
    echo ""
}

# 显示帮助
show_help() {
    cat << EOF
用法: $0 [选项]

选项:
    -r, --real-time     实时监控模式 (默认，每秒更新)
    -s, --single        单次显示
    -i, --interval SEC  自定义更新间隔 (秒)
    -f, --full          显示全部指标
    -q, --quick         仅显示快速指标 (fps, 负载)
    --fps               仅显示 FPS 指标
    --load              仅显示负载指标
    --blur              仅显示 Blur 指标
    --vrr               仅显示 VRR 指标
    --latency           仅显示输入延迟指标
    --export FILE       导出为 JSON 格式
    -h, --help          显示此帮助信息

示例:
    # 实时监控 (默认)
    $0

    # 每2秒更新一次
    $0 -i 2

    # 仅显示快速指标
    $0 -q

    # 导出当前指标为 JSON
    $0 --export metrics.json

    # 仅显示一次所有指标
    $0 -s -f
EOF
}

# 导出为 JSON
export_metrics() {
    local file=$1
    local m=$(get_metrics)
    echo "$m" > "$file"
    echo "✓ 指标已导出到: $file"
}

# 默认模式：实时监控
real_time_monitor() {
    local interval=${1:-1}
    while true; do
        clear
        echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
        echo -e "${MAGENTA}  JWM 性能监控仪表板${NC} - $(timestamp)"
        echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
        echo ""

        local m=$(get_metrics)

        # 检查是否成功获取指标
        if [ "$(echo "$m" | jq '.fps' 2>/dev/null)" = "null" ]; then
            echo -e "${RED}✗ 无法获取指标，请确保 JWM 正在运行${NC}"
            sleep 1
            continue
        fi

        if [ "$SHOW_QUICK" = true ]; then
            show_fps_metrics "$m"
            show_load_metrics "$m"
            show_summary "$m"
        elif [ "$SHOW_MODE" != "none" ]; then
            case "$SHOW_MODE" in
                fps) show_fps_metrics "$m" ;;
                load) show_load_metrics "$m" ;;
                blur) show_blur_cache_metrics "$m" ;;
                vrr) show_vrr_metrics "$m" ;;
                latency) show_input_latency_metrics "$m" ;;
            esac
        else
            show_summary "$m"
            show_fps_metrics "$m"
            show_load_metrics "$m"
            show_blur_cache_metrics "$m"
            show_temporal_blur_metrics "$m"
            show_render_metrics "$m"
            show_vrr_metrics "$m"
            show_input_latency_metrics "$m"
        fi

        echo -e "${MAGENTA}按 Ctrl+C 退出，下一次更新在 ${interval}s 后${NC}"
        sleep "$interval"
    done
}

# 单次显示
single_display() {
    echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
    echo -e "${MAGENTA}  JWM 性能报告${NC} - $(timestamp)"
    echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
    echo ""

    local m=$(get_metrics)

    # 检查是否成功获取指标
    if [ "$(echo "$m" | jq '.fps' 2>/dev/null)" = "null" ]; then
        echo -e "${RED}✗ 无法获取指标，请确保 JWM 正在运行${NC}"
        exit 1
    fi

    if [ "$SHOW_QUICK" = true ]; then
        show_fps_metrics "$m"
        show_load_metrics "$m"
        show_summary "$m"
    elif [ "$SHOW_MODE" != "none" ]; then
        case "$SHOW_MODE" in
            fps) show_fps_metrics "$m" ;;
            load) show_load_metrics "$m" ;;
            blur) show_blur_cache_metrics "$m" ;;
            vrr) show_vrr_metrics "$m" ;;
            latency) show_input_latency_metrics "$m" ;;
        esac
    else
        show_summary "$m"
        show_fps_metrics "$m"
        show_load_metrics "$m"
        show_blur_cache_metrics "$m"
        show_temporal_blur_metrics "$m"
        show_render_metrics "$m"
        show_vrr_metrics "$m"
        show_input_latency_metrics "$m"
    fi

    echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
}

# 解析命令行参数
MODE="real-time"
INTERVAL=1
SHOW_QUICK=false
SHOW_MODE="none"
SHOW_FULL=false

while [[ $# -gt 0 ]]; do
    case $1 in
        -r|--real-time) MODE="real-time"; shift ;;
        -s|--single) MODE="single"; shift ;;
        -i|--interval) INTERVAL="$2"; shift 2 ;;
        -f|--full) SHOW_FULL=true; shift ;;
        -q|--quick) SHOW_QUICK=true; shift ;;
        --fps) SHOW_MODE="fps"; shift ;;
        --load) SHOW_MODE="load"; shift ;;
        --blur) SHOW_MODE="blur"; shift ;;
        --vrr) SHOW_MODE="vrr"; shift ;;
        --latency) SHOW_MODE="latency"; shift ;;
        --export) export_metrics "$2"; exit 0; shift 2 ;;
        -h|--help) show_help; exit 0; shift ;;
        *) echo "未知选项: $1"; show_help; exit 1 ;;
    esac
done

# 执行选择的模式
case "$MODE" in
    real-time) real_time_monitor "$INTERVAL" ;;
    single) single_display ;;
    *) show_help ;;
esac
