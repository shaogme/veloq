use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::RawHandle;
use std::ptr;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_TCP, IPPROTO_UDP, SOCK_DGRAM, SOCK_STREAM, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, WSA_FLAG_OVERLAPPED, WSADATA, WSASocketW, WSAStartup, bind,
    closesocket, getsockname, listen,
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
        let s = unsafe { WSASocketW(af as i32, ty, protocol, ptr::null(), 0, WSA_FLAG_OVERLAPPED) };
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

    pub fn new_udp_v4() -> io::Result<Self> {
        Self::new(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
    }

    pub fn new_udp_v6() -> io::Result<Self> {
        Self::new(AF_INET6, SOCK_DGRAM, IPPROTO_UDP)
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
            let buf =
                unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<SOCKADDR_IN>()) }.to_vec();
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
                unsafe { std::slice::from_raw_parts(ptr, mem::size_of::<SOCKADDR_IN6>()) }.to_vec();
            (buf, mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

pub use windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE;

pub fn socket_addr_to_storage(addr: SocketAddr) -> (SOCKADDR_STORAGE, i32) {
    let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin_ptr = &mut storage as *mut _ as *mut SOCKADDR_IN;
            unsafe {
                (*sin_ptr).sin_family = AF_INET;
                (*sin_ptr).sin_port = a.port().to_be();
                (*sin_ptr).sin_addr.S_un.S_addr = u32::from_ne_bytes(a.ip().octets());
                mem::size_of::<SOCKADDR_IN>() as i32
            }
        }
        SocketAddr::V6(a) => {
            let sin6_ptr = &mut storage as *mut _ as *mut SOCKADDR_IN6;
            unsafe {
                (*sin6_ptr).sin6_family = AF_INET6;
                (*sin6_ptr).sin6_port = a.port().to_be();
                (*sin6_ptr).sin6_addr = mem::transmute(a.ip().octets());
                (*sin6_ptr).sin6_flowinfo = a.flowinfo();
                (*sin6_ptr).Anonymous.sin6_scope_id = a.scope_id();
                mem::size_of::<SOCKADDR_IN6>() as i32
            }
        }
    };
    (storage, len)
}
