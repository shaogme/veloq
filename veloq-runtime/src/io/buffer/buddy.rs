use super::{AllocResult, BufPool, BufferSize, DeallocParams, FixedBuf};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

// Buddy System Constants
const ARENA_SIZE: usize = 16 * 1024 * 1024; // 16MB Total
const MIN_BLOCK_SIZE: usize = 4096; // 4KB

// Number of orders: 4KB, 8KB, 16KB, 32KB, 64KB, 128KB, 256KB, 512KB, 1MB, 2MB, 4MB, 8MB, 16MB
const NUM_ORDERS: usize = 13;

const TAG_ALLOCATED: u8 = 0x80;
const TAG_ORDER_MASK: u8 = 0x7F;

/// 侵入式双向链表节点，存储在空闲块的头部
#[repr(C)]
struct FreeNode {
    prev: Option<NonNull<FreeNode>>,
    next: Option<NonNull<FreeNode>>,
}

/// 核心分配器逻辑，管理内存块和空闲列表
/// 独立于 BufPool trait，便于单独测试
struct BuddyAllocator {
    // 保持对内存的所有权
    _memory_owner: Vec<u8>,
    // 内存基地址，用于指针计算
    base_ptr: *mut u8,

    // 每个阶数（Order）对应的空闲链表头
    free_heads: [Option<NonNull<FreeNode>>; NUM_ORDERS],

    // 块标签数组，索引为 block_offset / 4096
    // 记录块的 Order 和是否已分配状态
    tags: Vec<u8>,
}

impl BuddyAllocator {
    fn new() -> Self {
        // 分配 Arena 内存
        let mut memory = Vec::with_capacity(ARENA_SIZE);
        memory.resize(ARENA_SIZE, 0);
        let base_ptr = memory.as_mut_ptr();

        let mut free_heads = [None; NUM_ORDERS];

        // 初始化最大的块（Order 12, 16MB）
        let max_order = NUM_ORDERS - 1;

        // SAFETY: 刚刚分配的内存，指针有效且大小足够
        let root_node_ptr = unsafe { NonNull::new_unchecked(base_ptr as *mut FreeNode) };
        unsafe {
            *(base_ptr as *mut FreeNode) = FreeNode {
                prev: None,
                next: None,
            };
        }

        free_heads[max_order] = Some(root_node_ptr);

        let leaf_count = ARENA_SIZE / MIN_BLOCK_SIZE;
        let mut tags = vec![0u8; leaf_count];
        // 标记第一个最大块为空闲
        tags[0] = max_order as u8;

        Self {
            _memory_owner: memory,
            base_ptr,
            free_heads,
            tags,
        }
    }

    /// 根据大小计算需要的阶数 (Order)
    fn get_order(&self, size: usize) -> usize {
        if size <= MIN_BLOCK_SIZE {
            return 0;
        }
        let mut order = 0;
        let mut s = MIN_BLOCK_SIZE;
        while s < size {
            s <<= 1;
            order += 1;
        }
        order
    }

    /// 分配指定大小的内存块
    fn alloc(&mut self, size: usize) -> Option<(NonNull<u8>, usize)> {
        if size > ARENA_SIZE {
            return None;
        }

        let needed_order = self.get_order(size);
        if needed_order >= NUM_ORDERS {
            return None;
        }

        // 寻找合适的空闲块
        for order in needed_order..NUM_ORDERS {
            if let Some(node_ptr) = self.free_heads[order] {
                // 1. 从链表移除空闲块
                // SAFETY: node_ptr 来源于 free_heads，保证有效且属于该 order
                unsafe { self.pop_front(order, node_ptr) };

                let curr_ptr = node_ptr.as_ptr() as *mut u8;
                let mut curr_order = order;
                let base_ptr = self.base_ptr;

                // 2. 迭代分裂直到达到所需大小
                while curr_order > needed_order {
                    curr_order -= 1;
                    let block_size = MIN_BLOCK_SIZE << curr_order;

                    // Buddy 是高地址的那一半
                    // SAFETY: 向下分裂时，block_size 必定在当前块范围内
                    let buddy_ptr = unsafe { curr_ptr.add(block_size) };

                    // 将 Buddy 初始化为 FreeNode 并加入对应的空闲链表
                    // SAFETY: buddy_ptr 指向有效的未使用内存
                    unsafe {
                        self.push_front(
                            curr_order,
                            NonNull::new_unchecked(buddy_ptr as *mut FreeNode),
                        )
                    };

                    // 更新 Buddy 的 Tag
                    // SAFETY: 都在 Arena 范围内
                    let buddy_offset = unsafe { buddy_ptr.offset_from(base_ptr) } as usize;
                    let buddy_idx = buddy_offset / MIN_BLOCK_SIZE;
                    self.tags[buddy_idx] = curr_order as u8;
                }

                // 3. 标记分配出的块
                // SAFETY: 指针在 Arena 范围内
                let offset = unsafe { curr_ptr.offset_from(base_ptr) } as usize;
                let idx = offset / MIN_BLOCK_SIZE;
                self.tags[idx] = (needed_order as u8) | TAG_ALLOCATED;

                return Some((
                    // SAFETY: curr_ptr 是有效的非空指针
                    unsafe { NonNull::new_unchecked(curr_ptr) },
                    MIN_BLOCK_SIZE << needed_order,
                ));
            }
        }
        None
    }

    /// 释放内存块
    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, cap: usize) {
        let base_ptr = self.base_ptr;
        let ptr_raw = ptr.as_ptr();
        // SAFETY: 假定 ptr 是由 alloc 返回的，在 Arena 范围内
        let offset = unsafe { ptr_raw.offset_from(base_ptr) } as usize;

        let mut curr_offset = offset;
        let mut curr_order = self.get_order(cap);

        // 立即标记为空闲
        let idx = curr_offset / MIN_BLOCK_SIZE;
        self.tags[idx] = curr_order as u8;

        // 尝试向上合并
        while curr_order < NUM_ORDERS - 1 {
            let block_size = MIN_BLOCK_SIZE << curr_order;
            let buddy_offset = curr_offset ^ block_size;

            if buddy_offset >= ARENA_SIZE {
                break;
            }

            let buddy_idx = buddy_offset / MIN_BLOCK_SIZE;
            let buddy_tag = self.tags[buddy_idx];

            // 检查 Buddy 是否空闲且 Order 一致
            if (buddy_tag & TAG_ALLOCATED) == 0 && (buddy_tag & TAG_ORDER_MASK) == curr_order as u8
            {
                // 合并 Buddy
                // SAFETY: buddy_offset 经过检查在 Arena 范围内
                let buddy_ptr = unsafe { base_ptr.add(buddy_offset) } as *mut FreeNode;
                // SAFETY: buddy_ptr 非空
                let buddy_non_null = unsafe { NonNull::new_unchecked(buddy_ptr) };

                // 从空闲链表中移除 Buddy
                // SAFETY: buddy 是空闲块，必定在链表中
                unsafe { self.remove(curr_order, buddy_non_null) };

                // 更新为合并后的大块
                curr_offset = std::cmp::min(curr_offset, buddy_offset);
                curr_order += 1;

                // 更新新块的 Tag
                let new_idx = curr_offset / MIN_BLOCK_SIZE;
                self.tags[new_idx] = curr_order as u8;
            } else {
                break;
            }
        }

        // 将最终的空闲块加入链表
        // SAFETY: curr_offset 始终在 Arena 范围内
        let final_ptr = unsafe { base_ptr.add(curr_offset) } as *mut FreeNode;
        // SAFETY: final_ptr 有效
        unsafe { self.push_front(curr_order, NonNull::new_unchecked(final_ptr)) };
    }

    // --- 链表操作辅助函数 ---

    unsafe fn push_front(&mut self, order: usize, mut node_ptr: NonNull<FreeNode>) {
        let head = self.free_heads[order];
        let node = unsafe { node_ptr.as_mut() };

        node.next = head;
        node.prev = None;

        if let Some(mut head_ptr) = head {
            unsafe { head_ptr.as_mut() }.prev = Some(node_ptr);
        }

        self.free_heads[order] = Some(node_ptr);
    }

    unsafe fn pop_front(&mut self, order: usize, mut node_ptr: NonNull<FreeNode>) {
        let node = unsafe { node_ptr.as_mut() };
        let next = node.next;

        self.free_heads[order] = next;

        if let Some(mut next_ptr) = next {
            unsafe { next_ptr.as_mut() }.prev = None;
        }

        // 清理指针是个好习惯
        node.next = None;
        node.prev = None;
    }

    unsafe fn remove(&mut self, order: usize, mut node_ptr: NonNull<FreeNode>) {
        let node = unsafe { node_ptr.as_mut() };
        let prev = node.prev;
        let next = node.next;

        if let Some(mut prev_ptr) = prev {
            unsafe { prev_ptr.as_mut() }.next = next;
        } else {
            // 是头节点
            self.free_heads[order] = next;
        }

        if let Some(mut next_ptr) = next {
            unsafe { next_ptr.as_mut() }.prev = prev;
        }

        node.prev = None;
        node.next = None;
    }

    // --- 测试辅助函数 ---
    #[cfg(test)]
    fn count_free(&self, order: usize) -> usize {
        let mut count = 0;
        let mut curr = self.free_heads[order];
        unsafe {
            while let Some(node) = curr {
                count += 1;
                curr = node.as_ref().next;
            }
        }
        count
    }
}

/// 各种 BufferPool 实现的包装器
#[derive(Clone)]
pub struct BuddyPool {
    inner: Rc<RefCell<BuddyAllocator>>,
}

impl std::fmt::Debug for BuddyPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuddyPool").finish_non_exhaustive()
    }
}

impl BuddyPool {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(BuddyAllocator::new())),
        }
    }

    pub fn alloc(&self, size: BufferSize) -> Option<FixedBuf<BuddyPool>> {
        let size_bytes = size.size();
        let mut inner = self.inner.borrow_mut();

        if let Some((ptr, cap)) = inner.alloc(size_bytes) {
            // io_uring 注册时，我们通常注册整个 Arena (例如作为 index 0)，
            // 读写操作使用 (buf_index=0, addr=ptr, len)。
            // 因此 global_index 始终为 0 (假定外部机制注册了 handle)。
            return Some(FixedBuf::new(self.clone(), ptr, cap, 0, 0));
        }
        None
    }
}

impl BufPool for BuddyPool {
    fn new() -> Self {
        BuddyPool::new()
    }

    fn alloc_mem(&self, size: usize) -> AllocResult {
        let mut inner = self.inner.borrow_mut();
        match inner.alloc(size) {
            Some((ptr, cap)) => AllocResult::Allocated {
                ptr,
                cap,
                global_index: 0,
                context: 0,
            },
            None => AllocResult::Failed,
        }
    }

    unsafe fn dealloc_mem(&self, params: DeallocParams) {
        let mut inner = self.inner.borrow_mut();
        unsafe { inner.dealloc(params.ptr, params.cap) };
    }

    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec> {
        let inner = self.inner.borrow();
        vec![libc::iovec {
            iov_base: inner.base_ptr as *mut _,
            iov_len: ARENA_SIZE,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_basic() {
        let mut allocator = BuddyAllocator::new();
        // 初始状态：1个 MaxOrder 块
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 1);

        // 分配 4KB
        let (ptr1, size1) = allocator.alloc(4096).unwrap();
        assert_eq!(size1, 4096);

        // 分裂路径验证
        // 16M -> ... -> 8K -> 4K(Allocated) + 4K(Free)
        // 所有的中间级 (8K, 16K ... 8M) 都应该各有一个 Free 块
        assert_eq!(allocator.count_free(0), 1); // 剩下一个 4K
        assert_eq!(allocator.count_free(1), 1); // 剩下一个 8K
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 0); // MaxOrder 没了

        unsafe { allocator.dealloc(ptr1, size1) };

        // 释放后应完全合并
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 1);
        assert_eq!(allocator.count_free(0), 0);
    }

    #[test]
    fn test_alloc_merge_specific() {
        let mut allocator = BuddyAllocator::new();

        // 分配两个 4KB
        let (ptr1, _) = allocator.alloc(4096).unwrap();
        let (ptr2, _) = allocator.alloc(4096).unwrap();

        assert_eq!(allocator.count_free(0), 0); // 两个 4K 都被用了
        assert_eq!(allocator.count_free(1), 1); // 8K 还在

        // 释放第一个
        unsafe { allocator.dealloc(ptr1, 4096) };
        assert_eq!(allocator.count_free(0), 1); // 有一个 4K 它是空闲的，但无法合并（buddy 占用）

        // 释放第二个 -> 触发合并
        unsafe { allocator.dealloc(ptr2, 4096) };
        assert_eq!(allocator.count_free(0), 0); // 4K 没了，合并成 8K，然后一路向上
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 1);
    }

    #[test]
    fn test_oom() {
        let mut allocator = BuddyAllocator::new();
        let mut ptrs = Vec::new();

        // 耗尽 Arena：分配 16MB 的内存 (4096 个 4KB)
        // 为加速测试，直接分配 Order 12
        let (ptr, _) = allocator.alloc(ARENA_SIZE).unwrap();
        ptrs.push(ptr);

        assert!(allocator.alloc(4096).is_none());

        unsafe { allocator.dealloc(ptrs.pop().unwrap(), ARENA_SIZE) };
        assert!(allocator.alloc(4096).is_some());
    }

    #[test]
    fn test_random_pattern() {
        // 模拟更复杂的交叉释放场景
        let mut allocator = BuddyAllocator::new();

        // Alloc 16K
        let (p1, s1) = allocator.alloc(16384).unwrap();
        // Alloc 4K
        let (p2, s2) = allocator.alloc(4096).unwrap();
        // Alloc 4K
        let (p3, s3) = allocator.alloc(4096).unwrap();

        // Free 16K (Order 2)
        unsafe { allocator.dealloc(p1, s1) };
        // 现在有 Order 2 空闲。Order 0 两个占用。

        // Free 4K (p3)
        unsafe { allocator.dealloc(p3, s3) };
        // Free 4K (p2) -> Merge with p3 -> Order 1.
        // Order 1 Merge with ? (Depends on implementation details of split, but eventually should clean up)
        unsafe { allocator.dealloc(p2, s2) };

        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 1);
    }

    #[test]
    fn test_pool_integration() {
        let pool = BuddyPool::new();
        let buf = pool.alloc(BufferSize::Size4K).unwrap();
        assert_eq!(buf.capacity(), 4096);
        drop(buf);
        // Ensure no panic on drop
    }
}
