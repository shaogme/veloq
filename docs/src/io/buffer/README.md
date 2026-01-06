# buffer 模块文档

本文档详细介绍了 `veloq-runtime` 中的 `io::buffer` 模块。该模块负责高性能异步 I/O 的内存管理，特别针对 io_uring 和 IOCP 的需求进行了优化。

## 1. 概要 (Overview)

`src/io/buffer` 模块提供了一套兼容现代异步 I/O 接口的内存池抽象。与传统的 `malloc/free` 不同，本模块的设计目标是：

*   **地址稳定 (Address Stability)**: 异步操作提交给内核后，缓冲区的物理内存地址必须保持不变。
*   **按需对齐 (Alignment)**: 支持 Direct I/O (O_DIRECT) 所需的严格对齐（通常为 512 字节或 4096 字节）。
*   **类型擦除 (Type Erasure)**: 通过手动虚函数表 (VTable) 实现多态，使得 `FixedBuf` 句柄可以被统一管理，而无需携带泛型参数。
*   **零拷贝友好**: 预注册的缓冲区 (Fixed Buffers) 可以减少内核态和用户态之间的内存拷贝（特别是在 io_uring 上）。

核心组件包括：
*   **`BufPool` Trait**: 内存池接口规范。
*   **`FixedBuf`**: 用户持有的缓冲区句柄，RAII 风格管理的内存资源。
*   **`BuddyPool`**: 基于伙伴系统 (Buddy System) 的通用分配器。
*   **`HybridPool`**: 结合了 Slab 和全局堆分配的混合分配器，针对常见 I/O 大小进行了优化。

## 2. 理念和思路 (Philosophy and Design)

### 2.1 内存稳定性与生命周期
在 Proactor 模式（io_uring/IOCP）中，操作系统内核直接读写用户空间的缓冲区。如果缓冲区在 I/O 完成前被移动（例如 Vec 扩容）或释放，将导致严重的数据损坏或崩溃。

**解决方案**: `FixedBuf` 拥有底层的内存块，并且不允许在原地进行扩容操作（扩容需要重新分配）。其生命周期通过 Rust 的所有权系统与 I/O 操作绑定，确保 I/O 进行期间缓冲区有效。

### 2.2 Direct I/O 对齐
为了发挥 NVMe SSD 的最大性能，通常需要使用 Direct I/O 绕过系统页缓存。这对内存对齐有严格要求：
1.  **起始地址对齐**: 必须对齐到扇区大小（512B）或页大小（4KB）。
2.  **长度对齐**: 读写长度通常也要求是扇区大小的倍数。

本模块中的 `AlignedMemory` 和所有 Pool 实现都强制执行 4KB (Page Size) 对齐，确保生成的缓冲区天然满足 Direct I/O 要求。

### 2.3 手动 VTable 与类型擦除
为了在 `Driver` 和 `Context` 中存储不同类型的缓冲区而不引入复杂的泛型（这会传染整个代码库），本模块采用了类似 C++ 虚函数的各种手动 VTable 技术。
*   `FixedBuf` 内部持有 `pool_data` (指针) 和 `vtable` (静态引用)。
*   `drop` 时，`FixedBuf` 调用 vtable 中的 `dealloc` 函数指针，将内存归还给原本的 pool。
*   这种设计使得 `FixedBuf` 结构体的大小固定，且可以跨模块传递，无需关心它是由 BuddyPool 还是 HybridPool 分配的。

## 3. 模块内结构 (Internal Structure)

```
src/io/buffer.rs           // 核心定义：BufPool Trait, FixedBuf, AlignedMemory, AnyBufPool
src/io/buffer/
├── buddy.rs               // BuddyPool: 伙伴系统实现
└── hybrid.rs              // HybridPool: Slab + Global 混合实现
```

*   **`buffer.rs`**: 定义了所有公共接口。`AnyBufPool` 是一个为了兼容线程局部存储 (TLS) 而设计的类型擦除容器。
*   **`buddy.rs`**: 实现了一个经典的 Binary Buddy Allocator。管理一块连续的大内存（Arena），支持从 4KB 到 4MB+ 的动态分裂与合并。
*   **`hybrid.rs`**: 针对网络 I/O 中常见的固定大小（4K, 8K, 16K, 32K, 64K）使用了 Slab 分配器，以获得 O(1) 的分配速度；对于非常规大小则回退到系统分配器。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 核心抽象 (`buffer.rs`)

**`FixedBuf`**:
```rust
pub struct FixedBuf {
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    global_index: u16,      // 用于 io_uring fixed buffer index
    pool_data: NonNull<()>, // 指向 Pool 实例的指针
    vtable: &'static PoolVTable, // 虚函数表
    context: usize,         // 分配上下文（如 Buddy 的 order 或 Hybrid 的 slab_index）
}
```
`FixedBuf` 将通常存储在堆内存头部的元数据（Header）内联到了结构体中。这样做的好处是保持了 Payload 的纯净和严格对齐，避免了 Header 对齐带来的内存浪费。

**`BufPool` Trait**:
定义了 `alloc_mem` 和 `get_registration_buffers`。后者是 Linux 特有的，用于提取底层的大块内存区域并注册到 io_uring，实现 `IORING_OP_READ_FIXED` 等零拷贝操作。

### 4.2 BuddyPool (`buddy.rs`)
*   **机制**: 维护 14 个链表 (`free_heads`)，分别对应 2^0 到 2^13 个 4KB 块。
*   **分配**: 计算所需的 `order`，如果当前 order 没空闲块，则向上查找并分裂。
*   **释放**: 释放时检查 `buddy`（兄弟块）是否空闲，如果是则合并，并递归向上合并。
*   **Tags**: 使用 `Vec<u8>` 存储每个块的状态（Allocated/Free）和 Order，用于在释放时快速定位 Buddy。
*   **优点**: 内存碎片率较低，适合通用场景。
*   **缺点**: 分裂和合并有一定计算开销。

### 4.3 HybridPool (`hybrid.rs`)
*   **机制**: 预定义了 5 种规格的 Slab (4K, 8K, 16K, 32K, 64K)。每个 Slab 是一块连续的大内存，被切分为固定大小的 slot。
*   **分配**:
    *   **Small (<64K)**: 根据大小映射到对应的 Slab，O(1) 弹出空闲索引。使用 BitSet 记录分配状态以防止 Double Free。
    *   **Large (>64K)**: 直接调用系统 `alloc` (Global Fallback)。
*   **释放**:
    *   根据 `context` 判断是 Slab 还是 Global。
    *   如果是 Slab，将索引还回 `free_indices` 栈。
*   **优点**: 对于固定大小的网络包（如 4KB, 16KB），分配极其迅速，无碎片。
*   **缺点**: 对于不规则大小的内存利用率可能不如 Buddy。

## 5. 存在的问题和 TODO (Issues and TODOs)

1.  **硬编码常量**:
    *   `buddy.rs` 中的 `ARENA_SIZE` 固定为 32MB。这在生产环境中可能太大（对于大量空闲线程）或太小（对于高负载线程）。
    *   **TODO**: 支持在创建 Pool 时配置 Arena 大小。

2.  **注册机制的局限性**:
    *   `get_registration_buffers` 目前是一次性返回所有底层内存。对于动态扩容的 Pool（如果未来实现），这种静态注册机制也需要更新。
    *   目前 `HybridPool` 的 Global Fallback 分配的内存无法享受 io_uring 的 Fixed Buffer 加速（因为它们不在预分配的 Slab 中）。

3.  **线程安全性假定**:
    *   目前的实现虽然用了 `Rc<RefCell<...>>`，本质上是非线程安全的 (Thread-Local)。这符合 `thread-per-core` 模型，但如果需要在线程间传递 `FixedBuf` 并跨线程释放，目前的设计会 Panic 或导致未定义行为（因为 Pool 是 TLS 的）。
    *   **TODO**: `FixedBuf` 目前标记为 `!Send`，这是正确的。未来如果支持跨线程窃取任务，需要考虑缓冲区跨线程归还的机制（例如通过 Mesh 发送回原线程）。

4.  **BitSet 依赖**:
    *   `HybridPool` 依赖 `veloq_bitset`，需要确保该依赖的性能足够高。

## 6. 未来的方向 (Future Directions)

1.  **动态 Arena**:
    *   实现可增长的 Buddy System，当 32MB 用完时，自动申请新的 Arena 并链接起来。

2.  **Huge Page 支持**:
    *   在 Linux 上使用 `madvise(MADV_HUGEPAGE)` 或显式 HugeTLB 分配大块内存，进一步减少 TLB Miss，提高大内存访问性能。

3.  **Unified Buffer Management**:
    *   结合 io_uring 的 `IORING_REGISTER_BUFFERS2`，实现更加动态的 Registered Buffers 更新机制。
