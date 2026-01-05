use super::open_options::OpenOptions; // Use generic OpenOptions
use crate::io::buffer::{BufPool, FixedBuf};
use crate::io::driver::PlatformDriver;
use crate::io::op::{Fallocate, Fsync, IoFd, Op, ReadFixed, SyncFileRange, WriteFixed};

use std::cell::RefCell;
use std::future::{Future, IntoFuture};
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::rc::Weak;

#[cfg(not(unix))]
macro_rules! ignore {
    ($($x:expr),* $(,)?) => {
        {
            $(
                let _ = $x;
            )*
        }
    };
}

pub struct File {
    pub(crate) fd: IoFd,
    pub(crate) driver: Weak<RefCell<PlatformDriver>>,
    pub(crate) pos: RefCell<u64>,
}

pub struct SyncRangeBuilder<'a> {
    file: &'a File,
    offset: u64,
    nbytes: u64,
    flags: u32,
}

impl<'a> SyncRangeBuilder<'a> {
    fn new(file: &'a File, offset: u64, nbytes: u64) -> Self {
        #[cfg(unix)]
        let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
            | libc::SYNC_FILE_RANGE_WRITE
            | libc::SYNC_FILE_RANGE_WAIT_AFTER;
        #[cfg(not(unix))]
        let flags = 0;

        Self {
            file,
            offset,
            nbytes,
            flags: flags as u32,
        }
    }

    pub fn wait_before(mut self, wait: bool) -> Self {
        #[cfg(unix)]
        if wait {
            self.flags |= libc::SYNC_FILE_RANGE_WAIT_BEFORE as u32;
        } else {
            self.flags &= !(libc::SYNC_FILE_RANGE_WAIT_BEFORE as u32);
        }
        #[cfg(not(unix))]
        ignore!(wait, &mut self);
        self
    }

    pub fn write(mut self, write: bool) -> Self {
        #[cfg(unix)]
        if write {
            self.flags |= libc::SYNC_FILE_RANGE_WRITE as u32;
        } else {
            self.flags &= !(libc::SYNC_FILE_RANGE_WRITE as u32);
        }
        #[cfg(not(unix))]
        ignore!(write, &mut self);
        self
    }

    pub fn wait_after(mut self, wait: bool) -> Self {
        #[cfg(unix)]
        if wait {
            self.flags |= libc::SYNC_FILE_RANGE_WAIT_AFTER as u32;
        } else {
            self.flags &= !(libc::SYNC_FILE_RANGE_WAIT_AFTER as u32);
        }
        #[cfg(not(unix))]
        ignore!(wait, &mut self);
        self
    }
}

impl<'a> IntoFuture for SyncRangeBuilder<'a> {
    type Output = io::Result<()>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let op = SyncFileRange {
            fd: self.file.fd,
            offset: self.offset,
            nbytes: self.nbytes,
            flags: self.flags,
        };
        let driver = self.file.driver.clone();
        Box::pin(async move {
            let (res, _) = Op::new(op, driver).await;
            res.map(|_| ())
        })
    }
}

impl File {
    pub async fn open(path: impl AsRef<Path>, pool: &dyn BufPool) -> io::Result<File> {
        OpenOptions::new().read(true).open(path, pool).await
    }

    pub async fn create(path: impl AsRef<Path>, pool: &dyn BufPool) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path, pool)
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
    ///
    /// Returns a specific Future (Builder) that allows configuring flags.
    /// Usage: `file.sync_range(0, 100).wait_before(false).write(true).await`
    pub fn sync_range(&self, offset: u64, nbytes: u64) -> SyncRangeBuilder<'_> {
        SyncRangeBuilder::new(self, offset, nbytes)
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

impl crate::io::AsyncBufRead for File {
    async fn read(&self, buf: FixedBuf) -> (io::Result<usize>, FixedBuf) {
        let offset = *self.pos.borrow();
        let (res, buf) = self.read_at(buf, offset).await;
        if let Ok(n) = res {
            *self.pos.borrow_mut() += n as u64;
        }
        (res, buf)
    }
}

impl crate::io::AsyncBufWrite for File {
    async fn write(&self, buf: FixedBuf) -> (io::Result<usize>, FixedBuf) {
        let offset = *self.pos.borrow();
        let (res, buf) = self.write_at(buf, offset).await;
        if let Ok(n) = res {
            *self.pos.borrow_mut() += n as u64;
        }
        (res, buf)
    }

    fn flush(&self) -> impl std::future::Future<Output = io::Result<()>> {
        self.sync_data()
    }

    fn shutdown(&self) -> impl std::future::Future<Output = io::Result<()>> {
        self.sync_all()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        use crate::io::driver::Driver;
        use crate::io::op::IntoPlatformOp;

        // Try to submit a background close op
        let submitted = {
            let driver_weak = self.driver.clone();
            if let Some(driver_rc) = driver_weak.upgrade() {
                let close = crate::io::op::Close { fd: self.fd };
                let op: <PlatformDriver as Driver>::Op =
                    IntoPlatformOp::<PlatformDriver>::into_platform_op(close);

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
