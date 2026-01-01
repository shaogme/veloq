#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Socket, socket_addr_trans, to_socket_addr};

#[cfg(windows)]
pub use windows::{Socket, socket_addr_trans, to_socket_addr};
