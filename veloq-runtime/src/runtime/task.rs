use std::alloc::{self, Layout};
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::io::driver::RemoteWaker;
use crate::runtime::executor::ExecutorShared;

/// The state of the Task in its lifecycle.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Created, waiting in a queue (injector or local). Context not yet bound.
    Spawned = 0,
    /// Bound to a worker, ready to be polled.
    Bound = 1,
    /// Currently being polled.
    Running = 2,
    /// Completed or Dropped.
    Dead = 3,
}

/// A handle to a bound task (ready to run).
///
/// This struct wraps a pointer to a heap-allocated memory block containing both
/// the metadata (Header) and the Future itself.
///
/// Layout: [ Header ] [ Future ]
#[repr(transparent)]
pub struct Task {
    ptr: NonNull<Header>,
}

unsafe impl Send for Task {}
unsafe impl Sync for Task {}

/// A handle to a newly spawned task (not yet bound to an executor).
#[repr(transparent)]
pub struct SpawnedTask {
    ptr: NonNull<Header>,
}

unsafe impl Send for SpawnedTask {}
unsafe impl Sync for SpawnedTask {}

struct Header {
    /// Current state of the task.
    state: AtomicUsize,

    /// Reference count.
    /// 1 for the Task handle.
    /// +N for Wakers.
    references: AtomicUsize,

    vtable: &'static TaskVTable,

    // --- Late-Bound Context ---
    // Mutable via UnsafeCell, synchronized by lifecycle transitions (Spawned -> Bound).
    owner_id: UnsafeCell<usize>,

    // We use a Weak pointer to the queue inside UnsafeCell.
    queue: UnsafeCell<Option<std::rc::Weak<std::cell::RefCell<std::collections::VecDeque<Task>>>>>,

    shared: UnsafeCell<Option<Arc<ExecutorShared>>>,
}

struct TaskVTable {
    poll: unsafe fn(NonNull<Header>),
    drop_future: unsafe fn(NonNull<Header>),
    dealloc: unsafe fn(NonNull<Header>),
}

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
        let vtable = &TaskVTable {
            poll: poll_future::<F>,
            drop_future: drop_future::<F>,
            dealloc: dealloc_task::<F>,
        };

        let header = Header {
            state: AtomicUsize::new(Lifecycle::Spawned as usize),
            references: AtomicUsize::new(1),
            vtable,
            owner_id: UnsafeCell::new(usize::MAX),
            queue: UnsafeCell::new(None),
            shared: UnsafeCell::new(None),
        };

        let ptr = unsafe { alloc_task(future, header) };
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
            *header.owner_id.get() = id;
            *header.queue.get() = Some(queue);
            *header.shared.get() = Some(shared);

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

impl Task {
    /// Run the task.
    /// Creates a Waker and polls the future.
    /// Consumes the Task handle (ownership transfer to the poll/waker cycle).
    pub(crate) fn run(self) {
        let ptr = self.ptr;
        mem::forget(self); // Do not run Drop for Task, referencing is handed off to poll

        unsafe {
            (ptr.as_ref().vtable.poll)(ptr);
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        unsafe { drop_reference(self.ptr) }
    }
}

impl Drop for SpawnedTask {
    fn drop(&mut self) {
        unsafe { drop_reference(self.ptr) }
    }
}

// --- Layout & Helper Functions ---

#[repr(C)]
struct TaskRaw<F> {
    header: Header,
    future: UnsafeCell<Option<F>>,
}

unsafe fn alloc_task<F>(future: F, header: Header) -> NonNull<Header> {
    let layout = Layout::new::<TaskRaw<F>>();
    unsafe {
        let ptr = alloc::alloc(layout) as *mut TaskRaw<F>;
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }

        ptr.write(TaskRaw {
            header,
            future: UnsafeCell::new(Some(future)),
        });

        NonNull::new_unchecked(ptr as *mut Header)
    }
}

unsafe fn dealloc_task<F>(ptr: NonNull<Header>) {
    unsafe {
        let ptr = ptr.cast::<TaskRaw<F>>().as_ptr();
        let layout = Layout::new::<TaskRaw<F>>();
        alloc::dealloc(ptr as *mut u8, layout);
    }
}

unsafe fn drop_future<F>(ptr: NonNull<Header>) {
    unsafe {
        let raw = ptr.cast::<TaskRaw<F>>().as_ref();
        *raw.future.get() = None;
    }
}

unsafe fn poll_future<F: Future<Output = ()>>(ptr: NonNull<Header>) {
    let header = unsafe { ptr.as_ref() };

    // CAS Loop to transition from Bound -> Running
    loop {
        let state = header.state.load(Ordering::Acquire);

        if state == Lifecycle::Dead as usize {
            // Already dead, drop the executed reference
            unsafe { drop_reference(ptr) };
            return;
        }

        if state == Lifecycle::Running as usize {
            // Contention: Task is already running on another thread.
            // We must reschedule it to run later.
            // We transfer the ownership of this reference (ptr) back to the queue.
            let queue_ptr = header.queue.get();
            if let Some(weak_queue) = unsafe { &*queue_ptr } {
                if let Some(queue) = weak_queue.upgrade() {
                    let task = Task { ptr };
                    queue.borrow_mut().push_back(task);
                    return;
                }
            }
            // If queue is gone, we have to drop.
            unsafe { drop_reference(ptr) };
            return;
        }

        // Try to lock (Bound -> Running)
        if header
            .state
            .compare_exchange(
                state,
                Lifecycle::Running as usize,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            break;
        }
        // CAS failed, retry
    }

    // --- Critical Section ---
    let slot = unsafe {
        let raw = ptr.cast::<TaskRaw<F>>().as_ref();
        &mut *raw.future.get()
    };

    if let Some(future) = slot {
        let waker = unsafe { waker_from_ptr(ptr) };
        let mut cx = Context::from_waker(&waker);
        let pinned = unsafe { Pin::new_unchecked(future) };

        match pinned.poll(&mut cx) {
            Poll::Ready(_) => {
                *slot = None;
                header
                    .state
                    .store(Lifecycle::Dead as usize, Ordering::Release);
            }
            Poll::Pending => {
                header
                    .state
                    .store(Lifecycle::Bound as usize, Ordering::Release);
            }
        }
    } else {
        header
            .state
            .store(Lifecycle::Dead as usize, Ordering::Release);
    }

    unsafe { drop_reference(ptr) };
}

unsafe fn drop_reference(ptr: NonNull<Header>) {
    unsafe {
        let header = ptr.as_ref();
        if header.references.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);

            (header.vtable.drop_future)(ptr);

            let shared_ptr = header.shared.get();
            if let Some(shared) = (*shared_ptr).take() {
                drop(shared);
            }

            let queue_ptr = header.queue.get();
            if let Some(queue) = (*queue_ptr).take() {
                drop(queue);
            }

            (header.vtable.dealloc)(ptr);
        }
    }
}

// --- Waker ---

const WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(unsafe_clone, unsafe_wake, unsafe_wake_by_ref, unsafe_drop);

unsafe fn waker_from_ptr(ptr: NonNull<Header>) -> Waker {
    unsafe {
        ptr.as_ref().references.fetch_add(1, Ordering::Relaxed);
        let raw = RawWaker::new(ptr.as_ptr() as *const (), &WAKER_VTABLE);
        Waker::from_raw(raw)
    }
}

unsafe fn unsafe_clone(ptr: *const ()) -> RawWaker {
    unsafe {
        let header = &*(ptr as *const Header);
        header.references.fetch_add(1, Ordering::Relaxed);
        RawWaker::new(ptr, &WAKER_VTABLE)
    }
}

unsafe fn unsafe_wake(ptr: *const ()) {
    unsafe {
        let non_null = NonNull::new_unchecked(ptr as *mut Header);
        wake_task(non_null);
        unsafe_drop(ptr);
    }
}

unsafe fn unsafe_wake_by_ref(ptr: *const ()) {
    unsafe {
        let non_null = NonNull::new_unchecked(ptr as *mut Header);
        wake_task(non_null);
    }
}

unsafe fn unsafe_drop(ptr: *const ()) {
    unsafe {
        let ptr = NonNull::new_unchecked(ptr as *mut Header);
        drop_reference(ptr);
    }
}

unsafe fn wake_task(ptr: NonNull<Header>) {
    let header = unsafe { ptr.as_ref() };

    // If running logic ensures binding, we can safely access owner_id.
    let owner_id = unsafe { *header.owner_id.get() };

    // Check if handling locally
    let is_local = crate::runtime::context::is_current_worker(owner_id);

    if is_local {
        // Push to local queue
        let queue_ptr = header.queue.get();
        if let Some(weak_queue) = unsafe { &*queue_ptr } {
            if let Some(queue) = weak_queue.upgrade() {
                // Here we need to reconstruct the Task struct to push it into the queue.
                // Since `Task` is just a pointer, we can re-create it.
                // However, we must INCREASE the reference count because VecDeque will OWN this Task.
                header.references.fetch_add(1, Ordering::Relaxed);

                let task = Task { ptr };

                queue.borrow_mut().push_back(task);
            }
        }
    } else {
        // Remote Wake
        let shared_ptr = header.shared.get();
        if let Some(shared) = unsafe { &*shared_ptr } {
            // Need to push to remote_queue.
            header.references.fetch_add(1, Ordering::Relaxed);
            let task = Task { ptr };

            let _ = shared.remote_queue.send(task);
            let _ = shared.waker.wake();
        }
    }
}
