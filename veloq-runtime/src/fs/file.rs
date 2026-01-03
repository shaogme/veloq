use super::open_options::OpenOptions; // Use generic OpenOptions
use crate::io::buffer::{BufPool, FixedBuf};
use crate::io::driver::PlatformDriver;
use crate::io::op::{Fallocate, Fsync, IoFd, Op, ReadFixed, SyncFileRange, WriteFixed};
use crate::runtime::context::RuntimeContext;
use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::rc::Weak;

pub struct File<P: BufPool> {
    pub(crate) fd: IoFd,
    pub(crate) driver: Weak<RefCell<PlatformDriver<P>>>,
    pub(crate) pos: RefCell<u64>,
}

impl<P: BufPool> File<P> {
    pub async fn open(path: impl AsRef<Path>, context: &RuntimeContext<P>) -> io::Result<File<P>> {
        OpenOptions::new().read(true).open(path, context).await
    }

    pub async fn create(
        path: impl AsRef<Path>,
        context: &RuntimeContext<P>,
    ) -> io::Result<File<P>> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path, context)
            .await
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn seek(&self, pos: u64) {
        *self.pos.borrow_mut() = pos;
    }

    pub fn stream_position(&self) -> u64 {
        *self.pos.borrow()
    }

    pub async fn read_at(&self, buf: FixedBuf, offset: u64) -> (io::Result<usize>, FixedBuf) {
        let op = ReadFixed {
            fd: self.fd,
            buf,
            offset,
        };
        let driver = self.driver.clone();
        let (res, op) = Op::new(op, driver).await;
        (res, op.buf)
    }

    pub async fn write_at(&self, buf: FixedBuf, offset: u64) -> (io::Result<usize>, FixedBuf) {
        let op = WriteFixed {
            fd: self.fd,
            buf,
            offset,
        };
        let driver = self.driver.clone();
        let (res, op) = Op::new(op, driver).await;
        (res, op.buf)
    }

    pub async fn sync_all(&self) -> io::Result<()> {
        let op = Fsync {
            fd: self.fd,
            datasync: false,
        };
        let driver = self.driver.clone();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }

    pub async fn sync_data(&self) -> io::Result<()> {
        let op = Fsync {
            fd: self.fd,
            datasync: true,
        };
        let driver = self.driver.clone();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }

    /// Sync a file range.
    ///
    /// On Windows, this falls back to `FlushFileBuffers` which syncs the entire file, ignoring the range.
    pub async fn sync_range(&self, offset: u64, nbytes: u64) -> io::Result<()> {
        #[cfg(unix)]
        let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
            | libc::SYNC_FILE_RANGE_WRITE
            | libc::SYNC_FILE_RANGE_WAIT_AFTER;
        #[cfg(not(unix))]
        let flags = 0;

        let op = SyncFileRange {
            fd: self.fd,
            offset,
            nbytes,
            flags: flags as u32,
        };
        let driver = self.driver.clone();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }

    pub async fn fallocate(&self, offset: u64, len: u64) -> io::Result<()> {
        let op = Fallocate {
            fd: self.fd,
            mode: 0, // Default mode
            offset,
            len,
        };
        let driver = self.driver.clone();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }
}

impl<P: BufPool> crate::io::AsyncBufRead<P> for File<P> {
    fn read(
        &self,
        buf: FixedBuf,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf)> {
        async move {
            let offset = *self.pos.borrow();
            let (res, buf) = self.read_at(buf, offset).await;
            if let Ok(n) = res {
                *self.pos.borrow_mut() += n as u64;
            }
            (res, buf)
        }
    }
}

impl<P: BufPool> crate::io::AsyncBufWrite for File<P> {
    fn write(
        &self,
        buf: FixedBuf,
    ) -> impl std::future::Future<Output = (io::Result<usize>, FixedBuf)> {
        async move {
            let offset = *self.pos.borrow();
            let (res, buf) = self.write_at(buf, offset).await;
            if let Ok(n) = res {
                *self.pos.borrow_mut() += n as u64;
            }
            (res, buf)
        }
    }

    fn flush(&self) -> impl std::future::Future<Output = io::Result<()>> {
        self.sync_data()
    }

    fn shutdown(&self) -> impl std::future::Future<Output = io::Result<()>> {
        self.sync_all()
    }
}

impl<P: BufPool> Drop for File<P> {
    fn drop(&mut self) {
        use crate::io::driver::Driver;
        use crate::io::op::IntoPlatformOp;

        // Try to submit a background close op
        let submitted = {
            let driver_weak = self.driver.clone();
            if let Some(driver_rc) = driver_weak.upgrade() {
                let close = crate::io::op::Close { fd: self.fd };
                let op: <PlatformDriver<P> as Driver>::Op =
                    IntoPlatformOp::<PlatformDriver<P>>::into_platform_op(close);

                if let Ok(mut driver) = driver_rc.try_borrow_mut() {
                    driver.submit_background(op).is_ok()
                } else {
                    false
                }
            } else {
                false
            }
        };

        if !submitted {
            // Fallback to synchronous close
            if let Some(fd) = self.fd.raw() {
                #[cfg(unix)]
                unsafe {
                    libc::close(fd as i32);
                }
                #[cfg(windows)]
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(fd as _);
                }
            }
        }
    }
}
