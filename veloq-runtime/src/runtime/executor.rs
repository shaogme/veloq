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
use std::sync::{Arc, RwLock};

use crossbeam_queue::SegQueue;

pub type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Handle to a remote executor, used for task injection and load monitoring.
#[derive(Clone)]
pub struct ExecutorHandle {
    pub(crate) injector: Arc<SegQueue<Job>>,
    pub(crate) waker: Arc<dyn RemoteWaker>,
    pub(crate) injected_load: Arc<AtomicUsize>,
    pub(crate) local_load: Arc<AtomicUsize>,
}

impl ExecutorHandle {
    pub fn total_load(&self) -> usize {
        self.injected_load.load(Ordering::Relaxed) + self.local_load.load(Ordering::Relaxed)
    }
}

/// A registry that maintains the set of all active executors.
/// Used for global task spawning (P2C) and work stealing.
pub struct ExecutorRegistry {
    workers: RwLock<Vec<ExecutorHandle>>,
    epoch: AtomicUsize, // Incremented whenever a new worker is added
    next: AtomicUsize,  // Source of randomness for P2C
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self {
            workers: RwLock::new(Vec::new()),
            epoch: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
        }
    }

    pub fn register(&self, handle: ExecutorHandle) {
        let mut workers = self.workers.write().unwrap();
        workers.push(handle);
        self.epoch.fetch_add(1, Ordering::Release);
    }

    pub fn current_epoch(&self) -> usize {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn get_all_handles(&self) -> Vec<ExecutorHandle> {
        self.workers.read().unwrap().clone()
    }

    /// Spawn a task using Power of Two Choices (P2C)
    pub fn spawn(&self, job: Job) {
        let workers = self.workers.read().unwrap();
        let count = workers.len();
        if count == 0 {
            panic!("No workers available in registry");
        }

        let seed = self.next.fetch_add(1, Ordering::Relaxed);
        let idx1 = seed % count;
        let idx2 = {
            let mut x = seed;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x % count
        };

        let w1 = &workers[idx1];
        let w2 = &workers[idx2]; // It's okay if idx1 == idx2

        let load1 = w1.total_load();
        let load2 = w2.total_load();

        let target = if load1 <= load2 { w1 } else { w2 };

        target.injector.push(job);
        target.injected_load.fetch_add(1, Ordering::Relaxed);
        target.waker.wake().expect("Failed to wake worker");
    }
}

/// Global spawner that acts as a frontend to the Registry.
#[derive(Clone)]
pub struct Spawner {
    registry: Arc<ExecutorRegistry>,
}

impl Spawner {
    pub fn new(registry: Arc<ExecutorRegistry>) -> Self {
        Self { registry }
    }

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

        self.registry.spawn(job);
        handle
    }
}

pub struct LocalExecutor<P: BufPool> {
    driver: Rc<RefCell<PlatformDriver<P>>>,
    queue: Rc<RefCell<VecDeque<Rc<Task>>>>,
    buffer_pool: Rc<P>,

    // Always present components for task injection
    injector: Arc<SegQueue<Job>>,
    injected_load: Arc<AtomicUsize>,
    local_load: Arc<AtomicUsize>,

    // Optional connection to the global registry
    registry: Option<Arc<ExecutorRegistry>>,

    // Local cache for work stealing (Lazy Cache)
    cached_peers: Vec<ExecutorHandle>,
    local_epoch: usize,
}

impl<P: BufPool> LocalExecutor<P> {
    pub fn driver_handle(&self) -> std::rc::Weak<RefCell<PlatformDriver<P>>> {
        Rc::downgrade(&self.driver)
    }

    /// Create a new standalone LocalExecutor.
    pub fn new(pool: P) -> Self {
        let config = crate::config::Config::default();
        Self::new_with_config(pool, config)
    }

    pub fn new_with_config(pool: P, config: crate::config::Config) -> Self {
        let driver = Rc::new(RefCell::new(
            PlatformDriver::<P>::new(&config).expect("Failed to create driver"),
        ));
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        let buffer_pool = Rc::new(pool);

        driver
            .borrow_mut()
            .register_buffer_pool(buffer_pool.as_ref())
            .expect("Failed to register buffer pool");

        Self {
            driver,
            queue,
            buffer_pool,
            injector: Arc::new(SegQueue::new()),
            injected_load: Arc::new(AtomicUsize::new(0)),
            local_load: Arc::new(AtomicUsize::new(0)),
            registry: None,
            cached_peers: Vec::new(),
            local_epoch: 0,
        }
    }

    /// Attach this executor to a registry.
    /// This enables the executor to steal tasks from others in the registry.
    pub fn with_registry(mut self, registry: Arc<ExecutorRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            injector: self.injector.clone(),
            waker: self.driver.borrow().create_waker(),
            injected_load: self.injected_load.clone(),
            local_load: self.local_load.clone(),
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
        self.local_load.fetch_add(1, Ordering::Relaxed);
        handle
    }

    pub fn block_on<F, Fut>(&mut self, func: F) -> Fut::Output
    where
        F: FnOnce(&RuntimeContext<P>) -> Fut,
        Fut: Future,
    {
        let spawner = self.registry.as_ref().map(|reg| Spawner::new(reg.clone()));

        let context = RuntimeContext::new(
            Rc::downgrade(&self.driver),
            Rc::downgrade(&self.queue),
            Rc::downgrade(&self.buffer_pool),
            spawner,
        );

        let future = func(&context);
        let mut pinned_future = Box::pin(future);
        let main_woken = Rc::new(RefCell::new(true));
        let waker = main_task_waker(main_woken.clone());
        let mut cx = Context::from_waker(&waker);

        const BUDGET: usize = 64;

        loop {
            let mut executed = 0;

            while executed < BUDGET {
                let mut did_work = false;

                // 1. Poll Main Future
                if *main_woken.borrow() {
                    *main_woken.borrow_mut() = false;
                    did_work = true;
                    if let Poll::Ready(val) = pinned_future.as_mut().poll(&mut cx) {
                        return val;
                    }
                }

                // 2. Poll Local Queue
                let task = self.queue.borrow_mut().pop_front();
                if let Some(task) = task {
                    self.local_load.fetch_sub(1, Ordering::Relaxed);
                    task.run();
                    executed += 1;
                    continue;
                }

                // 3. Poll Injector
                if let Some(job) = self.injector.pop() {
                    self.injected_load.fetch_sub(1, Ordering::Relaxed);
                    self.local_load.fetch_add(1, Ordering::Relaxed);
                    let task = Task::new(job, Rc::downgrade(&self.queue));
                    self.queue.borrow_mut().push_back(task);
                    // Loop back to run it
                    continue;
                }

                // 4. Steal from Registry (Work Stealing)
                if let Some(registry) = &self.registry {
                    // Lazy Cache Update (Epoch Check)
                    let current_epoch = registry.current_epoch();
                    if self.local_epoch != current_epoch {
                        self.cached_peers = registry.get_all_handles();
                        self.local_epoch = current_epoch;
                    }

                    if !self.cached_peers.is_empty() {
                        // Random start for stealing to reduce contention
                        let seed = self as *const _ as usize;
                        let start_idx = seed.wrapping_add(executed); // simple perturb
                        let count = self.cached_peers.len();

                        for i in 0..count {
                            let idx = (start_idx + i) % count;
                            let target = &self.cached_peers[idx];

                            // Don't steal from self
                            if Arc::ptr_eq(&target.injector, &self.injector) {
                                continue;
                            }

                            if let Some(job) = target.injector.pop() {
                                target.injected_load.fetch_sub(1, Ordering::Relaxed);
                                self.local_load.fetch_add(1, Ordering::Relaxed);
                                let task = Task::new(job, Rc::downgrade(&self.queue));
                                self.queue.borrow_mut().push_back(task);
                                did_work = true;
                                break; // Break cache loop, loop back to main while to run
                            }
                        }
                    }
                }

                if !did_work {
                    break;
                }
            }

            // 5. IO Wait
            let has_pending_tasks = !self.queue.borrow().is_empty() || *main_woken.borrow();
            let mut driver = self.driver.borrow_mut();
            if has_pending_tasks {
                driver.submit_queue().unwrap();
                driver.process_completions();
            } else {
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

pub struct Runtime {
    handles: Vec<std::thread::JoinHandle<()>>,
    registry: Arc<ExecutorRegistry>,
    config: Arc<crate::config::Config>,
}

impl Runtime {
    pub fn new(config: crate::config::Config) -> Self {
        Self {
            handles: Vec::new(),
            registry: Arc::new(ExecutorRegistry::new()),
            config: Arc::new(config),
        }
    }

    pub fn spawn_worker<F, Fut, P>(&mut self, future_factory: F)
    where
        P: BufPool + Default + 'static,
        F: FnOnce(RuntimeContext<P>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let registry = self.registry.clone();
        let config = self.config.clone();

        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();

        let thread_handle = std::thread::spawn(move || {
            let mut executor = LocalExecutor::<P>::new_with_config(P::default(), (*config).clone());
            executor = executor.with_registry(registry);

            handle_tx.send(executor.handle()).unwrap();

            // Wait for main thread to register us before we start
            let _ = ack_rx.recv();

            executor.block_on(move |cx| future_factory(cx.clone()));
        });

        let executor_handle = handle_rx.recv().expect("Worker thread failed to start");
        self.registry.register(executor_handle);
        let _ = ack_tx.send(());

        self.handles.push(thread_handle);
    }

    pub fn spawner(&self) -> Spawner {
        Spawner::new(self.registry.clone())
    }

    pub fn spawn<Fut>(&self, future: Fut) -> crate::runtime::join::JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawner().spawn(future)
    }

    pub fn block_on_all(self) {
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

// ============ Main Task Waker Implementation ============

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
    std::mem::forget(rc.clone());
    std::mem::forget(rc);
    RawWaker::new(ptr, &MAIN_WAKER_VTABLE)
}

unsafe fn main_waker_wake(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
    *rc.borrow_mut() = true;
}

unsafe fn main_waker_wake_by_ref(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
    *rc.borrow_mut() = true;
    std::mem::forget(rc);
}

unsafe fn main_waker_drop(ptr: *const ()) {
    let _ = unsafe { Rc::from_raw(ptr as *const RefCell<bool>) };
}
