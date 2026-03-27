# JWM X11 Compositor 功能点检清单

## 一、核心渲染

| 功能 | 文件 | 说明 |
|------|------|------|
| GLX 硬件加速合成 | `mod.rs` | GLX context + glow OpenGL 封装 |
| 双缓冲 | `mod.rs` | `glXSwapBuffers` + VSync |
| Texture From Pixmap (TFP) | `tfp.rs` | 每窗口 visual 匹配 FBConfig，RGBA/RGB 双路径 |
| Tile 损伤追踪 | `mod.rs` | 8×6 tile 网格，scissor 裁剪脏区域 |
| 跳帧优化 | `mod.rs` | scene hash 不变则跳过渲染 |
| 全屏 Unredirect | `mod.rs` | 全屏窗口自动取消合成（直通显卡） |

## 二、窗口视觉效果

| 功能 | 文件 | 说明 |
|------|------|------|
| 透明度 | `mod.rs` + `shaders.rs` | 每窗口 opacity，premultiplied alpha |
| 圆角 | `shaders.rs` | SDF smoothstep 抗锯齿 |
| 阴影 | `shaders.rs` | 可配置半径/偏移/颜色/扩散 |
| 边框 | `mod.rs` | 聚焦/非聚焦窗口不同颜色 |
| 非活动窗口变暗 | `mod.rs` | `inactive_dim` |
| 窗口缩放动画 | `mod.rs` | open/close 时 scale 过渡 |
| Fade 动画 | `mod.rs` | 窗口出现/消失淡入淡出 |

## 三、模糊 (Blur)

| 功能 | 文件 | 说明 |
|------|------|------|
| Dual Kawase Blur | `pipeline.rs` | 4 级下采样 + 3 级上采样 mipmap |
| 毛玻璃 (Frosted Glass) | `mod.rs` | 背景模糊 + hash 缓存 |
| 自适应降质 | `mod.rs` | 帧时间超预算自动降低模糊级别 |
| 按窗口类名排除 | `mod.rs` | 规则配置跳过特定窗口的 blur |
| Blur Mask | `mod.rs` | frame-extents 感知的模糊遮罩 |

## 四、特效

| 功能 | 文件 | 说明 |
|------|------|------|
| 粒子系统 | `effects.rs` | 窗口关闭时的粒子爆散动画 |
| 果冻窗口 (Wobbly) | `effects.rs` | 弹簧物理变形，可调刚度/阻尼 |
| 运动拖影 (Motion Trail) | `mod.rs` | 拖动窗口时的残影 |
| Genie 最小化 | `effects.rs` | 缩进 dock 的压缩动画 |
| 窗口打开涟漪 | `effects.rs` | 创建窗口时扩散波纹 |
| 聚焦高亮 | `effects.rs` | 焦点切换注意力引导 |
| 边缘发光 | `mod.rs` | 屏幕边缘环境光效果 |
| 3D 倾斜 | `shaders.rs` | 透视矩阵 tilt 效果 |

## 五、工作区切换动画

| 动画 | 文件 | 说明 |
|------|------|------|
| Slide | `transitions.rs` | 左右滑动 |
| Cube | `transitions.rs` | 3D 立方体旋转（Y轴90°） |
| Fade | `transitions.rs` | 交叉淡化 |
| Flip | `transitions.rs` | 垂直翻页 |
| Zoom | `transitions.rs` | 缩放进出 |
| Stack | `transitions.rs` | 卡片堆叠 |
| Blinds | `transitions.rs` | 百叶窗 |
| CoverFlow | `transitions.rs` | Apple 风格封面流 |
| Helix | `transitions.rs` | 螺旋变换 |
| Portal | `transitions.rs` | 传送门/涡流+发光 |

## 六、特殊模式

| 功能 | 文件 | 说明 |
|------|------|------|
| Expose / Mission Control | `expose.rs` | 网格排列所有窗口缩略图 |
| 3D Alt-Tab 棱柱 | `overview.rs` | 六面体旋转窗口切换器 |
| Boss Key (Peek) | `mod.rs` | 快速隐藏/显示所有窗口 |
| Snap 预览 | `mod.rs` | 窗口智能吸附可视化 |
| 窗口标签页 | `mod.rs` | 多窗口分组 tab bar |
| 屏幕录制 | `mod.rs` | PBO 双缓冲 + ffmpeg stdin 实时录屏 |

## 七、后处理 (Post-Processing)

| 功能 | 文件 | 说明 |
|------|------|------|
| 色温调节 | `postprocess.rs` | 蓝光过滤 / 暖色偏移 |
| 饱和度 | `postprocess.rs` | 色彩鲜艳度调整 |
| 亮度 / 对比度 | `postprocess.rs` | 全局画面色调 |
| 颜色反转 | `postprocess.rs` | 无障碍功能 |
| 灰度 | `postprocess.rs` | 无障碍功能 |
| 色盲校正 | `postprocess.rs` | 红/绿/蓝色盲三种模式 |
| 放大镜 | `postprocess.rs` | 鼠标跟随区域放大 |

## 八、X11 扩展依赖

| 扩展 | 用途 |
|------|------|
| **XComposite** | 重定向子窗口渲染到 pixmap |
| **XDamage** | 窗口像素更新通知 |
| **XFixes** | Shape 操作辅助 |
| **GLX** | TFP 绑定、context 管理、SwapBuffers |

## 九、性能优化汇总

1. **Scene Hash 跳帧** — 无变化不渲染
2. **8×6 Tile 损伤追踪** — scissor 只重绘脏区
3. **自适应模糊降质** — 帧时间超标自动减级
4. **Blur 缓存** — 背景 hash 未变则复用
5. **全屏 Unredirect** — 全屏游戏直通显卡
6. **异步壁纸加载** — 后台线程解码，就绪后上传
7. **PBO 双缓冲录屏** — 异步 readback 无 GPU stall
8. **Fence Sync** — 可选异步 pixmap 刷新同步

## 十、调试工具

| 功能 | 说明 |
|------|------|
| Debug HUD | 实时 FPS、帧时间统计 |
| Extended HUD | draw call 数、纹理内存、blur 缓存命中率 |
| 截图 | PNG 导出当前帧或单窗口缩略图 |
| 屏幕标注 | `annotations.rs` 实时画线叠加层 |

---

**代码位置**: `src/backend/x11/compositor/` 目录下共 11 个 Rust 源文件，主模块 `mod.rs` 约 205K。集成点在 `src/backend/x11/backend.rs`。
