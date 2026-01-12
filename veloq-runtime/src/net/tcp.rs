use crate::io::buffer::FixedBuf;
use crate::io::driver::PlatformDriver;
use crate::io::op::{Accept, Connect, IoFd, Op, OpLifecycle, RawHandle, ReadFixed, WriteFixed};
use crate::io::socket::Socket;
use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Weak;

pub struct TcpListener {
    fd: RawHandle,
    driver: Weak<RefCell<PlatformDriver>>,
}

pub struct TcpStream {
    fd: RawHandle,
    driver: Weak<RefCell<PlatformDriver>>,
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { Socket::from_raw(self.fd) };
        #[cfg(windows)]
        let _ = unsafe { Socket::from_raw(self.fd) };
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { Socket::from_raw(self.fd) };
        #[cfg(windows)]
        let _ = unsafe { Socket::from_raw(self.fd) };
    }
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
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

        let driver = crate::runtime::context::current().driver();

        Ok(Self {
            fd: socket.into_raw() as RawHandle,
            driver,
        })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        // Pre-allocate resources (platform specific)
        #[cfg(target_os = "windows")]
        let pre_alloc = Accept::pre_alloc(self.fd)?;

        // Create the Op
        #[cfg(target_os = "windows")]
        let op = Accept::into_op(self.fd, pre_alloc);

        #[cfg(target_os = "linux")]
        let op = Accept::into_op(self.fd, ());

        // Submit and Await
        // Submit and Await
        let future = Op::new(op).submit_local(self.driver.clone());
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
        let socket = unsafe { ManuallyDrop::new(Socket::from_raw(self.fd)) };
        #[cfg(windows)]
        let socket = unsafe { ManuallyDrop::new(Socket::from_raw(self.fd)) };
        socket.local_addr()
    }
}

impl TcpStream {
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let socket = if addr.is_ipv4() {
            Socket::new_tcp_v4()?
        } else {
            Socket::new_tcp_v6()?
        };
        let fd = socket.into_raw() as RawHandle;

        let (raw_addr, raw_addr_len) = crate::io::socket::socket_addr_to_storage(addr);
        #[allow(clippy::unnecessary_cast)]
        let op = Connect {
            fd: IoFd::Raw(fd),
            addr: raw_addr,
            addr_len: raw_addr_len as u32,
        };

        let driver = crate::runtime::context::current().driver();
        let future = Op::new(op).submit_local(driver.clone());
        let (res, _op_back) = future.await;
        res?;

        Ok(Self { fd, driver })
    }

    pub async fn recv(&self, buf: FixedBuf) -> (io::Result<usize>, FixedBuf) {
        let op = ReadFixed {
            fd: IoFd::Raw(self.fd),
            buf,
            offset: 0,
        };
        let future = Op::new(op).submit_local(self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }

    pub async fn send(&self, buf: FixedBuf) -> (io::Result<usize>, FixedBuf) {
        let op = WriteFixed {
            fd: IoFd::Raw(self.fd),
            buf,
            offset: 0,
        };
        let future = Op::new(op).submit_local(self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }
}

impl crate::io::AsyncBufRead for TcpStream {
    fn read(
        &self,
        buf: FixedBuf,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf)> {
        self.recv(buf)
    }
}

impl crate::io::AsyncBufWrite for TcpStream {
    fn write(
        &self,
        buf: FixedBuf,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf)> {
        self.send(buf)
    }

    fn flush(&self) -> impl std::future::Future<Output = io::Result<()>> {
        std::future::ready(Ok(()))
    }

    fn shutdown(&self) -> impl std::future::Future<Output = io::Result<()>> {
        std::future::ready(Ok(()))
    }
}
