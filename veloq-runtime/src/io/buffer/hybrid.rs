use crate::io::buffer::BufPoolExt;

use super::{
    AlignedMemory, AllocResult, BufPool, BufferHeader, DeallocParams, FixedBuf,
    NO_REGISTRATION_INDEX, PoolVTable,
};
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

// Alignment requirement for Direct I/O.
// We use 4096 (Page Size) to ensure compatibility with strict Direct I/O requirements.
// This also ensures that the payload length (Capacity) remains a multiple of 4096.
const ALIGNMENT: usize = 4096;
const PAGE_SIZE: usize = 4096;

const SIZE_8K: usize = 8192;
const SIZE_16K: usize = 16384;
const SIZE_32K: usize = 32768;
const SIZE_64K: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BufferSize {
    /// 8KB (4KB Payload with 4KB Alignment)
    Size8K,
    /// 16KB
    Size16K,
    /// 32KB
    Size32K,
    /// 64KB
    Size64K,
    /// Custom size
    Custom(usize),
}

impl BufferSize {
    #[inline(always)]
    pub fn size(&self) -> usize {
        match self {
            BufferSize::Size8K => SIZE_8K - ALIGNMENT,
            BufferSize::Size16K => SIZE_16K - ALIGNMENT,
            BufferSize::Size32K => SIZE_32K - ALIGNMENT,
            BufferSize::Size64K => SIZE_64K - ALIGNMENT,
            BufferSize::Custom(size) => *size,
        }
    }

    #[inline(always)]
    pub fn slab_index(&self) -> Option<usize> {
        match self {
            BufferSize::Size8K => Some(0),
            BufferSize::Size16K => Some(1),
            BufferSize::Size32K => Some(2),
            BufferSize::Size64K => Some(3),
            BufferSize::Custom(_) => None,
        }
    }

    pub fn best_fit(size: usize) -> Self {
        if size <= SIZE_8K {
            BufferSize::Size8K
        } else if size <= SIZE_16K {
            BufferSize::Size16K
        } else if size <= SIZE_32K {
            BufferSize::Size32K
        } else if size <= SIZE_64K {
            BufferSize::Size64K
        } else {
            BufferSize::Custom(size)
        }
    }
}

// Slab configuration definition
struct SlabConfig {
    block_size: usize,
    count: usize,
}

// Define 4 classes of buffer sizes
// Class 0: 8KB, 512 count -> 4MB
// Class 1: 16KB, 128 count -> 2MB
// Class 2: 32KB, 64 count -> 2MB
// Class 3: 64KB, 32 count -> 2MB
// Total memory: 10MB
const SLABS: [SlabConfig; 4] = [
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

struct Slab {
    config: SlabConfig,
    memory: AlignedMemory,
    free_indices: Vec<usize>,
    global_index_offset: u16,
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
    slabs: Vec<Slab>,
}

impl Default for HybridAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl HybridAllocator {
    pub fn new() -> Self {
        let mut slabs = Vec::with_capacity(SLABS.len());
        let mut current_global_offset = 0;

        for config in SLABS.iter() {
            let total_size = config.block_size * config.count;
            let memory = AlignedMemory::new(total_size, PAGE_SIZE);

            // Free indices stack: initially all indices 0..count
            let free_indices: Vec<usize> = (0..config.count).collect();

            slabs.push(Slab {
                config: SlabConfig {
                    block_size: config.block_size,
                    count: config.count,
                },
                memory,
                free_indices,
                global_index_offset: current_global_offset,
            });

            current_global_offset += config.count as u16;
        }

        Self { slabs }
    }

    /// Allocate memory. `size` is the total size requirement including header.
    pub fn alloc(&mut self, needed_total: usize) -> Option<RawAlloc> {
        // Try to find best slab request size
        let best = BufferSize::best_fit(needed_total);

        if let Some(slab_idx) = best.slab_index() {
            let slab = &mut self.slabs[slab_idx];

            // Ensure the slab block size is actually sufficient
            // best_fit returns Size8K for <= 8192.
            // If needed_total is 4136, it fits.
            // If needed_total is 4096, it fits.
            // If needed_total is 4100, best_fit returns Size16K. Fits.
            // So this check is mostly sanity.
            if slab.config.block_size >= needed_total {
                if let Some(index) = slab.free_indices.pop() {
                    let block_ptr =
                        unsafe { slab.memory.as_ptr().add(index * slab.config.block_size) };

                    // Encode slab_idx (0-3) and index (0-512) into usize context
                    let context = (slab_idx << 16) | index;

                    return Some(RawAlloc {
                        ptr: unsafe { NonNull::new_unchecked(block_ptr) },
                        cap: slab.config.block_size,
                        global_index: slab.global_index_offset + index as u16,
                        context,
                    });
                }
            } else {
                // If the "best fit" slab is too small (should not happen with correct best_fit), fall through
            }
        }

        // Fallback: Global Allocator
        if needed_total > SIZE_64K {
            let layout = Layout::from_size_align(needed_total, PAGE_SIZE).unwrap();
            let block_ptr = unsafe { alloc(layout) };
            if block_ptr.is_null() {
                handle_alloc_error(layout);
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
    pub unsafe fn dealloc(&mut self, block_ptr: NonNull<u8>, cap: usize, context: usize) {
        if context == GLOBAL_ALLOC_CONTEXT {
            let layout = Layout::from_size_align(cap, PAGE_SIZE).unwrap();
            unsafe { dealloc(block_ptr.as_ptr(), layout) };
            return;
        }

        let slab_idx = context >> 16;
        let index = context & 0xFFFF;

        if let Some(slab) = self.slabs.get_mut(slab_idx) {
            slab.free_indices.push(index);
        }
    }

    #[cfg(test)]
    pub fn count_free(&self, slab_idx: usize) -> usize {
        self.slabs[slab_idx].free_indices.len()
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
};

unsafe fn hybrid_dealloc_shim(pool_data: NonNull<()>, params: DeallocParams) {
    let pool_rc = unsafe { Rc::from_raw(pool_data.as_ptr() as *const RefCell<HybridAllocator>) };

    // Calculate block start and total capacity
    let offset = ALIGNMENT;
    let block_ptr = unsafe { params.ptr.as_ptr().sub(offset) };
    let total_cap = params.cap + offset;

    let mut inner = pool_rc.borrow_mut();
    unsafe {
        inner.dealloc(NonNull::new_unchecked(block_ptr), total_cap, params.context);
    }
}

impl HybridPool {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(HybridAllocator::new())),
        }
    }

    pub fn alloc(&self, size: BufferSize) -> Option<FixedBuf> {
        self.alloc_mem_inner(size.size())
            .map(|(ptr, cap, idx)| unsafe {
                let mut buf = FixedBuf::new(ptr, cap, idx);
                if buf.capacity() > size.size() {
                    buf.set_len(size.size());
                }
                buf
            })
    }

    pub fn alloc_len(&self, len: usize) -> Option<FixedBuf> {
        self.alloc_mem_inner(len).map(|(ptr, cap, idx)| unsafe {
            let mut buf = FixedBuf::new(ptr, cap, idx);
            buf.set_len(len);
            buf
        })
    }

    // Helper to return proper types for FixedBuf or AllocResult
    fn alloc_mem_inner(&self, size: usize) -> Option<(NonNull<u8>, usize, u16)> {
        let mut inner = self.inner.borrow_mut();
        let offset = ALIGNMENT;
        let needed_total = size + offset;

        if let Some(raw) = inner.alloc(needed_total) {
            let block_ptr = raw.ptr.as_ptr();
            let pool_raw = Rc::into_raw(self.inner.clone());

            unsafe {
                let payload_ptr = block_ptr.add(offset);
                let header_ptr = (payload_ptr as *mut BufferHeader).offset(-1);

                (*header_ptr) = BufferHeader {
                    vtable: &HYBRID_POOL_VTABLE,
                    pool_data: NonNull::new_unchecked(pool_raw as *mut ()),
                    context: raw.context,
                };

                let payload_cap = raw.cap - offset;

                Some((
                    NonNull::new_unchecked(payload_ptr),
                    payload_cap,
                    raw.global_index,
                ))
            }
        } else {
            None
        }
    }
}

impl Default for HybridPool {
    fn default() -> Self {
        Self::new()
    }
}

impl BufPool for HybridPool {
    fn alloc_mem(&self, size: usize) -> AllocResult {
        if let Some((ptr, cap, global_index)) = self.alloc_mem_inner(size) {
            let header_ptr = unsafe { (ptr.as_ptr() as *mut BufferHeader).offset(-1) };
            let context = unsafe { (*header_ptr).context };

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

    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec> {
        let inner = self.inner.borrow();
        let mut iovecs = Vec::new();

        for slab in &inner.slabs {
            for i in 0..slab.config.count {
                let ptr = unsafe { slab.memory.as_ptr().add(i * slab.config.block_size) };
                iovecs.push(libc::iovec {
                    iov_base: ptr as *mut _,
                    iov_len: slab.config.block_size,
                });
            }
        }
        iovecs
    }
}

impl BufPoolExt for HybridPool {
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
    fn test_allocator_basic() {
        let mut allocator = HybridAllocator::new();
        // Check initial free counts
        assert_eq!(allocator.count_free(0), 512); // 8K slab
        assert_eq!(allocator.count_free(1), 128); // 16K slab
        assert_eq!(allocator.count_free(2), 64); // 32K slab
        assert_eq!(allocator.count_free(3), 32); // 64K slab

        // Alloc 8K (fits in 8K slab?)
        // needed_total = 4096 (payload) + 4096 (offset) = 8192
        // best_fit(8192) -> Size8K.
        // So it takes from slab 0 (8K).
        let raw = allocator.alloc(8192).unwrap();
        assert_eq!(raw.cap, SIZE_8K);
        assert_eq!(raw.context >> 16, 0); // Slab index 0
        assert_eq!(allocator.count_free(0), 511);
        assert_eq!(allocator.count_free(1), 128);

        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context) };
        assert_eq!(allocator.count_free(0), 512); // Restored
    }

    #[test]
    fn test_allocator_small() {
        let mut allocator = HybridAllocator::new();
        // Request very small size. 100 bytes + 4096 offset
        // 4196 needed.
        let raw = allocator.alloc(4196).unwrap();
        // best_fit(4196) -> Size8K
        assert_eq!(raw.cap, SIZE_8K);
        assert_eq!(raw.context >> 16, 0); // Slab 0
        assert_eq!(allocator.count_free(0), 511);
        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context) };
    }

    #[test]
    fn test_allocator_large() {
        let mut allocator = HybridAllocator::new();
        // Request 1MB
        let raw = allocator.alloc(1024 * 1024).unwrap();
        assert!(raw.cap >= 1024 * 1024);
        assert_eq!(raw.context, GLOBAL_ALLOC_CONTEXT);
        assert_eq!(raw.global_index, NO_REGISTRATION_INDEX);

        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context) };
        // No check on slab counts since it was global
    }

    #[test]
    fn test_hybrid_pool_integration() {
        let pool = HybridPool::new();
        let buf = pool.alloc(BufferSize::Size8K).unwrap();
        // Size8K request implies 8192 block. 4096 payload.
        // 4096 + 4096 = 8192 -> Size8K slab.
        assert!(buf.capacity() >= 4096);
        let idx = buf.buf_index();
        // Should be valid index
        assert_ne!(idx, NO_REGISTRATION_INDEX);

        drop(buf);
    }

    #[test]
    fn test_hybrid_alloc_clamping() {
        let pool = HybridPool::new();
        let buf = pool.alloc(BufferSize::Size8K).unwrap();
        // 8K Block. 4096 alignment. Capacity 4096.
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.capacity(), 4096);
    }

    #[test]
    fn test_hybrid_all_sizes() {
        let pool = HybridPool::new();

        let sizes = [
            BufferSize::Size8K,
            BufferSize::Size16K,
            BufferSize::Size32K,
            BufferSize::Size64K,
        ];

        for size in sizes {
            let buf = pool.alloc(size).unwrap();
            let min_expected = match size {
                BufferSize::Size8K => 4096,
                BufferSize::Size16K => 12288, // 16384 - 4096
                BufferSize::Size32K => 28672, // 32768 - 4096
                BufferSize::Size64K => 61440, // 65536 - 4096
                _ => 0,
            };
            assert!(buf.capacity() >= min_expected);
            // Verify alignment
            let ptr = buf.as_slice().as_ptr() as usize;
            assert_eq!(ptr % 4096, 0);
        }
    }
}
