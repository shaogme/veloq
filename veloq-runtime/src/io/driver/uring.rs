use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use io_uring::{IoUring, opcode, squeue};
use std::collections::VecDeque;
use std::io;
use std::os::unix::io::RawFd;
use std::task::{Context, Poll};

use crate::io::driver::{Injector, RemoteCompleter, RemoteWaker};
use crossbeam_queue::SegQueue;
use std::sync::Arc;

pub mod op;
pub mod submit;

use tracing::{debug, trace};

use crate::io::driver::uring::op::UringOp;
use crate::io::op::IntoPlatformOp;

#[derive(Default)]
pub struct UringOpState {
    pub submitted: bool,
    pub next: Option<usize>,
    pub remote_completer: Option<Box<dyn RemoteCompleter<UringOp>>>,
}

impl UringOpState {
    pub fn new() -> Self {
        Self::default()
    }
}

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

pub struct UringInjector {
    queue: Arc<SegQueue<Box<dyn FnOnce(&mut UringDriver) + Send>>>,
    waker_fd: RawFd,
}

impl Injector<UringDriver> for UringInjector {
    fn inject(&self, f: Box<dyn FnOnce(&mut UringDriver) + Send>) -> io::Result<()> {
        self.queue.push(f);
        // Wake up driver
        let buf = 1u64.to_ne_bytes();
        let ret = unsafe { libc::write(self.waker_fd, buf.as_ptr() as *const _, 8) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            // EAGAIN is fine, driver is already awake or buffer full (unlikely for eventfd)
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(());
            }
            // If it fails, we still pushed to queue, worst case driver picks it up later?
            // But we should report error.
            return Err(err);
        }
        Ok(())
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
    /// Payload (UringOpState) tracks submission state and backlog list.
    ops: OpRegistry<UringOp, UringOpState>,
    /// Head of the intrusive backlog list.
    backlog_head: Option<usize>,
    /// Tail of the intrusive backlog list.
    backlog_tail: Option<usize>,
    /// Queue for cancellation requests that failed to submit.
    pending_cancellations: VecDeque<usize>,

    waker_fd: RawFd,
    waker_token: Option<usize>,
    buffers_registered: bool,

    // Injector support
    queue: Arc<SegQueue<Box<dyn FnOnce(&mut UringDriver) + Send>>>,
}

impl UringDriver {
    pub fn new(config: &crate::config::Config) -> io::Result<Self> {
        let entries = config.uring.entries;
        let mut builder = IoUring::builder();

        builder
            .setup_coop_taskrun() // Reduce IPIs (Kernel 5.19+)
            .setup_single_issuer() // Optimized for single-threaded submission (Kernel 6.0+)
            .setup_defer_taskrun(); // Defer work until enter (Kernel 6.1+)

        if config.uring.mode == crate::config::IoMode::Polling {
            builder.setup_sqpoll(config.uring.sqpoll_idle_ms); // Kernel 5.1+
        }

        let ring = builder.build(entries).or_else(|e| {
            // Fallback for older kernels if flags are unsupported (EINVAL)
            if e.raw_os_error() == Some(libc::EINVAL) {
                // If the optimized build failed, try a basic one.
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

        debug!("Initalized UringDriver with {} entries", entries);

        let mut driver = Self {
            ring,
            ops,
            backlog_head: None,
            backlog_tail: None,
            pending_cancellations: VecDeque::new(),
            waker_fd,
            waker_token: None,
            buffers_registered: false,
            queue: Arc::new(SegQueue::new()),
        };

        driver.submit_waker();

        Ok(driver)
    }

    fn submit_waker(&mut self) {
        if self.waker_token.is_some() {
            return;
        }

        let fd = self.waker_fd;
        let op = crate::io::op::Wakeup {
            fd: crate::io::op::IoFd::Raw(crate::io::RawHandle { fd }),
        };
        // Use into_platform_op to convert to UringOp
        let uring_op = <crate::io::op::Wakeup as IntoPlatformOp<UringDriver>>::into_platform_op(op);

        let user_data = self
            .ops
            .insert(OpEntry::new(Some(uring_op), UringOpState::new()));
        self.waker_token = Some(user_data);

        // Generate SQE
        let sqe = if let Some(entry) = self.ops.get_mut(user_data) {
            if let Some(ref mut resources) = entry.resources {
                Some(unsafe { (resources.vtable.make_sqe)(resources).user_data(user_data as u64) })
            } else {
                None
            }
        } else {
            None
        };

        if let Some(sqe) = sqe {
            if self.push_entry(sqe) {
                if let Some(entry) = self.ops.get_mut(user_data) {
                    entry.platform_data.submitted = true;
                }
            } else {
                // Waker failed to submit. This is bad but handled by backlog logic.
                self.push_backlog(user_data);
            }
        }
    }

    pub fn submit_to_kernel(&mut self) -> io::Result<()> {
        trace!("submit_to_kernel entered");
        if self.ring.params().is_setup_sqpoll() {
            if self.ring.submission().need_wakeup() {
                self.ring.submit()?;
            }
        } else {
            self.ring.submit()?;
        }
        // Always try to flush backlog after submit, as submit likely freed up SQ space
        self.flush_backlog();
        Ok(())
    }

    /// Wait for completions.
    pub fn wait(&mut self) -> io::Result<()> {
        // Try to flush backlog first before waiting
        self.flush_cancellations();
        self.flush_backlog();

        if self.ops.is_empty() {
            return Ok(());
        }

        if !self.ring.completion().is_empty() {
            self.process_completions_internal();
            return Ok(());
        }

        self.ring.submit_and_wait(1)?;
        self.process_completions_internal();
        self.process_injected();

        // After wait (which implies submit), we might have space
        self.flush_cancellations();
        self.flush_backlog();
        Ok(())
    }

    /// Process the completion queue.
    fn process_completions_internal(&mut self) {
        let mut needs_waker_resubmit = false;

        {
            let mut cqe_kicker = self.ring.completion();
            cqe_kicker.sync();

            trace!("Processing completions, count={}", cqe_kicker.len());

            for cqe in cqe_kicker {
                let user_data = cqe.user_data() as usize;

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

                    let res = if let Some(ref mut resources) = op.resources {
                        unsafe { (resources.vtable.on_complete)(resources, cqe.result()) }
                    } else if cqe.result() >= 0 {
                        Ok(cqe.result() as usize)
                    } else {
                        Err(io::Error::from_raw_os_error(-cqe.result()))
                    };

                    if op.cancelled {
                        self.ops.remove(user_data);
                    } else {
                        // Check if it has a remote completer
                        if let Some(completer) = op.platform_data.remote_completer.take() {
                            // Must take resources
                            if let Some(res_op) = op.resources.take() {
                                completer.complete(res, res_op);
                            }
                            // Remove op
                            self.ops.remove(user_data);
                        } else {
                            op.result = Some(res);
                            if let Some(waker) = op.waker.take() {
                                waker.wake();
                            }
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
            // Ensure waker is in the ring immediately to avoid lost wakeups
            self.flush_backlog();
        }
    }

    /// Try to push an entry to the submission queue.
    /// Returns true if successful, false if SQ is full.
    fn push_entry(&mut self, entry: squeue::Entry) -> bool {
        trace!("Pushing SQE user_data={}", entry.get_user_data());
        let mut sq = self.ring.submission();

        if unsafe { sq.push(&entry) }.is_ok() {
            return true;
        }

        // SQ full, try to submit (flush)
        drop(sq);
        let _ = self.ring.submit(); // Ignore error here, we retry push anyway

        let mut sq = self.ring.submission();
        if unsafe { sq.push(&entry) }.is_ok() {
            return true;
        }

        debug!("SQ full even after flush");
        false
    }

    /// Try to submit pending cancellations
    fn flush_cancellations(&mut self) {
        let mut submitted_count = 0;
        let limit = self.pending_cancellations.len();

        while submitted_count < limit {
            if let Some(user_data) = self.pending_cancellations.front().cloned() {
                // If the operation is gone or completed, we don't need to cancel anymore
                if !self.ops.contains(user_data) {
                    self.pending_cancellations.pop_front();
                    continue;
                }

                let cancel_sqe = opcode::AsyncCancel::new(user_data as u64)
                    .build()
                    .user_data(CANCEL_USER_DATA);

                if self.push_entry(cancel_sqe) {
                    self.pending_cancellations.pop_front();
                    submitted_count += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Attempt to submit operations from the backlog.
    fn flush_backlog(&mut self) {
        while let Some(user_data) = self.backlog_head {
            // 1. Check state
            let (is_cancelled, is_submitted, has_resources) =
                if let Some(entry) = self.ops.get_mut(user_data) {
                    (
                        entry.cancelled,
                        entry.platform_data.submitted,
                        entry.resources.is_some(),
                    )
                } else {
                    // Op missing? Pop and continue
                    self.pop_backlog();
                    continue;
                };

            // Optimize: If cancelled, do NOT submit.
            if is_cancelled {
                self.pop_backlog();
                // Complete with ECANCELED
                if let Some(entry) = self.ops.get_mut(user_data) {
                    entry.result = Some(Err(io::Error::from_raw_os_error(libc::ECANCELED)));
                    if let Some(waker) = entry.waker.take() {
                        waker.wake();
                    }
                }
                continue;
            }

            if is_submitted {
                self.pop_backlog();
                continue;
            }

            if !has_resources {
                self.pop_backlog();
                continue;
            }

            // 2. Generate SQE
            let sqe = {
                let entry = self.ops.get_mut(user_data).unwrap();
                let res = entry.resources.as_mut().unwrap();
                unsafe { (res.vtable.make_sqe)(res).user_data(user_data as u64) }
            };

            // 3. Push
            if self.push_entry(sqe) {
                self.pop_backlog();
                if let Some(entry) = self.ops.get_mut(user_data) {
                    entry.platform_data.submitted = true;
                    if let Some(waker) = entry.waker.take() {
                        waker.wake();
                    }
                }
            } else {
                // Full
                break;
            }
        }
    }

    fn push_backlog(&mut self, user_data: usize) {
        if let Some(tail) = self.backlog_tail {
            // Update old tail
            if let Some(entry) = self.ops.get_mut(tail) {
                entry.platform_data.next = Some(user_data);
            }
            self.backlog_tail = Some(user_data);
        } else {
            // Empty
            self.backlog_head = Some(user_data);
            self.backlog_tail = Some(user_data);
        }
        // Ensure new node terminates
        if let Some(entry) = self.ops.get_mut(user_data) {
            entry.platform_data.next = None;
        }
    }

    fn pop_backlog(&mut self) -> Option<usize> {
        let head = self.backlog_head?;
        // get next
        let next = if let Some(entry) = self.ops.get_mut(head) {
            entry.platform_data.next
        } else {
            None
        };

        self.backlog_head = next;
        if next.is_none() {
            self.backlog_tail = None;
        }

        if let Some(entry) = self.ops.get_mut(head) {
            entry.platform_data.next = None;
        }

        Some(head)
    }

    pub fn register_buffer_regions(
        &mut self,
        regions: &[crate::io::buffer::BufferRegion],
    ) -> io::Result<Vec<usize>> {
        if self.buffers_registered {
            // Assume existing registration matches?
            // Since we return indices, and they are usually 0..N, we return based on input length.
            // Ideally we shouldn't registering twice unless regions are different?
            // For now, simple behavior.
            return Ok((0..regions.len()).collect());
        }

        let iovecs: Vec<libc::iovec> = regions
            .iter()
            .map(|region| libc::iovec {
                iov_base: region.ptr.as_ptr() as *mut _,
                iov_len: region.len,
            })
            .collect();

        match unsafe { self.ring.submitter().register_buffers(&iovecs) } {
            Ok(_) => {
                self.buffers_registered = true;
                Ok((0..regions.len()).collect())
            }
            Err(e) => {
                if e.raw_os_error() == Some(libc::EBUSY) {
                    self.buffers_registered = true;
                    Ok((0..regions.len()).collect())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn cancel_op_internal(&mut self, user_data: usize) {
        if let Some(op) = self.ops.get_mut(user_data) {
            op.cancelled = true;
            op.waker = None;

            let cancel_sqe = opcode::AsyncCancel::new(user_data as u64)
                .build()
                .user_data(CANCEL_USER_DATA);

            if !self.push_entry(cancel_sqe) {
                // Store for later retry
                self.pending_cancellations.push_back(user_data);
            }
        }
        // Remove from backlog if present? O(N).
        // We let flush_backlog handle it (it checks cancelled).
    }
    fn process_injected(&mut self) {
        while let Some(f) = self.queue.pop() {
            f(self);
        }
    }
}

use crate::io::driver::Driver;

impl Driver for UringDriver {
    type Op = UringOp;
    type RemoteInjector = UringInjector;

    fn injector(&self) -> Arc<Self::RemoteInjector> {
        Arc::new(UringInjector {
            queue: self.queue.clone(),
            waker_fd: self.waker_fd,
        })
    }

    fn reserve_op(&mut self) -> usize {
        let id = self.ops.insert(OpEntry::new(None, UringOpState::new()));
        trace!(id, "Reserved op slot");
        id
    }

    fn attach_remote_completer(
        &mut self,
        user_data: usize,
        completer: Box<dyn RemoteCompleter<Self::Op>>,
    ) {
        if let Some(entry) = self.ops.get_mut(user_data) {
            entry.platform_data.remote_completer = Some(completer);
        }
    }

    fn submit(
        &mut self,
        user_data: usize,
        op: Self::Op,
    ) -> Result<Poll<()>, (io::Error, Self::Op)> {
        // 1. Store resources
        if let Some(entry) = self.ops.get_mut(user_data) {
            entry.resources = Some(op);
        }

        // 2. Generate SQE
        let sqe = {
            let entry = self.ops.get_mut(user_data).expect("invalid user_data");
            let op = entry.resources.as_mut().expect("resources missing");
            unsafe { (op.vtable.make_sqe)(op).user_data(user_data as u64) }
        };

        // 3. Push
        if self.push_entry(sqe) {
            trace!(user_data, "Submitted to SQ");
            if let Some(entry) = self.ops.get_mut(user_data) {
                entry.platform_data.submitted = true;
            }
            Ok(Poll::Ready(()))
        } else {
            debug!(user_data, "SQ full, pushing to backlog");
            // SQ Full. Add to backlog.
            if let Some(entry) = self.ops.get_mut(user_data) {
                entry.platform_data.submitted = false;
            }
            self.push_backlog(user_data);
            Ok(Poll::Pending)
        }
    }

    fn submit_background(&mut self, mut op: Self::Op) -> io::Result<()> {
        if op.vtable.make_sqe as usize == submit::make_sqe_close as usize {
            let sqe = unsafe { (op.vtable.make_sqe)(&mut op).user_data(BACKGROUND_USER_DATA) };

            if !self.push_entry(sqe) {
                return Err(io::Error::other("sq full"));
            }
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "background op only supports Close",
            ))
        }
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, Self::Op)> {
        // First check if stored in backlog (not submitted)
        if let Some(entry) = self.ops.get_mut(user_data)
            && !entry.platform_data.submitted
        {
            // Not in ring yet. Try to flush backlog.
            self.flush_backlog();
            self.flush_cancellations();

            // Check again
            let entry = self.ops.get_mut(user_data).unwrap();
            if !entry.platform_data.submitted {
                // Still not in ring. Register waker.
                entry.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }

        // Delegate to ops registry for result check
        self.ops.poll_op(user_data, cx)
    }

    fn submit_queue(&mut self) -> io::Result<()> {
        self.flush_cancellations();
        self.flush_backlog();
        self.submit_to_kernel()
    }

    fn wait(&mut self) -> io::Result<()> {
        self.wait()?;
        self.process_injected();
        Ok(())
    }

    fn process_completions(&mut self) {
        self.process_completions_internal();
        self.process_injected();
        self.flush_cancellations();
        self.flush_backlog();
    }

    fn cancel_op(&mut self, user_data: usize) {
        self.cancel_op_internal(user_data);
    }

    fn register_buffer_regions(
        &mut self,
        regions: &[crate::io::buffer::BufferRegion],
    ) -> io::Result<Vec<usize>> {
        self.register_buffer_regions(regions)
    }

    fn register_files(
        &mut self,
        files: &[crate::io::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>> {
        let fds: Vec<i32> = files.iter().map(|h| h.fd).collect();
        self.ring.submitter().register_files(&fds)?;

        let mut fixed_fds = Vec::with_capacity(files.len());
        for i in 0..files.len() {
            fixed_fds.push(crate::io::op::IoFd::Fixed(i as u32));
        }
        Ok(fixed_fds)
    }

    fn unregister_files(&mut self, _files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
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

    fn inner_handle(&self) -> crate::io::RawHandle {
        use std::os::unix::io::AsRawFd;
        crate::io::RawHandle {
            fd: self.ring.as_raw_fd(),
        }
    }

    fn notify_mesh(&mut self, handle: crate::io::RawHandle) -> io::Result<()> {
        let fd = handle.fd;
        // Send a MsgRing to the target ring. (Kernel 5.18+)
        // We set data to BACKGROUND_USER_DATA so the target treats it as a wake-up (and ignores the CQE).
        // We set our user_data to BACKGROUND_USER_DATA so we also ignore the completion of the MsgRing op itself.
        let sqe = opcode::MsgRingData::new(io_uring::types::Fd(fd), 0, BACKGROUND_USER_DATA, None)
            .build()
            .user_data(BACKGROUND_USER_DATA);

        if !self.push_entry(sqe) {
            return Err(io::Error::new(io::ErrorKind::Other, "SQ full"));
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
