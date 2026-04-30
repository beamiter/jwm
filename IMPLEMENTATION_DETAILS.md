# 实现细节

## 环形缓冲区实现

### 内存布局计算

SharedRingBuffer采用精确的内存布局计算确保不同后端间的互操作性：

```rust
let generic_header_size = size_of::<GenericHeader>();     // 128B
let backend_header_size = strategy.backend_size();        // 可变
let backend_header_align = strategy.backend_align();      // 可变

let backend_offset = align_up(generic_header_size, backend_header_align);
let messages_offset = align_up(
    backend_offset + backend_header_size,
    std::mem::align_of::<MessageSlot>()
);
let messages_size = buffer_size * size_of::<MessageSlot>();
let commands_offset = align_up(
    messages_offset + messages_size,
    std::mem::align_of::<SharedCommand>()
);
let commands_size = CMD_BUFFER_SIZE * size_of::<SharedCommand>();
let total_size = commands_offset + commands_size;
```

**对齐策略:**

```
┌─────────────────────────────────┐
│ GenericHeader (128B, align=128) │ Offset: 0
├─────────────────────────────────┤
│ [Padding to backend align]      │ Offset: 128
├─────────────────────────────────┤
│ Backend Header (variable)       │ Offset: backend_offset
├─────────────────────────────────┤
│ [Padding to MessageSlot align]  │
├─────────────────────────────────┤
│ MessageSlot[0] (40B)            │ Offset: messages_offset
│ MessageSlot[1]                  │
│ ...                             │
│ MessageSlot[N-1]                │
├─────────────────────────────────┤
│ [Padding to Command align]      │
├─────────────────────────────────┤
│ SharedCommand[0] (32B)          │ Offset: commands_offset
│ SharedCommand[1]                │
│ ...                             │
│ SharedCommand[15]               │
└─────────────────────────────────┘
```

### 环形缓冲区索引管理

#### 索引运算

```rust
#[inline]
fn buffer_mask(&self) -> u32 {
    self.buffer_size() - 1  // buffer_size 必须是2的幂
}

#[inline]
fn wrap_index(idx: u32, mask: u32) -> u32 {
    idx & mask  // 快速取模
}
```

#### 写入操作

```rust
pub fn write(&self, message: &SharedMessage) -> Result<()> {
    unsafe {
        let header = &*self.header;
        
        // 1. 原子读取当前索引
        let write_idx = header.write_idx.load(Ordering::Relaxed);
        let read_idx = header.read_idx.load(Ordering::Acquire);
        
        // 2. 检查缓冲区是否满
        let next_write = (write_idx + 1) & self.buffer_mask();
        if next_write == (read_idx & self.buffer_mask()) {
            return Err(Error::new(ErrorKind::WouldBlock, "Buffer full"));
        }
        
        // 3. 获取目标插槽
        let slot_idx = (write_idx & self.buffer_mask()) as usize;
        let slot = &mut *self.message_slots.add(slot_idx);
        
        // 4. 计算校验和
        let checksum = calculate_message_checksum(message);
        
        // 5. 写入消息数据
        slot.timestamp = now_millis();
        slot.checksum = checksum;
        slot.message = *message;
        
        // 6. 原子更新写入索引（Release确保上述写入对读者可见）
        header.write_idx.store(write_idx + 1, Ordering::Release);
        
        // 7. 如果读者在等待，唤醒它
        if header.is_destroyed.load(Ordering::Acquire) == false {
            let _ = self.backend.signal_message();
        }
    }
    Ok(())
}
```

#### 读取操作

```rust
pub fn read(&self, timeout: Option<Duration>) -> Result<SharedMessage> {
    unsafe {
        let header = &*self.header;
        
        loop {
            // 1. 原子读取索引
            let read_idx = header.read_idx.load(Ordering::Relaxed);
            let write_idx = header.write_idx.load(Ordering::Acquire);
            
            // 2. 检查是否有数据
            if read_idx != write_idx {
                // 3. 获取消息
                let slot_idx = (read_idx & self.buffer_mask()) as usize;
                let slot = &*self.message_slots.add(slot_idx);
                
                // 4. 验证校验和
                let expected_checksum = calculate_message_checksum(&slot.message);
                if slot.checksum != expected_checksum {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Checksum mismatch"
                    ));
                }
                
                // 5. 更新读索引
                header.read_idx.store(read_idx + 1, Ordering::Release);
                
                return Ok(slot.message);
            }
            
            // 6. 无数据，根据参数决定是否等待
            if timeout.is_none() {
                return Err(Error::new(ErrorKind::WouldBlock, "No data"));
            }
            
            // 7. 等待数据，带自适应轮询
            let has_data = || {
                let read_idx = header.read_idx.load(Ordering::Relaxed);
                let write_idx = header.write_idx.load(Ordering::Acquire);
                read_idx != write_idx
            };
            
            match self.backend.wait_for_message(
                has_data,
                self.adaptive_poll_spins,
                timeout
            ) {
                Ok(true) => continue,  // 有数据，重新检查
                Ok(false) => return Err(Error::new(ErrorKind::TimedOut, "Timeout")),
                Err(e) => return Err(e),
            }
        }
    }
}
```

### 校验和计算

校验和采用简单的XOR混合策略，既快又能检测大多数位翻转：

```rust
fn calculate_message_checksum(m: &SharedMessage) -> u32 {
    let mut sum = 0u32;
    
    // 时间戳
    mix_u64(&mut sum, m.timestamp);
    
    let mi = &m.monitor_info;
    
    // 标量字段
    mix_i32(&mut sum, mi.monitor_num);
    mix_i32(&mut sum, mi.monitor_width);
    mix_i32(&mut sum, mi.monitor_height);
    mix_i32(&mut sum, mi.monitor_x);
    mix_i32(&mut sum, mi.monitor_y);
    
    // 标签状态（压缩为位）
    for ts in &mi.tag_status_vec {
        let bits: u8 = (ts.is_selected as u8)
            | ((ts.is_urg as u8) << 1)
            | ((ts.is_filled as u8) << 2)
            | ((ts.is_occ as u8) << 3);
        sum = sum.wrapping_add(bits as u32);
    }
    
    // 字符数组
    for &b in &mi.client_name {
        sum = sum.wrapping_add(b as u32);
    }
    for &b in &mi.ltsymbol {
        sum = sum.wrapping_add(b as u32);
    }
    
    sum
}
```

**校验和策略特点:**
- 使用wrapping_add确保溢出时的一致性行为
- 对所有字节进行XOR混合，检测单比特翻转
- 计算量小，不超过消息写入时间的1%

## 同步后端实现

### Futex后端

```rust
#[repr(C)]
pub struct FutexHeader {
    msg_futex: i32,      // futex变量，用于消息同步
    cmd_futex: i32,      // futex变量，用于命令同步
}

impl SyncBackend for FutexBackend {
    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        // 1. 活跃轮询阶段
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
            std::hint::spin_loop();  // CPU友好的旋转
        }
        
        // 2. futex等待阶段
        let futex_addr = unsafe { &(*self.msg_futex_ptr).msg_futex };
        let val = futex_addr.load(Ordering::Relaxed);
        
        match futex_wait(futex_addr, val, timeout) {
            Ok(_) => {
                // 唤醒后重新检查数据
                Ok(has_data())
            }
            Err(e) => Err(e),
        }
    }
    
    fn signal_message(&self) -> Result<()> {
        let futex_addr = unsafe { &(*self.msg_futex_ptr).msg_futex };
        futex_addr.fetch_add(1, Ordering::Release);
        futex_wake(futex_addr, 1)
    }
}
```

**Futex的工作流:**
```
写入→变量改变→futex_wake()
             ↓
          读进程在futex_wait()
          ↓ (内核唤醒)
          返回，重新检查数据
```

### Semaphore后端

```rust
#[repr(C)]
pub struct SemaphoreHeader {
    msg_sem: sem_t,     // POSIX信号量，初值为0
    cmd_sem: sem_t,     // POSIX信号量，初值为0
}

impl SyncBackend for SemaphoreBackend {
    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        // 轮询
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
            std::hint::spin_loop();
        }
        
        // 信号量等待
        match sem_timedwait(&self.sem, timeout) {
            Ok(_) => Ok(has_data()),
            Err(e) => Err(e),
        }
    }
    
    fn signal_message(&self) -> Result<()> {
        sem_post(&self.sem)
    }
}
```

**信号量的优势:**
- 可以累积计数（允许多个待处理事件）
- 跨平台支持
- 线程和进程都可用

### EventFd后端

```rust
#[repr(C)]
pub struct EventFdHeader {
    msg_fd: i32,        // eventfd文件描述符
    cmd_fd: i32,        // eventfd文件描述符
}

impl SyncBackend for EventFdBackend {
    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        // 轮询
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
        }
        
        // eventfd等待
        let fd = self.msg_fd;
        let mut buf = [0u8; 8];
        
        // 设置非阻塞，使用select/poll确定超时
        match self.read_with_timeout(fd, &mut buf, timeout) {
            Ok(_) => Ok(has_data()),
            Err(e) => Err(e),
        }
    }
    
    fn signal_message(&self) -> Result<()> {
        let val: u64 = 1;
        write(self.msg_fd, &val.to_le_bytes())
    }
}
```

**EventFd的特点:**
- 可与epoll/select集成
- 适合多进程监控场景
- 更节能（避免轮询）

## 自适应轮询策略

自适应轮询是一个关键的性能优化，平衡低延迟和CPU开销：

```rust
const DEFAULT_ADAPTIVE_POLL_SPINS: u32 = 400;

pub fn read(&self, timeout: Option<Duration>) -> Result<SharedMessage> {
    // 首先进行活跃轮询
    for _ in 0..self.adaptive_poll_spins {
        // 检查并读取
        if let Some(msg) = try_read_message() {
            return Ok(msg);
        }
        
        // CPU友好的自旋
        std::hint::spin_loop();
    }
    
    // 轮询超时，使用事件等待
    // 这避免了没有数据时的CPU浪费
}
```

**轮询次数的效果:**

| 轮询次数 | CPU时间 | 延迟 | 功耗 |
|---------|---------|------|------|
| 0 | 低 | 1-10ms | 低 |
| 100 | 中 | 100-500μs | 中 |
| 400 | 高 | 10-100μs | 高 |
| 1000+ | 极高 | <10μs | 极高 |

建议: 根据消息频率选择合适值

## 内存序(Memory Ordering)

SharedRingBuffer的内存序使用策略：

### Acquire-Release同步

```rust
// 写入端
header.write_idx.store(new_idx, Ordering::Release);
//                                ↑
//                        所有此前的写入对读端可见

// 读取端  
let idx = header.read_idx.load(Ordering::Acquire);
//                              ↑
//                        此后的操作不会在读之前执行
```

### 松散操作（Relaxed）

```rust
// 在循环内频繁读取，无竞争
let write_idx = header.write_idx.load(Ordering::Relaxed);
let read_idx = header.read_idx.load(Ordering::Relaxed);
```

### Sequential Consistency

仅在必要时使用，例如调试或验证：

```rust
// 追踪最后一条消息时间戳
header.last_timestamp.compare_exchange(
    old,
    new,
    Ordering::SeqCst,  // 最强的同步
    Ordering::SeqCst
)?;
```

## Drop实现和资源清理

```rust
impl Drop for SharedRingBuffer {
    fn drop(&mut self) {
        if let Err(e) = self.destroy() {
            log::warn!("Error destroying buffer: {}", e);
        }
    }
}

pub fn destroy(&self) -> Result<()> {
    unsafe {
        // 标记为已销毁，防止进一步的同步操作
        (*self.header).is_destroyed.store(true, Ordering::Release);
    }
    
    // 后端清理
    self.backend.cleanup(self.is_creator);
    
    // 共享内存自动释放（Shmem的Drop实现）
    // 如果是创建者，删除文件；非创建者只释放映射
    Ok(())
}
```

## 并发安全分析

### 竞态条件避免

```
写线程                      读线程
┌──────────────┐          ┌──────────────┐
│ write_idx++  │          │ read_idx     │
│ (Release)    │          │ (Acquire)    │
└────┬─────────┘          └──────┬───────┘
     │ happens-before ────────────│
     └──────────────────────────────┘
     写入的所有效果对读端可见
```

### 缓冲区满/空检查

```rust
// 检查满：读和写不能同时有未读数据
let next_write = (write_idx + 1) % buffer_size;
if next_write == (read_idx % buffer_size) {
    // 满了，无法写入
}

// 检查空：write_idx不能超过read_idx太多
if read_idx == write_idx {
    // 空的，无数据读取
}
```

## 故障恢复机制

### Magic Number验证

```rust
const RING_BUFFER_MAGIC: u64 = 0x52494E47_42554646;  // "RINGBUFF"

// 打开时验证
let magic = header.magic.load(Ordering::Acquire);
if magic != RING_BUFFER_MAGIC {
    return Err(Error::new(ErrorKind::InvalidData, "Invalid magic"));
}
```

### 版本兼容性

```rust
const RING_BUFFER_VERSION: u64 = 8;

// 版本检查允许升级
let version = header.version.load(Ordering::Acquire);
if version > RING_BUFFER_VERSION {
    log::warn!("Buffer version {} > {}, may have issues", version, RING_BUFFER_VERSION);
}
```

### 完整性验证流程

```
打开 → Magic检查 ─Good→ Version检查 ─Good→ 初始化后端 → 使用
       │                 │                   │
       └─Bad→ 返回错误   └─Bad→ 警告并继续 └─Bad→ 返回错误
```

## 性能优化技巧

### 1. 预分配缓冲区

```rust
// 创建适当大小的缓冲区，避免频繁满
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(256),  // 预分配更大的缓冲区
    None
)?;
```

### 2. 调整轮询参数

```rust
// 低延迟应用：增加轮询次数
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(16),
    Some(1000)  // 1000次轮询，延迟 < 50μs
)?;

// 低功耗应用：减少轮询次数
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::EventFd,
    Some(16),
    Some(10)    // 10次轮询，快速切换到事件等待
)?;
```

### 3. 批量操作

```rust
// 虽然是SPSC，但可以在消息内携带多项数据
let mut msg = SharedMessage::new();
// 填充msg的多个字段
buffer.write(&msg)?;
```

### 4. 选择合适的同步策略

- **Futex**: 高频消息（> 1000/s）
- **EventFd**: 中频消息（10-1000/s）
- **Semaphore**: 低频消息（< 10/s）或跨平台

## 测试与验证

### 单元测试

库包含结构对齐、消息序列化等单元测试：

```rust
#[test]
fn test_struct_alignment() {
    assert!(std::mem::size_of::<SharedMessage>() > 0);
    assert_eq!(std::mem::size_of::<SharedCommand>(), 32);
}
```

### 基准测试

```bash
# 运行基准测试
cargo bench --bench ring_buffer_bench

# 输出示例：
# write (no contention) ... time: [850.5 ns 851.2 ns 852.1 ns]
# read (no contention)  ... time: [123.4 ns 124.1 ns 124.8 ns]
```

### 压力测试

```bash
# 运行压力测试（10M消息）
cargo bench --bench stress_test
```

## 调试与日志

库使用 `log` crate进行日志记录：

```rust
use log::{info, warn, error};

// 启用日志
export RUST_LOG=shared_structures=debug

// 在代码中查看
info!("Successfully opened shared ring buffer: {}", path);
warn!("Failed to open existing buffer, creating new one");
error!("Failed to create shared ring buffer: {}", err);
```

## 已知的实现权衡

| 特性 | 选择 | 理由 |
|------|------|------|
| 环形缓冲区大小 | 编译时固定 | 性能和简化内存管理 |
| 索引类型 | u32 | 足够大且更快 |
| 校验和算法 | 简单XOR | 轻量级且足够的覆盖 |
| 轮询策略 | 自适应 | 平衡延迟和功耗 |
| 同步后端 | 可选feature | 灵活选择vs编译大小 |
