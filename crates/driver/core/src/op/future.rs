use std::{
    error::Error,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::trace;

use crate::{
    DriverCoreError, DriverError, DriverReport, DriverResult,
    driver::{
        AnomalyAttach, CancelRequest, CompletionAccess, CompletionAnomalyKind,
        CompletionAnomalyReason, CompletionRecord, CompletionToken, CompletionValue, Driver,
        DriverSubmitResult, OpToken, PollRecordResult, RemoteCancelSender, RemoteWaker,
        SharedCompletionTable, SubmitStatus,
    },
    op::{DriverProvider, IntoMultishotOp, IntoPlatformOp, Op},
    slot::{SlotError, SlotSpec},
};

use diagweave::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// 槽位已被回收，用于新一代操作 (Generation Mismatch)。
    /// 调用方应当认为关联的 IO 后端（如 Socket 或 Buffer）已处于不确定状态。
    GenerationMismatch,
    /// 内部错误：操作负载丢失 (Completion sidecar missing)。
    PayloadMissing,
    /// 内部错误：擦除后的 payload 与操作类型不匹配。
    PayloadTypeMismatch,
    /// 其它未知原因造成的资源丢失。
    Other,
}

impl std::fmt::Display for LostReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GenerationMismatch => write!(f, "generation mismatch (slot recycled)"),
            Self::PayloadMissing => write!(f, "payload missing"),
            Self::PayloadTypeMismatch => write!(f, "payload type mismatch"),
            Self::Other => write!(f, "unknown resource loss"),
        }
    }
}

/// 描述 IO 操作丢失及其原因的结构化错误。
#[derive(Debug)]
pub struct OpError<E> {
    pub reason: LostReason,
    pub source: DriverReport<E>,
}

impl<E> OpError<E> {
    pub fn new(reason: LostReason, source: DriverReport<E>) -> Self {
        Self { reason, source }
    }

    /// 如果原因为 GenerationMismatch，则认为该错误是致命的（资源状态不确定）。
    pub fn is_lethal(&self) -> bool {
        matches!(self.reason, LostReason::GenerationMismatch)
    }
}

impl<E> std::fmt::Display for OpError<E>
where
    E: Error + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.source)
    }
}

/// The result of an IO operation.
#[derive(Debug)]
pub enum OpResult<T, E, R = usize> {
    /// Operation completed (successfully or with IO error).
    Completed(DriverResult<R, E>, T),
    /// Operation failed because the resource ownership was lost.
    ResourceLost(OpError<E>),
}

impl<T, E, R> OpResult<T, E, R>
where
    E: Error + Send + Sync + 'static,
{
    /// Unwraps the result, assuming the operation completed (panics if Lost).
    pub fn unwrap(self) -> (R, T) {
        match self {
            OpResult::Completed(Ok(res), data) => (res, data),
            OpResult::Completed(Err(e), _) => panic!("OpResult::Completed(Err({}))", e),
            OpResult::ResourceLost(e) => panic!("OpResult::ResourceLost({})", e),
        }
    }

    /// Returns the result and the resource implementation (if available).
    pub fn into_inner(self) -> (DriverResult<R, E>, Option<T>) {
        match self {
            OpResult::Completed(res, data) => (res, Some(data)),
            OpResult::ResourceLost(err) => (Err(err.source), None),
        }
    }
}

/// The completion projection for a submitted operation.
#[derive(Debug)]
pub struct OpCompletion<T, E, R = usize> {
    pub result: DriverResult<R, E>,
    pub output: T,
}

impl<T, E, R> OpCompletion<T, E, R> {
    pub fn new(result: DriverResult<R, E>, output: T) -> Self {
        Self { result, output }
    }

    pub fn into_parts(self) -> (DriverResult<R, E>, T) {
        (self.result, self.output)
    }
}

impl LostReason {
    #[inline]
    pub(crate) fn from_anomaly_reason(reason: CompletionAnomalyReason) -> Self {
        match reason {
            CompletionAnomalyReason::StaleGeneration => Self::GenerationMismatch,
            CompletionAnomalyReason::UnknownSlot
            | CompletionAnomalyReason::NonActiveSlot
            | CompletionAnomalyReason::BackendContextUnknown
            | CompletionAnomalyReason::BackendSpecific(_) => Self::Other,
        }
    }
}

impl<E> OpError<E>
where
    E: DriverError,
{
    #[inline]
    pub(crate) fn payload_missing() -> Self {
        Self::new(
            LostReason::PayloadMissing,
            E::from_core_report(
                DriverCoreError::Internal
                    .to_report()
                    .push_ctx("scope", "driver-core/op")
                    .attach_note("operation payload lost: completion sidecar missing"),
            ),
        )
    }

    #[inline]
    pub(crate) fn payload_projection(source: DriverReport<E>) -> Self {
        Self::new(LostReason::PayloadTypeMismatch, source)
    }

    #[inline]
    pub(crate) fn from_completion_anomaly(
        kind: CompletionAnomalyKind,
        attach: AnomalyAttach,
    ) -> Self {
        let reason = LostReason::from_anomaly_reason(kind.reason());

        let mut report = DriverCoreError::Internal
            .to_report()
            .push_ctx("scope", "driver-core/op")
            .with_ctx("completion_token", attach.token.raw())
            .with_ctx("completion_anomaly", format!("{:?}", kind.reason()))
            .attach_note("operation completion became unavailable");

        if let Some(index) = kind.index() {
            report = report.with_ctx("slot_index", index);
        }
        if let Some(expected_generation) = kind.expected_generation() {
            report = report.with_ctx("expected_generation", expected_generation);
        }
        if let Some(actual_generation) = kind.actual_generation() {
            report = report.with_ctx("actual_generation", actual_generation);
        }
        if let Some(status) = kind.status() {
            report = report.with_ctx("slot_status", format!("{status:?}"));
        }
        if let Some(backend) = kind.backend().or_else(|| attach.raw.map(|raw| raw.backend)) {
            report = report.with_ctx("completion_backend", format!("{backend:?}"));
        }
        if let Some(backend_context) = kind.backend_context_value() {
            report = report.with_ctx("completion_backend_context", backend_context);
        }
        if let Some(raw) = attach.raw {
            report = report
                .with_ctx("raw_result", raw.res)
                .with_ctx("completion_flags", raw.flags);
        }

        Self::new(reason, E::from_core_report(report))
    }
}

impl<T, E, R> OpResult<T, E, R>
where
    E: DriverError,
{
    #[inline]
    pub(crate) fn from_completion_record<Op, Spec>(record: CompletionRecord<Spec>) -> Poll<Self>
    where
        Spec: SlotSpec<Error = E>,
        Op: IntoPlatformOp<Spec, Output = T, Completion = R>,
    {
        let CompletionRecord {
            event,
            payload: payload_erased,
            detail,
            mut cleanup,
            // 单发操作恒为 `Final`；流的终点判定是 `MultishotOp` 的事。
            continuation: _,
        } = record;
        let payload = match Op::try_payload_from_erased(payload_erased) {
            Ok(payload) => payload,
            Err(report) => {
                let _ = cleanup.run();
                return Poll::Ready(Self::ResourceLost(OpError::payload_projection(report)));
            }
        };
        cleanup.disarm();
        let res =
            detail.unwrap_or_else(|| Spec::Completion::from_event_res::<Spec::Error>(event.res()));
        let completion = Op::complete(payload, res);
        Poll::Ready(Self::Completed(completion.result, completion.output))
    }

    #[inline]
    pub(crate) fn poll_table_once<Op, Spec>(
        table: &dyn CompletionAccess<Spec>,
        token: OpToken,
    ) -> Poll<Self>
    where
        Spec: SlotSpec<Error = E>,
        Op: IntoPlatformOp<Spec, Output = T, Completion = R>,
    {
        match table.try_take_record(token) {
            Ok(PollRecordResult::Ready(record)) => Self::from_completion_record::<Op, Spec>(record),
            Ok(PollRecordResult::Unavailable { kind, attach }) => Poll::Ready(Self::ResourceLost(
                OpError::from_completion_anomaly(kind, attach),
            )),
            Ok(PollRecordResult::Pending) => Poll::Pending,
            Err(report) => Poll::Ready(Self::ResourceLost(OpError::new(LostReason::Other, report))),
        }
    }
}

type DetachedOpMarker<T, Spec> = (T, Spec);

/// A Future representing a detached operation.
pub struct DetachedOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoPlatformOp<Spec>,
{
    pub(crate) completion_table: Option<SharedCompletionTable<Spec>>,
    pub(crate) cancel_sender: Option<RemoteCancelSender>,
    pub(crate) cancel_waker: Option<Arc<dyn RemoteWaker<Spec::Error>>>,
    pub(crate) token: Option<OpToken>,
    pub(crate) immediate_failure: Option<(DriverReport<Spec::Error>, T::UserPayload)>,
    pub(crate) immediate_resource_lost: Option<OpError<Spec::Error>>,
    pub(crate) _phantom: std::marker::PhantomData<DetachedOpMarker<T, Spec>>,
}

unsafe impl<T, Spec> std::marker::Send for DetachedOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoPlatformOp<Spec> + std::marker::Send,
{
}

impl<T, Spec> Drop for DetachedOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoPlatformOp<Spec>,
{
    fn drop(&mut self) {
        if let Some(token) = self.token {
            if let Some(table) = self.completion_table.as_ref() {
                table.mark_orphaned(token);
            }
            if let Some(cancel_sender) = self.cancel_sender.as_ref() {
                let _ = cancel_sender.send(CancelRequest::abandon(token));
            }
        }
        if let Some(cancel_waker) = self.cancel_waker.as_ref()
            && let Err(e) = cancel_waker.wake()
        {
            trace!("DetachedOp cancel wake failed: {}", e);
        }
    }
}

impl<T, Spec> Future for DetachedOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoPlatformOp<Spec>,
{
    type Output = OpResult<T::Output, Spec::Error, T::Completion>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        if let Some((e, payload)) = this.immediate_failure.take() {
            let completion = T::complete(payload, Err(e));
            return Poll::Ready(OpResult::Completed(completion.result, completion.output));
        }
        if let Some(err) = this.immediate_resource_lost.take() {
            return Poll::Ready(OpResult::ResourceLost(err));
        }

        let table = this
            .completion_table
            .as_ref()
            .expect("DetachedOp missing completion_table but no immediate_failure");
        let token = this
            .token
            .expect("DetachedOp missing completion token but no immediate_failure");
        if let Poll::Ready(result) = Self::Output::poll_table_once::<T, Spec>(&**table, token) {
            return Poll::Ready(result);
        }

        table.register_waker(token, cx.waker());
        Self::Output::poll_table_once::<T, Spec>(&**table, token)
    }
}

#[derive(Clone, Copy)]
pub enum LocalState {
    Defined,
    Submitted,
    Completed,
}

/// A Future wrapper for asynchronous IO operations executed locally.
pub struct LocalOp<'a, T, P>
where
    P: DriverProvider,
    T: IntoPlatformOp<P::SlotSpec>,
{
    pub(crate) state: LocalState,
    pub(crate) data: Option<T>,
    pub(crate) provider: P,
    pub(crate) token: Option<OpToken>,
    pub(crate) marker: std::marker::PhantomData<&'a ()>,
}

impl<'a, T, P> LocalOp<'a, T, P>
where
    P: DriverProvider,
    T: IntoPlatformOp<P::SlotSpec>,
{
    pub fn new(data: T, provider: P) -> Self {
        Self {
            state: LocalState::Defined,
            data: Some(data),
            provider,
            token: None,
            marker: std::marker::PhantomData,
        }
    }
}

impl<'a, T, P> Future for LocalOp<'a, T, P>
where
    P: DriverProvider,
    T: IntoPlatformOp<P::SlotSpec>,
{
    type Output = OpResult<T::Output, SlotError<P::SlotSpec>, T::Completion>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let op = unsafe { self.get_unchecked_mut() };

        if let LocalState::Defined = op.state {
            trace!(
                op = %std::any::type_name::<T>(),
                "LocalOp::poll: submit begin"
            );

            let data = op.data.take().expect("Op started without data");
            let (driver_op, payload) = data.into_kernel_and_payload();

            let submit_res = op.provider.with_driver(|mut driver| {
                let mut slot = match driver.reserve_op() {
                    Ok(v) => v,
                    Err(e) => return Err((e, driver_op, payload)),
                };
                let token = slot.token();
                slot.set_payload(T::payload_into_erased(payload));

                let mut driver_op_opt = Some(driver_op);
                let result = slot.submit(&mut driver_op_opt);

                let mut fallback_payload = None;
                match &result {
                    DriverSubmitResult::Submitted(_)
                    | DriverSubmitResult::Failed {
                        status: SubmitStatus::InFlight,
                        ..
                    } => {
                        let _ = slot.persist();
                    }
                    DriverSubmitResult::Failed {
                        status: SubmitStatus::Void,
                        ..
                    } => {
                        if let Some(val) = driver_op_opt.take() {
                            drop(val);
                        }
                        fallback_payload = slot.recover_payload();
                    }
                }
                Ok((token, result, fallback_payload))
            });

            match submit_res {
                Err((e, driver_op, payload)) => {
                    drop(driver_op);
                    let completion = T::complete(payload, Err(e));
                    return Poll::Ready(OpResult::Completed(completion.result, completion.output));
                }
                Ok((token, result, fallback_payload)) => {
                    op.token = Some(token);
                    match result {
                        DriverSubmitResult::Submitted(_) => {
                            op.state = LocalState::Submitted;
                            trace!(
                                op = %std::any::type_name::<T>(),
                                token = CompletionToken::user(token).raw(),
                                "LocalOp::poll: submitted"
                            );
                        }
                        DriverSubmitResult::Failed { report, status } => match status {
                            SubmitStatus::Void => {
                                let Some(payload_erased) = fallback_payload else {
                                    return Poll::Ready(OpResult::ResourceLost(
                                        OpError::payload_missing(),
                                    ));
                                };
                                let payload = match T::try_payload_from_erased(payload_erased) {
                                    Ok(payload) => payload,
                                    Err(report) => {
                                        return Poll::Ready(OpResult::ResourceLost(
                                            OpError::payload_projection(report),
                                        ));
                                    }
                                };
                                trace!(
                                    op = %std::any::type_name::<T>(),
                                    status = ?status,
                                    error = %report,
                                    "LocalOp::poll: submit failed synchronously"
                                );
                                let completion = T::complete(payload, Err(report));
                                return Poll::Ready(OpResult::Completed(
                                    completion.result,
                                    completion.output,
                                ));
                            }
                            SubmitStatus::InFlight => {
                                op.state = LocalState::Submitted;
                                trace!(
                                    op = %std::any::type_name::<T>(),
                                    token = CompletionToken::user(token).raw(),
                                    status = ?status,
                                    "LocalOp::poll: submitted in flight"
                                );
                            }
                        },
                    }
                }
            }
        }

        if let LocalState::Submitted = op.state {
            let token = op
                .token
                .expect("LocalOp submitted state missing completion token");
            let res = op.provider.with_driver(|mut driver| {
                let mut is_ready = false;
                let mut ready_val = None;

                match Self::Output::poll_table_once::<T, P::SlotSpec>(
                    &*driver.completion_table(),
                    token,
                ) {
                    Poll::Ready(result) => {
                        is_ready = true;
                        ready_val = Some(result);
                    }
                    Poll::Pending => {
                        driver.register_completion_waker(token, cx.waker());
                        match Self::Output::poll_table_once::<T, P::SlotSpec>(
                            &*driver.completion_table(),
                            token,
                        ) {
                            Poll::Ready(result) => {
                                is_ready = true;
                                ready_val = Some(result);
                            }
                            Poll::Pending => {}
                        }
                    }
                }
                (is_ready, ready_val)
            });

            if res.0 {
                op.state = LocalState::Completed;
                trace!(
                    op = %std::any::type_name::<T>(),
                    token = CompletionToken::user(token).raw(),
                    "LocalOp::poll: completion ready"
                );
                Poll::Ready(res.1.unwrap())
            } else {
                trace!(
                    op = %std::any::type_name::<T>(),
                    token = CompletionToken::user(token).raw(),
                    "LocalOp::poll: completion pending"
                );
                Poll::Pending
            }
        } else {
            panic!("Polled after completion");
        }
    }
}

impl<'a, T, P> Drop for LocalOp<'a, T, P>
where
    P: DriverProvider,
    T: IntoPlatformOp<P::SlotSpec>,
{
    fn drop(&mut self) {
        if let LocalState::Submitted = self.state
            && let Some(token) = self.token
        {
            self.provider.with_driver(|mut driver| {
                driver.completion_table().mark_orphaned(token);
                let _ = driver.cancel_op(CancelRequest::abandon(token));
            });
        }
    }
}

pub trait OpSubmitter<'a, P: DriverProvider>: Clone + std::marker::Send + Sync {
    type Future<T: IntoPlatformOp<P::SlotSpec> + std::marker::Send>: Future<
        Output = OpResult<T::Output, SlotError<P::SlotSpec>, T::Completion>,
    >;

    fn submit<T>(&self, op: Op<T>, provider: P) -> Self::Future<T>
    where
        T: IntoPlatformOp<P::SlotSpec> + std::marker::Send;

    fn from_current_context() -> Self;
}

pub struct LocalSubmitter<P>(std::marker::PhantomData<fn() -> P>);

impl<P> Clone for LocalSubmitter<P> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<P> Copy for LocalSubmitter<P> {}

impl<P> LocalSubmitter<P> {
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}
impl<P> Default for LocalSubmitter<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, P: DriverProvider> OpSubmitter<'a, P> for LocalSubmitter<P> {
    type Future<T: IntoPlatformOp<P::SlotSpec> + std::marker::Send> = LocalOp<'a, T, P>;

    fn submit<T>(&self, op: Op<T>, provider: P) -> LocalOp<'a, T, P>
    where
        T: IntoPlatformOp<P::SlotSpec> + std::marker::Send,
    {
        trace!("Submitting local op");
        op.submit_local(provider)
    }

    fn from_current_context() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
pub struct DetachedSubmitter;

impl DetachedSubmitter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DetachedSubmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, P: DriverProvider> OpSubmitter<'a, P> for DetachedSubmitter {
    type Future<T: IntoPlatformOp<P::SlotSpec> + std::marker::Send> =
        DetachedOp<T, <P::Driver<'a> as Driver>::SlotSpec>;

    fn submit<T>(&self, op: Op<T>, provider: P) -> Self::Future<T>
    where
        T: IntoPlatformOp<P::SlotSpec> + std::marker::Send,
    {
        provider.with_driver(|mut driver| op.submit_detached(&mut driver))
    }

    fn from_current_context() -> Self {
        Self::new()
    }
}

/// 一个 multishot 操作产出的完成流。
///
/// 与 [`DetachedOp`] 的差别只在「一条 vs 多条」：提交、取消、waker 注册、`Drop` 时的
/// orphan 处理全部同构。流的终点由记录自己携带的
/// [`CompletionContinuation`](crate::driver::CompletionContinuation) 决定——取到
/// `Final` 的那一条之后，`poll_next` 返回 `None`。
pub struct MultishotOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec>,
{
    pub(crate) completion_table: Option<SharedCompletionTable<Spec>>,
    pub(crate) cancel_sender: Option<RemoteCancelSender>,
    pub(crate) cancel_waker: Option<Arc<dyn RemoteWaker<Spec::Error>>>,
    pub(crate) token: Option<OpToken>,
    /// 提交就失败了：产出这一个错误项，然后流结束。
    pub(crate) immediate_failure: Option<DriverReport<Spec::Error>>,
    pub(crate) finished: bool,
    pub(crate) _phantom: std::marker::PhantomData<fn() -> (T, Spec)>,
}

impl<T, Spec> MultishotOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec>,
{
    pub(crate) fn failed(report: DriverReport<Spec::Error>) -> Self {
        Self {
            completion_table: None,
            cancel_sender: None,
            cancel_waker: None,
            token: None,
            immediate_failure: Some(report),
            finished: false,
            _phantom: std::marker::PhantomData,
        }
    }

    pub(crate) fn armed(
        completion_table: SharedCompletionTable<Spec>,
        cancel_sender: RemoteCancelSender,
        cancel_waker: Arc<dyn RemoteWaker<Spec::Error>>,
        token: OpToken,
    ) -> Self {
        Self {
            completion_table: Some(completion_table),
            cancel_sender: Some(cancel_sender),
            cancel_waker: Some(cancel_waker),
            token: Some(token),
            immediate_failure: None,
            finished: false,
            _phantom: std::marker::PhantomData,
        }
    }

    /// 该操作是否仍在内核里（用于诊断与测试）。
    pub fn is_armed(&self) -> bool {
        !self.finished && self.token.is_some()
    }

    fn take_one(&mut self) -> Poll<Option<MultishotItem<T, Spec>>> {
        let (Some(table), Some(token)) = (self.completion_table.as_ref(), self.token) else {
            self.finished = true;
            return Poll::Ready(None);
        };

        match table.try_take_record(token) {
            Ok(PollRecordResult::Ready(record)) => {
                if record.continuation.is_final() {
                    // 终态记录被取走的同时 slot 已经归还、generation 已经推进，token 从此
                    // 失效——`Drop` 不能再拿它去 mark_orphaned/cancel。
                    self.finished = true;
                    self.token = None;
                }
                Poll::Ready(Some(item_from_record::<T, Spec>(record)))
            }
            Ok(PollRecordResult::Unavailable { kind, attach }) => {
                self.finished = true;
                self.token = None;
                Poll::Ready(Some(OpResult::ResourceLost(
                    OpError::from_completion_anomaly(kind, attach),
                )))
            }
            Ok(PollRecordResult::Pending) => Poll::Pending,
            Err(report) => {
                self.finished = true;
                self.token = None;
                Poll::Ready(Some(OpResult::ResourceLost(OpError::new(
                    LostReason::Other,
                    report,
                ))))
            }
        }
    }
}

type MultishotItem<T, Spec> = OpResult<
    <T as IntoMultishotOp<Spec>>::Item,
    <Spec as SlotSpec>::Error,
    <T as IntoPlatformOp<Spec>>::Completion,
>;

#[inline]
fn item_from_record<T, Spec>(record: CompletionRecord<Spec>) -> MultishotItem<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec>,
{
    let CompletionRecord {
        event,
        payload: payload_erased,
        detail,
        mut cleanup,
        continuation: _,
    } = record;
    let item = match T::try_item_from_erased(payload_erased) {
        Ok(item) => item,
        Err(report) => {
            let _ = cleanup.run();
            return OpResult::ResourceLost(OpError::payload_projection(report));
        }
    };
    cleanup.disarm();
    let res =
        detail.unwrap_or_else(|| Spec::Completion::from_event_res::<Spec::Error>(event.res()));
    let completion = T::complete_item(item, res);
    OpResult::Completed(completion.result, completion.output)
}

impl<T, Spec> futures_core::Stream for MultishotOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec>,
{
    type Item = MultishotItem<T, Spec>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };

        if let Some(report) = this.immediate_failure.take() {
            this.finished = true;
            return Poll::Ready(Some(OpResult::ResourceLost(OpError::new(
                LostReason::Other,
                report,
            ))));
        }
        if this.finished {
            return Poll::Ready(None);
        }

        if let Poll::Ready(item) = this.take_one() {
            return Poll::Ready(item);
        }

        // 注册与发布竞速：注册之后再取一次，避免丢唤醒（与 `DetachedOp::poll` 同构）。
        if let (Some(table), Some(token)) = (this.completion_table.as_ref(), this.token) {
            table.register_waker(token, cx.waker());
        }
        this.take_one()
    }
}

unsafe impl<T, Spec> std::marker::Send for MultishotOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec> + std::marker::Send,
    T::Item: std::marker::Send,
{
}

impl<T, Spec> Drop for MultishotOp<T, Spec>
where
    Spec: SlotSpec,
    T: IntoMultishotOp<Spec>,
{
    fn drop(&mut self) {
        // token 仍在说明操作还没终止：既要放弃信箱里剩下的记录（`mark_orphaned` 会逐条
        // 跑它们的 cleanup），也要请求内核取消，否则 multishot 会一直投递下去。
        if let Some(token) = self.token {
            if let Some(table) = self.completion_table.as_ref() {
                table.mark_orphaned(token);
            }
            if let Some(cancel_sender) = self.cancel_sender.as_ref() {
                let _ = cancel_sender.send(CancelRequest::abandon(token));
            }
        }
        if let Some(cancel_waker) = self.cancel_waker.as_ref()
            && let Err(e) = cancel_waker.wake()
        {
            trace!("MultishotOp cancel wake failed: {}", e);
        }
    }
}
