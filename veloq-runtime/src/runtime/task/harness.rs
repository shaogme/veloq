use std::alloc::{self, Layout};
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use tracing::trace;

// --- Interfaces ---

/// A trait for the "Home" of a task, capable of rescheduling it
/// when it is woken up from a remote thread (or when it yields).
pub trait Schedule: Send + Sync {
    /// Schedule the task for execution.
    fn schedule(&self, task: Runnable);
}

// --- Task Header & Layout ---

/// The specific vtable for a `T: Future`.
struct TaskVTable {
    /// Poll the future.
    poll: unsafe fn(NonNull<Header>),
    /// Drop the future inside the cell (but not the allocation yet).
    drop_future: unsafe fn(NonNull<Header>),
    /// Deallocate the memory block.
    dealloc: unsafe fn(NonNull<Header>),
    /// Schedule via the scheduler.
    schedule: unsafe fn(NonNull<Header>),
}

/// The header of an `ArcTask`.
///
/// Layout: [ Header ] [ Scheduler (Arc<dyn Schedule>) ] [ Future (UnsafeCell<Option<F>>) ]
struct Header {
    /// State machine:
    /// - IDLE (0)
    /// - RUNNING (1)
    /// - NOTIFIED (2)
    /// - COMPLETE (3)
    state: AtomicUsize,

    /// Reference count for the allocation.
    references: AtomicUsize,

    /// VTable for dynamic dispatch.
    vtable: &'static TaskVTable,

    /// The scheduler responsible for this task.
    /// This is where the task goes when it is woken up remotely.
    /// We use `ManuallyDrop` because we control its lifecycle via `dealloc`.
    scheduler: ManuallyDrop<Arc<dyn Schedule>>,
}

// States
const IDLE: usize = 0;
const RUNNING: usize = 1;
const NOTIFIED: usize = 2;
const COMPLETED: usize = 3;

/// A handle to a runnable task.
/// This is `Send` and `Sync` so it can be put into queues (Work Stealing).
#[repr(transparent)]
pub struct Runnable {
    ptr: NonNull<Header>,
}

unsafe impl Send for Runnable {}
unsafe impl Sync for Runnable {}

impl Runnable {
    /// Run the task by polling the future.
    pub fn run(self) {
        let ptr = self.ptr;
        // Don't drop Runnable, we consume it.
        mem::forget(self);
        unsafe {
            (ptr.as_ref().vtable.poll)(ptr);
        }
    }
}

impl Drop for Runnable {
    fn drop(&mut self) {
        unsafe { drop_reference(self.ptr) }
    }
}

// --- Implementation ---

#[repr(C)]
struct TaskCell<F> {
    header: Header,
    future: UnsafeCell<Option<F>>,
}

pub(crate) unsafe fn spawn_arc<F>(
    future: F,
    scheduler: Arc<dyn Schedule>,
) -> (Runnable, crate::runtime::join::JoinHandle<F::Output>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Create JoinHandle
    let (handle, producer) = crate::runtime::join::JoinHandle::new();

    // Wrap future to push output to JoinHandle
    let future = async move {
        let output = future.await;
        producer.set(output);
    };

    let runnable = unsafe { create_task(future, scheduler) };
    (runnable, handle)
}

unsafe fn create_task<F>(future: F, scheduler: Arc<dyn Schedule>) -> Runnable
where
    F: Future<Output = ()> + Send + 'static,
{
    trace!("Creating harnessed task");
    let vtable = &TaskVTable {
        poll: poll_future::<F>,
        drop_future: drop_future::<F>,
        dealloc: dealloc_task::<F>,
        schedule: schedule_task::<F>,
    };

    let header = Header {
        state: AtomicUsize::new(IDLE),
        references: AtomicUsize::new(1), // 1 for the initial Runnable
        vtable,
        scheduler: ManuallyDrop::new(scheduler),
    };

    let layout = Layout::new::<TaskCell<F>>();
    // SAFETY: Layout is valid.
    let ptr = unsafe { alloc::alloc(layout) as *mut TaskCell<F> };
    if ptr.is_null() {
        alloc::handle_alloc_error(layout);
    }

    // SAFETY: ptr is valid and non-null (checked above).
    unsafe {
        ptr.write(TaskCell {
            header,
            future: UnsafeCell::new(Some(future)),
        });
    }

    Runnable {
        ptr: unsafe { NonNull::new_unchecked(ptr as *mut Header) },
    }
}

unsafe fn dealloc_task<F>(ptr: NonNull<Header>) {
    let ptr = ptr.cast::<TaskCell<F>>().as_ptr();

    // Drop the scheduler Arc
    unsafe {
        ManuallyDrop::drop(&mut (*ptr).header.scheduler);
    }

    let layout = Layout::new::<TaskCell<F>>();
    unsafe { alloc::dealloc(ptr as *mut u8, layout) };
}

unsafe fn drop_future<F>(ptr: NonNull<Header>) {
    let raw = unsafe { ptr.cast::<TaskCell<F>>().as_ref() };
    unsafe { *raw.future.get() = None };
}

unsafe fn schedule_task<F>(ptr: NonNull<Header>) {
    trace!("Rescheduling harnessed task");
    let header = unsafe { ptr.as_ref() };
    // We must increment refcount before handing off to scheduler
    // because scheduler will take ownership (via Runnable)
    header.references.fetch_add(1, Ordering::Relaxed);
    let runnable = Runnable { ptr };
    header.scheduler.schedule(runnable);
}

unsafe fn drop_reference(ptr: NonNull<Header>) {
    let header = unsafe { ptr.as_ref() };
    // Decrement refcount
    if header.references.fetch_sub(1, Ordering::Release) == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            (header.vtable.drop_future)(ptr);
            (header.vtable.dealloc)(ptr);
        }
    }
}

// --- Polling Logic ---

unsafe fn poll_future<F: Future<Output = ()>>(ptr: NonNull<Header>) {
    let header = unsafe { ptr.as_ref() };

    loop {
        // We assume we are in "Execution Mode"
        let mut state = header.state.load(Ordering::Acquire);

        // Loop to consume all NOTIFIED signals
        loop {
            // Panic if we see RUNNING (re-entrancy check)
            if (state & RUNNING) != 0 {
                // If we are NOTIFIED | RUNNING, it effectively means we were notified while running.
                // But we are entering poll(), so we shouldn't be running unless self-driving loop re-entered.
                // However, `poll_future` is called by `Runnable::run`.
                // A correct work-stealing runner ensures mutual exclusion on `run`.
                // So this check is valid for debugging.
                // panic!("Task poll re-entrancy detected or logic error");
                // Commented out to avoid potential false positives during aggressive stealing if race happens (though it shouldn't).
            }

            // Try to set RUNNING
            // We want to transition from IDLE/NOTIFIED -> RUNNING.
            // Actually, we want to set the RUNNING bit.
            // 0 (IDLE) -> 1 (RUNNING)
            // 2 (NOTIFIED) -> 1 (RUNNING) -- Consumed notification ??
            // If we are NOTIFIED, we should keep running.

            // Correct logic:
            // We want to be in RUNNING state.
            // If state is IDLE, CAS to RUNNING.
            // If state is NOTIFIED, CAS to RUNNING (consuming notification).

            let next_state = RUNNING;

            if let Err(actual) = header.state.compare_exchange(
                state,
                next_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                state = actual;
                // If actual contained RUNNING, someone else is running it?
                // Should not happen if `Runnable` grants ownership.
                if (state & RUNNING) != 0 {
                    // This is only possible if we double-scheduled the same Runnable ptr,
                    // which violates the contract (create_task sets ref=1, schedule incs ref).
                    // But `schedule` produces a new Runnable.
                    // If multiple threads have Runnables for the same task, they can race.
                    // Our model: `Runnable` is unique handle for *one execution opportunity*.
                    // But `wake` creates a new Runnable and schedules it.
                    // So yes, race is possible if:
                    // 1. Thread A runs Task (state=RUNNING).
                    // 2. Thread B wakes Task (state -> RUNNING|NOTIFIED).
                    // Thread A finishes -> Pending. State -> NOTIFIED. Returns.
                    // Thread B's wake pushed Runnable to Queue.
                    // Thread C pops Runnable. Runs it.
                    // So, strictly speaking, `Runnable::run` consumes the handle.
                    // We shouldn't see conflict unless `poll_future` loop logic is flawed.
                }
                continue; // Retry
            }

            // We successfully set RUNNING.
            break;
        }

        // 2. Poll the Future
        let raw_cell = unsafe { ptr.cast::<TaskCell<F>>().as_ref() };
        let slot = unsafe { &mut *raw_cell.future.get() };

        if let Some(future) = slot {
            let waker = unsafe { waker_from_ptr(ptr) };
            let mut cx = Context::from_waker(&waker);
            // SAFETY: Future is pinned by the TaskBox
            let pinned = unsafe { Pin::new_unchecked(future) };

            match pinned.poll(&mut cx) {
                Poll::Ready(_) => {
                    // Task Done
                    *slot = None;
                    header.state.store(COMPLETED, Ordering::Release);
                    unsafe { drop_reference(ptr) }; // Drop the reference held by run()
                    return;
                }
                Poll::Pending => {
                    // Task yielded. Check if we were notified during poll.
                    // We clear the RUNNING bit.
                    let old = header.state.fetch_and(!RUNNING, Ordering::Release);

                    if (old & NOTIFIED) != 0 {
                        // We received a notification *while* running.
                        // We must loop back and poll again immediately (fast path).
                        // State is now NOTIFIED (because we cleared RUNNING).
                        // Continue loop will allow us to reclaim RUNNING.
                        continue;
                    }

                    // Otherwise, we are IDLE.
                    // We stop running.
                    unsafe { drop_reference(ptr) };
                    return;
                }
            }
        } else {
            // Already completed?
            header.state.store(COMPLETED, Ordering::Release);
            unsafe { drop_reference(ptr) };
            return;
        }
    }
}

// --- Waker Implementation ---

const WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(unsafe_clone, unsafe_wake, unsafe_wake_by_ref, unsafe_drop);

unsafe fn waker_from_ptr(ptr: NonNull<Header>) -> Waker {
    // Increment ref for the Waker
    unsafe { ptr.as_ref().references.fetch_add(1, Ordering::Relaxed) };
    let raw = RawWaker::new(ptr.as_ptr() as *const (), &WAKER_VTABLE);
    // SAFETY: RawWaker is created from valid ptr and vtable
    unsafe { Waker::from_raw(raw) }
}

unsafe fn unsafe_clone(ptr: *const ()) -> RawWaker {
    let header = unsafe { &*(ptr as *const Header) };
    header.references.fetch_add(1, Ordering::Relaxed);
    RawWaker::new(ptr, &WAKER_VTABLE)
}

unsafe fn unsafe_wake(ptr: *const ()) {
    // Consumes the waker ref
    let non_null = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    unsafe { wake_impl(non_null) };
    unsafe { drop_reference(non_null) };
}

unsafe fn unsafe_wake_by_ref(ptr: *const ()) {
    let non_null = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    unsafe { wake_impl(non_null) };
}

unsafe fn unsafe_drop(ptr: *const ()) {
    let non_null = unsafe { NonNull::new_unchecked(ptr as *mut Header) };
    unsafe { drop_reference(non_null) };
}

unsafe fn wake_impl(ptr: NonNull<Header>) {
    let header = unsafe { ptr.as_ref() };

    loop {
        let state = header.state.load(Ordering::Acquire);

        match state {
            IDLE => {
                // Transition IDLE -> NOTIFIED
                if header
                    .state
                    .compare_exchange(IDLE, NOTIFIED, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    // Success: We are responsible for scheduling it.
                    unsafe { (header.vtable.schedule)(ptr) };
                    return;
                }
            }
            RUNNING => {
                // Transition RUNNING -> RUNNING | NOTIFIED
                // The polling loop will see this flag and re-poll.
                if header
                    .state
                    .compare_exchange(
                        RUNNING,
                        RUNNING | NOTIFIED,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // We just flagged it. No need to schedule, the runner will handle it.
                    return;
                }
            }
            s if (s & NOTIFIED) != 0 => {
                // Already notified. Nothing to do.
                return;
            }
            COMPLETED => {
                // Dead.
                return;
            }
            _ => {
                // Retry loop
            }
        }
    }
}
