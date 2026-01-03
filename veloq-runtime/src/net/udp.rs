use crate::io::buffer::{BufPool, FixedBuf};
use crate::io::driver::PlatformDriver;
use crate::io::op::{Connect, IoFd, Op, RawHandle, Recv, RecvFrom, Send, SendTo};
use crate::io::socket::Socket;
use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Weak;

pub struct UdpSocket<P: BufPool> {
    fd: RawHandle,
    driver: Weak<RefCell<PlatformDriver<P>>>,
}

impl<P: BufPool> Drop for UdpSocket<P> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { Socket::from_raw(self.fd as i32) };
        #[cfg(windows)]
        let _ = unsafe { Socket::from_raw(self.fd as *mut std::ffi::c_void) };
    }
}

impl<P: BufPool> UdpSocket<P> {
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        driver: Weak<RefCell<PlatformDriver<P>>>,
    ) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No address provided"))?;

        let socket = if addr.is_ipv4() {
            Socket::new_udp_v4()?
        } else {
            Socket::new_udp_v6()?
        };

        socket.bind(addr)?;

        Ok(Self {
            fd: socket.into_raw() as RawHandle,
            driver,
        })
    }

    pub async fn send_to(
        &self,
        buf: FixedBuf<P>,
        target: SocketAddr,
    ) -> (io::Result<usize>, FixedBuf<P>) {
        let op = SendTo {
            fd: IoFd::Raw(self.fd),
            buf,
            addr: target,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back): (io::Result<usize>, SendTo<P>) = future.await;
        (res, op_back.buf)
    }

    pub async fn recv_from(
        &self,
        buf: FixedBuf<P>,
    ) -> (io::Result<(usize, SocketAddr)>, FixedBuf<P>) {
        let op = RecvFrom {
            fd: IoFd::Raw(self.fd),
            buf,
            addr: None,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back): (io::Result<usize>, RecvFrom<P>) = future.await;

        match res {
            Ok(n) => {
                let addr = op_back.addr.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
                (Ok((n, addr)), op_back.buf)
            }
            Err(e) => (Err(e), op_back.buf),
        }
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

    pub async fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        let (raw_addr, raw_addr_len) = crate::io::socket::socket_addr_to_storage(addr);
        let op = Connect {
            fd: IoFd::Raw(self.fd),
            addr: raw_addr,
            addr_len: raw_addr_len as u32,
        };
        let driver = self.driver.clone();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
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

    pub async fn recv(&self, buf: FixedBuf<P>) -> (io::Result<usize>, FixedBuf<P>) {
        let op = Recv {
            fd: IoFd::Raw(self.fd),
            buf,
        };
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }
}

impl<P: BufPool> crate::io::AsyncBufRead<P> for UdpSocket<P> {
    fn read(
        &self,
        buf: FixedBuf<P>,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf<P>)> {
        self.recv(buf)
    }
}

impl<P: BufPool> crate::io::AsyncBufWrite<P> for UdpSocket<P> {
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
