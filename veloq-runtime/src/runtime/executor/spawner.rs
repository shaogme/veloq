use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_queue::SegQueue;
use crossbeam_utils::CachePadded;

use crate::io::driver::{Driver, RemoteWaker};
use crate::runtime::mesh::{self, Consumer, Producer};

// --- Job Definition ---

pub type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

pub(crate) fn pack_job<F, Output>(async_fn: F) -> (crate::runtime::join::JoinHandle<Output>, Job)
where
    F: AsyncFnOnce() -> Output + Send + 'static,
    Output: Send + 'static,
{
    let (handle, producer) = crate::runtime::join::JoinHandle::new();
    let job = Box::new(move || {
        let future = async_fn();
        Box::pin(async move {
            let output = future.await;
            producer.set(output);
        }) as Pin<Box<dyn Future<Output = ()>>>
    });
    (handle, job)
}

// --- Shared State ---

use crate::runtime::task::Task;

pub(crate) struct ExecutorShared {
    pub(crate) injector: SegQueue<Job>,
    pub(crate) pinned: SegQueue<Job>,
    pub(crate) remote_queue: std::sync::mpsc::Sender<Task>,
    pub(crate) waker: LateBoundWaker,
    pub(crate) injected_load: CachePadded<AtomicUsize>,
    pub(crate) local_load: CachePadded<AtomicUsize>,
}

pub(crate) struct LateBoundWaker {
    waker: std::cell::UnsafeCell<Option<Arc<dyn RemoteWaker>>>,
    ready: std::sync::atomic::AtomicBool,
}

unsafe impl Send for LateBoundWaker {}
unsafe impl Sync for LateBoundWaker {}

impl LateBoundWaker {
    pub fn new() -> Self {
        Self {
            waker: std::cell::UnsafeCell::new(None),
            ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set(&self, waker: Arc<dyn RemoteWaker>) {
        unsafe { *self.waker.get() = Some(waker) };
        self.ready.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl RemoteWaker for LateBoundWaker {
    fn wake(&self) -> std::io::Result<()> {
        if self.ready.load(std::sync::atomic::Ordering::Acquire) {
            let w = unsafe { &*self.waker.get() };
            if let Some(w) = w {
                return w.wake();
            }
        }
        Ok(())
    }
}

pub struct MeshContext {
    pub(crate) id: usize,
    pub(crate) state: Arc<std::sync::atomic::AtomicU8>,
    pub(crate) ingress: Vec<Consumer<Job>>,
    pub(crate) egress: Vec<Producer<Job>>,
    pub(crate) peer_handles: Arc<Vec<AtomicUsize>>,
}

impl MeshContext {
    pub fn send_to(
        &mut self,
        peer_id: usize,
        job: Job,
        driver: &mut crate::io::driver::PlatformDriver,
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

    pub fn poll_ingress<F>(&mut self, mut on_job: F) -> bool
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

// --- Executor Handles ---

/// Handle to a remote executor, used for task injection and load monitoring.
#[derive(Clone)]
pub struct ExecutorHandle {
    pub(crate) id: usize,
    pub(crate) shared: Arc<ExecutorShared>,
}

impl ExecutorHandle {
    pub(crate) fn schedule(&self, job: Job) {
        self.shared.injector.push(job);
        self.shared.injected_load.fetch_add(1, Ordering::Relaxed);
        self.shared.waker.wake().expect("Failed to wake executor");
    }

    pub(crate) fn schedule_pinned(&self, job: Job) {
        self.shared.pinned.push(job);
        self.shared.injected_load.fetch_add(1, Ordering::Relaxed);
        self.shared.waker.wake().expect("Failed to wake executor");
    }

    pub fn total_load(&self) -> usize {
        self.shared.injected_load.load(Ordering::Relaxed)
            + self.shared.local_load.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> usize {
        self.id
    }
}

// --- Spawner & Registry ---

/// A static registry that maintains the set of all active executors.
/// Workers are pre-allocated at runtime startup.
pub struct ExecutorRegistry {
    handles: Arc<Vec<ExecutorHandle>>,
}

impl Default for ExecutorRegistry {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ExecutorRegistry {
    pub fn new(handles: Vec<ExecutorHandle>) -> Self {
        Self {
            handles: Arc::new(handles),
        }
    }

    pub fn all(&self) -> &[ExecutorHandle] {
        &self.handles
    }
}

/// Global spawner that acts as a frontend to the Registry.
#[derive(Clone)]
pub struct Spawner {
    registry: Arc<ExecutorRegistry>,
    seed: Cell<usize>,
}

impl Spawner {
    pub fn new(registry: Arc<ExecutorRegistry>) -> Self {
        // Random seed init
        let seed = Box::into_raw(Box::new(0)) as usize;
        Self {
            registry,
            seed: Cell::new(seed),
        }
    }

    pub fn spawn<F, Output>(&self, async_fn: F) -> crate::runtime::join::JoinHandle<Output>
    where
        F: AsyncFnOnce() -> Output + Send + 'static,
        Output: Send + 'static,
    {
        let (handle, job) = pack_job(async_fn);
        self.spawn_job(job);
        handle
    }

    fn select_worker(&self) -> ExecutorHandle {
        let workers = self.registry.all();

        match self.p2c_select(workers) {
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
        driver: &RefCell<crate::io::driver::PlatformDriver>,
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

        let mut seed = self.seed.get();
        // Simple Xorshift
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        self.seed.set(seed);

        let idx1 = seed % count;
        let idx2 = (seed >> 32) as usize % count;

        let w1 = &workers[idx1];
        let w2 = &workers[idx2];

        let load1 = w1.total_load();
        let load2 = w2.total_load();

        if load1 <= load2 { Some(w1) } else { Some(w2) }
    }

    pub fn spawn_to<F, Output>(
        &self,
        async_fn: F,
        worker_id: usize,
    ) -> crate::runtime::join::JoinHandle<Output>
    where
        F: AsyncFnOnce() -> Output + Send + 'static,
        Output: Send + 'static,
    {
        let (handle, job) = pack_job(async_fn);
        self.spawn_job_to(job, worker_id);
        handle
    }

    pub(crate) fn spawn_job_to(&self, job: Job, worker_id: usize) {
        let workers = self.registry.all();

        if let Some(target) = workers.get(worker_id) {
            target.schedule_pinned(job);
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
        driver: &RefCell<crate::io::driver::PlatformDriver>,
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
