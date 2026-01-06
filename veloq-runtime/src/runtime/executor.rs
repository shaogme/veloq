use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::io::buffer::BufPool;
use crate::io::driver::{Driver, PlatformDriver};
use crate::runtime::join::LocalJoinHandle;
use crate::runtime::task::Task;

use crate::io::driver::RemoteWaker;
use crate::runtime::context::RuntimeContext;
use smr_swap::{LocalReader, SmrReader, SmrSwap};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_queue::SegQueue;

use crate::runtime::mesh::{self, Consumer, Producer};

pub type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) fn pack_job<F>(future: F) -> (crate::runtime::join::JoinHandle<F::Output>, Job)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (handle, producer) = crate::runtime::join::JoinHandle::new();
    let job = Box::pin(async move {
        let output = future.await;
        producer.set(output);
    });
    (handle, job)
}

pub(crate) struct MeshContext {
    pub(crate) id: usize,
    state: Arc<AtomicU8>,
    ingress: Vec<Consumer<Job>>,
    egress: Vec<Producer<Job>>,
    peer_handles: Arc<Vec<AtomicUsize>>,
}

impl MeshContext {
    pub fn send_to(
        &mut self,
        peer_id: usize,
        job: Job,
        driver: &mut PlatformDriver,
    ) -> Result<(), Job> {
        if peer_id >= self.egress.len() {
            return Err(job);
        }
        let producer = &mut self.egress[peer_id];

        if let Err(job) = producer.push(job) {
            return Err(job);
        }

        let state = producer.target_state();
        if state == mesh::PARKED || state == mesh::PARKING {
            let handle = self.peer_handles[peer_id].load(Ordering::Acquire);
            if handle != 0 {
                let _ = driver.notify_mesh(handle as _);
            }
        }
        Ok(())
    }

    fn poll_ingress<F>(&mut self, mut on_job: F) -> bool
    where
        F: FnMut(Job),
    {
        let mut did_work = false;

        for consumer in &mut self.ingress {
            if let Some(job) = consumer.pop() {
                on_job(job);
                did_work = true;
            }
        }
        did_work
    }
}

/// Handle to a remote executor, used for task injection and load monitoring.
#[derive(Clone)]
pub struct ExecutorHandle {
    pub(crate) id: usize,
    pub(crate) injector: Arc<SegQueue<Job>>,
    pub(crate) waker: Arc<dyn RemoteWaker>,
    pub(crate) injected_load: Arc<AtomicUsize>,
    pub(crate) local_load: Arc<AtomicUsize>,
}

impl ExecutorHandle {
    pub(crate) fn schedule(&self, job: Job) {
        self.injector.push(job);
        self.injected_load.fetch_add(1, Ordering::Relaxed);
        self.waker.wake().expect("Failed to wake worker");
    }

    pub fn total_load(&self) -> usize {
        self.injected_load.load(Ordering::Relaxed) + self.local_load.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> usize {
        self.id
    }
}

/// A registry that maintains the set of all active executors.
/// Used for global task spawning (P2C) and work stealing.
pub struct ExecutorRegistry {
    writer: Mutex<SmrSwap<Vec<ExecutorHandle>>>,
    reader_factory: SmrReader<Vec<ExecutorHandle>>,
    epoch: AtomicUsize, // Incremented whenever a new worker is added
    next: AtomicUsize,  // Source of randomness for P2C
}

impl Default for ExecutorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        let swap = SmrSwap::new(Vec::new());
        let reader_factory = swap.reader();
        Self {
            writer: Mutex::new(swap),
            reader_factory,
            epoch: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
        }
    }

    pub fn register(&self, handle: ExecutorHandle) {
        let mut guard = self.writer.lock().unwrap();
        guard.update(|current| {
            let mut new = current.clone();
            new.push(handle.clone());
            new
        });
        self.epoch.fetch_add(1, Ordering::Release);
    }

    pub fn reader(&self) -> SmrReader<Vec<ExecutorHandle>> {
        self.reader_factory.clone()
    }
}

/// Global spawner that acts as a frontend to the Registry.
#[derive(Clone)]
pub struct Spawner {
    registry: Arc<ExecutorRegistry>,
    reader: RefCell<LocalReader<Vec<ExecutorHandle>>>,
}

impl Spawner {
    pub fn new(registry: Arc<ExecutorRegistry>) -> Self {
        let reader = registry.reader().local();
        Self {
            registry,
            reader: RefCell::new(reader),
        }
    }

    pub fn spawn<F>(&self, future: F) -> crate::runtime::join::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (handle, job) = pack_job(future);
        self.spawn_job(job);
        handle
    }

    fn select_worker(&self) -> ExecutorHandle {
        let reader = self.reader.borrow();
        let workers = reader.load();

        match self.p2c_select(&workers) {
            Some(target) => target.clone(),
            None => panic!("No workers available in registry"),
        }
    }

    pub(crate) fn spawn_job(&self, job: Job) {
        self.select_worker().schedule(job);
    }

    pub(crate) fn spawn_with_mesh(
        &self,
        job: Job,
        mesh: &RefCell<MeshContext>,
        driver: &RefCell<PlatformDriver>,
    ) {
        let target = self.select_worker();

        // Try mesh first if target has ID
        if target.id() != usize::MAX {
            let mut mesh = mesh.borrow_mut();
            let mut driver = driver.borrow_mut();

            if let Err(returned_job) = mesh.send_to(target.id(), job, &mut driver) {
                // Fallback on full/error -> inject
                drop(mesh);
                drop(driver);
                target.schedule(returned_job);
            }
            return;
        }

        // No valid ID (unlikely for registered worker) -> inject
        target.schedule(job);
    }

    fn p2c_select<'a>(&self, workers: &'a [ExecutorHandle]) -> Option<&'a ExecutorHandle> {
        let count = workers.len();
        if count == 0 {
            return None;
        }

        let seed = self.registry.next.fetch_add(1, Ordering::Relaxed);
        let idx1 = seed % count;
        let idx2 = {
            let mut x = seed;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x % count
        };

        let w1 = &workers[idx1];
        let w2 = &workers[idx2];

        let load1 = w1.total_load();
        let load2 = w2.total_load();

        if load1 <= load2 { Some(w1) } else { Some(w2) }
    }

    pub fn spawn_to<F>(
        &self,
        future: F,
        worker_id: usize,
    ) -> crate::runtime::join::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (handle, job) = pack_job(future);
        self.spawn_job_to(job, worker_id);
        handle
    }

    pub(crate) fn spawn_job_to(&self, job: Job, worker_id: usize) {
        let reader = self.reader.borrow();
        let workers = reader.load();

        if let Some(target) = workers.get(worker_id) {
            target.schedule(job);
        } else {
            // Panic if the worker_id is invalid, as this implies a logic error in the caller
            // assuming the existence of a specific worker.
            panic!("Worker {} not found in registry", worker_id);
        }
    }
    pub(crate) fn spawn_to_with_mesh(
        &self,
        job: Job,
        worker_id: usize,
        mesh: &RefCell<MeshContext>,
        driver: &RefCell<PlatformDriver>,
    ) {
        // Try Mesh
        let mut mesh = mesh.borrow_mut();
        let mut driver = driver.borrow_mut();

        if let Err(returned_job) = mesh.send_to(worker_id, job, &mut driver) {
            // Mesh full or error, fallback to global injector
            // Drop locks before fallback
            drop(mesh);
            drop(driver);
            self.spawn_job_to(returned_job, worker_id);
        }
    }
}

/// Builder for LocalExecutor.
/// Allows explicit configuration of the driver and buffer registration.
pub struct LocalExecutorBuilder {
    config: crate::config::Config,
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
        }
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

        LocalExecutor {
            driver: Rc::new(RefCell::new(driver)),
            queue,
            injector: Arc::new(SegQueue::new()),
            injected_load: Arc::new(AtomicUsize::new(0)),
            local_load: Arc::new(AtomicUsize::new(0)),
            registry: None,
            registry_reader: None,
            mesh: None,
        }
    }
}

pub struct LocalExecutor {
    driver: Rc<RefCell<PlatformDriver>>,
    queue: Rc<RefCell<VecDeque<Rc<Task>>>>,

    // Always present components for task injection
    injector: Arc<SegQueue<Job>>,
    injected_load: Arc<AtomicUsize>,
    local_load: Arc<AtomicUsize>,

    // Optional connection to the global registry
    registry: Option<Arc<ExecutorRegistry>>,
    registry_reader: Option<LocalReader<Vec<ExecutorHandle>>>,

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
        let reader = registry.reader().local();
        self.registry = Some(registry);
        self.registry_reader = Some(reader);
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
            injector: self.injector.clone(),
            waker: self.driver.borrow().create_waker(),
            injected_load: self.injected_load.clone(),
            local_load: self.local_load.clone(),
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
        self.local_load.fetch_add(1, Ordering::Relaxed);
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

    fn enqueue_job(&self, job: Job) {
        self.local_load.fetch_add(1, Ordering::Relaxed);
        let task = Task::new(job, Rc::downgrade(&self.queue));
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
        if let Some(job) = self.injector.pop() {
            self.injected_load.fetch_sub(1, Ordering::Relaxed);
            self.enqueue_job(job);
            return true;
        }
        false
    }

    fn try_steal(&self, executed: usize) -> bool {
        if let Some(reader) = &self.registry_reader {
            let workers = reader.load();
            let count = workers.len();

            if count > 0 {
                let seed = self as *const _ as usize;
                let start_idx = seed.wrapping_add(executed);

                for i in 0..count {
                    let idx = (start_idx + i) % count;
                    let target = &workers[idx];

                    if Arc::ptr_eq(&target.injector, &self.injector) {
                        continue;
                    }

                    // Steal from injector
                    if let Some(job) = target.injector.pop() {
                        target.injected_load.fetch_sub(1, Ordering::Relaxed);
                        self.enqueue_job(job);
                        return true;
                    }
                }
            }
        }
        false
    }

    fn park_and_wait(&self, main_woken: &RefCell<bool>) {
        let has_pending_tasks = !self.queue.borrow().is_empty() || *main_woken.borrow();
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
        let main_woken = Rc::new(RefCell::new(true));
        let waker = main_task_waker(main_woken.clone());
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
            self.park_and_wait(&main_woken);
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

// Define a struct to hold Mesh Fabric until distributed
struct MeshFabric {
    // [worker_id] -> Ingress/Egress
    ingress: Vec<Vec<Consumer<Job>>>,
    egress: Vec<Vec<Producer<Job>>>,
    states: Vec<Arc<AtomicU8>>,
}

#[allow(dead_code)]
pub struct Runtime {
    handles: Vec<std::thread::JoinHandle<()>>,
    registry: Arc<ExecutorRegistry>,
    config: Arc<crate::config::Config>,

    // Mesh
    mesh_fabric: Arc<Mutex<Option<MeshFabric>>>,
    peer_handles: Arc<Vec<AtomicUsize>>,
    worker_count: usize,
    next_worker_id: AtomicUsize,
}

impl Runtime {
    pub fn new(config: crate::config::Config) -> Self {
        let worker_count = config.worker_threads.unwrap_or_else(num_cpus::get);

        // Initialize Mesh Fabric
        let mut ingress_storage: Vec<Vec<Consumer<Job>>> =
            (0..worker_count).map(|_| Vec::new()).collect();
        let mut egress_storage: Vec<Vec<Producer<Job>>> =
            (0..worker_count).map(|_| Vec::new()).collect();
        let mut states = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            states.push(Arc::new(AtomicU8::new(mesh::RUNNING)));
        }

        // Create N*N channels
        // Channel(i -> j): Producer in egress[i], Consumer in ingress[j]
        for i in 0..worker_count {
            for j in 0..worker_count {
                // Producer i needs target (j) state
                let target_state = states[j].clone();
                let (tx, rx) = mesh::channel(256, target_state); // Capacity 256 per channel?
                egress_storage[i].push(tx);
                ingress_storage[j].push(rx);
            }
        }

        let fabric = MeshFabric {
            ingress: ingress_storage,
            egress: egress_storage,
            states,
        };

        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            handles.push(AtomicUsize::new(0));
        }

        Self {
            handles: Vec::new(),
            registry: Arc::new(ExecutorRegistry::new()),
            config: Arc::new(config),
            mesh_fabric: Arc::new(Mutex::new(Some(fabric))),
            peer_handles: Arc::new(handles),
            worker_count,
            next_worker_id: AtomicUsize::new(0),
        }
    }

    /// Spawn a worker thread with user-provided initialization logic.
    ///
    /// The `init_fn` closure is executed in the new thread and is responsible for:
    /// 1. Creating and configuring a `LocalExecutor` (e.g. registering buffers).
    /// 2. Creating the main future to run.
    ///
    /// This allows full user control over resource management (like buffer pools).
    pub fn spawn_worker<Init, Fut>(&mut self, init_fn: Init)
    where
        Init: FnOnce() -> (LocalExecutor, Fut) + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let registry = self.registry.clone();

        // Mesh Setup
        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let fabric_clone = self.mesh_fabric.clone();
        let peer_handles = self.peer_handles.clone();
        let use_mesh = worker_id < self.worker_count;

        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();

        let thread_handle = std::thread::spawn(move || {
            // User initializes executor and future
            let (mut executor, future) = init_fn();

            // Runtime attaches registry
            executor = executor.with_registry(registry);

            // Attach Mesh if applicable
            if use_mesh {
                let mut fabric_guard = fabric_clone.lock().unwrap();
                if let Some(fabric) = fabric_guard.as_mut() {
                    // Take ownership of channels
                    // Note: This is a bit fragile if worker_id >= ingress.len(),
                    // but we guard with use_mesh check.
                    let ingress = std::mem::take(&mut fabric.ingress[worker_id]);
                    let egress = std::mem::take(&mut fabric.egress[worker_id]);
                    let state = fabric.states[worker_id].clone();

                    executor.attach_mesh(worker_id, state, ingress, egress, peer_handles);
                }
            }

            handle_tx.send(executor.handle()).unwrap();

            // Wait for main thread to register us before we start
            let _ = ack_rx.recv();

            executor.block_on(future);
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
