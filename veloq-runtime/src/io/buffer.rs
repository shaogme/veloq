use std::ptr::NonNull;

pub mod buddy;
pub mod hybrid;

pub use buddy::BuddyPool;
pub use hybrid::HybridPool;

// Backward compatibility or default choice
pub type BufferPool = HybridPool;

pub const NO_REGISTRATION_INDEX: u16 = u16::MAX;

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
    #[inline(always)]
    pub fn size(&self) -> usize {
        match self {
            BufferSize::Size4K => 4096,
            BufferSize::Size16K => 16384,
            BufferSize::Size64K => 65536,
            BufferSize::Custom(size) => *size,
        }
    }
}

#[derive(Debug)]
pub struct DeallocParams {
    pub ptr: NonNull<u8>,
    pub cap: usize,
    pub context: usize,
}

#[derive(Debug)]
pub enum AllocResult {
    Allocated {
        ptr: NonNull<u8>,
        cap: usize,
        global_index: u16,
        context: usize,
    },
    Failed,
}

/// Trait for memory pool implementation allows custom memory management
pub trait BufPool: Clone + std::fmt::Debug + 'static {
    fn new() -> Self;
    /// Allocate memory of at least `size` bytes.
    fn alloc_mem(&self, size: usize) -> AllocResult;

    /// Deallocate memory.
    unsafe fn dealloc_mem(&self, params: DeallocParams);

    /// Get all buffers for io_uring registration.
    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec>;
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
    #[inline(always)]
    pub fn new(pool: P, ptr: NonNull<u8>, cap: usize, global_index: u16, context: usize) -> Self {
        Self {
            pool,
            ptr,
            len: cap,
            cap,
            global_index,
            context,
        }
    }

    #[inline(always)]
    pub fn buf_index(&self) -> u16 {
        self.global_index
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[inline(always)]
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Access the full capacity as a mutable slice for writing data before set_len is called.
    #[inline(always)]
    pub fn spare_capacity_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }

    // Pointer to start of capacity
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.cap);
        self.len = len;
    }
}

impl<P: BufPool> Drop for FixedBuf<P> {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe {
            self.pool.dealloc_mem(DeallocParams {
                ptr: self.ptr,
                cap: self.cap,
                context: self.context,
            });
        }
    }
}
