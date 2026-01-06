# IO 模块文档

本文档详细介绍了 `veloq-runtime` 的核心 I/O 模块 (`src/io`)。作为运行时的基石，该模块提供了一套基于 **Proactor** 模式的高性能异步 I/O 抽象。

## 1. 概要 (Overview)

`src/io` 模块不仅仅是文件和网络操作的集合，它定义了 Veloq 运行时的 I/O 交互模型。与 Rust 标准库或 Tokio 中常见的 `poll_read` / `poll_write` (Reactor 模型) 不同，Veloq 采用 **所有权传递 (Ownership Passing)** 的方式进行 I/O。

核心差异在于：
*   **传统模型**: 借用缓冲区 (`&mut [u8]`)。这在 io_uring 等异步接口上很难实现零拷贝，因为内核可能会在 Future 被 drop 后继续写入该缓冲区。
*   **Veloq 模型**: 传递缓冲区所有权 (`FixedBuf`)。用户将缓冲区交给运行时，I/O 完成后运行时归还缓冲区和结果。

该模块导出了所有必要的组件来构建上层网络协议和文件系统操作。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 基于所有权的异步 I/O Trait
在 `src/io.rs` 中定义了两个核心 Trait：`AsyncBufRead` 和 `AsyncBufWrite`。
```rust
pub trait AsyncBufRead {
    fn read(&self, buf: FixedBuf) -> impl Future<Output = (io::Result<usize>, FixedBuf)>;
}
```
这种设计强制要求调用者在发起 I/O 时放弃对缓冲区的控制权。这完美契合 `io_uring` 和 `IOCP` 的工作方式——一旦操作提交，内存区域必须保持稳定且由内核独占，直到完成信号到达。

### 2.2 模块化分层
`io` 模块采用严格的分层架构：
*   **顶层 (`io.rs`)**: 定义通用的行为 (Traits) 和入口。
*   **操作层 (`op.rs`)**: 定义具体的 I/O 意图（如“读取文件”、“连接 Socket”），这些定义是平台无关的。
*   **资源层 (`socket.rs`, `buffer.rs`)**: 管理 I/O 所需的资源（句柄、内存）。
*   **驱动层 (`driver.rs`)**: 处理与操作系统的脏活累活，实现 `PlatformOp` 到系统调用的映射。

## 3. 模块内结构 (Internal Structure)

```
src/io.rs              // 核心入口，定义 AsyncBufRead/AsyncBufWrite Traits
src/io/
├── buffer.rs          // 内存管理 (FixedBuf, BufPool)
├── driver.rs          // 驱动抽象 (Driver Trait, PlatformDriver)
├── op.rs              // 操作定义 (Op Future, Read, Write...)
└── socket.rs          // Socket/Handle 抽象
```

*   **`buffer`**: 提供 `FixedBuf`，这是所有 I/O 操作的数据载体。详见 `docs/src/io/buffer/README.md`。
*   **`driver`**: 定义 `Driver` trait，这是 Reactor/Proactor 的核心接口。详见 `docs/src/io/driver/README.md`。
*   **`op`**: 将 `Driver` 的底层能力封装为用户友好的 `Future`。详见 `docs/src/io/socket/README.md`（注：op 文档归档在 socket 下）。
*   **`socket`**: 提供跨平台的句柄封装。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 `AsyncBufRead` / `AsyncBufWrite`
这两个 Trait 是 Veloq I/O 生态的一等公民。
*   **设计权衡**: 相比于 `AsyncRead` (`poll_read`)，`AsyncBufRead` 对编译器更友好（无需处理复杂的生命周期），但对用户代码有侵入性（需要管理 Buffer 池）。
*   **返回值**: `(Result<usize>, FixedBuf)`。即使 I/O 失败，缓冲区也必须归还给用户，以便重用或释放。如果设计成只返回 `Result`，那么在错误发生时缓冲区就会泄漏（被 drop 掉），这是不可接受的。

### 4.2 模块重导出
`io.rs` 充当了 Facade（门面）：
```rust
pub mod buffer;
pub mod driver;
pub mod op;
pub(crate) mod socket; // socket 内部实现细节较多，对外主要暴露类型
```
这种结构隐藏了内部实现的复杂性，用户在使用时通常只需要引入 `veoq_runtime::io` 即可访问大部分功能。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **生态兼容性**:
    *   现有的 Rust 异步生态（Tokio, Hyper 等）严重依赖 `AsyncRead`/`AsyncWrite`。Veloq 的 `AsyncBufRead` 无法直接与它们互通。
    *   **TODO**: 提供适配层 (`Compat`)，使用内部缓冲区来模拟 `AsyncRead`，但这会引入一次内存拷贝，牺牲部分性能以换取兼容性。

2.  **Trait 方法的泛化**:
    *   目前 `read` 接受 `FixedBuf` 具体类型。未来可能通过泛型支持任何实现了 `AsIoSlice` 的类型，但这会增加 VTable 调度的复杂性。

3.  **Vectored I/O**:
    *   目前的 Trait 仅支持单个 buffer (`read`, `write`)。对于 `readv`/`writev` (Scatter/Gather) 的支持尚未在顶层 Trait 中体现。
    *   **TODO**: 添加 `read_vectored` 和 `write_vectored` 支持。

## 6. 未来的方向 (Future Directions)

1.  **统一流抽象 (Unified Stream Abstraction)**:
    *   构建基于 `AsyncBufRead` 的 `BufReader` 和 `BufWriter` 等高级工具，提供行读取、按分隔符读取等功能。

2.  **Pipeline I/O**:
    *   探索类似 Linux `splice` 或 `sendfile` 的高级 Trait，允许数据在两个文件描述符之间直接传输，完全绕过用户态缓冲区。

3.  **Completion-based Traits**:
    *   考虑引入 `AsyncReadCom` 等 Trait，直接返回 `impl Future`，进一步利用 Rust 2024 的 `async fn` in trait 特性（目前 Veloq 手写了 Future 返回值以保证向后兼容或特殊控制）。
