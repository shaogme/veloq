#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

#[cfg(unix)]
pub type SysRawOp = RawFd;
#[cfg(windows)]
pub type SysRawOp = RawHandle;

use crate::runtime::buffer::FixedBuf;

pub enum IoResources {
    ReadFixed(ReadFixed),
    WriteFixed(WriteFixed),
    Send(Send),
    Recv(Recv),
    Timeout(Timeout),
    Accept(Accept),
    Connect(Connect),
    // Placeholder for when we have no resource back yet or it's empty
    None,
}

/// Trait for operations that require platform-specific resource pre-allocation
/// and post-processing logic.
pub trait OpLifecycle: Sized {
    /// Resources allocated before the operation is submitted.
    type PreAlloc;
    /// The final output of the operation after completion.
    type Output;

    /// Perform pre-allocation.
    /// `fd` is provided in case the pre-allocation depends on the main resource (e.g. address family).
    fn pre_alloc(fd: SysRawOp) -> std::io::Result<Self::PreAlloc>;

    /// Create the Op struct from the main resource and pre-allocated resources.
    fn into_op(fd: SysRawOp, pre: Self::PreAlloc) -> Self;

    /// Convert the completed Op back into the desired output.
    fn into_output(self, res: std::io::Result<u32>) -> std::io::Result<Self::Output>;
}

pub trait IoOp: Sized {
    fn into_resource(self) -> IoResources;
    fn from_resource(res: IoResources) -> Self;
}

enum State {
    Defined,
    Submitted,
    Completed,
}

use crate::runtime::driver::{Driver, PlatformDriver};
use std::cell::RefCell;
use std::rc::Weak;

pub struct Op<T: IoOp + 'static> {
    state: State,
    data: Option<T>,
    user_data: usize,
    driver: Weak<RefCell<PlatformDriver>>,
}

impl<T: IoOp> Op<T> {
    pub fn new(data: T, driver: Weak<RefCell<PlatformDriver>>) -> Self {
        Self {
            state: State::Defined,
            data: Some(data),
            user_data: 0,
            driver,
        }
    }
}

impl<T: IoOp + 'static> Future for Op<T> {
    type Output = (std::io::Result<u32>, T);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let op = unsafe { self.get_unchecked_mut() };

        match op.state {
            State::Defined => {
                let driver_rc = op.driver.upgrade().expect("Driver has been dropped");
                let mut driver = driver_rc.borrow_mut();

                // Submit to driver
                let data = op.data.take().expect("Op started without data");
                let resources = data.into_resource();
                let user_data = driver.reserve_op();
                op.user_data = user_data;
                driver.submit_op_resources(user_data, resources);

                op.state = State::Submitted;
                
                // Register waker immediately by polling the op
                // This ensures that if the operation completes quickly, we get woken up
                let _ = driver.poll_op(user_data, cx);
                Poll::Pending
            }
            State::Submitted => {
                let driver_rc = op.driver.upgrade().expect("Driver has been dropped");
                let mut driver = driver_rc.borrow_mut();

                match driver.poll_op(op.user_data, cx) {
                    Poll::Ready((res, resources_enum)) => {
                        op.state = State::Completed;
                        // Convert resources back to T
                        let data = T::from_resource(resources_enum);
                        Poll::Ready((res, data))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            State::Completed => panic!("Polled after completion"),
        }
    }
}

impl<T: IoOp + 'static> Drop for Op<T> {
    fn drop(&mut self) {
        if let State::Submitted = self.state {
            if let Some(driver_rc) = self.driver.upgrade() {
                driver_rc.borrow_mut().cancel_op(self.user_data);
            }
        }
    }
}

pub struct ReadFixed {
    pub fd: SysRawOp,
    pub buf: FixedBuf,
    pub offset: u64,
}

impl IoOp for ReadFixed {
    fn into_resource(self) -> IoResources {
        IoResources::ReadFixed(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::ReadFixed(r) => r,
            _ => panic!("Resource type mismatch for ReadFixed"),
        }
    }
}

pub struct WriteFixed {
    pub fd: SysRawOp,
    pub buf: FixedBuf,
    pub offset: u64,
}

impl IoOp for WriteFixed {
    fn into_resource(self) -> IoResources {
        IoResources::WriteFixed(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::WriteFixed(r) => r,
            _ => panic!("Resource type mismatch for WriteFixed"),
        }
    }
}

pub struct Recv {
    pub fd: SysRawOp,
    pub buf: FixedBuf,
}

impl IoOp for Recv {
    fn into_resource(self) -> IoResources {
        IoResources::Recv(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Recv(r) => r,
            _ => panic!("Resource type mismatch for Recv"),
        }
    }
}

pub struct Send {
    pub fd: SysRawOp,
    pub buf: FixedBuf,
}

impl IoOp for Send {
    fn into_resource(self) -> IoResources {
        IoResources::Send(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Send(r) => r,
            _ => panic!("Resource type mismatch for Send"),
        }
    }
}

pub struct Timeout {
    pub duration: Duration,
    #[cfg(target_os = "linux")]
    pub ts: [i64; 2],
}

impl IoOp for Timeout {
    fn into_resource(self) -> IoResources {
        IoResources::Timeout(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Timeout(r) => r,
            _ => panic!("Resource type mismatch for Timeout"),
        }
    }
}

pub struct Accept {
    pub fd: SysRawOp,
    /// Buffer to hold the address.
    /// On Linux: sockaddr_storage
    /// On Windows: Output buffer for AcceptEx (addresses)
    pub addr: Box<[u8]>,

    pub addr_len: Box<u32>,
    #[cfg(windows)]
    pub accept_socket: SysRawOp,
    pub remote_addr: Option<std::net::SocketAddr>,
}

impl OpLifecycle for Accept {
    #[cfg(windows)]
    type PreAlloc = SysRawOp;
    #[cfg(unix)]
    type PreAlloc = ();

    type Output = (SysRawOp, std::net::SocketAddr);

    fn pre_alloc(_fd: SysRawOp) -> std::io::Result<Self::PreAlloc> {
        #[cfg(windows)]
        {
            // FIXME: accurately detect family from _fd or generic
            // For now assuming IPv4 or relying on internal logic
            use crate::runtime::sys::socket::Socket;
            Ok(Socket::new_tcp_v4()?.into_raw())
        }
        #[cfg(unix)]
        {
            Ok(())
        }
    }

    #[allow(unused_variables)]
    fn into_op(fd: SysRawOp, pre: Self::PreAlloc) -> Self {
        // Buffer size check
        #[cfg(windows)]
        let buf_size = 288; // (sizeof(sockaddr_storage) + 16) * 2
        #[cfg(unix)]
        let buf_size = std::mem::size_of::<libc::sockaddr_storage>();

        let addr_buf = vec![0u8; buf_size].into_boxed_slice();
        let addr_len = Box::new(buf_size as u32);

        Self {
            fd,
            addr: addr_buf,
            addr_len,
            #[cfg(windows)]
            accept_socket: pre,
            remote_addr: None,
        }
    }

    fn into_output(self, res: std::io::Result<u32>) -> std::io::Result<Self::Output> {
        #[cfg(unix)]
        let fd = res? as SysRawOp;
        
        #[cfg(windows)]
        let fd = {
            res?;
            self.accept_socket
        };

        use crate::runtime::sys::socket::to_socket_addr;
        let addr = if let Some(a) = self.remote_addr {
            a
        } else {
            to_socket_addr(&self.addr).unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
        };

        Ok((fd, addr))
    }
}

impl IoOp for Accept {
    fn into_resource(self) -> IoResources {
        IoResources::Accept(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Accept(r) => r,
            _ => panic!("Resource type mismatch for Accept"),
        }
    }
}

pub struct Connect {
    pub fd: SysRawOp,
    pub addr: Box<[u8]>,
    pub addr_len: u32,
}

impl IoOp for Connect {
    fn into_resource(self) -> IoResources {
        IoResources::Connect(self)
    }

    fn from_resource(res: IoResources) -> Self {
        match res {
            IoResources::Connect(r) => r,
            _ => panic!("Resource type mismatch for Connect"),
        }
    }
}
