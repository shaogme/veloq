use core::hash::{BuildHasher, Hasher};

use crate::{K, fast_hash_bytes};

/// 极速非安全哈希器，适用于完全受信任的内部数据
#[derive(Debug, Clone)]
pub struct FastHasher {
    hash: u64,
}

impl FastHasher {
    /// 使用指定的种子创建一个新的 FastHasher
    #[inline]
    pub const fn new(seed: u64) -> Self {
        Self { hash: seed }
    }
}

impl Default for FastHasher {
    #[inline]
    fn default() -> Self {
        Self::new(0)
    }
}

impl FastHasher {
    #[inline]
    fn add_to_hash(&mut self, i: u64) {
        self.hash = self.hash.wrapping_add(i).wrapping_mul(K);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        const ROTATE: u32 = 26;
        self.hash.rotate_left(ROTATE)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.write_u64(fast_hash_bytes(bytes));
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }

    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.add_to_hash(i as u64);
        self.add_to_hash((i >> 64) as u64);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }
}

/// 支持在 HashMap / HashSet 中直接使用的极速非安全构造器
#[derive(Debug, Clone, Copy, Default)]
pub struct FastBuildHasher;

impl FastBuildHasher {
    /// 创建一个新的 FastBuildHasher
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

impl BuildHasher for FastBuildHasher {
    type Hasher = FastHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        FastHasher::new(0)
    }
}
