use crate::runtime::op::{Accept, Connect, ReadFixed, Recv, RecvFrom, Send, SendTo, WriteFixed};
use std::io;
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR,
    WSAGetLastError, WSARecvFrom, WSASendTo, bind, getsockname,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, OVERLAPPED};

use super::ext::Extensions;

pub(crate) unsafe fn submit_read(
    op: &mut ReadFixed,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    let entry_ext = unsafe { &mut *overlapped }; // In reality this is OverlappedEntry.inner, but we treat it as OVERLAPPED
    entry_ext.Anonymous.Anonymous.Offset = op.offset as u32;
    entry_ext.Anonymous.Anonymous.OffsetHigh = (op.offset >> 32) as u32;

    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    let mut bytes_read = 0;
    let ret = unsafe {
        ReadFile(
            op.fd as HANDLE,
            op.buf.as_mut_ptr() as *mut _,
            op.buf.capacity() as u32,
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

pub(crate) unsafe fn submit_write(
    op: &mut WriteFixed,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    let entry_ext = unsafe { &mut *overlapped };
    entry_ext.Anonymous.Anonymous.Offset = op.offset as u32;
    entry_ext.Anonymous.Anonymous.OffsetHigh = (op.offset >> 32) as u32;

    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    let mut bytes_written = 0;
    let ret = unsafe {
        WriteFile(
            op.fd as HANDLE,
            op.buf.as_slice().as_ptr() as *const _,
            op.buf.len() as u32,
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

pub(crate) unsafe fn submit_recv(
    op: &mut Recv,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    let entry_ext = unsafe { &mut *overlapped };
    entry_ext.Anonymous.Anonymous.Offset = 0;
    entry_ext.Anonymous.Anonymous.OffsetHigh = 0;

    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    let mut bytes_read = 0;
    let ret = unsafe {
        ReadFile(
            op.fd as HANDLE,
            op.buf.as_mut_ptr() as *mut _,
            op.buf.capacity() as u32,
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

pub(crate) unsafe fn submit_send(
    op: &mut Send,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    let entry_ext = unsafe { &mut *overlapped };
    entry_ext.Anonymous.Anonymous.Offset = 0;
    entry_ext.Anonymous.Anonymous.OffsetHigh = 0;

    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    let mut bytes_written = 0;
    let ret = unsafe {
        WriteFile(
            op.fd as HANDLE,
            op.buf.as_slice().as_ptr() as *const _,
            op.buf.len() as u32,
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

pub(crate) unsafe fn submit_accept(
    op: &mut Accept,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
    ext: &Extensions,
) -> (Option<io::Error>, bool) {
    let accept_socket = op.accept_socket;

    // AcceptEx requires: LocalAddr + 16, RemoteAddr + 16.
    const MIN_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
    let buf_len = op.addr.len();
    if buf_len < 2 * MIN_ADDR_LEN {
        return (
            Some(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Accept buffer too small for AcceptEx",
            )),
            true,
        );
    }

    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };
    unsafe { CreateIoCompletionPort(accept_socket as HANDLE, port, 0, 0) };

    let split = MIN_ADDR_LEN;
    let mut bytes_received = 0;

    let ret = unsafe {
        (ext.accept_ex)(
            op.fd as SOCKET,
            accept_socket as SOCKET,
            op.addr.as_mut_ptr() as *mut _,
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

pub(crate) unsafe fn submit_connect(
    op: &mut Connect,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
    ext: &Extensions,
) -> (Option<io::Error>, bool) {
    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    // ConnectEx requires the socket to be bound.
    // We check if it is already bound using getsockname.
    let mut need_bind = true;
    let mut name: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
    let mut namelen = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;

    if unsafe {
        getsockname(
            op.fd as SOCKET,
            &mut name as *mut _ as *mut SOCKADDR,
            &mut namelen,
        )
    } == 0
    {
        // Check if it's wildcard (port 0)
        let family = name.ss_family;
        if family == AF_INET as u16 {
            let addr_in = unsafe { &*(&name as *const _ as *const SOCKADDR_IN) };
            // If port is non-zero, it is bound to a specific port
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
        // We bind to 0.0.0.0:0 or [::]:0 depending on family.
        // Use safe check for address family
        let family = if op.addr.len() >= 2 {
            u16::from_ne_bytes([op.addr[0], op.addr[1]])
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

        let _ = unsafe { bind(op.fd as SOCKET, ptr, len) };
    }

    let mut bytes_sent = 0;
    let ret = unsafe {
        (ext.connect_ex)(
            op.fd as SOCKET,
            op.addr.as_ptr() as *const SOCKADDR,
            op.addr_len as i32,
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

/// Submit an asynchronous UDP send operation using WSASendTo.
///
/// # Safety
/// The caller must ensure that:
/// - `op` remains valid for the duration of the async operation
/// - `overlapped` points to a valid OVERLAPPED structure
/// - The socket is associated with the completion port
pub(crate) unsafe fn submit_send_to(
    op: &mut SendTo,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    // Associate socket with IOCP
    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    // Update WSABUF to point to current buffer data
    // This is safe because both wsabuf and buf are owned by op
    op.wsabuf.len = op.buf.len() as u32;
    op.wsabuf.buf = op.buf.as_slice().as_ptr() as *mut u8;

    let mut bytes_sent = 0u32;
    let ret = unsafe {
        WSASendTo(
            op.fd as SOCKET,
            op.wsabuf.as_ref(),
            1, // dwBufferCount
            &mut bytes_sent,
            0, // dwFlags
            op.addr.as_ptr() as *const SOCKADDR,
            op.addr_len as i32,
            overlapped as *mut _,
            None, // lpCompletionRoutine (not used with IOCP)
        )
    };

    if ret == SOCKET_ERROR {
        let err = unsafe { WSAGetLastError() };
        // WSA_IO_PENDING = 997 = ERROR_IO_PENDING
        if err != ERROR_IO_PENDING as i32 {
            return (Some(io::Error::from_raw_os_error(err)), true);
        }
    }
    (None, false)
}

/// Submit an asynchronous UDP receive operation using WSARecvFrom.
///
/// # Safety
/// The caller must ensure that:
/// - `op` remains valid for the duration of the async operation
/// - `overlapped` points to a valid OVERLAPPED structure
/// - The socket is associated with the completion port
pub(crate) unsafe fn submit_recv_from(
    op: &mut RecvFrom,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
) -> (Option<io::Error>, bool) {
    // Associate socket with IOCP
    unsafe { CreateIoCompletionPort(op.fd as HANDLE, port, 0, 0) };

    // Update WSABUF to point to current buffer data
    op.wsabuf.len = op.buf.capacity() as u32;
    op.wsabuf.buf = op.buf.as_mut_ptr();

    let mut bytes_received = 0u32;
    let ret = unsafe {
        WSARecvFrom(
            op.fd as SOCKET,
            op.wsabuf.as_ref(),
            1, // dwBufferCount
            &mut bytes_received,
            op.flags.as_mut(), // lpFlags (in/out)
            op.addr.as_mut_ptr() as *mut SOCKADDR,
            op.addr_len.as_mut(), // lpFromlen (in/out)
            overlapped as *mut _,
            None, // lpCompletionRoutine (not used with IOCP)
        )
    };

    if ret == SOCKET_ERROR {
        let err = unsafe { WSAGetLastError() };
        // WSA_IO_PENDING = 997 = ERROR_IO_PENDING
        if err != ERROR_IO_PENDING as i32 {
            return (Some(io::Error::from_raw_os_error(err)), true);
        }
    }
    (None, false)
}
