use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use io_uring::{IoUring, opcode, squeue};
use std::io;
use std::os::unix::io::RawFd;
use std::task::{Context, Poll};

use crate::io::driver::RemoteWaker;
use std::sync::Arc;

pub mod op;
mod submit;

use op::{UringOp, UringWakeup};
use submit::UringSubmit;

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
const BACKGROUND_USER_DATA: u64 = u64::MAX - 2;

pub struct UringDriver {
    /// The actual io_uring instance
    ring: IoUring,
    /// Store for in-flight operations.
    /// The key (usize) is used as the io_uring user_data.
    ops: OpRegistry<UringOp, ()>,
    waker_fd: RawFd,
    waker_token: Option<usize>,
}

impl UringDriver {
    pub fn new(config: &crate::config::Config) -> io::Result<Self> {
        let entries = config.uring.entries;
        let mut builder = IoUring::builder();

        builder
            .setup_coop_taskrun() // Reduce IPIs
            .setup_single_issuer() // Optimized for single-threaded submission
            .setup_defer_taskrun(); // Defer work until enter

        if config.uring.mode == crate::config::IoMode::Polling {
            builder.setup_sqpoll(config.uring.sqpoll_idle_ms);
        }

        let ring = builder.build(entries).or_else(|e| {
            // Fallback for older kernels if flags are unsupported (EINVAL)
            if e.raw_os_error() == Some(libc::EINVAL) {
                // If the optimized build failed, try a basic one.
                // Note: This might degrade from Polling to Interrupt if Polling was requested but unsupported.
                // A production system might want to log a warning here.
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

        let fd = self.waker_fd as usize;
        let op = UringWakeup::new(fd);
        let uring_op = UringOp::Wakeup(op);

        let user_data = self.ops.insert(OpEntry::new(Some(uring_op), ()));
        self.waker_token = Some(user_data);

        // Generate and push SQE
        if let Some(entry) = self.ops.get_mut(user_data) {
            if let Some(ref mut resources) = entry.resources {
                let sqe = resources.make_sqe().user_data(user_data as u64);
                self.push_entry(sqe);
            }
        }
    }

    pub fn submit_to_kernel(&mut self) -> io::Result<()> {
        if self.ring.params().is_setup_sqpoll() {
            if self.ring.submission().need_wakeup() {
                self.ring.submit()?;
            }
        } else {
            self.ring.submit()?;
        }
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
            self.process_completions_internal();
            return Ok(());
        }

        self.ring.submit_and_wait(1)?;
        self.process_completions_internal();
        Ok(())
    }

    /// Process the completion queue.
    fn process_completions_internal(&mut self) {
        let mut needs_waker_resubmit = false;

        {
            let mut cqe_kicker = self.ring.completion();
            cqe_kicker.sync();

            for cqe in cqe_kicker {
                let user_data = cqe.user_data() as usize;

                // Skip special user_data values:
                // - u64::MAX: reserved/special
                // - CANCEL_USER_DATA: completion of our cancel requests (we don't need to handle these)
                // - BACKGROUND_USER_DATA: fire-and-forget background ops (e.g. Close)
                if user_data == u64::MAX as usize
                    || user_data == CANCEL_USER_DATA as usize
                    || user_data == BACKGROUND_USER_DATA as usize
                {
                    continue;
                }

                if Some(user_data) == self.waker_token {
                    needs_waker_resubmit = true;
                    continue;
                }

                if self.ops.contains(user_data) {
                    let op = &mut self.ops[user_data];

                    // Call on_complete on the resources
                    let res = if let Some(ref mut resources) = op.resources {
                        resources.on_complete(cqe.result())
                    } else {
                        // No resources, just convert result
                        if cqe.result() >= 0 {
                            Ok(cqe.result() as usize)
                        } else {
                            Err(io::Error::from_raw_os_error(-cqe.result()))
                        }
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

    /// Get a submission queue entry to fill.
    fn push_entry(&mut self, entry: squeue::Entry) {
        let mut sq = self.ring.submission();
        
        // Attempt to push the entry
        if unsafe { sq.push(&entry) }.is_err() {
            // SQ is full. We must submit current entries to clear space.
            drop(sq); // Drop mutable borrow to call submit
            if let Err(e) = self.ring.submit() {
                // If submit fails, we have a serious problem. 
                // Panic is appropriate here as we can't recover easily and maintaining consistency is hard.
                panic!("io_uring submit failed during push recovery: {}", e);
            }
            
            // Retry push
            let mut sq = self.ring.submission();
            if unsafe { sq.push(&entry) }.is_err() {
                // If it still fails, it essentially means the kernel isn't consuming SQEs fast enough 
                // or we are trying to push more than the ring size at once without processing completions.
                // For a robust runtime, we might want to wait/park, but for now panicking prevents hidden deadlocks.
                panic!("io_uring submission queue is full and cannot be cleared");
            }
        }
    }

    /// Register buffers with the kernel invocation of io_uring.
    pub fn register_buffers(&mut self, iovecs: Vec<libc::iovec>) -> io::Result<()> {
        // Safety: iovecs are valid as long as the caller (BufferPool) keeps them valid.
        // In our case, BufferPool keeps 'memory' alive forever (static/thread_local).
        unsafe { self.ring.submitter().register_buffers(&iovecs) }
    }

    /// Called when the Future is dropped.
    fn cancel_op_internal(&mut self, user_data: usize) {
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
}

use crate::io::driver::Driver;

impl Driver for UringDriver {
    type Op = UringOp;

    fn reserve_op(&mut self) -> usize {
        self.ops.insert(OpEntry::new(None, ()))
    }

    fn submit(&mut self, user_data: usize, op: Self::Op) {
        // 1. Store resources (Move to Stable Memory first)
        if let Some(entry) = self.ops.get_mut(user_data) {
            entry.resources = Some(op);
        }

        // 2. Generate SQE using the Op in its final memory location
        // This ensures that any pointers to 'self' passed to the kernel (e.g. msghdr, iovec)
        // refer to the stable address in the slab, not a temporary stack address.
        let sqe = {
            let entry = self.ops.get_mut(user_data).expect("invalid user_data");
            let op = entry.resources.as_mut().expect("resources missing");
            op.make_sqe().user_data(user_data as u64)
        };

        // 3. Push to ring
        self.push_entry(sqe);
    }

    fn submit_background(&mut self, mut op: Self::Op) -> io::Result<()> {
        match op {
            UringOp::Close(_) => {
                let sqe = op.make_sqe().user_data(BACKGROUND_USER_DATA);

                // Try to push
                // Optimization: direct access to ring submission to handle full queue
                let mut sq = self.ring.submission();
                if unsafe { sq.push(&sqe) }.is_err() {
                    // Queue is full. Try to submit existing to clear space.
                    drop(sq); // mutable borrow end
                    self.ring.submit()?;

                    let mut sq = self.ring.submission();
                    if unsafe { sq.push(&sqe) }.is_err() {
                        return Err(io::Error::new(io::ErrorKind::Other, "sq full"));
                    }
                }
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "background op only supports Close",
            )),
        }
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, Self::Op)> {
        self.ops.poll_op(user_data, cx)
    }

    fn submit_queue(&mut self) -> io::Result<()> {
        self.submit_to_kernel()
    }

    fn wait(&mut self) -> io::Result<()> {
        self.wait()
    }

    fn process_completions(&mut self) {
        self.process_completions_internal();
    }

    fn cancel_op(&mut self, user_data: usize) {
        self.cancel_op_internal(user_data);
    }

    fn register_buffer_pool(&mut self, pool: &crate::io::buffer::BufferPool) -> io::Result<()> {
        let iovecs = pool.get_all_ptrs();
        self.register_buffers(iovecs)
    }

    fn register_files(
        &mut self,
        files: &[crate::io::op::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>> {
        // Note: this replaces the entire file table in io_uring currently.
        // A more advanced implementation would use IORING_REGISTER_FILES_UPDATE
        // to incrementally add files, or manage a sparse table.
        // For now, we assume this is called once or manages the full set.
        // Convert usize to i32 for io_uring
        let fds: Vec<i32> = files.iter().map(|&f| f as i32).collect();
        self.ring.submitter().register_files(&fds)?;

        let mut fixed_fds = Vec::with_capacity(files.len());
        for i in 0..files.len() {
            fixed_fds.push(crate::io::op::IoFd::Fixed(i as u32));
        }
        Ok(fixed_fds)
    }

    fn unregister_files(&mut self, _files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
        // specific file unregistration not strictly supported by raw unregister_files (which kills all)
        // unless we use update with -1.
        // For now, unregister all.
        self.ring.submitter().unregister_files()
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
