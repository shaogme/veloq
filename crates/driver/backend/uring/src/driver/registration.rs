use crate::{
    config::{IoFd, OwnedRawHandle, RawHandle, RawHandleKind, UringRawHandle},
    driver::UringDriver,
    error::{UringError, UringResult},
};
use diagweave::prelude::*;
use std::{mem::ManuallyDrop, time::Duration};
use veloq_buf::heap::ChunkId;
use veloq_driver_core::driver::RegisterFd;

pub(crate) const MAX_CHUNKS: usize = 1024;
pub(crate) const REGISTER_FAILURE_RETRY_COOLDOWN: Duration = Duration::from_millis(250);
const MIN_FILE_TABLE_CAPACITY: usize = 1;
const INITIAL_FILE_GENERATION: u64 = 1;

#[derive(Debug)]
pub(crate) struct FileSlot {
    pub(crate) entry: Option<RegisteredFileEntry>,
    pub(crate) generation: u64,
}

#[derive(Debug)]
pub(crate) enum RegisteredFileEntry {
    BorrowedFd { fd: i32, kind: RawHandleKind },
    OwnedHandle(OwnedRawHandle),
}

impl RegisteredFileEntry {
    #[inline]
    pub(crate) fn fd(&self) -> i32 {
        match self {
            Self::BorrowedFd { fd, .. } => *fd,
            Self::OwnedHandle(handle) => handle.raw().as_fd(),
        }
    }

    #[inline]
    pub(crate) fn kind(&self) -> RawHandleKind {
        match self {
            Self::BorrowedFd { kind, .. } => *kind,
            Self::OwnedHandle(handle) => handle.kind(),
        }
    }
}

pub(crate) fn resolve_registered_fixed_fd(
    file_slots: &[FileSlot],
    fd: IoFd,
    expected_kind: Option<RawHandleKind>,
    scope: &'static str,
) -> UringResult<u32> {
    let idx = fd.fixed_index();
    let index = idx as usize;
    let Some(slot) = file_slots.get(index) else {
        return UringError::ResolveFd
            .push_ctx("scope", scope)
            .with_ctx("fd_fixed_index", idx)
            .with_ctx("fd_generation", fd.generation())
            .attach_note("registered file descriptor index out of bounds");
    };

    if slot.generation != fd.generation() {
        return UringError::ResolveFd
            .push_ctx("scope", scope)
            .with_ctx("fd_fixed_index", idx)
            .with_ctx("fd_generation", fd.generation())
            .attach_note("stale registered file descriptor generation")
            .with_ctx("current_generation", slot.generation);
    }

    let Some(entry) = slot.entry.as_ref() else {
        return UringError::ResolveFd
            .push_ctx("scope", scope)
            .with_ctx("fd_fixed_index", idx)
            .with_ctx("fd_generation", fd.generation())
            .attach_note("invalid registered file descriptor");
    };

    if let Some(expected_kind) = expected_kind {
        let current_kind = entry.kind();
        if current_kind != expected_kind {
            return UringError::ResolveFd
                .push_ctx("scope", scope)
                .with_ctx("fd_fixed_index", idx)
                .with_ctx("fd_generation", fd.generation())
                .with_ctx("expected_kind", format!("{expected_kind:?}"))
                .with_ctx("current_kind", format!("{current_kind:?}"))
                .attach_note("registered file descriptor kind mismatch");
        }
    }

    Ok(idx)
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UringRegistrationStats {
    pub(crate) chunk_register_attempts: u64,
    pub(crate) chunk_register_success: u64,
    pub(crate) chunk_register_failures: u64,
    pub(crate) chunk_register_skipped_recent_failure: u64,
    pub(crate) submission_missing_chunk_info: u64,
}

impl<'a> UringDriver<'a> {
    #[inline]
    fn advance_file_generation(generation: &mut u64) {
        *generation = generation.wrapping_add(1);
        if *generation == 0 {
            *generation = INITIAL_FILE_GENERATION;
        }
    }

    #[inline]
    pub(crate) fn register_chunk_internal(
        &mut self,
        id: ChunkId,
        ptr: *const u8,
        len: usize,
    ) -> UringResult<()> {
        self.submit_env().register_chunk(id, ptr, len)
    }

    fn unregister_file_slot(
        &mut self,
        idx: u32,
        advance_generation: bool,
        scope: &'static str,
    ) -> UringResult<()> {
        let index = idx as usize;
        if index >= self.file_slots.len() {
            return Ok(());
        }

        let Some(entry) = self.file_slots[index].entry.take() else {
            return Ok(());
        };

        if let Err(e) = self.ring.submitter().register_files_update(idx, &[-1]) {
            self.file_slots[index].entry = Some(entry);
            return Err(UringError::Registration.io_report(scope, e));
        }

        self.free_file_slots.push(idx);
        if advance_generation {
            Self::advance_file_generation(&mut self.file_slots[index].generation);
        }
        Ok(())
    }

    fn rollback_file_slots(&mut self, registered: &mut Vec<u32>) -> UringResult<()> {
        let mut first_error = None;
        while let Some(idx) = registered.pop() {
            if let Err(report) =
                self.unregister_file_slot(idx, false, "driver.register_files_internal.rollback")
                && first_error.is_none()
            {
                first_error = Some(report);
            }
        }

        if let Some(report) = first_error {
            Err(report.attach_note("registered file rollback failed"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn unregister_fixed_fd(&mut self, fd: IoFd) -> UringResult<()> {
        if !self.file_table_initialized {
            return Ok(());
        }
        let idx = fd.fixed_index();
        let index = idx as usize;
        if index < self.file_slots.len() {
            if self.file_slots[index].generation != fd.generation() {
                return Ok(());
            }
            self.unregister_file_slot(idx, true, "driver.unregister_fixed_fd")?;
        }
        Ok(())
    }

    pub(crate) fn unregister_close_owned_fd(&mut self, fd: IoFd) -> UringResult<()> {
        if !self.file_table_initialized {
            return Ok(());
        }
        let idx = fd.fixed_index();
        let index = idx as usize;
        if index >= self.file_slots.len() {
            return Ok(());
        }
        if self.file_slots[index].generation != fd.generation() {
            return Ok(());
        }
        let Some(entry) = self.file_slots[index].entry.take() else {
            return Ok(());
        };
        if let Err(e) = self.ring.submitter().register_files_update(idx, &[-1]) {
            self.file_slots[index].entry = Some(entry);
            return Err(UringError::Registration.io_report("driver.unregister_close_owned_fd", e));
        }
        self.free_file_slots.push(idx);
        Self::advance_file_generation(&mut self.file_slots[index].generation);
        let _ = ManuallyDrop::new(entry);
        Ok(())
    }

    pub(crate) fn replace_registered_fixed_fd(
        &mut self,
        fixed_fd: IoFd,
        raw: RawHandle,
    ) -> UringResult<()> {
        if !self.file_table_initialized {
            return UringError::InvalidState
                .push_ctx("scope", "driver.replace_registered_fixed_fd")
                .with_ctx("fd_fixed_index", fixed_fd.fixed_index())
                .attach_note("registered file table is not initialized");
        }

        let idx = fixed_fd.fixed_index();
        let index = idx as usize;
        let Some(slot) = self.file_slots.get_mut(index) else {
            return UringError::InvalidState
                .push_ctx("scope", "driver.replace_registered_fixed_fd")
                .with_ctx("fd_fixed_index", idx)
                .with_ctx("fd_generation", fixed_fd.generation())
                .attach_note("registered file index out of bounds");
        };
        if slot.generation != fixed_fd.generation() {
            return UringError::InvalidState
                .push_ctx("scope", "driver.replace_registered_fixed_fd")
                .with_ctx("fd_fixed_index", idx)
                .with_ctx("fd_generation", fixed_fd.generation())
                .attach_note("registered file generation mismatch while replacing fd");
        }
        if slot.entry.is_none() {
            return UringError::InvalidState
                .push_ctx("scope", "driver.replace_registered_fixed_fd")
                .with_ctx("fd_fixed_index", idx)
                .with_ctx("fd_generation", fixed_fd.generation())
                .attach_note("registered file slot is empty while replacing fd");
        }

        let fd = raw.raw().as_fd();
        self.ring
            .submitter()
            .register_files_update(idx, &[fd])
            .map_err(|e| {
                UringError::Registration.io_report(
                    "driver.replace_registered_fixed_fd.register_files_update",
                    e,
                )
            })?;
        slot.entry = Some(RegisteredFileEntry::BorrowedFd {
            fd,
            kind: raw.kind(),
        });
        Ok(())
    }

    pub(crate) fn ensure_file_table_initialized(&mut self) -> UringResult<()> {
        if self.file_table_initialized {
            return Ok(());
        }

        let capacity = self.ops.capacity().max(MIN_FILE_TABLE_CAPACITY);
        let sparse = vec![-1; capacity];
        self.ring.submitter().register_files(&sparse).map_err(|e| {
            UringError::Registration.io_report("driver.ensure_file_table_initialized", e)
        })?;

        self.file_slots = (0..capacity)
            .map(|_| FileSlot {
                entry: None,
                generation: INITIAL_FILE_GENERATION,
            })
            .collect();
        self.free_file_slots = (0..capacity as u32).rev().collect();
        self.file_table_initialized = true;
        Ok(())
    }

    /// Registers `fds` into the `fds.len()` consecutive table slots starting at `start`.
    fn register_file_run(&mut self, start: u32, fds: &[i32]) -> UringResult<()> {
        let updated = self
            .ring
            .submitter()
            .register_files_update(start, fds)
            .map_err(|e| {
                UringError::Registration
                    .io_report("driver.register_files_internal.register_files_update", e)
            })?;
        if updated != fds.len() {
            return UringError::Registration
                .push_ctx(
                    "scope",
                    "driver.register_files_internal.register_files_update",
                )
                .with_ctx("start_index", start)
                .with_ctx("requested_files", fds.len())
                .with_ctx("updated_files", updated)
                .attach_note("io_uring updated fewer registered file entries than requested");
        }
        Ok(())
    }

    pub(crate) fn register_files_internal<'h>(
        &mut self,
        files: Vec<RegisterFd<'h, UringRawHandle>>,
    ) -> UringResult<Vec<IoFd>> {
        self.ensure_file_table_initialized()?;

        let requested = files.len();
        let available = self.free_file_slots.len();
        if requested > available {
            return UringError::InvalidState
                .push_ctx("scope", "driver.register_files_internal")
                .with_ctx("requested_files", requested)
                .with_ctx("free_file_slots", available)
                .attach_note("io_uring registered file table exhausted");
        }
        if requested == 0 {
            return Ok(Vec::new());
        }

        // Claim every slot up front and register them in ascending order. `free_file_slots` is
        // seeded in reverse, so a fresh table hands out consecutive indices and the whole batch
        // collapses into a single `register_files_update` instead of one syscall per fd.
        let mut slots = Vec::with_capacity(requested);
        for _ in 0..requested {
            slots.push(self.free_file_slots.pop().expect(
                "register_files_internal capacity precheck guarantees enough free file slots",
            ));
        }
        slots.sort_unstable();

        let entries = files
            .into_iter()
            .map(|file| match file {
                RegisterFd::Borrowed(b) => RegisteredFileEntry::BorrowedFd {
                    fd: b.raw().as_fd(),
                    kind: b.kind(),
                },
                RegisterFd::Owned(o) => RegisteredFileEntry::OwnedHandle(o),
            })
            .collect::<Vec<_>>();
        let fds = entries
            .iter()
            .map(RegisteredFileEntry::fd)
            .collect::<Vec<_>>();
        let mut entries = entries.into_iter();

        let mut registered_slots = Vec::with_capacity(requested);
        let mut cursor = 0usize;
        while cursor < requested {
            let run_end = consecutive_run_end(&slots, cursor);
            let outcome = self.register_file_run(slots[cursor], &fds[cursor..run_end]);

            // The run's entries are recorded even when the update failed: a partial update may
            // have left some fds in the kernel table, and the rollback below needs the entries
            // in place to reset those slots to -1.
            for idx in slots[cursor..run_end].iter().copied() {
                let entry = entries
                    .next()
                    .expect("one registered file entry per claimed file slot");
                self.file_slots[idx as usize].entry = Some(entry);
                registered_slots.push(idx);
            }
            cursor = run_end;

            if let Err(report) = outcome {
                // Hand back the slots this batch never got to.
                self.free_file_slots.extend_from_slice(&slots[cursor..]);
                if let Err(rollback_report) = self.rollback_file_slots(&mut registered_slots) {
                    return Err(rollback_report
                        .attach_note("rollback failed after registered file update failure"));
                }
                return Err(report);
            }
        }

        // `slots` is sorted and `entries` was consumed in the same order, so slot `i` still
        // belongs to input file `i`.
        let fixed_fds = slots
            .iter()
            .copied()
            .map(|idx| IoFd::fixed_with_generation(idx, self.file_slots[idx as usize].generation))
            .collect();
        Ok(fixed_fds)
    }
}

/// Returns the end (exclusive) of the run of consecutive indices starting at `start`.
fn consecutive_run_end(slots: &[u32], start: usize) -> usize {
    let mut end = start + 1;
    while end < slots.len() && slots[end] == slots[end - 1] + 1 {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::consecutive_run_end;

    /// Walks `slots` the way `register_files_internal` does, collecting one run per syscall.
    fn runs(slots: &[u32]) -> Vec<&[u32]> {
        let mut runs = Vec::new();
        let mut cursor = 0;
        while cursor < slots.len() {
            let end = consecutive_run_end(slots, cursor);
            runs.push(&slots[cursor..end]);
            cursor = end;
        }
        runs
    }

    #[test]
    fn a_fresh_file_table_registers_the_whole_batch_in_one_call() {
        assert_eq!(runs(&[0, 1, 2, 3]), vec![&[0, 1, 2, 3][..]]);
    }

    #[test]
    fn holes_in_the_free_list_split_the_batch_into_runs() {
        assert_eq!(
            runs(&[1, 2, 5, 9, 10]),
            vec![&[1, 2][..], &[5][..], &[9, 10][..]]
        );
    }

    #[test]
    fn a_single_slot_is_one_run() {
        assert_eq!(runs(&[7]), vec![&[7][..]]);
    }
}
