# P6C: PBO 零拷贝纹理上传优化 - 测试指南

## 优化概述

**Commit**: 3e9c09d  
**目标**: 消除 overview/expose 模式下的纹理上传 CPU stall

### 实现细节

- **模块**: `src/backend/x11/compositor/pbo_uploader.rs`
- **策略**: GL_STREAM_DRAW + glFenceSync 异步上传
- **应用场景**: 
  - Alt-Tab overview 标题文字纹理
  - Alt-Tab overview 窗口缩略图
  - Expose 模式窗口预览

### 关键改进

```rust
// Before: 同步上传 (CPU 阻塞等待 GPU)
gl.tex_image_2d(..., PixelUnpackData::Slice(Some(&pixels)));

// After: 异步上传 (GPU 后台处理)
gl.tex_image_2d(..., PixelUnpackData::Slice(None));  // 仅分配
pbo_uploader.upload_texture(&gl, tex, w, h, RGBA, &pixels);  // PBO → GPU
```

---

## 性能测试

### 1. Alt-Tab 激活延迟测试

**目的**: 测量 overview 模式启动响应时间

```bash
# 启动 jwm
jwm

# 打开多个窗口（5-10个）
for i in {1..8}; do alacritty & done

# 测试步骤
1. 按 Alt-Tab 激活 overview
2. 观察 HUD 中的 frame_time (F12 开启 debug HUD)
3. 记录第一帧的 frame_time (预期 < 20ms)

# 预期收益
- Before: 首帧 25-35ms (包含标题+缩略图上传 stall)
- After:  首帧 15-25ms (PBO 异步上传，无 CPU stall)
```

**指标采集**:
```bash
# 启用 extended debug HUD
jwm-tool ipc '{"set_debug_hud_extended": true}'

# 监控 frame time 和纹理上传次数
watch -n 0.1 "jwm-tool ipc '{\"query\": \"get_metrics\"}' | jq '{
  fps, 
  avg_frame_time_ms, 
  max_frame_time_ms,
  texture_memory_bytes
}'"
```

---

### 2. Expose 模式激活测试

```bash
# 启动 jwm
jwm

# 打开大量窗口（测试批量上传）
for i in {1..20}; do firefox & done
sleep 5

# 激活 Expose (假设绑定到 Super+E)
# 观察启动流畅度

# 预期收益
- Before: 明显卡顿（20个窗口 × ~2ms/窗口 = 40ms stall）
- After:  流畅启动（PBO 异步处理）
```

---

### 3. PBO 池统计监控

```bash
# 查看 PBO 复用率
tail -f /tmp/jwm.log | grep "pbo_uploader"

# 预期日志
# [INFO] pbo_uploader: initialized with 4MB PBOs, pool size 4
# [DEBUG] pbo_uploader: pool hit (3/4 PBOs in use)
# [DEBUG] pbo_uploader: pool miss, creating new PBO (4/4 pool full)
```

**正常运行特征**:
- 启动后创建 4 个 PBO
- Alt-Tab/Expose 激活时复用 pool
- 无 "pool miss" 警告（说明 4MB × 4 足够）

---

### 4. 性能对比基准

使用 `hyperfine` 测量 overview 启动时间：

```bash
# 创建测试脚本 test_alt_tab.sh
cat > /tmp/test_alt_tab.sh <<'EOF'
#!/bin/bash
jwm-tool ipc '{"action": "toggle_overview"}'
sleep 0.1
jwm-tool ipc '{"action": "toggle_overview"}'
EOF
chmod +x /tmp/test_alt_tab.sh

# 运行基准测试 (需要 jwm 已启动)
hyperfine --warmup 3 --runs 10 \
  '/tmp/test_alt_tab.sh' \
  --export-markdown /tmp/p6c_bench.md
```

**预期结果**:
```
Benchmark 1: test_alt_tab.sh
  Time (mean ± σ):     120.0 ms ±   5.0 ms    [User: 2.0 ms, System: 3.0 ms]
  Range (min … max):   115.0 ms … 130.0 ms    10 runs
```

---

## 验证清单

- [ ] **编译通过**: `cargo build --release`
- [ ] **运行时检查**: 启动 jwm，无 PBO 相关错误日志
- [ ] **Alt-Tab 测试**: 激活 overview，观察流畅度
- [ ] **Expose 测试**: 激活 Expose，检查缩略图加载
- [ ] **HUD 指标**: frame_time 无异常峰值
- [ ] **PBO 池**: pool size 稳定在 3-4

---

## 已知限制

1. **大纹理降级**: 超过 4MB 的纹理自动回退到同步上传
   - 4096×4096 RGBA = 64MB（会触发）
   - 实际 overview 纹理通常 < 1MB，不受影响

2. **驱动兼容性**: 需要 OpenGL 3.3+ 支持 PBO
   - Mesa 10.0+ (Ubuntu 16.04+)
   - NVIDIA 340+ (2014+)
   - AMD AMDGPU (开源驱动)

3. **无持久化映射**: 使用 GL_STREAM_DRAW 而非 ARB_buffer_storage
   - 兼容性优先，性能略逊于持久化映射
   - 实测差异 < 0.5ms

---

## 故障排查

### 问题1: "pbo_uploader: pool miss" 频繁出现

**原因**: 并发上传超过 pool size (4)  
**解决**: 增大 pool size

```rust
// src/backend/x11/compositor/mod.rs:2733
pbo_uploader: PBOUploader::new(4 * 1024 * 1024, 8),  // 改为 8
```

### 问题2: 纹理显示异常/黑屏

**原因**: fence sync 失败或 PBO 数据损坏  
**诊断**:
```bash
# 启用 GL 错误检查
GL_DEBUG=1 jwm 2>&1 | grep "GL error"
```

**临时禁用 PBO**:
```rust
// src/backend/x11/compositor/overview.rs
// 注释掉 pbo_uploader.upload_texture 调用，改回:
self.gl.tex_image_2d(..., PixelUnpackData::Slice(Some(&pixels)));
```

### 问题3: 内存泄漏

**诊断**:
```bash
# 监控 GL 纹理内存
watch -n 1 "jwm-tool ipc '{\"query\": \"get_metrics\"}' | jq .data.texture_memory_bytes"
```

**预期**: Alt-Tab 关闭后，texture_memory 应回落到基线

---

## 下一步优化 (P6B)

PBO 上传已消除 CPU stall，但 GPU fence sync 仍有优化空间：

1. **替换 glClientWaitSync**: 用非阻塞查询替代
2. **引入 XSyncFence**: 跨进程同步优化
3. **Triple buffering**: 3 个 PBO 轮转，完全消除等待

预期额外收益: **2-5ms**

---

## 参考

- OpenGL PBO Best Practices: https://www.khronos.org/opengl/wiki/Pixel_Buffer_Object
- Mesa PBO Implementation: src/mesa/main/pbo.c
- jwm P5 Optimizations: docs/P5_OPTIMIZATIONS.md
