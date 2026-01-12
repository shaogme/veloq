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
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::io::driver::{Driver, PlatformDriver};
use crate::io::socket::SockAddrStorage;
use crate::io::{RawHandle, buffer::FixedBuf};

use std::thread::{self, ThreadId};

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
pub trait IntoPlatformOp<D: Driver>: Sized + std::marker::Send {
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

    /// Submit this operation to a remote IO driver via injector.
    pub fn submit_remote<D>(self, injector: &std::sync::Arc<D::RemoteInjector>) -> RemoteOpFuture<T>
    where
        T: IntoPlatformOp<D> + std::marker::Send + 'static,
        D: Driver,
    {
        let (tx, rx) = veloq_sync::oneshot::channel();
        let data = self.data;

        let closure = Box::new(move |driver: &mut D| {
            let op_platform = data.into_platform_op();
            let user_data = driver.reserve_op();

            let completer = Box::new(GenericCompleter {
                tx,
                _phantom: std::marker::PhantomData,
            });
            driver.attach_remote_completer(user_data, completer);

            if let Err((_e, _op)) = driver.submit(user_data, op_platform) {
                // Error handling: if submit fails, we can't easily return error via tx
                // as completer is already attached.
                // We log failure or ignore (driver might handle it).
            }
        });

        let _ = injector.inject(closure);

        RemoteOpFuture { rx }
    }
}

use crate::io::driver::{Injector, RemoteCompleter};

struct GenericCompleter<T, D> {
    tx: veloq_sync::oneshot::Sender<(std::io::Result<usize>, T)>,
    _phantom: std::marker::PhantomData<fn() -> D>,
}

impl<T, D> RemoteCompleter<D::Op> for GenericCompleter<T, D>
where
    D: Driver,
    T: IntoPlatformOp<D> + std::marker::Send,
{
    fn complete(self: Box<Self>, res: std::io::Result<usize>, op: D::Op) {
        let data = T::from_platform_op(op);
        let _ = self.tx.send((res, data));
    }
}

pub struct RemoteOpFuture<T> {
    rx: veloq_sync::oneshot::Receiver<(std::io::Result<usize>, T)>,
}

impl<T> Future for RemoteOpFuture<T> {
    type Output = (std::io::Result<usize>, T);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(res)) => Poll::Ready(res),
            Poll::Ready(Err(_)) => {
                // Panic is safer than returning invalid data
                panic!("Remote driver dropped operation");
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// Reopen impl block to continue original methods
impl<T> Op<T> {
    /// Submit this operation to a local IO driver.
    /// Returns a `LocalOp` future that resolves when the operation completes.
    /// Submit this operation to a local IO driver.
    /// Returns a `LocalOp` future that resolves when the operation completes.
    pub fn submit_local(self) -> LocalOp<T>
    where
        T: IntoPlatformOp<PlatformDriver> + 'static,
    {
        LocalOp::new(self.data)
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
    _marker: PhantomData<std::rc::Rc<T>>,
}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> LocalOp<T> {
    /// Create a new local operation future.
    pub fn new(data: T) -> Self {
        Self {
            state: State::Defined,
            data: Some(data),
            user_data: 0,
            _marker: Default::default(),
        }
    }

    /// Wraps this LocalOp to be `Send`, allowing it to be used in futures that require `Send`.
    ///
    /// # Panics
    ///
    /// The returned future MUST be polled on the same thread where this method is called.
    /// Polling it on a different thread will cause a panic.
    pub fn into_thread_bound(self) -> ThreadBoundOp<T> {
        ThreadBoundOp {
            inner: self,
            origin_thread: thread::current().id(),
        }
    }
}

/// A wrapper that mechanically implements `Send` for `LocalOp`,
/// but dynamically enforces thread-affinity at runtime.
///
/// This allows `LocalOp` to be used in `alloc::Box::pin` or `spawn_future` which requires `Send`,
/// while ensuring safety by panicking if moved to another thread.
pub struct ThreadBoundOp<T: IntoPlatformOp<PlatformDriver> + 'static> {
    inner: LocalOp<T>,
    origin_thread: ThreadId,
}

// SAFETY: We enforce thread-affinity via runtime checks in poll().
unsafe impl<T: IntoPlatformOp<PlatformDriver>> std::marker::Send for ThreadBoundOp<T> {}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> Future for ThreadBoundOp<T> {
    type Output = (std::io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Runtime safety check
        if thread::current().id() != self.origin_thread {
            panic!("ThreadBoundOp polled on a different thread! This is unsafe for LocalOp.");
        }

        // SAFETY: We are just polling the inner field, structure is pinned projected.
        // LocalOp does not have Unpin fields that need special handling here?
        // LocalOp is Unpin as long as T is Unpin (which it is).
        // Actually, we can just use map_unchecked_mut logic or simple matching since field is private.
        // Or simpler:
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll(cx)
    }
}

impl<T: IntoPlatformOp<PlatformDriver> + 'static> Future for LocalOp<T> {
    type Output = (std::io::Result<usize>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let op = unsafe { self.get_unchecked_mut() };

        match op.state {
            State::Defined => {
                let ctx = crate::runtime::context::current();
                let driver_rc = ctx.driver().upgrade().expect("Driver has been dropped");
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
                let ctx = crate::runtime::context::current();
                let driver_rc = ctx.driver().upgrade().expect("Driver has been dropped");
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
        if let State::Submitted = self.state {
            // Try to get current driver to cancel
            if let Some(ctx) = crate::runtime::context::try_current() {
                if let Some(driver_rc) = ctx.driver().upgrade() {
                    driver_rc.borrow_mut().cancel_op(self.user_data);
                }
            }
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
        Ok(RawHandle {
            handle: Socket::new_tcp_v4()?.into_raw(),
        })
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
            let fd = res?.into();
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
