# 架构设计

## 系统概述

Shared Structures是一个基于共享内存的进程间通信库，采用SPSC（Single Producer Single Consumer）模型，提供低延迟、零复制的数据传输机制。

```
┌─────────────────────────────────────────────────────────────┐
│                  SharedRingBuffer                            │
│  ┌─────────────────────────────────────────────────────────┐│
│  │  GenericHeader (128B, cache-aligned)                    ││
│  │  ├─ Magic Number & Version (验证有效性)               ││
│  │  ├─ Message Ring Buffer Indices (write_idx, read_idx) ││
│  │  ├─ Command Ring Buffer Indices                        ││
│  │  ├─ Status & Timestamps                                ││
│  │  └─ Padding (用于后端头部的对齐)                      ││
│  ├─ Backend Header (可变大小，后端特定)                  ││
│  │  └─ Futex/Semaphore/EventFd 状态                      ││
│  ├─ Message Ring Buffer (16+ slots)                       ││
│  │  └─ [MessageSlot] x N                                  ││
│  │     ├─ Timestamp (8B)                                  ││
│  │     ├─ Checksum (4B)                                   ││
│  │     └─ Message (可变)                                  ││
│  └─ Command Ring Buffer (16 slots)                        ││
│     └─ [SharedCommand] x 16                               ││
│        ├─ Command Type                                    ││
│        ├─ Parameter                                       ││
│        └─ Monitor ID & Timestamp                          ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
                          ↓
                   Shared Memory File
                  (/dev/shm/path or flink)
```

## 核心设计原则

### 1. 内存布局优化

**Cache对齐**:
- GenericHeader使用128字节对齐，防止false sharing
- 每个组件在共享内存中精确对齐，避免缓存行污染

**内存紧凑性**:
- 固定大小的消息结构（SharedMessage/SharedCommand）
- 预分配的环形缓冲区，无动态分配
- 精确的字节级布局控制（#[repr(C)]）

### 2. 双缓冲区设计

库维护两个独立的环形缓冲区：

| 组件 | 用途 | 大小 | 同步方式 |
|------|------|------|----------|
| Message Buffer | 数据传输 | 可配置(16+) | 自适应轮询+事件 |
| Command Buffer | 控制命令 | 固定16 | 独立同步 |

### 3. 同步策略

系统支持三层同步机制：

**第一层: 活跃轮询（Adaptive Polling）**
```
生产者写入 → 消费者快速轮询 (< 1us)
└─ 适合高频消息（μs级）
```

**第二层: 事件通知（Event Signaling）**
```
轮询超过自适应阈值(400 spins) → 切换到事件等待
└─ 适合低频消息（ms级以上）
```

**第三层: 后端同步（Backend Sync）**
```
Futex/Semaphore/EventFd → 操作系统级等待
└─ 保证最终的进程唤醒
```

## 模块架构

### 高层模块

```
┌─────────────────────────────────────┐
│      Public API (lib.rs)            │
│  ├─ SharedRingBuffer                │
│  ├─ SharedMessage / MonitorInfo     │
│  ├─ SharedCommand / CommandType     │
│  └─ SyncStrategy                    │
└──────────────┬──────────────────────┘
               │
        ┌──────▼──────────────────────┐
        │   SharedRingBuffer Core      │
        │  (shared_ring_buffer.rs)     │
        │  ├─ create()                 │
        │  ├─ open()                   │
        │  ├─ write() / read()         │
        │  ├─ write_command()          │
        │  └─ read_command()           │
        └──────────────┬───────────────┘
                       │
        ┌──────────────▼───────────────┐
        │  Synchronization Backends    │
        │      (backends/)             │
        │  ├─ Futex                    │
        │  ├─ Semaphore                │
        │  ├─ EventFd                  │
        │  └─ Common Interface         │
        └──────────────────────────────┘
```

### 后端实现

每个同步后端实现 `SyncBackend` Trait：

```rust
pub trait SyncBackend: Send + Sync {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()>;
    fn wait_for_message(&self, has_data: impl Fn() -> bool, ...) -> Result<bool>;
    fn wait_for_command(&self, has_data: impl Fn() -> bool, ...) -> Result<bool>;
    fn signal_message(&self) -> Result<()>;
    fn signal_command(&self) -> Result<()>;
    fn cleanup(&mut self, is_creator: bool);
}
```

**后端选择关系图**:
```
┌─────────────────────────────────────┐
│      Feature Gate Selection         │
├─────────────────────────────────────┤
│ Futex      → use-futex feature      │
│ Semaphore  → use-semaphore feature  │
│ EventFd    → use-eventfd feature    │
│ (Default)  → Futex (lowest latency) │
└─────────────────────────────────────┘
```

## 数据流

### 写入流程

```
1. 获取write_idx (通过atomic load)
2. 计算目标插槽: slot_idx = write_idx & buffer_mask
3. 获取指针: slot = &message_slots[slot_idx]
4. 计算消息校验和
5. 写入: timestamp, checksum, message
6. 递增write_idx (通过atomic compare-and-swap / 直接写)
7. 如果消费者在等待 → signal_message()
```

### 读取流程

```
1. 检查read_idx < write_idx
2. 如果无数据:
   a. 超时未设置 → 返回错误
   b. 超时已设置 → 进入wait_for_message()
3. 获取slot_idx = read_idx & buffer_mask
4. 读取消息和校验和
5. 验证校验和是否匹配
6. 递增read_idx
7. 返回消息
```

## 消息结构

### SharedMessage

```
┌─────────────────────────────────────┐
│     SharedMessage (总 ~5KB)          │
├─────────────────────────────────────┤
│ timestamp (8B)      [毫秒时间戳]     │
│ MonitorInfo {       [监控信息]       │
│   monitor_num (4B)  [监控器编号]     │
│   monitor_width (4B)                 │
│   monitor_height (4B)                │
│   monitor_x (4B)                     │
│   monitor_y (4B)                     │
│   tag_status_vec [TagStatus; 9] (36B)│
│   client_name [u8; 128] (128B)       │
│   ltsymbol [u8; 32] (32B)            │
│ }                                    │
└─────────────────────────────────────┘
```

### SharedCommand

```
┌──────────────────────────────────────┐
│    SharedCommand (32B)                │
├──────────────────────────────────────┤
│ cmd_type (4B)       [命令类型]       │
│ parameter (4B)      [参数值]         │
│ monitor_id (4B)     [目标监控器ID]  │
│ timestamp (8B)      [命令时间戳]     │
└──────────────────────────────────────┘
```

### TagStatus

```
┌────────────────────┐
│ TagStatus (4B)     │
├────────────────────┤
│ is_selected (1b)   │
│ is_urg (1b)        │
│ is_filled (1b)     │
│ is_occ (1b)        │
│ [padding] (28b)    │
└────────────────────┘
```

## 命令类型

```rust
pub enum CommandType {
    None = 0,           // 无操作
    ViewTag = 1,        // 查看特定标签
    ToggleTag = 2,      // 切换标签状态
    SetLayout = 3,      // 设置窗口布局
}
```

## 同步策略详细设计

### Futex (Linux Fast Userspace Mutex)

**优点**:
- 延迟极低（微秒级）
- 无系统调用开销（快速路径）
- 仅在竞争时进入内核

**流程**:
```
写入 → 读取进程等待中? → futex_wake(addr, n)
读取 → 检查有数据? → futex_wait(addr, val, timeout)
```

### Semaphore (POSIX信号量)

**优点**:
- 传统且广泛支持
- 可在进程间和线程间使用
- 计数能力（允许多个等待者）

**流程**:
```
写入 → sem_post(&sem)
读取 → 无数据? → sem_timedwait(&sem, timeout)
```

### EventFd

**优点**:
- 事件驱动模型
- 可与epoll/select集成
- 适合高并发场景

**流程**:
```
写入 → write(eventfd, 1)
读取 → 无数据? → read(eventfd) 阻塞，直到有事件
```

## 故障恢复

### 损坏检测

- **Magic Number验证**: 确保共享内存是有效的库实例
- **Version检查**: 向前兼容性验证
- **Checksum验证**: 每条消息的完整性检查

### 清理机制

```
Creator Process:
  └─ SharedRingBuffer drop()
     ├─ backend.cleanup(is_creator=true)
     └─ ShmemConf::force_create_flink() 删除文件

Non-creator Process:
  └─ SharedRingBuffer drop()
     └─ backend.cleanup(is_creator=false)
        └─ 仅释放本地资源
```

## 并发特性

### 内存顺序

- **写入**: 使用 `Ordering::Release` 确保所有写入对其他进程可见
- **读取**: 使用 `Ordering::Acquire` 确保读取最新值
- **环形缓冲区索引**: 使用 `Ordering::Relaxed`（无竞争）或 `Ordering::SeqCst`（验证）

### 线程安全

- 实现 `Send + Sync` 确保可在线程间传递
- 所有共享状态使用原子操作
- 环形缓冲区本身线程不安全（SPSC模型），但每个进程内的使用是安全的

## 性能考虑

### 缓存效应

1. **L1缓存**: GenericHeader(128B) 单独一条缓存行
2. **L3缓存**: 整个共享内存段可能在L3
3. **NUMA**: 在NUMA系统中，共享内存绑定到创建进程的NUMA节点

### 轮询策略

```
Start → 活跃轮询 (400 spins)
        ↓ (每次检查一个原子操作)
      有数据? ─Yes→ 返回
        ↓ No
      超过spins? ─No→ 继续轮询
        ↓ Yes
      事件等待 (futex/sem/eventfd)
        ↓
      返回或超时
```

## 扩展点

### 添加新的同步后端

1. 在 `backends/` 创建新文件（如 `backends/custom.rs`）
2. 实现 `SyncBackend` trait
3. 定义后端特定的 Header 结构
4. 在 `backends/mod.rs` 中添加模块声明
5. 在 `backends/common.rs` 的 `AnySyncBackend` 中添加枚举变体
6. 在 `Cargo.toml` 中添加对应的 feature

### 修改消息格式

1. 修改 `shared_message.rs` 中的 `SharedMessage` 或 `MonitorInfo`
2. 更新 `RING_BUFFER_VERSION` 以维护兼容性追踪
3. 更新校验和计算逻辑
4. 更新相关文档和测试

## 已知限制

1. **SPSC模型**: 不支持多生产者或多消费者
2. **固定消息大小**: SharedMessage 大小固定，无法动态扩展
3. **单机通信**: 基于共享内存，仅限同一机器上的进程
4. **Linux专属**: 依赖于Linux特有的同步机制
5. **32位环形缓冲区**: 索引为u32，最多支持4GB索引空间

## 安全性分析

### 内存安全

- ✓ 使用Rust类型系统防止缓冲区溢出
- ✓ 指针操作均为unsafe块，仔细标记和注释
- ✓ 共享内存创建和清理有明确的所有权

### 数据完整性

- ✓ 校验和验证消息未损坏
- ✓ Magic Number和版本防止格式错配
- ✓ 原子操作保证索引一致性

### 进程隔离

- ✓ 文件权限控制共享内存访问
- ✓ 命令验证可防止恶意命令
- ✗ 不防止同一用户的恶意进程（设计限制）
