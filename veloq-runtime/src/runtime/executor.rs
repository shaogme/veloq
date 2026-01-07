use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;

use crate::io::buffer::BufPool;
use crate::io::driver::{Driver, PlatformDriver, RemoteWaker};
use crate::runtime::context::RuntimeContext;
pub(crate) use crate::runtime::executor::spawner::{CachePadded, ExecutorShared};
use crate::runtime::join::LocalJoinHandle;
use crate::runtime::mesh::{self, Consumer, Producer};
use crate::runtime::task::Task;

// Re-export common types from spawner which acts as the definition source for these
pub use self::spawner::{ExecutorHandle, ExecutorRegistry, Job, MeshContext, Spawner};

pub mod spawner;

// ============ LocalExecutor Implementation ============

pub struct LocalExecutorBuilder {
    config: crate::config::Config,
    shared: Option<Arc<ExecutorShared>>,
}

impl Default for LocalExecutorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalExecutorBuilder {
    pub fn new() -> Self {
        Self {
            config: crate::config::Config::default(),
            shared: None,
        }
    }

    pub(crate) fn with_shared(mut self, shared: Arc<ExecutorShared>) -> Self {
        self.shared = Some(shared);
        self
    }

    pub fn config(mut self, config: crate::config::Config) -> Self {
        self.config = config;
        self
    }

    /// Build the LocalExecutor.
    /// Buffer pool management is now external to the executor.
    pub fn build(self) -> LocalExecutor {
        #[allow(unused_mut)]
        let mut driver = PlatformDriver::new(&self.config).expect("Failed to create driver");

        let queue = Rc::new(RefCell::new(VecDeque::new()));
        let waker = driver.create_waker();

        let shared = self.shared.unwrap_or_else(|| {
            Arc::new(ExecutorShared {
                injector: SegQueue::new(),
                pinned: SegQueue::new(),
                waker: crate::runtime::executor::spawner::LateBoundWaker::new(),
                injected_load: CachePadded(AtomicUsize::new(0)),
                local_load: CachePadded(AtomicUsize::new(0)),
            })
        });

        // Bind the driver's waker to the shared state (Late Binding)
        shared.waker.set(waker);

        LocalExecutor {
            driver: Rc::new(RefCell::new(driver)),
            queue,
            shared,
            registry: None,
            mesh: None,
        }
    }
}

pub struct LocalExecutor {
    driver: Rc<RefCell<PlatformDriver>>,
    queue: Rc<RefCell<VecDeque<Rc<Task>>>>,

    // Shared components
    shared: Arc<ExecutorShared>,

    // Optional connection to the global registry
    registry: Option<Arc<ExecutorRegistry>>,

    // Mesh Networking
    mesh: Option<Rc<RefCell<MeshContext>>>,
}

impl LocalExecutor {
    pub fn driver_handle(&self) -> std::rc::Weak<RefCell<PlatformDriver>> {
        Rc::downgrade(&self.driver)
    }

    /// Create a new builder for LocalExecutor.
    pub fn builder() -> LocalExecutorBuilder {
        LocalExecutorBuilder::new()
    }

    /// Create a new standalone LocalExecutor.
    /// Note: This will NOT register any buffer pool with the driver on Linux.
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Create a new standalone LocalExecutor with config.
    /// Note: This will NOT register any buffer pool with the driver on Linux.
    pub fn new_with_config(config: crate::config::Config) -> Self {
        Self::builder().config(config).build()
    }

    /// Attach this executor to a registry.
    /// This enables the executor to steal tasks from others in the registry.
    pub fn with_registry(mut self, registry: Arc<ExecutorRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn handle(&self) -> ExecutorHandle {
        let id = self
            .mesh
            .as_ref()
            .map(|m| m.borrow().id)
            .unwrap_or(usize::MAX);
        ExecutorHandle {
            id,
            shared: self.shared.clone(),
        }
    }

    /// Register buffers from a buffer pool with the underlying driver.
    ///
    /// This is primarily for io_uring on Linux to register fixed buffers.
    /// On other platforms, this may be a no-op.
    pub fn register_buffers(&self, pool: &dyn BufPool) {
        #[cfg(target_os = "linux")]
        {
            let bufs = pool.get_registration_buffers();
            self.driver
                .borrow_mut()
                .register_buffers(&bufs)
                .expect("Failed to register buffer pool");
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pool;
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
        self.shared.local_load.0.fetch_add(1, Ordering::Relaxed);
        handle
    }

    pub(crate) fn attach_mesh(
        &mut self,
        id: usize,
        state: Arc<AtomicU8>,
        ingress: Vec<Consumer<Job>>,
        egress: Vec<Producer<Job>>,
        peer_handles: Arc<Vec<AtomicUsize>>,
    ) {
        let mesh = MeshContext {
            id,
            state,
            ingress,
            egress,
            peer_handles,
        };
        self.mesh = Some(Rc::new(RefCell::new(mesh)));
    }

    fn enqueue_job(&self, job_factory: Job) {
        self.shared.local_load.0.fetch_add(1, Ordering::Relaxed);
        let future = job_factory();
        let task = Task::from_boxed(future, Rc::downgrade(&self.queue));
        self.queue.borrow_mut().push_back(task);
    }

    fn try_poll_mesh(&self) -> bool {
        if let Some(mesh_rc) = &self.mesh {
            let mut mesh = mesh_rc.borrow_mut();
            return mesh.poll_ingress(|job| self.enqueue_job(job));
        }
        false
    }

    fn try_poll_injector(&self) -> bool {
        // Check pinned first (strictly specific to this worker)
        if let Some(job) = self.shared.pinned.pop() {
            self.shared.injected_load.0.fetch_sub(1, Ordering::Relaxed);
            self.enqueue_job(job);
            return true;
        }

        if let Some(job) = self.shared.injector.pop() {
            self.shared.injected_load.0.fetch_sub(1, Ordering::Relaxed);
            self.enqueue_job(job);
            return true;
        }
        false
    }

    fn try_steal(&self, executed: usize) -> bool {
        if let Some(registry) = &self.registry {
            let workers = registry.all();
            let count = workers.len();

            if count > 0 {
                let seed = self as *const _ as usize;
                let start_idx = seed.wrapping_add(executed);

                for i in 0..count {
                    let idx = (start_idx + i) % count;
                    let target = &workers[idx];

                    if Arc::ptr_eq(&target.shared, &self.shared) {
                        continue;
                    }

                    // Steal from injector
                    if let Some(job) = target.shared.injector.pop() {
                        target
                            .shared
                            .injected_load
                            .0
                            .fetch_sub(1, Ordering::Relaxed);
                        self.enqueue_job(job);
                        return true;
                    }
                }
            }
        }
        false
    }

    fn park_and_wait(&self, main_woken: &AtomicBool) {
        let has_pending_tasks =
            !self.queue.borrow().is_empty() || main_woken.load(Ordering::Acquire);
        let mut driver = self.driver.borrow_mut();

        if has_pending_tasks {
            driver.submit_queue().unwrap();
            driver.process_completions();
        } else {
            let mut can_park = true;

            if let Some(mesh_rc) = &self.mesh {
                let mut mesh = mesh_rc.borrow_mut();
                // 1. Set PARKING
                mesh.state.store(mesh::PARKING, Ordering::Release);

                // 2. Poll Mesh (Double check)
                if mesh.poll_ingress(|job| self.enqueue_job(job)) {
                    mesh.state.store(mesh::RUNNING, Ordering::Relaxed);
                    can_park = false;
                } else {
                    // 3. Commit PARKED
                    mesh.state.store(mesh::PARKED, Ordering::Release);
                }
            }

            if can_park {
                driver.wait().unwrap();
            }

            // Restore RUNNING
            if let Some(mesh_rc) = &self.mesh {
                let mesh = mesh_rc.borrow();
                mesh.state.store(mesh::RUNNING, Ordering::Release);
            }
        }
    }

    /// Run the executor loop indefinitely.
    /// Used by worker threads in the Runtime.
    pub fn run(&self) {
        let spawner = self.registry.as_ref().map(|reg| Spawner::new(reg.clone()));

        // Pass the Mesh context if available
        let mesh_weak = self.mesh.as_ref().map(Rc::downgrade);

        let context = RuntimeContext::new(
            self.driver_handle(),
            Rc::downgrade(&self.queue),
            spawner,
            mesh_weak,
            self.handle(),
        );

        let _guard = crate::runtime::context::enter(context);

        // Auto-Register (Passive Scan)
        if let Some(pool) = crate::runtime::context::current_pool() {
            crate::runtime::context::current().register_buffers(&pool);
        }

        // Register Mesh Handle
        if let Some(mesh_rc) = &self.mesh {
            let mesh = mesh_rc.borrow();
            let handle = self.driver.borrow().inner_handle();
            mesh.peer_handles[mesh.id].store(handle as usize, Ordering::Release);
        }

        let main_woken = Arc::new(AtomicBool::new(false));

        const BUDGET: usize = 64;

        loop {
            let mut executed = 0;

            while executed < BUDGET {
                let mut did_work = false;

                // 0. Poll Mesh (Highest Priority)
                if self.try_poll_mesh() {
                    did_work = true;
                }

                // 1. Poll Main Future (Skipped for worker)

                // 2. Poll Local Queue
                let task = self.queue.borrow_mut().pop_front();
                if let Some(task) = task {
                    self.shared.local_load.0.fetch_sub(1, Ordering::Relaxed);
                    task.run();
                    executed += 1;
                    continue;
                }

                // 3. Poll Injector
                if self.try_poll_injector() {
                    continue;
                }

                // 4. Steal from Registry
                if self.try_steal(executed) {
                    did_work = true;
                }

                if !did_work {
                    break;
                }
            }

            // 5. IO Wait & Park
            self.park_and_wait(&main_woken);
        }
    }

    pub fn block_on<F>(&mut self, future: F) -> F::Output
    where
        F: Future,
    {
        let spawner = self.registry.as_ref().map(|reg| Spawner::new(reg.clone()));

        // Pass the Mesh context if available
        let mesh_weak = self.mesh.as_ref().map(Rc::downgrade);

        let context = RuntimeContext::new(
            self.driver_handle(),
            Rc::downgrade(&self.queue),
            spawner,
            mesh_weak,
            self.handle(),
        );

        let _guard = crate::runtime::context::enter(context);

        // Auto-Register (Passive Scan)
        if let Some(pool) = crate::runtime::context::current_pool() {
            crate::runtime::context::current().register_buffers(&pool);
        }

        // Register Mesh Handle
        if let Some(mesh_rc) = &self.mesh {
            let mesh = mesh_rc.borrow();
            let handle = self.driver.borrow().inner_handle();
            mesh.peer_handles[mesh.id].store(handle as usize, Ordering::Release);
        }

        let mut pinned_future = Box::pin(future);
        let remote_waker = self.driver.borrow().create_waker();
        let state = Arc::new(AtomicWakerState {
            flag: AtomicBool::new(true),
            remote: remote_waker,
        });
        let waker = waker_from_state(state.clone());
        let mut cx = Context::from_waker(&waker);

        const BUDGET: usize = 64;

        loop {
            let mut executed = 0;

            while executed < BUDGET {
                let mut did_work = false;

                // 0. Poll Mesh (Highest Priority)
                if self.try_poll_mesh() {
                    did_work = true;
                }

                // 1. Poll Main Future
                if state.flag.swap(false, Ordering::AcqRel) {
                    did_work = true;
                    executed += 1;
                    if let Poll::Ready(val) = pinned_future.as_mut().poll(&mut cx) {
                        return val;
                    }
                }

                // 2. Poll Local Queue
                let task = self.queue.borrow_mut().pop_front();
                if let Some(task) = task {
                    self.shared.local_load.0.fetch_sub(1, Ordering::Relaxed);
                    // task is Arc<Task>, run() takes self: Arc<Task>
                    task.run();
                    executed += 1;
                    continue;
                }

                // 3. Poll Injector
                if self.try_poll_injector() {
                    continue;
                }

                // 4. Steal from Registry
                if self.try_steal(executed) {
                    did_work = true;
                }

                if !did_work {
                    break;
                }
            }

            // 5. IO Wait & Park
            self.park_and_wait(&state.flag);
        }
    }
}

impl Drop for LocalExecutor {
    fn drop(&mut self) {
        // Clear the task queue to drop all futures.
        // This explicitly drops tasks (and their buffers/sockets) before the driver is dropped.
        if let Ok(mut queue) = self.queue.try_borrow_mut() {
            queue.clear();
        }

        // Pump the driver to process cancellations and completions.
        // This is critical for IOCP safety to avoid Heap Corruption from pending cancellations,
        // as the kernel might try to write to buffers that are being freed.
        if let Ok(mut driver) = self.driver.try_borrow_mut() {
            let _ = driver.submit_queue();
            let _ = driver.process_completions();
        }
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

// ============ Thread-Safe Main Task Waker Implementation ============

struct AtomicWakerState {
    flag: AtomicBool,
    remote: Arc<dyn RemoteWaker>,
}

fn waker_from_state(state: Arc<AtomicWakerState>) -> Waker {
    let ptr = Arc::into_raw(state) as *const ();
    let raw = RawWaker::new(ptr, &ATOMIC_WAKER_VTABLE);
    unsafe { Waker::from_raw(raw) }
}

const ATOMIC_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(atomic_clone, atomic_wake, atomic_wake_by_ref, atomic_drop);

unsafe fn atomic_clone(ptr: *const ()) -> RawWaker {
    let arc = unsafe { Arc::from_raw(ptr as *const AtomicWakerState) };
    std::mem::forget(arc.clone());
    std::mem::forget(arc);
    RawWaker::new(ptr, &ATOMIC_WAKER_VTABLE)
}

unsafe fn atomic_wake(ptr: *const ()) {
    let arc = unsafe { Arc::from_raw(ptr as *const AtomicWakerState) };
    arc.flag.store(true, Ordering::Release);
    let _ = arc.remote.wake();
}

unsafe fn atomic_wake_by_ref(ptr: *const ()) {
    let state = unsafe { &*(ptr as *const AtomicWakerState) };
    state.flag.store(true, Ordering::Release);
    let _ = state.remote.wake();
}

unsafe fn atomic_drop(ptr: *const ()) {
    let _ = unsafe { Arc::from_raw(ptr as *const AtomicWakerState) };
}
