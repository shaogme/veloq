use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// A node in the intrusive queue.
///
/// Note: To avoid complex generics for Intrusive traits right now,
/// we will use a simple pointer-based wrapper approach where the User allocates the Node.
/// Or, for the specific use case of SegQueue<Job>, we can implement a MpscQueue<T>
/// that allocates nodes internally (like crossbeam).
///
/// To match the "Extreme Performance" requirement while being ergonomic,
/// we will implement a Non-Intrusive MPSC Queue (internally allocates nodes) first,
/// but optimize it heavily.
struct Node<T> {
    value: Option<T>,
    next: AtomicPtr<Node<T>>,
}

impl<T> Node<T> {
    fn new(value: Option<T>) -> *mut Self {
        Box::into_raw(Box::new(Self {
            value,
            next: AtomicPtr::new(ptr::null_mut()),
        }))
    }
}

/// A Multi-Producer Single-Consumer Queue.
///
/// This queue is lock-free, unbounded, and linearizable.
/// It uses the classic Vyukov MPSC algorithm.
pub struct MpscQueue<T> {
    head: AtomicPtr<Node<T>>,
    tail: UnsafeCell<*mut Node<T>>,
}

unsafe impl<T: Send> Send for MpscQueue<T> {}
unsafe impl<T: Send> Sync for MpscQueue<T> {}

impl<T> MpscQueue<T> {
    /// Create a new empty queue.
    pub fn new() -> Self {
        // Create a dummy stub node
        let stub = Node::new(None);
        Self {
            head: AtomicPtr::new(stub),
            tail: UnsafeCell::new(stub),
        }
    }

    /// Push a value into the queue.
    /// This is wait-free and atomic.
    pub fn push(&self, value: T) {
        let node = Node::new(Some(value));
        // XCHG: atomically swap head to new node
        let prev = self.head.swap(node, Ordering::AcqRel);
        // Link old head to new node
        unsafe {
            (*prev).next.store(node, Ordering::Release);
        }
    }

    /// Pop a value from the queue.
    /// This is wait-free for the consumer (if not empty).
    /// Returns None if empty.
    ///
    /// Note: This function is strictly Single-Consumer.
    /// Calling this concurrently from multiple threads is UB.
    /// However, since we define `inner` usage such that only the Owner calls pop, it's safe.
    /// If we want to enforce it, we could use &mut self, but that limits `Arc<MpscQueue>`.
    /// Typical pattern: The "Consumer" side is `!Sync`, holding a reference.
    /// For this crate, we assume the caller ensures single consumer.
    pub fn pop(&self) -> Option<T> {
        unsafe {
            let tail = *self.tail.get();
            let next = (*tail).next.load(Ordering::Acquire);

            if !next.is_null() {
                // Queue is not empty
                // 'next' is the node containing the value (tail is dummy/stub)
                // Move tail to next
                *self.tail.get() = next;

                // Read value
                let value = (*next).value.take();

                // Reclaim the OLD tail
                let _ = Box::from_raw(tail);

                return value;
            }

            None
        }
    }

    /// Check if the queue is empty.
    /// This is a loose check; it's consistent eventually.
    pub fn is_empty(&self) -> bool {
        unsafe {
            let tail = *self.tail.get();
            let next = (*tail).next.load(Ordering::Relaxed);
            next.is_null()
        }
    }
}

impl<T> Drop for MpscQueue<T> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = *self.tail.get();
            while !curr.is_null() {
                let next = (*curr).next.load(Ordering::Relaxed);
                let _ = Box::from_raw(curr);
                curr = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_simple_push_pop() {
        let q = MpscQueue::new();
        q.push(1);
        q.push(2);
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn test_concurrent_push() {
        let q = Arc::new(MpscQueue::new());
        let mut handlers = vec![];

        for i in 0..10 {
            let q = q.clone();
            handlers.push(thread::spawn(move || {
                for j in 0..1000 {
                    q.push(i * 1000 + j);
                }
            }));
        }

        for h in handlers {
            h.join().unwrap();
        }

        let mut all = Vec::new();
        while let Some(v) = q.pop() {
            all.push(v);
        }

        all.sort();
        assert_eq!(all.len(), 10000);
        for i in 0..10000 {
            assert_eq!(all[i], i);
        }
    }
}
