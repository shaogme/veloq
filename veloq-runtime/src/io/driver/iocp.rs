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

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_TIMEOUT,
};
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

use crate::io::buffer::BufPool;

pub struct IocpDriver<P: BufPool> {
    port: HANDLE,
    ops: OpRegistry<IocpOp, PlatformData>,
    extensions: Extensions,
    wheel: Wheel<usize>,
    registered_files: Vec<Option<HANDLE>>,
    free_slots: Vec<usize>,
    pool: ThreadPool,
    _marker: std::marker::PhantomData<P>,
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

impl<P: BufPool> IocpDriver<P> {
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

        Ok(Self {
            port,
            ops: OpRegistry::with_capacity(entries as usize),
            extensions,
            wheel: Wheel::new(WheelConfig::default()),
            registered_files: Vec::new(),
            free_slots: Vec::new(),
            pool: ThreadPool::new(16, 128, 1024, Duration::from_secs(30)),
            _marker: std::marker::PhantomData,
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
            if completion_key == WAKEUP_USER_DATA {
                return Ok(());
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

            if op.result.is_none() {
                if let Some(iocp_op) = op.resources.as_mut() {
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
}

impl<P: BufPool> Driver for IocpDriver<P> {
    type Op = IocpOp;
    type Pool = P;

    fn reserve_op(&mut self) -> usize {
        self.ops.insert(OpEntry::new(None, PlatformData::None))
    }

    fn submit(&mut self, user_data: usize, op: Self::Op) {
        if let Some(op_entry) = self.ops.get_mut(user_data) {
            // Important: we must pin the op in resources first, then get pointers
            op_entry.resources = Some(op);
            let op_ref = op_entry.resources.as_mut().unwrap();

            op_ref.header.user_data = user_data;
            let port = self.port;
            let overlapped = &mut op_ref.header.inner as *mut OVERLAPPED;
            let ext = &self.extensions;
            let files = &self.registered_files;

            let result = unsafe { (op_ref.vtable.submit)(op_ref, port, overlapped, ext, files) };

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
                    if let Err(_) = self.pool.execute(task) {
                        op_entry.result = Some(Err(io::Error::new(
                            io::ErrorKind::Other,
                            "Thread pool overloaded",
                        )));
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
    }

    fn submit_background(&mut self, op: Self::Op) -> io::Result<()> {
        let user_data = self.reserve_op();
        if let Some(op_entry) = self.ops.get_mut(user_data) {
            op_entry.platform_data = PlatformData::Background;
            op_entry.resources = Some(op);
            let op_ref = op_entry.resources.as_mut().unwrap();
            op_ref.header.user_data = user_data;

            let port = self.port;
            let overlapped = &mut op_ref.header.inner as *mut OVERLAPPED;
            let ext = &self.extensions;
            let files = &self.registered_files;

            let result = unsafe { (op_ref.vtable.submit)(op_ref, port, overlapped, ext, files) };

            match result {
                Ok(SubmissionResult::Offload(task)) => {
                    if let Err(_) = self.pool.execute(task) {
                        // Failed to submit background task
                        self.ops.remove(user_data);
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            "Thread pool overloaded",
                        ));
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
        if self.ops.is_empty() {
            return Ok(());
        }
        self.get_completion(u32::MAX)
    }

    fn process_completions(&mut self) {
        let _ = self.get_completion(1);
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
                    if let Some(res) = &mut op.resources {
                        if let Some(fd) = res.get_fd() {
                            if let Ok(handle) = submit::resolve_fd(fd, &self.registered_files) {
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

    fn register_buffer_pool(&mut self, _pool: &Self::Pool) -> io::Result<()> {
        Ok(())
    }

    fn register_files(
        &mut self,
        files: &[crate::io::op::RawHandle],
    ) -> io::Result<Vec<crate::io::op::IoFd>> {
        let mut registered = Vec::with_capacity(files.len());
        for &handle in files {
            let idx = if let Some(idx) = self.free_slots.pop() {
                self.registered_files[idx] = Some(handle as HANDLE);
                idx
            } else {
                self.registered_files.push(Some(handle as HANDLE));
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
                if idx < self.registered_files.len() {
                    if self.registered_files[idx].is_some() {
                        self.registered_files[idx] = None;
                        self.free_slots.push(idx);
                    }
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

impl<P: BufPool> Drop for IocpDriver<P> {
    fn drop(&mut self) {
        let mut pending_count = 0;
        for (_user_data, op) in self.ops.iter_mut() {
            if op.resources.is_some() && op.result.is_none() {
                if !op.cancelled {
                    if let Some(res) = op.resources.as_mut() {
                        if let Some(fd) = res.get_fd() {
                            if let Ok(handle) = submit::resolve_fd(fd, &self.registered_files) {
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
