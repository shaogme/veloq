use atomic_waker::AtomicWaker;
use intrusive_linklist::{Adapter, Link, container_of, offset_of};
use std::{marker::PhantomPinned, ptr::NonNull};

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

pub struct WaiterAdapter;

impl WaiterAdapter {
    pub const NEW: Self = Self;
}

unsafe impl Adapter for WaiterAdapter {
    type Value = WaiterNode;

    unsafe fn get_value(&self, link: NonNull<Link>) -> NonNull<Self::Value> {
        let ptr = link.as_ptr();
        // container_of macro might use raw pointer arithmetic that creates intermediate invalid refs if not careful,
        // but our implementation uses raw pointers.
        let value_ptr = container_of!(ptr, WaiterNode, link) as *mut WaiterNode;
        unsafe { NonNull::new_unchecked(value_ptr) }
    }

    unsafe fn get_link(&self, value: NonNull<Self::Value>) -> NonNull<Link> {
        let ptr = value.as_ptr() as *mut u8;
        unsafe {
            let link_ptr = ptr.add(offset_of!(WaiterNode, link)) as *mut Link;
            NonNull::new_unchecked(link_ptr)
        }
    }
}
