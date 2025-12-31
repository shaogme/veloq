# io-uring Crate 开发文档

`io-uring` 是一个 Rust 的 `io_uring` 接口库，提供了对 Linux `io_uring` 异步 I/O 接口的低层封装。本文档详细介绍了其公共 API 和主要模块。

## 目录

- [核心结构 (Core Structures)](#核心结构-core-structures)
- [提交队列 (Submission Queue)](#提交队列-submission-queue)
- [完成队列 (Completion Queue)](#完成队列-completion-queue)
- [操作码 (Opcodes)](#操作码-opcodes)
- [类型定义 (Types)](#类型定义-types)
- [注册 (Register)](#注册-register)

## 核心结构 (Core Structures)

### `IoUring`

`IoUring` 是库的主要入口点，代表了一个 `io_uring` 实例。它是泛型结构体 `IoUring<S, C>`，其中 `S` 和 `C` 分别代表提交队列条目（SQE）和完成队列条目（CQE）的类型（默认为 `squeue::Entry` 和 `cqueue::Entry`）。

*   **创建实例**:
    *   `IoUring::new(entries: u32) -> io::Result<Self>`: 创建具有默认配置的实例。`entries` 指定队列大小（必须是 2 的幂）。
    *   `IoUring::builder() -> Builder`: 获取 `Builder` 以自定义配置。

*   **提交**:
    *   `submitter() -> Submitter`: 获取提交器（`Submitter`），用于提交 SQE 和注册资源。
    *   `submit() -> io::Result<usize>`: 提交提交队列中的所有条目。
    *   `submit_and_wait(want: usize) -> io::Result<usize>`: 提交并等待至少 `want` 个完成事件。

*   **队列访问**:
    *   `submission() -> SubmissionQueue`: 获取提交队列的借用。
    *   `completion() -> CompletionQueue`: 获取完成队列的借用。
    *   `split()`: 将 `IoUring` 分解为 `Submitter`、`SubmissionQueue` 和 `CompletionQueue`，以便并发使用。

### `Builder`

用于配置和构建 `IoUring` 实例。

*   `dontfork()`: 设置在 fork 后不共享 ring（`MADV_DONTFORK`）。
*   `setup_defer_taskrun()`: 延迟任务运行，提示内核推迟工作直到调用 `io_uring_enter` 获取事件 (Kernel 6.1+)。
*   `setup_coop_taskrun()`: 协作式任务运行，减少 IPI 中断 (Kernel 5.19+)。
*   `setup_single_issuer()`: 单一提交者模式 Hint (Kernel 6.0+)。
*   `setup_sqpoll(idle: u32)`: 启用内核轮询线程 (SQPOLL)，减少系统调用开销。
*   `setup_iopoll()`: 启用 I/O 轮询 (IOPOLL)，适用于支持轮询的文件系统和 O_DIRECT。
*   `setup_clamp()`: 将队列大小限制在最大值，而不是报错。
*   `build(entries: u32) -> io::Result<IoUring>`: 构建实例。

## 提交队列 (Submission Queue)

位于 `io_uring::squeue` 模块。

### `SubmissionQueue`

*   `push(entry: &Entry) -> Result<(), PushError>`: 将条目推送到队列。如果队列已满则返回错误。
*   `sync()`: 同步队列状态，刷新添加的条目使内核可见，并更新头部位置。
*   `capacity()`, `len()`, `is_full()`, `is_empty()`: 队列状态查询。
*   `dropped()`: 获取因队列满而被丢弃的条目数（如果有）。

### `Entry`

代表一个提交队列条目 (SQE)。通常通过 `opcode` 模块中的构建器创建，但也可以修改通用属性。

*   **Modifier 方法**:
    *   `user_data(data: u64)`: 设置用户数据，会在 CQE 中原样返回。
    *   `flags(flags: Flags)`: 设置标志（如 `IO_LINK`, `ASYNC` 等）。
    *   `personality(p: u16)`: 设置个性化凭证 (Personality)。
    *   `clear_flags()`: 清除标志。

### `Flags` (在 `squeue::Flags` 中定义)

*   `IO_LINK`: 链接下一个 SQE，当前一个完成后才执行下一个。
*   `IO_HARDLINK`: 类似于 `IO_LINK`，但即使前一个失败也不断开链接。
*   `IO_DRAIN`: 之前的所有请求完成后才执行此请求。
*   `ASYNC`: 强制异步执行。
*   `FIXED_FILE`: 使用注册的文件描述符索引。
*   `BUFFER_SELECT`: 自动缓冲区选择 (Buffer Selection)。
*   `SKIP_SUCCESS`: 成功时不生成 CQE (Kernel 5.17+)。

## 完成队列 (Completion Queue)

位于 `io_uring::cqueue` 模块。

### `CompletionQueue`

实现了 `Iterator` trait，可以遍历获取 `Entry`。

*   `next() -> Option<Entry>`: 获取下一个完成条目。
*   `sync()`: 同步队列状态，将消费的头部位置告知内核。
*   `overflow() -> u32`: 获取因 CQ 环满而产生的溢出事件数量。
*   `is_full()`, `is_empty()`, `capacity()`: 队列状态查询。

### `Entry` (CQE)

代表一个完成队列条目。

*   `result() -> i32`: 操作结果。非负值为成功（如读取的字节数），负值为错误码（如 `-libc::EAGAIN`）。
*   `user_data() -> u64`: 对应的 SQE 设置的用户数据。
*   `flags() -> u32`: 完成标志。

*   **辅助函数**:
    *   `buffer_select(flags) -> Option<u16>`: 获取选中的缓冲区 ID（配合 `BUFFER_SELECT` 使用）。
    *   `more(flags) -> bool`: 是否有多射 (multishot) 的更多事件（`IORING_CQE_F_MORE`）。
    *   `sock_nonempty(flags) -> bool`: Socket 是否还有数据可读。
    *   `notif(flags) -> bool`: 是否为通知事件（Zero Copy send）。

## 操作码 (Opcodes)

位于 `io_uring::opcode` 模块。遵循 Builder 模式：`opcode::Name::new(...).field(...).build()`。

常用操作包括：

*   **No-op**:
    *   `Nop`: 空操作，仅用于测试或占位。

*   **读写操作**:
    *   `Readv`, `Writev`: 向量化读写 (类似 `preadv2`, `pwritev2`)。
    *   `ReadFixed`, `WriteFixed`: 使用注册缓冲区的读写。
    *   `Fsync`: 文件同步。
    *   `SyncFileRange`: 同步文件范围。

*   **文件管理**:
    *   `OpenAt`: 打开文件（`openat2` 语义）。
    *   `Fallocate`: 文件空间预分配。
    *   `Statx`: 获取文件状态。

*   **网络操作**:
    *   `Connect`: 连接 Socket。
    *   `Accept`: 接受连接。
    *   `SendMsg`, `RecvMsg`: 发送/接收消息。
    *   `RecvMsgMulti`: 多射接收 (Multishot Recv)。
    *   `Socket`: 创建 Socket (Kernel 5.19+)。

*   **其他**:
    *   `PollAdd`: 添加文件描述符轮询。
    *   `PollRemove`: 移除轮询。
    *   `Timeout`: 注册超时。
    *   `TimeoutRemove`: 移除超时。
    *   `LinkTimeout`: 链接超时。
    *   `AsyncCancel`: 异步取消请求。
    *   `ProvideBuffers`: 提供缓冲区组。
    *   `RemoveBuffers`: 移除缓冲区组。

**示例**:
```rust
use io_uring::{opcode, types};

// 构建一个 Readv 操作
let entry = opcode::Readv::new(
    types::Fd(fd), 
    iovecs.as_ptr(), 
    iovecs.len() as _
)
.offset(0)
.build()
.user_data(0x1234);
```

## 类型定义 (Types)

位于 `io_uring::types` 模块，提供了一些类型安全的封装。

*   `Fd(pub RawFd)`: 包装标准文件描述符。
*   `Fixed(pub u32)`: 包装已注册的文件描述符索引。
*   `Timespec`: 对应 `__kernel_timespec`，用于超时设置。
*   `SubmitArgs`: `submit_with_args` 使用的参数，包含信号掩码等。
*   `AsyncCancelFlags`: 异步取消的标志 (ALL, FD, ANY)。
*   `CancelBuilder`: 用于构建取消请求的辅助器，支持按 `user_data` 或 `fd` 取消。
*   `TimeoutFlags`: 超时标志 (ABS, BOOTTIME, REALTIME)。

## 注册 (Register)

位于 `io_uring::register` 模块。`Submitter` 提供了相关方法。

*   `Probe`: 用于探测当前内核支持的操作码。
    ```rust
    let mut probe = Probe::new();
    submitter.register_probe(&mut probe)?;
    if probe.is_supported(opcode::Read::CODE) { ... }
    ```
*   `Restriction`: 用于设置 ring 的限制（白名单操作码等），增强安全性。
*   `register_files`, `unregister_files`: 注册/注销文件描述符表，以使用 `Fixed` 描述符。
*   `register_buffers`: 注册固定缓冲区，减少内存映射开销。
