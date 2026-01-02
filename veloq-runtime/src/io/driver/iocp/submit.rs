//! IOCP Operation Submission Implementations
//!
//! This module implements the `IocpSubmit` trait for all operation types,
//! providing the logic to submit operations and handle completions.

use crate::io::op::{Connect, IoFd, ReadFixed, Recv, Send as OpSend, WriteFixed};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, WSARecvFrom,
    WSASendTo, bind, getsockname, setsockopt,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, OVERLAPPED};

use super::blocking::{BlockingTask, CompletionInfo};
use super::ext::Extensions;
use super::op::{IocpOp, OverlappedEntry};

// ============================================================================
// Submission Result Types
// ============================================================================

pub enum SubmissionResult {
    /// Operation is pending, will complete via IOCP.
    Pending,
    /// Post to completion queue immediately (e.g., wakeup).
    PostToQueue,
    /// Offload to thread pool for synchronous execution.
    Offload(BlockingTask),
}

// ============================================================================
// IocpSubmit Trait
// ============================================================================

pub(crate) trait IocpSubmit {
    /// Submit the operation to IOCP.
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult>;

    /// Handle completion event.
    fn on_complete(&mut self, result: usize, _ext: &Extensions) -> io::Result<usize> {
        Ok(result)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

pub(crate) fn resolve_fd(fd: IoFd, registered_files: &[Option<HANDLE>]) -> io::Result<HANDLE> {
    match fd {
        IoFd::Raw(h) => Ok(h as HANDLE),
        IoFd::Fixed(idx) => {
            if let Some(Some(h)) = registered_files.get(idx as usize) {
                Ok(*h)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid registered file descriptor",
                ))
            }
        }
    }
}

// ============================================================================
// Macro for File Operations
// ============================================================================

macro_rules! impl_file_op {
    ($Op:ty, $init_fn:expr, $api_fn:expr) => {
        impl IocpSubmit for $Op {
            unsafe fn submit(
                &mut self,
                port: HANDLE,
                overlapped: *mut OVERLAPPED,
                _ext: &Extensions,
                registered_files: &[Option<HANDLE>],
            ) -> io::Result<SubmissionResult> {
                let entry_ext = unsafe { &mut *overlapped };
                $init_fn(&*self, entry_ext);

                let handle = resolve_fd(self.fd, registered_files)?;
                unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

                let mut bytes = 0;
                let ret = $api_fn(&mut *self, handle, &mut bytes, overlapped);

                if ret == 0 {
                    let err = unsafe { GetLastError() };
                    if err != ERROR_IO_PENDING {
                        return Err(io::Error::from_raw_os_error(err as i32));
                    }
                }
                Ok(SubmissionResult::Pending)
            }
        }
    };
}

// ============================================================================
// Direct Operations (Cross-Platform Structs)
// ============================================================================

impl_file_op!(
    ReadFixed,
    |op: &Self, entry: &mut OVERLAPPED| {
        entry.Anonymous.Anonymous.Offset = op.offset as u32;
        entry.Anonymous.Anonymous.OffsetHigh = (op.offset >> 32) as u32;
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        ReadFile(
            handle,
            op.buf.as_mut_ptr() as *mut _,
            op.buf.capacity() as u32,
            bytes,
            overlapped,
        )
    }
);

impl_file_op!(
    WriteFixed,
    |op: &Self, entry: &mut OVERLAPPED| {
        entry.Anonymous.Anonymous.Offset = op.offset as u32;
        entry.Anonymous.Anonymous.OffsetHigh = (op.offset >> 32) as u32;
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        WriteFile(
            handle,
            op.buf.as_slice().as_ptr() as *const _,
            op.buf.len() as u32,
            bytes,
            overlapped,
        )
    }
);

impl_file_op!(
    Recv,
    |_op, entry: &mut OVERLAPPED| {
        entry.Anonymous.Anonymous.Offset = 0;
        entry.Anonymous.Anonymous.OffsetHigh = 0;
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        ReadFile(
            handle,
            op.buf.as_mut_ptr() as *mut _,
            op.buf.capacity() as u32,
            bytes,
            overlapped,
        )
    }
);

impl_file_op!(
    OpSend,
    |_op, entry: &mut OVERLAPPED| {
        entry.Anonymous.Anonymous.Offset = 0;
        entry.Anonymous.Anonymous.OffsetHigh = 0;
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        WriteFile(
            handle,
            op.buf.as_slice().as_ptr() as *const _,
            op.buf.len() as u32,
            bytes,
            overlapped,
        )
    }
);

impl IocpSubmit for Connect {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        let handle = resolve_fd(self.fd, registered_files)?;

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        // ConnectEx requires the socket to be bound.
        // We check if it is already bound using getsockname.
        let mut need_bind = true;
        let mut name: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut namelen = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;

        if unsafe {
            getsockname(
                handle as SOCKET,
                &mut name as *mut _ as *mut SOCKADDR,
                &mut namelen,
            )
        } == 0
        {
            // Check if it's wildcard (port 0)
            let family = name.ss_family;
            if family == AF_INET as u16 {
                let addr_in = unsafe { &*(&name as *const _ as *const SOCKADDR_IN) };
                if addr_in.sin_port != 0 {
                    need_bind = false;
                }
            } else if family == AF_INET6 as u16 {
                let addr_in6 = unsafe { &*(&name as *const _ as *const SOCKADDR_IN6) };
                if addr_in6.sin6_port != 0 {
                    need_bind = false;
                }
            }
        }

        if need_bind {
            let family = self.addr.ss_family; // Fixed: Use available field

            let mut bind_addr: SOCKADDR_IN = unsafe { std::mem::zeroed() };
            bind_addr.sin_family = AF_INET;

            let mut bind_addr6: SOCKADDR_IN6 = unsafe { std::mem::zeroed() };
            bind_addr6.sin6_family = AF_INET6;

            let (ptr, len) = if family == AF_INET as u16 {
                (
                    &bind_addr as *const _ as *const SOCKADDR,
                    std::mem::size_of::<SOCKADDR_IN>() as i32,
                )
            } else {
                (
                    &bind_addr6 as *const _ as *const SOCKADDR,
                    std::mem::size_of::<SOCKADDR_IN6>() as i32,
                )
            };

            let _ = unsafe { bind(handle as SOCKET, ptr, len) };
        }

        let mut bytes_sent = 0;
        let ret = unsafe {
            (ext.connect_ex)(
                handle as SOCKET,
                &self.addr as *const _ as *const SOCKADDR, // Fixed: Cast
                self.addr_len as i32,
                std::ptr::null(), // Send buffer
                0,                // Send data length
                &mut bytes_sent,
                overlapped,
            )
        };

        if ret == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        Ok(SubmissionResult::Pending)
    }

    fn on_complete(&mut self, result: usize, _ext: &Extensions) -> io::Result<usize> {
        let fd = self
            .fd
            .raw()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Invalid socket fd"))?;

        let ret = unsafe {
            setsockopt(
                fd as SOCKET,
                SOL_SOCKET,
                SO_UPDATE_CONNECT_CONTEXT,
                std::ptr::null(),
                0,
            )
        };

        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }
}

// ============================================================================
// IocpOp Enum Dispatch
// ============================================================================

impl IocpSubmit for IocpOp {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        match self {
            IocpOp::ReadFixed(op, _) => unsafe {
                op.submit(port, overlapped, ext, registered_files)
            },
            IocpOp::WriteFixed(op, _) => unsafe {
                op.submit(port, overlapped, ext, registered_files)
            },
            IocpOp::Recv(op, _) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IocpOp::Send(op, _) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IocpOp::Connect(op, _) => unsafe { op.submit(port, overlapped, ext, registered_files) },

            IocpOp::Accept(op, extras) => {
                let handle = resolve_fd(op.fd, registered_files)?;

                // Use the pre-created socket from generic op
                let accept_socket = op.accept_socket as HANDLE;

                unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

                // AcceptEx requires: LocalAddr + 16, RemoteAddr + 16.
                const MIN_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
                let split = MIN_ADDR_LEN;
                let mut bytes_received = 0;

                let ret = unsafe {
                    (ext.accept_ex)(
                        handle as SOCKET,
                        accept_socket as SOCKET,
                        extras.accept_buffer.as_mut_ptr() as *mut _,
                        0,            // dwReceiveDataLength
                        split as u32, // dwLocalAddressLength
                        split as u32, // dwRemoteAddressLength
                        &mut bytes_received,
                        overlapped,
                    )
                };

                if ret == 0 {
                    let err = unsafe { GetLastError() };
                    if err != ERROR_IO_PENDING {
                        return Err(io::Error::from_raw_os_error(err as i32));
                    }
                }
                Ok(SubmissionResult::Pending)
            }

            IocpOp::SendTo(op, extras) => {
                let handle = resolve_fd(op.fd, registered_files)?;
                unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

                // Update WSABUF in extras from Op buf
                extras.wsabuf.len = op.buf.len() as u32;
                extras.wsabuf.buf = op.buf.as_slice().as_ptr() as *mut u8;

                let mut bytes = 0;
                let ret = unsafe {
                    WSASendTo(
                        handle as SOCKET,
                        &extras.wsabuf,
                        1,
                        &mut bytes,
                        0,
                        &extras.addr as *const _ as *const SOCKADDR,
                        extras.addr_len, // i32
                        overlapped as *mut _,
                        None,
                    )
                };
                if ret == SOCKET_ERROR {
                    let err = unsafe { WSAGetLastError() };
                    if err != ERROR_IO_PENDING as i32 {
                        return Err(io::Error::from_raw_os_error(err));
                    }
                }
                Ok(SubmissionResult::Pending)
            }

            IocpOp::RecvFrom(op, extras) => {
                let handle = resolve_fd(op.fd, registered_files)?;
                unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

                extras.wsabuf.len = op.buf.capacity() as u32;
                extras.wsabuf.buf = op.buf.as_mut_ptr();

                let mut bytes = 0;
                let ret = unsafe {
                    WSARecvFrom(
                        handle as SOCKET,
                        &extras.wsabuf,
                        1,
                        &mut bytes,
                        &mut extras.flags,
                        &mut extras.addr as *mut _ as *mut SOCKADDR,
                        &mut extras.addr_len,
                        overlapped as *mut _,
                        None,
                    )
                };
                if ret == SOCKET_ERROR {
                    let err = unsafe { WSAGetLastError() };
                    if err != ERROR_IO_PENDING as i32 {
                        return Err(io::Error::from_raw_os_error(err));
                    }
                }
                Ok(SubmissionResult::Pending)
            }

            IocpOp::Open(op, extras) => {
                let path = extras.path.clone();
                let flags = op.flags;
                let mode = op.mode;

                let entry = unsafe { &*(overlapped as *mut OverlappedEntry) };
                let user_data = entry.user_data;
                let completion = CompletionInfo {
                    port: port as usize,
                    user_data,
                    overlapped: overlapped as usize,
                };
                let task = BlockingTask::Open {
                    path,
                    flags,
                    mode,
                    completion,
                };
                Ok(SubmissionResult::Offload(task))
            }

            IocpOp::Close(op, _entry) => {
                let handle = resolve_fd(op.fd, registered_files)?;
                let handle_val = handle as usize;

                let entry = unsafe { &*(overlapped as *mut OverlappedEntry) };
                let user_data = entry.user_data;
                let completion = CompletionInfo {
                    port: port as usize,
                    user_data,
                    overlapped: overlapped as usize,
                };
                let task = BlockingTask::Close {
                    handle: handle_val,
                    completion,
                };
                Ok(SubmissionResult::Offload(task))
            }

            IocpOp::Fsync(op, _entry) => {
                let handle = resolve_fd(op.fd, registered_files)?;
                let handle_val = handle as usize;

                let entry = unsafe { &*(overlapped as *mut OverlappedEntry) };
                let user_data = entry.user_data;
                let completion = CompletionInfo {
                    port: port as usize,
                    user_data,
                    overlapped: overlapped as usize,
                };
                let task = BlockingTask::Fsync {
                    handle: handle_val,
                    completion,
                };
                Ok(SubmissionResult::Offload(task))
            }

            IocpOp::Wakeup(_, _) => Ok(SubmissionResult::PostToQueue),
            IocpOp::Timeout(_, _) => Ok(SubmissionResult::Pending),
        }
    }

    fn on_complete(&mut self, result: usize, ext: &Extensions) -> io::Result<usize> {
        match self {
            IocpOp::ReadFixed(op, _) => op.on_complete(result, ext),
            IocpOp::WriteFixed(op, _) => op.on_complete(result, ext),
            IocpOp::Recv(op, _) => op.on_complete(result, ext),
            IocpOp::Send(op, _) => op.on_complete(result, ext),
            IocpOp::Connect(op, _) => op.on_complete(result, ext),
            IocpOp::Close(_, _) => Ok(result),
            IocpOp::Fsync(_, _) => Ok(result),
            IocpOp::Wakeup(_, _) => Ok(result),
            IocpOp::Timeout(_, _) => Ok(result),

            IocpOp::Accept(op, extras) => {
                let accept_socket = op.accept_socket as SOCKET;
                let listen_handle = op.fd.raw().ok_or(io::Error::from_raw_os_error(0))? as SOCKET;

                let ret = unsafe {
                    setsockopt(
                        accept_socket,
                        SOL_SOCKET,
                        SO_UPDATE_ACCEPT_CONTEXT,
                        &listen_handle as *const _ as *const _,
                        std::mem::size_of_val(&listen_handle) as i32,
                    )
                };
                if ret != 0 {
                    return Err(io::Error::last_os_error());
                }

                const MIN_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
                let split = MIN_ADDR_LEN;

                let mut local_sockaddr: *mut SOCKADDR = std::ptr::null_mut();
                let mut remote_sockaddr: *mut SOCKADDR = std::ptr::null_mut();
                let mut local_len: i32 = 0;
                let mut remote_len: i32 = 0;

                unsafe {
                    (ext.get_accept_ex_sockaddrs)(
                        extras.accept_buffer.as_ptr() as *const _,
                        0,
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
                            let ip = Ipv4Addr::from(addr_in.sin_addr.S_un.S_addr.to_ne_bytes());
                            let port = u16::from_be(addr_in.sin_port);
                            op.remote_addr = Some(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                        } else if family == AF_INET6 as u16 {
                            let addr_in6 = &*(remote_sockaddr as *const SOCKADDR_IN6);
                            let ip = Ipv6Addr::from(addr_in6.sin6_addr.u.Byte);
                            let port = u16::from_be(addr_in6.sin6_port);
                            let flowinfo = addr_in6.sin6_flowinfo;
                            let scope_id = addr_in6.Anonymous.sin6_scope_id;
                            op.remote_addr = Some(SocketAddr::V6(SocketAddrV6::new(
                                ip, port, flowinfo, scope_id,
                            )));
                        }
                    }
                }
                Ok(result)
            }

            IocpOp::SendTo(_, _) => Ok(result),

            IocpOp::RecvFrom(_, _) => Ok(result),

            IocpOp::Open(_, _) => Ok(result),
        }
    }
}
