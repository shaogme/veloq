use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

// Simple constant size chunk for now: 4KB
const CHUNK_SIZE: usize = 4096;
const POOL_SIZE: usize = 1024; // 1024 * 4KB = 4MB

struct PoolInner {
    memory: Vec<u8>,
    free_indices: Vec<usize>,
}

pub struct BufferPool {
    inner: Rc<RefCell<PoolInner>>,
}

impl BufferPool {
    pub fn new() -> Self {
        let total_size = CHUNK_SIZE * POOL_SIZE;
        // Align memory to page size if possible, but Vec usually aligns well enough for simple testing.
        // For production, use explicit allocation.
        let mut memory = Vec::with_capacity(total_size);
        memory.resize(total_size, 0); // zero init

        let free_indices = (0..POOL_SIZE).collect();

        Self {
            inner: Rc::new(RefCell::new(PoolInner {
                memory,
                free_indices,
            })),
        }
    }

    pub fn alloc(&self) -> Option<FixedBuf> {
        let mut inner = self.inner.borrow_mut();
        if let Some(index) = inner.free_indices.pop() {
            let ptr = unsafe { inner.memory.as_mut_ptr().add(index * CHUNK_SIZE) };
            Some(FixedBuf {
                index,
                ptr: NonNull::new(ptr).unwrap(),
                len: 0,
                cap: CHUNK_SIZE,
                pool: self.inner.clone(), // Keep reference to return on drop
            })
        } else {
            None
        }
    }

    // Support registering these buffers with io_uring
    #[cfg(target_os = "linux")]
    pub(crate) fn get_all_ptrs(&self) -> Vec<libc::iovec> {
        let inner = self.inner.borrow();
        (0..POOL_SIZE)
            .map(|i| {
                let ptr = unsafe { inner.memory.as_ptr().add(i * CHUNK_SIZE) };
                libc::iovec {
                    iov_base: ptr as *mut _,
                    iov_len: CHUNK_SIZE,
                }
            })
            .collect()
    }
}

pub struct FixedBuf {
    index: usize,
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    pool: Rc<RefCell<PoolInner>>,
}

// Safety: This buffer is generally not Send because it refers to thread-local pool logic
// but in Thread-per-Core it stays on thread.
// We rely on simple logic.

impl FixedBuf {
    pub fn buf_index(&self) -> u16 {
        self.index as u16
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

impl Drop for FixedBuf {
    fn drop(&mut self) {
        // Return index to pool
        let mut inner = self.pool.borrow_mut();
        inner.free_indices.push(self.index);
    }
}
