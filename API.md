# API 文档

## 目录

- [SharedRingBuffer](#sharedringbuffer) - 核心环形缓冲区
- [SharedMessage](#sharedmessage) - 消息结构
- [MonitorInfo](#monitorinfo) - 监控信息
- [SharedCommand](#sharedcommand) - 命令结构
- [CommandType](#commandtype) - 命令类型枚举
- [TagStatus](#tagstatus) - 标签状态
- [SyncStrategy](#syncstrategy) - 同步策略
- [常量](#常量)

---

## SharedRingBuffer

高性能的SPSC环形缓冲区，用于进程间数据交换。

### 创建与打开

#### `create(path, strategy, buffer_size, adaptive_poll_spins) -> Result<Self>`

创建一个新的共享环形缓冲区。

**参数:**
- `path: &str` - 共享内存文件路径（如 `/tmp/my_buffer`）
- `strategy: SyncStrategy` - 同步策略（Futex/Semaphore/EventFd）
- `buffer_size: Option<usize>` - 消息缓冲区大小，默认16，必须是2的幂
- `adaptive_poll_spins: Option<u32>` - 自适应轮询次数，默认400

**返回:** `Result<Self>` - 成功则返回SharedRingBuffer实例

**错误:**
- 缓冲区大小非2的幂
- 共享内存创建失败
- 后端初始化失败

**示例:**
```rust
let buffer = SharedRingBuffer::create(
    "/tmp/my_ipc",
    SyncStrategy::Futex,
    Some(16),
    Some(400)
)?;
```

#### `open(path, strategy, adaptive_poll_spins) -> Result<Self>`

打开已存在的共享环形缓冲区。

**参数:**
- `path: &str` - 共享内存文件路径
- `strategy: SyncStrategy` - 同步策略
- `adaptive_poll_spins: Option<u32>` - 自适应轮询次数

**返回:** `Result<Self>` - 成功则返回SharedRingBuffer实例

**错误:**
- 共享内存不存在
- 版本或magic验证失败
- 后端初始化失败

**示例:**
```rust
let buffer = SharedRingBuffer::open(
    "/tmp/my_ipc",
    SyncStrategy::Futex,
    None
)?;
```

#### `create_shared_ring_buffer(path, strategy) -> Option<Self>`

便捷方法：尝试打开，失败则创建。

**参数:**
- `path: &str` - 共享内存路径
- `strategy: SyncStrategy` - 同步策略

**返回:** `Option<Self>` - 成功返回Some，失败返回None

**示例:**
```rust
let buffer = SharedRingBuffer::create_shared_ring_buffer(
    "/tmp/my_ipc",
    SyncStrategy::Futex
)?;
```

### 消息操作

#### `write(&self, message: &SharedMessage) -> Result<()>`

写入一条消息到缓冲区。

**参数:**
- `message: &SharedMessage` - 要写入的消息

**返回:** `Result<()>` - 成功为Ok，失败返回Err

**错误:**
- 缓冲区满（无写入位置）
- 创建或写入过程中的同步错误

**性能:** O(1)，通常 < 1 μs（无竞争时）

**示例:**
```rust
let mut msg = SharedMessage::new();
msg.get_monitor_info_mut().set_client_name("my_app");
msg.get_monitor_info_mut().monitor_num = 1;
buffer.write(&msg)?;
```

#### `read(&self, timeout: Option<Duration>) -> Result<SharedMessage>`

读取一条消息。

**参数:**
- `timeout: Option<Duration>` - 最大等待时间
  - `None` - 如果无消息则立即返回Err
  - `Some(d)` - 等待最多d时间

**返回:** `Result<SharedMessage>` - 成功返回消息，失败返回Err

**错误:**
- 缓冲区空且无数据到达
- 校验和验证失败（数据损坏）
- 等待超时
- 后端同步错误

**性能:**
- 有消息：< 1 μs
- 无消息且设置超时：进入事件等待，延迟 1-10 ms

**示例:**
```rust
// 非阻塞读取
match buffer.read(None) {
    Ok(msg) => println!("Got: {:?}", msg),
    Err(_) => println!("No data available"),
}

// 阻塞读取，超时1秒
match buffer.read(Some(Duration::from_secs(1))) {
    Ok(msg) => println!("Got: {:?}", msg),
    Err(e) => println!("Timeout or error: {}", e),
}
```

#### `try_read(&self) -> Result<SharedMessage>`

非阻塞读取。等同于 `read(None)`。

**示例:**
```rust
if let Ok(msg) = buffer.try_read() {
    println!("Message: {:?}", msg);
}
```

#### `write_command(&self, command: &SharedCommand) -> Result<()>`

写入一条命令。

**参数:**
- `command: &SharedCommand` - 要写入的命令

**返回:** `Result<()>` - 成功为Ok

**错误:**
- 命令缓冲区满
- 同步错误

**性能:** O(1)，通常 < 1 μs

**示例:**
```rust
let cmd = SharedCommand::view_tag(1 << 2, 0);
buffer.write_command(&cmd)?;
```

#### `read_command(&self, timeout: Option<Duration>) -> Result<SharedCommand>`

读取一条命令。

**参数:**
- `timeout: Option<Duration>` - 最大等待时间

**返回:** `Result<SharedCommand>` - 成功返回命令

**示例:**
```rust
if let Ok(cmd) = buffer.read_command(Some(Duration::from_millis(100))) {
    println!("Command: {:?}", cmd.get_command_type());
}
```

#### `try_read_command(&self) -> Result<SharedCommand>`

非阻塞读取命令。等同于 `read_command(None)`。

### 管理方法

#### `destroy(&self) -> Result<()>`

显式销毁共享环形缓冲区。

**返回:** `Result<()>` - 成功为Ok

**说明:** 通常由Drop trait自动调用。仅当需要显式控制清理时调用。

**示例:**
```rust
buffer.destroy()?;
```

#### `is_valid(&self) -> bool`

检查缓冲区是否有效。

**返回:** `bool` - 有效则为true

---

## SharedMessage

消息结构，包含时间戳和监控信息。

### 构造方法

#### `new() -> Self`

创建一个新的默认消息。

**返回:** `SharedMessage` - 时间戳为当前时间，其他字段为默认值

**示例:**
```rust
let msg = SharedMessage::new();
```

#### `with_monitor_info(info: MonitorInfo) -> Self`

用指定的MonitorInfo创建消息。

**参数:**
- `info: MonitorInfo` - 监控信息

**返回:** `SharedMessage` - 包含指定信息的消息

**示例:**
```rust
let mut info = MonitorInfo::default();
info.set_client_name("app1");
let msg = SharedMessage::with_monitor_info(info);
```

### 访问方法

#### `get_timestamp(&self) -> u64`

获取消息时间戳（毫秒）。

**返回:** `u64` - Unix时间戳，单位毫秒

#### `update_timestamp(&mut self)`

更新消息时间戳为当前时间。

#### `get_monitor_info(&self) -> &MonitorInfo`

获取监控信息的不可变引用。

#### `get_monitor_info_mut(&mut self) -> &mut MonitorInfo`

获取监控信息的可变引用。

**示例:**
```rust
msg.get_monitor_info_mut().set_client_name("my_app");
msg.get_monitor_info_mut().monitor_num = 1;
```

---

## MonitorInfo

监控信息结构，包含监控器详情、标签状态和应用名称。

### 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `monitor_num` | `i32` | 监控器编号 |
| `monitor_width` | `i32` | 监控器宽度（像素） |
| `monitor_height` | `i32` | 监控器高度（像素） |
| `monitor_x` | `i32` | 监控器X坐标 |
| `monitor_y` | `i32` | 监控器Y坐标 |
| `tag_status_vec` | `[TagStatus; 9]` | 标签状态数组（最多9个标签） |
| `client_name` | `[u8; 128]` | 客户端名称（最多127字节UTF-8） |
| `ltsymbol` | `[u8; 32]` | LT符号字符串（最多31字节UTF-8） |

### 方法

#### `set_client_name(&mut self, name: &str)`

设置客户端名称。

**参数:**
- `name: &str` - 客户端名称，超过127字符会被截断

**示例:**
```rust
info.set_client_name("my_application");
```

#### `get_client_name(&self) -> String`

获取客户端名称。

**返回:** `String` - 存储的客户端名称

#### `set_ltsymbol(&mut self, symbol: &str)`

设置LT符号。

**参数:**
- `symbol: &str` - 符号字符串，超过31字符会被截断

#### `get_ltsymbol(&self) -> String`

获取LT符号。

#### `set_tag_status(&mut self, index: usize, status: TagStatus)`

设置指定位置的标签状态。

**参数:**
- `index: usize` - 标签索引（0-8）
- `status: TagStatus` - 标签状态

**示例:**
```rust
let status = TagStatus::new(true, false, true, false);
info.set_tag_status(0, status);
```

#### `get_tag_status(&self, index: usize) -> Option<TagStatus>`

获取指定位置的标签状态。

**参数:**
- `index: usize` - 标签索引

**返回:** `Option<TagStatus>` - Some(状态) 或 None（超出范围）

---

## SharedCommand

控制命令结构。

### 构造方法

#### `new(cmd_type: CommandType, parameter: u32, monitor_id: i32) -> Self`

创建指定类型的命令。

**参数:**
- `cmd_type: CommandType` - 命令类型
- `parameter: u32` - 命令参数
- `monitor_id: i32` - 目标监控器ID

**返回:** `SharedCommand` - 包含当前时间戳的命令

#### `view_tag(tag_bit: u32, monitor_id: i32) -> Self`

创建"查看标签"命令。

**参数:**
- `tag_bit: u32` - 标签位掩码（如 `1 << 2` 表示第2个标签）
- `monitor_id: i32` - 目标监控器ID

**示例:**
```rust
let cmd = SharedCommand::view_tag(1 << 2, 0);
```

#### `toggle_tag(tag_bit: u32, monitor_id: i32) -> Self`

创建"切换标签"命令。

#### `set_layout(layout_idx: u32, monitor_id: i32) -> Self`

创建"设置布局"命令。

**参数:**
- `layout_idx: u32` - 布局索引
- `monitor_id: i32` - 目标监控器ID

### 访问方法

#### `get_command_type(&self) -> CommandType`

获取命令类型。

#### `get_parameter(&self) -> u32`

获取命令参数。

#### `get_monitor_id(&self) -> i32`

获取目标监控器ID。

#### `get_timestamp(&self) -> u64`

获取命令时间戳（毫秒）。

---

## CommandType

命令类型枚举。

```rust
pub enum CommandType {
    None = 0,           // 无操作
    ViewTag = 1,        // 查看特定标签
    ToggleTag = 2,      // 切换标签状态
    SetLayout = 3,      // 设置窗口布局
}
```

### 转换

```rust
// 从u32转换
let cmd_type = CommandType::from(1u32);  // ViewTag

// 转为u32
let val = u32::from(CommandType::ViewTag);  // 1
```

---

## TagStatus

标签状态结构，使用四个布尔标志。

### 字段

| 字段 | 说明 |
|------|------|
| `is_selected` | 标签是否被选中 |
| `is_urg` | 是否为紧急标记 |
| `is_filled` | 是否已填充 |
| `is_occ` | 是否被占用 |

### 构造方法

#### `new(is_selected: bool, is_urg: bool, is_filled: bool, is_occ: bool) -> Self`

创建指定状态的TagStatus。

**示例:**
```rust
let status = TagStatus::new(true, false, true, false);
```

#### `default() -> Self`

创建默认值（所有标志为false）。

---

## SyncStrategy

同步策略枚举，用于选择进程间同步机制。

```rust
pub enum SyncStrategy {
    #[cfg(feature = "futex")]
    Futex,          // 基于futex系统调用
    #[cfg(feature = "semaphore")]
    Semaphore,      // 基于POSIX信号量
    #[cfg(feature = "eventfd")]
    EventFd,        // 基于Linux eventfd
}
```

### 选择指南

| 策略 | 延迟 | CPU开销 | 场景 |
|------|------|--------|------|
| **Futex** | 极低 | 中 | 高频消息（μs级），对延迟敏感 |
| **Semaphore** | 中 | 低 | 跨平台兼容，传统系统 |
| **EventFd** | 中 | 低 | 低频消息，可与epoll集成 |

### 方法

#### `backend_size(&self) -> usize`

返回此策略的后端头部大小。

#### `backend_align(&self) -> usize`

返回此策略的对齐要求。

---

## 常量

```rust
pub const MAX_CLIENT_NAME_LEN: usize = 128;  // 客户端名称最大长度
pub const MAX_LT_SYMBOL_LEN: usize = 32;    // LT符号最大长度
pub const MAX_TAGS: usize = 9;              // 最大标签数量
```

---

## 错误处理

所有I/O操作返回 `std::io::Result<T>`。

### 常见错误

```rust
use std::io::{Error, ErrorKind};

// 缓冲区满
ErrorKind::WouldBlock

// 无数据
ErrorKind::WouldBlock

// 校验和失败
ErrorKind::InvalidData

// 创建/打开失败
ErrorKind::NotFound
ErrorKind::PermissionDenied

// 同步错误
ErrorKind::TimedOut
ErrorKind::Interrupted
```

### 处理示例

```rust
match buffer.read(Some(Duration::from_secs(1))) {
    Ok(msg) => {
        println!("Received: {:?}", msg.get_timestamp());
    }
    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
        println!("Read timeout");
    }
    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
        println!("Data corruption detected");
    }
    Err(e) => {
        println!("Other error: {}", e);
    }
}
```

---

## 线程安全性

- `SharedRingBuffer` 实现 `Send + Sync`
- 设计为SPSC模型（单生产者-单消费者）
- 进程间安全，但单进程内需要外部同步

**在单个进程内的多线程使用:**
```rust
use std::sync::Arc;

let buffer = Arc::new(
    SharedRingBuffer::create("/tmp/ipc", SyncStrategy::Futex, None, None)?
);

let buffer_clone = buffer.clone();
std::thread::spawn(move || {
    // 在另一个线程读取
    if let Ok(msg) = buffer_clone.read(None) {
        println!("Got message");
    }
});
```

---

## 示例程序

### 完整的生产者-消费者示例

```rust
use shared_structures::{SharedRingBuffer, SharedMessage, SyncStrategy};
use std::time::Duration;

// 生产者
fn producer() -> std::io::Result<()> {
    let buffer = SharedRingBuffer::create_shared_ring_buffer(
        "/tmp/my_ipc",
        SyncStrategy::Futex
    ).ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::Other,
        "Failed to create buffer"
    ))?;

    for i in 0..100 {
        let mut msg = SharedMessage::new();
        msg.get_monitor_info_mut().monitor_num = i;
        msg.get_monitor_info_mut().set_client_name(&format!("app_{}", i));
        buffer.write(&msg)?;
        println!("Produced message {}", i);
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

// 消费者
fn consumer() -> std::io::Result<()> {
    let buffer = SharedRingBuffer::create_shared_ring_buffer(
        "/tmp/my_ipc",
        SyncStrategy::Futex
    ).ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::Other,
        "Failed to open buffer"
    ))?;

    for _ in 0..100 {
        match buffer.read(Some(Duration::from_secs(5))) {
            Ok(msg) => {
                let info = msg.get_monitor_info();
                println!(
                    "Consumed: num={}, name={}",
                    info.monitor_num,
                    info.get_client_name()
                );
            }
            Err(e) => println!("Read error: {}", e),
        }
    }
    Ok(())
}
```
