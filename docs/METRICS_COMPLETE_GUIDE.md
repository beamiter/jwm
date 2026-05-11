# JWM 完整 Metrics 展现指南

## 概述

本文档介绍如何**完整展现 jwm 的所有性能指标 (metrics)**，包括实时监控、性能报告生成、数据对比分析等多个角度。

### 完整指标体系

```
JWM 性能指标体系 (CompositorMetrics)
├── 📊 FPS & 时间指标
│   ├── fps: 当前帧率
│   ├── frame_count: 总帧数
│   ├── avg_frame_time_ms: 平均帧时间
│   ├── max_frame_time_ms: 最大帧时间 (峰值)
│   └── min_frame_time_ms: 最小帧时间 (最优)
│
├── ⚡ 资源负载
│   ├── gpu_load_percent: GPU 负载 (0-100%)
│   └── cpu_load_percent: CPU 负载 (0-100%)
│
├── 🎯 Blur 缓存优化
│   ├── blur_cache_hits: 缓存命中数
│   ├── blur_cache_misses: 缓存未命中数
│   └── blur_cache_hit_rate: 命中率百分比
│
├── ⏱️ Temporal Blur 优化 (P4)
│   ├── temporal_blur_reuse_count: 复用计数
│   ├── temporal_blur_total_count: 总计数
│   └── temporal_blur_reuse_rate: 复用率百分比
│
├── 🎨 渲染效率
│   ├── draw_calls: 绘制调用数
│   ├── texture_memory_bytes: 纹理内存占用
│   ├── window_count: 窗口数量
│   ├── dirty_regions_count: 脏区域数
│   ├── dirty_fraction_percent: 脏区域占比
│   └── blur_quality: Blur 质量等级
│
├── 🎮 VRR 可变刷新率
│   ├── vrr_enabled: VRR 是否启用
│   ├── vrr_active: VRR 是否活跃
│   └── current_refresh_rate: 当前刷新率 (Hz)
│
└── ⌨️ 输入延迟
    ├── input_latency_avg_ms: 平均延迟
    ├── input_latency_p50_ms: P50 延迟 (50分位)
    ├── input_latency_p95_ms: P95 延迟 (95分位)
    └── input_latency_p99_ms: P99 延迟 (99分位)
```

---

## 方案一：实时仪表板监控 (Dashboard)

### 基本用法

```bash
# 启动实时监控 (默认每秒刷新)
./tools/metrics_dashboard.sh

# 自定义刷新间隔 (2秒)
./tools/metrics_dashboard.sh -i 2

# 仅显示关键指标
./tools/metrics_dashboard.sh -q

# 显示全部详细指标
./tools/metrics_dashboard.sh -f
```

### 单个指标监控

```bash
# 仅监控 FPS
./tools/metrics_dashboard.sh --fps

# 仅监控负载
./tools/metrics_dashboard.sh --load

# 仅监控 Blur 缓存
./tools/metrics_dashboard.sh --blur

# 仅监控 VRR
./tools/metrics_dashboard.sh --vrr

# 仅监控输入延迟
./tools/metrics_dashboard.sh --latency
```

### 单次展示

```bash
# 显示一次所有指标 (不持续刷新)
./tools/metrics_dashboard.sh -s

# 单次显示快速指标
./tools/metrics_dashboard.sh -s -q
```

### 导出数据

```bash
# 导出当前指标为 JSON 格式
./tools/metrics_dashboard.sh --export metrics_snapshot.json

# 可用于后续分析或导入其他工具
cat metrics_snapshot.json | jq '.fps, .gpu_load_percent'
```

### 实时监控示例

```bash
# 实时监控，观察 Alt+Tab 操作的性能影响
$ ./tools/metrics_dashboard.sh

════════════════════════════════════════════════════════════════
  JWM 性能监控仪表板 - 2026-05-11 10:30:45
════════════════════════════════════════════════════════════════

📋 综合概览
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  帧率性能             : 🟢 59.5 fps
  GPU 负载             : 🟢 42%
  CPU 负载             : 🟢 35%
  Blur 缓存命中率       : 87.3%
  输入延迟             : 🟢 12.45 ms

📊 FPS & 时间指标
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  当前帧率             : 59.5 fps
  平均帧时间          : 16.81 ms
  最大帧时间          : 22.34 ms
  最小帧时间          : 15.12 ms
  总帧数               : 1234567

⚡ 负载指标
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  GPU 负载             : [==================]----- ] 42%
  CPU 负载             : [===============----------- ] 35%

... (更多指标)

按 Ctrl+C 退出，下一次更新在 1s 后
```

---

## 方案二：性能报告生成 (HTML Report)

### 生成完整 HTML 报告

```bash
# 生成 HTML 性能报告
./tools/generate_report.sh

# 指定输出目录
./tools/generate_report.sh /tmp/reports

# 打开报告 (Linux)
xdg-open jwm_performance_report_20260511_103045.html

# 打开报告 (macOS)
open jwm_performance_report_20260511_103045.html
```

### 报告包含内容

✅ **综合性能评分** (0-100)
- 基于 FPS、负载、缓存命中率、延迟的加权评分

✅ **FPS & 时间分析**
- 当前帧率
- 平均/最大/最小帧时间
- 总帧数统计

✅ **资源负载可视化**
- GPU 负载柱状图
- CPU 负载柱状图
- 实时百分比显示

✅ **Blur 缓存优化效果**
- 缓存命中率 (%)
- 命中/未命中计数
- 缓存有效性评估

✅ **Temporal Blur (P4) 优化**
- 复用率统计
- 复用/总计数对比
- 优化效果验证

✅ **输入延迟分析**
- 平均延迟
- 分位延迟 (P50/P95/P99)
- 性能评级 (优秀/良好/需改进)

✅ **渲染效率指标**
- 绘制调用数 (draw calls)
- 纹理内存占用
- 窗口数量
- 脏区域统计

✅ **VRR 状态**
- VRR 启用/禁用状态
- 当前活跃状态
- 刷新率 (Hz)

✅ **优化建议**
- 智能诊断和改进建议
- 基于当前数据的个性化优化方案

✅ **已实现优化清单**
- P6C: 零拷贝纹理上传
- P6B: GPU Fence 同步
- P7A: 智能预测性渲染
- P7C: 智能缓存预热
- P4: Temporal Blur 复用
- P3: 自适应模糊

### 报告示例

```
═══════════════════════════════════════════════════════════════
  JWM 性能分析报告
═══════════════════════════════════════════════════════════════

综合性能评分: 87 / 100  🟢 优秀

当前帧率         : 59.5 FPS
平均帧时间       : 16.81 ms
最大帧时间       : 22.34 ms (spike)

GPU 负载: 42%  ████████░░░░░░░░░░░░
CPU 负载: 35%  ███████░░░░░░░░░░░░░░

Blur 缓存命中率: 87.3%
  命中: 4,521 次
  未命中: 651 次

输入延迟分析:
  平均:     12.45 ms  ✓ 优秀
  P50:      10.23 ms  ✓ 优秀
  P95:      18.34 ms  ✓ 优秀
  P99:      21.56 ms  ✓ 良好

═══════════════════════════════════════════════════════════════
```

---

## 方案三：性能对比分析 (Comparison)

### 对比优化前后的性能

```bash
# 1. 保存优化前的基线
./tools/compare_metrics.sh save baseline_before.json

# 2. 进行优化 (启用新特性、调整参数等)
# ... 编辑配置、应用补丁等 ...

# 3. 保存优化后的当前指标
./tools/compare_metrics.sh save current_after.json

# 4. 对比两个快照
./tools/compare_metrics.sh compare baseline_before.json current_after.json
```

### 对比结果示例

```
╔════════════════════════════════════════════════════════════════════╗
║              JWM 性能对比分析报告                                  ║
╚════════════════════════════════════════════════════════════════════╝

📊 关键指标对比
═══════════════════════════════════════════════════════════════════

【FPS 帧率】
  基线:     50.3 fps
  当前:     59.5 fps
  变化:     +9.2 fps (+18.3%)
  ✓ 性能改善

【输入延迟】
  基线:     20.34ms
  当前:     12.45ms
  改善:     7.89 ms (+38.8%)
  ✓ 延迟降低

【GPU 负载】
  基线:     65%
  当前:     42%
  降低:     23% (+35.4%)
  ✓ 负载降低

【Blur 缓存命中率】
  基线:     72.3%
  当前:     87.3%
  提升:     15.0% (+20.8%)
  ✓ 命中率提升

═══════════════════════════════════════════════════════════════════

📈 详细指标对比表

指标                        | 基线          | 当前          | 变化
─────────────────────────────────────────────────────────────────
FPS                         | 50.30         | 59.50         | +9.20
平均帧时间 (ms)            | 19.88         | 16.81         | -3.07
最大帧时间 (ms)            | 35.24         | 22.34         | -12.90
平均延迟 (ms)              | 20.34         | 12.45         | -7.89
P95 延迟 (ms)              | 28.56         | 18.34         | -10.22
GPU 负载                    | 65%           | 42%           | -23%
CPU 负载                    | 58%           | 35%           | -23%
Blur 缓存命中率            | 72.1%         | 87.3%         | +15.2%
脏区域占比                 | 45.2%         | 28.3%         | -16.9%

═══════════════════════════════════════════════════════════════════
```

### 实时性能变化监控

```bash
# 实时监控相对于初始快照的性能变化
./tools/compare_metrics.sh monitor

╔════════════════════════════════════════════════════════════════════╗
║           JWM 实时性能监控 - 10:30:45                             ║
╚════════════════════════════════════════════════════════════════════╝

  FPS:      59.5 fps    📈 +2.3
  GPU 负载: 42%         📉 -5%
  延迟:     12.45ms     ✓ 改善 -3.21 ms
```

---

## 方案四：编程接口查询 (API)

### 使用 jwm-tool 查询指标

```bash
# 获取完整的 JSON 格式指标
jwm-tool msg get_metrics --raw | jq '.data'

# 提取特定指标
jwm-tool msg get_metrics --raw | jq '.data.fps'
jwm-tool msg get_metrics --raw | jq '.data | {fps, gpu_load_percent, avg_frame_time_ms}'

# 持续监控指标变化 (bash 脚本)
while true; do
    clear
    echo "=== 实时指标 ==="
    jwm-tool msg get_metrics --raw | jq '.data | {
        fps,
        avg_frame_time_ms,
        gpu_load_percent,
        cpu_load_percent,
        blur_cache_hit_rate,
        input_latency_avg_ms
    }'
    sleep 1
done
```

### JSON 输出示例

```json
{
  "fps": 59.5,
  "frame_count": 1234567,
  "avg_frame_time_ms": 16.81,
  "max_frame_time_ms": 22.34,
  "min_frame_time_ms": 15.12,
  "gpu_load_percent": 42,
  "cpu_load_percent": 35,
  "draw_calls": 1250,
  "texture_memory_bytes": 524288000,
  "blur_cache_hits": 4521,
  "blur_cache_misses": 651,
  "blur_cache_hit_rate": 87.3,
  "temporal_blur_reuse_count": 892,
  "temporal_blur_total_count": 1000,
  "temporal_blur_reuse_rate": 89.2,
  "dirty_regions_count": 15,
  "dirty_fraction_percent": 28.3,
  "window_count": 8,
  "blur_quality": "High",
  "vrr_enabled": true,
  "vrr_active": true,
  "current_refresh_rate": 144,
  "input_latency_avg_ms": 12.45,
  "input_latency_p50_ms": 10.23,
  "input_latency_p95_ms": 18.34,
  "input_latency_p99_ms": 21.56
}
```

### 自定义脚本示例

```bash
#!/bin/bash
# 自定义性能监控脚本

while true; do
    metrics=$(jwm-tool msg get_metrics --raw' | jq '.data')
    
    fps=$(echo "$metrics" | jq '.fps')
    gpu=$(echo "$metrics" | jq '.gpu_load_percent')
    latency=$(echo "$metrics" | jq '.input_latency_avg_ms')
    
    # 性能评估
    if (( $(echo "$fps < 30" | bc -l) )); then
        echo "⚠️ 低帧率: $fps fps"
    fi
    
    if (( gpu > 80 )); then
        echo "⚠️ GPU 过载: ${gpu}%"
    fi
    
    if (( $(echo "$latency > 30" | bc -l) )); then
        echo "⚠️ 高延迟: $latency ms"
    fi
    
    sleep 5
done
```

---

## 方案五：综合分析工作流

### 完整的性能优化评估流程

```bash
#!/bin/bash
# 完整的性能评估和优化验证流程

echo "🚀 JWM 性能评估工作流"
echo "═════════════════════════════════════════════"

# 1️⃣ 保存基线
echo "第1步: 保存优化前的基线..."
./tools/compare_metrics.sh save baseline_$(date +%Y%m%d).json

# 2️⃣ 生成基线报告
echo "第2步: 生成基线性能报告..."
./tools/generate_report.sh baseline_reports/

# 3️⃣ 运行优化脚本 (示例)
echo "第3步: 应用优化..."
# cargo build --release
# systemctl restart jwm  # 或相应的启动方式

# 4️⃣ 等待系统稳定
echo "第4步: 等待系统稳定 (60秒)..."
sleep 60

# 5️⃣ 保存优化后的指标
echo "第5步: 保存优化后的指标..."
./tools/compare_metrics.sh save optimized_$(date +%Y%m%d).json

# 6️⃣ 生成优化后的报告
echo "第6步: 生成优化后的性能报告..."
./tools/generate_report.sh optimized_reports/

# 7️⃣ 对比分析
echo "第7步: 性能对比分析..."
./tools/compare_metrics.sh compare \
    baseline_$(date +%Y%m%d).json \
    optimized_$(date +%Y%m%d).json

echo ""
echo "✅ 评估完成！"
echo ""
echo "📊 查看详细报告:"
echo "  baseline_reports/jwm_performance_report_*.html"
echo "  optimized_reports/jwm_performance_report_*.html"
```

### 运行结果

```bash
$ ./performance_evaluation.sh

🚀 JWM 性能评估工作流
═════════════════════════════════════════════
第1步: 保存优化前的基线...
✓ 指标已保存到: baseline_20260511.json

第2步: 生成基线性能报告...
⏳ 收集 10 个样本 (每 1s 一次)...
  [1/10] FPS: 50.3, 帧时: 19.88ms
  [2/10] FPS: 50.5, 帧时: 19.77ms
  ...
✓ 报告已生成: baseline_reports/jwm_performance_report_20260511_103045.html

第3步: 应用优化...
  Compiling jwm v0.1.0...
  Finished release...

... (优化过程)

第5步: 保存优化后的指标...
✓ 指标已保存到: optimized_20260511.json

第7步: 性能对比分析...

【FPS 帧率】
  基线:     50.3 fps
  当前:     59.5 fps
  变化:     +9.2 fps (+18.3%)
  ✓ 性能改善

... (更多数据)

✅ 评估完成！
```

---

## 性能指标解读指南

### 帧率 (FPS)

| FPS 范围 | 评级 | 说明 |
|---------|------|------|
| ≥ 60    | 🟢 优秀 | 流畅体验，适合桌面应用 |
| 30-60   | 🟡 良好 | 可接受，但可能有卡顿感 |
| < 30    | 🔴 需改进 | 明显卡顿，影响用户体验 |

### GPU/CPU 负载

| 负载范围 | 评级 | 说明 |
|---------|------|------|
| 0-40%   | 🟢 低负载 | 系统有充足资源，可承受突发负载 |
| 40-70%  | 🟡 适中 | 系统运行正常 |
| 70-100% | 🔴 高负载 | 接近上限，可能导致卡顿 |

### 输入延迟

| 延迟范围 | 评级 | 用户体验 |
|---------|------|---------|
| < 20ms  | 🟢 优秀 | 无感知延迟，输入响应迅速 |
| 20-30ms | 🟡 良好 | 基本感受不到延迟 |
| > 30ms  | 🔴 差 | 明显感受到延迟 |

### Blur 缓存命中率

| 命中率范围 | 说明 |
|---------|------|
| > 80%   | 缓存策略有效 |
| 60-80%  | 缓存命中率一般 |
| < 60%   | 需要优化缓存策略 |

---

## 常见使用场景

### 场景 1: 快速性能检查

```bash
# 仅显示关键指标，快速了解当前性能状态
./tools/metrics_dashboard.sh -q
```

### 场景 2: 长期监控

```bash
# 实时监控，每5秒刷新一次
./tools/metrics_dashboard.sh -i 5

# 后台运行，定期记录数据
while true; do
    ./tools/metrics_dashboard.sh --export metrics_$(date +%H%M%S).json
    sleep 300  # 每5分钟记录一次
done
```

### 场景 3: 优化效果验证

```bash
# 对比优化前后
./tools/compare_metrics.sh compare baseline.json after_optimization.json

# 生成详细报告供分析
./tools/generate_report.sh
```

### 场景 4: 性能异常诊断

```bash
# 导出实时数据用于离线分析
for i in {1..60}; do
    ./tools/metrics_dashboard.sh --export debug_${i}.json
    sleep 1
done

# 分析数据序列，找出异常
cat debug_*.json | jq '.fps' | sort -n
```

---

## 工具集总结

| 工具 | 功能 | 使用场景 |
|------|------|---------|
| `metrics_dashboard.sh` | 实时仪表板监控 | 快速查看、实时监控 |
| `generate_report.sh` | 生成 HTML 报告 | 详细分析、报告展示 |
| `compare_metrics.sh` | 性能对比分析 | 优化效果验证、前后对比 |
| `jwm-tool ipc` | 编程接口 | 自定义脚本、集成 |

---

## 安装和设置

```bash
# 1. 确保 jwm-tool 已安装
which jwm-tool || ./tools/install_jwm_scripts.sh

# 2. 给脚本添加执行权限
chmod +x tools/metrics_dashboard.sh
chmod +x tools/generate_report.sh
chmod +x tools/compare_metrics.sh

# 3. (可选) 创建别名便于快速使用
echo "alias jwm-dash='~/projects/jwm/tools/metrics_dashboard.sh'" >> ~/.bashrc
echo "alias jwm-report='~/projects/jwm/tools/generate_report.sh'" >> ~/.bashrc
echo "alias jwm-compare='~/projects/jwm/tools/compare_metrics.sh'" >> ~/.bashrc
source ~/.bashrc

# 4. 验证安装
jwm-dash --help
jwm-report
jwm-compare save baseline.json
```

---

## 总结

通过以上 4 种展现方式，可以**完整地从多个角度展现 jwm 的所有性能指标**：

🎯 **实时仪表板** → 快速监控当前状态
📊 **HTML 报告** → 详细分析和可视化
📈 **性能对比** → 优化效果验证
🔧 **编程接口** → 自定义集成和脚本

根据需要选择合适的工具和方式，完整展现系统性能！
