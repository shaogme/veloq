use crate::io::buffer::BufPoolExt;

use super::{
    AlignedMemory, AllocResult, BufPool, BufferHeader, DeallocParams, FixedBuf, PoolVTable,
};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

// Buddy System Constants
const ARENA_SIZE: usize = 32 * 1024 * 1024; // 32MB Total to support higher concurrency with overhead
const MIN_BLOCK_SIZE: usize = 8192; // 8KB to support 4KB payload with 4KB alignment

// Alignment requirement for Direct I/O.
// We use 4096 (Page Size) to ensure compatibility with strict Direct I/O requirements.
// This also ensures that the payload length (Capacity) remains a multiple of 4096.
const ALIGNMENT: usize = 4096;

// Number of orders: 4KB, 8KB, 16KB, 32KB, 64KB, 128KB, 256KB, 512KB, 1MB, 2MB, 4MB, 8MB, 16MB
const NUM_ORDERS: usize = 13;

const TAG_ALLOCATED: u8 = 0x80;
const TAG_ORDER_MASK: u8 = 0x7F;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BufferSize {
    /// 8KB (Order 0) -> 4KB Payload
    Size8K = 0,
    /// 16KB (Order 1)
    Size16K = 1,
    /// 32KB (Order 2)
    Size32K = 2,
    /// 64KB (Order 3)
    Size64K = 3,
    /// 128KB (Order 4)
    Size128K = 4,
    /// 256KB (Order 5)
    Size256K = 5,
    /// 512KB (Order 6)
    Size512K = 6,
    /// 1MB (Order 7)
    Size1M = 7,
    /// 2MB (Order 8)
    Size2M = 8,
    /// 4MB (Order 9)
    Size4M = 9,
    /// 8MB (Order 10)
    Size8M = 10,
    /// 16MB (Order 11)
    Size16M = 11,
    /// 32MB (Order 12)
    Size32M = 12,
}

impl BufferSize {
    #[inline(always)]
    pub fn size(&self) -> usize {
        MIN_BLOCK_SIZE << (*self as usize)
    }

    #[inline(always)]
    pub fn order(&self) -> usize {
        *self as usize
    }

    pub fn from_order(order: usize) -> Option<Self> {
        match order {
            0 => Some(BufferSize::Size8K),
            1 => Some(BufferSize::Size16K),
            2 => Some(BufferSize::Size32K),
            3 => Some(BufferSize::Size64K),
            4 => Some(BufferSize::Size128K),
            5 => Some(BufferSize::Size256K),
            6 => Some(BufferSize::Size512K),
            7 => Some(BufferSize::Size1M),
            8 => Some(BufferSize::Size2M),
            9 => Some(BufferSize::Size4M),
            10 => Some(BufferSize::Size8M),
            11 => Some(BufferSize::Size16M),
            12 => Some(BufferSize::Size32M),
            _ => None,
        }
    }

    pub fn best_fit(size: usize) -> Option<Self> {
        if size > ARENA_SIZE {
            return None;
        }
        if size <= MIN_BLOCK_SIZE {
            return Some(BufferSize::Size8K);
        }

        let mut s = MIN_BLOCK_SIZE;
        let mut order = 0;
        while s < size {
            s <<= 1;
            order += 1;
        }

        Self::from_order(order)
    }
}

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
    _memory_owner: AlignedMemory,
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
        let memory = AlignedMemory::new(ARENA_SIZE, 4096);
        let base_ptr = memory.as_ptr();

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

    /// 分配指定大小的内存块
    fn alloc(&mut self, size: BufferSize) -> Option<(NonNull<u8>, BufferSize)> {
        let needed_order = size.order();

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
                    size,
                ));
            }
        }
        None
    }

    /// 释放内存块
    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, size: BufferSize) {
        let base_ptr = self.base_ptr;
        let ptr_raw = ptr.as_ptr();
        // SAFETY: 假定 ptr 是由 alloc 返回的，在 Arena 范围内
        let offset = unsafe { ptr_raw.offset_from(base_ptr) } as usize;

        let mut curr_offset = offset;
        let mut curr_order = size.order();

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

// Static VTable for Type Erasure
static BUDDY_POOL_VTABLE: PoolVTable = PoolVTable {
    dealloc: buddy_dealloc_shim,
};

unsafe fn buddy_dealloc_shim(pool_data: NonNull<()>, params: DeallocParams) {
    // 1. Recover the Pool Rc
    let pool_rc = unsafe { Rc::from_raw(pool_data.as_ptr() as *const RefCell<BuddyAllocator>) };
    // Rc is now alive again (and will drop at end of scope, decrementing validity)

    // 2. Adjust params to find original block start
    // In alloc: ptr = block_start + ALIGNMENT
    // So block_start = ptr - ALIGNMENT
    let offset = ALIGNMENT;
    let block_start_ptr = unsafe { params.ptr.as_ptr().sub(offset) };
    let block_start_non_null = unsafe { NonNull::new_unchecked(block_start_ptr) };

    // 3. Recover size from context (Buddy uses Order in context)
    let order = params.context;
    let size = match BufferSize::from_order(order) {
        Some(s) => s,
        None => {
            #[cfg(debug_assertions)]
            panic!("Invalid order in dealloc: {}", order);
            #[cfg(not(debug_assertions))]
            return;
        }
    };

    // 4. Perform dealloc
    let mut inner = pool_rc.borrow_mut();
    unsafe { inner.dealloc(block_start_non_null, size) };
}

impl BuddyPool {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(BuddyAllocator::new())),
        }
    }

    pub fn alloc(&self, size: BufferSize) -> Option<FixedBuf> {
        let mut inner = self.inner.borrow_mut();

        if let Some((block_ptr, actual_size)) = inner.alloc(size) {
            let offset = ALIGNMENT;
            let capacity = actual_size.size();

            // Check if capacity is enough for alignment overhead
            if capacity <= offset {
                unsafe { inner.dealloc(block_ptr, actual_size) };
                return None;
            }

            // Write header
            unsafe {
                let payload_ptr = block_ptr.as_ptr().add(offset);
                // FixedBuf expects header at payload_ptr - sizeof(BufferHeader)
                let header_ptr = (payload_ptr as *mut BufferHeader).offset(-1);

                // Increment Rc strong count for the FixedBuf lifecycle
                let pool_raw = Rc::into_raw(self.inner.clone());

                (*header_ptr) = BufferHeader {
                    vtable: &BUDDY_POOL_VTABLE,
                    pool_data: NonNull::new_unchecked(pool_raw as *mut ()),
                    context: actual_size.order(), // Save order to restore size
                };

                // Setup FixedBuf
                // Payload starts at offset, length is capacity - offset
                // This guarantees:
                // 1. Payload address is (base + offset) -> aligned if base is aligned
                // 2. Payload length is (BlockSize - offset) -> aligned if BlockSize and offset are aligned
                let payload_len = capacity - offset;

                return Some(FixedBuf::new(
                    NonNull::new_unchecked(payload_ptr),
                    payload_len,
                    0, // Global index is 0
                ));
            }
        }
        None
    }

    pub fn alloc_len(&self, len: usize) -> Option<FixedBuf> {
        let offset = ALIGNMENT;
        let needed = len.checked_add(offset)?;
        let size = BufferSize::best_fit(needed)?;

        self.alloc(size).map(|mut buf| {
            buf.set_len(len);
            buf
        })
    }
}

impl Default for BuddyPool {
    fn default() -> Self {
        Self::new()
    }
}

impl BufPool for BuddyPool {
    fn alloc_mem(&self, size: usize) -> AllocResult {
        // Warning: This method is low-level. If used by FixedBuf, it expects BufferHeader.
        // But AllocResult uses raw ptr.
        // We inject header for alloc_mem too, assuming it's for FixedBuf construction or compatible usages.

        let mut inner = self.inner.borrow_mut();

        let buf_size = match BufferSize::best_fit(size) {
            Some(s) => s,
            None => return AllocResult::Failed,
        };

        match inner.alloc(buf_size) {
            Some((block_ptr, actual_size)) => {
                let offset = ALIGNMENT;
                let capacity = actual_size.size();

                if capacity <= offset {
                    unsafe { inner.dealloc(block_ptr, actual_size) };
                    return AllocResult::Failed;
                }

                unsafe {
                    // Start payload at aligned offset
                    let payload_ptr = block_ptr.as_ptr().add(offset);
                    // Header is just before payload
                    let header_ptr = (payload_ptr as *mut BufferHeader).offset(-1);

                    let pool_raw = Rc::into_raw(self.inner.clone());

                    (*header_ptr) = BufferHeader {
                        vtable: &BUDDY_POOL_VTABLE,
                        pool_data: NonNull::new_unchecked(pool_raw as *mut ()),
                        context: actual_size.order(),
                    };

                    let payload_len = capacity - offset;

                    AllocResult::Allocated {
                        ptr: NonNull::new_unchecked(payload_ptr),
                        cap: payload_len,
                        global_index: 0,
                        context: actual_size.order(),
                    }
                }
            }
            None => AllocResult::Failed,
        }
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

impl BufPoolExt for BuddyPool {
    type BufferSize = BufferSize;

    #[inline(always)]
    fn alloc(&self, size: Self::BufferSize) -> Option<FixedBuf> {
        self.alloc(size)
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

        // 分配 8KB (Order 0)
        let (ptr1, size1) = allocator.alloc(BufferSize::Size8K).unwrap();
        assert_eq!(size1, BufferSize::Size8K);

        // 分裂路径验证
        // 16M -> ... -> 16K -> 8K(Allocated) + 8K(Free)
        // 所有的中间级 (16K ... 16M) 都应该各有一个 Free 块
        assert_eq!(allocator.count_free(0), 1); // 剩下一个 8K
        assert_eq!(allocator.count_free(1), 1); // 剩下一个 16K
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 0); // MaxOrder 没了

        unsafe { allocator.dealloc(ptr1, size1) };

        // 释放后应完全合并
        assert_eq!(allocator.count_free(NUM_ORDERS - 1), 1);
        assert_eq!(allocator.count_free(0), 0);
    }

    #[test]
    fn test_pool_integration() {
        let pool = BuddyPool::new();
        // With 4096 alignment, a 8K block has 4096 capacity.
        // Minimal usable block is 8K.
        let buf = pool.alloc(BufferSize::Size8K).unwrap();
        // Capacity should be 8192 - 4096 = 4096
        assert_eq!(buf.capacity(), 8192 - ALIGNMENT);
        drop(buf);
        // Ensure no panic on drop and proper rc cleanup
    }
}
