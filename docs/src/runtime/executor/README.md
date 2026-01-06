# Veloq Executor 与调度系统

本文档详细阐述 `src/runtime/executor` 模块的内部工作机制。这是 Veloq 运行时的“心脏”，负责任务的调度、负载均衡和执行循环。

## 1. 概要 (Overview)

`Executor` 模块实现了 **Work-Stealing** 和 **P2C (Power of Two Choices)** 相结合的混合调度算法。它管理着 Worker 线程的生命周期，协调 I/O 驱动 (`Driver`)、本地任务队列 (`LocalQueue`) 和网格通信 (`Mesh`)。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 混合调度策略 (Hybrid Scheduling)
单一的 Work-Stealing（接收端负载均衡）或单一的 Round-Robin（发送端负载均衡）都有局限性。Veloq 结合了两者的优点：
- **发送端 (P2C)**: 当产生新任务时，使用“两个随机选择”算法，比较两个 Worker 的负载，将任务发往较空闲的那个。这实现了 O(1) 的快速负载分布。
- **接收端 (Work-Stealing)**: 当 Worker 自己的任务做完时，它会主动去“窃取”其他 Worker 的任务。这作为兜底机制，处理负载不均的残余情况。

### 2.2 优先级倒置 (Priority Inversion for Latency)
在 Worker 的主循环中，我们显式定义了轮询顺序：
1.  **Mesh Messages** (最高优先级): 处理来自其他 Worker 的任务/消息。这保证了跨核通信的低延迟。
2.  **Local Queue**: 处理本地生成的任务。
3.  **Global Injector**: 处理全局注入的任务。
4.  **Work Stealing**: 最后尝试去偷任务。

### 2.3 动态注册 (Dynamic Registry)
不同于静态数组，`ExecutorRegistry` 支持动态扩缩容（虽然目前 Runtime 启动时是固定的），并使用 `smr-swap` 实现了无锁的读取快照，确保调度器在遍历 Worker 列表时不会被锁阻塞。

## 3. 模块内结构 (Internal Structure)

- `executor.rs`: 定义 `LocalExecutor` 结构体和 `run` 主循环。
- `spawner.rs`:
    - `Spawner`: 提供给用户的句柄，用于 spawn 任务。
    - `ExecutorRegistry`: 全局注册表，维护所有活跃的 ExecutorHandle。
    - `MeshContext`: 封装 Mesh 的发送端和接收端。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 主循环 (`LocalExecutor::run`)
核心是一个带有 **Budget** (预算) 的循环：
```rust
loop {
    while executed < BUDGET {
        // 1. Mesh Polling
        if self.try_poll_mesh() { ... }
        
        // 2. Local Queue
        if let Some(task) = self.queue.pop() { ... }
        
        // 3. Global Injector
        if self.try_poll_injector() { ... }
        
        // 4. Stealing
        if self.try_steal() { ... }
    }
    
    // 5. Park
    self.park_and_wait();
}
```
`BUDGET` 机制（默认 64）非常重要，它防止计算密集型任务饿死 I/O 事件的轮询。每执行一定数量的任务后，必须强制检查 I/O (park_and_wait)，即使还有任务堆积。

### 4.2 停车与唤醒 (`park_and_wait`)
这是一个精细的状态机：
1.  **Set PARKING**: 标记自己准备停车。
2.  **Double Check**: 再次检查 Mesh 和队列，防止在标记期间有新任务到达（由 `MeshContext::send_to` 中的 Notify 逻辑保证）。
3.  **Commit PARKED**: 真正进入睡眠。
4.  **Driver.wait()**: 调用底层的 `epoll_wait` / `GetQueuedCompletionStatus`。

### 4.3 P2C 实现 (`Spawner::p2c_select`)
```rust
let idx1 = rand % count;
let idx2 = rand2 % count;
let load1 = workers[idx1].total_load();
let load2 = workers[idx2].total_load();
return min(load1, load2);
```
这里的 `total_load` 是 `injected_load` (远程注入) + `local_load` (本地积压) 的总和，能较准确反映 Worker 的繁忙程度。

### 4.4 smr-swap 的使用
`ExecutorRegistry` 使用 `smr_swap::SmrSwap` 来存储 `Vec<ExecutorHandle>`。
- **写**: `update()` 是序列化的（Mutex保护），属于低频操作（仅在 Worker 启动时）。
- **读**: `reader().load()` 是完全无锁 wait-free 的，所有 Spawner 在做 P2C 选择时都通过此路径，性能极高。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **负载倾斜后的 Ping-Pong**:
    在某些极端情况下，P2C 可能导致任务在一个群组内来回反弹，或者 Work Stealing 过于激进导致缓存失效。
    *TODO*: 实现指数退避 (Exponential Backoff) 的 Stealing 策略。

2.  **NUMA 感知**:
    目前的 Stealing 是随机的。在多 Socket 机器上，跨 NUMA 节点 Stealing 代价巨大。
    *TODO*: 实现分层的 Stealing，优先在同 NUMA 节点内窃取。

3.  **Worker ID 回收**:
    目前 `Worker ID` 是单调递增的，如果你频繁创建销毁 Runtime，ID 可能会耗尽（虽然 usize 很大）。

## 6. 未来的方向 (Future Directions)

1.  **时间片轮转 (Time Slicing)**:
    目前任务协作是完全自愿的。未来可能结合 `Cooperative Task` 自动插入 yield 点，防止单个任务霸占 Budget。

2.  **自适应 Budget**:
    根据系统的 I/O 压力动态调整 `BUDGET`。如果 I/O 延迟敏感，减小 Budget；如果吞吐敏感，增大 Budget。
