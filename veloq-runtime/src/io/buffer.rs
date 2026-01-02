use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

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
        block_size: 4096,
        count: 1024,
    },
    SlabConfig {
        block_size: 16384,
        count: 128,
    },
    SlabConfig {
        block_size: 65536,
        count: 32,
    },
];

pub const NO_REGISTRATION_INDEX: u16 = u16::MAX;
const GLOBAL_ALLOC_CONTEXT: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn size(&self) -> usize {
        match self {
            BufferSize::Size4K => 4096,
            BufferSize::Size16K => 16384,
            BufferSize::Size64K => 65536,
            BufferSize::Custom(size) => *size,
        }
    }
}

/// Trait for memory pool implementation allows custom memory management
pub trait BufPool: Clone + std::fmt::Debug + 'static {
    fn new() -> Self;
    /// Allocate memory of at least `size` bytes.
    /// Returns (ptr, capacity, global_index, context)
    ///
    /// - `global_index` is the index used for io_uring registration (if applicable)
    /// - `context` is an opaque value passed back to dealloc
    fn alloc_mem(&self, size: usize) -> Option<(NonNull<u8>, usize, u16, usize)>;

    /// Deallocate memory.
    unsafe fn dealloc_mem(&self, ptr: NonNull<u8>, cap: usize, context: usize);

    /// Get all buffers for io_uring registration.
    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec>;
}

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
pub struct BufferPool {
    inner: Rc<RefCell<PoolInner>>,
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool").finish_non_exhaustive()
    }
}

impl BufferPool {
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

        Self {
            inner: Rc::new(RefCell::new(PoolInner { slabs })),
        }
    }

    /// Allocate a buffer from the specified size class.
    pub fn alloc(&self, size: BufferSize) -> Option<FixedBuf<BufferPool>> {
        self.alloc_mem(size.size())
            .map(|(ptr, cap, global_index, context)| FixedBuf {
                pool: self.clone(),
                ptr,
                cap,
                len: 0,
                global_index,
                context,
            })
    }

    // Support registering these buffers with io_uring
}

impl BufPool for BufferPool {
    fn new() -> Self {
        BufferPool::new()
    }

    fn alloc_mem(&self, size: usize) -> Option<(NonNull<u8>, usize, u16, usize)> {
        let mut inner = self.inner.borrow_mut();

        // Try to find best slab (smallest that fits)
        if let Some(slab_idx) = SLABS.iter().position(|c| c.block_size >= size) {
            let slab = &mut inner.slabs[slab_idx];

            if let Some(index) = slab.free_indices.pop() {
                let ptr = unsafe { slab.memory.as_mut_ptr().add(index * slab.config.block_size) };

                // Encode slab_idx (0-2) and index (0-1024) into usize context
                // Using (slab_idx << 16) | index
                let context = (slab_idx << 16) | index;

                return Some((
                    NonNull::new(ptr).unwrap(),
                    slab.config.block_size,
                    slab.global_index_offset + index as u16,
                    context,
                ));
            }
        }

        // Fallback: Global Allocator (Hybrid Strategy)
        // Used when requested size is larger than largest slab, or slab is full (optional fallback)
        // For simplicity, we only fallback if size is explicitly large or if we want to support graceful degradation.
        // Current logic: If it fits a slab config but slab is empty, we return None (standard pool behavior).
        // Only if it doesn't fit ANY slab config (i.e. too big), we go to global alloc.

        // However, to support the "Larger chunks" requirement:
        // If size > largest slab block size, we MUST use global alloc.
        let max_slab_size = SLABS.last().map(|s| s.block_size).unwrap_or(0);
        if size > max_slab_size {
            let mut vec = Vec::with_capacity(size);
            let ptr = unsafe { NonNull::new_unchecked(vec.as_mut_ptr()) };
            let cap = vec.capacity();
            std::mem::forget(vec);

            return Some((ptr, cap, NO_REGISTRATION_INDEX, GLOBAL_ALLOC_CONTEXT));
        }

        None
    }

    unsafe fn dealloc_mem(&self, ptr: NonNull<u8>, cap: usize, context: usize) {
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

pub struct FixedBuf<P: BufPool> {
    pool: P,
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    global_index: u16,
    context: usize,
}

// Safety: This buffer is generally not Send because it refers to thread-local pool logic
// but in Thread-per-Core it stays on thread.

impl<P: BufPool> FixedBuf<P> {
    pub fn buf_index(&self) -> u16 {
        self.global_index
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Access the full capacity as a mutable slice for writing data before set_len is called.
    pub fn spare_capacity_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }

    // Pointer to start of capacity
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.cap);
        self.len = len;
    }
}

impl<P: BufPool> Drop for FixedBuf<P> {
    fn drop(&mut self) {
        unsafe {
            self.pool.dealloc_mem(self.ptr, self.cap, self.context);
        }
    }
}
