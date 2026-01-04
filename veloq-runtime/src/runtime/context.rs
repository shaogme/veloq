//! Explicit context for the async runtime.
//!
//! This module provides the `RuntimeContext` which is passed to tasks
//! allowing them to spawn new tasks and access runtime resources.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::{Rc, Weak};

use crate::io::buffer::BufPool;
use crate::io::driver::PlatformDriver;
use crate::runtime::executor::Spawner;
use crate::runtime::join::{JoinHandle, LocalJoinHandle};
use crate::runtime::task::Task;

thread_local! {
    static CONTEXT: RefCell<Option<RuntimeContext>> = RefCell::new(None);
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
    CONTEXT.with(|ctx| {
        ctx.borrow()
            .clone()
            .expect("Runtime context not set. Are you running inside an executor?")
    })
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
}

impl RuntimeContext {
    /// Create a new RuntimeContext.
    pub(crate) fn new(
        driver: Weak<RefCell<PlatformDriver>>,
        queue: Weak<RefCell<VecDeque<Rc<Task>>>>,
        spawner: Option<Spawner>,
    ) -> Self {
        Self {
            driver,
            queue,
            spawner,
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

        spawner.spawn(future)
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
