// use crate::buffer::FixedBuf;

#[cfg(target_os = "windows")]
pub(crate) mod blocking;
pub(crate) mod op_registry;
pub(crate) mod stable_slab;
use std::io;
use std::task::{Context, Poll};

/// Platform-specific operation trait
pub trait PlatformOp: 'static {}

pub trait Driver {
    /// Platform-specific operation type
    type Op: PlatformOp;

    /// Register a new operation. Returns the user_data key.
    fn reserve_op(&mut self) -> usize;

    /// Submit an operation with its resources directly.
    fn submit(&mut self, user_data: usize, op: Self::Op);

    /// Poll operation status.
    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, Self::Op)>;

    /// Submit queued operations to the kernel.
    fn submit_queue(&mut self) -> io::Result<()>;

    /// Wait for completions.
    fn wait(&mut self) -> io::Result<()>;

    /// Process the completion queue.
    fn process_completions(&mut self);

    /// Cancel an operation.
    fn cancel_op(&mut self, user_data: usize);

    /// Register a buffer pool with the driver.
    /// This allows the driver to optimize buffer access (e.g. fixed buffers in io_uring).
    fn register_buffer_pool(&mut self, pool: &crate::io::buffer::BufferPool) -> io::Result<()>;

    /// Register a set of file descriptors/handles.
    /// Returns a list of `IoFd` that can be used in subsequent operations.
    fn register_files(
        &mut self,
        files: &[crate::io::op::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>>;

    /// Unregister a set of file descriptors/handles.
    fn unregister_files(&mut self, files: Vec<crate::io::op::IoFd>) -> io::Result<()>;

    /// Submit a fire-and-forget operation (e.g. Close).
    /// The driver takes ownership of resources and ensures cleanup.
    fn submit_background(&mut self, op: Self::Op) -> io::Result<()>;

    /// Wake up the driver from blocking wait.
    fn wake(&mut self) -> io::Result<()>;

    /// Create a thread-safe waker.
    fn create_waker(&self) -> std::sync::Arc<dyn RemoteWaker>;
}

pub trait RemoteWaker: Send + Sync {
    fn wake(&self) -> io::Result<()>;
}

// Platform-specific driver implementations

#[cfg(target_os = "linux")]
pub(crate) mod uring;

#[cfg(target_os = "linux")]
pub use uring::UringDriver as PlatformDriver;

#[cfg(target_os = "windows")]
pub(crate) mod iocp;

#[cfg(target_os = "windows")]
pub use iocp::IocpDriver as PlatformDriver;
