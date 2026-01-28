#[cfg(not(feature = "loom"))]
pub mod atomic {
    pub use core::sync::atomic::Ordering;

    #[derive(Debug, Default)]
    #[repr(transparent)]
    pub struct AtomicUsize {
        inner: core::sync::atomic::AtomicUsize,
    }

    impl From<usize> for AtomicUsize {
        fn from(v: usize) -> Self {
            Self::new(v)
        }
    }

    impl AtomicUsize {
        pub const fn new(v: usize) -> Self {
            Self {
                inner: core::sync::atomic::AtomicUsize::new(v),
            }
        }

        pub fn swap(&self, val: usize, order: Ordering) -> usize {
            self.inner.swap(val, order)
        }

        pub fn compare_exchange(
            &self,
            current: usize,
            new: usize,
            success: Ordering,
            failure: Ordering,
        ) -> Result<usize, usize> {
            self.inner.compare_exchange(current, new, success, failure)
        }

        pub fn fetch_and(&self, val: usize, order: Ordering) -> usize {
            self.inner.fetch_and(val, order)
        }

        pub fn fetch_or(&self, val: usize, order: Ordering) -> usize {
            self.inner.fetch_or(val, order)
        }
    }
}

#[cfg(feature = "loom")]
pub mod atomic {
    pub use loom::sync::atomic::*;
}

pub mod cell {
    #[cfg(not(feature = "loom"))]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        cell: core::cell::UnsafeCell<T>,
    }

    #[cfg(not(feature = "loom"))]
    impl<T> UnsafeCell<T> {
        pub const fn new(value: T) -> Self {
            Self {
                cell: core::cell::UnsafeCell::new(value),
            }
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<T: ?Sized> UnsafeCell<T> {
        pub unsafe fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut T) -> R,
        {
            unsafe { f(&mut *self.cell.get()) }
        }
    }

    #[cfg(feature = "loom")]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        inner: loom::cell::UnsafeCell<T>,
    }

    #[cfg(feature = "loom")]
    impl<T> UnsafeCell<T> {
        pub fn new(data: T) -> Self {
            Self {
                inner: loom::cell::UnsafeCell::new(data),
            }
        }
    }

    #[cfg(feature = "loom")]
    impl<T: ?Sized> UnsafeCell<T> {
        pub unsafe fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut T) -> R,
        {
            // SAFETY: Caller guarantees safety related to the reference usage.
            // Loom tracks the mutable access duration of the closure.
            self.inner.get_mut().with(|ptr| unsafe { f(&mut *ptr) })
        }
    }
}
