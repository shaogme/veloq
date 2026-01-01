use super::{IoFd, IoOp, IoResources, OpLifecycle, SysRawOp};
use crate::io::buffer::FixedBuf;
use std::time::Duration;

pub struct Timeout {
    pub duration: Duration,
    pub ts: [i64; 2],
}

impl IoOp for Timeout {
    fn into_resource(self) -> IoResources {
        IoResources::Timeout(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Timeout(r) => r,
            _ => panic!("Resource type mismatch for Timeout"),
        }
    }
}

pub struct Accept {
    pub fd: IoFd,
    /// Buffer to hold the address.
    pub addr: Box<[u8]>,
    pub addr_len: Box<u32>,
    pub remote_addr: Option<std::net::SocketAddr>,
}

impl OpLifecycle for Accept {
    type PreAlloc = ();
    type Output = (SysRawOp, std::net::SocketAddr);

    fn pre_alloc(_fd: SysRawOp) -> std::io::Result<Self::PreAlloc> {
        Ok(())
    }

    #[allow(unused_variables)]
    fn into_op(fd: SysRawOp, pre: Self::PreAlloc) -> Self {
        let buf_size = std::mem::size_of::<libc::sockaddr_storage>();
        let addr_buf = vec![0u8; buf_size].into_boxed_slice();
        let addr_len = Box::new(buf_size as u32);

        Self {
            fd: IoFd::Raw(fd),
            addr: addr_buf,
            addr_len,
            remote_addr: None,
        }
    }

    fn into_output(self, res: std::io::Result<usize>) -> std::io::Result<Self::Output> {
        let fd = res? as SysRawOp;
        use crate::io::socket::to_socket_addr;
        let addr = if let Some(a) = self.remote_addr {
            a
        } else {
            to_socket_addr(&self.addr).unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
        };

        Ok((fd, addr))
    }
}

impl IoOp for Accept {
    fn into_resource(self) -> IoResources {
        IoResources::Accept(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Accept(r) => r,
            _ => panic!("Resource type mismatch for Accept"),
        }
    }
}

pub struct SendTo {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: Box<[u8]>,
    pub addr_len: u32,
    pub msghdr: Box<libc::msghdr>,
    pub iovec: Box<libc::iovec>,
}

impl SendTo {
    pub fn new(fd: SysRawOp, buf: FixedBuf, target: std::net::SocketAddr) -> Self {
        let (raw_addr, raw_addr_len) = crate::io::socket::socket_addr_trans(target);
        let addr = raw_addr.into_boxed_slice();
        let addr_len = raw_addr_len as u32;

        let mut iovec = Box::new(libc::iovec {
            iov_base: buf.as_slice().as_ptr() as *mut _,
            iov_len: buf.len(),
        });
        let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msghdr.msg_name = addr.as_ptr() as *mut _;
        msghdr.msg_namelen = addr_len;
        msghdr.msg_iov = iovec.as_mut() as *mut _;
        msghdr.msg_iovlen = 1;

        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len,
            msghdr,
            iovec,
        }
    }
}

impl IoOp for SendTo {
    fn into_resource(self) -> IoResources {
        IoResources::SendTo(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::SendTo(r) => r,
            _ => panic!("Resource type mismatch for SendTo"),
        }
    }
}

pub struct RecvFrom {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub addr: Box<[u8]>,
    pub addr_len: Box<u32>,
    pub msghdr: Box<libc::msghdr>,
    pub iovec: Box<libc::iovec>,
}

impl RecvFrom {
    pub fn new(fd: SysRawOp, mut buf: FixedBuf) -> Self {
        let addr_buf_size = std::mem::size_of::<libc::sockaddr_storage>();
        let addr = vec![0u8; addr_buf_size].into_boxed_slice();
        let addr_len = Box::new(addr_buf_size as u32);

        let mut iovec = Box::new(libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.capacity(),
        });
        let mut msghdr: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msghdr.msg_name = addr.as_ptr() as *mut _;
        msghdr.msg_namelen = *addr_len;
        msghdr.msg_iov = iovec.as_mut() as *mut _;
        msghdr.msg_iovlen = 1;

        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len,
            msghdr,
            iovec,
        }
    }

    pub fn get_addr_len(&self) -> usize {
        self.msghdr.msg_namelen as usize
    }
}

impl IoOp for RecvFrom {
    fn into_resource(self) -> IoResources {
        IoResources::RecvFrom(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::RecvFrom(r) => r,
            _ => panic!("Resource type mismatch for RecvFrom"),
        }
    }
}

pub struct Wakeup {
    pub fd: IoFd,
    pub buf: Box<[u8; 8]>,
}

impl Wakeup {
    pub fn new(fd: SysRawOp) -> Self {
        Self {
            fd: IoFd::Raw(fd),
            buf: Box::new([0u8; 8]),
        }
    }
}

impl IoOp for Wakeup {
    fn into_resource(self) -> IoResources {
        IoResources::Wakeup(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Wakeup(r) => r,
            _ => panic!("Resource type mismatch for Wakeup"),
        }
    }
}
