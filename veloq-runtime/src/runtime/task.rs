use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Weak;
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::io::driver::RemoteWaker;
use crate::runtime::executor::ExecutorShared;

pub(crate) struct Task {
    future: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    policy: SchedulePolicy,
}

pub(crate) enum SchedulePolicy {
    Local {
        owner_id: usize,
        local_queue: Weak<RefCell<VecDeque<Arc<Task>>>>,
        shared: Arc<ExecutorShared>,
    },
}

// SAFETY:
// 1. `future` is accessed only by the owner thread (guaranteed by logic in run()).
// 2. `policy` is immutable after creation.
// 3. `SchedulePolicy::Local` fields:
//    - `owner_id`: usize, Copy, Send, Sync.
//    - `local_queue`: Weak<...>, not Send/Sync normally. But we only access it
//      if `is_current_worker(owner_id)` returns true.
//    - `shared`: Arc<ExecutorShared>, Send + Sync.
// We must implement Send + Sync manually and ensure safety invariant.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl Task {
    pub(crate) fn new(
        future: impl Future<Output = ()> + 'static,
        owner_id: usize,
        local_queue: Weak<RefCell<VecDeque<Arc<Task>>>>,
        shared: Arc<ExecutorShared>,
    ) -> Arc<Self> {
        Self::from_boxed(Box::pin(future), owner_id, local_queue, shared)
    }

    pub(crate) fn from_boxed(
        future: Pin<Box<dyn Future<Output = ()>>>,
        owner_id: usize,
        local_queue: Weak<RefCell<VecDeque<Arc<Task>>>>,
        shared: Arc<ExecutorShared>,
    ) -> Arc<Self> {
        Arc::new(Self {
            future: UnsafeCell::new(Some(future)),
            policy: SchedulePolicy::Local {
                owner_id,
                local_queue,
                shared,
            },
        })
    }

    pub(crate) fn run(self: Arc<Self>) {
        // SAFETY: Only the owner thread calls run() on a Task.
        // We can verify this with debug_assert if we want.
        let future_slot = unsafe { &mut *self.future.get() };
        if let Some(future) = future_slot.as_mut() {
            let waker = waker(self.clone());
            let mut cx = Context::from_waker(&waker);
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(_) => {
                    *future_slot = None;
                }
                Poll::Pending => {}
            }
        }
    }
}

// Waker vtable implementation
const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let task = unsafe { Arc::from_raw(ptr as *const Task) };
    std::mem::forget(task.clone()); // Increment count for new waker
    std::mem::forget(task); // Keep original valid
    RawWaker::new(ptr, &VTABLE)
}

unsafe fn wake(ptr: *const ()) {
    let task = unsafe { Arc::from_raw(ptr as *const Task) };
    wake_task(&task);
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let task = unsafe { Arc::from_raw(ptr as *const Task) };
    let task_clone = task.clone();
    std::mem::forget(task);
    wake_task(&task_clone);
}

unsafe fn drop_waker(ptr: *const ()) {
    let _ = unsafe { Arc::from_raw(ptr as *const Task) }; // Decrement count
}

fn wake_task(task: &Arc<Task>) {
    match &task.policy {
        SchedulePolicy::Local {
            owner_id,
            local_queue,
            shared,
        } => {
            if crate::runtime::context::is_current_worker(*owner_id) {
                if let Some(queue) = local_queue.upgrade() {
                    queue.borrow_mut().push_back(task.clone());
                }
            } else {
                let _ = shared.remote_queue.send(task.clone());
                let _ = shared.waker.wake();
            }
        }
    }
}

pub(crate) fn waker(task: Arc<Task>) -> Waker {
    let ptr = Arc::into_raw(task) as *const ();
    let raw = RawWaker::new(ptr, &VTABLE);
    unsafe { Waker::from_raw(raw) }
}
