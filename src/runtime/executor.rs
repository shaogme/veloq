use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::runtime::driver::{Driver, PlatformDriver};
use crate::runtime::join::JoinHandle;
use crate::runtime::task::Task;

pub struct LocalExecutor {
    driver: Rc<RefCell<PlatformDriver>>,
    queue: Rc<RefCell<VecDeque<Rc<Task>>>>,
}

impl LocalExecutor {
    pub fn driver_handle(&self) -> std::rc::Weak<RefCell<PlatformDriver>> {
        Rc::downgrade(&self.driver)
    }

    pub fn new() -> Self {
        // Initialize Driver with 1024 entries
        // Driver now initializes and registers its own BufferPool
        let driver = Rc::new(RefCell::new(
            PlatformDriver::new(1024).expect("Failed to create driver"),
        ));
        let queue = Rc::new(RefCell::new(VecDeque::new()));

        Self { driver, queue }
    }

    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (handle, producer) = JoinHandle::new();
        let task = Task::new(async move {
            let output = future.await;
            producer.set(output);
        }, Rc::downgrade(&self.queue));
        self.queue.borrow_mut().push_back(task);
        handle
    }

    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Enter the runtime context - this enables spawn() and current_driver()
        let _guard = crate::runtime::context::enter(
            Rc::downgrade(&self.driver),
            Rc::downgrade(&self.queue),
        );

        let mut pinned_future = Box::pin(future);

        // Create a real waker for the main future.
        // When woken, it sets the flag to true so we know to poll the main future.
        let main_woken = Rc::new(RefCell::new(true)); // Start as true to ensure first poll
        let waker = main_task_waker(main_woken.clone());
        let mut cx = Context::from_waker(&waker);

        loop {
            // 1. If main future was woken, poll it
            if *main_woken.borrow() {
                *main_woken.borrow_mut() = false;

                if let Poll::Ready(val) = pinned_future.as_mut().poll(&mut cx) {
                    return val;
                }
            }

            // 2. Run ONE spawned task (not all!) - this is key for fair scheduling
            let task = self.queue.borrow_mut().pop_front();
            if let Some(task) = task {
                task.run();
            }

            // 3. Handle IO completions
            let has_pending_tasks = !self.queue.borrow().is_empty() || *main_woken.borrow();

            let mut driver = self.driver.borrow_mut();
            if has_pending_tasks {
                // There are tasks waiting to run, don't block on IO
                driver.submit().unwrap();
                driver.process_completions();
            } else {
                // No tasks ready, we MUST wait for IO to make progress
                driver.wait().unwrap();
            }
        }
    }
}

/// A Thread-per-Core Runtime that manages multiple independent worker threads.
pub struct Runtime {
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Runtime {
    /// Create a new Runtime.
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    /// Spawn a new worker thread.
    ///
    /// The `future_factory` is called inside the new thread to create the main Future for that thread.
    /// This ensures that the Future (and any Task it spawns) is created within the thread's context.
    pub fn spawn_worker<F, Fut>(&mut self, future_factory: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let handle = std::thread::spawn(move || {
            let executor = LocalExecutor::new();
            executor.block_on(future_factory());
        });
        self.handles.push(handle);
    }

    /// Wait for all worker threads to complete.
    pub fn block_on_all(self) {
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

// ============ Main Task Waker Implementation ============
// This waker is used for the main future in block_on.
// When woken, it sets a flag so the executor knows to poll the main future.

fn main_task_waker(woken: Rc<RefCell<bool>>) -> Waker {
    let ptr = Rc::into_raw(woken) as *const ();
    let raw = RawWaker::new(ptr, &MAIN_WAKER_VTABLE);
    unsafe { Waker::from_raw(raw) }
}

const MAIN_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    main_waker_clone,
    main_waker_wake,
    main_waker_wake_by_ref,
    main_waker_drop,
);

unsafe fn main_waker_clone(ptr: *const ()) -> RawWaker {
    let rc = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
    std::mem::forget(rc.clone()); // Increment ref count for new waker
    std::mem::forget(rc); // Keep original valid
    RawWaker::new(ptr, &MAIN_WAKER_VTABLE)
}

unsafe fn main_waker_wake(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
    *rc.borrow_mut() = true;
    // wake() consumes the waker, so we don't forget - ref count decrements
}

unsafe fn main_waker_wake_by_ref(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
    *rc.borrow_mut() = true;
    std::mem::forget(rc); // Keep ref count since wake_by_ref doesn't consume
}

unsafe fn main_waker_drop(ptr: *const ()) {
    let _ = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) }; // Decrement ref count
}
