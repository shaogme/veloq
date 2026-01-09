use veloq_bitset::BitSet;

use super::{
    AlignedMemory, AllocError, AllocResult, BufPool, DeallocParams, FixedBuf,
    NO_REGISTRATION_INDEX, PoolVTable,
};
use std::alloc::{Layout, alloc, dealloc};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

// Alignment requirement for Direct I/O.
// We use 4096 (Page Size) to ensure compatibility with strict Direct I/O requirements.
// This also ensures that the payload length (Capacity) remains a multiple of 4096.
const PAGE_SIZE: usize = 4096;

const SIZE_4K: usize = 4096;
const SIZE_8K: usize = 8192;
const SIZE_16K: usize = 16384;
const SIZE_32K: usize = 32768;
const SIZE_64K: usize = 65536;

// Slab configuration definition
struct SlabConfig {
    block_size: usize,
    count: usize,
}

// Define 5 classes of buffer sizes
// Class 0: 4KB, 1024 count -> 4MB
// Class 1: 8KB, 512 count -> 4MB
// Class 2: 16KB, 128 count -> 2MB
// Class 3: 32KB, 64 count -> 2MB
// Class 4: 64KB, 32 count -> 2MB
// Total memory: 14MB
const SLABS: [SlabConfig; 5] = [
    SlabConfig {
        block_size: SIZE_4K,
        count: 1024,
    },
    SlabConfig {
        block_size: SIZE_8K,
        count: 512,
    },
    SlabConfig {
        block_size: SIZE_16K,
        count: 128,
    },
    SlabConfig {
        block_size: SIZE_32K,
        count: 64,
    },
    SlabConfig {
        block_size: SIZE_64K,
        count: 32,
    },
];

const GLOBAL_ALLOC_CONTEXT: usize = usize::MAX;
const NEXT_NONE: usize = usize::MAX;

struct Slab {
    config: SlabConfig,
    base_offset: usize,
    free_head: usize,
    allocated: BitSet,
    free_count: usize,
}

/// Raw allocation result from HybridAllocator
pub struct RawAlloc {
    pub ptr: NonNull<u8>,
    pub cap: usize,
    pub global_index: u16,
    pub context: usize,
}

/// Core allocator logic, managing slabs and global fallback
/// Independent of BufPool trait for easier testing
pub struct HybridAllocator {
    memory: AlignedMemory,
    total_size: usize,
    slabs: Vec<Slab>,
}

impl Default for HybridAllocator {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

impl HybridAllocator {
    pub fn new() -> Result<Self, AllocError> {
        let mut total_arena_size = 0;
        for config in SLABS.iter() {
            total_arena_size += config.block_size * config.count;
        }

        let memory = AlignedMemory::new(total_arena_size, PAGE_SIZE)?;
        let arena_base_ptr = memory.as_ptr();

        let mut slabs = Vec::with_capacity(SLABS.len());
        let mut current_offset = 0;

        for config in SLABS.iter() {
            // Intrusive list initialization
            let slab_base_ptr = unsafe { arena_base_ptr.add(current_offset) };

            for i in 0..config.count {
                let offset = i * config.block_size;
                let next_idx = if i < config.count - 1 {
                    i + 1
                } else {
                    NEXT_NONE
                };
                unsafe {
                    let ptr = slab_base_ptr.add(offset) as *mut usize;
                    *ptr = next_idx;
                }
            }

            slabs.push(Slab {
                config: SlabConfig {
                    block_size: config.block_size,
                    count: config.count,
                },
                base_offset: current_offset,
                free_head: 0,
                allocated: BitSet::new(config.count),
                free_count: config.count,
            });

            current_offset += config.block_size * config.count;
        }

        Ok(Self {
            memory,
            total_size: total_arena_size,
            slabs,
        })
    }

    /// Allocate memory. `size` is the total size requirement including header.
    pub fn alloc(&mut self, needed_total: usize) -> Option<RawAlloc> {
        // Find best slab request size
        let slab_idx = if needed_total <= SIZE_4K {
            Some(0)
        } else if needed_total <= SIZE_8K {
            Some(1)
        } else if needed_total <= SIZE_16K {
            Some(2)
        } else if needed_total <= SIZE_32K {
            Some(3)
        } else if needed_total <= SIZE_64K {
            Some(4)
        } else {
            None
        };

        if let Some(idx) = slab_idx {
            let slab = &mut self.slabs[idx];

            if slab.config.block_size >= needed_total {
                if slab.free_head != NEXT_NONE {
                    let index = slab.free_head;
                    let block_offset = slab.base_offset + index * slab.config.block_size;
                    let block_ptr = unsafe { self.memory.as_ptr().add(block_offset) };

                    // Read next free index embedded in the block
                    let next_free = unsafe { *(block_ptr as *const usize) };
                    slab.free_head = next_free;
                    slab.free_count -= 1;

                    // Encode slab_idx (0-3) and index (0-512) into usize context
                    let context = (idx << 16) | index;

                    if slab.allocated.set(index).is_err() {
                        return None;
                    }

                    return Some(RawAlloc {
                        ptr: unsafe { NonNull::new_unchecked(block_ptr) },
                        cap: slab.config.block_size,
                        global_index: 0, // All slabs are in the single region 0
                        context,
                    });
                }
            }
            // If full, fall through to global or fail?
            // Currently assuming fall through if slab is full is not implemented in previous version either,
            // (Wait, previously it did not fall through if `best.slab_index()` was some).
            // But if slab is full (free_indices empty), it returns None here.
            // Previous code: if slab valid -> try alloc. if fail -> None?
            // Previous code: `if let Some(index) = slab.free_indices.pop() ... else { // If the "best fit" slab is too small... }`
            // Actually previous code returned None if slab match but full. It did NOT fall back to global alloc unless size > 64K.
            // So I should keep that behavior.
        }

        // Fallback: Global Allocator
        if needed_total > SIZE_64K {
            let layout = Layout::from_size_align(needed_total, PAGE_SIZE).unwrap();
            let block_ptr = unsafe { alloc(layout) };
            if block_ptr.is_null() {
                // Return None instead of panicking on alloc error here to match signature
                return None;
            }
            // Zero init
            unsafe { std::ptr::write_bytes(block_ptr, 0, needed_total) };

            let cap = needed_total;

            return Some(RawAlloc {
                ptr: unsafe { NonNull::new_unchecked(block_ptr) },
                cap,
                global_index: NO_REGISTRATION_INDEX,
                context: GLOBAL_ALLOC_CONTEXT,
            });
        }

        None
    }

    /// Deallocate memory block.
    /// `block_ptr`: pointer to the start of the block (header position).
    /// `cap`: total capacity of the block (needed for global dealloc).
    /// `context`: context from allocation.
    /// # Safety
    /// Caller must ensure block_ptr is valid and matches context
    pub unsafe fn dealloc(
        &mut self,
        block_ptr: NonNull<u8>,
        cap: usize,
        context: usize,
    ) -> Result<(), String> {
        if context == GLOBAL_ALLOC_CONTEXT {
            let layout = Layout::from_size_align(cap, PAGE_SIZE)
                .map_err(|e| format!("Layout error: {}", e))?;
            unsafe { dealloc(block_ptr.as_ptr(), layout) };
            return Ok(());
        }

        let slab_idx = context >> 16;
        let index = context & 0xFFFF;

        if let Some(slab) = self.slabs.get_mut(slab_idx) {
            match slab.allocated.get(index) {
                Ok(true) => {
                    // OK, is allocated
                }
                Ok(false) => {
                    return Err(format!(
                        "Double free detected in HybridAllocator: slab={}, index={}",
                        slab_idx, index
                    ));
                }
                Err(e) => {
                    return Err(format!("BitSet access error: {}", e));
                }
            }

            if let Err(e) = slab.allocated.clear(index) {
                return Err(format!("BitSet clear error: {}", e));
            }

            // Return to free list (push to head)
            let offset = slab.base_offset + index * slab.config.block_size;
            let block_ptr = unsafe { self.memory.as_ptr().add(offset) };
            unsafe {
                *(block_ptr as *mut usize) = slab.free_head;
            }
            slab.free_head = index;
            slab.free_count += 1;

            Ok(())
        } else {
            Err(format!("Invalid slab index: {}", slab_idx))
        }
    }

    #[cfg(test)]
    pub fn count_free(&self, slab_idx: usize) -> usize {
        self.slabs[slab_idx].free_count
    }
}

#[derive(Clone)]
pub struct HybridPool {
    inner: Rc<RefCell<HybridAllocator>>,
}

impl std::fmt::Debug for HybridPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridPool").finish_non_exhaustive()
    }
}

// VTable Shim
static HYBRID_POOL_VTABLE: PoolVTable = PoolVTable {
    dealloc: hybrid_dealloc_shim,
    resolve_region_info: hybrid_resolve_region_info_shim,
};

unsafe fn hybrid_dealloc_shim(pool_data: NonNull<()>, params: DeallocParams) {
    let pool_rc = unsafe { Rc::from_raw(pool_data.as_ptr() as *const RefCell<HybridAllocator>) };

    // Calculate block start and total capacity
    let block_ptr = params.ptr.as_ptr();
    let total_cap = params.cap;

    let mut inner = pool_rc.borrow_mut();
    unsafe {
        if let Err(_e) = inner.dealloc(NonNull::new_unchecked(block_ptr), total_cap, params.context)
        {
            #[cfg(debug_assertions)]
            eprintln!("HybridPool dealloc error: {}", _e);
        }
    }
}

unsafe fn hybrid_resolve_region_info_shim(
    pool_data: NonNull<()>,
    buf: &FixedBuf,
) -> (usize, usize) {
    let raw = pool_data.as_ptr() as *const RefCell<HybridAllocator>;
    let rc = std::mem::ManuallyDrop::new(unsafe { Rc::from_raw(raw) });

    let idx = buf.buf_index();
    if idx == NO_REGISTRATION_INDEX {
        panic!("Accessing region info of global fallback buffer");
    }

    let inner = rc.borrow();
    // For slab allocations, they are all in region 0 (memory)
    // The provided buf.buf_index() should be 0 if it came from alloc.
    // However, if we support multiple pools or something dynamic later, we might need checking.
    // For now, assume single region 0.

    let base = inner.memory.as_ptr() as usize;
    let ptr = buf.as_ptr() as usize;
    (0, ptr - base)
}

impl HybridPool {
    pub fn new() -> Result<Self, AllocError> {
        Ok(Self {
            inner: Rc::new(RefCell::new(HybridAllocator::new()?)),
        })
    }

    pub fn alloc(&self, len: usize) -> Option<FixedBuf> {
        self.alloc_mem_inner(len)
            .map(|(ptr, cap, idx, context)| unsafe {
                let mut buf =
                    FixedBuf::new(ptr, cap, idx, self.pool_data(), self.vtable(), context);
                buf.set_len(len);
                buf
            })
    }

    // Helper to return proper types for FixedBuf or AllocResult
    fn alloc_mem_inner(&self, size: usize) -> Option<(NonNull<u8>, usize, u16, usize)> {
        let mut inner = self.inner.borrow_mut();

        let needed_total = size; // No offset needed

        if let Some(raw) = inner.alloc(needed_total) {
            let block_ptr = raw.ptr.as_ptr();

            // No header writing

            unsafe {
                Some((
                    NonNull::new_unchecked(block_ptr),
                    raw.cap,
                    raw.global_index,
                    raw.context,
                ))
            }
        } else {
            None
        }
    }
}

impl BufPool for HybridPool {
    fn alloc_mem(&self, size: usize) -> AllocResult {
        if let Some((ptr, cap, global_index, context)) = self.alloc_mem_inner(size) {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            }
        } else {
            AllocResult::Failed
        }
    }

    fn get_memory_regions(&self) -> Vec<crate::io::buffer::BufferRegion> {
        let inner = self.inner.borrow();
        vec![crate::io::buffer::BufferRegion {
            ptr: unsafe { NonNull::new_unchecked(inner.memory.as_ptr() as *mut _) },
            len: inner.total_size,
        }]
    }

    fn vtable(&self) -> &'static PoolVTable {
        &HYBRID_POOL_VTABLE
    }

    fn pool_data(&self) -> NonNull<()> {
        unsafe {
            let raw = Rc::into_raw(self.inner.clone());
            NonNull::new_unchecked(raw as *mut ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator_basic() {
        let mut allocator = HybridAllocator::new().unwrap();
        // Check initial free counts
        assert_eq!(allocator.count_free(0), 1024); // 4K slab
        assert_eq!(allocator.count_free(1), 512); // 8K slab
        assert_eq!(allocator.count_free(2), 128); // 16K slab
        assert_eq!(allocator.count_free(3), 64); // 32K slab
        assert_eq!(allocator.count_free(4), 32); // 64K slab

        // Alloc 4K (fits in 4K slab)
        let raw = allocator.alloc(4096).unwrap();
        assert_eq!(raw.cap, SIZE_4K);
        // All slab allocations should be in region 0
        assert_eq!(raw.global_index, 0);
        assert_eq!(allocator.count_free(0), 1023);
        assert_eq!(allocator.count_free(1), 512);

        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context).unwrap() };
        assert_eq!(allocator.count_free(0), 1024); // Restored
    }

    #[test]
    fn test_allocator_small() {
        let mut allocator = HybridAllocator::new().unwrap();
        // Request very small size. 100 bytes
        let raw = allocator.alloc(100).unwrap();
        // best_fit(100) -> Size4K
        assert_eq!(raw.cap, SIZE_4K);
        assert_eq!(raw.global_index, 0);
        assert_eq!(allocator.count_free(0), 1023);
        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context).unwrap() };
    }

    #[test]
    fn test_allocator_large() {
        let mut allocator = HybridAllocator::new().unwrap();
        // Request 1MB
        let raw = allocator.alloc(1024 * 1024).unwrap();
        assert!(raw.cap >= 1024 * 1024);
        assert_eq!(raw.context, GLOBAL_ALLOC_CONTEXT);
        assert_eq!(raw.global_index, NO_REGISTRATION_INDEX);

        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context).unwrap() };
    }

    #[test]
    fn test_hybrid_pool_integration() {
        let pool = HybridPool::new().unwrap();
        let buf = pool.alloc(4096).unwrap();
        // Size4K request implies 4096 block.
        assert!(buf.capacity() >= 4096);
        let idx = buf.buf_index();
        // Should be valid index (0)
        assert_eq!(idx, 0);
        assert_ne!(idx, NO_REGISTRATION_INDEX);

        drop(buf);
    }

    #[test]
    fn test_hybrid_alloc_clamping() {
        let pool = HybridPool::new().unwrap();
        let buf = pool.alloc(4096).unwrap();
        // 4K Block. 0 alignment. Capacity 4096.
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.capacity(), 4096);
    }

    #[test]
    fn test_hybrid_all_sizes() {
        let pool = HybridPool::new().unwrap();

        let sizes = [4096, 8192, 16384, 32768, 65536];

        for size in sizes {
            let buf = pool.alloc(size).unwrap();
            let min_expected = size;
            assert!(buf.capacity() >= min_expected);
            // Verify alignment
            let ptr = buf.as_slice().as_ptr() as usize;
            assert_eq!(ptr % 4096, 0);
        }
    }
    #[test]
    fn test_double_free() {
        let mut allocator = HybridAllocator::new().unwrap();
        let raw = allocator.alloc(4096).unwrap();
        unsafe {
            allocator
                .dealloc(raw.ptr, raw.cap, raw.context)
                .expect("First dealloc should succeed");
            // Double free
            let res = allocator.dealloc(raw.ptr, raw.cap, raw.context);
            assert!(res.is_err());
            let err_msg = res.unwrap_err();
            assert!(err_msg.contains("Double free detected"));
        }
    }
}
