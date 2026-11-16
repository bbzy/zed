use super::{git_panel_settings::GitPanelSettings, git_status_icon};
use buffer_diff::BufferDiff;
use editor::display_map::ToDisplayPoint as _;
use editor::{
    DefaultDiffHunkRenderer, DiffHunkRenderer, Editor, EditorEvent, RowExt as _, SplittableEditor,
};
use git_ui_core::file_diff_view::{diff_context_control, diff_style_controls};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Focusable as _, Render, ScrollHandle,
    Subscription, WeakEntity, Window,
};
use language::{Buffer, OffsetRangeExt as _, Point};
use multi_buffer::{PathKey, excerpt_context_lines};
use settings::{Settings as _, StatusStyle};
use std::{ops::Range, sync::Arc};
use ui::{DiffStat, Divider, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle};

async fn load_review_diff(
    snapshot: &kk_workbench::Snapshot,
    target: &super::kk_diff::DiffTarget,
) -> anyhow::Result<(String, String)> {
    snapshot.repository().check_lifetime()?;
    let kk_workbench::DiffScope::Comparison {
        base,
        target: modified,
    } = &target.scope
    else {
        anyhow::bail!("Review Diff requires a comparison");
    };
    let base = snapshot.exact_revision(base).await?.full_commit_id;
    let current = snapshot.repository().snapshot_saved_working_copy().await?;
    let scope = kk_workbench::DiffScope::Comparison {
        base,
        target: modified.clone(),
    };
    let bytes = current.diff(&scope, None).await?;
    let provenance = format!(
        "KK diff from {} at operation {}: {}\nExact comparison: {:?}",
        current.repository().root().display(),
        current.operation(),
        target.label,
        scope
    );
    Ok((String::from_utf8(bytes)?, provenance))
}

pub(super) struct KkDiffControls {
    pub editor: Entity<SplittableEditor>,
    pub showing_full_file: bool,
    ignore_trim_whitespace: bool,
    pub status: Option<git::status::FileStatus>,
    pub generic_style_controls: bool,
    pub aggregate: bool,
    pub comparison_view: Option<WeakEntity<super::kk_multi_diff::KkMultiDiff>>,
    context: Option<(
        kk_workbench::Snapshot,
        super::kk_diff::DiffTarget,
        WeakEntity<workspace::Workspace>,
    )>,
    message: Option<Entity<markdown::Markdown>>,
    message_text: String,
    message_expanded: bool,
    diff_display: Option<Entity<super::kk_settings::DiffDisplayPreference>>,
    diff_display_subscription: Option<Subscription>,
    commit_information: Option<Entity<super::kk_settings::CommitInformation>>,
    commit_information_subscription: Option<Subscription>,
    message_scroll: ScrollHandle,
    review_error: Option<String>,
    review_task: gpui::Task<()>,
    path_order: std::collections::HashMap<language::BufferId, u64>,
    review_count: usize,
    buffers: Vec<(Entity<Buffer>, Entity<BufferDiff>, bool)>,
    initial_position: Option<(language::BufferId, Point)>,
    _diff_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl KkDiffControls {
    pub fn new(
        editor: Entity<SplittableEditor>,
        showing_full_file: bool,
        workspace: WeakEntity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let controls = cx.weak_entity();
        cx.defer_in(window, {
            let workspace = workspace.clone();
            move |controls, window, cx| {
                let preference = workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<super::kk_panel::KkPanel>(cx))
                    .map(|panel| {
                        (
                            panel.read(cx).commit_information.clone(),
                            panel.read(cx).diff_display.clone(),
                        )
                    });
                if let Some((preference, display)) = preference {
                    controls.bind_commit_information(preference, cx);
                    controls.bind_diff_display(display, window, cx);
                }
            }
        });
        let window_handle = window.window_handle();
        let subscriptions = vec![
            cx.observe_new(move |panel: &mut super::kk_panel::KkPanel, _, cx| {
                if !panel.is_for_workspace(&workspace) {
                    return;
                }
                let preference = panel.commit_information.clone();
                let display = panel.diff_display.clone();
                let controls = controls.clone();
                cx.defer(move |cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            controls
                                .update(cx, |controls: &mut Self, cx| {
                                    controls.bind_commit_information(preference, cx);
                                    controls.bind_diff_display(display, window, cx);
                                })
                                .log_err();
                        })
                        .log_err();
                });
            }),
            cx.observe(&editor, |_, _, cx| cx.notify()),
            cx.subscribe(&editor, |this, _, event, cx| {
                if let EditorEvent::ReviewCommentsChanged { total_count } = event {
                    this.review_count = *total_count;
                }
                cx.notify();
            }),
        ];
        editor.update(cx, |editor, cx| {
            editor.set_diff_hunk_renderer(Some(Arc::new(KkDiffHunkRenderer)), cx)
        });
        Self {
            editor,
            showing_full_file,
            ignore_trim_whitespace: true,
            status: None,
            generic_style_controls: false,
            aggregate: false,
            comparison_view: None,
            context: None,
            message: None,
            message_text: String::new(),
            message_expanded: false,
            diff_display: None,
            diff_display_subscription: None,
            commit_information: None,
            commit_information_subscription: None,
            message_scroll: ScrollHandle::new(),
            review_error: None,
            review_task: gpui::Task::ready(()),
            path_order: Default::default(),
            review_count: 0,
            buffers: Vec::new(),
            initial_position: None,
            _diff_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn set_context(
        &mut self,
        snapshot: kk_workbench::Snapshot,
        target: super::kk_diff::DiffTarget,
        workspace: WeakEntity<workspace::Workspace>,
        cx: &mut Context<Self>,
    ) {
        if let Some(revision) = target
            .revision
            .as_ref()
            .filter(|revision| !revision.is_working_copy)
        {
            if self.message.is_none() || self.message_text != revision.full_description {
                self.message_text = revision.full_description.clone();
                self.message_expanded = false;
                let registry = self
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .project()
                    .map(|project| project.read(cx).languages().clone());
                self.message = Some(cx.new(|cx| {
                    markdown::Markdown::new(
                        revision.full_description.clone().into(),
                        registry,
                        None,
                        cx,
                    )
                }));
            }
        }
        let changed = self
            .context
            .as_ref()
            .is_none_or(|(previous, previous_target, _)| {
                previous.operation() != snapshot.operation()
                    || previous_target.scope != target.scope
            });
        self.context = Some((snapshot, target, workspace));
        if changed {
            cx.notify();
        }
    }

    pub(super) fn toggle_commit_details(&mut self, cx: &mut Context<Self>) {
        if let Some(preference) = &self.commit_information {
            preference.update(cx, |preference, cx| preference.toggle(cx));
        }
        cx.notify();
    }

    fn bind_commit_information(
        &mut self,
        preference: Entity<super::kk_settings::CommitInformation>,
        cx: &mut Context<Self>,
    ) {
        if self.commit_information.as_ref() == Some(&preference) {
            return;
        }
        self.commit_information_subscription =
            Some(cx.observe(&preference, |_, _, cx| cx.notify()));
        self.commit_information = Some(preference);
        cx.notify();
    }

    pub(super) fn showing_commit_details(&self, cx: &App) -> bool {
        self.commit_information
            .as_ref()
            .is_none_or(|preference| preference.read(cx).visible())
    }

    fn review_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((snapshot, target, workspace)) = self.context.clone() else {
            return;
        };
        self.review_error = None;
        self.review_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let label = target.label.clone();
                let (diff, provenance) = cx
                    .background_spawn(async move { load_review_diff(&snapshot, &target).await })
                    .await?;
                let workspace = workspace
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("Workspace was closed"))?;
                workspace.update_in(cx, |_, window, cx| {
                    window.dispatch_action(
                        Box::new(zed_actions::agent::ReviewBranchDiff {
                            diff_text: diff.into(),
                            base_ref: label.into(),
                            context: Some(provenance.into()),
                        }),
                        cx,
                    );
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.review_error = Some(format!("Could not review diff: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        });
        cx.notify();
    }

    pub fn set_order(
        &mut self,
        buffers: impl IntoIterator<Item = language::BufferId>,
        cx: &mut Context<Self>,
    ) {
        let order = buffers
            .into_iter()
            .enumerate()
            .map(|(index, buffer)| (buffer, index as u64))
            .collect();
        if self.path_order != order {
            self.path_order = order;
            self.update_excerpts(false, cx);
        }
    }

    pub fn set_buffers(
        &mut self,
        buffers: Vec<(Entity<Buffer>, Entity<BufferDiff>, bool)>,
        cx: &mut Context<Self>,
    ) {
        self._diff_subscriptions = buffers
            .iter()
            .map(|(_, diff, _)| {
                cx.subscribe(diff, |this, _, _, cx| {
                    if !this.showing_full_file {
                        this.update_excerpts(false, cx);
                    }
                    cx.notify();
                })
            })
            .collect();
        self.buffers = buffers;
        self.apply_whitespace_preference(cx);
        cx.notify();
    }

    pub(super) fn set_initial_position(
        &mut self,
        buffer: language::BufferId,
        position: Point,
        cx: &mut Context<Self>,
    ) {
        self.initial_position = Some((buffer, position));
        if !self.showing_full_file {
            self.update_excerpts(false, cx);
        }
    }

    pub fn excerpt_ranges(
        buffer: &Entity<Buffer>,
        diff: &Entity<BufferDiff>,
        showing_full_file: bool,
        cx: &App,
    ) -> (Vec<Range<Point>>, u32) {
        if showing_full_file {
            (vec![Point::zero()..buffer.read(cx).max_point()], 0)
        } else {
            let snapshot = buffer.read(cx).snapshot();
            let mut ranges: Vec<_> = diff
                .read(cx)
                .snapshot(cx)
                .hunks(&snapshot.text)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect();
            // Saving must not detach a restoration's buffer from native Undo/Redo.
            let buffer = buffer.read(cx);
            if ranges.is_empty()
                && (buffer.is_dirty()
                    || buffer.peek_undo_stack().is_some()
                    || buffer.peek_redo_stack().is_some())
            {
                ranges.push(Point::zero()..Point::zero());
            }
            (ranges, excerpt_context_lines(cx))
        }
    }

    fn bind_diff_display(
        &mut self,
        preference: Entity<super::kk_settings::DiffDisplayPreference>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_display.as_ref() == Some(&preference) {
            return;
        }
        let value = preference.read(cx).showing_full_file();
        if self.showing_full_file != value {
            self.set_full_file(value, window, cx);
        }
        self.ignore_trim_whitespace = preference.read(cx).ignore_trim_whitespace();
        self.apply_whitespace_preference(cx);
        self.diff_display_subscription =
            Some(
                cx.observe_in(&preference, window, |this, preference, window, cx| {
                    this.ignore_trim_whitespace = preference.read(cx).ignore_trim_whitespace();
                    this.apply_whitespace_preference(cx);
                    let value = preference.read(cx).showing_full_file();
                    if this.showing_full_file != value {
                        this.set_full_file(value, window, cx);
                    }
                    cx.notify();
                }),
            );
        self.diff_display = Some(preference);
    }

    fn apply_whitespace_preference(&self, cx: &mut Context<Self>) {
        for (_, diff, is_text) in &self.buffers {
            if *is_text {
                diff.update(cx, |diff, cx| {
                    diff.set_ignore_trim_whitespace(self.ignore_trim_whitespace, cx);
                });
            }
        }
    }

    pub(super) fn toggle_ignore_trim_whitespace(&mut self, cx: &mut Context<Self>) {
        if let Some(preference) = self.diff_display.clone() {
            preference.update(cx, |preference, cx| {
                preference.toggle_ignore_trim_whitespace(cx)
            });
            self.ignore_trim_whitespace = preference.read(cx).ignore_trim_whitespace();
        } else {
            self.ignore_trim_whitespace = !self.ignore_trim_whitespace;
        }
        self.apply_whitespace_preference(cx);
        cx.notify();
    }

    pub fn toggle_full_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = !self.showing_full_file;
        if let Some(preference) = self.diff_display.clone() {
            preference.update(cx, |preference, cx| preference.toggle(cx));
        }
        self.set_full_file(value, window, cx);
    }

    pub fn set_full_file(
        &mut self,
        showing_full_file: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let showing_full_file = self
            .diff_display
            .as_ref()
            .map_or(showing_full_file, |preference| {
                preference.read(cx).showing_full_file()
            });
        if self.showing_full_file == showing_full_file {
            return;
        }
        let view_position = if showing_full_file {
            let editors = {
                let editor = self.editor.read(cx);
                editor
                    .lhs_editor()
                    .into_iter()
                    .chain([editor.rhs_editor()])
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for editor in editors {
                editor.update(cx, |editor, cx| editor.refresh_selection_anchors(cx));
            }
            let focused = self.editor.read(cx).focused_editor().clone();
            let offset = focused.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let cursor_row = editor
                    .selections
                    .newest_anchor()
                    .head()
                    .to_display_point(&snapshot.display_snapshot)
                    .row()
                    .as_f64();
                let scroll_position = editor.scroll_position(cx);
                gpui::point(scroll_position.x, scroll_position.y - cursor_row)
            });
            Some((focused, offset))
        } else {
            None
        };
        self.showing_full_file = showing_full_file;
        // Expanding in place keeps selections and deleted-line anchors attached to their buffers.
        self.update_excerpts(!showing_full_file, cx);
        if let Some((focused, offset)) = view_position {
            focused.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let cursor_row = editor
                    .selections
                    .newest_anchor()
                    .head()
                    .to_display_point(&snapshot.display_snapshot)
                    .row()
                    .as_f64();
                editor.set_scroll_position(
                    gpui::point(offset.x, cursor_row + offset.y),
                    window,
                    cx,
                );
            });
        }
        let right = self.editor.read(cx).rhs_editor().clone();
        right.update(cx, |editor, cx| {
            editor.set_allow_git_diff_scrollbar_markers(self.showing_full_file, cx);
        });
        cx.notify();
    }

    pub fn update_excerpts(&mut self, reset: bool, cx: &mut Context<Self>) {
        let review_ranges = {
            let right = self.editor.read(cx).rhs_editor().read(cx);
            let snapshot = right.buffer().read(cx).snapshot(cx);
            right
                .diff_review_ranges(cx)
                .flat_map(|range| {
                    snapshot
                        .range_to_buffer_ranges(range.clone())
                        .into_iter()
                        .chain(snapshot.range_to_buffer_ranges(range.end..range.end))
                })
                .map(|(buffer, range, _)| (buffer.remote_id(), range.to_point(buffer)))
                .collect::<Vec<_>>()
        };
        for (buffer, diff, _) in &self.buffers {
            let (mut ranges, context_lines) =
                Self::excerpt_ranges(buffer, diff, self.showing_full_file, cx);
            if !self.showing_full_file {
                let buffer_id = buffer.read(cx).remote_id();
                if let Some((source, position)) = self.initial_position
                    && source == buffer_id
                {
                    ranges.push(position..position);
                }

                ranges.extend(
                    review_ranges
                        .iter()
                        .filter(|(source, _)| *source == buffer_id)
                        .map(|(_, range)| range.clone()),
                );
                ranges.sort_by_key(|range| (range.start, range.end));
            }
            self.editor.update(cx, |editor, cx| {
                let mut path = PathKey::for_buffer(buffer, cx);
                if let Some(order) = self.path_order.get(&buffer.read(cx).remote_id()) {
                    path.sort_prefix = Some(*order);
                }
                if reset {
                    editor.remove_excerpts_for_path(path.clone(), cx);
                }
                editor.update_excerpts_for_path(
                    path,
                    buffer.clone(),
                    ranges,
                    context_lines,
                    diff.clone(),
                    cx,
                );
            });
        }
    }

    pub fn capture_excerpt_layout(&self, cx: &App) -> Vec<(language::BufferId, Vec<Range<Point>>)> {
        let snapshot = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx);
        self.buffers
            .iter()
            .map(|(buffer, _, _)| {
                let buffer = buffer.read(cx).snapshot();
                let ranges = snapshot
                    .excerpts_for_buffer(buffer.remote_id())
                    .map(|excerpt| excerpt.context.to_point(&buffer))
                    .collect();
                (buffer.remote_id(), ranges)
            })
            .collect()
    }

    pub fn restore_excerpt_layout(
        &mut self,
        layout: Vec<(language::BufferId, Vec<Range<Point>>)>,
        cx: &mut Context<Self>,
    ) {
        for (buffer, diff, _) in &self.buffers {
            let Some((_, ranges)) = layout
                .iter()
                .find(|(id, _)| *id == buffer.read(cx).remote_id())
            else {
                continue;
            };
            self.editor.update(cx, |editor, cx| {
                let path = editor
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .path_for_buffer(buffer.read(cx).remote_id())
                    .cloned()
                    .unwrap_or_else(|| PathKey::for_buffer(buffer, cx));
                editor.remove_excerpts_for_path(path.clone(), cx);
                editor.update_excerpts_for_path(
                    path,
                    buffer.clone(),
                    ranges.clone(),
                    0,
                    diff.clone(),
                    cx,
                );
            });
        }
        cx.notify();
    }

    pub fn restore_all(&self, window: &mut Window, cx: &mut App) {
        let right = self.editor.read(cx).rhs_editor().clone();
        let snapshot = right.read(cx).buffer().read(cx).snapshot(cx);
        let hunks = snapshot
            .diff_hunks()
            .filter(|hunk| {
                !hunk.is_created_file()
                    && right
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .diff_for(hunk.buffer_id)
                        .is_some_and(|diff| {
                            diff.read(cx)
                                .operations()
                                .is_some_and(|operations| operations.supports_restore())
                        })
            })
            .collect();
        right.update(cx, |editor, cx| editor.apply_restore(hunks, window, cx));
    }

    pub fn statistics(&self, cx: &App) -> Option<(u32, u32)> {
        let mut counts = None;
        for (_, diff, text) in &self.buffers {
            if *text {
                let (added, deleted) = diff.read(cx).changed_row_counts();
                let (total_added, total_deleted) = counts.get_or_insert((0, 0));
                *total_added += added;
                *total_deleted += deleted;
            }
        }

        counts
    }

    pub fn render_actions(
        controls: Entity<Self>,
        window: &Window,
        cx: &mut App,
    ) -> impl IntoElement + use<> {
        let this = controls.read(cx);
        let editor = this.editor.clone();
        let focus = editor.focus_handle(cx);
        let right_handle = editor.read(cx).rhs_editor().clone();
        let right = right_handle.read(cx);
        let snapshot = right.buffer().read(cx).snapshot(cx);
        let statistics = this.statistics(cx);
        let has_changes = snapshot.diff_hunks().next().is_some();
        let restore = snapshot.all_buffer_ids().any(|id| {
            right.buffer().read(cx).diff_for(id).is_some_and(|diff| {
                diff.read(cx)
                    .operations()
                    .is_some_and(|operations| operations.supports_restore())
            })
        });
        let ignore_trim_whitespace = this.ignore_trim_whitespace;
        let review_count = this.review_count;
        let aggregate = this.aggregate;
        let showing_commit_details = this.showing_commit_details(cx);
        let context = this.context.clone();
        let comparison_view = this.comparison_view.clone();
        let has_restore_all = snapshot.diff_hunks().any(|hunk| {
            !hunk.is_created_file()
                && right
                    .buffer()
                    .read(cx)
                    .diff_for(hunk.buffer_id)
                    .is_some_and(|diff| {
                        diff.read(cx)
                            .operations()
                            .is_some_and(|operations| operations.supports_restore())
                    })
        });
        let selected = editor.read(cx).focused_editor().clone();
        let selected_restore = selected.update(cx, |editor, cx| {
            editor.can_restore_selected_hunks(window, cx)
        });
        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .child(
                IconButton::new("ignore-trim-whitespace", IconName::Space)
                    .aria_label("Toggle Trim Whitespace Changes")
                    .icon_size(IconSize::Small)
                    .toggle_state(!ignore_trim_whitespace)
                    .tooltip(Tooltip::text(if ignore_trim_whitespace {
                        "Show Trim Whitespace Changes"
                    } else {
                        "Ignore Trim Whitespace"
                    }))
                    .on_click({
                        let controls = controls.clone();
                        move |_, _, cx| {
                            controls.update(cx, |controls, cx| {
                                controls.toggle_ignore_trim_whitespace(cx);
                            });
                        }
                    }),
            )
            .when_some(context, |this, (snapshot, target, workspace)| {
                let historical = target
                    .revision
                    .clone()
                    .filter(|revision| !revision.is_working_copy);
                this.when_some(historical, |this, revision| {
                    let details_controls = controls.clone();
                    let search_editor = editor.clone();
                    let workspace = workspace.clone();
                    this.child(
                        IconButton::new("toggle-commit-details", IconName::Eye)
                            .aria_label("Toggle Commit Information")
                            .icon_size(IconSize::Small)
                            .toggle_state(showing_commit_details)
                            .tooltip(Tooltip::text(if showing_commit_details {
                                "Hide Commit Information"
                            } else {
                                "Show Commit Information"
                            }))
                            .on_click(move |_, _, cx| {
                                details_controls.update(cx, |controls, cx| {
                                    controls.toggle_commit_details(cx);
                                });
                            }),
                    )
                    .child(
                        IconButton::new("buffer-search", IconName::MagnifyingGlass)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Buffer Search"))
                            .on_click(move |_, window, cx| {
                                git_ui_core::file_diff_view::dispatch_diff_action(
                                    &search_editor,
                                    &zed_actions::buffer_search::Deploy::find(),
                                    window,
                                    cx,
                                )
                            }),
                    )
                    .child(
                        IconButton::new("show-in-kk-history", IconName::GitGraph)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Show in KK History"))
                            .on_click({
                                let root = snapshot.repository().root().to_owned();
                                move |_, window, cx| {
                                    if let Some(workspace) = workspace.upgrade() {
                                        workspace.update(cx, |workspace, cx| {
                                            workspace.focus_panel::<super::kk_panel::KkPanel>(
                                                window, cx,
                                            );
                                            if let Some(panel) =
                                                workspace.panel::<super::kk_panel::KkPanel>(cx)
                                            {
                                                panel.update(cx, |panel, cx| {
                                                    panel.reveal_revision(
                                                        root.clone(),
                                                        revision.clone(),
                                                        cx,
                                                    )
                                                });
                                            }
                                        });
                                    }
                                }
                            }),
                    )
                })
                .when(
                    aggregate && matches!(target.scope, kk_workbench::DiffScope::Comparison { .. }),
                    |this| {
                        this.child(Label::new("Base:").color(Color::Muted))
                            .child(
                                ui::Button::new(
                                    "kk-diff-base",
                                    target.label.trim_end_matches(" → @").to_owned(),
                                )
                                .color(Color::Accent)
                                .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("Select Comparison Base"))
                                .on_click(move |_, window, cx| {
                                    if let Some(workspace) = workspace.upgrade() {
                                        workspace.update(cx, |workspace, cx| {
                                            workspace.toggle_modal(window, cx, |window, cx| {
                                                let view = comparison_view.clone();
                                                super::kk_revision_picker::new(
                                                    snapshot.clone(),
                                                    super::kk_revision_picker::Purpose::Compare,
                                                    window,
                                                    cx,
                                                    move |_, revision, window, cx| {
                                                        if let Some(view) = &view {
                                                            view.update(cx, |view, cx| {
                                                                view.set_comparison_base(
                                                                    revision, window, cx,
                                                                )
                                                            })
                                                            .log_err();
                                                        }
                                                    },
                                                )
                                            });
                                        });
                                    }
                                }),
                            )
                            .when(
                                has_changes
                                    && agent_settings::AgentSettings::get_global(cx).enabled(cx),
                                |this| {
                                    let controls = controls.clone();
                                    this.child(Divider::vertical()).child(
                                        ui::Button::new("review-diff", "Review Diff")
                                            .start_icon(
                                                Icon::new(IconName::ZedAssistant)
                                                    .size(IconSize::Small)
                                                    .color(Color::Muted),
                                            )
                                            .on_click(move |_, window, cx| {
                                                controls.update(cx, |controls, cx| {
                                                    controls.review_diff(window, cx)
                                                })
                                            }),
                                    )
                                },
                            )
                    },
                )
            })
            .when_some(statistics, |this, (added, deleted)| {
                this.child(DiffStat::new(
                    "solo-diff-stat",
                    added as usize,
                    deleted as usize,
                ))
                .child(Divider::vertical().ml_1())
            })
            .child(git_ui_core::file_diff_view::diff_navigation_controls(
                editor.clone(),
                has_changes,
                cx,
            ))
            .when(restore, |this| {
                let editor = editor.clone();
                this.child(Divider::vertical()).child(
                    ui::Button::new("restore", "Restore")
                        .disabled(!selected_restore)
                        .tooltip(Tooltip::for_action_title_in(
                            "Restore selected hunk",
                            &git::Restore,
                            &focus,
                        ))
                        .on_click(move |_, window, cx| {
                            git_ui_core::file_diff_view::dispatch_diff_action(
                                &editor,
                                &git::Restore,
                                window,
                                cx,
                            )
                        }),
                )
            })
            .when(aggregate && restore, |this| {
                let controls = controls.clone();
                this.child(Divider::vertical()).child(
                    ui::Button::new("restore-all", "Restore All")
                        .disabled(!has_restore_all)
                        .tooltip(Tooltip::text("Restore All Changes"))
                        .on_click(move |_, window, cx| {
                            controls.update(cx, |controls, cx| controls.restore_all(window, cx));
                        }),
                )
            })
            .when(review_count > 0, |this| {
                this.child(Divider::vertical()).child(
                    git_ui_core::file_diff_view::render_send_review_to_agent_button(
                        review_count,
                        &focus,
                    )
                    .on_click(move |_, window, cx| {
                        git_ui_core::file_diff_view::dispatch_diff_action(
                            &editor,
                            &editor::actions::SendReviewToAgent,
                            window,
                            cx,
                        )
                    }),
                )
            })
    }
}

impl Render for KkDiffControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = self
            .context
            .as_ref()
            .and_then(|(_, target, _)| target.revision.as_ref())
            .filter(|revision| !revision.is_working_copy)
            .filter(|_| self.showing_commit_details(cx))
            .cloned();
        v_flex()
            .w_full()
            .when_some(
                self.commit_information
                    .as_ref()
                    .and_then(|preference| preference.read(cx).error.clone()),
                |view, error| view.child(Label::new(error).color(Color::Error)),
            )
            .children(details.and_then(|revision| {
                let message = self.message.clone()?;
                let view = cx.weak_entity();
                let sha: gpui::SharedString = revision.full_commit_id.into();
                Some(
                    super::commit_metadata::CommitHeader {
                        author_name: revision.author_name.into(),
                        author_email: revision.author_email.into(),
                        timestamp: revision.commit_timestamp,
                        description: revision.full_description,
                        change_id_prefix: Some(revision.change_id_prefix),
                        identifiers: vec![
                            ("Change ID", revision.full_change_id.into()),
                            ("Commit SHA", sha),
                        ],
                        avatar: super::commit_metadata::avatar_placeholder(
                            Some(rems_from_px(40_f32).into()),
                            window,
                            cx,
                        ),
                        editor: self.editor.clone(),
                        message,
                        expanded: self.message_expanded,
                        message_scroll_handle: self.message_scroll.clone(),
                        on_toggle: std::rc::Rc::new(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.message_expanded = !this.message_expanded;
                                cx.notify();
                            })
                            .log_err();
                        }),
                    }
                    .render(window, cx),
                )
            }))
            .when_some(
                self.diff_display
                    .as_ref()
                    .and_then(|preference| preference.read(cx).error.clone()),
                |this, error| this.child(Label::new(error).color(Color::Error)),
            )
            .when_some(self.review_error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error))
            })
    }
}

struct KkDiffHunkRenderer;

impl DiffHunkRenderer for KkDiffHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        status: &buffer_diff::DiffHunkStatus,
        hunk_range: Range<multi_buffer::Anchor>,
        is_created_file: bool,
        line_height: gpui::Pixels,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        DefaultDiffHunkRenderer.render_hunk_controls(
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

    fn render_hunk_as_staged(&self, _: &buffer_diff::DiffHunkStatus, _: &App) -> bool {
        false
    }
}

pub struct KkDiffStyleToolbar {
    kk_diff: Option<Entity<KkDiffControls>>,
    _subscription: Option<Subscription>,
}

impl KkDiffStyleToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            kk_diff: None,
            _subscription: None,
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for KkDiffStyleToolbar {}

impl ToolbarItemView for KkDiffStyleToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.kk_diff = active_pane_item.and_then(|item| item.act_as::<KkDiffControls>(cx));
        self._subscription = self
            .kk_diff
            .as_ref()
            .map(|controls| cx.observe(controls, |_, _, cx| cx.notify()));
        if self.kk_diff.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

pub struct KkDiffActionsToolbar {
    kk_diff: Option<Entity<KkDiffControls>>,
    _subscription: Option<Subscription>,
}

impl KkDiffActionsToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            kk_diff: None,
            _subscription: None,
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for KkDiffActionsToolbar {}

impl ToolbarItemView for KkDiffActionsToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.kk_diff = active_pane_item.and_then(|item| item.act_as::<KkDiffControls>(cx));
        self._subscription = self
            .kk_diff
            .as_ref()
            .map(|controls| cx.observe(controls, |_, _, cx| cx.notify()));
        if self.kk_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for KkDiffStyleToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(controls) = &self.kk_diff {
            let editor = controls.read(cx).editor.clone();
            let showing_full_file = controls.read(cx).showing_full_file;
            let status = controls.read(cx).status;
            let generic_style_controls = controls.read(cx).generic_style_controls;
            let show_status_icon =
                GitPanelSettings::get_global(cx).status_style != StatusStyle::LabelColor;
            let controls = controls.downgrade();
            let on_toggle = move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                controls
                    .update(cx, |controls, cx| controls.toggle_full_file(window, cx))
                    .log_err();
            };
            let controls = if generic_style_controls {
                h_flex()
                    .pl_0p5()
                    .gap_1()
                    .child(diff_context_control(showing_full_file, on_toggle))
            } else {
                diff_style_controls(editor, showing_full_file, on_toggle)
            };
            return controls
                .when_some(
                    show_status_icon.then_some(status).flatten(),
                    |this, status| this.child(git_status_icon(status)),
                )
                .into_any_element();
        }
        gpui::Empty.into_any_element()
    }
}

impl Render for KkDiffActionsToolbar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(controls) = &self.kk_diff {
            KkDiffControls::render_actions(controls.clone(), window, cx).into_any_element()
        } else {
            gpui::Empty.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn kk_diff_full_file_expansion_preserves_cursor_and_viewport(cx: &mut TestAppContext) {
        use editor::SelectionEffects;
        use fs::FakeFs;
        use gpui::VisualContext as _;
        use language::{Capability, Selection};
        use multi_buffer::MultiBuffer;
        use project::Project;
        use settings::SettingsStore;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.minimum_split_diff_width = Some(0.0);
                });
            });
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let original = (0..240)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let modified = (0..240)
            .map(|row| {
                if row % 30 == 0 || row == 239 {
                    format!("changed {row}\n")
                } else {
                    format!("line {row}\n")
                }
            })
            .collect::<String>();
        for file_count in [1, 2] {
            for (style, left, deleted) in [
                (settings::DiffViewStyle::Unified, false, false),
                (settings::DiffViewStyle::Unified, false, true),
                (settings::DiffViewStyle::Split, false, false),
                (settings::DiffViewStyle::Split, true, false),
            ] {
                let buffers = (0..file_count)
                    .map(|_| {
                        let buffer = cx.new(|cx| Buffer::local(modified.clone(), cx));
                        let diff = cx.new(|cx| {
                            BufferDiff::new_with_base_text(
                                &original,
                                &buffer.read(cx).text_snapshot(),
                                cx,
                            )
                        });
                        (buffer, diff, true)
                    })
                    .collect::<Vec<_>>();
                let (target, target_diff, _) = buffers.last().expect("last file").clone();
                let editor = cx.new_window_entity(|window, cx| {
                    let multibuffer = cx.new(|cx| {
                        let mut buffer = MultiBuffer::new(Capability::ReadOnly);
                        buffer.set_all_diff_hunks_expanded(cx);
                        buffer
                    });
                    SplittableEditor::new(
                        style,
                        multibuffer,
                        project.clone(),
                        workspace.clone(),
                        window,
                        cx,
                    )
                });
                let controls = cx.new_window_entity(|window, cx| {
                    let mut controls = KkDiffControls::new(
                        editor.clone(),
                        false,
                        workspace.downgrade(),
                        window,
                        cx,
                    );
                    let order = buffers
                        .iter()
                        .map(|(buffer, _, _)| buffer.read(cx).remote_id())
                        .collect::<Vec<_>>();
                    controls.set_order(order, cx);
                    controls.set_buffers(buffers, cx);
                    controls.update_excerpts(true, cx);
                    controls
                });
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                cx.run_until_parked();
                for row in [0, 120, 239] {
                    for (end_column, reversed) in [(1, false), (4, false), (4, true)] {
                        controls.update_in(cx, |controls, window, cx| {
                            controls.set_full_file(false, window, cx)
                        });
                        let focused = editor.update_in(cx, |editor, window, cx| {
                            if style == settings::DiffViewStyle::Split {
                                editor.split(window, cx);
                            }
                            if left {
                                editor.lhs_editor().expect("left editor")
                            } else {
                                editor.rhs_editor()
                            }
                            .clone()
                        });
                        focused.update_in(cx, |editor, window, cx| {
                            window.focus(&editor.focus_handle(cx), cx);
                            let snapshot = editor.snapshot(window, cx);
                            let source = snapshot.buffer_snapshot();
                            let target_id = target.read(cx).remote_id();
                            let source_id = if left {
                                target_diff.read(cx).base_text(cx).remote_id()
                            } else {
                                target_id
                            };
                            let source_buffer =
                                source.buffer_for_id(source_id).expect("selected file");
                            let anchor_at = |column| {
                                let anchor = source
                                    .anchor_in_buffer(source_buffer.anchor_before(Point::new(
                                        row,
                                        if deleted { 0 } else { column },
                                    )))
                                    .expect("selection anchor");
                                if deleted {
                                    anchor.with_diff_base_anchor(
                                        source
                                            .diff_for_buffer_id(target_id)
                                            .expect("diff")
                                            .base_text()
                                            .anchor_before(Point::new(row, column)),
                                    )
                                } else {
                                    anchor
                                }
                            };
                            let start = anchor_at(1);
                            let end = anchor_at(end_column);
                            editor.change_selections(
                                SelectionEffects::no_scroll(),
                                window,
                                cx,
                                |selections| {
                                    selections.select(vec![Selection {
                                        id: 1,
                                        start,
                                        end,
                                        reversed,
                                        goal: language::SelectionGoal::None,
                                    }]);
                                },
                            );
                            let cursor = editor.selections.newest_anchor().head();
                            assert_eq!(cursor.diff_base_anchor().is_some(), deleted);
                            let snapshot = editor.snapshot(window, cx);
                            let cursor_row = cursor
                                .to_display_point(&snapshot.display_snapshot)
                                .row()
                                .as_f64();
                            editor.set_scroll_position(
                                gpui::point(0., cursor_row - 5.25),
                                window,
                                cx,
                            );
                        });
                        cx.run_until_parked();
                        cx.update(|window, cx| window.draw(cx).clear(cx));
                        let before = focused.update_in(cx, |editor, window, cx| {
                            let snapshot = editor.snapshot(window, cx);
                            let selections =
                                editor.selections.all_anchors(&snapshot.display_snapshot);
                            let cursor_row = editor
                                .selections
                                .newest_anchor()
                                .head()
                                .to_display_point(&snapshot.display_snapshot)
                                .row()
                                .as_f64();
                            (selections, cursor_row - editor.scroll_position(cx).y)
                        });
                        controls.update_in(cx, |controls, window, cx| {
                            controls.toggle_full_file(window, cx)
                        });
                        cx.run_until_parked();
                        cx.update(|window, cx| window.draw(cx).clear(cx));
                        focused.update_in(cx, |editor, window, cx| {
                            let snapshot = editor.snapshot(window, cx);
                            assert_eq!(editor.selections.all_anchors(&snapshot.display_snapshot), before.0, "{style:?}, left={left}, deleted={deleted}, files={file_count}, row={row}, reversed={reversed}");
                            let cursor_row = editor.selections.newest_anchor().head().to_display_point(&snapshot.display_snapshot).row().as_f64();
                            assert!((cursor_row - editor.scroll_position(cx).y - before.1).abs() < 0.01, "cursor moved on screen: {style:?}, left={left}, deleted={deleted}, files={file_count}, row={row}");
                        });
                        let expanded = controls
                            .read_with(cx, |controls, cx| controls.capture_excerpt_layout(cx));
                        assert_eq!(expanded.len(), file_count);
                        assert!(
                            expanded
                                .iter()
                                .all(|(_, ranges)| ranges == &[Point::zero()..Point::new(240, 0)])
                        );
                        controls.update_in(cx, |controls, window, cx| {
                            controls.set_full_file(true, window, cx)
                        });
                        assert_eq!(
                            controls
                                .read_with(cx, |controls, cx| controls.capture_excerpt_layout(cx)),
                            expanded
                        );
                    }
                }
            }
        }
    }

    #[gpui::test]
    async fn kk_diff_review_uses_saved_contents_and_keeps_the_displayed_base(
        cx: &mut TestAppContext,
    ) {
        let directory = super::super::kk_multi_diff::tests::local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, std::path::Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("displayed snapshot");
        let parent = snapshot.exact_revision("@-").await.expect("displayed base");
        let target = super::super::kk_diff::DiffTarget::comparison(
            parent.revset(),
            parent.revset(),
            "parent".into(),
            None,
        );
        super::super::kk_panel::remote_tests::run_kk(
            &root,
            &["squash", "--into", "@-", "-m", "rewritten base", "a.txt"],
        )
        .await
        .expect("rewrite mutable base");
        let pending = kk_workbench::SavedSnapshot::capture(&repository, ["a.txt".into()])
            .expect("pending classification capture");
        std::fs::write(root.join("a.txt"), "saved after the page opened\n")
            .expect("later saved content");
        kk_workbench::record_saved_file(repository.root(), "a.txt").expect("record later save");
        let before = repository.snapshot(false).await.expect("deferred snapshot");
        let error = load_review_diff(&snapshot, &target)
            .await
            .expect_err("deferred saved contents cannot be reviewed");
        assert!(
            error.to_string().contains("pending classification"),
            "{error}"
        );
        assert_eq!(
            repository
                .snapshot(false)
                .await
                .expect("unchanged operation")
                .operation(),
            before.operation(),
            "review rejection does not capture over queued classification inputs"
        );
        drop(pending);
        let (diff, provenance) = load_review_diff(&snapshot, &target)
            .await
            .expect("current review contents");
        assert!(diff.contains("+saved after the page opened"), "{diff}");
        assert!(diff.contains("-old a"), "{diff}");
        assert!(provenance.contains(&parent.full_commit_id), "{provenance}");
        assert!(
            !provenance.contains(snapshot.operation()),
            "review pins the new snapshot"
        );
        let old = snapshot
            .diff(&target.scope, Some("a.txt"))
            .await
            .expect("displayed snapshot stays immutable");
        assert!(
            String::from_utf8(old)
                .expect("text diff")
                .contains("+new a")
        );
    }
}
