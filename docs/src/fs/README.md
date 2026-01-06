# File System (FS) 模块文档

本文档详细介绍了 `veloq-runtime` 的文件系统模块 (`src/fs`)。该模块旨在提供接近硬件极限的文件 I/O 性能，特别是在支持异步 I/O (io_uring / IOCP) 的现代操作系统上。

## 1. 概要 (Overview)

`src/fs` 模块提供了对底层文件描述符（或句柄）的异步封装。与标准库的 `std::fs` 不同，本模块的设计初衷是**非阻塞 (Non-blocking)** 和**零拷贝 (Zero-Copy)**。

核心组件包括：
*   **`File`**: 文件的异步句柄，实现了 `AsyncBufRead` 和 `AsyncBufWrite`。
*   **`OpenOptions`**: 高度可配置的文件打开构建器，支持跨平台的缓冲模式设置（如 Direct I/O）。

## 2. 理念和思路 (Concepts)

### 2.1 显式缓冲控制 (Explicit Buffering Control)
传统的缓冲 I/O (Buffered I/O) 依赖操作系统的 Page Cache。虽然简单，但在高吞吐场景下（如数据库 WAL、大文件传输）会导致双重拷贝和不可预测的延迟。
`Veloq` 通过 `BufferingMode` 提供了对 I/O 模式的精细控制：
*   **`Buffered`**: 标准模式，使用 Page Cache。
*   **`Direct`**: 绕过 Page Cache (O_DIRECT / NO_BUFFERING)，直接在用户态缓冲区和磁盘间传输数据。这要求缓冲区必须对齐（由 `BufPool` 保证）。
*   **`DirectSync`**: Direct I/O 加上同步写入 (O_DSYNC / WRITE_THROUGH)，确保数据落盘。

### 2.2 异步资源释放 (Async Drop)
在 Rust 中，`Drop` 是同步运行的。如果关闭文件描述符 (`close`/`CloseHandle`) 发生阻塞，会严重影响 Event Loop 的响应度。
`File` 实现了**非阻塞 Drop** 机制：在销毁时，它会尝试向 Runtime 提交一个后台关闭操作 (`Close` Op)。只有在 Runtime 无法访问（如已关闭）时，才会回退到同步关闭。

## 3. 模块内结构 (Internal Structure)

```
src/
├── fs.rs            // 模块定义与导出
└── fs/
    ├── file.rs      // 核心文件结构体
    └── open_options.rs // 打开选项与标志位处理
```

*   **`fs.rs`**: 模块入口，负责导出子模块 (`file`, `open_options`) 和统一对外类型 (`File`, `OpenOptions`)。遵循 Rust 2018 模块标准，摒弃了 `mod.rs`。
*   **`file.rs`**: 封装了 `IoFd` 和对 `PlatformDriver` 的弱引用。提供了 `read_at`, `write_at`, `sync_range` 等高级操作。

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 `OpenOptions::open`
这是文件打开的入口。
1.  **Buffer 获取**: 打开文件时，如果是 Windows 平台或需要将路径传给内核，需要分配内存。这里使用了当前线程绑定的 `BufPool`。
2.  **Op 构建**: 调用 `build_op`，根据 `BufferingMode` 设置标志位。
    *   *Windows 特殊处理*: 利用位掩码 (`FAKE_NO_BUFFERING`) 将 Direct I/O 标志传递给底层的阻塞线程池或原生 API。
3.  **驱动绑定**: 文件句柄创建后，会捕获当前上下文的 `Driver`，用于后续的 I/O 操作提交。

### 4.2 `File` 的异步读写
`File` 实现了 `AsyncBufRead` 和 `AsyncBufWrite`，这意味着它可以无缝集成到 `veloq` 的异步生态中。
此外，它提供了原子偏移量的 I/O：
*   **`read_at` / `write_at`**: 这对实现数据库至关重要，允许并发地读写文件的不同部分而无需修改文件指针 (`seek`)。底层对应 `pread` / `pwrite` (or equivalent)。

### 4.3 `SyncRangeBuilder`
提供了细粒度的刷盘控制：
```rust
file.sync_range(0, 1024)
    .wait_before(false)
    .write(true)
    .await
```
这段利用 Builder 模式的代码最终生成一个 `SyncFileRange` Op。
*   **Linux**: 完美映射到 `sync_file_range` 系统调用。
*   **Windows**: 当前回退到 `FlushFileBuffers` (全文件 sync)，因为 Win32 API 缺乏精确的范围同步机制。

## 5. 存在的问题和 TODO

1.  **文件元数据操作**: 
    *   目前缺少 `metadata()`, `set_permissions()`, `set_len()` 等标准操作。需要添加对应的异步 Op。
2.  **目录操作**:
    *   缺少 `read_dir` (I/O intensive) 的异步实现。
3.  **Windows 范围同步**:
    *   Windows 的范围同步回退到全量同步由于性能原因可能不可接受。**TODO**: 调研是否有未公开 API 或利用 `LockFile` 等机制间接实现更细粒度的控制，或者明确文档警示。
4.  **路径处理**:
    *   目前路径处理涉及一次从 `OsStr` 到 `BufPool` 缓冲区的拷贝。对于极高频打开操作，这可能存在微小开销。

## 6. 未来的方向

1.  **Registered Files (io_uring)**:
    *   支持 `io_uring` 的 "Fixed Files" 特性。允许用户注册文件描述符，减少内核层面的引用计数开销。这对高频 I/O 场景提效显著。
2.  **IO 优先级**:
    *   暴露 I/O 优先级设置（如 `ioprio_set`），允许关键任务抢占磁盘带宽。
