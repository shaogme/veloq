use std::alloc::{self, Layout};
use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;

/// Common header for all tasks.
#[repr(C)]
pub struct Header<T, V: 'static> {
    /// State of the task.
    pub state: AtomicUsize,

    /// Reference count.
    pub references: AtomicUsize,

    /// VTable for dynamic dispatch.
    pub vtable: &'static V,

    /// Extended data (Scheduler, or Context fields).
    pub data: T,
}

/// The memory layout of the task allocation.
/// Layout: [ Header ] [ Future ]
#[repr(C)]
pub struct TaskCell<F, T, V: 'static> {
    pub header: Header<T, V>,
    pub future: UnsafeCell<Option<F>>,
}

/// Allocates a new task on the heap.
pub unsafe fn alloc_task<F, T, V: 'static>(
    future: F,
    data: T,
    vtable: &'static V,
    initial_state: usize,
) -> NonNull<Header<T, V>> {
    let layout = Layout::new::<TaskCell<F, T, V>>();
    unsafe {
        let ptr = alloc::alloc(layout) as *mut TaskCell<F, T, V>;
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }

        ptr.write(TaskCell {
            header: Header {
                state: AtomicUsize::new(initial_state),
                references: AtomicUsize::new(1),
                vtable,
                data,
            },
            future: UnsafeCell::new(Some(future)),
        });

        NonNull::new_unchecked(ptr as *mut Header<T, V>)
    }
}

pub unsafe fn dealloc_task<F, T, V: 'static>(ptr: NonNull<Header<T, V>>) {
    unsafe {
        let ptr = ptr.cast::<TaskCell<F, T, V>>().as_ptr();
        let layout = Layout::new::<TaskCell<F, T, V>>();
        alloc::dealloc(ptr as *mut u8, layout);
    }
}

pub unsafe fn drop_future<F, T, V: 'static>(ptr: NonNull<Header<T, V>>) {
    unsafe {
        let raw = ptr.cast::<TaskCell<F, T, V>>().as_ref();
        *raw.future.get() = None;
    }
}

/// Access the future inside the cell.
pub unsafe fn get_future<'a, F, T, V: 'static>(ptr: NonNull<Header<T, V>>) -> &'a mut Option<F> {
    unsafe {
        let raw = ptr.cast::<TaskCell<F, T, V>>().as_ref();
        &mut *raw.future.get()
    }
}
