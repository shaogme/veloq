# Veloq Runtime 异步 I/O 驱动架构文档

本文档详细阐述了 `veloq-runtime` 核心 I/O 驱动层 (`src/io/driver`) 的设计哲学、架构结构及实现细节。该层旨在屏蔽操作系统底层的异步 I/O 差异（Linux io_uring 与 Windows IOCP），为上层运行时提供统一的高性能 Proactor 接口。

## 1. 概要 (Overview)

`src/io/driver` 是运行时与操作系统内核交互的边界。与 Rust 生态中常见的 Reactor 模型（如 Tokio 基于 mio/epoll）不同，Veloq 采用了纯 **Proactor** 模型。

- **Reactor**: 关注“就绪”事件（Readiness）。当 socket 可读时通知应用，应用再调用 `read`。
- **Proactor**: 关注“完成”事件（Completion）。应用直接提交 `read` 操作，内核将数据写入缓冲区后通知应用完成。

这种设计是为了原生适配现代高性能 I/O 接口（io_uring 和 IOCP 均为 Proactor 性质），避免中间层的模拟开销，并最大化利用内核的零拷贝和批量处理能力。

统一的核心抽象是 `Driver` Trait，它定义了操作提交、轮询、取消和资源管理的行为。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 统一的 Proactor 抽象
尽管 io_uring（基于环形队列）和 IOCP（基于完成端口队列）在 API 形式上差异巨大，但在逻辑上它们都遵循：
`提交请求 (Submit) -> 等待 (Wait) -> 处理完成 (Process Completion)`

Veloq 抽象出了这一公共流程：
1.  **Reserve**: 在用户态分配一个 Slot (User Data)，用于关联上下文。
2.  **Submit**: 将操作描述符提交给内核，并传入 User Data。
3.  **Poll**: 上层 `Future` 在 `poll` 时检查 Driver 中该 Slot 的状态。
4.  **Complete**: 驱动收到内核完成通知，通过 User Data 找到 Slot，填入结果并唤醒 Waker。

### 2.2 内存稳定性 (Stable Memory)
异步 I/O 的一个核心挑战是**缓冲区的生命周期**。在 Proactor 模式下，当操作被提交给内核后，应用程序**必须**保证传递给内核的缓冲区指针在操作完成前一直有效。

- 普通的 `Vec` 或 `HashMap` 在扩容时会移动元素，导致指针失效（Use-After-Free）。
- **解决方案**: 引入 `StableSlab` (`src/io/driver/stable_slab.rs`)。这是一个分页的 Slab 分配器，保证元素一旦插入，其内存地址在生命周期内绝对固定，直到被显式移除。这对于 io_uring 的 `user_data` 索引和 IOCP 的 `OVERLAPPED` 指针至关重要。

### 2.3 零开销类型擦除 (Zero-Cost Type Erasure)
驱动需要支持多种 I/O 操作（Read, Write, Connect, Accept, Close 等）。
- **传统做法**: 使用巨大的 `Enum` 包裹所有可能的操作。缺点是内存浪费（结构体大小取决于最大的那个变体）。
- **Veloq 做法**: 使用 **Union + VTable** (`PlatformOp` Trait 和具体的 `Op` 实现)。
    - **Payload**: 使用 `union` 存储不同操作的数据载荷。
    - **VTable**: 每个操作携带一个静态虚函数表（VTable），包含构建提交项、处理完成回调、销毁逻辑等指针。
    - 这种类似 C++ 虚函数的机制是在编译期生成的，避免了运行时的动态内存分配（Heap Allocation），同时保持了数据结构的紧凑。

### 2.4 Mesh 网络协同与远程注入
驱动不仅处理本地 I/O，还通过 **Injector** 机制深度集成了跨线程调度能力。
- **Injector**: 每个驱动实例暴露一个线程安全的注入器 (`Injector<D>`)。
- **Closure Injection**: 允许其他线程将闭包 (`Box<dyn FnOnce(&mut Driver) + Send>`) 发送到驱动线程执行。这构成了 `RemoteOp` 的基础——远程线程如果不持有 Driver 的资源的线程所有权，可以通过注入闭包，“委托” Driver 线程提交操作并挂载回调。
- **Inner Handle / Notify**: 暴露底层的 `RawFd` 或 `Handle` 并提供唤醒机制，用于实现高效的事件通知。

## 3. 模块内结构 (Internal Structure)

```
src/io/
├── driver.rs           // Driver 模块定义与接口 (Trait, PlatformOp)
└── driver/             // Driver 具体实现与组件
    ├── op_registry.rs  // 通用操作注册表
    ├── stable_slab.rs  // 稳定内存分配器
    ├── iocp.rs         // Windows 平台实现入口 (取代 mod.rs)
    ├── iocp/           // Windows 子模块目录
    │   ├── blocking.rs
    │   ├── op.rs
    │   └── ...
    ├── uring.rs        // Linux 平台实现入口 (取代 mod.rs)
    └── uring/          // Linux 子模块目录
        ├── submit.rs
        └── ...
```

- **`driver.rs`**: 定义了驱动必须实现的接口规范。
- **`op_registry.rs`**: 不依赖平台的通用组件，管理 `OpEntry`（包含 Waker、Result、资源本身）。它是 `StableSlab` 的一层封装，处理与 `Context` 和 `Waker` 的交互。
- **`stable_slab.rs`**: 核心数据结构，提供 O(1) 的插入、删除和索引访问，且保证内存地址稳定。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 Driver Trait (`driver.rs`)
```rust
pub trait Driver: 'static {
    type Op: PlatformOp;
    type RemoteInjector: Injector<Self>;
    
    // 核心生命周期
    fn reserve_op(&mut self) -> usize;
    fn submit(&mut self, user_data: usize, op: Self::Op) -> Result<Poll<()>, ...>;
    fn poll_op(&mut self, user_data: usize, cx: &mut Context) -> Poll<...>;
    
    // 远程操作支持
    fn injector(&self) -> Arc<Self::RemoteInjector>;
    fn attach_remote_completer(&mut self, user_data: usize, completer: Box<dyn RemoteCompleter<Self::Op>>);

    // 驱动循环
    fn wait(&mut self) -> io::Result<()>;
    fn process_completions(&mut self);
    
    // 资源管理
    fn register_files(...) -> ...;
    fn submit_background(&mut self, op: Self::Op) -> ...;
}
```
`Driver` 是单线程的（`&mut self`），这种设计消除了大部分锁竞争。跨线程交互完全依赖 `Injector` 和 `RemoteCompleter`：
1.  **Remote Submission**: 远程线程通过 `injector.inject()` 发送闭包。
2.  **Execution**: 驱动线程执行闭包，调用 `submit` 提交操作，并调用 `attach_remote_completer` 将一个回调挂在到 Slot 上。
3.  **Completion**: 当操作完成时，驱动检测到 Slot 上有 `PlatformData::Remote(completer)`，直接调用 `completer.complete()` 而不是唤醒 Waker。这通常会通过 Channel 通知远程 Future。

### 4.2 StableSlab (`stable_slab.rs`)
- **存储结构**: `Vec<Box<[Slot<T>; PAGE_SIZE]>>`。
- **扩容机制**: 当空间不足时，分配一个新的 Page (`Box<[Slot]>`) 并加入 Vec。现有的 Pages 及其内部元素的地址保持不变。
- **Free List**: 维护一个空闲链表 (`free_head`)，新元素复用空闲槽位。
- **主要用途**: 
    - 存储 `IocpOp` / `UringOp`。
    - `user_data` (usize) 直接编码了 `(page_index, slot_index)`，实现 O(1) 查找。

### 4.3 OpRegistry (`op_registry.rs`)
它是连接 `Driver` 和 `Future` 的桥梁。
- **Payload**: `OpEntry<Op, P>`。包含：
    - `resources`: 实际的 I/O 操作对象（如 Socket 句柄、缓冲区）。操作完成前，所有权属于驱动。
    - `waker`: 任务挂起时的 Waker (用于 Local Op)。
    - `platform_data`: 平台相关数据，也可以存储 `RemoteCompleter` (用于 Remote Op) 或 Timer/Background 标识。
    - `result`: 存放操作完成后的结果。
- **Poll 逻辑**:
    - `poll_op` 被上层 `Future` 调用 (仅限 Local Op)。
    - 如果 `result` 存在 -> 取出 Result 和 Resources，返回 `Poll::Ready`，并移除 Slot。
    - 否则 -> 更新 `waker`，返回 `Poll::Pending`。
- **Remote Completion 逻辑**:
    - 在 `process_completions` 中，如果发现 Slot 关联的是 `RemoteCompleter`，则**不更新 Waker**，也不等待 `poll_op`。
    - 直接取出 Result 和 Resources，调用 `completer.complete(result, resources)`。这将触发远程 Channel 的发送。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **Backlog 策略差异**:
    - **Linux**: io_uring 的 SQ 也是环形队列，会满。`UringDriver` 必须在用户态实现一个 Backlog 链表来暂存无法提交的操作。
    - **Windows**: IOCP 本身没有“提交队列满”的概念（它是直接调 API），但为了防止内存无限增长，我们人为限制了 `Slab` 的大小。
    - **TODO**: 统一 Backpressure（背压）策略，当驱动过载时，向上层返回明确的错误或挂起信号，而不是无限缓冲。

2.  **Buffer Registration 抽象泄漏 (已解决)**:
    - 引入了**逻辑区域映射 (Logical Region Mapping)** 层。
    - `BufferPool` 将内存暴露为带索引的 Region。
    - 驱动层（IOCP/Uring）分别将这些索引映射为 RIO Buffer ID 或 io_uring fixed indices。
    - 结果：`FixedBuf` 只需携带一个通用的 `region_index`，实现了跨平台的 O(1) 提交，完全屏蔽了底层差异。

3.  **同步文件 I/O**:
    - 在 io_uring 上文件 I/O 是真异步的。在 IOCP 上，部分文件操作（特别是打开/关闭）仍通过线程池模拟。这种差异导致性能特性的不一致。
    - **TODO**: 在 Linux 上对于不支持 io_uring 的动作也应有统一的线程池回退机制（目前可能有隐式阻塞）。

## 6. 未来的方向 (Future Directions)

1.  **支持更多后端**:
    - 虽然目前专注高性能 Proactor，但为了兼容性（如 macOS），未来可能需要引入 `kqueue` 后端。但这需要适配层模拟 Proactor 行为（类似 Tokio 的做法，但在 Driver 层内部封装）。

2.  **Direct I/O 与 Zerocopy**:
    - 进一步深挖 `IORING_OP_SEND_ZC` 和 Windows RIO。
    - 目标是实现网络栈的零拷贝发送和接收，这对于高吞吐场景（如 100Gbps 网络）是必须的。

3.  **内核旁路 (Kernel Bypass) 集成**:
    - 随着 io_uring 支持 `IORING_OP_URING_CMD`，未来可以直接对接 NVMe 驱动或用户态网络栈（如 AF_XDP），进一步绕过内核协议栈开销。
