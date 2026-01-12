use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use tracing::debug;

/// State: The worker is actively running tasks.
pub const RUNNING: u8 = 0;
/// State: The worker is preparing to park (checking queues one last time).
pub const PARKING: u8 = 1;
/// State: The worker is fully parked on the driver (sleeping).
pub const PARKED: u8 = 2;

// --- Headers for Cache Isolation ---

#[repr(align(128))]
struct ProducerHeader {
    tail: AtomicUsize,
}

#[repr(align(128))]
struct ConsumerHeader {
    head: AtomicUsize,
}

/// A fixed-size, lock-free, Single-Producer Single-Consumer (SPSC) ring buffer.
///
/// Designed for full-mesh communication in the runtime.
/// Relying on `#[repr(align(128))]` on headers to prevent false sharing.
#[repr(C)]
struct Inner<T> {
    buffer: Box<[UnsafeCell<Option<T>>]>,
    mask: usize,

    // Writer state (tail)
    prod: ProducerHeader,

    // Reader state (head)
    cons: ConsumerHeader,
}

unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        let mut buffer = Vec::with_capacity(cap);
        for _ in 0..cap {
            buffer.push(UnsafeCell::new(None));
        }

        Self {
            buffer: buffer.into_boxed_slice(),
            mask: cap - 1,
            prod: ProducerHeader {
                tail: AtomicUsize::new(0),
            },
            cons: ConsumerHeader {
                head: AtomicUsize::new(0),
            },
        }
    }
}

pub struct Producer<T> {
    inner: Arc<Inner<T>>,
    // Local cache of head to reduce coherency traffic
    head_cache: usize,
    // Reference to the consumer's state for Notify-on-Park
    target_state: Arc<AtomicU8>,
}

pub struct Consumer<T> {
    inner: Arc<Inner<T>>,
    // Local cache of tail to reduce coherency traffic
    tail_cache: usize,
}

unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

impl<T> Producer<T> {
    pub fn push(&mut self, item: T) -> Result<(), T> {
        let head = self.head_cache;
        let tail = self.inner.prod.tail.load(Ordering::Relaxed);
        let mask = self.inner.mask;

        // Check if full
        if tail.wrapping_sub(head) >= mask + 1 {
            // Update head cache
            let new_head = self.inner.cons.head.load(Ordering::Acquire);
            self.head_cache = new_head;
            if tail.wrapping_sub(new_head) >= mask + 1 {
                return Err(item);
            }
        }

        // Write data
        let idx = tail & mask;
        unsafe {
            *self.inner.buffer[idx].get() = Some(item);
        }

        // Commit tail
        self.inner
            .prod
            .tail
            .store(tail.wrapping_add(1), Ordering::Release);

        Ok(())
    }

    pub fn target_state(&self) -> u8 {
        self.target_state.load(Ordering::Acquire)
    }
}

impl<T> Consumer<T> {
    pub fn pop(&mut self) -> Option<T> {
        let tail = self.tail_cache;
        let head = self.inner.cons.head.load(Ordering::Relaxed);
        let mask = self.inner.mask;

        // Check if empty
        if head == tail {
            // Update tail cache
            let new_tail = self.inner.prod.tail.load(Ordering::Acquire);
            self.tail_cache = new_tail;
            if head == new_tail {
                return None;
            }
        }

        // Read data
        let idx = head & mask;
        let item = unsafe { (*self.inner.buffer[idx].get()).take() };

        // Commit head
        self.inner
            .cons
            .head
            .store(head.wrapping_add(1), Ordering::Release);

        item
    }

    /// Pop multiple items into a local buffer.
    /// Returns the number of items popped.
    pub fn drain_into(&mut self, dest: &mut Vec<T>, limit: usize) -> usize {
        let mut count = 0;

        let mut head = self.inner.cons.head.load(Ordering::Relaxed);
        // Refresh tail cache once
        self.tail_cache = self.inner.prod.tail.load(Ordering::Acquire);
        let tail = self.tail_cache;
        let mask = self.inner.mask;

        while count < limit && head != tail {
            let idx = head & mask;
            let item = unsafe { (*self.inner.buffer[idx].get()).take().unwrap() };
            dest.push(item);

            head = head.wrapping_add(1);
            count += 1;
        }

        if count > 0 {
            self.inner.cons.head.store(head, Ordering::Release);
        }

        count
    }

    pub fn is_empty(&self) -> bool {
        let head = self.inner.cons.head.load(Ordering::Relaxed);
        let tail = self.inner.prod.tail.load(Ordering::Acquire);
        head == tail
    }
}

/// Create a pair of Producer and Consumer.
pub fn channel<T>(capacity: usize, target_state: Arc<AtomicU8>) -> (Producer<T>, Consumer<T>) {
    debug!("Creating mesh channel capacity={}", capacity);
    let inner = Arc::new(Inner::new(capacity));

    let p = Producer {
        inner: inner.clone(),
        head_cache: 0,
        target_state,
    };

    let c = Consumer {
        inner,
        tail_cache: 0,
    };

    (p, c)
}
