//! Re-export buffer management types from `veloq-buf`.
//!
//! This module replaces the local implementation with the shared one from `veloq-buf`.

pub use veloq_buf::buffer::{
    AllocError, AllocResult, AnyBufPool, BackingPool, BufPool, BufferConfig, BufferRegion,
    BufferRegistrar, DeallocParams, FixedBuf, PoolSpec, PoolVTable, RegisteredPool,
};

pub use veloq_buf::GlobalMemoryInfo;

// Re-export specific pool implementations if needed by runtime users,
// but usually Runtime uses BufferConfig.
pub use veloq_buf::buffer::{BuddyPool, HybridPool};

pub use veloq_buf::buffer::buddy::BuddySpec;
pub use veloq_buf::buffer::hybrid::HybridSpec;
