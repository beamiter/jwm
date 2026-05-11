# JWM Metrics 快速开始指南

## 🚀 30秒快速开始

```bash
# 查看实时性能指标（推荐首选）
./tools/metrics_dashboard.sh

# 生成详细的 HTML 性能报告
./tools/generate_report.sh

# 对比优化前后的性能
./tools/compare_metrics.sh save baseline.json
./tools/compare_metrics.sh save after.json
./tools/compare_metrics.sh compare baseline.json after.json
```

---

## 📊 四种展现方式对比

### 1️⃣ 实时仪表板 (最直观)

```bash
# 启动仪表板，实时显示所有指标
./tools/metrics_dashboard.sh

# 效果：
# ✓ 彩色终端输出，易于阅读
# ✓ 自动刷新，实时监控
# ✓ 包含性能评级和柱状图
# ✓ 最适合快速检查和持续监控
```

**最佳用途**：
- 快速性能检查
- 实时监控系统状态
- 观察特定操作的性能影响
- 性能调试和故障排查

**命令示例**：
```bash
./tools/metrics_dashboard.sh           # 默认，每秒刷新
./tools/metrics_dashboard.sh -q        # 仅关键指标
./tools/metrics_dashboard.sh -i 2      # 每2秒刷新
./tools/metrics_dashboard.sh --fps     # 仅显示 FPS 指标
```

---

### 2️⃣ HTML 性能报告 (最全面)

```bash
# 生成完整的 HTML 性能分析报告
./tools/generate_report.sh

# 效果：
# ✓ 在浏览器中打开，精美界面
# ✓ 包含所有指标的详细图表
# ✓ 自动生成优化建议
# ✓ 可保存、分享、打印
```

**最佳用途**：
- 详细的性能分析
- 生成性能报告给团队
- 存档历史性能数据
- 可视化性能指标

**生成和查看**：
```bash
./tools/generate_report.sh
# 输出: jwm_performance_report_20260511_103045.html

# 用浏览器打开
xdg-open jwm_performance_report_20260511_103045.html
```

---

### 3️⃣ 性能对比分析 (最有价值)

```bash
# 保存优化前的基线
./tools/compare_metrics.sh save baseline.json

# (进行优化...)

# 保存优化后的结果
./tools/compare_metrics.sh save optimized.json

# 对比分析
./tools/compare_metrics.sh compare baseline.json optimized.json

# 效果：
# ✓ 清晰的前后对比表格
# ✓ 显示改进百分比
# ✓ 评估优化效果
```

**最佳用途**：
- 验证优化是否生效
- 量化性能改进
- 对比不同配置
- 性能基准测试

**完整工作流**：
```bash
# 1. 保存基线
./tools/compare_metrics.sh save before.json

# 2. 进行优化 (启用 P7A, P7C 等)
# ... 修改配置或代码 ...

# 3. 保存结果
./tools/compare_metrics.sh save after.json

# 4. 对比
./tools/compare_metrics.sh compare before.json after.json
```

---

### 4️⃣ 编程接口 (最灵活)

```bash
# 直接查询 JSON 格式的指标
jwm-tool msg get_metrics --raw' | jq '.data'

# 效果：
# ✓ 原始数据，便于自定义处理
# ✓ 可集成到其他工具或脚本
# ✓ 支持数据分析和可视化
```

**最佳用途**：
- 集成到自定义脚本
- 导出数据进行分析
- 自动化监控和告警
- 与其他工具集成

**使用示例**：
```bash
# 提取单个指标
jwm-tool msg get_metrics --raw' | jq '.data.fps'

# 导出为 JSON 文件
jwm-tool msg get_metrics --raw' | jq '.data' > metrics.json

# 自定义查询
jwm-tool msg get_metrics --raw' | jq '.data | {
  fps,
  gpu_load_percent,
  input_latency_avg_ms,
  blur_cache_hit_rate
}'
```

---

## 📈 完整指标列表 (21 个指标)

```
FPS & 时间指标
├── fps                           # 当前帧率
├── frame_count                  # 总帧数
├── avg_frame_time_ms            # 平均帧时间
├── max_frame_time_ms            # 最大帧时间
└── min_frame_time_ms            # 最小帧时间

资源负载 (2个)
├── gpu_load_percent             # GPU 负载
└── cpu_load_percent             # CPU 负载

Blur 缓存优化 (3个)
├── blur_cache_hits              # 缓存命中数
├── blur_cache_misses            # 缓存未命中数
└── blur_cache_hit_rate          # 命中率 (%)

Temporal Blur P4 优化 (3个)
├── temporal_blur_reuse_count    # 复用计数
├── temporal_blur_total_count    # 总计数
└── temporal_blur_reuse_rate     # 复用率 (%)

渲染效率 (6个)
├── draw_calls                   # 绘制调用数
├── texture_memory_bytes         # 纹理内存
├── window_count                 # 窗口数量
├── dirty_regions_count          # 脏区域数
├── dirty_fraction_percent       # 脏区域占比 (%)
└── blur_quality                 # Blur 质量等级

VRR 可变刷新率 (3个)
├── vrr_enabled                  # VRR 启用状态
├── vrr_active                   # VRR 活跃状态
└── current_refresh_rate         # 当前刷新率 (Hz)

输入延迟 (4个)
├── input_latency_avg_ms         # 平均延迟
├── input_latency_p50_ms         # P50 延迟
├── input_latency_p95_ms         # P95 延迟
└── input_latency_p99_ms         # P99 延迟
```

---

## 🎯 按场景选择工具

### 场景 A: "我想快速看一下当前性能"
```bash
→ 使用仪表板
./tools/metrics_dashboard.sh -q
```

### 场景 B: "我需要详细的性能分析报告"
```bash
→ 生成 HTML 报告
./tools/generate_report.sh
xdg-open jwm_performance_report_*.html
```

### 场景 C: "我要验证我的优化是否有效"
```bash
→ 使用对比分析
./tools/compare_metrics.sh save before.json
# 进行优化...
./tools/compare_metrics.sh save after.json
./tools/compare_metrics.sh compare before.json after.json
```

### 场景 D: "我要自动化监控或集成到脚本"
```bash
→ 使用编程接口
jwm-tool msg get_metrics --raw' | jq '.data'
```

---

## 💡 实用命令速记

### 仪表板命令
```bash
# 基础用法
./tools/metrics_dashboard.sh                    # 实时监控

# 单次显示
./tools/metrics_dashboard.sh -s                 # 显示一次
./tools/metrics_dashboard.sh -s -q              # 显示快速指标

# 自定义刷新
./tools/metrics_dashboard.sh -i 2               # 每2秒刷新
./tools/metrics_dashboard.sh -i 5               # 每5秒刷新

# 单个指标
./tools/metrics_dashboard.sh --fps              # 仅 FPS
./tools/metrics_dashboard.sh --load             # 仅负载
./tools/metrics_dashboard.sh --blur             # 仅缓存
./tools/metrics_dashboard.sh --latency          # 仅延迟

# 导出数据
./tools/metrics_dashboard.sh --export data.json # 导出 JSON
```

### 报告生成命令
```bash
# 生成报告
./tools/generate_report.sh                      # 默认当前目录
./tools/generate_report.sh /tmp/reports         # 指定目录

# 自动打开报告 (Linux)
./tools/generate_report.sh && xdg-open jwm_performance_report_*.html

# 自动打开报告 (macOS)
./tools/generate_report.sh && open jwm_performance_report_*.html
```

### 对比分析命令
```bash
# 保存快照
./tools/compare_metrics.sh save baseline.json   # 保存基线
./tools/compare_metrics.sh save current.json    # 保存当前

# 对比分析
./tools/compare_metrics.sh compare baseline.json current.json

# 实时监控变化
./tools/compare_metrics.sh monitor              # 相对初始快照的变化
```

### 直接查询
```bash
# 获取完整指标
jwm-tool msg get_metrics --raw' | jq '.data'

# 获取 FPS
jwm-tool msg get_metrics --raw' | jq '.data.fps'

# 获取多个指标
jwm-tool msg get_metrics --raw' | jq '.data | {fps, gpu_load_percent, input_latency_avg_ms}'

# 输出为 JSON 文件
jwm-tool msg get_metrics --raw' | jq '.data' > snapshot.json

# 定期采样 (每秒一次，共60次)
for i in {1..60}; do 
    jwm-tool msg get_metrics --raw' | jq '.data' >> metrics_log.json
    sleep 1
done
```

---

## 🏆 优化建议速查表

| 症状 | 原因 | 解决方案 |
|------|------|---------|
| FPS < 30 | GPU 超载 | 降低 Blur 质量，启用 Temporal Blur (P4) |
| GPU > 80% | 渲染压力大 | 启用 P7A 自适应 FPS，减少 draw calls |
| 输入延迟 > 30ms | 事件处理慢 | 启用 P6A 优先级队列 |
| Blur 命中率 < 50% | 缓存策略差 | 启用 P7C 缓存预热 |
| 功耗高 | FPS 固定 60 | 启用 P7A 智能预测渲染（可省电 40-60%） |
| 脏区域占比 > 50% | 更新频繁 | 启用 P6C PBO 零拷贝优化 |

---

## 📚 完整文档索引

```
docs/
├── P6-P7_COMPLETE_OPTIMIZATIONS.md    # P6-P7 优化总结
├── METRICS_COMPLETE_GUIDE.md          # 完整 metrics 指南
└── METRICS_QUICK_START.md             # 本文件

tools/
├── metrics_dashboard.sh               # 实时仪表板
├── generate_report.sh                 # 报告生成
├── compare_metrics.sh                 # 对比分析
└── jwm-tool (IPC 接口)                # 编程接口
```

---

## ✅ 常见问题

### Q1: 如何确保获取的指标是准确的？

A: 以下几点可确保准确性：
- 让系统运行至少 5-10 秒，使指标稳定
- 避免在进行其他高负载操作时采样
- 多次采样取平均值
- 对比前后数据时，保持相同的系统状态

### Q2: 如何长期监控性能趋势？

A: 可创建定时采样脚本：
```bash
#!/bin/bash
while true; do
    timestamp=$(date +%Y%m%d_%H%M%S)
    ./tools/metrics_dashboard.sh --export "metrics_$timestamp.json"
    sleep 300  # 每5分钟采样一次
done
```

### Q3: 如何找出性能瓶颈？

A: 按优先级检查：
1. 查看 FPS - 如果 < 30，说明有性能问题
2. 查看 GPU/CPU 负载 - 定位是哪个资源有瓶颈
3. 查看 Blur 缓存命中率 - 可能需要优化缓存
4. 查看输入延迟 - 影响用户体验的关键指标
5. 查看脏区域占比 - 说明渲染效率

### Q4: HTML 报告如何分享？

A: 直接发送 HTML 文件即可：
```bash
# 报告是独立的 HTML 文件，无需其他依赖
./tools/generate_report.sh
# 将生成的 jwm_performance_report_*.html 发送给其他人
# 他们可以直接用浏览器打开查看
```

---

## 🎓 学习路径

1. **入门** → 使用仪表板快速了解系统性能
2. **进阶** → 生成 HTML 报告做详细分析  
3. **实战** → 使用对比工具验证优化效果
4. **高级** → 编写自定义脚本自动化监控

---

## 🚀 下一步

1. 立即试用：`./tools/metrics_dashboard.sh`
2. 生成报告：`./tools/generate_report.sh`
3. 验证优化：按照 [P6-P7_COMPLETE_OPTIMIZATIONS.md](./P6-P7_COMPLETE_OPTIMIZATIONS.md) 启用优化特性
4. 对比效果：`./tools/compare_metrics.sh compare before.json after.json`

---

**现在就开始探索 jwm 的完整性能指标吧！** 🎉
