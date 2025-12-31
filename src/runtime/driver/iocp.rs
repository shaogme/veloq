mod ext;
mod ops;
#[cfg(test)]
mod tests;

use crate::runtime::buffer::{BufferPool, FixedBuf};
use crate::runtime::driver::Driver;
use crate::runtime::op::IoResources;
use ext::Extensions;
use slab::Slab;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;
use std::task::{Context, Poll, Waker};
use std::time::Instant;
use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOL_SOCKET, setsockopt,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};

struct TimerEntry {
    deadline: Instant,
    user_data: usize,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

pub struct IocpDriver {
    port: HANDLE,
    ops: Slab<OperationLifecycle>,
    buffer_pool: BufferPool,
    extensions: Extensions,
    timers: BinaryHeap<TimerEntry>,
}

#[repr(C)]
struct OverlappedEntry {
    inner: OVERLAPPED,
    user_data: usize,
}

struct OperationLifecycle {
    waker: Option<Waker>,
    result: Option<io::Result<u32>>,
    resources: IoResources,
    #[allow(dead_code)] // Kept alive by this box
    overlapped: Option<Box<OverlappedEntry>>,
    cancelled: bool,
}

impl IocpDriver {
    pub fn new(entries: u32) -> io::Result<Self> {
        // Create a new completion port.
        // FileHandle = INVALID_HANDLE_VALUE
        // ExistingCompletionPort = NULL
        // CompletionKey = 0 (ignored)
        // NumberOfConcurrentThreads = 0 (default to num processors)
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };

        if port.is_null() {
            return Err(io::Error::last_os_error());
        }

        // Load extensions
        let extensions = Extensions::new()?;

        Ok(Self {
            port,
            ops: Slab::with_capacity(entries as usize),
            buffer_pool: BufferPool::new(),
            extensions,
            timers: BinaryHeap::new(),
        })
    }

    /// Retrieve completion events from the port.
    /// timeout_ms: 0 for poll, u32::MAX for wait.
    fn get_completion(&mut self, timeout_ms: u32) -> io::Result<()> {
        let mut bytes_transferred = 0;
        let mut completion_key = 0;
        let mut overlapped = std::ptr::null_mut();

        let now = Instant::now();
        let mut wait_ms = timeout_ms;
        if let Some(timer) = self.timers.peek() {
            let delay = if timer.deadline > now {
                (timer.deadline - now).as_millis() as u32
            } else {
                0
            };
            wait_ms = std::cmp::min(wait_ms, delay);
        }

        // Single event retrieval for now.
        // In a real high-perf driver, we would use GetQueuedCompletionStatusEx to get multiple.
        let res = unsafe {
            GetQueuedCompletionStatus(
                self.port,
                &mut bytes_transferred,
                &mut completion_key,
                &mut overlapped,
                wait_ms,
            )
        };

        // Process expired timers
        let now = Instant::now();
        while let Some(timer) = self.timers.peek() {
            if timer.deadline > now {
                break;
            }
            let entry = self.timers.pop().unwrap();
            let user_data = entry.user_data;
            if let Some(op) = self.ops.get_mut(user_data) {
                if !op.cancelled && op.result.is_none() {
                    op.result = Some(Ok(0));
                    if let Some(waker) = op.waker.take() {
                        waker.wake();
                    }
                }
            }
        }

        if overlapped.is_null() {
            // If overlapped is null, the failure is related to dequeuing itself (e.g. timeout)
            // or the port itself.
            if res == 0 {
                let err = unsafe { GetLastError() };
                if err == WAIT_TIMEOUT {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            // If res != 0 but overlapped is null, it's a successful dequeue of something custom?
            // Or maybe a "notification" (PostQueuedCompletionStatus).
            // We assume we only use it for IO here.
            return Ok(());
        }

        // We have a valid overlapped pointer, pointing to our OverlappedEntry
        let entry = unsafe { &*(overlapped as *const OverlappedEntry) };
        let user_data = entry.user_data;

        if self.ops.contains(user_data) {
            let op = &mut self.ops[user_data];

            // If we already have a result (e.g. invalid submission), keep it.
            // Otherwise use the IOCP result.
            if op.result.is_none() {
                let result = if res == 0 {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_HANDLE_EOF {
                        Ok(bytes_transferred)
                    } else {
                        Err(io::Error::from_raw_os_error(err as i32))
                    }
                } else {
                    Ok(bytes_transferred)
                };

                // Post-completion handling for Accept/Connect
                if result.is_ok() {
                    match &mut op.resources {
                        IoResources::Accept(accept_op) => {
                            // Update Accept Context
                            let accept_socket = accept_op.accept_socket;
                            let listen_socket = accept_op.fd as SOCKET;
                            // The value passed to setsockopt for SO_UPDATE_ACCEPT_CONTEXT is the listening socket handle
                            let ret = unsafe {
                                setsockopt(
                                    accept_socket as SOCKET,
                                    SOL_SOCKET,
                                    SO_UPDATE_ACCEPT_CONTEXT,
                                    &listen_socket as *const _ as *const _,
                                    std::mem::size_of_val(&listen_socket) as i32,
                                )
                            };
                            if ret != 0 {
                                op.result = Some(Err(io::Error::last_os_error()));
                            } else {
                                // Parse addresses and backfill
                                // AcceptEx requires: LocalAddr + 16, RemoteAddr + 16.
                                // LocalAddr/RemoteAddr max safe size is SOCKADDR_STORAGE (128).
                                // So we need 128 + 16 = 144 bytes per address.
                                const MIN_ADDR_LEN: usize =
                                    std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
                                let split = MIN_ADDR_LEN;

                                let mut local_sockaddr: *mut SOCKADDR = std::ptr::null_mut();
                                let mut remote_sockaddr: *mut SOCKADDR = std::ptr::null_mut();
                                let mut local_len: i32 = 0;
                                let mut remote_len: i32 = 0;

                                unsafe {
                                    (self.extensions.get_accept_ex_sockaddrs)(
                                        accept_op.addr.as_ptr() as *const _,
                                        0, // received data length (we didn't ask for data)
                                        split as u32,
                                        split as u32,
                                        &mut local_sockaddr,
                                        &mut local_len,
                                        &mut remote_sockaddr,
                                        &mut remote_len,
                                    );
                                }

                                if !remote_sockaddr.is_null() && remote_len > 0 {
                                    unsafe {
                                        let family = (*remote_sockaddr).sa_family;
                                        if family == AF_INET as u16 {
                                            let addr_in = &*(remote_sockaddr as *const SOCKADDR_IN);
                                            let ip = std::net::Ipv4Addr::from(
                                                addr_in.sin_addr.S_un.S_addr.to_ne_bytes(),
                                            );
                                            let port = u16::from_be(addr_in.sin_port);
                                            accept_op.remote_addr = Some(std::net::SocketAddr::V4(
                                                std::net::SocketAddrV4::new(ip, port),
                                            ));
                                        } else if family == AF_INET6 as u16 {
                                            let addr_in6 =
                                                &*(remote_sockaddr as *const SOCKADDR_IN6);
                                            let ip =
                                                std::net::Ipv6Addr::from(addr_in6.sin6_addr.u.Byte);
                                            let port = u16::from_be(addr_in6.sin6_port);
                                            let flowinfo = addr_in6.sin6_flowinfo;
                                            let scope_id = addr_in6.Anonymous.sin6_scope_id;
                                            accept_op.remote_addr = Some(std::net::SocketAddr::V6(
                                                std::net::SocketAddrV6::new(
                                                    ip, port, flowinfo, scope_id,
                                                ),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        IoResources::Connect(connect_op) => {
                            // Update Connect Context
                            let ret = unsafe {
                                setsockopt(
                                    connect_op.fd as SOCKET,
                                    SOL_SOCKET,
                                    SO_UPDATE_CONNECT_CONTEXT,
                                    std::ptr::null(),
                                    0,
                                )
                            };
                            if ret != 0 {
                                op.result = Some(Err(io::Error::last_os_error()));
                            }
                        }
                        _ => {}
                    }
                }

                if op.result.is_none() {
                    op.result = Some(result);
                }
            }

            if let Some(waker) = op.waker.take() {
                waker.wake();
            }
        }

        Ok(())
    }

    pub fn alloc_fixed_buffer(&self) -> Option<FixedBuf> {
        self.buffer_pool.alloc()
    }
}

impl Driver for IocpDriver {
    fn reserve_op(&mut self) -> usize {
        self.ops.insert(OperationLifecycle {
            waker: None,
            result: None,
            resources: IoResources::None,
            overlapped: None,
            cancelled: false,
        })
    }

    fn submit_op_resources(&mut self, user_data: usize, mut resources: IoResources) {
        // 1. Prepare stable Overlapped
        let mut entry = Box::new(OverlappedEntry {
            inner: unsafe { std::mem::zeroed() },
            user_data,
        });

        let overlapped_ptr = &mut entry.inner as *mut OVERLAPPED;
        let (res_err, post_completion) = match &mut resources {
            IoResources::ReadFixed(op) => unsafe {
                ops::submit_read(op, self.port, overlapped_ptr)
            },
            IoResources::WriteFixed(op) => unsafe {
                ops::submit_write(op, self.port, overlapped_ptr)
            },
            IoResources::Recv(op) => unsafe { ops::submit_recv(op, self.port, overlapped_ptr) },
            IoResources::Send(op) => unsafe { ops::submit_send(op, self.port, overlapped_ptr) },
            IoResources::Accept(op) => unsafe {
                ops::submit_accept(op, self.port, overlapped_ptr, &self.extensions)
            },
            IoResources::Connect(op) => unsafe {
                ops::submit_connect(op, self.port, overlapped_ptr, &self.extensions)
            },
            IoResources::None => (None, true),
            IoResources::Timeout(op) => {
                let deadline = Instant::now() + op.duration;
                self.timers.push(TimerEntry {
                    deadline,
                    user_data,
                });
                (None, false)
            }
            IoResources::SendTo(op) => unsafe {
                ops::submit_send_to(op, self.port, overlapped_ptr)
            },
            IoResources::RecvFrom(op) => unsafe {
                ops::submit_recv_from(op, self.port, overlapped_ptr)
            },
        };

        if let Some(op) = self.ops.get_mut(user_data) {
            op.resources = resources;
            op.overlapped = Some(entry);

            if let Some(err) = res_err {
                op.result = Some(Err(err));
            }

            if post_completion {
                // Post a completion packet to wake up the loop
                // We use 0 bytes transferred.
                unsafe {
                    PostQueuedCompletionStatus(self.port, 0, 0, overlapped_ptr);
                }
            }
        }
    }

    fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<u32>, IoResources)> {
        if let Some(op) = self.ops.get_mut(user_data) {
            if let Some(res) = op.result.take() {
                let op_lifecycle = self.ops.remove(user_data);
                Poll::Ready((res, op_lifecycle.resources))
            } else {
                op.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        } else {
            Poll::Ready((
                Err(io::Error::new(io::ErrorKind::Other, "Op not found")),
                IoResources::None,
            ))
        }
    }

    fn submit(&mut self) -> io::Result<()> {
        // IOCP submits immediately on syscall
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

            // Extract fd from resources
            let fd = match &op.resources {
                IoResources::ReadFixed(r) => Some(r.fd as HANDLE),
                IoResources::WriteFixed(r) => Some(r.fd as HANDLE),
                IoResources::Recv(r) => Some(r.fd as HANDLE),
                IoResources::Send(r) => Some(r.fd as HANDLE),
                IoResources::Accept(r) => Some(r.fd as HANDLE),
                IoResources::Connect(r) => Some(r.fd as HANDLE),
                IoResources::SendTo(r) => Some(r.fd as HANDLE),
                IoResources::RecvFrom(r) => Some(r.fd as HANDLE),
                _ => None,
            };

            // Call CancelIoEx if we have both fd and overlapped
            if let (Some(fd), Some(overlapped)) = (fd, &op.overlapped) {
                unsafe {
                    use windows_sys::Win32::System::IO::CancelIoEx;
                    let _ = CancelIoEx(fd, &overlapped.inner as *const _ as *mut _);
                    // We ignore the result because:
                    // 1. The operation might have already completed
                    // 2. The handle might be invalid
                    // 3. We've already marked it as cancelled
                }
            }
        }
    }

    fn alloc_fixed_buffer(&self) -> Option<FixedBuf> {
        self.buffer_pool.alloc()
    }
}

impl Drop for IocpDriver {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.port) };
    }
}
