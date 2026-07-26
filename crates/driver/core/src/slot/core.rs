use super::{Generation, SlotCompletion, SlotError, SlotPayload, SlotSidecarData, SlotSpec};
use crate::{
    DriverResult,
    driver::{CompletionCleanupGuard, UserCompletionEvent},
};
use bilge::prelude::*;
use std::{
    fmt::{self, Debug},
    marker::PhantomData,
};
use veloq_std::sync::{
    Mutex,
    atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};
use veloq_waker::AtomicWaker;

/// slot 的**生命周期**状态，与编译期类型态（[`crate::slot::Reserved`] /
/// [`crate::slot::InFlightWaiting`] / [`crate::slot::InFlightOrphaned`]）一一对应，
/// `Idle` 表示「没有可借出的 slot」。
///
/// 「完成已就绪」与「发布中」是与生命周期**正交**的两个维度，分别由
/// [`PackedCoreState::ready`] / [`PackedCoreState::finalizing`] 标志位承载，不再挤进
/// 本枚举——它们描述的是 cell 上的完成信箱，而不是 slot 的归属。
///
/// 四个变体恰好填满 2 位，因此不存在 `#[fallback]` 变体：非法位模式在类型层面就不可
/// 表示，下游不必再为「不可能的状态」写兜底分支。
#[bitsize(2)]
#[derive(FromBits, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Idle,
    Reserved,
    InFlightWaiting,
    InFlightOrphaned,
}

/// cell 的完整可观测状态：生命周期 + 两个信箱标志位。
///
/// 诊断与 [`crate::slot::SlotSnapshot`] 携带的是它而不是裸 [`SlotState`]，否则
/// 「Idle 且信箱里压着一条未消费的完成」会被记成一个信息量为零的 `Idle`。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SlotStatus {
    pub state: SlotState,
    /// 信箱里有一条已发布、尚未被消费的完成记录。
    pub ready: bool,
    /// driver 线程正在发布完成记录，远端修改必须自旋等待。
    pub finalizing: bool,
}

impl SlotStatus {
    pub const fn new(state: SlotState, ready: bool, finalizing: bool) -> Self {
        Self {
            state,
            ready,
            finalizing,
        }
    }

    pub const fn of(state: SlotState) -> Self {
        Self::new(state, false, false)
    }

    /// slot 可被 [`crate::driver::registry::OpRegistry::alloc`] 重新分配的充要条件。
    ///
    /// 注意 `ready` 也会让 slot 保持不可分配：detached future 仍可能来消费信箱里的
    /// 那条完成。
    pub const fn is_idle(self) -> bool {
        matches!(self.state, SlotState::Idle) && !self.ready
    }
}

impl Debug for SlotStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.state)?;
        if self.ready {
            f.write_str("+ready")?;
        }
        if self.finalizing {
            f.write_str("+finalizing")?;
        }
        Ok(())
    }
}

/// `generation_bits` 是 [`Generation`] 的存储形态，不对外暴露：generation 的语义
/// （回绕、只能按差值符号位比较）由 `Generation` 保证，位域只负责宽度。读写一律走
/// [`PackedCoreState::generation`] / [`PackedCoreState::with_generation`]。
#[bitsize(64)]
#[derive(FromBits, DebugBits, Clone, Copy, PartialEq, Eq)]
pub struct PackedCoreState {
    pub(crate) generation_bits: u32,
    pub state: SlotState,
    pub ready: bool,
    pub finalizing: bool,
    reserved_bits: u28,
}

impl PackedCoreState {
    /// 全新或刚被回收的 slot 状态：`Idle` + 清空全部标志位。
    ///
    /// `reserved_bits` 由 bilge 的保留字段约定自动置零，不出现在构造参数里。
    pub fn idle(generation: Generation) -> Self {
        Self::new(generation.get(), SlotState::Idle, false, false)
    }

    pub fn generation(self) -> Generation {
        Generation::new(self.generation_bits())
    }

    pub fn status(self) -> SlotStatus {
        SlotStatus::new(self.state(), self.ready(), self.finalizing())
    }

    pub fn with_state(mut self, state: SlotState) -> Self {
        self.set_state(state);
        self
    }

    pub fn with_ready(mut self, ready: bool) -> Self {
        self.set_ready(ready);
        self
    }

    pub fn with_finalizing(mut self, finalizing: bool) -> Self {
        self.set_finalizing(finalizing);
        self
    }

    pub fn with_generation(mut self, generation: Generation) -> Self {
        self.set_generation_bits(generation.get());
        self
    }
}

pub struct AtomicPackedCoreState(AtomicU64);

impl AtomicPackedCoreState {
    pub fn new(state: PackedCoreState) -> Self {
        Self(AtomicU64::new(u64::from(state)))
    }

    pub fn load(&self, order: Ordering) -> PackedCoreState {
        PackedCoreState::from(self.0.load(order))
    }

    pub fn store(&self, state: PackedCoreState, order: Ordering) {
        self.0.store(u64::from(state), order);
    }

    pub fn compare_exchange(
        &self,
        current: PackedCoreState,
        new: PackedCoreState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<PackedCoreState, PackedCoreState> {
        self.0
            .compare_exchange(u64::from(current), u64::from(new), success, failure)
            .map(PackedCoreState::from)
            .map_err(PackedCoreState::from)
    }

    pub fn compare_exchange_weak(
        &self,
        current: PackedCoreState,
        new: PackedCoreState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<PackedCoreState, PackedCoreState> {
        self.0
            .compare_exchange_weak(u64::from(current), u64::from(new), success, failure)
            .map(PackedCoreState::from)
            .map_err(PackedCoreState::from)
    }
}

pub struct SlotStorage<Spec: SlotSpec> {
    pub result: Option<DriverResult<SlotCompletion<Spec>, SlotError<Spec>>>,
    pub payload: Option<SlotPayload<Spec>>,
    pub sidecar: SlotSidecarData<Spec>,
}

impl<Spec: SlotSpec> SlotStorage<Spec> {
    pub fn new() -> Self {
        Self {
            result: None,
            payload: None,
            sidecar: SlotSidecarData::<Spec>::default(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn with_mut<F, X>(&mut self, f: F) -> X
    where
        F: FnOnce(
            &mut Option<DriverResult<SlotCompletion<Spec>, SlotError<Spec>>>,
            &mut Option<SlotPayload<Spec>>,
            &mut SlotSidecarData<Spec>,
        ) -> X,
    {
        f(&mut self.result, &mut self.payload, &mut self.sidecar)
    }
}

impl<Spec: SlotSpec> Default for SlotStorage<Spec> {
    fn default() -> Self {
        Self::new()
    }
}

type SlotMarker<Spec> = PhantomData<fn() -> Spec>;

pub struct SlotData<Spec: SlotSpec> {
    pub(crate) core_state: AtomicPackedCoreState,
    pub next_free: AtomicUsize,
    pub(crate) completion_res: AtomicI32,
    pub(crate) completion_flags: AtomicU32,
    pub(crate) completion_data: Mutex<CompletionData<Spec>>,
    pub(crate) completion_waker: AtomicWaker,
    marker: SlotMarker<Spec>,
}

#[derive(Default)]
pub(crate) enum CompletionData<Spec: SlotSpec> {
    #[default]
    Empty,
    User {
        event: UserCompletionEvent,
        payload: SlotPayload<Spec>,
        detail: Option<DriverResult<SlotCompletion<Spec>, SlotError<Spec>>>,
        cleanup: CompletionCleanupGuard,
    },
}

impl<Spec: SlotSpec> fmt::Debug for CompletionData<Spec>
where
    SlotPayload<Spec>: fmt::Debug,
    SlotCompletion<Spec>: fmt::Debug,
    SlotError<Spec>: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::User {
                event,
                payload,
                detail,
                cleanup,
            } => f
                .debug_struct("User")
                .field("event", event)
                .field("payload", payload)
                .field("detail", detail)
                .field("cleanup", cleanup)
                .finish(),
        }
    }
}

impl<Spec: SlotSpec> SlotData<Spec> {
    pub(crate) const NULL_INDEX: usize = usize::MAX;

    pub fn new() -> Self {
        Self {
            core_state: AtomicPackedCoreState::new(PackedCoreState::idle(Generation::ZERO)),
            next_free: AtomicUsize::new(Self::NULL_INDEX),
            completion_res: AtomicI32::new(0),
            completion_flags: AtomicU32::new(0),
            completion_data: Mutex::new(CompletionData::<Spec>::default()),
            completion_waker: AtomicWaker::new(),
            marker: PhantomData,
        }
    }

    pub(crate) fn state(&self, ordering: Ordering) -> SlotState {
        self.core_state.load(ordering).state()
    }

    pub(crate) fn status(&self, ordering: Ordering) -> SlotStatus {
        self.core_state.load(ordering).status()
    }

    pub fn generation(&self, ordering: Ordering) -> Generation {
        self.core_state.load(ordering).generation()
    }

    pub(crate) fn load_core_state(&self, ordering: Ordering) -> PackedCoreState {
        self.core_state.load(ordering)
    }

    pub(crate) fn set_state(&self, state: SlotState, ordering: Ordering) {
        let mut current = self.core_state.load(Ordering::Acquire);
        loop {
            let new = current.with_state(state);
            match self
                .core_state
                .compare_exchange_weak(current, new, ordering, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    pub(crate) fn reset(&self, generation: Generation) {
        self.core_state
            .store(PackedCoreState::idle(generation), Ordering::Release);
    }

    /// 归还 slot 的生命周期归属，但**不动信箱**：`ready` 标志位原样保留，detached
    /// future 仍能消费已发布的那条完成。
    ///
    /// 与 [`Self::reset`] 的区别正在于此——`reset` 会连 `ready` 一起清掉，因此调用方
    /// 必须先把信箱里的记录取出并清理（见 `OpRegistry::recycle_at_index`）。
    pub(crate) fn free(&self) {
        let mut current = self.core_state.load(Ordering::Acquire);
        loop {
            let new = current.with_state(SlotState::Idle);
            match self.core_state.compare_exchange_weak(
                current,
                new,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    pub(crate) fn completion_with_record_data<F, X>(&self, f: F) -> X
    where
        F: FnOnce(&mut CompletionData<Spec>) -> X,
    {
        let mut data = self.completion_data.lock();
        f(&mut *data)
    }
}

impl<Spec: SlotSpec> Default for SlotData<Spec> {
    fn default() -> Self {
        Self::new()
    }
}
