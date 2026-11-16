use super::*;
use buffer_diff::{BufferDiff, DiffHunkStatus, FileStatus};

#[derive(Clone)]
struct ResolvedDiffHunk {
    buffer_range: Range<text::Anchor>,
    diff_base_byte_range: Range<usize>,
}

#[derive(Clone)]
struct ResolvedDiffHunks {
    diff: Entity<BufferDiff>,
    buffer: Option<Entity<Buffer>>,
    hunks: Vec<ResolvedDiffHunk>,
}

pub trait DiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;

    fn render_hunk_as_staged(&self, status: &DiffHunkStatus, _cx: &App) -> bool {
        !status.has_secondary_hunk()
    }
}

pub struct DefaultDiffHunkRenderer;

impl DiffHunkRenderer for DefaultDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        render_diff_hunk_controls(
            row,
            status,
            hunk_range,
            is_created_file,
            line_height,
            editor,
            window,
            cx,
        )
    }
}

pub struct HiddenDiffHunkRenderer;

impl DiffHunkRenderer for HiddenDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }
}

pub struct HiddenUnstagedDiffHunkRenderer;

impl DiffHunkRenderer for HiddenUnstagedDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        _row: u32,
        _status: &DiffHunkStatus,
        _hunk_range: Range<Anchor>,
        _is_created_file: bool,
        _line_height: Pixels,
        _editor: &Entity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        gpui::Empty.into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DisplayDiffHunk {
    Folded {
        display_row: DisplayRow,
    },
    Unfolded {
        is_created_file: bool,
        diff_base_byte_range: Range<usize>,
        display_row_range: Range<DisplayRow>,
        multi_buffer_range: Range<Anchor>,
        status: DiffHunkStatus,
        word_diffs: Vec<Range<MultiBufferOffset>>,
    },
}

/// Represents a diff review button indicator that shows up when hovering over lines in the gutter
/// in diff view mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhantomDiffReviewIndicator {
    /// The starting anchor of the selection (or the only row if not dragging).
    pub(super) start: Anchor,
    /// The ending anchor of the selection. Equal to start_anchor for single-line selection.
    pub(super) end: Anchor,
    /// There's a small debounce between hovering over the line and showing the indicator.
    /// We don't want to show the indicator when moving the mouse from editor to e.g. project panel.
    pub(super) is_active: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DiffReviewDragState {
    start_anchor: Anchor,
    current_anchor: Anchor,
}

/// Identifies a specific hunk in the diff buffer.
/// Used as a key to group comments by their location.
#[derive(Clone, Debug)]
pub(super) struct DiffHunkKey {
    /// The file path (relative to worktree) this hunk belongs to.
    pub(super) file_path: Arc<util::rel_path::RelPath>,
    /// An anchor at the start of the hunk. This tracks position as the buffer changes.
    pub(super) hunk_start_anchor: Anchor,
}

/// A review comment stored locally before being sent to the Agent panel.
#[derive(Clone)]
pub(super) struct StoredReviewComment {
    /// Unique identifier for this comment (for edit/delete operations).
    pub(super) id: usize,
    /// The comment text entered by the user.
    pub(super) comment: String,
    /// Anchors for the code range being reviewed.
    pub(super) range: Range<Anchor>,
    // Keep source coordinates independent of whether deleted rows are displayed.
    source_snapshot: MultiBufferSnapshot,
    /// Whether this comment is currently being edited inline.
    pub(super) is_editing: bool,
}

/// Represents an active diff review overlay that appears when clicking the "Add Review" button.
pub(super) struct DiffReviewOverlay {
    pub(super) anchor_range: Range<Anchor>,
    source_snapshot: MultiBufferSnapshot,
    /// The block ID for the overlay.
    pub(super) block_id: CustomBlockId,
    /// The editor entity for the review input.
    pub(super) prompt_editor: Entity<Editor>,
    /// The hunk key this overlay belongs to.
    pub(super) hunk_key: DiffHunkKey,
    /// Whether the comments section is expanded.
    pub(super) comments_expanded: bool,
    /// Editors for comments currently being edited inline.
    /// Key: comment ID, Value: Editor entity for inline editing.
    pub(super) inline_edit_editors: HashMap<usize, Entity<Editor>>,
    /// Subscriptions for inline edit editors' action handlers.
    /// Key: comment ID, Value: Subscription keeping the Newline action handler alive.
    pub(super) inline_edit_subscriptions: HashMap<usize, Subscription>,
    /// The current user's avatar URI for display in comment rows.
    pub(super) user_avatar_uri: Option<SharedUri>,
    /// Subscription to keep the action handler alive.
    _subscription: Subscription,
}

impl DiffReviewDragState {
    pub(super) fn row_range(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> std::ops::RangeInclusive<DisplayRow> {
        let start = self.start_anchor.to_display_point(snapshot).row();
        let current = self.current_anchor.to_display_point(snapshot).row();

        (start..=current).sorted()
    }
}

impl StoredReviewComment {
    fn new(
        id: usize,
        comment: String,
        anchor_range: Range<Anchor>,
        source_snapshot: MultiBufferSnapshot,
    ) -> Self {
        Self {
            id,
            comment,
            range: anchor_range,
            source_snapshot,
            is_editing: false,
        }
    }
}

impl Editor {
    pub fn diff_hunks_in_ranges<'a>(
        &'a self,
        ranges: &'a [Range<Anchor>],
        buffer: &'a MultiBufferSnapshot,
    ) -> impl 'a + Iterator<Item = MultiBufferDiffHunk> {
        ranges.iter().flat_map(move |range| {
            let end_excerpt = buffer.excerpt_containing(range.end..range.end);
            let range = range.to_point(buffer);
            let mut peek_end = range.end;
            if range.end.row < buffer.max_row().0 {
                peek_end = Point::new(range.end.row + 1, 0);
            }
            buffer
                .diff_hunks_in_range(range.start..peek_end)
                .filter(move |hunk| {
                    if let Some((_, excerpt_range)) = &end_excerpt
                        && let Some(end_anchor) =
                            buffer.anchor_in_excerpt(excerpt_range.context.end)
                        && let Some(hunk_end_anchor) =
                            buffer.anchor_in_excerpt(hunk.excerpt_range.context.end)
                        && hunk_end_anchor.cmp(&end_anchor, buffer).is_gt()
                    {
                        false
                    } else {
                        true
                    }
                })
        })
    }

    fn resolve_diff_hunks(
        &self,
        hunks: Vec<MultiBufferDiffHunk>,
        cx: &App,
    ) -> Vec<ResolvedDiffHunks> {
        let multibuffer = self.buffer().read(cx);
        let chunk_by = hunks.into_iter().chunk_by(|hunk| hunk.buffer_id);
        let mut resolved = Vec::new();

        for (source_buffer_id, hunks) in &chunk_by {
            let Some(diff) = multibuffer.diff_for(source_buffer_id) else {
                continue;
            };
            let diff_snapshot = diff.read(cx).snapshot(cx);
            let main_buffer_id = diff_snapshot.buffer_id();
            let buffer = multibuffer.buffer(main_buffer_id).or_else(|| {
                self.project
                    .as_ref()
                    .and_then(|project| project.read(cx).buffer_for_id(main_buffer_id, cx))
            });
            let mut resolved_hunks = Vec::new();

            for hunk in hunks {
                if hunk.buffer_id == main_buffer_id {
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                    });
                } else {
                    let diff_base_byte_range =
                        hunk.diff_base_byte_range.start.0..hunk.diff_base_byte_range.end.0;
                    let Some(hunk) = diff_snapshot
                        .hunks_intersecting_base_text_range(
                            diff_base_byte_range.clone(),
                            diff_snapshot.buffer_snapshot(),
                        )
                        .find(|hunk| hunk.diff_base_byte_range == diff_base_byte_range)
                    else {
                        continue;
                    };
                    resolved_hunks.push(ResolvedDiffHunk {
                        buffer_range: hunk.buffer_range,
                        diff_base_byte_range: hunk.diff_base_byte_range,
                    });
                }
            }

            if !resolved_hunks.is_empty() {
                resolved.push(ResolvedDiffHunks {
                    diff,
                    buffer,
                    hunks: resolved_hunks,
                });
            }
        }

        resolved
    }

    pub fn diff_hunk_renderer(&self) -> Arc<dyn DiffHunkRenderer> {
        self.diff_hunk_renderer
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultDiffHunkRenderer))
    }

    pub fn set_diff_hunk_renderer(
        &mut self,
        renderer: Option<Arc<dyn DiffHunkRenderer>>,
        cx: &mut Context<Self>,
    ) {
        let had_renderer = self.diff_hunk_renderer.is_some();
        let has_renderer = renderer.is_some();
        self.diff_hunk_renderer = renderer;

        if had_renderer && !has_renderer {
            self.buffer
                .update(cx, |buffer, cx| buffer.set_all_diff_hunks_collapsed(cx));
        }

        cx.notify();
    }

    pub(crate) fn set_diff_hunk_action_target(&mut self, target: Option<WeakEntity<Editor>>) {
        self.diff_hunk_action_target = target;
    }
    pub fn expand_selected_diff_hunks(&mut self, cx: &mut Context<Self>) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.buffer
            .update(cx, |buffer, cx| buffer.expand_diff_hunks(ranges, cx))
    }
    pub fn restore_diff(
        &mut self,
        _: &crate::actions::RestoreDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_hunk_action_target.is_none() && self.read_only(cx) {
            return;
        }
        let selections = self
            .selections
            .all(&self.display_snapshot(cx))
            .into_iter()
            .map(|s| s.range())
            .collect();
        self.restore_hunks_in_ranges(selections, window, cx);
    }

    pub fn can_restore_selected_hunks(&self, window: &Window, cx: &mut App) -> bool {
        let read_only = match &self.diff_hunk_action_target {
            Some(target) => target
                .upgrade()
                .is_none_or(|target| target.read(cx).read_only(cx)),
            None => self.read_only(cx),
        };
        if read_only {
            return false;
        }

        let snapshot = self.snapshot(window, cx);
        let ranges = self
            .selections
            .all(&snapshot.display_snapshot)
            .into_iter()
            .map(|selection| selection.range());
        let hunks = snapshot.hunks_for_ranges(ranges);
        self.resolve_diff_hunks(hunks, cx).iter().any(|hunks| {
            hunks.buffer.as_ref().is_some_and(|buffer| {
                buffer.read(cx).capability() == language::Capability::ReadWrite
            }) && hunks
                .diff
                .read(cx)
                .operations()
                .is_some_and(|operations| operations.supports_restore())
                && hunks.hunks.iter().any(|hunk| {
                    !(hunk.diff_base_byte_range.is_empty()
                        && hunk.buffer_range.start.is_min()
                        && hunk.buffer_range.end.is_max())
                })
        })
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        self.addons
            .iter()
            .find_map(|(_, addon)| addon.override_status_for_buffer_id(buffer_id, cx))
    }

    pub fn go_to_hunk_before_or_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        direction: Direction,
        wrap_around: bool,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let row = if direction == Direction::Next {
            self.hunk_after_position(snapshot, position, wrap_around)
                .map(|hunk| hunk.row_range.start)
        } else {
            self.hunk_before_position(snapshot, position, wrap_around)
        };

        if let Some(row) = row {
            let destination = Point::new(row.0, 0);
            let autoscroll = Autoscroll::center();

            self.unfold_ranges(&[destination..destination], false, false, cx);
            self.change_selections(SelectionEffects::scroll(autoscroll), window, cx, |s| {
                s.select_ranges([destination..destination]);
            });
        }
    }

    pub fn set_expand_all_diff_hunks(&mut self, cx: &mut App) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_all_diff_hunks_expanded(cx);
        });
    }

    pub fn expand_all_diff_hunks(
        &mut self,
        _: &ExpandAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn show_diff_review_overlay(
        &mut self,
        display_range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Range { start, end } = display_range.sorted();

        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let editor_snapshot = self.snapshot(window, cx);

        // Convert display rows to multibuffer points
        let start_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(start.as_display_point(), Bias::Left);
        let end_point = editor_snapshot
            .display_snapshot
            .display_point_to_point(end.as_display_point(), Bias::Left);
        let end_multi_buffer_row = MultiBufferRow(end_point.row);

        // Create anchor range for the selected lines (start of first line to end of last line)
        let line_end = Point::new(
            end_point.row,
            buffer_snapshot.line_len(end_multi_buffer_row),
        );
        let anchor_range =
            buffer_snapshot.anchor_after(start_point)..buffer_snapshot.anchor_before(line_end);

        // Compute the hunk key for this display row
        let file_path = buffer_snapshot
            .file_at(start_point)
            .map(|file: &Arc<dyn language::File>| file.path().clone())
            .unwrap_or_else(|| Arc::from(util::rel_path::RelPath::empty()));
        let hunk_start_anchor = buffer_snapshot.anchor_before(start_point);
        let new_hunk_key = DiffHunkKey {
            file_path,
            hunk_start_anchor,
        };

        // Check if we already have an overlay for this hunk
        if let Some(existing_overlay) = self.diff_review_overlays.iter().find(|overlay| {
            Self::hunk_keys_match(&overlay.hunk_key, &new_hunk_key, &buffer_snapshot)
        }) {
            // Just focus the existing overlay's prompt editor
            let focus_handle = existing_overlay.prompt_editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
            return;
        }

        // Dismiss overlays that have no comments for their hunks
        self.dismiss_overlays_without_comments(cx);

        // Get the current user's avatar URI from the project's user_store
        let user_avatar_uri = self.project.as_ref().and_then(|project| {
            let user_store = project.read(cx).user_store();
            user_store
                .read(cx)
                .current_user()
                .map(|user| user.avatar_uri.clone())
        });

        // Create anchor at the end of the last row so the block appears immediately below it
        // Use multibuffer coordinates for anchor creation
        let line_len = buffer_snapshot.line_len(end_multi_buffer_row);
        let anchor = buffer_snapshot.anchor_after(Point::new(end_multi_buffer_row.0, line_len));

        // Use the hunk key we already computed
        let hunk_key = new_hunk_key;

        // Create the prompt editor for the review input
        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a review comment...", window, cx);
            editor
        });

        // Register the Newline action on the prompt editor to submit the review
        let parent_editor = cx.entity().downgrade();
        let subscription = prompt_editor.update(cx, |prompt_editor, _cx| {
            prompt_editor.register_action({
                let parent_editor = parent_editor.clone();
                move |_: &crate::actions::Newline, window, cx| {
                    if let Some(editor) = parent_editor.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.submit_diff_review_comment(window, cx);
                        });
                    }
                }
            })
        });

        // Calculate initial height based on existing comments for this hunk
        let initial_height = self.calculate_overlay_height(&hunk_key, true, &buffer_snapshot);

        // Create the overlay block
        let prompt_editor_for_render = prompt_editor.clone();
        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let block = BlockProperties {
            style: BlockStyle::Sticky,
            placement: BlockPlacement::Below(anchor),
            height: Some(initial_height),
            render: Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor_for_render,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            }),
            priority: 0,
        };

        let block_ids = self.insert_blocks([block], None, cx);
        let Some(block_id) = block_ids.into_iter().next() else {
            log::error!("Failed to insert diff review overlay block");
            return;
        };

        self.diff_review_overlays.push(DiffReviewOverlay {
            anchor_range,
            source_snapshot: buffer_snapshot,
            block_id,
            prompt_editor: prompt_editor.clone(),
            hunk_key,
            comments_expanded: true,
            inline_edit_editors: HashMap::default(),
            inline_edit_subscriptions: HashMap::default(),
            user_avatar_uri,
            _subscription: subscription,
        });

        // Focus the prompt editor
        let focus_handle = prompt_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);

        cx.notify();
    }

    /// Stores the diff review comment locally.
    /// Comments are stored per-hunk and can later be batch-submitted to the Agent panel.
    pub fn submit_diff_review_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Find the overlay that currently has focus
        let overlay_index = self
            .diff_review_overlays
            .iter()
            .position(|overlay| overlay.prompt_editor.focus_handle(cx).is_focused(window));
        let Some(overlay_index) = overlay_index else {
            return;
        };
        let overlay = &self.diff_review_overlays[overlay_index];

        let comment_text = overlay.prompt_editor.read(cx).text(cx).trim().to_string();
        if comment_text.is_empty() {
            return;
        }

        let anchor_range = overlay.anchor_range.clone();
        let hunk_key = overlay.hunk_key.clone();

        self.add_review_comment(hunk_key.clone(), comment_text, anchor_range, cx);

        // Clear the prompt editor but keep the overlay open
        if let Some(overlay) = self.diff_review_overlays.get(overlay_index) {
            overlay.prompt_editor.update(cx, |editor, cx| {
                editor.clear(window, cx);
            });
        }

        // Refresh the overlay to update the block height for the new comment
        self.refresh_diff_review_overlay_height(&hunk_key, window, cx);

        cx.notify();
    }

    /// Returns the prompt editor for the diff review overlay, if one is active.
    /// This is primarily used for testing.
    pub fn diff_review_prompt_editor(&self) -> Option<&Entity<Editor>> {
        self.diff_review_overlays
            .first()
            .map(|overlay| &overlay.prompt_editor)
    }

    pub fn diff_review_ranges(&self, cx: &App) -> impl Iterator<Item = Range<Anchor>> + use<> {
        let current = self.buffer.read(cx).snapshot(cx);
        // A comment can span files whose order has changed since it was written.
        let sources = self
            .stored_review_comments
            .iter()
            .flat_map(|(_, comments)| {
                comments
                    .iter()
                    .map(|comment| (&comment.source_snapshot, &comment.range))
            })
            .chain(
                self.diff_review_overlays
                    .iter()
                    .map(|overlay| (&overlay.source_snapshot, &overlay.anchor_range)),
            );
        let mut ranges = Vec::new();
        for (snapshot, range) in sources {
            for (buffer, range, deleted) in snapshot
                .range_to_buffer_ranges_with_deleted_hunks(range.clone())
                .chain(snapshot.range_to_buffer_ranges_with_deleted_hunks(range.end..range.end))
            {
                let anchors = if let Some(deleted) = deleted {
                    let Some(source) = deleted
                        .buffer_id()
                        .and_then(|id| snapshot.buffer_for_id(id))
                    else {
                        continue;
                    };
                    let anchor = deleted.text_anchor_in(source);
                    // Keep the insertion site when restored text no longer has a deleted hunk.
                    anchor.bias_left(source)..anchor.bias_right(source)
                } else {
                    buffer.anchor_before(range.start)..buffer.anchor_after(range.end)
                };
                if let Some(range) = current.anchor_range_in_buffer(anchors) {
                    ranges.push(range);
                }
            }
        }
        ranges.into_iter()
    }

    pub fn diff_review_text(&self, cx: &App) -> String {
        let current_snapshot = self.buffer.read(cx).snapshot(cx);
        let mut text = String::new();
        for (hunk, comments) in &self.stored_review_comments {
            for comment in comments {
                let snapshot = &comment.source_snapshot;
                let locations_start = text.len();
                // Review selections include the final row; source segments are half-open.
                let ranges = snapshot
                    .range_to_buffer_ranges_with_deleted_hunks(comment.range.clone())
                    .map(|(buffer, range, deleted)| (buffer, range, deleted, false))
                    .chain(
                        snapshot
                            .range_to_buffer_ranges_with_deleted_hunks(
                                comment.range.end..comment.range.end,
                            )
                            .take(1)
                            .map(|(buffer, range, deleted)| (buffer, range, deleted, true)),
                    )
                    .map(|(buffer, range, deleted, endpoint)| {
                        let source = deleted
                            .and_then(|anchor| anchor.buffer_id())
                            .and_then(|buffer_id| snapshot.buffer_for_id(buffer_id))
                            .unwrap_or(buffer);
                        (buffer, range, source, deleted.is_some(), endpoint)
                    })
                    .coalesce(|previous, next| {
                        let (
                            previous_buffer,
                            ref previous_range,
                            previous_source,
                            previous_deleted,
                            previous_endpoint,
                        ) = previous;
                        let (buffer, ref range, _, deleted, endpoint) = next;
                        if previous_buffer.remote_id() == buffer.remote_id()
                            && previous_deleted == deleted
                            && previous_range.end == range.start
                        {
                            Ok((
                                previous_buffer,
                                previous_range.start..range.end,
                                previous_source,
                                previous_deleted,
                                previous_endpoint || endpoint,
                            ))
                        } else {
                            Err((previous, next))
                        }
                    });
                for (original, range, source, deleted, includes_endpoint) in ranges {
                    let buffer = if deleted {
                        original
                    } else if let Some(buffer) =
                        current_snapshot.buffer_for_id(original.remote_id())
                    {
                        buffer
                    } else {
                        continue;
                    };
                    let start = original.anchor_after(range.start.0);
                    let end = original.anchor_before(range.end.0);
                    if !buffer.can_resolve(&start) || !buffer.can_resolve(&end) {
                        continue;
                    }
                    let start = start.to_point(buffer);
                    let end = end.to_point(buffer);
                    let (start, end) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    let path = current_snapshot
                        .buffer_for_id(source.remote_id())
                        .and_then(|buffer| buffer.file())
                        .or_else(|| source.file())
                        .map(|file| file.path().as_ref())
                        .unwrap_or(hunk.file_path.as_ref());
                    let end = if !includes_endpoint && start != end && end.column == 0 {
                        end.row
                    } else {
                        end.row + 1
                    };
                    let start = start.row + 1;
                    let side = if deleted { " (deleted)" } else { "" };
                    text.push_str(&format!("{path}:{start}-{end}{side}\n"));
                }
                if text.len() == locations_start {
                    continue;
                }
                text.push_str(&comment.comment);
                text.push_str("\n\n");
            }
        }
        text
    }

    /// Sets whether the comments section is expanded in the diff review overlay.
    /// This is primarily used for testing.
    pub fn set_diff_review_comments_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        for overlay in &mut self.diff_review_overlays {
            overlay.comments_expanded = expanded;
        }
        cx.notify();
    }

    /// Returns the total count of stored review comments across all hunks.
    pub(super) fn total_review_comment_count(&self) -> usize {
        self.stored_review_comments
            .iter()
            .map(|(_, v)| v.len())
            .sum()
    }

    /// Adds a new review comment to a specific hunk.
    pub(super) fn add_review_comment(
        &mut self,
        hunk_key: DiffHunkKey,
        comment: String,
        anchor_range: Range<Anchor>,
        cx: &mut Context<Self>,
    ) -> usize {
        let id = self.next_review_comment_id;
        self.next_review_comment_id += 1;

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let source_snapshot = self
            .diff_review_overlays
            .iter()
            .find(|overlay| overlay.anchor_range == anchor_range)
            .map(|overlay| overlay.source_snapshot.clone())
            .unwrap_or_else(|| snapshot.clone());
        let stored_comment = StoredReviewComment::new(id, comment, anchor_range, source_snapshot);
        let key_point = hunk_key.hunk_start_anchor.to_point(&snapshot);

        // Find existing entry for this hunk or add a new one
        if let Some((_, comments)) = self.stored_review_comments.iter_mut().find(|(k, _)| {
            k.file_path == hunk_key.file_path
                && k.hunk_start_anchor.to_point(&snapshot) == key_point
        }) {
            comments.push(stored_comment);
        } else {
            self.stored_review_comments
                .push((hunk_key, vec![stored_comment]));
        }

        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.total_review_comment_count(),
        });
        cx.notify();
        id
    }
    fn restore_diff_hunks(&mut self, hunks: Vec<ResolvedDiffHunks>, cx: &mut Context<Self>) {
        let mut revert_changes = Vec::new();
        for hunks in hunks {
            let Some(buffer) = hunks.buffer else {
                continue;
            };
            let diff_snapshot = hunks.diff.read(cx).snapshot(cx);
            let changes = hunks
                .hunks
                .into_iter()
                .filter_map(|hunk| {
                    if hunk.diff_base_byte_range == (0..0)
                        && hunk.buffer_range.start.is_min()
                        && hunk.buffer_range.end.is_max()
                    {
                        return None;
                    }
                    let original_text = diff_snapshot
                        .base_text()
                        .as_rope()
                        .slice(hunk.diff_base_byte_range.start..hunk.diff_base_byte_range.end);
                    Some((hunk.buffer_range, original_text))
                })
                .collect::<Vec<_>>();
            if !changes.is_empty() {
                revert_changes.push((buffer, changes));
            }
        }

        for (buffer, changes) in revert_changes {
            buffer.update(cx, |buffer, cx| {
                buffer.edit(
                    changes
                        .into_iter()
                        .map(|(range, text)| (range, text.to_string())),
                    None,
                    cx,
                );
            });
        }
    }

    pub(super) fn go_to_next_hunk(
        &mut self,
        _: &GoToHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Next,
            true,
            window,
            cx,
        );
    }

    pub(super) fn collapse_all_diff_hunks(
        &mut self,
        _: &CollapseAllDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.collapse_diff_hunks(vec![Anchor::Min..Anchor::Max], cx)
        });
    }

    pub fn toggle_all_diff_hunks(
        &mut self,
        _: &ToggleAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_any_expanded_diff_hunks(cx) {
            self.collapse_all_diff_hunks(&CollapseAllDiffHunks, window, cx);
        } else {
            self.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx);
        }
    }

    pub(super) fn toggle_selected_diff_hunks(
        &mut self,
        _: &ToggleSelectedDiffHunks,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges: Vec<_> = self
            .selections
            .disjoint_anchors()
            .iter()
            .map(|s| s.range())
            .collect();
        self.toggle_diff_hunks_in_ranges(ranges, cx);
    }

    pub(super) fn show_diff_review_button(&self) -> bool {
        self.show_diff_review_button
    }

    pub(super) fn render_diff_review_button(
        &self,
        display_row: DisplayRow,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_color = cx.theme().colors().text;
        let icon_color = cx.theme().colors().icon_accent;

        h_flex()
            .id("diff_review_button")
            .cursor_pointer()
            .w(width - px(1.))
            .h(relative(0.9))
            .justify_center()
            .rounded_sm()
            .border_1()
            .border_color(text_color.opacity(0.1))
            .bg(text_color.opacity(0.15))
            .hover(|s| {
                s.bg(icon_color.opacity(0.4))
                    .border_color(icon_color.opacity(0.5))
            })
            .child(Icon::new(IconName::Plus).size(IconSize::Small))
            .tooltip(Tooltip::text("Add Review (drag to select multiple lines)"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |editor, _event: &gpui::MouseDownEvent, window, cx| {
                    editor.start_diff_review_drag(display_row, window, cx);
                }),
            )
    }

    pub(super) fn start_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(DisplayPoint::new(display_row, 0), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        self.diff_review_drag_state = Some(DiffReviewDragState {
            start_anchor: anchor,
            current_anchor: anchor,
        });
        cx.notify();
    }

    pub(super) fn update_diff_review_drag(
        &mut self,
        display_row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_review_drag_state.is_none() {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let point = snapshot
            .display_snapshot
            .display_point_to_point(display_row.as_display_point(), Bias::Left);
        let anchor = snapshot.buffer_snapshot().anchor_before(point);
        if let Some(drag_state) = &mut self.diff_review_drag_state {
            drag_state.current_anchor = anchor;
            cx.notify();
        }
    }

    pub(super) fn end_diff_review_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag_state) = self.diff_review_drag_state.take() {
            let snapshot = self.snapshot(window, cx);
            let range = drag_state.row_range(&snapshot.display_snapshot);
            self.show_diff_review_overlay(*range.start()..*range.end(), window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_diff_review_drag(&mut self, cx: &mut Context<Self>) {
        self.diff_review_drag_state = None;
        cx.notify();
    }

    /// Dismisses all diff review overlays.
    pub(super) fn dismiss_all_diff_review_overlays(&mut self, cx: &mut Context<Self>) {
        if self.diff_review_overlays.is_empty() {
            return;
        }
        let block_ids: HashSet<_> = self
            .diff_review_overlays
            .drain(..)
            .map(|overlay| overlay.block_id)
            .collect();
        self.remove_blocks(block_ids, None, cx);
        cx.notify();
    }

    /// Action handler for SubmitDiffReviewComment.
    pub(super) fn submit_diff_review_comment_action(
        &mut self,
        _: &SubmitDiffReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_diff_review_comment(window, cx);
    }

    /// Returns comments for a specific hunk, ordered by creation time.
    pub(super) fn comments_for_hunk<'a>(
        &'a self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> &'a [StoredReviewComment] {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, comments)| comments.as_slice())
            .unwrap_or(&[])
    }

    /// Returns the count of comments for a specific hunk.
    pub(super) fn hunk_comment_count(
        &self,
        key: &DiffHunkKey,
        snapshot: &MultiBufferSnapshot,
    ) -> usize {
        let key_point = key.hunk_start_anchor.to_point(snapshot);
        self.stored_review_comments
            .iter()
            .find(|(k, _)| {
                k.file_path == key.file_path && k.hunk_start_anchor.to_point(snapshot) == key_point
            })
            .map(|(_, v)| v.len())
            .unwrap_or(0)
    }

    /// Removes a review comment by ID from any hunk.
    pub(super) fn remove_review_comment(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(index) = comments.iter().position(|c| c.id == id) {
                comments.remove(index);
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Updates a review comment's text by ID.
    pub(super) fn update_review_comment(
        &mut self,
        id: usize,
        new_comment: String,
        cx: &mut Context<Self>,
    ) -> bool {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.comment = new_comment;
                comment.is_editing = false;
                cx.emit(EditorEvent::ReviewCommentsChanged {
                    total_count: self.total_review_comment_count(),
                });
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Sets a comment's editing state.
    pub(super) fn set_comment_editing(
        &mut self,
        id: usize,
        is_editing: bool,
        cx: &mut Context<Self>,
    ) {
        for (_, comments) in self.stored_review_comments.iter_mut() {
            if let Some(comment) = comments.iter_mut().find(|c| c.id == id) {
                comment.is_editing = is_editing;
                cx.notify();
                return;
            }
        }
    }

    /// Removes review comments whose anchors are no longer valid or whose
    /// associated diff hunks no longer exist.
    ///
    /// This should be called when the buffer changes to prevent orphaned comments
    /// from accumulating.
    pub(super) fn cleanup_orphaned_review_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let original_count = self.total_review_comment_count();

        // Remove comments with invalid hunk anchors
        self.stored_review_comments
            .retain(|(hunk_key, _)| hunk_key.hunk_start_anchor.is_valid(&snapshot));

        // Also clean up individual comments with invalid anchor ranges
        for (_, comments) in &mut self.stored_review_comments {
            comments.retain(|comment| {
                comment.range.start.is_valid(&snapshot) && comment.range.end.is_valid(&snapshot)
            });
        }

        // Remove empty hunk entries
        self.stored_review_comments
            .retain(|(_, comments)| !comments.is_empty());

        let new_count = self.total_review_comment_count();
        if new_count != original_count {
            cx.emit(EditorEvent::ReviewCommentsChanged {
                total_count: new_count,
            });
            cx.notify();
        }
    }

    /// Toggles the expanded state of the comments section in the overlay.
    pub(super) fn toggle_review_comments_expanded(
        &mut self,
        _: &ToggleReviewCommentsExpanded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Find the overlay that currently has focus, or use the first one
        let overlay_info = self.diff_review_overlays.iter_mut().find_map(|overlay| {
            if overlay.prompt_editor.focus_handle(cx).is_focused(window) {
                overlay.comments_expanded = !overlay.comments_expanded;
                Some(overlay.hunk_key.clone())
            } else {
                None
            }
        });

        // If no focused overlay found, toggle the first one
        let hunk_key = overlay_info.or_else(|| {
            self.diff_review_overlays.first_mut().map(|overlay| {
                overlay.comments_expanded = !overlay.comments_expanded;
                overlay.hunk_key.clone()
            })
        });

        if let Some(hunk_key) = hunk_key {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
            cx.notify();
        }
    }

    /// Handles the EditReviewComment action - sets a comment into editing mode.
    pub(super) fn edit_review_comment(
        &mut self,
        action: &EditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_id = action.id;

        // Set the comment to editing mode
        self.set_comment_editing(comment_id, true, cx);

        // Find the overlay that contains this comment and create an inline editor if needed
        // First, find which hunk this comment belongs to
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        let snapshot = self.buffer.read(cx).snapshot(cx);
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    overlay.inline_edit_editors.entry(comment_id)
                {
                    // Find the comment text
                    let comment_text = self
                        .stored_review_comments
                        .iter()
                        .flat_map(|(_, comments)| comments)
                        .find(|c| c.id == comment_id)
                        .map(|c| c.comment.clone())
                        .unwrap_or_default();

                    // Create inline editor
                    let parent_editor = cx.entity().downgrade();
                    let inline_editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text(&*comment_text, window, cx);
                        // Select all text for easy replacement
                        editor.select_all(&crate::actions::SelectAll, window, cx);
                        editor
                    });

                    // Register the Newline action to confirm the edit
                    let subscription = inline_editor.update(cx, |inline_editor, _cx| {
                        inline_editor.register_action({
                            let parent_editor = parent_editor.clone();
                            move |_: &crate::actions::Newline, window, cx| {
                                if let Some(editor) = parent_editor.upgrade() {
                                    editor.update(cx, |editor, cx| {
                                        editor.confirm_edit_review_comment(comment_id, window, cx);
                                    });
                                }
                            }
                        })
                    });

                    // Store the subscription to keep the action handler alive
                    overlay
                        .inline_edit_subscriptions
                        .insert(comment_id, subscription);

                    // Focus the inline editor
                    let focus_handle = inline_editor.focus_handle(cx);
                    window.focus(&focus_handle, cx);

                    entry.insert(inline_editor);
                }
            }
        }

        cx.notify();
    }

    /// Confirms an inline edit of a review comment.
    pub(super) fn confirm_edit_review_comment(
        &mut self,
        comment_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Get the new text from the inline editor
        // Find the overlay containing this comment's inline editor
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        let new_text = hunk_key
            .as_ref()
            .and_then(|hunk_key| {
                self.diff_review_overlays
                    .iter()
                    .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            })
            .as_ref()
            .and_then(|overlay| overlay.inline_edit_editors.get(&comment_id))
            .map(|editor| editor.read(cx).text(cx).trim().to_string());

        if let Some(new_text) = new_text {
            if !new_text.is_empty() {
                self.update_review_comment(comment_id, new_text, cx);
            }
        }

        // Remove the inline editor and its subscription
        if let Some(hunk_key) = hunk_key {
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
        }

        // Clear editing state
        self.set_comment_editing(comment_id, false, cx);
    }

    /// Cancels an inline edit of a review comment.
    pub(super) fn cancel_edit_review_comment(
        &mut self,
        comment_id: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Find which hunk this comment belongs to
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        // Remove the inline editor and its subscription
        if let Some(hunk_key) = hunk_key {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            if let Some(overlay) = self
                .diff_review_overlays
                .iter_mut()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, &hunk_key, &snapshot))
            {
                overlay.inline_edit_editors.remove(&comment_id);
                overlay.inline_edit_subscriptions.remove(&comment_id);
            }
        }

        // Clear editing state
        self.set_comment_editing(comment_id, false, cx);
    }

    /// Action handler for ConfirmEditReviewComment.
    pub(super) fn confirm_edit_review_comment_action(
        &mut self,
        action: &ConfirmEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_edit_review_comment(action.id, window, cx);
    }

    /// Action handler for CancelEditReviewComment.
    pub(super) fn cancel_edit_review_comment_action(
        &mut self,
        action: &CancelEditReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_edit_review_comment(action.id, window, cx);
    }

    /// Handles the DeleteReviewComment action - removes a comment.
    pub(super) fn delete_review_comment(
        &mut self,
        action: &DeleteReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Get the hunk key before removing the comment
        // Find the hunk key from the comment itself
        let comment_id = action.id;
        let hunk_key = self
            .stored_review_comments
            .iter()
            .find_map(|(key, comments)| {
                if comments.iter().any(|c| c.id == comment_id) {
                    Some(key.clone())
                } else {
                    None
                }
            });

        // Also get it from the overlay for refresh purposes
        let overlay_hunk_key = self
            .diff_review_overlays
            .first()
            .map(|o| o.hunk_key.clone());

        self.remove_review_comment(action.id, cx);

        // Refresh the overlay height after removing a comment
        if let Some(hunk_key) = hunk_key.or(overlay_hunk_key) {
            self.refresh_diff_review_overlay_height(&hunk_key, window, cx);
        }
    }
    pub fn apply_restore(
        &mut self,
        hunks: Vec<MultiBufferDiffHunk>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunks = self.resolve_diff_hunks(hunks, cx);
        if hunks.is_empty() {
            return;
        }
        if let Some(target) = self.diff_hunk_action_target.clone() {
            target
                .update(cx, |editor, cx| {
                    editor.restore_resolved_diff_hunks(hunks, window, cx);
                })
                .log_err();
            return;
        }
        self.restore_resolved_diff_hunks(hunks, window, cx);
    }

    fn restore_resolved_diff_hunks(
        &mut self,
        hunks: Vec<ResolvedDiffHunks>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let mut restorable_hunks = Vec::new();
        for hunks in hunks {
            let Some(operations) = hunks.diff.read(cx).operations() else {
                continue;
            };
            if !operations.supports_restore() {
                continue;
            }

            restorable_hunks.push(hunks);
        }
        if restorable_hunks.is_empty() {
            return;
        }
        self.transact(window, cx, |editor, window, cx| {
            editor.restore_diff_hunks(restorable_hunks, cx);
            let selections = editor
                .selections
                .all::<MultiBufferOffset>(&editor.display_snapshot(cx));
            editor.change_selections(
                SelectionEffects::no_scroll(),
                window,
                cx,
                |selections_state| {
                    selections_state.select(selections);
                },
            );
        });
    }

    pub(super) fn clear_expanded_diff_hunks(&mut self, cx: &mut Context<Self>) -> bool {
        self.buffer.update(cx, |buffer, cx| {
            let ranges = vec![Anchor::Min..Anchor::Max];
            if !buffer.all_diff_hunks_expanded()
                && buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx)
            {
                buffer.collapse_diff_hunks(ranges, cx);
                true
            } else {
                false
            }
        })
    }

    pub(super) fn has_any_expanded_diff_hunks(&self, cx: &App) -> bool {
        if self.buffer.read(cx).all_diff_hunks_expanded() {
            return true;
        }
        let ranges = vec![Anchor::Min..Anchor::Max];
        self.buffer
            .read(cx)
            .has_expanded_diff_hunks_in_ranges(&ranges, cx)
    }

    pub(super) fn toggle_single_diff_hunk(&mut self, range: Range<Anchor>, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.toggle_single_diff_hunk(range, cx);
        })
    }

    pub(super) fn apply_all_diff_hunks(
        &mut self,
        _: &ApplyAllDiffHunks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }

        let buffers = self.buffer.read(cx).all_buffers();
        for branch_buffer in buffers {
            branch_buffer.update(cx, |branch_buffer, cx| {
                branch_buffer.merge_into_base(Vec::new(), cx);
            });
        }

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }

    pub(super) fn apply_selected_diff_hunks(
        &mut self,
        _: &ApplyDiffHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let snapshot = self.snapshot(window, cx);
        let hunks = snapshot.hunks_for_ranges(
            self.selections
                .all(&snapshot.display_snapshot)
                .into_iter()
                .map(|selection| selection.range()),
        );
        let mut ranges_by_buffer = HashMap::default();
        self.transact(window, cx, |editor, _window, cx| {
            for hunk in hunks {
                if let Some(buffer) = editor.buffer.read(cx).buffer(hunk.buffer_id) {
                    ranges_by_buffer
                        .entry(buffer.clone())
                        .or_insert_with(Vec::new)
                        .push(hunk.buffer_range.to_offset(buffer.read(cx)));
                }
            }

            for (buffer, ranges) in ranges_by_buffer {
                buffer.update(cx, |buffer, cx| {
                    buffer.merge_into_base(ranges, cx);
                });
            }
        });

        if let Some(project) = self.project.clone() {
            self.save(
                SaveOptions {
                    format: true,
                    force_format: false,
                    autosave: false,
                },
                project,
                window,
                cx,
            )
            .detach_and_log_err(cx);
        }
    }
    pub(super) fn go_to_prev_hunk(
        &mut self,
        _: &GoToPreviousHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.snapshot(window, cx);
        let selection = self.selections.newest::<Point>(&snapshot.display_snapshot);
        self.go_to_hunk_before_or_after_position(
            &snapshot,
            selection.head(),
            Direction::Prev,
            true,
            window,
            cx,
        );
    }

    /// Calculates the appropriate block height for the diff review overlay.
    /// Height is in lines: 2 for input row, 1 for header when comments exist,
    /// and 2 lines per comment when expanded.
    pub(super) fn calculate_overlay_height(
        &self,
        hunk_key: &DiffHunkKey,
        comments_expanded: bool,
        snapshot: &MultiBufferSnapshot,
    ) -> u32 {
        let comment_count = self.hunk_comment_count(hunk_key, snapshot);
        let base_height: u32 = 2; // Input row with avatar and buttons

        if comment_count == 0 {
            base_height
        } else if comments_expanded {
            // Header (1 line) + 2 lines per comment
            base_height + 1 + (comment_count as u32 * 2)
        } else {
            // Just header when collapsed
            base_height + 1
        }
    }
    pub fn restore_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let hunks = self.diff_hunks_in_ranges(&ranges, &snapshot).collect();
        self.apply_restore(hunks, window, cx);
    }

    fn toggle_diff_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Editor>,
    ) {
        self.buffer.update(cx, |buffer, cx| {
            let expand = !buffer.has_expanded_diff_hunks_in_ranges(&ranges, cx);
            buffer.expand_or_collapse_diff_hunks(ranges, expand, cx);
        })
    }
    fn restore_hunks_in_ranges(
        &mut self,
        ranges: Vec<Range<Point>>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let hunks = self.snapshot(window, cx).hunks_for_ranges(ranges);
        self.apply_restore(hunks, window, cx);
    }
    fn hunk_after_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferDiffHunk> {
        let result = snapshot
            .buffer_snapshot()
            .diff_hunks_in_range(position..snapshot.buffer_snapshot().max_point())
            .find(|hunk| hunk.row_range.start.0 > position.row);

        if wrap_around {
            result.or_else(|| {
                snapshot
                    .buffer_snapshot()
                    .diff_hunks_in_range(Point::zero()..position)
                    .find(|hunk| hunk.row_range.end.0 < position.row)
            })
        } else {
            result
        }
    }

    fn hunk_before_position(
        &mut self,
        snapshot: &EditorSnapshot,
        position: Point,
        wrap_around: bool,
    ) -> Option<MultiBufferRow> {
        let result = snapshot.buffer_snapshot().diff_hunk_before(position);

        if wrap_around {
            result.or_else(|| snapshot.buffer_snapshot().diff_hunk_before(Point::MAX))
        } else {
            result
        }
    }

    /// Dismisses overlays that have no comments stored for their hunks.
    /// Keeps overlays that have at least one comment.
    fn dismiss_overlays_without_comments(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);

        // First, compute which overlays have comments (to avoid borrow issues with retain)
        let overlays_with_comments: Vec<bool> = self
            .diff_review_overlays
            .iter()
            .map(|overlay| self.hunk_comment_count(&overlay.hunk_key, &snapshot) > 0)
            .collect();

        // Now collect block IDs to remove and retain overlays
        let mut block_ids_to_remove = HashSet::default();
        let mut index = 0;
        self.diff_review_overlays.retain(|overlay| {
            let has_comments = overlays_with_comments[index];
            index += 1;
            if !has_comments {
                block_ids_to_remove.insert(overlay.block_id);
            }
            has_comments
        });

        if !block_ids_to_remove.is_empty() {
            self.remove_blocks(block_ids_to_remove, None, cx);
            cx.notify();
        }
    }

    /// Refreshes the diff review overlay block to update its height and render function.
    /// Uses resize_blocks and replace_blocks to avoid visual flicker from remove+insert.
    fn refresh_diff_review_overlay_height(
        &mut self,
        hunk_key: &DiffHunkKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Extract all needed data from overlay first to avoid borrow conflicts
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let (comments_expanded, block_id, prompt_editor) = {
            let Some(overlay) = self
                .diff_review_overlays
                .iter()
                .find(|overlay| Self::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot))
            else {
                return;
            };

            (
                overlay.comments_expanded,
                overlay.block_id,
                overlay.prompt_editor.clone(),
            )
        };

        // Calculate new height
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let new_height = self.calculate_overlay_height(hunk_key, comments_expanded, &snapshot);

        // Update the block height using resize_blocks (avoids flicker)
        let mut heights = HashMap::default();
        heights.insert(block_id, new_height);
        self.resize_blocks(heights, None, cx);

        // Update the render function using replace_blocks (avoids flicker)
        let hunk_key_for_render = hunk_key.clone();
        let editor_handle = cx.entity().downgrade();
        let render: Arc<dyn Fn(&mut BlockContext) -> AnyElement + Send + Sync> =
            Arc::new(move |cx| {
                Self::render_diff_review_overlay(
                    &prompt_editor,
                    &hunk_key_for_render,
                    &editor_handle,
                    cx,
                )
            });

        let mut renderers = HashMap::default();
        renderers.insert(block_id, render);
        self.replace_blocks(renderers, None, cx);
    }

    /// Compares two DiffHunkKeys for equality by resolving their anchors.
    fn hunk_keys_match(a: &DiffHunkKey, b: &DiffHunkKey, snapshot: &MultiBufferSnapshot) -> bool {
        a.file_path == b.file_path
            && a.hunk_start_anchor.to_point(snapshot) == b.hunk_start_anchor.to_point(snapshot)
    }

    fn render_diff_review_overlay(
        prompt_editor: &Entity<Editor>,
        hunk_key: &DiffHunkKey,
        editor_handle: &WeakEntity<Editor>,
        cx: &mut BlockContext,
    ) -> AnyElement {
        fn format_line_ranges(ranges: &[(u32, u32)]) -> Option<String> {
            if ranges.is_empty() {
                return None;
            }
            let formatted: Vec<String> = ranges
                .iter()
                .map(|(start, end)| {
                    let start_line = start + 1;
                    let end_line = end + 1;
                    if start_line == end_line {
                        format!("Line {start_line}")
                    } else {
                        format!("Lines {start_line}-{end_line}")
                    }
                })
                .collect();
            // Don't show label for single line in single excerpt
            if ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                return None;
            }
            Some(formatted.join(" ⋯ "))
        }

        let theme = cx.theme();
        let colors = theme.colors();

        let (comments, comments_expanded, inline_editors, user_avatar_uri, line_ranges) =
            editor_handle
                .upgrade()
                .map(|editor| {
                    let editor = editor.read(cx);
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let comments = editor.comments_for_hunk(hunk_key, &snapshot).to_vec();
                    let (expanded, editors, avatar_uri, line_ranges) = editor
                        .diff_review_overlays
                        .iter()
                        .find(|overlay| {
                            Editor::hunk_keys_match(&overlay.hunk_key, hunk_key, &snapshot)
                        })
                        .map(|o| {
                            let start_point = o.anchor_range.start.to_point(&snapshot);
                            let end_point = o.anchor_range.end.to_point(&snapshot);
                            // Get line ranges per excerpt to detect discontinuities
                            let buffer_ranges =
                                snapshot.range_to_buffer_ranges(start_point..end_point);
                            let ranges: Vec<(u32, u32)> = buffer_ranges
                                .iter()
                                .map(|(buffer_snapshot, range, _)| {
                                    let start = buffer_snapshot.offset_to_point(range.start.0).row;
                                    let end = buffer_snapshot.offset_to_point(range.end.0).row;
                                    (start, end)
                                })
                                .collect();
                            (
                                o.comments_expanded,
                                o.inline_edit_editors.clone(),
                                o.user_avatar_uri.clone(),
                                if ranges.is_empty() {
                                    None
                                } else {
                                    Some(ranges)
                                },
                            )
                        })
                        .unwrap_or((true, HashMap::default(), None, None));
                    (comments, expanded, editors, avatar_uri, line_ranges)
                })
                .unwrap_or((Vec::new(), true, HashMap::default(), None, None));

        let comment_count = comments.len();
        let avatar_size = px(20.);
        let action_icon_size = IconSize::XSmall;

        v_flex()
            .w_full()
            .bg(colors.editor_background)
            .border_b_1()
            .border_color(colors.border)
            .px_2()
            .pb_2()
            .gap_2()
            // Line range indicator (only shown for multi-line selections or multiple excerpts)
            .when_some(line_ranges, |el, ranges| {
                let label = format_line_ranges(&ranges);
                if let Some(label) = label {
                    el.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                } else {
                    el
                }
            })
            // Top row: editable input with user's avatar
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .bg(colors.surface_background)
                    .child(
                        div()
                            .size(avatar_size)
                            .flex_shrink_0()
                            .rounded_full()
                            .overflow_hidden()
                            .child(if let Some(ref avatar_uri) = user_avatar_uri {
                                Avatar::new(avatar_uri.clone())
                                    .size(avatar_size)
                                    .into_any_element()
                            } else {
                                Icon::new(IconName::Person)
                                    .size(IconSize::Small)
                                    .color(ui::Color::Muted)
                                    .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .bg(colors.editor_background)
                            .px_2()
                            .py_1()
                            .child(prompt_editor.clone()),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_1()
                            .child(
                                IconButton::new("diff-review-close", IconName::Close)
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .tooltip(Tooltip::text("Close"))
                                    .on_click(|_, window, cx| {
                                        window
                                            .dispatch_action(Box::new(crate::actions::Cancel), cx);
                                    }),
                            )
                            .child(
                                IconButton::new("diff-review-add", IconName::Return)
                                    .icon_color(ui::Color::Muted)
                                    .icon_size(action_icon_size)
                                    .tooltip(Tooltip::text("Add comment"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(
                                            Box::new(crate::actions::SubmitDiffReviewComment),
                                            cx,
                                        );
                                    }),
                            ),
                    ),
            )
            // Expandable comments section (only shown when there are comments)
            .when(comment_count > 0, |el| {
                el.child(Self::render_comments_section(
                    comments,
                    comments_expanded,
                    inline_editors,
                    user_avatar_uri,
                    avatar_size,
                    action_icon_size,
                    colors,
                ))
            })
            .into_any_element()
    }

    fn render_comments_section(
        comments: Vec<StoredReviewComment>,
        expanded: bool,
        inline_editors: HashMap<usize, Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let comment_count = comments.len();

        v_flex()
            .w_full()
            .gap_1()
            // Header with expand/collapse toggle
            .child(
                h_flex()
                    .id("review-comments-header")
                    .w_full()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .rounded_md()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(|_, window: &mut Window, cx| {
                        window.dispatch_action(
                            Box::new(crate::actions::ToggleReviewCommentsExpanded),
                            cx,
                        );
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(ui::Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "{} Comment{}",
                            comment_count,
                            if comment_count == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            // Comments list (when expanded)
            .when(expanded, |el| {
                el.children(comments.into_iter().map(|comment| {
                    let inline_editor = inline_editors.get(&comment.id).cloned();
                    Self::render_comment_row(
                        comment,
                        inline_editor,
                        user_avatar_uri.clone(),
                        avatar_size,
                        action_icon_size,
                        colors,
                    )
                }))
            })
    }

    fn render_comment_row(
        comment: StoredReviewComment,
        inline_editor: Option<Entity<Editor>>,
        user_avatar_uri: Option<SharedUri>,
        avatar_size: Pixels,
        action_icon_size: IconSize,
        colors: &theme::ThemeColors,
    ) -> impl IntoElement {
        let comment_id = comment.id;
        let is_editing = inline_editor.is_some();

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .bg(colors.surface_background)
            .child(
                div()
                    .size(avatar_size)
                    .flex_shrink_0()
                    .rounded_full()
                    .overflow_hidden()
                    .child(if let Some(ref avatar_uri) = user_avatar_uri {
                        Avatar::new(avatar_uri.clone())
                            .size(avatar_size)
                            .into_any_element()
                    } else {
                        Icon::new(IconName::Person)
                            .size(IconSize::Small)
                            .color(ui::Color::Muted)
                            .into_any_element()
                    }),
            )
            .child(if let Some(editor) = inline_editor {
                // Inline edit mode: show an editable text field
                div()
                    .flex_1()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .bg(colors.editor_background)
                    .px_2()
                    .py_1()
                    .child(editor)
                    .into_any_element()
            } else {
                // Display mode: show the comment text
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(colors.text)
                    .child(comment.comment)
                    .into_any_element()
            })
            .child(if is_editing {
                // Editing mode: show close and confirm buttons
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(
                            format!("diff-review-cancel-edit-{comment_id}"),
                            IconName::Close,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Cancel"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::CancelEditReviewComment {
                                    id: comment_id,
                                }),
                                cx,
                            );
                        }),
                    )
                    .child(
                        IconButton::new(
                            format!("diff-review-confirm-edit-{comment_id}"),
                            IconName::Return,
                        )
                        .icon_color(ui::Color::Muted)
                        .icon_size(action_icon_size)
                        .tooltip(Tooltip::text("Confirm"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::actions::ConfirmEditReviewComment {
                                    id: comment_id,
                                }),
                                cx,
                            );
                        }),
                    )
                    .into_any_element()
            } else {
                // Display mode: no action buttons for now (edit/delete not yet implemented)
                gpui::Empty.into_any_element()
            })
    }
}

#[cfg(test)]
impl Editor {
    /// Returns the line range for the first diff review overlay, if one is active.
    /// Returns (start_row, end_row) as physical line numbers in the underlying file.
    pub(super) fn diff_review_line_range(&self, cx: &App) -> Option<(u32, u32)> {
        let overlay = self.diff_review_overlays.first()?;
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let start_point = overlay.anchor_range.start.to_point(&snapshot);
        let end_point = overlay.anchor_range.end.to_point(&snapshot);
        let start_row = snapshot
            .point_to_buffer_point(start_point)
            .map(|(_, p)| p.row)
            .unwrap_or(start_point.row);
        let end_row = snapshot
            .point_to_buffer_point(end_point)
            .map(|(_, p)| p.row)
            .unwrap_or(end_point.row);
        Some((start_row, end_row))
    }

    /// Takes all stored comments from all hunks, clearing the storage.
    /// Returns a Vec of (hunk_key, comments) pairs.
    pub(super) fn take_all_review_comments(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<(DiffHunkKey, Vec<StoredReviewComment>)> {
        // Dismiss all overlays when taking comments (e.g., when sending to agent)
        self.dismiss_all_diff_review_overlays(cx);
        let comments = std::mem::take(&mut self.stored_review_comments);
        // Reset the ID counter since all comments have been taken
        self.next_review_comment_id = 0;
        cx.emit(EditorEvent::ReviewCommentsChanged { total_count: 0 });
        cx.notify();
        comments
    }
}

impl EditorSnapshot {
    pub(super) fn display_diff_hunks_for_rows<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        folded_buffers: &'a HashSet<BufferId>,
    ) -> impl 'a + Iterator<Item = DisplayDiffHunk> {
        let buffer_start = DisplayPoint::new(display_rows.start, 0).to_point(self);
        let buffer_end = DisplayPoint::new(display_rows.end, 0).to_point(self);

        self.buffer_snapshot()
            .diff_hunks_in_range(buffer_start..buffer_end)
            .filter_map(|hunk| {
                if folded_buffers.contains(&hunk.buffer_id)
                    || (hunk.row_range.is_empty() && self.buffer.all_diff_hunks_expanded())
                {
                    return None;
                }

                let hunk_start_point = Point::new(hunk.row_range.start.0, 0);
                let hunk_end_point = if hunk.row_range.end > hunk.row_range.start {
                    let last_row = MultiBufferRow(hunk.row_range.end.0 - 1);
                    let line_len = self.buffer_snapshot().line_len(last_row);
                    Point::new(last_row.0, line_len)
                } else {
                    Point::new(hunk.row_range.end.0, 0)
                };

                let hunk_display_start = self.point_to_display_point(hunk_start_point, Bias::Left);
                let hunk_display_end = self.point_to_display_point(hunk_end_point, Bias::Right);

                let display_hunk = if hunk_display_start.column() != 0 {
                    DisplayDiffHunk::Folded {
                        display_row: hunk_display_start.row(),
                    }
                } else {
                    let mut end_row = hunk_display_end.row();
                    if hunk.row_range.end > hunk.row_range.start || hunk_display_end.column() > 0 {
                        end_row.0 += 1;
                    }
                    let is_created_file = hunk.is_created_file();
                    let multi_buffer_range = hunk.multi_buffer_range.clone();

                    DisplayDiffHunk::Unfolded {
                        status: hunk.status(),
                        diff_base_byte_range: hunk.diff_base_byte_range.start.0
                            ..hunk.diff_base_byte_range.end.0,
                        word_diffs: hunk.word_diffs,
                        display_row_range: hunk_display_start.row()..end_row,
                        multi_buffer_range,
                        is_created_file,
                    }
                };

                Some(display_hunk)
            })
    }

    fn hunks_for_ranges(
        &self,
        ranges: impl IntoIterator<Item = Range<Point>>,
    ) -> Vec<MultiBufferDiffHunk> {
        let mut hunks = Vec::new();
        let mut processed_buffer_rows: HashMap<BufferId, HashSet<Range<text::Anchor>>> =
            HashMap::default();
        for query_range in ranges {
            let query_rows =
                MultiBufferRow(query_range.start.row)..MultiBufferRow(query_range.end.row + 1);
            for hunk in self.buffer_snapshot().diff_hunks_in_range(
                Point::new(query_rows.start.0, 0)..Point::new(query_rows.end.0, 0),
            ) {
                // Include deleted hunks that are adjacent to the query range, because
                // otherwise they would be missed.
                let mut intersects_range = hunk.row_range.overlaps(&query_rows);
                if hunk.status().is_deleted() {
                    intersects_range |= hunk.row_range.start == query_rows.end;
                    intersects_range |= hunk.row_range.end == query_rows.start;
                }
                if intersects_range {
                    if !processed_buffer_rows
                        .entry(hunk.buffer_id)
                        .or_default()
                        .insert(hunk.buffer_range.start..hunk.buffer_range.end)
                    {
                        continue;
                    }
                    hunks.push(hunk);
                }
            }
        }

        hunks
    }
}
pub fn render_diff_hunk_controls(
    row: u32,
    _status: &DiffHunkStatus,
    hunk_range: Range<Anchor>,
    is_created_file: bool,
    line_height: Pixels,
    editor: &Entity<Editor>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let operations = hunk_range
        .start
        .buffer_id()
        .and_then(|buffer_id| editor.read(cx).buffer().read(cx).diff_for(buffer_id))
        .and_then(|diff| diff.read(cx).operations());
    let show_restore = operations
        .as_ref()
        .is_some_and(|operations| operations.supports_restore());
    let show_hunk_navigation = !editor.read(cx).buffer().read(cx).all_diff_hunks_expanded();

    if !show_restore && !show_hunk_navigation {
        return gpui::Empty.into_any_element();
    }

    h_flex()
        .h(line_height)
        .mr_1()
        .gap_1()
        .px_0p5()
        .pb_1()
        .border_x_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .rounded_b_lg()
        .bg(cx.theme().colors().editor_background)
        .gap_1()
        .block_mouse_except_scroll()
        .shadow_md()
        .when(show_restore, |el| {
            el.child(
                Button::new(("restore", row as u64), "Restore")
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Restore Hunk",
                                &crate::actions::RestoreDiff,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let point = hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                editor.restore_hunks_in_ranges(vec![point..point], window, cx);
                            });
                        }
                    })
                    .disabled(is_created_file),
            )
        })
        .when(show_hunk_navigation, |el| {
            el.child(
                IconButton::new(("next-hunk", row as u64), IconName::ArrowDown)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    // .disabled(!has_multiple_hunks)
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in("Next Hunk", &GoToHunk, &focus_handle, cx)
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let position = hunk_range.end.to_point(&snapshot.buffer_snapshot());
                                editor.go_to_hunk_before_or_after_position(
                                    &snapshot,
                                    position,
                                    Direction::Next,
                                    true,
                                    window,
                                    cx,
                                );
                                editor.expand_selected_diff_hunks(cx);
                            });
                        }
                    }),
            )
            .child(
                IconButton::new(("prev-hunk", row as u64), IconName::ArrowUp)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    // .disabled(!has_multiple_hunks)
                    .tooltip({
                        let focus_handle = editor.focus_handle(cx);
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                "Previous Hunk",
                                &GoToPreviousHunk,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.clone();
                        move |_event, window, cx| {
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.snapshot(window, cx);
                                let point = hunk_range.start.to_point(&snapshot.buffer_snapshot());
                                editor.go_to_hunk_before_or_after_position(
                                    &snapshot,
                                    point,
                                    Direction::Prev,
                                    true,
                                    window,
                                    cx,
                                );
                                editor.expand_selected_diff_hunks(cx);
                            });
                        }
                    }),
            )
        })
        .into_any_element()
}

impl Editor {}

#[cfg(test)]
mod review_export_tests {
    use super::*;
    use crate::test::editor_test_context::EditorTestContext;

    fn start_review(context: &mut EditorTestContext, range: Range<DisplayRow>) {
        context.update_editor(|editor, window, cx| {
            editor.show_diff_review_overlay(range, window, cx);
            let prompt = editor
                .diff_review_prompt_editor()
                .expect("review input")
                .clone();
            prompt.update(cx, |prompt, cx| {
                prompt.insert("Keep this behavior", window, cx);
            });
        });
    }

    fn submit_review(context: &mut EditorTestContext, range: Range<DisplayRow>) -> String {
        start_review(context, range);
        context.update_editor(|editor, window, cx| {
            editor.submit_diff_review_comment(window, cx);
            assert_eq!(editor.total_review_comment_count(), 1);
            editor.diff_review_text(cx)
        })
    }

    #[gpui::test]
    async fn kk_diff_review_text_preserves_comments_and_tracks_line_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = EditorTestContext::new(cx).await;
        context.set_state("ˇfirst\nsecond\nthird\n");
        context.update_editor(|editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let start = snapshot.anchor_before(Point::new(1, 0));
            let end = snapshot.anchor_after(Point::new(1, 6));
            let hunk = DiffHunkKey {
                file_path: Arc::from(
                    util::rel_path::RelPath::from_unix_str("file").expect("test path"),
                ),
                hunk_start_anchor: start,
            };
            editor.add_review_comment(hunk, "Keep this behavior".into(), start..end, cx);
            assert_eq!(
                editor.diff_review_text(cx),
                "file:2-2\nKeep this behavior\n\n"
            );
            assert_eq!(editor.total_review_comment_count(), 1);
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(Point::zero()..Point::zero(), "inserted\n")], None, cx)
            });
            assert_eq!(
                editor.diff_review_text(cx),
                "file:3-3\nKeep this behavior\n\n"
            );
            assert_eq!(editor.total_review_comment_count(), 1);
        });
    }

    #[gpui::test]
    async fn kk_diff_review_text_preserves_deleted_and_current_locations(
        cx: &mut gpui::TestAppContext,
    ) {
        crate::editor_tests::init_test(cx, |_| {});
        for submit_before_hiding in [true, false] {
            let mut context = EditorTestContext::new(cx).await;
            context.set_state("ˇaaa\neee\n");
            context.set_diff_base("aaa\nbbb\nccc\nddd\neee\n");
            context.update_editor(|editor, window, cx| {
                editor.expand_all_diff_hunks(&crate::ExpandAllDiffHunks, window, cx);
            });
            context.run_until_parked();
            context.update_editor(|editor, _, cx| {
                assert_eq!(editor.text(cx), "aaa\nbbb\nccc\nddd\neee\n");
            });
            start_review(&mut context, DisplayRow(2)..DisplayRow(4));
            let expected = "file:3-4 (deleted)\nfile:2-2\nKeep this behavior\n\n";
            if submit_before_hiding {
                context.update_editor(|editor, window, cx| {
                    editor.submit_diff_review_comment(window, cx);
                    assert_eq!(editor.diff_review_text(cx), expected);
                });
            }
            context.update_editor(|editor, _, cx| {
                editor
                    .buffer()
                    .update(cx, |buffer, cx| buffer.set_show_deleted_hunks(false, cx));
            });
            context.run_until_parked();
            context.update_editor(|editor, window, cx| {
                if !submit_before_hiding {
                    let prompt = editor
                        .diff_review_prompt_editor()
                        .expect("retained review input");
                    window.focus(&prompt.focus_handle(cx), cx);
                    editor.submit_diff_review_comment(window, cx);
                }
                assert_eq!(editor.total_review_comment_count(), 1);
                assert_eq!(editor.diff_review_text(cx), expected);
            });
        }
    }

    #[gpui::test]
    async fn kk_diff_review_text_preserves_selected_blank_endpoints(cx: &mut gpui::TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        for (current, base, range, locations) in [
            ("ˇfirst\n", None, DisplayRow(0)..DisplayRow(1), "file:1-2\n"),
            ("ˇ", None, DisplayRow(0)..DisplayRow(0), "file:1-1\n"),
            (
                "ˇfirst\n\nthird\n",
                None,
                DisplayRow(0)..DisplayRow(1),
                "file:1-2\n",
            ),
            (
                "ˇfirst\n\nthird\n",
                None,
                DisplayRow(1)..DisplayRow(1),
                "file:2-2\n",
            ),
            (
                "ˇaaa\n\n",
                Some("aaa\nbbb\n"),
                DisplayRow(1)..DisplayRow(2),
                "file:2-2 (deleted)\nfile:2-2\n",
            ),
            (
                "ˇaaa\nccc\n",
                Some("aaa\nbbb\n\nccc\n"),
                DisplayRow(1)..DisplayRow(2),
                "file:2-3 (deleted)\n",
            ),
        ] {
            let mut context = EditorTestContext::new(cx).await;
            context.set_state(current);
            if let Some(base) = base {
                context.set_diff_base(base);
                context.update_editor(|editor, window, cx| {
                    editor.expand_all_diff_hunks(&crate::ExpandAllDiffHunks, window, cx);
                });
                context.run_until_parked();
            }
            assert_eq!(
                submit_review(&mut context, range),
                format!("{locations}Keep this behavior\n\n")
            );
        }
    }
}
