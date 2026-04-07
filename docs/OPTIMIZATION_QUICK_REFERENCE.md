# JWM 优化模块快速参考

## 模块导入
```rust
use jwm::backend::x11::compositor::{
    PerfMetrics,
    TexturePool,
    ShaderCache,
    PixelBufferPool,
    FrameRateLimiter,
    AdaptiveFrameRate,
    AdaptiveBlur,
    PerMonitorRenderer,
    DirtyRegionTracker,
    DirtyRect,
    OptimizationManager,
    OptimizationStatus,
};

use jwm::backend::x11::event_coalescer::EventCoalescer;
use jwm::backend::x11::batch::X11RequestBatcher;
```

## 快速配置

### 基础设置
```rust
// 创建优化管理器
let cache_dir = PathBuf::from("/tmp/jwm_cache");
let mut opt_mgr = OptimizationManager::new(cache_dir, 60);  // 60fps 目标

// 配置性能指标
opt_mgr.perf_metrics.set_gpu_load(50);  // 当前 50% GPU 负载

// 启用/禁用模糊
opt_mgr.set_blur_enabled(true);

// 设置目标帧率
opt_mgr.set_target_fps(120);
```

### 多显示器
```rust
let mut renderer = PerMonitorRenderer::new();
renderer.add_monitor(0, 0, 0, 1920, 1080);      // 显示器 0
renderer.add_monitor(1, 1920, 0, 1920, 1080);   // 显示器 1
```

## 渲染循环集成

```rust
fn render_frame(&mut self) {
    // 1. 开始帧
    let frame_duration = self.opt_mgr.frame_start();
    
    // 2. 处理脏区
    if let Some(dirty) = self.dirty_tracker.merged() {
        unsafe {
            gl.scissor(dirty.x, dirty.y, dirty.width as i32, dirty.height as i32);
        }
    }
    
    // 3. 渲染
    self.render_windows(&gl);
    
    // 4. 结束帧
    self.opt_mgr.frame_end();
    
    // 5. 清空脏区和事件队列
    self.dirty_tracker.clear();
    self.event_coalescer.clear();
    
    // 6. 检查性能
    if self.frame_count % 60 == 0 {
        let status = self.opt_mgr.get_status();
        log::info!("Performance: {}", status.summary());
    }
}
```

## 常用任务速查

### 任务 1: 标记窗口更新
```rust
// 窗口移动/调整大小
let window_rect = DirtyRect::new(x, y, w, h);
dirty_tracker.mark_dirty(window_rect);

// 整个屏幕
dirty_tracker.mark_all_dirty();
```

### 任务 2: 处理鼠标输入
```rust
// 在事件处理中
event_coalescer.coalesce_motion(x, y);

// 在帧边界
if let Some(motion) = event_coalescer.flush_motion() {
    let (x, y) = (motion.x, motion.y);
    // 处理最终的鼠标位置
}
```

### 任务 3: 获取纹理
```rust
let texture = texture_pool.acquire(&gl, width, height)?;
// 使用纹理...
texture_pool.release(&gl, texture, width, height);
```

### 任务 4: 编译着色器
```rust
let program = shader_cache.get_or_compile(
    &gl,
    "my_effect",
    VERTEX_SHADER_SOURCE,
    FRAGMENT_SHADER_SOURCE
)?;
```

### 任务 5: 截图
```rust
let buffer = pixel_pool.acquire(width * height * 4);
unsafe {
    gl.read_pixels(0, 0, width as i32, height as i32,
        glow::RGBA, glow::UNSIGNED_BYTE,
        glow::PixelPackData::Slice(Some(&mut buffer)));
}
// 处理 buffer...
pixel_pool.release(buffer);
```

## 性能监控

### 获取当前状态
```rust
let status = opt_mgr.get_status();
println!("FPS: {:.1}", status.recent_fps);
println!("GPU Load: {}%", status.gpu_load);
println!("Blur Quality: {:?}", status.blur_quality);
```

### 记录统计
```rust
opt_mgr.log_stats();
// 输出详细的性能统计信息
```

### 自定义监控
```rust
if opt_mgr.perf_metrics.recent_fps() < 40.0 {
    log::warn!("Low FPS detected!");
}

if opt_mgr.perf_metrics.gpu_load() > 90 {
    log::info!("GPU overloaded, reducing blur quality");
    opt_mgr.set_blur_enabled(false);
}
```

## 配置预设

### 高性能系统 (RTX 3080 级别)
```rust
let opt_mgr = OptimizationManager::new(cache_dir, 144);
opt_mgr.frame_rate_limiter.set_vsync(false);
opt_mgr.adaptive_blur.set_quality(BlurQuality::Full);
```

### 均衡系统 (集成显卡)
```rust
let opt_mgr = OptimizationManager::new(cache_dir, 60);
opt_mgr.frame_rate_limiter.set_vsync(true);
opt_mgr.adaptive_frame_rate.update_load(50);
```

### 低功耗系统 (笔记本)
```rust
let opt_mgr = OptimizationManager::new(cache_dir, 30);
opt_mgr.frame_rate_limiter.set_vsync(true);
opt_mgr.frame_rate_limiter.set_target_fps(30);
```

## 调试命令

### 启用详细日志
```bash
RUST_LOG=debug,jwm::backend::x11::compositor=trace cargo run
```

### 查看特定模块日志
```bash
RUST_LOG=compositor=debug cargo run
RUST_LOG=blur=debug cargo run
RUST_LOG=shader=debug cargo run
```

## 故障排查速查表

| 问题 | 解决方案 |
|------|---------|
| FPS 波动 | 启用脏区追踪 + 事件合并 |
| 高 GPU 使用率 | 降低目标 FPS / 启用模糊自适应 |
| 着色器编译慢 | 检查着色器缓存目录 |
| 内存泄漏 | 检查 texture_pool 和 pixel_pool 统计 |
| 响应延迟 | 增加帧率 / 禁用 VSync |
| 功耗过高 | 启用帧率限制 / 降低目标 FPS |

## 性能目标

```rust
// 理想目标
- FPS >= target (60fps 默认)
- GPU Load <= 85%
- CPU Load <= 75%
- Latency < frame_budget

// 警告阈值
- FPS < 85% * target → 调整配置
- GPU Load > 90% → 降低质量
- CPU Load > 85% → 减少工作

// 关键阈值
- FPS < 50% * target → 严重问题，启用省电模式
- GPU Load > 98% → 立即降低质量
```

## 内存占用估计

```rust
// 假设 1920x1080 分辨率
- PerfMetrics:      < 1 KB
- TexturePool:      取决于纹理大小 (通常 100-500 MB)
- PixelBufferPool:  < 10 KB (缓冲数量)
- ShaderCache:      < 5 MB (编译的着色器)
- PerMonitorRenderer: < 1 KB
- DirtyRegionTracker: < 1 KB
- EventCoalescer:   < 1 KB

总计: ~ 100-500 MB (主要为纹理池)
```

## 性能调优建议

1. **初期**: 启用所有优化，监控性能
2. **如果 FPS 稳定**: 尝试更高的分辨率或效果
3. **如果 FPS 波动**: 启用脏区追踪和事件合并
4. **如果 GPU 过载**: 启用自适应模糊和帧率限制
5. **如果响应延迟**: 增加目标帧率，禁用 VSync
6. **如果功耗过高**: 启用帧率限制和 VSync

## 相关文件

- 完整指南: `docs/OPTIMIZATION_GUIDE.md`
- 实现总结: `docs/OPTIMIZATION_IMPLEMENTATION_SUMMARY.md`
- 源代码: `src/backend/x11/compositor/`

## 常见问题

**Q: 如何完全禁用优化?**
A: 注释掉 OptimizationManager 的初始化和更新调用。

**Q: 能否在运行时切换配置?**
A: 可以，所有参数都可以通过 OptimizationManager 动态调整。

**Q: 哪个优化效果最明显?**
A: 脏区追踪在大多数场景效果最好，其次是事件合并和模糊自适应。
