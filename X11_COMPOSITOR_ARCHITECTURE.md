# JWM X11 Compositor Architecture & Design

## 目录
1. [概述](#概述)
2. [X11 Compositor 工作原理](#x11-compositor-工作原理)
3. [GLX 技术选型](#glx-技术选型)
4. [整体架构](#整体架构)
5. [渲染管道详解](#渲染管道详解)
6. [关键优化技术](#关键优化技术)
7. [模块设计](#模块设计)
8. [性能特性](#性能特性)
9. [扩展点](#扩展点)

---

## 概述

JWM 是一个用 **Rust** 实现的现代 X11 窗口管理器，内置高性能的 OpenGL Compositor（合成器）。其核心特点是：

- **高性能渲染**：使用 OpenGL（GLX）+ 零拷贝 TFP（Texture From Pixmap）
- **自适应帧率**：根据场景动态调整渲染频率，节省电量
- **复杂视觉效果**：支持模糊、阴影、动画、过渡等
- **事件驱动架构**：基于 Calloop 事件循环
- **多显示器支持**：每显示器独立渲染和配置

---

## X11 Compositor 工作原理

### 1.1 什么是 X11 Compositor

X11 Compositor 是一个特殊的窗口，负责：

1. **重定向（Redirect）**：拦截所有窗口的绘制，将其输出到离屏 Pixmap
2. **合成（Composite）**：将多个窗口的 Pixmap 按 Z-order 合成到屏幕
3. **特效处理**：在合成过程中应用视觉效果（模糊、阴影、旋转等）
4. **呈现（Present）**：通过 VSync 同步将最终结果显示到屏幕

### 1.2 工作流程概览

```
┌─────────────────────────────────────────────┐
│          X11 Event Loop (Calloop)            │
│  ┌──────────────────────────────────────┐  │
│  │  X11 Events / Damage Notifications   │  │
│  │  Signals / Timers / Config Changes   │  │
│  └────────────┬──────────────────────┬──┘  │
│               │                      │      │
│        Event Handler        Frame Tick     │
│        (20ms interval)      (immediate)    │
└─────────────────────────────────────────────┘
          │                           │
          ↓                           ↓
┌──────────────────────────────────────────────┐
│          Compositor Render Pipeline           │
│  · Shader Hot-reload                        │
│  · Window TFP Texture Update                │
│  · Damage Region Tracking & Scissor         │
│  · Scene Rendering (Pass 1-N)               │
│  · Effects & Post-processing                │
│  · Screen Present (VSync/Present)           │
└──────────────────────────────────────────────┘
```

### 1.3 Composite Extension 的关键协议

| 概念 | 说明 |
|------|------|
| **CompositeRedirect** | 拦截窗口输出到 Pixmap |
| **DamageNotify** | X11 通知 Compositor 哪些区域被修改 |
| **GLXPixmap** | 将 Pixmap 绑定为 GL 纹理的中间对象 |
| **Overlay Window** | 合成器使用的顶层窗口（用于渲染） |

---

## GLX 技术选型

### 2.1 为什么选择 GLX？

JWM X11 Compositor 使用 **GLX（OpenGL X11）** 而非 XRender 或 EGL。

#### 技术对比

| 方案 | 渲染 API | 优点 | 缺点 | JWM 选择 |
|------|---------|------|------|---------|
| **GLX** | OpenGL | 高性能、零拷贝、VSync控制、现代 GPU 支持 | 需要 GPU | ✅ **采用** |
| **XRender** | 2D 矢量 | 简单、CPU 实现 | 性能低、功能有限、无 VSync | ❌ |
| **EGL** | OpenGL ES | 现代、跨平台 | X11 支持有限、集成复杂 | ❌ |

### 2.2 GLX 架构

```
┌────────────────────────────────────────┐
│         应用程序 (JWM)                  │
└────────────────┬───────────────────────┘
                 │
        ┌────────┴─────────┐
        │                  │
        ↓                  ↓
┌──────────────┐    ┌──────────────┐
│  Xlib (x11)  │    │  GLX (glx)   │
│  连接/窗口   │    │  OpenGL 上下文│
└──────┬───────┘    └──────┬───────┘
       │                   │
       └────────┬──────────┘
                │
                ↓
        ┌──────────────────┐
        │  X11 Server      │
        │  (Display + GPU) │
        └──────────────────┘
```

### 2.3 GLX 核心接口

```rust
// src/backend/x11/compositor/glx_context.rs
pub struct GlxContext {
    display: *mut xlib::Display,      // X11 display
    context: glx::GLXContext,         // OpenGL context
    drawable: glx::GLXDrawable,       // Render target
}

// 核心函数
glXMakeContextCurrent()   // 激活上下文
glXSwapBuffers()          // 呈现帧（VSync）
glXGetProcAddress()       // 加载扩展函数
```

### 2.4 TFP - Texture From Pixmap（零拷贝纹理）

**核心优势**：将 X11 Pixmap 直接映射到 GPU 内存中的 GL Texture，无需 CPU 拷贝。

```
X11 Window Pixmap (GPU Memory)
    ↓
glXCreatePixmap() → GLXPixmap
    ↓
glXBindTexImageEXT() → GL Texture
    ↓
GPU 直接渲染该纹理（0 拷贝）
```

**TFP 状态机**：

```rust
// src/backend/x11/compositor/tfp.rs
struct WindowTexture {
    gl_texture: glow::Texture,           // GPU 纹理
    glx_pixmap: glx::GLXPixmap,          // GLX Pixmap 对象
    dirty: bool,                         // Pixmap 内容是否有更新
    pending_fence: Option<glow::Sync>,   // GPU 同步 fence
}
```

**更新流程**（每帧）：

```rust
// 从 render_frame()
if window.dirty && window.glx_pixmap {
    gl.bind_texture(TEXTURE_2D, window.gl_texture);
    
    // 释放旧 pixmap
    glXReleaseTexImageEXT(display, glx_pixmap, GLX_FRONT_LEFT_EXT);
    
    // 绑定新 pixmap
    glXBindTexImageEXT(display, glx_pixmap, GLX_FRONT_LEFT_EXT, nullptr);
    
    // GPU 同步
    window.pending_fence = gl.fence_sync(SYNC_GPU_COMMANDS_COMPLETE);
    
    window.dirty = false;
}
```

---

## 整体架构

### 3.1 文件结构

```
src/backend/x11/
├── mod.rs                    # X11 backend 主模块
├── backend.rs                # X11Backend 结构和事件循环
├── ids.rs                    # WindowId 映射表
├── batch.rs                  # 批量 X11 请求
├── event_coalescer.rs        # 事件合并优化
│
└── compositor/               # 合成引擎核心
    ├── mod.rs                # Compositor 结构和 render_frame()
    ├── glx_context.rs        # GLX 上下文包装
    ├── tfp.rs                # Texture From Pixmap 实现
    ├── pipeline.rs           # FBO 和 blur 管道
    ├── present.rs            # VSync / Present 呈现
    ├── oml_sync_control.rs   # OML_sync_control 扩展
    ├── audio_sync.rs         # 音频同步
    │
    ├── shaders.rs            # GLSL 着色器编译
    ├── shader_cache.rs       # 着色器缓存
    ├── effects.rs            # 视觉效果实现
    ├── transitions.rs        # 窗口过渡动画
    ├── blur_optimize.rs      # Kawase 模糊优化
    ├── dirty_region.rs       # 脏区域追踪
    │
    ├── postprocess.rs        # 后处理效果
    ├── texture_pool.rs       # 纹理对象池
    ├── pixel_buffer_pool.rs  # PBO 缓冲池
    ├── perf_metrics.rs       # 性能指标收集
    ├── frame_rate.rs         # 自适应帧率
    ├── per_monitor.rs        # 每显示器渲染
    ├── optimization_manager.rs # 优化策略管理
    │
    ├── font.rs               # 字体渲染
    ├── annotations.rs        # 用户标注
    ├── overview.rs           # 窗口概览
    ├── expose.rs             # Expose 效果
    ├── math.rs               # 数学工具函数
    └── types.rs              # 类型定义
```

### 3.2 核心数据结构

#### `X11Backend` (src/backend/x11/backend.rs)

```rust
pub struct X11Backend {
    conn: Arc<RustConnection>,          // X11 连接
    screen: Screen,                     // 屏幕信息
    root: WindowId,                     // 根窗口 ID
    ids: X11IdRegistry,                 // WindowId 映射
    atoms: Atoms,                       // X11 atoms
    
    window_ops: Box<dyn WindowOps>,     // 窗口操作接口
    input_ops: Box<dyn InputOps>,       // 输入操作接口
    property_ops: Box<dyn PropertyOps>, // 属性操作接口
    output_ops: Box<dyn OutputOps>,     // 显示器操作接口
    
    compositor: Option<Compositor>,     // 合成引擎（可选）
}
```

#### `Compositor` (src/backend/x11/compositor/mod.rs)

```rust
pub struct Compositor {
    // GLX 上下文
    xlib_display: *mut xlib::Display,
    glx_context: glx::GLXContext,
    glx_drawable: glx::GLXDrawable,
    
    // OpenGL
    gl: Arc<glow::Context>,
    program: glow::Program,              // 主着色器程序
    
    // 窗口管理
    windows: HashMap<u32, WindowTexture>, // 所有窗口的纹理
    
    // 缓存和池
    texture_pool: TexturePool,
    shader_cache: ShaderCache,
    pixel_buffer_pool: PixelBufferPool,
    
    // 优化
    damage_tracker: DirtyRegionTracker,  // 脏区域追踪
    blur_cache_hash: u64,                // 模糊缓存检验和
    frame_stats: FrameStats,
    
    // 视觉效果
    fade_animations: HashMap<u32, FadeAnimation>,
    genie_animations: HashMap<u32, GenieAnimation>,
    wallpaper_texture: Option<glow::Texture>,
    
    // VSync 控制
    vsync_method: VsyncMethod,
    oml_sync_control: OmlSyncControl,
}
```

---

## 渲染管道详解

### 4.1 render_frame() 函数流程

核心函数：`src/backend/x11/compositor/mod.rs:3689`

**render_frame() 完整流程**：

```rust
pub fn render_frame(&mut self, scene: &[(u32, i32, i32, u32, u32)], focused: Option<u32>) -> bool {
    
    // ========== 初期化阶段 ==========
    
    // 1. Shader 热重载检测（每60帧检测一次）
    if shader_hot_reload_enabled {
        poll_shader_hot_reload()
    }
    
    // 2. VRR 状态检测（可变刷新率）
    update_vrr_state()
    
    // 3. 检查全屏程序优化（直接输出，跳过合成）
    if check_fullscreen_unredirect(scene) {
        return false  // 不需要合成，直接显示
    }
    
    // ========== 动画阶段 ==========
    
    // 4. 更新各种动画
    fades_active = tick_fades()              // 淡入淡出动画
    wobbly_active = tick_wobbly()            // 晃动物理效果
    genie_active = tick_genie()              // 魔灯最小化效果
    ripples_active = tick_ripples()          // 波纹点击效果
    expose_animating = tick_expose()         // 窗口概览动画
    focus_highlight_active = tick_focus_highlight()  // 焦点高亮环
    
    any_animating = fades_active || wobbly_active || ...
    
    // ========== 渲染优化决策 ==========
    
    // 5. 脏区域检测：判断是否需要渲染
    has_dirty = scene.iter().any(|win| {
        windows[win].dirty || windows[win].needs_pixmap_refresh
    })
    
    explicit_render = std::mem::replace(&mut needs_render, false)
    
    force_render = pending_screenshot.is_some() 
                || debug_hud 
                || animation_active
                || overview_active
                || explicit_render
    
    scene_changed = hash(scene, focused) != last_scene_hash
    
    // 关键优化：如果没有变化，跳过整个 GL 渲染
    if !has_dirty && !fades_active && !force_render && !scene_changed {
        return false  // 本帧无需渲染
    }
    
    // ========== 纹理更新阶段（TFP） ==========
    
    // 6. 确保 GLX 上下文是当前的
    if !context_current {
        glXMakeContextCurrent(xlib_display, glx_drawable, glx_drawable, glx_context)
        context_current = true
    }
    
    // 7. 批量刷新窗口 Pixmap -> GL 纹理
    refresh_pixmaps()
    
    // 优先级更新（焦点窗口优先）
    tfp_budget = Duration::from_micros(3000)  // 3ms 预算
    for win in priority_ordered_windows {
        // 焦点窗口总是更新
        if budget_exhausted && Some(win) != focused {
            continue
        }
        
        if window.dirty && window.glx_pixmap {
            // 音频同步检查
            if audio_sync.should_render(win) {
                // TFP 更新
                gl.bind_texture(TEXTURE_2D, window.gl_texture)
                tfp.release(glx_pixmap, GLX_FRONT_LEFT_EXT)
                tfp.bind(glx_pixmap, GLX_FRONT_LEFT_EXT, nullptr)
                window.pending_fence = gl.fence_sync(SYNC_GPU_COMMANDS_COMPLETE)
                window.dirty = false
            }
        }
    }
    
    // ========== 遮挡剔除（Occlusion Culling） ==========
    
    // 8. 检测完全遮挡的窗口，跳过渲染它们下面的内容
    first_visible = 0
    for i in (0..scene.len()).reverse() {
        if scene[i] is_opaque_and_fills_screen() {
            first_visible = i
            break
        }
    }
    visible_scene = scene[first_visible..]
    
    // ========== OpenGL 渲染 ==========
    
    // 9. 如果启用后处理，目标改为离屏 FBO
    if postprocess_active {
        gl.bind_framebuffer(FRAMEBUFFER, postprocess_fbo)
    }
    
    // 10. 启用脏区域剪刀测试（只渲染改变的区域）
    damage_bounds = damage_tracker.dirty_bounds()
    if damage_bounds && !force_render {
        gl.enable(SCISSOR_TEST)
        // GL scissor 坐标系：原点在左下
        gl_y = screen_h - dy - dh
        gl.scissor(dx, gl_y, dw, dh)
    }
    
    // 11. 清屏
    gl.viewport(0, 0, screen_w, screen_h)
    gl.clear(COLOR_BUFFER_BIT)
    
    // 建立投影矩阵（正交投影）
    proj = ortho(0, screen_w, screen_h, 0, -1, 1)
    
    // ========== Pass 1: 墙纸渲染 ==========
    
    // 12. 渲染背景墙纸
    if !wallpaper_occluded {
        gl.use_program(wallpaper_program)
        
        if has_per_monitor_wallpapers {
            // 多显示器不同墙纸
            for monitor in monitors {
                gl.scissor(monitor.area)
                gl.bind_texture(TEXTURE_2D, monitor.wallpaper_texture)
                gl.draw_arrays(TRIANGLE_STRIP, 0, 4)
            }
        } else if has_global_wallpaper {
            // 全局墙纸
            gl.bind_texture(TEXTURE_2D, wallpaper_texture)
            gl.draw_arrays(TRIANGLE_STRIP, 0, 4)
        }
        
        // 墙纸过渡动画
        if wallpaper_transitioning {
            gl.bind_texture(TEXTURE_2D, old_wallpaper_texture)
            gl.uniform_1_f32(opacity, fade_alpha)
            gl.draw_arrays(TRIANGLE_STRIP, 0, 4)
        }
    }
    
    // ========== Pass 2: 阴影 ==========
    
    // 13. 渲染窗口阴影（feature 14）
    if shadow_enabled {
        gl.use_program(shadow_program)
        gl.uniform_matrix_4_f32_slice(projection, false, &proj)
        
        for window in visible_scene {
            if shadow_exclude.matches(window) { continue }
            if window.is_shaped { continue }  // 不渲染异形窗口阴影
            
            fade = window.fade_opacity
            if fade <= 0 { continue }
            
            // 计算阴影几何（支持窗口倾斜）
            radius = window.corner_radius_override || global.corner_radius
            gl.uniform_1_f32(radius, radius)
            gl.uniform_4_f32(shadow_color, r, g, b, a * fade)
            
            draw_shadow_quad(window)
        }
    }
    
    // ========== Pass 3-5: 各种图形效果 ==========
    
    // 14. 魔灯效果、波纹、焦点高亮 等
    draw_genie_effects()
    draw_ripples()
    draw_focus_highlight()
    
    // ========== Pass 6: 窗口背景（Blur Base） ==========
    
    // 15. 如果有窗口使用 frosted blur，先渲染场景到 FBO
    if any_window_uses_frosted_blur {
        gl.bind_framebuffer(FRAMEBUFFER, scene_fbo)
        
        for window in visible_scene_excluding_frosted {
            draw_window_quad(window)
        }
    }
    
    // ========== Pass 7: 模糊处理（Dual Kawase） ==========
    
    // 16. 多级金字塔模糊
    if frosted_window_active {
        for level in blur_fbo_levels {
            gl.bind_framebuffer(FRAMEBUFFER, level.fbo)
            gl.use_program(kawase_blur_program)
            gl.draw_arrays(TRIANGLE_STRIP, 0, 4)
        }
    }
    
    // ========== Pass 8: 主窗口栈 ==========
    
    // 17. 回到屏幕 FBO，渲染主窗口
    gl.bind_framebuffer(FRAMEBUFFER, 0)
    
    for window in visible_scene {
        if overview_skip(window) { continue }
        
        opacity = window.fade_opacity * window.anim_opacity
        radius = window.corner_radius_override || global.corner_radius
        blur_amount = window.blur_amount
        
        gl.use_program(window_program)
        gl.uniform_1_f32(opacity, opacity)
        gl.uniform_1_f32(radius, radius)
        gl.uniform_1_i32(texture, 0)
        
        if blur_amount > 0 {
            gl.active_texture(TEXTURE1)
            gl.bind_texture(TEXTURE_2D, blur_fbo.texture)
            gl.uniform_1_i32(blur_texture, 1)
        }
        
        gl.bind_texture(TEXTURE_2D, window.gl_texture)
        draw_quad(window.rect)
    }
    
    // ========== Pass 9: UI 叠加层 ==========
    
    // 18. 调试 HUD、标注、窗口概览等
    if debug_hud { draw_debug_metrics() }
    if annotation_active { draw_annotations() }
    if overview_active { draw_overview_3d_prism() }
    
    // ========== 后处理 ==========
    
    // 19. 应用全屏后处理效果
    if postprocess_active {
        gl.bind_framebuffer(FRAMEBUFFER, 0)
        gl.use_program(postprocess_program)
        draw_postprocess_quad()
    }
    
    // ========== 禁用脏区域剪刀 ==========
    
    // 20. 清理状态
    if use_scissor {
        gl.disable(SCISSOR_TEST)
    }
    
    // ========== 屏幕呈现（VSync） ==========
    
    // 21. 提交渲染结果到屏幕
    present_frame()  // 使用 VSync 方法呈现
    
    return true
}
```

### 4.2 事件循环

```rust
// src/backend/x11/backend.rs:1192
fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError> {
    let mut event_loop = EventLoop::try_new()?;
    
    // 注册事件源
    handle.insert_source(x11_source, |event, _, data| {
        // 1. 更新 Compositor 状态（damage、生命周期）
        data.backend.compositor_handle_event(&event);
        
        // 2. 通知 WM（布局、焦点、窗口管理）
        data.handler.handle_event(data.backend, event);
    })?;
    
    // 注册 20ms 定时器
    handle.insert_source(timer, |_, _, data| {
        data.handler.update(data.backend)
    })?;
    
    // 主循环
    loop {
        // 如果需要渲染，设置超短超时（1ms）
        let timeout = if needs_tick() || compositor_needs_render() {
            Some(Duration::from_millis(1))
        } else {
            None  // 阻塞等待事件
        };
        
        event_loop.dispatch(timeout, &mut loop_data)?;
        
        // 立即渲染（高优先级，减少延迟）
        handler.render_compositor_immediate(backend);
        
        if should_exit { break }
    }
}
```

---

## 关键优化技术

### 5.1 脏区域追踪（Dirty Region Tracking）

**文件**：`src/backend/x11/compositor/dirty_region.rs`

**目的**：只更新改变的区域，减少 GPU 工作量

**机制**：
```
DamageNotify Event
    ↓
记录脏矩形
    ↓
render_frame() 中启用 SCISSOR_TEST
    ↓
GPU 只处理脏区域
```

**优化效果**：大幅减少 GPU 像素处理（特别是高分辨率屏幕）

### 5.2 遮挡剔除（Occlusion Culling）

**原理**：从上往下查找第一个填充整屏且不透明的窗口，跳过其下面的窗口

```rust
// 从后往前扫描
for i in (0..scene.len()).reverse() {
    if scene[i].fills_screen() && !has_alpha && !has_fade {
        first_visible = i
        break
    }
}
// 只渲染 visible_scene
```

**收益**：数量众多的下层窗口不进入 GPU 渲染管线

### 5.3 自适应帧率（Adaptive Frame Rate）

**文件**：`src/backend/x11/compositor/frame_rate.rs`

**原理**：当场景没有变化时，跳过整个 GL 渲染

```rust
if !has_dirty && !fades_active && !force_render && !scene_changed {
    return false  // 不渲染该帧
}
```

**优势**：
- 静止时零 GPU 使用
- 节省电量（延长笔记本电池寿命）
- 降低功耗散热

### 5.4 纹理像素映射（TFP）

**文件**：`src/backend/x11/compositor/tfp.rs`

**零拷贝机制**：
```
X11 Window Pixmap (GPU VRAM)
    ↓ [Direct GPU Mapping]
GL Texture Object
    ↓
Direct GPU Rendering（0 CPU 拷贝）
```

**关键同步**：
```rust
// GPU Fence 防止竞态条件
glXReleaseTexImageEXT()      // 释放旧绑定
glXBindTexImageEXT()         // 绑定新 pixmap
fence = gl.fence_sync()      // 创建同步 fence
glClientWaitSync(fence)      // 下次更新前等待 GPU 完成
```

### 5.5 Kawase 双向模糊（Blur Optimization）

**文件**：`src/backend/x11/compositor/blur_optimize.rs`

**优势**：高效的高斯模糊实现

```
原始纹理（1024x768）
    ↓ [Kawase Blur Level 1]
512x384 （downsample + blur）
    ↓ [Kawase Blur Level 2]
256x192
    ↓ [Kawase Blur Level 3]
128x96
    ↓
Upsample + Composite
    ↓
最终模糊结果（性能优）
```

### 5.6 音频同步（Audio Sync）

**文件**：`src/backend/x11/compositor/audio_sync.rs`

**问题解决**：
- **问题**：视频播放器的帧率被强制同步到合成器帧率，导致音视频不同步
- **解决**：检查音频时间码，只在音频需要新帧时更新视频纹理

```rust
if audio_sync.should_render(window) {
    // 更新该窗口纹理
    tfp.bind()
} else {
    // 跳过，使用上一帧纹理
    continue
}
```

### 5.7 着色器热重载

**目的**：无需重启即可更改着色器代码

**频率**：每 60 帧检查一次（约 1 秒）

```rust
if shader_hot_reload_enabled && frame_count % 60 == 0 {
    poll_shader_hot_reload()  // 检查文件改动，重新编译
}
```

---

## 模块设计

### 6.1 Compositor 核心模块

| 模块 | 职责 | 关键函数 |
|------|------|---------|
| `mod.rs` | 主结构、render_frame() | `render_frame()`, `create_window()`, `destroy_window()` |
| `glx_context.rs` | GLX 上下文管理 | `make_current()`, `swap_buffers()` |
| `tfp.rs` | Pixmap->Texture 映射 | `create_window_texture()`, `refresh_pixmaps()` |
| `pipeline.rs` | FBO 和 blur 管道 | `create_blur_fbos()`, `create_scene_fbo()` |
| `present.rs` | VSync 呈现 | `present_frame()` |

### 6.2 优化和缓存模块

| 模块 | 职责 |
|------|------|
| `dirty_region.rs` | 脏矩形追踪、损伤累积 |
| `texture_pool.rs` | GL Texture 对象池 |
| `pixel_buffer_pool.rs` | PBO 缓冲池（上传像素） |
| `shader_cache.rs` | 编译后的着色器缓存 |
| `frame_rate.rs` | 自适应帧率控制 |
| `optimization_manager.rs` | 优化策略统一管理 |

### 6.3 视觉效果模块

| 模块 | 效果 |
|------|------|
| `effects.rs` | 通用效果框架 |
| `blur_optimize.rs` | Kawase 模糊 |
| `transitions.rs` | 窗口切换过渡 |
| `postprocess.rs` | 全屏后处理 |

### 6.4 UI 和调试模块

| 模块 | 功能 |
|------|------|
| `perf_metrics.rs` | FPS、GPU 使用率等性能指标 |
| `font.rs` | 字体渲染（用于 HUD） |
| `annotations.rs` | 用户标注（如截图工具） |
| `overview.rs` | 3D 窗口概览 |

---

## 性能特性

### 7.1 VSync 策略

```rust
// src/backend/x11/compositor/mod.rs:86
enum VsyncMethod {
    Global,          // glXSwapInterval=1（传统，所有窗口同步）
    OmlSyncControl,  // GLX_OML_sync_control（每窗口 MSC 时序）
    Present,         // X11 Present extension（最灵活，独立呈现）
}
```

| 方法 | 特点 | 适用场景 |
|------|------|---------|
| **Global** | 简单、广泛支持 | 大多数 X11 系统 |
| **OML** | 每窗口独立 | 需要精细 VSync 控制 |
| **Present** | 最现代、最灵活 | 新型驱动 + 硬件 |

### 7.2 帧率管理

```rust
// 事件循环中
timeout = if needs_render {
    Some(Duration::from_millis(1))  // 渲染时：超短超时，快速响应
} else {
    None  // 闲置时：阻塞等待，节省 CPU
}
```

### 7.3 性能指标

**文件**：`src/backend/x11/compositor/perf_metrics.rs`

包含：
- 帧率（FPS）
- 帧时间（Frame Time）
- GPU 使用率（如可用）
- 纹理内存使用
- 脏区域大小

---

## 扩展点

### 8.1 添加新的视觉效果

1. 在 `src/backend/x11/compositor/effects.rs` 中实现新效果
2. 在 `render_frame()` 中添加新的 Pass
3. 示例：现有的模糊、阴影、过渡等

### 8.2 优化策略扩展

1. 修改 `OptimizationManager` 中的策略
2. 调整阈值或启用新的优化路径
3. 示例：自适应分辨率、动态纹理压缩等

### 8.3 新的 VSync 方法

1. 在 `VsyncMethod` enum 中添加新变体
2. 在 `present_frame()` 中实现逻辑
3. 支持 HDR、高刷新率等新特性

### 8.4 音频集成扩展

1. 增强 `audio_sync.rs` 中的音频时间码解析
2. 支持多音频流同步
3. 添加网络音频源（如 PulseAudio、JACK）

### 8.5 HDR 支持

**计划**（待实现）：
- 10-bit 颜色输出
- 扩展色彩空间
- 修改着色器和纹理格式

---

## 相关文件映射

| 功能 | 文件 |
|------|------|
| 事件循环 | `src/backend/x11/backend.rs:1192` |
| 渲染主入口 | `src/backend/x11/compositor/mod.rs:3689` |
| TFP 实现 | `src/backend/x11/compositor/tfp.rs` |
| GLX 初始化 | `src/backend/x11/compositor/mod.rs` (init code) |
| 脏区域追踪 | `src/backend/x11/compositor/dirty_region.rs` |
| 着色器编译 | `src/backend/x11/compositor/shaders.rs` |

---

## TODO - 待补充内容

- [ ] GLX 扩展详解（OML_sync_control、Present）
- [ ] 着色器编译过程
- [ ] 性能对标（不同优化配置下的 FPS、功耗）
- [ ] 调试工具和性能分析方法
- [ ] 常见问题排查（flickering、tearing、black screen）
- [ ] HDR 实现详解
- [ ] 多显示器高级特性
- [ ] 驱动兼容性列表
- [ ] 参考资源和论文

---

*最后更新：2026-04-30*

