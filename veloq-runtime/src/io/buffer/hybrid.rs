use super::{AllocResult, BufPool, DeallocParams, FixedBuf, NO_REGISTRATION_INDEX};
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

struct PoolInner {
    slabs: Vec<Slab>,
}

#[derive(Clone)]
pub struct HybridPool {
    inner: Rc<RefCell<PoolInner>>,
}

impl std::fmt::Debug for HybridPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridPool").finish_non_exhaustive()
    }
}

impl HybridPool {
    pub fn new() -> Self {
        Self::new_inner()
    }

    pub fn new_inner() -> Self {
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

        Self {
            inner: Rc::new(RefCell::new(PoolInner { slabs })),
        }
    }

    /// Allocate a buffer from the specified size class.
    pub fn alloc(&self, size: BufferSize) -> Option<FixedBuf<HybridPool>> {
        match self.alloc_mem(size.size()) {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            } => Some(FixedBuf::new(self.clone(), ptr, cap, global_index, context)),
            AllocResult::Failed => None,
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

    fn alloc(&self, size: Self::BufferSize) -> Option<FixedBuf<Self>> {
        self.alloc(size)
    }

    fn alloc_mem(&self, size: usize) -> AllocResult {
        let mut inner = self.inner.borrow_mut();

        // Try to find best slab (smallest that fits)
        // Try to find best slab (smallest that fits)
        let best = BufferSize::best_fit(size);
        if let Some(slab_idx) = best.slab_index() {
            let slab = &mut inner.slabs[slab_idx];

            if let Some(index) = slab.free_indices.pop() {
                let ptr = unsafe { slab.memory.as_mut_ptr().add(index * slab.config.block_size) };

                // Encode slab_idx (0-2) and index (0-1024) into usize context
                // Using (slab_idx << 16) | index
                let context = (slab_idx << 16) | index;

                return AllocResult::Allocated {
                    ptr: NonNull::new(ptr).unwrap(),
                    cap: slab.config.block_size,
                    global_index: slab.global_index_offset + index as u16,
                    context,
                };
            }
        }

        // Fallback: Global Allocator (Hybrid Strategy)
        if size > SIZE_64K {
            let mut vec = Vec::with_capacity(size);
            let ptr = unsafe { NonNull::new_unchecked(vec.as_mut_ptr()) };
            let cap = vec.capacity();
            std::mem::forget(vec);

            return AllocResult::Allocated {
                ptr,
                cap,
                global_index: NO_REGISTRATION_INDEX,
                context: GLOBAL_ALLOC_CONTEXT,
            };
        }

        AllocResult::Failed
    }

    unsafe fn dealloc_mem(&self, params: DeallocParams) {
        let ptr = params.ptr;
        let cap = params.cap;
        let context = params.context;
        if context == GLOBAL_ALLOC_CONTEXT {
            // Global dealloc
            let _ = unsafe { Vec::from_raw_parts(ptr.as_ptr(), 0, cap) };
            return;
        }

        let mut inner = self.inner.borrow_mut();
        let slab_idx = context >> 16;
        let index = context & 0xFFFF;

        if let Some(slab) = inner.slabs.get_mut(slab_idx) {
            slab.free_indices.push(index);
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
    fn test_hybrid_slab_alloc() {
        let pool = HybridPool::new();
        let buf = pool.alloc(BufferSize::Size4K).unwrap();
        assert_eq!(buf.capacity(), 4096);
        assert!(buf.buf_index() != NO_REGISTRATION_INDEX);
    }

    #[test]
    fn test_hybrid_large_alloc() {
        let pool = HybridPool::new();
        let mut buf = pool.alloc(BufferSize::Custom(1024 * 1024)).unwrap(); // 1MB
        assert!(buf.capacity() >= 1024 * 1024);
        assert_eq!(buf.buf_index(), NO_REGISTRATION_INDEX);

        // Test write
        let slice = buf.as_slice_mut();
        slice[0] = 1;

        let slice = buf.as_slice();
        assert_eq!(slice[0], 1);
        assert_eq!(buf.len(), buf.capacity());
    }
}
