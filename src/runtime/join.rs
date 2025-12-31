use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// A handle to a spawned task.
pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

struct JoinState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new() -> (Self, JoinProducer<T>) {
        let state = Rc::new(RefCell::new(JoinState {
            value: None,
            waker: None,
        }));
        (
            Self {
                state: state.clone(),
            },
            JoinProducer { state },
        )
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        if let Some(val) = state.value.take() {
            Poll::Ready(val)
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

pub(crate) struct JoinProducer<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> JoinProducer<T> {
    pub(crate) fn set(self, value: T) {
        let mut state = self.state.borrow_mut();
        state.value = Some(value);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}
