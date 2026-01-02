use crate::io::op::{
    Accept, Connect, IoFd, IoResources, ReadFixed, Recv, RecvFrom, Send as OpSend, SendTo,
    WriteFixed,
};
use std::io;
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, WSARecvFrom,
    WSASendTo, bind, getsockname, setsockopt,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, OVERLAPPED};

use super::ext::Extensions;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

pub enum SubmissionResult {
    Pending,
    PostToQueue,
    Offload(Box<dyn FnOnce() -> io::Result<usize> + Send>),
}

pub(crate) trait IocpSubmit {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult>;

    fn on_complete(&mut self, result: usize, _ext: &Extensions) -> io::Result<usize> {
        Ok(result)
    }
}

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

macro_rules! impl_wsa_op {
    ($Op:ty, $prep_buf:expr, $api_fn:expr) => {
        impl IocpSubmit for $Op {
            unsafe fn submit(
                &mut self,
                port: HANDLE,
                overlapped: *mut OVERLAPPED,
                _ext: &Extensions,
                registered_files: &[Option<HANDLE>],
            ) -> io::Result<SubmissionResult> {
                let handle = resolve_fd(self.fd, registered_files)?;
                unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

                $prep_buf(&mut *self);

                let mut bytes = 0;
                let ret = $api_fn(&mut *self, handle, &mut bytes, overlapped);

                if ret == SOCKET_ERROR {
                    let err = unsafe { WSAGetLastError() };
                    if err != ERROR_IO_PENDING as i32 {
                        return Err(io::Error::from_raw_os_error(err));
                    }
                }
                Ok(SubmissionResult::Pending)
            }
        }
    };
}

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

impl IocpSubmit for Accept {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        let accept_socket = self.accept_socket;

        // AcceptEx requires: LocalAddr + 16, RemoteAddr + 16.
        const MIN_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
        let buf_len = self.addr.len();
        if buf_len < 2 * MIN_ADDR_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Accept buffer too small for AcceptEx",
            ));
        }

        let handle = resolve_fd(self.fd, registered_files)?;

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };
        unsafe { CreateIoCompletionPort(accept_socket as HANDLE, port, 0, 0) };

        let split = MIN_ADDR_LEN;
        let mut bytes_received = 0;

        let ret = unsafe {
            (ext.accept_ex)(
                handle as SOCKET,
                accept_socket as SOCKET,
                self.addr.as_mut_ptr() as *mut _,
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

    fn on_complete(&mut self, result: usize, ext: &Extensions) -> io::Result<usize> {
        let accept_socket = self.accept_socket;

        let listen_handle = self
            .fd
            .raw()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Invalid listen socket fd"))?;
        let listen_socket = listen_handle as usize as SOCKET;

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
                self.addr.as_ptr() as *const _,
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
                    self.remote_addr = Some(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                } else if family == AF_INET6 as u16 {
                    let addr_in6 = &*(remote_sockaddr as *const SOCKADDR_IN6);
                    let ip = Ipv6Addr::from(addr_in6.sin6_addr.u.Byte);
                    let port = u16::from_be(addr_in6.sin6_port);
                    let flowinfo = addr_in6.sin6_flowinfo;
                    let scope_id = addr_in6.Anonymous.sin6_scope_id;
                    self.remote_addr = Some(SocketAddr::V6(SocketAddrV6::new(
                        ip, port, flowinfo, scope_id,
                    )));
                }
            }
        }

        Ok(result)
    }
}

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
            let family = if self.addr.len() >= 2 {
                u16::from_ne_bytes([self.addr[0], self.addr[1]])
            } else {
                AF_INET as u16
            };

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
                self.addr.as_ptr() as *const SOCKADDR,
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
                fd as usize as SOCKET,
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

impl_wsa_op!(
    SendTo,
    |op: &mut Self| {
        op.wsabuf.len = op.buf.len() as u32;
        op.wsabuf.buf = op.buf.as_slice().as_ptr() as *mut u8;
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        WSASendTo(
            handle as SOCKET,
            op.wsabuf.as_ref(),
            1,
            bytes,
            0,
            op.addr.as_ptr() as *const SOCKADDR,
            op.addr_len as i32,
            overlapped as *mut _,
            None,
        )
    }
);

impl_wsa_op!(
    RecvFrom,
    |op: &mut Self| {
        op.wsabuf.len = op.buf.capacity() as u32;
        op.wsabuf.buf = op.buf.as_mut_ptr();
    },
    |op: &mut Self, handle, bytes, overlapped| unsafe {
        WSARecvFrom(
            handle as SOCKET,
            op.wsabuf.as_ref(),
            1,
            bytes,
            op.flags.as_mut(),
            op.addr.as_mut_ptr() as *mut SOCKADDR,
            op.addr_len.as_mut(),
            overlapped as *mut _,
            None,
        )
    }
);

macro_rules! impl_iocp_submit {
    (
        WithFd { $( $(#[$meta_fd:meta])* $VariantFd:ident($InnerFd:ty) ),* $(,)? },
        WithoutFd { $( $(#[$meta_no_fd:meta])* $VariantNoFd:ident($InnerNoFd:ty) ),* $(,)? }
    ) => {
        impl IocpSubmit for IoResources {
            unsafe fn submit(
                &mut self,
                port: HANDLE,
                overlapped: *mut OVERLAPPED,
                ext: &Extensions,
                registered_files: &[Option<HANDLE>],
            ) -> io::Result<SubmissionResult> {
                match self {
                    $( $(#[$meta_fd])* IoResources::$VariantFd(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },)*
                    $( $(#[$meta_no_fd])* IoResources::$VariantNoFd(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },)*
                    IoResources::None => Ok(SubmissionResult::PostToQueue),
                }
            }

            fn on_complete(&mut self, result: usize, ext: &Extensions) -> io::Result<usize> {
                match self {
                    $( $(#[$meta_fd])* IoResources::$VariantFd(op) => op.on_complete(result, ext),)*
                    $( $(#[$meta_no_fd])* IoResources::$VariantNoFd(op) => op.on_complete(result, ext),)*
                    IoResources::None => Ok(result),
                }
            }
        }
    }
}

// We need to implement IocpSubmit for Wakeup and Timeout since they are now part of the generic loop.
impl IocpSubmit for crate::io::op::Wakeup {
    unsafe fn submit(
        &mut self,
        _port: HANDLE,
        _overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        _registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        Ok(SubmissionResult::PostToQueue)
    }
}

impl IocpSubmit for crate::io::op::Timeout {
    unsafe fn submit(
        &mut self,
        _port: HANDLE,
        _overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        _registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        Ok(SubmissionResult::Pending)
    }
}

veloq_macros::for_all_io_ops!(impl_iocp_submit);

impl IocpSubmit for crate::io::op::Open {
    unsafe fn submit(
        &mut self,
        _port: HANDLE,
        _overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        _registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        let path = self.path.clone();
        let flags = self.flags;
        let mode = self.mode;

        Ok(SubmissionResult::Offload(Box::new(move || {
            // Synchronous open for IOCP MVP
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::Storage::FileSystem::{
                CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
            };

            // self.path is Vec<u16> on Windows
            let path_ptr = path.as_ptr();

            let handle = unsafe {
                CreateFileW(
                    path_ptr,
                    flags as u32, // Simplified mapping
                    0,            // share mode (0 = no share) - maybe should be configurable
                    std::ptr::null(),
                    mode as u32, // creation disposition
                    FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };

            if handle == INVALID_HANDLE_VALUE {
                let err = unsafe { GetLastError() };
                return Err(io::Error::from_raw_os_error(err as i32));
            }

            Ok(handle as usize)
        })))
    }
}

impl IocpSubmit for crate::io::op::Close {
    unsafe fn submit(
        &mut self,
        _port: HANDLE,
        _overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        use windows_sys::Win32::Foundation::CloseHandle;
        let handle = resolve_fd(self.fd, registered_files)?;
        let handle = handle as usize;

        Ok(SubmissionResult::Offload(Box::new(move || {
            let ret = unsafe { CloseHandle(handle as HANDLE) };
            if ret == 0 {
                let err = unsafe { GetLastError() };
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            Ok(0)
        })))
    }
}

impl IocpSubmit for crate::io::op::Fsync {
    unsafe fn submit(
        &mut self,
        _port: HANDLE,
        _overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> io::Result<SubmissionResult> {
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
        let handle = resolve_fd(self.fd, registered_files)?;
        let handle = handle as usize;

        Ok(SubmissionResult::Offload(Box::new(move || {
            let ret = unsafe { FlushFileBuffers(handle as HANDLE) };
            if ret == 0 {
                let err = unsafe { GetLastError() };
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            Ok(0)
        })))
    }
}
