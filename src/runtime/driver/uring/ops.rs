use crate::runtime::op::{Accept, Connect, IoResources, ReadFixed, Recv, RecvFrom, Send, SendTo, Timeout, WriteFixed};
use io_uring::{opcode, squeue, types};

// Internal trait to generate SQEs
pub(crate) trait UringOp {
    fn make_sqe(&mut self) -> squeue::Entry;
}

impl UringOp for ReadFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::ReadFixed::new(
            types::Fd(self.fd),
            self.buf.as_mut_ptr(),
            self.buf.capacity() as u32,
            self.buf.buf_index(),
        )
        .offset(self.offset)
        .build()
    }
}

impl UringOp for WriteFixed {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::WriteFixed::new(
            types::Fd(self.fd),
            self.buf.as_slice().as_ptr(),
            self.buf.len() as u32,
            self.buf.buf_index(),
        )
        .offset(self.offset)
        .build()
    }
}

impl UringOp for IoResources {
    fn make_sqe(&mut self) -> squeue::Entry {
        match self {
            IoResources::ReadFixed(op) => op.make_sqe(),
            IoResources::WriteFixed(op) => op.make_sqe(),
            IoResources::Send(op) => op.make_sqe(),
            IoResources::Recv(op) => op.make_sqe(),
            IoResources::Timeout(op) => op.make_sqe(),
            IoResources::Accept(op) => op.make_sqe(),
            IoResources::Connect(op) => op.make_sqe(),
            IoResources::SendTo(op) => op.make_sqe(),
            IoResources::RecvFrom(op) => op.make_sqe(),
            IoResources::None => opcode::Nop::new().build(),
        }
    }
}

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
        opcode::Accept::new(
            types::Fd(self.fd),
            self.addr.as_mut_ptr() as *mut _,
            self.addr_len.as_mut() as *mut _,
        )
        .build()
    }
}

impl UringOp for Connect {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::Connect::new(
            types::Fd(self.fd),
            self.addr.as_ptr() as *const _,
            self.addr_len,
        )
        .build()
    }
}

impl UringOp for Recv {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::Recv::new(
            types::Fd(self.fd),
            self.buf.as_mut_ptr(),
            self.buf.capacity() as u32,
        )
        .build()
    }
}

impl UringOp for Send {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::Send::new(
            types::Fd(self.fd),
            self.buf.as_slice().as_ptr(),
            self.buf.len() as u32,
        )
        .build()
    }
}

impl UringOp for SendTo {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::SendMsg::new(types::Fd(self.fd), &*self.msghdr as *const _).build()
    }
}

impl UringOp for RecvFrom {
    fn make_sqe(&mut self) -> squeue::Entry {
        opcode::RecvMsg::new(types::Fd(self.fd), &mut *self.msghdr as *mut _).build()
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
        // Just verify it doesn't crash and returns something.
        // Inspecting raw sqe is hard without access to io_uring internals, 
        // but we can check if it constructed successfully.
        // For strict correctness we might need to cast to io_uring_sqe but that's unsafe and hidden.
        // Validating the side effect (self.ts populated)
        assert_eq!(op.ts[0], 1);
        assert_eq!(op.ts[1], 0);
    }
}
