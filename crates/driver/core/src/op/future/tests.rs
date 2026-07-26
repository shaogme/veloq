//! `DetachedOp` 的生命周期收尾。
//!
//! 这里的两条用例是一对：一个已经取到终态完成的 future 在 drop 时**什么都不该做**，而一个
//! 仍在途的 future 在 drop 时**必须**放弃 slot 并请求取消。两者共用同一段 `Drop`，所以它们
//! 只能一起断言。

use super::*;

use crate::{
    DriverCoreError, DriverError, DriverResult,
    driver::{
        CompletionAccess, CompletionBackend, CompletionCleanupGuard, CompletionContinuation,
        CompletionMutationOutcome, CompletionPacket, CompletionWritePermit, PlatformOp,
        RecordCompletionResult, UserCompletionEvent,
    },
    op::{IntoPlatformOp, OpKind},
    slot::{self, Generation},
};
use std::sync::mpsc;
use veloq_std::sync::atomic::{AtomicUsize, Ordering};

struct DummyPlatformOp;

impl PlatformOp for DummyPlatformOp {
    type CleanupContext<'a> = ();
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct DummyError;

impl std::fmt::Display for DummyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dummy error")
    }
}

impl std::error::Error for DummyError {}

impl DriverError for DummyError {
    #[inline]
    fn from_core_report(report: Report<DriverCoreError>) -> Report<Self> {
        report.map_err(|_| DummyError)
    }
}

struct DummySlotSpec;

impl slot::SlotSpec for DummySlotSpec {
    type Op = DummyPlatformOp;
    type UserPayload = ();
    type PlatformData = ();
    type Sidecar = ();
    type Error = DummyError;
    type Completion = usize;
    type CompletionDiagnostics = ();
}

struct DummyOp;

impl IntoPlatformOp<DummySlotSpec> for DummyOp {
    type UserPayload = ();
    type Output = ();
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::Wakeup;

    fn into_kernel_and_payload(self) -> (DummyPlatformOp, Self::UserPayload) {
        (DummyPlatformOp, ())
    }

    fn payload_into_erased(_payload: Self::UserPayload) {}

    fn try_payload_from_erased(_erased: ()) -> DriverResult<Self::UserPayload, DummyError> {
        Ok(())
    }

    fn complete(
        _payload: Self::UserPayload,
        res: DriverResult<usize, DummyError>,
    ) -> OpCompletion<Self::Output, DummyError, Self::Completion> {
        OpCompletion::new(res, ())
    }
}

/// 只回答 `try_take_record` 与 `mark_orphaned` 的最小完成表：前者按「还剩几条记录」回答
/// 「已就绪 / 仍在途」，后者数一下自己被调用了几次。
///
/// 记录在取的时候现造而不是预先存着——存一条 `CompletionRecord` 需要一把锁，而 loom
/// feature 下的锁只能在 model 里用，这里的用例并不是 loom model。
#[derive(Default)]
struct MockTable {
    /// 还没被取走的完成条数；`0` 表示这个操作仍在途。
    ready: AtomicUsize,
    orphaned: AtomicUsize,
}

impl MockTable {
    fn with_ready_record() -> Self {
        Self {
            ready: AtomicUsize::new(1),
            orphaned: AtomicUsize::new(0),
        }
    }

    fn orphaned(&self) -> usize {
        self.orphaned.load(Ordering::Relaxed)
    }
}

impl CompletionAccess<DummySlotSpec> for MockTable {
    fn record_completion(
        &self,
        _permit: CompletionWritePermit,
        _packet: CompletionPacket<DummySlotSpec>,
    ) -> RecordCompletionResult<DummySlotSpec> {
        unreachable!("the mock table is never written to")
    }

    fn try_take_record(
        &self,
        token: OpToken,
    ) -> Result<PollRecordResult<DummySlotSpec>, Report<DummyError>> {
        if self.ready.load(Ordering::Relaxed) == 0 {
            return Ok(PollRecordResult::Pending);
        }
        self.ready.fetch_sub(1, Ordering::Relaxed);
        Ok(PollRecordResult::Ready(CompletionRecord {
            event: UserCompletionEvent::from_parts(CompletionBackend::Core, token, 0, 0),
            payload: (),
            detail: None,
            cleanup: CompletionCleanupGuard::none(),
            continuation: CompletionContinuation::Final,
        }))
    }

    fn register_waker(
        &self,
        _token: OpToken,
        _waker: &std::task::Waker,
    ) -> CompletionMutationOutcome {
        CompletionMutationOutcome::Applied
    }

    fn mark_waiting(&self, _token: OpToken) -> CompletionMutationOutcome {
        CompletionMutationOutcome::Applied
    }

    fn discard_ready_records(&self, _token: OpToken) -> CompletionMutationOutcome {
        CompletionMutationOutcome::Applied
    }

    fn mark_orphaned(&self, _token: OpToken) -> CompletionMutationOutcome {
        self.orphaned.fetch_add(1, Ordering::Relaxed);
        CompletionMutationOutcome::Applied
    }

    #[cfg(any(test, feature = "loom"))]
    fn debug_get_state(&self, _idx: usize) -> u8 {
        0
    }

    #[cfg(any(test, feature = "loom"))]
    fn debug_status_string(&self, _idx: usize) -> String {
        String::from("mock")
    }
}

#[derive(Default)]
struct CountingWaker {
    wakes: AtomicUsize,
}

impl RemoteWaker<DummyError> for CountingWaker {
    fn wake(&self) -> DriverResult<(), DummyError> {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct Harness {
    op: Option<DetachedOp<DummyOp, DummySlotSpec>>,
    table: Arc<MockTable>,
    waker: Arc<CountingWaker>,
    cancels: mpsc::Receiver<CancelRequest>,
}

impl Harness {
    fn new(table: Arc<MockTable>) -> Self {
        let token = test_token();
        let waker = Arc::new(CountingWaker::default());
        let (tx, cancels) = mpsc::channel();
        let op = DetachedOp {
            completion_table: Some(table.clone() as SharedCompletionTable<DummySlotSpec>),
            cancel_sender: Some(tx),
            cancel_waker: Some(waker.clone() as Arc<dyn RemoteWaker<DummyError>>),
            token: Some(token),
            immediate_failure: None,
            immediate_resource_lost: None,
            _phantom: std::marker::PhantomData,
        };
        Self {
            op: Some(op),
            table,
            waker,
            cancels,
        }
    }

    fn poll_once(&mut self) -> Poll<OpResult<(), DummyError, usize>> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let op = self.op.as_mut().expect("op still alive");
        Pin::new(op).poll(&mut cx)
    }

    fn drop_op(&mut self) {
        drop(self.op.take());
    }

    fn wakes(&self) -> usize {
        self.waker.wakes.load(Ordering::Relaxed)
    }
}

fn test_token() -> OpToken {
    OpToken::from_registry_parts(3, Generation::new(7)).expect("test token should be encodable")
}

/// 取到终态完成之后 token 就失效了（slot 已归还、generation 已推进）。此时再
/// `mark_orphaned` / `abandon` 命中的必然是 generation 校验，只会在诊断里记一条假的
/// `StaleGeneration`，并让驱动为一个结束了的操作白跑一次唤醒。
#[test]
fn a_completed_detached_op_leaves_its_slot_alone_on_drop() {
    let mut harness = Harness::new(Arc::new(MockTable::with_ready_record()));

    assert!(matches!(harness.poll_once(), Poll::Ready(_)));
    harness.drop_op();

    assert_eq!(harness.table.orphaned(), 0, "已完成的操作不该再被放弃");
    assert_eq!(harness.wakes(), 0, "没有取消请求就不该唤醒驱动");
    assert!(
        harness.cancels.try_recv().is_err(),
        "已完成的操作不该再投取消请求"
    );
}

/// 回归锚点：上面那条不能顺手把真正需要的取消路径关掉。
#[test]
fn an_unfinished_detached_op_still_orphans_and_cancels_on_drop() {
    let mut harness = Harness::new(Arc::new(MockTable::default()));

    assert!(matches!(harness.poll_once(), Poll::Pending));
    harness.drop_op();

    assert_eq!(harness.table.orphaned(), 1);
    assert_eq!(harness.wakes(), 1);
    let cancel = harness
        .cancels
        .try_recv()
        .expect("在途操作必须投出取消请求");
    assert_eq!(cancel.target, test_token());
}

/// 提交就失败的 future 从来没有过 token，drop 时同样什么都不做。
#[test]
fn a_never_submitted_detached_op_has_nothing_to_release() {
    let op: DetachedOp<DummyOp, DummySlotSpec> = DetachedOp {
        completion_table: None,
        cancel_sender: None,
        cancel_waker: None,
        token: None,
        immediate_failure: None,
        immediate_resource_lost: Some(OpError::payload_missing()),
        _phantom: std::marker::PhantomData,
    };
    drop(op);
}
