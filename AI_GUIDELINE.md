# AI_GUIDELINE.md

此文件为 AI 在处理本仓库代码时提供指导。

## 核心原则 (Core Principles)

1.  **回复语言**：始终使用**中文**回复。
2.  **代码风格**：
    *   **严禁使用 `mod.rs`**。必须遵守 Rust 2018 Edition 及更新版本的目录结构标准。
    *   模块 `foo` 应该定义在 `foo.rs` 中。如果 `foo` 有子模块，应创建 `foo/` 目录，但父模块代码仍保留在 `foo.rs` 中，而不是 `foo/mod.rs`。
3.  **禁止猜测**：严禁猜测代码逻辑或文件内容。在修改或回答之前，必须先读取相关代码。
4.  **主动报告**：在阅读代码时，主动发现并报告潜在的错误、安全漏洞或性能问题，不要等到用户询问。
5.  **绝对路径**：在使用任何文件修改工具（如 `write_to_file`, `replace_file_content`）时，**必须**使用文件的**绝对路径**。
6.  **Rust Edition 2024**：本项目采用 **Rust Edition 2024**。请充分利用新特性，特别是**异步闭包 (Async Closures)** 和  `AsyncFnOnce` / `AsyncFnMut` / `AsyncFn` trait 的内置支持，避免手动装箱 `Future`。

## 常用命令 (Commands)

### 构建与测试
- **构建**: `cargo build`
- **测试**: `cargo test`
- **运行单个测试**: `cargo test test_name`
- **Lint**: `cargo clippy`
- **格式化**: `cargo fmt`

### Docker
- **构建镜像**: `docker build -t veloq .`
- **运行容器**: `docker run -it veloq`
- **直接运行检查**: `docker-compose run --rm standalone cargo check`
- **直接运行测试**: `docker-compose run --rm standalone cargo test`
- **运行性能基准测试**: `docker-compose run --rm standalone cargo bench`
- **更新 nix 依赖**: `docker-compose run --rm flake-update`

## 架构 (Architecture)

本项目包含三个核心 Crate：`veloq-wheel`、`veloq-queue` 和 `veloq-runtime`。

### `veloq-wheel`
高性能分层时间轮 (Hierarchical Timing Wheel)。
- **组件**:
  - `Wheel`: 核心结构，管理分层 (L0, L1) 的任务。
  - `SlotMap`: 存储任务 (`WheelEntry`)，使用稳定的 `TaskId` 作为键，实现 O(1) 访问。
  - `WheelEntry`: 任务节点，形成单向链表（针对惰性取消进行了优化）。
- **关键机制**:
  - **惰性取消 (Lazy Cancellation)**: `cancel()` 仅将任务标记为已移除（将 `item` 设为 `None`）。任务在 `advance()` 推进到相应槽位时才会被物理移除。这避免了昂贵的链表解绑操作。
  - **级联 (Cascading)**: 随着时间推进，高层级 (L1) 的任务会被移动到 L0 或过期。

### `veloq-runtime`
高性能异步 I/O 运行时。
- **核心组件**:
  - **Runtime Core (`src/runtime/`)**:
    - **Runtime (`runtime.rs`)**: 运行时入口，定义 `Runtime` 结构体和组装逻辑。
    - **Executor (`src/runtime/executor/`)**:
      - `LocalExecutor`: 线程局部执行器。管理 `Pinned` (本地/非 Send) 任务和 `Stealable` (通用/Send) 任务的执行。
      - **调度**: 结合工作窃取 (Work Stealing) 和 P2C (Power of Two Choices)。`Spawner` 利用 `ExecutorRegistry` 进行全局负载均衡。
      - **队列**: 区分 `pinned_receiver` (本地任务通道), `remote_receiver` (远程任务通道), 和 `stealer`/`injector` (任务窃取队列)。
    - **Task System (`src/runtime/task/`)**:
      - **架构分离**: 区分 `Task` (Pinned, `!Send`) 和 `Runnable` (Stealable, `Send`)。
      - `harness.rs`: 实现 `Runnable` 的原子状态机和调度逻辑 (`HarnessedTask`)。
      - `spawned.rs`: 定义 `SpawnedTask`，表示待绑定的初始任务状态。
      - `join.rs`: 提供 `JoinHandle` (Send, 基于 Arc/Atomic) 和 `LocalJoinHandle` (!Send, 基于 Rc/RefCell)。
    - **Context (`src/runtime/context.rs`)**:
      - `RuntimeContext`: 线程局部上下文。暴露 `spawn` (全局), `spawn_local` (本地), `spawn_to` (定向) 接口，维护 `ExecutorHandle` 和 `BufPool`。
    - **Blocking (`src/runtime/blocking.rs`)**:
      - 全局动态线程池，处理阻塞任务 (`BlockingTask`)。支持闭包 (`Fn`) 和系统阻塞操作 (`SysOp`)。
  - **Driver (`src/io/driver.rs`)**:
    - 平台特定 I/O 的抽象层（Linux 上使用 io_uring，Windows 上使用 IOCP）。
    - **Windows IOCP (`src/io/driver/iocp/`)**:
      - `IocpDriver` (`iocp.rs`): 核心驱动，管理完成端口 (IOCP) 和时间轮。
      - `submit.rs`: 处理 I/O 操作的提交，支持原生 IOCP 操作（如 `ReadFile`, `WSASendTo`）和阻塞任务的分流。
      - `op.rs`: 定义 `IocpOp` 和 VTable，使用 `OVERLAPPED` 结构与内核交互。
      - `ext.rs`: 加载 Winsock 扩展函数指针（如 `ConnectEx`, `AcceptEx`）。
    - **Linux io_uring (`src/io/driver/uring/`)**:
      - `UringDriver` (`uring.rs`): 核心驱动，管理 `io_uring` 实例 (`IoUring`) 和操作注册表。
      - `submit.rs`: 实现了各操作的提交逻辑 (`make_sqe_*`) 和完成回调 (`on_complete_*`)。使用宏 (`impl_lifecycle!`) 简化了代码。
      - `op.rs`: 定义 `UringOp` 和 VTable (`OpVTable`)。使用 `union UringOpPayload` 存储不同操作的负载，并通过 `ManuallyDrop` 管理生命周期，实现了类型擦除和动态分发。
    - **StableSlab (`src/io/driver/stable_slab.rs`)**: 提供地址稳定的内存分配，用于存储 I/O 操作对象，确保异步回调安全。
    - **OpRegistry (`src/io/driver/op_registry.rs`)**: 管理飞行中的 I/O 操作。
    - 关键操作: `submit_op`, `poll_op`, `process_completions`.
  - **Buffers (`src/io/buffer.rs`)**:
    - `BufPool`: 内存池 Trait。
    - `FixedBuf`: 具有稳定地址的缓冲区，用于异步 I/O（io_uring 所需）。
    - `BuddyPool`/`HybridPool`: 具体的分配器实现。**注意**：`BufPool` 现在必须通过 `context::bind_pool` 绑定到线程，否则会导致 IO 错误或 Panic。

## 代码结构 (Code Structure)
- `veloq-wheel/`: 核心时间轮库。
- `veloq-runtime/`: 异步运行时。
  - `src/io/`: I/O 驱动和缓冲区管理。
  - `src/runtime/`: 任务执行、调度逻辑及下层 Context 管理。

## 工具使用说明 (Tool Usage Guidelines)

- **write_to_file**:
    - **绝对路径**：参数 `TargetFile` **必须**使用**绝对路径**。
    - 该工具默认**不会覆盖**现有文件。
    - 如果意图是**更新**或**覆盖**文件内容，必须显式将 `Overwrite` 参数设置为 `true`，否则工具会报错。
    - 修改部分代码时，优先使用 `replace_file_content` 或 `multi_replace_file_content`，仅在需要大面积修改文件或创建新文件时使用 `write_to_file`。

- **replace_file_content** / **multi_replace_file_content**:
    - **绝对路径**：参数 `TargetFile` **必须**使用**绝对路径**。
    - **精确匹配**：`TargetContent` 必须与文件内容**完全一致**，包括空格、缩进和换行符。
    - **上下文唯一性**：务必提供足够的上下文行，确保 `TargetContent` 在目标范围内是**唯一**的。如果匹配到多个实例，工具会报错。
    - **行号一致性**：`StartLine` 和 `EndLine` 必须准确包裹 `TargetContent`。如果 `TargetContent` 跨越了指定的行范围，替换将失败。
    - **验证步骤**：在使用替换工具前，**必须**先使用 `view_file` 读取目标文件的最新内容和行号，严禁凭记忆或旧的 `view_file` 结果进行操作，因为文件行号可能因之前的编辑而改变。
    - **失败处理**：如果工具报错 "target content not found"，请重新 `view_file` 确认内容和行号，检查是否有隐藏字符或缩进差异。
    - **建议**: 在大面积修改文件时使用 `write_to_file`，多部分修改时使用 `multi_replace_file_content`，单部分修改时使用 `replace_file_content`。

- **view_file**:
    - **操作前必读**：执行任何修改操作前，**必须**读取文件的最新状态。严禁依赖历史读取结果。
    - **行号确认**：确保目标行号与当前文件内容一致。

- **run_command**:
    - **后台运行**：对于耗时命令（如 Loom 测试），应合理设置 `WaitMsBeforeAsync`。若命令长时间未返回，请通过 `command_status` 检查进度。
    - **环境隔离**：运行测试时，尽量使用 `--lib` 或具体的 test case 缩小范围，避免无关编译错误干扰。
