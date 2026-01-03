use std::ptr::NonNull;

pub mod buddy;
pub mod hybrid;

pub use buddy::BuddyPool;
pub use hybrid::HybridPool;

pub const NO_REGISTRATION_INDEX: u16 = u16::MAX;

#[repr(C)]
pub struct BufferHeader {
    pub vtable: &'static PoolVTable,
    pub pool_data: NonNull<()>,
    pub context: usize,
}

pub struct PoolVTable {
    pub dealloc: unsafe fn(pool_data: NonNull<()>, params: DeallocParams),
}

#[derive(Debug)]
pub struct DeallocParams {
    pub ptr: NonNull<u8>, // Points to the Payload (data), not the header
    pub cap: usize,       // Capacity of the Payload
    pub context: usize,   // Context restored from header
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
    type BufferSize: Copy + std::fmt::Debug;

    /// Allocate memory.
    fn alloc(&self, size: Self::BufferSize) -> Option<FixedBuf>;

    /// Allocate memory of at least `size` bytes (Low level).
    fn alloc_mem(&self, size: usize) -> AllocResult;

    /// Get all buffers for io_uring registration.
    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec>;
}

#[derive(Debug)]
pub struct FixedBuf {
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    global_index: u16,
}

// Safety: This buffer is generally not Send because it refers to thread-local pool logic
// but in Thread-per-Core it stays on thread.

impl FixedBuf {
    /// Internal constructor.
    /// `ptr` must point to the payload (after BufferHeader).
    /// The memory at `ptr - size_of::<BufferHeader>()` must be a valid initialized BufferHeader.
    #[inline(always)]
    pub unsafe fn new(ptr: NonNull<u8>, cap: usize, global_index: u16) -> Self {
        Self {
            ptr,
            len: cap,
            cap,
            global_index,
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

    #[inline(always)]
    pub(crate) unsafe fn header_ptr(&self) -> *mut BufferHeader {
        unsafe { (self.ptr.as_ptr() as *mut BufferHeader).offset(-1) }
    }
}

impl Drop for FixedBuf {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe {
            // Retrieve header
            let header_ptr = self.header_ptr();
            let header = &*header_ptr;

            let params = DeallocParams {
                ptr: self.ptr,
                cap: self.cap,
                context: header.context,
            };

            // Call dealloc via vtable
            (header.vtable.dealloc)(header.pool_data, params);
        }
    }
}
