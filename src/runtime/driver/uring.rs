use crate::runtime::buffer::{BufferPool, FixedBuf};
use crate::runtime::op::IoResources;
use io_uring::{IoUring, squeue};
use slab::Slab;
use std::io;
use std::task::{Context, Poll, Waker};


mod ops;
use ops::UringOp;

pub struct UringDriver {
    /// The actual io_uring instance
    ring: IoUring,
    /// Store for in-flight operations.
    /// The key (usize) is used as the io_uring user_data.
    ops: Slab<OperationLifecycle>,
    /// Managed buffer pool
    buffer_pool: BufferPool,
}

struct OperationLifecycle {
    waker: Option<Waker>,
    /// If the operation completes but the future hasn't polled it yet.
    result: Option<io::Result<u32>>,
    /// Resources held by the kernel (e.g., buffers).
    /// Kept here to ensure they live as long as the kernel needs them.
    /// When the operation completes, these are returned to the user or dropped.
    resources: IoResources,
    /// If true, the Future has been dropped, so we should discard resources upon completion.
    cancelled: bool,
}

impl UringDriver {
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;
        let buffer_pool = BufferPool::new();

        // Register buffers immediately
        let ops = Slab::with_capacity(entries as usize);

        let driver = Self {
            ring,
            ops,
            buffer_pool,
        };

        // We need to register buffers.
        // Note: Driver::register_buffers requires &mut self, but we are constructing it.
        // We can do it on the ring directly or helper.
        // Let's use the helper we have, but we need to structure it so we can access `buffer_pool`

        let iovecs = driver.buffer_pool.get_all_ptrs();
        unsafe { driver.ring.submitter().register_buffers(&iovecs) }?;

        Ok(driver)
    }

    pub fn submit(&mut self) -> io::Result<()> {
        self.ring.submit()?;
        Ok(())
    }

    /// Wait for completions.
    pub fn wait(&mut self) -> io::Result<()> {
        // If we have no ops, we shouldn't block indefinitely in a real app if we have other things (timers).
        // But for now, let's wait for at least one event if we have ops in flight.
        // Or blindly enter.
        // Phase 1 had a dummy waker; now we can block properly.
        if self.ops.is_empty() {
            return Ok(());
        }
        self.ring.submit_and_wait(1)?;
        self.process_completions();
        Ok(())
    }

    /// Process the completion queue.
    pub fn process_completions_internal(&mut self) {
        let mut cqe_kicker = self.ring.completion();
        cqe_kicker.sync();

        for cqe in cqe_kicker {
            let user_data = cqe.user_data() as usize;

            if user_data == u64::MAX as usize {
                // Special value?
                continue;
            }

            if self.ops.contains(user_data) {
                let op = &mut self.ops[user_data];
                let res = if cqe.result() >= 0 {
                    Ok(cqe.result() as u32)
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

    /// Register a new operation.
    /// Returns the user_data key.
    /// `resources` are the moved-in buffers/fds that must live until completion.
    /// Reserve a slot for an operation.
    pub fn reserve_op_internal(&mut self) -> usize {
        self.ops.insert(OperationLifecycle {
            waker: None,
            result: None,
            resources: IoResources::None, // Default to None
            cancelled: false,
        })
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
    ) -> Poll<(io::Result<u32>, IoResources)> {
        if let Some(op) = self.ops.get_mut(user_data) {
            if let Some(res) = op.result.take() {
                // Operation complete.
                // We remove it from Slab and take resources
                let op_lifecycle = self.ops.remove(user_data);
                // Return resources
                Poll::Ready((res, op_lifecycle.resources))
            } else {
                // Still pending. Update waker.
                op.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        } else {
            // Should not happen unless logic error
            Poll::Ready((
                Err(io::Error::new(io::ErrorKind::Other, "Op not found")),
                IoResources::None,
            ))
        }
    }

    /// Called when the Future is dropped.
    pub fn cancel_op(&mut self, user_data: usize) {
        if let Some(op) = self.ops.get_mut(user_data) {
            // We cannot remove it yet, because the kernel still has the pointer!
            // We just mark it cancelled. When CQE arrives, we'll drop resources.
            op.cancelled = true;
            op.waker = None;

            // Optionally: submit a cancel SQE to kernel to speed things up.
            // For now, let's just detach.
        }
    }

    pub fn alloc_fixed_buffer(&self) -> Option<FixedBuf> {
        self.buffer_pool.alloc()
    }
}

use crate::runtime::driver::Driver;

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
    ) -> Poll<(io::Result<u32>, IoResources)> {
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

    fn alloc_fixed_buffer(&self) -> Option<FixedBuf> {
        self.alloc_fixed_buffer()
    }
}
