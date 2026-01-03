//! io_uring Platform-Specific Operation Definitions
//!
//! This module defines:
//! - `UringOp`: The Type-Erased operation struct using Unions and VTables
//! - `OpVTable`: The virtual table for dynamic dispatch without enums
//! - `IntoPlatformOp` implementations using blind casting

use crate::io::buffer::BufPool;
use crate::io::driver::PlatformOp;
use crate::io::driver::uring::UringDriver;
use crate::io::driver::uring::submit;
use crate::io::op::{
    Accept, Close, Connect, Fallocate, Fsync, IntoPlatformOp, IoFd, Open, ReadFixed, Recv,
    RecvFrom, Send as OpSend, SendTo, SyncFileRange, Timeout, Wakeup, WriteFixed,
};
use io_uring::squeue;
use std::io;
use std::mem::ManuallyDrop;

// ============================================================================
// VTable Definition
// ============================================================================

pub type MakeSqeFn = unsafe fn(op: &mut UringOp) -> squeue::Entry;
pub type OnCompleteFn = unsafe fn(op: &mut UringOp, result: i32) -> io::Result<usize>;
pub type DropFn = unsafe fn(op: &mut UringOp);
pub type GetFdFn = unsafe fn(op: &UringOp) -> Option<IoFd>;

pub struct OpVTable {
    pub make_sqe: MakeSqeFn,
    pub on_complete: OnCompleteFn,
    pub drop: DropFn,
    pub get_fd: GetFdFn,
}

// ============================================================================
// UringOp Struct & Union (Type-Erased)
// ============================================================================

#[repr(C)]
pub struct UringOp {
    /// Virtual Table for dynamic dispatch
    pub vtable: &'static OpVTable,

    /// Type-erased payload
    pub payload: UringOpPayload,
}

impl PlatformOp for UringOp {}

impl UringOp {
    pub fn get_fd(&self) -> Option<IoFd> {
        unsafe { (self.vtable.get_fd)(self) }
    }
}

impl Drop for UringOp {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop)(self) };
    }
}

// Ensure proper alignment
#[repr(C)]
pub union UringOpPayload {
    pub read: ManuallyDrop<ReadFixed>,
    pub write: ManuallyDrop<WriteFixed>,
    pub recv: ManuallyDrop<Recv>,
    pub send: ManuallyDrop<OpSend>,
    pub connect: ManuallyDrop<Connect>,
    pub accept: ManuallyDrop<Accept>,
    pub send_to: ManuallyDrop<SendToPayload>,
    pub recv_from: ManuallyDrop<RecvFromPayload>,
    pub open: ManuallyDrop<OpenPayload>,
    pub close: ManuallyDrop<Close>,
    pub fsync: ManuallyDrop<Fsync>,
    pub sync_range: ManuallyDrop<SyncFileRange>,
    pub fallocate: ManuallyDrop<Fallocate>,
    pub wakeup: ManuallyDrop<WakeupPayload>,
    pub timeout: ManuallyDrop<TimeoutPayload>,
}

// ============================================================================
// Payload Structures for Complex Ops
// ============================================================================

pub struct SendToPayload {
    pub op: SendTo,
    pub msg_name: libc::sockaddr_storage,
    pub msg_namelen: libc::socklen_t,
    pub iovec: [libc::iovec; 1],
    pub msghdr: libc::msghdr,
}

pub struct RecvFromPayload {
    pub op: RecvFrom,
    pub msg_name: libc::sockaddr_storage,
    pub msg_namelen: libc::socklen_t,
    pub iovec: [libc::iovec; 1],
    pub msghdr: libc::msghdr,
}

pub struct OpenPayload {
    pub op: Open,
}

pub struct WakeupPayload {
    pub op: Wakeup,
    pub buf: [u8; 8],
}

pub struct TimeoutPayload {
    pub op: Timeout,
    pub ts: [i64; 2],
}

// ============================================================================
// IntoPlatformOp Implementations
// ============================================================================

macro_rules! impl_into_uring_op {
    ($Type:ident, $Field:ident, $MakeSqe:ident, $OnComplete:ident, $Drop:ident, $GetFd:ident) => {
        impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for $Type {
            fn into_platform_op(self) -> UringOp {
                const TABLE: OpVTable = OpVTable {
                    make_sqe: submit::$MakeSqe,
                    on_complete: submit::$OnComplete,
                    drop: submit::$Drop,
                    get_fd: submit::$GetFd,
                };

                UringOp {
                    vtable: &TABLE,
                    payload: UringOpPayload {
                        $Field: ManuallyDrop::new(self),
                    },
                }
            }
            fn from_platform_op(op: UringOp) -> Self {
                let op = ManuallyDrop::new(op);
                unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.$Field)) }
            }
        }
    };
}

impl_into_uring_op!(
    ReadFixed,
    read,
    make_sqe_read_fixed,
    on_complete_read_fixed,
    drop_read_fixed,
    get_fd_read_fixed
);
impl_into_uring_op!(
    WriteFixed,
    write,
    make_sqe_write_fixed,
    on_complete_write_fixed,
    drop_write_fixed,
    get_fd_write_fixed
);
impl_into_uring_op!(Recv, recv, make_sqe_recv, on_complete_recv, drop_recv, get_fd_recv);
impl_into_uring_op!(OpSend, send, make_sqe_send, on_complete_send, drop_send, get_fd_send);

impl_into_uring_op!(
    Connect,
    connect,
    make_sqe_connect,
    on_complete_connect,
    drop_connect,
    get_fd_connect
);
impl_into_uring_op!(Close, close, make_sqe_close, on_complete_close, drop_close, get_fd_close);
impl_into_uring_op!(Fsync, fsync, make_sqe_fsync, on_complete_fsync, drop_fsync, get_fd_fsync);
impl_into_uring_op!(
    SyncFileRange,
    sync_range,
    make_sqe_sync_range,
    on_complete_sync_range,
    drop_sync_range,
    get_fd_sync_range
);
impl_into_uring_op!(
    Fallocate,
    fallocate,
    make_sqe_fallocate,
    on_complete_fallocate,
    drop_fallocate,
    get_fd_fallocate
);
impl_into_uring_op!(
    Accept,
    accept,
    make_sqe_accept,
    on_complete_accept,
    drop_accept,
    get_fd_accept
);

// Manual implementations for ops with extras

impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for SendTo {
    fn into_platform_op(self) -> UringOp {
        const TABLE: OpVTable = OpVTable {
            make_sqe: submit::make_sqe_send_to,
            on_complete: submit::on_complete_send_to,
            drop: submit::drop_send_to,
            get_fd: submit::get_fd_send_to,
        };

        let (msg_name, msg_namelen) = crate::io::socket::socket_addr_to_storage(self.addr);
        let payload = SendToPayload {
            op: self,
            msg_name,
            msg_namelen: msg_namelen as libc::socklen_t,
            iovec: [unsafe { std::mem::zeroed() }],
            msghdr: unsafe { std::mem::zeroed() },
        };

        UringOp {
            vtable: &TABLE,
            payload: UringOpPayload {
                send_to: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: UringOp) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.send_to)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for RecvFrom {
    fn into_platform_op(self) -> UringOp {
        const TABLE: OpVTable = OpVTable {
            make_sqe: submit::make_sqe_recv_from,
            on_complete: submit::on_complete_recv_from,
            drop: submit::drop_recv_from,
            get_fd: submit::get_fd_recv_from,
        };

        let payload = RecvFromPayload {
            op: self,
            msg_name: unsafe { std::mem::zeroed() },
            msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            iovec: [unsafe { std::mem::zeroed() }],
            msghdr: unsafe { std::mem::zeroed() },
        };

        UringOp {
            vtable: &TABLE,
            payload: UringOpPayload {
                recv_from: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: UringOp) -> Self {
        let op = ManuallyDrop::new(op);
        let payload = unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.recv_from)) };
        payload.op
    }
}

impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for Open {
    fn into_platform_op(self) -> UringOp {
        const TABLE: OpVTable = OpVTable {
            make_sqe: submit::make_sqe_open,
            on_complete: submit::on_complete_open,
            drop: submit::drop_open,
            get_fd: submit::get_fd_open,
        };

        let payload = OpenPayload { op: self };
        UringOp {
            vtable: &TABLE,
            payload: UringOpPayload {
                open: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: UringOp) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.open)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for Wakeup {
    fn into_platform_op(self) -> UringOp {
        const TABLE: OpVTable = OpVTable {
            make_sqe: submit::make_sqe_wakeup,
            on_complete: submit::on_complete_wakeup,
            drop: submit::drop_wakeup,
            get_fd: submit::get_fd_wakeup,
        };

        let payload = WakeupPayload {
            op: self,
            buf: [0; 8],
        };
        UringOp {
            vtable: &TABLE,
            payload: UringOpPayload {
                wakeup: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: UringOp) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.wakeup)).op }
    }
}

impl<P: BufPool> IntoPlatformOp<UringDriver<P>> for Timeout {
    fn into_platform_op(self) -> UringOp {
        const TABLE: OpVTable = OpVTable {
            make_sqe: submit::make_sqe_timeout,
            on_complete: submit::on_complete_timeout,
            drop: submit::drop_timeout,
            get_fd: submit::get_fd_timeout,
        };

        // We can just initialize ts with zeros, will be filled in make_sqe
        let payload = TimeoutPayload {
            op: self,
            ts: [0; 2],
        };
        UringOp {
            vtable: &TABLE,
            payload: UringOpPayload {
                timeout: ManuallyDrop::new(payload),
            },
        }
    }
    fn from_platform_op(op: UringOp) -> Self {
        let op = ManuallyDrop::new(op);
        unsafe { ManuallyDrop::into_inner(std::ptr::read(&op.payload.timeout)).op }
    }
}
