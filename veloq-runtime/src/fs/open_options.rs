use crate::io::{
    buffer::{AllocResult, BufPool, FixedBuf},
    op::{Op, Open},
};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
    custom_flags: i32,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
            custom_flags: 0,
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.custom_flags = flags;
        self
    }

    /// 对外的公共 API：清晰、线性、无平台噪音
    pub async fn open<P: BufPool>(
        &self,
        path: impl AsRef<Path>,
        context: &crate::runtime::context::RuntimeContext<P>,
    ) -> std::io::Result<super::file::File<P>> {
        // 1. 根据不同平台生成对应的 Op 参数
        let op = self.build_op(path.as_ref(), context).await?;

        // 2. 提交给 runtime (显式传递 driver)
        let driver = context.driver();
        let (res, _) = Op::new(op, driver.clone()).await;

        // 3. 转换结果
        let fd = res? as crate::io::op::RawHandle;
        use crate::io::op::IoFd;
        Ok(super::file::File {
            fd: IoFd::Raw(fd),
            driver,
            pos: std::cell::RefCell::new(0),
        })
    }

    // ==========================================
    // Unix 平台实现
    // ==========================================
    #[cfg(unix)]
    async fn build_op<P: BufPool>(
        &self,
        path: &Path,
        context: &crate::runtime::context::RuntimeContext<P>,
    ) -> std::io::Result<Open<P>> {
        use std::os::unix::ffi::OsStrExt;

        let path_bytes = path.as_os_str().as_bytes();
        // ensure null termination
        let len = path_bytes.len() + 1;

        let pool = context
            .buffer_pool()
            .upgrade()
            .expect("runtime dropped")
            .as_ref()
            .clone();

        let mut buf = match pool.alloc_mem(len) {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            } => FixedBuf::new(pool.clone(), ptr, cap, global_index, context),
            AllocResult::Failed => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "buf pool exhausted",
                ));
            }
        };

        // Write path + null
        let slice = buf.as_slice_mut();
        if slice.len() < len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "path too long for buffer",
            ));
        }
        slice[..len - 1].copy_from_slice(path_bytes);
        slice[len - 1] = 0;

        buf.set_len(len);

        // 标志位计算
        let mut flags = if self.read && !self.write && !self.append {
            libc::O_RDONLY
        } else if !self.read && self.write && !self.append {
            libc::O_WRONLY
        } else if self.append {
            libc::O_WRONLY | libc::O_APPEND
        } else {
            libc::O_RDWR
        };

        if self.create {
            flags |= libc::O_CREAT;
        }
        if self.create_new {
            flags |= libc::O_EXCL | libc::O_CREAT;
        }
        if self.truncate {
            flags |= libc::O_TRUNC;
        }
        flags |= self.custom_flags;

        Ok(Open {
            path: buf,
            flags,
            mode: self.mode,
        })
    }

    // ==========================================
    // Windows 平台实现
    // ==========================================
    #[cfg(windows)]
    async fn build_op<P: BufPool>(
        &self,
        path: &Path,
        context: &crate::runtime::context::RuntimeContext<P>,
    ) -> std::io::Result<Open<P>> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;

        // 1. Process Path (UTF-16 + Null)
        let path_w: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let len_bytes = path_w.len() * 2;

        let pool = context
            .buffer_pool()
            .upgrade()
            .expect("runtime dropped")
            .as_ref()
            .clone();

        let mut buf = match pool.alloc_mem(len_bytes) {
            AllocResult::Allocated {
                ptr,
                cap,
                global_index,
                context,
            } => FixedBuf::new(pool.clone(), ptr, cap, global_index, context),
            AllocResult::Failed => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "buf pool exhausted",
                ));
            }
        };

        let slice = buf.as_slice_mut();
        if slice.len() < len_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "path too long for buffer",
            ));
        }

        // Copy u16s to u8 buffer
        unsafe {
            std::ptr::copy_nonoverlapping(
                path_w.as_ptr() as *const u8,
                slice.as_mut_ptr(),
                len_bytes,
            );
            buf.set_len(len_bytes);
        }

        // 2. Process Access
        let mut access = 0;
        if self.read {
            access |= GENERIC_READ;
        }
        if self.write {
            access |= GENERIC_WRITE;
        }
        if self.append {
            access |= FILE_APPEND_DATA;
        }

        // 3. Disposition
        const OPEN_EXISTING: u32 = 3;
        const CREATE_NEW: u32 = 1;
        const CREATE_ALWAYS: u32 = 2;
        const OPEN_ALWAYS: u32 = 4;
        const TRUNCATE_EXISTING: u32 = 5;

        let disposition = match (self.create, self.create_new, self.truncate) {
            (_, true, _) => CREATE_NEW,
            (true, _, true) => CREATE_ALWAYS,
            (true, _, false) => OPEN_ALWAYS,
            (false, _, true) => TRUNCATE_EXISTING,
            (false, _, false) => OPEN_EXISTING,
        };

        Ok(Open {
            path: buf,
            flags: access as i32,
            mode: disposition,
        })
    }
}
