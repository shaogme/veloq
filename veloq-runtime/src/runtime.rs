pub mod context;
pub mod executor;
pub mod join;
pub mod mesh;
pub mod task;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize};

use crate::io::buffer::AnyBufPool;
use crate::runtime::executor::spawner::LateBoundWaker;
use crate::runtime::executor::{
    CachePadded, ExecutorHandle, ExecutorRegistry, ExecutorShared, Spawner,
};
use crate::runtime::mesh::{Consumer, Producer};
use crossbeam_queue::SegQueue;

pub use context::{RuntimeContext, spawn, spawn_local, spawn_to, yield_now};
pub use executor::LocalExecutor;
pub use join::{JoinHandle, LocalJoinHandle};

/// A helper struct to organize and distribute mesh channels.
/// It uses flattened vectors for better memory locality.
/// - Ingress: Grouped by Receiver (Transposed) -> `[Receiver * N + Sender]`
/// - Egress: Grouped by Sender (Row Major) -> `[Sender * N + Receiver]`
struct MeshMatrix<T> {
    size: usize,
    ingress: Vec<Option<Consumer<T>>>,
    egress: Vec<Option<Producer<T>>>,
}

impl<T: Send> MeshMatrix<T> {
    fn new(size: usize, states: &[Arc<AtomicU8>]) -> Self {
        let capacity = size * size;
        let mut ingress = Vec::with_capacity(capacity);
        let mut egress = Vec::with_capacity(capacity);

        // Pre-fill with None
        for _ in 0..capacity {
            ingress.push(None);
            egress.push(None);
        }

        for i in 0..size {
            for j in 0..size {
                let target_state = states[j].clone();
                let (tx, rx) = mesh::channel(256, target_state);

                // Egress: Sender i, Receiver j -> Row Major: i * N + j
                egress[i * size + j] = Some(tx);

                // Ingress: Receiver j, Sender i -> Column Major (Transposed): j * N + i
                // This ensures that for receiver j, all incoming channels are contiguous.
                ingress[j * size + i] = Some(rx);
            }
        }

        Self {
            size,
            ingress,
            egress,
        }
    }

    fn take_worker_channels(&mut self, worker_id: usize) -> (Vec<Consumer<T>>, Vec<Producer<T>>) {
        let start = worker_id * self.size;
        let end = start + self.size;

        let ingress_batch = self.ingress[start..end]
            .iter_mut()
            .map(|opt| opt.take().expect("Ingress channel already distributed"))
            .collect();

        let egress_batch = self.egress[start..end]
            .iter_mut()
            .map(|opt| opt.take().expect("Egress channel already distributed"))
            .collect();

        (ingress_batch, egress_batch)
    }
}

pub type PoolConstructor = Arc<dyn Fn(usize) -> AnyBufPool + Send + Sync>;

pub struct RuntimeBuilder {
    config: crate::config::Config,
    pool_constructor: Option<PoolConstructor>,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self {
            config: crate::config::Config::default(),
            pool_constructor: None,
        }
    }

    pub fn config(mut self, config: crate::config::Config) -> Self {
        self.config = config;
        self
    }

    pub fn pool_constructor<F>(mut self, f: F) -> Self
    where
        F: Fn(usize) -> AnyBufPool + Send + Sync + 'static,
    {
        self.pool_constructor = Some(Arc::new(f));
        self
    }

    pub fn build(self) -> std::io::Result<Runtime> {
        let worker_count = self.config.worker_threads.unwrap_or_else(num_cpus::get);

        // Default Pool Constructor
        let pool_constructor = self.pool_constructor.unwrap_or_else(|| {
            Arc::new(|_| {
                let pool = crate::io::buffer::BuddyPool::new()
                    .expect("Failed to create default BuddyPool");
                crate::io::buffer::AnyBufPool::new(pool)
            })
        });

        let mut states = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            states.push(Arc::new(AtomicU8::new(mesh::RUNNING)));
        }

        let mut mesh_matrix = MeshMatrix::new(worker_count, &states);

        let mut peer_handles_storage = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            peer_handles_storage.push(AtomicUsize::new(0));
        }
        let peer_handles = Arc::new(peer_handles_storage);

        // Pre-allocate Shared State and Handles
        let mut shared_states = Vec::with_capacity(worker_count);
        let mut handles = Vec::with_capacity(worker_count);

        for i in 0..worker_count {
            let shared = Arc::new(ExecutorShared {
                injector: SegQueue::new(),
                pinned: SegQueue::new(),
                waker: LateBoundWaker::new(),
                injected_load: CachePadded(AtomicUsize::new(0)),
                local_load: CachePadded(AtomicUsize::new(0)),
            });
            shared_states.push(shared.clone());

            handles.push(ExecutorHandle { id: i, shared });
        }

        let registry = Arc::new(ExecutorRegistry::new(handles));
        let mut thread_handles = Vec::with_capacity(worker_count);
        let barrier = Arc::new(std::sync::Barrier::new(worker_count + 1));

        // Spawn Workers
        for worker_id in 0..worker_count {
            let registry = registry.clone();
            let peer_handles_clone = peer_handles.clone();
            let config_clone = self.config.clone();
            let pool_constructor = pool_constructor.clone();

            // Take ownership of the specific mesh components for this worker
            let (ingress, egress) = mesh_matrix.take_worker_channels(worker_id);
            let state = states[worker_id].clone();

            let shared = shared_states[worker_id].clone(); // Get pre-allocated shared

            let builder = std::thread::Builder::new().name(format!("veloq-worker-{}", worker_id));
            let barrier = barrier.clone();

            let handle = builder.spawn(move || {
                let mut executor = LocalExecutor::builder()
                    .config(config_clone)
                    .with_shared(shared) // Inject shared state
                    .build();

                executor = executor.with_registry(registry.clone()); // Inject registry

                // Attach Mesh
                executor.attach_mesh(worker_id, state, ingress, egress, peer_handles_clone);

                // Bind Buffer Pool
                let pool = pool_constructor(worker_id);
                // This binds to TLS and since run() enters context, it will register buffers.
                crate::runtime::context::bind_pool(pool);

                // Wait for all workers to be ready
                barrier.wait();

                // Run Loop
                executor.run();
            })?;

            thread_handles.push(handle);
        }

        // Wait for all workers to come online
        barrier.wait();

        Ok(Runtime {
            handles: thread_handles,
            registry,
            peer_handles,
            worker_count,
            next_worker_id: AtomicUsize::new(worker_count), // All IDs assigned
        })
    }
}

pub struct Runtime {
    handles: Vec<std::thread::JoinHandle<()>>,
    registry: Arc<ExecutorRegistry>,

    #[allow(dead_code)]
    peer_handles: Arc<Vec<AtomicUsize>>,
    #[allow(dead_code)]
    worker_count: usize,
    #[allow(dead_code)]
    next_worker_id: AtomicUsize,
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    // Legacy New (Optional, can delegate to Builder)
    pub fn new(config: crate::config::Config) -> Self {
        Self::builder()
            .config(config)
            .build()
            .expect("Failed to build runtime")
    }

    pub fn spawner(&self) -> Spawner {
        Spawner::new(self.registry.clone())
    }

    pub fn spawn<F, Output>(&self, async_fn: F) -> crate::runtime::join::JoinHandle<Output>
    where
        F: AsyncFnOnce() -> Output + Send + 'static,
        Output: Send + 'static,
    {
        self.spawner().spawn(async_fn)
    }

    /// Block on a future using a local executor on the current thread,
    /// participating in the runtime as an external (non-mesh) node.
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: std::future::Future,
    {
        let mut executor = LocalExecutor::new();
        executor = executor.with_registry(self.registry.clone());
        executor.block_on(future)
    }

    pub fn block_on_all(self) {
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}
