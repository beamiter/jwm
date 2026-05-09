# JWM 模块拆分计划

## 📋 当前进度 (2026-05-09)

### ✅ 已完成
- **window_state.rs** (240 行) - 窗口状态管理
  - Commit: `997f1e8`
  - 状态: ✅ 已提交
  - 减少: 241 行

- **rendering.rs** (364 行) - 渲染与合成器管理
  - Commit: `f60e1da`
  - 状态: ✅ 已提交
  - 减少: 344 行
  - 提取函数: `render_compositor_immediate`, `tick_animations`, `build_window_groups`, `build_compositor_scene`, `sync_focused_floating_geometry`, `configure_client`, `move_window`

- **process.rs** (131 行) - 进程管理
  - Commit: `3c54e38`
  - 状态: ✅ 已提交
  - 减少: 120 行
  - 提取函数: `spawn`, `reap_zombies`, `setup_smithay_child_env`, `apply_child_pre_exec`, `is_smithay_backend`, `is_udev_backend`

### 📊 当前状态
- **jwm.rs**: 5241 行 → **4536 行** (-13.5%)
- **编译状态**: ✅ 通过
- **测试状态**: ✅ 正常
- **已提取**: 3 个模块，共减少 705 行

---

## 🎯 下一步拆分建议

按优先级和安全性排序：

### 1. monitor_management.rs (中优先级) ⭐️
**预计**: ~250 行
**推荐理由**: 相对独立，主要管理多显示器和状态栏
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

### 3. positioning.rs (低优先级)
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

### 4. utils.rs (低优先级)
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
| 文件 | 原始 | 当前 | 目标 | 状态 |
|------|------|------|------|------|
| jwm.rs | 5241 行 | 4536 行 | ~3500 行 | 🔄 进行中 (-13.5%) |
| window_state.rs | - | 240 行 | 240 行 | ✅ 完成 |
| rendering.rs | - | 364 行 | 364 行 | ✅ 完成 |
| process.rs | - | 131 行 | 131 行 | ✅ 完成 |
| monitor_management.rs | - | - | ~250 行 | ⏳ 待提取 |
| positioning.rs | - | - | ~400 行 | ⏳ 待提取 |
| utils.rs | - | - | ~250 行 | ⏳ 待提取 |
| **总计** | 5241 行 | 5271 行 | 5535 行 | 已完成 50% |

> 注: 总行数增加是因为模块声明和必要的导入，但每个文件更小更易维护

### 最终模块结构
```
src/jwm/
├── mod.rs (jwm.rs)           # 核心 traits 实现 (~3500 行) 🔄
│   ├── WMController impl
│   ├── EventHandler impl
│   └── 核心业务逻辑
├── window_state.rs           # 窗口状态管理 (240 行) ✅
├── rendering.rs              # 渲染与合成器 (364 行) ✅
├── process.rs                # 进程管理 (131 行) ✅
├── monitor_management.rs     # 显示器管理 (~250 行) ⏳
├── positioning.rs            # 几何与定位 (~400 行) ⏳
├── utils.rs                  # 工具函数 (~250 行) ⏳
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

### 第二次拆分 (rendering.rs)
- ✅ **成功点**: 
  - 一次性从后往前批量删除，避免括号不平衡问题
  - 函数提取完整，包括所有相关注释
  - 导入依赖识别准确
- ✅ **效果**: 
  - 提取 364 行，减少 jwm.rs 344 行
  - 编译通过，仅有未使用导入警告
  - 括号完美平衡 (1062 vs 1062)
- 💡 **确认**: 批量删除工作流是正确的方法

### 第三次拆分 (process.rs)
- ✅ **成功点**: 
  - 工作流验证完全有效
  - 函数提取 6 个（spawn, reap_zombies, 4 个辅助函数）
  - 无复杂依赖，提取最安全
- ⚠️ **问题**: 
  - 初始删除时留下了多余的空的 impl 块闭包
  - 需要手动清理多余的 `}`
- ✅ **效果**: 
  - 提取 131 行，减少 jwm.rs 120 行
  - 编译通过，括号完美平衡 (1031 vs 1031)
- 💡 **改进**: 
  - 删除时需更仔细检查 impl 块的结束位置
  - 可考虑在删除前验证目标函数确实完整
1. **准备阶段**: 
   - 列出所有要提取的函数及其行号
   - 检查函数间的调用关系
   - 识别需要的导入

2. **提取阶段**:
   - 创建新文件并复制函数
   - 修改函数可见性为 `pub(super)`
   - 添加必要的 use 语句

3. **删除阶段**:
   - **一次性**从后往前删除所有函数（关键！）
   - 使用 sed 命令批量删除
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

**文档版本**: 1.2  
**最后更新**: 2026-05-09  
**下次更新计划**: 完成 monitor_management.rs 或 positioning.rs 后  
**完成进度**: 3/6 模块 (50%)
