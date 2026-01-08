mod blocking;
mod ext;
pub mod op;
mod submit;
#[cfg(test)]
mod tests;

use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use crate::io::driver::{Driver, RemoteWaker};
use blocking::ThreadPool;
use ext::Extensions;
use op::{IocpOp, OverlappedEntry};
use std::io;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use submit::SubmissionResult;

const WAKEUP_USER_DATA: usize = usize::MAX;
const RIO_EVENT_KEY: usize = usize::MAX - 1;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    RIO_BUFFERID, RIO_CQ, RIO_NOTIFICATION_COMPLETION, RIO_RQ, RIORESULT,
};
const RIO_NOTIFICATION_COMPLETION_TYPE_IOCP: u32 = 1;
const RIO_INVALID_BUFFERID: RIO_BUFFERID = 0;

use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use veloq_wheel::{TaskId, Wheel, WheelConfig};

pub enum PlatformData {
    Timer(TaskId),
    Background,
    None,
}

// Helper to store registration details
#[derive(Debug, Clone, Copy)]
pub struct RioBufferInfo {
    pub(crate) id: RIO_BUFFERID,
}

pub struct IocpDriver {
    pub(crate) port: HANDLE,
    pub(crate) ops: OpRegistry<IocpOp, PlatformData>,
    pub(crate) extensions: Extensions,
    pub(crate) wheel: Wheel<usize>,
    pub(crate) registered_files: Vec<Option<HANDLE>>,
    pub(crate) free_slots: Vec<usize>,
    pub(crate) pool: ThreadPool,

    // RIO Support
    pub(crate) rio_cq: Option<RIO_CQ>,
    pub(crate) registered_bufs: Vec<RioBufferInfo>,
    // RIO Request Queues per socket
    pub(crate) rio_rqs: std::collections::HashMap<HANDLE, RIO_RQ>,
    // RIO Request Queues for registered files (O(1) lookup)
    pub(crate) registered_rio_rqs: Vec<Option<RIO_RQ>>,
    // RIO Registration for Slab Pages (for Address Buffers)
    // Maps PageIndex -> (RIO_BUFFERID, BaseAddress)
    pub(crate) slab_rio_pages: Vec<Option<(RIO_BUFFERID, usize)>>,
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
    pub fn new(config: &crate::config::Config) -> io::Result<Self> {
        // Create a new completion port.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };

        if port.is_null() {
            return Err(io::Error::last_os_error());
        }

        // Load extensions
        let extensions = Extensions::new()?;

        let entries = config.iocp.entries;

        // Initialize RIO CQ if available
        let rio_cq = if let Some(table) = extensions.rio_table.as_ref() {
            let notification = RIO_NOTIFICATION_COMPLETION {
                Type: RIO_NOTIFICATION_COMPLETION_TYPE_IOCP as i32,
                Anonymous: windows_sys::Win32::Networking::WinSock::RIO_NOTIFICATION_COMPLETION_0 {
                    Iocp:
                        windows_sys::Win32::Networking::WinSock::RIO_NOTIFICATION_COMPLETION_0_1 {
                            IocpHandle: port,
                            CompletionKey: RIO_EVENT_KEY as *mut std::ffi::c_void,
                            Overlapped: std::ptr::null_mut(),
                        },
                },
            };

            // Size: use entries count or reasonable default
            let queue_size = entries.max(1024);
            // Function pointer might be Option
            if let Some(create_fn) = table.RIOCreateCompletionQueue {
                let cq = unsafe { create_fn(queue_size, &notification as *const _) };
                if cq == 0 { None } else { Some(cq) }
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self {
            port,
            ops: OpRegistry::with_capacity(entries as usize),
            extensions,
            wheel: Wheel::new(WheelConfig::default()),
            registered_files: Vec::new(),
            free_slots: Vec::new(),
            pool: ThreadPool::new(16, 128, 1024, Duration::from_secs(30)),
            rio_cq,
            registered_bufs: Vec::new(),
            rio_rqs: std::collections::HashMap::new(),
            registered_rio_rqs: Vec::new(),
            slab_rio_pages: Vec::new(),
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
        let user_data = if !overlapped.is_null() {
            let entry = unsafe { &*(overlapped as *const OverlappedEntry) };
            entry.user_data
        } else {
            if res == 0 {
                let err = unsafe { GetLastError() };
                if err == WAIT_TIMEOUT {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            if completion_key == RIO_EVENT_KEY {
                // RIO event is triggered. Process RIO CQ.
                return self.process_rio_completions();
            }
            completion_key
        };

        if self.ops.contains(user_data) {
            // Check if cancelled
            if self.ops[user_data].cancelled {
                self.ops.remove(user_data);
                return Ok(());
            }

            // Check if background
            if let PlatformData::Background = self.ops[user_data].platform_data {
                // Background op finished. Just remove it.
                self.ops.remove(user_data);
                return Ok(());
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
                    let mut result = Ok(bytes_transferred as usize);
                    // Use VTable on_complete if available
                    if let Some(on_comp) = iocp_op.vtable.on_complete {
                        result = unsafe {
                            (on_comp)(iocp_op, bytes_transferred as usize, &self.extensions)
                        };
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

    fn process_rio_completions(&mut self) -> io::Result<()> {
        if let Some(cq) = self.rio_cq {
            let dequeue_fn = self
                .extensions
                .rio_table
                .as_ref()
                .unwrap()
                .RIODequeueCompletion
                .unwrap();

            // Stack buffer for completions
            const MAX_RIO_RESULTS: usize = 128;
            let mut results: [RIORESULT; MAX_RIO_RESULTS] = unsafe { std::mem::zeroed() };

            loop {
                // Dequeue completions
                let count = unsafe { dequeue_fn(cq, results.as_mut_ptr(), MAX_RIO_RESULTS as u32) };

                if count == windows_sys::Win32::Networking::WinSock::RIO_CORRUPT_CQ {
                    return Err(io::Error::from_raw_os_error(
                        windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE as i32,
                    ));
                }

                if count == 0 {
                    break;
                }

                // Process completions
                for i in 0..count as usize {
                    let res = &results[i];
                    // RequestContext stored the user_data (OpRegistry index)
                    let user_data = res.RequestContext as usize;

                    if self.ops.contains(user_data) {
                        let op = &mut self.ops[user_data];
                        if op.cancelled {
                            // Already cancelled, just ignore (or remove if needed, but cancel_op handles removal usually)
                        } else {
                            // Status check
                            let result = if res.Status == 0 {
                                Ok(res.BytesTransferred as usize)
                            } else {
                                Err(io::Error::from_raw_os_error(res.Status as i32))
                            };

                            op.result = Some(result);
                            op.platform_data = PlatformData::None;
                            if let Some(waker) = op.waker.take() {
                                waker.wake();
                            }
                        }
                    }
                }

                // If we filled the buffer, there might be more, continue loop.
                if count < MAX_RIO_RESULTS as u32 {
                    break;
                }
            }

            // Re-arm notification (RIONotify)
            // This is critical for Edge-Triggered-like behavior of RIO IOCP notification
            let notify_fn = self
                .extensions
                .rio_table
                .as_ref()
                .unwrap()
                .RIONotify
                .unwrap();
            let ret = unsafe { notify_fn(cq) };
            if ret != 0 {
                return Err(io::Error::from_raw_os_error(ret as i32));
            }
        }
        Ok(())
    }

    pub fn register_buffers(&mut self, pool: &dyn crate::io::buffer::BufPool) -> io::Result<()> {
        // If RIO is not available, this is a no-op (or maybe we should return error if user expects it?)
        // For now, we assume graceful fallback, so just return Ok if no RIO.
        let register_fn = match &self.extensions.rio_table {
            Some(table) if self.rio_cq.is_some() => table.RIORegisterBuffer,
            _ => return Ok(()),
        };

        if let Some(reg_fn) = register_fn {
            let regions = pool.get_memory_regions();
            self.registered_bufs.clear();
            self.registered_bufs.reserve(regions.len());

            for region in regions {
                let len = region.len;
                // Register buffer with RIO
                let id = unsafe { reg_fn(region.ptr.as_ptr() as *const u8, len as u32) };

                if id == RIO_INVALID_BUFFERID {
                    return Err(io::Error::last_os_error());
                }

                self.registered_bufs.push(RioBufferInfo { id });
            }
        }
        Ok(())
    }
}

impl Driver for IocpDriver {
    type Op = IocpOp;

    fn reserve_op(&mut self) -> usize {
        self.ops.insert(OpEntry::new(None, PlatformData::None))
    }

    fn submit(
        &mut self,
        user_data: usize,
        op: Self::Op,
    ) -> Result<Poll<()>, (io::Error, Self::Op)> {
        // Ensure RIO registration for address buffers if needed
        if self.rio_cq.is_some() {
            let page_idx = user_data >> OpRegistry::<IocpOp, PlatformData>::PAGE_SHIFT;
            if page_idx >= self.slab_rio_pages.len() {
                self.slab_rio_pages.resize(page_idx + 1, None);
            }
            if self.slab_rio_pages[page_idx].is_none() {
                if let Some((ptr, len)) = self.ops.get_page_slice(page_idx) {
                    if let Some(table) = &self.extensions.rio_table
                        && let Some(reg_fn) = table.RIORegisterBuffer
                    {
                        let id = unsafe { reg_fn(ptr, len as u32) };
                        if id != RIO_INVALID_BUFFERID {
                            self.slab_rio_pages[page_idx] = Some((id, ptr as usize));
                        }
                    }
                }
            }
        }

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
                rio_rqs: &mut self.rio_rqs,
                registered_rio_rqs: &mut self.registered_rio_rqs,
                rio_cq: self.rio_cq,
                registered_bufs: &self.registered_bufs,
                slab_rio_pages: &self.slab_rio_pages,
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
        Ok(Poll::Ready(()))
    }

    fn submit_background(&mut self, op: Self::Op) -> io::Result<()> {
        let user_data = self.reserve_op();

        // Ensure RIO registration for address buffers if needed
        if self.rio_cq.is_some() {
            let page_idx = user_data >> OpRegistry::<IocpOp, PlatformData>::PAGE_SHIFT;
            if page_idx >= self.slab_rio_pages.len() {
                self.slab_rio_pages.resize(page_idx + 1, None);
            }
            if self.slab_rio_pages[page_idx].is_none() {
                if let Some((ptr, len)) = self.ops.get_page_slice(page_idx) {
                    if let Some(table) = &self.extensions.rio_table
                        && let Some(reg_fn) = table.RIORegisterBuffer
                    {
                        let id = unsafe { reg_fn(ptr, len as u32) };
                        if id != RIO_INVALID_BUFFERID {
                            self.slab_rio_pages[page_idx] = Some((id, ptr as usize));
                        }
                    }
                }
            }
        }

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
                rio_rqs: &mut self.rio_rqs,
                registered_rio_rqs: &mut self.registered_rio_rqs,
                rio_cq: self.rio_cq,
                registered_bufs: &self.registered_bufs,
                slab_rio_pages: &self.slab_rio_pages,
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
            op.cancelled = true;

            match &mut op.platform_data {
                PlatformData::Timer(id) => {
                    self.wheel.cancel(*id);
                }
                PlatformData::Background => {
                    // Should not happen for cancel_op usually, but same logic
                }
                PlatformData::None => {
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

    fn register_buffers(&mut self, pool: &dyn crate::io::buffer::BufPool) -> io::Result<()> {
        self.register_buffers(pool)
    }

    fn register_files(
        &mut self,
        files: &[crate::io::op::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>> {
        let mut registered = Vec::with_capacity(files.len());
        for &handle in files {
            let idx = if let Some(idx) = self.free_slots.pop() {
                self.registered_files[idx] = Some(handle as HANDLE);
                // Ensure corresponding RIO RQ slot is cleared (initially None)
                if idx < self.registered_rio_rqs.len() {
                    self.registered_rio_rqs[idx] = None;
                }
                idx
            } else {
                self.registered_files.push(Some(handle as HANDLE));
                self.registered_rio_rqs.push(None);
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
                    self.registered_files[idx] = None;
                    if idx < self.registered_rio_rqs.len() {
                        self.registered_rio_rqs[idx] = None;
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
        if res == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn inner_handle(&self) -> crate::io::op::RawHandle {
        self.port
    }

    fn notify_mesh(&mut self, handle: crate::io::op::RawHandle) -> io::Result<()> {
        let port = handle as HANDLE;
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
}

impl IocpDriver {}

impl Drop for IocpDriver {
    fn drop(&mut self) {
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

        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.port) };
    }
}
