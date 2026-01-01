// use crate::io::buffer::{BufferPool, FixedBuf};
use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use crate::io::op::{IoOp, IoResources, Wakeup};
use io_uring::{IoUring, opcode, squeue};
use std::io;
use std::os::unix::io::RawFd;
use std::task::{Context, Poll};

use crate::io::driver::RemoteWaker;
use std::sync::Arc;

mod ops;
use ops::UringOp;

struct UringWaker(RawFd);

impl RemoteWaker for UringWaker {
    fn wake(&self) -> io::Result<()> {
        let buf = 1u64.to_ne_bytes();
        let ret = unsafe { libc::write(self.0, buf.as_ptr() as *const _, 8) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for UringWaker {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Special user_data value for cancel operations.
/// We use u64::MAX - 1 because u64::MAX is already reserved.
/// CQEs with this user_data are ignored (they're just confirmations that cancel was submitted).
const CANCEL_USER_DATA: u64 = u64::MAX - 1;

pub struct UringDriver {
    /// The actual io_uring instance
    ring: IoUring,
    /// Store for in-flight operations.
    /// The key (usize) is used as the io_uring user_data.
    ops: OpRegistry<()>,
    waker_fd: RawFd,
    waker_token: Option<usize>,
}



impl UringDriver {
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::builder()
            .setup_coop_taskrun() // Reduce IPIs
            .setup_single_issuer() // Optimized for single-threaded submission
            .setup_defer_taskrun() // Defer work until enter
            .build(entries)
            .or_else(|e| {
                // Fallback for older kernels if flags are unsupported (EINVAL)
                if e.raw_os_error() == Some(libc::EINVAL) {
                    IoUring::new(entries)
                } else {
                    Err(e)
                }
            })?;

        let ops = OpRegistry::with_capacity(entries as usize);

        let waker_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if waker_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut driver = Self {
            ring,
            ops,
            waker_fd,
            waker_token: None,
        };

        driver.submit_waker();

        Ok(driver)
    }

    fn submit_waker(&mut self) {
        if self.waker_token.is_some() {
            return;
        }

        let fd = self.waker_fd;
        let op = Wakeup::new(fd);
        let resources = op.into_resource();

        let user_data = self.reserve_op_internal();
        self.waker_token = Some(user_data);

        self.submit_op_resources_internal(user_data, resources);
    }

    pub fn submit(&mut self) -> io::Result<()> {
        self.ring.submit()?;
        Ok(())
    }

    /// Wait for completions.
    pub fn wait(&mut self) -> io::Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }

        // Optimization: check if we have completions available in userspace queue
        // before issuing a syscall to wait.
        if !self.ring.completion().is_empty() {
            self.process_completions();
            return Ok(());
        }

        self.ring.submit_and_wait(1)?;
        self.process_completions();
        Ok(())
    }

    /// Process the completion queue.
    pub fn process_completions_internal(&mut self) {
        let mut needs_waker_resubmit = false;
        
        {
            let mut cqe_kicker = self.ring.completion();
            cqe_kicker.sync();

            for cqe in cqe_kicker {
                let user_data = cqe.user_data() as usize;

                // Skip special user_data values:
                // - u64::MAX: reserved/special
                // - CANCEL_USER_DATA: completion of our cancel requests (we don't need to handle these)
                if user_data == u64::MAX as usize || user_data == CANCEL_USER_DATA as usize {
                    continue;
                }

                if Some(user_data) == self.waker_token {
                    needs_waker_resubmit = true;
                    continue;
                }

                if self.ops.contains(user_data) {
                    let op = &mut self.ops[user_data];
                    let res = if cqe.result() >= 0 {
                        Ok(cqe.result() as usize)
                    } else {
                        Err(io::Error::from_raw_os_error(-cqe.result()))
                    };

                    if op.cancelled {
                        // Future is gone. Cleanup.
                        // 'resources' will be dropped when we remove from slab.
                        self.ops.remove(user_data);
                    } else {
                        // Store result and wake future
                        op.result = Some(res);
                        if let Some(waker) = op.waker.take() {
                            waker.wake();
                        }
                    }
                }
            }
        }
        
        if needs_waker_resubmit {
             if let Some(token) = self.waker_token.take() {
                 self.ops.remove(token);
             }
             self.submit_waker();
        }
    }

    /// Register a new operation.
    /// Returns the user_data key.
    /// `resources` are the moved-in buffers/fds that must live until completion.
    /// Reserve a slot for an operation.
    pub fn reserve_op_internal(&mut self) -> usize {
        self.ops.insert(OpEntry::new(IoResources::None, ()))
    }

    /// Store resources for a reserved operation.
    pub fn store_op_resources(&mut self, user_data: usize, resources: IoResources) {
        if let Some(op) = self.ops.get_mut(user_data) {
            op.resources = resources;
        }
    }

    /// Get a submission queue entry to fill.
    /// The caller must fill it and verify it's valid.
    /// NOTE: This API is tricky because `squeue::Entry` setup usually consumes it.
    /// We'll let the Op construct the Entry and pass it here to push.
    pub fn push_entry(&mut self, entry: squeue::Entry) {
        let mut sq = self.ring.submission();
        // unsafe because we must ensure the entry is valid. Use io-uring guarantee.
        let _ = unsafe { sq.push(&entry) };
    }

    /// Submit an operation with its resources directly.
    pub fn submit_op_resources_internal(&mut self, user_data: usize, mut resources: IoResources) {
        // 1. Create SQE
        let sqe = resources.make_sqe().user_data(user_data as u64);

        // 2. Store resources
        self.store_op_resources(user_data, resources);

        // 3. Push to ring
        self.push_entry(sqe);
    }

    /// Register buffers with the kernel invocation of io_uring.
    pub fn register_buffers(&mut self, iovecs: Vec<libc::iovec>) -> io::Result<()> {
        // Safety: iovecs are valid as long as the caller (BufferPool) keeps them valid.
        // In our case, BufferPool keeps 'memory' alive forever (static/thread_local).
        unsafe { self.ring.submitter().register_buffers(&iovecs) }
    }

    /// Called by the Future when it is polled.
    pub fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<(io::Result<usize>, IoResources)> {
        self.ops.poll_op(user_data, cx)
    }

    /// Called when the Future is dropped.
    pub fn cancel_op(&mut self, user_data: usize) {
        if let Some(op) = self.ops.get_mut(user_data) {
            // We cannot remove it yet, because the kernel still has the pointer!
            // We just mark it cancelled. When CQE arrives, we'll drop resources.
            op.cancelled = true;
            op.waker = None;

            // Submit a cancel SQE to kernel to speed up cancellation.
            // This tells the kernel to try to cancel the operation identified by user_data.
            // Note: Cancellation is best-effort; the operation might complete before
            // the cancel request is processed. Either way, we'll get a CQE for the
            // original operation (possibly with -ECANCELED) and can clean up then.
            let cancel_sqe = opcode::AsyncCancel::new(user_data as u64)
                .build()
                .user_data(CANCEL_USER_DATA);
            self.push_entry(cancel_sqe);
        }
    }



    pub fn register_files(&mut self, files: &[crate::io::op::SysRawOp]) -> io::Result<Vec<crate::io::op::IoFd>> {
        // Note: this replaces the entire file table in io_uring currently.
        // A more advanced implementation would use IORING_REGISTER_FILES_UPDATE
        // to incrementally add files, or manage a sparse table.
        // For now, we assume this is called once or manages the full set.
        self.ring.submitter().register_files(files)?;
        
        let mut fixed_fds = Vec::with_capacity(files.len());
        for i in 0..files.len() {
            fixed_fds.push(crate::io::op::IoFd::Fixed(i as u32));
        }
        Ok(fixed_fds)
    }

    pub fn unregister_files(&mut self, _files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
        // specific file unregistration not strictly supported by raw unregister_files (which kills all)
        // unless we use update with -1.
        // For now, unregister all.
        self.ring.submitter().unregister_files()
    }
}

use crate::io::driver::Driver;

impl Driver for UringDriver {
    fn reserve_op(&mut self) -> usize {
        self.reserve_op_internal()
    }

    fn submit_op_resources(&mut self, user_data: usize, resources: IoResources) {
        self.submit_op_resources_internal(user_data, resources);
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, IoResources)> {
        self.poll_op(user_data, cx)
    }

    fn submit(&mut self) -> io::Result<()> {
        self.submit()
    }

    fn wait(&mut self) -> io::Result<()> {
        self.wait()
    }

    fn process_completions(&mut self) {
        self.process_completions_internal();
    }

    fn cancel_op(&mut self, user_data: usize) {
        self.cancel_op(user_data);
    }

    fn register_buffer_pool(&mut self, pool: &crate::io::buffer::BufferPool) -> io::Result<()> {
        let iovecs = pool.get_all_ptrs();
        self.register_buffers(iovecs)
    }

    fn register_files(&mut self, files: &[crate::io::op::SysRawOp]) -> io::Result<Vec<crate::io::op::IoFd>> {
        self.register_files(files)
    }

    fn unregister_files(&mut self, files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
        self.unregister_files(files)
    }

    fn wake(&mut self) -> io::Result<()> {
        let buf = 1u64.to_ne_bytes();
        let ret = unsafe { libc::write(self.waker_fd, buf.as_ptr() as *const _, 8) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    fn create_waker(&self) -> Arc<dyn RemoteWaker> {
        let new_fd = unsafe { libc::dup(self.waker_fd) };
        if new_fd < 0 {
            panic!("Failed to dup waker fd");
        }
        Arc::new(UringWaker(new_fd))
    }
}

impl Drop for UringDriver {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.waker_fd);
        }
    }
}
