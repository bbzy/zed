use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task};
use imara_diff::{Algorithm, Diff, InternedInput, sources::lines};
use language::{
    Capability, DiffOptions, Language, LanguageName, LanguageRegistry,
    language_settings::LanguageSettings, word_diff_ranges,
};
use rope::Rope;
use std::{
    cmp::Ordering,
    iter,
    ops::{Range, RangeInclusive},
    sync::Arc,
};
use sum_tree::SumTree;
use text::{
    Anchor, Bias, BufferId, Edit, OffsetRangeExt, Patch, Point, ToOffset as _, ToPoint as _,
};
use util::{ResultExt, debug_panic};

pub const MAX_WORD_DIFF_LINE_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Conflicted,
}

impl FileStatus {
    pub fn is_created(self) -> bool {
        self == Self::Added
    }

    pub fn is_modified(self) -> bool {
        self == Self::Modified
    }

    pub fn is_deleted(self) -> bool {
        self == Self::Deleted
    }

    pub fn is_conflicted(self) -> bool {
        self == Self::Conflicted
    }
}

pub struct BufferDiff {
    pub buffer_id: BufferId,
    base_text_buffer: Entity<language::Buffer>,
    diff_snapshot: Option<BufferDiffSnapshot>,
    secondary_diff: Option<Entity<BufferDiff>>,
    buffer_snapshot: text::BufferSnapshot,
    operations: Option<Arc<dyn DiffOperations>>,
    ignore_trim_whitespace: bool,
    whitespace_task: Option<Task<()>>,
}

pub trait DiffOperations {
    fn supports_restore(&self) -> bool;
}

pub struct RestoreDiffOperations;

impl DiffOperations for RestoreDiffOperations {
    fn supports_restore(&self) -> bool {
        true
    }
}

#[cfg(any(test, feature = "test-support"))]
struct TestDiffOperations;

#[cfg(any(test, feature = "test-support"))]
impl DiffOperations for TestDiffOperations {
    fn supports_restore(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct BufferDiffSnapshot {
    hunks: SumTree<InternalDiffHunk>,
    pending_hunks: SumTree<PendingHunk>,
    base_text: language::BufferSnapshot,
    base_text_exists: bool,
    ignore_trim_whitespace: bool,
    buffer_snapshot: text::BufferSnapshot,
    secondary_diff: Option<Arc<BufferDiffSnapshot>>,
}

impl std::fmt::Debug for BufferDiffSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferDiffSnapshot")
            .field("hunks", &self.hunks)
            .field("remote_id", &self.base_text.remote_id())
            .field("secondary_diff", &self.secondary_diff)
            .finish()
    }
}

#[derive(Clone)]
pub struct BufferDiffUpdate {
    hunks: SumTree<InternalDiffHunk>,
    base_text: language::BufferSnapshot,
    base_text_exists: bool,
    ignore_trim_whitespace: bool,
    buffer_snapshot: text::BufferSnapshot,
}

impl BufferDiffUpdate {
    pub fn set_base_text_snapshot(
        &mut self,
        base_text: language::BufferSnapshot,
        base_text_exists: bool,
    ) {
        self.base_text = base_text;
        self.base_text_exists = base_text_exists;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiffHunkStatus {
    pub kind: DiffHunkStatusKind,
    pub secondary: DiffHunkSecondaryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffHunkStatusKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// The relationship between a hunk and the secondary comparison.
pub enum DiffHunkSecondaryStatus {
    HasSecondaryHunk,
    OverlapsWithSecondaryHunk,
    NoSecondaryHunk,
    SecondaryHunkAdditionPending,
    SecondaryHunkRemovalPending,
}

/// A diff hunk resolved to rows in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The buffer range as points.
    pub range: Range<Point>,
    /// The range in the buffer to which this hunk corresponds.
    pub buffer_range: Range<Anchor>,
    /// The range in the buffer's diff base text to which this hunk corresponds.
    pub diff_base_byte_range: Range<usize>,
    pub secondary_status: DiffHunkSecondaryStatus,
    // Anchors representing the word diff locations in the active buffer
    pub buffer_word_diffs: Vec<Range<Anchor>>,
    // Offsets relative to the start of the deleted diff that represent word diff locations
    pub base_word_diffs: Vec<Range<usize>>,
}

/// We store [`InternalDiffHunk`]s internally so we don't need to store the additional row range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalDiffHunk {
    buffer_range: Range<Anchor>,
    diff_base_byte_range: Range<usize>,
    diff_base_point_range: Range<Point>,
    base_word_diffs: Vec<Range<usize>>,
    buffer_word_diffs: Vec<Range<Anchor>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHunk {
    buffer_range: Range<Anchor>,
    diff_base_byte_range: Range<usize>,
    buffer_version: clock::Global,
    sense: PendingSense,
}

impl PendingHunk {
    pub fn new(
        buffer_range: Range<Anchor>,
        diff_base_byte_range: Range<usize>,
        buffer_version: clock::Global,
        sense: PendingSense,
    ) -> Self {
        Self {
            buffer_range,
            diff_base_byte_range,
            buffer_version,
            sense,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSense {
    SetSecondaryStatus(DiffHunkSecondaryStatus),
    Suppress,
}

#[derive(Debug, Clone)]
pub struct DiffHunkSummary {
    buffer_range: Range<Anchor>,
    diff_base_byte_range: Range<usize>,
    added_rows: u32,
    removed_rows: u32,
}

impl sum_tree::Item for InternalDiffHunk {
    type Summary = DiffHunkSummary;

    fn summary(&self, buffer: &text::BufferSnapshot) -> Self::Summary {
        let buffer_start = self.buffer_range.start.to_point(buffer);
        let buffer_end = self.buffer_range.end.to_point(buffer);
        DiffHunkSummary {
            buffer_range: self.buffer_range.clone(),
            diff_base_byte_range: self.diff_base_byte_range.clone(),
            added_rows: buffer_end.row.saturating_sub(buffer_start.row),
            removed_rows: self
                .diff_base_point_range
                .end
                .row
                .saturating_sub(self.diff_base_point_range.start.row),
        }
    }
}

impl sum_tree::Item for PendingHunk {
    type Summary = DiffHunkSummary;

    fn summary(&self, _cx: &text::BufferSnapshot) -> Self::Summary {
        DiffHunkSummary {
            buffer_range: self.buffer_range.clone(),
            diff_base_byte_range: self.diff_base_byte_range.clone(),
            added_rows: 0,
            removed_rows: 0,
        }
    }
}

impl sum_tree::Summary for DiffHunkSummary {
    type Context<'a> = &'a text::BufferSnapshot;

    fn zero(buffer: &text::BufferSnapshot) -> Self {
        DiffHunkSummary {
            buffer_range: Anchor::min_min_range_for_buffer(buffer.remote_id()),
            diff_base_byte_range: 0..0,
            added_rows: 0,
            removed_rows: 0,
        }
    }

    fn add_summary(&mut self, other: &Self, buffer: Self::Context<'_>) {
        self.buffer_range.start = *self
            .buffer_range
            .start
            .min(&other.buffer_range.start, buffer);
        self.buffer_range.end = *self.buffer_range.end.max(&other.buffer_range.end, buffer);

        self.diff_base_byte_range.start = self
            .diff_base_byte_range
            .start
            .min(other.diff_base_byte_range.start);
        self.diff_base_byte_range.end = self
            .diff_base_byte_range
            .end
            .max(other.diff_base_byte_range.end);

        self.added_rows += other.added_rows;
        self.removed_rows += other.removed_rows;
    }
}

impl sum_tree::SeekTarget<'_, DiffHunkSummary, DiffHunkSummary> for Anchor {
    fn cmp(&self, cursor_location: &DiffHunkSummary, buffer: &text::BufferSnapshot) -> Ordering {
        if self
            .cmp(&cursor_location.buffer_range.start, buffer)
            .is_lt()
        {
            Ordering::Less
        } else if self.cmp(&cursor_location.buffer_range.end, buffer).is_gt() {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

impl sum_tree::SeekTarget<'_, DiffHunkSummary, DiffHunkSummary> for usize {
    fn cmp(&self, cursor_location: &DiffHunkSummary, _cx: &text::BufferSnapshot) -> Ordering {
        if *self < cursor_location.diff_base_byte_range.start {
            Ordering::Less
        } else if *self > cursor_location.diff_base_byte_range.end {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

impl BufferDiffSnapshot {
    #[cfg(test)]
    fn new_sync(
        buffer: &text::BufferSnapshot,
        diff_base: String,
        cx: &mut gpui::TestAppContext,
    ) -> BufferDiffSnapshot {
        let buffer_diff = cx.new(|cx| BufferDiff::new_with_base_text(&diff_base, buffer, cx));
        buffer_diff.update(cx, |buffer_diff, cx| buffer_diff.snapshot(cx))
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_snapshot.remote_id()
    }

    pub fn buffer_snapshot(&self) -> &text::BufferSnapshot {
        &self.buffer_snapshot
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn changed_row_counts(&self) -> (u32, u32) {
        let summary = self.hunks.summary();
        (summary.added_rows, summary.removed_rows)
    }

    pub fn base_text_string(&self) -> Option<String> {
        self.base_text_exists.then(|| self.base_text.text())
    }

    pub fn base_text_exists(&self) -> bool {
        self.base_text_exists
    }

    pub fn secondary_diff(&self) -> Option<&BufferDiffSnapshot> {
        self.secondary_diff.as_deref()
    }

    pub fn buffer_version(&self) -> &clock::Global {
        self.buffer_snapshot.version()
    }

    fn original_buffer_snapshot(&self) -> &text::BufferSnapshot {
        &self.buffer_snapshot
    }

    #[ztracing::instrument(skip_all)]
    pub fn hunks_intersecting_range<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let secondary_diff = self.secondary_diff.as_deref();
        let range = range.to_offset(buffer);
        let filter = move |summary: &DiffHunkSummary| {
            let summary_range = summary.buffer_range.to_offset(buffer);
            let before_start = summary_range.end < range.start;
            let after_end = summary_range.start > range.end;
            !before_start && !after_end
        };
        self.hunks_intersecting_range_impl(filter, buffer, secondary_diff)
    }

    /// Like [`hunks_intersecting_range`], but ignores optimistic pending hunks
    /// (both secondary-status overrides and suppressions) and does not compute a
    /// secondary status.
    pub fn raw_hunks_intersecting_range<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let range = range.to_offset(buffer);
        let filter = move |summary: &DiffHunkSummary| {
            let summary_range = summary.buffer_range.to_offset(buffer);
            !(summary_range.end < range.start) && !(summary_range.start > range.end)
        };
        self.hunks
            .filter::<_, DiffHunkSummary>(buffer, filter)
            .map(move |hunk| {
                let buffer_range = hunk.buffer_range.clone();
                DiffHunk {
                    range: buffer_range.to_point(buffer),
                    diff_base_byte_range: hunk.diff_base_byte_range.clone(),
                    buffer_range,
                    secondary_status: DiffHunkSecondaryStatus::NoSecondaryHunk,
                    base_word_diffs: hunk.base_word_diffs.clone(),
                    buffer_word_diffs: hunk.buffer_word_diffs.clone(),
                }
            })
    }

    /// Maps a range in this diff's main buffer to the range it covers in the
    /// base text, expanding to whole hunks wherever the range endpoints fall
    /// inside or touch a hunk (`edit_for_old_position` is inclusive on both
    /// boundaries, matching `raw_hunks_intersecting_range`). Used by the
    /// index-write path to compute the index-coordinate footprint of a staging
    /// operation; like the raw hunks, the mapping ignores optimistic pending
    /// hunks.
    pub fn base_text_range_for_buffer_range(
        &self,
        range: Range<Anchor>,
        buffer: &text::BufferSnapshot,
    ) -> Range<usize> {
        let point_range = range.to_point(buffer);
        let patch = self.patch_for_buffer_range(point_range.start..=point_range.end, buffer);
        let start_point = patch.edit_for_old_position(point_range.start).new.start;
        let end_point = patch.edit_for_old_position(point_range.end).new.end;
        let base_text = self.base_text();
        let start = base_text.point_to_offset(start_point.min(base_text.max_point()));
        let end = base_text.point_to_offset(end_point.min(base_text.max_point()));
        start.min(end)..end
    }

    pub fn hunks_intersecting_range_rev<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let filter = move |summary: &DiffHunkSummary| {
            let before_start = summary.buffer_range.end.cmp(&range.start, buffer).is_lt();
            let after_end = summary.buffer_range.start.cmp(&range.end, buffer).is_gt();
            !before_start && !after_end
        };
        self.hunks_intersecting_range_rev_impl(filter, buffer)
    }

    pub fn hunks_intersecting_base_text_range<'a>(
        &'a self,
        range: Range<usize>,
        main_buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let secondary_diff = self.secondary_diff.as_deref();
        let filter = move |summary: &DiffHunkSummary| {
            let before_start = summary.diff_base_byte_range.end < range.start;
            let after_end = summary.diff_base_byte_range.start > range.end;
            !before_start && !after_end
        };
        self.hunks_intersecting_range_impl(filter, main_buffer, secondary_diff)
    }

    pub fn hunks_intersecting_base_text_range_rev<'a>(
        &'a self,
        range: Range<usize>,
        main_buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let filter = move |summary: &DiffHunkSummary| {
            let before_start = summary.diff_base_byte_range.end.cmp(&range.start).is_lt();
            let after_end = summary.diff_base_byte_range.start.cmp(&range.end).is_gt();
            !before_start && !after_end
        };
        self.hunks_intersecting_range_rev_impl(filter, main_buffer)
    }

    pub fn hunks<'a>(
        &'a self,
        buffer_snapshot: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        self.hunks_intersecting_range(
            Anchor::min_max_range_for_buffer(buffer_snapshot.remote_id()),
            buffer_snapshot,
        )
    }

    pub fn hunks_in_row_range<'a>(
        &'a self,
        range: Range<u32>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let start = buffer.anchor_before(Point::new(range.start, 0));
        let end = buffer.anchor_after(Point::new(range.end, 0));
        self.hunks_intersecting_range(start..end, buffer)
    }

    pub fn range_to_hunk_range(
        &self,
        range: Range<Anchor>,
        buffer: &text::BufferSnapshot,
    ) -> (Option<Range<Anchor>>, Option<Range<usize>>) {
        let first_hunk = self.hunks_intersecting_range(range.clone(), buffer).next();
        let last_hunk = self.hunks_intersecting_range_rev(range, buffer).next();
        let range = first_hunk
            .as_ref()
            .zip(last_hunk.as_ref())
            .map(|(first, last)| first.buffer_range.start..last.buffer_range.end);
        let base_text_range = first_hunk
            .zip(last_hunk)
            .map(|(first, last)| first.diff_base_byte_range.start..last.diff_base_byte_range.end);
        (range, base_text_range)
    }

    pub fn base_text(&self) -> &language::BufferSnapshot {
        &self.base_text
    }

    /// If this function returns `true`, the base texts are equal. If this
    /// function returns `false`, they might be equal, but might not. This
    /// result is used to avoid recalculating diffs in situations where we know
    /// nothing has changed.
    pub fn base_texts_definitely_eq(&self, other: &Self) -> bool {
        if self.base_text_exists != other.base_text_exists {
            return false;
        }
        let left = &self.base_text;
        let right = &other.base_text;
        let (old_id, old_version, old_empty) = (left.remote_id(), left.version(), left.is_empty());
        let (new_id, new_version, new_empty) =
            (right.remote_id(), right.version(), right.is_empty());
        (new_id == old_id && new_version == old_version) || (new_empty && old_empty)
    }

    /// Returns the last hunk whose start is less than or equal to the given position.
    fn hunk_before_base_text_offset<'a>(
        &self,
        target: usize,
        cursor: &mut sum_tree::Cursor<'a, '_, InternalDiffHunk, DiffHunkSummary>,
    ) -> Option<&'a InternalDiffHunk> {
        cursor.seek_forward(&target, Bias::Left);
        if cursor
            .item()
            .is_none_or(|hunk| target < hunk.diff_base_byte_range.start)
        {
            cursor.prev();
        }
        cursor
            .item()
            .filter(|hunk| target >= hunk.diff_base_byte_range.start)
    }

    fn hunk_before_buffer_anchor<'a>(
        &self,
        target: Anchor,
        cursor: &mut sum_tree::Cursor<'a, '_, InternalDiffHunk, DiffHunkSummary>,
        buffer: &text::BufferSnapshot,
    ) -> Option<&'a InternalDiffHunk> {
        cursor.seek_forward(&target, Bias::Left);
        if cursor
            .item()
            .is_none_or(|hunk| target.cmp(&hunk.buffer_range.start, buffer).is_lt())
        {
            cursor.prev();
        }
        cursor
            .item()
            .filter(|hunk| target.cmp(&hunk.buffer_range.start, buffer).is_ge())
    }

    /// Returns a patch mapping the provided main buffer snapshot to the base text of this diff.
    ///
    /// The returned patch is guaranteed to be accurate for all main buffer points in the provided range,
    /// but not necessarily for points outside that range.
    pub fn patch_for_buffer_range<'a>(
        &'a self,
        range: RangeInclusive<Point>,
        buffer: &'a text::BufferSnapshot,
    ) -> Patch<Point> {
        if !self.base_text_exists {
            return Patch::new(vec![Edit {
                old: Point::zero()..buffer.max_point(),
                new: Point::zero()..Point::zero(),
            }]);
        }

        let mut edits_since_diff = Patch::new(
            buffer
                .edits_since::<Point>(&self.buffer_snapshot.version)
                .collect::<Vec<_>>(),
        );
        edits_since_diff.invert();

        let mut start_point = edits_since_diff.old_to_new(*range.start());
        if let Some(first_edit) = edits_since_diff.edits().first() {
            start_point = start_point.min(first_edit.new.start);
        }

        let original_snapshot = self.original_buffer_snapshot();
        let base_text = self.base_text();

        let mut cursor = self.hunks.cursor(original_snapshot);
        self.hunk_before_buffer_anchor(
            original_snapshot.anchor_before(start_point),
            &mut cursor,
            original_snapshot,
        );
        if cursor.item().is_none() {
            cursor.next();
        }

        let mut prefix_edit = cursor.prev_item().map(|prev_hunk| Edit {
            old: Point::zero()..prev_hunk.buffer_range.end.to_point(original_snapshot),
            new: Point::zero()..prev_hunk.diff_base_byte_range.end.to_point(base_text),
        });

        let mut range_end = edits_since_diff.old_to_new(*range.end());
        if let Some(last_edit) = edits_since_diff.edits().last() {
            range_end = range_end.max(last_edit.new.end);
        }
        let range_end = original_snapshot.anchor_before(range_end);

        let hunk_iter = std::iter::from_fn(move || {
            if let Some(edit) = prefix_edit.take() {
                return Some(edit);
            }
            let hunk = cursor.item()?;
            if hunk
                .buffer_range
                .start
                .cmp(&range_end, original_snapshot)
                .is_gt()
            {
                return None;
            }
            let edit = Edit {
                old: hunk.buffer_range.to_point(original_snapshot),
                new: hunk.diff_base_byte_range.to_point(base_text),
            };
            cursor.next();
            Some(edit)
        });

        edits_since_diff.compose(hunk_iter)
    }

    #[cfg(test)]
    pub(crate) fn patch_for_buffer_range_naive<'a>(
        &'a self,
        buffer: &'a text::BufferSnapshot,
    ) -> Patch<Point> {
        let original_snapshot = self.original_buffer_snapshot();

        let edits_since: Vec<Edit<Point>> = buffer
            .edits_since::<Point>(original_snapshot.version())
            .collect();
        let mut inverted_edits_since = Patch::new(edits_since);
        inverted_edits_since.invert();

        inverted_edits_since.compose(
            self.hunks
                .iter()
                .map(|hunk| {
                    let old_start = hunk.buffer_range.start.to_point(original_snapshot);
                    let old_end = hunk.buffer_range.end.to_point(original_snapshot);
                    let new_start = self
                        .base_text()
                        .offset_to_point(hunk.diff_base_byte_range.start);
                    let new_end = self
                        .base_text()
                        .offset_to_point(hunk.diff_base_byte_range.end);
                    Edit {
                        old: old_start..old_end,
                        new: new_start..new_end,
                    }
                })
                .chain(if !self.base_text_exists && self.hunks.is_empty() {
                    Some(Edit {
                        old: Point::zero()..original_snapshot.max_point(),
                        new: Point::zero()..Point::zero(),
                    })
                } else {
                    None
                }),
        )
    }

    /// Returns a patch mapping the base text of this diff to the provided main buffer snapshot.
    ///
    /// The returned patch is guaranteed to be accurate for all base text points in the provided range,
    /// but not necessarily for points outside that range.
    pub fn patch_for_base_text_range<'a>(
        &'a self,
        range: RangeInclusive<Point>,
        buffer: &'a text::BufferSnapshot,
    ) -> Patch<Point> {
        if !self.base_text_exists {
            return Patch::new(vec![Edit {
                old: Point::zero()..Point::zero(),
                new: Point::zero()..buffer.max_point(),
            }]);
        }

        let edits_since_diff = buffer
            .edits_since::<Point>(&self.buffer_snapshot.version)
            .collect::<Vec<_>>();

        let mut hunk_patch = Vec::new();
        let mut cursor = self.hunks.cursor(self.original_buffer_snapshot());
        let hunk_before = self
            .hunk_before_base_text_offset(range.start().to_offset(self.base_text()), &mut cursor);

        if let Some(hunk) = hunk_before
            && let Some(first_edit) = edits_since_diff.first()
            && hunk
                .buffer_range
                .start
                .to_point(self.original_buffer_snapshot())
                > first_edit.old.start
        {
            cursor.reset();
            self.hunk_before_buffer_anchor(
                self.original_buffer_snapshot()
                    .anchor_before(first_edit.old.start),
                &mut cursor,
                self.original_buffer_snapshot(),
            );
        }
        if cursor.item().is_none() {
            cursor.next();
        }
        if let Some(prev_hunk) = cursor.prev_item() {
            hunk_patch.push(Edit {
                old: Point::zero()
                    ..prev_hunk
                        .diff_base_byte_range
                        .end
                        .to_point(self.base_text()),
                new: Point::zero()
                    ..prev_hunk
                        .buffer_range
                        .end
                        .to_point(self.original_buffer_snapshot()),
            })
        }
        let range_end = range.end().to_offset(self.base_text());
        while let Some(hunk) = cursor.item()
            && (hunk.diff_base_byte_range.start <= range_end
                || edits_since_diff.last().is_some_and(|last_edit| {
                    hunk.buffer_range
                        .start
                        .to_point(self.original_buffer_snapshot())
                        <= last_edit.old.end
                }))
        {
            hunk_patch.push(Edit {
                old: hunk.diff_base_byte_range.to_point(self.base_text()),
                new: hunk.buffer_range.to_point(self.original_buffer_snapshot()),
            });
            cursor.next();
        }

        Patch::new(hunk_patch).compose(edits_since_diff)
    }

    #[cfg(test)]
    pub(crate) fn patch_for_base_text_range_naive<'a>(
        &'a self,
        buffer: &'a text::BufferSnapshot,
    ) -> Patch<Point> {
        let original_snapshot = self.original_buffer_snapshot();

        let mut hunk_edits: Vec<Edit<Point>> = Vec::new();
        for hunk in self.hunks.iter() {
            let old_start = self
                .base_text()
                .offset_to_point(hunk.diff_base_byte_range.start);
            let old_end = self
                .base_text()
                .offset_to_point(hunk.diff_base_byte_range.end);
            let new_start = hunk.buffer_range.start.to_point(original_snapshot);
            let new_end = hunk.buffer_range.end.to_point(original_snapshot);
            hunk_edits.push(Edit {
                old: old_start..old_end,
                new: new_start..new_end,
            });
        }
        if !self.base_text_exists && hunk_edits.is_empty() {
            hunk_edits.push(Edit {
                old: Point::zero()..Point::zero(),
                new: Point::zero()..original_snapshot.max_point(),
            })
        }
        let hunk_patch = Patch::new(hunk_edits);

        hunk_patch.compose(buffer.edits_since::<Point>(original_snapshot.version()))
    }

    pub fn buffer_point_to_base_text_range(
        &self,
        point: Point,
        buffer: &text::BufferSnapshot,
    ) -> Range<Point> {
        let patch = self.patch_for_buffer_range(point..=point, buffer);
        let edit = patch.edit_for_old_position(point);
        edit.new
    }

    pub fn base_text_point_to_buffer_range(
        &self,
        point: Point,
        buffer: &text::BufferSnapshot,
    ) -> Range<Point> {
        let patch = self.patch_for_base_text_range(point..=point, buffer);
        let edit = patch.edit_for_old_position(point);
        edit.new
    }

    pub fn buffer_point_to_base_text_point(
        &self,
        point: Point,
        buffer: &text::BufferSnapshot,
    ) -> Point {
        let patch = self.patch_for_buffer_range(point..=point, buffer);
        let edit = patch.edit_for_old_position(point);
        if point == edit.old.end {
            edit.new.end
        } else {
            edit.new.start
        }
    }

    pub fn base_text_point_to_buffer_point(
        &self,
        point: Point,
        buffer: &text::BufferSnapshot,
    ) -> Point {
        let patch = self.patch_for_base_text_range(point..=point, buffer);
        let edit = patch.edit_for_old_position(point);
        if point == edit.old.end {
            edit.new.end
        } else {
            edit.new.start
        }
    }
}

impl BufferDiffSnapshot {
    fn hunks_intersecting_range_impl<'a>(
        &'a self,
        filter: impl 'a + Fn(&DiffHunkSummary) -> bool,
        buffer: &'a text::BufferSnapshot,
        secondary: Option<&'a Self>,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let anchor_iter = self
            .hunks
            .filter::<_, DiffHunkSummary>(buffer, filter)
            .flat_map(move |hunk| {
                [
                    (
                        hunk.buffer_range.start,
                        (
                            hunk.buffer_range.start,
                            hunk.diff_base_byte_range.start,
                            hunk,
                        ),
                    ),
                    (
                        hunk.buffer_range.end,
                        (hunk.buffer_range.end, hunk.diff_base_byte_range.end, hunk),
                    ),
                ]
            });

        let mut pending_hunks_cursor = self.pending_hunks.cursor::<DiffHunkSummary>(buffer);
        pending_hunks_cursor.next();

        let mut secondary_cursor = None;
        if let Some(secondary) = secondary.as_ref() {
            let mut cursor = secondary.hunks.cursor::<DiffHunkSummary>(buffer);
            cursor.next();
            secondary_cursor = Some(cursor);
        }

        let max_point = buffer.max_point();
        let mut summaries = buffer.summaries_for_anchors_with_payload::<Point, _, _>(anchor_iter);
        iter::from_fn(move || {
            loop {
                let (start_point, (start_anchor, start_base, hunk)) = summaries.next()?;
                let (mut end_point, (mut end_anchor, end_base, _)) = summaries.next()?;

                let base_word_diffs = hunk.base_word_diffs.clone();
                let buffer_word_diffs = hunk.buffer_word_diffs.clone();

                if !start_anchor.is_valid(buffer) {
                    continue;
                }

                if end_point.column > 0 && end_point < max_point {
                    end_point.row += 1;
                    end_point.column = 0;
                    end_anchor = buffer.anchor_before(end_point);
                }

                let mut secondary_status = DiffHunkSecondaryStatus::NoSecondaryHunk;

                let mut has_pending = false;
                if start_anchor
                    .cmp(&pending_hunks_cursor.start().buffer_range.start, buffer)
                    .is_gt()
                {
                    pending_hunks_cursor.seek_forward(&start_anchor, Bias::Left);
                }

                if let Some(pending_hunk) = pending_hunks_cursor.item() {
                    let mut pending_range = pending_hunk.buffer_range.to_point(buffer);
                    if pending_range.end.column > 0 {
                        pending_range.end.row += 1;
                        pending_range.end.column = 0;
                    }

                    if pending_range == (start_point..end_point)
                        && !buffer.has_edits_since_in_range(
                            &pending_hunk.buffer_version,
                            start_anchor..end_anchor,
                        )
                    {
                        match pending_hunk.sense {
                            PendingSense::SetSecondaryStatus(status) => {
                                has_pending = true;
                                secondary_status = status;
                            }
                            PendingSense::Suppress => continue,
                        }
                    }
                }

                if let (Some(secondary_cursor), false) = (secondary_cursor.as_mut(), has_pending) {
                    if start_anchor
                        .cmp(&secondary_cursor.start().buffer_range.start, buffer)
                        .is_gt()
                    {
                        secondary_cursor.seek_forward(&start_anchor, Bias::Left);
                    }

                    if let Some(secondary_hunk) = secondary_cursor.item() {
                        let mut secondary_range = secondary_hunk.buffer_range.to_point(buffer);
                        if secondary_range.end.column > 0 {
                            secondary_range.end.row += 1;
                            secondary_range.end.column = 0;
                        }
                        if secondary_range.is_empty()
                            && secondary_hunk.diff_base_byte_range.is_empty()
                        {
                            // ignore
                        } else if secondary_range == (start_point..end_point) {
                            secondary_status = DiffHunkSecondaryStatus::HasSecondaryHunk;
                        } else if secondary_range.start <= end_point {
                            secondary_status = DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk;
                        }
                    }
                }

                return Some(DiffHunk {
                    range: start_point..end_point,
                    diff_base_byte_range: start_base..end_base,
                    buffer_range: start_anchor..end_anchor,
                    base_word_diffs,
                    buffer_word_diffs,
                    secondary_status,
                });
            }
        })
    }

    fn hunks_intersecting_range_rev_impl<'a>(
        &'a self,
        filter: impl 'a + Fn(&DiffHunkSummary) -> bool,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let mut cursor = self.hunks.filter::<_, DiffHunkSummary>(buffer, filter);

        iter::from_fn(move || {
            cursor.prev();

            let hunk = cursor.item()?;
            let range = hunk.buffer_range.to_point(buffer);

            Some(DiffHunk {
                range,
                diff_base_byte_range: hunk.diff_base_byte_range.clone(),
                buffer_range: hunk.buffer_range.clone(),
                // The secondary status is not used by callers of this method.
                secondary_status: DiffHunkSecondaryStatus::NoSecondaryHunk,
                base_word_diffs: hunk.base_word_diffs.clone(),
                buffer_word_diffs: hunk.buffer_word_diffs.clone(),
            })
        })
    }
}

fn build_diff_options(
    language: Option<LanguageName>,
    language_scope: Option<language::LanguageScope>,
    cx: &App,
) -> Option<DiffOptions> {
    #[cfg(any(test, feature = "test-support"))]
    {
        if !cx.has_global::<settings::SettingsStore>() {
            return Some(DiffOptions {
                language_scope,
                max_word_diff_line_count: MAX_WORD_DIFF_LINE_COUNT,
                ..Default::default()
            });
        }
    }

    LanguageSettings::resolve(None, language.as_ref(), cx)
        .word_diff_enabled
        .then_some(DiffOptions {
            language_scope,
            max_word_diff_line_count: MAX_WORD_DIFF_LINE_COUNT,
            ..Default::default()
        })
}

fn trim_line(line: &str) -> &str {
    line.strip_suffix('\n')
        .unwrap_or(line)
        .trim_matches([' ', '\t'])
}

fn comparison_line(line: &str, ignore_trim_whitespace: bool) -> std::borrow::Cow<'_, [u8]> {
    if ignore_trim_whitespace {
        let mut content = trim_line(line).to_owned();
        if line.ends_with('\n') {
            content.push('\n');
        }
        std::borrow::Cow::Owned(content.into_bytes())
    } else {
        std::borrow::Cow::Borrowed(line.as_bytes())
    }
}

fn compute_hunks(
    diff_base: Option<(Arc<str>, Rope)>,
    buffer: &text::BufferSnapshot,
    diff_options: Option<DiffOptions>,
    ignore_trim_whitespace: bool,
) -> SumTree<InternalDiffHunk> {
    let mut tree = SumTree::new(buffer);

    if let Some((diff_base, diff_base_rope)) = diff_base {
        let buffer_text = buffer.as_rope().to_string();

        // A common case in Zed is that the empty buffer is represented as just a newline,
        // but if we just compute a naive diff you get a "preserved" line in the middle,
        // which is a bit odd.
        if !ignore_trim_whitespace
            && buffer_text == "\n"
            && diff_base.ends_with("\n")
            && diff_base.len() > 1
        {
            tree.push(
                InternalDiffHunk {
                    buffer_range: buffer.anchor_before(0)..buffer.anchor_before(0),
                    diff_base_byte_range: 0..diff_base.len() - 1,
                    diff_base_point_range: Point::new(0, 0)
                        ..diff_base_rope.offset_to_point(diff_base.len() - 1),
                    base_word_diffs: Vec::default(),
                    buffer_word_diffs: Vec::default(),
                },
                buffer,
            );
            return tree;
        }

        let comparison_line = |line| comparison_line(line, ignore_trim_whitespace);
        let mut input = InternedInput::default();
        input.update_before(lines(diff_base.as_ref()).map(comparison_line));
        input.update_after(lines(buffer_text.as_str()).map(comparison_line));
        let mut diff = Diff::compute(Algorithm::Histogram, &input);
        // Align ambiguous hunks consistently so comparisons against different
        // base texts can correlate the same logical change.
        diff.postprocess_lines(&input);
        let mut sink = HunkSink::new(
            &diff_base,
            &diff_base_rope,
            buffer,
            diff_options.as_ref(),
            ignore_trim_whitespace,
        );
        for hunk in diff.hunks() {
            sink.process_change(hunk.before, hunk.after);
        }
        for hunk in sink.finish() {
            tree.push(hunk, buffer);
        }
    } else {
        tree.push(
            InternalDiffHunk {
                buffer_range: Anchor::min_max_range_for_buffer(buffer.remote_id()),
                diff_base_byte_range: 0..0,
                diff_base_point_range: Point::new(0, 0)..Point::new(0, 0),
                base_word_diffs: Vec::default(),
                buffer_word_diffs: Vec::default(),
            },
            buffer,
        );
    }

    tree
}
struct HunkSink<'a> {
    diff_base_rope: &'a Rope,
    buffer: &'a text::BufferSnapshot,
    diff_options: Option<&'a DiffOptions>,
    ignore_trim_whitespace: bool,
    old_line_offsets: Vec<usize>,
    hunks: Vec<InternalDiffHunk>,
}

impl<'a> HunkSink<'a> {
    fn new(
        diff_base: &'a str,
        diff_base_rope: &'a Rope,
        buffer: &'a text::BufferSnapshot,
        diff_options: Option<&'a DiffOptions>,
        ignore_trim_whitespace: bool,
    ) -> Self {
        let old_line_offsets = Self::compute_line_offsets(diff_base);
        Self {
            diff_base_rope,
            buffer,
            diff_options,
            old_line_offsets,
            ignore_trim_whitespace,
            hunks: Vec::new(),
        }
    }

    fn compute_line_offsets(text: &str) -> Vec<usize> {
        let mut offsets = vec![0];
        let mut offset = 0;
        for line in lines(text) {
            offset += line.len();
            offsets.push(offset);
        }
        offsets
    }
}

impl HunkSink<'_> {
    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        let old_start = before.start as usize;
        let old_end = before.end as usize;
        let new_start = after.start as usize;
        let new_end = after.end as usize;

        let diff_base_byte_range = self.old_line_offsets[old_start]..self.old_line_offsets[old_end];

        let buffer_row_range = (new_start as u32)..(new_end as u32);

        let start = Point::new(buffer_row_range.start, 0);
        let end = Point::new(buffer_row_range.end, 0);
        let buffer_range = self.buffer.anchor_before(start)..self.buffer.anchor_before(end);

        let base_line_count = old_end - old_start;
        let buffer_line_count = new_end - new_start;

        let (base_word_diffs, buffer_word_diffs) = if let Some(diff_options) = self.diff_options
            && !buffer_row_range.is_empty()
            && base_line_count == buffer_line_count
            && diff_options.max_word_diff_line_count >= base_line_count
        {
            let base_text: String = self
                .diff_base_rope
                .chunks_in_range(diff_base_byte_range.clone())
                .collect();
            let buffer_text: String = self.buffer.text_for_range(buffer_range.clone()).collect();

            let options = || DiffOptions {
                language_scope: diff_options.language_scope.clone(),
                ..*diff_options
            };
            let (base_word_diffs, buffer_word_diffs_relative) = if self.ignore_trim_whitespace {
                let mut base_ranges = Vec::new();
                let mut buffer_ranges = Vec::new();
                let mut base_offset = 0;
                let mut buffer_offset = 0;
                for (base_line, buffer_line) in lines(&base_text).zip(lines(&buffer_text)) {
                    let base_content = trim_line(base_line);
                    let buffer_content = trim_line(buffer_line);
                    let base_start = base_offset + base_line.len()
                        - base_line.trim_start_matches([' ', '\t']).len();
                    let buffer_start = buffer_offset + buffer_line.len()
                        - buffer_line.trim_start_matches([' ', '\t']).len();
                    let (base_changes, buffer_changes) =
                        word_diff_ranges(base_content, buffer_content, options());
                    base_ranges.extend(
                        base_changes
                            .into_iter()
                            .map(|range| base_start + range.start..base_start + range.end),
                    );
                    buffer_ranges.extend(
                        buffer_changes
                            .into_iter()
                            .map(|range| buffer_start + range.start..buffer_start + range.end),
                    );
                    base_offset += base_line.len();
                    buffer_offset += buffer_line.len();
                }
                (base_ranges, buffer_ranges)
            } else {
                word_diff_ranges(&base_text, &buffer_text, options())
            };

            let buffer_start_offset = buffer_range.start.to_offset(self.buffer);
            let buffer_word_diffs = buffer_word_diffs_relative
                .into_iter()
                .map(|range| {
                    let start = self.buffer.anchor_after(buffer_start_offset + range.start);
                    let end = self.buffer.anchor_after(buffer_start_offset + range.end);
                    start..end
                })
                .collect();

            (base_word_diffs, buffer_word_diffs)
        } else {
            (Vec::default(), Vec::default())
        };

        self.hunks.push(InternalDiffHunk {
            buffer_range,
            diff_base_byte_range: diff_base_byte_range.clone(),
            diff_base_point_range: self
                .diff_base_rope
                .offset_to_point(diff_base_byte_range.start)
                ..self
                    .diff_base_rope
                    .offset_to_point(diff_base_byte_range.end),
            base_word_diffs,
            buffer_word_diffs,
        });
    }

    fn finish(self) -> Vec<InternalDiffHunk> {
        self.hunks
    }
}

fn compare_hunks(
    new_hunks: &SumTree<InternalDiffHunk>,
    old_hunks: &SumTree<InternalDiffHunk>,
    old_snapshot: &text::BufferSnapshot,
    new_snapshot: &text::BufferSnapshot,
    old_base_text: &text::BufferSnapshot,
    new_base_text: &text::BufferSnapshot,
) -> DiffChanged {
    let mut new_cursor = new_hunks.cursor::<()>(new_snapshot);
    let mut old_cursor = old_hunks.cursor::<()>(new_snapshot);
    old_cursor.next();
    new_cursor.next();
    let mut start = None;
    let mut end = None;
    let mut base_text_start: Option<Anchor> = None;
    let mut base_text_end: Option<Anchor> = None;

    let mut last_unchanged_new_hunk_end: Option<text::Anchor> = None;
    let mut has_changes = false;
    let mut extended_end_candidate: Option<text::Anchor> = None;
    let base_text_changed = old_base_text.remote_id() != new_base_text.remote_id()
        || new_base_text
            .version()
            .changed_since(old_base_text.version());

    loop {
        match (new_cursor.item(), old_cursor.item()) {
            (Some(new_hunk), Some(old_hunk)) => {
                match new_hunk
                    .buffer_range
                    .start
                    .cmp(&old_hunk.buffer_range.start, new_snapshot)
                {
                    Ordering::Less => {
                        has_changes = true;
                        extended_end_candidate = None;
                        start.get_or_insert(new_hunk.buffer_range.start);
                        base_text_start.get_or_insert(
                            new_base_text.anchor_before(new_hunk.diff_base_byte_range.start),
                        );
                        end.replace(new_hunk.buffer_range.end);
                        let new_diff_range_end =
                            new_base_text.anchor_after(new_hunk.diff_base_byte_range.end);
                        if base_text_end.is_none_or(|base_text_end| {
                            new_diff_range_end
                                .cmp(&base_text_end, &new_base_text)
                                .is_gt()
                        }) {
                            base_text_end = Some(new_diff_range_end)
                        }
                        new_cursor.next();
                    }
                    Ordering::Equal => {
                        let base_text_differs = base_text_changed
                            && !old_base_text
                                .chars_for_range(old_hunk.diff_base_byte_range.clone())
                                .eq(new_base_text
                                    .chars_for_range(new_hunk.diff_base_byte_range.clone()));
                        if new_hunk != old_hunk || base_text_differs {
                            has_changes = true;
                            extended_end_candidate = None;
                            start.get_or_insert(new_hunk.buffer_range.start);
                            base_text_start.get_or_insert(
                                new_base_text.anchor_before(new_hunk.diff_base_byte_range.start),
                            );
                            if old_hunk
                                .buffer_range
                                .end
                                .cmp(&new_hunk.buffer_range.end, new_snapshot)
                                .is_ge()
                            {
                                end.replace(old_hunk.buffer_range.end);
                            } else {
                                end.replace(new_hunk.buffer_range.end);
                            }

                            let old_hunk_diff_base_range_end =
                                old_base_text.anchor_after(old_hunk.diff_base_byte_range.end);
                            let new_hunk_diff_base_range_end =
                                new_base_text.anchor_after(new_hunk.diff_base_byte_range.end);

                            base_text_end.replace(
                                *old_hunk_diff_base_range_end
                                    .max(&new_hunk_diff_base_range_end, new_base_text),
                            );
                        } else {
                            if !has_changes {
                                last_unchanged_new_hunk_end = Some(new_hunk.buffer_range.end);
                            } else if extended_end_candidate.is_none() {
                                extended_end_candidate = Some(new_hunk.buffer_range.start);
                            }
                        }

                        new_cursor.next();
                        old_cursor.next();
                    }
                    Ordering::Greater => {
                        has_changes = true;
                        extended_end_candidate = None;
                        start.get_or_insert(old_hunk.buffer_range.start);
                        base_text_start.get_or_insert(
                            old_base_text.anchor_after(old_hunk.diff_base_byte_range.start),
                        );
                        end.replace(old_hunk.buffer_range.end);
                        let old_diff_range_end =
                            old_base_text.anchor_after(old_hunk.diff_base_byte_range.end);
                        if base_text_end.is_none_or(|base_text_end| {
                            old_diff_range_end
                                .cmp(&base_text_end, new_base_text)
                                .is_gt()
                        }) {
                            base_text_end = Some(old_diff_range_end)
                        }
                        old_cursor.next();
                    }
                }
            }
            (Some(new_hunk), None) => {
                has_changes = true;
                extended_end_candidate = None;
                start.get_or_insert(new_hunk.buffer_range.start);
                base_text_start
                    .get_or_insert(new_base_text.anchor_after(new_hunk.diff_base_byte_range.start));
                if end.is_none_or(|end| end.cmp(&new_hunk.buffer_range.end, &new_snapshot).is_le())
                {
                    end.replace(new_hunk.buffer_range.end);
                }
                let new_base_text_end =
                    new_base_text.anchor_after(new_hunk.diff_base_byte_range.end);
                if base_text_end.is_none_or(|base_text_end| {
                    new_base_text_end.cmp(&base_text_end, new_base_text).is_gt()
                }) {
                    base_text_end = Some(new_base_text_end)
                }
                new_cursor.next();
            }
            (None, Some(old_hunk)) => {
                has_changes = true;
                extended_end_candidate = None;
                start.get_or_insert(old_hunk.buffer_range.start);
                base_text_start
                    .get_or_insert(old_base_text.anchor_after(old_hunk.diff_base_byte_range.start));
                if end.is_none_or(|end| end.cmp(&old_hunk.buffer_range.end, &new_snapshot).is_le())
                {
                    end.replace(old_hunk.buffer_range.end);
                }
                let old_base_text_end =
                    old_base_text.anchor_after(old_hunk.diff_base_byte_range.end);
                if base_text_end.is_none_or(|base_text_end| {
                    old_base_text_end.cmp(&base_text_end, new_base_text).is_gt()
                }) {
                    base_text_end = Some(old_base_text_end);
                }
                old_cursor.next();
            }
            (None, None) => break,
        }
    }

    let changed_range = start.zip(end).map(|(start, end)| start..end);
    let base_text_changed_range = base_text_start
        .zip(base_text_end)
        .map(|(start, end)| (start..end).to_offset(new_base_text));

    let extended_range = if has_changes && let Some(changed_range) = changed_range.clone() {
        let extended_start = *last_unchanged_new_hunk_end
            .unwrap_or(text::Anchor::min_for_buffer(new_snapshot.remote_id()))
            .min(&changed_range.start, new_snapshot);
        let extended_start = new_snapshot
            .anchored_edits_since_in_range::<usize>(
                &old_snapshot.version(),
                extended_start..changed_range.start,
            )
            .map(|(_, anchors)| anchors.start)
            .min_by(|a, b| a.cmp(b, new_snapshot))
            .unwrap_or(changed_range.start);

        let extended_end = *extended_end_candidate
            .unwrap_or(text::Anchor::max_for_buffer(new_snapshot.remote_id()))
            .max(&changed_range.end, new_snapshot);
        let extended_end = new_snapshot
            .anchored_edits_since_in_range::<usize>(
                &old_snapshot.version(),
                changed_range.end..extended_end,
            )
            .map(|(_, anchors)| anchors.end)
            .max_by(|a, b| a.cmp(b, new_snapshot))
            .unwrap_or(changed_range.end);

        Some(extended_start..extended_end)
    } else {
        None
    };

    DiffChanged {
        changed_range,
        base_text_changed_range,
        extended_range,
        base_text_changed: false,
    }
}

impl std::fmt::Debug for BufferDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferChangeSet")
            .field("buffer_id", &self.buffer_id)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiffChanged {
    pub changed_range: Option<Range<text::Anchor>>,
    pub base_text_changed_range: Option<Range<usize>>,
    pub extended_range: Option<Range<text::Anchor>>,
    pub base_text_changed: bool,
}

#[derive(Clone, Debug)]
pub enum BufferDiffEvent {
    BaseTextChanged,
    DiffChanged(DiffChanged),
}

impl EventEmitter<BufferDiffEvent> for BufferDiff {}

impl BufferDiff {
    pub fn new(
        buffer: &text::BufferSnapshot,
        language: Option<Arc<Language>>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Self {
        let base_text = cx.new(|cx| {
            let mut base_buffer = language::Buffer::local("", cx);
            base_buffer.set_capability(Capability::ReadOnly, cx);
            if let Some(language_registry) = language_registry {
                base_buffer.set_language_registry(language_registry);
            }
            base_buffer.set_language_async(language, cx);
            base_buffer
        });

        BufferDiff {
            buffer_id: buffer.remote_id(),
            base_text_buffer: base_text,
            diff_snapshot: None,
            buffer_snapshot: buffer.clone(),
            secondary_diff: None,
            operations: None,
            ignore_trim_whitespace: false,
            whitespace_task: None,
        }
    }

    pub fn new_with_base_text_buffer(
        buffer: &text::BufferSnapshot,
        base_text_buffer: Entity<language::Buffer>,
        _cx: &mut App,
    ) -> Self {
        BufferDiff {
            buffer_id: buffer.remote_id(),
            base_text_buffer,
            diff_snapshot: None,
            buffer_snapshot: buffer.clone(),
            secondary_diff: None,
            operations: None,
            ignore_trim_whitespace: false,
            whitespace_task: None,
        }
    }

    pub fn new_unchanged(
        buffer: &text::BufferSnapshot,
        language: Option<Arc<Language>>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let base_text = buffer.text();
        let base_text = cx.new(|cx| {
            let mut base_buffer = language::Buffer::local(base_text, cx);
            base_buffer.set_capability(Capability::ReadOnly, cx);
            if let Some(language_registry) = language_registry {
                base_buffer.set_language_registry(language_registry);
            }
            base_buffer.set_language_async(language, cx);
            base_buffer
        });

        let base_text_snapshot = base_text.read(cx).snapshot();

        let diff_snapshot = BufferDiffSnapshot {
            hunks: SumTree::new(buffer),
            pending_hunks: SumTree::new(buffer),
            base_text: base_text_snapshot,
            base_text_exists: true,
            ignore_trim_whitespace: false,
            buffer_snapshot: buffer.clone(),
            secondary_diff: None,
        };

        BufferDiff {
            buffer_id: buffer.remote_id(),
            base_text_buffer: base_text,
            diff_snapshot: Some(diff_snapshot),
            buffer_snapshot: buffer.clone(),
            secondary_diff: None,
            operations: None,
            ignore_trim_whitespace: false,
            whitespace_task: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_base_text(
        base_text: &str,
        buffer: &text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = BufferDiff::new(buffer, None, None, cx);
        this.set_operations(Arc::new(TestDiffOperations));
        let mut base_text = base_text.to_owned();
        text::LineEnding::normalize(&mut base_text);
        let base_text_buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(base_text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let base_text = base_text_buffer.read(cx).snapshot();
        this.base_text_buffer = base_text_buffer;
        let update = cx.foreground_executor().block_on(this.update_diff(
            buffer.clone(),
            &base_text,
            Some(Arc::from(base_text.text())),
            cx,
        ));
        this.set_snapshot(update, cx);
        this
    }

    pub fn ignore_trim_whitespace(&self) -> bool {
        self.ignore_trim_whitespace
    }

    pub fn set_ignore_trim_whitespace(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.ignore_trim_whitespace == enabled {
            return;
        }
        self.ignore_trim_whitespace = enabled;
        self.whitespace_task = None;
        let Some(snapshot) = self.diff_snapshot.clone() else {
            return;
        };
        let update = self.update_diff(
            snapshot.buffer_snapshot.clone(),
            &snapshot.base_text,
            snapshot
                .base_text_exists
                .then(|| Arc::from(snapshot.base_text.text())),
            cx,
        );
        self.whitespace_task = Some(cx.spawn(async move |this, cx| {
            let update = update.await;
            this.update(cx, |this, cx| {
                if this.ignore_trim_whitespace == update.ignore_trim_whitespace
                    && this.buffer_snapshot.version() == snapshot.buffer_snapshot.version()
                    && this.base_text_exists() == snapshot.base_text_exists
                    && this.base_text(cx).version() == snapshot.base_text.version()
                {
                    this.set_snapshot(update, cx);
                }
            })
            .log_err();
        }));
    }

    pub fn set_secondary_diff(&mut self, diff: Entity<BufferDiff>) {
        self.secondary_diff = Some(diff);
    }

    pub fn set_operations(&mut self, operations: Arc<dyn DiffOperations>) {
        self.operations = Some(operations);
    }

    pub fn operations(&self) -> Option<Arc<dyn DiffOperations>> {
        self.operations.clone()
    }

    pub fn secondary_diff(&self) -> Option<Entity<BufferDiff>> {
        self.secondary_diff.clone()
    }

    pub fn clear_pending_hunks(&mut self, cx: &mut Context<Self>) {
        let Some(diff_snapshot) = &mut self.diff_snapshot else {
            return;
        };
        let Some((first, last)) = diff_snapshot
            .pending_hunks
            .first()
            .zip(diff_snapshot.pending_hunks.last())
        else {
            return;
        };
        let changed_range = first.buffer_range.start..last.buffer_range.end;
        let base_text_changed_range =
            first.diff_base_byte_range.start..last.diff_base_byte_range.end;
        let buffer = diff_snapshot.buffer_snapshot.clone();
        diff_snapshot.pending_hunks = SumTree::new(&buffer);
        cx.emit(BufferDiffEvent::DiffChanged(DiffChanged {
            changed_range: Some(changed_range.clone()),
            base_text_changed_range: Some(base_text_changed_range),
            extended_range: Some(changed_range),
            base_text_changed: false,
        }));
    }

    /// Installs optimistic pending hunks in this diff, merging them with any
    /// existing pending hunks (newest wins on overlap) and emitting a
    /// `DiffChanged` covering both the new hunks and any existing pending hunks
    /// they replace. `hunks` must be sorted by `buffer_range.start` and
    /// non-overlapping.
    ///
    /// `buffer` must be a current snapshot of this diff's main buffer: the
    /// incoming hunks carry anchors minted from the current buffer, which this
    /// diff's internal snapshot (from the last settled recalculation) may not
    /// have observed yet.
    pub fn set_pending_hunks(
        &mut self,
        hunks: &[PendingHunk],
        buffer: &text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) {
        if hunks.is_empty() {
            return;
        }
        let Some(diff_snapshot) = self.diff_snapshot.as_mut() else {
            return;
        };

        let mut new_pending = SumTree::new(buffer);
        let mut old = diff_snapshot
            .pending_hunks
            .cursor::<DiffHunkSummary>(buffer);
        let mut changed_start: Option<Anchor> = None;
        let mut changed_end: Option<Anchor> = None;
        let mut base_start = usize::MAX;
        let mut base_end = 0usize;
        let mut extend_changed_range = |buffer_range: &Range<Anchor>, base_range: &Range<usize>| {
            changed_start = Some(changed_start.map_or(buffer_range.start, |start| {
                *start.min(&buffer_range.start, buffer)
            }));
            changed_end = Some(
                changed_end.map_or(buffer_range.end, |end| *end.max(&buffer_range.end, buffer)),
            );
            base_start = base_start.min(base_range.start);
            base_end = base_end.max(base_range.end);
        };
        for hunk in hunks {
            let preceding = old.slice(&hunk.buffer_range.start, Bias::Left);
            new_pending.append(preceding, buffer);

            // Drop any overlapping or adjacent existing pending hunks, folding
            // them into the changed range so that views repaint their full
            // extent (a replaced hunk can be wider than its replacement).
            while let Some(old_hunk) = old.item() {
                if old_hunk
                    .buffer_range
                    .start
                    .cmp(&hunk.buffer_range.end, buffer)
                    .is_gt()
                {
                    break;
                }
                extend_changed_range(&old_hunk.buffer_range, &old_hunk.diff_base_byte_range);
                old.next();
            }

            extend_changed_range(&hunk.buffer_range, &hunk.diff_base_byte_range);
            new_pending.push(hunk.clone(), buffer);
        }
        new_pending.append(old.suffix(), buffer);
        drop(old);
        diff_snapshot.pending_hunks = new_pending;

        if let (Some(start), Some(end)) = (changed_start, changed_end) {
            let changed_range = Some(start..end);
            cx.emit(BufferDiffEvent::DiffChanged(DiffChanged {
                changed_range: changed_range.clone(),
                base_text_changed_range: Some(base_start..base_end),
                extended_range: changed_range,
                base_text_changed: false,
            }));
        }
    }
    pub fn suppress_all_hunks_pending(
        &mut self,
        buffer: &text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) {
        let version = buffer.version().clone();
        let hunks = self
            .snapshot(cx)
            .raw_hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                buffer,
            )
            .map(|hunk| {
                PendingHunk::new(
                    hunk.buffer_range,
                    hunk.diff_base_byte_range,
                    version.clone(),
                    PendingSense::Suppress,
                )
            })
            .collect::<Vec<_>>();
        self.set_pending_hunks(&hunks, buffer, cx);
    }
    pub fn update_diff(
        &self,
        buffer: text::BufferSnapshot,
        base_text_snapshot: &language::BufferSnapshot,
        base_text: Option<Arc<str>>,
        cx: &App,
    ) -> Task<BufferDiffUpdate> {
        let base_text = base_text.map(|t| text::LineEnding::normalize_arc(t));
        debug_assert_eq!(
            base_text.as_deref().unwrap_or_default(),
            &base_text_snapshot.text()
        );
        debug_assert_eq!(
            base_text_snapshot.remote_id(),
            self.base_text_buffer.read(cx).remote_id()
        );

        let language = base_text_snapshot.language();
        let diff_options = build_diff_options(
            language.map(|l| l.name()),
            language.map(|l| l.default_scope()),
            cx,
        );
        let ignore_trim_whitespace = self.ignore_trim_whitespace;
        let buffer_snapshot = buffer.clone();
        let base_text_snapshot = base_text_snapshot.clone();
        let base_text_exists = base_text.is_some();
        let unchanged_hunks = self.diff_snapshot.as_ref().and_then(|diff_snapshot| {
            if diff_snapshot.ignore_trim_whitespace == ignore_trim_whitespace
                && diff_snapshot.base_text_exists == base_text_exists
                && diff_snapshot.base_text.version() == base_text_snapshot.version()
                && diff_snapshot.buffer_snapshot.version() == buffer_snapshot.version()
            {
                Some(diff_snapshot.hunks.clone())
            } else {
                None
            }
        });

        cx.background_executor().spawn(async move {
            let hunks = if let Some(unchanged_hunks) = unchanged_hunks {
                unchanged_hunks
            } else if let Some(base_text) = base_text {
                compute_hunks(
                    Some((base_text, base_text_snapshot.as_rope().clone())),
                    &buffer,
                    diff_options,
                    ignore_trim_whitespace,
                )
            } else {
                compute_hunks(None, &buffer, diff_options, ignore_trim_whitespace)
            };

            BufferDiffUpdate {
                ignore_trim_whitespace,
                hunks,
                base_text: base_text_snapshot,
                base_text_exists,
                buffer_snapshot,
            }
        })
    }

    pub fn set_snapshot_with_secondary(
        &mut self,
        update: BufferDiffUpdate,
        secondary_diff_change: Option<Range<Anchor>>,
        clear_pending_hunks: bool,
        cx: &mut Context<Self>,
    ) -> Option<Range<Anchor>> {
        log::debug!("set snapshot with secondary {secondary_diff_change:?}");

        let BufferDiffUpdate {
            hunks: new_hunks,
            ignore_trim_whitespace,
            base_text: new_base_text,
            base_text_exists: new_base_text_exists,
            buffer_snapshot: new_buffer_snapshot,
        } = update;
        let buffer = &new_buffer_snapshot;
        let old_snapshot = self
            .diff_snapshot
            .clone()
            .unwrap_or_else(|| BufferDiffSnapshot {
                hunks: SumTree::new(buffer),
                pending_hunks: SumTree::new(buffer),
                base_text: new_base_text.clone(),
                base_text_exists: false,
                ignore_trim_whitespace: self.ignore_trim_whitespace,
                buffer_snapshot: new_buffer_snapshot.clone(),
                secondary_diff: None,
            });
        let mut new_snapshot = BufferDiffSnapshot {
            ignore_trim_whitespace,
            hunks: new_hunks.clone(),
            base_text: new_base_text.clone(),
            base_text_exists: new_base_text_exists,
            buffer_snapshot: new_buffer_snapshot.clone(),
            pending_hunks: old_snapshot.pending_hunks.clone(),
            secondary_diff: None,
        };

        let old_base_text_exists = old_snapshot.base_text_exists;
        let old_buffer_snapshot = &old_snapshot.buffer_snapshot;
        let old_base_text = &old_snapshot.base_text;
        let base_text_changed = old_base_text_exists != new_base_text_exists
            || (new_base_text_exists
                && (old_base_text.remote_id() != new_base_text.remote_id()
                    || new_base_text
                        .version()
                        .changed_since(old_base_text.version())));
        let DiffChanged {
            mut changed_range,
            mut base_text_changed_range,
            mut extended_range,
            base_text_changed: _,
        } = match (old_base_text_exists, new_base_text_exists) {
            (false, false) if self.diff_snapshot.is_some() => DiffChanged::default(),
            (true, true) => compare_hunks(
                &new_hunks,
                &old_snapshot.hunks,
                old_buffer_snapshot,
                buffer,
                old_base_text,
                &new_base_text,
            ),
            _ => {
                let full_range = text::Anchor::min_max_range_for_buffer(self.buffer_id);
                let full_base_range = 0..new_base_text.len();
                DiffChanged {
                    changed_range: Some(full_range.clone()),
                    base_text_changed_range: Some(full_base_range),
                    extended_range: Some(full_range),
                    base_text_changed: false,
                }
            }
        };

        if base_text_changed || clear_pending_hunks {
            if let Some((first, last)) = old_snapshot
                .pending_hunks
                .first()
                .zip(old_snapshot.pending_hunks.last())
            {
                let pending_range = first.buffer_range.start..last.buffer_range.end;
                if let Some(range) = &mut changed_range {
                    range.start = *range.start.min(&pending_range.start, buffer);
                    range.end = *range.end.max(&pending_range.end, buffer);
                } else {
                    changed_range = Some(pending_range.clone());
                }

                if let Some(base_text_range) = base_text_changed_range.as_mut() {
                    base_text_range.start =
                        base_text_range.start.min(first.diff_base_byte_range.start);
                    base_text_range.end = base_text_range.end.max(last.diff_base_byte_range.end);
                } else {
                    base_text_changed_range =
                        Some(first.diff_base_byte_range.start..last.diff_base_byte_range.end);
                }

                if let Some(ext) = &mut extended_range {
                    ext.start = *ext.start.min(&pending_range.start, buffer);
                    ext.end = *ext.end.max(&pending_range.end, buffer);
                } else {
                    extended_range = Some(pending_range);
                }
            }
            new_snapshot.pending_hunks = SumTree::new(buffer);
        }

        if let Some(secondary_changed_range) = secondary_diff_change
            && let (Some(secondary_hunk_range), Some(secondary_base_range)) =
                old_snapshot.range_to_hunk_range(secondary_changed_range, buffer)
        {
            if let Some(range) = &mut changed_range {
                range.start = *secondary_hunk_range.start.min(&range.start, buffer);
                range.end = *secondary_hunk_range.end.max(&range.end, buffer);
            } else {
                changed_range = Some(secondary_hunk_range.clone());
            }

            if let Some(base_text_range) = base_text_changed_range.as_mut() {
                base_text_range.start = secondary_base_range.start.min(base_text_range.start);
                base_text_range.end = secondary_base_range.end.max(base_text_range.end);
            } else {
                base_text_changed_range = Some(secondary_base_range);
            }

            if let Some(ext) = &mut extended_range {
                ext.start = *ext.start.min(&secondary_hunk_range.start, buffer);
                ext.end = *ext.end.max(&secondary_hunk_range.end, buffer);
            } else {
                extended_range = Some(secondary_hunk_range);
            }
        }

        self.diff_snapshot = Some(new_snapshot);
        self.buffer_snapshot = new_buffer_snapshot;

        let result = DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range,
            base_text_changed,
        };
        if result.base_text_changed {
            cx.emit(BufferDiffEvent::BaseTextChanged);
        }
        let changed_range = result.changed_range.clone();
        cx.emit(BufferDiffEvent::DiffChanged(result));
        changed_range
    }

    pub fn set_snapshot(
        &mut self,
        new_state: BufferDiffUpdate,
        cx: &mut Context<Self>,
    ) -> Option<Range<Anchor>> {
        self.set_snapshot_with_secondary(new_state, None, false, cx)
    }

    pub fn base_text(&self, cx: &App) -> language::BufferSnapshot {
        self.base_text_buffer.read(cx).snapshot()
    }

    pub fn base_text_exists(&self) -> bool {
        self.diff_snapshot
            .as_ref()
            .is_some_and(|diff_snapshot| diff_snapshot.base_text_exists)
    }

    pub fn changed_row_counts(&self) -> (u32, u32) {
        self.diff_snapshot
            .as_ref()
            .map_or((0, 0), |diff_snapshot| diff_snapshot.changed_row_counts())
    }

    pub fn snapshot(&self, cx: &App) -> BufferDiffSnapshot {
        let mut snapshot = self.diff_snapshot.clone().unwrap_or_else(|| {
            let base_text = self.base_text_buffer.read(cx).snapshot();
            BufferDiffSnapshot {
                hunks: SumTree::new(&self.buffer_snapshot),
                pending_hunks: SumTree::new(&self.buffer_snapshot),
                base_text,
                base_text_exists: false,
                ignore_trim_whitespace: self.ignore_trim_whitespace,
                buffer_snapshot: self.buffer_snapshot.clone(),
                secondary_diff: None,
            }
        });
        snapshot.secondary_diff = self.secondary_diff.as_ref().map(|diff| {
            debug_assert!(diff.read(cx).secondary_diff.is_none());
            Arc::new(diff.read(cx).snapshot(cx))
        });
        snapshot
    }

    ///
    /// Dropping the returned task cancels the update, leaving the diff
    /// unchanged. Calls must not overlap; to re-run this when the buffer or
    /// base text changes, store the task somewhere that the next call will
    /// overwrite, so that the previous call is cancelled.
    pub fn set_base_text(
        &mut self,
        base_text: Option<Arc<str>>,
        buffer: text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let base_text_exists = base_text.is_some();
            let base_text = base_text.unwrap_or_default();
            let Some(base_text_diff) = this
                .update(cx, |this, cx| {
                    this.base_text_buffer.update(cx, |base_text_buffer, cx| {
                        base_text_buffer.diff(base_text.clone(), cx)
                    })
                })
                .log_err()
            else {
                return;
            };
            let base_text_diff = base_text_diff.await;
            let Some(edited_base_text) = this
                .update(cx, |this, cx| {
                    if this.base_text_buffer.read(cx).version() != base_text_diff.base_version {
                        log::warn!("dropping concurrent diff update");
                        debug_panic!("incorrect concurrent call to set_base_text");
                        return None;
                    }
                    let edited_base_text =
                        this.base_text_buffer.update(cx, |base_text_buffer, cx| {
                            base_text_buffer.set_line_ending(base_text_diff.line_ending, cx);
                            assert!(base_text_buffer.version() == base_text_diff.base_version);
                            base_text_buffer.snapshot_with_edits(base_text_diff.edits, cx)
                        });
                    Some(edited_base_text)
                })
                .log_err()
                .flatten()
            else {
                return;
            };
            let edited_base_text = edited_base_text.await;
            let base_text_snapshot = edited_base_text.snapshot().clone();
            let Some(state) = this
                .update(cx, |this, cx| {
                    this.update_diff(
                        buffer.clone(),
                        &base_text_snapshot,
                        base_text_exists.then(|| base_text.clone()),
                        cx,
                    )
                })
                .log_err()
            else {
                return;
            };
            let mut state = state.await;
            loop {
                let Some(next) = this
                    .update(cx, |this, cx| {
                        (state.ignore_trim_whitespace != this.ignore_trim_whitespace).then(|| {
                            this.update_diff(
                                buffer.clone(),
                                &base_text_snapshot,
                                base_text_exists.then(|| base_text.clone()),
                                cx,
                            )
                        })
                    })
                    .log_err()
                else {
                    return;
                };
                let Some(next) = next else { break };
                state = next.await;
            }
            this.update(cx, |this, cx| {
                if &this.base_text_buffer.read(cx).version() != edited_base_text.base_version() {
                    log::warn!("dropping concurrent diff update");
                    debug_panic!("incorrect concurrent call to set_base_text");
                    return;
                }

                this.base_text_buffer.update(cx, |base_text_buffer, cx| {
                    base_text_buffer.fast_forward(edited_base_text, cx)
                });
                this.set_snapshot(state, cx);
            })
            .log_err();
        })
    }

    pub fn base_text_string(&self, _cx: &App) -> Option<String> {
        self.diff_snapshot.as_ref().and_then(|diff_snapshot| {
            if diff_snapshot.base_text_exists {
                Some(diff_snapshot.base_text.text())
            } else {
                None
            }
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn recalculate_diff_sync(&mut self, buffer: &text::BufferSnapshot, cx: &mut Context<Self>) {
        let base_text = self.base_text(cx);
        let fut = self.update_diff(
            buffer.clone(),
            &base_text,
            self.base_text_exists().then(|| Arc::from(base_text.text())),
            cx,
        );
        let fg_executor = cx.foreground_executor().clone();
        let snapshot = fg_executor.block_on(fut);
        let _changed_range = self.set_snapshot(snapshot, cx);
    }

    pub fn base_text_buffer(&self) -> &Entity<language::Buffer> {
        &self.base_text_buffer
    }
}

impl DiffHunk {
    pub fn is_created_file(&self) -> bool {
        self.diff_base_byte_range == (0..0)
            && self.buffer_range.start.is_min()
            && self.buffer_range.end.is_max()
    }

    pub fn status(&self) -> DiffHunkStatus {
        let kind = if self.buffer_range.start == self.buffer_range.end {
            DiffHunkStatusKind::Deleted
        } else if self.diff_base_byte_range.is_empty() {
            DiffHunkStatusKind::Added
        } else {
            DiffHunkStatusKind::Modified
        };
        DiffHunkStatus {
            kind,
            secondary: self.secondary_status,
        }
    }
}

impl DiffHunkStatus {
    pub fn has_secondary_hunk(&self) -> bool {
        matches!(
            self.secondary,
            DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending
                | DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk
        )
    }

    pub fn is_pending(&self) -> bool {
        matches!(
            self.secondary,
            DiffHunkSecondaryStatus::SecondaryHunkAdditionPending
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending
        )
    }

    pub fn is_deleted(&self) -> bool {
        self.kind == DiffHunkStatusKind::Deleted
    }

    pub fn is_added(&self) -> bool {
        self.kind == DiffHunkStatusKind::Added
    }

    pub fn is_modified(&self) -> bool {
        self.kind == DiffHunkStatusKind::Modified
    }

    pub fn added(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Added,
            secondary,
        }
    }

    pub fn modified(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Modified,
            secondary,
        }
    }

    pub fn deleted(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Deleted,
            secondary,
        }
    }

    pub fn deleted_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Deleted,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }

    pub fn added_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Added,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }

    pub fn modified_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Modified,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[track_caller]
pub fn assert_hunks<ExpectedText, HunkIter>(
    diff_hunks: HunkIter,
    buffer: &text::BufferSnapshot,
    diff_base: &str,
    // Line range, deleted, added, status
    expected_hunks: &[(Range<u32>, ExpectedText, ExpectedText, DiffHunkStatus)],
) where
    HunkIter: Iterator<Item = DiffHunk>,
    ExpectedText: AsRef<str>,
{
    let actual_hunks = diff_hunks
        .map(|hunk| {
            (
                hunk.range.clone(),
                &diff_base[hunk.diff_base_byte_range.clone()],
                buffer
                    .text_for_range(hunk.range.clone())
                    .collect::<String>(),
                hunk.status(),
            )
        })
        .collect::<Vec<_>>();

    let expected_hunks: Vec<_> = expected_hunks
        .iter()
        .map(|(line_range, deleted_text, added_text, status)| {
            (
                Point::new(line_range.start, 0)..Point::new(line_range.end, 0),
                deleted_text.as_ref(),
                added_text.as_ref().to_string(),
                *status,
            )
        })
        .collect();

    pretty_assertions::assert_eq!(actual_hunks, expected_hunks);
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::{assert_eq, assert_ne};
    use rand::{Rng as _, rngs::StdRng};
    use text::{Buffer, BufferId, ReplicaId, Rope};
    use unindent::Unindent as _;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    #[gpui::test]
    async fn test_ignore_trim_whitespace_rules_and_original_ranges(cx: &mut gpui::TestAppContext) {
        for (base, modified, empty) in [
            ("  value \n", "\tvalue\t\n", true),
            (" \t\n", "\n", true),
            ("  value", "value\t", true),
            ("a b\n", "ab\n", false),
            ("a b\n", "a  b\n", false),
            ("value\n", "value\n\n", false),
            ("value\n\n", "value\n", false),
            ("value\n", "value", false),
            ("", " \n", false),
            (" \n", "", false),
        ] {
            let buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).expect("id"), modified);
            let diff = cx.new(|cx| BufferDiff::new_with_base_text(base, &buffer, cx));
            assert!(
                !diff.read_with(cx, |diff, cx| diff.snapshot(cx).is_empty()),
                "exact: {base:?} {modified:?}"
            );
            diff.update(cx, |diff, cx| diff.set_ignore_trim_whitespace(true, cx));
            cx.run_until_parked();
            diff.read_with(cx, |diff, cx| {
                assert_eq!(
                    diff.snapshot(cx).is_empty(),
                    empty,
                    "trim: {base:?} {modified:?}"
                );
                assert_eq!(diff.base_text_string(cx).as_deref(), Some(base));
                assert_eq!(diff.buffer_snapshot.text(), modified);
            });
            diff.update(cx, |diff, cx| diff.set_ignore_trim_whitespace(false, cx));
            cx.run_until_parked();
            assert!(!diff.read_with(cx, |diff, cx| diff.snapshot(cx).is_empty()));
        }

        let base = "  same \n  old \n\tend\n";
        let modified = "same\n\tnew\t\nend \n";
        let buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).expect("id"), modified);
        let diff = cx.new(|cx| BufferDiff::new_with_base_text(base, &buffer, cx));
        diff.update(cx, |diff, cx| diff.set_ignore_trim_whitespace(true, cx));
        cx.run_until_parked();
        diff.read_with(cx, |diff, cx| {
            let snapshot = diff.snapshot(cx);
            assert_hunks(
                snapshot.hunks(&buffer),
                &buffer,
                base,
                &[(
                    1..2,
                    "  old \n",
                    "\tnew\t\n",
                    DiffHunkStatus::modified_none(),
                )],
            );
            let hunk = snapshot.hunks(&buffer).next().expect("content change");
            assert_eq!(hunk.base_word_diffs, vec![2..5]);
            assert_eq!(
                hunk.buffer_word_diffs
                    .iter()
                    .map(|range| range.to_offset(&buffer))
                    .collect::<Vec<_>>(),
                vec![6..9]
            );
        });
    }

    #[gpui::test(iterations = 10)]
    async fn test_ignore_trim_whitespace_during_content_updates(cx: &mut gpui::TestAppContext) {
        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).expect("id"), "old\n");
        let diff = cx.new(|cx| BufferDiff::new_with_base_text("old\n", &buffer, cx));
        buffer.edit([(0..4, "  new \n")]);
        let update = diff.update(cx, |diff, cx| {
            let update =
                diff.set_base_text(Some(Arc::from("new\n")), buffer.snapshot().clone(), cx);
            diff.set_ignore_trim_whitespace(true, cx);
            update
        });
        cx.executor().simulate_random_delay().await;
        diff.update(cx, |diff, cx| {
            diff.set_ignore_trim_whitespace(false, cx);
            diff.set_ignore_trim_whitespace(true, cx);
        });
        update.await;
        cx.run_until_parked();
        diff.read_with(cx, |diff, cx| {
            assert!(diff.snapshot(cx).is_empty());
            assert!(diff.snapshot(cx).ignore_trim_whitespace);
            assert_eq!(diff.base_text_string(cx).as_deref(), Some("new\n"));
            assert_eq!(diff.buffer_snapshot.text(), "  new \n");
        });
        diff.update(cx, |diff, cx| diff.set_ignore_trim_whitespace(false, cx));
        cx.run_until_parked();
        assert!(!diff.read_with(cx, |diff, cx| diff.snapshot(cx).is_empty()));
    }

    #[gpui::test]
    async fn test_buffer_diff_simple(cx: &mut gpui::TestAppContext) {
        let diff_base = "
            one
            two
            three
        "
        .unindent();

        let buffer_text = "
            one
            HELLO
            three
        "
        .unindent();

        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text);
        let mut diff = BufferDiffSnapshot::new_sync(&buffer, diff_base.clone(), cx);
        assert_hunks(
            diff.hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                &buffer,
            ),
            &buffer,
            &diff_base,
            &[(1..2, "two\n", "HELLO\n", DiffHunkStatus::modified_none())],
        );

        buffer.edit([(0..0, "point five\n")]);
        diff = BufferDiffSnapshot::new_sync(&buffer, diff_base.clone(), cx);
        assert_hunks(
            diff.hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                &buffer,
            ),
            &buffer,
            &diff_base,
            &[
                (0..1, "", "point five\n", DiffHunkStatus::added_none()),
                (2..3, "two\n", "HELLO\n", DiffHunkStatus::modified_none()),
            ],
        );

        diff = cx.update(|cx| BufferDiff::new(&buffer, None, None, cx).snapshot(cx));
        assert_hunks::<&str, _>(
            diff.hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                &buffer,
            ),
            &buffer,
            &diff_base,
            &[],
        );
    }

    #[gpui::test]
    async fn test_buffer_diff_with_secondary(cx: &mut gpui::TestAppContext) {
        let head_text = "
            zero
            one
            two
            three
            four
            five
            six
            seven
            eight
            nine
        "
        .unindent();

        let index_text = "
            zero
            one
            TWO
            three
            FOUR
            five
            six
            seven
            eight
            NINE
        "
        .unindent();

        let buffer_text = "
            zero
            one
            TWO
            three
            FOUR
            FIVE
            six
            SEVEN
            eight
            nine
        "
        .unindent();

        let buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text);
        let unstaged_diff = BufferDiffSnapshot::new_sync(&buffer, index_text, cx);
        let mut uncommitted_diff = BufferDiffSnapshot::new_sync(&buffer, head_text.clone(), cx);
        uncommitted_diff.secondary_diff = Some(Arc::new(unstaged_diff));

        let expected_hunks = vec![
            (2..3, "two\n", "TWO\n", DiffHunkStatus::modified_none()),
            (
                4..6,
                "four\nfive\n",
                "FOUR\nFIVE\n",
                DiffHunkStatus::modified(DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk),
            ),
            (
                7..8,
                "seven\n",
                "SEVEN\n",
                DiffHunkStatus::modified(DiffHunkSecondaryStatus::HasSecondaryHunk),
            ),
        ];

        assert_hunks(
            uncommitted_diff.hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                &buffer,
            ),
            &buffer,
            &head_text,
            &expected_hunks,
        );
    }

    #[gpui::test]
    async fn test_buffer_diff_range(cx: &mut TestAppContext) {
        let diff_base = "
            one
            two
            three
            four
            five
            six
            seven
            eight
            nine
            ten
        "
        .unindent();

        let buffer_text = "
            A
            one
            B
            two
            C
            three
            HELLO
            four
            five
            SIXTEEN
            seven
            eight
            WORLD
            nine

            ten

        "
        .unindent();

        let buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text);
        let diff = BufferDiffSnapshot::new_sync(buffer.snapshot(), diff_base.clone(), cx);
        assert_eq!(
            diff.hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(buffer.remote_id()),
                &buffer
            )
            .count(),
            8
        );

        assert_hunks(
            diff.hunks_intersecting_range(
                buffer.anchor_before(Point::new(7, 0))..buffer.anchor_before(Point::new(12, 0)),
                &buffer,
            ),
            &buffer,
            &diff_base,
            &[
                (6..7, "", "HELLO\n", DiffHunkStatus::added_none()),
                (9..10, "six\n", "SIXTEEN\n", DiffHunkStatus::modified_none()),
                (12..13, "", "WORLD\n", DiffHunkStatus::added_none()),
            ],
        );
    }
    #[gpui::test]
    async fn test_set_pending_hunks_change_covers_replaced_hunks(cx: &mut TestAppContext) {
        let base_text = "
            zero
            one
            two
            three
            four
            five
        "
        .unindent();
        let buffer_text = "
            ZERO
            one
            two
            THREE
            four
            FIVE
        "
        .unindent();
        let buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text);
        let diff = cx.new(|cx| BufferDiff::new_with_base_text(&base_text, &buffer, cx));

        // Install a whole-file pending hunk, as the no-HEAD staging paths do.
        let version = buffer.version();
        diff.update(cx, |diff, cx| {
            diff.set_pending_hunks(
                &[PendingHunk::new(
                    Anchor::min_max_range_for_buffer(buffer.remote_id()),
                    0..base_text.len(),
                    version.clone(),
                    PendingSense::SetSecondaryStatus(
                        DiffHunkSecondaryStatus::SecondaryHunkRemovalPending,
                    ),
                )],
                &buffer,
                cx,
            )
        });

        let (tx, rx) = mpsc::channel();
        let subscription =
            cx.update(|cx| cx.subscribe(&diff, move |_, event, _| tx.send(event.clone()).unwrap()));

        // Replace it with a narrower hunk; the emitted change must still cover
        // the whole extent of the replaced hunk.
        diff.update(cx, |diff, cx| {
            diff.set_pending_hunks(
                &[PendingHunk::new(
                    buffer.anchor_before(Point::new(3, 0))..buffer.anchor_before(Point::new(4, 0)),
                    base_text.find("three").unwrap()..base_text.find("four").unwrap(),
                    version,
                    PendingSense::Suppress,
                )],
                &buffer,
                cx,
            )
        });

        drop(subscription);
        let events = rx.into_iter().collect::<Vec<_>>();
        match events.as_slice() {
            [
                BufferDiffEvent::DiffChanged(DiffChanged {
                    changed_range: Some(changed_range),
                    ..
                }),
            ] => {
                assert_eq!(
                    changed_range.to_point(&buffer),
                    Point::zero()..buffer.max_point(),
                );
            }
            _ => panic!("unexpected events: {:?}", events),
        }
    }

    #[gpui::test]
    async fn test_buffer_diff_compare(cx: &mut TestAppContext) {
        let base_text = "
            zero
            one
            two
            three
            four
            five
            six
            seven
            eight
            nine
        "
        .unindent();

        let buffer_text_1 = "
            one
            three
            four
            five
            SIX
            seven
            eight
            NINE
        "
        .unindent();

        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text_1);

        let empty_diff = cx.update(|cx| BufferDiff::new(&buffer, None, None, cx).snapshot(cx));
        let diff_1 = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_1.hunks,
            &empty_diff.hunks,
            &buffer,
            &buffer,
            &diff_1.base_text(),
            &diff_1.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(0, 0)..Point::new(8, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_1.base_text()),
            Point::new(0, 0)..Point::new(10, 0)
        );

        // Edit does affects the diff because it recalculates word diffs.
        buffer.edit_via_marked_text(
            &"
                one
                three
                four
                five
                «SIX.5»
                seven
                eight
                NINE
            "
            .unindent(),
        );
        let diff_2 = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_2.hunks,
            &diff_1.hunks,
            &buffer,
            &buffer,
            diff_2.base_text(),
            diff_2.base_text(),
        );
        assert_eq!(
            changed_range.unwrap().to_point(&buffer),
            Point::new(4, 0)..Point::new(5, 0),
        );
        assert_eq!(
            base_text_changed_range
                .unwrap()
                .to_point(diff_2.base_text()),
            Point::new(6, 0)..Point::new(7, 0),
        );

        // Edit turns a deletion hunk into a modification.
        buffer.edit_via_marked_text(
            &"
                one
                «THREE»
                four
                five
                SIX.5
                seven
                eight
                NINE
            "
            .unindent(),
        );
        let diff_3 = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_3.hunks,
            &diff_2.hunks,
            &buffer,
            &buffer,
            diff_3.base_text(),
            diff_3.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(1, 0)..Point::new(2, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_3.base_text()),
            Point::new(2, 0)..Point::new(4, 0)
        );

        // Edit turns a modification hunk into a deletion.
        buffer.edit_via_marked_text(
            &"
                one
                THREE
                four
                five«»
                seven
                eight
                NINE
            "
            .unindent(),
        );
        let diff_4 = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_4.hunks,
            &diff_3.hunks,
            &buffer,
            &buffer,
            diff_4.base_text(),
            diff_4.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(3, 4)..Point::new(4, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_4.base_text()),
            Point::new(6, 0)..Point::new(7, 0)
        );

        // Edit introduces a new insertion hunk.
        buffer.edit_via_marked_text(
            &"
                one
                THREE
                four«
                FOUR.5
                »five
                seven
                eight
                NINE
            "
            .unindent(),
        );
        let diff_5 = BufferDiffSnapshot::new_sync(buffer.snapshot(), base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_5.hunks,
            &diff_4.hunks,
            &buffer,
            &buffer,
            diff_5.base_text(),
            diff_5.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(3, 0)..Point::new(4, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_5.base_text()),
            Point::new(5, 0)..Point::new(5, 0)
        );

        // Edit removes a hunk.
        buffer.edit_via_marked_text(
            &"
                one
                THREE
                four
                FOUR.5
                five
                seven
                eight
                «nine»
            "
            .unindent(),
        );
        let diff_6 = BufferDiffSnapshot::new_sync(buffer.snapshot(), base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_6.hunks,
            &diff_5.hunks,
            &buffer,
            &buffer,
            diff_6.base_text(),
            diff_6.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(7, 0)..Point::new(8, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_6.base_text()),
            Point::new(9, 0)..Point::new(10, 0)
        );

        buffer.edit_via_marked_text(
            &"
                one
                THREE
                four«»
                five
                seven
                eight
                «NINE»
            "
            .unindent(),
        );

        let diff_7 = BufferDiffSnapshot::new_sync(buffer.snapshot(), base_text.clone(), cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_7.hunks,
            &diff_6.hunks,
            &buffer,
            &buffer,
            diff_7.base_text(),
            diff_7.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(2, 4)..Point::new(7, 0));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_7.base_text()),
            Point::new(5, 0)..Point::new(10, 0)
        );

        buffer.edit_via_marked_text(
            &"
                one
                THREE
                four
                five«»seven
                eight
                NINE
            "
            .unindent(),
        );

        let diff_8 = BufferDiffSnapshot::new_sync(buffer.snapshot(), base_text, cx);
        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &diff_8.hunks,
            &diff_7.hunks,
            &buffer,
            &buffer,
            diff_8.base_text(),
            diff_8.base_text(),
        );
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(3, 0)..Point::new(3, 4));
        let base_text_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_text_range.to_point(diff_8.base_text()),
            Point::new(5, 0)..Point::new(8, 0)
        );
    }
    #[gpui::test]
    async fn test_changed_ranges(cx: &mut gpui::TestAppContext) {
        let base_text = "
            one
            two
            three
            four
            five
            six
        "
        .unindent();
        let buffer_text = "
            one
            TWO
            three
            four
            FIVE
            six
        "
        .unindent();
        let buffer = cx.new(|cx| language::Buffer::local(buffer_text, cx));
        let diff = cx.new(|cx| {
            BufferDiff::new_with_base_text(&base_text, &buffer.read(cx).text_snapshot(), cx)
        });
        cx.run_until_parked();
        let (tx, rx) = mpsc::channel();
        let subscription =
            cx.update(|cx| cx.subscribe(&diff, move |_, event, _| tx.send(event.clone()).unwrap()));

        let snapshot = buffer.update(cx, |buffer, cx| {
            buffer.set_text(
                "
                ONE
                TWO
                THREE
                FOUR
                FIVE
                SIX
            "
                .unindent(),
                cx,
            );
            buffer.text_snapshot()
        });
        let base_text_snapshot = diff.read_with(cx, |diff, cx| diff.base_text(cx));
        let update = diff
            .update(cx, |diff, cx| {
                diff.update_diff(
                    snapshot.clone(),
                    &base_text_snapshot,
                    Some(Arc::from(base_text_snapshot.text())),
                    cx,
                )
            })
            .await;
        diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
        cx.run_until_parked();
        drop(subscription);
        let events = rx.into_iter().collect::<Vec<_>>();
        match events.as_slice() {
            [
                BufferDiffEvent::DiffChanged(DiffChanged {
                    changed_range: _,
                    base_text_changed_range,
                    extended_range: _,
                    base_text_changed: _,
                }),
            ] => {
                // TODO(cole) this seems like it should pass but currently fails (see compare_hunks)
                // assert_eq!(
                //     *changed_range,
                //     Some(Anchor::min_max_range_for_buffer(
                //         buffer.read_with(cx, |buffer, _| buffer.remote_id())
                //     ))
                // );
                assert_eq!(*base_text_changed_range, Some(0..base_text.len()));
            }
            _ => panic!("unexpected events: {:?}", events),
        }
    }

    #[gpui::test]
    async fn test_extended_range(cx: &mut TestAppContext) {
        let base_text = "
            aaa
            bbb





            ccc
            ddd
        "
        .unindent();

        let buffer_text = "
            aaa
            bbb





            CCC
            ddd
        "
        .unindent();

        let mut buffer = Buffer::new(ReplicaId::LOCAL, BufferId::new(1).unwrap(), buffer_text);
        let old_buffer = buffer.snapshot().clone();
        let diff_a = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);

        buffer.edit([(Point::new(1, 3)..Point::new(1, 3), "\n")]);
        let diff_b = BufferDiffSnapshot::new_sync(&buffer, base_text, cx);

        let DiffChanged {
            changed_range,
            base_text_changed_range: _,
            extended_range,
            base_text_changed: _,
        } = compare_hunks(
            &diff_b.hunks,
            &diff_a.hunks,
            &old_buffer,
            &buffer,
            &diff_a.base_text(),
            &diff_a.base_text(),
        );

        let changed_range = changed_range.unwrap();
        assert_eq!(
            changed_range.to_point(&buffer),
            Point::new(7, 0)..Point::new(9, 0),
            "changed_range should span from old hunk position to new hunk end"
        );

        let extended_range = extended_range.unwrap();
        assert_eq!(
            extended_range.start.to_point(&buffer),
            Point::new(1, 3),
            "extended_range.start should extend to include the edit outside changed_range"
        );
        assert_eq!(
            extended_range.end.to_point(&buffer),
            Point::new(9, 0),
            "extended_range.end should collapse to changed_range.end when no edits in end margin"
        );

        let base_text_2 = "
            one
            two
            three
            four
            five
            six
            seven
            eight
        "
        .unindent();

        let buffer_text_2 = "
            ONE
            two
            THREE
            four
            FIVE
            six
            SEVEN
            eight
        "
        .unindent();

        let mut buffer_2 = Buffer::new(ReplicaId::LOCAL, BufferId::new(2).unwrap(), buffer_text_2);
        let old_buffer_2 = buffer_2.snapshot().clone();
        let diff_2a = BufferDiffSnapshot::new_sync(&buffer_2, base_text_2.clone(), cx);

        buffer_2.edit([(Point::new(4, 0)..Point::new(4, 4), "FIVE_CHANGED")]);
        let diff_2b = BufferDiffSnapshot::new_sync(&buffer_2, base_text_2, cx);

        let DiffChanged {
            changed_range,
            base_text_changed_range: _,
            extended_range,
            base_text_changed: _,
        } = compare_hunks(
            &diff_2b.hunks,
            &diff_2a.hunks,
            &old_buffer_2,
            &buffer_2,
            &diff_2a.base_text(),
            &diff_2a.base_text(),
        );

        let changed_range = changed_range.unwrap();
        assert_eq!(
            changed_range.to_point(&buffer_2),
            Point::new(4, 0)..Point::new(5, 0),
            "changed_range should be just the hunk that changed (FIVE)"
        );

        let extended_range = extended_range.unwrap();
        assert_eq!(
            extended_range.to_point(&buffer_2),
            Point::new(4, 0)..Point::new(5, 0),
            "extended_range should equal changed_range when edit is within the hunk"
        );
    }

    #[gpui::test]
    async fn test_buffer_diff_compare_with_base_text_change(_cx: &mut TestAppContext) {
        // Use a shared base text buffer so that anchors from old and new snapshots
        // share the same remote_id and resolve correctly across versions.
        let initial_base = "aaa\nbbb\nccc\nddd\neee\n";
        let mut base_text_buffer = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(99).unwrap(),
            initial_base.to_string(),
        );

        // --- Scenario 1: Base text gains a line, producing a new deletion hunk ---
        //
        // Buffer has a modification (ccc → CCC). When the base text gains
        // a new line "XXX" after "aaa", the diff now also contains a
        // deletion for that line, and the modification hunk shifts in the
        // base text.
        let buffer_text_1 = "aaa\nbbb\nCCC\nddd\neee\n";
        let buffer = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(1).unwrap(),
            buffer_text_1.to_string(),
        );

        let old_base_snapshot_1 = base_text_buffer.snapshot().clone();
        let old_hunks_1 = compute_hunks(
            Some((Arc::from(initial_base), Rope::from(initial_base))),
            buffer.snapshot(),
            None,
            false,
        );

        // Insert "XXX\n" after "aaa\n" in the base text.
        base_text_buffer.edit([(4..4, "XXX\n")]);
        let new_base_str_1: Arc<str> = Arc::from(base_text_buffer.text().as_str());
        let new_base_snapshot_1 = base_text_buffer.snapshot();

        let new_hunks_1 = compute_hunks(
            Some((new_base_str_1.clone(), Rope::from(new_base_str_1.as_ref()))),
            buffer.snapshot(),
            None,
            false,
        );

        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &new_hunks_1,
            &old_hunks_1,
            &buffer.snapshot(),
            &buffer.snapshot(),
            &old_base_snapshot_1,
            &new_base_snapshot_1,
        );

        // The new deletion hunk (XXX) starts at buffer row 1 and the
        // modification hunk (ccc → CCC) now has a different
        // diff_base_byte_range, so the changed range spans both.
        let range = changed_range.unwrap();
        assert_eq!(range.to_point(&buffer), Point::new(1, 0)..Point::new(3, 0),);
        let base_range = base_text_changed_range.unwrap();
        assert_eq!(
            base_range.to_point(&new_base_snapshot_1),
            Point::new(1, 0)..Point::new(4, 0),
        );

        // --- Scenario 2: Base text changes to match the buffer (hunk disappears) ---
        //
        // Start fresh with a simple base text.
        let simple_base = "one\ntwo\nthree\n";
        let mut base_buf_2 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(100).unwrap(),
            simple_base.to_string(),
        );

        let buffer_text_2 = "one\nTWO\nthree\n";
        let buffer_2 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(2).unwrap(),
            buffer_text_2.to_string(),
        );

        let old_base_snapshot_2 = base_buf_2.snapshot().clone();
        let old_hunks_2 = compute_hunks(
            Some((Arc::from(simple_base), Rope::from(simple_base))),
            buffer_2.snapshot(),
            None,
            false,
        );

        // The base text is edited so "two" becomes "TWO", now matching the buffer.
        base_buf_2.edit([(4..7, "TWO")]);
        let new_base_str_2: Arc<str> = Arc::from(base_buf_2.text().as_str());
        let new_base_snapshot_2 = base_buf_2.snapshot();

        let new_hunks_2 = compute_hunks(
            Some((new_base_str_2.clone(), Rope::from(new_base_str_2.as_ref()))),
            buffer_2.snapshot(),
            None,
            false,
        );

        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &new_hunks_2,
            &old_hunks_2,
            &buffer_2.snapshot(),
            &buffer_2.snapshot(),
            &old_base_snapshot_2,
            &new_base_snapshot_2,
        );

        // The old modification hunk (two → TWO) is now gone because the
        // base text matches the buffer. The changed range covers where the
        // old hunk used to be.
        let range = changed_range.unwrap();
        assert_eq!(
            range.to_point(&buffer_2),
            Point::new(1, 0)..Point::new(2, 0),
        );
        let base_range = base_text_changed_range.unwrap();
        // The old hunk's diff_base_byte_range covered "two\n" (bytes 4..8).
        // anchor_after(4) is right-biased at the start of the deleted "two",
        // so after the edit replacing "two" with "TWO" it resolves past the
        // insertion to Point(1, 3).
        assert_eq!(
            base_range.to_point(&new_base_snapshot_2),
            Point::new(1, 3)..Point::new(2, 0),
        );

        // --- Scenario 3: Base text edit changes one hunk but not another ---
        //
        // Two modification hunks exist. Only one of them is resolved by
        // the base text change; the other remains identical.
        let base_3 = "aaa\nbbb\nccc\nddd\neee\n";
        let mut base_buf_3 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(101).unwrap(),
            base_3.to_string(),
        );

        let buffer_text_3 = "aaa\nBBB\nccc\nDDD\neee\n";
        let buffer_3 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(3).unwrap(),
            buffer_text_3.to_string(),
        );

        let old_base_snapshot_3 = base_buf_3.snapshot().clone();
        let old_hunks_3 = compute_hunks(
            Some((Arc::from(base_3), Rope::from(base_3))),
            buffer_3.snapshot(),
            None,
            false,
        );

        // Change "ddd" to "DDD" in the base text so that hunk disappears,
        // but "bbb" stays, so its hunk remains.
        base_buf_3.edit([(12..15, "DDD")]);
        let new_base_str_3: Arc<str> = Arc::from(base_buf_3.text().as_str());
        let new_base_snapshot_3 = base_buf_3.snapshot();

        let new_hunks_3 = compute_hunks(
            Some((new_base_str_3.clone(), Rope::from(new_base_str_3.as_ref()))),
            buffer_3.snapshot(),
            None,
            false,
        );

        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &new_hunks_3,
            &old_hunks_3,
            &buffer_3.snapshot(),
            &buffer_3.snapshot(),
            &old_base_snapshot_3,
            &new_base_snapshot_3,
        );

        // Only the second hunk (ddd → DDD) disappeared; the first hunk
        // (bbb → BBB) is unchanged, so the changed range covers only line 3.
        let range = changed_range.unwrap();
        assert_eq!(
            range.to_point(&buffer_3),
            Point::new(3, 0)..Point::new(4, 0),
        );
        let base_range = base_text_changed_range.unwrap();
        // anchor_after(12) is right-biased at the start of deleted "ddd",
        // so after the edit replacing "ddd" with "DDD" it resolves past
        // the insertion to Point(3, 3).
        assert_eq!(
            base_range.to_point(&new_base_snapshot_3),
            Point::new(3, 3)..Point::new(4, 0),
        );

        // --- Scenario 4: Both buffer and base text change simultaneously ---
        //
        // The buffer gains an edit that introduces a new hunk while the
        // base text also changes.
        let base_4 = "alpha\nbeta\ngamma\ndelta\n";
        let mut base_buf_4 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(102).unwrap(),
            base_4.to_string(),
        );

        let buffer_text_4 = "alpha\nBETA\ngamma\ndelta\n";
        let mut buffer_4 = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(4).unwrap(),
            buffer_text_4.to_string(),
        );

        let old_base_snapshot_4 = base_buf_4.snapshot().clone();
        let old_buffer_snapshot_4 = buffer_4.snapshot().clone();
        let old_hunks_4 = compute_hunks(
            Some((Arc::from(base_4), Rope::from(base_4))),
            buffer_4.snapshot(),
            None,
            false,
        );

        // Edit the buffer: change "delta" to "DELTA" (new modification hunk).
        buffer_4.edit_via_marked_text(
            &"
                alpha
                BETA
                gamma
                «DELTA»
            "
            .unindent(),
        );

        // Edit the base text: change "beta" to "BETA" (resolves that hunk).
        base_buf_4.edit([(6..10, "BETA")]);
        let new_base_str_4: Arc<str> = Arc::from(base_buf_4.text().as_str());
        let new_base_snapshot_4 = base_buf_4.snapshot();

        let new_hunks_4 = compute_hunks(
            Some((new_base_str_4.clone(), Rope::from(new_base_str_4.as_ref()))),
            buffer_4.snapshot(),
            None,
            false,
        );

        let DiffChanged {
            changed_range,
            base_text_changed_range,
            extended_range: _,
            base_text_changed: _,
        } = compare_hunks(
            &new_hunks_4,
            &old_hunks_4,
            &old_buffer_snapshot_4,
            &buffer_4.snapshot(),
            &old_base_snapshot_4,
            &new_base_snapshot_4,
        );

        // The old BETA hunk (line 1) is gone and a new DELTA hunk (line 3)
        // appeared, so the changed range spans from line 1 through line 4.
        let range = changed_range.unwrap();
        assert_eq!(
            range.to_point(&buffer_4),
            Point::new(1, 0)..Point::new(4, 0),
        );
        let base_range = base_text_changed_range.unwrap();
        // The old BETA hunk's base range started at byte 6 ("beta"). After
        // the base text edit replacing "beta" with "BETA", anchor_after(6)
        // resolves past the insertion to Point(1, 4).
        assert_eq!(
            base_range.to_point(&new_base_snapshot_4),
            Point::new(1, 4)..Point::new(4, 0),
        );
    }

    #[gpui::test(iterations = 100)]
    async fn test_patch_for_range_random(cx: &mut TestAppContext, mut rng: StdRng) {
        fn gen_line(rng: &mut StdRng) -> String {
            if rng.random_bool(0.2) {
                "\n".to_owned()
            } else {
                let c = rng.random_range('A'..='Z');
                format!("{c}{c}{c}\n")
            }
        }

        fn gen_text(rng: &mut StdRng, line_count: usize) -> String {
            (0..line_count).map(|_| gen_line(rng)).collect()
        }

        fn gen_edits_from(rng: &mut StdRng, base: &str) -> String {
            let mut old_lines: Vec<&str> = base.lines().collect();
            let mut result = String::new();

            while !old_lines.is_empty() {
                let unchanged_count = rng.random_range(0..=old_lines.len());
                for _ in 0..unchanged_count {
                    if old_lines.is_empty() {
                        break;
                    }
                    result.push_str(old_lines.remove(0));
                    result.push('\n');
                }

                if old_lines.is_empty() {
                    break;
                }

                let deleted_count = rng.random_range(0..=old_lines.len().min(3));
                for _ in 0..deleted_count {
                    if old_lines.is_empty() {
                        break;
                    }
                    old_lines.remove(0);
                }

                let minimum_added = if deleted_count == 0 { 1 } else { 0 };
                let added_count = rng.random_range(minimum_added..=3);
                for _ in 0..added_count {
                    result.push_str(&gen_line(rng));
                }
            }

            result
        }

        fn random_point_in_text(rng: &mut StdRng, lines: &[&str]) -> Point {
            if lines.is_empty() {
                return Point::zero();
            }
            let row = rng.random_range(0..lines.len() as u32);
            let line = lines[row as usize];
            let col = if line.is_empty() {
                0
            } else {
                rng.random_range(0..=line.len() as u32)
            };
            Point::new(row, col)
        }

        fn random_range_in_text(rng: &mut StdRng, lines: &[&str]) -> RangeInclusive<Point> {
            let start = random_point_in_text(rng, lines);
            let end = random_point_in_text(rng, lines);
            if start <= end {
                start..=end
            } else {
                end..=start
            }
        }

        fn points_in_range(range: &RangeInclusive<Point>, lines: &[&str]) -> Vec<Point> {
            let mut points = Vec::new();
            for row in range.start().row..=range.end().row {
                if row as usize >= lines.len() {
                    points.push(Point::new(row, 0));
                    continue;
                }
                let line = lines[row as usize];
                let start_col = if row == range.start().row {
                    range.start().column
                } else {
                    0
                };
                let end_col = if row == range.end().row {
                    range.end().column
                } else {
                    line.len() as u32
                };
                for col in start_col..=end_col {
                    points.push(Point::new(row, col));
                }
            }
            points
        }

        let rng = &mut rng;

        let line_count = rng.random_range(5..20);
        let base_text = gen_text(rng, line_count);
        let initial_buffer_text = gen_edits_from(rng, &base_text);

        let mut buffer = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(1).unwrap(),
            initial_buffer_text.clone(),
        );

        let diff = BufferDiffSnapshot::new_sync(&buffer, base_text.clone(), cx);

        let edit_count = rng.random_range(1..=5);
        for _ in 0..edit_count {
            let buffer_text = buffer.text();
            if buffer_text.is_empty() {
                buffer.edit([(0..0, gen_line(rng))]);
            } else {
                let lines: Vec<&str> = buffer_text.lines().collect();
                let start_row = rng.random_range(0..lines.len());
                let end_row = rng.random_range(start_row..=lines.len().min(start_row + 3));

                let start_col = if start_row < lines.len() {
                    rng.random_range(0..=lines[start_row].len())
                } else {
                    0
                };
                let end_col = if end_row < lines.len() {
                    rng.random_range(0..=lines[end_row].len())
                } else {
                    0
                };

                let start_offset = buffer
                    .point_to_offset(Point::new(start_row as u32, start_col as u32))
                    .min(buffer.len());
                let end_offset = buffer
                    .point_to_offset(Point::new(end_row as u32, end_col as u32))
                    .min(buffer.len());

                let (start, end) = if start_offset <= end_offset {
                    (start_offset, end_offset)
                } else {
                    (end_offset, start_offset)
                };

                let new_text = if rng.random_bool(0.3) {
                    String::new()
                } else {
                    let line_count = rng.random_range(0..=2);
                    gen_text(rng, line_count)
                };

                buffer.edit([(start..end, new_text)]);
            }
        }

        let buffer_snapshot = buffer.snapshot();

        let buffer_text = buffer_snapshot.text();
        let buffer_lines: Vec<&str> = buffer_text.lines().collect();
        let base_lines: Vec<&str> = base_text.lines().collect();

        let test_count = 10;
        for _ in 0..test_count {
            let range = random_range_in_text(rng, &buffer_lines);
            let points = points_in_range(&range, &buffer_lines);

            let optimized_patch = diff.patch_for_buffer_range(range.clone(), &buffer_snapshot);
            let naive_patch = diff.patch_for_buffer_range_naive(&buffer_snapshot);

            for point in points {
                let optimized_edit = optimized_patch.edit_for_old_position(point);
                let naive_edit = naive_patch.edit_for_old_position(point);

                assert_eq!(
                    optimized_edit,
                    naive_edit,
                    "patch_for_buffer_range mismatch at point {:?} in range {:?}\nbase_text: {:?}\ninitial_buffer: {:?}\ncurrent_buffer: {:?}",
                    point,
                    range,
                    base_text,
                    initial_buffer_text,
                    buffer_snapshot.text()
                );
            }
        }

        for _ in 0..test_count {
            let range = random_range_in_text(rng, &base_lines);
            let points = points_in_range(&range, &base_lines);

            let optimized_patch = diff.patch_for_base_text_range(range.clone(), &buffer_snapshot);
            let naive_patch = diff.patch_for_base_text_range_naive(&buffer_snapshot);

            for point in points {
                let optimized_edit = optimized_patch.edit_for_old_position(point);
                let naive_edit = naive_patch.edit_for_old_position(point);

                assert_eq!(
                    optimized_edit,
                    naive_edit,
                    "patch_for_base_text_range mismatch at point {:?} in range {:?}\nbase_text: {:?}\ninitial_buffer: {:?}\ncurrent_buffer: {:?}",
                    point,
                    range,
                    base_text,
                    initial_buffer_text,
                    buffer_snapshot.text()
                );
            }
        }
    }

    #[gpui::test]
    async fn test_set_base_text_with_crlf(cx: &mut gpui::TestAppContext) {
        let base_text_crlf = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";
        let base_text_lf = "one\ntwo\nthree\nfour\nfive\n";
        assert_ne!(base_text_crlf.len(), base_text_lf.len());

        let buffer_text = "one\nTWO\nthree\nfour\nfive\n";
        let buffer = Buffer::new(
            ReplicaId::LOCAL,
            BufferId::new(1).unwrap(),
            buffer_text.to_string(),
        );
        let buffer_snapshot = buffer.snapshot();

        let diff = cx.new(|cx| BufferDiff::new(&buffer_snapshot, None, None, cx));
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some(Arc::from(base_text_crlf)), buffer_snapshot.clone(), cx)
        })
        .await;
        cx.run_until_parked();

        let snapshot = diff.update(cx, |diff, cx| diff.snapshot(cx));
        snapshot.buffer_point_to_base_text_range(Point::new(0, 0), &buffer_snapshot);
        snapshot.buffer_point_to_base_text_range(Point::new(1, 0), &buffer_snapshot);
    }
}
