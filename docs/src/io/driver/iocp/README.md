# Windows IOCP 驱动文档

本文档详细介绍了 `veloq-runtime` 中基于 Windows I/O Completion Ports (IOCP) 的异步驱动实现。

## 1. 概要 (Overview)

`veloq-runtime` 的 Windows 驱动层实现了 `Driver` trait，为上层运行时提供异步 I/O 支持。该驱动采用了 **Proactor** 模式，利用 Windows 内核提供的 IOCP 机制来处理网络和文件 I/O 事件。

一个显著的特点是它实现了 **混合 I/O 模型**：对于标准句柄使用传统的 IOCP 模型，而对于支持 Registered I/O (RIO) 的网络操作，则自动尝试升级到更高效的 RIO 路径，以降低内核开销。

## 2. 理念和思路 (Concepts)

### 2.1 Proactor 模型
IOCP 是原生的 Proactor 模型。应用程序“投递”一个 I/O 请求（如 `ReadFile` 或 `RIOReceive`），内核在操作完成后通知应用程序。在此期间，驱动必须保证传递给内核的数据结构（如 `OVERLAPPED` 或 `RIO_BUF`）地址固定且有效。

### 2.2 内存稳定性与 StableSlab
由于异步操作通过指针引用 `OVERLAPPED`，我们使用了 `StableSlab` 来存储飞行中（In-Flight）的操作。
- `StableSlab` 保证元素在插入后直到被移除前，其内存地址不会改变。
- 这允许我们将 `IocpOp` 的指针直接转换为 `OVERLAPPED` 指针传递给系统 API。

### 2.3 类型擦除与 VTable (Type Erasure)
为了避免使用巨大的 `Enum` 定义所有 I/O 操作，驱动使用了自定义的类型擦除技术 (`src/io/driver/iocp/op.rs`)。
- **Union Payload**: 所有具体的 Op 负载（如 `ReadFixed`, `Connect`, `SendToPayload`）存储在一个 `union` 中。
- **VTable Logic**: 配合 `define_iocp_ops!` 宏，自动为每个 Op 生成 `OpVTable`（包含 `submit`, `on_complete`, `drop`, `get_fd` 等函数指针）。
- 这使得 `IocpOp` 结构体保持紧凑，同时支持动态分发，无需堆分配。

### 2.4 阻塞操作分流 (Blocking Offload)
文件系统的某些元数据操作（如 `Open`, `Close`, `Fsync`）在 Windows 上往往是同步阻塞的。为了不阻塞 Reactor 线程，这些操作被自动分流到内部的 `ThreadPool` (`src/io/driver/iocp/blocking.rs`)。
- 线程池任务完成后，手动调用 `PostQueuedCompletionStatus` 向 IOCP 端口发送完成通知，使其在主循环中表现为普通的异步事件。

### 2.5 Registered I/O (RIO) 集成
为了追求极致的网络性能，驱动集成了 Windows RIO 扩展。
- **缓冲区注册**: 通过 `register_buffers` 接口，预先将内存注册到内核。
- **逻辑区域映射 (Logical Region Mapping)**: Veloq 引入了创新的缓冲区管理设计。`BufferPool` 负责将内存划分为若干“逻辑区域”，每个区域对应一个索引。Windows 驱动将这些索引直接映射到 RIO Buffer ID，避免了在提交路径进行二分查找 (O(logN))，实现了 O(1) 的超高速缓冲区解析。
- **请求队列 (RQ)**: 为每个 Socket 创建 RIO Request Queue。
- **完成队列 (CQ)**: 全局使用一个 RIO Completion Queue，并将其完成通知绑定到 IOCP 端口。
- **自动降级**: 在提交操作时 (`submit.rs`)，驱动会通过 `resolve_region_info` 快速判断 Buffer 是否已注册。如果环境支持 RIO 且 Buffer 有效，则走 RIO 路径；否则回退到普通 `ReadFile`/`WSARecv`。

## 3. 模块内结构 (Internal Structure)

```
src/io/driver/iocp/
├── op.rs       // IocpOp 定义, VTable, Union Payload, 宏定义
├── submit.rs   // 核心提交逻辑，包含各 Op 的 submit_* 函数及 RIO 升级检查
├── blocking.rs // 阻塞任务线程池
├── ext.rs      // Winsock 扩展加载 (ConnectEx, AcceptEx, RIO Table)
└── tests.rs    // 单元测试
```

外部交互：
- `iocp.rs`: 驱动入口，包含 `IocpDriver` 结构体和主 `Poll` 循环。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 IocpDriver (`iocp.rs`)
`IocpDriver` 是整个驱动的核心，它管理着 IOCP 端口和 RIO 资源：
- **资源管理**:
    - `port`: IOCP 句柄。
    - `rio_cq`: 全局 RIO 完成队列（如果可用）。
    - `rio_rqs`: 句柄到 RIO 请求队列的映射。
    - `registered_bufs`: 已注册的 RIO 缓冲区信息。
- **主循环 (`get_completion`)**:
    1. 计算定时器超时。
    2. 调用 `GetQueuedCompletionStatus` 等待事件。
    3. 如果收到 `RIO_EVENT_KEY`，则进入 `process_rio_completions` 处理 RIO 完成事件。
    4. 如果是普通 IOCP 事件或阻塞池任务，直接处理 `OVERLAPPED` 关联的 `user_data`。
    5. 处理时间轮（Wheel）超时。

### 4.2 扩展加载 (`ext.rs`)
Windows 的高性能扩展 API 需要在运行时加载。`Extensions::new()` 负责：
- 创建临时 Socket。
- 使用 `WSAIoctl` 加载 `AcceptEx`, `ConnectEx`, `GetAcceptExSockaddrs`。
- 尝试加载 `RIO_EXTENSION_FUNCTION_TABLE`。注意：这需要使用专门的 GUID (`WSAID_MULTIPLE_RIO`) 和 Opcode (`SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER`)，而不是标准的扩展加载方式。如果系统不支持（如 Win7），则无缝降级，`rio_table` 为 `None`。

### 4.3 智能提交 (`submit.rs`)
该模块定义了所有操作的提交逻辑。宏 `submit_io_op!` 和 `impl_lifecycle!` 大简化了代码。
- **Recv/Send 的双路径**:
    以 `submit_recv` 为例：
    1. 检查 `ctx.rio_cq` 是否存在（RIO 可用）。
    2. **O(1) 缓冲区解析**: 调用 `val.buf.resolve_region_info()` 获取区域索引。直接使用该索引访问 `registered_bufs` 数组获取 RIO Buffer ID，不再需要昂贵的二分查找。
    3. 检查/创建 Socket 对应的 Request Queue (`ensure_rio_rq`)。
    4. 若条件满足，调用 `RIOReceive`；否则调用 `ReadFile` / `WSARecv`。
    这种设计对上层透明，用户只需使用 `register_buffers` 即可享受 RIO 加速。

### 4.4 操作定义 (`op.rs`)
通过 `define_iocp_ops!` 宏定义了如 `Accept`, `SendTo`, `Wakeup` 等操作。
- 对于复杂操作（如 `Accept` 需要预分配 buffer，`SendTo` 需要地址结构），宏支持自定义 `contruct` 和 `destruct` 闭包来管理 `Payload` 中的附加数据。
- `SubmitContext` 结构体在提交时被借用传递，包含了 `ext` (扩展表), `rio_rqs`, `registered_files` 等所有提交所需的上下文。

## 5. 存在的问题和 TODO

1.  **SyncFileRange 语义**:
    - Windows 缺乏对应 Linux `sync_file_range` 的细粒度刷新 API。目前实现为全文件 Flush，性能可能低于预期。

2.  **RIO 覆盖范围**:
    - 目前 RIO 路径主要覆盖了 `Recv` 和 `Send`。`SendTo`/`RecvFrom` (UDP) 的 RIO 路径尚未完全实现或测试。

3.  **线程池策略**:
    - `blocking.rs` 中的线程池虽然支持动态伸缩，但缺乏基于负载的精细化调度，海量阻塞文件操作可能导致线程数抖动。

## 6. 未来的方向 (Future Directions)

1.  **UDP RIO 优化**:
    - 完善 `submit_send_to`和 `submit_recv_from` 的 RIO 分支，使 UDP 应用也能利用 RIO 的高性能。

2.  **完成通知批处理**:
    - 目前 `GetQueuedCompletionStatus` 是一次处理一个。可以引入 `GetQueuedCompletionStatusEx` 批量获取，减少系统调用。`process_rio_completions` 已经实现了批量出队。

3.  **错误码标准化**:
    - 进一步完善 WSA Error 到 `std::io::Error` 的映射，提供更跨平台一致的错误语义。
