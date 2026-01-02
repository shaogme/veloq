use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_NORMAL, FILE_END_OF_FILE_INFO,
    FILE_FLAG_OVERLAPPED, FileAllocationInfo, FileEndOfFileInfo, FlushFileBuffers,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::PostQueuedCompletionStatus;

#[derive(Debug, Clone, Copy)]
pub struct CompletionInfo {
    pub port: usize,
    pub user_data: usize,
    pub overlapped: usize, // *mut OVERLAPPED
}

pub enum BlockingTask {
    Open {
        path: Vec<u16>,
        flags: i32,
        mode: u32,
        completion: CompletionInfo,
    },
    Close {
        handle: usize,
        completion: CompletionInfo,
    },
    Fsync {
        handle: usize,
        completion: CompletionInfo,
    },
    SyncFileRange {
        handle: usize,
        completion: CompletionInfo,
    },
    Fallocate {
        handle: usize,
        mode: i32,
        offset: u64,
        len: u64,
        completion: CompletionInfo,
    },
}

impl BlockingTask {
    fn run(self) {
        match self {
            BlockingTask::Open {
                path,
                flags,
                mode,
                completion,
            } => {
                let handle = unsafe {
                    CreateFileW(
                        path.as_ptr(),
                        flags as u32,
                        0,
                        std::ptr::null(),
                        mode as u32,
                        FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL,
                        std::ptr::null_mut(),
                    )
                };

                let result = if handle == INVALID_HANDLE_VALUE {
                    let err = unsafe { GetLastError() };
                    Err(std::io::Error::from_raw_os_error(err as i32))
                } else {
                    Ok(handle as usize)
                };
                Self::complete(completion, result);
            }
            BlockingTask::Close { handle, completion } => {
                let ret = unsafe { CloseHandle(handle as HANDLE) };
                let result = if ret == 0 {
                    let err = unsafe { GetLastError() };
                    Err(std::io::Error::from_raw_os_error(err as i32))
                } else {
                    Ok(0)
                };
                Self::complete(completion, result);
            }
            BlockingTask::Fsync { handle, completion } => {
                let ret = unsafe { FlushFileBuffers(handle as HANDLE) };
                let result = if ret == 0 {
                    let err = unsafe { GetLastError() };
                    Err(std::io::Error::from_raw_os_error(err as i32))
                } else {
                    Ok(0)
                };
                Self::complete(completion, result);
            }
            BlockingTask::SyncFileRange { handle, completion } => {
                // Windows doesn't support fine-grained sync_file_range.
                // Fallback to FlushFileBuffers equivalent to Fsync.
                let ret = unsafe { FlushFileBuffers(handle as HANDLE) };
                let result = if ret == 0 {
                    let err = unsafe { GetLastError() };
                    Err(std::io::Error::from_raw_os_error(err as i32))
                } else {
                    Ok(0)
                };
                Self::complete(completion, result);
            }
            BlockingTask::Fallocate {
                handle,
                mode,
                offset,
                len,
                completion,
            } => {
                let result = (|| {
                    // Calculate required size
                    let req_size = offset + len;

                    // 1. Set allocation size (reserve space)
                    // Note: This might truncate if req_size < current allocation,
                    // but standard fallocate usage usually implies growing or ensuring space.
                    // For safety, one might check current size, but simple fallocate usually sets it.
                    let mut alloc_info = FILE_ALLOCATION_INFO {
                        AllocationSize: req_size as i64,
                    };
                    let ret = unsafe {
                        SetFileInformationByHandle(
                            handle as HANDLE,
                            FileAllocationInfo,
                            &mut alloc_info as *mut _ as *mut _,
                            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
                        )
                    };
                    if ret == 0 {
                        return Err(std::io::Error::from_raw_os_error(
                            unsafe { GetLastError() } as i32
                        ));
                    }

                    // 2. If not KEEP_SIZE (mode 0), update file size
                    // Standard fallocate (mode 0) updates file size.
                    if mode == 0 {
                        let mut eof_info = FILE_END_OF_FILE_INFO {
                            EndOfFile: req_size as i64,
                        };
                        let ret = unsafe {
                            SetFileInformationByHandle(
                                handle as HANDLE,
                                FileEndOfFileInfo,
                                &mut eof_info as *mut _ as *mut _,
                                std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
                            )
                        };
                        if ret == 0 {
                            return Err(std::io::Error::from_raw_os_error(
                                unsafe { GetLastError() } as i32,
                            ));
                        }
                    }

                    Ok(0)
                })();
                Self::complete(completion, result);
            }
        }
    }

    fn complete(info: CompletionInfo, result: std::io::Result<usize>) {
        if info.overlapped == 0 {
            return;
        }
        use crate::io::driver::iocp::op::OverlappedEntry;
        use windows_sys::Win32::System::IO::OVERLAPPED;

        // Access via raw pointer to pinned entry is safe because Op is pinned in StableSlab
        let ptr = info.overlapped as *mut OverlappedEntry;
        unsafe {
            (*ptr).blocking_result = Some(result);
            PostQueuedCompletionStatus(
                info.port as HANDLE,
                0,
                info.user_data,
                ptr as *mut OVERLAPPED,
            );
        }
    }
}

#[derive(Debug)]
pub enum ThreadPoolError {
    Overloaded,
}

struct PoolState {
    queue: Mutex<VecDeque<BlockingTask>>,
    cond: Condvar,
    active_workers: AtomicUsize,
    idle_workers: AtomicUsize,
}

pub struct ThreadPool {
    state: Arc<PoolState>,
    core_threads: usize,
    max_threads: usize,
    queue_capacity: usize,
    keep_alive: Duration,
}

struct WorkerGuard {
    state: Arc<PoolState>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.state.active_workers.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ThreadPool {
    pub fn new(
        core_threads: usize,
        max_threads: usize,
        queue_capacity: usize,
        keep_alive: Duration,
    ) -> Self {
        assert!(core_threads <= max_threads);

        let state = Arc::new(PoolState {
            queue: Mutex::new(VecDeque::with_capacity(queue_capacity)),
            cond: Condvar::new(),
            active_workers: AtomicUsize::new(0),
            idle_workers: AtomicUsize::new(0),
        });

        Self {
            state,
            core_threads,
            max_threads,
            queue_capacity,
            keep_alive,
        }
    }

    pub fn execute(&self, task: BlockingTask) -> Result<(), ThreadPoolError> {
        let state = &self.state;
        let mut queue = state.queue.lock().unwrap();

        // 1. Try to wake an idle worker
        if state.idle_workers.load(Ordering::SeqCst) > 0 {
            queue.push_back(task);
            state.cond.notify_one();
            return Ok(());
        }

        // 2. Try to spawn a new worker if under limit
        let active = state.active_workers.load(Ordering::SeqCst);
        if active < self.max_threads {
            state.active_workers.fetch_add(1, Ordering::SeqCst);
            let state_clone = state.clone();
            let keep_alive = self.keep_alive;
            let core_threads = self.core_threads;

            queue.push_back(task);

            let _ = thread::Builder::new()
                .name("veloq-blocking-worker".into())
                .spawn(move || Self::worker_loop(state_clone, keep_alive, core_threads));

            return Ok(());
        }

        // 3. Queue if capable
        if queue.len() < self.queue_capacity {
            queue.push_back(task);
            state.cond.notify_one();
            Ok(())
        } else {
            Err(ThreadPoolError::Overloaded)
        }
    }

    fn worker_loop(state: Arc<PoolState>, keep_alive: Duration, core_threads: usize) {
        let _guard = WorkerGuard {
            state: state.clone(),
        };

        loop {
            let task = {
                let mut queue = state.queue.lock().unwrap();

                while queue.is_empty() {
                    state.idle_workers.fetch_add(1, Ordering::SeqCst);
                    let (guard, result) = state.cond.wait_timeout(queue, keep_alive).unwrap();
                    state.idle_workers.fetch_sub(1, Ordering::SeqCst);
                    queue = guard;

                    if result.timed_out() && queue.is_empty() {
                        if state.active_workers.load(Ordering::SeqCst) > core_threads {
                            return;
                        }
                    }
                }
                queue.pop_front()
            };

            if let Some(task) = task {
                let _ = catch_unwind(AssertUnwindSafe(|| task.run()));
            }
        }
    }
}
