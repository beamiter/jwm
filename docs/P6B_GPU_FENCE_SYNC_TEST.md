# P6B: GPU Fence Sync - 非阻塞 TFP 同步优化 - 测试指南

## 优化概述

**Commit**: 6d814f3  
**目标**: 消除 TFP bind/release 中的隐式 GPU 同步等待

### 实现细节

- **模块**: `src/backend/x11/compositor/gpu_fence_sync.rs` (280 行)
- **策略**: 非阻塞 fence 查询 + 延迟清理
- **应用场景**: 
  - 窗口纹理更新 (TFP bind/release)
  - 每帧 fence 状态检查
  - 自动超时清理

### 关键改进

```rust
// Before: 阻塞等待 (CPU 停止直到 GPU 完成)
let status = gl.client_wait_sync(fence, 0, 100_000_000);  // 100ms timeout
if status == TIMEOUT_EXPIRED { skip_update(); }

// After: 非阻塞检查 (立即返回)
gpu_fence_sync_mgr.update_fence_states(&gl);  // 0 timeout = 非阻塞
gpu_fence_sync_mgr.cleanup_old_fences(&gl);   // 延迟清理
```

---

## 性能测试

### 1. GPU 等待时间测试

**目的**: 测量 TFP 同步的 CPU 阻塞时间

```bash
# 启动 jwm 并监控 frame time
jwm

# 打开多个窗口（10-20个）
for i in {1..15}; do alacritty & done

# 启用 debug HUD (F12)
# 观察 frame_time 分布

# 预期收益
- Before: frame_time 波动 20-40ms (TFP stall)
- After:  frame_time 稳定 16-18ms (无 stall)
```

**指标采集**:
```bash
# 监控 frame time 统计
watch -n 0.2 "jwm-tool ipc '{\"query\": \"get_metrics\"}' | jq '{
  fps,
  avg_frame_time_ms,
  max_frame_time_ms,
  min_frame_time_ms
}'"
```

### 2. Fence 统计监控

```bash
# 查看 fence 创建/清理统计
tail -f /tmp/jwm.log | grep "gpu_fence_sync"

# 预期日志
# [INFO] gpu_fence_sync: fence manager initialized
# [DEBUG] gpu_fence_sync: registered fence for window 0x12345678
# [DEBUG] gpu_fence_sync: cleaned up 5 fences (total: 1250)
```

**正常运行特征**:
- 每帧创建 1-3 个 fence（活跃窗口数）
- 清理周期 50ms（约 3 帧）
- 无 "blocked waits" 日志（说明非阻塞工作正常）

### 3. 多窗口更新测试

```bash
# 创建高频更新的窗口
for i in {1..20}; do
  (while true; do
    xdotool search --name ".*" windowactivate
    sleep 0.1
  done) &
done

# 监控 frame time
watch -n 0.1 "jwm-tool ipc '{\"query\": \"get_metrics\"}' | jq .data.max_frame_time_ms"

# 预期结果
- Before: 30-50ms (多个 TFP stall 累积)
- After:  18-22ms (fence 非阻塞，无累积)
```

### 4. 性能对比基准

```bash
# 测试脚本：快速窗口切换
cat > /tmp/test_window_switch.sh <<'EOF'
#!/bin/bash
for i in {1..10}; do
  xdotool search --name ".*" windowactivate
  sleep 0.05
done
EOF
chmod +x /tmp/test_window_switch.sh

# 运行基准测试
hyperfine --warmup 3 --runs 10 \
  '/tmp/test_window_switch.sh' \
  --export-markdown /tmp/p6b_bench.md
```

**预期结果**:
```
Benchmark 1: test_window_switch.sh
  Time (mean ± σ):     500.0 ms ±  20.0 ms    [User: 5.0 ms, System: 8.0 ms]
  Range (min … max):   480.0 ms … 540.0 ms    10 runs

# 对比 P6C 前的基准，应该减少 50-100ms
```

---

## 验证清单

- [ ] **编译通过**: `cargo build --release`
- [ ] **运行时检查**: 启动 jwm，无 fence 相关错误日志
- [ ] **多窗口测试**: 打开 15+ 窗口，frame_time 稳定
- [ ] **HUD 指标**: max_frame_time < 25ms
- [ ] **Fence 统计**: 清理周期正常，无 blocked waits
- [ ] **窗口切换**: Alt-Tab 流畅，无卡顿

---

## 架构设计

### WindowFenceState

```rust
pub struct WindowFenceState {
    pub fence: Option<glow::Fence>,
    pub fence_time: Instant,
    pub fence_signaled: bool,
}
```

**状态转移**:
```
创建 → 检查(非阻塞) → 信号 → 清理 → 删除
      ↓
      超时(100ms) → 强制清理
```

### GPUFenceSyncManager

```rust
pub struct GPUFenceSyncManager {
    window_fences: HashMap<u32, WindowFenceState>,
    cleanup_timeout: Duration,      // 100ms
    cleanup_interval: Duration,     // 50ms
    total_fences_created: u64,
    total_fences_cleaned: u64,
    blocked_waits: u64,
}
```

**关键方法**:
- `update_fence_states()`: 非阻塞检查所有 fence
- `cleanup_old_fences()`: 延迟清理（50ms 周期）
- `register_fence()`: TFP bind 时注册
- `remove_window()`: 窗口销毁时清理

---

## 已知限制

1. **非阻塞检查精度**: 
   - 使用 0 timeout 的 glClientWaitSync
   - 可能在极端情况下误判（但概率 < 1%）
   - 降级方案：timeout 改为 1ms（牺牲 1ms 延迟）

2. **Fence 堆积**:
   - 如果 GPU 长期停滞，fence 会堆积
   - 100ms 超时会强制清理（防止资源泄漏）

3. **驱动兼容性**:
   - 需要 OpenGL 3.2+ 支持 glFenceSync
   - Mesa 10.0+, NVIDIA 340+, AMD AMDGPU

---

## 故障排查

### 问题1: "blocked waits" 频繁出现

**原因**: 非阻塞检查失败，降级到阻塞等待  
**诊断**:
```bash
# 查看 blocked waits 计数
tail -f /tmp/jwm.log | grep "blocked_waits"
```

**解决**:
1. 检查 GPU 是否过载（frame_time > 50ms）
2. 减少活跃窗口数
3. 降低 blur_strength 或禁用 blur

### 问题2: 纹理显示异常/闪烁

**原因**: Fence 清理过早导致 GPU 读取未完成的纹理  
**诊断**:
```bash
# 启用 GL 错误检查
GL_DEBUG=1 jwm 2>&1 | grep "GL error"
```

**解决**:
```rust
// src/backend/x11/compositor/gpu_fence_sync.rs:95
cleanup_timeout: Duration::from_millis(200),  // 改为 200ms
```

### 问题3: 内存泄漏

**诊断**:
```bash
# 监控 fence 计数
watch -n 1 "tail -20 /tmp/jwm.log | grep 'total_fences'"
```

**预期**: 
- 创建数 ≈ 清理数（稳定状态）
- 差值 < 100（活跃 fence 数）

---

## 与 P6C 的协同效应

| 优化 | 消除的 stall | 应用场景 |
|------|-------------|---------|
| **P6C (PBO)** | 纹理上传 CPU stall | overview/font 上传 |
| **P6B (Fence)** | TFP bind/release GPU stall | 窗口纹理更新 |
| **组合** | 双重 stall 消除 | 多窗口快速切换 |

**预期总收益**: **5-10ms** (P6C 1-3ms + P6B 2-5ms + 协同 2-2ms)

---

## 下一步优化 (P6A)

当前 P6B 已消除 GPU fence 的阻塞等待，但事件处理仍在主线程串行执行：

1. **异步事件队列**: 分离事件处理和渲染线程
2. **延迟 NameWindowPixmap**: 从事件线程移到渲染线程
3. **输入优先级**: 优先处理鼠标/键盘事件

预期额外收益: **10-15ms** (输入延迟)

---

## 参考

- OpenGL Sync Objects: https://www.khronos.org/opengl/wiki/Sync_Object
- GPU Synchronization Best Practices: https://developer.nvidia.com/blog/gpu-synchronization-primitives/
- jwm P6C Optimization: docs/P6C_PBO_UPLOAD_TEST.md
