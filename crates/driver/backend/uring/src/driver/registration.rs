use crate::{
    config::{IoFd, RawHandle, UringRawHandle},
    driver::UringDriver,
    error::{UringError, UringResult},
};
use diagweave::prelude::*;
use std::{mem::ManuallyDrop, time::Duration};
use veloq_buf::heap::ChunkId;
use veloq_driver_core::driver::RegisterFd;

mod file_table;

pub(crate) use file_table::{ClaimedSlots, FileTable, RegisteredFileEntry, SqeFd};

pub(crate) const MAX_CHUNKS: usize = 1024;
pub(crate) const REGISTER_FAILURE_RETRY_COOLDOWN: Duration = Duration::from_millis(250);
/// Upper bound on [`UringConfig::file_table_capacity`](crate::config::UringConfig).
///
/// The kernel's own limit is smaller still (`IORING_MAX_FIXED_FILES`), but it rejects an
/// oversized table only *after* the sparse `-1` vector has been built — and that vector is
/// four bytes per entry, so an unchecked `u32` would ask for gigabytes before the syscall got
/// a chance to say no.
const MAX_FILE_TABLE_CAPACITY: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UringRegistrationStats {
    pub(crate) chunk_register_attempts: u64,
    pub(crate) chunk_register_success: u64,
    pub(crate) chunk_register_failures: u64,
    pub(crate) chunk_register_skipped_recent_failure: u64,
    pub(crate) submission_missing_chunk_info: u64,
    /// Descriptors handed out without a kernel table entry because the table was full.
    pub(crate) file_table_fallback_registrations: u64,
}

impl<'a> UringDriver<'a> {
    #[inline]
    pub(crate) fn register_chunk_internal(
        &mut self,
        id: ChunkId,
        ptr: *const u8,
        len: usize,
    ) -> UringResult<()> {
        self.submit_env().register_chunk(id, ptr, len)
    }

    /// Clears the kernel table entry for `idx`, if it has one.
    ///
    /// Fallback slots live only in userspace, so releasing one is pure bookkeeping — skipping
    /// the `register_files_update` there is not an optimisation, it is required: the kernel
    /// table has no entry at that index to reset.
    fn clear_kernel_file_entry(&mut self, idx: u32, scope: &'static str) -> UringResult<()> {
        if !self.file_table.is_fixed(idx) {
            return Ok(());
        }
        self.ring
            .submitter()
            .register_files_update(idx, &[-1])
            .map(|_| ())
            .map_err(|e| UringError::Registration.io_report(scope, e))
    }

    fn unregister_file_slot(
        &mut self,
        idx: u32,
        advance_generation: bool,
        scope: &'static str,
    ) -> UringResult<()> {
        let Some(entry) = self.file_table.take_entry(idx) else {
            return Ok(());
        };

        if let Err(report) = self.clear_kernel_file_entry(idx, scope) {
            self.file_table.install_entry(idx, entry);
            return Err(report);
        }

        self.file_table.release(idx);
        if advance_generation {
            self.file_table.advance_generation(idx);
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
        if !self.file_table.is_initialized() || !self.file_table.matches_generation(fd) {
            return Ok(());
        }
        self.unregister_file_slot(fd.fixed_index(), true, "driver.unregister_fixed_fd")
    }

    /// Retires the slot behind a `Close` whose descriptor the kernel already closed.
    pub(crate) fn unregister_close_owned_fd(&mut self, fd: IoFd) -> UringResult<()> {
        if !self.file_table.is_initialized() || !self.file_table.matches_generation(fd) {
            return Ok(());
        }
        let idx = fd.fixed_index();
        let Some(entry) = self.file_table.take_entry(idx) else {
            return Ok(());
        };
        if let Err(report) = self.clear_kernel_file_entry(idx, "driver.unregister_close_owned_fd") {
            self.file_table.install_entry(idx, entry);
            return Err(report);
        }
        self.file_table.release(idx);
        self.file_table.advance_generation(idx);
        // The `Close` operation already closed the descriptor; dropping the owned handle here
        // would close a number the kernel may have handed to someone else.
        let _ = ManuallyDrop::new(entry);
        Ok(())
    }

    pub(crate) fn replace_registered_fixed_fd(
        &mut self,
        fixed_fd: IoFd,
        raw: RawHandle,
    ) -> UringResult<()> {
        let scope = "driver.replace_registered_fixed_fd";
        let idx = fixed_fd.fixed_index();
        let invalid = |note: &'static str| {
            UringError::InvalidState
                .push_ctx("scope", scope)
                .with_ctx("fd_fixed_index", idx)
                .with_ctx("fd_generation", fixed_fd.generation())
                .attach_note(note)
        };

        if !self.file_table.is_initialized() {
            return invalid("registered file table is not initialized");
        }
        if self.file_table.generation(idx).is_none() {
            return invalid("registered file index out of bounds");
        }
        if !self.file_table.matches_generation(fixed_fd) {
            return invalid("registered file generation mismatch while replacing fd");
        }
        if self.file_table.entry(idx).is_none() {
            return invalid("registered file slot is empty while replacing fd");
        }

        let fd = raw.raw().as_fd();
        if self.file_table.is_fixed(idx) {
            self.ring
                .submitter()
                .register_files_update(idx, &[fd])
                .map_err(|e| {
                    UringError::Registration.io_report(
                        "driver.replace_registered_fixed_fd.register_files_update",
                        e,
                    )
                })?;
        }
        self.file_table.install_entry(
            idx,
            RegisteredFileEntry::BorrowedFd {
                fd,
                kind: raw.kind(),
            },
        );
        Ok(())
    }

    pub(crate) fn ensure_file_table_initialized(&mut self) -> UringResult<()> {
        if self.file_table.is_initialized() {
            return Ok(());
        }

        let capacity = self.file_table.fixed_capacity();
        if capacity > MAX_FILE_TABLE_CAPACITY {
            return UringError::InvalidInput
                .push_ctx("scope", "driver.ensure_file_table_initialized")
                .with_ctx("file_table_capacity", capacity)
                .with_ctx("max_file_table_capacity", MAX_FILE_TABLE_CAPACITY)
                .attach_note("configured registered file table capacity is too large");
        }
        if capacity > 0 {
            let sparse = vec![-1; capacity];
            self.ring.submitter().register_files(&sparse).map_err(|e| {
                UringError::Registration.io_report("driver.ensure_file_table_initialized", e)
            })?;
        }

        self.file_table.mark_initialized();
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

        if files.is_empty() {
            return Ok(Vec::new());
        }

        let claimed = self
            .file_table
            .claim(files.len())
            .push_ctx("scope", "driver.register_files_internal")?;
        self.registration_stats.file_table_fallback_registrations = self
            .registration_stats
            .file_table_fallback_registrations
            .saturating_add(claimed.direct.len() as u64);

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

        self.install_claimed_files(claimed, entries)
    }

    /// Publishes `entries` into `claimed`, telling the kernel about the fixed half.
    ///
    /// `entries` is paired with the slots in [`ClaimedSlots::in_input_order`], so the returned
    /// descriptors line up with the caller's input.
    fn install_claimed_files(
        &mut self,
        claimed: ClaimedSlots,
        entries: Vec<RegisteredFileEntry>,
    ) -> UringResult<Vec<IoFd>> {
        debug_assert_eq!(claimed.fixed.len() + claimed.direct.len(), entries.len());
        let fds = entries
            .iter()
            .map(RegisteredFileEntry::fd)
            .collect::<Vec<_>>();
        let mut entries = entries.into_iter();

        let mut installed = Vec::with_capacity(fds.len());
        let mut cursor = 0usize;
        while cursor < claimed.fixed.len() {
            let run_end = consecutive_run_end(&claimed.fixed, cursor);
            let outcome = self.register_file_run(claimed.fixed[cursor], &fds[cursor..run_end]);

            // The run's entries are recorded even when the update failed: a partial update may
            // have left some fds in the kernel table, and the rollback below needs the entries
            // in place to reset those slots to -1.
            for idx in claimed.fixed[cursor..run_end].iter().copied() {
                let entry = entries
                    .next()
                    .expect("one registered file entry per claimed file slot");
                self.file_table.install_entry(idx, entry);
                installed.push(idx);
            }
            cursor = run_end;

            if let Err(report) = outcome {
                // Hand back the slots this batch never got to.
                self.file_table
                    .release_all(claimed.fixed[cursor..].iter().copied());
                self.file_table.release_all(claimed.direct.iter().copied());
                if let Err(rollback_report) = self.rollback_file_slots(&mut installed) {
                    return Err(rollback_report
                        .attach_note("rollback failed after registered file update failure"));
                }
                return Err(report);
            }
        }

        // Fallback slots need no kernel round trip at all — the raw fd travels in the SQE.
        for idx in claimed.direct.iter().copied() {
            let entry = entries
                .next()
                .expect("one registered file entry per claimed file slot");
            self.file_table.install_entry(idx, entry);
        }

        Ok(claimed
            .in_input_order()
            .map(|idx| {
                self.file_table
                    .descriptor(idx)
                    .expect("installed slot exists")
            })
            .collect())
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

    /// Walks `slots` the way `install_claimed_files` does, collecting one run per syscall.
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
