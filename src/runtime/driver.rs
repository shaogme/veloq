use crate::runtime::buffer::FixedBuf;
use crate::runtime::op::IoResources;
use std::io;
use std::task::{Context, Poll};

pub trait Driver {
    /// Register a new operation. Returns the user_data key.
    fn reserve_op(&mut self) -> usize;

    /// Submit an operation with its resources directly.
    fn submit_op_resources(&mut self, user_data: usize, resources: IoResources);

    /// Poll operation status.
    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<u32>, IoResources)>;

    /// Submit queued operations to the kernel.
    fn submit(&mut self) -> io::Result<()>;

    /// Wait for completions.
    fn wait(&mut self) -> io::Result<()>;

    /// Process the completion queue.
    fn process_completions(&mut self);

    /// Cancel an operation.
    fn cancel_op(&mut self, user_data: usize);

    /// Allocate a fixed buffer from the driver's pool.
    fn alloc_fixed_buffer(&self) -> Option<FixedBuf>;
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
