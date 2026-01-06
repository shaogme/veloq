use super::stable_slab::StableSlab;
use crate::io::driver::PlatformOp;
use std::io;
use std::ops::{Index, IndexMut};
use std::task::{Context, Poll, Waker};

pub struct OpEntry<Op: PlatformOp, P> {
    pub waker: Option<Waker>,
    pub result: Option<io::Result<usize>>,
    pub resources: Option<Op>,
    pub cancelled: bool,
    #[allow(dead_code)]
    pub platform_data: P,
}

impl<Op: PlatformOp, P> OpEntry<Op, P> {
    pub fn new(resources: Option<Op>, platform_data: P) -> Self {
        Self {
            waker: None,
            result: None,
            resources,
            cancelled: false,
            platform_data,
        }
    }
}

pub struct OpRegistry<Op: PlatformOp, P> {
    slab: StableSlab<OpEntry<Op, P>>,
}

impl<Op: PlatformOp, P> OpRegistry<Op, P> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slab: StableSlab::with_capacity(capacity),
        }
    }

    pub fn insert(&mut self, entry: OpEntry<Op, P>) -> usize {
        self.slab.insert(entry)
    }

    pub fn get_mut(&mut self, user_data: usize) -> Option<&mut OpEntry<Op, P>> {
        self.slab.get_mut(user_data)
    }

    pub fn contains(&self, user_data: usize) -> bool {
        self.slab.contains(user_data)
    }

    #[allow(dead_code)]
    pub fn remove(&mut self, user_data: usize) -> OpEntry<Op, P> {
        self.slab.remove(user_data)
    }
    
    #[cfg(target_os = "windows")]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut OpEntry<Op, P>)> {
        self.slab.iter_mut()
    }

    pub fn poll_op(
        &mut self,
        user_data: usize,
        cx: &mut Context<'_>,
    ) -> Poll<(io::Result<usize>, Op)> {
        if let Some(op) = self.slab.get_mut(user_data) {
            if let Some(res) = op.result.take() {
                let mut entry = self.slab.remove(user_data);
                // Resources should be present if we have a result (implying submission happened)
                // If None, it means logic error.
                let resources = entry
                    .resources
                    .take()
                    .expect("Op completed but resources missing");
                Poll::Ready((res, resources))
            } else {
                op.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        } else {
            // Op not found. Can't return DriverOp.
            // This poll signature assumes we always return Op back.
            // If we can't find it, we can't return Op.
            // In previous code `IoResources::None` was returned.
            panic!("Op not found in registry and no None variant available");
        }
    }
}

impl<Op: PlatformOp, P> Index<usize> for OpRegistry<Op, P> {
    type Output = OpEntry<Op, P>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.slab[index]
    }
}

impl<Op: PlatformOp, P> IndexMut<usize> for OpRegistry<Op, P> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.slab[index]
    }
}
