#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use libc::sockaddr_storage as SockAddrStorage;
#[cfg(unix)]
pub use unix::{Socket, socket_addr_to_storage, to_socket_addr};

#[cfg(windows)]
pub use windows::{
    SOCKADDR_STORAGE as SockAddrStorage, Socket, socket_addr_to_storage, to_socket_addr,
};
