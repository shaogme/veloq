//! IOCP Platform-Specific Operation Definitions
//!
//! This module defines:
//! - `IocpAbi`: The platform-specific ABI for Windows IOCP
//! - `IocpOp` type alias: The unified operation enum specialized for IOCP
//! - Platform-specific "extra" structures
//! - `IntoPlatformOp` implementations

use crate::io::driver::PlatformOp;
use crate::io::driver::iocp::IocpDriver;
use crate::io::op::{
    Accept, Close, Connect, Fallocate, Fsync, IntoPlatformOp, OpAbi, Open, Operation, ReadFixed,
    Recv, RecvFrom, Send as OpSend, SendTo, SyncFileRange, Timeout, Wakeup, WriteFixed,
};
use crate::io::socket::SockAddrStorage;
use std::io;
use windows_sys::Win32::Networking::WinSock::WSABUF;

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
// Platform-Specific Extra State Structures
// ============================================================================

/// Common state for simple overlapped operations.
pub type IocpState = OverlappedEntry;

pub struct IocpAcceptExtras {
    pub entry: OverlappedEntry,
    /// Buffer for AcceptEx to store local and remote addresses.
    /// Must be at least 2 * (sizeof(SOCKADDR_STORAGE) + 16).
    pub accept_buffer: [u8; 288],
}

pub struct IocpSendToExtras {
    pub entry: OverlappedEntry,
    pub wsabuf: WSABUF,
    pub addr: SockAddrStorage,
    pub addr_len: i32,
}

pub struct IocpRecvFromExtras {
    pub entry: OverlappedEntry,
    pub wsabuf: WSABUF,
    pub flags: u32,
    pub addr: SockAddrStorage,
    pub addr_len: i32,
}

pub struct IocpOpenExtras {
    pub entry: OverlappedEntry,
    pub path: Vec<u16>,
}

pub struct IocpWakeupExtras {
    pub entry: OverlappedEntry,
}

pub struct IocpTimeoutExtras; // Timeout doesn't use overlapped in IocpSubmit impl currently (returns Pending immediately)

// ============================================================================
// IocpAbi Implementation
// ============================================================================

pub struct IocpAbi;

impl OpAbi for IocpAbi {
    type ReadFixed = IocpState;
    type WriteFixed = IocpState;
    type Recv = IocpState;
    type Send = IocpState;
    type Connect = IocpState;
    type Close = IocpState;
    type Fsync = IocpState;
    type SyncFileRange = OverlappedEntry;
    type Fallocate = OverlappedEntry;

    type Accept = IocpAcceptExtras;
    type SendTo = IocpSendToExtras;
    type RecvFrom = IocpRecvFromExtras;
    type Open = IocpOpenExtras;
    type Wakeup = IocpWakeupExtras;
    type Timeout = IocpTimeoutExtras; // Or () if we don't store state
}

pub type IocpOp = Operation<IocpAbi>;

// IocpOp needs to be Send
unsafe impl Send for IocpAbi {}
// Wait, OpAbi trait requires Send. And Operation<P> is Send if P is OpAbi (which implies Send).
// But P::AssocTypes also need to be Send.
// OverlappedEntry contains raw pointers (OVERLAPPED).
// So we need unsafe impl Send for OverlappedEntry.
unsafe impl Send for OverlappedEntry {}

impl PlatformOp for IocpOp {}

impl IocpOp {
    pub fn entry_mut(&mut self) -> Option<&mut OverlappedEntry> {
        match self {
            Self::ReadFixed(_, entry) => Some(entry),
            Self::WriteFixed(_, entry) => Some(entry),
            Self::Recv(_, entry) => Some(entry),
            Self::Send(_, entry) => Some(entry),
            Self::Accept(_, extras) => Some(&mut extras.entry),
            Self::Connect(_, entry) => Some(entry),
            Self::RecvFrom(_, extras) => Some(&mut extras.entry),
            Self::SendTo(_, extras) => Some(&mut extras.entry),
            Self::Open(_, extras) => Some(&mut extras.entry),
            Self::Close(_, entry) => Some(entry),
            Self::Fsync(_, entry) => Some(entry),
            Self::SyncFileRange(_, entry) => Some(entry),
            Self::Fallocate(_, entry) => Some(entry),
            Self::Wakeup(_, extras) => Some(&mut extras.entry),
            Self::Timeout(_, _) => None,
        }
    }
}

// ============================================================================
// IntoPlatformOp Implementations
// ============================================================================

macro_rules! impl_into_iocp_op_simple {
    ($Type:ident) => {
        impl IntoPlatformOp<IocpDriver> for $Type {
            fn into_platform_op(self) -> IocpOp {
                IocpOp::$Type(self, OverlappedEntry::new(0))
            }
            fn from_platform_op(op: IocpOp) -> Self {
                match op {
                    IocpOp::$Type(val, _) => val,
                    _ => panic!(concat!(
                        "Driver returned mismatched Op type: expected ",
                        stringify!($Type)
                    )),
                }
            }
        }
    };
}

impl_into_iocp_op_simple!(ReadFixed);
impl_into_iocp_op_simple!(WriteFixed);
impl_into_iocp_op_simple!(Recv);
impl_into_iocp_op_simple!(Connect);
impl_into_iocp_op_simple!(Close);
impl_into_iocp_op_simple!(Fsync);

// Manual implementation for SyncFileRange (empty/stub for now)
impl IntoPlatformOp<IocpDriver> for SyncFileRange {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::SyncFileRange(self, OverlappedEntry::new(0))
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::SyncFileRange(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected SyncFileRange"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for Fallocate {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Fallocate(self, OverlappedEntry::new(0))
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Fallocate(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Fallocate"),
        }
    }
}

// Manual implementations

impl IntoPlatformOp<IocpDriver> for Accept {
    fn into_platform_op(self) -> IocpOp {
        let extras = IocpAcceptExtras {
            entry: OverlappedEntry::new(0),
            accept_buffer: [0; 288],
        };
        IocpOp::Accept(self, extras)
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Accept(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Accept"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for SendTo {
    fn into_platform_op(self) -> IocpOp {
        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(self.addr); // Fix: use helper
        let wsabuf = WSABUF {
            len: self.buf.len() as u32,
            buf: self.buf.as_slice().as_ptr() as *mut u8,
        };
        let extras = IocpSendToExtras {
            entry: OverlappedEntry::new(0),
            wsabuf,
            addr,
            addr_len: addr_len as i32,
        };
        IocpOp::SendTo(self, extras)
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::SendTo(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected SendTo"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for RecvFrom {
    fn into_platform_op(mut self) -> IocpOp {
        let wsabuf = WSABUF {
            len: self.buf.capacity() as u32,
            buf: self.buf.as_mut_ptr(),
        };
        let extras = IocpRecvFromExtras {
            entry: OverlappedEntry::new(0),
            wsabuf,
            flags: 0,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<SockAddrStorage>() as i32,
        };
        IocpOp::RecvFrom(self, extras)
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::RecvFrom(mut val, extras) => {
                // Convert buffer to SocketAddr if needed
                let len = extras.addr_len as usize;
                let addr = unsafe {
                    let s = std::slice::from_raw_parts(&extras.addr as *const _ as *const u8, len);
                    crate::io::socket::to_socket_addr(s).ok()
                };
                val.addr = addr;
                val
            }
            _ => panic!("Driver returned mismatched Op type: expected RecvFrom"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for Open {
    fn into_platform_op(self) -> IocpOp {
        // The Open struct definition states that on Windows, path is Vec<u16> bytes (serialized).
        // Reconstruct Vec<u16> from Vec<u8>.
        let path: Vec<u16> = self
            .path
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        let extras = IocpOpenExtras {
            entry: OverlappedEntry::new(0),
            path,
        };
        IocpOp::Open(self, extras)
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Open(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Open"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for Timeout {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Timeout(self, IocpTimeoutExtras)
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Timeout(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Timeout"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for Wakeup {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Wakeup(
            self,
            IocpWakeupExtras {
                entry: OverlappedEntry::new(0),
            },
        )
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Wakeup(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Wakeup"),
        }
    }
}

impl IntoPlatformOp<IocpDriver> for OpSend {
    fn into_platform_op(self) -> IocpOp {
        IocpOp::Send(self, OverlappedEntry::new(0))
    }

    fn from_platform_op(op: IocpOp) -> Self {
        match op {
            IocpOp::Send(val, _) => val,
            _ => panic!("Driver returned mismatched Op type: expected Send"),
        }
    }
}
