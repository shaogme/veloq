use crate::io::op::IoResources;
use slab::Slab;
use std::io;
use std::ops::{Index, IndexMut};
use std::task::{Context, Poll, Waker};

pub struct OpEntry<P> {
    pub waker: Option<Waker>,
    pub result: Option<io::Result<u32>>,
    pub resources: IoResources,
    pub cancelled: bool,
    #[allow(dead_code)]
    pub platform_data: P,
}

impl<P> OpEntry<P> {
    pub fn new(resources: IoResources, platform_data: P) -> Self {
        Self {
            waker: None,
            result: None,
            resources,
            cancelled: false,
            platform_data,
        }
    }
}

pub struct OpRegistry<P> {
    slab: Slab<OpEntry<P>>,
}

impl<P> OpRegistry<P> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slab: Slab::with_capacity(capacity),
        }
    }

    pub fn insert(&mut self, entry: OpEntry<P>) -> usize {
        self.slab.insert(entry)
    }

    pub fn get_mut(&mut self, user_data: usize) -> Option<&mut OpEntry<P>> {
        self.slab.get_mut(user_data)
    }

    pub fn contains(&self, user_data: usize) -> bool {
        self.slab.contains(user_data)
    }

    #[allow(dead_code)]
    pub fn remove(&mut self, user_data: usize) -> OpEntry<P> {
        self.slab.remove(user_data)
    }

    pub fn is_empty(&self) -> bool {
        self.slab.is_empty()
    }

    pub fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<u32>, IoResources)> {
        if let Some(op) = self.slab.get_mut(user_data) {
            if let Some(res) = op.result.take() {
                let entry = self.slab.remove(user_data);
                Poll::Ready((res, entry.resources))
            } else {
                op.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        } else {
            Poll::Ready((
                Err(io::Error::new(io::ErrorKind::Other, "Op not found")),
                IoResources::None,
            ))
        }
    }
}

impl<P> Index<usize> for OpRegistry<P> {
    type Output = OpEntry<P>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.slab[index]
    }
}

impl<P> IndexMut<usize> for OpRegistry<P> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.slab[index]
    }
}
