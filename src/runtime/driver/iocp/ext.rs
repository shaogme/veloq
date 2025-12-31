use std::io;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IPPROTO_TCP, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKET, SOCK_STREAM,
    WSAID_ACCEPTEX, WSAID_CONNECTEX, WSAID_GETACCEPTEXSOCKADDRS, WSASocketW, WSA_FLAG_OVERLAPPED,
    WSAIoctl, closesocket, INVALID_SOCKET,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::Networking::WinSock::SOCKADDR;

// Function pointer types for WinSock extensions
pub(crate) type LpfnAcceptEx = unsafe extern "system" fn(
    slistensocket: SOCKET,
    sacceptsocket: SOCKET,
    lpoutputbuffer: *mut std::ffi::c_void,
    dwreceivedatalength: u32,
    dwlocaladdresslength: u32,
    dwremoteaddresslength: u32,
    lpdwbytesreceived: *mut u32,
    lpoverlapped: *mut OVERLAPPED,
) -> i32;

pub(crate) type LpfnConnectEx = unsafe extern "system" fn(
    s: SOCKET,
    name: *const SOCKADDR,
    namelen: i32,
    lpsendbuffer: *const std::ffi::c_void,
    dwsenddatalength: u32,
    lpdwbytessent: *mut u32,
    lpoverlapped: *mut OVERLAPPED,
) -> i32;

pub(crate) type LpfnGetAcceptExSockaddrs = unsafe extern "system" fn(
    lpoutputbuffer: *const std::ffi::c_void,
    dwreceivedatalength: u32,
    dwlocaladdresslength: u32,
    dwremoteaddresslength: u32,
    localsockaddr: *mut *mut SOCKADDR,
    localsockaddrlength: *mut i32,
    remotesockaddr: *mut *mut SOCKADDR,
    remotesockaddrlength: *mut i32,
);

pub struct Extensions {
    pub(crate) accept_ex: LpfnAcceptEx,
    pub(crate) connect_ex: LpfnConnectEx,
    pub(crate) get_accept_ex_sockaddrs: LpfnGetAcceptExSockaddrs,
}

impl Extensions {
    pub(crate) fn new() -> io::Result<Self> {
        unsafe {
            let socket = WSASocketW(
                AF_INET as i32,
                SOCK_STREAM,
                IPPROTO_TCP,
                std::ptr::null(),
                0,
                WSA_FLAG_OVERLAPPED,
            );
            if socket == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }

            let accept_ex = Self::get_extension(socket, WSAID_ACCEPTEX)?;
            let connect_ex = Self::get_extension(socket, WSAID_CONNECTEX)?;
            let get_accept_ex_sockaddrs = Self::get_extension(socket, WSAID_GETACCEPTEXSOCKADDRS)?;

            closesocket(socket);

            Ok(Self {
                accept_ex: std::mem::transmute(accept_ex),
                connect_ex: std::mem::transmute(connect_ex),
                get_accept_ex_sockaddrs: std::mem::transmute(get_accept_ex_sockaddrs),
            })
        }
    }

    unsafe fn get_extension(
        socket: SOCKET,
        guid: windows_sys::core::GUID,
    ) -> io::Result<*const std::ffi::c_void> {
        let mut guid = guid;
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut bytes_returned = 0;

        let ret = unsafe {
            WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &mut guid as *mut _ as *mut _,
                std::mem::size_of_val(&guid) as u32,
                &mut ptr as *mut _ as *mut _,
                std::mem::size_of_val(&ptr) as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
                None,
            )
        };

        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(ptr as *const _)
    }
}
