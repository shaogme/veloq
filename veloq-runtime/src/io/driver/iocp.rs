mod ext;
mod ops;
#[cfg(test)]
mod tests;

// use crate::io::buffer::{BufferPool, FixedBuf};
use super::blocking::{ThreadPool, ThreadPoolError};
use crate::io::driver::op_registry::{OpEntry, OpRegistry};
use crate::io::driver::{Driver, RemoteWaker};
use crate::io::op::IoResources;
use ext::Extensions;
use ops::{IocpSubmit, SubmissionResult};
use std::io;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

const WAKEUP_USER_DATA: usize = usize::MAX;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_HANDLE_EOF, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use veloq_wheel::{TaskId, Wheel, WheelConfig};

pub enum PlatformData {
    Overlapped(Box<OverlappedEntry>),
    Timer(TaskId),
    None,
}

pub struct IocpDriver {
    port: HANDLE,
    ops: OpRegistry<PlatformData>,
    extensions: Extensions,
    wheel: Wheel<usize>,
    registered_files: Vec<Option<HANDLE>>,
    free_slots: Vec<usize>,
    pool: ThreadPool,
}

#[repr(C)]
pub struct OverlappedEntry {
    inner: OVERLAPPED,
    user_data: usize,
    blocking_result: Option<io::Result<usize>>,
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

        // Use a default capacity if not specified in config (since I need to add it)
        // For now, I'll assume we update Config to have entries or use a default constant/config val.
        // Actually, the previous code used `entries` passed in.
        // I will add `entries` to IocpConfig.
        let entries = config.iocp.entries;

        Ok(Self {
            port,
            ops: OpRegistry::with_capacity(entries as usize),
            extensions,
            wheel: Wheel::new(WheelConfig::default()),
            registered_files: Vec::new(),
            free_slots: Vec::new(),
            pool: ThreadPool::new(16, 128, 1024, Duration::from_secs(30)),
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

            let op = &mut self.ops[user_data];
            let mut result_is_ready = false;

            if op.result.is_none() {
                match &mut op.platform_data {
                    PlatformData::Overlapped(entry) => {
                        // Check if this was a blocking task offloaded to thread pool
                        if let Some(res) = entry.blocking_result.take() {
                            op.result = Some(res);
                            result_is_ready = true;
                        } else {
                            // Standard IOCP completion
                            let result = if res == 0 {
                                let err = unsafe { GetLastError() };
                                if err == ERROR_HANDLE_EOF {
                                    Ok(bytes_transferred as usize)
                                } else {
                                    Err(io::Error::from_raw_os_error(err as i32))
                                }
                            } else {
                                Ok(bytes_transferred as usize)
                            };

                            if result.is_ok() {
                                // Apply post-processing (Accept/Connect fixups)
                                let bytes = result.unwrap();
                                let final_res = op.resources.on_complete(bytes, &self.extensions);
                                op.result = Some(final_res);
                            } else {
                                op.result = Some(result);
                            }
                            result_is_ready = true;
                        }
                    }
                    _ => {}
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

impl Driver for IocpDriver {
    fn reserve_op(&mut self) -> usize {
        self.ops
            .insert(OpEntry::new(IoResources::None, PlatformData::None))
    }

    fn submit_op_resources(&mut self, user_data: usize, mut resources: IoResources) {
        if let IoResources::Timeout(op) = &resources {
            // Handle timeout
            let id = self.wheel.insert(user_data, op.duration);
            if let Some(op_entry) = self.ops.get_mut(user_data) {
                op_entry.resources = resources;
                op_entry.platform_data = PlatformData::Timer(id);
            }
            return;
        }

        // 1. Prepare stable Overlapped
        let mut entry = Box::new(OverlappedEntry {
            inner: unsafe { std::mem::zeroed() },
            user_data,
            blocking_result: None,
        });

        let overlapped_ptr = &mut entry.inner as *mut OVERLAPPED;

        // Pass registered_files to submit
        let submission_result = unsafe {
            resources.submit(
                self.port,
                overlapped_ptr,
                &self.extensions,
                &self.registered_files,
            )
        };

        if let Some(op) = self.ops.get_mut(user_data) {
            match submission_result {
                Ok(SubmissionResult::Pending) => {
                    op.resources = resources;
                    op.platform_data = PlatformData::Overlapped(entry);
                    // Start async operation, wait for completion on port
                }

                Ok(SubmissionResult::PostToQueue) => {
                    op.resources = resources;
                    op.platform_data = PlatformData::Overlapped(entry);
                    unsafe {
                        PostQueuedCompletionStatus(self.port, 0, 0, overlapped_ptr);
                    }
                }
                Ok(SubmissionResult::Offload(task_fn)) => {
                    op.resources = resources;
                    // Keep the entry alive in platform_data
                    // The raw pointer is safe to use in the thread because we keep the Box alive here
                    op.platform_data = PlatformData::Overlapped(entry);

                    // We need a raw pointer that is valid. Box inside PlatformData::Overlapped owns it.
                    // Accessing it via raw pointer in another thread is unsafe.
                    // We must ensure 'op' does not drop 'entry' while thread is running.
                    // This is guaranteed because cancellation is deferred until completion event.

                    // However, we can't easily get the raw pointer equivalent to 'entry' because we moved it into the enum.
                    // But we already have 'overlapped_ptr' from before the move!
                    // And cast it back to OverlappedEntry for the task to write result.

                    let entry_ptr_val = overlapped_ptr as usize;
                    let port_val = self.port as usize;

                    match self.pool.execute(move || {
                        // Catch panic inside the task to ensure we always report completion
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task_fn()))
                                .unwrap_or_else(|_| {
                                    Err(io::Error::new(
                                        io::ErrorKind::Other,
                                        "blocking task panicked",
                                    ))
                                });

                        let ptr = entry_ptr_val as *mut OverlappedEntry;
                        unsafe {
                            (*ptr).blocking_result = Some(result);
                            PostQueuedCompletionStatus(
                                port_val as HANDLE,
                                0,
                                user_data,
                                ptr as *mut OVERLAPPED,
                            );
                        }
                    }) {
                        Ok(_) => {}
                        Err(ThreadPoolError::Overloaded) => {
                            // Fail the operation immediately
                            op.result = Some(Err(io::Error::new(
                                io::ErrorKind::Other,
                                "blocking pool overloaded",
                            )));
                            // We don't need to clean up platform_data here, user will see result and drop op,
                            // triggering cancel_op which handles cleanup.
                        }
                    }
                }
                Err(e) => {
                    op.resources = resources;
                    op.result = Some(Err(e));
                }
            }
        }
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, IoResources)> {
        self.ops.poll_op(user_data, cx)
    }

    fn submit(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> io::Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }
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
                    // Remove immediately as no kernel resource is held
                    // Actually, modifying self.ops while holding RefMut from get_mut is unsafe/impossible?
                    // But here we obtained `op` via `get_mut`. To remove, we need access to `ops`.
                    // We must release `op` reference before removing.
                }
                PlatformData::Overlapped(overlapped) => {
                    // Check for handles and CancelIoEx
                    if let Some(fd) = op.resources.get_fd() {
                        if let Ok(handle) = ops::resolve_fd(fd, &self.registered_files) {
                            unsafe {
                                use windows_sys::Win32::System::IO::CancelIoEx;
                                let _ = CancelIoEx(handle, &overlapped.inner as *const _ as *mut _);
                            }
                        }
                    }
                }
                PlatformData::None => {}
            }
        }

        // Post-processing for removal
        let should_remove = if let Some(op) = self.ops.get_mut(user_data) {
            matches!(
                op.platform_data,
                PlatformData::Timer(_) | PlatformData::None
            )
        } else {
            false
        };

        if should_remove {
            self.ops.remove(user_data);
        }
    }

    fn register_buffer_pool(&mut self, _pool: &crate::io::buffer::BufferPool) -> io::Result<()> {
        // No-op for Windows currently
        Ok(())
    }

    fn register_files(
        &mut self,
        files: &[crate::io::op::SysRawOp],
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

    fn submit_background(&mut self, op: IoResources) -> io::Result<()> {
        match op {
            IoResources::Close(close) => {
                if let Some(fd) = close.fd.raw() {
                    // Offload CloseHandle to thread pool
                    // We must own the fd. SysRawOp is Copy (u32/size_t or similar handle), so we copy it.
                    let fd_val = fd as usize;

                    self.pool
                        .execute(move || unsafe {
                            windows_sys::Win32::Foundation::CloseHandle(fd_val as HANDLE);
                        })
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::Other, "blocking pool overloaded")
                        })?;

                    Ok(())
                } else {
                    Ok(())
                }
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported background op",
            )),
        }
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

impl Drop for IocpDriver {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.port) };
    }
}
