#!/usr/bin/env bash
# JWM Performance Report Generator - 生成完整性能分析报告

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

if (($# > 1)); then
    echo "用法: $0 [output-directory]" >&2
    exit 2
fi

OUTPUT_DIR="${1:-.}"
mkdir -p -- "$OUTPUT_DIR"
readonly OUTPUT_DIR

require_commands() {
    local command
    for command in "$@"; do
        command -v "$command" >/dev/null 2>&1 || {
            echo "缺少必需命令: $command" >&2
            return 1
        }
    done
}

validate_metrics() {
    jq -e '
        . as $metrics
        | def finite_nonnegative($name):
            ($metrics | has($name))
            and (($metrics[$name] | type) == "number")
            and ($metrics[$name] | isfinite)
            and ($metrics[$name] >= 0);
          def percentage($name):
            finite_nonnegative($name) and ($metrics[$name] <= 100);
          def count($name):
            finite_nonnegative($name)
            and (($metrics[$name] | floor) == $metrics[$name]);
          def typed($name; $expected):
            ($metrics | has($name))
            and (($metrics[$name] | type) == $expected);
          all([
              "fps", "avg_frame_time_ms", "max_frame_time_ms",
              "min_frame_time_ms", "input_latency_avg_ms",
              "input_latency_p50_ms", "input_latency_p95_ms",
              "input_latency_p99_ms"
          ][]; finite_nonnegative(.))
          and all([
              "gpu_load_percent", "cpu_load_percent", "blur_cache_hit_rate",
              "temporal_blur_reuse_rate", "dirty_fraction_percent"
          ][]; percentage(.))
          and all([
              "frame_count", "blur_cache_hits", "blur_cache_misses",
              "temporal_blur_reuse_count", "temporal_blur_total_count",
              "draw_calls", "texture_memory_bytes", "window_count",
              "dirty_regions_count", "current_refresh_rate"
          ][]; count(.))
          and all(["vrr_enabled", "vrr_active"][]; typed(.; "boolean"))
          and typed("blur_quality"; "string")
    ' >/dev/null <<<"$1"
}

# 函数：获取指标
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
    validate_metrics "$metrics" || return 1
    printf '%s\n' "$metrics"
}

# 等待样本数据
collect_samples() {
    local samples=${JWM_REPORT_SAMPLES:-10}
    local interval=${JWM_REPORT_INTERVAL:-1}
    local fps_sum=0
    local frame_time_sum=0
    local i m fps frame avg_fps avg_frame

    if [[ ! $samples =~ ^[1-9][0-9]*$ ]]; then
        echo "JWM_REPORT_SAMPLES must be a positive integer." >&2
        return 2
    fi
    if [[ ! $interval =~ ^[0-9]+([.][0-9]+)?$ ]]; then
        echo "JWM_REPORT_INTERVAL must be a non-negative number." >&2
        return 2
    fi

    echo "⏳ 收集 $samples 个样本 (每 ${interval}s 一次)..." >&2

    for ((i = 1; i <= samples; i++)); do
        m="$(get_metrics)" || {
            echo "无法从正在运行的 JWM 获取指标。" >&2
            return 1
        }
        IFS=$'\t' read -r fps frame < <(
            jq -er '[.fps, .avg_frame_time_ms]
                | select(all(.[]; type == "number")) | @tsv' <<<"$m"
        ) || {
            echo "JWM 返回了无效的 FPS/帧时间指标。" >&2
            return 1
        }

        fps_sum="$(jq -nr --argjson sum "$fps_sum" --argjson value "$fps" \
            '$sum + $value')"
        frame_time_sum="$(jq -nr --argjson sum "$frame_time_sum" \
            --argjson value "$frame" '$sum + $value')"

        printf "  [%d/%d] FPS: %.1f, 帧时: %.2fms\n" \
            "$i" "$samples" "$fps" "$frame" >&2
        if ((i < samples)); then
            sleep "$interval"
        fi
    done

    avg_fps="$(jq -nr --argjson sum "$fps_sum" --argjson count "$samples" \
        '$sum / $count')"
    avg_frame="$(jq -nr --argjson sum "$frame_time_sum" \
        --argjson count "$samples" '$sum / $count')"

    echo "✓ 样本收集完成" >&2
    printf '%s,%s\n' "$avg_fps" "$avg_frame"
}

# 生成 HTML 报告
generate_html_report() {
    local metrics_b64=$1
    local output_file=$2

    cat > "$output_file" << 'EOF'
<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>JWM 性能分析报告</title>
    <style>
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }

        body {
            font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            padding: 20px;
        }

        .container {
            max-width: 1400px;
            margin: 0 auto;
            background: white;
            border-radius: 12px;
            box-shadow: 0 20px 60px rgba(0,0,0,0.3);
            overflow: hidden;
        }

        .header {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 40px;
            text-align: center;
        }

        .header h1 {
            font-size: 2.5em;
            margin-bottom: 10px;
        }

        .header p {
            font-size: 1.1em;
            opacity: 0.9;
        }

        .timestamp {
            text-align: center;
            color: #666;
            padding: 20px;
            font-size: 0.9em;
            border-bottom: 1px solid #eee;
        }

        .content {
            padding: 40px;
        }

        .metrics-grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
            gap: 20px;
            margin-bottom: 40px;
        }

        .metric-card {
            background: linear-gradient(135deg, #f5f7fa 0%, #c3cfe2 100%);
            border-radius: 12px;
            padding: 20px;
            border-left: 4px solid #667eea;
            box-shadow: 0 4px 12px rgba(0,0,0,0.1);
        }

        .metric-card h3 {
            color: #333;
            margin-bottom: 15px;
            font-size: 1em;
        }

        .metric-value {
            font-size: 2em;
            font-weight: bold;
            color: #667eea;
            margin-bottom: 5px;
        }

        .metric-unit {
            color: #666;
            font-size: 0.9em;
        }

        .status-indicator {
            display: inline-block;
            width: 12px;
            height: 12px;
            border-radius: 50%;
            margin-right: 8px;
            vertical-align: middle;
        }

        .status-good { background-color: #4caf50; }
        .status-warning { background-color: #ff9800; }
        .status-danger { background-color: #f44336; }

        .section {
            margin-bottom: 40px;
        }

        .section-title {
            font-size: 1.5em;
            color: #333;
            margin-bottom: 20px;
            padding-bottom: 10px;
            border-bottom: 2px solid #667eea;
        }

        .chart-container {
            position: relative;
            height: 300px;
            margin-bottom: 30px;
        }

        table {
            width: 100%;
            border-collapse: collapse;
            margin-bottom: 20px;
        }

        table th {
            background: #f5f5f5;
            padding: 12px;
            text-align: left;
            color: #333;
            font-weight: 600;
            border-bottom: 2px solid #ddd;
        }

        table td {
            padding: 10px 12px;
            border-bottom: 1px solid #eee;
        }

        table tr:hover {
            background: #f9f9f9;
        }

        .metric-bar {
            display: flex;
            align-items: center;
            gap: 10px;
        }

        .bar {
            flex-grow: 1;
            height: 24px;
            background: #e0e0e0;
            border-radius: 4px;
            overflow: hidden;
        }

        .bar-fill {
            height: 100%;
            background: linear-gradient(90deg, #667eea, #764ba2);
            display: flex;
            align-items: center;
            justify-content: center;
            color: white;
            font-size: 0.85em;
            font-weight: 600;
        }

        .summary-box {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 20px;
            border-radius: 8px;
            margin-bottom: 20px;
        }

        .summary-box h4 {
            margin-bottom: 10px;
        }

        .summary-item {
            display: flex;
            justify-content: space-between;
            padding: 5px 0;
        }

        .footer {
            text-align: center;
            padding: 20px;
            color: #999;
            border-top: 1px solid #eee;
        }

        .performance-score {
            font-size: 3em;
            font-weight: bold;
            color: #667eea;
            text-align: center;
            margin: 20px 0;
        }

        .recommendation {
            background: #e3f2fd;
            border-left: 4px solid #2196f3;
            padding: 15px;
            margin: 10px 0;
            border-radius: 4px;
        }

        .recommendation strong {
            color: #1976d2;
        }

        .improvement-badge {
            display: inline-block;
            background: #4caf50;
            color: white;
            padding: 4px 12px;
            border-radius: 20px;
            font-size: 0.85em;
            margin: 5px 5px 5px 0;
        }
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>🎨 JWM 性能分析报告</h1>
            <p>完整的性能指标仪表板与优化建议</p>
        </div>

        <div class="timestamp" id="timestamp"></div>

        <div class="content">
            <!-- 性能评分 -->
            <div class="section">
                <div class="section-title">📊 综合性能评分</div>
                <div class="performance-score" id="score"></div>
                <div class="summary-box" id="summary">
                    <div class="summary-item">
                        <span>当前帧率</span>
                        <strong id="summary-fps">-</strong>
                    </div>
                    <div class="summary-item">
                        <span>系统负载</span>
                        <strong id="summary-load">-</strong>
                    </div>
                    <div class="summary-item">
                        <span>平均输入延迟</span>
                        <strong id="summary-latency">-</strong>
                    </div>
                    <div class="summary-item">
                        <span>Blur 缓存命中率</span>
                        <strong id="summary-blur">-</strong>
                    </div>
                </div>
            </div>

            <!-- FPS 和时间指标 -->
            <div class="section">
                <div class="section-title">📈 FPS & 时间指标</div>
                <div class="metrics-grid">
                    <div class="metric-card">
                        <h3><span class="status-indicator" id="fps-status"></span>当前帧率</h3>
                        <div class="metric-value" id="fps">-</div>
                        <div class="metric-unit">frames per second</div>
                    </div>
                    <div class="metric-card">
                        <h3>平均帧时间</h3>
                        <div class="metric-value" id="avg-frame">-</div>
                        <div class="metric-unit">milliseconds</div>
                    </div>
                    <div class="metric-card">
                        <h3>最大帧时间</h3>
                        <div class="metric-value" id="max-frame">-</div>
                        <div class="metric-unit">milliseconds (spike)</div>
                    </div>
                    <div class="metric-card">
                        <h3>最小帧时间</h3>
                        <div class="metric-value" id="min-frame">-</div>
                        <div class="metric-unit">milliseconds (best)</div>
                    </div>
                </div>
            </div>

            <!-- 负载指标 -->
            <div class="section">
                <div class="section-title">⚡ 资源负载</div>
                <div class="metrics-grid">
                    <div class="metric-card">
                        <h3>GPU 负载</h3>
                        <div class="metric-bar">
                            <div class="bar">
                                <div class="bar-fill" id="gpu-bar" style="width: 0%">0%</div>
                            </div>
                        </div>
                    </div>
                    <div class="metric-card">
                        <h3>CPU 负载</h3>
                        <div class="metric-bar">
                            <div class="bar">
                                <div class="bar-fill" id="cpu-bar" style="width: 0%">0%</div>
                            </div>
                        </div>
                    </div>
                </div>
            </div>

            <!-- Blur 缓存指标 -->
            <div class="section">
                <div class="section-title">🎯 Blur 缓存优化</div>
                <div class="metrics-grid">
                    <div class="metric-card">
                        <h3>缓存命中率</h3>
                        <div class="metric-value" id="blur-rate">-</div>
                        <div class="metric-unit">hit rate</div>
                    </div>
                    <div class="metric-card">
                        <h3>缓存命中数</h3>
                        <div class="metric-value" id="blur-hits">-</div>
                        <div class="metric-unit">total hits</div>
                    </div>
                    <div class="metric-card">
                        <h3>缓存未命中数</h3>
                        <div class="metric-value" id="blur-misses">-</div>
                        <div class="metric-unit">total misses</div>
                    </div>
                </div>
            </div>

            <!-- Temporal Blur (P4) -->
            <div class="section">
                <div class="section-title">⏱️ Temporal Blur 优化 (P4)</div>
                <div class="metrics-grid">
                    <div class="metric-card">
                        <h3>复用率</h3>
                        <div class="metric-value" id="temporal-rate">-</div>
                        <div class="metric-unit">reuse rate</div>
                    </div>
                    <div class="metric-card">
                        <h3>复用计数</h3>
                        <div class="metric-value" id="temporal-reuse">-</div>
                        <div class="metric-unit">total reuses</div>
                    </div>
                    <div class="metric-card">
                        <h3>总计数</h3>
                        <div class="metric-value" id="temporal-total">-</div>
                        <div class="metric-unit">total count</div>
                    </div>
                </div>
            </div>

            <!-- 输入延迟 -->
            <div class="section">
                <div class="section-title">⌨️ 输入延迟</div>
                <table>
                    <thead>
                        <tr>
                            <th>指标</th>
                            <th>延迟 (ms)</th>
                            <th>性能评级</th>
                        </tr>
                    </thead>
                    <tbody>
                        <tr>
                            <td>平均延迟</td>
                            <td id="latency-avg">-</td>
                            <td id="latency-avg-status">-</td>
                        </tr>
                        <tr>
                            <td>P50 延迟</td>
                            <td id="latency-p50">-</td>
                            <td id="latency-p50-status">-</td>
                        </tr>
                        <tr>
                            <td>P95 延迟</td>
                            <td id="latency-p95">-</td>
                            <td id="latency-p95-status">-</td>
                        </tr>
                        <tr>
                            <td>P99 延迟</td>
                            <td id="latency-p99">-</td>
                            <td id="latency-p99-status">-</td>
                        </tr>
                    </tbody>
                </table>
            </div>

            <!-- 渲染指标 -->
            <div class="section">
                <div class="section-title">🎨 渲染指标</div>
                <table>
                    <thead>
                        <tr>
                            <th>项目</th>
                            <th>值</th>
                        </tr>
                    </thead>
                    <tbody>
                        <tr>
                            <td>绘制调用</td>
                            <td id="draw-calls">-</td>
                        </tr>
                        <tr>
                            <td>纹理内存</td>
                            <td id="texture-mem">-</td>
                        </tr>
                        <tr>
                            <td>窗口数量</td>
                            <td id="window-count">-</td>
                        </tr>
                        <tr>
                            <td>脏区域数量</td>
                            <td id="dirty-regions">-</td>
                        </tr>
                        <tr>
                            <td>脏区域占比</td>
                            <td id="dirty-frac">-</td>
                        </tr>
                        <tr>
                            <td>Blur 质量等级</td>
                            <td id="blur-quality">-</td>
                        </tr>
                    </tbody>
                </table>
            </div>

            <!-- VRR 指标 -->
            <div class="section">
                <div class="section-title">🎮 VRR 可变刷新率</div>
                <table>
                    <thead>
                        <tr>
                            <th>项目</th>
                            <th>状态</th>
                        </tr>
                    </thead>
                    <tbody>
                        <tr>
                            <td>VRR 启用</td>
                            <td id="vrr-enabled">-</td>
                        </tr>
                        <tr>
                            <td>VRR 活跃</td>
                            <td id="vrr-active">-</td>
                        </tr>
                        <tr>
                            <td>当前刷新率</td>
                            <td id="refresh-rate">-</td>
                        </tr>
                    </tbody>
                </table>
            </div>

            <!-- 优化建议 -->
            <div class="section">
                <div class="section-title">💡 优化建议</div>
                <div id="recommendations"></div>
            </div>

            <!-- 优化成果展示 -->
            <div class="section">
                <div class="section-title">🏆 已实现的优化 (P0-P7)</div>
                <div id="optimizations"></div>
            </div>

        </div>

        <div class="footer">
            <p>JWM 性能分析报告 | 生成时间: <span id="footer-time"></span></p>
        </div>
    </div>

    <script>
        const metricsPayload =
EOF
    printf "            '%s';\n" "$metrics_b64" >> "$output_file"
    cat >> "$output_file" << 'EOF'
        const metricsBytes = Uint8Array.from(
            atob(metricsPayload), character => character.charCodeAt(0)
        );
        const metricsData = JSON.parse(
            new TextDecoder('utf-8', { fatal: true }).decode(metricsBytes)
        );

        function formatBytes(bytes) {
            const units = ['B', 'KB', 'MB', 'GB'];
            let size = bytes;
            let unitIndex = 0;
            while (size >= 1024 && unitIndex < units.length - 1) {
                size /= 1024;
                unitIndex++;
            }
            return size.toFixed(2) + ' ' + units[unitIndex];
        }

        function getStatusIndicator(value, good, warning) {
            if (value >= good) return '<span class="status-indicator status-good"></span> 优秀';
            if (value >= warning) return '<span class="status-indicator status-warning"></span> 良好';
            return '<span class="status-indicator status-danger"></span> 需改进';
        }

        function getLatencyStatus(ms) {
            if (ms <= 20) return '<span class="status-indicator status-good"></span> 优秀 (≤20ms)';
            if (ms <= 30) return '<span class="status-indicator status-warning"></span> 良好 (≤30ms)';
            return '<span class="status-indicator status-danger"></span> 需改进 (>30ms)';
        }

        function updateMetrics() {
            // FPS 指标
            const fps = metricsData.sampled_avg_fps ?? metricsData.fps ?? 0;
            const avgFrame = metricsData.sampled_avg_frame_time_ms ?? metricsData.avg_frame_time_ms ?? 0;
            const maxFrame = metricsData.max_frame_time_ms || 0;
            const minFrame = metricsData.min_frame_time_ms || 0;

            document.getElementById('fps').textContent = fps.toFixed(1);
            document.getElementById('avg-frame').textContent = avgFrame.toFixed(2);
            document.getElementById('max-frame').textContent = maxFrame.toFixed(2);
            document.getElementById('min-frame').textContent = minFrame.toFixed(2);

            // 帧率状态
            const fpsStatus = fps >= 60 ? 'status-good' : (fps >= 30 ? 'status-warning' : 'status-danger');
            document.getElementById('fps-status').className = 'status-indicator ' + fpsStatus;

            // 负载指标
            const gpuLoad = metricsData.gpu_load_percent || 0;
            const cpuLoad = metricsData.cpu_load_percent || 0;

            document.getElementById('gpu-bar').style.width = gpuLoad + '%';
            document.getElementById('gpu-bar').textContent = gpuLoad + '%';
            document.getElementById('cpu-bar').style.width = cpuLoad + '%';
            document.getElementById('cpu-bar').textContent = cpuLoad + '%';

            // Blur 缓存
            const blurRate = metricsData.blur_cache_hit_rate || 0;
            const blurHits = metricsData.blur_cache_hits || 0;
            const blurMisses = metricsData.blur_cache_misses || 0;

            document.getElementById('blur-rate').textContent = blurRate.toFixed(1) + '%';
            document.getElementById('blur-hits').textContent = blurHits.toLocaleString();
            document.getElementById('blur-misses').textContent = blurMisses.toLocaleString();

            // Temporal Blur
            const temporalRate = metricsData.temporal_blur_reuse_rate || 0;
            const temporalReuse = metricsData.temporal_blur_reuse_count || 0;
            const temporalTotal = metricsData.temporal_blur_total_count || 0;

            document.getElementById('temporal-rate').textContent = temporalRate.toFixed(1) + '%';
            document.getElementById('temporal-reuse').textContent = temporalReuse.toLocaleString();
            document.getElementById('temporal-total').textContent = temporalTotal.toLocaleString();

            // 输入延迟
            const latencyAvg = metricsData.input_latency_avg_ms || 0;
            const latencyP50 = metricsData.input_latency_p50_ms || 0;
            const latencyP95 = metricsData.input_latency_p95_ms || 0;
            const latencyP99 = metricsData.input_latency_p99_ms || 0;

            document.getElementById('latency-avg').textContent = latencyAvg.toFixed(2) + ' ms';
            document.getElementById('latency-p50').textContent = latencyP50.toFixed(2) + ' ms';
            document.getElementById('latency-p95').textContent = latencyP95.toFixed(2) + ' ms';
            document.getElementById('latency-p99').textContent = latencyP99.toFixed(2) + ' ms';

            document.getElementById('latency-avg-status').innerHTML = getLatencyStatus(latencyAvg);
            document.getElementById('latency-p50-status').innerHTML = getLatencyStatus(latencyP50);
            document.getElementById('latency-p95-status').innerHTML = getLatencyStatus(latencyP95);
            document.getElementById('latency-p99-status').innerHTML = getLatencyStatus(latencyP99);

            // 渲染指标
            document.getElementById('draw-calls').textContent = (metricsData.draw_calls || 0).toLocaleString();
            document.getElementById('texture-mem').textContent = formatBytes(metricsData.texture_memory_bytes || 0);
            document.getElementById('window-count').textContent = metricsData.window_count || 0;
            document.getElementById('dirty-regions').textContent = metricsData.dirty_regions_count || 0;
            document.getElementById('dirty-frac').textContent = (metricsData.dirty_fraction_percent || 0).toFixed(1) + '%';
            document.getElementById('blur-quality').textContent = metricsData.blur_quality || 'Normal';

            // VRR
            document.getElementById('vrr-enabled').textContent = metricsData.vrr_enabled ? '✓ 启用' : '✗ 禁用';
            document.getElementById('vrr-active').textContent = metricsData.vrr_active ? '✓ 活跃' : '✗ 不活跃';
            document.getElementById('refresh-rate').textContent = (metricsData.current_refresh_rate || 0) + ' Hz';

            // 综合评分
            const score = Math.round(
                Math.max(0, Math.min(fps / 60 * 100, 100)) * 0.4 +
                Math.max(0, Math.min(100 - gpuLoad, 100)) * 0.3 +
                Math.max(0, Math.min(blurRate, 100)) * 0.2 +
                Math.max(0, Math.min(1 - latencyAvg / 50, 1) * 100) * 0.1
            );

            document.getElementById('score').textContent = score.toFixed(0) + ' / 100';

            // 汇总：指标值只通过 textContent 写入 DOM。
            document.getElementById('summary-fps').textContent = `${fps.toFixed(1)} FPS`;
            document.getElementById('summary-load').textContent = `GPU ${gpuLoad}% / CPU ${cpuLoad}%`;
            document.getElementById('summary-latency').textContent = `${latencyAvg.toFixed(2)} ms`;
            document.getElementById('summary-blur').textContent = `${blurRate.toFixed(1)}%`;

            // 优化建议
            const recommendations = [];
            if (fps < 30) recommendations.push('帧率过低，建议降低 Blur 质量或关闭部分动画效果');
            if (gpuLoad > 80) recommendations.push('GPU 负载过高，可以考虑启用 Temporal Blur 复用优化');
            if (blurRate < 50) recommendations.push('Blur 缓存命中率较低，可以通过 P7C 缓存预热改善');
            if (latencyAvg > 30) recommendations.push('输入延迟偏高，建议启用 P6A 事件优先级队列');

            if (recommendations.length === 0) {
                recommendations.push('🎉 系统运行状态良好，无明显优化建议');
            }

            let recHtml = '';
            recommendations.forEach(rec => {
                recHtml += '<div class="recommendation"><strong>💡</strong> ' + rec + '</div>';
            });
            document.getElementById('recommendations').innerHTML = recHtml;

            // 已实现的优化
            const optimizations = `
                <div style="display: grid; grid-template-columns: repeat(2, 1fr); gap: 15px;">
                    <div class="recommendation">
                        <strong>✓ P6C: 零拷贝纹理上传</strong><br/>
                        通过 PBO 优化，减少 CPU stall，收益 1-3ms
                    </div>
                    <div class="recommendation">
                        <strong>✓ P6B: GPU Fence 非阻塞同步</strong><br/>
                        消除 GPU bubble，收益 2-5ms
                    </div>
                    <div class="recommendation">
                        <strong>✓ P7A: 智能预测性渲染</strong><br/>
                        自适应 FPS，功耗降低 40-60%
                    </div>
                    <div class="recommendation">
                        <strong>✓ P7C: 智能缓存预热</strong><br/>
                        预加载常用 Shader，冷启动优化 2-5ms
                    </div>
                    <div class="recommendation">
                        <strong>✓ P4: Temporal Blur 复用</strong><br/>
                        跨帧 Blur 复用，减少渲染预算
                    </div>
                    <div class="recommendation">
                        <strong>✓ P3: 自适应模糊</strong><br/>
                        根据内容动态调整 Blur 质量
                    </div>
                </div>
            `;
            document.getElementById('optimizations').innerHTML = optimizations;
        }

        // 初始化时间
        const now = new Date();
        document.getElementById('timestamp').textContent = '生成时间：' + now.toLocaleString('zh-CN');
        document.getElementById('footer-time').textContent = now.toLocaleString('zh-CN');

        // 更新指标
        updateMetrics();
    </script>
</body>
</html>
EOF
}

# 主程序
require_commands jwm-tool jq base64

echo "🚀 JWM 性能报告生成器"
echo "====================="
echo ""

# 收集样本
IFS=',' read -r avg_fps avg_frame < <(collect_samples)

# 获取当前指标，并把采样均值写进报告数据，而不是收集后丢弃。
m="$(get_metrics)" || {
    echo "无法从正在运行的 JWM 获取指标。" >&2
    exit 1
}
m="$(jq --argjson avg_fps "$avg_fps" --argjson avg_frame "$avg_frame" \
    '. + {
        sampled_avg_fps: $avg_fps,
        sampled_avg_frame_time_ms: $avg_frame
    }' <<<"$m")"

echo ""
echo "📝 生成 HTML 报告..."

# Base64 keeps arbitrary JSON strings from terminating the script element or
# corrupting a sed replacement. The generated report remains fully offline.
metrics_b64="$(jq -c '.' <<<"$m" | base64 | tr -d '\n')"

# Reserve a unique name, render to a hidden sibling, and publish with one
# atomic rename. Concurrent invocations cannot overwrite or interleave.
REPORT_TMP=$(mktemp --tmpdir="$OUTPUT_DIR" '.jwm_performance_report.XXXXXXXX.tmp')
cleanup_report() {
    if [[ -n ${REPORT_TMP:-} ]]; then
        rm -f -- "$REPORT_TMP"
    fi
}
trap cleanup_report EXIT
report_name=${REPORT_TMP##*/}
report_token=${report_name#.jwm_performance_report.}
report_token=${report_token%.tmp}
REPORT_FILE="$OUTPUT_DIR/jwm_performance_report_$(date +%Y%m%d_%H%M%S)_${report_token}.html"

# 生成自包含 HTML。
generate_html_report "$metrics_b64" "$REPORT_TMP"
# Treat the destination as one path, even if another process races us by
# creating a directory with the reserved report name.
mv -T -- "$REPORT_TMP" "$REPORT_FILE"
REPORT_TMP=
trap - EXIT

echo "✓ 报告已生成: $REPORT_FILE"
echo ""
echo "📊 报告内容包括:"
echo "  • FPS 和帧时间分析"
echo "  • GPU/CPU 负载指标"
echo "  • Blur 缓存优化效果"
echo "  • Temporal Blur 复用率 (P4)"
echo "  • 输入延迟分析"
echo "  • 渲染效率指标"
echo "  • VRR 状态"
echo "  • 优化建议"
echo "  • 已实现优化清单"
echo ""
echo "🌐 在浏览器中打开:"
echo "  xdg-open $REPORT_FILE"
