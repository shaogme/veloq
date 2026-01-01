use super::{IoFd, IoOp, IoResources, OpLifecycle, SysRawOp};
use crate::io::buffer::FixedBuf;
use std::time::Duration;

pub struct Timeout {
    pub duration: Duration,
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
    pub addr: Box<[u8]>,
    pub addr_len: Box<u32>,
    pub accept_socket: SysRawOp,
    pub remote_addr: Option<std::net::SocketAddr>,
}

impl OpLifecycle for Accept {
    type PreAlloc = SysRawOp;
    type Output = (SysRawOp, std::net::SocketAddr);

    fn pre_alloc(_fd: SysRawOp) -> std::io::Result<Self::PreAlloc> {
        // FIXME: accurately detect family from _fd or generic
        // For now assuming IPv4 or relying on internal logic
        use crate::io::socket::Socket;
        Ok(Socket::new_tcp_v4()?.into_raw())
    }

    #[allow(unused_variables)]
    fn into_op(fd: SysRawOp, pre: Self::PreAlloc) -> Self {
        let buf_size = 288; // (sizeof(sockaddr_storage) + 16) * 2
        let addr_buf = vec![0u8; buf_size].into_boxed_slice();
        let addr_len = Box::new(buf_size as u32);

        Self {
            fd: IoFd::Raw(fd),
            addr: addr_buf,
            addr_len,
            accept_socket: pre,
            remote_addr: None,
        }
    }

    fn into_output(self, res: std::io::Result<u32>) -> std::io::Result<Self::Output> {
        res?;
        let fd = self.accept_socket;

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
    pub wsabuf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl SendTo {
    pub fn new(fd: SysRawOp, buf: FixedBuf, target: std::net::SocketAddr) -> Self {
        let (raw_addr, raw_addr_len) = crate::io::socket::socket_addr_trans(target);
        let addr = raw_addr.into_boxed_slice();
        let addr_len = raw_addr_len as u32;

        use windows_sys::Win32::Networking::WinSock::WSABUF;
        let wsabuf = Box::new(WSABUF {
            len: buf.len() as u32,
            buf: buf.as_slice().as_ptr() as *mut u8,
        });
        Self {
            fd: IoFd::Raw(fd),
            buf,
            addr,
            addr_len,
            wsabuf,
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
    pub addr_len: Box<i32>,
    pub flags: Box<u32>,
    pub wsabuf: Box<windows_sys::Win32::Networking::WinSock::WSABUF>,
}

impl RecvFrom {
    pub fn new(fd: SysRawOp, mut buf: FixedBuf) -> Self {
        let addr_buf_size = 128usize;
        let addr = vec![0u8; addr_buf_size].into_boxed_slice();

        use windows_sys::Win32::Networking::WinSock::WSABUF;
        let addr_len = Box::new(addr_buf_size as i32);
        let wsabuf = Box::new(WSABUF {
            len: buf.capacity() as u32,
            buf: buf.as_mut_ptr(),
        });
        let flags = Box::new(0u32);
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
        *self.addr_len as usize
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
