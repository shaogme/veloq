use crate::io::op::{
    Accept, Connect, IoResources, ReadFixed, Recv, RecvFrom, Send, SendTo, Timeout, Wakeup,
    WriteFixed,
};
use io_uring::{opcode, squeue, types};

// Internal trait to generate SQEs
pub(crate) trait UringOp {
    fn make_sqe(&mut self) -> squeue::Entry;

    fn on_complete(&mut self, result: i32) -> std::io::Result<usize> {
        if result >= 0 {
            Ok(result as usize)
        } else {
            Err(std::io::Error::from_raw_os_error(-result))
        }
    }
}

impl UringOp for ReadFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::ReadFixed::new(
                types::Fd(fd),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
            crate::io::op::IoFd::Fixed(idx) => opcode::ReadFixed::new(
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

impl UringOp for WriteFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::WriteFixed::new(
                types::Fd(fd),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
                self.buf.buf_index(),
            )
            .offset(self.offset)
            .build(),
            crate::io::op::IoFd::Fixed(idx) => opcode::WriteFixed::new(
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

macro_rules! impl_uring_op {
    (
        WithFd { $( $(#[$meta_fd:meta])* $VariantFd:ident($InnerFd:ty) ),* $(,)? },
        WithoutFd { $( $(#[$meta_no_fd:meta])* $VariantNoFd:ident($InnerNoFd:ty) ),* $(,)? }
    ) => {
        impl UringOp for IoResources {
            fn make_sqe(&mut self) -> squeue::Entry {
                match self {
                    $( $(#[$meta_fd])* IoResources::$VariantFd(op) => op.make_sqe(),)*
                    $( $(#[$meta_no_fd])* IoResources::$VariantNoFd(op) => op.make_sqe(),)*
                    IoResources::None => opcode::Nop::new().build(),
                }
            }

            fn on_complete(&mut self, result: i32) -> std::io::Result<usize> {
                match self {
                    $( $(#[$meta_fd])* IoResources::$VariantFd(op) => op.on_complete(result),)*
                    $( $(#[$meta_no_fd])* IoResources::$VariantNoFd(op) => op.on_complete(result),)*
                    IoResources::None => {
                        if result >= 0 {
                            Ok(result as usize)
                        } else {
                            Err(std::io::Error::from_raw_os_error(-result))
                        }
                    }
                }
            }
        }
    }
}

veloq_macros::for_all_io_ops!(impl_uring_op);

impl UringOp for Timeout {
    fn make_sqe(&mut self) -> squeue::Entry {
        self.ts[0] = self.duration.as_secs() as i64;
        self.ts[1] = self.duration.subsec_nanos() as i64;
        let ts_ptr = self.ts.as_ptr() as *const types::Timespec;
        opcode::Timeout::new(ts_ptr).build()
    }
}

impl UringOp for Accept {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::Accept::new(
                types::Fd(fd),
                self.addr.as_mut_ptr() as *mut _,
                self.addr_len.as_mut() as *mut _,
            )
            .build(),
            crate::io::op::IoFd::Fixed(idx) => opcode::Accept::new(
                types::Fixed(idx),
                self.addr.as_mut_ptr() as *mut _,
                self.addr_len.as_mut() as *mut _,
            )
            .build(),
        }
    }

    fn on_complete(&mut self, result: i32) -> std::io::Result<usize> {
        if result >= 0 {
            // Try fallback parsing to populate remote_addr early
            if let Ok(addr) = crate::io::socket::to_socket_addr(&self.addr) {
                self.remote_addr = Some(addr);
            }
            Ok(result as usize)
        } else {
            Err(std::io::Error::from_raw_os_error(-result))
        }
    }
}

impl UringOp for Connect {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => {
                opcode::Connect::new(types::Fd(fd), self.addr.as_ptr() as *const _, self.addr_len)
                    .build()
            }
            crate::io::op::IoFd::Fixed(idx) => opcode::Connect::new(
                types::Fixed(idx),
                self.addr.as_ptr() as *const _,
                self.addr_len,
            )
            .build(),
        }
    }
}

impl UringOp for Recv {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::Recv::new(
                types::Fd(fd),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
            )
            .build(),
            crate::io::op::IoFd::Fixed(idx) => opcode::Recv::new(
                types::Fixed(idx),
                self.buf.as_mut_ptr(),
                self.buf.capacity() as u32,
            )
            .build(),
        }
    }
}

impl UringOp for Send {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::Send::new(
                types::Fd(fd),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
            )
            .build(),
            crate::io::op::IoFd::Fixed(idx) => opcode::Send::new(
                types::Fixed(idx),
                self.buf.as_slice().as_ptr(),
                self.buf.len() as u32,
            )
            .build(),
        }
    }
}

impl UringOp for SendTo {
    fn make_sqe(&mut self) -> squeue::Entry {
        // SendMsg does not support Fixed File in older kernels/wrapper commonly.
        // But let's check if helper supports it.
        // If not, we might need to error or use Fd(idx) + FIXED_FILE flag manually.
        // For now, let's assume types::Fixed works if the crate is up to date.
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => {
                opcode::SendMsg::new(types::Fd(fd), &*self.msghdr as *const _).build()
            }
            crate::io::op::IoFd::Fixed(idx) => {
                opcode::SendMsg::new(types::Fixed(idx), &*self.msghdr as *const _).build()
            }
        }
    }
}

impl UringOp for RecvFrom {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => {
                opcode::RecvMsg::new(types::Fd(fd), &mut *self.msghdr as *mut _).build()
            }
            crate::io::op::IoFd::Fixed(idx) => {
                opcode::RecvMsg::new(types::Fixed(idx), &mut *self.msghdr as *mut _).build()
            }
        }
    }
}

impl UringOp for Wakeup {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => {
                opcode::Read::new(types::Fd(fd), self.buf.as_mut_ptr(), 8).build()
            }
            _ => panic!("Wakeup only supports raw fd"),
        }
    }
}

impl UringOp for crate::io::op::Open {
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

impl UringOp for crate::io::op::Close {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::Close::new(types::Fd(fd)).build(),
            crate::io::op::IoFd::Fixed(idx) => {
                // Assume Raw for now as File usually holds Raw Fd.
                opcode::Close::new(types::Fixed(idx)).build()
            }
        }
    }
}

impl UringOp for crate::io::op::Fsync {
    fn make_sqe(&mut self) -> squeue::Entry {
        let flags = if self.datasync {
            // IORING_FSYNC_DATASYNC
            io_uring::types::FsyncFlags::DATASYNC
        } else {
            io_uring::types::FsyncFlags::empty()
        };

        match self.fd {
            crate::io::op::IoFd::Raw(fd) => opcode::Fsync::new(types::Fd(fd)).flags(flags).build(),
            crate::io::op::IoFd::Fixed(idx) => {
                opcode::Fsync::new(types::Fixed(idx)).flags(flags).build()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_sqe() {
        let duration = std::time::Duration::from_secs(1);
        let mut op = Timeout {
            duration,
            ts: [0, 0],
        };
        let _sqe = op.make_sqe();
        assert_eq!(op.ts[0], 1);
        assert_eq!(op.ts[1], 0);
    }
}
