mod blocking;
mod ext;
pub mod op;
pub mod rio;
mod submit;
#[cfg(test)]
mod tests;

use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use crate::io::driver::{Driver, RemoteWaker};
use blocking::ThreadPool;
use ext::Extensions;
use op::{IocpOp, OverlappedEntry};
use rio::RioState;
use std::io;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use submit::SubmissionResult;
use tracing::{debug, trace};

const WAKEUP_USER_DATA: usize = usize::MAX;
const RUN_CLOSURE_KEY: usize = usize::MAX - 2;
pub(crate) const RIO_EVENT_KEY: usize = usize::MAX - 1;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use veloq_wheel::{TaskId, Wheel, WheelConfig};

use super::{Injector, RemoteCompleter};
use std::sync::Arc;

pub struct IocpInjector {
    port: HANDLE,
}

unsafe impl Send for IocpInjector {}
unsafe impl Sync for IocpInjector {}

impl Injector<IocpDriver> for IocpInjector {
    fn inject(&self, f: Box<dyn FnOnce(&mut IocpDriver) + Send>) -> std::io::Result<()> {
        let ptr = Box::into_raw(Box::new(f));
        let res =
            unsafe { PostQueuedCompletionStatus(self.port, 0, RUN_CLOSURE_KEY, ptr as *mut _) };
        if res == 0 {
            let err = std::io::Error::last_os_error();
            tracing::error!(
                ?err,
                "IocpInjector::inject failed to PostQueuedCompletionStatus"
            );
            // Restore box to drop it
            let _ = unsafe { Box::from_raw(ptr) };
            return Err(err);
        }
        Ok(())
    }
}

pub enum PlatformData {
    Timer(TaskId),
    Background,
    Remote(Box<dyn RemoteCompleter<IocpOp>>),
    None,
}

pub type PreInit = usize;

pub struct IocpDriver {
    pub(crate) port: HANDLE,
    pub(crate) ops: OpRegistry<IocpOp, PlatformData>,
    pub(crate) extensions: Extensions,
    pub(crate) wheel: Wheel<usize>,
    pub(crate) registered_files: Vec<Option<HANDLE>>,
    pub(crate) free_slots: Vec<usize>,
    pub(crate) pool: ThreadPool,
    pub(crate) injector: Arc<IocpInjector>,

    // RIO Support (Decoupled)
    pub(crate) rio_state: Option<RioState>,
}

struct IocpWaker(HANDLE);

unsafe impl Send for IocpWaker {}
unsafe impl Sync for IocpWaker {}

impl RemoteWaker for IocpWaker {
    fn wake(&self) -> io::Result<()> {
        let res = unsafe {
            PostQueuedCompletionStatus(self.0, 0, WAKEUP_USER_DATA, std::ptr::null_mut())
        };
        if res == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for IocpWaker {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

impl IocpDriver {
    pub fn create_pre_init(_config: &crate::config::Config) -> io::Result<PreInit> {
        // Create a new completion port.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };

        if port.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(port as usize)
        }
    }

    pub fn pre_init_handle(pre: &PreInit) -> crate::io::RawHandle {
        crate::io::RawHandle { handle: *pre as _ }
    }

    pub fn new(config: &crate::config::Config) -> io::Result<Self> {
        let pre = Self::create_pre_init(config)?;
        Self::new_from_pre_init(config, pre)
    }

    pub fn new_from_pre_init(
        config: &crate::config::Config,
        port_val: PreInit,
    ) -> io::Result<Self> {
        let port = port_val as HANDLE;
        debug!(port = ?port, "Initializing IocpDriver");
        // Load extensions
        let extensions = Extensions::new()?;
        let entries = config.iocp.entries;

        // Initialize RIO State
        let mut rio_state = RioState::new(port, entries, &extensions)?;

        let ops = OpRegistry::with_capacity(entries as usize);

        // Pre-register existing pages (created by with_capacity)
        if let Some(rio) = &mut rio_state {
            for i in 0..ops.page_count() {
                rio.ensure_slab_page_registration(i, &ops, &extensions);
            }
        }

        Ok(Self {
            port,
            ops,
            extensions,
            wheel: Wheel::new(WheelConfig::default()),
            registered_files: Vec::new(),
            free_slots: Vec::new(),
            pool: ThreadPool::new(16, 128, 1024, Duration::from_secs(30)),
            injector: Arc::new(IocpInjector { port }),
            rio_state,
        })
    }

    /// Retrieve completion events from the port.
    /// timeout_ms: 0 for poll, u32::MAX for wait.
    fn get_completion(&mut self, timeout_ms: u32) -> io::Result<()> {
        let mut bytes_transferred = 0;
        let mut completion_key = 0;
        let mut overlapped = std::ptr::null_mut();

        // Calculate timeout based on wheel
        let mut wait_ms = timeout_ms;
        if let Some(delay) = self.wheel.next_timeout() {
            let millis = delay.as_millis().min(u32::MAX as u128) as u32;
            wait_ms = std::cmp::min(wait_ms, millis);
        }

        trace!(wait_ms, "Entering GetQueuedCompletionStatus");
        let start = Instant::now();
        let res = unsafe {
            GetQueuedCompletionStatus(
                self.port,
                &mut bytes_transferred,
                &mut completion_key,
                &mut overlapped,
                wait_ms,
            )
        };
        let elapsed = start.elapsed();

        // Process expired timers
        let expired = self.wheel.advance(elapsed);
        for user_data in expired {
            if let Some(op) = self.ops.get_mut(user_data) {
                if !op.cancelled && op.result.is_none() {
                    op.result = Some(Ok(0));
                    if let Some(waker) = op.waker.take() {
                        waker.wake();
                    }
                }
                // Clean up platform data
                op.platform_data = PlatformData::None;
            }
        }

        // Determine user_data from overlapped or completion_key
        let user_data = if completion_key == RUN_CLOSURE_KEY {
            trace!("Received RUN_CLOSURE_KEY");
            // Execute closure
            if !overlapped.is_null() {
                let f = unsafe {
                    Box::from_raw(overlapped as *mut Box<dyn FnOnce(&mut IocpDriver) + Send>)
                };
                f(self);
            }
            return Ok(());
        } else if completion_key == RIO_EVENT_KEY {
            // RIO event is triggered. Process RIO CQ.
            if let Some(rio) = &mut self.rio_state {
                return rio.process_completions(&mut self.ops, &self.extensions);
            } else {
                // Should not happen if RIO_EVENT_KEY triggered but state disappeared?
                return Ok(());
            }
        } else if !overlapped.is_null() {
            let entry = unsafe { &*(overlapped as *const OverlappedEntry) };
            entry.user_data
        } else {
            if res == 0 {
                let err = unsafe { GetLastError() };
                if err == WAIT_TIMEOUT {
                    return Ok(());
                }
                debug!(err, "GetQueuedCompletionStatus returned error");
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            completion_key
        };

        trace!(user_data, bytes_transferred, "CQE received");

        if self.ops.contains(user_data) {
            // Check if cancelled
            if self.ops[user_data].cancelled {
                let entry = self.ops.remove(user_data);

                // If it was a remote op, we must complete it (with error) so the waiter doesn't panic
                if let PlatformData::Remote(completer) = entry.platform_data {
                    if let Some(op) = entry.resources {
                        completer.complete(Err(io::Error::from(io::ErrorKind::Interrupted)), op);
                    }
                }

                return Ok(());
            }

            let op_entry = &mut self.ops[user_data];

            // Check if background or remote
            match &mut op_entry.platform_data {
                PlatformData::Background => {
                    trace!(user_data, "Background op completion ignored");
                    self.ops.remove(user_data);
                    return Ok(());
                }
                PlatformData::Remote(_) => {
                    // We need to move the completer out
                    let data = std::mem::replace(&mut op_entry.platform_data, PlatformData::None);
                    if let PlatformData::Remote(completer) = data {
                        // We must take the resources (op) out
                        let resources = op_entry.resources.take();
                        if let Some(iocp_op) = resources {
                            // Extract result
                            let result = if res == 0 {
                                Err(io::Error::last_os_error())
                            } else {
                                Ok(bytes_transferred as usize)
                            };

                            // The result in op.result might be set if we used blocking offload?
                            // But usually Remote op is just pure IO submit.
                            // However, we should respect if result was already present?
                            // For IOCP, standard completion path just gives us bytes_transferred.

                            completer.complete(result, iocp_op);
                            self.ops.remove(user_data);
                        } else {
                            // Should not happen?
                            tracing::error!(
                                user_data,
                                "RemoteOp completion missing resources - Task will likely panic"
                            );
                            self.ops.remove(user_data);
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }

            let op = &mut self.ops[user_data];
            let mut result_is_ready = false;

            if op.result.is_none()
                && let Some(iocp_op) = op.resources.as_mut()
            {
                // Check for blocking result first
                if let Some(entry) = iocp_op.header.blocking_result.take() {
                    op.result = Some(entry);
                    result_is_ready = true;
                } else {
                    // Normal IO completion
                    let mut result = if res == 0 {
                        let e = io::Error::last_os_error();
                        // trace!(?e, "IO failed");
                        Err(e)
                    } else {
                        Ok(bytes_transferred as usize)
                    };

                    if result.is_ok() {
                        // Use VTable on_complete if available
                        if let Some(on_comp) = iocp_op.vtable.on_complete {
                            let val = result.unwrap();
                            result = unsafe { (on_comp)(iocp_op, val, &self.extensions) };
                        }
                    }
                    op.result = Some(result);
                    result_is_ready = true;
                }
            }

            if result_is_ready {
                // Clean up resources
                op.platform_data = PlatformData::None;
                if let Some(waker) = op.waker.take() {
                    waker.wake();
                }
            }
        }

        Ok(())
    }

    pub fn register_buffer_regions(
        &mut self,
        regions: &[crate::io::buffer::BufferRegion],
    ) -> io::Result<Vec<usize>> {
        if let Some(rio) = &mut self.rio_state {
            rio.register_buffers(regions, &self.extensions)?;
            // RIO state stores IDs sequentially in registered_bufs matching the regions input
            return Ok((0..regions.len()).collect());
        }
        // If not RIO, we might just return dummy indices if we supported other mechanisms,
        // but currently IOCP driver purely relies on RIO for registration.
        // If no RIO, we effectively "do nothing" but return tokens that won't be used (or will fail later).
        Ok((0..regions.len()).collect())
    }
}

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

    fn attach_remote_completer(
        &mut self,
        user_data: usize,
        completer: Box<dyn RemoteCompleter<Self::Op>>,
    ) {
        if let Some(op) = self.ops.get_mut(user_data) {
            op.platform_data = PlatformData::Remote(completer);
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
                    if self.pool.execute(task).is_err() {
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

        // Check if we need to complete a Remote op synchronously (e.g. if submit failed)
        let should_complete_remote = if let Some(op) = self.ops.get_mut(user_data) {
            op.result.is_some() && matches!(op.platform_data, PlatformData::Remote(_))
        } else {
            false
        };

        if should_complete_remote {
            let mut entry = self.ops.remove(user_data);
            // Extract result
            let result = entry.result.take().unwrap_or(Ok(0));

            // Extract completer
            if let PlatformData::Remote(completer) = entry.platform_data {
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
                    if self.pool.execute(task).is_err() {
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
        if let Some(op) = self.ops.get_mut(user_data) {
            trace!(user_data, "Cancelling op");
            op.cancelled = true;

            match &mut op.platform_data {
                PlatformData::Timer(id) => {
                    self.wheel.cancel(*id);
                }
                PlatformData::Background => {
                    // Should not happen for cancel_op usually, but same logic
                }
                PlatformData::None | PlatformData::Remote(_) => {
                    // Try to CancelIoEx if resources exist
                    if let Some(res) = &mut op.resources
                        && let Some(fd) = res.get_fd()
                        && let Ok(handle) = submit::resolve_fd(fd, &self.registered_files)
                    {
                        // Direct access to header
                        let entry = &mut res.header;
                        unsafe {
                            use windows_sys::Win32::System::IO::CancelIoEx;
                            let _ = CancelIoEx(handle, &entry.inner as *const _ as *mut _);
                        }
                    }
                }
            }
        }

        let should_remove = if let Some(op) = self.ops.get_mut(user_data) {
            match op.platform_data {
                PlatformData::Timer(_) => true,
                _ => {
                    // For IO ops, we can remove if the result is already available
                    op.result.is_some()
                }
            }
        } else {
            false
        };

        if should_remove {
            self.ops.remove(user_data);
        }
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
        let mut registered = Vec::with_capacity(files.len());
        for &handle in files {
            let idx = if let Some(idx) = self.free_slots.pop() {
                self.registered_files[idx] = Some(handle.handle);
                if let Some(rio) = &mut self.rio_state {
                    rio.clear_registered_rq(idx);
                }
                idx
            } else {
                self.registered_files.push(Some(handle.handle));
                if let Some(rio) = &mut self.rio_state {
                    rio.resize_registered_rqs(self.registered_files.len());
                }
                self.registered_files.len() - 1
            };
            registered.push(crate::io::op::IoFd::Fixed(idx as u32));
        }
        Ok(registered)
    }

    fn unregister_files(&mut self, files: Vec<crate::io::op::IoFd>) -> io::Result<()> {
        for fd in files {
            if let crate::io::op::IoFd::Fixed(idx) = fd {
                let idx = idx as usize;
                if idx < self.registered_files.len() && self.registered_files[idx].is_some() {
                    self.registered_files[idx] = None;
                    if let Some(rio) = &mut self.rio_state {
                        rio.clear_registered_rq(idx);
                    }
                    self.free_slots.push(idx);
                }
            }
        }
        Ok(())
    }

    fn wake(&mut self) -> io::Result<()> {
        let res = unsafe {
            PostQueuedCompletionStatus(self.port, 0, WAKEUP_USER_DATA, std::ptr::null_mut())
        };
        trace!("Wake up posted");
        if res == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn inner_handle(&self) -> crate::io::RawHandle {
        self.port.into()
    }

    fn notify_mesh(&mut self, handle: crate::io::RawHandle) -> io::Result<()> {
        let port = handle.handle;
        let res =
            unsafe { PostQueuedCompletionStatus(port, 0, WAKEUP_USER_DATA, std::ptr::null_mut()) };
        if res == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn create_waker(&self) -> std::sync::Arc<dyn RemoteWaker> {
        let process = unsafe { GetCurrentProcess() };
        let mut new_handle = INVALID_HANDLE_VALUE;
        let res = unsafe {
            DuplicateHandle(
                process,
                self.port,
                process,
                &mut new_handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if res == 0 {
            panic!("Failed to dup handle");
        }
        std::sync::Arc::new(IocpWaker(new_handle))
    }

    type RemoteInjector = IocpInjector;

    fn injector(&self) -> std::sync::Arc<Self::RemoteInjector> {
        self.injector.clone()
    }
}

impl IocpDriver {}

impl Drop for IocpDriver {
    fn drop(&mut self) {
        debug!("Dropping IocpDriver");
        let mut pending_count = 0;
        for (_user_data, op) in self.ops.iter_mut() {
            if op.resources.is_some() && op.result.is_none() {
                if !op.cancelled
                    && let Some(res) = op.resources.as_mut()
                    && let Some(fd) = res.get_fd()
                    && let Ok(handle) = submit::resolve_fd(fd, &self.registered_files)
                {
                    // Direct access to header
                    let entry = &mut res.header;
                    unsafe {
                        use windows_sys::Win32::System::IO::CancelIoEx;
                        let _ = CancelIoEx(handle, &entry.inner as *const _ as *mut _);
                    }
                }
                pending_count += 1;
            }
        }

        let mut ops_drained = 0;

        while ops_drained < pending_count {
            let mut bytes = 0;
            let mut key = 0;
            let mut overlapped = std::ptr::null_mut();

            let res = unsafe {
                GetQueuedCompletionStatus(self.port, &mut bytes, &mut key, &mut overlapped, 100)
            };

            if !overlapped.is_null() {
                ops_drained += 1;
            } else if res == 0 {
                let err = unsafe { GetLastError() };
                if err == WAIT_TIMEOUT {
                    continue;
                }
            }
        }

        // Complete any remaining remote ops to avoid panics in their waiters.
        // During shutdown, we must ensure all RemoteOps return their resources.
        for (_user_data, op_entry) in self.ops.iter_mut() {
            if let PlatformData::Remote(_) = op_entry.platform_data {
                let data = std::mem::replace(&mut op_entry.platform_data, PlatformData::None);
                if let PlatformData::Remote(completer) = data {
                    if let Some(op) = op_entry.resources.take() {
                        completer.complete(Err(io::Error::from(io::ErrorKind::Interrupted)), op);
                    }
                }
            }
        }

        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.port) };
    }
}
