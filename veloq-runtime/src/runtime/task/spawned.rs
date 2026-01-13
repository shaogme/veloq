use std::cell::UnsafeCell;
use std::future::Future;
use std::mem;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::runtime::executor::ExecutorShared;
use crate::runtime::task::{
    self, Lifecycle, PinnedData, PinnedVTable, Task, dealloc_task_impl, drop_future_impl,
    poll_future_impl, raw,
};

/// A handle to a newly spawned task (not yet bound to an executor).
#[repr(transparent)]
pub struct SpawnedTask {
    pub(crate) ptr: NonNull<raw::Header<PinnedData, PinnedVTable>>,
}

unsafe impl Send for SpawnedTask {}
unsafe impl Sync for SpawnedTask {}

impl SpawnedTask {
    /// Create a new local Task. Future is not required to be Send.
    ///
    /// # Safety
    /// The caller must ensure that this Task is NEVER sent to another thread.
    /// This is typically ensured by pushing it only to the LocalExecutor's private queue.
    #[inline]
    pub(crate) unsafe fn new_local<F>(future: F) -> Self
    where
        F: Future<Output = ()> + 'static,
    {
        unsafe { Self::new_unchecked(future) }
    }

    /// Internal helper to create task without Send bound check.
    pub(crate) unsafe fn new_unchecked<F>(future: F) -> Self
    where
        F: Future<Output = ()> + 'static,
    {
        let vtable = &PinnedVTable {
            poll: poll_future_impl::<F>,
            drop_future: drop_future_impl::<F>,
            dealloc: dealloc_task_impl::<F>,
        };

        let data = PinnedData {
            owner_id: UnsafeCell::new(usize::MAX),
            queue: UnsafeCell::new(None),
            shared: UnsafeCell::new(None),
        };

        let ptr = unsafe { raw::alloc_task(future, data, vtable, Lifecycle::Spawned as usize) };
        SpawnedTask { ptr }
    }

    /// Bind the task to a specific worker context.
    /// Must be called before `run()`.
    /// Returns a bound `Task`.
    pub(crate) unsafe fn bind(
        self,
        id: usize,
        queue: std::rc::Weak<std::cell::RefCell<std::collections::VecDeque<Task>>>,
        shared: Arc<ExecutorShared>,
    ) -> Task {
        unsafe {
            let header = self.ptr.as_ref();

            // Update context fields
            *header.data.owner_id.get() = id;
            *header.data.queue.get() = Some(queue);
            *header.data.shared.get() = Some(shared);

            // Transition state to Bound
            header
                .state
                .store(Lifecycle::Bound as usize, Ordering::Release);

            let ptr = self.ptr;
            mem::forget(self); // Prevent Drop of SpawnedTask, ownership is transferred to Task
            Task { ptr }
        }
    }
}

impl Drop for SpawnedTask {
    fn drop(&mut self) {
        unsafe { task::drop_reference(self.ptr) }
    }
}
