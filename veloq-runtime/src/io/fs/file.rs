use super::open_options::OpenOptions; // Use generic OpenOptions
use crate::io::buffer::FixedBuf;
use crate::io::op::{Fsync, IoFd, Op, ReadFixed, WriteFixed};
use std::io;
use std::path::Path;

pub struct File {
    pub(crate) fd: IoFd,
}

impl File {
    pub async fn open(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).open(path).await
    }

    pub async fn create(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub async fn read_at(&self, buf: FixedBuf, offset: u64) -> (io::Result<usize>, FixedBuf) {
        let op = ReadFixed {
            fd: self.fd,
            buf,
            offset,
        };
        let driver = crate::runtime::current_driver();
        let (res, op) = Op::new(op, driver).await;
        (res, op.buf)
    }

    pub async fn write_at(&self, buf: FixedBuf, offset: u64) -> (io::Result<usize>, FixedBuf) {
        let op = WriteFixed {
            fd: self.fd,
            buf,
            offset,
        };
        let driver = crate::runtime::current_driver();
        let (res, op) = Op::new(op, driver).await;
        (res, op.buf)
    }

    pub async fn sync_all(&self) -> io::Result<()> {
        let op = Fsync {
            fd: self.fd,
            datasync: false,
        };
        let driver = crate::runtime::current_driver();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }

    pub async fn sync_data(&self) -> io::Result<()> {
        let op = Fsync {
            fd: self.fd,
            datasync: true,
        };
        let driver = crate::runtime::current_driver();
        let (res, _) = Op::new(op, driver).await;
        res.map(|_| ())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        use crate::io::driver::Driver;
        use crate::io::op::IoOp;

        // Try to submit a background close op
        let submitted = {
            let driver_weak = crate::runtime::current_driver();
            if let Some(driver_rc) = driver_weak.upgrade() {
                let op = crate::io::op::Close { fd: self.fd }.into_resource();

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
                    libc::close(fd);
                }
                #[cfg(windows)]
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(fd as _);
                }
            }
        }
    }
}
