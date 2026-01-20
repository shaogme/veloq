pub mod blocking_ops;
mod ext;
mod inner;
pub mod op;
pub mod rio;
mod submit;
#[cfg(test)]
mod tests;

use crate::io::driver::op_registry::OpEntry;
use crate::io::driver::{DetachedCompleter, Driver, RemoteWaker};
use std::io;
use std::task::{Context, Poll};
use tracing::{debug, trace};
use windows_sys::Win32::System::IO::{OVERLAPPED, PostQueuedCompletionStatus};

pub use inner::{IocpDriver, PlatformData};
use op::IocpOp;
use submit::SubmissionResult;

impl Driver for IocpDriver {
    type Op = IocpOp;

    fn reserve_op(&mut self) -> usize {
        let old_pages = self.ops.page_count();
        let user_data = self.ops.insert(OpEntry::new(None, PlatformData::None));
        trace!(user_data, "Reserved op slot");

        if self.ops.page_count() > old_pages {
            // New page allocated, register it immediately
            if let Some(rio) = &mut self.rio_state {
                let new_page_idx = self.ops.page_count() - 1;
                rio.ensure_slab_page_registration(new_page_idx, &self.ops, &self.extensions);
            }
        }
        user_data
    }

    fn attach_detached_completer(
        &mut self,
        user_data: usize,
        completer: Box<dyn DetachedCompleter<Self::Op>>,
    ) {
        if let Some(op) = self.ops.get_mut(user_data) {
            op.platform_data = PlatformData::Detached(completer);
        }
    }

    fn submit(
        &mut self,
        user_data: usize,
        op: Self::Op,
    ) -> Result<Poll<()>, (io::Error, Self::Op)> {
        trace!(user_data, "Submitting op");
        // Since RIO slab registration is handled eagerly in reserve_op (and new),
        // we no longer need to check/register it here.
        // This resolves the borrow checker conflict.

        if let Some(op_entry) = self.ops.get_mut(user_data) {
            // Important: we must pin the op in resources first, then get pointers
            op_entry.resources = Some(op);
            let op_ref = op_entry.resources.as_mut().unwrap();

            op_ref.header.user_data = user_data;

            // Construct SubmitContext utilizing Split Borrow
            let mut ctx = crate::io::driver::iocp::op::SubmitContext {
                port: self.port,
                overlapped: &mut op_ref.header.inner as *mut OVERLAPPED,
                ext: &self.extensions,
                registered_files: &self.registered_files,
                rio: self.rio_state.as_mut(),
                // ops removed from context
            };

            let result = unsafe { (op_ref.vtable.submit)(op_ref, &mut ctx) };

            match result {
                Ok(SubmissionResult::Pending) => {
                    // Op submitted successfully
                }
                Ok(SubmissionResult::PostToQueue) => {
                    // E.g. Wakeup. Post immediately.
                    let _ = unsafe {
                        PostQueuedCompletionStatus(self.port, 0, user_data, std::ptr::null_mut())
                    };
                }
                Ok(SubmissionResult::Offload(task)) => {
                    use crate::runtime::blocking::get_blocking_pool;
                    if get_blocking_pool().execute(task).is_err() {
                        op_entry.result = Some(Err(io::Error::other("Thread pool overloaded")));
                        if let Some(waker) = op_entry.waker.take() {
                            waker.wake();
                        }
                    }
                }
                Ok(SubmissionResult::Timer(duration)) => {
                    let timeout = self.wheel.insert(user_data, duration);
                    op_entry.platform_data = PlatformData::Timer(timeout);
                }
                Err(e) => {
                    op_entry.result = Some(Err(e));
                    if let Some(waker) = op_entry.waker.take() {
                        waker.wake();
                    }
                }
            }
        }

        // Check if we need to complete a Detached op synchronously (e.g. if submit failed)
        let should_complete_detached = if let Some(op) = self.ops.get_mut(user_data) {
            op.result.is_some() && matches!(op.platform_data, PlatformData::Detached(_))
        } else {
            false
        };

        if should_complete_detached {
            let mut entry = self.ops.remove(user_data);
            // Extract result
            let result = entry.result.take().unwrap_or(Ok(0));

            // Extract completer
            if let PlatformData::Detached(completer) = entry.platform_data {
                // Extract resources
                if let Some(iocp_op) = entry.resources {
                    completer.complete(result, iocp_op);
                }
            }
        }

        Ok(Poll::Ready(()))
    }

    fn submit_background(&mut self, op: Self::Op) -> io::Result<()> {
        let user_data = self.reserve_op();

        // Same RIO registration logic for background ops (though usually used for file ops, safety check doesn't hurt)
        // Eager registration handles this now.

        if let Some(op_entry) = self.ops.get_mut(user_data) {
            op_entry.platform_data = PlatformData::Background;
            op_entry.resources = Some(op);
            let op_ref = op_entry.resources.as_mut().unwrap();
            op_ref.header.user_data = user_data;

            let mut ctx = crate::io::driver::iocp::op::SubmitContext {
                port: self.port,
                overlapped: &mut op_ref.header.inner as *mut OVERLAPPED,
                ext: &self.extensions,
                registered_files: &self.registered_files,
                rio: self.rio_state.as_mut(),
                // ops removed
            };

            let result = unsafe { (op_ref.vtable.submit)(op_ref, &mut ctx) };

            match result {
                Ok(SubmissionResult::Offload(task)) => {
                    use crate::runtime::blocking::get_blocking_pool;
                    if get_blocking_pool().execute(task).is_err() {
                        // Failed to submit background task
                        self.ops.remove(user_data);
                        return Err(io::Error::other("Thread pool overloaded"));
                    }
                }
                Ok(_) => {
                    // Expected Offload for Close, but if other types are used, this is fine too.
                    // If it is Pending, it will complete properly and be cleaned up.
                }
                Err(e) => {
                    debug!(error = ?e, user_data, "Background submit failed");
                    self.ops.remove(user_data);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, Self::Op)> {
        self.ops.poll_op(user_data, cx)
    }

    fn submit_queue(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> io::Result<()> {
        self.get_completion(u32::MAX)
    }

    fn process_completions(&mut self) {
        let _ = self.get_completion(0);
    }

    fn cancel_op(&mut self, user_data: usize) {
        self.cancel_op_internal(user_data);
    }

    fn register_buffer_regions(
        &mut self,
        regions: &[crate::io::buffer::BufferRegion],
    ) -> io::Result<Vec<usize>> {
        IocpDriver::register_buffer_regions(self, regions)
    }

    fn register_files(
        &mut self,
        files: &[crate::io::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>> {
        IocpDriver::register_files(self, files)
    }

    fn unregister_files(&mut self, files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
        IocpDriver::unregister_files(self, files)
    }

    fn wake(&mut self) -> io::Result<()> {
        IocpDriver::wake(self)
    }

    fn inner_handle(&self) -> crate::io::RawHandle {
        self.port.into()
    }

    fn create_waker(&self) -> std::sync::Arc<dyn RemoteWaker> {
        IocpDriver::create_waker(self)
    }

    fn driver_id(&self) -> usize {
        0
    }
}
