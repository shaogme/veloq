use atomic_waker::AtomicWaker;
use intrusive_collections::{
    Adapter, LinkedListLink, PointerOps, container_of, linked_list::LinkOps as ListOps, offset_of,
};
use std::{marker::PhantomPinned, ptr::NonNull};

pub struct WaiterNode {
    pub(crate) waker: AtomicWaker,
    pub(crate) link: LinkedListLink,
    _p: PhantomPinned,
}

impl WaiterNode {
    pub fn new() -> Self {
        Self {
            waker: AtomicWaker::new(),
            link: LinkedListLink::new(),
            _p: PhantomPinned,
        }
    }
}

pub struct WaiterAdapter {
    pointer_ops: WaiterPointerOps,
    link_ops: ListOps,
}

impl WaiterAdapter {
    pub const NEW: Self = Self {
        pointer_ops: WaiterPointerOps,
        link_ops: ListOps,
    };
}

unsafe impl Adapter for WaiterAdapter {
    type LinkOps = ListOps;
    type PointerOps = WaiterPointerOps;

    unsafe fn get_value(
        &self,
        link: <Self::LinkOps as intrusive_collections::LinkOps>::LinkPtr,
    ) -> *const <Self::PointerOps as PointerOps>::Value {
        container_of!(link.as_ptr(), WaiterNode, link)
    }

    unsafe fn get_link(
        &self,
        value: *const <Self::PointerOps as PointerOps>::Value,
    ) -> <Self::LinkOps as intrusive_collections::LinkOps>::LinkPtr {
        unsafe {
            let ptr = (value as *const u8).add(offset_of!(WaiterNode, link));
            NonNull::new_unchecked(ptr as *mut _)
        }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointer_ops
    }
}

pub struct WaiterPointerOps;

unsafe impl PointerOps for WaiterPointerOps {
    type Value = WaiterNode;
    type Pointer = NonNull<WaiterNode>;

    unsafe fn from_raw(&self, value: *const Self::Value) -> Self::Pointer {
        NonNull::new(value as *mut _).expect("Pointer cannot be null")
    }

    fn into_raw(&self, ptr: Self::Pointer) -> *const Self::Value {
        ptr.as_ptr()
    }
}
