use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

pub(crate) struct Task {
    future: Mutex<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    queue: Weak<Mutex<VecDeque<Arc<Task>>>>,
}

impl Task {
    pub(crate) fn new(
        future: impl Future<Output = ()> + 'static,
        queue: Weak<Mutex<VecDeque<Arc<Task>>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            future: Mutex::new(Some(Box::pin(future))),
            queue,
        })
    }

    pub(crate) fn run(self: Arc<Self>) {
        let mut future_slot = self.future.lock().unwrap();
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
    let rc = unsafe { Arc::from_raw(ptr as *const Task) };
    std::mem::forget(rc.clone()); // Increment count for new waker
    std::mem::forget(rc); // Keep original valid
    RawWaker::new(ptr, &VTABLE)
}

unsafe fn wake(ptr: *const ()) {
    let rc = unsafe { Arc::from_raw(ptr as *const Task) };
    // self-schedule
    if let Some(queue) = rc.queue.upgrade() {
        queue.lock().unwrap().push_back(rc);
    }
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let rc = unsafe { Arc::from_raw(ptr as *const Task) };
    if let Some(queue) = rc.queue.upgrade() {
        queue.lock().unwrap().push_back(rc.clone());
    }
    std::mem::forget(rc);
}

unsafe fn drop_waker(ptr: *const ()) {
    let _ = unsafe { Arc::from_raw(ptr as *const Task) }; // Decrement count
}

pub(crate) fn waker(task: Arc<Task>) -> Waker {
    let ptr = Arc::into_raw(task) as *const ();
    let raw = RawWaker::new(ptr, &VTABLE);
    unsafe { Waker::from_raw(raw) }
}
