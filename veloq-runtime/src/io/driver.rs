// use crate::buffer::FixedBuf;

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
    /// Returns `Ok(Poll::...)` on success (Ready or Pending/Queued).
    /// Returns `Err((Error, Op))` if submission failed and the Op was NOT consumed/stored.
    fn submit(&mut self, user_data: usize, op: Self::Op)
    -> Result<Poll<()>, (io::Error, Self::Op)>;

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

    /// Register memory regions with the driver.
    /// Returns a list of handles (tokens) corresponding to the regions.
    /// Replaces `register_buffers`.
    fn register_buffer_regions(
        &mut self,
        regions: &[crate::io::buffer::BufferRegion],
    ) -> io::Result<Vec<usize>>;

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

    /// Notify another driver instance (Mesh Wakeup).
    fn notify_mesh(&mut self, handle: crate::io::op::RawHandle) -> io::Result<()>;

    /// Wake up the driver from blocking wait.
    fn wake(&mut self) -> io::Result<()>;

    /// Get the low-level driver handle (RawFd on Linux, HANDLE on Windows).
    /// Used for direct mesh communication (e.g. MSG_RING).
    fn inner_handle(&self) -> crate::io::op::RawHandle;

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
