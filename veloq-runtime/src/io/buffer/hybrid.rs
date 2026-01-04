use super::{
    AllocResult, BufPool, BufferHeader, DeallocParams, FixedBuf, NO_REGISTRATION_INDEX, PoolVTable,
};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

const SIZE_4K: usize = 4096;
const SIZE_16K: usize = 16384;
const SIZE_64K: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BufferSize {
    /// 4KB
    Size4K,
    /// 16KB
    Size16K,
    /// 64KB
    Size64K,
    /// Custom size
    Custom(usize),
}

impl BufferSize {
    #[inline(always)]
    pub fn size(&self) -> usize {
        match self {
            BufferSize::Size4K => SIZE_4K,
            BufferSize::Size16K => SIZE_16K,
            BufferSize::Size64K => SIZE_64K,
            BufferSize::Custom(size) => *size,
        }
    }

    #[inline(always)]
    pub fn slab_index(&self) -> Option<usize> {
        match self {
            BufferSize::Size4K => Some(0),
            BufferSize::Size16K => Some(1),
            BufferSize::Size64K => Some(2),
            BufferSize::Custom(_) => None,
        }
    }

    pub fn best_fit(size: usize) -> Self {
        if size <= SIZE_4K {
            BufferSize::Size4K
        } else if size <= SIZE_16K {
            BufferSize::Size16K
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

// Define 3 classes of buffer sizes
// Class 0: 4KB, 1024 count -> 4MB
// Class 1: 16KB, 128 count -> 2MB
// Class 2: 64KB, 32 count -> 2MB
// Total memory: ~8MB
const SLABS: [SlabConfig; 3] = [
    SlabConfig {
        block_size: SIZE_4K,
        count: 1024,
    },
    SlabConfig {
        block_size: SIZE_16K,
        count: 128,
    },
    SlabConfig {
        block_size: SIZE_64K,
        count: 32,
    },
];

const GLOBAL_ALLOC_CONTEXT: usize = usize::MAX;

struct Slab {
    config: SlabConfig,
    memory: Vec<u8>,
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

impl HybridAllocator {
    pub fn new() -> Self {
        let mut slabs = Vec::with_capacity(SLABS.len());
        let mut current_global_offset = 0;

        for config in SLABS.iter() {
            let total_size = config.block_size * config.count;
            let mut memory = Vec::with_capacity(total_size);
            memory.resize(total_size, 0); // zero init

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

            // Ensure the slab block size is actually sufficient (e.g. if best_fit matched but logical logic thinks otherwise)
            // best_fit returns Size4K for <= 4096.
            // If needed_total is 4096, it fits.
            // If needed_total is 4100, best_fit returns Size16K. Fits.
            // So this check is mostly sanity.
            if slab.config.block_size >= needed_total {
                if let Some(index) = slab.free_indices.pop() {
                    let block_ptr =
                        unsafe { slab.memory.as_mut_ptr().add(index * slab.config.block_size) };

                    // Encode slab_idx (0-2) and index (0-1024) into usize context
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
            let mut vec: Vec<u8> = Vec::with_capacity(needed_total);
            let block_ptr = vec.as_mut_ptr();
            let cap = vec.capacity();
            std::mem::forget(vec);

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
    pub unsafe fn dealloc(&mut self, block_ptr: NonNull<u8>, cap: usize, context: usize) {
        if context == GLOBAL_ALLOC_CONTEXT {
            // Reconstruct Vec to drop it
            let _vec = unsafe { Vec::from_raw_parts(block_ptr.as_ptr(), 0, cap) };
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
    let header_size = std::mem::size_of::<BufferHeader>();
    let block_ptr = unsafe { params.ptr.as_ptr().sub(header_size) };
    let total_cap = params.cap + header_size;

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
        let header_size = std::mem::size_of::<BufferHeader>();
        let needed_total = size + header_size;

        if let Some(raw) = inner.alloc(needed_total) {
            let block_ptr = raw.ptr.as_ptr();
            let header_ptr = block_ptr as *mut BufferHeader;
            let pool_raw = Rc::into_raw(self.inner.clone());

            unsafe {
                (*header_ptr) = BufferHeader {
                    vtable: &HYBRID_POOL_VTABLE,
                    pool_data: NonNull::new_unchecked(pool_raw as *mut ()),
                    context: raw.context,
                };

                let payload_ptr = block_ptr.add(header_size);
                let payload_cap = raw.cap - header_size;

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
    type BufferSize = BufferSize;

    fn alloc(&self, size: Self::BufferSize) -> Option<FixedBuf> {
        self.alloc(size)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator_basic() {
        let mut allocator = HybridAllocator::new();
        // Check initial free counts
        assert_eq!(allocator.count_free(0), 1024); // 4K slab
        assert_eq!(allocator.count_free(1), 128); // 16K slab
        assert_eq!(allocator.count_free(2), 32); // 64K slab

        // Alloc 4K (fits in 4K slab?)
        // needed_total = 4096 (payload) + 24 (header) = 4120
        // best_fit(4120) -> Size16K.
        // So it takes from slab 1 (16K).
        let raw = allocator.alloc(4120).unwrap();
        assert_eq!(raw.cap, SIZE_16K);
        assert_eq!(raw.context >> 16, 1); // Slab index 1
        assert_eq!(allocator.count_free(0), 1024);
        assert_eq!(allocator.count_free(1), 127); // Decremented

        unsafe { allocator.dealloc(raw.ptr, raw.cap, raw.context) };
        assert_eq!(allocator.count_free(1), 128); // Restored
    }

    #[test]
    fn test_allocator_small() {
        let mut allocator = HybridAllocator::new();
        // Request very small size. 100 bytes + 24
        let raw = allocator.alloc(124).unwrap();
        // best_fit(124) -> Size4K
        assert_eq!(raw.cap, SIZE_4K);
        assert_eq!(raw.context >> 16, 0); // Slab 0
        assert_eq!(allocator.count_free(0), 1023);
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
        let buf = pool.alloc(BufferSize::Size4K).unwrap();
        // As established, Size4K request implies 4096 payload.
        // 4096+24 = 4120 -> Size16K slab.
        assert!(buf.capacity() >= 4096);
        let idx = buf.buf_index();
        // Should be valid index
        assert_ne!(idx, NO_REGISTRATION_INDEX);

        drop(buf);
    }

    #[test]
    fn test_hybrid_alloc_clamping() {
        let pool = HybridPool::new();
        let buf = pool.alloc(BufferSize::Size4K).unwrap();
        // Since it bumped to 16K slab to fit header, capacity > 4096.
        // But we expect len to be clamped to 4096.
        assert_eq!(buf.len(), 4096);
        assert!(buf.capacity() > 4096);
    }
}
