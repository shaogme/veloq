use std::path::Path;
use crate::io::op::{Op, Open};

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
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

    /// 对外的公共 API：清晰、线性、无平台噪音
    pub async fn open(&self, path: impl AsRef<Path>) -> std::io::Result<super::file::File> {
        // 1. 根据不同平台生成对应的 Op 参数
        let op = self.build_op(path.as_ref())?;

        // 2. 提交给 runtime (这一部分没有任何变化，但现在看起来更显眼了)
        let driver = crate::runtime::current_driver();
        let (res, _) = Op::new(op, driver).await;

        // 3. 转换结果
        let fd = res? as crate::io::op::SysRawOp;
        use crate::io::op::IoFd;
        Ok(super::file::File { fd: IoFd::Raw(fd) })
    }

    // ==========================================
    // Unix 平台实现
    // ==========================================
    #[cfg(unix)]
    fn build_op(&self, path: &Path) -> std::io::Result<Open> {
        use std::os::unix::ffi::OsStrExt;

        // 路径转换
        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())?;

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

        Ok(Open {
            path: path_c,
            flags,
            mode: self.mode,
        })
    }

    // ==========================================
    // Windows 平台实现
    // ==========================================
    #[cfg(windows)]
    fn build_op(&self, path: &Path) -> std::io::Result<Open> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;

        // 1. 处理路径
        let mut path_w: Vec<u16> = path.as_os_str().encode_wide().collect();
        path_w.push(0);

        // 2. 处理 Access (读写权限)
        let mut access = 0;
        if self.read {
            access |= GENERIC_READ;
        }
        if self.write {
            access |= GENERIC_WRITE;
        }
        if self.append {
            access |= FILE_APPEND_DATA;
        } // 注意: append在windows下比较特殊

        // 3. Disposition (创建/打开策略) - 使用 match 模式匹配更清晰
        const OPEN_EXISTING: u32 = 3;
        const CREATE_NEW: u32 = 1;
        const CREATE_ALWAYS: u32 = 2;
        const OPEN_ALWAYS: u32 = 4;
        const TRUNCATE_EXISTING: u32 = 5;

        let disposition = match (self.create, self.create_new, self.truncate) {
            (_, true, _) => CREATE_NEW,            // create_new 优先级最高
            (true, _, true) => CREATE_ALWAYS,      // create + truncate = 覆盖创建
            (true, _, false) => OPEN_ALWAYS,       // create = 不存在建，存在开
            (false, _, true) => TRUNCATE_EXISTING, // truncate = 必须存在并截断
            (false, _, false) => OPEN_EXISTING,    // 默认 = 必须存在
        };

        Ok(Open {
            path: path_w,
            flags: access as i32, // 对应 Win32 dwDesiredAccess
            mode: disposition,    // 对应 Win32 dwCreationDisposition
        })
    }
}
