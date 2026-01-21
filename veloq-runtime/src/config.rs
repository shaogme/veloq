use std::time::Duration;

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

#[derive(Debug, Clone)]
pub struct BlockingPoolConfig {
    pub core_threads: usize,
    pub max_threads: usize,
    pub queue_capacity: usize,
    pub keep_alive: Duration,
}

impl Default for BlockingPoolConfig {
    fn default() -> Self {
        Self {
            core_threads: 16,
            max_threads: 512,
            queue_capacity: 10000,
            keep_alive: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub uring: UringConfig,
    pub iocp: IocpConfig,
    pub worker_threads: Option<usize>,
    pub direct_io: bool,
    pub blocking_pool: BlockingPoolConfig,
    pub internal_queue_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uring: UringConfig::default(),
            iocp: IocpConfig::default(),
            worker_threads: None,
            direct_io: false,
            blocking_pool: BlockingPoolConfig::default(),
            internal_queue_capacity: 1024,
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn uring(self, uring: UringConfig) -> Self {
        Self { uring, ..self }
    }

    pub fn iocp(self, iocp: IocpConfig) -> Self {
        Self { iocp, ..self }
    }

    pub fn worker_threads(self, worker_threads: usize) -> Self {
        Self {
            worker_threads: Some(worker_threads),
            ..self
        }
    }

    pub fn direct_io(self, direct_io: bool) -> Self {
        Self { direct_io, ..self }
    }

    pub fn internal_queue_capacity(self, capacity: usize) -> Self {
        Self {
            internal_queue_capacity: capacity,
            ..self
        }
    }

    pub fn blocking_pool(self, blocking_pool: BlockingPoolConfig) -> Self {
        Self {
            blocking_pool,
            ..self
        }
    }
}
