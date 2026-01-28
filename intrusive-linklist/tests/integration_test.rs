use intrusive_linklist::{Adapter, Link, LinkedList};
use std::boxed::Box;
use std::ptr::NonNull;

struct MyNode {
    id: usize,
    link: Link,
}

struct MyAdapter;

unsafe impl Adapter for MyAdapter {
    type Value = MyNode;

    unsafe fn get_link(&self, value: NonNull<MyNode>) -> NonNull<Link> {
        let ptr = value.as_ptr();
        unsafe {
            // Address of the link field
            NonNull::new_unchecked(std::ptr::addr_of_mut!((*ptr).link))
        }
    }

    unsafe fn get_value(&self, link: NonNull<Link>) -> NonNull<MyNode> {
        let link_ptr = link.as_ptr();
        // Use standard library offset_of if available
        let offset = core::mem::offset_of!(MyNode, link); // stable in recent Rust
        unsafe {
            let val_ptr = (link_ptr as *mut u8).sub(offset) as *mut MyNode;
            NonNull::new_unchecked(val_ptr)
        }
    }
}

#[test]
fn test_integration_flow() {
    let mut list = LinkedList::new(MyAdapter);

    // Create nodes
    let nodes: Vec<_> = (0..5)
        .map(|i| {
            Box::new(MyNode {
                id: i,
                link: Link::new(),
            })
        })
        .collect();

    // Push all
    for node in nodes {
        let ptr = Box::into_raw(node);
        unsafe {
            list.push_back(NonNull::new_unchecked(ptr));
        }
    }

    assert_eq!(list.len(), 5);

    // Verify order and remove
    let mut count = 0;
    while let Some(ptr) = list.pop_front() {
        unsafe {
            let node_ref = ptr.as_ref();
            assert_eq!(node_ref.id, count);
            count += 1;
            // Clean up memory
            let _ = Box::from_raw(ptr.as_ptr());
        }
    }

    assert_eq!(count, 5);
    assert!(list.is_empty());
}

#[test]
fn test_cursor_integration() {
    let mut list = LinkedList::new(MyAdapter);
    let node1 = Box::into_raw(Box::new(MyNode {
        id: 100,
        link: Link::new(),
    }));
    let node2 = Box::into_raw(Box::new(MyNode {
        id: 200,
        link: Link::new(),
    }));
    let node3 = Box::into_raw(Box::new(MyNode {
        id: 300,
        link: Link::new(),
    }));

    unsafe {
        list.push_back(NonNull::new_unchecked(node1));
        list.push_back(NonNull::new_unchecked(node2));
        list.push_back(NonNull::new_unchecked(node3));
    }

    let mut cursor = list.front_mut();

    // Find 200 and remove
    while let Some(node) = cursor.get() {
        if unsafe { node.as_ref().id } == 200 {
            let removed = cursor.remove().unwrap();
            unsafe {
                assert_eq!(removed.as_ref().id, 200);
                let _ = Box::from_raw(removed.as_ptr());
            }
            // After removal, cursor points to 300
        } else {
            cursor.move_next();
        }
    }

    drop(cursor);
    assert_eq!(list.len(), 2);

    let v1 = list.pop_front().unwrap();
    let v2 = list.pop_front().unwrap();

    unsafe {
        assert_eq!(v1.as_ref().id, 100);
        assert_eq!(v2.as_ref().id, 300);
        let _ = Box::from_raw(v1.as_ptr());
        let _ = Box::from_raw(v2.as_ptr());
    }
}
