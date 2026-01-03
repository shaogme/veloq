//! IOCP Platform-Specific Operation Definitions
//!
//! This module defines:
//! - `IocpOp`: The Type-Erased operation struct using Unions and VTables
//! - `OpVTable`: The virtual table for dynamic dispatch without enums
//! - `IntoPlatformOp` implementations using blind casting

use crate::io::buffer::BufPool;
use crate::io::driver::PlatformOp;
use crate::io::driver::iocp::IocpDriver;
use crate::io::driver::iocp::ext::Extensions;
use crate::io::driver::iocp::submit::{self, SubmissionResult};
use crate::io::op::{
    Accept, Close, Connect, Fallocate, Fsync, IntoPlatformOp, IoFd, Open, ReadFixed, Recv,
    RecvFrom, Send as OpSend, SendTo, SyncFileRange, Timeout, Wakeup, WriteFixed,
};
use crate::io::socket::SockAddrStorage;
use std::io;
use std::mem::ManuallyDrop;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Networking::WinSock::WSABUF;
use windows_sys::Win32::System::IO::OVERLAPPED;

// ============================================================================
// OverlappedEntry Definition
// ============================================================================

#[repr(C)]
pub struct OverlappedEntry {
    pub inner: OVERLAPPED,
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
// VTable Definition
// ============================================================================

pub type SubmitFn<P> = unsafe fn(
    op: &mut IocpOp<P>,
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
    ext: &Extensions,
    registered_files: &[Option<HANDLE>],
) -> io::Result<SubmissionResult>;

pub type OnCompleteFn<P> =
    unsafe fn(op: &mut IocpOp<P>, result: usize, ext: &Extensions) -> io::Result<usize>;

pub type DropFn<P> = unsafe fn(op: &mut IocpOp<P>);

pub type GetFdFn<P> = unsafe fn(op: &IocpOp<P>) -> Option<IoFd>;

pub struct OpVTable<P: BufPool> {
    pub submit: SubmitFn<P>,
    pub on_complete: Option<OnCompleteFn<P>>,
    pub drop: DropFn<P>,
    pub get_fd: GetFdFn<P>,
}

// ============================================================================
// IocpOp Struct & Union (Type-Erased)
// ============================================================================

#[repr(C)]
pub struct IocpOp<P: BufPool> {
    /// Public header accessible directly by Driver
    pub header: OverlappedEntry,

    /// Virtual Table for dynamic dispatch
    pub vtable: &'static OpVTable<P>,

    /// Type-erased payload
    pub payload: IocpOpPayload<P>,
}

impl<P: BufPool> PlatformOp for IocpOp<P> {}

impl<P: BufPool> IocpOp<P> {
    /// Helper to access the OverlappedEntry (header).
    /// Kept for compatibility with existing Driver code.
    pub fn entry_mut(&mut self) -> Option<&mut OverlappedEntry> {
        Some(&mut self.header)
    }

    pub fn get_fd(&self) -> Option<IoFd> {
        unsafe { (self.vtable.get_fd)(self) }
    }
}

impl<P: BufPool> Drop for IocpOp<P> {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self) };
    }
}

// Ensure proper alignment
#[repr(C)]
pub union IocpOpPayload<P: BufPool> {
    pub read: ManuallyDrop<ReadFixed<P>>,
    pub write: ManuallyDrop<WriteFixed<P>>,
    pub recv: ManuallyDrop<Recv<P>>,
    pub send: ManuallyDrop<OpSend<P>>,
    pub connect: ManuallyDrop<Connect>,
    pub accept: ManuallyDrop<AcceptPayload>,
    pub send_to: ManuallyDrop<SendToPayload<P>>,
    pub recv_from: ManuallyDrop<RecvFromPayload<P>>,
    pub open: ManuallyDrop<OpenPayload<P>>,
    pub close: ManuallyDrop<Close>,
    pub fsync: ManuallyDrop<Fsync>,
    pub sync_range: ManuallyDrop<SyncFileRange>,
    pub fallocate: ManuallyDrop<Fallocate>,
    pub wakeup: ManuallyDrop<WakeupPayload>,
    pub timeout: ManuallyDrop<Timeout>,
}

// ============================================================================
// Payload Structures for Complex Ops
// ============================================================================

pub struct AcceptPayload {
    pub op: Accept,
    pub accept_buffer: [u8; 288],
}

pub struct SendToPayload<P: BufPool> {
    pub op: SendTo<P>,
    pub wsabuf: WSABUF,
    pub addr: SockAddrStorage,
    pub addr_len: i32,
}

pub struct RecvFromPayload<P: BufPool> {
    pub op: RecvFrom<P>,
    pub wsabuf: WSABUF,
    pub flags: u32,
    pub addr: SockAddrStorage,
    pub addr_len: i32,
}

pub struct OpenPayload<P: BufPool> {
    pub op: Open<P>,
}

pub struct WakeupPayload {
    pub op: Wakeup,
}

// ============================================================================
// IntoPlatformOp Implementations
// ============================================================================

macro_rules! impl_into_iocp_op {
    ($Type:ident, $Field:ident, $Submit:ident, $Complete:expr, $Drop:ident, $GetFd:ident) => {
        impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for $Type<P> {
            fn into_platform_op(self) -> IocpOp<P> {
                struct VTableHolder<P: BufPool>(P);
                impl<P: BufPool> VTableHolder<P> {
                    const TABLE: OpVTable<P> = OpVTable {
                        submit: submit::$Submit,
                        on_complete: $Complete,
                        drop: submit::$Drop,
                        get_fd: submit::$GetFd,
                    };
                }

                IocpOp {
                    header: OverlappedEntry::new(0),
                    vtable: &VTableHolder::<P>::TABLE,
                    payload: IocpOpPayload {
                        $Field: ManuallyDrop::new(self),
                    },
                }
            }
            fn from_platform_op(op: IocpOp<P>) -> Self {
                let op = ManuallyDrop::new(op);
                unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.$Field)) }
            }
        }
    };
}

macro_rules! impl_into_iocp_op_simple {
    ($Type:ident, $Field:ident, $Submit:ident, $Complete:expr, $Drop:ident, $GetFd:ident) => {
        impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for $Type {
            fn into_platform_op(self) -> IocpOp<P> {
                struct VTableHolder<P: BufPool>(P);
                impl<P: BufPool> VTableHolder<P> {
                    const TABLE: OpVTable<P> = OpVTable {
                        submit: submit::$Submit,
                        on_complete: $Complete,
                        drop: submit::$Drop,
                        get_fd: submit::$GetFd,
                    };
                }

                IocpOp {
                    header: OverlappedEntry::new(0),
                    vtable: &VTableHolder::<P>::TABLE,
                    payload: IocpOpPayload {
                        $Field: ManuallyDrop::new(self),
                    },
                }
            }
            fn from_platform_op(op: IocpOp<P>) -> Self {
                let op = ManuallyDrop::new(op);
                unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.$Field)) }
            }
        }
    };
}

impl_into_iocp_op!(
    ReadFixed,
    read,
    submit_read_fixed,
    None,
    drop_read_fixed,
    get_fd_read_fixed
);
impl_into_iocp_op!(
    WriteFixed,
    write,
    submit_write_fixed,
    None,
    drop_write_fixed,
    get_fd_write_fixed
);
impl_into_iocp_op!(Recv, recv, submit_recv, None, drop_recv, get_fd_recv);
impl_into_iocp_op!(OpSend, send, submit_send, None, drop_send, get_fd_send);

impl_into_iocp_op_simple!(
    Connect,
    connect,
    submit_connect,
    Some(submit::on_complete_connect),
    drop_connect,
    get_fd_connect
);
impl_into_iocp_op_simple!(Close, close, submit_close, None, drop_close, get_fd_close);
impl_into_iocp_op_simple!(Fsync, fsync, submit_fsync, None, drop_fsync, get_fd_fsync);
impl_into_iocp_op_simple!(
    SyncFileRange,
    sync_range,
    submit_sync_range,
    None,
    drop_sync_range,
    get_fd_sync_range
);
impl_into_iocp_op_simple!(
    Fallocate,
    fallocate,
    submit_fallocate,
    None,
    drop_fallocate,
    get_fd_fallocate
);
impl_into_iocp_op_simple!(
    Timeout,
    timeout,
    submit_timeout,
    None,
    drop_timeout,
    get_fd_timeout
);

impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for Accept {
    fn into_platform_op(self) -> IocpOp<P> {
        struct VTableHolder<P: BufPool>(P);
        impl<P: BufPool> VTableHolder<P> {
            const TABLE: OpVTable<P> = OpVTable {
                submit: submit::submit_accept,
                on_complete: Some(submit::on_complete_accept),
                drop: submit::drop_accept,
                get_fd: submit::get_fd_accept,
            };
        }

        let payload = AcceptPayload {
            op: self,
            accept_buffer: [0; 288],
        };
        IocpOp {
            header: OverlappedEntry::new(0),
            vtable: &VTableHolder::<P>::TABLE,
            payload: IocpOpPayload {
                accept: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: IocpOp<P>) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.accept)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for SendTo<P> {
    fn into_platform_op(self) -> IocpOp<P> {
        struct VTableHolder<P: BufPool>(P);
        impl<P: BufPool> VTableHolder<P> {
            const TABLE: OpVTable<P> = OpVTable {
                submit: submit::submit_send_to,
                on_complete: None,
                drop: submit::drop_send_to,
                get_fd: submit::get_fd_send_to,
            };
        }

        let (addr, addr_len) = crate::io::socket::socket_addr_to_storage(self.addr);
        let wsabuf = WSABUF {
            len: self.buf.len() as u32,
            buf: self.buf.as_slice().as_ptr() as *mut u8,
        };
        let payload = SendToPayload {
            op: self,
            wsabuf,
            addr,
            addr_len: addr_len as i32,
        };
        IocpOp {
            header: OverlappedEntry::new(0),
            vtable: &VTableHolder::<P>::TABLE,
            payload: IocpOpPayload {
                send_to: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: IocpOp<P>) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.send_to)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for RecvFrom<P> {
    fn into_platform_op(mut self) -> IocpOp<P> {
        struct VTableHolder<P: BufPool>(P);
        impl<P: BufPool> VTableHolder<P> {
            const TABLE: OpVTable<P> = OpVTable {
                submit: submit::submit_recv_from,
                on_complete: None,
                drop: submit::drop_recv_from,
                get_fd: submit::get_fd_recv_from,
            };
        }

        let wsabuf = WSABUF {
            len: self.buf.capacity() as u32,
            buf: self.buf.as_mut_ptr(),
        };
        let payload = RecvFromPayload {
            op: self,
            wsabuf,
            flags: 0,
            addr: unsafe { std::mem::zeroed() },
            addr_len: std::mem::size_of::<SockAddrStorage>() as i32,
        };
        IocpOp {
            header: OverlappedEntry::new(0),
            vtable: &VTableHolder::<P>::TABLE,
            payload: IocpOpPayload {
                recv_from: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: IocpOp<P>) -> Self {
        let op = ManuallyDrop::new(op);
        let payload = unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.recv_from)) };
        let mut val = payload.op;
        let len = payload.addr_len as usize;
        let addr = unsafe {
            let s = std::slice::from_raw_parts(&payload.addr as *const _ as *const u8, len);
            crate::io::socket::to_socket_addr(s).ok()
        };
        val.addr = addr;
        val
    }
}

impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for Open<P> {
    fn into_platform_op(self) -> IocpOp<P> {
        struct VTableHolder<P: BufPool>(P);
        impl<P: BufPool> VTableHolder<P> {
            const TABLE: OpVTable<P> = OpVTable {
                submit: submit::submit_open,
                on_complete: None,
                drop: submit::drop_open,
                get_fd: submit::get_fd_open,
            };
        }

        let payload = OpenPayload { op: self };
        IocpOp {
            header: OverlappedEntry::new(0),
            vtable: &VTableHolder::<P>::TABLE,
            payload: IocpOpPayload {
                open: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: IocpOp<P>) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.open)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<IocpDriver<P>> for Wakeup {
    fn into_platform_op(self) -> IocpOp<P> {
        struct VTableHolder<P: BufPool>(P);
        impl<P: BufPool> VTableHolder<P> {
            const TABLE: OpVTable<P> = OpVTable {
                submit: submit::submit_wakeup,
                on_complete: None,
                drop: submit::drop_wakeup,
                get_fd: submit::get_fd_wakeup,
            };
        }

        let payload = WakeupPayload { op: self };
        IocpOp {
            header: OverlappedEntry::new(0),
            vtable: &VTableHolder::<P>::TABLE,
            payload: IocpOpPayload {
                wakeup: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: IocpOp<P>) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.wakeup)).op }
    }
}
