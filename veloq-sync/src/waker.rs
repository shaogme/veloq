use std::marker::PhantomPinned;
use veloq_atomic_waker::AtomicWaker;
use veloq_intrusive_linklist::{Link, intrusive_adapter};

pub struct WaiterNode {
    pub(crate) waker: AtomicWaker,
    pub(crate) link: Link,
    pub(crate) kind: usize,
    _p: PhantomPinned,
}

impl WaiterNode {
    pub fn new() -> Self {
        Self {
            waker: AtomicWaker::new(),
            link: Link::new(),
            kind: 0,
            _p: PhantomPinned,
        }
    }
}

intrusive_adapter!(pub WaiterAdapter = WaiterNode { link: Link });

impl WaiterAdapter {
    pub const NEW: Self = Self;
}
