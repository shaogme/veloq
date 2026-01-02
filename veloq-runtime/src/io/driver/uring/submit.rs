//! io_uring Operation Submission Implementations
//!
//! This module implements the `UringSubmit` trait for all operation types,
//! providing the logic to generate SQEs and handle completions.

use crate::io::op::{
    Accept, Close, Connect, Fallocate, Fsync, IoFd, ReadFixed, Recv, Send, SyncFileRange,
    WriteFixed,
};
use io_uring::{opcode, squeue, types};
use std::io;

use super::op::UringOp;

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

use crate::io::buffer::{BufPool, NO_REGISTRATION_INDEX};

impl<P: BufPool> UringSubmit for ReadFixed<P> {
    fn make_sqe(&mut self) -> squeue::Entry {
        let buf_index = self.buf.buf_index();
        if buf_index == NO_REGISTRATION_INDEX {
            match self.fd {
                IoFd::Raw(fd) => opcode::Read::new(
                    types::Fd(fd as i32),
                    self.buf.as_mut_ptr(),
                    self.buf.capacity() as u32,
                )
                .offset(self.offset)
                .build(),
                IoFd::Fixed(idx) => opcode::Read::new(
                    types::Fixed(idx),
                    self.buf.as_mut_ptr(),
                    self.buf.capacity() as u32,
                )
                .offset(self.offset)
                .build(),
            }
        } else {
            match self.fd {
                IoFd::Raw(fd) => opcode::ReadFixed::new(
                    types::Fd(fd as i32),
                    self.buf.as_mut_ptr(),
                    self.buf.capacity() as u32,
                    buf_index,
                )
                .offset(self.offset)
                .build(),
                IoFd::Fixed(idx) => opcode::ReadFixed::new(
                    types::Fixed(idx),
                    self.buf.as_mut_ptr(),
                    self.buf.capacity() as u32,
                    buf_index,
                )
                .offset(self.offset)
                .build(),
            }
        }
    }
}

impl<P: BufPool> UringSubmit for WriteFixed<P> {
    fn make_sqe(&mut self) -> squeue::Entry {
        let buf_index = self.buf.buf_index();
        if buf_index == NO_REGISTRATION_INDEX {
            match self.fd {
                IoFd::Raw(fd) => opcode::Write::new(
                    types::Fd(fd as i32),
                    self.buf.as_slice().as_ptr(),
                    self.buf.len() as u32,
                )
                .offset(self.offset)
                .build(),
                IoFd::Fixed(idx) => opcode::Write::new(
                    types::Fixed(idx),
                    self.buf.as_slice().as_ptr(),
                    self.buf.len() as u32,
                )
                .offset(self.offset)
                .build(),
            }
        } else {
            match self.fd {
                IoFd::Raw(fd) => opcode::WriteFixed::new(
                    types::Fd(fd as i32),
                    self.buf.as_slice().as_ptr(),
                    self.buf.len() as u32,
                    buf_index,
                )
                .offset(self.offset)
                .build(),
                IoFd::Fixed(idx) => opcode::WriteFixed::new(
                    types::Fixed(idx),
                    self.buf.as_slice().as_ptr(),
                    self.buf.len() as u32,
                    buf_index,
                )
                .offset(self.offset)
                .build(),
            }
        }
    }
}

impl<P: BufPool> UringSubmit for Recv<P> {
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

impl<P: BufPool> UringSubmit for Send<P> {
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

impl UringSubmit for Accept {
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

impl UringSubmit for SyncFileRange {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::SyncFileRange::new(types::Fd(fd as i32), self.nbytes as u32)
                .offset(self.offset)
                .flags(self.flags)
                .build(),
            IoFd::Fixed(idx) => opcode::SyncFileRange::new(types::Fixed(idx), self.nbytes as u32)
                .offset(self.offset)
                .flags(self.flags)
                .build(),
        }
    }
}

impl UringSubmit for Fallocate {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            IoFd::Raw(fd) => opcode::Fallocate::new(types::Fd(fd as i32), self.len as u64)
                .offset(self.offset)
                .mode(self.mode)
                .build(),
            IoFd::Fixed(idx) => opcode::Fallocate::new(types::Fixed(idx), self.len as u64)
                .offset(self.offset)
                .mode(self.mode)
                .build(),
        }
    }
}

impl<P: BufPool> UringSubmit for UringOp<P> {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self {
            UringOp::ReadFixed(op, _) => op.make_sqe(),
            UringOp::WriteFixed(op, _) => op.make_sqe(),
            UringOp::Recv(op, _) => op.make_sqe(),
            UringOp::Send(op, _) => op.make_sqe(),
            UringOp::Connect(op, _) => op.make_sqe(),
            UringOp::Close(op, _) => op.make_sqe(),
            UringOp::Fsync(op, _) => op.make_sqe(),
            UringOp::SyncFileRange(op, _) => op.make_sqe(),
            UringOp::Fallocate(op, _) => op.make_sqe(),
            UringOp::Accept(op, _) => op.make_sqe(),

            UringOp::SendTo(op, extras) => {
                // Initialize internal pointers
                extras.iovec[0].iov_base = op.buf.as_slice().as_ptr() as *mut _;
                extras.iovec[0].iov_len = op.buf.len();

                extras.msghdr.msg_name = &mut extras.addr as *mut _ as *mut libc::c_void;
                extras.msghdr.msg_namelen = extras.addr_len;
                extras.msghdr.msg_iov = extras.iovec.as_mut_ptr();
                extras.msghdr.msg_iovlen = 1;
                // extras.msghdr.msg_control = std::ptr::null_mut(); // already zeroed in new()
                // extras.msghdr.msg_controllen = 0;

                match op.fd {
                    IoFd::Raw(fd) => {
                        opcode::SendMsg::new(types::Fd(fd as i32), &extras.msghdr as *const _)
                            .build()
                    }
                    IoFd::Fixed(idx) => {
                        opcode::SendMsg::new(types::Fixed(idx), &extras.msghdr as *const _).build()
                    }
                }
            }

            UringOp::RecvFrom(op, extras) => {
                // Initialize internal pointers
                extras.iovec[0].iov_base = op.buf.as_mut_ptr() as *mut _;
                extras.iovec[0].iov_len = op.buf.capacity();

                extras.msghdr.msg_name = &mut extras.addr as *mut _ as *mut libc::c_void;
                extras.msghdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
                extras.msghdr.msg_iov = extras.iovec.as_mut_ptr();
                extras.msghdr.msg_iovlen = 1;
                // extras.msghdr.msg_control = std::ptr::null_mut();
                // extras.msghdr.msg_controllen = 0;

                match op.fd {
                    IoFd::Raw(fd) => {
                        opcode::RecvMsg::new(types::Fd(fd as i32), &mut extras.msghdr as *mut _)
                            .build()
                    }
                    IoFd::Fixed(idx) => {
                        opcode::RecvMsg::new(types::Fixed(idx), &mut extras.msghdr as *mut _)
                            .build()
                    }
                }
            }

            UringOp::Open(op, extras) => {
                let path_ptr = extras.path.as_ptr();
                // OpenAt: dir_fd, path, flags, mode
                // We use AT_FDCWD for dir_fd to support absolute and relative paths from CWD.
                opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
                    .flags(op.flags)
                    .mode(op.mode)
                    .build()
            }

            UringOp::Timeout(op, extras) => {
                extras.ts[0] = op.duration.as_secs() as i64;
                extras.ts[1] = op.duration.subsec_nanos() as i64;
                let ts_ptr = extras.ts.as_ptr() as *const types::Timespec;
                opcode::Timeout::new(ts_ptr).build()
            }

            UringOp::Wakeup(op, extras) => {
                match op.fd {
                    IoFd::Raw(fd) => {
                        // We need a buffer to read into.
                        opcode::Read::new(types::Fd(fd as i32), extras.buf.as_mut_ptr(), 8).build()
                    }
                    _ => panic!("Wakeup only supports raw fd"),
                }
            }
        }
    }

    fn on_complete(&mut self, result: i32) -> io::Result<usize> {
        match self {
            UringOp::ReadFixed(op, _) => op.on_complete(result),
            UringOp::WriteFixed(op, _) => op.on_complete(result),
            UringOp::Recv(op, _) => op.on_complete(result),
            UringOp::Send(op, _) => op.on_complete(result),
            UringOp::Connect(op, _) => op.on_complete(result),
            UringOp::Close(op, _) => op.on_complete(result),
            UringOp::Fsync(op, _) => op.on_complete(result),
            UringOp::SyncFileRange(op, _) => op.on_complete(result),
            UringOp::Fallocate(op, _) => op.on_complete(result),
            UringOp::Accept(op, _) => op.on_complete(result),

            // Ops with custom logic if needed, otherwise default
            UringOp::SendTo(_, _) => {
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                }
            }
            UringOp::RecvFrom(_, _) => {
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                }
            }
            UringOp::Open(_, _) => {
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                }
            }
            UringOp::Wakeup(_, _) => {
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                }
            }
            UringOp::Timeout(_, _) => {
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    Err(io::Error::from_raw_os_error(-result))
                }
            }
        }
    }
}
