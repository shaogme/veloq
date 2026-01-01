use crate::io::buffer::FixedBuf;
use crate::io::driver::PlatformDriver;
use crate::io::op::{Op, RecvFrom, SendTo, SysRawOp};
use crate::io::sys::socket::Socket;
use std::cell::RefCell;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Weak;

pub struct UdpSocket {
    fd: SysRawOp,
    driver: Weak<RefCell<PlatformDriver>>,
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = unsafe { Socket::from_raw(self.fd) };
    }
}

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        driver: Weak<RefCell<PlatformDriver>>,
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
            fd: socket.into_raw(),
            driver,
        })
    }

    pub async fn send_to(&self, buf: FixedBuf, target: SocketAddr) -> (io::Result<u32>, FixedBuf) {
        let op = SendTo::new(self.fd, buf, target);
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;
        (res, op_back.buf)
    }

    pub async fn recv_from(&self, buf: FixedBuf) -> (io::Result<(u32, SocketAddr)>, FixedBuf) {
        let op = RecvFrom::new(self.fd, buf);
        let future = Op::new(op, self.driver.clone());
        let (res, op_back) = future.await;

        match res {
            Ok(n) => {
                let len = op_back.get_addr_len();
                let addr = crate::io::sys::socket::to_socket_addr(&op_back.addr[..len])
                    .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                (Ok((n, addr)), op_back.buf)
            }
            Err(e) => (Err(e), op_back.buf),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        use std::mem::ManuallyDrop;
        let socket = unsafe { ManuallyDrop::new(Socket::from_raw(self.fd)) };
        socket.local_addr()
    }
}
