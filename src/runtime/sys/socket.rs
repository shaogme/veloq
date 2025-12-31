use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;

#[cfg(unix)]
pub use unix::{Socket, socket_addr_trans, to_socket_addr};

#[cfg(windows)]
pub use windows::{Socket, socket_addr_trans, to_socket_addr};

#[cfg(unix)]
mod unix {
    use super::*;
    use libc::{c_int, sockaddr, sockaddr_in, sockaddr_in6, socklen_t};
    use std::mem;
    use std::net::{Ipv4Addr, Ipv6Addr};

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
                let buf = unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<sockaddr_in>()) }
                    .to_vec();
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
                    unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<sockaddr_in6>()) }
                        .to_vec();
                (buf, mem::size_of::<sockaddr_in6>() as socklen_t)
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::mem;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::ptr;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_TCP, SOCK_STREAM, SOCKADDR, SOCKADDR_IN,
        SOCKADDR_IN6, WSA_FLAG_OVERLAPPED, WSADATA, WSASocketW, WSAStartup, bind, closesocket,
        getsockname, listen,
    };

    /// Winsock Initialization Hook
    ///
    /// This mimics C++ global constructors to run code before `main`.
    /// `.CRT$XCU` is the linker section used by MSVC for C++ dynamic initializers.
    /// placing a function pointer here ensures `WSAStartup` is called by the CRT start-up routines.
    ///
    /// This resolves the "10093: WSAStartup not called" error globally without user intervention.
    #[used]
    #[unsafe(link_section = ".CRT$XCU")]
    static INIT_WINSOCK: unsafe extern "C" fn() = {
        unsafe extern "C" fn init() {
            unsafe {
                let mut data: WSADATA = std::mem::zeroed();
                let _ = WSAStartup(0x0202, &mut data);
            }
        }
        init
    };

    pub fn to_socket_addr(buf: &[u8]) -> io::Result<SocketAddr> {
        if buf.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid address length",
            ));
        }
        let family = unsafe { *(buf.as_ptr() as *const u16) };
        match family {
            AF_INET => {
                if buf.len() < mem::size_of::<SOCKADDR_IN>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid address length",
                    ));
                }
                let sin = unsafe { &*(buf.as_ptr() as *const SOCKADDR_IN) };
                let s_addr = unsafe { sin.sin_addr.S_un.S_addr };
                let ip = Ipv4Addr::from(u32::from_be(s_addr));
                let port = u16::from_be(sin.sin_port);
                Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            AF_INET6 => {
                if buf.len() < mem::size_of::<SOCKADDR_IN6>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid address length",
                    ));
                }
                let sin6 = unsafe { &*(buf.as_ptr() as *const SOCKADDR_IN6) };
                let addr_bytes = unsafe { sin6.sin6_addr.u.Byte };
                let ip = Ipv6Addr::from(addr_bytes);
                let port = u16::from_be(sin6.sin6_port);
                let flowinfo = sin6.sin6_flowinfo;
                let scope_id = unsafe { sin6.Anonymous.sin6_scope_id };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    ip, port, flowinfo, scope_id,
                )))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unsupported address family",
            )),
        }
    }

    pub struct Socket {
        handle: RawHandle,
    }

    impl Socket {
        fn new(af: u16, ty: i32, protocol: i32) -> io::Result<Self> {
            let s =
                unsafe { WSASocketW(af as i32, ty, protocol, ptr::null(), 0, WSA_FLAG_OVERLAPPED) };
            if s == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                handle: s as RawHandle,
            })
        }

        pub fn new_tcp_v4() -> io::Result<Self> {
            Self::new(AF_INET, SOCK_STREAM, IPPROTO_TCP)
        }

        pub fn new_tcp_v6() -> io::Result<Self> {
            Self::new(AF_INET6, SOCK_STREAM, IPPROTO_TCP)
        }

        pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
            let (raw_addr, raw_addr_len) = socket_addr_trans(addr);
            let ret = unsafe {
                bind(
                    self.handle as usize,
                    raw_addr.as_ptr() as *const SOCKADDR,
                    raw_addr_len,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn listen(&self, backlog: i32) -> io::Result<()> {
            let ret = unsafe { listen(self.handle as usize, backlog) };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn into_raw(self) -> RawHandle {
            let h = self.handle;
            mem::forget(self);
            h
        }

        pub unsafe fn from_raw(handle: RawHandle) -> Self {
            Self { handle }
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            let mut buf = [0u8; 128];
            let mut len = 128 as i32;
            let ret = unsafe {
                getsockname(
                    self.handle as usize,
                    buf.as_mut_ptr() as *mut SOCKADDR,
                    &mut len,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
            to_socket_addr(&buf[..len as usize])
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            unsafe { closesocket(self.handle as usize) };
        }
    }

    pub fn socket_addr_trans(addr: SocketAddr) -> (Vec<u8>, i32) {
        match addr {
            SocketAddr::V4(a) => {
                let mut sin: SOCKADDR_IN = unsafe { mem::zeroed() };
                sin.sin_family = AF_INET;
                sin.sin_port = a.port().to_be();
                sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(a.ip().octets());

                let ptr = &sin as *const _ as *const u8;
                let buf = unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<SOCKADDR_IN>()) }
                    .to_vec();
                (buf, mem::size_of::<SOCKADDR_IN>() as i32)
            }
            SocketAddr::V6(a) => {
                let mut sin6: SOCKADDR_IN6 = unsafe { mem::zeroed() };
                sin6.sin6_family = AF_INET6;
                sin6.sin6_port = a.port().to_be();
                sin6.sin6_addr = unsafe { mem::transmute(a.ip().octets()) };
                sin6.sin6_flowinfo = a.flowinfo();
                sin6.Anonymous.sin6_scope_id = a.scope_id();

                let ptr = &sin6 as *const _ as *const u8;
                let buf =
                    unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<SOCKADDR_IN6>()) }
                        .to_vec();
                (buf, mem::size_of::<SOCKADDR_IN6>() as i32)
            }
        }
    }
}
