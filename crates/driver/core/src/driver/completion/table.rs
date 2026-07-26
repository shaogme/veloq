use crate::DriverError;
use crate::slot::{self, Generation};
use diagweave::prelude::*;
use std::{sync::Arc, task::Waker};
use veloq_std::sync::atomic::Ordering;

use super::{
    AnomalyAttach, AnomalyOutcome, CompletionAnomalyKind, CompletionInput, CompletionPacket,
    CompletionRecord, CompletionWritePermit, DriverCompletionDiagnosticsBackend, OpToken,
    RecordCompletionOutcome, RecordCompletionResult, UserCompletionEvent, run_completion_cleanup,
    types::CompletionMutationOutcome,
};

pub type SharedCompletionTable<Spec> = Arc<dyn CompletionAccess<Spec>>;

/// Result of a completion poll, enabling detection of recycled slots.
pub enum PollRecordResult<Spec: slot::SlotSpec> {
    /// Operation completed successfully or with an error.
    Ready(CompletionRecord<Spec>),
    /// Operation completion became unavailable; materialize at the poll boundary.
    Unavailable {
        kind: CompletionAnomalyKind,
        attach: AnomalyAttach,
    },
    /// Operation is still in flight.
    Pending,
}

pub trait CompletionAccess<Spec: slot::SlotSpec>: Send + Sync {
    fn record_completion(
        &self,
        permit: CompletionWritePermit,
        packet: CompletionPacket<Spec>,
    ) -> RecordCompletionResult<Spec>;

    fn try_take_record(
        &self,
        token: OpToken,
    ) -> Result<PollRecordResult<Spec>, Report<Spec::Error>>;

    fn register_waker(&self, token: OpToken, waker: &Waker) -> CompletionMutationOutcome;

    fn mark_waiting(&self, token: OpToken) -> CompletionMutationOutcome;

    fn discard_ready_record(&self, token: OpToken) -> CompletionMutationOutcome;

    fn mark_orphaned(&self, token: OpToken) -> CompletionMutationOutcome;

    #[cfg(any(test, feature = "loom"))]
    fn debug_get_state(&self, idx: usize) -> u8;
}

pub const CELL_STATE_IDLE: u8 = 0;
pub const CELL_STATE_WAITING: u8 = 1;
pub const CELL_STATE_READY: u8 = 2;
pub const CELL_STATE_ORPHANED: u8 = 3;
pub const CELL_STATE_BUSY: u8 = 4;

#[inline(always)]
fn spin_yield() {
    #[cfg(feature = "loom")]
    let _ = veloq_std::thread::yield_now();
    #[cfg(not(feature = "loom"))]
    std::hint::spin_loop();
}

#[inline]
fn recorded_outcome<Spec: slot::SlotSpec>(
    input: &CompletionInput<Spec>,
) -> RecordCompletionOutcome {
    match input {
        CompletionInput::User(_) => RecordCompletionOutcome::RecordedUser,
    }
}

#[inline]
fn mutation_missing(token: OpToken) -> CompletionMutationOutcome {
    let (idx, generation) = token.parts();
    CompletionMutationOutcome::Rejected(AnomalyOutcome::Missing(
        CompletionAnomalyKind::unknown_slot(idx, generation),
    ))
}

#[inline]
fn mutation_generation_mismatch(
    idx: usize,
    expected_generation: Generation,
    actual_generation: Generation,
    status: slot::SlotStatus,
) -> CompletionMutationOutcome {
    if actual_generation.is_newer_than(expected_generation) {
        CompletionMutationOutcome::Rejected(AnomalyOutcome::Stale(CompletionAnomalyKind::stale(
            idx,
            expected_generation,
            actual_generation,
            status,
        )))
    } else {
        CompletionMutationOutcome::Rejected(AnomalyOutcome::NonActive(
            CompletionAnomalyKind::non_active(idx, expected_generation, status),
        ))
    }
}

#[inline]
fn mutation_non_active(
    idx: usize,
    generation: Generation,
    status: slot::SlotStatus,
) -> CompletionMutationOutcome {
    CompletionMutationOutcome::Rejected(AnomalyOutcome::NonActive(
        CompletionAnomalyKind::non_active(idx, generation, status),
    ))
}

impl<Spec> slot::SlotTable<Spec>
where
    Spec: slot::SlotSpec,
    slot::SlotPayload<Spec>: Send,
    slot::SlotError<Spec>: Send,
    slot::SlotCompletion<Spec>: Send,
    Spec::CompletionDiagnostics: DriverCompletionDiagnosticsBackend,
{
    #[inline]
    fn recorded_completion(
        &self,
        outcome: RecordCompletionOutcome,
    ) -> RecordCompletionResult<Spec> {
        self.diagnostics.record_completion_outcome(&outcome);
        RecordCompletionResult::Recorded(outcome)
    }

    #[inline]
    fn rejected_completion(
        &self,
        outcome: RecordCompletionOutcome,
        event: UserCompletionEvent,
        packet: CompletionPacket<Spec>,
    ) -> RecordCompletionResult<Spec> {
        if let RecordCompletionOutcome::Rejected(anomaly_outcome) = outcome {
            self.diagnostics.record_anomaly_outcome(
                anomaly_outcome,
                AnomalyAttach::from_raw_completion(event.raw()),
            );
        }
        self.diagnostics.record_completion_outcome(&outcome);
        RecordCompletionResult::Rejected {
            outcome,
            packet: Box::new(packet),
        }
    }

    #[inline]
    fn recorded_mutation(
        &self,
        token: OpToken,
        outcome: CompletionMutationOutcome,
    ) -> CompletionMutationOutcome {
        if let Some(anomaly_outcome) = outcome.anomaly_outcome() {
            self.diagnostics
                .record_anomaly_outcome(anomaly_outcome, AnomalyAttach::from_op_token(token));
        }
        outcome
    }

    /// 丢弃一条已就绪但不会再被消费的完成：归还 payload、丢掉 detail，并执行
    /// `cleanup`（完成式 I/O 下内核可能仍持有其中的资源）。
    #[inline]
    pub(crate) fn run_discarded_record_cleanup(&self, record_data: slot::CompletionData<Spec>) {
        match record_data {
            slot::CompletionData::User {
                event: _,
                payload,
                detail,
                mut cleanup,
            } => {
                drop(payload);
                drop(detail);
                let _ = run_completion_cleanup(&self.diagnostics, &mut cleanup);
            }
            slot::CompletionData::Empty => {}
        }
    }
}

impl<Spec> CompletionAccess<Spec> for slot::SlotTable<Spec>
where
    Spec: slot::SlotSpec,
    slot::SlotPayload<Spec>: Send,
    slot::SlotError<Spec>: Send,
    slot::SlotCompletion<Spec>: Send,
{
    fn record_completion(
        &self,
        _permit: CompletionWritePermit,
        packet: CompletionPacket<Spec>,
    ) -> RecordCompletionResult<Spec> {
        let op_token = packet.token();
        let event = packet.event;
        let (idx, generation) = op_token.parts();
        let success_outcome = recorded_outcome(&packet.input);
        if idx >= self.slots.len() {
            return self.rejected_completion(
                RecordCompletionOutcome::Rejected(AnomalyOutcome::Missing(
                    CompletionAnomalyKind::unknown_slot(idx, generation),
                )),
                event,
                packet,
            );
        }
        let cell = &self.slots[idx];

        // 判定顺序统一为 generation → finalizing → ready → state：前两项是「这条完成
        // 该不该由我处理」，`ready` 是信箱维度（已有一条未消费的记录就不能覆盖），最后
        // 才轮到 slot 的生命周期。
        let claimed = loop {
            let current = cell.load_core_state(Ordering::Acquire);
            let status = current.status();
            let cell_gen = current.generation();

            if generation.is_older_than(cell_gen) {
                return self.rejected_completion(
                    RecordCompletionOutcome::Rejected(AnomalyOutcome::Stale(
                        CompletionAnomalyKind::stale(idx, generation, cell_gen, status),
                    )),
                    event,
                    packet,
                );
            }
            if generation.is_newer_than(cell_gen) {
                let outcome = if status.is_idle() {
                    RecordCompletionOutcome::Rejected(AnomalyOutcome::NonActive(
                        CompletionAnomalyKind::non_active(idx, generation, status),
                    ))
                } else {
                    RecordCompletionOutcome::Rejected(AnomalyOutcome::Stale(
                        CompletionAnomalyKind::stale(idx, generation, cell_gen, status),
                    ))
                };
                return self.rejected_completion(outcome, event, packet);
            }

            if status.finalizing {
                spin_yield();
                continue;
            }
            if status.ready {
                return self.rejected_completion(
                    RecordCompletionOutcome::Rejected(AnomalyOutcome::NonActive(
                        CompletionAnomalyKind::non_active(idx, generation, status),
                    )),
                    event,
                    packet,
                );
            }

            match status.state {
                slot::SlotState::InFlightWaiting => match cell.core_state.compare_exchange(
                    current,
                    current.with_finalizing(true).with_generation(generation),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break current.with_finalizing(true),
                    Err(_) => continue,
                },
                slot::SlotState::Idle | slot::SlotState::Reserved => {
                    return self.rejected_completion(
                        RecordCompletionOutcome::Rejected(AnomalyOutcome::NonActive(
                            CompletionAnomalyKind::non_active(idx, generation, status),
                        )),
                        event,
                        packet,
                    );
                }
                slot::SlotState::InFlightOrphaned => {
                    return self.rejected_completion(
                        RecordCompletionOutcome::OrphanedDropped,
                        event,
                        packet,
                    );
                }
            }
        };

        let input = packet.input;
        cell.completion_with_record_data(|record| {
            *record = match input {
                CompletionInput::User(completion) => slot::CompletionData::User {
                    event,
                    payload: completion.payload,
                    detail: completion.detail,
                    cleanup: completion.cleanup,
                },
            };
        });
        cell.completion_res.store(event.res(), Ordering::Release);
        cell.completion_flags
            .store(event.flags(), Ordering::Release);
        self.note_ready_completion();
        // 发布：清掉 finalizing、立起 ready。生命周期状态**保持不变**——slot 的归还由
        // driver 线程随后的 `finalize_*` → `free()` 负责，两件事互不依赖。
        cell.core_state.store(
            claimed
                .with_finalizing(false)
                .with_ready(true)
                .with_generation(generation),
            Ordering::Release,
        );

        cell.completion_waker.wake();
        self.recorded_completion(success_outcome)
    }

    fn try_take_record(
        &self,
        token: OpToken,
    ) -> Result<PollRecordResult<Spec>, Report<Spec::Error>> {
        let attach = AnomalyAttach::from_op_token(token);
        let (idx, generation) = token.parts();
        if idx >= self.slots.len() {
            let kind = CompletionAnomalyKind::unknown_slot(idx, generation);
            self.diagnostics.record_anomaly_kind(kind, attach);
            return Ok(PollRecordResult::Unavailable { kind, attach });
        }
        let cell = &self.slots[idx];

        let current = cell.load_core_state(Ordering::Acquire);
        let status = current.status();
        let cell_gen = current.generation();

        if cell_gen.is_newer_than(generation) {
            let kind = CompletionAnomalyKind::stale(idx, generation, cell_gen, status);
            self.diagnostics.record_anomaly_kind(kind, attach);
            return Ok(PollRecordResult::Unavailable { kind, attach });
        }

        if cell_gen.is_older_than(generation) {
            let kind = CompletionAnomalyKind::non_active(idx, generation, status);
            self.diagnostics.record_anomaly_kind(kind, attach);
            return Ok(PollRecordResult::Unavailable { kind, attach });
        }

        if !status.ready {
            // 消费方不自旋：发布中（`finalizing`）与在途一样按 Pending 处理，发布完成
            // 后 `record_completion` 会唤醒已注册的 waker。
            return if status.finalizing || status.state == slot::SlotState::InFlightWaiting {
                Ok(PollRecordResult::Pending)
            } else {
                let kind = CompletionAnomalyKind::non_active(idx, generation, status);
                self.diagnostics.record_anomaly_kind(kind, attach);
                Ok(PollRecordResult::Unavailable { kind, attach })
            };
        }

        if cell
            .core_state
            .compare_exchange(
                current,
                current
                    .with_ready(false)
                    .with_state(slot::SlotState::Idle)
                    .with_generation(generation.next()),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(PollRecordResult::Pending);
        }

        self.clear_ready_completion();
        let record_data = cell.completion_with_record_data(std::mem::take);

        match record_data {
            slot::CompletionData::User {
                event,
                payload,
                detail,
                cleanup,
            } => Ok(PollRecordResult::Ready(CompletionRecord {
                event,
                payload,
                detail,
                cleanup,
            })),
            slot::CompletionData::Empty => {
                let report = crate::DriverCoreError::Internal
                    .to_report()
                    .push_ctx("scope", "try_take_record")
                    .attach_note(format!(
                        "corrupt slot state: mailbox marked ready but holds no record. index: {}, generation: {}",
                        idx, generation
                    ));
                Err(Spec::Error::from_core_report(report))
            }
        }
    }

    fn register_waker(&self, token: OpToken, waker: &Waker) -> CompletionMutationOutcome {
        let (idx, generation) = token.parts();
        if idx >= self.slots.len() {
            return self.recorded_mutation(token, mutation_missing(token));
        }
        let cell = &self.slots[idx];

        let current = cell.load_core_state(Ordering::Acquire);
        let status = current.status();
        let cell_gen = current.generation();

        if cell_gen != generation {
            return self.recorded_mutation(
                token,
                mutation_generation_mismatch(idx, generation, cell_gen, status),
            );
        }

        cell.completion_waker.register(waker);

        let current_after = cell.load_core_state(Ordering::Acquire);
        let status_after = current_after.status();
        let generation_after = current_after.generation();
        if generation_after != generation {
            return self.recorded_mutation(
                token,
                mutation_generation_mismatch(idx, generation, generation_after, status_after),
            );
        }
        if status_after.ready {
            // 注册与发布竞速：记录已就绪，自己唤醒自己，避免丢唤醒。
            waker.wake_by_ref();
            return self.recorded_mutation(token, CompletionMutationOutcome::Applied);
        }

        let outcome =
            if status_after.finalizing || status_after.state == slot::SlotState::InFlightWaiting {
                CompletionMutationOutcome::Applied
            } else {
                mutation_non_active(idx, generation, status_after)
            };
        self.recorded_mutation(token, outcome)
    }

    fn mark_waiting(&self, token: OpToken) -> CompletionMutationOutcome {
        let (idx, generation) = token.parts();
        if idx >= self.slots.len() {
            return self.recorded_mutation(token, mutation_missing(token));
        }
        let cell = &self.slots[idx];

        loop {
            let current = cell.load_core_state(Ordering::Acquire);
            let status = current.status();
            let cell_generation = current.generation();

            if cell_generation != generation {
                return self.recorded_mutation(
                    token,
                    mutation_generation_mismatch(idx, generation, cell_generation, status),
                );
            }

            if status.finalizing {
                spin_yield();
                continue;
            }
            if status.ready {
                return self.recorded_mutation(token, CompletionMutationOutcome::Applied);
            }

            return match status.state {
                slot::SlotState::InFlightWaiting => {
                    self.recorded_mutation(token, CompletionMutationOutcome::Applied)
                }
                slot::SlotState::Idle
                | slot::SlotState::Reserved
                | slot::SlotState::InFlightOrphaned => {
                    self.recorded_mutation(token, mutation_non_active(idx, generation, status))
                }
            };
        }
    }

    fn discard_ready_record(&self, token: OpToken) -> CompletionMutationOutcome {
        let (idx, generation) = token.parts();
        if idx >= self.slots.len() {
            return self.recorded_mutation(token, mutation_missing(token));
        }
        let cell = &self.slots[idx];

        loop {
            let current = cell.load_core_state(Ordering::Acquire);
            let status = current.status();
            let cell_gen = current.generation();

            if status.finalizing {
                spin_yield();
                continue;
            }
            if cell_gen != generation {
                return self.recorded_mutation(
                    token,
                    mutation_generation_mismatch(idx, generation, cell_gen, status),
                );
            }
            if !status.ready {
                return self.recorded_mutation(token, mutation_non_active(idx, generation, status));
            }

            if cell
                .core_state
                .compare_exchange(
                    current,
                    current
                        .with_ready(false)
                        .with_state(slot::SlotState::Idle)
                        .with_generation(generation.next()),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.clear_ready_completion();
                let record_data = cell.completion_with_record_data(std::mem::take);
                self.run_discarded_record_cleanup(record_data);
                return self.recorded_mutation(token, CompletionMutationOutcome::Applied);
            }
        }
    }

    fn mark_orphaned(&self, token: OpToken) -> CompletionMutationOutcome {
        let (idx, generation) = token.parts();
        if idx >= self.slots.len() {
            return self.recorded_mutation(token, mutation_missing(token));
        }
        let cell = &self.slots[idx];

        loop {
            let current = cell.load_core_state(Ordering::Acquire);
            let status = current.status();
            let cell_gen = current.generation();

            if status.finalizing {
                spin_yield();
                continue;
            }
            if cell_gen != generation {
                return self.recorded_mutation(
                    token,
                    mutation_generation_mismatch(idx, generation, cell_gen, status),
                );
            }
            // 完成已经发布：放弃它等于丢弃信箱里那条记录，走 discard 才能跑 cleanup。
            if status.ready {
                return self.discard_ready_record(token);
            }

            match status.state {
                slot::SlotState::InFlightWaiting => {
                    if cell
                        .core_state
                        .compare_exchange(
                            current,
                            current
                                .with_state(slot::SlotState::InFlightOrphaned)
                                .with_generation(generation),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return self.recorded_mutation(token, CompletionMutationOutcome::Applied);
                    }
                }
                slot::SlotState::Idle
                | slot::SlotState::Reserved
                | slot::SlotState::InFlightOrphaned => {
                    return self
                        .recorded_mutation(token, mutation_non_active(idx, generation, status));
                }
            }
        }
    }

    #[cfg(any(test, feature = "loom"))]
    fn debug_get_state(&self, idx: usize) -> u8 {
        let status = self.slots[idx].load_core_state(Ordering::Acquire).status();
        if status.finalizing {
            return CELL_STATE_BUSY;
        }
        if status.ready {
            return CELL_STATE_READY;
        }
        match status.state {
            slot::SlotState::InFlightWaiting => CELL_STATE_WAITING,
            slot::SlotState::InFlightOrphaned => CELL_STATE_ORPHANED,
            slot::SlotState::Idle | slot::SlotState::Reserved => CELL_STATE_IDLE,
        }
    }
}

#[cfg(test)]
#[cfg(not(feature = "loom"))]
mod tests;
