#!/usr/bin/env bash
# JWM Metrics Dashboard - 实时性能监控
# 完整展现所有 compositor metrics

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

# 颜色定义
RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
BLUE=$'\033[0;34m'
CYAN=$'\033[0;36m'
MAGENTA=$'\033[0;35m'
NC=$'\033[0m' # No Color

require_commands() {
    local required
    for required in "$@"; do
        command -v "$required" >/dev/null 2>&1 || {
            echo "缺少必需命令: $required" >&2
            return 1
        }
    done
}

extract_valid_metrics() {
    jq -e '
        def nonnegative: type == "number" and isfinite and . >= 0;
        def percent: nonnegative and . <= 100;
        def count: nonnegative and floor == .;

        select(type == "object" and .success == true)
        | .data
        | select(
            type == "object"
            and ([
                .fps, .avg_frame_time_ms, .max_frame_time_ms,
                .min_frame_time_ms, .input_latency_avg_ms,
                .input_latency_p50_ms, .input_latency_p95_ms,
                .input_latency_p99_ms
            ] | all(.[]; nonnegative))
            and ([
                .gpu_load_percent, .cpu_load_percent,
                .blur_cache_hit_rate, .temporal_blur_reuse_rate,
                .dirty_fraction_percent
            ] | all(.[]; percent))
            and ([
                .frame_count, .blur_cache_hits, .blur_cache_misses,
                .temporal_blur_reuse_count, .temporal_blur_total_count,
                .draw_calls, .texture_memory_bytes, .window_count,
                .dirty_regions_count, .current_refresh_rate
            ] | all(.[]; count))
            and (.blur_quality | type == "string")
            and (.vrr_enabled | type == "boolean")
            and (.vrr_active | type == "boolean")
        )
    ' <<<"$1"
}

# 获取当前时间
timestamp() {
    date '+%Y-%m-%d %H:%M:%S'
}

# 获取 metrics
get_metrics() {
    local response metrics
    response="$(jwm-tool msg get_metrics --raw 2>/dev/null)" || return 1
    [[ -n $response ]] || return 1
    metrics="$(extract_valid_metrics "$response")" || return 1
    [[ -n $metrics ]] || return 1
    printf '%s\n' "$metrics"
}

# 格式化字节为可读格式
format_bytes() {
    local bytes=$1
    awk -v bytes="$bytes" 'BEGIN {
        if (bytes < 1024) printf "%.0fB\n", bytes
        else if (bytes < 1024 * 1024) printf "%.0fKB\n", bytes / 1024
        else if (bytes < 1024 * 1024 * 1024) printf "%.0fMB\n", bytes / (1024 * 1024)
        else printf "%.0fGB\n", bytes / (1024 * 1024 * 1024)
    }'
}

# 绘制简单柱状图
draw_bar() {
    local percent=$1
    local width=20
    local filled empty
    filled="$(awk -v percent="$percent" -v width="$width" 'BEGIN {
        value = int(percent * width / 100 + 0.5)
        if (value < 0) value = 0
        if (value > width) value = width
        print value
    }')"
    empty=$((width - filled))

    printf "["
    printf '%*s' "$filled" '' | tr ' ' '='
    printf '%*s' "$empty" '' | tr ' ' '-'
    printf "] %5.1f%%\n" "$percent"
}

# 核心指标展示
show_fps_metrics() {
    local m=$1
    local fps avg_frame max_frame min_frame frame_count
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}📊 FPS & 时间指标${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r fps avg_frame max_frame min_frame frame_count < <(
        jq -r '[
            .fps, .avg_frame_time_ms, .max_frame_time_ms,
            .min_frame_time_ms, .frame_count
        ] | @tsv' <<<"$m"
    )

    printf "  %-25s: ${GREEN}%.1f fps${NC}\n" "当前帧率" "$fps"
    printf "  %-25s: %.2f ms\n" "平均帧时间" "$avg_frame"
    printf "  %-25s: %.2f ms\n" "最大帧时间" "$max_frame"
    printf "  %-25s: %.2f ms\n" "最小帧时间" "$min_frame"
    printf "  %-25s: %.0f\n" "总帧数" "$frame_count"
    echo ""
}

# 负载指标展示
show_load_metrics() {
    local m=$1
    local gpu cpu
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${MAGENTA}⚡ 负载指标${NC}"
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r gpu cpu < <(
        jq -r '[.gpu_load_percent, .cpu_load_percent] | @tsv' <<<"$m"
    )

    printf "  %-25s: " "GPU 负载"
    draw_bar "$gpu"
    printf "  %-25s: " "CPU 负载"
    draw_bar "$cpu"
    echo ""
}

# Blur 缓存指标
show_blur_cache_metrics() {
    local m=$1
    local hits misses rate
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}🎯 Blur 缓存指标${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r hits misses rate < <(
        jq -r '[.blur_cache_hits, .blur_cache_misses, .blur_cache_hit_rate] | @tsv' <<<"$m"
    )

    printf "  %-25s: %.0f\n" "缓存命中" "$hits"
    printf "  %-25s: %.0f\n" "缓存未命中" "$misses"
    printf "  %-25s: %.1f%%\n" "命中率" "$rate"
    echo ""
}

# Temporal Blur 指标 (P4)
show_temporal_blur_metrics() {
    local m=$1
    local reuse total rate
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${YELLOW}⏱️  Temporal Blur 指标 (P4)${NC}"
    echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r reuse total rate < <(
        jq -r '[
            .temporal_blur_reuse_count,
            .temporal_blur_total_count,
            .temporal_blur_reuse_rate
        ] | @tsv' <<<"$m"
    )

    printf "  %-25s: %.0f\n" "复用计数" "$reuse"
    printf "  %-25s: %.0f\n" "总计数" "$total"
    printf "  %-25s: %.1f%%\n" "复用率" "$rate"
    echo ""
}

# 渲染指标
show_render_metrics() {
    local m=$1
    local draws mem windows dirty dirty_frac blur_quality
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}🎨 渲染指标${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r draws mem windows dirty dirty_frac < <(
        jq -r '[
            .draw_calls, .texture_memory_bytes, .window_count,
            .dirty_regions_count, .dirty_fraction_percent
        ] | @tsv' <<<"$m"
    )
    blur_quality="$(jq -r '
        .blur_quality
        | gsub("[\u0000-\u001f\u007f-\u009f\u2028-\u202e\u2066-\u2069]"; " ")
        | .[0:80]
    ' <<<"$m")"

    printf "  %-25s: %.0f\n" "绘制调用" "$draws"
    printf "  %-25s: %s\n" "纹理内存" "$(format_bytes "$mem")"
    printf "  %-25s: %.0f\n" "窗口数量" "$windows"
    printf "  %-25s: %.0f\n" "脏区域数" "$dirty"
    printf "  %-25s: %.1f%%\n" "脏区域占比" "$dirty_frac"
    printf "  %-25s: %s\n" "Blur 质量" "$blur_quality"
    echo ""
}

# VRR 指标
show_vrr_metrics() {
    local m=$1
    local vrr_enabled vrr_active refresh enabled_text active_text
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}🎮 VRR 指标${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r vrr_enabled vrr_active refresh < <(
        jq -r '[.vrr_enabled, .vrr_active, .current_refresh_rate] | @tsv' <<<"$m"
    )
    enabled_text="✗ 否"
    active_text="✗ 否"
    if [[ $vrr_enabled == true ]]; then
        enabled_text="${GREEN}✓ 是${NC}"
    fi
    if [[ $vrr_active == true ]]; then
        active_text="${GREEN}✓ 是${NC}"
    fi

    printf "  %-25s: %s\n" "VRR 启用" "$enabled_text"
    printf "  %-25s: %s\n" "VRR 活跃" "$active_text"
    printf "  %-25s: %.0f Hz\n" "当前刷新率" "$refresh"
    echo ""
}

# 输入延迟指标
show_input_latency_metrics() {
    local m=$1
    local avg p50 p95 p99
    echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${RED}⌨️  输入延迟指标${NC}"
    echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r avg p50 p95 p99 < <(
        jq -r '[
            .input_latency_avg_ms, .input_latency_p50_ms,
            .input_latency_p95_ms, .input_latency_p99_ms
        ] | @tsv' <<<"$m"
    )

    printf "  %-25s: %.2f ms\n" "平均延迟" "$avg"
    printf "  %-25s: %.2f ms\n" "P50 延迟" "$p50"
    printf "  %-25s: %.2f ms\n" "P95 延迟" "$p95"
    printf "  %-25s: %.2f ms\n" "P99 延迟" "$p99"
    echo ""
}

number_greater_than() {
    awk -v left="$1" -v right="$2" 'BEGIN { exit !(left > right) }'
}

load_status() {
    local value=$1
    if number_greater_than "$value" 80; then
        printf '🔴\n'
    elif number_greater_than "$value" 60; then
        printf '🟡\n'
    else
        printf '🟢\n'
    fi
}

latency_status() {
    local value=$1
    if number_greater_than "$value" 30; then
        printf '🔴\n'
    elif number_greater_than "$value" 20; then
        printf '🟡\n'
    else
        printf '🟢\n'
    fi
}

# 综合指标概览
show_summary() {
    local m=$1
    local fps gpu cpu blur_rate input_avg
    local fps_status gpu_status cpu_status input_status
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}📋 综合概览${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    IFS=$'\t' read -r fps gpu cpu blur_rate input_avg < <(
        jq -r '[
            .fps, .gpu_load_percent, .cpu_load_percent,
            .blur_cache_hit_rate, .input_latency_avg_ms
        ] | @tsv' <<<"$m"
    )

    # 性能评级
    if number_greater_than 30 "$fps"; then
        fps_status="🔴"
    elif number_greater_than 60 "$fps"; then
        fps_status="🟡"
    else
        fps_status="🟢"
    fi
    gpu_status="$(load_status "$gpu")"
    cpu_status="$(load_status "$cpu")"
    input_status="$(latency_status "$input_avg")"

    printf "  %-25s: %s %.1f fps\n" "帧率性能" "$fps_status" "$fps"
    printf "  %-25s: %s %.0f%%\n" "GPU 负载" "$gpu_status" "$gpu"
    printf "  %-25s: %s %.0f%%\n" "CPU 负载" "$cpu_status" "$cpu"
    printf "  %-25s: %.1f%%\n" "Blur 缓存命中率" "$blur_rate"
    printf "  %-25s: %s %.2f ms\n" "输入延迟" "$input_status" "$input_avg"
    echo ""
}

# 显示帮助
show_help() {
    while IFS= read -r line; do
        printf '%s\n' "$line"
    done << EOF
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
    local m
    m="$(get_metrics)" || {
        echo -e "${RED}✗ 无法获取有效指标，请确保 JWM 正在运行${NC}" >&2
        return 1
    }
    jq '.' <<<"$m" > "$file"
    echo "✓ 指标已导出到: $file"
}

# 默认模式：实时监控
real_time_monitor() {
    local interval=${1:-1}
    while true; do
        if [[ -t 1 ]]; then
            printf '\033[2J\033[H'
        fi
        echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
        echo -e "${MAGENTA}  JWM 性能监控仪表板${NC} - $(timestamp)"
        echo -e "${MAGENTA}════════════════════════════════════════════════════════════════${NC}"
        echo ""

        local m
        m="$(get_metrics)" || {
            echo -e "${RED}✗ 无法获取有效指标，请确保 JWM 正在运行${NC}"
            sleep "$interval"
            continue
        }

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

    local m
    m="$(get_metrics)" || {
        echo -e "${RED}✗ 无法获取有效指标，请确保 JWM 正在运行${NC}" >&2
        return 1
    }

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

valid_interval() {
    local value=$1
    [[ $value =~ ^[0-9]+([.][0-9]+)?$ && $value =~ [1-9] ]]
}

select_mode() {
    local requested=$1
    if [[ $MODE_EXPLICIT == true && $MODE != "$requested" ]]; then
        echo "互斥模式不能同时使用: $MODE 与 $requested" >&2
        return 2
    fi
    MODE=$requested
    MODE_EXPLICIT=true
}

select_view() {
    local requested=$1
    SHOW_QUICK=false
    SHOW_MODE="none"
    case "$requested" in
        full) ;;
        quick) SHOW_QUICK=true ;;
        *) SHOW_MODE=$requested ;;
    esac
    VIEW_SELECTED=true
}

# 解析命令行参数
MODE="real-time"
MODE_EXPLICIT=false
INTERVAL=1
INTERVAL_SET=false
SHOW_QUICK=false
SHOW_MODE="none"
VIEW_SELECTED=false
EXPORT_FILE=""
HELP_REQUESTED=false

while (($# > 0)); do
    case $1 in
        -r|--real-time)
            select_mode "real-time"
            shift
            ;;
        -s|--single)
            select_mode "single"
            shift
            ;;
        -i|--interval)
            if (($# < 2)) || [[ $2 == -* ]]; then
                echo "--interval 需要一个正数秒值" >&2
                exit 2
            fi
            INTERVAL=$2
            INTERVAL_SET=true
            shift 2
            ;;
        -f|--full)
            select_view full
            shift
            ;;
        -q|--quick)
            select_view quick
            shift
            ;;
        --fps|--load|--blur|--vrr|--latency)
            select_view "${1#--}"
            shift
            ;;
        --export)
            if (($# < 2)) || [[ -z $2 || $2 == -* ]]; then
                echo "--export 需要一个文件名" >&2
                exit 2
            fi
            select_mode "export"
            EXPORT_FILE=$2
            shift 2
            ;;
        -h|--help)
            HELP_REQUESTED=true
            shift
            ;;
        *)
            echo "未知选项: $1" >&2
            show_help >&2
            exit 2
            ;;
    esac
done

if [[ $HELP_REQUESTED == true ]]; then
    show_help
    exit 0
fi
if ! valid_interval "$INTERVAL"; then
    echo "更新间隔必须是大于 0 的数字: $INTERVAL" >&2
    exit 2
fi
if [[ $MODE != real-time && $INTERVAL_SET == true ]]; then
    echo "--interval 仅适用于实时监控模式" >&2
    exit 2
fi
if [[ $MODE == export && $VIEW_SELECTED == true ]]; then
    echo "指标视图选项不能与 --export 同时使用" >&2
    exit 2
fi

case "$MODE" in
    export) require_commands jwm-tool jq ;;
    single) require_commands jwm-tool jq awk date tr ;;
    real-time) require_commands jwm-tool jq awk date tr sleep ;;
esac

# 执行选择的模式
case "$MODE" in
    real-time) real_time_monitor "$INTERVAL" ;;
    single) single_display ;;
    export) export_metrics "$EXPORT_FILE" ;;
esac
