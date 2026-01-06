# Net 模块文档

本文档详细介绍了 `veloq-runtime` 的网络模块 (`src/net`)。该模块提供了高性能的 TCP 和 UDP 异步原语。

## 1. 概要 (Overview)

`src/net` 模块提供了类似于标准库 `std::net` 的接口，但完全基于 Veloq 的 Proactor 异步模型构建。它支持：
*   **TCP**: `TcpListener` 和 `TcpStream`，支持高性能的流式传输。
*   **UDP**: `UdpSocket`，支持无连接的数据报传输。

所有网络套接字在创建时（`bind` / `connect`）都会自动绑定到当前线程的 I/O 驱动 (`PlatformDriver`)。

## 2. 理念和思路 (Concepts)

### 2.1 零抽象成本 (Zero Cost Abstraction)
网络组件是非常薄的包装层。它们仅持有：
*   `fd`: 原始文件描述符/句柄。
*   `driver`: 对驱动的弱引用。
这意味着 `TcpStream` 的大小仅相当于两个指针，且方法调用直接转换为 Op 提交，无额外中间层。

### 2.2 平台差异的屏蔽
*   **Linux (io_uring)**: 使用标准 socket API (`connect`, `accept`, `send`, `recv`)。
*   **Windows (IOCP)**: 使用 Winsock 的扩展函数 (`ConnectEx`, `AcceptEx`)。这些扩展函数通常比标准 BSD socket API 性能更高，但 API 签名差异巨大。`src/net` 内部处理了这些差异（例如 `AcceptEx` 需要预分配缓冲区来存放地址信息）。

## 3. 模块内结构 (Internal Structure)

```
src/
├── net.rs           // 模块定义与导出
└── net/
    ├── tcp.rs       // TcpListener, TcpStream
    └── udp.rs       // UdpSocket
```

## 4. 代码详细分析 (Detailed Analysis)

### 4.1 套接字创建与绑定
以 `TcpListener::bind` 为例：
1.  **地址解析**: 使用 `std::net::ToSocketAddrs` 解析地址。
2.  **Socket 创建**: 内部调用 `socket2` 或类似逻辑创建 OS socket。
3.  **驱动关联**: 这一步至关重要。
    ```rust
    let driver = crate::runtime::context::current().driver();
    ```
    它隐式获取了当前任务所在的 Runtime Driver。这意味着**所有的网络对象必须在 Runtime 环境内创建**，否则会 panic 或报错。

### 4.2 `TcpListener::accept`
跨平台差异最显著的地方：
*   **Windows**: 使用 `AcceptEx`。这是一个完全异步的操作，不仅接受连接，还可以通过预读取（Pre-read）数据来减少系统调用。Veloq 的实现目前专注于接受连接，但为了兼容 `AcceptEx`，必须预分配一块内存用于内核写入本地和远程地址。
*   **Linux**: 简单的提交 `Accept` Op。

### 4.3 `UdpSocket::recv_from`
该方法返回 `(usize, SocketAddr)`。
*   内部提交 `RecvFrom` Op。
*   当 Op 完成时，驱动层已将数据写入 `FixedBuf`，并将源地址信息写入 OS 结构体。`recv_from` 负责将这些原始字节转换为 Rust 的 `SocketAddr` 类型。

## 5. 存在的问题和 TODO

1.  **Socket 选项**:
    *   目前缺少设置 `TTL`, `NoDelay`, `KeepAlive`, `ReusePort` 等常用选项的公开 API。用户目前可能需要通过 `std::os::unix::io::AsRawFd` 绕过 Veloq 去设置，这不优雅。
    *   **TODO**: 在 `TcpStream`/`UdpSocket` 上直接暴露配置方法。

2.  **Split API**:
    *   Tokio 提供了 `split()` 方法将 Stream 拆分为 `ReadHalf` 和 `WriteHalf`，允许在不同任务中并发读写。虽然 `TcpStream` 本身可以 clone（因为底层是引用计数或共享句柄），但提供明确的 Split API 更符合用户习惯。

3.  **IPv6 Dual Stack**:
    *   目前的 `bind` 逻辑虽然支持 v4/v6，但对于双栈行为（`IPV6_V6ONLY`）的控制未明确暴露。

## 6. 未来的方向

1.  **Zero-Copy Networking**:
    *   集成 Linux `MSG_ZEROCOPY` 支持。对于大包发送，这能显著减少内核态内存拷贝。

2.  **QUIC 支持**:
    *   基于 `UdpSocket` 构建对 QUIC 协议（如 `quinn`）的深度集成，利用 `io_uring` 的 `sendmsg`/`recvmsg` 批量处理能力 (GSO/GRO) 提升吞吐。
