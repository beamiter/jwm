# JWM 模块拆分计划

## 📋 当前进度 (2026-05-09)

### ✅ 已完成
- **window_state.rs** (240 行) - 窗口状态管理
  - Commit: `997f1e8`
  - 状态: ✅ 已提交
  - 减少: 241 行

### 📊 当前状态
- **jwm.rs**: 5241 行 → **5000 行** (-4.6%)
- **编译状态**: ✅ 通过
- **测试状态**: ✅ 正常

---

## 🎯 下一步拆分建议

按优先级和安全性排序：

### 1. rendering.rs (高优先级) 
**预计**: ~350 行

#### 需要提取的函数
| 函数名 | 原始行号 | 说明 |
|--------|---------|------|
| `render_compositor_immediate` | 929-954 | 渲染器立即渲染 |
| `tick_animations` | 2107-2218 | 动画帧更新 |
| `build_window_groups` | 2219-2247 | 构建窗口标签组 |
| `build_compositor_scene` | 2248-2361 | 构建渲染场景 |
| `sync_focused_floating_geometry` | 2363-2386 | 同步浮动窗口几何 |
| `configure_client` | 2388-2419 | 配置客户端窗口 |
| `move_window` | 2421-2430 | 移动窗口 |

#### 依赖关系
- **被调用者**: X11 backend (render_compositor_immediate), 主循环 (tick_animations)
- **调用其他**: `get_selected_client_key`, `is_client_visible_on_monitor`, `get_monitor_clients`
- **访问字段**: `last_stacking`, `or_window_geometries`, `secondary_bars`, `animations`

#### 提取步骤
```bash
# 1. 创建 rendering.rs 并添加必要导入
cat > src/jwm/rendering.rs << 'EOF'
use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use std::collections::HashMap;
use log::info;

use super::Jwm;

impl Jwm {
    // 在这里添加函数...
}
EOF

# 2. 从 jwm.rs 提取函数（手动复制并添加 pub(super)）
# 3. 添加模块声明到 jwm.rs
sed -i '17 a pub mod rendering;' src/jwm.rs

# 4. 从后往前删除函数（避免行号偏移）
sed -i '2421,2430d' src/jwm.rs  # move_window
sed -i '2388,2419d' src/jwm.rs  # configure_client
sed -i '2363,2386d' src/jwm.rs  # sync_focused_floating_geometry
sed -i '2248,2361d' src/jwm.rs  # build_compositor_scene
sed -i '2219,2247d' src/jwm.rs  # build_window_groups
sed -i '2107,2218d' src/jwm.rs  # tick_animations
sed -i '929,954d' src/jwm.rs    # render_compositor_immediate

# 5. 验证编译
cargo check --lib
```

---

### 2. monitor_management.rs (中优先级)
**预计**: ~250 行

#### 需要提取的函数
| 函数名 | 原始行号 | 说明 |
|--------|---------|------|
| `createmon` | 2433 | 创建显示器 |
| `dirtomon` | 2463 | 方向查找显示器 |
| `ensure_secondary_bars_running` | 2486 | 确保状态栏运行 |
| `spawn_secondary_bar` | 2533 | 生成状态栏进程 |
| `flush_pending_bar_updates` | 2713 | 刷新状态栏更新 |
| `switch_to_monitor` | 4851 | 切换到显示器 |

#### 特点
- 相对独立
- 主要管理多显示器和状态栏
- 依赖 `CONFIG`、`message` 字段

---

### 3. process.rs (中优先级)
**预计**: ~200 行

#### 需要提取的函数
| 函数名 | 原始行号 | 说明 |
|--------|---------|------|
| `spawn` | 2887 | 生成子进程 |
| `reap_zombies` | 3056 | 回收僵尸进程 |
| `setup_smithay_child_env` | 2847 | 设置 Smithay 环境 |
| `apply_child_pre_exec` | 2872 | 应用 pre-exec 钩子 |
| `is_smithay_backend` | 2825 | 检查后端类型 |
| `is_udev_backend` | 2838 | 检查 udev 后端 |

#### 特点
- 进程管理相关
- 几乎无内部依赖
- 最安全提取

---

### 4. positioning.rs (低优先级)
**预计**: ~400 行

#### 需要提取的函数
| 函数名 | 原始行号 | 说明 |
|--------|---------|------|
| `resize_client` | 1869 | 调整客户端大小 |
| `resizeclient` | 1889 | 实际调整操作 |
| `recttomon` | 2794 | 矩形到显示器 |
| `wintomon` | 2805 | 窗口到显示器 |
| `getrootptr` | 2786 | 获取根指针位置 |

#### 注意
- 与 layout 模块紧密耦合
- 调用 `arrange` 频繁
- 建议最后拆分

---

### 5. utils.rs (低优先级)
**预计**: ~250 行

#### 需要提取的函数
- `clean_mask` - 清理按键掩码
- `target_to_monitor` - 目标转换
- `get_transient_for` - 获取临时父窗口
- `truncate_chars` - 字符串截断
- `fetch_window_title` - 获取窗口标题
- `check_monitor_consistency` - 检查显示器一致性

#### 特点
- 纯工具函数
- 无状态依赖
- 可选提取

---

## ⚠️ 拆分注意事项

### 常见陷阱
1. **括号不平衡**
   - 删除函数后检查 `{` 和 `}` 数量
   - 使用: `grep -o '{' file.rs | wc -l`

2. **残留代码**
   - 检查删除位置前后是否有孤立的注释或括号
   - 特别注意 doc comments (`///`)

3. **行号偏移**
   - **必须从后往前删除**函数
   - 删除前记录所有函数的行号
   - 每次删除后重新验证

4. **可见性设置**
   - 提取的函数使用 `pub(super)` 而不是 `pub`
   - 只在函数定义行添加，不要修改函数体

### 验证清单
- [ ] 括号平衡: `grep -o '{' | wc -l` == `grep -o '}' | wc -l`
- [ ] 编译通过: `cargo check --lib`
- [ ] 无警告（除了 unused imports）
- [ ] 提交前测试: `cargo test`

---

## 📈 预期最终效果

### 文件大小对比
| 文件 | 当前 | 目标 | 减少 |
|------|------|------|------|
| jwm.rs | 5000 行 | ~3500 行 | -30% |
| window_state.rs | 240 行 | 240 行 | 新增 |
| rendering.rs | - | ~350 行 | 新增 |
| monitor_management.rs | - | ~250 行 | 新增 |
| process.rs | - | ~200 行 | 新增 |
| positioning.rs | - | ~400 行 | 新增 |
| utils.rs | - | ~250 行 | 新增 |
| **总计** | 5000 行 | 5190 行 | +190 行 |

> 注: 总行数增加是因为模块声明和必要的导入，但每个文件更小更易维护

### 最终模块结构
```
src/jwm/
├── mod.rs (jwm.rs)           # 核心 traits 实现 (~3500 行)
│   ├── WMController impl
│   ├── EventHandler impl
│   └── 核心业务逻辑
├── window_state.rs           # 窗口状态管理 (240 行) ✅
├── rendering.rs              # 渲染与合成器 (350 行)
├── monitor_management.rs     # 显示器管理 (250 行)
├── process.rs                # 进程管理 (200 行)
├── positioning.rs            # 几何与定位 (400 行)
├── utils.rs                  # 工具函数 (250 行)
├── client.rs                 # 客户端管理 ✅
├── navigation.rs             # 窗口导航 ✅
├── input_handler.rs          # 输入处理 ✅
├── lifecycle.rs              # 生命周期 ✅
├── ipc_handler.rs            # IPC 处理 ✅
├── focus.rs                  # 焦点管理 ✅
├── geometry.rs               # 几何计算 ✅
├── constraints.rs            # 约束管理 ✅
└── ... (其他已有模块)
```

---

## 🔧 快速参考命令

### 提取函数模板
```bash
# 1. 创建新文件
cat > src/jwm/MODULE_NAME.rs << 'EOF'
use crate::backend::api::Backend;
// 添加其他必要导入

use super::Jwm;

impl Jwm {
    // 粘贴函数，并将 fn 改为 pub(super) fn
}
EOF

# 2. 添加模块声明
sed -i '17 a pub mod MODULE_NAME;' src/jwm.rs

# 3. 删除函数（从后往前）
sed -i 'START_LINE,END_LINEd' src/jwm.rs

# 4. 验证
cargo check --lib
git diff --stat
```

### 检查括号平衡
```bash
echo "左括号: $(grep -o '{' src/jwm.rs | wc -l)"
echo "右括号: $(grep -o '}' src/jwm.rs | wc -l)"
```

### 查找函数位置
```bash
grep -n "fn FUNCTION_NAME" src/jwm.rs
```

---

## 📝 提交建议

每完成一个模块后单独提交：

```bash
git add src/jwm/MODULE_NAME.rs src/jwm.rs
git commit -m "refact: Extract MODULE_NAME to jwm/MODULE_NAME.rs

Extracted MODULE-related functions from jwm.rs:
- function1: description
- function2: description
...

Stats: jwm.rs reduced from XXXX to YYYY lines (-ZZ lines)"
```

---

## 🎓 经验教训

### 第一次拆分 (window_state.rs)
- ✅ **成功点**: 函数选择合理，依赖关系清晰
- ⚠️ **问题**: 多次遇到括号不平衡（因为分步删除）
- 💡 **改进**: 应该一次性记录所有行号，然后从后往前批量删除

### 建议工作流
1. **准备阶段**: 
   - 列出所有要提取的函数及其行号
   - 检查函数间的调用关系
   - 识别需要的导入

2. **提取阶段**:
   - 创建新文件并复制函数
   - 修改函数可见性为 `pub(super)`
   - 添加必要的 use 语句

3. **删除阶段**:
   - **一次性**从后往前删除所有函数
   - 不要分步删除（避免行号混乱）

4. **验证阶段**:
   - 检查括号平衡
   - `cargo check`
   - `git diff` 确认改动正确

---

## 📞 遇到问题？

### 常见错误及解决方案

**错误**: `unexpected closing delimiter: }`
- **原因**: 删除时破坏了括号平衡
- **解决**: 检查删除位置前后是否有多余的 `}` 或遗漏的 `{`

**错误**: `method XXX is private`
- **原因**: 忘记添加 `pub(super)`
- **解决**: 在函数定义前添加 `pub(super)`

**错误**: `duplicate definitions with name XXX`
- **原因**: 函数在 jwm.rs 中没有删除干净
- **解决**: `grep -n "fn XXX" src/jwm.rs` 查找并删除残留

---

**文档版本**: 1.0  
**最后更新**: 2026-05-09  
**下次更新计划**: 完成 rendering.rs 后
