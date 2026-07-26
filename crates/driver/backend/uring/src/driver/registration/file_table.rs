//! Bookkeeping for the descriptors a [`UringDriver`](crate::driver::UringDriver) hands out.
//!
//! The kernel's registered file table is a fixed-size allocation sized by
//! [`UringConfig::file_table_capacity`](crate::config::UringConfig::file_table_capacity). This
//! table is the userspace mirror of it, plus the overflow area: slots past `fixed_capacity`
//! have no kernel entry and their submissions carry the raw fd instead of a fixed index.
//!
//! Both kinds live in one `slots` vector so an [`IoFd`] means the same thing either way — a
//! slot index plus a generation — and callers only learn which kind they got when they resolve
//! it into an [`SqeFd`] at submission time.

use crate::{
    config::{FileTableExhaustion, IoFd, OwnedRawHandle, RawHandleKind},
    error::{UringError, UringResult},
};
use diagweave::prelude::*;
use tracing::warn;

const INITIAL_FILE_GENERATION: u64 = 1;

/// How a resolved descriptor is spelled in an SQE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqeFd {
    /// An index into the kernel's registered file table; the SQE sets `IOSQE_FIXED_FILE`.
    Fixed(u32),
    /// A raw descriptor, submitted without the registered-file fast path.
    Direct(i32),
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

#[derive(Debug)]
struct FileSlot {
    entry: Option<RegisteredFileEntry>,
    generation: u64,
}

impl FileSlot {
    #[inline]
    const fn vacant() -> Self {
        Self {
            entry: None,
            generation: INITIAL_FILE_GENERATION,
        }
    }
}

/// The slot indices a batch registration reserved, split by how they reach the kernel.
///
/// `fixed` is sorted ascending so consecutive runs can be pushed to the kernel in one
/// `register_files_update`, and it always corresponds to the *first* `fixed.len()` inputs of the
/// batch — the caller pairs entries with slots by walking `fixed` then `direct`.
#[derive(Debug, Default)]
pub(crate) struct ClaimedSlots {
    pub(crate) fixed: Vec<u32>,
    pub(crate) direct: Vec<u32>,
}

impl ClaimedSlots {
    /// Every claimed index, in the order inputs were assigned to them.
    #[inline]
    pub(crate) fn in_input_order(&self) -> impl Iterator<Item = u32> + '_ {
        self.fixed.iter().chain(self.direct.iter()).copied()
    }
}

pub(crate) struct FileTable {
    /// `slots[..fixed_capacity]` mirror the kernel table one-to-one; the rest are fallback
    /// slots that exist only here. The vector tracks the high-water mark of concurrently
    /// registered descriptors — released fallback slots go back to `free_direct` for reuse
    /// rather than shrinking it, so an [`IoFd`] index stays meaningful for the driver's life.
    slots: Vec<FileSlot>,
    fixed_capacity: usize,
    free_fixed: Vec<u32>,
    free_direct: Vec<u32>,
    exhaustion: FileTableExhaustion,
    initialized: bool,
    fallback_reported: bool,
}

impl FileTable {
    pub(crate) fn new(fixed_capacity: u32, exhaustion: FileTableExhaustion) -> Self {
        Self {
            slots: Vec::new(),
            fixed_capacity: fixed_capacity as usize,
            free_fixed: Vec::new(),
            free_direct: Vec::new(),
            exhaustion,
            initialized: false,
            fallback_reported: false,
        }
    }

    #[inline]
    pub(crate) const fn is_initialized(&self) -> bool {
        self.initialized
    }

    #[inline]
    pub(crate) const fn fixed_capacity(&self) -> usize {
        self.fixed_capacity
    }

    /// Whether `index` owns an entry in the kernel's registered file table.
    #[inline]
    pub(crate) fn is_fixed(&self, index: u32) -> bool {
        (index as usize) < self.fixed_capacity
    }

    /// Seeds the userspace mirror once the kernel table exists.
    ///
    /// Callers must have registered a sparse table of `fixed_capacity` entries first (or have
    /// nothing to register, when the capacity is zero).
    pub(crate) fn mark_initialized(&mut self) {
        debug_assert!(!self.initialized, "file table initialized twice");
        self.slots = (0..self.fixed_capacity)
            .map(|_| FileSlot::vacant())
            .collect();
        self.free_fixed = (0..self.fixed_capacity as u32).rev().collect();
        self.initialized = true;
    }

    #[inline]
    pub(crate) fn entry(&self, index: u32) -> Option<&RegisteredFileEntry> {
        self.slots.get(index as usize)?.entry.as_ref()
    }

    #[inline]
    pub(crate) fn generation(&self, index: u32) -> Option<u64> {
        Some(self.slots.get(index as usize)?.generation)
    }

    /// Whether `fd` still names the registration it was handed out for.
    #[inline]
    pub(crate) fn matches_generation(&self, fd: IoFd) -> bool {
        self.generation(fd.fixed_index()) == Some(fd.generation())
    }

    /// Turns a user-facing descriptor into the form an SQE needs.
    pub(crate) fn resolve(
        &self,
        fd: IoFd,
        expected_kind: Option<RawHandleKind>,
        scope: &'static str,
    ) -> UringResult<SqeFd> {
        let index = fd.fixed_index();
        let Some(slot) = self.slots.get(index as usize) else {
            return UringError::ResolveFd
                .push_ctx("scope", scope)
                .with_ctx("fd_fixed_index", index)
                .with_ctx("fd_generation", fd.generation())
                .attach_note("registered file descriptor index out of bounds");
        };

        if slot.generation != fd.generation() {
            return UringError::ResolveFd
                .push_ctx("scope", scope)
                .with_ctx("fd_fixed_index", index)
                .with_ctx("fd_generation", fd.generation())
                .attach_note("stale registered file descriptor generation")
                .with_ctx("current_generation", slot.generation);
        }

        let Some(entry) = slot.entry.as_ref() else {
            return UringError::ResolveFd
                .push_ctx("scope", scope)
                .with_ctx("fd_fixed_index", index)
                .with_ctx("fd_generation", fd.generation())
                .attach_note("invalid registered file descriptor");
        };

        if let Some(expected_kind) = expected_kind {
            let current_kind = entry.kind();
            if current_kind != expected_kind {
                return UringError::ResolveFd
                    .push_ctx("scope", scope)
                    .with_ctx("fd_fixed_index", index)
                    .with_ctx("fd_generation", fd.generation())
                    .with_ctx("expected_kind", format!("{expected_kind:?}"))
                    .with_ctx("current_kind", format!("{current_kind:?}"))
                    .attach_note("registered file descriptor kind mismatch");
            }
        }

        Ok(if self.is_fixed(index) {
            SqeFd::Fixed(index)
        } else {
            SqeFd::Direct(entry.fd())
        })
    }

    /// Reserves `count` slots, falling back past the kernel table when it is full.
    ///
    /// Nothing is written to the slots themselves — the caller fills them in with
    /// [`Self::install_entry`] once it knows the kernel accepted the registration.
    pub(crate) fn claim(&mut self, count: usize) -> UringResult<ClaimedSlots> {
        let from_kernel_table = count.min(self.free_fixed.len());
        let overflow = count - from_kernel_table;

        if overflow > 0 && !self.exhaustion.falls_back() {
            return UringError::InvalidState
                .push_ctx("scope", "driver.file_table.claim")
                .with_ctx("requested_files", count)
                .with_ctx("free_file_slots", from_kernel_table)
                .with_ctx("file_table_capacity", self.fixed_capacity)
                .attach_note("io_uring registered file table exhausted");
        }

        let mut fixed = Vec::with_capacity(from_kernel_table);
        for _ in 0..from_kernel_table {
            fixed.push(
                self.free_fixed
                    .pop()
                    .expect("claim takes at most free_fixed.len() entries"),
            );
        }
        // The free list is seeded in reverse, so a fresh table hands out consecutive indices;
        // sorting keeps that property visible to the batching in `register_files_internal`.
        fixed.sort_unstable();

        let mut direct = Vec::with_capacity(overflow);
        for _ in 0..overflow {
            match self.claim_fallback_slot() {
                Ok(index) => direct.push(index),
                Err(report) => {
                    // Undo the partial claim so a failure leaves the table untouched.
                    self.release_all(fixed.drain(..).chain(direct.drain(..)));
                    return Err(report);
                }
            }
        }

        if overflow > 0 {
            self.report_fallback(overflow);
        }
        Ok(ClaimedSlots { fixed, direct })
    }

    fn claim_fallback_slot(&mut self) -> UringResult<u32> {
        if let Some(index) = self.free_direct.pop() {
            return Ok(index);
        }
        let index = u32::try_from(self.slots.len()).map_err(|_| {
            UringError::InvalidState
                .to_report()
                .push_ctx("scope", "driver.file_table.claim_fallback_slot")
                .with_ctx("file_slots", self.slots.len())
                .attach_note("registered file slot index exceeds the IoFd range")
        })?;
        self.slots.push(FileSlot::vacant());
        Ok(index)
    }

    fn report_fallback(&mut self, count: usize) {
        if self.fallback_reported {
            return;
        }
        self.fallback_reported = true;
        warn!(
            file_table_capacity = self.fixed_capacity,
            fallback_files = count,
            "io_uring registered file table is full; further descriptors submit as raw fds"
        );
    }

    /// Stores the handle backing `index`. The slot must have been claimed first.
    #[inline]
    pub(crate) fn install_entry(&mut self, index: u32, entry: RegisteredFileEntry) {
        self.slots[index as usize].entry = Some(entry);
    }

    #[inline]
    pub(crate) fn take_entry(&mut self, index: u32) -> Option<RegisteredFileEntry> {
        self.slots.get_mut(index as usize)?.entry.take()
    }

    /// Returns `index` to its free list. The entry must already be gone.
    pub(crate) fn release(&mut self, index: u32) {
        debug_assert!(
            self.entry(index).is_none(),
            "released a file slot that still owns a handle"
        );
        if self.is_fixed(index) {
            self.free_fixed.push(index);
        } else {
            self.free_direct.push(index);
        }
    }

    pub(crate) fn release_all(&mut self, indices: impl IntoIterator<Item = u32>) {
        for index in indices {
            self.release(index);
        }
    }

    /// Invalidates every [`IoFd`] previously handed out for `index`.
    pub(crate) fn advance_generation(&mut self, index: u32) {
        let Some(slot) = self.slots.get_mut(index as usize) else {
            return;
        };
        slot.generation = slot.generation.wrapping_add(1);
        if slot.generation == 0 {
            slot.generation = INITIAL_FILE_GENERATION;
        }
    }

    /// The descriptor for `index`, valid until the slot is released.
    #[inline]
    pub(crate) fn descriptor(&self, index: u32) -> Option<IoFd> {
        Some(IoFd::fixed_with_generation(index, self.generation(index)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{FileTable, RegisteredFileEntry, SqeFd};
    use crate::config::{FileTableExhaustion, IoFd, RawHandleKind};

    fn borrowed(fd: i32) -> RegisteredFileEntry {
        RegisteredFileEntry::BorrowedFd {
            fd,
            kind: RawHandleKind::File,
        }
    }

    fn table(capacity: u32, exhaustion: FileTableExhaustion) -> FileTable {
        let mut table = FileTable::new(capacity, exhaustion);
        table.mark_initialized();
        table
    }

    /// Claims `count` slots and installs one entry per slot, returning the descriptors.
    fn register(table: &mut FileTable, fds: &[i32]) -> Vec<IoFd> {
        let claimed = table.claim(fds.len()).expect("claim failed");
        let indices = claimed.in_input_order().collect::<Vec<_>>();
        for (index, fd) in indices.iter().copied().zip(fds.iter().copied()) {
            table.install_entry(index, borrowed(fd));
        }
        indices
            .into_iter()
            .map(|index| table.descriptor(index).expect("claimed slot exists"))
            .collect()
    }

    #[test]
    fn slots_within_the_capacity_resolve_to_fixed_indices() {
        let mut table = table(4, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[10, 11]);

        assert_eq!(
            table.resolve(fds[0], None, "test").unwrap(),
            SqeFd::Fixed(0)
        );
        assert_eq!(
            table.resolve(fds[1], None, "test").unwrap(),
            SqeFd::Fixed(1)
        );
    }

    #[test]
    fn slots_past_the_capacity_resolve_to_raw_fds() {
        let mut table = table(2, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[10, 11, 12, 13]);

        assert_eq!(
            table.resolve(fds[1], None, "test").unwrap(),
            SqeFd::Fixed(1)
        );
        assert_eq!(
            table.resolve(fds[2], None, "test").unwrap(),
            SqeFd::Direct(12)
        );
        assert_eq!(
            table.resolve(fds[3], None, "test").unwrap(),
            SqeFd::Direct(13)
        );
    }

    #[test]
    fn a_zero_capacity_table_submits_everything_as_raw_fds() {
        let mut table = table(0, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[7]);

        assert_eq!(
            table.resolve(fds[0], None, "test").unwrap(),
            SqeFd::Direct(7)
        );
    }

    #[test]
    fn overflow_is_rejected_when_fallback_is_disabled() {
        let mut table = table(2, FileTableExhaustion::Fail);
        let _ = register(&mut table, &[10, 11]);

        assert!(table.claim(1).is_err());
        // The rejected claim must not have consumed anything.
        assert!(table.claim(0).is_ok());
    }

    #[test]
    fn a_partially_claimed_batch_is_fully_returned_on_failure() {
        let mut table = table(1, FileTableExhaustion::Fail);
        assert!(table.claim(2).is_err());

        let fds = register(&mut table, &[10]);
        assert_eq!(
            table.resolve(fds[0], None, "test").unwrap(),
            SqeFd::Fixed(0)
        );
    }

    #[test]
    fn released_fallback_slots_are_reused_before_new_ones() {
        let mut table = table(1, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[10, 11, 12]);
        let reused = fds[2].fixed_index();

        table.take_entry(reused);
        table.release(reused);
        table.advance_generation(reused);

        let again = register(&mut table, &[13]);
        assert_eq!(again[0].fixed_index(), reused);
        assert_ne!(again[0].generation(), fds[2].generation());
    }

    #[test]
    fn a_released_slot_rejects_its_stale_descriptor() {
        let mut table = table(2, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[10]);
        let index = fds[0].fixed_index();

        table.take_entry(index);
        table.release(index);
        table.advance_generation(index);

        assert!(table.resolve(fds[0], None, "test").is_err());
        assert!(!table.matches_generation(fds[0]));
    }

    #[test]
    fn resolve_rejects_a_kind_mismatch() {
        let mut table = table(2, FileTableExhaustion::Fallback);
        let fds = register(&mut table, &[10]);

        assert!(
            table
                .resolve(fds[0], Some(RawHandleKind::Socket), "test")
                .is_err()
        );
        assert!(
            table
                .resolve(fds[0], Some(RawHandleKind::File), "test")
                .is_ok()
        );
    }
}
