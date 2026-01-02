//! io_uring Platform-Specific Operation Definitions
//!
//! This module defines:
//! - `UringOp` enum: The platform-specific operation enum for io_uring
//! - Platform-specific operation state structures with Linux-specific fields
//! - `IntoPlatformOp` implementations for converting generic ops to UringOp

use crate::io::driver::PlatformOp;
use crate::io::driver::uring::UringDriver;
use crate::io::op::{
    Accept, Close, Connect, Fsync, IntoPlatformOp, Open, ReadFixed, Recv, RecvFrom,
    Send as OpSend, SendTo, Timeout, Wakeup, WriteFixed, IoFd, RawHandle,
};
use crate::io::buffer::FixedBuf;
use std::net::SocketAddr;

// ============================================================================
// Platform-Specific Operation State Structures
// ============================================================================

/// io_uring specific state for Accept operation.
/// Stores Linux-specific fields like libc sockaddr buffers.
pub struct UringAccept {
    pub fd: IoFd,
    pub addr: libc::sockaddr_storage,
    pub addr_len: libc::socklen_t,
    pub remote_addr: Option<SocketAddr>,
}

impl From<Accept> for UringAccept {
    fn from(accept: Accept) -> Self {
        // We typically don't have the addr yet for Accept, it's an output buffer.
        // But the generic Accept struct has `addr: Box<[u8]>`?
        // Let's check the generic Accept struct definition if needed. 
        // Based on previous view, generic Accept has `addr: Box<[u8]>`.
        // We will ignore the Box from generic Accept and create our own storage.
        Self {
            fd: accept.fd,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<libc::sockaddr_storage>() as _,
            remote_addr: accept.remote_addr,
        }
    }
}

impl From<UringAccept> for Accept {
    fn from(uring_accept: UringAccept) -> Self {
        Self {
            fd: uring_accept.fd,
            addr: uring_accept.addr,
            addr_len: uring_accept.addr_len,
            remote_addr: uring_accept.remote_addr,
        }
    }
}

/// io_uring specific state for SendTo operation.
/// Includes Linux-specific msghdr and iovec structures.
pub struct UringSendTo {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: libc::sockaddr_storage,
    pub addr_len: libc::socklen_t,
    pub msghdr: libc::msghdr,
    pub iovec: [libc::iovec; 1],
}

impl UringSendTo {
    pub fn new(fd: RawHandle, buf: FixedBuf, target: SocketAddr) -> Self {
        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(target);

        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len,
            msghdr: unsafe { std::mem::zeroed() },
            iovec: [unsafe { std::mem::zeroed() }],
        }
    }
}

impl From<SendTo> for UringSendTo {
    fn from(send_to: SendTo) -> Self {
        // Use helper to convert SocketAddr to storage without allocation
        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(send_to.addr);
        
        Self {
            fd: send_to.fd,
            buf: send_to.buf,
            addr,
            addr_len,
            msghdr: unsafe { std::mem::zeroed() },
            iovec: [unsafe { std::mem::zeroed() }],
        }
    }
}

impl From<UringSendTo> for SendTo {
    fn from(uring_send_to: UringSendTo) -> Self {
        // Convert storage back to SocketAddr
        // We can assume valid address if it came from us, but to be safe/correct:
        let addr_bytes = unsafe {
            std::slice::from_raw_parts(
                &uring_send_to.addr as *const _ as *const u8,
                uring_send_to.addr_len as usize,
            )
        };
        let addr = crate::io::socket::to_socket_addr(addr_bytes)
            .expect("Failed to convert storage back to SocketAddr");

        Self {
            fd: uring_send_to.fd,
            buf: uring_send_to.buf,
            addr,
        }
    }
}

/// io_uring specific state for RecvFrom operation.
/// Includes Linux-specific msghdr and iovec structures.
pub struct UringRecvFrom {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: libc::sockaddr_storage,
    pub addr_len: libc::socklen_t,
    pub msghdr: libc::msghdr,
    pub iovec: [libc::iovec; 1],
}

impl UringRecvFrom {
    pub fn new(fd: RawHandle, buf: FixedBuf) -> Self {
        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<libc::sockaddr_storage>() as _,
            msghdr: unsafe { std::mem::zeroed() },
            iovec: [unsafe { std::mem::zeroed() }],
        }
    }

    pub fn get_addr_len(&self) -> usize {
        self.msghdr.msg_namelen as usize
    }
}

impl From<RecvFrom> for UringRecvFrom {
    fn from(recv_from: RecvFrom) -> Self {
        Self {
            fd: recv_from.fd,
            buf: recv_from.buf,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<libc::sockaddr_storage>() as _,
            msghdr: unsafe { std::mem::zeroed() },
            iovec: [unsafe { std::mem::zeroed() }],
        }
    }
}

impl From<UringRecvFrom> for RecvFrom {
    fn from(uring_recv_from: UringRecvFrom) -> Self {
        // Try to parse the address from storage
        // The kernel updates msghdr.msg_namelen with the actual address length
        let len = uring_recv_from.msghdr.msg_namelen as usize;
        let addr_bytes = unsafe {
            std::slice::from_raw_parts(
                &uring_recv_from.addr as *const _ as *const u8,
                len,
            )
        };
        // It's possible the recv didn't get an address (e.g. connected socket), 
        // or partial read? But usually RecvFrom expects addr.
        let addr = crate::io::socket::to_socket_addr(addr_bytes).ok();

        Self {
            fd: uring_recv_from.fd,
            buf: uring_recv_from.buf,
            addr,
        }
    }
}

/// io_uring specific state for Timeout operation.
/// Includes the timespec buffer needed by the kernel.
pub struct UringTimeout {
    pub duration: std::time::Duration,
    /// Timespec buffer: [seconds, nanoseconds]
    pub ts: [i64; 2],
}

impl From<Timeout> for UringTimeout {
    fn from(timeout: Timeout) -> Self {
        Self {
            duration: timeout.duration,
            ts: [0, 0],
        }
    }
}

impl From<UringTimeout> for Timeout {
    fn from(uring_timeout: UringTimeout) -> Self {
        Self {
            duration: uring_timeout.duration,
        }
    }
}

/// io_uring specific state for Wakeup operation.
/// Uses eventfd read buffer.
pub struct UringWakeup {
    pub fd: IoFd,
    pub buf: [u8; 8],
}

impl UringWakeup {
    pub fn new(fd: RawHandle) -> Self {
        Self {
            fd: IoFd::Raw(fd),
            buf: [0u8; 8],
        }
    }
}

impl From<Wakeup> for UringWakeup {
    fn from(wakeup: Wakeup) -> Self {
        Self {
            fd: wakeup.fd,
            buf: [0u8; 8],
        }
    }
}

impl From<UringWakeup> for Wakeup {
    fn from(uring_wakeup: UringWakeup) -> Self {
        Self {
            fd: uring_wakeup.fd,
        }
    }
}

/// io_uring specific state for Open operation.
/// Uses CString for path on Linux.
pub struct UringOpen {
    pub path: std::ffi::CString,
    pub flags: i32,
    pub mode: u32,
}

impl From<Open> for UringOpen {
    fn from(open: Open) -> Self {
        // Convert UTF-8 bytes to CString
        let path = std::ffi::CString::new(open.path)
            .unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
        Self {
            path,
            flags: open.flags,
            mode: open.mode,
        }
    }
}

impl From<UringOpen> for Open {
    fn from(uring_open: UringOpen) -> Self {
        Self {
            path: uring_open.path.into_bytes(),
            flags: uring_open.flags,
            mode: uring_open.mode,
        }
    }
}

// ============================================================================
// UringOp Enum Definition
// ============================================================================

/// The io_uring platform-specific operation enum.
/// Each variant wraps either a cross-platform op struct or a platform-specific state.
pub enum UringOp {
    ReadFixed(ReadFixed),
    WriteFixed(WriteFixed),
    Recv(Recv),
    Send(OpSend),
    Accept(UringAccept),
    Connect(Connect),
    RecvFrom(UringRecvFrom),
    SendTo(UringSendTo),
    Open(UringOpen),
    Close(Close),
    Fsync(Fsync),
    Wakeup(UringWakeup),
    Timeout(UringTimeout),
}

impl PlatformOp for UringOp {}

// ============================================================================
// IntoPlatformOp Implementations
// ============================================================================

macro_rules! impl_into_uring_op_direct {
    ($Type:ident) => {
        impl IntoPlatformOp<UringDriver> for $Type {
            fn into_platform_op(self) -> UringOp {
                UringOp::$Type(self)
            }
            fn from_platform_op(op: UringOp) -> Self {
                match op {
                    UringOp::$Type(val) => val,
                    _ => panic!(concat!(
                        "Driver returned mismatched Op type: expected ",
                        stringify!($Type)
                    )),
                }
            }
        }
    };
}

macro_rules! impl_into_uring_op_convert {
    ($GenericType:ident, $UringType:ident, $Variant:ident) => {
        impl IntoPlatformOp<UringDriver> for $GenericType {
            fn into_platform_op(self) -> UringOp {
                UringOp::$Variant(self.into())
            }
            fn from_platform_op(op: UringOp) -> Self {
                match op {
                    UringOp::$Variant(val) => val.into(),
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
impl_into_uring_op_direct!(ReadFixed);
impl_into_uring_op_direct!(WriteFixed);
impl_into_uring_op_direct!(Recv);
impl_into_uring_op_direct!(Connect);
impl_into_uring_op_direct!(Close);
impl_into_uring_op_direct!(Fsync);

// Conversions for platform-specific state
impl_into_uring_op_convert!(Accept, UringAccept, Accept);
impl_into_uring_op_convert!(SendTo, UringSendTo, SendTo);
impl_into_uring_op_convert!(RecvFrom, UringRecvFrom, RecvFrom);
impl_into_uring_op_convert!(Open, UringOpen, Open);
impl_into_uring_op_convert!(Timeout, UringTimeout, Timeout);
impl_into_uring_op_convert!(Wakeup, UringWakeup, Wakeup);

// Manual implementation for Send because of name conflict with OpSend
impl IntoPlatformOp<UringDriver> for OpSend {
    fn into_platform_op(self) -> UringOp {
        UringOp::Send(self)
    }
    fn from_platform_op(op: UringOp) -> Self {
        match op {
            UringOp::Send(val) => val,
            _ => panic!("Driver returned mismatched Op type: expected Send"),
        }
    }
}
