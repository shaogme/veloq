use libc::{c_int, sockaddr, sockaddr_in, sockaddr_in6, socklen_t};
use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::io::RawFd;

pub fn to_socket_addr(buf: &[u8]) -> io::Result<SocketAddr> {
    if buf.len() < mem::size_of::<libc::sa_family_t>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid address length",
        ));
    }
    let family = unsafe { *(buf.as_ptr() as *const libc::sa_family_t) } as i32;
    match family {
        libc::AF_INET => {
            if buf.len() < mem::size_of::<sockaddr_in>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid address length",
                ));
            }
            let sin = unsafe { &*(buf.as_ptr() as *const sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            if buf.len() < mem::size_of::<sockaddr_in6>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid address length",
                ));
            }
            let sin6 = unsafe { &*(buf.as_ptr() as *const sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported address family",
        )),
    }
}

pub struct Socket {
    fd: RawFd,
}

impl Socket {
    pub fn new_v4(ty: c_int) -> io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_INET, ty, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub fn new_v6(ty: c_int) -> io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_INET6, ty, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub fn new_tcp_v4() -> io::Result<Self> {
        Self::new_v4(libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
    }

    pub fn new_tcp_v6() -> io::Result<Self> {
        Self::new_v6(libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
    }

    pub fn new_udp_v4() -> io::Result<Self> {
        Self::new_v4(libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
    }

    pub fn new_udp_v6() -> io::Result<Self> {
        Self::new_v6(libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
    }

    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        let (raw_addr, raw_addr_len) = socket_addr_trans(addr);
        let ret =
            unsafe { libc::bind(self.fd, raw_addr.as_ptr() as *const sockaddr, raw_addr_len) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        let ret = unsafe { libc::listen(self.fd, backlog as c_int) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn into_raw(self) -> RawFd {
        let fd = self.fd;
        mem::forget(self);
        fd
    }

    pub unsafe fn from_raw(fd: RawFd) -> Self {
        Self { fd }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        let ret = unsafe {
            libc::getsockname(
                self.fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        to_socket_addr(unsafe {
            std::slice::from_raw_parts(&storage as *const _ as *const u8, len as usize)
        })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

pub fn socket_addr_trans(addr: SocketAddr) -> (Vec<u8>, socklen_t) {
    match addr {
        SocketAddr::V4(a) => {
            let mut sin: sockaddr_in = unsafe { mem::zeroed() };
            sin.sin_family = libc::AF_INET as _;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());

            let ptr = &sin as *const _ as *const u8;
            let buf =
                unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<sockaddr_in>()) }.to_vec();
            (buf, mem::size_of::<sockaddr_in>() as socklen_t)
        }
        SocketAddr::V6(a) => {
            let mut sin6: sockaddr_in6 = unsafe { mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as _;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_scope_id = a.scope_id();

            let ptr = &sin6 as *const _ as *const u8;
            let buf =
                unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<sockaddr_in6>()) }.to_vec();
            (buf, mem::size_of::<sockaddr_in6>() as socklen_t)
        }
    }
}

pub fn socket_addr_to_storage(addr: SocketAddr) -> (libc::sockaddr_storage, socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin_ptr = &mut storage as *mut _ as *mut sockaddr_in;
            unsafe {
                (*sin_ptr).sin_family = libc::AF_INET as _;
                (*sin_ptr).sin_port = a.port().to_be();
                (*sin_ptr).sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                mem::size_of::<sockaddr_in>() as socklen_t
            }
        }
        SocketAddr::V6(a) => {
            let sin6_ptr = &mut storage as *mut _ as *mut sockaddr_in6;
            unsafe {
                (*sin6_ptr).sin6_family = libc::AF_INET6 as _;
                (*sin6_ptr).sin6_port = a.port().to_be();
                (*sin6_ptr).sin6_addr.s6_addr = a.ip().octets();
                (*sin6_ptr).sin6_flowinfo = a.flowinfo();
                (*sin6_ptr).sin6_scope_id = a.scope_id();
                mem::size_of::<sockaddr_in6>() as socklen_t
            }
        }
    };
    (storage, len)
}
