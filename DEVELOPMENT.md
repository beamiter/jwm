# 开发指南

## 环境设置

### 系统要求

```bash
# Ubuntu/Debian
sudo apt-get install build-essential pkg-config libssl-dev

# Fedora/RHEL
sudo dnf install gcc pkg-config openssl-devel

# macOS（虽然futex/eventfd不支持，但可以用于编译/测试）
brew install pkg-config
```

### Rust设置

```bash
# 安装Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 更新工具链
rustup update

# 安装额外工具
cargo install cargo-criterion
cargo install cargo-expand
cargo install cargo-udeps
```

## 项目结构导航

```
shared_structures/
├── src/
│   ├── lib.rs                           # 库入口，导出公共API
│   ├── shared_message.rs                # 消息/命令/标签定义
│   │   ├── SharedMessage struct         # 包含监控信息和时间戳
│   │   ├── MonitorInfo struct           # 监控器详情
│   │   ├── SharedCommand struct         # 命令消息
│   │   ├── CommandType enum             # 命令类型
│   │   └── TagStatus struct             # 标签状态
│   │
│   ├── shared_ring_buffer.rs            # 核心SPSC环形缓冲区
│   │   ├── SharedRingBuffer struct
│   │   ├── MessageSlot (内部)           # 消息插槽
│   │   ├── create()                     # 创建新缓冲区
│   │   ├── open()                       # 打开已有缓冲区
│   │   ├── write()/read()               # 消息读写
│   │   ├── write_command()/read_command() # 命令读写
│   │   └── destroy()                    # 清理资源
│   │
│   └── backends/
│       ├── mod.rs                       # 模块声明
│       ├── common.rs                    # 公共接口
│       │   ├── SyncStrategy enum        # 运行时策略选择
│       │   ├── SyncBackend trait        # 后端接口
│       │   ├── GenericHeader struct     # 共享内存头部
│       │   └── AnySyncBackend enum      # 后端枚举包装
│       │
│       ├── futex.rs                     # Futex实现
│       │   ├── FutexBackend struct
│       │   ├── FutexHeader struct
│       │   └── futex系统调用封装
│       │
│       ├── semaphore.rs                 # Semaphore实现
│       │   ├── SemaphoreBackend struct
│       │   ├── SemaphoreHeader struct
│       │   └── POSIX信号量封装
│       │
│       └── eventfd.rs                   # EventFd实现
│           ├── EventFdBackend struct
│           ├── EventFdHeader struct
│           └── eventfd系统调用封装
│
├── benches/
│   ├── ring_buffer_bench.rs             # Criterion基准测试
│   └── stress_test.rs                   # 压力测试（10M消息）
│
├── Cargo.toml                           # 项目配置
├── README.md                            # 项目概览（新增）
├── ARCHITECTURE.md                      # 架构设计（新增）
├── API.md                               # API文档（新增）
├── IMPLEMENTATION_DETAILS.md            # 实现细节（新增）
├── PERFORMANCE.md                       # 性能指南（新增）
└── DEVELOPMENT.md                       # 开发指南（本文件）
```

## 开发工作流

### 1. 代码修改流程

#### 创建特性分支

```bash
# 从master创建特性分支
git checkout -b feat/add-new-feature

# 或bug修复
git checkout -b fix/memory-leak-issue
```

#### 本地开发

```bash
# 构建项目
cargo build --all-features

# 运行测试
cargo test --all-features

# 检查代码规范
cargo clippy --all-features -- -D warnings

# 格式化代码
cargo fmt --check
cargo fmt  # 自动格式化

# 运行基准测试
cargo bench --all-features
```

#### 提交更改

```bash
# 检查修改
git status
git diff

# 暂存修改
git add src/...

# 提交（注意提交信息规范）
git commit -m "feat: add support for batch operations

- Add batch_write method for writing multiple messages
- Optimize memory layout for batch scenarios
- Add tests for batch operations"
```

#### 创建Pull Request

```bash
# 推送分支
git push origin feat/add-new-feature

# 在GitHub创建PR（使用web界面或gh cli）
gh pr create --title "Add batch operation support" \
  --body "This PR adds support for batch writing messages to improve throughput"
```

### 2. 测试策略

#### 单元测试

在源文件末尾添加测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_feature() {
        // 测试实现
        assert_eq!(result, expected);
    }

    #[test]
    #[should_panic]
    fn test_error_case() {
        // 测试错误情况
        panic!("Expected error");
    }
}
```

运行测试：

```bash
# 运行所有测试
cargo test --all-features

# 运行特定测试
cargo test test_new_feature

# 运行测试并显示输出
cargo test -- --nocapture

# 多线程测试（检测竞态）
cargo test -- --test-threads=1
```

#### 集成测试

在顶级tests/目录中创建集成测试（如果需要）：

```rust
// tests/integration_test.rs
use shared_structures::*;

#[test]
fn test_producer_consumer() {
    // 集成测试：完整的生产者-消费者流程
}
```

#### 基准测试

使用Criterion进行基准测试：

```rust
// benches/my_benchmark.rs
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn my_benchmark(c: &mut Criterion) {
    c.bench_function("my_operation", |b| {
        b.iter(|| {
            // 需要测试的操作
        })
    });
}

criterion_group!(benches, my_benchmark);
criterion_main!(benches);
```

运行基准：

```bash
cargo bench --bench ring_buffer_bench --release

# 生成对比报告
cargo bench --bench ring_buffer_bench --release -- --save-baseline my_baseline
cargo bench --bench ring_buffer_bench --release -- --baseline my_baseline
```

### 3. 代码规范

#### 命名约定

```rust
// 常量：全大写加下划线
const MAX_BUFFER_SIZE: usize = 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5000;

// 函数名：小驼峰
pub fn create_buffer() {}
fn calculate_checksum() {}

// 结构体：帕斯卡命名
pub struct SharedRingBuffer {}
struct MessageSlot {}

// 枚举变体：帕斯卡命名
pub enum SyncStrategy {
    Futex,
    Semaphore,
}

// 模块：小写加下划线
mod shared_message;
mod backends;

// 私有成员：下划线前缀（可选）
struct Header {
    _padding: [u8; 32],
}
```

#### 文档注释

```rust
/// 创建一个新的共享环形缓冲区
///
/// # Arguments
///
/// * `path` - 共享内存文件路径
/// * `strategy` - 同步策略
///
/// # Returns
///
/// 成功返回 `Ok(SharedRingBuffer)`，失败返回 `Err`
///
/// # Examples
///
/// ```
/// use shared_structures::{SharedRingBuffer, SyncStrategy};
///
/// let buffer = SharedRingBuffer::create(
///     "/tmp/my_buffer",
///     SyncStrategy::Futex,
///     None,
///     None
/// )?;
/// ```
pub fn create(path: &str, strategy: SyncStrategy, ...) -> Result<Self> {
    // 实现
}
```

#### 注释指南

- **何时写注释**: 非显而易见的逻辑、算法选择、绕过的bug
- **避免注释**: 显而易见的代码、已通过提交信息记录的内容

```rust
// 好：解释为什么
// 使用Acquire而非SeqCst，因为只需要单向同步屏障
let idx = header.write_idx.load(Ordering::Acquire);

// 不好：解释了什么，而不是为什么
// 从header加载write_idx
let idx = header.write_idx.load(Ordering::Acquire);
```

### 4. 添加新功能

#### 添加新的同步后端

1. **创建后端文件**:

```bash
# 创建backends/custom.rs
cat > src/backends/custom.rs << 'EOF'
use crate::backends::common::SyncBackend;
use std::io::Result;

#[repr(C)]
pub struct CustomHeader {
    // 后端特定的状态
}

pub struct CustomBackend {
    // 内部实现
}

impl SyncBackend for CustomBackend {
    // 实现trait方法
}
EOF
```

2. **在backends/mod.rs中声明**:

```rust
#[cfg(feature = "custom")]
pub mod custom;
```

3. **在backends/common.rs中添加到枚举**:

```rust
#[cfg(feature = "custom")]
use crate::backends::custom::CustomBackend;

pub enum AnySyncBackend {
    #[cfg(feature = "futex")]
    Futex(FutexBackend),
    #[cfg(feature = "custom")]
    Custom(CustomBackend),  // 新增
}

impl SyncBackend for AnySyncBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        match self {
            #[cfg(feature = "custom")]
            Self::Custom(b) => b.init(is_creator, backend_ptr),
            // ...其他分支
        }
    }
    // 实现其他方法
}
```

4. **在Cargo.toml中添加特性**:

```toml
[features]
custom = []
default = ["futex", "semaphore", "eventfd"]
```

5. **添加测试**:

```rust
#[test]
fn test_custom_backend() {
    let buffer = SharedRingBuffer::create(
        "/tmp/test",
        SyncStrategy::Custom,
        None,
        None
    ).expect("Failed to create buffer");
    
    // 测试实现
}
```

#### 修改消息格式

1. **更新结构**:

```rust
// 在shared_message.rs中
#[repr(C)]
pub struct MonitorInfo {
    // 现有字段...
    pub new_field: u32,  // 新增字段
}
```

2. **更新版本号**:

```rust
// 在shared_ring_buffer.rs中
const RING_BUFFER_VERSION: u64 = 9;  // 从8升级到9
```

3. **更新校验和**:

```rust
fn calculate_message_checksum(m: &SharedMessage) -> u32 {
    // ...现有代码...
    mix_u32(&mut sum, mi.new_field);  // 包含新字段
}
```

4. **更新测试和文档**:

```rust
#[test]
fn test_new_field_handling() {
    let mut info = MonitorInfo::default();
    info.new_field = 42;
    // 验证新字段
}
```

### 5. 调试技巧

#### 启用调试日志

```bash
# 设置日志级别
RUST_LOG=shared_structures=debug cargo run

# 更详细的日志
RUST_LOG=shared_structures=trace cargo run

# 特定模块
RUST_LOG=shared_structures::backends::futex=debug cargo run
```

#### 使用rust-gdb调试

```bash
# 启用调试符号
cargo build --debug

# 启动调试器
rust-gdb ./target/debug/my_app

# gdb命令
(gdb) break shared_ring_buffer.rs:100
(gdb) run
(gdb) print buffer.write_idx
(gdb) continue
```

#### 内存检查（Valgrind）

```bash
# 检查内存泄漏
valgrind --leak-check=full ./target/debug/my_app

# 更详细的检查
valgrind --leak-check=full --show-leak-kinds=all ./target/debug/my_app
```

#### 使用lldb（macOS）

```bash
lldb ./target/debug/my_app
(lldb) breakpoint set --file shared_ring_buffer.rs --line 100
(lldb) run
```

## 性能分析

### 使用perf进行分析

```bash
# 记录CPU采样
perf record -g ./target/release/my_app

# 生成报告
perf report

# 火焰图
perf record -g ./target/release/my_app
perf script | flamegraph.pl > perf.svg
```

### 使用Criterion分析基准

```bash
# 生成详细的HTML报告
cargo bench --bench ring_buffer_bench --release

# 报告位置：target/criterion/
```

### 缓存分析

```bash
# 获取缓存命中率
perf stat -e cache-references,cache-misses ./target/release/my_app
```

## 常见开发任务

### 运行所有检查

```bash
#!/bin/bash
set -e

echo "Formatting..."
cargo fmt --check

echo "Linting..."
cargo clippy --all-features -- -D warnings

echo "Testing..."
cargo test --all-features

echo "Benchmarking..."
cargo bench --all-features

echo "All checks passed!"
```

### 创建发行版本

```bash
# 更新版本号
vim Cargo.toml  # 修改version字段

# 创建git标签
git tag -a v0.2.0 -m "Release version 0.2.0"

# 推送标签
git push origin v0.2.0

# 发布到crates.io（如适用）
cargo publish
```

### 生成文档

```bash
# 构建文档
cargo doc --all-features --open

# 文档位置：target/doc/shared_structures/index.html
```

## 依赖管理

### 更新依赖

```bash
# 检查过时的依赖
cargo outdated

# 更新依赖
cargo update

# 只更新特定依赖
cargo update -p serde
```

### 审计依赖安全性

```bash
# 安装cargo-audit
cargo install cargo-audit

# 检查已知漏洞
cargo audit
```

## 持续集成

### CI配置示例（GitHub Actions）

创建 `.github/workflows/ci.yml`:

```yaml
name: CI

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      - uses: actions-rs/cargo@v1
        with:
          command: test
          args: --all-features

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      - uses: actions-rs/cargo@v1
        with:
          command: clippy
          args: -- -D warnings

  fmt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      - uses: actions-rs/cargo@v1
        with:
          command: fmt
          args: -- --check
```

## 故障排除

### 常见编译错误

**错误**: `error: feature "futex" not found`

```
解决: 确保在Cargo.toml中启用了对应的feature
[features]
default = ["futex", "semaphore", "eventfd"]
```

**错误**: `error: linking with cc failed`

```
解决: 安装build-essential
sudo apt-get install build-essential
```

### 常见运行时错误

**错误**: `Invalid magic` - 共享内存格式错误

```rust
解决: 清理旧的共享内存文件
rm /dev/shm/my_buffer
// 重新运行应用
```

**错误**: `Permission denied` - 权限不足

```bash
解决: 检查共享内存文件权限
ls -la /dev/shm/my_buffer
chmod 666 /dev/shm/my_buffer
```

## 获取帮助

### 相关资源

- [Rust官方文档](https://doc.rust-lang.org/)
- [Cargo文档](https://doc.rust-lang.org/cargo/)
- [Linux futex](https://man7.org/linux/man-pages/man2/futex.2.html)
- [项目README](./README.md)
- [架构设计](./ARCHITECTURE.md)

### 贡献流程

1. Fork本仓库
2. 创建特性分支 (`git checkout -b feat/AmazingFeature`)
3. 提交更改 (`git commit -m 'Add AmazingFeature'`)
4. 推送到分支 (`git push origin feat/AmazingFeature`)
5. 创建Pull Request

### 报告问题

提交Issue时请包含：

- 清晰的问题描述
- 最小化的复现代码
- 系统环境信息 (`rustc --version`, `uname -a`)
- 错误日志（如适用）
- 已尝试的解决方案

### 提议改进

我们欢迎性能改进、API优化、新后端等建议。请提交Issue讨论大型变更。

## 许可证

项目遵循指定的开源许可证。贡献代码即表示同意该许可证条款。
