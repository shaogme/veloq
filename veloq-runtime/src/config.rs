
/// I/O Driver Operation Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// Interrupt driven mode (syscalls + waiting)
    Interrupt,
    /// Polling mode (SQPOLL on Linux, busy-wait on Windows)
    Polling,
}

#[derive(Debug, Clone)]
pub struct UringConfig {
    pub mode: IoMode,
    pub entries: u32,
    pub sqpoll_idle_ms: u32,
}

impl Default for UringConfig {
    fn default() -> Self {
        Self {
            mode: IoMode::Interrupt,
            entries: 1024,
            sqpoll_idle_ms: 2000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IocpConfig {
    pub mode: IoMode,
    pub entries: u32,
}

impl Default for IocpConfig {
    fn default() -> Self {
        Self {
            mode: IoMode::Interrupt,
            entries: 1024,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub uring: UringConfig,
    pub iocp: IocpConfig,
    pub worker_threads: Option<usize>,
    pub direct_io: bool,
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }
}
