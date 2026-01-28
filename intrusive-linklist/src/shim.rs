pub mod cell {
    #[cfg(not(feature = "loom"))]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        cell: std::cell::UnsafeCell<T>,
    }

    #[cfg(not(feature = "loom"))]
    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for UnsafeCell<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            unsafe { (*self.cell.get()).fmt(f) }
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<T> UnsafeCell<T> {
        pub const fn new(value: T) -> Self {
            Self {
                cell: std::cell::UnsafeCell::new(value),
            }
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<T: ?Sized> UnsafeCell<T> {
        #[inline(always)]
        pub unsafe fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut T) -> R,
        {
            unsafe { f(&mut *self.cell.get()) }
        }

        #[inline(always)]
        pub unsafe fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&T) -> R,
        {
            unsafe { f(&*self.cell.get()) }
        }
    }

    #[cfg(feature = "loom")]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        inner: loom::cell::UnsafeCell<T>,
    }

    #[cfg(feature = "loom")]
    impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for UnsafeCell<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.inner.get().with(|ptr| unsafe { (*ptr).fmt(f) })
        }
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
        #[inline(always)]
        pub unsafe fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&T) -> R,
        {
            // SAFETY: Caller guarantees safety related to the reference usage.
            // Loom tracks the immutable access duration of the closure.
            self.inner.get().with(|ptr| unsafe { f(&*ptr) })
        }

        #[inline(always)]
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
