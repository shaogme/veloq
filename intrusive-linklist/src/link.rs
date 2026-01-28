use core::cell::UnsafeCell;
use core::ptr::NonNull;

/// 侵入式链表的链接节点，必须嵌入在数据结构中使用。
#[derive(Debug)]
pub struct Link {
    // 使用 UnsafeCell 允许在只有 &Link 引用时修改指针（通常配合外层锁使用）
    pub(crate) next: UnsafeCell<Option<NonNull<Link>>>,
    pub(crate) prev: UnsafeCell<Option<NonNull<Link>>>,
    pub(crate) linked: UnsafeCell<bool>,
}

impl Link {
    pub const fn new() -> Self {
        Self {
            next: UnsafeCell::new(None),
            prev: UnsafeCell::new(None),
            linked: UnsafeCell::new(false),
        }
    }

    /// 检查节点是否链接在某个列表中。
    pub fn is_linked(&self) -> bool {
        unsafe { *self.linked.get() }
    }

    /// 强制断开连接（unsafe，需确保已从列表中移除）
    pub unsafe fn unsafe_unlink(&self) {
        unsafe {
            *self.next.get() = None;
            *self.prev.get() = None;
            *self.linked.get() = false;
        }
    }
}

// 默认实现 Default
impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl Send for Link {}
unsafe impl Sync for Link {}
