use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;
use crossbeam_utils::CachePadded;

use crate::io::buffer::{BufferRegion, BufferRegistrar};
use crate::io::driver::{Driver, PlatformDriver, RemoteWaker};
use crate::runtime::context::RuntimeContext;
pub(crate) use crate::runtime::executor::spawner::ExecutorShared;
use crate::runtime::join::LocalJoinHandle;
use crate::runtime::mesh::{self, Consumer, Producer};
use crate::runtime::task::{SpawnedTask, Task};

// Re-export common types from spawner which acts as the definition source for these
pub use self::spawner::{ExecutorHandle, ExecutorRegistry, Job, MeshContext, Spawner};

pub mod spawner;

// ============ LocalExecutor Implementation ============

pub struct LocalExecutorBuilder {
    config: crate::config::Config,
    shared: Option<Arc<ExecutorShared>>,
    remote_receiver: Option<mpsc::Receiver<Task>>,
    pinned_receiver: Option<mpsc::Receiver<SpawnedTask>>,
}

impl LocalExecutorBuilder {
    pub fn new() -> Self {
        Self {
            config: crate::config::Config::default(),
            shared: None,
            remote_receiver: None,
            pinned_receiver: None,
        }
    }

    pub(crate) fn with_shared(mut self, shared: Arc<ExecutorShared>) -> Self {
        self.shared = Some(shared);
        self
    }

    pub(crate) fn with_remote_receiver(mut self, receiver: mpsc::Receiver<Task>) -> Self {
        self.remote_receiver = Some(receiver);
        self
    }

    pub(crate) fn with_pinned_receiver(mut self, receiver: mpsc::Receiver<SpawnedTask>) -> Self {
        self.pinned_receiver = Some(receiver);
        self
    }

    pub fn config(mut self, config: crate::config::Config) -> Self {
        self.config = config;
        self
    }

    /// Build the LocalExecutor.
    ///
    /// Requires a `pool_constructor` closure that creates an `AnyBufPool` using the provided `BufferRegistrar`.
    pub fn build<F>(self, pool_constructor: F) -> LocalExecutor
    where
        F: FnOnce(Box<dyn BufferRegistrar>) -> crate::io::buffer::AnyBufPool,
    {
        let driver_val = PlatformDriver::new(&self.config).expect("Failed to create driver");
        // Wrap driver early to create registrar
        let driver = Rc::new(RefCell::new(driver_val));

        let queue = Rc::new(RefCell::new(VecDeque::new()));

        // Borrow driver to create waker
        let waker = driver.borrow().create_waker();

        let (shared, remote_receiver, pinned_receiver) = if let Some(shared) = self.shared {
            let remote_rec = self
                .remote_receiver
                .expect("Shared state provided without remote receiver");
            let pinned_rec = self
                .pinned_receiver
                .expect("Shared state provided without pinned receiver");
            (shared, remote_rec, pinned_rec)
        } else {
            let (remote_tx, remote_rx) = mpsc::channel();
            let (pinned_tx, pinned_rx) = mpsc::channel();
            // Default state is RUNNING
            let state = Arc::new(AtomicU8::new(mesh::RUNNING));

            let shared = Arc::new(ExecutorShared {
                injector: SegQueue::new(),
                pinned: pinned_tx,
                remote_queue: remote_tx,
                waker: crate::runtime::executor::spawner::LateBoundWaker::new(),
                injected_load: CachePadded::new(AtomicUsize::new(0)),
                local_load: CachePadded::new(AtomicUsize::new(0)),
                state,
            });
            (shared, remote_rx, pinned_rx)
        };

        // Bind the driver's waker to the shared state (Late Binding)
        shared.waker.set(waker);

        // Construct Registrar and Pool
        let registrar = Box::new(ExecutorRegistrar {
            driver: Rc::downgrade(&driver),
        });

        let buf_pool = pool_constructor(registrar);

        LocalExecutor {
            driver,
            queue,
            shared,
            remote_receiver,
            pinned_receiver,
            registry: None,
            mesh: None,
            id: usize::MAX,
            buf_pool,
        }
    }
}

pub struct LocalExecutor {
    driver: Rc<RefCell<PlatformDriver>>,
    queue: Rc<RefCell<VecDeque<Task>>>,

    // Shared components
    shared: Arc<ExecutorShared>,
    remote_receiver: mpsc::Receiver<Task>,
    pinned_receiver: mpsc::Receiver<SpawnedTask>,

    // Optional connection to the global registry
    registry: Option<Arc<ExecutorRegistry>>,

    // Mesh Networking
    mesh: Option<Rc<RefCell<MeshContext>>>,
    // Cached ID (usize::MAX if not in mesh)
    id: usize,
    // Buffer Pool
    buf_pool: crate::io::buffer::AnyBufPool,
}

impl LocalExecutor {
    /// Get the raw handle (fd) of the underlying driver.
    /// Used for Mesh initialization.
    pub fn raw_driver_handle(&self) -> usize {
        self.driver.borrow().inner_handle() as usize
    }

    pub fn driver_handle(&self) -> std::rc::Weak<RefCell<PlatformDriver>> {
        Rc::downgrade(&self.driver)
    }

    /// Create a new builder for LocalExecutor.
    pub fn builder() -> LocalExecutorBuilder {
        LocalExecutorBuilder::new()
    }

    /// Attach this executor to a registry.
    /// This enables the executor to steal tasks from others in the registry.
    pub fn with_registry(mut self, registry: Arc<ExecutorRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            id: self.id,
            shared: self.shared.clone(),
        }
    }

    pub fn registrar(&self) -> Box<dyn BufferRegistrar> {
        Box::new(ExecutorRegistrar {
            driver: Rc::downgrade(&self.driver),
        })
    }

    pub fn pool(&self) -> crate::io::buffer::AnyBufPool {
        self.buf_pool.clone()
    }

    pub fn spawn_local<F, T>(&self, future: F) -> LocalJoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (handle, producer) = LocalJoinHandle::new();
        // SAFETY: This is spawn_local, so we can use new_local (!Send future).
        let task = unsafe {
            SpawnedTask::new_local(async move {
                let output = future.await;
                producer.set(output);
            })
        };
        let task = unsafe {
            task.bind(
                self.handle().id,
                Rc::downgrade(&self.queue),
                self.shared.clone(),
            )
        };
        self.queue.borrow_mut().push_back(task);
        self.shared.local_load.fetch_add(1, Ordering::Relaxed);
        handle
    }

    pub(crate) fn attach_mesh(
        &mut self,
        id: usize,
        ingress: Vec<Consumer<Job>>,
        egress: Vec<Producer<Job>>,
        peer_handles: Arc<Vec<AtomicUsize>>,
    ) {
        // We MUST ensure that the state passed here is consistent with self.shared.state
        // For now, we assume RuntimeBuilder sets this up correctly (by passing shared.state's clone or same source).
        // Since `ExecutorShared` is generally immutable once created, if they differ, it's a bug in setup.
        // Ideally we should ASSERT here:
        // assert!(Arc::ptr_eq(&self.shared.state, &state), "Mesh state must match Shared state");

        let mesh = MeshContext {
            id,
            ingress,
            egress,
            peer_handles,
        };
        self.mesh = Some(Rc::new(RefCell::new(mesh)));
        self.id = id;
    }

    fn enqueue_job(&self, task: Job) {
        self.shared.local_load.fetch_add(1, Ordering::Relaxed);
        let task = unsafe {
            task.bind(
                self.handle().id,
                Rc::downgrade(&self.queue),
                self.shared.clone(),
            )
        };
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
        if let Ok(job) = self.pinned_receiver.try_recv() {
            self.shared.injected_load.fetch_sub(1, Ordering::Relaxed);
            self.enqueue_job(job);
            return true;
        }

        // Check remote queue (Woken tasks from other threads)
        if let Ok(task) = self.remote_receiver.try_recv() {
            self.queue.borrow_mut().push_back(task);
            return true;
        }

        if let Some(job) = self.shared.injector.pop() {
            self.shared.injected_load.fetch_sub(1, Ordering::Relaxed);
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
                        target.shared.injected_load.fetch_sub(1, Ordering::Relaxed);
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
            let state = &self.shared.state;

            // 1. Set PARKING
            // This tells remote wakers: "I might sleep soon, so you should probably syscall wake me".
            state.store(mesh::PARKING, Ordering::Release);

            // 2. Poll Mesh (Double check)
            if let Some(mesh_rc) = &self.mesh {
                let mut mesh = mesh_rc.borrow_mut();
                if mesh.poll_ingress(|job| self.enqueue_job(job)) {
                    state.store(mesh::RUNNING, Ordering::Relaxed);
                    can_park = false;
                }
            }

            // Double check remote queues
            if can_park {
                if !self.pinned_receiver.try_recv().is_err()
                    || !self.remote_receiver.try_recv().is_err()
                    || !self.shared.injector.is_empty()
                {
                    state.store(mesh::RUNNING, Ordering::Relaxed);
                    can_park = false;
                }
            }

            if can_park {
                // 3. Commit PARKED
                state.store(mesh::PARKED, Ordering::Release);

                // Final check (race condition barrier) needed?
                // In IOCP/Uring, if a wake happens after PARKED is set but before wait(), it posts to the port/fd.
                // The wait() will picking it up immediately.
                // The only race uses remote queues:
                // A: Set PARKED.
                // B: Push task. See PARKED. Wake().
                // A: Wait(). Wakeup consumed.
                // Correct.

                driver.wait().unwrap();
            }

            // Restore RUNNING
            state.store(mesh::RUNNING, Ordering::Release);
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
            self.buf_pool.clone(),
        );

        let _guard = crate::runtime::context::enter(context);

        // Auto-Register removed: Pools should be pre-registered (Scheme 1).

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
                    self.shared.local_load.fetch_sub(1, Ordering::Relaxed);
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
            self.buf_pool.clone(),
        );

        let _guard = crate::runtime::context::enter(context);

        // Auto-Register removed.

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
                    self.shared.local_load.fetch_sub(1, Ordering::Relaxed);
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

// Default implementation removed as LocalExecutor now requires explicit buffer pool.

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

#[derive(Debug, Clone)]
struct ExecutorRegistrar<D: Driver> {
    driver: std::rc::Weak<RefCell<D>>,
}

impl<D: Driver> BufferRegistrar for ExecutorRegistrar<D> {
    fn register(&self, regions: &[BufferRegion]) -> std::io::Result<Vec<usize>> {
        let driver_rc = self
            .driver
            .upgrade()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "Driver dropped"))?;
        let mut driver = driver_rc.borrow_mut();
        driver.register_buffer_regions(regions)
    }
}
