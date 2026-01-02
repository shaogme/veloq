use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::io::driver::{Driver, PlatformDriver};
use crate::runtime::join::LocalJoinHandle;
use crate::runtime::task::Task;

use crate::io::buffer::BufPool;
use crate::io::driver::RemoteWaker;
use crate::runtime::context::RuntimeContext;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_queue::SegQueue;

pub type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Global spawner that can schedule tasks to workers
#[derive(Clone)]
pub struct Spawner {
    workers: Arc<Mutex<Vec<WorkerHandle>>>,
    next: Arc<AtomicUsize>, // For round-robin scheduling
}

struct WorkerHandle {
    injector: Arc<SegQueue<Job>>,
    waker: Arc<dyn RemoteWaker>,
    injected_load: Arc<AtomicUsize>,
    local_load: Arc<AtomicUsize>,
}

impl Spawner {
    pub fn spawn<F>(&self, future: F) -> crate::runtime::join::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (handle, producer) = crate::runtime::join::JoinHandle::new();

        let job: Job = Box::pin(async move {
            let output = future.await;
            producer.set(output);
        });

        let workers = self.workers.lock().unwrap();
        let worker_count = workers.len();
        if worker_count == 0 {
            panic!("No workers available in runtime");
        }

        // Power of Two Choices (P2C)
        // We use the atomic counter 'next' as a source of randomness
        let seed = self.next.fetch_add(1, Ordering::Relaxed);

        // Simple hash to get two distinct-ish indices
        let idx1 = seed % worker_count;
        let idx2 = {
            let mut x = seed;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x % worker_count
        };

        let worker1 = &workers[idx1];
        let worker2 = &workers[idx2];

        let load1 = worker1.injected_load.load(Ordering::Relaxed)
            + worker1.local_load.load(Ordering::Relaxed);
        let load2 = worker2.injected_load.load(Ordering::Relaxed)
            + worker2.local_load.load(Ordering::Relaxed);

        let worker = if load1 <= load2 { worker1 } else { worker2 };

        // Push and wake
        worker.injector.push(job);
        worker.injected_load.fetch_add(1, Ordering::Relaxed);
        worker.waker.wake().expect("Failed to wake worker");

        handle
    }
}

pub struct LocalExecutor<P: BufPool> {
    driver: Rc<RefCell<PlatformDriver<P>>>,
    queue: Rc<RefCell<VecDeque<Rc<Task>>>>,
    buffer_pool: Rc<P>,
    injector: Option<Arc<SegQueue<Job>>>,
    injected_load: Option<Arc<AtomicUsize>>,
    local_load: Option<Arc<AtomicUsize>>,
    spawner: Option<Spawner>,
}

impl<P: BufPool> LocalExecutor<P> {
    pub fn driver_handle(&self) -> std::rc::Weak<RefCell<PlatformDriver<P>>> {
        Rc::downgrade(&self.driver)
    }

    /// Create a new local executor for testing (no global injection).
    pub fn new(pool: P) -> Self {
        Self::create_internal(
            None,
            None,
            None,
            None,
            None,
            pool,
            crate::config::Config::default(),
        )
    }

    /// Create a new local executor with global injection support.
    pub fn with_injector(
        injector: Arc<SegQueue<Job>>,
        injected_load: Arc<AtomicUsize>,
        local_load: Arc<AtomicUsize>,
        spawner: Spawner,
        pool: P,
        config: crate::config::Config,
    ) -> Self {
        Self::create_internal(
            None,
            Some(injector),
            Some(injected_load),
            Some(local_load),
            Some(spawner),
            pool,
            config,
        )
    }

    /// Create a new local executor from an existing driver (used for worker threads).
    pub fn from_driver(
        driver: PlatformDriver<P>,
        injector: Arc<SegQueue<Job>>,
        injected_load: Arc<AtomicUsize>,
        local_load: Arc<AtomicUsize>,
        spawner: Spawner,
        pool: P,
    ) -> Self {
        Self::create_internal(
            Some(driver),
            Some(injector),
            Some(injected_load),
            Some(local_load),
            Some(spawner),
            pool,
            crate::config::Config::default(),
        )
    }

    fn create_internal(
        driver: Option<PlatformDriver<P>>,
        injector: Option<Arc<SegQueue<Job>>>,
        injected_load: Option<Arc<AtomicUsize>>,
        local_load: Option<Arc<AtomicUsize>>,
        spawner: Option<Spawner>,
        pool: P,
        config: crate::config::Config,
    ) -> Self {
        // Initialize Driver with config if not provided
        let driver = Rc::new(RefCell::new(driver.unwrap_or_else(|| {
            PlatformDriver::new(&config).expect("Failed to create driver")
        })));
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        let buffer_pool = Rc::new(pool);

        // Register buffer pool with driver
        driver
            .borrow_mut()
            .register_buffer_pool(buffer_pool.as_ref())
            .expect("Failed to register buffer pool");

        Self {
            driver,
            queue,
            buffer_pool,
            injector,
            injected_load,
            local_load,
            spawner,
        }
    }

    pub fn spawn_local<F, T>(&self, future: F) -> LocalJoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (handle, producer) = LocalJoinHandle::new();
        let task = Task::new(
            async move {
                let output = future.await;
                producer.set(output);
            },
            Rc::downgrade(&self.queue),
        );
        self.queue.borrow_mut().push_back(task);
        if let Some(load) = &self.local_load {
            load.fetch_add(1, Ordering::Relaxed);
        }
        handle
    }

    pub fn block_on<F, Fut>(&self, func: F) -> Fut::Output
    where
        F: FnOnce(&RuntimeContext<P>) -> Fut,
        Fut: Future,
    {
        // Create the runtime context to pass to the future factory
        let context = RuntimeContext::new(
            Rc::downgrade(&self.driver),
            Rc::downgrade(&self.queue),
            Rc::downgrade(&self.buffer_pool),
            self.spawner.clone(),
        );

        let future = func(&context);
        let mut pinned_future = Box::pin(future);

        // Create a real waker for the main future.
        // When woken, it sets the flag to true so we know to poll the main future.
        let main_woken = Rc::new(RefCell::new(true)); // Start as true to ensure first poll
        let waker = main_task_waker(main_woken.clone());
        let mut cx = Context::from_waker(&waker);

        // Budget for batch polling to reduce syscall overhead
        const BUDGET: usize = 64;

        loop {
            let mut executed = 0;

            // Run a batch of tasks before checking IO to allow SQE batching
            while executed < BUDGET {
                let mut did_work = false;

                // 1. If main future was woken, poll it
                if *main_woken.borrow() {
                    *main_woken.borrow_mut() = false;
                    did_work = true;

                    if let Poll::Ready(val) = pinned_future.as_mut().poll(&mut cx) {
                        return val;
                    }
                }

                // 2. Refill: Check Injector if local queue is empty
                if self.queue.borrow().is_empty() {
                    if let Some(injector) = &self.injector {
                        // Optimistically drain injector to local queue
                        loop {
                            match injector.pop() {
                                Some(job) => {
                                    if let Some(load) = &self.injected_load {
                                        load.fetch_sub(1, Ordering::Relaxed);
                                    }
                                    if let Some(load) = &self.local_load {
                                        load.fetch_add(1, Ordering::Relaxed);
                                    }
                                    let task = Task::new(job, Rc::downgrade(&self.queue));
                                    self.queue.borrow_mut().push_back(task);
                                }
                                None => break,
                            }
                        }
                    }
                }

                // 3. Refill: Steal tasks if local queue is still empty
                if self.queue.borrow().is_empty() {
                    if let Some(spawner) = &self.spawner {
                        if let Ok(workers) = spawner.workers.try_lock() {
                            let worker_count = workers.len();
                            if worker_count > 1 {
                                let seed = self as *const _ as usize;
                                let start_idx = seed % worker_count;

                                for i in 0..worker_count {
                                    let idx = (start_idx + i) % worker_count;
                                    let target = &workers[idx];

                                    // Don't steal from self check handled implicitly by empty queue check + separate injector logic
                                    if let Some(job) = target.injector.pop() {
                                        target.injected_load.fetch_sub(1, Ordering::Relaxed);
                                        if let Some(load) = &self.local_load {
                                            load.fetch_add(1, Ordering::Relaxed);
                                        }
                                        let task = Task::new(job, Rc::downgrade(&self.queue));
                                        self.queue.borrow_mut().push_back(task);
                                        break; // Found one, loop back to execute
                                    }
                                }
                            }
                        }
                    }
                }

                // 4. Run ONE task
                let task = self.queue.borrow_mut().pop_front();
                if let Some(task) = task {
                    if let Some(load) = &self.local_load {
                        load.fetch_sub(1, Ordering::Relaxed);
                    }
                    task.run();
                    did_work = true;
                }

                if did_work {
                    executed += 1;
                } else {
                    // No work found in this attempt
                    break;
                }
            }

            // 5. Handle IO completions
            // If we did work, we might have generated IO requests.
            // If the queue is non-empty (budget exhausted), we just flush and poll casually.
            // If the queue is empty, we must block.
            let has_pending_tasks = !self.queue.borrow().is_empty() || *main_woken.borrow();

            let mut driver = self.driver.borrow_mut();
            if has_pending_tasks {
                // There are tasks waiting to run, don't block on IO, just flush and check
                // This call should be fast (non-blocking syscall or just queue logic)
                driver.submit_queue().unwrap();
                driver.process_completions();
            } else {
                // No tasks ready, we MUST wait for IO to make progress
                // This will block until at least one completion arrives
                driver.wait().unwrap();
            }
        }
    }
}

impl<P: BufPool + Default> Default for LocalExecutor<P> {
    fn default() -> Self {
        Self::new(P::default())
    }
}

/// A Thread-per-Core Runtime that manages multiple independent worker threads.
pub struct Runtime {
    handles: Vec<std::thread::JoinHandle<()>>,
    workers: Arc<Mutex<Vec<WorkerHandle>>>,
    config: Arc<crate::config::Config>,
}

impl Runtime {
    /// Create a new Runtime.
    pub fn new(config: crate::config::Config) -> Self {
        Self {
            handles: Vec::new(),
            workers: Arc::new(Mutex::new(Vec::new())),
            config: Arc::new(config),
        }
    }

    /// Spawn a new worker thread.
    ///
    /// The `future_factory` is called inside the new thread to create the main Future for that thread.
    /// It receives a `RuntimeContext` which can be used to spawn tasks.
    pub fn spawn_worker<F, Fut, P>(&mut self, future_factory: F)
    where
        P: BufPool + Default + 'static,
        F: FnOnce(RuntimeContext<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let injector = Arc::new(SegQueue::new());
        let injected_load = Arc::new(AtomicUsize::new(0));
        let local_load = Arc::new(AtomicUsize::new(0));

        // Solution 2: Executor creates driver. Signal back waker.
        let (waker_tx, waker_rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();

        let workers = self.workers.clone();

        // This spawner shares the same worker list
        let spawner = Spawner {
            workers: workers.clone(),
            next: Arc::new(AtomicUsize::new(0)),
        };

        let injector_clone = injector.clone();
        let injected_load_clone = injected_load.clone();
        let local_load_clone = local_load.clone();

        let config = self.config.clone();

        let handle = std::thread::spawn(move || {
            let executor = LocalExecutor::<P>::with_injector(
                injector_clone,
                injected_load_clone,
                local_load_clone,
                spawner,
                P::default(),
                (*config).clone(),
            );

            // Send waker back to main thread
            let waker = executor.driver.borrow().create_waker();
            waker_tx.send(waker).unwrap();

            // Wait for main thread to register us
            // This prevents "No workers available" race if the worker immediately spawns
            ack_rx
                .recv()
                .expect("Failed to receive ack from main thread");

            executor.block_on(move |cx| future_factory(cx.clone()));
        });

        let waker = waker_rx
            .recv()
            .expect("Failed to receive waker from worker");

        self.handles.push(handle);
        workers.lock().unwrap().push(WorkerHandle {
            injector,
            waker,
            injected_load,
            local_load,
        });

        // Notify worker it can start
        ack_tx.send(()).unwrap();
    }

    pub fn spawner(&self) -> Spawner {
        Spawner {
            workers: self.workers.clone(),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Spawn a future onto the runtime (round-robin)
    pub fn spawn<Fut>(&self, future: Fut) -> crate::runtime::join::JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        // Simple temporary spawner just for this call
        // Note: For frequent calls, one should cache spawner or use context access.
        // But Runtime::spawn() is typically top-level.
        let spawner = self.spawner();
        spawner.spawn(future)
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
