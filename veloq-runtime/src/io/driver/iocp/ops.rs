use crate::io::op::{
    Accept, Connect, IoFd, IoResources, ReadFixed, Recv, RecvFrom, Send, SendTo, WriteFixed,
};
use std::io;
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR,
    WSAGetLastError, WSARecvFrom, WSASendTo, bind, getsockname,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, OVERLAPPED};

use super::ext::Extensions;

pub(crate) trait IocpSubmit {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool);
}

fn resolve_fd(fd: IoFd, registered_files: &[Option<HANDLE>]) -> io::Result<HANDLE> {
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

impl IocpSubmit for ReadFixed {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let entry_ext = unsafe { &mut *overlapped };
        entry_ext.Anonymous.Anonymous.Offset = self.offset as u32;
        entry_ext.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;

        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        let mut bytes_read = 0;
        let ret = unsafe {
            ReadFile(
                handle,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.capacity() as u32,
                &mut bytes_read,
                overlapped,
            )
        };

        if ret == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for WriteFixed {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let entry_ext = unsafe { &mut *overlapped };
        entry_ext.Anonymous.Anonymous.Offset = self.offset as u32;
        entry_ext.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;

        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        let mut bytes_written = 0;
        let ret = unsafe {
            WriteFile(
                handle,
                self.buf.as_slice().as_ptr() as *const _,
                self.buf.len() as u32,
                &mut bytes_written,
                overlapped,
            )
        };

        if ret == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for Recv {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let entry_ext = unsafe { &mut *overlapped };
        entry_ext.Anonymous.Anonymous.Offset = 0;
        entry_ext.Anonymous.Anonymous.OffsetHigh = 0;

        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        let mut bytes_read = 0;
        let ret = unsafe {
            ReadFile(
                handle,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.capacity() as u32,
                &mut bytes_read,
                overlapped,
            )
        };

        if ret == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for Send {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let entry_ext = unsafe { &mut *overlapped };
        entry_ext.Anonymous.Anonymous.Offset = 0;
        entry_ext.Anonymous.Anonymous.OffsetHigh = 0;

        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        let mut bytes_written = 0;
        let ret = unsafe {
            WriteFile(
                handle,
                self.buf.as_slice().as_ptr() as *const _,
                self.buf.len() as u32,
                &mut bytes_written,
                overlapped,
            )
        };

        if ret == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for Accept {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let accept_socket = self.accept_socket;

        // AcceptEx requires: LocalAddr + 16, RemoteAddr + 16.
        const MIN_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
        let buf_len = self.addr.len();
        if buf_len < 2 * MIN_ADDR_LEN {
            return (
                Some(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Accept buffer too small for AcceptEx",
                )),
                true,
            );
        }

        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

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
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for Connect {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };

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
                return (Some(io::Error::from_raw_os_error(err as i32)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for SendTo {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };
        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        self.wsabuf.len = self.buf.len() as u32;
        self.wsabuf.buf = self.buf.as_slice().as_ptr() as *mut u8;

        let mut bytes_sent = 0u32;
        let ret = unsafe {
            WSASendTo(
                handle as SOCKET,
                self.wsabuf.as_ref(),
                1,
                &mut bytes_sent,
                0,
                self.addr.as_ptr() as *const SOCKADDR,
                self.addr_len as i32,
                overlapped as *mut _,
                None,
            )
        };

        if ret == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            if err != ERROR_IO_PENDING as i32 {
                return (Some(io::Error::from_raw_os_error(err)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for RecvFrom {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        _ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        let handle = match resolve_fd(self.fd, registered_files) {
            Ok(h) => h,
            Err(e) => return (Some(e), true),
        };
        unsafe { CreateIoCompletionPort(handle, port, 0, 0) };

        self.wsabuf.len = self.buf.capacity() as u32;
        self.wsabuf.buf = self.buf.as_mut_ptr();

        let mut bytes_received = 0u32;
        let ret = unsafe {
            WSARecvFrom(
                handle as SOCKET,
                self.wsabuf.as_ref(),
                1,
                &mut bytes_received,
                self.flags.as_mut(),
                self.addr.as_mut_ptr() as *mut SOCKADDR,
                self.addr_len.as_mut(),
                overlapped as *mut _,
                None,
            )
        };

        if ret == SOCKET_ERROR {
            let err = unsafe { WSAGetLastError() };
            if err != ERROR_IO_PENDING as i32 {
                return (Some(io::Error::from_raw_os_error(err)), true);
            }
        }
        (None, false)
    }
}

impl IocpSubmit for IoResources {
    unsafe fn submit(
        &mut self,
        port: HANDLE,
        overlapped: *mut OVERLAPPED,
        ext: &Extensions,
        registered_files: &[Option<HANDLE>],
    ) -> (Option<io::Error>, bool) {
        match self {
            IoResources::ReadFixed(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::WriteFixed(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::Recv(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::Send(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::Accept(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::Connect(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::SendTo(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::RecvFrom(op) => unsafe { op.submit(port, overlapped, ext, registered_files) },
            IoResources::None => (None, true), 
            IoResources::Timeout(_) => (None, false),
        }
    }
}
