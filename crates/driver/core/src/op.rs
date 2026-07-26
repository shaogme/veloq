mod future;
pub mod types;

pub use future::*;
pub use types::OpKind;

use std::marker::{PhantomData, Send};

use tracing::trace;

use crate::{
    DriverCoreError, DriverError, DriverReport, DriverResult,
    driver::{Driver, DriverSubmitResult, SubmitStatus},
    slot::{SlotCompletion, SlotError, SlotOp, SlotPayload, SlotSpec},
};
use diagweave::prelude::*;

pub trait DriverProvider: Clone + Unpin {
    type SlotSpec: SlotSpec;
    type Driver<'a>: Driver<SlotSpec = Self::SlotSpec>
    where
        Self: 'a;

    fn with_driver<'a, R>(&'a self, f: impl FnOnce(Self::Driver<'a>) -> R) -> R;
}

/// Trait to convert a user-facing operation to a platform-specific driver operation.
pub trait IntoPlatformOp<Spec: SlotSpec>: Sized + Send {
    type UserPayload: Send;
    type Output;
    type Completion;
    const PAYLOAD_KIND: types::OpKind;

    fn into_kernel_and_payload(self) -> (SlotOp<Spec>, Self::UserPayload);

    fn payload_into_erased(payload: Self::UserPayload) -> SlotPayload<Spec>;

    fn try_payload_from_erased(
        erased: SlotPayload<Spec>,
    ) -> DriverResult<Self::UserPayload, SlotError<Spec>>;

    fn complete(
        payload: Self::UserPayload,
        res: DriverResult<SlotCompletion<Spec>, SlotError<Spec>>,
    ) -> OpCompletion<Self::Output, SlotError<Spec>, Self::Completion>;
}

/// 一个「一次提交、多条完成」的操作。
///
/// 与 [`IntoPlatformOp`] 的关系是**并列的两个概念，不是同一个东西的两种用法**：
///
/// - `IntoPlatformOp::UserPayload` 是**提交 payload**——它在提交时进入 slot，multishot
///   期间原样留在那里（内核可能仍持有指向它的指针），取消与 orphan cleanup 都要用它；
/// - `Item` 是**每条完成的产物**——accept 是一个新 fd，recv 是一条收到的数据。
///
/// 早期设计想把两者合成一个 `UserPayload`，代价是提交时要塞一个「空壳 item」
/// （`Option<FixedBuf>::None` 之类），此后每一处 payload 访问都要处理一个永远不该
/// 出现的 `None`。拆开之后两者都没有空状态。
///
/// item 仍然擦除进同一个 [`SlotPayload`] 里（后端的 payload enum 加变体），所以信箱的
/// 记录形状不变。
pub trait IntoMultishotOp<Spec: SlotSpec>: IntoPlatformOp<Spec> {
    /// 每条完成产出的东西。
    type Item;

    /// 把后端在完成路径上构造、擦除进 `SlotPayload` 的 item 还原回来。
    fn try_item_from_erased(erased: SlotPayload<Spec>)
    -> DriverResult<Self::Item, SlotError<Spec>>;

    /// 把一条完成的结果投影成用户可见的产物。
    fn complete_item(
        item: Self::Item,
        res: DriverResult<SlotCompletion<Spec>, SlotError<Spec>>,
    ) -> OpCompletion<Self::Item, SlotError<Spec>, Self::Completion>;
}

#[inline]
pub fn payload_projection_mismatch_report<E>(
    expected_payload: &'static str,
    erased_payload: &'static str,
) -> DriverReport<E>
where
    E: DriverError,
{
    E::from_core_report(
        DriverCoreError::Internal
            .to_report()
            .push_ctx("scope", "driver-core/op/payload_projection")
            .with_ctx("expected_payload", expected_payload)
            .with_ctx("erased_payload", erased_payload)
            .attach_note("operation payload variant mismatch"),
    )
}

/// A generic wrapper for IO operation data.
pub struct Op<T> {
    pub data: T,
}

impl<T> Op<T> {
    pub fn new(data: T) -> Self {
        Self { data }
    }

    pub fn submit_detached<D>(self, driver: &mut D) -> DetachedOp<T, D::SlotSpec>
    where
        T: IntoPlatformOp<D::SlotSpec> + Send,
        D: Driver,
    {
        let data = self.data;
        trace!("Submitting detached op");

        match driver.reserve_op() {
            Ok(mut slot) => {
                let (kernel_op, payload) = data.into_kernel_and_payload();
                let mut op_platform = Some(kernel_op);
                let completion_table = slot.completion_table();
                let cancel_sender = slot.remote_cancel_sender();
                let cancel_waker = slot.create_waker();
                slot.set_payload(T::payload_into_erased(payload));

                match slot.submit(&mut op_platform) {
                    DriverSubmitResult::Submitted(_) => {
                        let token = slot.persist().token();
                        completion_table.mark_waiting(token);
                        DetachedOp {
                            completion_table: Some(completion_table),
                            cancel_sender: Some(cancel_sender),
                            cancel_waker: Some(cancel_waker),
                            token: Some(token),
                            immediate_failure: None,
                            immediate_resource_lost: None,
                            _phantom: PhantomData,
                        }
                    }
                    DriverSubmitResult::Failed { report, status } => {
                        trace!(
                            "Submit failed synchronously: {} (status={:?})",
                            report, status
                        );
                        match status {
                            SubmitStatus::Void => {
                                let Some(payload_erased) = slot.recover_payload() else {
                                    if let Some(op) = op_platform.take() {
                                        drop(op);
                                    }
                                    return DetachedOp {
                                        completion_table: None,
                                        cancel_sender: None,
                                        cancel_waker: None,
                                        token: None,
                                        immediate_failure: None,
                                        immediate_resource_lost: Some(OpError::payload_missing()),
                                        _phantom: PhantomData,
                                    };
                                };

                                let payload = match T::try_payload_from_erased(payload_erased) {
                                    Ok(payload) => payload,
                                    Err(report) => {
                                        if let Some(op) = op_platform.take() {
                                            drop(op);
                                        }
                                        return DetachedOp {
                                            completion_table: None,
                                            cancel_sender: None,
                                            cancel_waker: None,
                                            token: None,
                                            immediate_failure: None,
                                            immediate_resource_lost: Some(
                                                OpError::payload_projection(report),
                                            ),
                                            _phantom: PhantomData,
                                        };
                                    }
                                };
                                if let Some(op) = op_platform.take() {
                                    drop(op);
                                }
                                DetachedOp {
                                    completion_table: None,
                                    cancel_sender: None,
                                    cancel_waker: None,
                                    token: None,
                                    immediate_failure: Some((report, payload)),
                                    immediate_resource_lost: None,
                                    _phantom: PhantomData,
                                }
                            }
                            SubmitStatus::InFlight => {
                                let token = slot.persist().token();
                                completion_table.mark_waiting(token);
                                DetachedOp {
                                    completion_table: Some(completion_table),
                                    cancel_sender: Some(cancel_sender),
                                    cancel_waker: Some(cancel_waker),
                                    token: Some(token),
                                    immediate_failure: None,
                                    immediate_resource_lost: None,
                                    _phantom: PhantomData,
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let (kernel_op, payload) = data.into_kernel_and_payload();
                drop(kernel_op);
                DetachedOp {
                    completion_table: None,
                    cancel_sender: None,
                    cancel_waker: None,
                    token: None,
                    immediate_failure: Some((e, payload)),
                    immediate_resource_lost: None,
                    _phantom: PhantomData,
                }
            }
        }
    }

    pub fn submit_local<'a, P: DriverProvider>(self, provider: P) -> LocalOp<'a, T, P>
    where
        T: IntoPlatformOp<P::SlotSpec>,
    {
        LocalOp::new(self.data, provider)
    }

    /// 提交一个 multishot 操作，得到它的完成流。
    ///
    /// 与 [`Self::submit_detached`] 的差别只在返回类型：提交路径本身完全相同，多条完成
    /// 是 slot 层的性质（`CompletionContinuation`），不是提交层的。
    ///
    /// 提交失败时流会产出唯一一个错误项然后结束；提交 payload 由 `recover_payload` 取回
    /// 后丢弃——multishot 的提交 payload 里没有用户交出的 buffer（那是 provided buffer
    /// 或每条完成各自的 item），所以丢弃它不损失任何用户资源。
    pub fn submit_multishot<D>(self, driver: &mut D) -> MultishotOp<T, D::SlotSpec>
    where
        T: IntoMultishotOp<D::SlotSpec> + Send,
        D: Driver,
    {
        let data = self.data;
        trace!("Submitting multishot op");

        let mut slot = match driver.reserve_op() {
            Ok(slot) => slot,
            Err(report) => {
                let (kernel_op, payload) = data.into_kernel_and_payload();
                drop(kernel_op);
                drop(payload);
                return MultishotOp::failed(report);
            }
        };

        let (kernel_op, payload) = data.into_kernel_and_payload();
        let mut op_platform = Some(kernel_op);
        let completion_table = slot.completion_table();
        let cancel_sender = slot.remote_cancel_sender();
        let cancel_waker = slot.create_waker();
        slot.set_payload(T::payload_into_erased(payload));

        match slot.submit(&mut op_platform) {
            DriverSubmitResult::Submitted(_)
            | DriverSubmitResult::Failed {
                status: SubmitStatus::InFlight,
                ..
            } => {
                let token = slot.persist().token();
                completion_table.mark_waiting(token);
                MultishotOp::armed(completion_table, cancel_sender, cancel_waker, token)
            }
            DriverSubmitResult::Failed {
                report,
                status: SubmitStatus::Void,
            } => {
                drop(op_platform.take());
                drop(slot.recover_payload());
                MultishotOp::failed(report)
            }
        }
    }
}
