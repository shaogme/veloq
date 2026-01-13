pub mod blocking;
pub mod context;
pub mod executor;
pub mod join;
pub mod task;

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use crossbeam_deque::Worker;
use crossbeam_queue::SegQueue;
use crossbeam_utils::CachePadded;
use tracing::{debug, trace};

use crate::config::Config;
use crate::io::buffer::{AnyBufPool, BuddyPool, BufferRegistrar, RegisteredPool};
use crate::io::driver::RemoteWaker;
use crate::runtime::executor::spawner::LateBoundWaker;
use crate::runtime::executor::{ExecutorHandle, ExecutorRegistry, ExecutorShared, Spawner};
use crate::runtime::task::harness::Runnable;
use crate::runtime::task::{SpawnedTask, Task};
pub use context::{RuntimeContext, spawn, spawn_local, spawn_to, yield_now};
pub use executor::LocalExecutor;
pub use join::{JoinHandle, LocalJoinHandle};

pub type PoolConstructor = Arc<dyn Fn(usize, Box<dyn BufferRegistrar>) -> AnyBufPool + Send + Sync>;

struct WorkerPrep {
    shared: Arc<ExecutorShared>,
    remote_receiver: mpsc::Receiver<Task>,
    pinned_receiver: mpsc::Receiver<SpawnedTask>,
    // Worker local queue for stealable tasks
    stealable_worker: Worker<Runnable>,
    pool_constructor: PoolConstructor,
    config: Config,
    barrier: Arc<Barrier>,
}

pub struct RuntimeBuilder {
    config: Config,
    pool_constructor: Option<PoolConstructor>,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self {
            config: Config::default(),
            pool_constructor: None,
        }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    pub fn pool_constructor<F>(mut self, f: F) -> Self
    where
        F: Fn(usize, Box<dyn BufferRegistrar>) -> AnyBufPool + Send + Sync + 'static,
    {
        self.pool_constructor = Some(Arc::new(f));
        self
    }

    pub fn build(self) -> std::io::Result<Runtime> {
        let worker_count = self.config.worker_threads.unwrap_or_else(num_cpus::get);
        debug!("Building Runtime with {} workers", worker_count);
        if worker_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Worker count must be > 0",
            ));
        }

        // Default Pool Constructor
        let pool_constructor = self.pool_constructor.unwrap_or_else(|| {
            Arc::new(|_, registrar| {
                let pool = BuddyPool::new().expect("Failed to create default BuddyPool");
                let reg_pool =
                    RegisteredPool::new(pool, registrar).expect("Failed to create RegisteredPool");
                AnyBufPool::new(reg_pool)
            })
        });

        let mut states = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            states.push(Arc::new(AtomicU8::new(executor::RUNNING)));
        }
        trace!("Initialized {} worker states", worker_count);

        // Pre-allocate peer handles storage (initially 0/invalid)
        let mut peer_handles_storage = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            peer_handles_storage.push(AtomicUsize::new(0));
        }
        let peer_handles = Arc::new(peer_handles_storage);

        // Pre-allocate Shared State and Handles
        let mut shared_states = Vec::with_capacity(worker_count);
        let mut handles = Vec::with_capacity(worker_count);
        let mut remote_receivers = Vec::with_capacity(worker_count);
        let mut pinned_receivers = Vec::with_capacity(worker_count);
        // Temporary storage for workers to be moved into threads
        let mut stealable_workers = Vec::with_capacity(worker_count);

        for i in 0..worker_count {
            let (tx, rx) = mpsc::channel();
            let (pinned_tx, pinned_rx) = mpsc::channel();

            // Create stealable worker
            let worker = Worker::new_fifo();
            let stealer = worker.stealer();
            stealable_workers.push(Some(worker));

            let shared = Arc::new(ExecutorShared {
                pinned: pinned_tx,
                remote_queue: tx,
                future_injector: SegQueue::new(),
                stealer,
                waker: LateBoundWaker::new(),
                injected_load: CachePadded::new(AtomicUsize::new(0)),
                local_load: CachePadded::new(AtomicUsize::new(0)),
                state: states[i].clone(),
                shutdown: AtomicBool::new(false),
            });
            shared_states.push(shared.clone());
            remote_receivers.push(Some(rx));
            pinned_receivers.push(Some(pinned_rx));

            handles.push(ExecutorHandle { id: i, shared });
        }

        let registry = Arc::new(ExecutorRegistry::new(handles));
        let mut thread_handles = Vec::with_capacity(worker_count);

        // Barrier for N workers.
        // All workers (including Worker 0) must reach this barrier after publishing their handle
        // but before starting their loop.
        let barrier = Arc::new(Barrier::new(worker_count));

        // Spawn Workers 1 to N-1 (Skip 0)
        for worker_id in 1..worker_count {
            let registry = registry.clone();
            let peer_handles_clone = peer_handles.clone();
            let config_clone = self.config.clone();
            let pool_constructor = pool_constructor.clone();

            let shared = shared_states[worker_id].clone(); // Get pre-allocated shared
            let remote_receiver = remote_receivers[worker_id]
                .take()
                .expect("Receiver already taken");
            let pinned_receiver = pinned_receivers[worker_id]
                .take()
                .expect("Pinned receiver already taken");
            let stealable_worker = stealable_workers[worker_id]
                .take()
                .expect("Worker already taken");

            let builder = thread::Builder::new().name(format!("veloq-worker-{}", worker_id));
            let barrier = barrier.clone();

            let handle = builder.spawn(move || {
                debug!("Worker {} thread started", worker_id);
                let mut executor = LocalExecutor::builder()
                    .config(config_clone)
                    .with_shared(shared) // Inject shared state
                    .with_remote_receiver(remote_receiver) // Inject remote receiver
                    .with_pinned_receiver(pinned_receiver) // Inject pinned receiver
                    .with_worker(stealable_worker) // Inject stealable worker
                    .build(|registrar| {
                        let pool = pool_constructor(worker_id, registrar);
                        AnyBufPool::new(pool)
                    });

                executor = executor.with_registry(registry.clone()); // Inject registry
                executor = executor.with_id(worker_id); // Set Worker ID

                // Publish Handle
                let fd = executor.raw_driver_handle();
                peer_handles_clone[worker_id].store(fd, Ordering::Release);

                // Wait for all workers to be ready (handles published)
                barrier.wait();

                // Run Loop
                executor.run();
            })?;

            thread_handles.push(handle);
        }

        // Prepare Worker 0
        let shared = shared_states[0].clone();
        let remote_receiver = remote_receivers[0].take().unwrap();
        let pinned_receiver = pinned_receivers[0].take().unwrap();
        let stealable_worker = stealable_workers[0].take().unwrap();

        let worker_0_prep = WorkerPrep {
            shared,
            remote_receiver,
            pinned_receiver,
            stealable_worker,
            pool_constructor: pool_constructor.clone(),
            config: self.config.clone(),
            barrier: barrier.clone(), // Pass barrier to Worker 0
        };

        Ok(Runtime {
            handles: thread_handles,
            registry,
            peer_handles,
            worker_count,
            worker_0_prep: Some(worker_0_prep),
        })
    }
}

pub struct Runtime {
    handles: Vec<thread::JoinHandle<()>>,
    registry: Arc<ExecutorRegistry>,
    peer_handles: Arc<Vec<AtomicUsize>>,
    #[allow(dead_code)]
    worker_count: usize,
    worker_0_prep: Option<WorkerPrep>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        debug!("Runtime shutting down");
        // 1. Notify all workers to stop
        for handle in self.registry.all() {
            handle.shared.shutdown.store(true, Ordering::Relaxed);
            // Wake them up so they see the shutdown signal
            let _ = handle.shared.waker.wake();
        }

        // 2. Wait for worker threads to finish
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    // Legacy New (Optional, can delegate to Builder)
    pub fn new(config: Config) -> Self {
        Self::builder()
            .config(config)
            .build()
            .expect("Failed to build runtime")
    }

    pub fn spawner(&self) -> Spawner {
        Spawner::new(self.registry.clone())
    }

    pub fn spawn<F, Output>(&self, future: F) -> JoinHandle<Output>
    where
        F: Future<Output = Output> + Send + 'static,
        Output: Send + 'static,
    {
        self.spawner().spawn(future)
    }

    /// Block on a future using a local executor on the current thread,
    /// participating in the runtime as Worker 0 (an internal mesh node).
    ///
    /// This method consumes the Runtime, setting up the current thread as the first worker.
    /// It waits for all other pre-spawned workers to be ready before starting execution.
    pub fn block_on<F>(mut self, future: F) -> F::Output
    where
        F: Future,
    {
        debug!("Block_on entered (Worker 0)");
        let prep = self
            .worker_0_prep
            .take()
            .expect("Runtime already started or invalid state");

        let mut executor = LocalExecutor::builder()
            .config(prep.config)
            .with_shared(prep.shared) // Inject shared state
            .with_remote_receiver(prep.remote_receiver) // Inject remote receiver
            .with_pinned_receiver(prep.pinned_receiver) // Inject pinned receiver
            .with_worker(prep.stealable_worker) // Inject stealable worker
            .build(|registrar| {
                // Bind Buffer Pool via constructor
                (prep.pool_constructor)(0, registrar)
            });

        executor = executor.with_registry(self.registry.clone());
        executor = executor.with_id(0); // Set Worker ID (Worker 0)

        // Publish Handle
        let fd = executor.raw_driver_handle();
        self.peer_handles[0].store(fd, Ordering::Release);

        // Wait for all workers to be ready
        prep.barrier.wait();

        // Run Future
        // block_on in LocalExecutor runs the loop until future completes.
        executor.block_on(future)
    }
}
