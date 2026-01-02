//! IOCP Platform-Specific Operation Definitions
//!
//! This module defines:
//! - `IocpOp` enum: The platform-specific operation enum for Windows IOCP
//! - Platform-specific operation state structures with Windows-specific fields
//! - `IntoPlatformOp` implementations for converting generic ops to IocpOp

use crate::io::buffer::FixedBuf;
use crate::io::driver::PlatformOp;
use crate::io::driver::iocp::IocpDriver;
use crate::io::op::{
    Accept, Close, Connect, Fsync, IntoPlatformOp, IoFd, Open, RawHandle, ReadFixed, Recv,
    RecvFrom, Send as OpSend, SendTo, Timeout, Wakeup, WriteFixed,
};
use crate::io::socket::SockAddrStorage;
use std::io;
use std::net::SocketAddr;
use windows_sys::Win32::Networking::WinSock::WSABUF;

// ============================================================================
// Platform-Specific Operation State Structures
// ============================================================================

/// IOCP specific state for Accept operation.
/// Stores the pre-created accept socket required by AcceptEx.
pub struct IocpAccept {
    pub fd: IoFd,
    pub addr: SockAddrStorage,
    pub addr_len: u32,
    /// Pre-created socket for the accepted connection (Windows-specific).
    pub accept_socket: Option<RawHandle>,
    /// Buffer for AcceptEx to store local and remote addresses.
    /// Must be at least 2 * (sizeof(SOCKADDR_STORAGE) + 16).
    pub accept_buffer: [u8; 288],
    pub remote_addr: Option<SocketAddr>,
}

impl From<Accept> for IocpAccept {
    fn from(accept: Accept) -> Self {
        // accept_socket is now properly passed from Accept::pre_alloc
        Self {
            fd: accept.fd,
            addr: accept.addr,
            addr_len: accept.addr_len,
            accept_socket: Some(accept.accept_socket),
            accept_buffer: [0; 288],
            remote_addr: accept.remote_addr,
        }
    }
}

impl From<IocpAccept> for Accept {
    fn from(iocp_accept: IocpAccept) -> Self {
        unsafe {
            // We need to move fields out of a type that implements Drop.
            // We use ptr::read to take ownership of fields, and then mem::forget the container
            // so its Drop implementation (which closes the socket) is NOT called.
            // The socket ownership is transferred to the resulting Accept struct.

            let fd = iocp_accept.fd; // Copy
            let addr = iocp_accept.addr; // Copy struct
            let addr_len = iocp_accept.addr_len; // Copy
            let remote_addr = iocp_accept.remote_addr; // Copy
            // accept_buffer is dropped as it is not needed in Accept

            // We take the socket.
            // Note: iocp_accept.accept_socket is Option<RawHandle>.
            // We read the Option.
            let accept_socket_opt = std::ptr::read(&iocp_accept.accept_socket);

            // Prevent iocp_accept destructor from running
            std::mem::forget(iocp_accept);

            Self {
                fd,
                addr,
                addr_len,
                remote_addr,
                accept_socket: accept_socket_opt
                    .expect("accept_socket missing in completion conversion"),
            }
        }
    }
}

impl Drop for IocpAccept {
    fn drop(&mut self) {
        if let Some(socket) = self.accept_socket {
            unsafe {
                windows_sys::Win32::Networking::WinSock::closesocket(socket as _);
            }
        }
    }
}

/// IOCP specific state for SendTo operation.
/// Includes Windows-specific WSABUF structure.
pub struct IocpSendTo {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: SockAddrStorage,
    pub addr_len: u32,
    pub wsabuf: WSABUF,
}

impl IocpSendTo {
    pub fn new(fd: RawHandle, buf: FixedBuf, target: SocketAddr) -> Self {
        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(target);
        // Addr len for windows might be i32, convert to u32 for storage in struct if needed or keep i32
        // IocpSendTo struct defines addr_len as u32. socket_addr_to_storage returns (SockAddrStorage, i32) on windows.

        let wsabuf = WSABUF {
            len: buf.len() as u32,
            buf: buf.as_slice().as_ptr() as *mut u8,
        };

        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len: addr_len as u32,
            wsabuf,
        }
    }
}

impl From<SendTo> for IocpSendTo {
    fn from(send_to: SendTo) -> Self {
        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(send_to.addr);

        let wsabuf = WSABUF {
            len: send_to.buf.len() as u32,
            buf: send_to.buf.as_slice().as_ptr() as *mut u8,
        };

        Self {
            fd: send_to.fd,
            buf: send_to.buf,
            addr,
            addr_len: addr_len as u32,
            wsabuf,
        }
    }
}

impl From<IocpSendTo> for SendTo {
    fn from(iocp_send_to: IocpSendTo) -> Self {
        let addr = unsafe {
            let s = std::slice::from_raw_parts(
                &iocp_send_to.addr as *const _ as *const u8,
                iocp_send_to.addr_len as usize,
            );
            crate::io::socket::to_socket_addr(s)
                .expect("Failed to convert storage back to SocketAddr")
        };

        Self {
            fd: iocp_send_to.fd,
            buf: iocp_send_to.buf,
            addr,
        }
    }
}

/// IOCP specific state for RecvFrom operation.
/// Includes Windows-specific WSABUF structure and flags.
pub struct IocpRecvFrom {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: SockAddrStorage,
    pub addr_len: i32,
    pub flags: u32,
    pub wsabuf: WSABUF,
}

impl IocpRecvFrom {
    pub fn new(fd: RawHandle, mut buf: FixedBuf) -> Self {
        let addr: SockAddrStorage = unsafe { std::mem::zeroed() };
        let addr_len = std::mem::size_of::<SockAddrStorage>() as i32;

        let wsabuf = WSABUF {
            len: buf.capacity() as u32,
            buf: buf.as_mut_ptr(),
        };
        let flags = 0u32;

        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len,
            flags,
            wsabuf,
        }
    }

    pub fn get_addr_len(&self) -> usize {
        self.addr_len as usize
    }
}

impl From<RecvFrom> for IocpRecvFrom {
    fn from(recv_from: RecvFrom) -> Self {
        let addr: SockAddrStorage = unsafe { std::mem::zeroed() };
        let addr_len = std::mem::size_of::<SockAddrStorage>() as i32;

        let mut buf = recv_from.buf;
        let wsabuf = WSABUF {
            len: buf.capacity() as u32,
            buf: buf.as_mut_ptr(),
        };

        Self {
            fd: recv_from.fd,
            buf,
            addr,
            addr_len,
            flags: 0u32,
            wsabuf,
        }
    }
}

impl From<IocpRecvFrom> for RecvFrom {
    fn from(iocp_recv_from: IocpRecvFrom) -> Self {
        // Convert buffer to SocketAddr
        let len = iocp_recv_from.addr_len as usize;
        let addr = unsafe {
            let s = std::slice::from_raw_parts(&iocp_recv_from.addr as *const _ as *const u8, len);
            crate::io::socket::to_socket_addr(s).ok()
        };

        Self {
            fd: iocp_recv_from.fd,
            buf: iocp_recv_from.buf,
            addr,
        }
    }
}

/// IOCP specific state for Timeout operation.
pub struct IocpTimeout {
    pub duration: std::time::Duration,
}

impl From<Timeout> for IocpTimeout {
    fn from(timeout: Timeout) -> Self {
        Self {
            duration: timeout.duration,
        }
    }
}

impl From<IocpTimeout> for Timeout {
    fn from(iocp_timeout: IocpTimeout) -> Self {
        Self {
            duration: iocp_timeout.duration,
        }
    }
}

/// IOCP specific state for Wakeup operation.
/// Uses PostQueuedCompletionStatus, no additional fields needed.
pub struct IocpWakeup;

impl From<Wakeup> for IocpWakeup {
    fn from(_wakeup: Wakeup) -> Self {
        Self
    }
}

impl From<IocpWakeup> for Wakeup {
    fn from(_iocp_wakeup: IocpWakeup) -> Self {
        Self { fd: IoFd::Raw(0) }
    }
}

/// IOCP specific state for Open operation.
/// Uses UTF-16 encoding for Windows paths.
pub struct IocpOpen {
    pub path: Vec<u16>,
    pub flags: i32,
    pub mode: u32,
}

impl From<Open> for IocpOpen {
    fn from(open: Open) -> Self {
        // The path bytes are already packed UTF-16 (2 bytes per char, little-endian)
        // Convert back to Vec<u16>
        let path: Vec<u16> = open
            .path
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        Self {
            path,
            flags: open.flags,
            mode: open.mode,
        }
    }
}

impl From<IocpOpen> for Open {
    fn from(iocp_open: IocpOpen) -> Self {
        // Convert UTF-16 back to UTF-8
        let path_without_null: Vec<u16> =
            iocp_open.path.into_iter().take_while(|&c| c != 0).collect();
        let path = String::from_utf16_lossy(&path_without_null).into_bytes();

        Self {
            path,
            flags: iocp_open.flags,
            mode: iocp_open.mode,
        }
    }
}

// ============================================================================
// IocpOp Enum Definition
// ============================================================================

// ============================================================================
// OverlappedEntry Definition
// ============================================================================

#[repr(C)]
pub struct OverlappedEntry {
    pub inner: windows_sys::Win32::System::IO::OVERLAPPED,
    pub user_data: usize,
    pub blocking_result: Option<io::Result<usize>>,
}

impl OverlappedEntry {
    pub fn new(user_data: usize) -> Self {
        Self {
            inner: unsafe { std::mem::zeroed() },
            user_data,
            blocking_result: None,
        }
    }
}

// ============================================================================
// IocpOp Enum Definition
// ============================================================================

/// The IOCP platform-specific operation enum.
/// Each variant wraps either a cross-platform op struct or a platform-specific state.
pub enum IocpOp {
    ReadFixed {
        data: ReadFixed,
        entry: OverlappedEntry,
    },
    WriteFixed {
        data: WriteFixed,
        entry: OverlappedEntry,
    },
    Recv {
        data: Recv,
        entry: OverlappedEntry,
    },
    Send {
        data: OpSend,
        entry: OverlappedEntry,
    },
    Accept {
        data: IocpAccept,
        entry: OverlappedEntry,
    },
    Connect {
        data: Connect,
        entry: OverlappedEntry,
    },
    RecvFrom {
        data: IocpRecvFrom,
        entry: OverlappedEntry,
    },
    SendTo {
        data: IocpSendTo,
        entry: OverlappedEntry,
    },
    Open {
        data: IocpOpen,
        entry: OverlappedEntry,
    },
    Close {
        data: Close,
        entry: OverlappedEntry,
    },
    Fsync {
        data: Fsync,
        entry: OverlappedEntry,
    },
    // Wakeup does not need OVERLAPPED in the same way if strictly used for cancellation,
    // but PostQueuedCompletionStatus can use it.
    Wakeup {
        data: IocpWakeup,
        entry: OverlappedEntry,
    },
    Timeout(IocpTimeout),
}

// IocpOp needs to be Send so it can be passed to the Driver
unsafe impl Send for IocpOp {}

impl PlatformOp for IocpOp {}

// ============================================================================
// IntoPlatformOp Implementations
// ============================================================================

macro_rules! impl_into_iocp_op_direct {
    ($Type:ident) => {
        impl IntoPlatformOp<IocpDriver> for $Type {
            fn into_platform_op(self) -> IocpOp {
                IocpOp::$Type {
                    data: self,
                    entry: OverlappedEntry::new(0),
                }
            }
            fn from_platform_op(op: IocpOp) -> Self {
                match op {
                    IocpOp::$Type { data, .. } => data,
                    _ => panic!(concat!(
                        "Driver returned mismatched Op type: expected ",
                        stringify!($Type)
                    )),
                }
            }
        }
    };
}

macro_rules! impl_into_iocp_op_convert {
    ($GenericType:ident, $IocpType:ident, $Variant:ident) => {
        impl IntoPlatformOp<IocpDriver> for $GenericType {
            fn into_platform_op(self) -> IocpOp {
                IocpOp::$Variant {
                    data: self.into(),
                    entry: OverlappedEntry::new(0),
                }
            }
            fn from_platform_op(op: IocpOp) -> Self {
                match op {
                    IocpOp::$Variant { data, .. } => data.into(),
                    _ => panic!(concat!(
                        "Driver returned mismatched Op type: expected ",
                        stringify!($Variant)
                    )),
                }
            }
        }
    };
}

// Direct mappings (no conversion needed)
impl_into_iocp_op_direct!(ReadFixed);
impl_into_iocp_op_direct!(WriteFixed);
impl_into_iocp_op_direct!(Recv);
impl_into_iocp_op_direct!(Connect);
impl_into_iocp_op_direct!(Close);
impl_into_iocp_op_direct!(Fsync);

// Conversions for platform-specific state
impl_into_iocp_op_convert!(Accept, IocpAccept, Accept);
impl_into_iocp_op_convert!(SendTo, IocpSendTo, SendTo);
impl_into_iocp_op_convert!(RecvFrom, IocpRecvFrom, RecvFrom);
impl_into_iocp_op_convert!(Open, IocpOpen, Open);
// Timeout doesn't use Overlapped
impl IntoPlatformOp<IocpDriver> for Timeout {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Timeout(self.into())
    }
    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Timeout(t) => t.into(),
            _ => panic!("Driver returned mismatched Op type: expected Timeout"),
        }
    }
}

impl_into_iocp_op_convert!(Wakeup, IocpWakeup, Wakeup);

// Manual implementation for Send because of name conflict with OpSend
impl IntoPlatformOp<IocpDriver> for OpSend {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Send {
            data: self,
            entry: OverlappedEntry::new(0),
        }
    }
    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Send { data, .. } => data,
            _ => panic!("Driver returned mismatched Op type: expected Send"),
        }
    }
}

impl IocpOp {
    /// Get the file descriptor associated with this operation (if any).
    pub fn get_fd(&self) -> Option<IoFd> {
        match self {
            IocpOp::ReadFixed { data, .. } => Some(data.fd),
            IocpOp::WriteFixed { data, .. } => Some(data.fd),
            IocpOp::Recv { data, .. } => Some(data.fd),
            IocpOp::Send { data, .. } => Some(data.fd),
            IocpOp::Accept { data, .. } => Some(data.fd),
            IocpOp::Connect { data, .. } => Some(data.fd),
            IocpOp::RecvFrom { data, .. } => Some(data.fd),
            IocpOp::SendTo { data, .. } => Some(data.fd),
            IocpOp::Close { data, .. } => Some(data.fd),
            IocpOp::Fsync { data, .. } => Some(data.fd),
            IocpOp::Open { .. } | IocpOp::Wakeup { .. } | IocpOp::Timeout(_) => None,
        }
    }

    pub fn entry_mut(&mut self) -> Option<&mut OverlappedEntry> {
        match self {
            IocpOp::ReadFixed { entry, .. } => Some(entry),
            IocpOp::WriteFixed { entry, .. } => Some(entry),
            IocpOp::Recv { entry, .. } => Some(entry),
            IocpOp::Send { entry, .. } => Some(entry),
            IocpOp::Accept { entry, .. } => Some(entry),
            IocpOp::Connect { entry, .. } => Some(entry),
            IocpOp::RecvFrom { entry, .. } => Some(entry),
            IocpOp::SendTo { entry, .. } => Some(entry),
            IocpOp::Open { entry, .. } => Some(entry),
            IocpOp::Close { entry, .. } => Some(entry),
            IocpOp::Fsync { entry, .. } => Some(entry),
            IocpOp::Wakeup { entry, .. } => Some(entry),
            IocpOp::Timeout(_) => None,
        }
    }
}
