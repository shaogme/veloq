#![allow(dead_code)]

#[cfg(not(feature = "loom"))]
pub mod atomic {
    pub use std::sync::atomic::{
        AtomicBool as StdAtomicBool, AtomicI8 as StdAtomicI8, AtomicI16 as StdAtomicI16,
        AtomicI32 as StdAtomicI32, AtomicI64 as StdAtomicI64, AtomicIsize as StdAtomicIsize,
        AtomicPtr as StdAtomicPtr, AtomicU8 as StdAtomicU8, AtomicU16 as StdAtomicU16,
        AtomicU32 as StdAtomicU32, AtomicU64 as StdAtomicU64, AtomicUsize as StdAtomicUsize,
        Ordering, fence,
    };

    macro_rules! impl_atomic {
        ($name:ident, $inner:ty, $std_name:ident) => {
            #[derive(Debug, Default)]
            #[repr(transparent)]
            pub struct $name {
                inner: $std_name,
            }

            impl From<$inner> for $name {
                fn from(v: $inner) -> Self {
                    Self::new(v)
                }
            }

            impl $name {
                pub const fn new(v: $inner) -> Self {
                    Self {
                        inner: $std_name::new(v),
                    }
                }

                pub fn get_mut(&mut self) -> &mut $inner {
                    self.inner.get_mut()
                }

                pub fn into_inner(self) -> $inner {
                    self.inner.into_inner()
                }

                pub fn load(&self, order: Ordering) -> $inner {
                    self.inner.load(order)
                }

                pub fn store(&self, val: $inner, order: Ordering) {
                    self.inner.store(val, order)
                }

                pub fn swap(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.swap(val, order)
                }

                pub fn compare_exchange(
                    &self,
                    current: $inner,
                    new: $inner,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$inner, $inner> {
                    self.inner.compare_exchange(current, new, success, failure)
                }

                pub fn compare_exchange_weak(
                    &self,
                    current: $inner,
                    new: $inner,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$inner, $inner> {
                    self.inner
                        .compare_exchange_weak(current, new, success, failure)
                }

                pub fn with_mut<R, F: FnOnce(&mut $inner) -> R>(&mut self, f: F) -> R {
                    f(self.inner.get_mut())
                }
            }
        };
    }

    macro_rules! impl_atomic_int {
        ($name:ident, $inner:ty, $std_name:ident) => {
            impl_atomic!($name, $inner, $std_name);
            impl $name {
                pub fn fetch_add(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_add(val, order)
                }
                pub fn fetch_sub(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_sub(val, order)
                }
                pub fn fetch_and(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_and(val, order)
                }
                pub fn fetch_nand(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_nand(val, order)
                }
                pub fn fetch_or(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_or(val, order)
                }
                pub fn fetch_xor(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_xor(val, order)
                }
                pub fn fetch_max(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_max(val, order)
                }
                pub fn fetch_min(&self, val: $inner, order: Ordering) -> $inner {
                    self.inner.fetch_min(val, order)
                }
            }
        };
    }

    impl_atomic!(AtomicBool, bool, StdAtomicBool);
    impl AtomicBool {
        pub fn fetch_and(&self, val: bool, order: Ordering) -> bool {
            self.inner.fetch_and(val, order)
        }
        pub fn fetch_nand(&self, val: bool, order: Ordering) -> bool {
            self.inner.fetch_nand(val, order)
        }
        pub fn fetch_or(&self, val: bool, order: Ordering) -> bool {
            self.inner.fetch_or(val, order)
        }
        pub fn fetch_xor(&self, val: bool, order: Ordering) -> bool {
            self.inner.fetch_xor(val, order)
        }
    }

    impl_atomic_int!(AtomicI8, i8, StdAtomicI8);
    impl_atomic_int!(AtomicU8, u8, StdAtomicU8);
    impl_atomic_int!(AtomicI16, i16, StdAtomicI16);
    impl_atomic_int!(AtomicU16, u16, StdAtomicU16);
    impl_atomic_int!(AtomicI32, i32, StdAtomicI32);
    impl_atomic_int!(AtomicU32, u32, StdAtomicU32);
    impl_atomic_int!(AtomicI64, i64, StdAtomicI64);
    impl_atomic_int!(AtomicU64, u64, StdAtomicU64);
    impl_atomic_int!(AtomicIsize, isize, StdAtomicIsize);
    impl_atomic_int!(AtomicUsize, usize, StdAtomicUsize);

    #[derive(Debug)]
    #[repr(transparent)]
    pub struct AtomicPtr<T> {
        inner: StdAtomicPtr<T>,
    }

    impl<T> Default for AtomicPtr<T> {
        fn default() -> Self {
            Self::new(std::ptr::null_mut())
        }
    }

    impl<T> From<*mut T> for AtomicPtr<T> {
        fn from(p: *mut T) -> Self {
            Self::new(p)
        }
    }

    impl<T> AtomicPtr<T> {
        pub const fn new(p: *mut T) -> Self {
            Self {
                inner: StdAtomicPtr::new(p),
            }
        }
        pub fn get_mut(&mut self) -> &mut *mut T {
            self.inner.get_mut()
        }
        pub fn into_inner(self) -> *mut T {
            self.inner.into_inner()
        }
        pub fn load(&self, order: Ordering) -> *mut T {
            self.inner.load(order)
        }
        pub fn store(&self, ptr: *mut T, order: Ordering) {
            self.inner.store(ptr, order)
        }
        pub fn swap(&self, ptr: *mut T, order: Ordering) -> *mut T {
            self.inner.swap(ptr, order)
        }
        pub fn compare_exchange(
            &self,
            current: *mut T,
            new: *mut T,
            success: Ordering,
            failure: Ordering,
        ) -> Result<*mut T, *mut T> {
            self.inner.compare_exchange(current, new, success, failure)
        }
        pub fn compare_exchange_weak(
            &self,
            current: *mut T,
            new: *mut T,
            success: Ordering,
            failure: Ordering,
        ) -> Result<*mut T, *mut T> {
            self.inner
                .compare_exchange_weak(current, new, success, failure)
        }
        pub fn with_mut<R, F: FnOnce(&mut *mut T) -> R>(&mut self, f: F) -> R {
            f(self.inner.get_mut())
        }
    }
}

#[cfg(feature = "loom")]
pub mod atomic {
    pub use loom::sync::atomic::*;
}

#[cfg(not(feature = "loom"))]
pub use std::sync::Arc;

#[cfg(feature = "loom")]
pub use loom::sync::Arc;

pub mod queue {
    #[cfg(not(feature = "loom"))]
    pub use crossbeam_queue::{ArrayQueue, SegQueue};

    #[cfg(feature = "loom")]
    pub use self::loom_queues::{ArrayQueue, SegQueue};

    #[cfg(feature = "loom")]
    mod loom_queues {
        use loom::sync::Mutex;
        use std::collections::VecDeque;

        pub struct SegQueue<T> {
            inner: Mutex<VecDeque<T>>,
        }

        impl<T> SegQueue<T> {
            pub fn new() -> Self {
                Self {
                    inner: Mutex::new(VecDeque::new()),
                }
            }

            pub fn push(&self, t: T) {
                self.inner.lock().unwrap().push_back(t);
            }

            pub fn pop(&self) -> Option<T> {
                self.inner.lock().unwrap().pop_front()
            }

            pub fn is_empty(&self) -> bool {
                self.inner.lock().unwrap().is_empty()
            }
        }

        pub struct ArrayQueue<T> {
            inner: Mutex<VecDeque<T>>,
            cap: usize,
        }

        impl<T> ArrayQueue<T> {
            pub fn new(cap: usize) -> Self {
                Self {
                    inner: Mutex::new(VecDeque::new()),
                    cap,
                }
            }

            pub fn push(&self, t: T) -> Result<(), T> {
                let mut lock = self.inner.lock().unwrap();
                if lock.len() >= self.cap {
                    return Err(t);
                }
                lock.push_back(t);
                Ok(())
            }

            pub fn pop(&self) -> Option<T> {
                self.inner.lock().unwrap().pop_front()
            }

            pub fn is_full(&self) -> bool {
                self.inner.lock().unwrap().len() >= self.cap
            }
        }
    }
}

pub mod cell {
    #[cfg(not(feature = "loom"))]
    pub struct UnsafeCell<T: ?Sized> {
        cell: std::cell::UnsafeCell<T>,
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
        pub unsafe fn with_mut<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut T) -> R,
        {
            unsafe { f(&mut *self.cell.get()) }
        }

        pub unsafe fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&T) -> R,
        {
            unsafe { f(&*self.cell.get()) }
        }

        pub fn get(&self) -> *const T {
            self.cell.get() as *const T
        }

        pub fn get_mut(&self) -> *mut T {
            self.cell.get()
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<T> UnsafeCell<T> {
        pub fn into_inner(self) -> T {
            self.cell.into_inner()
        }
    }

    #[cfg(feature = "loom")]
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

        pub fn into_inner(self) -> T {
            self.inner.into_inner()
        }
    }

    #[cfg(feature = "loom")]
    impl<T: ?Sized> UnsafeCell<T> {
        pub fn get(&self) -> *const T {
            self.inner.get().with(|ptr| ptr)
        }

        pub fn get_mut(&self) -> *mut T {
            self.inner.get_mut().with(|ptr| ptr)
        }

        pub unsafe fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&T) -> R,
        {
            // SAFETY: Caller guarantees safety related to the reference usage.
            // Loom tracks the immutable access duration of the closure.
            self.inner.get().with(|ptr| unsafe { f(&*ptr) })
        }

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

pub mod lock {
    #[cfg(not(feature = "loom"))]
    use super::atomic::{AtomicBool, Ordering};

    #[cfg(not(feature = "loom"))]
    pub struct SpinLock<T> {
        locked: AtomicBool,
        data: std::cell::UnsafeCell<T>,
    }

    #[cfg(not(feature = "loom"))]
    unsafe impl<T: Send> Send for SpinLock<T> {}
    #[cfg(not(feature = "loom"))]
    unsafe impl<T: Send> Sync for SpinLock<T> {}

    #[cfg(not(feature = "loom"))]
    impl<T> SpinLock<T> {
        pub fn new(data: T) -> Self {
            Self {
                locked: AtomicBool::new(false),
                data: std::cell::UnsafeCell::new(data),
            }
        }

        pub fn lock(&self) -> SpinLockGuard<'_, T> {
            let backoff = crossbeam_utils::Backoff::new();
            while self.locked.swap(true, Ordering::Acquire) {
                backoff.snooze();
            }
            SpinLockGuard { lock: self }
        }
    }

    #[cfg(not(feature = "loom"))]
    pub struct SpinLockGuard<'a, T> {
        lock: &'a SpinLock<T>,
    }

    #[cfg(not(feature = "loom"))]
    impl<'a, T> Drop for SpinLockGuard<'a, T> {
        fn drop(&mut self) {
            self.lock.locked.store(false, Ordering::Release);
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<'a, T> std::ops::Deref for SpinLockGuard<'a, T> {
        type Target = T;
        fn deref(&self) -> &Self::Target {
            unsafe { &*self.lock.data.get() }
        }
    }

    #[cfg(not(feature = "loom"))]
    impl<'a, T> std::ops::DerefMut for SpinLockGuard<'a, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            unsafe { &mut *self.lock.data.get() }
        }
    }

    // --- Loom Friendly SpinLock Replacement ---
    #[cfg(feature = "loom")]
    pub struct SpinLock<T> {
        inner: loom::sync::Mutex<T>,
    }

    #[cfg(feature = "loom")]
    impl<T> SpinLock<T> {
        pub fn new(data: T) -> Self {
            Self {
                inner: loom::sync::Mutex::new(data),
            }
        }

        pub fn lock(&self) -> loom::sync::MutexGuard<'_, T> {
            self.inner.lock().unwrap()
        }
    }
}
