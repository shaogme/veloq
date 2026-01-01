#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(unix)]
pub type SysRawOp = RawFd;
#[cfg(windows)]
pub type SysRawOp = RawHandle;

use crate::io::buffer::FixedBuf;

/// Represents the source of an IO operation: either a raw handle/fd or a registered index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoFd {
    /// A raw system handle/fd.
    /// On Linux: RawFd
    /// On Windows: HANDLE / SOCKET
    Raw(SysRawOp),
    /// A registered index.
    /// On Linux: io_uring fixed file index.
    /// On Windows: Driver-internal registered handle index.
    Fixed(u32),
}

impl IoFd {
    pub fn raw(&self) -> Option<SysRawOp> {
        match self {
            Self::Raw(fd) => Some(*fd),
            Self::Fixed(_) => None,
        }
    }
}

pub enum IoResources {
    ReadFixed(ReadFixed),
    WriteFixed(WriteFixed),
    Send(Send),
    Recv(Recv),
    Timeout(Timeout),
    Accept(Accept),
    Connect(Connect),
    SendTo(SendTo),
    RecvFrom(RecvFrom),
    Wakeup(Wakeup),
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
    fn into_output(self, res: std::io::Result<usize>) -> std::io::Result<Self::Output>;
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

use crate::io::driver::{Driver, PlatformDriver};
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
    type Output = (std::io::Result<usize>, T);

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
    pub fd: IoFd,
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
    pub fd: IoFd,
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
    pub fd: IoFd,
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
    pub fd: IoFd,
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

pub struct Connect {
    pub fd: IoFd,
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

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;
