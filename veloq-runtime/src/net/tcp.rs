use crate::io::buffer::FixedBuf;
use crate::io::driver::PlatformDriver;
use crate::io::op::{Accept, Connect, IoFd, Op, Recv, Send, SysRawOp};
use crate::io::sys::socket::Socket;
use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Weak;

pub struct TcpListener {
    fd: SysRawOp,
    driver: Weak<RefCell<PlatformDriver>>,
}

pub struct TcpStream {
    fd: SysRawOp,
    driver: Weak<RefCell<PlatformDriver>>,
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        use crate::io::sys::socket::Socket;
        let _ = unsafe { Socket::from_raw(self.fd) };
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        use crate::io::sys::socket::Socket;
        let _ = unsafe { Socket::from_raw(self.fd) };
    }
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        driver: Weak<RefCell<PlatformDriver>>,
    ) -> io::Result<Self> {
        // Resolve address (take first one)
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No address provided"))?;

        let socket = if addr.is_ipv4() {
            Socket::new_tcp_v4()?
        } else {
            Socket::new_tcp_v6()?
        };

        socket.bind(addr)?;
        socket.listen(1024)?; // backlog

        Ok(Self {
            fd: socket.into_raw(),
            driver,
        })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        use crate::io::op::OpLifecycle;

        // 1. Pre-allocate resources (platform specific)
        let pre_alloc = Accept::pre_alloc(self.fd)?;

        // 2. Create the Op
        // Use IoFd::Raw
        let op = Accept::into_op(self.fd, pre_alloc);

        // 3. Submit and Await
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;

        // 4. Post-process to get output
        let (fd, addr) = op_back.into_output(res)?;

        let stream = TcpStream {
            fd,
            driver: self.driver.clone(),
        };

        Ok((stream, addr))
    }
    
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        use crate::io::sys::socket::Socket;
        use std::mem::ManuallyDrop;

        let socket = unsafe { ManuallyDrop::new(Socket::from_raw(self.fd)) };
        socket.local_addr()
    }
}

impl TcpStream {
    pub async fn connect(
        addr: SocketAddr,
        driver: Weak<RefCell<PlatformDriver>>,
    ) -> io::Result<Self> {
        let socket = if addr.is_ipv4() {
            Socket::new_tcp_v4()?
        } else {
            Socket::new_tcp_v6()?
        };
        let fd = socket.into_raw();

        let (raw_addr, raw_addr_len) = crate::io::sys::socket::socket_addr_trans(addr);
        let op = Connect {
            fd: IoFd::Raw(fd),
            addr: raw_addr.into_boxed_slice(),
            addr_len: raw_addr_len as u32,
        };

        let future = Op::new(op, driver.clone());
        let (res, _op_back) = future.await;
        res?;

        Ok(Self { fd, driver })
    }

    pub async fn recv(&self, buf: FixedBuf) -> (io::Result<u32>, FixedBuf) {
        let op = Recv {
            fd: IoFd::Raw(self.fd),
            buf,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }

    pub async fn send(&self, buf: FixedBuf) -> (io::Result<u32>, FixedBuf) {
        let op = Send {
            fd: IoFd::Raw(self.fd),
            buf,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }
}
