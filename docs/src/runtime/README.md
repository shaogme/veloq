# Veloq Runtime 核心架构文档

本文档主要介绍 `veloq-runtime` 核心运行时层 (`src/runtime`) 的架构设计、核心组件及实现原理。

## 1. 概要 (Overview)

Veloq Runtime 是一个基于 **Thread-per-Core** 模型的高性能异步运行时。它旨在充分利用现代多核硬件的特性，通过减少跨核通信、锁竞争和缓存抖动来最大化 I/O 和计算吞吐量。与 Tokio 等通用运行时不同，Veloq 更侧重于显式的资源管理和基于网格 (Mesh) 的任务调度。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 Thread-per-Core 与 Nothing-Shared
我们坚信在现代高并发场景下，**数据局部性 (Data Locality)** 是性能的关键。
- 也就是每个 Worker 线程拥有独立的 I/O 驱动 (Driver)、任务队列和缓冲区池。
- 尽量避免全局锁。线程间的交互主要通过显式的消息传递（Mesh Network）或高效的工作窃取（Work Stealing）进行。

### 2.2 Mesh Network (以通信代共享)
传统的运行时通常使用全局的 `Mutex<Queue>` 或 `SegQueue` 来进行跨线程任务调度，这在高负载下会产生严重的 CAS 争用。
Veloq 引入了 **Mesh** 概念：
- 每个 Worker 之间建立两两互联的 **SPSC (Single-Producer Single-Consumer)** 无锁通道。
- 当 Worker A 需要将任务发给 Worker B 时，它直接将任务推入 A->B 的 SPSC 通道，零锁竞争。
- 这种全互联拓扑形成了一个高效的任务分发网格。

### 2.3 显式上下文 (Explicit Context)
为了避免隐式的全局状态（如 TLS 中的隐藏变量），Veloq 提供了 `RuntimeContext`：
- 显式通过上下文访问 Spawner、Driver 和 Mesh。
- `bind_pool` 强制要求缓冲区池与当前线程绑定，确保 I/O 内存操作的安全性（避免跨线程释放）。

## 3. 模块内结构 (Internal Structure)

代码位于 `src/runtime/`：

```
src/runtime/
├── context.rs    // 线程局部上下文 (TLS)，提供 spawn 接口和资源访问
├── task.rs       // 任务 (Task) 定义，手动实现的 RawWakerVTable
├── mesh.rs       // Mesh 网络通信原语 (SPSC Channel)
├── join.rs       // JoinHandle 实现，管理任务结果的异步等待
├── executor.rs   // (入口) LocalExecutor 定义，Runtime 组装
└── executor/     // 执行器内部实现细节
    └── spawner.rs // 任务生成器、注册表 (Registry) 和负载均衡逻辑
```

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 Task (`task.rs`)
Veloq 的 Task 是对 `Future` 的轻量级封装。
- **RawWakerVTable**: 手动实现了虚函数表，而不是使用 `Arc::new(Mutex::new(..))`。
- **内存布局**:
  `Rc<Task>` 包含 `RefCell<Option<Pin<Box<Future>>>>` 和指向执行器队列的 `Weak` 指针。
  使用 `Rc` 而非 `Arc` 是因为 Task 通常在本地队列中流转，且 Veloq 鼓励本地计算。跨线程调度时，Task 会被打包成 `Job` (Box<FnOnce>) 传输，到达目标线程后再重新封装为本地 Task。

### 4.2 Mesh (`mesh.rs`)
实现了高性能的 SPSC 环形缓冲区。
- **Cache Padding**: 使用 `#[repr(align(128))]` 对 `Producer` 和 `Consumer` 的头尾指针进行隔离，防止伪共享 (False Sharing)。
- **Batching**: 支持批量处理（虽然目前主要单次 pop），为未来的吞吐优化留出空间。
- **Notify-on-Park**: 生产者在 push 时可以检查目标消费者的状态。如果目标处于 `PARKED` 状态，生产者会触发 I/O 驱动的唤醒机制 (Notify System)，确保目标及时处理消息。

### 4.3 JoinHandle (`join.rs`)
实现了任务结果的异步获取。
- **无锁状态机**: 使用 `AtomicU8` 维护状态 (`IDLE` -> `WAITING` -> `READY`)。
- **原子性**: 只有当消费者进入 `WAITING` 状态时，生产者才会尝试唤醒。这避免了不必要的 `Waker` 克隆和唤醒开销。
- **Local vs Send**: 区分 `LocalJoinHandle` (单线程内) 和 `JoinHandle` (跨线程)，分别优化开销。

### 4.4 Context (`context.rs`)
- **Guard 模式**: 使用 `ContextGuard` 确保 `enter()` 和退出时的环境恢复。
- **Buffer Pool 集成**: `bind_pool` 和 `try_bind_pool` 负责将 `BufPool` 注册到当前上下文，这是 io_uring 固定缓冲区机制生效的关键路径。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **Blocking IO 支持不足**:
    目前缺乏一个专用的 `blocking_spawn` 线程池来处理重 CPU 或阻塞系统调用（非 I/O 类）。如果在 Veloq 任务中执行长时间同步代码，会阻塞整个 Worker Loop。
    *TODO*: 引入专用的 Blocking Thread Pool。

2.  **Task Debugging**:
    目前的 Task 结构体非常精简，缺乏调试信息（如任务名称、创建堆栈）。
    *TODO*: 在 Debug 模式下注入追踪信息。

3.  **Local Task 饿死**:
    虽然有 Budget 机制，但在极端混合场景下（大量 Mesh 消息 + 本地任务），调度策略可能仍需微调。

## 6. 未来的方向 (Future Directions)

1.  **结构化并发 (Structured Concurrency)**:
    实现类似 `TaskScope` 的机制，支持任务组的自动取消和错误传播。

2.  **协作式抢占 (Cooperative Preemption)**:
    目前依赖用户代码中的 `.await` 点进行调度。如果用户写了死循环，Worker 会卡死。未来可考虑结合编译器插件或计时器信号进行强制让出检测。
