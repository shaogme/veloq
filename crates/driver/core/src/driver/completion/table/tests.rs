use super::*;

use crate::{
    DriverCoreError,
    driver::{
        AnomalyAttach, AnomalyOutcome, CompletionAnomalyKind, CompletionAnomalyReason,
        CompletionBackend, CompletionBackendHooks, CompletionCleanupGuard, CompletionControl,
        CompletionEnvelope, CompletionFlowExt, CompletionFlowOutcome, CompletionHookOutcome,
        CompletionIngress, CompletionSource, CompletionToken, HookResult, OpToken, PlatformOp,
        registry::OpRegistry,
    },
    slot::{
        self, CheckedSlotView, InFlightOrphaned, InFlightWaiting, SlotRegistryExt, SlotState,
        SlotView,
    },
};
use veloq_std::sync::atomic::Ordering;

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

impl crate::DriverError for DummyError {
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

fn test_token(index: usize, generation: u32) -> OpToken {
    OpToken::from_registry_parts(index, generation).expect("test token should be encodable")
}

fn test_event(token: OpToken, res: i32) -> UserCompletionEvent {
    UserCompletionEvent::from_parts(CompletionBackend::Core, token, res, 0)
}

#[derive(Default)]
struct TestHooks {
    cleanup: Option<CompletionCleanupGuard>,
}

impl CompletionBackendHooks<DummySlotSpec> for TestHooks {
    type BackendIngress = ();
    type BackendEffect = ();

    fn handle_control(
        &mut self,
        _control: CompletionControl,
    ) -> HookResult<DummySlotSpec, CompletionHookOutcome<DummySlotSpec, Self::BackendEffect>> {
        Ok(CompletionHookOutcome::Ignore { effect: () })
    }

    fn complete_waiting(
        &mut self,
        event: UserCompletionEvent,
        slot: slot::Slot<'_, InFlightWaiting, DummySlotSpec>,
        _source: CompletionSource<'_, Self::BackendIngress>,
    ) -> HookResult<DummySlotSpec, CompletionHookOutcome<DummySlotSpec, Self::BackendEffect>> {
        let mut completed = slot.complete();
        let _ = completed.take_op();
        let (payload, detail) = completed.take_completion_data();
        Ok(CompletionHookOutcome::User {
            event,
            payload: payload.expect("test slot payload should exist"),
            detail,
            cleanup: self.cleanup.take().unwrap_or_default(),
            effect: (),
        })
    }

    fn complete_orphaned(
        &mut self,
        _event: UserCompletionEvent,
        slot: slot::Slot<'_, InFlightOrphaned, DummySlotSpec>,
        _source: CompletionSource<'_, Self::BackendIngress>,
    ) -> HookResult<DummySlotSpec, CompletionHookOutcome<DummySlotSpec, Self::BackendEffect>> {
        let mut completed = slot.complete();
        let _ = completed.take_op();
        let (payload, detail) = completed.take_completion_data();
        let _ = payload;
        drop(detail);
        Ok(CompletionHookOutcome::Cleanup {
            cleanup: self.cleanup.take().unwrap_or_default(),
            effect: (),
        })
    }

    fn complete_corrupt(
        &mut self,
        event: UserCompletionEvent,
        kind: CompletionAnomalyKind,
        _source: CompletionSource<'_, Self::BackendIngress>,
    ) -> HookResult<DummySlotSpec, CompletionHookOutcome<DummySlotSpec, Self::BackendEffect>> {
        Ok(CompletionHookOutcome::Anomaly {
            kind,
            attach: AnomalyAttach::from_raw_completion(event.raw()),
            effect: (),
        })
    }

    fn finish_backend_effect(
        &mut self,
        _effect: Self::BackendEffect,
    ) -> HookResult<DummySlotSpec, ()> {
        Ok(())
    }
}

fn active_registry() -> (OpRegistry<DummySlotSpec>, OpToken) {
    let mut registry = OpRegistry::<DummySlotSpec>::new(1);
    let token = arm_slot(&mut registry);
    (registry, token)
}

fn arm_slot(registry: &mut OpRegistry<DummySlotSpec>) -> OpToken {
    let handle = registry.alloc(()).expect("slot allocation failed").handle;
    let token = test_token(handle.index, handle.generation);
    registry
        .with_slot_storage_mut(token, |_result, payload, _sidecar| {
            *payload = Some(());
        })
        .expect("slot storage should exist");
    let slot = match registry.checked_slot_view(token).unwrap() {
        CheckedSlotView::Valid(SlotView::Reserved(slot)) => slot
            .init_op_with(DummyPlatformOp, |_| {})
            .expect("reserved slot should accept op"),
        _ => panic!("reserved slot should be available"),
    };
    let _in_flight = slot
        .start_submission_with(None)
        .expect("reserved slot should start submission")
        .persist();
    token
}

fn accept_with_hooks(
    registry: &mut OpRegistry<DummySlotSpec>,
    event: UserCompletionEvent,
    hooks: &mut TestHooks,
) -> CompletionFlowOutcome {
    accept_ingress(registry, CompletionIngress::User(event), hooks)
}

/// 模拟内核回传：token 先编码成裸 `u64`，再由 `classify()` 解码回 `OpToken`。
/// 只有走这条路径才会覆盖 `CompletionToken` 的编解码，`CompletionIngress::User`
/// 是直接携带 `OpToken` 的，会绕过它。
fn accept_kernel_raw(
    registry: &mut OpRegistry<DummySlotSpec>,
    token: OpToken,
    res: i32,
    hooks: &mut TestHooks,
) -> CompletionFlowOutcome {
    let envelope = CompletionEnvelope::from_raw_parts(
        CompletionBackend::Core,
        CompletionToken::user(token).raw(),
        res,
        0,
    );
    accept_ingress(registry, CompletionIngress::Kernel(envelope), hooks)
}

fn accept_ingress(
    registry: &mut OpRegistry<DummySlotSpec>,
    ingress: CompletionIngress,
    hooks: &mut TestHooks,
) -> CompletionFlowOutcome {
    let diagnostics = registry.shared.completion_diagnostics();
    let table: SharedCompletionTable<DummySlotSpec> = registry.shared.clone();
    registry
        .accept_completion(&table, &diagnostics, hooks, ingress)
        .expect("test completion should succeed")
}

fn accept_user(registry: &mut OpRegistry<DummySlotSpec>, token: OpToken, res: i32) {
    let mut hooks = TestHooks::default();
    let _ = accept_with_hooks(registry, test_event(token, res), &mut hooks);
}

#[test]
fn record_completion_rejects_idle_future_generation() {
    let mut registry = OpRegistry::<DummySlotSpec>::new(1);
    let table = registry.shared.clone();
    let token = test_token(0, 1);

    let mut hooks = TestHooks::default();
    let outcome = accept_with_hooks(&mut registry, test_event(token, 0), &mut hooks);

    assert_eq!(outcome.anomaly, 1);
    assert_eq!(table.debug_get_state(0), CELL_STATE_IDLE);
}

#[test]
fn try_take_record_reports_future_generation_unavailable() {
    let table = slot::SlotTable::<DummySlotSpec>::new(1);
    let token = test_token(0, 1);

    match table.try_take_record(token).unwrap() {
        PollRecordResult::Unavailable { kind, .. } => {
            assert_eq!(kind.reason(), CompletionAnomalyReason::NonActiveSlot);
            assert!(matches!(
                kind,
                CompletionAnomalyKind::NonActive {
                    index: 0,
                    generation: 1,
                    ..
                }
            ));
        }
        PollRecordResult::Pending => panic!("future generation token must not stay pending"),
        PollRecordResult::Ready(_) => panic!("future generation token must not become ready"),
    }
}

#[test]
fn mark_waiting_does_not_activate_idle_future_generation() {
    let table = slot::SlotTable::<DummySlotSpec>::new(1);
    let token = test_token(0, 1);

    let outcome = table.mark_waiting(token);

    assert!(matches!(
        outcome,
        CompletionMutationOutcome::Rejected(AnomalyOutcome::NonActive(_))
    ));
    assert_eq!(table.debug_get_state(0), CELL_STATE_IDLE);
}

#[test]
fn mark_waiting_does_not_revive_orphaned_slot() {
    let table = slot::SlotTable::<DummySlotSpec>::new(1);
    table.slots[0].reset(1);
    table.slots[0].set_state(SlotState::InFlightOrphaned, Ordering::Release);
    let token = test_token(0, 1);

    let outcome = table.mark_waiting(token);

    assert!(matches!(
        outcome,
        CompletionMutationOutcome::Rejected(AnomalyOutcome::NonActive(_))
    ));
    assert_eq!(table.debug_get_state(0), CELL_STATE_ORPHANED);
}

#[test]
fn mark_orphaned_reports_stale_generation() {
    let table = slot::SlotTable::<DummySlotSpec>::new(1);
    table.slots[0].reset(2);
    table.slots[0].set_state(SlotState::InFlightWaiting, Ordering::Release);
    let token = test_token(0, 1);

    let outcome = table.mark_orphaned(token);

    assert!(matches!(
        outcome,
        CompletionMutationOutcome::Rejected(AnomalyOutcome::Stale(_))
    ));
    assert_eq!(table.debug_get_state(0), CELL_STATE_WAITING);
    assert_eq!(
        table.completion_diagnostics().snapshot().stale_completion,
        1
    );
}

#[test]
fn register_waker_reports_missing_slot() {
    let table = slot::SlotTable::<DummySlotSpec>::new(1);
    let waker = Waker::noop();
    let token = test_token(3, 1);

    let outcome = table.register_waker(token, waker);

    assert!(matches!(
        outcome,
        CompletionMutationOutcome::Rejected(AnomalyOutcome::Missing(_))
    ));
}

#[test]
fn duplicate_completion_does_not_clear_ready_data() {
    let (mut registry, token) = active_registry();
    let table = registry.shared.clone();

    let mut hooks = TestHooks::default();
    let first = accept_with_hooks(&mut registry, test_event(token, 11), &mut hooks);
    let duplicate = accept_with_hooks(&mut registry, test_event(token, 22), &mut hooks);

    assert_eq!(first.user_completed, 1);
    assert_eq!(duplicate.anomaly, 1);
    let record = match table.try_take_record(token).unwrap() {
        PollRecordResult::Ready(record) => record,
        PollRecordResult::Pending => panic!("first completion should be ready"),
        PollRecordResult::Unavailable { kind, .. } => {
            panic!("first completion should remain available: {kind:?}")
        }
    };
    assert_eq!(record.event.res(), 11);
}

/// 一轮 alloc/complete/consume 让 slot generation 推进 2。旧的 15 位 token 布局会在
/// generation 跨过 `0x8000` 时把编码截断，完成回来后被判成 Stale 静默丢弃——slot 永远
/// 停留在 `InFlightWaiting`，其 payload 永不归还。这里把单个 slot 跑满一整圈 15 位
/// 空间，确认完成在边界两侧都能正确路由。
#[test]
fn completion_routing_survives_the_legacy_15_bit_generation_boundary() {
    const ROUNDS: u32 = 0x8000 / 2 + 16;

    let mut registry = OpRegistry::<DummySlotSpec>::new(1);
    let table = registry.shared.clone();

    for round in 0..ROUNDS {
        let token = arm_slot(&mut registry);
        let res = round as i32;
        let mut hooks = TestHooks::default();
        let outcome = accept_kernel_raw(&mut registry, token, res, &mut hooks);

        assert_eq!(
            outcome.user_completed,
            1,
            "round {round}: completion for generation {:#x} was not routed to its slot",
            token.generation()
        );
        match table.try_take_record(token).unwrap() {
            PollRecordResult::Ready(record) => assert_eq!(record.event.res(), res),
            PollRecordResult::Pending => {
                panic!("round {round}: recorded completion should be ready")
            }
            PollRecordResult::Unavailable { kind, .. } => {
                panic!("round {round}: recorded completion was dropped: {kind:?}")
            }
        }
    }

    let snapshot = table.completion_diagnostics().snapshot();
    assert_eq!(snapshot.stale_completion, 0);
    assert!(
        table.slots[0].generation(Ordering::Acquire) > 0x8000,
        "the test must actually cross the 15-bit boundary"
    );
}

#[test]
fn ready_mark_orphaned_cleanup_leaves_diagnostic_stale_result() {
    let (mut registry, token) = active_registry();
    let table = registry.shared.clone();

    accept_user(&mut registry, token, 0);
    assert_eq!(
        table.mark_orphaned(token),
        CompletionMutationOutcome::Applied
    );

    assert!(matches!(
        table.try_take_record(token).unwrap(),
        PollRecordResult::Unavailable {
            kind,
            ..
        } if kind.reason() == CompletionAnomalyReason::StaleGeneration
    ));
    let snapshot = table.completion_diagnostics().snapshot();
    assert_eq!(snapshot.stale_completion, 1);
}
