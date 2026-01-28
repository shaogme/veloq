use crate::adapter::Adapter;
use crate::cursor::CursorMut;
use crate::link::Link;
use core::marker::PhantomData;
use core::ptr::NonNull;

pub struct LinkedList<A: Adapter> {
    pub(crate) head: Option<NonNull<Link>>,
    pub(crate) tail: Option<NonNull<Link>>,
    pub(crate) adapter: A,
    pub(crate) len: usize,
    _marker: PhantomData<Box<A::Value>>,
}

// 确保 Send/Sync 语义正确，取决于 Value 是否 Send/Sync
unsafe impl<A: Adapter> Send for LinkedList<A> where A::Value: Send {}
unsafe impl<A: Adapter> Sync for LinkedList<A> where A::Value: Sync {}

impl<A: Adapter> LinkedList<A> {
    pub const fn new(adapter: A) -> Self {
        Self {
            head: None,
            tail: None,
            adapter,
            len: 0,
            _marker: PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// 将节点添加到尾部
    pub fn push_back(&mut self, value: NonNull<A::Value>) {
        unsafe {
            let link_ptr = self.adapter.get_link(value);
            let link = link_ptr.as_ref();

            if link.is_linked() {
                panic!("Node is already linked");
            }

            let old_tail = self.tail;
            *link.next.get() = None;
            *link.prev.get() = old_tail;
            *link.linked.get() = true;

            if let Some(tail) = old_tail {
                let tail_link = tail.as_ref();
                *tail_link.next.get() = Some(link_ptr);
            } else {
                self.head = Some(link_ptr);
            }

            self.tail = Some(link_ptr);
            self.len += 1;
        }
    }

    /// 从头部移除节点
    pub fn pop_front(&mut self) -> Option<NonNull<A::Value>> {
        unsafe {
            let head = self.head?;
            // head 是 NonNull<Link>
            let head_link = head.as_ref();
            let next = *head_link.next.get();

            if let Some(next_ptr) = next {
                let next_link = next_ptr.as_ref();
                *next_link.prev.get() = None;
            } else {
                self.tail = None;
            }

            self.head = next;
            self.len -= 1;

            // 清理取出的 link 状态
            head_link.unsafe_unlink();

            Some(self.adapter.get_value(head))
        }
    }

    /// 获取头部 Cursor
    pub fn front_mut(&mut self) -> CursorMut<'_, A> {
        let head = self.head;
        CursorMut::new(self, head)
    }

    /// 根据数据指针创建 Cursor，用于 O(1) 移除。
    ///
    /// # Safety
    /// ptr 必须指向链表中的有效节点。
    pub unsafe fn cursor_mut_from_ptr(&mut self, ptr: NonNull<A::Value>) -> CursorMut<'_, A> {
        let link = unsafe { self.adapter.get_link(ptr) };
        CursorMut::new(self, Some(link))
    }
}

impl<A: Adapter> Drop for LinkedList<A> {
    fn drop(&mut self) {
        // 清理链表防止 Link 数据残留
        while self.pop_front().is_some() {}
    }
}

impl<A: Adapter> core::fmt::Debug for LinkedList<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinkedList")
            .field("len", &self.len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Adapter;
    use crate::link::Link;
    use std::boxed::Box;

    struct TestNode {
        val: i32,
        link: Link,
    }

    struct TestAdapter;

    unsafe impl Adapter for TestAdapter {
        type Value = TestNode;

        unsafe fn get_link(&self, value: NonNull<Self::Value>) -> NonNull<Link> {
            let ptr = value.as_ptr();
            unsafe { NonNull::new_unchecked(core::ptr::addr_of_mut!((*ptr).link)) }
        }

        unsafe fn get_value(&self, link: NonNull<Link>) -> NonNull<Self::Value> {
            let link_ptr = link.as_ptr();
            let val_ptr = crate::container_of!(link_ptr, TestNode, link) as *mut TestNode;
            unsafe { NonNull::new_unchecked(val_ptr) }
        }
    }

    #[test]
    fn test_push_pop() {
        let mut list = LinkedList::new(TestAdapter);
        assert!(list.is_empty());

        let node1 = Box::new(TestNode {
            val: 1,
            link: Link::new(),
        });
        let node2 = Box::new(TestNode {
            val: 2,
            link: Link::new(),
        });

        let ptr1 = NonNull::new(Box::into_raw(node1)).unwrap();
        let ptr2 = NonNull::new(Box::into_raw(node2)).unwrap();

        list.push_back(ptr1);
        list.push_back(ptr2);

        assert_eq!(list.len(), 2);
        assert!(!list.is_empty());

        let popped1 = list.pop_front().unwrap();
        unsafe {
            assert_eq!(popped1.as_ref().val, 1);
            let _ = Box::from_raw(popped1.as_ptr());
        }

        assert_eq!(list.len(), 1);

        let popped2 = list.pop_front().unwrap();
        unsafe {
            assert_eq!(popped2.as_ref().val, 2);
            let _ = Box::from_raw(popped2.as_ptr());
        }

        assert!(list.is_empty());
        assert!(list.pop_front().is_none());
    }

    #[test]
    fn test_drop_cleans_links() {
        // Test that dropping the list unlinks existing nodes, allowing them to be relinked or dropped safely?
        // Actually, drop calls pop_front which calls unsafe_unlink.
        // We should verify that the nodes are still valid but unlinked.

        let mut list = LinkedList::new(TestAdapter);
        let node = Box::new(TestNode {
            val: 10,
            link: Link::new(),
        });
        let ptr = NonNull::new(Box::into_raw(node)).unwrap();

        list.push_back(ptr);
        drop(list);

        unsafe {
            // Node should be unlinked
            let node_ref = ptr.as_ref();
            assert!(!node_ref.link.is_linked());
            // We can now drop the node
            let _ = Box::from_raw(ptr.as_ptr());
        }
    }
}
