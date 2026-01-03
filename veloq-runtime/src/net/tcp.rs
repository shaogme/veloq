use crate::io::buffer::{BufPool, FixedBuf};
use crate::io::driver::PlatformDriver;
use crate::io::op::{Accept, Connect, IoFd, Op, OpLifecycle, RawHandle, Recv, Send};
use crate::io::socket::Socket;
use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Weak;

pub struct TcpListener<P: BufPool> {
    fd: RawHandle,
    driver: Weak<RefCell<PlatformDriver<P>>>,
}

pub struct TcpStream<P: BufPool> {
    fd: RawHandle,
    driver: Weak<RefCell<PlatformDriver<P>>>,
}

impl<P: BufPool> Drop for TcpListener<P> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { Socket::from_raw(self.fd as i32) };
        #[cfg(windows)]
        let _ = unsafe { Socket::from_raw(self.fd as *mut std::ffi::c_void) };
    }
}

impl<P: BufPool> Drop for TcpStream<P> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { Socket::from_raw(self.fd as i32) };
        #[cfg(windows)]
        let _ = unsafe { Socket::from_raw(self.fd as *mut std::ffi::c_void) };
    }
}

impl<P: BufPool> TcpListener<P> {
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        driver: Weak<RefCell<PlatformDriver<P>>>,
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
            fd: socket.into_raw() as RawHandle,
            driver,
        })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream<P>, SocketAddr)> {
        // Pre-allocate resources (platform specific)
        let pre_alloc = Accept::pre_alloc(self.fd)?;

        // Create the Op
        let op = Accept::into_op(self.fd, pre_alloc);

        // Submit and Await
        let future = Op::new(op, self.driver.clone());
        let (res, op_back): (io::Result<usize>, Accept) = future.await;

        // Post-process to get output
        let (fd, addr) = op_back.into_output(res)?;

        let stream = TcpStream {
            fd,
            driver: self.driver.clone(),
        };

        Ok((stream, addr))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        use std::mem::ManuallyDrop;

        #[cfg(unix)]
        let socket = unsafe { ManuallyDrop::new(Socket::from_raw(self.fd as i32)) };
        #[cfg(windows)]
        let socket =
            unsafe { ManuallyDrop::new(Socket::from_raw(self.fd as *mut std::ffi::c_void)) };
        socket.local_addr()
    }
}

impl<P: BufPool> TcpStream<P> {
    pub async fn connect(
        addr: SocketAddr,
        driver: Weak<RefCell<PlatformDriver<P>>>,
    ) -> io::Result<Self> {
        let socket = if addr.is_ipv4() {
            Socket::new_tcp_v4()?
        } else {
            Socket::new_tcp_v6()?
        };
        let fd = socket.into_raw() as RawHandle;

        let (raw_addr, raw_addr_len) = crate::io::socket::socket_addr_to_storage(addr);
        let op = Connect {
            fd: IoFd::Raw(fd),
            addr: raw_addr,
            addr_len: raw_addr_len as u32,
        };

        let future = Op::new(op, driver.clone());
        let (res, _op_back) = future.await;
        res?;

        Ok(Self { fd, driver })
    }

    pub async fn recv(&self, buf: FixedBuf<P>) -> (io::Result<usize>, FixedBuf<P>) {
        let op = Recv {
            fd: IoFd::Raw(self.fd),
            buf,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }

    pub async fn send(&self, buf: FixedBuf<P>) -> (io::Result<usize>, FixedBuf<P>) {
        let op = Send {
            fd: IoFd::Raw(self.fd),
            buf,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }
}

impl<P: BufPool> crate::io::AsyncBufRead<P> for TcpStream<P> {
    fn read(
        &self,
        buf: FixedBuf<P>,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf<P>)> {
        self.recv(buf)
    }
}

impl<P: BufPool> crate::io::AsyncBufWrite<P> for TcpStream<P> {
    fn write(
        &self,
        buf: FixedBuf<P>,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf<P>)> {
        self.send(buf)
    }

    fn flush(&self) -> impl std::future::Future<Output = io::Result<()>> {
        std::future::ready(Ok(()))
    }

    fn shutdown(&self) -> impl std::future::Future<Output = io::Result<()>> {
        std::future::ready(Ok(()))
    }
}
