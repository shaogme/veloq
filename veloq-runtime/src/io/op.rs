//! # IO Operation Abstraction Layer
//!
//! This module defines platform-agnostic operation structures and traits.
//! All types here are completely cross-platform with no conditional compilation.
//!
//! Platform-specific implementations reside in:
//! - `io/driver/uring/op.rs` for Linux io_uring
//! - `io/driver/iocp/op.rs` for Windows IOCP

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::io::buffer::FixedBuf;
use crate::io::driver::{Driver, PlatformDriver};
use crate::io::socket::SockAddrStorage;
use std::cell::RefCell;
use std::rc::Weak;

// ============================================================================
// Core Types (Platform-Agnostic)
// ============================================================================

#[cfg(unix)]
pub type RawHandle = std::os::fd::RawFd; // i32 (4 bytes)
#[cfg(windows)]
pub type RawHandle = windows_sys::Win32::Foundation::HANDLE; // usually isize/ptr (8 bytes)

/// Represents the source of an IO operation: either a raw handle or a registered index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoFd {
    /// A raw system handle (fd on Unix, HANDLE on Windows).
    Raw(RawHandle),
    /// A registered index for pre-registered file descriptors.
    Fixed(u32),
}

impl IoFd {
    /// Returns the raw handle if this is a Raw variant.
    pub fn raw(&self) -> Option<RawHandle> {
        match self {
            Self::Raw(fd) => Some(*fd),
            Self::Fixed(_) => None,
        }
    }

    /// Creates an IoFd from a raw handle.
    pub fn from_raw(handle: RawHandle) -> Self {
        Self::Raw(handle)
    }
}

// ============================================================================
// Core Traits
// ============================================================================

/// Trait for managing the lifecycle of an operation.
/// Handles pre-allocation, construction, and output conversion.
pub trait OpLifecycle: Sized {
    /// Type for any pre-allocated resources needed before creating the op.
    type PreAlloc;
    /// The final output type after the operation completes.
    type Output;

    /// Pre-allocate any resources needed (e.g., accept socket on Windows).
    fn pre_alloc(fd: RawHandle) -> std::io::Result<Self::PreAlloc>;

    /// Construct the operation from a raw handle and pre-allocated resources.
    fn into_op(fd: RawHandle, pre: Self::PreAlloc) -> Self;

    /// Convert the completed operation result to the final output type.
    fn into_output(self, res: std::io::Result<usize>) -> std::io::Result<Self::Output>;
}

/// Trait to convert a user-facing operation to a platform-specific driver operation.
pub trait IntoPlatformOp<D: Driver>: Sized {
    /// Convert this operation into the platform driver's operation type.
    fn into_platform_op(self) -> D::Op;

    /// Convert from the platform driver's operation type back to this type.
    fn from_platform_op(op: D::Op) -> Self;
}

// ============================================================================
// Op (Generic Data Carrier)
// ============================================================================

/// A generic wrapper for IO operation data.
///
/// This struct represents the "intent" of an operation, holding only the data
/// required to perform the IO (e.g., buffers, file descriptors, flags).
/// It is decoupled from the execution backend (Driver).
pub struct Op<T> {
    pub data: T,
}

impl<T> Op<T> {
    /// Create a new operation intent with the given data.
    pub fn new(data: T) -> Self {
        Self { data }
    }

    /// Submit this operation to a local IO driver.
    /// Returns a `LocalOp` future that resolves when the operation completes.
    pub fn submit_local(self, driver: Weak<RefCell<PlatformDriver>>) -> LocalOp<T>
    where
        T: IntoPlatformOp<PlatformDriver> + 'static,
    {
        LocalOp::new(self.data, driver)
    }
}

// ============================================================================
// LocalOp (Future Implementation)
// ============================================================================

enum State {
    Defined,
    Submitted,
    Completed,
}

/// A Future wrapper for asynchronous IO operations executed locally.
///
/// This struct manages the lifecycle of an IO operation submitted to the local driver:
/// 1. Defined: Operation created but not submitted
/// 2. Submitted: Operation submitted to the driver
/// 3. Completed: Operation finished, result available
pub struct LocalOp<T: IntoPlatformOp<PlatformDriver> + 'static> {
    state: State,
    data: Option<T>,
    user_data: usize,
    driver: Weak<RefCell<PlatformDriver>>,
}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> LocalOp<T> {
    /// Create a new local operation future.
    pub fn new(data: T, driver: Weak<RefCell<PlatformDriver>>) -> Self {
        Self {
            state: State::Defined,
            data: Some(data),
            user_data: 0,
            driver,
        }
    }
}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> Future for LocalOp<T> {
    type Output = (std::io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let op = unsafe { self.get_unchecked_mut() };

        match op.state {
            State::Defined => {
                let driver_rc = op.driver.upgrade().expect("Driver has been dropped");
                let mut driver = driver_rc.borrow_mut();

                // Submit to driver
                let data = op.data.take().expect("Op started without data");
                let driver_op = data.into_platform_op();
                let user_data = driver.reserve_op();
                op.user_data = user_data;

                // Submit to driver.
                // Whether Ready or Pending, the op is now owned by the driver.
                // If Pending, it effectively means "Accepted but queued".
                // If Err((e, op)), driver rejected it and returned ownership.
                if let Err((e, val)) = driver.submit(user_data, driver_op) {
                    // Driver rejected submission and returned the op.
                    // Recover data and return error immediately.
                    let data = T::from_platform_op(val);
                    return Poll::Ready((Err(e), data));
                }

                op.state = State::Submitted;

                // Register waker immediately by polling the op
                match driver.poll_op(user_data, cx) {
                    Poll::Ready((res, driver_op)) => {
                        op.state = State::Completed;
                        let data = T::from_platform_op(driver_op);
                        Poll::Ready((res, data))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            State::Submitted => {
                let driver_rc = op.driver.upgrade().expect("Driver has been dropped");
                let mut driver = driver_rc.borrow_mut();

                match driver.poll_op(op.user_data, cx) {
                    Poll::Ready((res, driver_op)) => {
                        op.state = State::Completed;
                        // Convert resources back to T
                        let data = T::from_platform_op(driver_op);
                        Poll::Ready((res, data))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            State::Completed => panic!("Polled after completion"),
        }
    }
}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> Drop for LocalOp<T> {
    fn drop(&mut self) {
        if let State::Submitted = self.state
            && let Some(driver_rc) = self.driver.upgrade()
        {
            driver_rc.borrow_mut().cancel_op(self.user_data);
        }
    }
}

// ============================================================================
// Cross-Platform Operation Structures
// ============================================================================

/// Read from a file descriptor at a specific offset using a fixed buffer.
pub struct ReadFixed {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub offset: u64,
}

/// Write to a file descriptor at a specific offset using a fixed buffer.
pub struct WriteFixed {
    pub fd: IoFd,
    pub buf: FixedBuf,
    pub offset: u64,
}

/// Receive data from a socket into a fixed buffer.
pub struct Recv {
    pub fd: IoFd,
    pub buf: FixedBuf,
}

/// Send data from a fixed buffer to a socket.
pub struct Send {
    pub fd: IoFd,
    pub buf: FixedBuf,
}

/// Connect a socket to a remote address.
pub struct Connect {
    pub fd: IoFd,
    /// Raw address bytes (sockaddr representation), boxed to reduce struct size.
    pub addr: SockAddrStorage,
    pub addr_len: u32,
}

/// Open a file.
/// Path representation is platform-agnostic (raw bytes).
#[derive(Debug)]
pub struct Open {
    /// Path stored in a fixed buffer.
    /// - Unix: UTF-8 encoded, null-terminated.
    /// - Windows: UTF-16 encoded, null-terminated (stored as bytes).
    pub path: FixedBuf,
    pub flags: i32,
    pub mode: u32,
}

/// Close a file descriptor or handle.
pub struct Close {
    pub fd: IoFd,
}

/// Flush file buffers to disk.
pub struct Fsync {
    pub fd: IoFd,
    /// If true, only sync data (not metadata).
    pub datasync: bool,
}

/// Timeout operation (platform-specific timing).
pub struct Timeout {
    pub duration: std::time::Duration,
}

/// Wake up the event loop.
pub struct Wakeup {
    pub fd: IoFd,
}

/// Accept a new connection on a listening socket.
/// Result includes the new socket handle and remote address.
pub struct Accept {
    pub fd: IoFd,
    /// Buffer for storing the remote address.
    /// On Windows, we parse the result from the AcceptEx output buffer, so we don't need this storage.
    #[cfg(unix)]
    pub addr: SockAddrStorage,
    /// Length of the address buffer.
    #[cfg(unix)]
    pub addr_len: u32,
    /// Parsed remote address (populated after completion).
    pub remote_addr: Option<std::net::SocketAddr>,
    /// Pre-allocated accept socket (Windows only, required for AcceptEx).
    #[cfg(windows)]
    pub accept_socket: RawHandle,
}

/// Send data to a specific address (UDP).
pub struct SendTo {
    pub fd: IoFd,
    pub buf: FixedBuf,
    /// Target address.
    pub addr: std::net::SocketAddr,
}

/// Receive data and source address (UDP).
pub struct RecvFrom {
    pub fd: IoFd,
    pub buf: FixedBuf,
    /// Source address (populated after completion).
    pub addr: Option<std::net::SocketAddr>,
}

/// Sync file range.
pub struct SyncFileRange {
    pub fd: IoFd,
    pub offset: u64,
    pub nbytes: u64,
    pub flags: u32,
}

/// Pre-allocate file space.
pub struct Fallocate {
    pub fd: IoFd,
    pub mode: i32,
    pub offset: u64,
    pub len: u64,
}

#[cfg(windows)]
/// Receive data using Windows Registered I/O (RIO).
pub struct RioRecv {
    pub fd: IoFd,
    pub buf: FixedBuf,
}

#[cfg(windows)]
/// Send data using Windows Registered I/O (RIO).
pub struct RioSend {
    pub fd: IoFd,
    pub buf: FixedBuf,
}

// ============================================================================
// OpLifecycle Implementations
// ============================================================================

impl OpLifecycle for Accept {
    /// On Windows: pre-created accept socket handle
    /// On Unix: unit (no pre-allocation needed)
    #[cfg(unix)]
    type PreAlloc = ();
    #[cfg(windows)]
    type PreAlloc = RawHandle;

    type Output = (RawHandle, std::net::SocketAddr);

    #[cfg(windows)]
    fn pre_alloc(_fd: RawHandle) -> std::io::Result<Self::PreAlloc> {
        // On Windows, we need to pre-create a socket for AcceptEx
        use crate::io::socket::Socket;
        Ok(Socket::new_tcp_v4()?.into_raw() as RawHandle)
    }

    #[cfg(unix)]
    fn pre_alloc(_fd: RawHandle) -> std::io::Result<Self::PreAlloc> {
        Ok(())
    }

    #[allow(unused_variables)]
    fn into_op(fd: RawHandle, pre: Self::PreAlloc) -> Self {
        // Use stack/inline storage
        #[cfg(unix)]
        let addr_len = std::mem::size_of::<SockAddrStorage>() as u32;

        #[cfg(unix)]
        {
            Self {
                fd: IoFd::Raw(fd),
                addr: unsafe { std::mem::zeroed() },
                addr_len,
                remote_addr: None,
            }
        }
        #[cfg(windows)]
        {
            Self {
                fd: IoFd::Raw(fd),
                remote_addr: None,
                accept_socket: pre,
            }
        }
    }

    fn into_output(self, res: std::io::Result<usize>) -> std::io::Result<Self::Output> {
        #[cfg(unix)]
        {
            let fd = res? as RawHandle;
            use crate::io::socket::to_socket_addr;
            let addr = if let Some(a) = self.remote_addr {
                a
            } else {
                unsafe {
                    let s = std::slice::from_raw_parts(
                        &self.addr as *const _ as *const u8,
                        self.addr_len as usize,
                    );
                    to_socket_addr(s).unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
                }
            };
            Ok((fd, addr))
        }
        #[cfg(windows)]
        {
            res?;
            // On Windows, the accept_socket was pre-allocated and is the new connection
            // Helper to provide a default address if parsing failed or was irrelevant, preventing panic
            let addr = self
                .remote_addr
                .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
            Ok((self.accept_socket, addr))
        }
    }
}
