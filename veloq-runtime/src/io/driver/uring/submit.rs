//! io_uring Operation Submission Implementations
//!
//! This module implements the `UringSubmit` trait for all operation types,
//! providing the logic to generate SQEs and handle completions.

use crate::io::op::{Close, Connect, Fsync, IoFd, ReadFixed, Recv, Send, WriteFixed};
use io_uring::{opcode, squeue, types};
use std::io;

use super::op::{
    UringAccept, UringOp, UringOpen, UringRecvFrom, UringSendTo, UringTimeout, UringWakeup,
};

/// io_uring operation submission trait.
/// Each operation type implements this to generate SQEs and handle completion events.
pub(crate) trait UringSubmit {
    /// Generate a Submission Queue Entry (SQE) for this operation.
    fn make_sqe(&mut self) -> squeue::Entry;

    /// Handle completion event, converting kernel result to io::Result.
    fn on_complete(&mut self, result: i32) -> io::Result<usize> {
        if result >= 0 {
            Ok(result as usize)
        } else {
            Err(io::Error::from_raw_os_error(-result))
        }
    }
}

// ============================================================================
// Direct Operations (Cross-Platform Structs)
// ============================================================================

impl UringSubmit for ReadFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::ReadFixed::new(
                types::Fd(fd as i32),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
            IoFd::Fixed(idx) => opcode::ReadFixed::new(
                types::Fixed(idx),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
        }
    }
}

impl UringSubmit for WriteFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::WriteFixed::new(
                types::Fd(fd as i32),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
            IoFd::Fixed(idx) => opcode::WriteFixed::new(
                types::Fixed(idx),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
        }
    }
}

impl UringSubmit for Recv {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Recv::new(
                types::Fd(fd as i32),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
            )
            .build(),
            IoFd::Fixed(idx) => opcode::Recv::new(
                types::Fixed(idx),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
            )
            .build(),
        }
    }
}

impl UringSubmit for Send {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Send::new(
                types::Fd(fd as i32),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
            )
            .build(),
            IoFd::Fixed(idx) => opcode::Send::new(
                types::Fixed(idx),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
            )
            .build(),
        }
    }
}

impl UringSubmit for Connect {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Connect::new(
                types::Fd(fd as i32),
                &self.addr as *const _ as *const _,
                self.addr_len,
            )
            .build(),
            IoFd::Fixed(idx) => opcode::Connect::new(
                types::Fixed(idx),
                &self.addr as *const _ as *const _,
                self.addr_len,
            )
            .build(),
        }
    }
}

impl UringSubmit for Close {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Close::new(types::Fd(fd as i32)).build(),
            IoFd::Fixed(idx) => opcode::Close::new(types::Fixed(idx)).build(),
        }
    }
}

impl UringSubmit for Fsync {
    fn make_sqe(&mut self) -> squeue::Entry {
        let flags = if self.datasync {
            io_uring::types::FsyncFlags::DATASYNC
        } else {
            io_uring::types::FsyncFlags::empty()
        };

        match self.fd {
            IoFd::Raw(fd) => opcode::Fsync::new(types::Fd(fd as i32))
                .flags(flags)
                .build(),
            IoFd::Fixed(idx) => opcode::Fsync::new(types::Fixed(idx)).flags(flags).build(),
        }
    }
}

// ============================================================================
// Platform-Specific Operations (Uring* Structs)
// ============================================================================

impl UringSubmit for UringAccept {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Accept::new(
                types::Fd(fd as i32),
                &mut self.addr as *mut _ as *mut _,
                &mut self.addr_len as *mut _,
            )
            .build(),
            IoFd::Fixed(idx) => opcode::Accept::new(
                types::Fixed(idx),
                &mut self.addr as *mut _ as *mut _,
                &mut self.addr_len as *mut _,
            )
            .build(),
        }
    }

    fn on_complete(&mut self, result: i32) -> io::Result<usize> {
        if result >= 0 {
            // Try fallback parsing to populate remote_addr early
            // Cast sockaddr_storage to &[u8] for the helper
            let addr_bytes = unsafe {
                std::slice::from_raw_parts(
                    &self.addr as *const _ as *const u8,
                    self.addr_len as usize,
                )
            };
            if let Ok(addr) = crate::io::socket::to_socket_addr(addr_bytes) {
                self.remote_addr = Some(addr);
            }
            Ok(result as usize)
        } else {
            Err(io::Error::from_raw_os_error(-result))
        }
    }
}

impl UringSubmit for UringSendTo {
    fn make_sqe(&mut self) -> squeue::Entry {
        // Initialize internal pointers
        self.iovec[0].iov_base = self.buf.as_slice().as_ptr() as *mut _;
        self.iovec[0].iov_len = self.buf.len();

        self.msghdr.msg_name = &mut self.addr as *mut _ as *mut libc::c_void;
        self.msghdr.msg_namelen = self.addr_len;
        self.msghdr.msg_iov = self.iovec.as_mut_ptr();
        self.msghdr.msg_iovlen = 1;
        self.msghdr.msg_control = std::ptr::null_mut();
        self.msghdr.msg_controllen = 0;

        match self.fd {
            IoFd::Raw(fd) => {
                opcode::SendMsg::new(types::Fd(fd as i32), &self.msghdr as *const _).build()
            }
            IoFd::Fixed(idx) => {
                opcode::SendMsg::new(types::Fixed(idx), &self.msghdr as *const _).build()
            }
        }
    }
}

impl UringSubmit for UringRecvFrom {
    fn make_sqe(&mut self) -> squeue::Entry {
        // Initialize internal pointers
        self.iovec[0].iov_base = self.buf.as_mut_ptr() as *mut _;
        self.iovec[0].iov_len = self.buf.capacity();

        self.msghdr.msg_name = &mut self.addr as *mut _ as *mut libc::c_void;
        self.msghdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
        self.msghdr.msg_iov = self.iovec.as_mut_ptr();
        self.msghdr.msg_iovlen = 1;
        self.msghdr.msg_control = std::ptr::null_mut();
        self.msghdr.msg_controllen = 0;

        match self.fd {
            IoFd::Raw(fd) => {
                opcode::RecvMsg::new(types::Fd(fd as i32), &mut self.msghdr as *mut _).build()
            }
            IoFd::Fixed(idx) => {
                opcode::RecvMsg::new(types::Fixed(idx), &mut self.msghdr as *mut _).build()
            }
        }
    }
}

impl UringSubmit for UringOpen {
    fn make_sqe(&mut self) -> squeue::Entry {
        let path_ptr = self.path.as_ptr();
        // OpenAt: dir_fd, path, flags, mode
        // We use AT_FDCWD for dir_fd to support absolute and relative paths from CWD.
        opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
            .flags(self.flags)
            .mode(self.mode)
            .build()
    }
}

impl UringSubmit for UringTimeout {
    fn make_sqe(&mut self) -> squeue::Entry {
        self.ts[0] = self.duration.as_secs() as i64;
        self.ts[1] = self.duration.subsec_nanos() as i64;
        let ts_ptr = self.ts.as_ptr() as *const types::Timespec;
        opcode::Timeout::new(ts_ptr).build()
    }
}

impl UringSubmit for UringWakeup {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => {
                opcode::Read::new(types::Fd(fd as i32), self.buf.as_mut_ptr(), 8).build()
            }
            _ => panic!("Wakeup only supports raw fd"),
        }
    }
}

// ============================================================================
// UringOp Enum Dispatch
// ============================================================================

impl UringSubmit for UringOp {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self {
            UringOp::ReadFixed(op) => op.make_sqe(),
            UringOp::WriteFixed(op) => op.make_sqe(),
            UringOp::Recv(op) => op.make_sqe(),
            UringOp::Send(op) => op.make_sqe(),
            UringOp::Accept(op) => op.make_sqe(),
            UringOp::Connect(op) => op.make_sqe(),
            UringOp::RecvFrom(op) => op.make_sqe(),
            UringOp::SendTo(op) => op.make_sqe(),
            UringOp::Open(op) => op.make_sqe(),
            UringOp::Close(op) => op.make_sqe(),
            UringOp::Fsync(op) => op.make_sqe(),
            UringOp::Wakeup(op) => op.make_sqe(),
            UringOp::Timeout(op) => op.make_sqe(),
        }
    }

    fn on_complete(&mut self, result: i32) -> io::Result<usize> {
        match self {
            UringOp::ReadFixed(op) => op.on_complete(result),
            UringOp::WriteFixed(op) => op.on_complete(result),
            UringOp::Recv(op) => op.on_complete(result),
            UringOp::Send(op) => op.on_complete(result),
            UringOp::Accept(op) => op.on_complete(result),
            UringOp::Connect(op) => op.on_complete(result),
            UringOp::RecvFrom(op) => op.on_complete(result),
            UringOp::SendTo(op) => op.on_complete(result),
            UringOp::Open(op) => op.on_complete(result),
            UringOp::Close(op) => op.on_complete(result),
            UringOp::Fsync(op) => op.on_complete(result),
            UringOp::Wakeup(op) => op.on_complete(result),
            UringOp::Timeout(op) => op.on_complete(result),
        }
    }
}

impl UringOp {
    /// Get the file descriptor associated with this operation (if any).
    pub fn get_fd(&self) -> Option<IoFd> {
        match self {
            UringOp::ReadFixed(op) => Some(op.fd),
            UringOp::WriteFixed(op) => Some(op.fd),
            UringOp::Recv(op) => Some(op.fd),
            UringOp::Send(op) => Some(op.fd),
            UringOp::Accept(op) => Some(op.fd),
            UringOp::Connect(op) => Some(op.fd),
            UringOp::RecvFrom(op) => Some(op.fd),
            UringOp::SendTo(op) => Some(op.fd),
            UringOp::Close(op) => Some(op.fd),
            UringOp::Fsync(op) => Some(op.fd),
            UringOp::Open(_) | UringOp::Wakeup(_) | UringOp::Timeout(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_sqe() {
        let duration = std::time::Duration::from_secs(1);
        let mut op = UringTimeout {
            duration,
            ts: [0, 0],
        };
        let _sqe = op.make_sqe();
        assert_eq!(op.ts[0], 1);
        assert_eq!(op.ts[1], 0);
    }
}
