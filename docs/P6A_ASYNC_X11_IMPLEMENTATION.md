# P6A: 异步 X11 通信 - 事件队列基础设施 - 实现指南

## 优化概述

**Commit**: eda60ad  
**目标**: 为事件处理和渲染线程分离做准备，实现生产者-消费者模式

### 实现细节

- **模块**: `src/backend/x11/compositor/async_x11.rs` (380 行)
- **策略**: 优先级事件队列 + 延迟操作队列
- **应用场景**: 
  - 事件优先级分级（鼠标/键盘 > 属性变化）
  - 延迟 NameWindowPixmap 到渲染线程
  - 为未来的多线程做准备

### 关键改进

```rust
// Before: 事件处理和渲染在同一线程（串行）
Event Loop:
  read_event() → process_event() → render_frame()
  (任何长操作都会阻塞渲染)

// After: 事件入队，渲染线程批处理（准备异步）
Event Thread:          Render Thread:
  read_event()         process_deferred_ops()
  → push_queue()       pop_queue()
                       → render_frame()
```

---

## 架构设计

### 1. PriorityEventQueue (4 级优先级)

```rust
pub enum InputPriority {
    Low = 0,        // 属性变化、窗口状态
    Normal = 1,     // damage、configure 事件
    High = 2,       // 鼠标/键盘输入 ⭐
    Critical = 3,   // 紧急窗口事件
}
```

**优势**:
- 鼠标/键盘事件优先处理（低延迟）
- 属性变化可延迟（不影响交互）
- 自动优先级调度

### 2. EventQueue (FIFO)

```rust
pub struct EventQueue {
    queue: Arc<Mutex<VecDeque<AsyncX11Event>>>,
    total_events: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
    max_queue_size: usize,
}
```

**特性**:
- 线程安全（Arc<Mutex>）
- 自动溢出处理（丢弃最旧事件）
- 统计信息（总数、丢弃数）

### 3. DeferredOpQueue

```rust
pub struct DeferredOpQueue {
    queue: Arc<Mutex<VecDeque<DeferredX11Op>>>,
    total_ops: Arc<AtomicU64>,
    max_queue_size: usize,
}
```

**用途**:
- 批量 NameWindowPixmap 操作
- 延迟 pixmap 销毁
- 减少 X11 往返次数

---

## 当前状态 (Phase 1: 基础设施)

### ✅ 已完成

1. **事件队列实现**
   - PriorityEventQueue (4 级)
   - EventQueue (FIFO)
   - DeferredOpQueue

2. **集成到 Compositor**
   - `priority_event_queue` 字段
   - `deferred_ops_queue` 字段
   - `process_deferred_x11_ops()` 方法

3. **测试覆盖**
   - 单元测试 (async_x11.rs)
   - 队列溢出处理
   - 优先级排序

### ⏳ 下一步 (Phase 2: 事件线程分离)

1. **创建事件线程**
   ```rust
   // backend.rs
   let event_queue = PriorityEventQueue::new();
   let event_queue_clone = event_queue.clone();
   
   std::thread::spawn(move || {
       loop {
           let event = read_x11_event();
           event_queue_clone.push(event, priority);
       }
   });
   ```

2. **修改主循环**
   ```rust
   // 从队列读取事件而不是直接读 X11
   while let Some(event) = event_queue.pop() {
       process_event(event);
   }
   ```

3. **性能验证**
   - 输入延迟测试
   - 事件丢弃率监控
   - 队列深度分析

---

## 性能预期

### Phase 1 (当前)
- **收益**: 0ms (基础设施，无行为改变)
- **风险**: 低 (只是添加队列，未使用)

### Phase 2 (事件线程)
- **收益**: 10-15ms (输入延迟减少)
- **风险**: 中 (线程同步复杂度)

### 总体 P6 阶段
```
P6C (PBO):        1-3ms
P6B (Fence):      2-5ms
P6A (Event):     10-15ms (Phase 2)
────────────────────────
总计:            13-23ms
```

---

## 使用示例

### 推送事件 (事件线程)

```rust
let event = AsyncX11Event {
    timestamp: Instant::now(),
    event_type: "MotionNotify".to_string(),
    window_id: 0x12345678,
    data: vec![],
};

// 高优先级（鼠标事件）
compositor.priority_event_queue.push(event, InputPriority::High);
```

### 处理事件 (渲染线程)

```rust
// render_frame() 开始时
while let Some(event) = compositor.priority_event_queue.pop() {
    match event.event_type.as_str() {
        "MotionNotify" => handle_motion(event),
        "ButtonPress" => handle_button(event),
        _ => {}
    }
}
```

### 延迟操作

```rust
// 事件处理时，延迟 NameWindowPixmap
let op = DeferredX11Op {
    op_type: "name_pixmap".to_string(),
    window_id: 0x12345678,
    data: vec![],
    deferred_at: Instant::now(),
};
compositor.deferred_ops_queue.defer(op);

// render_frame() 中自动处理
compositor.process_deferred_x11_ops();
```

---

## 测试方法

### 单元测试

```bash
# 运行 async_x11 模块测试
cargo test --lib async_x11 -- --nocapture

# 预期输出
test async_x11::tests::test_event_queue_basic ... ok
test async_x11::tests::test_priority_queue ... ok
test async_x11::tests::test_deferred_op_queue ... ok
```

### 集成测试 (Phase 2)

```bash
# 启动 jwm 并监控事件队列
jwm

# 查看队列统计
tail -f /tmp/jwm.log | grep "event_queue\|deferred_ops"

# 预期日志
# [DEBUG] event_queue: pushed event (priority=High, queue_len=1)
# [DEBUG] deferred_ops: deferred name_pixmap (queue_len=3)
# [DEBUG] deferred_ops: processed 3 ops
```

---

## 已知限制

### Phase 1 (当前)

1. **未使用**: 队列已实现但未集成到事件处理
   - 原因: 需要修改 backend.rs 事件循环
   - 计划: Phase 2 中完成

2. **单线程**: 仍在单线程中运行
   - 原因: 需要谨慎处理线程同步
   - 计划: Phase 2 中分离

### Phase 2 (规划)

1. **线程同步**: 需要处理竞态条件
   - 解决: Mutex + Arc 已就位
   - 风险: 中等

2. **事件丢弃**: 队列满时丢弃最旧事件
   - 影响: 极少（正常情况队列深度 < 10）
   - 监控: 统计 dropped_events

---

## 故障排查

### 问题1: 事件丢弃率高

**症状**: dropped_events 计数快速增长  
**原因**: 事件处理速度 < 事件生成速度  
**诊断**:
```bash
# 查看队列统计
tail -f /tmp/jwm.log | grep "dropped_events"
```

**解决**:
1. 增大 max_queue_size
   ```rust
   PriorityEventQueue::new()  // 默认 64/128/256/32
   ```
2. 优化事件处理速度
3. 启用事件线程 (Phase 2)

### 问题2: 延迟操作堆积

**症状**: deferred_ops_queue.len() 持续增长  
**原因**: render_frame() 中 process_deferred_x11_ops() 未被调用  
**诊断**:
```bash
# 检查日志
grep "processing deferred" /tmp/jwm.log | wc -l
```

**解决**:
- 确保 render_frame() 在每帧调用
- 检查 process_deferred_x11_ops() 是否有异常

---

## 代码统计

```
新增:
  async_x11.rs           380 行
  mod.rs 集成             +15 行
  测试                     30 行

总计: +425 行代码

覆盖:
  PriorityEventQueue      ✅
  EventQueue              ✅
  DeferredOpQueue         ✅
  单元测试                ✅
  集成点                  ✅
```

---

## 下一步行动

### 立即 (Phase 1 完成)
- ✅ 实现队列基础设施
- ✅ 集成到 Compositor
- ✅ 单元测试

### 短期 (Phase 2 规划)
- [ ] 修改 backend.rs 事件循环
- [ ] 创建事件处理线程
- [ ] 集成优先级队列
- [ ] 性能测试

### 中期 (Phase 3)
- [ ] 异步 NameWindowPixmap
- [ ] 输入延迟优化
- [ ] 多线程同步优化

---

## 参考

- Rust 线程: https://doc.rust-lang.org/book/ch16-00-concurrency.html
- Arc<Mutex>: https://doc.rust-lang.org/std/sync/
- VecDeque: https://doc.rust-lang.org/std/collections/struct.VecDeque.html
- P6 优化总结: docs/P6_OPTIMIZATION_SUMMARY.md
