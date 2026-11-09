use super::{
    kk_diff::{DiffOpenMode, DiffTarget, KkDiff},
    kk_diff_controls::KkDiffControls,
};
use anyhow::{Context as _, Result, ensure};
use db::kvp::KeyValueStore;
use editor::items::{SerializedBufferContents, SerializedEditorState};
use editor::{Editor, EditorEvent, EditorSettings, SplittableEditor};
use futures::{FutureExt as _, StreamExt as _, future::Shared};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use kk_workbench::{ChangedFile, Snapshot};
use language::Capability;
use multi_buffer::MultiBuffer;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use ui::{CommonAnimationExt as _, ContextMenu, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, ItemId, SerializableItem, Workspace, WorkspaceId,
    item::{ItemEvent, ItemHandle as _, SaveOptions},
    searchable::SearchableItemHandle,
};

pub(super) struct KkMultiDiff {
    snapshot: Snapshot,
    target: DiffTarget,
    working_copy: bool,
    entries: Vec<Entity<KkDiff>>,
    files: Vec<ChangedFile>,
    panel: Option<WeakEntity<super::kk_panel::KkPanel>>,
    pub(super) loading: bool,
    initial_reveal_pending: bool,
    editor: Entity<SplittableEditor>,
    controls: Entity<KkDiffControls>,
    workspace: WeakEntity<Workspace>,
    window: gpui::AnyWindowHandle,
    refresh_generation: u64,
    refresh_snapshot: Option<Snapshot>,
    pending_target: Option<DiffTarget>,
    refresh_task: Task<()>,
    error: Option<String>,
    _entry_subscriptions: Vec<Subscription>,
    writes: Shared<Task<Result<(), String>>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Serialize, Deserialize)]
struct SavedMultiDiff {
    root: PathBuf,
    target: DiffTarget,
    files: Vec<ChangedFile>,
    working_copy: bool,
    operation: String,
    host: Option<String>,
    drafts: Vec<(String, SavedDraft)>,
    editor: SerializedEditorState,
    split: bool,
    left: bool,
    full_file: bool,
}

#[derive(Serialize, Deserialize)]
struct SavedDraft {
    #[serde(flatten)]
    contents: SerializedBufferContents,
    // A failed refresh must not retarget a retained draft during session recovery.
    #[serde(default)]
    source: Option<SavedDraftSource>,
}

#[derive(Serialize, Deserialize)]
struct SavedDraftSource {
    operation: String,
    target: DiffTarget,
}

fn session_namespace(workspace_id: WorkspaceId) -> String {
    format!("KKMultiDiffTabs:{}", i64::from(workspace_id))
}

struct MultiDiffAddon(WeakEntity<KkMultiDiff>);

impl editor::Addon for MultiDiffAddon {
    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        cx: &App,
    ) -> Option<git::status::FileStatus> {
        let view = self.0.upgrade()?;
        view.read(cx).entries.iter().find_map(|entry| {
            let entry = entry.read(cx);
            (entry.buffer.read(cx).remote_id() == buffer_id
                || entry.diff.read(cx).base_text_buffer().read(cx).remote_id() == buffer_id)
                .then(|| super::kk_diff::file_status(&entry.file))
        })
    }
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let Some(view) = self.0.upgrade() else {
            return menu;
        };
        let entry = view
            .read(cx)
            .entries
            .iter()
            .find(|entry| {
                let entry = entry.read(cx);
                entry.buffer.read(cx).remote_id() == buffer.remote_id()
                    || entry.diff.read(cx).base_text_buffer().read(cx).remote_id()
                        == buffer.remote_id()
            })
            .cloned();
        menu.when_some(entry, |menu, entry| {
            menu.entry("KKW: Open File in Project", None, move |window, cx| {
                entry.update(cx, |entry, cx| entry.open_file_in_project(window, cx));
            })
        })
    }
}

impl KkMultiDiff {
    pub fn open(
        snapshot: Snapshot,
        target: DiffTarget,
        files: Vec<ChangedFile>,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpenMode,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        Self::open_with_buffers(
            snapshot,
            target,
            files,
            working_copy,
            workspace,
            mode,
            Vec::new(),
            window,
            cx,
        )
    }

    fn open_with_buffers(
        snapshot: Snapshot,
        target: DiffTarget,
        files: Vec<ChangedFile>,
        working_copy: bool,
        workspace: WeakEntity<Workspace>,
        mode: DiffOpenMode,
        saved_buffers: Vec<(String, SavedDraft)>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let mut saved_buffers: std::collections::HashMap<_, _> =
            saved_buffers.into_iter().collect();
        let mut previous = Vec::new();
        let mut existing_view = None;
        if let Some(owner) = workspace.upgrade() {
            let existing = owner.read(cx).items_of_type::<Self>(cx).find(|item| {
                let view = item.read(cx);
                view.snapshot.repository().root() == snapshot.repository().root()
                    && view.target.identity == target.identity
                    && view.working_copy == working_copy
                    && (working_copy || view.target.scope == target.scope)
            });
            if let Some(existing) = existing {
                existing_view = Some(existing.clone());
                previous = existing.read(cx).entries.clone();
                if saved_buffers.is_empty()
                    && existing.read(cx).error.is_none()
                    && existing.read(cx).target.scope == target.scope
                    && existing.read(cx).snapshot.operation() == snapshot.operation()
                    && existing
                        .read(cx)
                        .snapshot
                        .repository()
                        .check_lifetime()
                        .is_ok()
                    && previous.iter().all(|entry| {
                        let entry = entry.read(cx);
                        entry.error().is_none()
                            && entry.snapshot().is_some_and(|snapshot| {
                                snapshot.repository().check_lifetime().is_ok()
                            })
                    })
                {
                    if mode != DiffOpenMode::Hidden {
                        owner.update(cx, |workspace, cx| {
                            workspace.activate_item(&existing, true, true, window, cx);
                        });
                    }
                    return Task::ready(Ok(existing));
                }
            }
        }
        if mode != DiffOpenMode::Hidden && saved_buffers.is_empty() && previous.is_empty() {
            let Some(owner) = workspace.upgrade() else {
                return Task::ready(Err(anyhow::anyhow!("Workspace was closed")));
            };
            return window.spawn(cx, async move |cx| {
                owner.update_in(cx, |owner, window, cx| {
                    let view = if let Some(view) = existing_view {
                        view.update(cx, |view, _| {
                            view.snapshot = snapshot.clone();
                            view.target = target.clone();
                            view.error = None;
                        });
                        view
                    } else {
                        let project = owner.project().clone();
                        let panel = owner.panel::<super::kk_panel::KkPanel>(cx);
                        let workspace = cx.entity();
                        cx.new(|cx| {
                            Self::new(
                                snapshot,
                                target,
                                working_copy,
                                Vec::new(),
                                panel,
                                project,
                                workspace,
                                window,
                                cx,
                            )
                        })
                    };
                    view.update(cx, |view, cx| {
                        view.files = files.clone();
                        view.start_loading(files, window, cx);
                    });
                    if !owner.activate_item(&view, true, true, window, cx) {
                        owner.add_item_to_active_pane(
                            Box::new(view.clone()),
                            None,
                            true,
                            window,
                            cx,
                        );
                    }
                    view
                })
            });
        }
        window.spawn(cx, async move |cx| {
            let mut entries = Vec::with_capacity(files.len());
            for files in files.chunks(8) {
                let tasks = cx.update(|window, cx| {
                    files
                        .iter()
                        .map(|file| {
                            let saved = saved_buffers.remove(&file.path);
                            let (entry_snapshot, entry_target) = if let Some(source) =
                                saved.as_ref().and_then(|saved| saved.source.as_ref())
                            {
                                let entry_snapshot = match snapshot
                                    .repository()
                                    .snapshot_at(source.operation.clone())
                                {
                                    Ok(snapshot) => snapshot,
                                    Err(error) => return Task::ready(Err(error)),
                                };
                                (entry_snapshot, source.target.clone())
                            } else {
                                (snapshot.clone(), target.clone())
                            };
                            let existing = previous
                                .iter()
                                .find(|entry| entry.read(cx).file.path == file.path)
                                .cloned();
                            KkDiff::load_for_multibuffer(
                                entry_snapshot,
                                entry_target,
                                file.clone(),
                                working_copy,
                                workspace.clone(),
                                saved.map(|saved| saved.contents),
                                existing,
                                window,
                                cx,
                            )
                        })
                        .collect::<Vec<_>>()
                })?;
                entries.extend(futures::future::try_join_all(tasks).await?);
            }
            let mut target = target;
            if !working_copy && target.revision.is_none() {
                target.revision = cx.update(|_, cx| {
                    entries
                        .iter()
                        .find_map(|entry| entry.read(cx).target().revision.clone())
                })?;
                if target.revision.is_none()
                    && let kk_workbench::DiffScope::Change(commit) = &target.scope
                {
                    let commit = commit.clone();
                    let snapshot = snapshot.clone();
                    target.revision = Some(
                        cx.background_spawn(async move { snapshot.exact_revision(&commit).await })
                            .await?,
                    );
                }
            }
            let workspace = workspace.upgrade().context("Workspace was closed")?;
            workspace.update_in(cx, |workspace, window, cx| {
                let existing = workspace.items_of_type::<Self>(cx).find(|item| {
                    let view = item.read(cx);
                    view.snapshot.repository().root() == snapshot.repository().root()
                        && view.target.identity == target.identity
                        && view.working_copy == working_copy
                        && (working_copy || view.target.scope == target.scope)
                });
                let view = if let Some(existing) = existing {
                    existing.update(cx, |view, cx| {
                        view.refresh_generation += 1;
                        view.refresh_task = Task::ready(());
                        view.refresh_snapshot = None;
                        view.pending_target = None;
                        view.error = None;
                        view.loading = false;
                        view.files = files.clone();
                        view.snapshot = snapshot;
                        view.target = target;
                        view.set_entries(entries, cx);
                    });
                    existing
                } else {
                    let project = workspace.project().clone();
                    let panel = workspace.panel::<super::kk_panel::KkPanel>(cx);
                    let workspace = cx.entity();
                    cx.new(|cx| {
                        Self::new(
                            snapshot,
                            target,
                            working_copy,
                            entries,
                            panel,
                            project,
                            workspace,
                            window,
                            cx,
                        )
                    })
                };
                if mode != DiffOpenMode::Hidden {
                    if !workspace.activate_item(&view, true, true, window, cx) {
                        workspace.add_item_to_active_pane(
                            Box::new(view.clone()),
                            None,
                            true,
                            window,
                            cx,
                        );
                    }
                }
                view
            })
        })
    }

    fn start_loading(
        &mut self,
        files: Vec<ChangedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loading = true;
        self.pending_target = None;
        self.initial_reveal_pending = self.entries.is_empty();
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let snapshot = self.snapshot.clone();
        let target = self.target.clone();
        let working_copy = self.working_copy;
        let workspace = self.workspace.clone();
        self.refresh_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let mut remaining = files.into_iter();
                let mut pending = futures::stream::FuturesOrdered::new();
                let mut errors = Vec::new();
                loop {
                    while pending.len() < 8 {
                        let Some(file) = remaining.next() else {
                            break;
                        };
                        let path = file.path.clone();
                        let task = cx.update(|window, cx| {
                            KkDiff::load_for_multibuffer(
                                snapshot.clone(),
                                target.clone(),
                                file,
                                working_copy,
                                workspace.clone(),
                                None,
                                None,
                                window,
                                cx,
                            )
                        })?;
                        pending.push_back(async move { (path, task.await) }.boxed_local());
                    }
                    let Some((path, entry)) = pending.next().await else {
                        break;
                    };
                    snapshot.repository().check_lifetime()?;
                    let current = this.update_in(cx, |this, window, cx| {
                        if this.refresh_generation != generation {
                            return false;
                        }
                        match entry {
                            Ok(entry) => {
                                let mut entries = this.entries.clone();
                                entries.push(entry);
                                this.set_entries(entries, cx);
                                if this.initial_reveal_pending {
                                    this.reveal_first_change(window, cx);
                                }
                            }
                            Err(error) => {
                                errors.push(format!("{path}: {error:#}"));
                                this.error = Some(errors.join("\n"));
                                cx.notify();
                            }
                        }
                        true
                    })?;
                    if !current {
                        return Ok(());
                    }
                    futures_lite::future::yield_now().await;
                }
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if this.refresh_generation == generation {
                    this.loading = false;
                    if let Err(error) = result {
                        this.error = Some(format!("Could not load diff: {error:#}"));
                    }
                    cx.notify();
                }
            })
            .log_err();
        });
        cx.notify();
    }

    fn new(
        snapshot: Snapshot,
        target: DiffTarget,
        working_copy: bool,
        entries: Vec<Entity<KkDiff>>,
        panel: Option<Entity<super::kk_panel::KkPanel>>,
        project: Entity<project::Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let showing_full_file = EditorSettings::get_global(cx).file_diff.show_full_file;
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(if working_copy {
                Capability::ReadWrite
            } else {
                Capability::ReadOnly
            });
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace.clone(),
                window,
                cx,
            );
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_diff_review_button(true, cx);
                editor.set_allow_git_diff_scrollbar_markers(showing_full_file, cx);
            });
            editor
        });
        let view = cx.weak_entity();
        let controls = cx.new(|cx| {
            let mut controls =
                KkDiffControls::new(editor.clone(), showing_full_file, workspace.downgrade(), cx);
            controls.generic_style_controls = true;
            controls.aggregate = true;
            controls.comparison_view = Some(view);
            controls
        });
        let mut subscriptions = vec![
            cx.subscribe(&editor, |this: &mut Self, _, event: &EditorEvent, cx| {
                if this.loading
                    && !this.entries.is_empty()
                    && matches!(
                        event,
                        EditorEvent::SelectionsChanged { local: true }
                            | EditorEvent::ScrollPositionChanged {
                                local: true,
                                autoscroll: false
                            }
                    )
                {
                    this.initial_reveal_pending = false;
                }
                this.attach_addons(cx);
                cx.emit(event.clone());
            }),
            cx.observe(&editor, |this, _, cx| this.attach_addons(cx)),
        ];
        if let Some(panel) = &panel {
            subscriptions
                .push(cx.observe(panel, |this: &mut Self, _, cx| this.update_file_order(cx)));
        }
        subscriptions.push(cx.observe_new({
            let workspace = workspace.downgrade();
            let view = cx.weak_entity();
            move |panel: &mut super::kk_panel::KkPanel, _, cx| {
                if !panel.is_for_workspace(&workspace) {
                    return;
                }
                let panel = cx.entity();
                let view = view.clone();
                cx.defer(move |cx| {
                    view.update(cx, |view, cx| {
                        view.panel = Some(panel.downgrade());
                        view._subscriptions
                            .push(cx.observe(&panel, |view, _, cx| view.update_file_order(cx)));
                        view.update_file_order(cx);
                    })
                    .log_err();
                });
            }
        }));
        let mut view = Self {
            snapshot,
            target,
            working_copy,
            entries: Vec::new(),
            files: entries
                .iter()
                .map(|entry| entry.read(cx).file.clone())
                .collect(),
            panel: panel.as_ref().map(Entity::downgrade),
            loading: false,
            initial_reveal_pending: false,
            editor,
            controls,
            workspace: workspace.downgrade(),
            window: window.window_handle(),
            refresh_generation: 0,
            refresh_snapshot: None,
            pending_target: None,
            refresh_task: Task::ready(()),
            error: None,
            _entry_subscriptions: Vec::new(),
            writes: Task::ready(Ok(())).shared(),
            _subscriptions: subscriptions,
        };
        view.set_entries(entries, cx);
        view.attach_addons(cx);
        view.reveal_first_change(window, cx);
        view
    }

    fn reveal_first_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let right = self.editor.read(cx).rhs_editor().clone();
        let first = right
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx)
            .diff_hunks()
            .next()
            .map(|hunk| language::Point::new(hunk.row_range.start.0, 0));
        if let Some(first) = first {
            self.initial_reveal_pending = false;
            right.update(cx, |right, cx| {
                right.change_selections(
                    editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([first..first]),
                )
            });
        }
    }

    fn set_entries(&mut self, mut entries: Vec<Entity<KkDiff>>, cx: &mut Context<Self>) {
        for entry in &self.entries {
            if entries
                .iter()
                .any(|candidate| candidate.read(cx).buffer == entry.read(cx).buffer)
            {
                continue;
            }
            if entry.read(cx).buffer.read(cx).is_dirty()
                || entry.read(cx).has_pending_classification()
            {
                entry.update(cx, |entry, cx| entry.refresh(self.snapshot.clone(), cx));
                if let Some(candidate) = entries
                    .iter_mut()
                    .find(|candidate| candidate.read(cx).file.path == entry.read(cx).file.path)
                {
                    if candidate.read(cx).buffer != entry.read(cx).buffer {
                        *candidate = entry.clone();
                    }
                } else {
                    entries.push(entry.clone());
                }
            }
        }
        self._entry_subscriptions = entries
            .iter()
            .map(|entry| cx.observe(entry, |_, _, cx| cx.notify()))
            .collect();
        for old in &self.entries {
            if !entries
                .iter()
                .any(|entry| entry.read(cx).buffer == old.read(cx).buffer)
            {
                let path = self
                    .editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .path_for_buffer(old.read(cx).buffer.read(cx).remote_id())
                    .cloned();
                let Some(path) = path else {
                    continue;
                };
                self.editor
                    .update(cx, |editor, cx| editor.remove_excerpts_for_path(path, cx));
            }
        }
        let buffers = entries
            .iter()
            .map(|entry| {
                let entry = entry.read(cx);
                (entry.buffer.clone(), entry.diff.clone(), entry.is_text())
            })
            .collect();
        self.controls.update(cx, |controls, cx| {
            controls.set_buffers(buffers, cx);
            controls.update_excerpts(false, cx);
        });
        self.entries = entries;
        self.update_file_order(cx);
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn update_file_order(&mut self, cx: &mut Context<Self>) {
        let mut files = self.files.clone();
        for entry in &self.entries {
            let file = &entry.read(cx).file;
            if !files.iter().any(|candidate| candidate.path == file.path) {
                files.push(file.clone());
            }
        }
        let ordered = if let Some(panel) = self.panel.as_ref().and_then(WeakEntity::upgrade) {
            panel.read(cx).file_order().files(&files)
        } else {
            files.iter().collect()
        };
        let buffers = ordered
            .into_iter()
            .filter_map(|file| {
                self.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == file.path)
                    .map(|entry| entry.read(cx).buffer.read(cx).remote_id())
            })
            .collect::<Vec<_>>();
        self.controls
            .update(cx, |controls, cx| controls.set_order(buffers, cx));
    }

    fn attach_addons(&self, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let editors = std::iter::once(editor.rhs_editor().clone())
            .chain(editor.lhs_editor().cloned())
            .collect::<Vec<_>>();
        for editor in editors {
            if editor.read(cx).addon::<MultiDiffAddon>().is_none() {
                let view = cx.weak_entity();
                editor.update(cx, |editor, _| editor.register_addon(MultiDiffAddon(view)));
            }
        }
    }

    pub(super) fn resolve_classification_uncertainty(
        &mut self,
        snapshot: &Snapshot,
        applied: bool,
        cx: &mut Context<Self>,
    ) {
        for entry in &self.entries {
            entry.update(cx, |entry, cx| {
                entry.resolve_classification_uncertainty(snapshot, applied, cx)
            });
        }
    }

    pub fn refresh(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        self.refresh_with_target(snapshot, None, cx);
    }

    pub(super) fn set_comparison_base(
        &mut self,
        revision: kk_workbench::Revision,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .entries
            .iter()
            .any(|entry| entry.read(cx).has_pending_classification())
        {
            self.error = Some(
                "Wait for the pending KK operation before changing the comparison base".into(),
            );
            cx.notify();
            return;
        }
        let target = super::kk_comparison::Comparison::new(&revision).diff_target();
        let snapshot = self
            .refresh_snapshot
            .as_ref()
            .unwrap_or(&self.snapshot)
            .clone();
        self.refresh_with_target(&snapshot, Some(target), cx);
    }

    fn refresh_with_target(
        &mut self,
        snapshot: &Snapshot,
        target: Option<DiffTarget>,
        cx: &mut Context<Self>,
    ) {
        let requested_retarget = target.is_some();
        let target = target.or_else(|| self.pending_target.clone());
        let retarget = target.is_some();
        if self.snapshot.repository().root() != snapshot.repository().root()
            || (!requested_retarget
                && self.refresh_snapshot.as_ref().is_some_and(|refresh| {
                    refresh.operation() == snapshot.operation()
                        && refresh.repository().check_lifetime().is_ok()
                }))
        {
            return;
        }
        for entry in &self.entries {
            if entry.read(cx).buffer.read(cx).is_dirty()
                || entry.read(cx).has_pending_classification()
            {
                entry.update(cx, |entry, cx| entry.refresh(snapshot.clone(), cx));
            }
        }
        if !retarget
            && self.snapshot.operation() == snapshot.operation()
            && self.snapshot.repository().check_lifetime().is_ok()
            && self.error.is_none()
            && self.entries.iter().all(|entry| {
                let entry = entry.read(cx);
                entry.error().is_none()
                    && entry
                        .snapshot()
                        .is_some_and(|snapshot| snapshot.repository().check_lifetime().is_ok())
            })
        {
            return;
        }
        self.refresh_generation += 1;
        if retarget {
            self.loading = true;
            cx.notify();
        }
        self.refresh_snapshot = Some(snapshot.clone());
        self.pending_target = target.clone();
        let generation = self.refresh_generation;
        let snapshot = snapshot.clone();
        let target = target.unwrap_or_else(|| self.target.clone());
        let workspace = self.workspace.clone();
        let window = self.window;
        let working_copy = self.working_copy;
        let previous = self.entries.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            let result = async {
                let (snapshot, target, mut files) = cx
                    .background_spawn(async move {
                        snapshot.repository().check_lifetime()?;
                        if retarget && let Some(intent) = target.comparison.clone() {
                            let comparison =
                                super::kk_comparison::Comparison::load(&snapshot, intent).await?;
                            Ok((
                                snapshot,
                                comparison.diff_target(),
                                comparison.files,
                            ))
                        } else {
                            let (target, _) = target.resolve(&snapshot).await?;
                            let files = snapshot.changed_files(&target.scope).await?;
                            anyhow::Ok((snapshot, target, files))
                        }
                    })
                    .await?;
                let previous_by_path = cx.update(|cx| {
                    previous.iter().map(|entry| {
                        (entry.read(cx).file.path.clone(), entry.clone())
                    }).collect::<HashMap<_, _>>()
                });
                if retarget {
                    let extras = cx.update(|cx| previous.iter().filter_map(|entry| {
                        let entry = entry.read(cx);
                        if !entry.buffer.read(cx).is_dirty() { return None; }
                        let path = entry.working_path(cx)?.strip_prefix(snapshot.repository().root()).ok()?.to_str()?.to_owned();
                        (!files.iter().any(|file| file.path == path)).then_some(ChangedFile {
                            path, change_type: kk_workbench::ChangeKind::Modified, renamed_from: None, has_conflict: entry.file.has_conflict,
                        })
                    }).collect::<Vec<_>>());
                    for mut file in extras {
                        if let kk_workbench::DiffScope::Comparison { base, .. } = &target.scope
                            && snapshot.file_information(base, &file.path).await?.exists == Some(false) {
                            file.change_type = kk_workbench::ChangeKind::Added;
                        }
                        files.push(file);
                    }
                }
                let opening = super::kk_panel::FileOpeningState::default().begin(&snapshot);
                let paths = cx.update(|cx| {
                    files
                        .iter()
                        .filter(|file| {
                            working_copy
                                && file.change_type != kk_workbench::ChangeKind::Deleted
                                && !previous_by_path.get(&file.path).is_some_and(|entry| entry.read(cx).file == **file)
                        })
                        .map(|file| file.path.clone())
                        .collect::<Vec<_>>()
                });
                let owner = workspace.upgrade().context("Workspace was closed")?;
                let panel = owner.read_with(cx, |workspace, cx| {
                    workspace.panel::<super::kk_panel::KkPanel>(cx)
                });
                let snapshot = if let Some(panel) = panel {
                    let preparation = window.update(cx, |_, window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.prepare_open(snapshot, paths, &opening, window, cx)
                        })
                    })?;
                    let Some(snapshot) = preparation
                        .await
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    else {
                        return Ok(());
                    };
                    snapshot
                } else {
                    snapshot
                };
                let mut entries = Vec::with_capacity(files.len());
                for (index, file) in files.iter().cloned().enumerate() {
                    if index % 32 == 0 {
                        futures_lite::future::yield_now().await;
                    }
                    let existing = previous_by_path.get(&file.path).cloned();
                    let retain = cx.update(|cx| {
                        !retarget
                            && existing.as_ref().is_some_and(|entry| {
                                let entry = entry.read(cx);
                                entry.file.path == file.path
                                    && entry.working_path(cx).is_none_or(|path| {
                                        path == snapshot.repository().root().join(&file.path)
                                    })
                                    && (entry.buffer.read(cx).is_dirty()
                                        || entry.has_pending_classification())
                            })
                    });
                    if retain {
                        if let Some(entry) = existing {
                            entries.push(entry);
                        }
                    } else {
                        if !opening.is_current() {
                            return Ok(());
                        }
                        let task = window.update(cx, |_, window, cx| {
                            KkDiff::load_for_multibuffer(
                                snapshot.clone(),
                                target.clone(),
                                file,
                                working_copy,
                                workspace.clone(),
                                None,
                                existing,
                                window,
                                cx,
                            )
                        })?;
                        entries.push(task.await?);
                    }
                }

                if !opening.is_current() {
                    return Ok(());
                }
                if retarget {
                    cx.update(|cx| {
                        for previous in &previous {
                            let previous = previous.read(cx);
                            if previous.buffer.read(cx).is_dirty() {
                                ensure!(entries.iter().any(|entry| entry.read(cx).buffer == previous.buffer),
                                    "Cannot represent unsaved edits in {} against this base. Save or discard them before changing the base", previous.file.path);
                            }
                        }
                        anyhow::Ok(())
                    })?;
                }
                this.update(cx, |this, cx| {
                    if this.refresh_generation == generation {
                        this.loading = false;
                        this.files = files;
                        this.snapshot = snapshot;
                        this.refresh_snapshot = None;
                        this.target = target;
                        this.pending_target = None;
                        this.error = None;
                        this.set_entries(entries, cx);
                    }
                })?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if this.refresh_generation == generation && this.refresh_snapshot.is_some() {
                    this.loading = false;
                    this.refresh_snapshot = None;
                    this.pending_target = None;
                    if let Err(error) = result {
                        let action = if retarget { "change comparison base" } else { "refresh diff" };
                        this.error = Some(format!("Could not {action}: {error:#}"));
                    }
                    cx.notify();
                }
            })
            .log_err();
        });
    }
}

impl EventEmitter<EditorEvent> for KkMultiDiff {}

impl Focusable for KkMultiDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for KkMultiDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let errors = self
            .entries
            .iter()
            .filter_map(|entry| entry.read(cx).error().map(str::to_owned))
            .collect::<Vec<_>>();
        self.controls.update(cx, |controls, cx| {
            controls.set_context(
                self.snapshot.clone(),
                self.target.clone(),
                self.workspace.clone(),
                cx,
            );
        });
        v_flex()
            .size_full()
            .when_some(self.error.clone(), |view, error| {
                view.child(Label::new(error).color(Color::Error))
            })
            .children(
                errors
                    .into_iter()
                    .map(|error| Label::new(error).color(Color::Error)),
            )
            .when(self.loading && self.entries.is_empty(), |view| {
                view.child(
                    div()
                        .flex_1()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Custom(TextSize::Large.rems(cx)))
                                .color(Color::Accent)
                                .with_rotate_animation(3),
                        ),
                )
            })
            .when(
                self.entries.is_empty() && !self.loading && self.error.is_none(),
                |view| view.child(Label::new("No changes").color(Color::Muted)),
            )
            .child(self.controls.clone())
            .when_some(self.pending_target.as_ref(), |view, target| {
                view.child(
                    h_flex()
                        .px_2()
                        .gap_1()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                                .with_rotate_animation(3),
                        )
                        .child(
                            Label::new(format!(
                                "Switching base to {}…",
                                target.label.trim_end_matches(" → @")
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
            })
            .when(!self.entries.is_empty() || !self.loading, |view| {
                view.child(div().flex_1().min_h_0().child(self.editor.clone()))
            })
    }
}

impl Item for KkMultiDiff {
    type Event = EditorEvent;

    fn can_split(&self) -> bool {
        !self.loading && self.error.is_none()
    }

    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        if !self.can_split() {
            return Task::ready(None);
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let Some(project) = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .project()
            .cloned()
        else {
            return Task::ready(None);
        };
        let style = self.editor.read(cx).diff_view_style();
        let full_file = self.controls.read(cx).showing_full_file;
        let layout = self.controls.read(cx).capture_excerpt_layout(cx);
        let state = self
            .editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, SerializedEditorState::capture);
        Task::ready(Some(cx.new(|cx| {
            let mut view = Self::new(
                self.snapshot.clone(),
                self.target.clone(),
                self.working_copy,
                self.entries.clone(),
                self.panel.as_ref().and_then(WeakEntity::upgrade),
                project,
                workspace,
                window,
                cx,
            );
            view.files = self.files.clone();
            view.controls.update(cx, |controls, cx| {
                controls.set_full_file(full_file, cx);
                controls.restore_excerpt_layout(layout, cx);
            });
            view.editor.update(cx, |editor, cx| {
                if editor.diff_view_style() != style {
                    editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                }
                editor
                    .rhs_editor()
                    .update(cx, |editor, cx| state.restore(editor, window, cx));
            });
            view
        })))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!(
            "{} ({} files)",
            self.target.tab_label(),
            self.files.len().max(self.entries.len())
        )
        .into()
    }
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff))
    }
    fn to_item_events(event: &EditorEvent, callback: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, callback);
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<KkDiffControls>() {
            Some(self.controls.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }
    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }
    fn set_nav_history(
        &mut self,
        history: workspace::ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, _| editor.set_nav_history(Some(history)))
        });
    }
    fn navigate(
        &mut self,
        data: std::sync::Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| {
                window.focus(&editor.focus_handle(cx), cx);
                editor.navigate(data, window, cx)
            })
    }
    fn capability(&self, cx: &App) -> Capability {
        self.editor.read(cx).capability(cx)
    }
    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.editor.read(cx).has_deleted_file(cx)
    }
    fn reload(
        &mut self,
        project: Entity<project::Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffers = self
            .entries
            .iter()
            .map(|entry| entry.read(cx).buffer.clone())
            .filter(|buffer| project::File::from_dyn(buffer.read(cx).file()).is_some())
            .collect();
        let multibuffer = self.editor.read(cx).rhs_editor().read(cx).buffer().clone();
        let reload = project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn(async move |this, cx| {
            let transaction = reload.await?;
            multibuffer.update(cx, |buffer, cx| buffer.push_transaction(&transaction.0, cx));
            this.update(cx, |_, cx| cx.notify())?;
            Ok(())
        })
    }
    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, callback);
    }
}

impl SerializableItem for KkMultiDiff {
    fn serialized_item_kind() -> &'static str {
        "KKMultiDiff"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let store = KeyValueStore::global(cx);
        let namespace = session_namespace(workspace_id);
        let alive: HashSet<_> = alive_items
            .into_iter()
            .map(|item| item.to_string())
            .collect();
        cx.background_spawn(async move {
            let store = store.scoped(&namespace);
            for key in store.keys()? {
                if !alive.contains(&key) {
                    store.delete(key).await?;
                }
            }
            Ok(())
        })
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let store = KeyValueStore::global(cx);
        let namespace = session_namespace(workspace_id);
        let remote = project.read(cx).kk_workbench_client(cx);
        let host = project
            .read(cx)
            .remote_connection_options(cx)
            .map(|options| options.host_key());
        let executable = super::kk_settings::KkWorkbenchSettings::get_global(cx)
            .executable_path
            .clone();
        let restore_unsaved = project::project_settings::ProjectSettings::get_global(cx)
            .session
            .restore_unsaved_buffers;
        window.spawn(cx, async move |cx| {
            let (saved, snapshot) = cx
                .background_spawn(async move {
                    let value = store
                        .scoped(&namespace)
                        .read(&item_id.to_string())?
                        .context("Saved KK multi-file diff is unavailable")?;
                    let saved: SavedMultiDiff = serde_json::from_str(&value)?;
                    ensure!(
                        saved.host == host,
                        "This diff belongs to another project host"
                    );
                    let repository = if let Some(remote) = remote {
                        remote
                            .open(
                                &saved.root,
                                &executable,
                                kk_workbench::Diagnostics::default(),
                            )
                            .await?
                    } else {
                        kk_workbench::Repository::open(&saved.root, &executable).await?
                    };
                    let snapshot = repository.snapshot_at(saved.operation.clone())?;
                    anyhow::Ok((saved, snapshot))
                })
                .await?;
            let view = cx
                .update(|window, cx| {
                    Self::open_with_buffers(
                        snapshot,
                        saved.target,
                        saved.files,
                        saved.working_copy,
                        workspace,
                        DiffOpenMode::Hidden,
                        if restore_unsaved {
                            saved.drafts
                        } else {
                            Vec::new()
                        },
                        window,
                        cx,
                    )
                })?
                .await?;
            view.update_in(cx, |view, window, cx| {
                for entry in &view.entries {
                    entry.update(cx, |entry, cx| entry.refresh(view.snapshot.clone(), cx));
                }
                view.controls.update(cx, |controls, cx| {
                    if controls.showing_full_file != saved.full_file {
                        controls.toggle_full_file(cx);
                    }
                });
                view.editor.update(cx, |editor, cx| {
                    if (editor.diff_view_style() == settings::DiffViewStyle::Split) != saved.split {
                        editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                    }
                    if saved.split {
                        editor.split(window, cx);
                    }
                    let selected = if saved.left {
                        editor.lhs_editor().unwrap_or_else(|| editor.rhs_editor())
                    } else {
                        editor.rhs_editor()
                    };
                    selected.update(cx, |editor, cx| saved.editor.restore(editor, window, cx));
                });
            })?;
            Ok(view)
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let namespace = session_namespace(workspace.database_id()?);
        let host = workspace
            .project()
            .read(cx)
            .remote_connection_options(cx)
            .map(|options| options.host_key());
        let store = KeyValueStore::global(cx);
        let previous = self.writes.clone();
        let owner = cx.entity();
        self.writes = cx
            .spawn(async move |this, cx| {
                let _owner = owner;
                if let Err(error) = previous.await {
                    log::warn!("Earlier KK multi-file diff persistence failed: {error}");
                }
                let saved = this
                    .update(cx, |this, cx| {
                        let restore_unsaved =
                            project::project_settings::ProjectSettings::get_global(cx)
                                .session
                                .restore_unsaved_buffers;
                        let mut files = this
                            .entries
                            .iter()
                            .map(|entry| {
                                let entry = entry.read(cx);
                                let mut file = entry.file.clone();
                                if let Some(path) = entry.working_path(cx) {
                                    let path = path
                                        .strip_prefix(this.snapshot.repository().root())
                                        .context("Diff file moved outside its repository")?
                                        .to_str()
                                        .context("Diff file path is not UTF-8")?
                                        .to_owned();
                                    if path != file.path {
                                        // Native rename can precede KK refresh; retain its original base.
                                        file.renamed_from.get_or_insert(file.path.clone());
                                        file.path = path;
                                        if file.change_type != kk_workbench::ChangeKind::Added {
                                            file.change_type = kk_workbench::ChangeKind::Renamed;
                                        }
                                    }
                                }
                                anyhow::Ok(file)
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let drafts = this
                            .entries
                            .iter()
                            .zip(&files)
                            .filter_map(|(entry, file)| {
                                let entry = entry.read(cx);
                                let buffer = entry.buffer.read(cx);
                                (restore_unsaved
                                    && buffer.capability() == Capability::ReadWrite
                                    && buffer.is_dirty())
                                .then(|| {
                                    (
                                        file.path.clone(),
                                        SavedDraft {
                                            contents: SerializedBufferContents::capture(buffer),
                                            source: entry.snapshot().map(|snapshot| {
                                                SavedDraftSource {
                                                    operation: snapshot.operation().to_owned(),
                                                    target: entry.target().clone(),
                                                }
                                            }),
                                        },
                                    )
                                })
                            })
                            .collect();
                        files.extend(
                            this.files
                                .iter()
                                .filter(|file| {
                                    !this
                                        .entries
                                        .iter()
                                        .any(|entry| entry.read(cx).file.path == file.path)
                                })
                                .cloned(),
                        );
                        let editor = this.editor.read(cx);
                        let selected = editor.focused_editor().clone();
                        let split = editor.diff_view_style() == settings::DiffViewStyle::Split;
                        let left = editor.lhs_editor() == Some(&selected);
                        anyhow::Ok(SavedMultiDiff {
                            root: this.snapshot.repository().root().to_owned(),
                            target: this.target.clone(),
                            files,
                            working_copy: this.working_copy,
                            operation: this.snapshot.operation().to_owned(),
                            host,
                            drafts,
                            split,
                            left,
                            full_file: this.controls.read(cx).showing_full_file,
                            editor: selected.update(cx, SerializedEditorState::capture),
                        })
                    })
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                cx.background_spawn(async move {
                    store
                        .scoped(&namespace)
                        .write(item_id.to_string(), serde_json::to_string(&saved)?)
                        .await
                })
                .await
                .map_err(|error| error.to_string())
            })
            .shared();
        let write = self.writes.clone();
        cx.spawn(async move |_, _| write.await).detach();
        let write = self.writes.clone();
        Some(cx.spawn(async move |_, _| write.await.map_err(anyhow::Error::msg)))
    }

    fn should_serialize(&self, event: &EditorEvent) -> bool {
        matches!(
            event,
            EditorEvent::BufferEdited
                | EditorEvent::Edited { .. }
                | EditorEvent::TransactionUndone { .. }
                | EditorEvent::Saved
                | EditorEvent::DirtyChanged
                | EditorEvent::SelectionsChanged { .. }
                | EditorEvent::ScrollPositionChanged { .. }
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::kk_panel::remote_tests::{RemoteFixture, run_kk};
    use editor::display_map::ToDisplayPoint as _;
    use gpui::TestAppContext;
    use language::OffsetRangeExt as _;
    use project::{FakeFs, Fs, Project};
    use serde_json::json;
    use std::path::Path;
    use workspace::MultiWorkspace;

    pub(crate) async fn local_fixture(cx: &mut TestAppContext) -> tempfile::TempDir {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::kk_diff::init(cx);
        });
        cx.background_spawn(async {
            let directory = tempfile::tempdir()?;
            let root = directory.path().join("repo");
            std::fs::create_dir(&root)?;
            run_kk(&root, &["init", "."]).await?;
            run_kk(
                &root,
                &["config", "set", "--repo", "user.name", "KK Diff Test"],
            )
            .await?;
            run_kk(
                &root,
                &[
                    "config",
                    "set",
                    "--repo",
                    "user.email",
                    "test@example.invalid",
                ],
            )
            .await?;
            std::fs::write(root.join("a.txt"), "old a\n")?;
            std::fs::create_dir(root.join("repo"))?;
            std::fs::write(root.join("repo/a.txt"), "old nested\n")?;
            std::fs::write(root.join("mixed.bin"), "old text\n")?;
            run_kk(&root, &["new", "-m", "changes"]).await?;
            std::fs::write(root.join("a.txt"), "new a\n")?;
            std::fs::remove_file(root.join("repo/a.txt"))?;
            std::fs::write(root.join("mixed.bin"), b"new\0binary")?;
            anyhow::Ok(directory)
        })
        .await
        .expect("prepare nested repository")
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_restored_deletion_keeps_reviewed_context(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let original = (0..80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(root.join("a.txt"), &original).expect("review base");
        run_kk(&root, &["new", "-m", "deletion review base"])
            .await
            .expect("freeze base");
        let modified = original.replace("line 60\n", "");
        std::fs::write(root.join("a.txt"), &modified).expect("deleted line");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
        let files = snapshot.changed_files(&target.scope).await.expect("files");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, json!({"a.txt":modified})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let view = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot,
                    target,
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("deletion diff");
        cx.condition(&view, |view, _| !view.loading).await;
        let (editor, controls, entry) = view.read_with(cx, |view, _| {
            (
                view.editor.clone(),
                view.controls.clone(),
                view.entries.first().expect("review entry").clone(),
            )
        });
        controls.update(cx, |controls, cx| controls.set_full_file(true, cx));
        editor.update_in(cx, |editor, window, cx| {
            if editor.diff_view_style() != settings::DiffViewStyle::Unified {
                editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
            }
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.snapshot(window, cx);
                let hunk = snapshot
                    .buffer_snapshot()
                    .diff_hunks()
                    .next()
                    .expect("deleted hunk");
                let row = language::Point::new(hunk.row_range.start.0, 0)
                    .to_display_point(&snapshot.display_snapshot)
                    .row();
                let source = snapshot.buffer_snapshot();
                let start = language::Point::new(hunk.row_range.start.0, 0);
                let end = language::Point::new(
                    start.row,
                    source.line_len(multi_buffer::MultiBufferRow(start.row)),
                );
                assert!(
                    source.range_to_buffer_ranges(start..end).is_empty(),
                    "selection is wholly deleted text"
                );
                assert!(
                    source.range_to_buffer_ranges(end..end).is_empty(),
                    "endpoint is inside the deleted line"
                );
                right.show_diff_review_overlay(row..row, window, cx);
                let prompt = right
                    .diff_review_prompt_editor()
                    .expect("review draft")
                    .clone();
                prompt.update(cx, |prompt, cx| {
                    prompt.insert("Keep the deleted line in view", window, cx)
                });
                right.submit_diff_review_comment(window, cx);
            });
        });
        controls.update_in(cx, |controls, window, cx| controls.restore_all(window, cx));
        cx.condition(&entry, |entry, cx| {
            entry.diff.read(cx).changed_row_counts() == (0, 0)
        })
        .await;
        controls.update(cx, |controls, cx| controls.set_full_file(false, cx));
        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| {
            let right = editor.rhs_editor().read(cx);
            assert!(
                right
                    .diff_review_text(cx)
                    .contains("Keep the deleted line in view")
            );
            let buffer = entry.read(cx).buffer.read(cx).snapshot();
            let snapshot = right.buffer().read(cx).snapshot(cx);
            assert!(
                snapshot
                    .excerpts_for_buffer(buffer.remote_id())
                    .any(|excerpt| {
                        let range = excerpt.context.to_point(&buffer);
                        range.start.row <= 60 && range.end.row >= 60
                    }),
                "the restored reviewed row stays in changes-only excerpts"
            );
        });
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_legacy_history_hydrates_details(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        run_kk(&root, &["new", "-m", "after historical revision"])
            .await
            .expect("freeze revision");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let revision = snapshot
            .exact_revision("@-")
            .await
            .expect("history revision");
        let target = DiffTarget::change(&revision);
        let files = snapshot.changed_files(&target.scope).await.expect("files");
        let mut legacy = serde_json::to_value(&target).expect("target JSON");
        legacy.as_object_mut().expect("target").remove("revision");
        let legacy: DiffTarget = serde_json::from_value(legacy).expect("old target format");
        assert!(legacy.revision.is_none());
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, json!({"a.txt":"new a\n"})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        for files in [files, Vec::new()] {
            let view = cx
                .update(|window, cx| {
                    KkMultiDiff::open_with_buffers(
                        snapshot.clone(),
                        legacy.clone(),
                        files,
                        false,
                        workspace.downgrade(),
                        DiffOpenMode::Hidden,
                        Vec::new(),
                        window,
                        cx,
                    )
                })
                .await
                .expect("legacy historical restoration");
            view.read_with(cx, |view, _| {
                let restored = view.target.revision.as_ref().expect("restored details");
                assert_eq!(restored.full_commit_id, revision.full_commit_id);
                assert_eq!(restored.author_email, revision.author_email);
                assert!(restored.commit_timestamp.is_some());
            });
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_loading_order_and_pane_clone(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        std::fs::create_dir_all(root.join("a")).expect("a directory");
        std::fs::create_dir_all(root.join("z")).expect("z directory");
        std::fs::write(root.join("a/z.txt"), "last name\n").expect("z file");
        std::fs::write(root.join("z/a.txt"), "first name\n").expect("a file");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
        let files = snapshot.changed_files(&target.scope).await.expect("files");
        let count = files.len();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, json!({"a.txt":"new a\n", "repo":{}, "mixed.bin":"new\0binary", "a":{"z.txt":"last name\n"}, "z":{"a.txt":"first name\n"}})).await;
        let project = Project::test(fs, [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let panel = cx
            .update(|window, cx| {
                super::super::kk_panel::KkPanel::load(workspace.downgrade(), window.to_async(cx))
            })
            .await
            .expect("panel");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx)
        });
        let view = panel
            .update_in(cx, |_, window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    target,
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("loading diff");
        view.read_with(cx, |view, _| {
            assert!(view.loading);
            assert!(view.entries.is_empty());
            assert_eq!(view.files.len(), count);
            assert!(!view.can_split());
        });
        let other = cx.new_window_entity(|window, cx| Editor::single_line(window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(other.clone()), None, true, window, cx)
        });
        cx.condition(&view, |view, _| !view.loading).await;
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_item(cx)
                .map(|item| item.item_id())),
            Some(other.entity_id())
        );
        view.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.entries.len(), count);
            assert!(view.can_split());
        });
        panel.update(cx, |panel, cx| {
            panel.set_file_order(super::super::kk_file_order::FileOrder::FileName, cx)
        });
        cx.run_until_parked();
        let order = view.read_with(cx, |view, cx| {
            let snapshot = view
                .editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx);
            snapshot
                .excerpts()
                .map(|range| range.context.start.buffer_id)
                .map(|id| {
                    view.entries
                        .iter()
                        .find(|entry| entry.read(cx).buffer.read(cx).remote_id() == id)
                        .expect("entry")
                        .read(cx)
                        .file
                        .path
                        .clone()
                })
                .collect::<Vec<_>>()
        });
        assert!(
            order.iter().position(|path| path == "z/a.txt")
                < order.iter().position(|path| path == "a/z.txt"),
            "{order:?}"
        );
        let clone = view
            .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
            .await
            .expect("pane clone");
        assert_ne!(
            view.read_with(cx, |view, _| view.editor.clone()),
            clone.read_with(cx, |view, _| view.editor.clone())
        );
        assert_eq!(clone.read_with(cx, |view, _| view.entries.len()), count);
        panel.update(cx, |panel, cx| {
            panel.set_file_order(super::super::kk_file_order::FileOrder::Repository, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let snapshot = view
                .editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx);
            let ids = snapshot
                .excerpts()
                .map(|range| range.context.start.buffer_id)
                .collect::<Vec<_>>();
            let id_for = |path| {
                view.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == path)
                    .expect("entry")
                    .read(cx)
                    .buffer
                    .read(cx)
                    .remote_id()
            };
            assert!(
                ids.iter().position(|id| *id == id_for("a/z.txt"))
                    < ids.iter().position(|id| *id == id_for("z/a.txt"))
            );
        });
        let parent = snapshot.exact_revision("@-").await.expect("base revision");
        let comparison = super::super::kk_comparison::Comparison::load(
            &snapshot,
            super::super::kk_comparison::Comparison::new(&parent).intent,
        )
        .await
        .expect("comparison");
        let draft = view.read_with(cx, |view, cx| {
            view.entries
                .iter()
                .find(|entry| entry.read(cx).file.path == "a.txt")
                .expect("draft entry")
                .read(cx)
                .buffer
                .clone()
        });
        draft.update(cx, |buffer, cx| buffer.edit([(0..0, "draft\n")], None, cx));
        view.update_in(cx, |view, window, cx| {
            view.set_comparison_base(parent, window, cx)
        });
        cx.condition(&view, |view, _| view.refresh_snapshot.is_none())
            .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.target.scope, comparison.scope());
            assert!(
                view.entries
                    .iter()
                    .any(|entry| entry.read(cx).buffer == draft)
            );
            assert_eq!(draft.read(cx).text(), "draft\nnew a\n");
        });
        let current = snapshot.exact_revision("@").await.expect("current base");
        let comparison = super::super::kk_comparison::Comparison::load(
            &snapshot,
            super::super::kk_comparison::Comparison::new(&current).intent,
        )
        .await
        .expect("current comparison");
        assert!(comparison.files.is_empty());
        view.update_in(cx, |view, window, cx| {
            view.set_comparison_base(current, window, cx)
        });
        cx.condition(&view, |view, _| view.refresh_snapshot.is_none())
            .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.target.scope, comparison.scope());
            assert_eq!(view.entries.len(), 1);
            let entry = view.entries.first().expect("unsaved file").read(cx);
            assert_eq!(entry.buffer, draft);
            assert_eq!(
                entry.diff.read(cx).base_text_buffer().read(cx).text(),
                "new a\n"
            );
        });
        view.update_in(cx, |view, window, cx| {
            view.controls
                .update(cx, |controls, cx| controls.restore_all(window, cx))
        });
        assert_eq!(draft.read_with(cx, |buffer, _| buffer.text()), "new a\n");
        let selected_base = snapshot.exact_revision("@-").await.expect("selected base");
        std::fs::write(root.join("a.txt"), "saved while the base is loading\n")
            .expect("later save");
        let next = repository
            .snapshot(true)
            .await
            .expect("save-triggered snapshot");
        assert_ne!(snapshot.operation(), next.operation());
        let expected = super::super::kk_comparison::Comparison::load(
            &next,
            super::super::kk_comparison::Comparison::new(&selected_base).intent,
        )
        .await
        .expect("new base on latest snapshot");
        view.update_in(cx, |view, window, cx| {
            view.set_comparison_base(selected_base, window, cx);
            view.refresh(&next, cx);
        });
        cx.condition(&view, |view, _| view.refresh_snapshot.is_none())
            .await;
        view.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.target.scope, expected.scope());
            assert_eq!(view.snapshot.operation(), next.operation());
            assert!(view.pending_target.is_none());
        });
        std::fs::write(
            root.join("introduced-later.txt"),
            "file introduced by the new snapshot\n",
        )
        .expect("new saved file");
        let newest = repository
            .snapshot(true)
            .await
            .expect("newer refresh snapshot");
        assert_ne!(next.operation(), newest.operation());
        let another_base = snapshot
            .exact_revision("root()")
            .await
            .expect("another available base");
        let expected = super::super::kk_comparison::Comparison::load(
            &newest,
            super::super::kk_comparison::Comparison::new(&another_base).intent,
        )
        .await
        .expect("another base on newest snapshot");
        view.update_in(cx, |view, window, cx| {
            assert_ne!(view.snapshot.operation(), newest.operation());
            view.refresh(&newest, cx);
            view.set_comparison_base(another_base, window, cx);
        });
        cx.condition(&view, |view, _| view.refresh_snapshot.is_none())
            .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.snapshot.operation(), newest.operation());
            assert_eq!(view.target.scope, expected.scope());
            assert!(
                view.entries
                    .iter()
                    .any(|entry| entry.read(cx).file.path == "introduced-later.txt")
            );
        });
        let confirmed_scope = view.read_with(cx, |view, _| view.target.scope.clone());
        let kept_text = draft.read_with(cx, |buffer, _| buffer.text());
        let mut invalid_base = snapshot
            .exact_revision("@-")
            .await
            .expect("base to simulate stale selection");
        invalid_base.full_change_id = "no longer a valid revision".into();
        view.update_in(cx, |view, window, cx| {
            view.set_comparison_base(invalid_base, window, cx);
            assert!(view.pending_target.is_some());
            assert_eq!(
                view.target.scope, confirmed_scope,
                "contents keep the confirmed base while the choice is pending"
            );
        });
        cx.condition(&view, |view, _| view.refresh_snapshot.is_none())
            .await;
        view.read_with(cx, |view, _| {
            assert!(
                view.error
                    .as_ref()
                    .is_some_and(|error| error.contains("Could not change comparison base"))
            );
            assert_eq!(view.target.scope, confirmed_scope);
            assert!(view.pending_target.is_none());
        });
        assert_eq!(draft.read_with(cx, |buffer, _| buffer.text()), kept_text);
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_deleted_working_file_restores_edits_and_saves(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
        let files = snapshot
            .changed_files(&target.scope)
            .await
            .expect("changed files");
        let deleted = files
            .iter()
            .find(|file| file.path == "repo/a.txt")
            .expect("deleted file")
            .clone();
        let binary = files
            .iter()
            .find(|file| file.path == "mixed.bin")
            .expect("binary file")
            .clone();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            directory.path(),
            json!({"repo": {"a.txt": "new a\n", "repo": {}, "mixed.bin": "new\0binary"}}),
        )
        .await;
        let project = Project::test(fs.clone(), [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let view = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    deleted,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("deleted diff");
        let buffer = view.read_with(cx, |view, _| view.buffer.clone());
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.capability()),
            Capability::ReadWrite
        );
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "");
        let right = cx
            .read(|cx| (&view as &dyn workspace::ItemHandle).act_as::<Editor>(cx))
            .expect("working editor");
        right.update_in(cx, |right, window, cx| {
            assert!(right.can_restore_selected_hunks(window, cx));
            right.git_restore(&git::Restore, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "old nested\n"
        );
        right.update_in(cx, |right, window, cx| {
            right.undo(&editor::actions::Undo, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "");
        right.update_in(cx, |right, window, cx| {
            right.git_restore(&git::Restore, window, cx)
        });
        buffer.update(cx, |buffer, cx| {
            buffer.set_has_bom(true);
            buffer.set_line_ending(language::LineEnding::Windows, cx);
        });
        let aggregate = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    files.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("deleted aggregate");
        cx.condition(&aggregate, |view, _| !view.loading).await;
        aggregate
            .update_in(cx, |view, window, cx| {
                view.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard restored missing file");
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "");
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        aggregate.update_in(cx, |view, window, cx| {
            view.editor
                .read(cx)
                .rhs_editor()
                .clone()
                .update(cx, |right, cx| {
                    right.undo(&editor::actions::Undo, window, cx)
                })
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "old nested\n"
        );
        aggregate
            .update_in(cx, |view, window, cx| {
                view.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard before singleton Undo");
        buffer.update(cx, |buffer, cx| {
            assert!(buffer.undo(cx).is_some());
        });
        buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.text(), "old nested\n");
            assert!(
                buffer.is_dirty(),
                "Undo recovers unsaved contents of the missing file"
            );
            assert!(buffer.has_bom());
            assert_eq!(buffer.line_ending(), language::LineEnding::Windows);
        });
        view.update_in(cx, |view, window, cx| {
            view.save(Default::default(), project.clone(), window, cx)
        })
        .await
        .expect("save restored file");
        assert_eq!(
            fs.load(&root.join("repo/a.txt"))
                .await
                .expect("restored file"),
            "\u{feff}old nested\r\n"
        );
        let information = cx
            .update(|window, cx| {
                KkDiff::open(
                    snapshot,
                    target,
                    binary,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("binary information");
        let controls = cx
            .read(|cx| (&information as &dyn workspace::ItemHandle).act_as::<KkDiffControls>(cx))
            .expect("information controls");
        assert_eq!(
            controls.read_with(cx, |controls, cx| controls.statistics(cx)),
            None
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_rebinds_same_operation_after_repository_reopens(
        cx: &mut TestAppContext,
    ) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let lifetime = kk_workbench::RepositoryLifetime::default();
        let snapshot = repository
            .snapshot(true)
            .await
            .expect("snapshot")
            .with_lifetime(Some(lifetime.clone()));
        let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
        let files = snapshot.changed_files(&target.scope).await.expect("files");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            directory.path(),
            json!({"repo": {"a.txt": "new a\n", "repo": {}, "mixed.bin": "new\0binary"}}),
        )
        .await;
        let project = Project::test(fs, [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        lifetime.invalidate();
        let failed = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    files.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("failed loading page");
        cx.condition(&failed, |view, _| !view.loading).await;
        assert!(failed.read_with(cx, |view, _| view.error.is_some()
            && view.entries.is_empty()
            && !view.can_split()));
        let snapshot = snapshot.with_lifetime(Some(kk_workbench::RepositoryLifetime::default()));
        let view = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    files.clone(),
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("bound aggregate");
        cx.condition(&view, |view, _| !view.loading).await;
        assert_eq!(view, failed, "retry must reuse the existing failed tab");
        let buffer = view.read_with(cx, |view, cx| {
            view.entries
                .iter()
                .find(|entry| entry.read(cx).file.path == "a.txt")
                .expect("working entry")
                .read(cx)
                .buffer
                .clone()
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx)
        });
        lifetime.invalidate();
        let abandoned_lifetime = kk_workbench::RepositoryLifetime::default();
        let abandoned = snapshot.with_lifetime(Some(abandoned_lifetime.clone()));
        let replacement_lifetime = kk_workbench::RepositoryLifetime::default();
        let replacement = snapshot.with_lifetime(Some(replacement_lifetime.clone()));
        view.update(cx, |view, cx| {
            view.refresh(&abandoned, cx);
            abandoned_lifetime.invalidate();
            view.refresh(&replacement, cx);
        });
        cx.condition(&view, |view, cx| {
            view.error.is_some()
                || (view.snapshot.repository().check_lifetime().is_ok()
                    && view.entries.iter().all(|entry| {
                        entry
                            .read(cx)
                            .snapshot()
                            .is_some_and(|snapshot| snapshot.repository().check_lifetime().is_ok())
                    }))
        })
        .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(
                view.entries
                    .iter()
                    .any(|entry| entry.read(cx).buffer == buffer)
            );
        });
        replacement_lifetime.invalidate();
        let reopened = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.with_lifetime(Some(kk_workbench::RepositoryLifetime::default())),
                    target,
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("reopen same operation");
        cx.condition(&reopened, |view, _| !view.loading).await;
        assert_eq!(view, reopened);
        reopened.read_with(cx, |view, cx| {
            assert!(view.snapshot.repository().check_lifetime().is_ok());
            assert!(view.entries.iter().all(|entry| {
                entry
                    .read(cx)
                    .snapshot()
                    .is_some_and(|snapshot| snapshot.repository().check_lifetime().is_ok())
            }));
            assert!(
                view.entries
                    .iter()
                    .any(|entry| entry.read(cx).buffer == buffer)
            );
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "unsaved\nnew a\n"
        );
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_keeps_review_context_on_toggle_and_refresh(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let (repository, snapshot, target, files, first_text, second_text) = cx
            .background_spawn({
                let root = root.clone();
                async move {
                    let original = (0..80)
                        .map(|line| format!("line {line}\n"))
                        .collect::<String>();
                    std::fs::write(root.join("a.txt"), &original)?;
                    std::fs::write(root.join("b.txt"), format!("\n{original}"))?;
                    run_kk(&root, &["new", "-m", "review base"]).await?;
                    let first_text = original.replace("line 60\n", "changed 60\n");
                    let second_text = format!("\n{first_text}");
                    std::fs::write(root.join("a.txt"), &first_text)?;
                    std::fs::write(root.join("b.txt"), &second_text)?;
                    let repository = kk_workbench::Repository::open(&root, Path::new("kk")).await?;
                    let snapshot = repository.snapshot(true).await?;
                    let target = DiffTarget::change(&snapshot.exact_revision("@").await?);
                    let files = snapshot.changed_files(&target.scope).await?;
                    anyhow::Ok((repository, snapshot, target, files, first_text, second_text))
                }
            })
            .await
            .expect("review context repository");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(&root, json!({"a.txt": first_text, "b.txt": second_text}))
            .await;
        let project = Project::test(fs.clone(), [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let view = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot,
                    target,
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("aggregate review");
        view.update(cx, |view, cx| {
            view.controls.update(cx, |controls, cx| {
                if !controls.showing_full_file {
                    controls.toggle_full_file(cx);
                }
            })
        });
        cx.condition(&view, |view, _| !view.loading).await;
        view.update_in(cx, |view, window, cx| {
            view.editor
                .read(cx)
                .rhs_editor()
                .clone()
                .update(cx, |right, cx| {
                    let snapshot = right.snapshot(window, cx);
                    let first = snapshot
                        .buffer_snapshot()
                        .diff_hunks()
                        .next()
                        .expect("first change")
                        .row_range
                        .start
                        .0;
                    assert!(first >= 60, "full-file change is near the bottom");
                    assert_eq!(
                        right
                            .selections
                            .newest::<language::Point>(&snapshot.display_snapshot)
                            .head()
                            .row,
                        first
                    );
                })
        });
        let (editor, controls, first, second) = view.read_with(cx, |view, cx| {
            let buffer = |path| {
                view.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == path)
                    .expect("review source")
                    .read(cx)
                    .buffer
                    .clone()
            };
            (
                view.editor.clone(),
                view.controls.clone(),
                buffer("a.txt"),
                buffer("b.txt"),
            )
        });
        controls.update(cx, |controls, cx| {
            if !controls.showing_full_file {
                controls.toggle_full_file(cx);
            }
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.rhs_editor().update(cx, |right, cx| {
                let snapshot = right.snapshot(window, cx);
                let buffers = right.buffer().read(cx);
                let start = buffers
                    .buffer_point_to_anchor(&first, language::Point::new(5, 0), cx)
                    .expect("first file range");
                let end = buffers
                    .buffer_point_to_anchor(&second, language::Point::zero(), cx)
                    .expect("blank endpoint in second file");
                let start = start.to_display_point(&snapshot.display_snapshot).row();
                let end = end.to_display_point(&snapshot.display_snapshot).row();
                right.show_diff_review_overlay(start..end, window, cx);
                let prompt = right
                    .diff_review_prompt_editor()
                    .expect("review input")
                    .clone();
                prompt.update(cx, |prompt, cx| {
                    prompt.insert("Keep both files", window, cx)
                });
                right.submit_diff_review_comment(window, cx);
            });
        });
        let expected = editor.read_with(cx, |editor, cx| {
            editor.rhs_editor().read(cx).diff_review_text(cx)
        });
        assert!(expected.contains("b.txt:1-1\n"), "{expected}");
        controls.update(cx, |controls, cx| {
            controls.set_order(
                [second.read(cx).remote_id(), first.read(cx).remote_id()],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor
                .rhs_editor()
                .read(cx)
                .diff_review_text(cx)),
            expected
        );
        controls.update(cx, |controls, cx| controls.toggle_full_file(cx));
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor
                .rhs_editor()
                .read(cx)
                .diff_review_text(cx)),
            expected
        );
        fs.insert_tree(&root, json!({"c.txt": "added\n"})).await;
        let next = cx
            .background_spawn(async move {
                std::fs::write(root.join("c.txt"), "added\n")?;
                repository.snapshot(true).await
            })
            .await
            .expect("new file snapshot");
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.entries.len() == 3 || view.error.is_some()
        })
        .await;
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.entries.len(), 3);
        });
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor
                .rhs_editor()
                .read(cx)
                .diff_review_text(cx)),
            expected
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_reconciles_uncertain_previews_after_single_tab_closes(
        cx: &mut TestAppContext,
    ) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let target = DiffTarget::comparison("@-".into(), "@-".into(), "base".into(), None);
        let files = snapshot.changed_files(&target.scope).await.expect("files");
        let file = files
            .iter()
            .find(|file| file.path == "a.txt")
            .expect("text file");
        for (applied, uncertain_before_open) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                directory.path(),
                json!({"repo": {"a.txt": "new a\n", "repo": {}, "mixed.bin": "new\0binary"}}),
            )
            .await;
            let project = Project::test(fs, [directory.path()], cx).await;
            let (multi_workspace, cx) =
                cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
            let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
            let single = cx
                .update(|window, cx| {
                    KkDiff::open(
                        snapshot.clone(),
                        target.clone(),
                        file.clone(),
                        true,
                        workspace.downgrade(),
                        DiffOpenMode::Open,
                        window,
                        cx,
                    )
                })
                .await
                .expect("single comparison");
            let preview = single
                .update(cx, |single, cx| {
                    single.preview_whole_file(kk_workbench::TransferDirection::Backward, cx)
                })
                .expect("classification preview");
            let failure = || super::super::kk_classification::ClassificationFailure {
                message: "classification result unknown".into(),
                uncertain: true,
            };
            if uncertain_before_open {
                single.update(cx, |entry, cx| {
                    entry.finish_classification_preview(preview.id, Err(failure()), cx)
                });
            }
            let view = cx
                .update(|window, cx| {
                    KkMultiDiff::open(
                        snapshot.clone(),
                        target.clone(),
                        files.clone(),
                        true,
                        workspace.downgrade(),
                        DiffOpenMode::Open,
                        window,
                        cx,
                    )
                })
                .await
                .expect("aggregate comparison");
            cx.condition(&view, |view, _| !view.loading).await;
            assert!(view.read_with(cx, |view, _| view.entries.contains(&single)));
            assert!(single.read_with(cx, |entry, _| entry.has_pending_classification()));
            if uncertain_before_open {
                assert!(single.read_with(cx, |entry, _| entry.error().is_some()));
            }
            let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
            pane.update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(single.entity_id(), workspace::SaveIntent::Close, window, cx)
            })
            .await
            .expect("close standalone comparison");
            assert_eq!(
                workspace.read_with(cx, |workspace, cx| workspace
                    .items_of_type::<KkDiff>(cx)
                    .count()),
                0
            );
            let entry = single.downgrade();
            drop(single);
            let entry = entry.upgrade().expect("aggregate retains pending entry");
            if !uncertain_before_open {
                entry.update(cx, |entry, cx| {
                    entry.finish_classification_preview(preview.id, Err(failure()), cx)
                });
            }
            assert!(entry.read_with(cx, |entry, _| entry.has_pending_classification()));
            let settled_snapshot = if applied {
                cx.background_spawn({
                    let root = root.clone();
                    async move {
                        run_kk(&root, &["new", "-m", "settled classification"]).await?;
                        kk_workbench::Repository::open(&root, Path::new("kk"))
                            .await?
                            .snapshot(true)
                            .await
                    }
                })
                .await
                .expect("applied snapshot")
            } else {
                snapshot.clone()
            };
            view.update(cx, |view, cx| {
                view.resolve_classification_uncertainty(&settled_snapshot, applied, cx)
            });
            let expected_base = if applied { "new a\n" } else { "old a\n" };
            cx.condition(&entry, |entry, cx| {
                !entry.has_pending_classification()
                    && entry.error().is_none()
                    && entry.diff.read(cx).base_text_buffer().read(cx).text() == expected_base
            })
            .await;
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_nested_paths_and_file_type_transitions(cx: &mut TestAppContext) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        let snapshot = repository.snapshot(true).await.expect("snapshot");
        let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
        let files = snapshot
            .changed_files(&target.scope)
            .await
            .expect("changed files");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            directory.path(),
            json!({"repo": {"a.txt": "new a\n", "repo": {}, "mixed.bin": "new\0binary"}}),
        )
        .await;
        let project = Project::test(fs.clone(), [directory.path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |view, _| view.workspace().clone());
        let view = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot,
                    target,
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await
            .expect("mixed aggregate");
        cx.condition(&view, |view, _| !view.loading).await;
        view.read_with(cx, |view, cx| {
            assert_eq!(view.entries.len(), 3);
            assert_eq!(
                view.editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .all_buffer_ids()
                    .count(),
                3
            );
            assert!(
                !view
                    .entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == "mixed.bin")
                    .expect("binary entry")
                    .read(cx)
                    .is_text()
            );
        });
        for _ in 0..2 {
            view.update(cx, |view, cx| {
                view.controls
                    .update(cx, |controls, cx| controls.toggle_full_file(cx))
            });
        }
        std::fs::write(root.join("mixed.bin"), "now text\n").expect("replace binary");
        fs.save(
            &root.join("mixed.bin"),
            &"now text\n".into(),
            Default::default(),
        )
        .await
        .expect("project text");
        let next = repository.snapshot(true).await.expect("text snapshot");
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.snapshot.operation() == next.operation() || view.error.is_some()
        })
        .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(
                view.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == "mixed.bin")
                    .expect("text entry")
                    .read(cx)
                    .is_text()
            );
            assert_eq!(
                view.editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .all_buffer_ids()
                    .count(),
                3
            );
        });
        std::fs::write(root.join("mixed.bin"), b"binary\0again").expect("replace text");
        fs.save(
            &root.join("mixed.bin"),
            &"binary\0again".into(),
            Default::default(),
        )
        .await
        .expect("project binary");
        cx.run_until_parked();
        let next = repository.snapshot(true).await.expect("binary snapshot");
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.snapshot.operation() == next.operation() || view.error.is_some()
        })
        .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(
                !view
                    .entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == "mixed.bin")
                    .expect("binary entry")
                    .read(cx)
                    .is_text()
            );
        });
        let buffer = view.read_with(cx, |view, cx| {
            view.entries
                .iter()
                .find(|entry| entry.read(cx).file.path == "a.txt")
                .expect("renamed source")
                .read(cx)
                .buffer
                .clone()
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx)
        });
        let (entry_id, destination) = project.read_with(cx, |project, cx| {
            let path = project
                .find_project_path(&root.join("a.txt"), cx)
                .expect("source project path");
            let entry = project
                .entry_for_path(&path, cx)
                .expect("source project entry")
                .id;
            let destination = project
                .find_project_path(&root.join("b.txt"), cx)
                .expect("renamed project path");
            (entry, destination)
        });
        project
            .update(cx, |project, cx| {
                project.rename_entry(entry_id, destination, cx)
            })
            .await
            .expect("native dirty rename");
        std::fs::rename(root.join("a.txt"), root.join("b.txt")).expect("repository rename");
        cx.condition(&buffer, |buffer, cx| {
            buffer.file().and_then(|file| file.file_system_abs_path(cx)) == Some(root.join("b.txt"))
        })
        .await;
        let next = repository.snapshot(true).await.expect("renamed snapshot");
        let (target, _) = view
            .read_with(cx, |view, _| view.target.clone())
            .resolve(&next)
            .await
            .expect("renamed target");
        let expected_count = next
            .changed_files(&target.scope)
            .await
            .expect("renamed files")
            .len();
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.snapshot.operation() == next.operation() || view.error.is_some()
        })
        .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.entries.len(), expected_count);
            let dirty = view
                .entries
                .iter()
                .filter(|entry| entry.read(cx).buffer.read(cx).is_dirty())
                .collect::<Vec<_>>();
            assert_eq!(
                dirty.len(),
                1,
                "{:?}",
                dirty
                    .iter()
                    .map(|entry| {
                        let entry = entry.read(cx);
                        (
                            entry.file.path.clone(),
                            entry.buffer.read(cx).text(),
                            entry.buffer.read(cx).has_conflict(),
                        )
                    })
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                dirty
                    .first()
                    .expect("dirty renamed entry")
                    .read(cx)
                    .file
                    .path,
                "b.txt"
            );
            assert_eq!(
                view.editor
                    .read(cx)
                    .rhs_editor()
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .all_buffer_ids()
                    .count(),
                expected_count
            );
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "unsaved\nnew a\n"
        );
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_cold_restore_preserves_deleted_and_binary_drafts(
        cx: &mut TestAppContext,
    ) {
        let directory = local_fixture(cx).await;
        let root = directory.path().join("repo");
        let repository = kk_workbench::Repository::open(&root, Path::new("kk"))
            .await
            .expect("repository");
        for (deleted, binary_base, renamed) in [
            (false, false, true),
            (false, false, false),
            (true, false, false),
            (false, true, false),
        ] {
            if root.join("b.txt").exists() {
                std::fs::remove_file(root.join("b.txt")).expect("reset renamed source");
            }
            std::fs::write(root.join("a.txt"), "new a\n").expect("reset source");
            let snapshot = repository.snapshot(true).await.expect("snapshot");
            let original_operation = snapshot.operation().to_owned();
            let target = DiffTarget::change(&snapshot.exact_revision("@").await.expect("revision"));
            let files = snapshot.changed_files(&target.scope).await.expect("files");
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                directory.path(),
                json!({"repo": {"a.txt": "new a\n", "repo": {}, "mixed.bin": "new\0binary"}}),
            )
            .await;
            let project = Project::test(fs, [directory.path()], cx).await;
            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let app_state = multi_workspace
                .read_with(cx, |view, cx| view.workspace().read(cx).app_state().clone());
            let workspace_id = cx
                .update(|_, cx| workspace::WorkspaceDb::global(cx))
                .next_id()
                .await
                .expect("workspace id");
            let workspace = cx.new_window_entity(|window, cx| {
                Workspace::new(
                    Some(workspace_id),
                    project.clone(),
                    app_state.clone(),
                    window,
                    cx,
                )
            });
            multi_workspace.update_in(cx, |view, window, cx| {
                view.activate(workspace.clone(), None, window, cx)
            });
            let view = cx
                .update(|window, cx| {
                    KkMultiDiff::open(
                        snapshot,
                        target,
                        files,
                        true,
                        workspace.downgrade(),
                        DiffOpenMode::Open,
                        window,
                        cx,
                    )
                })
                .await
                .expect("aggregate");
            cx.condition(&view, |view, _| !view.loading).await;
            let entry = view.read_with(cx, |view, cx| {
                view.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == "a.txt")
                    .expect("source entry")
                    .clone()
            });
            let buffer = entry.read_with(cx, |entry, _| entry.buffer.clone());
            let draft = if binary_base {
                "unsaved\0text\n"
            } else {
                "unsaved\n"
            };
            buffer.update(cx, |buffer, cx| buffer.edit([(0..0, draft)], None, cx));
            if renamed {
                let (entry_id, destination) = project.read_with(cx, |project, cx| {
                    let source = project
                        .find_project_path(&root.join("a.txt"), cx)
                        .expect("source path");
                    let entry_id = project
                        .entry_for_path(&source, cx)
                        .expect("source entry")
                        .id;
                    let destination = project
                        .find_project_path(&root.join("b.txt"), cx)
                        .expect("destination path");
                    (entry_id, destination)
                });
                project
                    .update(cx, |project, cx| {
                        project.rename_entry(entry_id, destination, cx)
                    })
                    .await
                    .expect("native dirty rename");
                std::fs::rename(root.join("a.txt"), root.join("b.txt")).expect("repository rename");
                cx.condition(&buffer, |buffer, cx| {
                    buffer.file().and_then(|file| file.file_system_abs_path(cx))
                        == Some(root.join("b.txt"))
                })
                .await;
                assert_eq!(
                    entry.read_with(cx, |entry, _| entry.file.path.clone()),
                    "a.txt"
                );
            } else if deleted {
                std::fs::remove_file(root.join("a.txt")).expect("delete source");
            } else {
                std::fs::write(root.join("a.txt"), b"external\0binary").expect("replace source");
            }
            if binary_base {
                run_kk(
                    &root,
                    &["squash", "--into", "@-", "-m", "binary parent", "a.txt"],
                )
                .await
                .expect("rewrite parent with binary base");
            }
            let next = if renamed {
                None
            } else {
                let next = repository.snapshot(true).await.expect("changed snapshot");
                view.update(cx, |view, cx| view.refresh(&next, cx));
                cx.condition(&view, |view, _| {
                    view.snapshot.operation() == next.operation() || view.error.is_some()
                })
                .await;
                Some(next)
            };
            if binary_base {
                cx.condition(&entry, |entry, _| entry.error().is_some())
                    .await;
            }
            view.read_with(cx, |view, cx| {
                assert!(view.error.is_none(), "{:?}", view.error);
                assert!(
                    view.entries
                        .iter()
                        .any(|entry| entry.read(cx).buffer == buffer)
                );
            });
            let item_id = u64::MAX - 2;
            workspace
                .update(cx, |workspace, cx| {
                    view.update(cx, |view, cx| view.serialize(workspace, item_id, false, cx))
                })
                .expect("serialize")
                .await
                .expect("persist");
            let store = cx.update(|_, cx| KeyValueStore::global(cx));
            let namespace = session_namespace(workspace_id);
            let saved = cx
                .background_spawn({
                    async move {
                        let store = store.scoped(&namespace);
                        let text = store
                            .read(&item_id.to_string())?
                            .context("saved aggregate")?;
                        let saved: SavedMultiDiff = serde_json::from_str(&text)?;
                        if deleted {
                            let mut legacy: serde_json::Value = serde_json::from_str(&text)?;
                            for draft in legacy["drafts"].as_array_mut().context("saved drafts")? {
                                draft[1]
                                    .as_object_mut()
                                    .context("saved draft")?
                                    .remove("source")
                                    .context("draft source")?;
                            }
                            store
                                .write(item_id.to_string(), serde_json::to_string(&legacy)?)
                                .await?;
                        }
                        anyhow::Ok(saved)
                    }
                })
                .await
                .expect("inspect saved source and prepare legacy format");
            if binary_base {
                let source = saved
                    .drafts
                    .iter()
                    .find(|(path, _)| path == "a.txt")
                    .and_then(|(_, draft)| draft.source.as_ref())
                    .expect("draft source");
                assert_eq!(source.operation, original_operation);
                assert_ne!(source.operation, saved.operation);
            }
            if renamed {
                assert!(saved.drafts.iter().any(|(path, _)| path == "b.txt"));
                let file = saved
                    .files
                    .iter()
                    .find(|file| file.path == "b.txt")
                    .expect("saved destination");
                assert_eq!(file.renamed_from.as_deref(), Some("a.txt"));
            }
            let cold_fs = FakeFs::new(cx.executor());
            let mut tree = json!({"repo": {"repo": {}, "mixed.bin": "new\0binary"}});
            if renamed {
                tree["repo"]["b.txt"] = json!("new a\n");
            } else if !deleted {
                tree["repo"]["a.txt"] = json!("external\0binary");
            }
            cold_fs.insert_tree(directory.path(), tree).await;
            let cold_project = Project::test(cold_fs.clone(), [directory.path()], &mut cx.cx).await;
            assert!(
                cold_project.read_with(cx, |project, cx| project.opened_buffers(cx).is_empty())
            );
            let cold_workspace = cx.new_window_entity(|window, cx| {
                Workspace::new(
                    Some(workspace_id),
                    cold_project.clone(),
                    app_state,
                    window,
                    cx,
                )
            });
            let restored = cx
                .update(|window, cx| {
                    KkMultiDiff::deserialize(
                        cold_project.clone(),
                        cold_workspace.downgrade(),
                        workspace_id,
                        item_id,
                        window,
                        cx,
                    )
                })
                .await
                .expect("cold restore");
            let cold_buffer = restored.read_with(cx, |view, cx| {
                let entry = view
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.read(cx).file.path == if renamed { "b.txt" } else { "a.txt" }
                    })
                    .expect("restored entry")
                    .read(cx);
                assert!(entry.is_text());
                assert_eq!(entry.buffer.read(cx).capability(), Capability::ReadWrite);
                assert_eq!(entry.buffer.read(cx).text(), format!("{draft}new a\n"));
                assert!(entry.buffer.read(cx).is_dirty());
                assert_eq!(entry.buffer.read(cx).encoding().name(), "UTF-8");
                assert!(!entry.buffer.read(cx).has_bom());
                entry.buffer.clone()
            });
            assert_ne!(cold_buffer, buffer);
            if binary_base {
                restored.update(cx, |view, cx| {
                    view.refresh(next.as_ref().expect("changed snapshot"), cx)
                });
                cx.condition(&restored, |view, cx| {
                    view.entries
                        .iter()
                        .any(|entry| entry.read(cx).error().is_some())
                })
                .await;
                assert_eq!(
                    cold_buffer.read_with(cx, |buffer, _| buffer.text()),
                    format!("{draft}new a\n")
                );
            }
            if renamed {
                assert_eq!(
                    cold_buffer.read_with(cx, |buffer, cx| {
                        buffer.file().and_then(|file| file.file_system_abs_path(cx))
                    }),
                    Some(root.join("b.txt"))
                );
                restored
                    .update_in(cx, |view, window, cx| {
                        view.save(Default::default(), cold_project.clone(), window, cx)
                    })
                    .await
                    .expect("save restored rename");
                assert_eq!(
                    cold_fs
                        .load(&root.join("b.txt"))
                        .await
                        .expect("saved destination"),
                    "unsaved\nnew a\n"
                );
                assert!(
                    cold_fs
                        .metadata(&root.join("a.txt"))
                        .await
                        .expect("old path metadata")
                        .is_none()
                );
                cold_buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "later\n")], None, cx));
            }
            cold_fs
                .save(
                    &root.join(if renamed { "b.txt" } else { "a.txt" }),
                    &"external edit\n".into(),
                    Default::default(),
                )
                .await
                .expect("external edit");
            cx.condition(&cold_buffer, |buffer, _| buffer.has_conflict())
                .await;
            assert!(restored.read_with(cx, |view, cx| view.has_conflict(cx)));
            restored
                .update_in(cx, |view, window, cx| {
                    view.reload(cold_project.clone(), window, cx)
                })
                .await
                .expect("discard through native reload");
            assert_eq!(
                cold_buffer.read_with(cx, |buffer, _| buffer.text()),
                "external edit\n"
            );
        }
    }

    #[gpui::test(iterations = 3)]
    async fn kk_diff_multifile_restores_remote_drafts_and_refreshes_file_membership(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        run_multi_session(cx, server_cx)
            .await
            .expect("remote multi-file diff workflow");
    }

    async fn run_multi_session(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> Result<()> {
        let fixture = RemoteFixture::create(cx, server_cx).await?;
        std::fs::write(fixture.root.join("second.txt"), "second\n")?;
        let project = fixture.project.clone();
        let remote = project
            .read_with(cx, |project, cx| project.kk_workbench_client(cx))
            .context("remote transport")?;
        let repository = remote
            .open(
                &fixture.root,
                Path::new("kk"),
                kk_workbench::Diagnostics::default(),
            )
            .await?;
        let snapshot = repository.snapshot(true).await?;
        let parent = snapshot.exact_revision("@-").await?;
        let target = DiffTarget::comparison(
            parent.revset(),
            parent.full_commit_id,
            "parent".into(),
            None,
        );
        let files = snapshot.changed_files(&target.scope).await?;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        cx.update(|_, cx| crate::kk_diff::init(cx));
        let app_state =
            multi_workspace.read_with(cx, |view, cx| view.workspace().read(cx).app_state().clone());
        let database = cx.update(|_, cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = database.next_id().await?;
        let workspace = cx.new_window_entity(|window, cx| {
            Workspace::new(
                Some(workspace_id),
                project.clone(),
                app_state.clone(),
                window,
                cx,
            )
        });
        multi_workspace.update_in(cx, |view, window, cx| {
            view.activate(workspace.clone(), None, window, cx)
        });
        let view = cx
            .update(|window, cx| {
                KkMultiDiff::open(
                    snapshot.clone(),
                    target.clone(),
                    files,
                    true,
                    workspace.downgrade(),
                    DiffOpenMode::Open,
                    window,
                    cx,
                )
            })
            .await?;
        cx.condition(&view, |view, _| !view.loading).await;
        let buffer = view
            .read_with(cx, |view, cx| {
                view.entries
                    .iter()
                    .find(|entry| entry.read(cx).file.path == "file.txt")
                    .map(|entry| entry.read(cx).buffer.clone())
            })
            .context("editable file")?;
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx)
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            view.controls
                .update(cx, |controls, cx| controls.toggle_full_file(cx));
            view.editor.update(cx, |editor, cx| {
                let right = editor.rhs_editor().clone();
                right.update(cx, |editor, cx| {
                    let anchor = editor
                        .buffer()
                        .read(cx)
                        .buffer_point_to_anchor(&buffer, language::Point::new(0, 3), cx)
                        .expect("selection");
                    editor.change_selections(
                        editor::SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| selections.select_ranges([anchor..anchor]),
                    );
                });
            });
        });
        let item_id = u64::MAX - 1;
        workspace
            .update(cx, |workspace, cx| {
                view.update(cx, |view, cx| view.serialize(workspace, item_id, false, cx))
            })
            .context("serialize multi-file diff")?
            .await?;
        let store = cx.update(|_, cx| KeyValueStore::global(cx));
        let json = store
            .scoped(&session_namespace(workspace_id))
            .read(&item_id.to_string())?
            .context("saved multi-file tab")?;
        let saved: SavedMultiDiff = serde_json::from_str(&json)?;
        assert_eq!(saved.drafts.len(), 1);
        assert!(!saved.full_file);
        let restored_workspace = cx.new_window_entity(|window, cx| {
            Workspace::new(Some(workspace_id), project.clone(), app_state, window, cx)
        });
        let restored = cx
            .update(|window, cx| {
                KkMultiDiff::deserialize(
                    project.clone(),
                    restored_workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await?;
        restored.read_with(cx, |view, cx| {
            assert_eq!(view.entries.len(), 2);
            assert!(
                view.entries
                    .iter()
                    .any(|entry| entry.read(cx).buffer == buffer)
            );
            assert!(!view.controls.read(cx).showing_full_file);
            assert_eq!(view.snapshot.operation(), snapshot.operation());
            assert_eq!(restored_workspace.read(cx).items(cx).count(), 0);
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "unsaved\ncaptured\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("file.txt"))?,
            "captured\n"
        );
        std::fs::write(fixture.root.join("third.txt"), "third\n")?;
        let next = repository.snapshot(true).await?;
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.entries.len() == 3 || view.error.is_some()
        })
        .await;
        view.read_with(cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.entries.len(), 3);
        });
        std::fs::remove_file(fixture.root.join("second.txt"))?;
        let next = repository.snapshot(true).await?;
        view.update(cx, |view, cx| view.refresh(&next, cx));
        cx.condition(&view, |view, _| {
            view.entries.len() == 2 || view.error.is_some()
        })
        .await;
        view.read_with(cx, |view, cx| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.entries.len(), 2);
            assert!(
                view.entries
                    .iter()
                    .all(|entry| entry.read(cx).file.path != "second.txt")
            );
        });
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "unsaved\ncaptured\n"
        );
        Ok(())
    }
}
