# Windows IOCP 驱动文档

本文档详细介绍了 `veloq-runtime` 中基于 Windows I/O Completion Ports (IOCP) 的异步驱动实现。

## 1. 概要 (Overview)

`veloq-runtime` 的 Windows 驱动层位于 `src/io/driver/iocp/` 目录下。它实现了 `Driver` trait，为上层运行时提供异步 I/O 支持。该驱动采用了 **Proactor** 模式，利用 Windows 内核提供的 IOCP 机制来处理网络和文件 I/O 事件。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 Proactor 模型
与 Linux 下常用的 Reactor 模型（如 epoll，尽管 io_uring 也是 Proactor）不同，IOCP 是原生的 Proactor 模型。应用程序“投递”一个 I/O 请求（如 `ReadFile`），内核在操作完成后通知应用程序。

在此驱动中，我们提交操作时会将 `OVERLAPPED` 结构体的指针传递给内核。为了安全地做到这一点，我们必须保证 `OVERLAPPED` 结构体在操作期间内存地址固定且有效。

### 2.2 内存稳定性与 StableSlab
由于异步操作通过指针引用 `OVERLAPPED`，我们使用了 `StableSlab` (`src/io/driver/stable_slab.rs`) 来存储飞行中（In-Flight）的操作。
- `StableSlab` 保证元素在插入后直到由于完成被移除前，其内存地址不会改变（即使 `Slab` 扩容）。
- 它通过分配一系列固定大小的页面（Page）来实现这一点，而不是像普通 `Vec` 那样重新分配。

### 2.3 类型擦除与 VTable (Type Erasure)
为了避免使用巨大的 `Enum` 来定义所有可能的 I/O 操作（这会导致 `Op` 结构体膨胀到最大变体的大小），本驱动使用了自定义的类型擦除技术 (`src/io/driver/iocp/op.rs`)。
- **Union Payload**: 所有具体的 Op 负载（如 `ReadFixed`, `Connect`）存储在一个 `union` 中。
- **VTable**: 每个 Op 类型提供一个 `OpVTable`（包含 `submit`, `on_complete`, `drop` 等函数指针）。
- 这使得 `IocpOp` 结构体保持紧凑，同时支持动态分发，且无需动态分配（Heap Allocation）每个 Op。

### 2.4 阻塞操作分流 (Blocking Offload)
Windows 的文件 I/O 即使使用 `OVERLAPPED`，在某些元数据操作（如 `Open`, `Close`）或由于缓存原因，仍可能在调用线程上同步阻塞。为了不阻塞运行时的 Reactor 线程，我们将这些操作分流到专用的线程池 (`src/io/driver/iocp/blocking.rs`)。
- 线程池执行完阻塞任务后，通过 `PostQueuedCompletionStatus` 向 IOCP 端口发送完成通知，使其看起来像一个普通的异步完成事件。

## 3. 模块内结构 (Internal Structure)

目录结构如下：

```
src/io/driver/
├── iocp.rs         // 驱动入口，IocpDriver 结构体定义，主循环逻辑
└── iocp/
    ├── op.rs       // IocpOp 定义，VTable 定义，Union Payload 宏
    ├── submit.rs   // 各个 Op 的具体提交逻辑 (submit_*) 和辅助函数
    ├── blocking.rs // 阻塞任务线程池 (ThreadPool) 用于文件 Open/Close 等
    ├── ext.rs      // Winsock 扩展函数加载 (ConnectEx, AcceptEx)
    └── tests.rs    // 测试用例
```

外部辅助模块：
- `../op_registry.rs`: 管理操作生命周期，将 `user_data` 映射到 `IocpOp`。
- `../stable_slab.rs`: 提供稳定地址的内存分配。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 IocpDriver (`iocp.rs`)
核心结构体 `IocpDriver` 拥有：
- `port`: IOCP 句柄。
- `ops`: `OpRegistry`，存储所有未完成的操作。
- `pool`: `ThreadPool`，用于执行阻塞任务。
- `wheel`: 时间轮，用于管理超时。

**主循环 (`get_completion`)**:
1. 计算下一个定时器的超时时间 `wait_ms`。
2. 调用 `GetQueuedCompletionStatus` 阻塞等待 I/O 完成或超时。
3. 处理超时事件：从时间轮中取出过期的 `user_data` 并唤醒对应任务。
4. 处理 I/O 完成事件：
   - 提取 `user_data`。
   - 在 `ops` 中找到对应的操作。
   - 如果是普通 I/O，调用 `vtable.on_complete`（如果存在），设置 `op.result`。
   - 如果是阻塞任务，从 `blocking_result` 获取结果。
   - 唤醒 `Waker`。

### 4.2 操作定义 (`op.rs`)
`IocpOp` 结构体设计精巧：
```rust
#[repr(C)]
pub struct IocpOp {
    pub header: OverlappedEntry, // 包含 OVERLAPPED，其地址传递给内核
    pub vtable: &'static OpVTable,
    pub payload: IocpOpPayload, // Union
}
```
宏 `define_iocp_ops!` 自动生成了 `IntoPlatformOp` 实现，将高层的 `Op` 结构体（如 `ReadFixed`）转换为底层的 `IocpOp`，并设置正确的 VTable。

### 4.3 提交逻辑 (`submit.rs`)
该模块包含一系列静态函数，如 `submit_read_fixed`。
- 它们接收 `&mut IocpOp`。
- 通过 `resolve_fd` 将逻辑句柄转换为 `HANDLE`。
- 调用 Windows API（如 `ReadFile`, `WSASendTo`）。
- 如果 API 返回 `ERROR_IO_PENDING`，返回 `SubmissionResult::Pending`。
- 对于 `Open`/`Close` 等，返回 `SubmissionResult::Offload(task)`，指示驱动将其放入线程池。

### 4.4 阻塞线程池 (`blocking.rs`)
实现了一个基于条件变量和 `SegQueue` 的动态线程池。
- **任务模型**: 定义了 `BlockingTask` 枚举 (`Open`, `Close`, `Fsync`, `Fallocate`)。
- **完成通知**: 任务完成后，调用 `BlockingTask::complete`，它会更新 `IocpOp` 中的 `blocking_result`，并通过 `PostQueuedCompletionStatus` 唤醒 IOCP 端口。这保证了所有完成事件（无论是真异步还是线程池模拟）都在同一个入口 (`get_completion`) 被处理。

### 4.5 扩展函数 (`ext.rs`)
Windows 的高性能网络 API（如 `ConnectEx` 和 `AcceptEx`）不是直接导出的，需要通过 `WSAIoctl` 在运行时获取函数指针。`Extensions` 结构体负责加载和持有这些指针。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **SyncFileRange 支持**:
    - Windows 没有对应 Linux `sync_file_range` 的细粒度控制。当前实现 (`submit_sync_range` -> `FlushFileBuffers`) 等同于全文件 `fsync`，性能开销较大。
    - **TODO**: 调研是否有更优替代方案，或者文档明确此行为差异。

2.  **线程池策略**:
    - 当前 `ThreadPool` 使用简单的动态伸缩策略。对于高并发且大量阻塞文件操作的场景，可能需要更复杂的调度或限制策略以防止线程爆炸。

3.  **缓冲区管理**:
    - 目前 `send` / `recv` 使用了 `FixedBuf`，但并未完全利用 Windows 的注册 I/O (Registered I/O, RIO) 特性。RIO 可以进一步减少内核开销。

4.  **AcceptEx 地址解析**:
    - `on_complete_accept` 中使用了 `GetAcceptExSockaddrs` 解析地址。这部分代码涉及复杂的指针操作，需要持续关注安全性。

## 6. 未来的方向 (Future Directions)

1.  **Registered I/O (RIO)**:
    - 探索集成 Windows RIO API，以显著降低高吞吐网络应用的 CPU 使用率。这将需要重构 buffer 管理部分以适应 RIO 的 Buffer Region 机制。

2.  **完成通知优化**:
    - 目前使用 `GetQueuedCompletionStatus` 一次取一个事件。可以使用 `GetQueuedCompletionStatusEx` 一次批量获取多个完成事件，减少系统调用次数（类似于 io_uring 的批量周转）。

3.  **取消机制 (Cancellation)**:
    - 目前的 `cancel_op` 尝试使用 `CancelIoEx`。需要确保在所有边缘情况下（如驱动正在析构、操作已经部分完成）该机制都能安全工作。

4.  **错误处理细化**:
    - 将 Windows特定的错误码更友好的映射到 `std::io::Error`，特别是网络相关的错误。
