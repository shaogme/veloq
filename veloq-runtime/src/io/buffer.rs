//! # Buffer Management for High-Performance I/O
//!
//! This module provides abstractions for managing memory buffers compatible with modern
//! asynchronous I/O interfaces (like `io_uring` on Linux and IOCP on Windows).
//!
//! ## Core Components
//!
//! - [`FixedBuf`]: An owned handle to a buffer allocated from a pool. It ensures the underlying
//!   memory remains valid as long as the handle exists. It uses type erasure (VTables) to
//!   delegate deallocation back to the source pool.
//! - [`BufPool`]: The base trait for memory pool implementations.
//!
//! ## Implementation Requirements
//!
//! Implementing a `BufPool` requires strict adherence to memory layout rules to support
//! Zero-Copy and Direct I/O (O_DIRECT / FILE_FLAG_NO_BUFFERING) operations.
//!
//! ### 1. Memory Stability
//! The pool must guarantee that the memory pointer in `FixedBuf` remains valid until
//! `dealloc` is called. For `io_uring`, this often means the memory must be registered
//! with the kernel and not moved.
//!
//! ### 2. Direct I/O Alignment (Critical)
//! To support `DirectSync` operations, the buffers returned by the pool must satisfy
//! strict alignment requirements imposed by the OS and hardware drivers:
//!
//! - **Payload Alignment**: The `ptr` points to the start of the user data payload.
//!   This address MUST be aligned to at least **512 bytes** (Sector Size). Ideally, align
//!   to **4096 bytes** (Page Size) for best performance.
//!   
//! - **Backing Memory Alignment**: The underlying allocation (Slab, Arena, or Block)
//!   should be **Page Aligned (4096 bytes)**. This prevents splitting pages across
//!   DRAM boundaries in ways that might degrade DMA performance.
//!
//! ### 3. Metadata Management
//! Unlike traditional implementations that might store metadata in an intrusive header
//! *before* the payload, `FixedBuf` stores necessary context (like pool references and VTable)
//! within the handle itself to avoid alignment complexities.
//!
//! **Implementors MUST ensure:**
//! - The `dealloc` function in the VTable can correctly free the memory using only:
//!   - The payload pointer (`ptr`)
//!   - The capacity (`cap`)
//!   - An opaque `context` value (usize) provided during allocation
//!
//! See [`buddy::BuddyPool`] and [`hybrid::HybridPool`] for reference implementations.

use std::{
    alloc::{Layout, LayoutError},
    ptr::NonNull,
};

pub mod buddy;
pub mod hybrid;

pub use buddy::BuddyPool;
pub use hybrid::HybridPool;

pub const NO_REGISTRATION_INDEX: u16 = u16::MAX;

#[derive(Debug)]
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
pub trait BufPool: std::fmt::Debug + 'static {
    /// Allocate memory with specific length.
    fn alloc_len(&self, len: usize) -> Option<FixedBuf> {
        match self.alloc_mem(len) {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            } => unsafe {
                let mut buf = FixedBuf::new(
                    ptr,
                    cap,
                    global_index,
                    self.pool_data(),
                    self.vtable(),
                    context,
                );
                buf.set_len(len);
                Some(buf)
            },
            AllocResult::Failed => None,
        }
    }

    /// Allocate memory of at least `size` bytes (Low level).
    fn alloc_mem(&self, size: usize) -> AllocResult;

    /// Get all buffers for io_uring registration.
    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec>;

    /// Get the VTable for this pool.
    fn vtable(&self) -> &'static PoolVTable;

    /// Get the raw pool data pointer (e.g. Rc<RefCell<Allocator>> as void ptr).
    fn pool_data(&self) -> NonNull<()>;
}

pub trait BufPoolExt: BufPool + Clone {
    type BufferSize: Copy + std::fmt::Debug;

    /// Allocate memory.
    fn alloc(&self, size: Self::BufferSize) -> Option<FixedBuf>;
}

#[derive(Debug)]
pub struct FixedBuf {
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    global_index: u16,
    // Metadata moved from Heap Header to Handle
    pool_data: NonNull<()>,
    vtable: &'static PoolVTable,
    context: usize,
}

// Safety: This buffer is generally not Send because it refers to thread-local pool logic
// but in Thread-per-Core it stays on thread.

impl FixedBuf {
    /// # Safety
    /// `ptr` must be valid and allocated by the pool associated with `vtable`.
    #[inline(always)]
    pub unsafe fn new(
        ptr: NonNull<u8>,
        cap: usize,
        global_index: u16,
        pool_data: NonNull<()>,
        vtable: &'static PoolVTable,
        context: usize,
    ) -> Self {
        Self {
            ptr,
            len: cap,
            cap,
            global_index,
            pool_data,
            vtable,
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

    #[inline(always)]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
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
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.cap);
        self.len = len;
    }
}

impl Drop for FixedBuf {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe {
            let params = DeallocParams {
                ptr: self.ptr,
                cap: self.cap,
                context: self.context,
            };

            // Call dealloc via vtable stored in handle
            (self.vtable.dealloc)(self.pool_data, params);
        }
    }
}

#[derive(Debug)]
pub enum AllocError {
    Layout(LayoutError),
    Oom,
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocError::Layout(e) => write!(f, "Layout error: {}", e),
            AllocError::Oom => write!(f, "Out of memory"),
        }
    }
}

impl std::error::Error for AllocError {}

pub(crate) struct AlignedMemory {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedMemory {
    fn new(size: usize, align: usize) -> Result<Self, AllocError> {
        let layout = Layout::from_size_align(size, align).map_err(AllocError::Layout)?;
        // SAFETY: Only creating a sized allocation with valid layout
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return Err(AllocError::Oom);
        }
        // Zero initialize for safety
        unsafe { std::ptr::write_bytes(ptr, 0, size) };
        Ok(Self {
            ptr: NonNull::new(ptr).unwrap(),
            layout,
        })
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for AlignedMemory {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// ============================================================================
// Type-Erased Box<dyn BufPool> Replacement (Thread-Local Friendly)
// ============================================================================

/// 手写 VTable，用于动态分发 BufPool 的方法而不使用 dyn
pub struct BufPoolVTable {
    pub alloc_mem: unsafe fn(*const (), usize) -> AllocResult,
    #[cfg(target_os = "linux")]
    pub get_registration_buffers: unsafe fn(*const ()) -> Vec<libc::iovec>,
    pub vtable: unsafe fn(*const ()) -> &'static PoolVTable,
    pub pool_data: unsafe fn(*const ()) -> NonNull<()>,
    pub clone: unsafe fn(*const ()) -> *mut (),
    pub drop: unsafe fn(*mut ()),
    pub fmt: unsafe fn(*const (), &mut std::fmt::Formatter<'_>) -> std::fmt::Result,
}

/// 类型擦除的 Pool 句柄，可存储在 TLS 中。
/// 类似于 `Box<dyn BufPool + Clone>`，但使用手写 VTable 避免了 dyn 限制和对象安全问题。
pub struct AnyBufPool {
    data: *mut (),
    vtable: &'static BufPoolVTable,
}

impl AnyBufPool {
    /// 从任意实现了 `BufPool + Clone` 的类型构造 `AnyBufPool`。
    pub fn new<P: BufPool + Clone + 'static>(pool: P) -> Self {
        unsafe fn alloc_mem_shim<P: BufPool>(ptr: *const (), size: usize) -> AllocResult {
            unsafe {
                let pool = &*(ptr as *const P);
                pool.alloc_mem(size)
            }
        }

        #[cfg(target_os = "linux")]
        unsafe fn get_registration_buffers_shim<P: BufPool>(ptr: *const ()) -> Vec<libc::iovec> {
            unsafe {
                let pool = &*(ptr as *const P);
                pool.get_registration_buffers()
            }
        }

        unsafe fn vtable_shim<P: BufPool>(ptr: *const ()) -> &'static PoolVTable {
            unsafe {
                let pool = &*(ptr as *const P);
                pool.vtable()
            }
        }

        unsafe fn pool_data_shim<P: BufPool>(ptr: *const ()) -> NonNull<()> {
            unsafe {
                let pool = &*(ptr as *const P);
                pool.pool_data()
            }
        }

        unsafe fn clone_shim<P: BufPool + Clone>(ptr: *const ()) -> *mut () {
            unsafe {
                let pool = &*(ptr as *const P);
                let new_pool = Box::new(pool.clone());
                Box::into_raw(new_pool) as *mut ()
            }
        }

        unsafe fn drop_shim<P: BufPool>(ptr: *mut ()) {
            unsafe {
                let _ = Box::from_raw(ptr as *mut P);
            }
        }

        unsafe fn fmt_shim<P: BufPool>(
            ptr: *const (),
            f: &mut std::fmt::Formatter<'_>,
        ) -> std::fmt::Result {
            unsafe {
                let pool = &*(ptr as *const P);
                std::fmt::Debug::fmt(pool, f)
            }
        }

        struct VTableGen<P>(std::marker::PhantomData<P>);

        impl<P: BufPool + Clone + 'static> VTableGen<P> {
            const VTABLE: BufPoolVTable = BufPoolVTable {
                alloc_mem: alloc_mem_shim::<P>,
                #[cfg(target_os = "linux")]
                get_registration_buffers: get_registration_buffers_shim::<P>,
                vtable: vtable_shim::<P>,
                pool_data: pool_data_shim::<P>,
                clone: clone_shim::<P>,
                drop: drop_shim::<P>,
                fmt: fmt_shim::<P>,
            };
        }

        AnyBufPool {
            data: Box::into_raw(Box::new(pool)) as *mut (),
            vtable: &VTableGen::<P>::VTABLE,
        }
    }
}

impl BufPool for AnyBufPool {
    fn alloc_mem(&self, size: usize) -> AllocResult {
        unsafe { (self.vtable.alloc_mem)(self.data, size) }
    }

    #[cfg(target_os = "linux")]
    fn get_registration_buffers(&self) -> Vec<libc::iovec> {
        unsafe { (self.vtable.get_registration_buffers)(self.data) }
    }

    // For non-Linux, generic trait impl doesn't require this method inside the impl
    // unless defined in the trait. The trait def has `#[cfg(target_os = "linux")]`
    // so `AnyBufPool` implementation must match.

    fn vtable(&self) -> &'static PoolVTable {
        unsafe { (self.vtable.vtable)(self.data) }
    }

    fn pool_data(&self) -> NonNull<()> {
        unsafe { (self.vtable.pool_data)(self.data) }
    }
}

impl Clone for AnyBufPool {
    fn clone(&self) -> Self {
        unsafe {
            let new_data = (self.vtable.clone)(self.data);
            Self {
                data: new_data,
                vtable: self.vtable,
            }
        }
    }
}

impl Drop for AnyBufPool {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self.data) }
    }
}

impl std::fmt::Debug for AnyBufPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe { (self.vtable.fmt)(self.data, f) }
    }
}
