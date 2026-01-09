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
    pub resolve_region_info: unsafe fn(pool_data: NonNull<()>, buf: &FixedBuf) -> (usize, usize),
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

impl AllocResult {
    #[inline]
    pub fn into_buf(self, pool: &dyn BackingPool) -> Option<FixedBuf> {
        match self {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            } => unsafe {
                Some(FixedBuf::new(
                    ptr,
                    cap,
                    global_index,
                    pool.pool_data(),
                    pool.vtable(),
                    context,
                ))
            },
            AllocResult::Failed => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BufferRegion {
    pub ptr: NonNull<u8>,
    pub len: usize,
}

unsafe impl Send for BufferRegion {}
unsafe impl Sync for BufferRegion {}

/// Trait abstraction for driver-specific buffer registration
pub trait BufferRegistrar {
    /// Register memory regions with the kernel.
    /// Returns a list of handles (tokens) corresponding to the regions.
    /// For RIO this is RIO_BUFFERID, for uring it might be ignored or index.
    fn register(&self, regions: &[BufferRegion]) -> std::io::Result<Vec<usize>>;
}

/// Memory pool implementation providing raw memory allocation.
/// This trait manages memory layout, allocation algorithms, and deallocation.
/// It does NOT handle driver registration.
pub trait BackingPool: std::fmt::Debug + 'static {
    /// Allocate memory without registration context.
    /// Returns allocation result containing ptr, capacity, and header context.
    /// The `global_index` in the result should be ignored or 0.
    fn alloc_mem(&self, size: usize) -> AllocResult;

    /// Get all memory regions managed by this pool.
    fn get_memory_regions(&self) -> Vec<BufferRegion>;

    /// Get the VTable for this pool (used by FixedBuf for deallocation).
    fn vtable(&self) -> &'static PoolVTable;

    /// Get the raw pool data pointer.
    fn pool_data(&self) -> NonNull<()>;

    /// Resolve Buffer to Region Index and Offset relative to the pool.
    fn resolve_region_info(&self, buf: &FixedBuf) -> (usize, usize) {
        let ptr = buf.as_ptr() as usize;
        let regions = self.get_memory_regions();
        for (i, region) in regions.iter().enumerate() {
            let base = region.ptr.as_ptr() as usize;
            if ptr >= base && ptr < base + region.len {
                return (i, ptr - base);
            }
        }
        panic!("Buffer not found in pool regions");
    }
}

/// High-level Buffer Pool trait.
/// Represents a pool that is ready for I/O operations (registered if necessary).
pub trait BufPool: std::fmt::Debug + 'static {
    /// Allocate a buffer ready for I/O.
    fn alloc(&self, len: usize) -> Option<FixedBuf>;
}

// 组合注册池

/// A wrapper that binds a backing pool with a registrar.
/// This is the bridge between raw memory and driver-aware buffers.
#[derive(Clone)]
pub struct RegisteredPool<P> {
    pool: P,
    // Rc is required to satisfy Clone for AnyBufPool
    #[allow(dead_code)]
    registrar: std::rc::Rc<dyn BufferRegistrar>,
    registration_ids: std::rc::Rc<Vec<usize>>,
}

impl<P: BackingPool> RegisteredPool<P> {
    pub fn new(pool: P, registrar: Box<dyn BufferRegistrar>) -> std::io::Result<Self> {
        let regions = pool.get_memory_regions();
        let ids = registrar.register(&regions)?;
        Ok(Self {
            pool,
            registrar: std::rc::Rc::from(registrar),
            registration_ids: std::rc::Rc::new(ids),
        })
    }
}

impl<P: BackingPool> std::fmt::Debug for RegisteredPool<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredPool")
            .field("pool", &self.pool)
            .field("registration_ids", &self.registration_ids)
            .finish()
    }
}

impl<P: BackingPool> BufPool for RegisteredPool<P> {
    fn alloc(&self, len: usize) -> Option<FixedBuf> {
        match self.pool.alloc_mem(len) {
            AllocResult::Allocated {
                ptr, cap, context, ..
            } => {
                // Use the first registration ID as the global index.
                // For complex multi-region pools, we might need mapping logic,
                // but currently Buddy/Hybrid are single-region arenas.
                let global_index =
                    self.registration_ids
                        .first()
                        .copied()
                        .unwrap_or(NO_REGISTRATION_INDEX as usize) as u16;

                unsafe {
                    let mut buf = FixedBuf::new(
                        ptr,
                        cap,
                        global_index,
                        self.pool.pool_data(),
                        self.pool.vtable(),
                        context,
                    );
                    buf.set_len(len);
                    Some(buf)
                }
            }
            AllocResult::Failed => None,
        }
    }
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

    /// Resolve which region this buffer belongs to and its offset.
    /// This is used for driver submission (RIO / io_uring).
    #[inline(always)]
    pub fn resolve_region_info(&self) -> (usize, usize) {
        unsafe { (self.vtable.resolve_region_info)(self.pool_data, self) }
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
    pub alloc: unsafe fn(*const (), usize) -> Option<FixedBuf>,
    pub clone: unsafe fn(*const ()) -> *mut (),
    pub drop: unsafe fn(*mut ()),
    pub fmt: unsafe fn(*const (), &mut std::fmt::Formatter<'_>) -> std::fmt::Result,
}

/// A type-erased handle to any `BufPool`.
pub struct AnyBufPool {
    data: *mut (),
    vtable: &'static BufPoolVTable,
}

impl AnyBufPool {
    /// 从任意实现了 `BufPool + Clone` 的类型构造 `AnyBufPool`。
    pub fn new<P: BufPool + Clone + 'static>(pool: P) -> Self {
        unsafe fn alloc_shim<P: BufPool>(ptr: *const (), size: usize) -> Option<FixedBuf> {
            unsafe {
                let pool = &*(ptr as *const P);
                pool.alloc(size)
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
                alloc: alloc_shim::<P>,
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
    fn alloc(&self, len: usize) -> Option<FixedBuf> {
        unsafe { (self.vtable.alloc)(self.data, len) }
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
