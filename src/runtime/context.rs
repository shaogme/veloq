//! Thread-local context for the async runtime.
//!
//! This module provides a way to access the current executor's context from anywhere
//! within async code, similar to how tokio::spawn() works.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::rc::{Rc, Weak};

use crate::runtime::driver::PlatformDriver;
use crate::runtime::join::JoinHandle;
use crate::runtime::task::Task;

// Thread-local storage for the current executor context.
thread_local! {
    static CONTEXT: RefCell<Option<ExecutorContext>> = const { RefCell::new(None) };
}

/// Lightweight context stored in TLS.
/// Uses Weak references to avoid preventing cleanup.
struct ExecutorContext {
    driver: Weak<RefCell<PlatformDriver>>,
    queue: Weak<RefCell<VecDeque<Rc<Task>>>>,
}

/// RAII guard that sets the context on creation and clears it on drop.
pub(crate) struct ContextGuard {
    _private: (),
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = None;
        });
    }
}

/// Enter the executor context. Called by LocalExecutor::block_on.
pub(crate) fn enter(
    driver: Weak<RefCell<PlatformDriver>>,
    queue: Weak<RefCell<VecDeque<Rc<Task>>>>,
) -> ContextGuard {
    CONTEXT.with(|ctx| {
        *ctx.borrow_mut() = Some(ExecutorContext { driver, queue });
    });
    ContextGuard { _private: () }
}

/// Spawn a new task on the current executor.
///
/// # Panics
/// Panics if called outside of a runtime context (i.e., not within block_on).
///
/// # Example
/// ```ignore
/// runtime::spawn(async {
///     println!("Hello from spawned task!");
/// });
/// ```
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    CONTEXT.with(|ctx| {
        let ctx = ctx.borrow();
        let ctx = ctx
            .as_ref()
            .expect("spawn() called outside of runtime context");

        let queue = ctx
            .queue
            .upgrade()
            .expect("executor has been dropped");

        let (handle, producer) = JoinHandle::new();
        let task = Task::new(async move {
            let output = future.await;
            producer.set(output);
        }, ctx.queue.clone());
        queue.borrow_mut().push_back(task);
        handle
    })
}

/// Get a weak reference to the current driver.
///
/// # Panics
/// Panics if called outside of a runtime context.
///
/// # Example
/// ```ignore
/// let driver = runtime::current_driver();
/// let stream = TcpStream::connect(addr, driver).await?;
/// ```
pub fn current_driver() -> Weak<RefCell<PlatformDriver>> {
    CONTEXT.with(|ctx| {
        let ctx = ctx.borrow();
        let ctx = ctx
            .as_ref()
            .expect("current_driver() called outside of runtime context");

        ctx.driver.clone()
    })
}

/// Check if we are currently inside a runtime context.
pub fn is_in_context() -> bool {
    CONTEXT.with(|ctx| ctx.borrow().is_some())
}

/// Yields execution back to the executor, allowing other tasks to run.
///
/// This is useful when you want to give other spawned tasks a chance to execute.
///
/// # Example
/// ```ignore
/// spawn(async { println!("spawned task"); });
/// yield_now().await; // Let the spawned task run
/// ```
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
