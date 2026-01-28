# veloq-intrusive-linklist

`veloq-intrusive-linklist` 是一个高性能、类型安全的 Rust **侵入式链表 (Intrusive Linked List)** 库。

与标准库中的 `Vec` 或 `LinkedList` 不同，侵入式链表将“链接”本身嵌入到数据结构中。这意味着节点本身负责存储其在链表中的连接状态，从而避免了额外的内存分配，并允许在已知节点引用的情况下以 O(1) 的时间复杂度将节点从链表中移除。

本项目旨在为构建高性能并发原语（如任务调度器、等待队列等）提供基础数据结构支持。

## ✨ 核心特性

- **零内存分配 (Zero Allocation)**：节点链接字段嵌入在结构体中，所有链表操作仅涉及指针操作，无需任何堆内存分配。
- **高性能 (High Performance)**：极大减少了缓存未命中（Cache Miss）和内存碎片。
- **游标支持 (Cursor API)**：提供 `Cursor` 和 `CursorMut`，支持双向遍历、原地插入和删除操作。
- **安全性检查 (Safety Checks)**：
  - 运行时检查防止节点重复插入或在未移除时被 Drop（Panic 保护）。
  - 使用 `Pin` 保证节点在链表中的内存地址稳定性。
- **宏辅助 (Macro Support)**：提供 `intrusive_adapter!` 宏，一键生成结构体与链接字段的映射适配器。
- **Loom 集成 (Loom Integration)**：可选集成 `loom`，用于并发模型检测（通过 `loom` feature 开启）。

## 🚀 快速开始

### 1. 定义节点结构体

首先，你需要在你的结构体中包含一个 `Link` 字段。

```rust
use veloq_intrusive_linklist::{Link, intrusive_adapter};

pub struct MyNode {
    pub data: i32,
    // 侵入式链接字段
    link: Link,
}

// 定义适配器，告诉链表如何从 MyNode 找到 link 字段
intrusive_adapter!(pub MyAdapter = MyNode { link: Link });
```

### 2. 基本使用

```rust
use veloq_intrusive_linklist::{LinkedList, Link};
use std::boxed::Box;
use std::pin::Pin;

fn main() {
    // 创建一个使用 MyAdapter 的链表
    let mut list = LinkedList::new(MyAdapter);

    // 节点通常需要固定在内存中 (Pinned)，因为链表依赖节点的稳定地址
    // 使用 Box::pin 是最简单的方式
    let mut node1 = Box::pin(MyNode { data: 10, link: Link::new() });
    let mut node2 = Box::pin(MyNode { data: 20, link: Link::new() });

    unsafe {
        // 将节点加入链表尾部
        // SAFETY: 我们必须保证 node1 和 node2 在链表中时依然有效
        list.push_back(node1.as_mut());
        list.push_back(node2.as_mut());
    }

    assert_eq!(list.len(), 2);

    // 弹出头部节点
    if let Some(mut popped_node) = list.pop_front() {
        println!("Popped: {}", popped_node.data); // Output: 10
    }

    assert_eq!(list.len(), 1);
}
```

## 📖 详细文档

### Cursor (游标)

`Cursor` 允许你在遍历链表的同时进行修改（如移除当前节点）。

```rust
let mut cursor = list.front_mut();

while let Some(node) = cursor.get() {
    if node.data == 20 {
        // 找到目标节点，将其从链表中移除
        // 此操作是 O(1) 的，并且不会销毁节点本身
        let removed_node = cursor.remove();
        println!("Removed: {}", removed_node.unwrap().data);
        // remove 后 cursor 会自动指向下一个节点
    } else {
        cursor.move_next();
    }
}
```

### Scoped Push (作用域插入)

有时你只需要在一个代码块范围内将节点放入链表，并在退出作用域时自动移除。`push_back_scoped` 提供了一个 RAII 风格的 Guard。

```rust
{
    let mut node = Box::pin(MyNode { data: 99, link: Link::new() });
    
    // 返回一个 RemoveOnDrop Guard
    let _guard = unsafe { list.push_back_scoped(node.as_mut()) };
    
    assert_eq!(list.len(), 1);
    
    // 当 _guard 离开作用域时，node 会自动从链表中移除
}
assert!(list.is_empty());
```

## ⚠️ 安全性说明 (Safety)

侵入式链表本质上大量涉及 `unsafe` 代码，因为它们绕过了 Rust 的所有权模型（链表不拥有节点，只是引用它们）。

1.  **Pinning**: 必须使用 `Pin<&mut T>` 传递节点。这是因为一旦节点被插入链表，其内存地址就不能改变（其他节点的 `next/prev` 指针指向它）。
2.  **生命周期**: 用户必须确保节点在链表中的存活时间。如果在节点被 Drop 之前没有从链表中移除，`Link` 的 Drop 实现会触发 Panic（以防止悬垂指针导致的更严重的不安全行为）。
3.  **线程安全**: `LinkedList<A>` 实现了 `Send` 和 `Sync`（前提是 `A::Value` 也是 `Send/Sync`），但在这个库层面上，它是一个非并发的数据结构。如果在多线程环境中使用，需要外部加锁（如 `Mutex<LinkedList<...>>`）。

## 🛠️ 宏与工具

### `intrusive_adapter!`

该宏用于生成 `Adapter` trait 的实现。

```rust
// 语法: Visibility AdapterName = StructName { LinkFieldName: Link }
intrusive_adapter!(pub TaskAdapter = Task { task_link: Link });
```

它会自动处理指针偏移计算，确保 `get_link` 和 `get_value` 能够正确转换。