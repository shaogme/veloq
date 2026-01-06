//! Explicit context for the async runtime.
//!
//! This module provides the `RuntimeContext` which is passed to tasks
//! allowing them to spawn new tasks and access runtime resources.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::{Rc, Weak};

use crate::io::buffer::{AnyBufPool, BufPool};
use crate::io::driver::PlatformDriver;
use crate::runtime::executor::Spawner;
use crate::runtime::join::{JoinHandle, LocalJoinHandle};
use crate::runtime::task::Task;

use std::cell::OnceCell;

thread_local! {
    static CONTEXT: RefCell<Option<RuntimeContext>> = const { RefCell::new(None) };
    static CURRENT_POOL: OnceCell<AnyBufPool> = const { OnceCell::new() };
}

/// Sets the thread-local runtime context.
pub(crate) fn enter(context: RuntimeContext) -> ContextGuard {
    CONTEXT.with(|ctx| {
        let prev = ctx.borrow_mut().replace(context);
        ContextGuard { prev }
    })
}

/// Guard that resets the runtime context when dropped.
pub(crate) struct ContextGuard {
    prev: Option<RuntimeContext>,
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = self.prev.take();
        });
    }
}

/// Retrieve the current runtime context.
///
/// # Panics
/// Panics if called outside a runtime context.
pub fn current() -> RuntimeContext {
    try_current().expect("Runtime context not set. Are you running inside an executor?")
}

/// Try to retrieve the current runtime context.
pub fn try_current() -> Option<RuntimeContext> {
    CONTEXT.with(|ctx| ctx.borrow().clone())
}

/// Context passed to runtime tasks.
///
/// This provides access to the executor's facilities like spawning tasks
/// and accessing the IO driver.
#[derive(Clone)]
pub struct RuntimeContext {
    pub(crate) driver: Weak<RefCell<PlatformDriver>>,
    pub(crate) queue: Weak<RefCell<VecDeque<Rc<Task>>>>,
    pub(crate) spawner: Option<Spawner>,
    pub(crate) mesh: Option<Weak<RefCell<crate::runtime::executor::MeshContext>>>,
}

impl RuntimeContext {
    /// Create a new RuntimeContext.
    pub(crate) fn new(
        driver: Weak<RefCell<PlatformDriver>>,
        queue: Weak<RefCell<VecDeque<Rc<Task>>>>,
        spawner: Option<Spawner>,
        mesh: Option<Weak<RefCell<crate::runtime::executor::MeshContext>>>,
    ) -> Self {
        Self {
            driver,
            queue,
            spawner,
            mesh,
        }
    }

    /// Spawn a new task on the current executor.
    ///
    /// # Panics
    /// Panics if the current executor does not have a global spawner (e.g. some local-only configurations).
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let spawner = self
            .spawner
            .as_ref()
            .expect("spawn() called on a context without a global spawner");

        let (handle, producer) = JoinHandle::new();
        let job = Box::pin(async move {
            let output = future.await;
            producer.set(output);
        });

        self.spawn_impl(job, spawner);
        handle
    }

    fn spawn_impl(&self, job: crate::runtime::executor::Job, spawner: &Spawner) {
        if let Some(mesh_weak) = &self.mesh
            && let Some(mesh_rc) = mesh_weak.upgrade()
            && let Some(driver_rc) = self.driver.upgrade()
        {
            let dispatched = spawner.with_workers(|workers| {
                if !workers.is_empty() {
                    use std::sync::atomic::{AtomicUsize, Ordering};
                    static RND: AtomicUsize = AtomicUsize::new(0);

                    let seed = RND.fetch_add(1, Ordering::Relaxed);
                    let count = workers.len();
                    let idx1 = seed % count;
                    let idx2 = (idx1 + 7) % count;

                    let w1 = &workers[idx1];
                    let w2 = &workers[idx2];
                    let target = if w1.total_load() <= w2.total_load() {
                        w1
                    } else {
                        w2
                    };

                    if target.id() != usize::MAX {
                        let mut mesh = mesh_rc.borrow_mut();
                        let mut driver = driver_rc.borrow_mut();

                        if let Err(returned_job) = mesh.send_to(target.id(), job, &mut driver) {
                            // Fallback on full/error - return ownership of job
                            return Err(returned_job);
                        }
                        return Ok(());
                    }
                }
                Err(job)
            });

            match dispatched {
                Ok(_) => return,
                Err(job) => {
                    // Fallback to global injector
                    spawner.spawn_job(job);
                    return;
                }
            }
        }

        // Default Fallback
        spawner.spawn_job(job);
    }

    /// Spawn a new local task on the current executor.
    ///
    /// Local tasks are not Send and are guaranteed to run on the current thread.
    pub fn spawn_local<F, T>(&self, future: F) -> LocalJoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let queue = self.queue.upgrade().expect("executor has been dropped");

        let (handle, producer) = LocalJoinHandle::new();
        let task = Task::new(
            async move {
                let output = future.await;
                producer.set(output);
            },
            self.queue.clone(),
        );
        queue.borrow_mut().push_back(task);
        handle
    }

    /// Get a weak reference to the current driver.
    pub fn driver(&self) -> Weak<RefCell<PlatformDriver>> {
        self.driver.clone()
    }

    /// Register buffers with the underlying driver.
    pub fn register_buffers(&self, pool: &dyn BufPool) {
        #[cfg(target_os = "linux")]
        if let Some(driver) = self.driver.upgrade() {
            let bufs = pool.get_registration_buffers();
            driver
                .borrow_mut()
                .register_buffers(&bufs)
                .expect("Failed to register buffer pool");
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pool;
        }
    }

    /// Spawn a new task on a specific worker thread.
    ///
    /// # Panics
    /// Panics if called outside of a runtime context, if the executor registry is missing,
    /// or if the `worker_id` is invalid.
    pub fn spawn_to<F>(&self, future: F, worker_id: usize) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let spawner = self
            .spawner
            .as_ref()
            .expect("spawn_to() called on a context without a global spawner");

        let (handle, producer) = JoinHandle::new();
        let job = Box::pin(async move {
            let output = future.await;
            producer.set(output);
        });

        // Optimization: If spawning to self, just push to local queue
        if let Some(mesh_weak) = &self.mesh
            && let Some(mesh_rc) = mesh_weak.upgrade()
        {
            if mesh_rc.borrow().id == worker_id {
                let queue = self
                    .queue
                    .upgrade()
                    .expect("executor has been dropped but context remains?");
                let task = Task::new(job, self.queue.clone());
                queue.borrow_mut().push_back(task);
                return handle;
            }

            // Try Mesh
            if let Some(driver_rc) = self.driver.upgrade() {
                let mut mesh = mesh_rc.borrow_mut();
                let mut driver = driver_rc.borrow_mut();

                if let Err(returned_job) = mesh.send_to(worker_id, job, &mut driver) {
                    // Mesh full or error, fallback to global injector
                    spawner.spawn_job_to(returned_job, worker_id);
                }
                return handle;
            }
        }

        // Fallback (e.g., no mesh or driver dropped)
        spawner.spawn_job_to(job, worker_id);
        handle
    }
}

/// Yields execution back to the executor, allowing other tasks to run.
///
/// This is useful when you want to give other spawned tasks a chance to execute.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// Future returned by `yield_now()`.
pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.yielded {
            std::task::Poll::Ready(())
        } else {
            self.yielded = true;
            // Wake ourselves so we get polled again
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// Bind a buffer pool to the current thread.
/// This should be called once per thread, usually during executor initialization.
///
/// # Panics
/// Panics if a pool is already bound to this thread.
pub fn bind_pool<P: BufPool + Clone + 'static>(pool: P) {
    try_bind_pool(pool).expect("Buffer pool already bound for this thread");
}

/// Error returned when trying to bind a pool to a thread that already has one.
#[derive(Debug, Clone, Copy)]
pub struct PoolAlreadyBound;

impl std::fmt::Display for PoolAlreadyBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Buffer pool already bound for this thread")
    }
}

impl std::error::Error for PoolAlreadyBound {}

/// Try to bind a buffer pool to the current thread.
/// Returns error if already bound.
pub fn try_bind_pool<P: BufPool + Clone + 'static>(pool: P) -> Result<(), PoolAlreadyBound> {
    let any_pool = AnyBufPool::new(pool);
    // 1. Set TLS
    CURRENT_POOL.with(|cell| cell.set(any_pool).map_err(|_| PoolAlreadyBound))?;

    // 2. Try Auto-Register (Active Hook)
    // If we are already running inside a RuntimeContext, register immediately.
    if let Some(ctx) = try_current()
        && let Some(pool) = current_pool()
    {
        ctx.register_buffers(&pool);
    }

    Ok(())
}

/// Get the current thread's buffer pool.
pub fn current_pool() -> Option<AnyBufPool> {
    CURRENT_POOL.with(|cell| cell.get().cloned())
}

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks. There is no
/// guarantee that the spawned task will execute to completion. When a task is spawned,
/// it triggers the provided future. The returned `JoinHandle` receives the result of
/// the future when the task completes.
///
/// This function requires the future to be `Send` as it may be executed on a different thread.
///
/// # Panics
///
/// Panics if called outside of a runtime context, or if the current runtime does not support
/// global spawning (missing executor registry).
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    current().spawn(future)
}

/// Spawns a `!Send` future on the current thread.
///
/// The task is guaranteed to run on the exact same thread that called `spawn_local`.
/// Unlike `spawn`, `spawn_local` allows spawning futures that do not implement `Send`.
///
/// # Panics
///
/// Panics if called outside of a runtime context.
pub fn spawn_local<F>(future: F) -> LocalJoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    current().spawn_local(future)
}

/// Spawns a new asynchronous task on a specific worker thread.
///
/// # Panics
///
/// Panics if called outside of a runtime context, or if the `worker_id` is invalid.
pub fn spawn_to<F>(future: F, worker_id: usize) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    current().spawn_to(future, worker_id)
}
